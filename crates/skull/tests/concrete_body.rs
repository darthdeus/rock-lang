//! `ConcreteBody` type tables.
//!
//! The property under test is not "a table exists". It is that the table is a
//! function of the INSTANCE: one generic body reached at two instantiations has
//! two tables that disagree on the same slot, and neither carries a free
//! generic. A table built under an empty substitution would fail every generic
//! slot instead, which is what the mutation arm at the bottom asserts.

use crate::common::{fn_name, graph_of, module_id, scalar_ctx, ws_with_stdlib};
use bone_semantic::hir::{BindingInfo, BodyOwner, Expr, TypeExpr};
use bone_semantic::inference::Workspace;
use comfy_la_arena::{ArenaMap, Idx, IdxRange};
use skull::concrete_body::{self, Callee, ConcreteBodies, IteratorOpKind, Slot};
use skull::instance_graph::{self, Instance, InstanceGraph, Site, SiteOp, SiteOutcome};
use skull_mir::concrete::{ConcreteBodyPlan, ConcreteTypeTables};
use skull_mir::mir::{Inst, MirFunc, Term};
use skull_mir::monoty::{MonoTy, MonoTyId, MonoTyReason};
use ustr::Ustr;

fn bodies_of_raw(src: &str) -> (Workspace, InstanceGraph, ConcreteBodies) {
    let ws = ws_with_stdlib(src);
    let g = graph_of(&ws);
    let bodies = concrete_body::build(&ws, &g);
    (ws, g, bodies)
}

fn bodies_of(src: &str) -> (Workspace, InstanceGraph, ConcreteBodies) {
    let (ws, g, mut bodies) = bodies_of_raw(src);
    bodies.refresh_flat_views();
    (ws, g, bodies)
}

fn plan_named<'a>(
    ws: &Workspace,
    bodies: &'a ConcreteBodies,
    name: &str,
) -> &'a std::sync::Arc<ConcreteBodyPlan> {
    bodies
        .plans
        .iter()
        .find(|(inst, _)| fn_name(ws, inst.func) == name)
        .map(|(_, plan)| plan)
        .unwrap_or_else(|| panic!("{name} has no concrete plan"))
}

fn callee_named(ws: &Workspace, plan: &ConcreteBodyPlan, name: &str) -> Instance {
    plan.callees
        .values()
        .find(|inst| fn_name(ws, inst.func) == name)
        .cloned()
        .unwrap_or_else(|| panic!("no concrete `{name}` call"))
}

fn expanded_with_prefix(ws: &Workspace, g: &InstanceGraph, prefix: &str) -> Instance {
    g.expanded()
        .find(|i| g.identity(ws, i).starts_with(prefix))
        .cloned()
        .unwrap_or_else(|| panic!("no expanded instance of `{prefix}`"))
}

fn assert_arena_tables(tables: &ConcreteTypeTables) {
    let _: &ArenaMap<Idx<Expr>, MonoTyId> = &tables.expr_types;
    let _: &ArenaMap<Idx<BindingInfo>, MonoTyId> = &tables.binding_types;
}

fn collect_nested_type_nodes(
    ws: &Workspace,
    inf: &bone_semantic::inference::BodyInference,
    ty: bone_semantic::prelude::TyId,
    out: &mut Vec<bone_semantic::prelude::TyId>,
) {
    let node = match ty {
        bone_semantic::prelude::TyId::Static(mid, idx) => ws.module(mid).ty_arena[idx],
        bone_semantic::prelude::TyId::Local(idx) => inf.ty_arena[idx],
    };
    match node {
        TypeExpr::Nullable(inner) => {
            let child = ty.with_idx(inner);
            out.push(child);
            collect_nested_type_nodes(ws, inf, child, out);
        }
        TypeExpr::App { params, .. } => {
            for param in params {
                let child = ty.with_idx(param);
                out.push(child);
                collect_nested_type_nodes(ws, inf, child, out);
            }
        }
        TypeExpr::Function { params, ret } => {
            for param in params {
                let child = ty.with_idx(param);
                out.push(child);
                collect_nested_type_nodes(ws, inf, child, out);
            }
            let child = ty.with_idx(ret);
            out.push(child);
            collect_nested_type_nodes(ws, inf, child, out);
        }
        TypeExpr::Tuple { elements } => {
            for element in elements {
                let child = ty.with_idx(element);
                out.push(child);
                collect_nested_type_nodes(ws, inf, child, out);
            }
        }
        TypeExpr::Builtin(_) | TypeExpr::Generic(..) | TypeExpr::Infer(..) => {}
    }
}

#[test]
fn concrete_plan_indexes_nested_type_nodes() {
    let (ws, g, bodies) = bodies_of(
        r#"
fn id<T>(x: T) -> T { return x }
fn bump(x: int) -> int { return x + 1 }
fn main() {
    let xs: [[int]] = [[1]]
    let pair: (int, int) = (1, 2)
    let func: fn(int) -> int = bump
    let nullable: int? = id(1)
    let a = id(xs)
    let b = id(pair)
    let c = id(func)
    let d = id(nullable)
    print("{a} {b} {c(1)} {d}")
}
"#,
    );
    let (main, plan) = bodies
        .plans
        .iter()
        .find(|(inst, _)| g.identity(&ws, inst).starts_with("main"))
        .expect("main has no concrete plan");
    let inf = ws
        .inference(main.func.module)
        .and_then(|ir| ir.per_fn.get(&BodyOwner::Function(main.func.func)))
        .expect("main has no inference");
    let mut nested = Vec::new();
    for ty in inf.expr_types.values().chain(inf.binding_types.values()) {
        collect_nested_type_nodes(&ws, inf, *ty, &mut nested);
    }
    let mut unique = Vec::new();
    for ty in nested {
        if !unique.contains(&ty) {
            unique.push(ty);
        }
    }
    assert!(
        !unique.is_empty(),
        "fixture exposed no nested type population"
    );
    for ty in unique {
        assert!(
            plan.types.ty_types.contains_key(&ty),
            "concrete plan omitted nested type node {ty:?}"
        );
    }
    let missing = plan
        .types
        .ty_types
        .keys()
        .next()
        .copied()
        .expect("concrete plan has no type nodes");
    let mut poisoned = (**plan).clone();
    poisoned.types.ty_types.remove(&missing);
    let scx = scalar_ctx(&ws, &g);
    assert!(
        skull_mir::lower::lower_instance_with_plans(&ws, &scx, &poisoned, &bodies.plans).is_err(),
        "lowering ignored a missing concrete type-node entry"
    );
}

/// The first production consumer of the concrete plan must see the complete
/// logical tree. Replacing the plan's child list with the old source-shaped
/// adapter makes this arm fail at the inner application.
#[test]
fn production_lowering_consumes_nested_concrete_type_plan() {
    let (ws, g, bodies) = bodies_of(
        r#"
fn keep<T>(x: T) -> T { return x }
fn main() {
    let xs: [[int]] = []
    let out = keep(xs)
}
"#,
    );
    let inst = bodies
        .bodies
        .keys()
        .find(|i| g.identity(&ws, i).starts_with("keep"))
        .cloned()
        .expect("keep has no concrete instance");
    let plan = bodies.plan_for(&inst).expect("keep has no production plan");
    let nested = plan
        .types
        .expr_types
        .values()
        .chain(plan.types.binding_types.values())
        .map(|id| g.interner().get(*id))
        .find(|ty| matches!(ty, MonoTy::App { params, .. } if matches!(params.first(), Some(MonoTy::App { .. }))));
    assert!(
        nested.is_some(),
        "the concrete plan lost the nested application tree"
    );

    let scx = scalar_ctx(&ws, &g);
    skull_mir::lower::lower_instance_with_plans(&ws, &scx, plan, &bodies.plans)
        .expect("plan-driven lowering failed");

    let mut poisoned = plan.clone();
    poisoned.types.expr_types = ArenaMap::default();
    assert!(
        skull_mir::lower::lower_instance_with_plans(&ws, &scx, &poisoned, &bodies.plans).is_err(),
        "lowering accepted a plan after its concrete expression table was removed"
    );
}

#[test]
fn production_lowering_rejects_a_declared_type_missing_from_the_plan() {
    let (mut ws, g, bodies) = bodies_of(
        r#"
fn main() {
    let value: int = 1
    print("{value}")
}
"#,
    );
    let mid = module_id(&ws, "<test>");
    let (inst, plan) = bodies
        .plans
        .iter()
        .find(|(inst, _)| g.identity(&ws, inst).starts_with("main"))
        .map(|(inst, plan)| (inst.clone(), plan.clone()))
        .expect("main has no concrete plan");
    let extra = ws.modules[mid.index()]
        .ty_arena
        .alloc(TypeExpr::Builtin(bone_semantic::hir::BuiltinTy::Float));
    let body = ws.modules[mid.index()]
        .bodies
        .get_mut(&BodyOwner::Function(inst.func.func))
        .expect("main has no body");
    let binding = body
        .binding_arena
        .iter()
        .find(|(_, binding)| binding.name == Ustr::from("value"))
        .map(|(idx, _)| idx)
        .expect("value binding");
    body.binding_arena[binding].declared_ty =
        Some(bone_semantic::prelude::TyId::Static(mid, extra));

    let scx = scalar_ctx(&ws, &g);
    let err = skull_mir::lower::lower_instance_with_plans(&ws, &scx, &plan, &bodies.plans)
        .expect_err("lowering accepted a declaration type absent from its concrete plan");
    assert!(
        err.message.contains("missing concrete type node"),
        "{err:?}"
    );
}

#[test]
fn production_lowering_rejects_a_signature_type_missing_from_the_plan() {
    let (mut ws, g, bodies) = bodies_of(
        r#"
fn id(value: int) -> int { return value }
fn main() { print("{id(1)}") }
"#,
    );
    let mid = module_id(&ws, "<test>");
    let (inst, plan) = bodies
        .plans
        .iter()
        .find(|(inst, _)| g.identity(&ws, inst).starts_with("id"))
        .map(|(inst, plan)| (inst.clone(), plan.clone()))
        .expect("id has no concrete plan");
    let extra = ws.modules[mid.index()]
        .ty_arena
        .alloc(TypeExpr::Builtin(bone_semantic::hir::BuiltinTy::Float));
    ws.modules[mid.index()].functions[inst.func.func].param_decls[0].ty =
        bone_semantic::prelude::TyId::Static(mid, extra);

    let scx = scalar_ctx(&ws, &g);
    let err = skull_mir::lower::lower_instance_with_plans(&ws, &scx, &plan, &bodies.plans)
        .expect_err("lowering accepted a signature type absent from its concrete plan");
    assert!(
        err.message.contains("missing concrete type node"),
        "{err:?}"
    );
}

#[test]
fn production_plan_marks_unresolved_slots_as_explicitly_untyped() {
    let (ws, g, bodies) = bodies_of(
        r#"
fn main() {
    let value = null
    print("{value}")
}
"#,
    );
    let (inst, plan) = bodies
        .plans
        .iter()
        .find(|(inst, _)| g.identity(&ws, inst).starts_with("main"))
        .expect("main has no concrete plan");
    let body = ws
        .module(inst.func.module)
        .bodies
        .get(&BodyOwner::Function(inst.func.func))
        .unwrap();
    let null = body
        .expr_arena
        .iter()
        .find(|(_, expr)| matches!(expr, Expr::Null))
        .map(|(expr, _)| expr)
        .expect("fixture has no null expression");
    assert!(
        plan.types.is_untyped_expr(null),
        "unresolved null slot was not recorded"
    );

    let scx = scalar_ctx(&ws, &g);
    let mut poisoned = (**plan).clone();
    poisoned.types.untyped_exprs.clear();
    let err = skull_mir::lower::lower_instance_with_plans(&ws, &scx, &poisoned, &bodies.plans)
        .expect_err("lowering accepted an untyped slot after its plan marker was removed");
    assert!(
        err.message.contains("missing concrete type node"),
        "{err:?}"
    );
}

