//! **What bone DOES record for a generic struct's field defaults, and what it
//! does not** (plan S 9 / Stage 3, the C+D prerequisite).
//!
//! These tests are read-only about `bone-inference`. They exist because the
//! obvious diagnosis of the generic struct-default gap is wrong, and a fix
//! designed from the wrong diagnosis would be built in the wrong crate.
//!
//! `check_struct_defaults` DOES propagate the field's declared type into the
//! initializer: it calls `instantiate_generics` on the struct's generics --
//! which replaces each declared `Generic(T)` with a FRESH inference variable --
//! substitutes the field type through that map, infers the default, and
//! `coerce`s the two, which unifies through `Nullable` on both sides. So the
//! callee's generic and the field's `T` end up in ONE symbolic inference class.
//!
//! What is missing is a durable relation from that class back to the struct's
//! declared generic SLOT. The map `instantiate_generics` returns is local to
//! the check and is not published, so a later consumer holding `Box<int>` has
//! no way to say "inference variable N is this struct's generic 0" and resolve
//! it. That, not "bone leaves it unsolved", is what the cut needs.
//!
//! There is deliberately NO bone-level sentinel for that absence here. A test
//! that reads today's `BodyInference` can only assert a structural zero over
//! fields that already exist, which stays green forever no matter what a future
//! recording adds -- it would be a gate with no population, not a sentinel. The
//! RED for it has to come after the intended recording/query seam exists and
//! returns empty, and that seam is in bone, which is author-owned. The CURRENT
//! symptom is guarded end-to-end by
//! `site_domain::a_generic_structs_field_default_names_no_instance_sentinel`.

use std::sync::LazyLock;

use bone_semantic::hir::{BodyOwner, TyId, TypeExpr};
use bone_semantic::inference::{BodyInference, Workspace};
use ustr::Ustr;

static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
    rock_host::stdlib_loader::stdlib_files()
        .iter()
        .map(|f| {
            let id = Ustr::from(format!("<stdlib:{}>", f.name).as_str());
            skull_parser::parse_module_clean(f.contents, id).expect("stdlib parse")
        })
        .collect()
});

/// `echo`'s default is a SIBLING REFERENCE to `value`, which is what exposes
/// the substituted DECLARED field type to a test. Without it the only thing
/// reachable is `seed()`'s own return type -- which bone builds FROM the call's
/// generic binding (`calls.rs`'s `substitute(ret_ty, &subs)`) before the field
/// coercion runs, so comparing the two compares the call with itself and stays
/// green with the coercion deleted. Measured: it did.
const SRC: &str = r#"
fn seed<T>() -> T? { return null }

struct Box<T> { value: T? = seed(), echo: T? = .value }

fn main() {
    let a: Box<int> = Box {}
    let b: Box<string> = Box {}
    print("{a.value} {a.echo} {b.value}")
}
"#;

fn check(src: &str) -> Workspace {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(src, Ustr::from("<test>")).expect("parse"));
    bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass")
}

/// Chase `Infer` substitutions to the final `TyId`, without peeling anything.
fn resolve(ws: &Workspace, inf: &BodyInference, ty: TyId) -> TyId {
    let mut cur = ty;
    for _ in 0..64 {
        let node = match cur {
            TyId::Static(mid, idx) => ws.module(mid).ty_arena[idx],
            TyId::Local(idx) => inf.ty_arena[idx],
        };
        match node {
            TypeExpr::Infer(var, _) => match inf.substitutions.get(var) {
                Some(next) => cur = next,
                None => return cur,
            },
            _ => return cur,
        }
    }
    cur
}

/// The unresolved inference VARIABLE `ty` bottoms out at, if any.
///
/// Peels `Nullable` on the way down: the field is declared `T?` and the callee
/// returns `T?`, so the shared class sits one level inside both.
fn unresolved_var(ws: &Workspace, inf: &BodyInference, ty: TyId) -> Option<String> {
    let mut cur = resolve(ws, inf, ty);
    for _ in 0..64 {
        let node = match cur {
            TyId::Static(mid, idx) => ws.module(mid).ty_arena[idx],
            TyId::Local(idx) => inf.ty_arena[idx],
        };
        match node {
            TypeExpr::Infer(var, _) => return Some(format!("{var:?}")),
            TypeExpr::Nullable(inner) => cur = resolve(ws, inf, cur.with_idx(inner)),
            _ => return None,
        }
    }
    None
}

fn defaults_inference(ws: &Workspace) -> (bone_semantic::prelude::ModuleId, &BodyInference) {
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    let (sidx, _) = ws
        .module(mid)
        .structs
        .iter()
        .find(|(_, s)| s.name.as_str() == "Box")
        .expect("Box");
    let inf = ws
        .inference(mid)
        .and_then(|ir| ir.per_fn.get(&BodyOwner::StructDefaults(sidx)))
        .expect("Box has a defaults body inference");
    (mid, inf)
}

/// The initializer's generic and the FIELD's declared type land in ONE symbolic
/// inference class.
///
/// This is the half that already works, and it is asserted because the missing
/// half is only meaningful against it: if these were two classes, the fix would
/// be in `coerce`/`unify` and nowhere near the site domain.
///
/// The comparison is the SIBLING REFERENCE's type against the CALL's generic
/// binding. `seed()`'s own return type is not usable here -- bone substitutes it
/// from that same binding before the field coercion runs, so comparing the two
/// is comparing the call with itself. `.value` reads `value`'s substituted
/// DECLARED type instead, so the two sides only agree if `coerce` actually
/// linked them. Mutation-verified: deleting the coercion in
/// `check_struct_defaults` turns this red, and turned the previous version of
/// this test green.
#[test]
fn a_generic_field_default_and_its_declared_type_share_one_inference_class() {
    let ws = check(SRC);
    let (mid, inf) = defaults_inference(&ws);
    let sidx = ws
        .module(mid)
        .structs
        .iter()
        .find(|(_, s)| s.name.as_str() == "Box")
        .map(|(i, _)| i)
        .expect("Box");
    let body = &ws.module(mid).bodies[&BodyOwner::StructDefaults(sidx)];
    let value_default =
        *body.field_defaults.get(&Ustr::from("value")).expect("`value` has a default");
    let echo_default =
        *body.field_defaults.get(&Ustr::from("echo")).expect("`echo` has a default");

    // The fixture must really be a sibling reference, or it reads some other
    // expression's type and proves nothing about the declared field type.
    assert!(
        matches!(body.expr_arena[echo_default], bone_semantic::hir::Expr::SiblingRef(n) if n.as_str() == "value"),
        "`echo`'s default is not a sibling reference: {:?}",
        body.expr_arena[echo_default]
    );

    // `value`'s SUBSTITUTED declared type, reached through the sibling ref.
    let declared = *inf.expr_types.get(echo_default).expect("the sibling ref is typed");
    // The binding the CALL recorded for `seed`'s own generic.
    let binding = inf
        .call_generic_bindings
        .get(&value_default)
        .and_then(|b| b.first().copied())
        .expect("the call records a binding for `seed`'s generic");

    let d_var = unresolved_var(&ws, inf, declared);
    let b_var = unresolved_var(&ws, inf, binding);
    assert!(
        d_var.is_some() && b_var.is_some(),
        "one side resolved to a concrete type, so this fixture is not the \
         generic case any more: declared={d_var:?} binding={b_var:?}"
    );
    assert_eq!(
        d_var, b_var,
        "the initializer's generic and the field's declared type are in TWO \
         inference classes -- `coerce` did not link them, and the gap is in \
         bone-inference rather than in the site domain"
    );
}
