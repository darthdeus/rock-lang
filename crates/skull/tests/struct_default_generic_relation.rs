//! **What bone records for a generic struct's field defaults** (plan S 9 /
//! Stage 3).
//!
//! Read-only about `bone-inference`. This gap looked like missing inference and
//! was not -- a fix designed from that reading would have been built in the
//! wrong crate -- so what bone does is asserted here rather than assumed.
//!
//! `check_struct_defaults` propagates the field's declared type into the
//! initializer: it calls `instantiate_generics` on the struct's generics --
//! which replaces each declared `Generic(T)` with a FRESH inference variable --
//! substitutes the field type through that map, infers the default, and
//! `coerce`s the two, which unifies through `Nullable` on both sides. So the
//! callee's generic and the field's `T` end up in ONE symbolic inference class.
//!
//! A consumer holding `Box<int>` also needs to say "inference variable N is
//! this struct's generic 0", which the class alone does not tell it.
//!
//! `BodyInference::struct_default_generic_slots` is that relation, and the
//! slot arms below are what say it names the right class rather than merely
//! being present. What CONSUMES it -- distinct sites, distinct concrete
//! instances -- is asserted end-to-end by
//! `site_domain::a_generic_structs_field_default_names_its_own_instance`.

use crate::common::ws_with_stdlib;
use bone_semantic::hir::{BodyOwner, TyId, TypeExpr};
use bone_semantic::inference::{BodyInference, Workspace};
use ustr::Ustr;

/// `echo`'s default is a SIBLING REFERENCE to `value`: the only way to reach the
/// substituted DECLARED field type. `seed()`'s own return type cannot serve --
/// bone builds it FROM the call's generic binding (`calls.rs`'s
/// `substitute(ret_ty, &subs)`) before the field coercion runs, so comparing
/// those two compares the call with itself and holds whether or not the
/// coercion exists.
const SRC: &str = r#"
fn seed<T>() -> T? { return null }

struct Box<T> { value: T? = seed(), echo: T? = .value }