#[test]
fn production_lowering_concrete_struct_literal_coerces_declared_float_fields() {
    let (ws, g, bodies) = bodies_of(
        r#"
nobox value V { x: float }
fn make() -> V { return V { x: 1 } }
fn main() { let v = make(); print("{v.x}") }
"#,
    );
    let make = bodies
        .plans
        .keys()
        .find(|inst| fn_name(&ws, inst.func) == "make")
        .cloned()
        .expect("make has no concrete plan");
    let plan = bodies.plans.get(&make).expect("make plan disappeared");
    let scx = scalar_ctx(&ws, &g);
    let mir = skull_mir::lower::lower_instance_with_plans(&ws, &scx, plan, &bodies.plans)
        .expect("plan-driven struct literal lowering failed");
    assert!(
        count_int_to_float(&mir) > 0,
        "a concrete float field lost the int-to-float coercion"
    );
}

#[test]
fn production_call_abi_requires_the_selected_callee_plan() {
    let (ws, g, bodies) = bodies_of(
        r#"
value V2 { x: int, y: int }
fn id<T>(x: T) -> T { return x }
fn main() {
    let v = id(V2 { x: 1, y: 2 })
    print("{v.x}")
}
"#,
    );
    let main_plan = plan_named(&ws, &bodies, "main");
    let callee = callee_named(&ws, main_plan, "id");
    let scx = scalar_ctx(&ws, &g);
    skull_mir::lower::lower_instance_with_plans(&ws, &scx, main_plan, &bodies.plans)
        .expect("call lowering rejected the intact selected callee plan");
    let mut plans = (*bodies.plans).clone();
    assert!(
        plans.remove(&callee).is_some(),
        "the fixture has no selected id plan"
    );
    assert!(
        skull_mir::lower::lower_instance_with_plans(&ws, &scx, main_plan, &plans).is_err(),
        "call lowering fell back after the selected callee ABI plan was removed"
    );
}

#[test]
fn production_scalar_call_rejects_a_missing_selected_parameter_slot() {
    let (ws, g, bodies) = bodies_of(
        r#"
nobox value V2 { x: float, y: float }
@scalar_abi fn id(v: V2) -> V2 { return v }
fn main() {
    let v = id(V2 { x: 1.0, y: 2.0 })
    print("{v.x}")
}
"#,
    );
    let main_plan = plan_named(&ws, &bodies, "main");
    let callee = callee_named(&ws, main_plan, "id");
    let site = main_plan
        .callees
        .iter()
        .find_map(|(site, inst)| (inst == &callee).then_some(site))
        .expect("id call site disappeared");
    let scx = scalar_ctx(&ws, &g);
    skull_mir::lower::lower_instance_with_plans(&ws, &scx, main_plan, &bodies.plans)
        .expect("the intact scalar call must lower");

    let mut caller = (**main_plan).clone();
    let signature = std::sync::Arc::make_mut(
        caller
            .callee_signatures
            .get_mut(site)
            .expect("id call signature disappeared"),
    );
    signature.params.clear();
    signature.param_tuple_spelled.clear();

    let mut plans = (*bodies.plans).clone();
    let callee_plan = std::sync::Arc::make_mut(
        plans
            .get_mut(&callee)
            .expect("selected callee plan disappeared"),
    );
    let signature = std::sync::Arc::make_mut(&mut callee_plan.signature);
    signature.params.clear();
    signature.param_tuple_spelled.clear();

    let poisoned_scx = scalar_ctx(&ws, &g);
    let err = skull_mir::lower::lower_instance_with_plans(&ws, &poisoned_scx, &caller, &plans)
        .expect_err("a missing scalar ABI slot must reject instead of boxing");
    assert!(
        err.message.contains("no ABI slot"),
        "unexpected missing-slot diagnostic: {err:?}"
    );
}

#[test]
fn production_ufcs_call_rejects_a_missing_receiver_slot() {
    let (ws, g, bodies) = bodies_of(
        r#"
nobox value V2 { x: float, y: float }
@scalar_abi fn id(v: V2) -> V2 { return v }
fn main() {
    let v: V2? = V2 { x: 1.0, y: 2.0 }
    let out = v?.id()
    print("{out?.x ?? 0.0}")
}
"#,
    );
    let main_plan = plan_named(&ws, &bodies, "main");
    let callee = callee_named(&ws, main_plan, "id");
    let site = main_plan
        .callees
        .iter()
        .find_map(|(site, inst)| (inst == &callee).then_some(site))
        .expect("id UFCS call site disappeared");
    let intact_scx = scalar_ctx(&ws, &g);
    skull_mir::lower::lower_instance_with_plans(&ws, &intact_scx, main_plan, &bodies.plans)
        .expect("the intact safe-nav UFCS call must lower");
    let mut caller = (**main_plan).clone();
    let signature = std::sync::Arc::make_mut(
        caller
            .callee_signatures
            .get_mut(site)
            .expect("id UFCS call signature disappeared"),
    );
    signature.params.clear();
    signature.param_tuple_spelled.clear();

    let mut plans = (*bodies.plans).clone();
    let callee_plan = std::sync::Arc::make_mut(
        plans
            .get_mut(&callee)
            .expect("selected UFCS callee plan disappeared"),
    );
    let signature = std::sync::Arc::make_mut(&mut callee_plan.signature);
    signature.params.clear();
    signature.param_tuple_spelled.clear();

    let scx = scalar_ctx(&ws, &g);
    let err = skull_mir::lower::lower_instance_with_plans(&ws, &scx, &caller, &plans)
        .expect_err("a missing UFCS receiver slot must reject instead of becoming opaque");
    assert!(
        err.message.contains("UFCS receiver has no ABI slot 0"),
        "unexpected missing-receiver diagnostic: {err:?}"
    );
    assert!(
        !err.labels.is_empty(),
        "missing receiver diagnostic lost its source span"
    );
}

#[test]
fn production_call_identity_consumes_the_plan_site_map() {
    let (ws, g, bodies) = bodies_of(
        r#"
fn id<T>(x: T) -> T { return x }
fn main() {
    let out = id(1)
    print("{out}")
}
"#,
    );
    let main_plan = bodies
        .plans
        .iter()
        .find(|(inst, _)| g.identity(&ws, inst).starts_with("main"))
        .map(|(_, plan)| plan)
        .expect("main has no concrete plan");
    assert!(
        !main_plan.callees.is_empty(),
        "fixture has no concrete site population"
    );
    let mut poisoned = (**main_plan).clone();
    poisoned.callees.clear();
    let scx = scalar_ctx(&ws, &g);
    assert!(
        skull_mir::lower::lower_instance_with_plans(&ws, &scx, &poisoned, &bodies.plans).is_err(),
        "lowering reconstructed a callee identity after its plan site was removed"
    );
}

#[test]
fn production_variadic_direct_and_ufcs_reuse_the_tail_slot() {
    let (ws, g, bodies) = bodies_of(
        r#"
nobox value V2 { x: int, y: int }
@scalar_abi fn variadic(receiver: V2, first: int, args: ...any) -> int { return receiver.x }
fn main() {
    let v = V2 { x: 11, y: 12 }
    let direct = variadic(v, 1, 2, 3)
    let ufcs = v.variadic(1, 2, 3)
    print("{direct} {ufcs}")
}
"#,
    );
    let main_plan = plan_named(&ws, &bodies, "main");
    let callee = main_plan
        .callees
        .values()
        .find(|inst| fn_name(&ws, inst.func) == "variadic")
        .expect("main has no variadic callee");
    let variadic_sites: Vec<_> = main_plan
        .callees
        .iter()
        .filter(|(_, inst)| *inst == callee)
        .collect();
    assert_eq!(
        variadic_sites.len(),
        2,
        "fixture lost its direct or UFCS call"
    );
    for (site, _) in variadic_sites {
        assert_eq!(
            main_plan
                .callee_signatures
                .get(site)
                .expect("variadic site signature")
                .params
                .len(),
            3,
            "fixture did not leave surplus arguments to the declared tail slot"
        );
    }
    let scx = scalar_ctx(&ws, &g);
    skull_mir::lower::lower_instance_with_plans(&ws, &scx, main_plan, &bodies.plans)
        .expect("direct and UFCS variadic calls did not lower");
}

#[test]
fn production_lambda_parameter_slot_loss_reports_a_spanned_gap() {
    let (mut ws, g, bodies) = bodies_of(
        r#"
fn main() {
    let f = (x: int) => x
    print("{f(1)}")
}
"#,
    );
    let (main, plan) = bodies
        .plans
        .iter()
        .find(|(inst, _)| fn_name(&ws, inst.func) == "main")
        .map(|(inst, plan)| (inst.clone(), plan.clone()))
        .expect("main has no concrete plan");
    let mid = main.func.module;
    let body = ws.modules[mid.index()]
        .bodies
        .get_mut(&BodyOwner::Function(main.func.func))
        .expect("main has no body");
    let (lambda, params, lambda_body) = body
        .expr_arena
        .iter()
        .find_map(|(expr, node)| match node {
            Expr::Lambda { params, body } => Some((expr, *params, *body)),
            _ => None,
        })
        .expect("fixture has no lambda");
    assert_eq!(
        params.len(),
        1,
        "fixture did not expose one lambda parameter"
    );
    let extra = body
        .binding_arena
        .iter()
        .map(|(binding, _)| binding)
        .find(|binding| *binding == params.end())
        .expect("fixture has no adjacent binding to poison the lambda range");
    body.expr_arena[lambda] = Expr::Lambda {
        params: IdxRange::new_inclusive(params.start()..=extra),
        body: lambda_body,
    };

    let scx = scalar_ctx(&ws, &g);
    let err = skull_mir::lower::lower_instance_with_plans(&ws, &scx, &plan, &bodies.plans)
        .expect_err("lambda lowering accepted a missing ABI parameter slot");
    assert!(
        err.message.contains("lambda ABI has no parameter slot 1"),
        "unexpected lambda missing-slot diagnostic: {err:?}"
    );
    assert!(
        !err.labels.is_empty(),
        "lambda missing-slot diagnostic lost its source span"
    );
}

fn count_int_to_float(mir: &MirFunc) -> usize {
    mir.insts
        .values()
        .filter(|inst| matches!(inst, Inst::IntToFloat(_)))
        .count()
        + mir.nested_fns.iter().map(count_int_to_float).sum::<usize>()
}

#[test]
fn production_call_coercion_consumes_the_selected_callee_signature() {
    let (ws, g, bodies) = bodies_of(
        r#"
fn id<T>(x: T) -> T { return x }
fn main() {
    let out = id(1)
    print("{out}")
}
"#,
    );
    let main_plan = bodies
        .plans
        .iter()
        .find(|(inst, _)| g.identity(&ws, inst).starts_with("main"))
        .map(|(_, plan)| plan)
        .expect("main has no concrete plan");
    let callee = callee_named(&ws, main_plan, "id");
    let callee_plan = bodies.plans.get(&callee).expect("id has no concrete plan");
    assert_eq!(
        callee_plan.signature.params.len(),
        1,
        "callee signature has no parameter population"
    );
    let float = g.interner().intern(MonoTy::Float);
    let site = main_plan
        .callees
        .iter()
        .find_map(|(site, inst)| (inst == &callee).then_some(site))
        .expect("id call site disappeared");
    let mut caller = (**main_plan).clone();
    std::sync::Arc::make_mut(
        caller
            .callee_signatures
            .get_mut(site)
            .expect("id call signature disappeared"),
    )
    .params[0] = float;
    let mut plans = (*bodies.plans).clone();
    let callee_plan = std::sync::Arc::make_mut(
        plans
            .get_mut(&callee)
            .expect("selected callee plan disappeared"),
    );
    std::sync::Arc::make_mut(&mut callee_plan.signature).params[0] = float;
    let scx = scalar_ctx(&ws, &g);
    let mir = skull_mir::lower::lower_instance_with_plans(&ws, &scx, &caller, &plans)
        .expect("plan-driven call lowering failed");
    assert!(
        count_int_to_float(&mir) > 0,
        "call lowering ignored the selected callee's concrete parameter signature"
    );
}

