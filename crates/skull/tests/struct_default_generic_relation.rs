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

const SRC: &str = r#"
fn seed<T>() -> T? { return null }

struct Box<T> { value: T? = seed() }

fn main() {
    let a: Box<int> = Box {}
    let b: Box<string> = Box {}
    print("{a.value} {b.value}")
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
/// This is the half that is already correct, and it is asserted first because
/// the missing half below is only meaningful against it: if these were two
/// classes, the fix would be in `coerce`/`unify` and nowhere near the site
/// domain.
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
    let dexpr = *body.field_defaults.get(&Ustr::from("value")).expect("value has a default");

    // The default expression's own type -- `seed()`'s return, i.e. `T?`.
    let dty = *inf.expr_types.get(dexpr).expect("the default is typed");
    // And the binding the CALL recorded for `seed`'s generic.
    let binding = inf
        .call_generic_bindings
        .get(&dexpr)
        .and_then(|b| b.first().copied())
        .expect("the call records a binding for `seed`'s generic");

    let d_var = unresolved_var(&ws, inf, dty);
    let b_var = unresolved_var(&ws, inf, binding);
    assert!(
        d_var.is_some() && b_var.is_some(),
        "one of the two resolved to a concrete type, so this fixture is not the \
         generic case any more: default={d_var:?} binding={b_var:?}"
    );
    assert_eq!(
        d_var, b_var,
        "the initializer's generic and the field's declared type are in TWO \
         inference classes -- `coerce` did not unify them, and the gap is in \
         bone-inference rather than in the site domain"
    );
}

/// PINNED: the inference class above has NO published relation to the struct's
/// declared generic SLOT.
///
/// `instantiate_generics` builds `VarId(T) -> Infer(N)` and returns it to
/// `check_struct_defaults`, which uses it and drops it. Nothing in
/// `BodyInference` maps `Infer(N)` back to "generic 0 of `Box`", so a consumer
/// holding the literal's `Box<int>` cannot resolve the default's type -- which
/// is exactly what instance-owned emission needs in order to name one concrete
/// callee per injection.
///
/// The assertion is the ABSENCE, stated over the struct's declared generics so
/// the zero has a denominator. **When this goes red the relation was
/// published**: replace it with the positive form -- for each declared generic
/// slot, the inference variable it was instantiated to -- and then key
/// `SiteDomain::StructDefaults` on the literal's concrete arguments.
#[test]
fn the_defaults_inference_class_has_no_link_to_the_structs_generic_slot_sentinel() {
    let ws = check(SRC);
    let (mid, inf) = defaults_inference(&ws);
    let sidx = ws
        .module(mid)
        .structs
        .iter()
        .find(|(_, s)| s.name.as_str() == "Box")
        .map(|(i, _)| i)
        .expect("Box");
    let generics = ws.module(mid).structs[sidx].generics.clone();
    assert_eq!(generics.len(), 1, "the fixture's struct declares one generic");

    // Every declared generic, asked of the DEFAULTS body's inference: does any
    // of them resolve to something the body can name?
    let mut linked = 0usize;
    for g in &generics {
        // The declared generic is a `Static` `Generic` node. In the defaults
        // body it was replaced by a fresh inference variable, and the map that
        // did so is not published -- so reading the declared slot here gives
        // back the generic itself, with nothing tying it to that variable.
        let node = match resolve(&ws, inf, *g) {
            TyId::Static(m, idx) => ws.module(m).ty_arena[idx],
            TyId::Local(idx) => inf.ty_arena[idx],
        };
        if !matches!(node, TypeExpr::Generic(..)) {
            linked += 1;
        }
    }
    assert_eq!(
        linked, 0,
        "PINNED BEHAVIOR (BUG): {linked} of {} declared generic(s) now resolve \
         to the defaults body's inference class. The relation is published -- \
         flip this to the positive form and key the site domain on the \
         literal's concrete arguments.",
        generics.len()
    );
}