fn main() {
    let a: Box<int> = Box {}
    let b: Box<string> = Box {}
    print("{a.value} {a.echo} {b.value}")
}
"#;

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
/// binding. `.value` reads `value`'s substituted DECLARED type, so the two
/// sides agree only if `coerce` linked them -- deleting the coercion in
/// `check_struct_defaults` turns this red.
#[test]
fn a_generic_field_default_and_its_declared_type_share_one_inference_class() {
    let ws = ws_with_stdlib(SRC);
    let (mid, inf) = defaults_inference(&ws);
    let sidx = ws
        .module(mid)
        .structs
        .iter()
        .find(|(_, s)| s.name.as_str() == "Box")
        .map(|(i, _)| i)
        .expect("Box");
    let body = &ws.module(mid).bodies[&BodyOwner::StructDefaults(sidx)];
    let value_default = *body
        .field_defaults
        .get(&Ustr::from("value"))
        .expect("`value` has a default");
    let echo_default = *body
        .field_defaults
        .get(&Ustr::from("echo"))
        .expect("`echo` has a default");

    // The fixture must really be a sibling reference, or it reads some other
    // expression's type and proves nothing about the declared field type.
    assert!(
        matches!(body.expr_arena[echo_default], bone_semantic::hir::Expr::SiblingRef(n) if n.as_str() == "value"),
        "`echo`'s default is not a sibling reference: {:?}",
        body.expr_arena[echo_default]
    );

    // `value`'s SUBSTITUTED declared type, reached through the sibling ref.
    let declared = *inf
        .expr_types
        .get(echo_default)
        .expect("the sibling ref is typed");
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

/// The published relation NAMES that class: slot `i` of
/// `struct_default_generic_slots` is the var the struct's declared generic `i`
/// was instantiated to.
///
/// This is the half the test above exists to contrast with. Asserting the arity
/// alone would pass on any vector of the right length, so the slot is compared
/// against the class the sibling reference and the call binding already agree
/// on -- if the recording published some other var, the two disagree.
#[test]
fn the_defaults_body_publishes_its_declared_generic_slots() {
    let ws = ws_with_stdlib(SRC);
    let (mid, inf) = defaults_inference(&ws);
    let sidx = ws
        .module(mid)
        .structs
        .iter()
        .find(|(_, s)| s.name.as_str() == "Box")
        .map(|(i, _)| i)
        .expect("Box");
    let def = &ws.module(mid).structs[sidx];

    assert_eq!(
        inf.struct_default_generic_slots.len(),
        def.generics.len(),
        "one slot per DECLARED generic, or a consumer cannot index by position"
    );

    let body = &ws.module(mid).bodies[&BodyOwner::StructDefaults(sidx)];
    let value_default = *body
        .field_defaults
        .get(&Ustr::from("value"))
        .expect("`value` has a default");
    let class = unresolved_var(
        &ws,
        inf,
        *inf.call_generic_bindings
            .get(&value_default)
            .and_then(|b| b.first())
            .expect("the call records a binding for `seed`'s generic"),
    );
    assert!(class.is_some(), "the fixture is no longer the generic case");

    assert_eq!(
        unresolved_var(&ws, inf, inf.struct_default_generic_slots[0]),
        class,
        "slot 0 is not the class `Box`'s `T` was instantiated to, so a consumer \
         holding `Box<int>` would bind the wrong variable"
    );
}

/// Two generics, and the slots are in DECLARATION order.
///
/// One generic cannot distinguish an ordered vector from a set, and the
/// consumer indexes by position: a reversed recording binds `A` to `B`'s
/// argument silently, since both slots are inference vars of the same shape.
#[test]
fn the_slots_are_in_declaration_order() {
    const PAIR: &str = r#"
fn seed<T>() -> T? { return null }

struct Pair<A, B> { a: A? = seed(), b: B? = seed() }

fn main() {
    let p: Pair<int, string> = Pair {}
    print("{p.a} {p.b}")
}
"#;
    let ws = ws_with_stdlib(PAIR);
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    let sidx = ws
        .module(mid)
        .structs
        .iter()
        .find(|(_, s)| s.name.as_str() == "Pair")
        .map(|(i, _)| i)
        .expect("Pair");
    let inf = ws
        .inference(mid)
        .and_then(|ir| ir.per_fn.get(&BodyOwner::StructDefaults(sidx)))
        .expect("Pair has a defaults body inference");
    let body = &ws.module(mid).bodies[&BodyOwner::StructDefaults(sidx)];

    assert_eq!(inf.struct_default_generic_slots.len(), 2);

    for (slot, field) in [(0usize, "a"), (1, "b")] {
        let default = *body
            .field_defaults
            .get(&Ustr::from(field))
            .expect("default");
        let class = unresolved_var(
            &ws,
            inf,
            *inf.call_generic_bindings
                .get(&default)
                .and_then(|b| b.first())
                .expect("binding"),
        );
        assert!(
            class.is_some(),
            "field `{field}` is no longer the generic case"
        );
        assert_eq!(
            unresolved_var(&ws, inf, inf.struct_default_generic_slots[slot]),
            class,
            "slot {slot} does not match field `{field}`'s class -- the slots are \
             not in declaration order"
        );
    }
}

/// A non-generic struct's defaults body publishes NO slots.
///
/// Without this the field could be populated unconditionally and every arm
/// above would still pass; the consumer needs the empty case to mean "nothing
/// to bind" rather than "not recorded yet".
#[test]
fn a_non_generic_structs_defaults_body_publishes_no_slots() {
    const PLAIN: &str = r#"
struct Plain { n: int = 3 }

fn main() {
    let p: Plain = Plain {}
    print("{p.n}")
}
"#;
    let ws = ws_with_stdlib(PLAIN);
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    let sidx = ws
        .module(mid)
        .structs
        .iter()
        .find(|(_, s)| s.name.as_str() == "Plain")
        .map(|(i, _)| i)
        .expect("Plain");
    let inf = ws
        .inference(mid)
        .and_then(|ir| ir.per_fn.get(&BodyOwner::StructDefaults(sidx)))
        .expect("Plain has a defaults body inference");

    assert!(inf.struct_default_generic_slots.is_empty());
}