#[test]
fn production_call_rejects_a_callee_signature_body_disagreement() {
    let (ws, g, bodies) = bodies_of(
        r#"
fn id<T>(x: T) -> T { return x }
fn main() {
    let out = id(1)
    print("{out}")
}
"#,
    );
    let main_plan = bodies
        .plans
        .iter()
        .find(|(inst, _)| g.identity(&ws, inst).starts_with("main"))
        .map(|(_, plan)| plan)
        .expect("main has no concrete plan");
    let callee = callee_named(&ws, main_plan, "id");
    let mut plans = (*bodies.plans).clone();
    let callee_plan = std::sync::Arc::make_mut(
        plans
            .get_mut(&callee)
            .expect("selected callee plan disappeared"),
    );
    std::sync::Arc::make_mut(&mut callee_plan.signature).params[0] =
        g.interner().intern(MonoTy::Float);
    let scx = scalar_ctx(&ws, &g);
    let err = skull_mir::lower::lower_instance_with_plans(&ws, &scx, main_plan, &plans)
        .expect_err("lowering accepted a mismatched callee body plan");
    assert!(
        err.message
            .contains("concrete callee signature disagrees with its body plan"),
        "unexpected mismatch diagnostic: {err:?}"
    );
}

#[test]
fn production_return_coercion_consumes_the_body_signature() {
    let (ws, g, bodies) = bodies_of(
        r#"
fn id<T>(x: T) -> T { return x }
fn main() {
    let out = id(1)
    print("{out}")
}
"#,
    );
    let callee = bodies
        .plans
        .keys()
        .find(|inst| fn_name(&ws, inst.func) == "id")
        .cloned()
        .expect("id has no concrete plan");
    let mut plan = (**bodies.plans.get(&callee).expect("id plan disappeared")).clone();
    assert!(
        plan.signature.result.is_some(),
        "body signature has no result population"
    );
    std::sync::Arc::make_mut(&mut plan.signature).result = Some(g.interner().intern(MonoTy::Float));
    let scx = scalar_ctx(&ws, &g);
    let mir = skull_mir::lower::lower_instance_with_plans(&ws, &scx, &plan, &bodies.plans)
        .expect("plan-driven return lowering failed");
    assert!(
        count_int_to_float(&mir) > 0,
        "return lowering ignored the body's concrete result signature"
    );
}

/// Each owner's plan keeps exactly the foreign domains it counted as unique,
/// so the unique total is the sum of the published per-owner tables. An
/// owner's own-body sites are requests but never foreign tables.
#[test]
fn foreign_domain_unique_counts_only_published_foreign_tables() {
    let (_ws, _g, bodies) = bodies_of(
        r#"
fn seed<T>() -> T? { return null }
struct Box<T> { value: T? = seed() }
fn twice(x: int) -> int { return x + x }
fn main() {
    let a: Box<int> = Box {}
    let b: Box<string> = Box {}
    let c: Box<int> = Box {}
    print("{a.value} {b.value} {c.value} {twice(1)} {twice(2)}")
}
"#,
    );
    let census = bodies.census();
    let published: usize = bodies
        .plans
        .values()
        .map(|plan| plan.foreign_domains.len())
        .chain(
            bodies
                .always_plans
                .values()
                .map(|plan| plan.foreign_domains.len()),
        )
        .sum();
    assert_eq!(
        census.foreign_domain_unique, published,
        "foreign unique must count published struct-default tables only: {census:?}"
    );
    assert!(
        census.foreign_domain_requests > census.foreign_domain_unique,
        "own-body sites are requests on top of the foreign tables: {census:?}"
    );
}

#[test]
fn production_plan_keeps_distinct_struct_default_domains() {
    let (ws, g, bodies) = bodies_of(
        r#"
fn seed<T>() -> T? { return null }
struct Box<T> { value: T? = seed() }
fn main() {
    let a: Box<int> = Box {}
    let b: Box<string> = Box {}
    print("{a.value} {b.value}")
}
"#,
    );
    let census = bodies.census();
    assert!(
        census.type_nodes_examined > 0,
        "the type-node population is empty: {census:?}"
    );
    assert!(
        census.type_node_conversion_attempts > 0,
        "the type-node conversion population is empty: {census:?}"
    );
    assert!(
        census.type_node_memo_hits > 0,
        "the body conversion memo did not reach the type-table pass: {census:?}"
    );
    assert!(
        census.type_node_conversion_attempts < census.type_nodes_examined,
        "the type-table pass re-converted every node: {census:?}"
    );
    assert!(
        census.struct_literals_examined > 0 && census.struct_field_conversion_attempts > 0,
        "the struct-field population is empty: {census:?}"
    );
    assert!(
        census.foreign_domain_requests >= census.foreign_domain_unique
            && census.foreign_domain_unique > 0,
        "the foreign-domain denominator is empty or inconsistent: {census:?}"
    );
    let main = bodies
        .plans
        .iter()
        .find(|(inst, _)| g.identity(&ws, inst).starts_with("main"))
        .map(|(_, plan)| plan)
        .expect("main has no concrete plan");
    assert_eq!(
        main.foreign_domains.len(),
        2,
        "expected both Box instantiation domains"
    );
    let mut rendered = main
        .foreign_domains
        .values()
        .flat_map(|tables| tables.expr_types.values())
        .map(|id| g.interner().render(*id))
        .collect::<Vec<_>>();
    rendered.sort();
    assert!(
        rendered.iter().any(|ty| ty == "int?"),
        "missing int default table: {rendered:?}"
    );
    assert!(
        rendered.iter().any(|ty| ty == "string?"),
        "missing string default table: {rendered:?}"
    );

    let mut poisoned = (**main).clone();
    let domain = poisoned
        .foreign_domains
        .keys()
        .next()
        .cloned()
        .expect("domain");
    poisoned.foreign_domains.remove(&domain);
    let scx = scalar_ctx(&ws, &g);
    assert!(
        skull_mir::lower::lower_instance_with_plans(&ws, &scx, &poisoned, &bodies.plans).is_err(),
        "lowering accepted a plan after its struct-default domain was removed"
    );
}

#[test]
fn production_plan_keeps_struct_default_domains_without_callees() {
    let (ws, g, bodies) = bodies_of(
        r#"
struct Box<T> { value: T? = null }
fn main() {
    let a: Box<int> = Box {}
    let b: Box<string> = Box {}
    print("{a.value} {b.value}")
}
"#,
    );
    let main = bodies
        .plans
        .iter()
        .find(|(inst, _)| g.identity(&ws, inst).starts_with("main"))
        .map(|(_, plan)| plan)
        .expect("main has no concrete plan");
    assert_eq!(
        main.foreign_domains.len(),
        2,
        "a default body without calls still needs both concrete domains"
    );
    let mut rendered = main
        .foreign_domains
        .values()
        .flat_map(|tables| tables.expr_types.values())
        .map(|id| g.interner().render(*id))
        .collect::<Vec<_>>();
    rendered.sort();
    assert!(
        rendered.iter().any(|ty| ty == "int?"),
        "missing int default table: {rendered:?}"
    );
    assert!(
        rendered.iter().any(|ty| ty == "string?"),
        "missing string default table: {rendered:?}"
    );
    let domain = main
        .foreign_domains
        .keys()
        .next()
        .cloned()
        .expect("default domain");
    let mut poisoned = (**main).clone();
    poisoned.foreign_domains.remove(&domain);
    let scx = scalar_ctx(&ws, &g);
    assert!(
        skull_mir::lower::lower_instance_with_plans(&ws, &scx, &poisoned, &bodies.plans).is_err(),
        "lowering accepted a struct default after its concrete domain was removed"
    );
}

#[test]
fn production_plan_covers_an_always_emitted_global() {
    let (ws, g, bodies) = bodies_of(
        r#"
global startup: int = read_startup()
fn read_startup() -> int { return 7 }
fn main() { print("{startup}") }
"#,
    );
    let user_mid = module_id(&ws, "<test>");
    let (owner, plan) = bodies
        .always_plans
        .iter()
        .find(|(owner, _)| {
            matches!(
                owner,
                skull_mir::concrete::ConcreteOwner::Global { module, .. } if *module == user_mid
            )
        })
        .expect("global has no concrete plan");
    assert!(
        plan.signature.result.is_some(),
        "global plan lost its concrete result type"
    );
    assert_eq!(
        plan.callees.len(),
        1,
        "always-emitted plan lost its direct call population"
    );
    let scx = scalar_ctx(&ws, &g);
    skull_mir::lower::lower_global_with_plans(
        &ws,
        &scx,
        plan,
        &bodies.plans,
        Ustr::from("__test_ginit"),
    )
    .expect("plan-driven global lowering failed");
    let mut poisoned = plan.clone();
    poisoned.types.expr_types = ArenaMap::default();
    assert!(
        skull_mir::lower::lower_global_with_plans(
            &ws,
            &scx,
            &poisoned,
            &bodies.plans,
            Ustr::from("__test_ginit"),
        )
        .is_err(),
        "global lowering ignored its concrete type plan"
    );
    let mut missing_callee = plan.clone();
    missing_callee.callees.clear();
    assert!(
        skull_mir::lower::lower_global_with_plans(
            &ws,
            &scx,
            &missing_callee,
            &bodies.plans,
            Ustr::from("__test_ginit"),
        )
        .is_err(),
        "global lowering ignored its concrete callee plan"
    );
    let call_site = plan.callees.keys().next().expect("global call site");
    let callee = plan.callees[call_site].clone();
    let mut mismatched_plans = (*bodies.plans).clone();
    let callee_plan = std::sync::Arc::make_mut(
        mismatched_plans
            .get_mut(&callee)
            .expect("global callee plan disappeared"),
    );
    std::sync::Arc::make_mut(&mut callee_plan.signature).result =
        Some(g.interner().intern(MonoTy::Float));
    let err = skull_mir::lower::lower_global_with_plans(
        &ws,
        &scx,
        plan,
        &mismatched_plans,
        Ustr::from("__test_ginit"),
    )
    .expect_err("global lowering accepted a mismatched callee body plan");
    assert!(
        err.message
            .contains("concrete callee signature disagrees with its body plan"),
        "unexpected always-emitted mismatch diagnostic: {err:?}"
    );
    let _ = owner;
}

#[test]
fn always_emitted_global_publishes_struct_default_domains_without_callees() {
    let (ws, g, bodies) = bodies_of(
        r#"
struct Box<T> { value: T? = null }
global startup: Box<int> = Box {}
fn main() { let value = startup.value }
"#,
    );
    let default_domain_population = g
        .contributions
        .iter()
        .filter(|(owner, contribution)| {
            matches!(owner, skull::instance_graph::Owner::AlwaysEmitted(..))
                && !contribution.struct_default_domains.is_empty()
        })
        .count();
    assert!(
        default_domain_population > 0,
        "fixture did not put a struct-default domain on an always-emitted contribution"
    );
    let user_mid = module_id(&ws, "<test>");
    let global = bodies
        .always_plans
        .iter()
        .find(|(owner, _)| {
            matches!(
                owner,
                skull_mir::concrete::ConcreteOwner::Global { module, .. } if *module == user_mid
            )
        })
        .map(|(_, plan)| plan)
        .expect("the global has no always-emitted plan");
    assert_eq!(
        global.foreign_domains.len(),
        1,
        "the always-emitted global lost its struct-default domain"
    );
}

