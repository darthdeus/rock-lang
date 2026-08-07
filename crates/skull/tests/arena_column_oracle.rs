//! `Arena<E>` column eligibility is decided by the SAME oracle as `[E]`.
//!
//! An arena's `data` is an ordinary `[E]`, so the two questions are one
//! question. They were two: `Builder::arena_column_elem_of` walked
//! `resolve_tyid`/`ty_node`/`spec_subst` by hand and called bare
//! `vec_column_elem`, while the Vec and Map paths moved onto
//! `concrete_ty` -> `*_column_elem_mono` -- which additionally filters on
//! `columns_available(ws, target, elem)`. The Arena walk applied no target
//! filter, so the two could disagree the moment that filter grew a body.
//!
//! Both now delegate to one `column_elem_payload`, so the shared decision is
//! spelled once. `columns_available` returns `true` unconditionally today, so no
//! INPUT can currently make them disagree -- which is exactly why the property
//! is asserted as agreement over a population rather than as a specific expected
//! answer, and why it is asserted for EVERY backend. A future JS-only
//! availability rule that reached Vec and not Arena would otherwise diverge with
//! nothing watching.

use bone_semantic::inference::Workspace;
use skull_mir::lower::{arena_column_elem_mono, vec_column_elem_mono};
use skull_mir::monoty::{MonoCtor, MonoTy};
use skull_parser::ast::FnBackend;
use std::sync::LazyLock;
use ustr::{ustr, Ustr};

static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
    rock_host::stdlib_loader::stdlib_files()
        .iter()
        .map(|f| {
            let id = Ustr::from(format!("<stdlib:{}>", f.name).as_str());
            skull_parser::parse_module_clean(f.contents, id).expect("stdlib parse")
        })
        .collect()
});

fn check(src: &str) -> Workspace {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(src, ustr("<test>")).expect("parse"));
    bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass")
}

const SRC: &str = r#"
value ColPoint { x: int, y: int }
struct NotAValue { x: int }

fn use_them(p: ColPoint, n: NotAValue) -> int { return p.x + n.x }
"#;

fn nominal(name: &str) -> MonoTy {
    MonoTy::App { ctor: MonoCtor::Nominal(ustr(name)), params: Vec::new() }
}

fn arena_of(elem: MonoTy) -> MonoTy {
    MonoTy::App { ctor: MonoCtor::Nominal(ustr("Arena")), params: vec![elem] }
}

fn vec_of(elem: MonoTy) -> MonoTy {
    MonoTy::App { ctor: MonoCtor::Array, params: vec![elem] }
}

/// Both sides of admission, so a rule that admitted nothing (or everything)
/// could not pass: an all-scalar value struct columnizes, and each rejection
/// class is rejected for its own reason.
#[test]
fn the_arena_oracle_admits_and_rejects_on_the_vec_rule() {
    let ws = check(SRC);
    let t = FnBackend::Lua;
    let admitted = arena_column_elem_mono(&ws, t, &arena_of(nominal("ColPoint")))
        .expect("an all-scalar value struct is column-eligible");
    assert_eq!(admitted.name.as_str(), "ColPoint");
    assert!(!admitted.nullable, "an arena element never carries a presence column");

    assert!(
        arena_column_elem_mono(&ws, t, &arena_of(nominal("NotAValue"))).is_none(),
        "a non-value struct has no leaf shape to columnize"
    );
    assert!(
        arena_column_elem_mono(&ws, t, &arena_of(MonoTy::Int)).is_none(),
        "a primitive element is not a value struct"
    );
    // The `for`-over-arena walk reads occupied/generations for liveness and has
    // no presence concept, so a nullable element must stay boxed.
    assert!(
        arena_column_elem_mono(
            &ws,
            t,
            &arena_of(MonoTy::Nullable(Box::new(nominal("ColPoint"))))
        )
        .is_none(),
        "a nullable element would need a presence column its only reader cannot address"
    );
    // Only an Arena. The oracle is reached from a site already gated on the
    // `Arena` nominal, and answering for another container would be answering a
    // question it was not asked.
    assert!(
        arena_column_elem_mono(&ws, t, &vec_of(nominal("ColPoint"))).is_none(),
        "a `[E]` is the Vec oracle's question"
    );
}

/// THE property: for every element type, `Arena<E>` and `[E]` give the same
/// column answer. Asserted over a population with a non-zero admitted count and
/// a non-zero rejected count, so agreement cannot be agreement about nothing.
#[test]
fn the_arena_and_vec_oracles_cannot_disagree() {
    let ws = check(SRC);
    let mut admitted = 0;
    let mut rejected = 0;

    // EVERY backend, not just the one lowering happens to run: availability is
    // the per-target half of the decision, so a rule that reached Vec and not
    // Arena would only ever show up on the target it was written for.
    for target in [FnBackend::Any, FnBackend::Lua, FnBackend::Js] {
        for elem in [nominal("ColPoint"), nominal("NotAValue"), MonoTy::Int, MonoTy::String] {
            let a = arena_column_elem_mono(&ws, target, &arena_of(elem.clone()));
            let v = vec_column_elem_mono(&ws, target, &vec_of(elem.clone()));
            assert_eq!(
                a,
                v,
                "Arena and Vec disagreed on `{}` for {target:?}: an arena's data \
                 IS a `[E]`, so a second eligibility rule is the bug",
                elem.render()
            );
            if a.is_some() {
                admitted += 1;
            } else {
                rejected += 1;
            }
        }
    }

    assert!(admitted > 0, "nothing was admitted, so agreement proves nothing");
    assert!(rejected > 0, "nothing was rejected, so agreement proves nothing");
}
