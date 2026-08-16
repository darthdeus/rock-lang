use super::*;

use bone_semantic::hir::{TypeExpr, VarId};

#[test]
fn missing_wire_slot_rejects_instead_of_boxing() {
    let slots = [RetSlot::Opaque];
    assert!(super::required_wire_slot(&slots, 0, false, "fixture").is_ok());
    let err = super::required_wire_slot(&slots, 1, false, "fixture")
        .expect_err("a missing non-variadic slot must reject");
    assert!(err.message.contains("fixture has no ABI slot 1"), "{}", err.message);

    let variadic = super::required_wire_slot(&slots, 3, true, "fixture")
        .expect("a variadic call reuses its declared tail slot");
    assert!(matches!(variadic, RetSlot::Opaque));
}

// -- Stage 0b route-table invariants (docs/monomorphization-plan.md S 2b).
//
// No Rock program can reach these states, which is why they are rejections
// rather than diagnostics: each one is constructed by handing the oracle a
// compiler state bone never produces.

fn spec_ws(src: &str) -> (Workspace, ModuleId) {
    let mut modules =
        vec![skull_parser::parse_module(src, ustr::Ustr::from("<test>")).expect("parse")];
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    let mid = *ws.module_ids.get(&ustr::Ustr::from("<test>")).expect("user module id");
    (ws, mid)
}
fn spec_fn(ws: &Workspace, mid: ModuleId, name: &str) -> FuncRef {
    let (func, _) = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == name)
        .expect("function by name");
    FuncRef { module: mid, func }
}

fn spec_inf<'a>(ws: &'a Workspace, mid: ModuleId, name: &str) -> &'a BodyInference {
    let fref = spec_fn(ws, mid, name);
    ws.inference(mid)
        .expect("inference")
        .per_fn
        .get(&BodyOwner::Function(fref.func))
        .expect("body inference")
}

/// A resolved generic direct call with no `call_generic_bindings` entry.
/// Inference records one for every such call, so an absent record means the
/// resolution that produced the `FuncRef` and the inference that recorded the
/// bindings disagree. The state is built by asking for the spec of a generic
/// callee at an expr that is not that call.
#[test]
fn missing_call_generic_bindings_rejects() {
    let (ws, mid) = spec_ws("fn id<T>(x: T) -> T { x }\nfn main() { let y = 1 }");
    let inf = spec_inf(&ws, mid, "main");
    let body = ws.module(mid).bodies.get(&BodyOwner::Function(spec_fn(&ws, mid, "main").func))
        .expect("main body");
    let e = body.expr_arena.iter().next().expect("an expr in main").0;
    assert!(!inf.call_generic_bindings.contains_key(&e), "the probe expr must have no record");
    let err = crate::lower::shape::callee_instance_subst(
        &ws,
        inf,
        e,
        spec_fn(&ws, mid, "id"),
        &SpecSubst::default(),
    )
    .expect_err("a generic callee with no recorded bindings must reject");
    assert!(err.message.contains("no recorded generic bindings"), "{}", err.message);
}

#[test]
fn invalid_logical_type_bindings_reject_with_callee_spans() {
    let (mut ws, mid) = spec_ws("fn id<T>(x: T) -> T { x }\nfn main() { let y = 1 }");
    let fref = spec_fn(&ws, mid, "id");
    let inf = spec_inf(&ws, mid, "main");
    let recovery = crate::lower::shape::instance_subst_from_bindings(
        &ws,
        inf,
        fref,
        &[ws.builtins.error()],
        &SpecSubst::default(),
    )
    .expect_err("inference recovery poison must reject");
    assert!(recovery.message.contains("not a logical type"), "{}", recovery.message);
    assert!(!recovery.labels.is_empty(), "recovery rejection lost its source label");
    assert_ne!(recovery.labels[0].span.file.as_str(), "<unknown>");

    let unresolved = {
        let owner = BodyOwner::Function(fref.func);
        let inference = ws.inferences[mid.raw() as usize].as_mut().expect("inference");
        let body_inf = inference.per_fn.get_mut(&owner).expect("id inference");
        body_inf
            .ty_arena
            .alloc(TypeExpr::Infer(VarId(991), ustr::Ustr::from("unresolved")))
    };
    let unresolved_err = crate::lower::shape::instance_subst_from_bindings(
        &ws,
        spec_inf(&ws, mid, "id"),
        fref,
        &[bone_semantic::prelude::TyId::Local(unresolved)],
        &SpecSubst::default(),
    )
    .expect_err("an unresolved inference variable must reject");
    assert!(unresolved_err.message.contains("unresolved inference"), "{}", unresolved_err.message);
    assert!(!unresolved_err.labels.is_empty(), "unresolved rejection lost its source label");
    assert_ne!(unresolved_err.labels[0].span.file.as_str(), "<unknown>");
}

/// A binding list whose length differs from the callee's generic count. The
/// silent `None` left the instantiation unspecialized; the two lists are
/// built per callee, so a mismatch is compiler state, never a program.
#[test]
fn spec_binding_count_mismatch_rejects() {
    let (ws, mid) = spec_ws("fn id<T>(x: T) -> T { x }\nfn main() { let y = 1 }");
    let err = crate::lower::shape::instance_subst_from_bindings(
        &ws,
        spec_inf(&ws, mid, "main"),
        spec_fn(&ws, mid, "id"),
        &[],
        &SpecSubst::default(),
    )
    .expect_err("0 bindings against a 1-generic callee must reject");
    assert!(err.message.contains("declares 1 generic"), "{}", err.message);
}

/// A declared entry of `Function::generics` that is not `TypeExpr::Generic`.
/// Constructed by repointing the declaration at a builtin node: the silent
/// route pushed `None` into the key and left the generic out of the
/// `SpecSubst` while the key still claimed the instantiation.
#[test]
fn non_generic_declared_generic_rejects() {
    let (mut ws, mid) =
        spec_ws("fn id<T>(x: T) -> T { x }\nfn main() { let y: int = 1 }");
    let int_ty = ws.builtins.int();
    let fref = spec_fn(&ws, mid, "id");
    ws.modules[mid.index()].functions[fref.func].generics[0] = int_ty;
    let err = crate::lower::shape::instance_subst_from_bindings(
        &ws,
        spec_inf(&ws, mid, "main"),
        fref,
        &[int_ty],
        &SpecSubst::default(),
    )
    .expect_err("a non-generic declared generic must reject");
    assert!(err.message.contains("is not a generic type variable"), "{}", err.message);
}