#[test]
fn production_decorator_lowering_keeps_its_owned_abi_plan() {
    let (ws, g, bodies) = bodies_of(
        r#"
deco init register(priority: int = 100) {
    print("{target.name}:{priority}")
}
@register
fn system() {}
fn main() {}
"#,
    );
    let user_mid = module_id(&ws, "<test>");
    let plan = bodies
        .always_plans
        .iter()
        .find(|(owner, _)| {
            matches!(
                owner,
                skull_mir::concrete::ConcreteOwner::Decorator { module, .. } if *module == user_mid
            )
        })
        .map(|(_, plan)| plan)
        .expect("decorator has no concrete plan");
    assert!(
        plan.signature.params.len() >= 2,
        "fixture did not publish target and decorator parameter slots"
    );
    let scx = scalar_ctx(&ws, &g);
    let mir = skull_mir::lower::lower_decorator_with_plans(
        &ws,
        &scx,
        plan,
        &bodies.plans,
        Ustr::from("__test_decorator"),
    )
    .expect("plan-driven decorator lowering failed");
    let abi = mir
        .abi_representation
        .as_ref()
        .expect("decorator lost its owned ABI representation");
    assert!(
        mir.rep_arena.is_some(),
        "decorator lost its representation arena"
    );
    assert_eq!(abi.params().len(), plan.signature.params.len());
    assert_eq!(mir.params.len(), plan.signature.params.len());
    assert!(abi.result().is_some(), "decorator lost its ABI result slot");
}

fn return_arity(mir: &MirFunc) -> usize {
    mir.blocks
        .values()
        .find_map(|block| match &block.term {
            Term::Return(values) => Some(values.len()),
            _ => None,
        })
        .expect("lowered body has no return")
}

#[test]
fn production_global_result_shape_consumes_the_concrete_signature() {
    let (ws, g, bodies) = bodies_of(
        r#"
nobox value V { x: float, y: float }
global G: V = V { x: 1.0, y: 2.0 }
fn main() { print("{G.x}") }
"#,
    );
    let user_mid = module_id(&ws, "<test>");
    let (owner, plan) = bodies
        .always_plans
        .iter()
        .find(|(owner, _)| {
            matches!(
                owner,
                skull_mir::concrete::ConcreteOwner::Global { module, .. } if *module == user_mid
            )
        })
        .expect("global has no concrete plan");
    let scx = scalar_ctx(&ws, &g);
    let intact = skull_mir::lower::lower_global_with_plans(
        &ws,
        &scx,
        plan,
        &bodies.plans,
        Ustr::from("__test_ginit"),
    )
    .expect("intact global plan failed")
    .expect("global initializer disappeared");
    let global_rep = intact
        .global_plan
        .as_ref()
        .expect("global initializer lost its owned plan");
    assert_eq!(
        global_rep.lanes(),
        2,
        "the global plan must own V's two physical lanes"
    );
    assert_eq!(
        return_arity(&intact),
        2,
        "fixture has no multi-leaf global result"
    );

    let mut poisoned = plan.clone();
    std::sync::Arc::make_mut(&mut poisoned.signature).result =
        Some(g.interner().intern(MonoTy::Float));
    let lowered = skull_mir::lower::lower_global_with_plans(
        &ws,
        &scx,
        &poisoned,
        &bodies.plans,
        Ustr::from("__test_ginit"),
    )
    .expect("poisoned global plan failed")
    .expect("poisoned global initializer disappeared");
    assert_eq!(
        return_arity(&lowered),
        1,
        "global lowering reconstructed its declared result shape"
    );
    let _ = owner;
}

#[test]
fn always_emitted_plan_retains_a_failed_callee_site() {
    let ws = ws_with_stdlib(
        r#"
fn pick<T>() -> int {
    let unused: T = 1
    return 1
}
global G: int = pick()
fn main() { let x = G }
"#,
    );
    let user_mid = module_id(&ws, "<test>");
    let roots = skull::mir_emit::emission_roots(&ws, Some("<project>"));
    let g = instance_graph::build(&ws, roots, false);
    let mut bodies = concrete_body::build(&ws, &g);
    bodies.refresh_flat_views();
    let (owner, plan) = bodies
        .always_plans
        .iter()
        .find(|(owner, _)| {
            matches!(
                owner,
                skull_mir::concrete::ConcreteOwner::Global { module, .. } if *module == user_mid
            )
        })
        .expect("global has no concrete plan");
    assert_eq!(
        plan.callees.len(),
        0,
        "failed call was fabricated as resolved"
    );
    assert_eq!(
        plan.failed_sites.len(),
        1,
        "failed site was dropped from the plan"
    );
    assert_eq!(
        plan.failed_sites.values().next(),
        Some(&"unconstrained-type-var")
    );
    let scx = scalar_ctx(&ws, &g);
    let err = skull_mir::lower::lower_global_with_plans(
        &ws,
        &scx,
        plan,
        &bodies.plans,
        Ustr::from("__test_ginit"),
    )
    .expect_err("lowering ignored the graph's failed always-emitted site");
    assert!(
        err.message.contains("unresolved callee site"),
        "failed site lost its lowering diagnostic: {err:?}"
    );
    let _ = owner;
}

/// Every instance the walk EXPANDED gets a body, and every one of its slots is
/// concrete. The denominator is stated: zero free types over zero examined
/// slots would be no evidence at all.
#[test]
fn every_expanded_instance_has_a_fully_concrete_body() {
    let (ws, g, bodies) = bodies_of(
        r#"
fn ident<T>(x: T) -> T { return x }
fn main() {
    let a = ident(1)
    let b = ident("s")
}
"#,
    );
    let c = bodies.census();
    assert!(
        c.expr_examined > 0,
        "no expression slots were converted: {c:?}"
    );
    assert!(
        c.binding_examined > 0,
        "no binding slots were converted: {c:?}"
    );
    assert_eq!(
        bodies.free_types.len(),
        0,
        "free types in supposedly concrete bodies, over {} expr and {} binding slots: {:?}",
        c.expr_examined,
        c.binding_examined,
        bodies
            .render_free_types(&ws, &g)
            .iter()
            .take(10)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        bodies.gaps.len(),
        0,
        "expanded instances with no body: {:?}",
        c.gaps
    );
    // The zero above is only as strong as the space it was measured over: a
    // body that never consults its substitution reports no free types for the
    // trivial reason. These say the substitution was actually load-bearing.
    assert!(
        c.generic_bodies > 0,
        "no body was built under a non-empty substitution: {c:?}"
    );
    assert!(
        c.generics_bound > 0,
        "no slot resolved a declared generic through the substitution, so zero \
         free types says nothing: {c:?}"
    );
    // Every expanded instance, not "most": a missing table is a hole in the
    // Stage-3 cutover, and the walk already RECORDS which bodies it entered.
    for i in g.expanded() {
        assert!(
            bodies.bodies.contains_key(i),
            "expanded instance `{}` has no ConcreteBody",
            g.identity(&ws, i)
        );
    }
}

/// The tables are per-INSTANCE. `ident<int>` and `ident<string>` share one HIR
/// body, so the same `Idx<Expr>` must map to two different `MonoTyId`s -- if the
/// tables were built once per FUNCTION this is the assertion that fails.
#[test]
fn one_generic_body_gets_two_disagreeing_tables() {
    let (ws, g, bodies) = bodies_of(
        r#"
fn ident<T>(x: T) -> T { return x }
fn main() {
    let a = ident(1)
    let b = ident("s")
}
"#,
    );
    let insts: Vec<&skull::instance_graph::Instance> = bodies
        .bodies
        .keys()
        .filter(|i| g.identity(&ws, i).starts_with("ident"))
        .collect();
    assert_eq!(
        insts.len(),
        2,
        "expected two ident instances, got {insts:?}"
    );
    let (a, b) = (&bodies.plans[insts[0]].types, &bodies.plans[insts[1]].types);
    let shared: Vec<_> = a
        .expr_types
        .iter()
        .filter_map(|(e, ty)| b.expr_types.get(e).map(|other| (e, *ty, *other)))
        .collect();
    assert!(!shared.is_empty(), "the two tables share no slot at all");
    let differing: Vec<_> = shared.iter().filter(|(_, x, y)| x != y).collect();
    assert!(
        !differing.is_empty(),
        "both instances of one generic body agree on every slot, so the tables are \
         not instance-specific: {shared:?}"
    );
    // Named, not merely different: the slots carry `int` on one side and
    // `string` on the other, which is what the substitution was supposed to
    // supply.
    let rendered: Vec<(String, String)> = differing
        .iter()
        .map(|(_, x, y)| (g.interner().render(*x), g.interner().render(*y)))
        .collect();
    assert!(
        rendered
            .iter()
            .any(|(x, y)| (x == "int" && y == "string") || (x == "string" && y == "int")),
        "the disagreement is not the instantiation: {rendered:?}"
    );
}

/// A non-generic instance's substitution is EMPTY, not absent, so its body is
/// built like any other. Without this the tables would exist only for generics,
/// and `if generic` would be back.
#[test]
fn a_non_generic_instance_gets_a_body_too() {
    let (ws, g, bodies) = bodies_of(
        r#"
fn plain(x: int) -> int { return x + 1 }
fn main() { let a = plain(1) }
"#,
    );
    let found = bodies
        .plans
        .iter()
        .find(|(i, _)| g.identity(&ws, i).starts_with("plain"))
        .map(|(_, plan)| &plan.types)
        .expect("`plain` has no ConcreteBody");
    assert_arena_tables(found);
    assert!(
        found.expr_types.values().next().is_some(),
        "`plain`'s table is empty"
    );
    assert_eq!(bodies.free_types.len(), 0);
}

#[test]
fn stale_inference_cannot_build_a_concrete_body() {
    let mut ws = ws_with_stdlib(
        r#"
fn plain(x: int) -> int { return x + 1 }
fn main() { let a = plain(1) }
"#,
    );
    let g = graph_of(&ws);
    let inst = expanded_with_prefix(&ws, &g, "plain");
    let owner = bone_semantic::hir::BodyOwner::Function(inst.func.func);
    ws.module_mut(inst.func.module)
        .bodies
        .get_mut(&owner)
        .expect("plain has no body")
        .generation += 1;

    let stale_roots = skull::mir_emit::emission_roots(&ws, None);
    let stale_graph = instance_graph::build(&ws, stale_roots, false);
    assert!(
        stale_graph.expanded().all(|i| i.func != inst.func),
        "the instance graph expanded a body from stale inference"
    );

    let mut bodies = concrete_body::build(&ws, &g);
    bodies.refresh_flat_views();

    assert!(
        !bodies.bodies.contains_key(&inst),
        "stale inference produced a ConcreteBody"
    );
    assert!(
        bodies
            .gaps
            .iter()
            .any(|gap| gap.instance == inst && gap.gap.label() == "stale-inference"),
        "stale inference was not classified: {:?}",
        bodies.census().gaps
    );
}

/// The failure arm fires, and says WHERE. Perturbing only the CONSUMER's
/// substitution -- the table builder's, never the walk's -- leaves a generic
/// body's slots unbound, which is exactly the condition the corpus census
/// reports zero of. A symmetric perturbation could not distinguish this arm
/// from a dead one.
#[test]
fn an_empty_substitution_leaves_the_generic_slots_unbound() {
    let src = r#"
fn ident<T>(x: T) -> T { return x }
fn main() { let a = ident(1) }
"#;
    let ws = ws_with_stdlib(src);
    let g = graph_of(&ws);
    let inst = expanded_with_prefix(&ws, &g, "ident");

    let module = ws.module(inst.func.module);
    let inf = ws
        .inference(inst.func.module)
        .and_then(|ir| {
            ir.per_fn
                .get(&bone_semantic::hir::BodyOwner::Function(inst.func.func))
        })
        .expect("ident has no inference");
    let body = module
        .bodies
        .get(&bone_semantic::hir::BodyOwner::Function(inst.func.func))
        .expect("ident has no body");

    let subst = g
        .substitution(&inst)
        .expect("ident instance kept no substitution")
        .clone();
    let with = concrete_body::convert_body(g.interner(), &ws, inf, body, &subst);
    assert_eq!(
        with.failures.len(),
        0,
        "the real substitution left slots unbound: {:?}",
        with.failures
    );
    assert!(with.examined() > 0);

    let without = concrete_body::convert_body(g.interner(), &ws, inf, body, &Default::default());
    assert!(
        !without.failures.is_empty(),
        "an EMPTY substitution converted every slot of a generic body, so the \
         substitution is not what makes the table concrete"
    );
    assert!(
        without
            .failures
            .iter()
            .any(|f| matches!(f.reason, MonoTyReason::UnboundGeneric(_))),
        "expected an unbound generic, got {:?}",
        without
            .failures
            .iter()
            .map(|f| f.reason.bucket())
            .collect::<Vec<_>>()
    );
    // A failure with no location cannot become a compile error at the cutover.
    for f in &without.failures {
        assert!(
            !instance_graph::is_spanless(&f.span),
            "a free-type failure at {:?} carries no span",
            f.slot
        );
    }
    assert!(
        without
            .failures
            .iter()
            .any(|f| matches!(f.slot, Slot::Expr(_))),
        "no expression slot failed, so only bindings were examined"
    );
}

