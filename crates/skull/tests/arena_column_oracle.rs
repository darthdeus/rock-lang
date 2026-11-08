//! `Arena<E>` and `[E]` project the same target-specific container plan.

use bone_semantic::inference::Workspace;
use skull_mir::lower::{ContainerFamily, ContainerPlan, RepArena};
use skull_mir::monoty::{MonoCtor, MonoTy};
use skull_parser::ast::FnBackend;
use std::sync::LazyLock;
use ustr::{Ustr, ustr};

static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
    rock_stdlib::stdlib_files()
        .iter()
        .map(|f| {
            let id = Ustr::from(rock_stdlib::stdlib_file_id(f.name).as_str());
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
    MonoTy::App {
        ctor: MonoCtor::Nominal(ustr(name)),
        params: Vec::new(),
    }
}

fn arena_of(elem: MonoTy) -> MonoTy {
    MonoTy::App {
        ctor: MonoCtor::Nominal(ustr("Arena")),
        params: vec![elem],
    }
}

fn vec_of(elem: MonoTy) -> MonoTy {
    MonoTy::App {
        ctor: MonoCtor::Array,
        params: vec![elem],
    }
}

fn column_elem(
    ws: &Workspace,
    target: FnBackend,
    family: ContainerFamily,
    ty: &MonoTy,
) -> Option<skull_mir::mir::ColumnElem> {
    let arena = RepArena::default();
    ContainerPlan::for_type(ws, target, &arena, family, ty)
        .expect("well-formed container type must have a plan")
        .column_elem(&arena)
}

/// Both sides of admission, so a rule that admitted nothing (or everything)
/// could not pass: an all-scalar value struct columnizes, and each rejection
/// class is rejected for its own reason.
#[test]
fn the_container_plan_admits_and_rejects_on_the_vec_rule() {
    let ws = check(SRC);
    let t = FnBackend::Lua;
    let admitted = column_elem(
        &ws,
        t,
        ContainerFamily::Arena,
        &arena_of(nominal("ColPoint")),
    )
    .expect("an all-scalar value struct is column-eligible");
    assert_eq!(admitted.name.as_str(), "ColPoint");
    assert!(
        !admitted.nullable,
        "an arena element never carries a presence column"
    );

    assert!(
        column_elem(
            &ws,
            t,
            ContainerFamily::Arena,
            &arena_of(nominal("NotAValue"))
        )
        .is_none(),
        "a non-value struct has no leaf shape to columnize"
    );
    assert!(
        column_elem(&ws, t, ContainerFamily::Arena, &arena_of(MonoTy::Int)).is_none(),
        "a primitive element is not a value struct"
    );
    // The `for`-over-arena walk reads occupied/generations for liveness and has
    // no presence concept, so a nullable element must stay boxed.
    assert!(
        column_elem(
            &ws,
            t,
            ContainerFamily::Arena,
            &arena_of(MonoTy::Nullable(Box::new(nominal("ColPoint"))))
        )
        .is_none(),
        "a nullable element would need a presence column its only reader cannot address"
    );
    // A plan is target/family-specific; asking the Arena planner about a Vec is
    // an invalid boundary rather than a boxed storage answer.
    assert!(
        ContainerPlan::for_type(
            &ws,
            t,
            &RepArena::default(),
            ContainerFamily::Arena,
            &vec_of(nominal("ColPoint")),
        )
        .is_err(),
        "a Vec must not be accepted by the Arena planner"
    );
}

/// THE property: for every element type, `Arena<E>` and `[E]` give the same
/// column answer. Asserted over a population with a non-zero admitted count and
/// a non-zero rejected count, so agreement cannot be agreement about nothing.
#[test]
fn the_arena_and_vec_plans_cannot_disagree() {
    let ws = check(SRC);
    let mut admitted = 0;
    let mut rejected = 0;

    // EVERY backend, not just the one lowering happens to run: availability is
    // the per-target half of the decision, so a rule that reached Vec and not
    // Arena would only ever show up on the target it was written for.
    for target in [FnBackend::Any, FnBackend::Lua, FnBackend::Js] {
        for elem in [
            nominal("ColPoint"),
            nominal("NotAValue"),
            MonoTy::Int,
            MonoTy::String,
        ] {
            let a = column_elem(&ws, target, ContainerFamily::Arena, &arena_of(elem.clone()));
            let v = column_elem(&ws, target, ContainerFamily::Vec, &vec_of(elem.clone()));
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

    assert!(
        admitted > 0,
        "nothing was admitted, so agreement proves nothing"
    );
    assert!(
        rejected > 0,
        "nothing was rejected, so agreement proves nothing"
    );
}
