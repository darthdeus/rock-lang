use super::*;

#[test]
fn missing_wire_slot_rejects_instead_of_boxing() {
    let slots = [RepLayout::Opaque];
    assert!(
        super::required_wire_slot(&slots, 0, false, "fixture", |message| {
            super::gap(message)
        },)
        .is_ok()
    );
    let err = super::required_wire_slot(&slots, 1, false, "fixture", super::gap)
        .expect_err("a missing non-variadic slot must reject");
    assert!(
        err.message.contains("fixture has no ABI slot 1"),
        "{}",
        err.message
    );

    let variadic = super::required_wire_slot(&slots, 3, true, "fixture", super::gap)
        .expect("a variadic call reuses its declared tail slot");
    assert!(matches!(variadic, RepLayout::Opaque));
}

// No Rock program reaches the states below: each hands the oracle a compiler
// state bone never produces, so they are rejections rather than diagnostics.
fn spec_ws(src: &str) -> (Workspace, ModuleId) {
    let mut modules =
        vec![skull_parser::parse_module_clean(src, ustr::Ustr::from("<test>")).expect("parse")];
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    let mid = *ws
        .module_ids
        .get(&ustr::Ustr::from("<test>"))
        .expect("user module id");
    (ws, mid)
}
fn spec_fn(ws: &Workspace, mid: ModuleId, name: &str) -> FuncRef {
    let (func, _) = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str().rsplit("::").next() == Some(name))
        .expect("function by name");
    FuncRef { module: mid, func }
}

#[test]
fn uniform_values_rejects_missing_and_unresolved_inference_records() {
    let (ws, mid) = spec_ws("fn vals(u: int) -> int { u }\nfn main() { vals(1) }");
    let main = spec_fn(&ws, mid, "main");
    let vals = spec_fn(&ws, mid, "vals");
    let owner = BodyOwner::Function(main.func);
    let body = &ws.module(mid).bodies[&owner];
    let (call, args) = body
        .expr_arena
        .iter()
        .find_map(|(e, expr)| {
            if let Expr::Call { args, .. } = expr {
                Some((e, args.into_iter().collect::<Vec<_>>()))
            } else {
                None
            }
        })
        .expect("call");
    let mut inf = ws.inference(mid).unwrap().per_fn[&owner].clone();
    let decl = &ws.module(mid).functions[vals.func];
    inf.expr_types.remove(call);
    let err = uniform_values_rejection_for_call(&ws, &inf, body, call, decl, &args)
        .expect("a missing packet inference record must reject");
    assert!(
        err.message.contains("has no inferred packet result"),
        "{err:?}"
    );

    let unresolved = inf.ty_arena.alloc(TypeExpr::Infer(
        bone_core::hir::VarId(u32::MAX),
        ustr::Ustr::from("packet"),
    ));
    inf.expr_types.insert(call, TyId::Local(unresolved));
    let err = uniform_values_rejection_for_call(&ws, &inf, body, call, decl, &args)
        .expect("an unresolved packet inference record must reject");
    assert!(
        err.message.contains("has no concrete packet result"),
        "{err:?}"
    );
}

const REFERENCE_CONTEXT_SRC: &str =
    "fn target(x: int) -> int { x }\nfn holder() { let f = target }\n";

#[test]
fn first_class_context_skips_value_ref_facts_and_updates() {
    let (ws, mid) = spec_ws(REFERENCE_CONTEXT_SRC);
    let target = spec_fn(&ws, mid, "target");
    let holder = spec_fn(&ws, mid, "holder");
    let owner = BodyOwner::Function(holder.func);
    let target_name = ws.module(target.module).functions[target.func].name;
    let tracked = ScalarCtx::new(&ws);
    assert!(tracked.value_references.taken_by_ref.contains(&target_name));
    assert!(
        tracked
            .value_references
            .per_body
            .get(&(mid, owner))
            .is_some_and(|refs| refs.contains(&target_name))
    );

    let mut scx = ScalarCtx::new_values_first_class(&ws);

    assert!(scx.value_references.taken_by_ref.is_empty());
    assert!(scx.value_references.per_body.is_empty());
    assert!(scx.value_references.ref_counts.is_empty());
    assert!(
        scx.update_body(&ws, mid, owner).is_empty(),
        "first-class updates cannot flip scalar-ABI eligibility"
    );
}

#[test]
fn updating_one_shared_reference_context_uses_copy_on_write() {
    let (mut ws, mid) = spec_ws(REFERENCE_CONTEXT_SRC);
    let target = spec_fn(&ws, mid, "target");
    let holder = spec_fn(&ws, mid, "holder");
    let owner = BodyOwner::Function(holder.func);
    let target_name = ws.module(target.module).functions[target.func].name;
    let original = ScalarCtx::new(&ws);
    let sibling = original.clone();
    let mut fresh = original.clone();
    assert!(std::sync::Arc::ptr_eq(
        &original.value_references,
        &sibling.value_references
    ));
    assert!(std::sync::Arc::ptr_eq(
        &original.value_references,
        &fresh.value_references
    ));

    ws.module_mut(mid).bodies.remove(&owner);
    assert_eq!(fresh.update_body(&ws, mid, owner), vec![target_name]);
    assert!(fresh.value_references.taken_by_ref.is_empty());
    assert!(fresh.value_references.per_body.is_empty());
    assert!(
        original
            .value_references
            .taken_by_ref
            .contains(&target_name)
    );
    assert!(sibling.value_references.taken_by_ref.contains(&target_name));
    assert!(std::sync::Arc::ptr_eq(
        &original.value_references,
        &sibling.value_references
    ));
    assert!(!std::sync::Arc::ptr_eq(
        &original.value_references,
        &fresh.value_references
    ));
}

/// `update_body` is the one ABI-state mutation crossing emissions; a plan served
/// after it is one the context already invalidated.
#[test]
fn a_cleared_abi_memo_cannot_serve_a_stale_plan() {
    let (ws, mid) = spec_ws("fn f(x: int) -> int { x }\nfn main() { let y = f(1) }");
    let mut scx = ScalarCtx::new(&ws);
    let inst = crate::instance::Instance::base(spec_fn(&ws, mid, "f"));
    let plan = std::sync::Arc::new(BackendFnAbiPlan::for_boundary_instance(&ws, &scx, &inst));
    scx.abi_plan_store(AbiQuestion::Owner, &inst, &plan);
    assert!(
        scx.abi_plan_cached(AbiQuestion::Owner, &inst).is_some(),
        "the stored plan must hit before the clear"
    );
    let main_fn = spec_fn(&ws, mid, "main").func;
    let _ = scx.update_body(&ws, mid, BodyOwner::Function(main_fn));
    assert!(
        scx.abi_plan_cached(AbiQuestion::Owner, &inst).is_none(),
        "update_body cleared the memo; a hit is a stale plan"
    );
}