/// SENTINEL -- pins the slots `ConcreteBody` cannot state a type for.
///
/// The tables are total over what inference RECORDED, and four expression kinds
/// get no `expr_types` entry at all, so they are invisible to the free-type
/// assertion rather than concrete. Corpus: 3,446 of 1,136,034 expression slots,
/// entirely `Range` / `Var` / `IfLet` / `Call`.
///
/// Each has a plausible reason to be absent -- a numeric-`for` range names no
/// Rock type, an `if let` in statement position produces no value, a `for`
/// header's call is recorded through `for_iter_protocol`, and an assignment
/// target is a place whose type the BINDING table already carries (0 untyped
/// bindings corpus-wide). Plausible is not measured: the callee-table slice
/// lowers exactly these forms and has to either consume the type from the table
/// or say which oracle answers instead.
///
/// INVERTED: it passes today BECAUSE the slots are untyped, and asserts exactly
/// that. When bone starts recording
/// a type for one of these, this goes red -- drop that kind from `PINNED`, and
/// check whether `ConcreteBody` should now be asserting concreteness for it.
#[test]
fn untyped_expression_slots_are_pinned() {
    const PINNED: [&str; 4] = ["Call", "IfLet", "Range", "Var"];
    let (ws, g, bodies) = bodies_of(
        r#"
fn body(v: [int]) -> int {
    let total = 0
    for i in 0..3 { total = total + i }
    for (i, x) in enumerate(v) { total = total + i + x }
    let m: int? = 1
    if let y = m { total = total + y }
    // An ANNOTATED binding: assigning to an inferred one records a type for the
    // target, so the annotation is the discriminant, not the assignment.
    let annotated: int = 0
    if v.len() > 0 { annotated = 1 } else { annotated = 2 }
    let inferred = 0
    if v.len() > 0 { inferred = 1 } else { inferred = 2 }
    return total + annotated + inferred
}
fn main() { let a = body([1, 2]) }
"#,
    );
    let mut kinds: std::collections::BTreeSet<&str> = Default::default();
    for (i, u) in &bodies.untyped {
        if g.identity(&ws, i).starts_with("body") {
            kinds.insert(u.kind);
        }
    }
    assert_eq!(
        kinds.iter().copied().collect::<Vec<_>>(),
        PINNED,
        "the untyped-slot population moved"
    );
    // The fixture can distinguish: its ordinary expressions ARE typed, so the
    // four above are a property of those kinds and not of the whole body.
    let body_inst = bodies
        .bodies
        .keys()
        .find(|i| g.identity(&ws, i).starts_with("body"))
        .expect("`body` has no ConcreteBody");
    let hir_body = ws
        .module(body_inst.func.module)
        .bodies
        .get(&bone_semantic::hir::BodyOwner::Function(
            body_inst.func.func,
        ))
        .expect("`body` has no HIR body");
    let mut annotated_targets = Vec::new();
    let mut inferred_targets = Vec::new();
    for (_, expr) in hir_body.expr_arena.iter() {
        let Expr::Assign { target, .. } = expr else {
            continue;
        };
        let Expr::Var(binding) = hir_body.expr_arena[*target] else {
            continue;
        };
        match hir_body.binding_arena[binding].name.as_str() {
            "annotated" => annotated_targets.push(*target),
            "inferred" => inferred_targets.push(*target),
            _ => {}
        }
    }
    assert_eq!(
        annotated_targets.len(),
        2,
        "fixture has the wrong annotated targets"
    );
    assert_eq!(
        inferred_targets.len(),
        2,
        "fixture has the wrong inferred targets"
    );
    let body_types = &bodies.plans[body_inst].types;
    assert!(
        annotated_targets
            .iter()
            .all(|target| !body_types.expr_types.contains_idx(*target)),
        "annotated assignment targets are no longer untyped; flip this sentinel"
    );
    assert!(
        inferred_targets
            .iter()
            .all(|target| body_types.expr_types.contains_idx(*target)),
        "the inferred assignment control became untyped"
    );
    assert!(
        body_types.expr_types.values().count() > 20,
        "almost nothing in `body` was typed, so the pinned kinds are not what is special"
    );
    assert_eq!(bodies.free_types.len(), 0);
}

// ---------------------------------------------------------------------------
// Callee tables (plan S 9): `direct_calls`, `fn_values`, `iterator_ops`,
// `synthesized_calls`, and the `Callee::{Rock, Boundary}` split.
//
// The property is not "a callee was found". It is that EVERY operation the walk
// resolved for an instance becomes exactly one of an entry, a located hole, or a
// located invalid-boundary rejection -- and that the entries name the walk's own
// instances, so no consumer downstream needs a substitution to read them.
// ---------------------------------------------------------------------------

/// A fixture exercising all four tables at once: direct and generic calls, a
/// function value, a `for` protocol, a bounded iterator chain, an operator
/// boundary and a synthesized column-Map entry.
const RICH: &str = r#"
value P { x: float, y: float }
fn ident<T>(x: T) -> T { return x }
fn wrap<T>(x: T) -> T { return ident(x) }
fn taker(f: fn(int) -> int) -> int { return f(1) }
fn plain(x: int) -> int { return x + 1 }
fn main() {
    let a = wrap(1)
    let b = wrap("s")
    let t = taker(plain)
    let v: [int] = [1, 2, 3]
    let s = 0
    for x in v { s = s + x }
    let n = v.iter().map(_ * 2).sum()
    let m: Map<string, P> = map_new()
    m["a"] = P { x: 1.0, y: 2.0 }
    let r = m["a"]
}
"#;

#[test]
fn concrete_result_order_gate_exercises_the_reversed_merge_population() {
    let ws = ws_with_stdlib(RICH);
    let roots = skull::mir_emit::emission_roots(&ws, None);
    let canonical_graph = instance_graph::build(&ws, roots.clone(), false);
    let (_, canonical_order) = concrete_body::build_with_result_order(
        &ws,
        &canonical_graph,
        concrete_body::BuildResultOrder::Canonical,
    );
    // Both order arms must use one interner: raw type IDs are arena-local.
    let reverse_graph = &canonical_graph;
    let (_, reverse_order) = concrete_body::build_with_result_order(
        &ws,
        reverse_graph,
        concrete_body::BuildResultOrder::Reverse,
    );
    assert!(
        canonical_order.len() > 1,
        "fixture exposed only one concrete result"
    );
    let mut expected = canonical_order.clone();
    expected.reverse();
    assert_eq!(
        reverse_order, expected,
        "the test did not force opposite result order"
    );
}

#[test]
#[cfg(not(feature = "sequential"))]
fn concrete_reverse_completion_gate_exercises_parallel_publication() {
    let ws = ws_with_stdlib(RICH);
    let graph = graph_of(&ws);
    let (mut scheduled, merge_order, trace) =
        concrete_body::build_with_reverse_completion(&ws, &graph);
    scheduled.refresh_flat_views();

    assert!(
        merge_order.len() > 1,
        "fixture exposed only one concrete result"
    );
    assert!(
        trace.worker_count > 1,
        "dedicated test pool used only one worker"
    );
    let canonical_ready = merge_order
        .iter()
        .filter(|instance| scheduled.bodies.contains_key(*instance))
        .cloned()
        .collect::<Vec<_>>();
    assert!(
        canonical_ready.len() > 1,
        "fixture exposed only one ready result"
    );
    assert!(
        trace.completion_order.len() > 1,
        "scheduler recorded fewer than two results"
    );
    let first = canonical_ready
        .iter()
        .position(|instance| instance == &trace.completion_order[0])
        .expect("first completion was not a ready result");
    let second = canonical_ready
        .iter()
        .position(|instance| instance == &trace.completion_order[1])
        .expect("second completion was not a ready result");
    assert!(
        first > second,
        "scheduler did not force an opposite completion order: {first} then {second}"
    );

    let census = scheduled.census();
    assert!(
        scheduled.generic_bodies > 0,
        "fixture published no generic body"
    );
    assert!(
        scheduled.build_stats.type_nodes_examined > 0,
        "fixture published no type nodes"
    );
    assert!(
        scheduled.signature_derivations > 0,
        "fixture derived no signatures"
    );
    assert!(
        scheduled.signature_requests > scheduled.signature_derivations,
        "fixture did not exercise signature reuse: {}/{}",
        scheduled.signature_requests,
        scheduled.signature_derivations
    );
    assert!(
        scheduled.build_stats.plan_successes > 0,
        "fixture published no ordinary plans"
    );
    assert!(
        census.direct_calls > 0,
        "fixture published no direct-call rows"
    );
    assert!(
        census.fn_values > 0,
        "fixture published no function-value rows"
    );
    assert!(
        census.iterator_ops.values().sum::<usize>() > 0,
        "fixture published no iterator rows"
    );
    assert!(
        census.synthesized_calls > 0,
        "fixture published no synthesized-call rows"
    );
    assert!(
        !scheduled.always_plans.is_empty(),
        "fixture published no always-emitted plans"
    );
}

#[cfg(feature = "sequential")]
#[test]
fn concrete_sequential_route_uses_one_worker_without_parallel_handshake() {
    let ws = ws_with_stdlib(RICH);
    let graph = graph_of(&ws);
    let (mut scheduled, trace) = concrete_body::build_with_default_trace(&ws, &graph);
    scheduled.refresh_flat_views();

    assert!(
        scheduled.plans.len() > 1,
        "fixture exposed too few ordinary plans"
    );
    assert!(
        !scheduled.always_plans.is_empty(),
        "fixture exposed no always-emitted plans"
    );
    assert_eq!(
        trace.worker_count, 1,
        "sequential ordinary production used multiple workers"
    );
    assert_eq!(
        trace.always_worker_count, 1,
        "sequential always-emitted production used multiple workers"
    );
    assert_eq!(
        trace.scheduler_entries, 0,
        "sequential production entered the parallel scheduler handshake"
    );
}

#[test]
fn concrete_result_order_merge_keeps_plan_and_failure_identity_canonical() {
    let src = r#"
fn keep<T>(x: T) -> T {
    return x
}
fn first() -> int {
    let unresolved = null
    return 1
}
fn second() -> int {
    let unresolved = null
    return 2
}
fn main() {
    let a = keep(1)
    let b = keep("s")
    let c = first()
    let d = second()
    print("{a}{b}")
}
"#;
    let ws = ws_with_stdlib(src);
    let roots = skull::mir_emit::emission_roots(&ws, None);
    let canonical_graph = instance_graph::build(&ws, roots.clone(), false);
    let (mut canonical, canonical_order) = concrete_body::build_with_result_order(
        &ws,
        &canonical_graph,
        concrete_body::BuildResultOrder::Canonical,
    );
    // Both order arms must use one interner: raw type IDs are arena-local.
    let reverse_graph = &canonical_graph;
    let (mut reverse, reverse_order) = concrete_body::build_with_result_order(
        &ws,
        reverse_graph,
        concrete_body::BuildResultOrder::Reverse,
    );
    canonical.refresh_flat_views();
    reverse.refresh_flat_views();
    assert!(canonical_order.len() > 1, "fixture exposed only one result");
    assert!(
        canonical.free_types.len() > 1,
        "fixture exposed no cross-result failure population: {:?}",
        canonical.render_free_types(&ws, &canonical_graph)
    );
    let expected_free = canonical_order
        .iter()
        .flat_map(|instance| {
            canonical
                .free_types
                .iter()
                .filter(move |(owner, _)| owner == instance)
                .map(|(owner, failure)| {
                    format!(
                        "{} {} {} at {}:{}",
                        canonical_graph.identity(&ws, owner),
                        failure.slot.kind(),
                        failure.reason.bucket(),
                        failure.span.file,
                        failure.span.line_range.0 + 1
                    )
                })
        })
        .collect::<Vec<_>>();
    assert_eq!(
        canonical.render_free_types(&ws, &canonical_graph),
        expected_free
    );
    let mut canonical_stats = canonical.build_stats;
    let mut reverse_stats = reverse.build_stats;
    canonical_stats.signature_ns = 0;
    reverse_stats.signature_ns = 0;
    canonical_stats.type_nodes_ns = 0;
    reverse_stats.type_nodes_ns = 0;
    canonical_stats.struct_fields_ns = 0;
    reverse_stats.struct_fields_ns = 0;
    canonical_stats.foreign_domains_ns = 0;
    reverse_stats.foreign_domains_ns = 0;
    assert_eq!(canonical_stats, reverse_stats);
    assert_eq!(*canonical.plans, *reverse.plans);
    assert_eq!(canonical.always_plans, reverse.always_plans);
    assert_eq!(canonical.free_types.len(), reverse.free_types.len());
    assert_eq!(canonical.callee_holes.len(), reverse.callee_holes.len());
    assert_eq!(canonical.plan_failures, reverse.plan_failures);
    assert_eq!(
        canonical.render_free_types(&ws, &canonical_graph),
        reverse.render_free_types(&ws, reverse_graph)
    );
    assert_eq!(reverse.render_free_types(&ws, reverse_graph), expected_free);
    assert_eq!(
        canonical.render_callee_holes(&ws, &canonical_graph),
        reverse.render_callee_holes(&ws, reverse_graph)
    );
    assert_eq!(
        canonical_order,
        reverse_order.into_iter().rev().collect::<Vec<_>>(),
        "the reverse arm did not expose the opposite incoming order"
    );
}

/// Every recorded site is accounted for EXACTLY once, and the accounting is
/// stated as an identity rather than as four independent non-zero counts: a
/// site that silently reached no table would leave the totals short while every
/// individual count still looked healthy.
#[test]
fn every_recorded_operation_becomes_one_entry_a_hole_or_a_rejection() {
    let (ws, g, bodies) = bodies_of(RICH);
    let c = bodies.census();
    assert!(
        c.sites_examined > 0,
        "no callee sites were examined at all: {c:?}"
    );
    assert_eq!(
        c.callee_entries() + c.callee_hole_total(),
        c.sites_examined,
        "sites went missing between the walk and the tables: {c:?}"
    );
    // Cross-structure, so it cannot be satisfied by both sides moving
    // together: the walk pushes one EDGE per resolved callee, and the sites are
    // a separate table written by the same recorders. A recorder that stopped
    // recording sites would keep the identity above (its denominator is the
    // sites) and break this one.
    let resolved = g
        .call_sites()
        .flat_map(|(_, m)| m.values())
        .filter(|r| matches!(r.outcome, SiteOutcome::Resolved(_)))
        .count();
    assert_eq!(
        resolved,
        g.edges().count(),
        "the walk recorded {} edges but {resolved} resolved sites",
        g.edges().count()
    );
    assert!(
        bodies.gaps.is_empty(),
        "an expanded instance got no body: {:?}",
        c.gaps
    );
    // Each table has a population, so the identity above is not satisfied by
    // one table carrying everything.
    assert!(c.direct_calls > 0, "no direct call was tabulated: {c:?}");
    assert!(c.fn_values > 0, "no function value was tabulated: {c:?}");
    assert!(
        c.synthesized_calls > 0,
        "no synthesized call was tabulated: {c:?}"
    );
    for kind in ["iter", "next", "bound-next"] {
        assert!(
            c.iterator_ops.contains_key(kind),
            "no `{kind}` iterator op was tabulated: {:?}",
            c.iterator_ops
        );
    }
    // Both arms of the split are populated: a fixture that reached only Rock
    // bodies could not tell `Callee::Boundary` from a dead variant.
    assert!(c.rock_callees > 0, "no Rock callee: {c:?}");
    assert!(c.boundary_callees > 0, "no boundary callee: {c:?}");
    assert_eq!(
        c.unknown_rock_callees, 0,
        "a Rock callee names an instance the graph does not have: {:?}",
        bodies.unknown_rock_callees
    );
    assert!(
        g.duplicate_sites().next().is_none(),
        "two operations collapsed onto one site key: {:?}",
        g.duplicate_sites().collect::<Vec<_>>()
    );
    // Every hole is located -- a hole with no span cannot become a compile
    // error at the Stage-3 cutover.
    for h in &bodies.callee_holes {
        assert!(
            !instance_graph::is_spanless(&h.span),
            "a callee hole carries no span: {:?}",
            h.site
        );
    }
    // A boundary callee names the INSTANCE the walk resolved and the class the
    // ONE classifier gives that declaration. It carries no target and no
    // spelling: a shared body has neither, and `artifact::realize` is what
    // turns this into a `BoundaryId`
    // (`artifact_view::an_artifact_realizes_the_shared_instance_to_the_sibling_that_serves_it`).
    for b in bodies.bodies.values() {
        for (site, callee) in b.callees() {
            let Callee::Boundary(inst, class) = callee else {
                continue;
            };
            let f = &ws.module(inst.func.module).functions[inst.func.func];
            assert_eq!(
                Some(*class),
                instance_graph::boundary_class_for(f, &g.erased_entries),
                "wrong boundary class at {:?}",
                site.op
            );
        }
    }
}

#[test]
fn concrete_plan_attribution_uses_one_authoritative_type_table() {
    let (_ws, _g, bodies) = bodies_of(RICH);
    let stats = bodies.build_stats;
    assert!(
        stats.plan_attempts > 0,
        "no ordinary concrete plans were attempted: {stats:?}"
    );
    assert_eq!(
        stats.plan_attempts,
        stats.plan_successes + stats.plan_failures,
        "ordinary plan outcomes lost a population: {stats:?}"
    );
    assert!(
        stats.source_expr_slots > 0 && stats.source_binding_slots > 0,
        "the fixture did not expose source type-table slots: {stats:?}"
    );
    assert!(
        stats.callee_scans > 0 && stats.callee_rows_visited > 0,
        "the fixture did not expose ConcreteBody::callees traversal: {stats:?}"
    );
    assert_eq!(
        stats.callee_scans,
        stats.plan_attempts * 3,
        "the ordinary plan lost a callee traversal: {stats:?}"
    );
    assert!(
        stats.plan_callee_map_inserts > 0 && stats.plan_callee_signature_inserts > 0,
        "the fixture did not expose plan callee consumers: {stats:?}"
    );
}

#[test]
fn fresh_publication_does_not_eagerly_fold_owner_records() {
    let (_ws, _g, bodies) = bodies_of_raw(RICH);
    assert!(
        !bodies.plans.is_empty(),
        "fixture exposed no concrete plans"
    );
    assert_eq!(
        bodies.eager_owner_fact_scans(),
        0,
        "fresh publication folded every owner record into a compatibility snapshot"
    );
    assert_eq!(
        bodies.build_stats.plan_successes, 0,
        "fresh publication eagerly folded owner counters into the compatibility snapshot"
    );
}

#[test]
fn fresh_workers_do_not_construct_global_concrete_indexes() {
    let before = concrete_body::concrete_default_invocations();
    let (_ws, _g, bodies) = bodies_of_raw(RICH);
    let default_invocations = concrete_body::concrete_default_invocations().saturating_sub(before);
    assert!(
        !bodies.plans.is_empty(),
        "fixture exposed no concrete plans"
    );
    assert_eq!(
        default_invocations, 1,
        "fresh workers constructed ConcreteBodies' global indexes instead of owner facts"
    );
}

#[test]
fn fresh_publication_publishes_the_authoritative_candidate_index() {
    let (_ws, _g, bodies) = bodies_of_raw(RICH);
    let mut rebuilt = rustc_hash::FxHashMap::default();
    for key in bodies.candidate_keys() {
        rebuilt
            .entry(key.func)
            .or_insert_with(rustc_hash::FxHashSet::default)
            .insert(key.clone());
    }
    assert!(
        !rebuilt.is_empty(),
        "fixture exposed no candidate population"
    );
    assert_eq!(
        bodies.candidate_keys_by_func(),
        &rebuilt,
        "fresh construction did not publish the candidate index from its producer"
    );
}

#[test]
fn local_owner_diagnostic_records_retract_and_reinstall_exactly() {
    let source_v1 = r#"
fn changing() -> int {
    let unresolved = null
    return 1
}
fn main() {
    let a = changing()
    let b = stable()
    let c = vanishing()
    print("{a}{b}{c}")
}
fn stable() -> int {
    let unresolved = null
    return 2
}
fn vanishing() -> int {
    let unresolved = null
    return 3
}
"#;
    let source_v2 = r#"
fn changing() -> int {
    let unresolved = null
    return 1
}
fn main() {
    let b = stable()
    print("{b}")
}
fn stable() -> int {
    let unresolved = null
    return 2
}
fn vanishing() -> int {
    let unresolved = null
    return 3
}
"#;
    let ws1 = ws_with_stdlib(source_v1);
    let roots = skull::mir_emit::emission_roots(&ws1, Some("/project"));
    let mut graph = instance_graph::build(&ws1, roots.clone(), false);
    let mut local = concrete_body::build(&ws1, &graph);
    let before = local.diagnostic_snapshot(&ws1, &graph);
    assert!(!before.is_empty(), "fixture exposed no diagnostic rows");
    assert!(
        before
            .iter()
            .any(|row| row.starts_with("free-type:") || row.starts_with("untyped:")),
        "fixture exposed neither free-type nor untyped rows: {before:?}"
    );
    let stable_identity = graph
        .expanded()
        .find(|instance| {
            ws1.module(instance.func.module).functions[instance.func.func]
                .name
                .as_str()
                == "stable"
        })
        .map(|instance| graph.identity(&ws1, instance))
        .expect("stable instance was not expanded");

    let ws2 = ws_with_stdlib(source_v2);
    let module = module_id(&ws2, "<test>");
    let old_module = module_id(&ws1, "<test>");
    let old_main_span = ws1
        .module(old_module)
        .functions
        .iter()
        .find(|(_, function)| function.name.as_str() == "main")
        .map(|(_, function)| function.span)
        .expect("old main span");
    let main = ws2
        .module(module)
        .functions
        .iter()
        .find(|(_, function)| function.name.as_str() == "main")
        .map(|(function, _)| (module, BodyOwner::Function(function)))
        .expect("main body");
    let BodyOwner::Function(main_fid) = main.1 else {
        panic!("main is not a function")
    };
    let new_main_span = ws2.module(module).functions[main_fid].span;
    let span_edit = instance_graph::TextEditDelta {
        file: Ustr::from("<test>"),
        at: old_main_span.offset_range.1,
        byte_delta: new_main_span.offset_range.1 as isize - old_main_span.offset_range.1 as isize,
        line_delta: new_main_span.line_range.1 as isize - old_main_span.line_range.1 as isize,
    };
    let update = graph.update_bodies_with_root_delta(&ws2, &[], &[], &[main], &[span_edit]);
    assert!(
        !update.removed.is_empty(),
        "the changed main body did not remove a live edge: {update:?}"
    );
    let _stats = local.update_local_with_span_edits(
        &ws2,
        &graph,
        &update.revisited,
        &update.removed,
        &update.revisited_owners,
        &[span_edit],
    );
    let fresh_roots = skull::mir_emit::emission_roots(&ws2, Some("/project"));
    let fresh_graph = instance_graph::build(&ws2, fresh_roots, false);
    let mut fresh = concrete_body::build(&ws2, &fresh_graph);
    assert_eq!(
        local.diagnostic_snapshot(&ws2, &graph),
        fresh.diagnostic_snapshot(&ws2, &fresh_graph),
        "local owner records diverged from fresh diagnostics"
    );
    let local_stats = local.snapshot_build_stats();
    let fresh_stats = fresh.snapshot_build_stats();
    assert_eq!(local_stats.plan_attempts, fresh_stats.plan_attempts);
    assert_eq!(local_stats.plan_successes, fresh_stats.plan_successes);
    assert_eq!(local_stats.plan_failures, fresh_stats.plan_failures);
    assert_eq!(
        local_stats.always_plan_attempts,
        fresh_stats.always_plan_attempts
    );
    assert_eq!(
        local_stats.type_nodes_examined,
        fresh_stats.type_nodes_examined
    );
    assert_eq!(
        local_stats.foreign_domain_unique,
        fresh_stats.foreign_domain_unique
    );
    assert!(
        local
            .diagnostic_snapshot(&ws2, &graph)
            .iter()
            .any(|row| row.contains(&stable_identity)),
        "the unrelated populated owner was lost during local replacement"
    );
    assert!(
        !local
            .diagnostic_snapshot(&ws2, &graph)
            .iter()
            .any(|row| row.contains("changing") || row.contains("vanishing")),
        "removed owner findings were retained"
    );
}

#[test]
fn owner_records_expose_plan_and_always_failure_populations() {
    let ws = ws_with_stdlib(
        r#"
fn bad() -> int {
    return 1
}
fn hole<T>() -> int {
    let unresolved: T = null
    return 1
}
fn ordinary(v: [int]) -> int {
    let unresolved = null
    let total = 0
    for i in 0..3 { total = total + i }
    for x in v { total = total + x }
    return total
}
global failed_call: int = bad()
fn main() {
    let a = ordinary([1, 2])
    let b = bad()
    let c = hole()
    print("{a}{b}{c}{failed_call}")
}
"#,
    );
    let roots = skull::mir_emit::emission_roots(&ws, Some("<project>"));
    let mut graph = instance_graph::build(&ws, roots, false);
    let bad = graph
        .expanded()
        .find(|instance| fn_name(&ws, instance.func) == "bad")
        .cloned()
        .expect("bad instance");
    assert!(
        !graph.root_instances.contains(&bad),
        "fixture made bad a root"
    );
    assert!(
        graph.index.subst.remove(&bad).is_some(),
        "fixture did not expose bad's substitution"
    );
    let mut bodies = concrete_body::build(&ws, &graph);
    bodies.refresh_flat_views();
    let snapshot = bodies.diagnostic_snapshot(&ws, &graph);
    assert!(!snapshot.is_empty(), "fixture exposed no owner findings");
    assert!(
        bodies.build_stats.plan_attempts > 0,
        "fixture exposed no ordinary plan population: {:?}",
        bodies.build_stats
    );
    assert!(
        bodies.build_stats.always_plan_attempts > 0,
        "fixture exposed no always-emitted plan population: {:?}",
        bodies.build_stats
    );
    assert!(
        !bodies.plan_failures.is_empty(),
        "fixture exposed no ordinary plan failure: {snapshot:?}"
    );
    assert!(
        !bodies.always_plan_failures.is_empty(),
        "fixture exposed no always-emitted plan failure: {snapshot:?}"
    );
    assert!(
        !bodies.free_types.is_empty(),
        "fixture exposed no free type: {snapshot:?}"
    );
    assert!(
        !bodies.untyped.is_empty(),
        "fixture exposed no untyped slot: {snapshot:?}"
    );
    assert!(
        !bodies.callee_holes.is_empty(),
        "fixture exposed no callee hole: {snapshot:?}"
    );
}

#[test]
fn local_owner_plan_failures_retract_with_the_last_live_edge() {
    let source_v1 = r#"
fn bad() -> int { return 1 }
fn stable() -> int {
    let unresolved = null
    return 2
}
global failed_call: int = bad()
fn main() {
    let a = bad()
    let b = stable()
    print("{a}{b}{failed_call}")
}
"#;
    let source_v2 = r#"
fn bad() -> int { return 1 }
fn stable() -> int {
    let unresolved = null
    return 2
}
global failed_call: int = 1
fn main() {
    let b = stable()
    print("{b}{failed_call}")
}
"#;
    let ws1 = ws_with_stdlib(source_v1);
    let roots = skull::mir_emit::emission_roots(&ws1, Some("<project>"));
    let mut graph = instance_graph::build(&ws1, roots, false);
    let bad = graph
        .expanded()
        .find(|instance| {
            ws1.module(instance.func.module).functions[instance.func.func]
                .name
                .as_str()
                == "bad"
        })
        .cloned()
        .expect("bad instance");
    assert!(
        graph.index.subst.remove(&bad).is_some(),
        "fixture did not expose bad's substitution"
    );
    let mut local = concrete_body::build(&ws1, &graph);
    let before = local.diagnostic_snapshot(&ws1, &graph);
    assert!(before.iter().any(|row| row.starts_with("plan-failure:")));
    assert!(
        before
            .iter()
            .any(|row| row.starts_with("always-plan-failure:"))
    );

    let ws2 = ws_with_stdlib(source_v2);
    let module = module_id(&ws2, "<test>");
    let owners = ws2
        .module(module)
        .functions
        .iter()
        .filter_map(|(function, f)| {
            matches!(f.name.as_str(), "main").then_some((module, BodyOwner::Function(function)))
        })
        .chain(std::iter::once((
            module,
            BodyOwner::Global(Idx::from_raw(0u32.into())),
        )))
        .collect::<Vec<_>>();
    let update = graph.update_bodies_with_root_delta(&ws2, &[], &[], &owners, &[]);
    assert!(
        !update.removed.is_empty(),
        "the last bad edge was not removed: {update:?}"
    );
    local.update_local(
        &ws2,
        &graph,
        &update.revisited,
        &update.removed,
        &update.revisited_owners,
    );
    let fresh_roots = skull::mir_emit::emission_roots(&ws2, Some("<project>"));
    let fresh_graph = instance_graph::build(&ws2, fresh_roots, false);
    let fresh = concrete_body::build(&ws2, &fresh_graph);
    assert_eq!(
        local.diagnostic_snapshot(&ws2, &graph),
        fresh.diagnostic_snapshot(&ws2, &fresh_graph),
        "local failure records diverged after the last edge disappeared"
    );
    assert!(
        local.diagnostic_snapshot(&ws2, &graph).iter().all(
            |row| !row.starts_with("plan-failure:") && !row.starts_with("always-plan-failure:")
        ),
        "stale plan failure survived owner retraction: {:?}",
        local.diagnostic_snapshot(&ws2, &graph)
    );
}

/// The callee table is a function of the INSTANCE. `wrap<int>` and
/// `wrap<string>` share one HIR body, so the same `Idx<Expr>` must name two
/// DIFFERENT callee instances -- which is what makes a substitution unnecessary
/// downstream. A table built per FUNCTION fails exactly here.
#[test]
fn one_generic_body_names_a_different_callee_per_instance() {
    let (ws, g, bodies) = bodies_of(RICH);
    let wraps: Vec<&skull::instance_graph::Instance> = bodies
        .bodies
        .keys()
        .filter(|i| g.identity(&ws, i).starts_with("wrap"))
        .collect();
    assert_eq!(
        wraps.len(),
        2,
        "expected two `wrap` instances, got {wraps:?}"
    );
    let (a, b) = (&bodies.bodies[wraps[0]], &bodies.bodies[wraps[1]]);
    let shared: Vec<(skull::instance_graph::Site, &Callee, &Callee)> = a
        .direct_calls
        .iter()
        .filter_map(|(s, x)| b.direct_calls.get(s).map(|y| (s.clone(), x, y)))
        .collect();
    assert!(
        !shared.is_empty(),
        "the two `wrap` bodies share no call site"
    );
    let named: Vec<(String, String)> = shared
        .iter()
        .filter_map(|(_, x, y)| match (x, y) {
            (Callee::Rock(x), Callee::Rock(y)) if x != y => {
                Some((g.identity(&ws, x), g.identity(&ws, y)))
            }
            _ => None,
        })
        .collect();
    assert!(
        named
            .iter()
            .any(|(x, y)| x.contains("p_int") && y.contains("p_string")
                || x.contains("p_string") && y.contains("p_int")),
        "the two instances of `wrap` call the same `ident`, so the callee table \
         is not instance-specific: {named:?}"
    );
}

/// A bounded body's placeholder is an operation in THAT body, not merely a
/// conformance record at its caller. Both the explicit `next(it)` form and a
/// `for` over the bounded generic must already name VecIter's concrete `next`
/// in their own `ConcreteBody`.
#[test]
fn bounded_body_operations_name_the_concrete_witness() {
    let (ws, g, bodies) = bodies_of(
        r#"
fn pull<I: Iterator<int>>(it: I) -> int {
    let (ok, x) = next(it)
    return if ok { x } else { 0 }
}
fn sum_it<I: Iterator<int>>(it: I) -> int {
    let total = 0
    for x in it { total += x }
    return total
}
fn main() {
    let a = pull([1, 2].iter())
    let b = sum_it([3, 4].iter())
}
"#,
    );
    assert!(
        g.bound_tree_disagreements().next().is_none(),
        "one instance got two conformance answers: {:?}",
        g.bound_tree_disagreements().collect::<Vec<_>>()
    );

    let pull = bodies
        .bodies
        .keys()
        .find(|i| g.identity(&ws, i).starts_with("pull"))
        .expect("no concrete `pull` instance");
    let pull_calls: Vec<String> = bodies.bodies[pull]
        .direct_calls
        .values()
        .map(|c| match c {
            Callee::Rock(i) => g.identity(&ws, i),
            Callee::Boundary(i, _) => g.identity(&ws, i),
        })
        .collect();
    assert!(
        pull_calls
            .iter()
            .any(|n| n.starts_with("next_VecIter") && n.contains("int")),
        "the explicit bounded call did not resolve inside `pull`: {pull_calls:?}"
    );

    let sum_it = bodies
        .bodies
        .keys()
        .find(|i| g.identity(&ws, i).starts_with("sum_it"))
        .expect("no concrete `sum_it` instance");
    let loop_nexts: Vec<String> = bodies.bodies[sum_it]
        .iterator_ops
        .values()
        .filter(|op| op.kind == IteratorOpKind::Next)
        .map(|op| match &op.callee {
            Callee::Rock(i) => g.identity(&ws, i),
            Callee::Boundary(i, _) => g.identity(&ws, i),
        })
        .collect();
    assert!(
        loop_nexts
            .iter()
            .any(|n| n.starts_with("next_VecIter") && n.contains("int")),
        "the caller-generic loop witness did not resolve inside `sum_it`: {loop_nexts:?}"
    );
}

#[test]
fn disagreeing_bound_trees_cannot_choose_a_body_by_discovery_order() {
    let ws = ws_with_stdlib(
        r#"
fn sum_it<I: Iterator<int>>(it: I) -> int {
    let total = 0
    for x in it { total += x }
    return total
}
fn main() { let n = sum_it([1, 2].iter()) }
"#,
    );
    let roots = skull::mir_emit::emission_roots(&ws, None);
    let mut g = instance_graph::build(&ws, roots, false);
    let inst = expanded_with_prefix(&ws, &g, "sum_it");
    let recorded = g.bound_tree(&inst).cloned().expect("no witness tree");
    assert!(
        !recorded.next.is_empty(),
        "fixture has no bound witness tree"
    );
    // A disagreement IS two owners supplying different witnesses, so it is
    // constructed as a second index entry rather than as a record beside them.
    // The synthetic owner sorts after every `Owner::Instance`, so the real
    // tree stays the kept one.
    g.index
        .trees
        .get_mut(&inst)
        .expect("the instance has a witness record")
        .dissent
        .insert(
            instance_graph::Owner::AlwaysEmitted(
                bone_semantic::prelude::ModuleId::builtin(),
                bone_semantic::hir::BodyOwner::Global(bone_semantic::prelude::Idx::from_raw(
                    0u32.into(),
                )),
            ),
            instance_graph::InstanceBoundTree::default(),
        );

    let mut bodies = concrete_body::build(&ws, &g);
    bodies.refresh_flat_views();
    assert!(
        !bodies.bodies.contains_key(&inst),
        "an ambiguous body was still built"
    );
    assert!(
        bodies
            .gaps
            .iter()
            .any(|gap| gap.instance == inst && gap.gap.label() == "inconsistent-bounds"),
        "the disagreement was not classified: {:?}",
        bodies.census().gaps
    );
}

/// A `for` head resolves to a concrete `iter` AND a concrete `next`, and a
/// chained iterator's nested witnesses each get their own entry. Both live at
/// one expression, which is why the table is keyed by SITE.
#[test]
fn iterator_operations_resolve_to_concrete_witnesses() {
    let (ws, g, bodies) = bodies_of(RICH);
    let main = bodies
        .bodies
        .keys()
        .find(|i| g.identity(&ws, i).starts_with("main@"))
        .expect("no `main` body");
    let ops = &bodies.bodies[main].iterator_ops;
    let named: Vec<(IteratorOpKind, String)> = ops
        .values()
        .map(|op| {
            (
                op.kind,
                match &op.callee {
                    Callee::Rock(i) => g.identity(&ws, i),
                    Callee::Boundary(i, _) => g.identity(&ws, i),
                },
            )
        })
        .collect();
    // Named, not merely present: the `for` over `[int]` resolves `next` at
    // `VecIter<int>`, so the ELEMENT type is in the callee's identity.
    assert!(
        named
            .iter()
            .any(|(k, n)| *k == IteratorOpKind::Iter && n.contains("p_int")),
        "no concrete `iter` at the int element: {named:?}"
    );
    assert!(
        named
            .iter()
            .any(|(k, n)| *k == IteratorOpKind::Next && n.starts_with("next_VecIter")),
        "no concrete `next` for the `for` protocol: {named:?}"
    );
    // The chain's nested witnesses: `map(..).sum()` resolves MapIter's `next`
    // and, one level down, VecIter's. Same expression, different paths.
    let nested: Vec<&Site> = ops
        .keys()
        .filter(|s| matches!(&s.op, SiteOp::BoundNext(p) if p.len() > 1))
        .collect();
    assert!(
        !nested.is_empty(),
        "no nested conformance witness was recorded: {:?}",
        ops.keys()
    );
    let deep_expr = nested[0].expr;
    let at_expr: Vec<&Site> = ops.keys().filter(|s| s.expr == deep_expr).collect();
    assert!(
        at_expr.len() > 1,
        "the nested witness has no sibling at its own expression, so keying by \
         expression alone would not have lost anything: {at_expr:?}"
    );
}

/// A synthesized column-Map entry names ONE runtime name per site, chosen by the
/// index's position and the stored value -- not the set of three the arm could
/// have emitted.
#[test]
fn a_column_map_index_names_the_entry_its_arm_emits() {
    let (ws, g, bodies) = bodies_of(
        r#"
value P { x: float, y: float }
fn main() {
    let m: Map<string, P> = map_new()
    m["w"] = P { x: 1.0, y: 2.0 }
    let r = m["r"]
    let n: P? = null
    m["n"] = n
}
"#,
    );
    let main = bodies
        .bodies
        .keys()
        .find(|i| g.identity(&ws, i).starts_with("main@"))
        .expect("no `main`");
    let mut names: Vec<&str> = bodies.bodies[main]
        .synthesized_calls
        .keys()
        .filter_map(|s| match s.op {
            SiteOp::Synthesized(n) => Some(n),
            _ => None,
        })
        .collect();
    names.sort();
    // The constructor spine, write, read, and nullable write -- one name each,
    // except the nullable write, whose arm genuinely emits both a slot and a
    // free.
    assert_eq!(
        names,
        vec![
            "__map_col_free",
            "__map_col_new_spine",
            "__map_col_slot",
            "__map_col_slot",
            "__map_col_slot_checked",
        ],
        "the synthesized names are not the ones the four arms emit"
    );
    assert_eq!(
        g.synthesized_approximated, 0,
        "a Map index site could not decide its entry"
    );
}

/// `__vec_data_sort`'s two arms split on the ELEMENT, so a site names one.
#[test]
fn a_column_vec_sort_names_one_of_its_two_arms() {
    // Read off the SITES, not the table: the stdlib's `sort` synthesizes a call
    // to a generic runtime entry with no binding record, so the site is a hole.
    // Which NAME the arm chose is the subject here, and the site carries it
    // either way -- that is the point of keying the operation.
    let sorted = |src: &str| -> Vec<&'static str> {
        let (_, g, _) = bodies_of(src);
        let mut v: Vec<&'static str> = g
            .call_sites()
            .flat_map(|(_, sites)| sites.keys())
            .filter_map(|s| match s.op {
                SiteOp::Synthesized(n @ ("__rock_col_sort" | "__rock_col_sort_leaves")) => Some(n),
                _ => None,
            })
            .collect();
        v.sort();
        v.dedup();
        v
    };
    let boxed = sorted(
        r#"
value P { x: float, y: float }
fn main() { let ps: [P] = []; ps.sort(_.x) }
"#,
    );
    let leaves = sorted(
        r#"
nobox value Q { x: float, y: float }
fn main() { let qs: [Q] = []; qs.sort(_.x) }
"#,
    );
    assert_eq!(
        boxed,
        vec!["__rock_col_sort"],
        "a boxed element took the leaf arm"
    );
    assert_eq!(
        leaves,
        vec!["__rock_col_sort_leaves"],
        "a `nobox` element did not take the leaf arm"
    );
}

/// The requirement the type-table slice deferred, answered by MEASUREMENT
/// rather than by reading the lowering paths: no callee or iterator entry is
/// keyed on an expression inference recorded no type for, so no table needs one.
///
/// The oracles that answer instead are named by construction --
/// `resolved_overload_calls` for a direct call, `IterWitness` /
/// `for_iter_protocol` / `bound_call_trees` for the iterator ops, and the
/// receiver's own type for a synthesized call (whose failures are
/// `storage_failures`, reported against `synthesized_examined`).
#[test]
fn no_untyped_expression_is_a_callee_key() {
    let (ws, g, bodies) = bodies_of(
        r#"
fn body(v: [int]) -> int {
    let total = 0
    for i in 0..3 { total = total + i }
    for (i, x) in enumerate(v) { total = total + i + x }
    let m: int? = 1
    if let y = m { total = total + y }
    let annotated: int = 0
    if v.len() > 0 { annotated = 1 } else { annotated = 2 }
    return total + annotated
}
fn main() { let a = body([1, 2]) }
"#,
    );
    let inst = bodies
        .bodies
        .keys()
        .find(|i| g.identity(&ws, i).starts_with("body"))
        .expect("no `body` instance");
    // The fixture can distinguish: it HAS untyped slots of every pinned kind,
    // and it HAS callee entries. A body with neither would pass vacuously.
    let untyped: Vec<&str> = bodies
        .untyped
        .iter()
        .filter(|(i, _)| i == inst)
        .map(|(_, u)| u.kind)
        .collect();
    for kind in ["Call", "IfLet", "Range", "Var"] {
        assert!(
            untyped.contains(&kind),
            "the fixture has no untyped `{kind}`: {untyped:?}"
        );
    }
    assert!(
        !bodies.bodies[inst].direct_calls.is_empty(),
        "the fixture recorded no callee at all"
    );
    assert!(
        bodies.census().untyped_as_callee_key.is_empty(),
        "a callee entry is keyed on an expression with no inferred type: {:?}",
        bodies.census().untyped_as_callee_key
    );
    // The untyped `Call` is a `for` header in TEMPLATE position
    // (`lower_for_enumerate` emits a numeric loop), so it is not a call at all
    // -- no callee names it, and neither does the iterator table.
    let header = bodies
        .untyped
        .iter()
        .find(|(i, u)| i == inst && u.kind == "Call")
        .map(|(_, u)| u.expr)
        .expect("no untyped `Call`");
    assert!(
        bodies.bodies[inst].callees().all(|(s, _)| s.expr != header),
        "the template-lowered `for` header has a callee entry after all"
    );
    assert_eq!(
        g.storage_failures().count(),
        0,
        "a synthesized receiver had no concrete type"
    );
}

/// A normal full emission builds the concrete-plan table ONCE and shares it;
/// the shader branch consuming `ConcreteBodies::plans` by handle is what this
/// pins -- byte identity cannot see a rebuild, because a rebuild produces the
/// same plans. The fixture registers a real shader pipeline so the branch
/// that historically rebuilt is exercised, and asserts that population.
#[test]
fn a_full_emission_builds_the_concrete_plan_table_exactly_once() {
    let src = r#"
struct SrcTex { src_tex: Sampler2D }

@shader(fragment)
fn probe_frag(@textures t: SrcTex, tc: Vec2) -> Vec4 {
    return texture(t.src_tex, tc)
}

fn main() { let key = rock_shader(probe_frag) print("{key}") }
"#;
    let ws = ws_with_stdlib(src);
    let mid = skull::user_module_id(&ws).unwrap();
    let before = concrete_body::build_invocations();
    let (emit, _cache) = skull::mir_emit::emit_full(&ws, mid).expect("emit_full");
    assert!(
        emit.lua.contains("__rock_shader_register"),
        "the fixture must reach shader registration, or a shader-branch rebuild is invisible here"
    );
    assert_eq!(
        concrete_body::build_invocations() - before,
        1,
        "a full emission must build the concrete-plan table exactly once and share it"
    );
}

/// Derivations strictly below requests is the signature memo's effectiveness
/// witness: `helper` is asked once per call site plus once as its own plan's
/// signature, so a bypass answers every request by deriving and the counts
/// collapse to equality.
#[test]
fn the_signature_memo_serves_repeated_callee_questions() {
    let (_, _, bodies) = bodies_of(
        "fn helper(x: int) -> int { return x }\n\
         fn first() -> int { return helper(1) }\n\
         fn second() -> int { return helper(2) }\n\
         fn main() { let r = first() + second() print(\"{r}\") }",
    );
    assert!(
        bodies.signature_requests > 0,
        "no signature question was asked"
    );
    assert!(
        bodies.signature_derivations < bodies.signature_requests,
        "every request derived ({} of {}) -- per-site callee questions bypassed the memo",
        bodies.signature_derivations,
        bodies.signature_requests,
    );
    assert_eq!(
        bodies.signature_slots_cloned, 0,
        "signature memo hits must use the shared immutable representation, not clone {} slots",
        bodies.signature_slots_cloned
    );
}

/// The per-body TyId memo runs strictly fewer conversions than the per-slot
/// examined census, while that census itself stays per-slot -- `generics_bound`
/// is pinned to its exact per-slot value on a stdlib-free fixture, measured
/// identical with the memo bypassed, so a hit that stops replaying its delta
/// goes red here.
#[test]
fn the_tyid_memo_deduplicates_conversions_and_replays_the_census() {
    let mut modules = vec![
        skull_parser::parse_module(
            "fn id<T>(x: T) -> T { x }\nfn main() { let a = id(1)\n let b = id(2) }",
            Ustr::from("<tyid-memo-test>"),
        )
        .expect("parse"),
    ];
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    let g = graph_of(&ws);
    let mut bodies = concrete_body::build(&ws, &g);
    bodies.refresh_flat_views();
    let slots = bodies.expr_examined + bodies.binding_examined;
    assert!(slots > 0, "the fixture lost its slots");
    assert!(
        bodies.slot_conversions < slots,
        "every slot converted ({} of {slots}) -- repeated TyIds bypassed the per-body memo",
        bodies.slot_conversions,
    );
    assert!(
        bodies.generics_bound > 0,
        "the fixture must bind generics through the substitution"
    );
    assert_eq!(
        bodies.generics_bound, GENERICS_BOUND_PER_SLOT,
        "generics_bound counts per SLOT; a memo hit must replay its cached delta"
    );
}

/// Measured on the fixture above with the TyId memo enabled AND bypassed --
/// the number is the same on both arms, which is what makes it a pin on the
/// per-slot replay rather than on the memo's presence.
const GENERICS_BOUND_PER_SLOT: usize = 3;
