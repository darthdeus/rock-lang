//! `ConcreteBody` type tables.
//!
//! The property under test is not "a table exists". It is that the table is a
//! function of the INSTANCE: one generic body reached at two instantiations has
//! two tables that disagree on the same slot, and neither carries a free
//! generic. A table built under an empty substitution would fail every generic
//! slot instead, which is what the mutation arm at the bottom asserts.

use crate::common::{body_id, fn_name, graph_of, module_id, scalar_ctx, ws_with_stdlib};
use bone_semantic::hir::{BodyOwner, Expr, TypeExpr};
use bone_semantic::inference::Workspace;
use bone_semantic::prelude::{ModuleId, TyId};
use comfy_la_arena::{ArenaMap, Idx, IdxRange};
use skull::concrete_body::{self, ConcreteBodies, Slot};
use skull::instance_graph::{self, Instance, InstanceGraph, Site, SiteOp, SiteOutcome};
use skull_mir::concrete::{ConcreteBodyPlan, ConcreteOwner, ConcretePlans};
use skull_mir::mir::{Inst, MirFunc, Term};
use skull_mir::monoty::{MonoTy, MonoTyReason};
use skull_parser::CompilerError;
use std::sync::Arc;
use ustr::Ustr;

fn bodies_of(src: &str) -> (Workspace, InstanceGraph, ConcreteBodies) {
    let ws = ws_with_stdlib(src);
    let g = graph_of(&ws);
    let bodies = concrete_body::build(&ws, &g);
    (ws, g, bodies)
}

/// Like `bodies_of`, but over the graph built from the project's emission
/// roots, which also roots user globals.
fn project_bodies_of(src: &str) -> (Workspace, InstanceGraph, ConcreteBodies) {
    let ws = ws_with_stdlib(src);
    let roots = skull::mir_emit::emission_roots(&ws, Some("<project>"));
    let g = instance_graph::build(&ws, roots, false);
    let bodies = concrete_body::build(&ws, &g);
    (ws, g, bodies)
}

fn plan_named<'a>(
    ws: &Workspace,
    bodies: &'a ConcreteBodies,
    name: &str,
) -> (&'a Instance, &'a Arc<ConcreteBodyPlan>) {
    bodies
        .plans
        .iter()
        .find(|(inst, _)| fn_name(ws, inst.func) == name)
        .unwrap_or_else(|| panic!("{name} has no concrete plan"))
}

fn callee_named(ws: &Workspace, plan: &ConcreteBodyPlan, name: &str) -> Instance {
    plan.callees
        .values()
        .find(|callee| fn_name(ws, callee.instance.func) == name)
        .map(|callee| callee.instance.clone())
        .unwrap_or_else(|| panic!("no concrete `{name}` call"))
}

fn call_site<'a>(plan: &'a ConcreteBodyPlan, callee: &Instance) -> &'a Site {
    plan.callees
        .iter()
        .find_map(|(site, recorded)| (recorded.instance == *callee).then_some(site))
        .expect("call site disappeared")
}

/// The always-emitted plan of the user module's global (or decorator).
fn user_always_plan<'a>(
    ws: &Workspace,
    bodies: &'a ConcreteBodies,
    decorator: bool,
) -> &'a ConcreteBodyPlan {
    let user = module_id(ws, "<test>");
    bodies
        .always_plans
        .iter()
        .find(|(owner, _)| match owner {
            ConcreteOwner::Global { module, .. } => !decorator && *module == user,
            ConcreteOwner::Decorator { module, .. } => decorator && *module == user,
            _ => false,
        })
        .map(|(_, plan)| plan)
        .expect("the user owner has no always-emitted plan")
}

fn lower(
    ws: &Workspace,
    g: &InstanceGraph,
    plan: &ConcreteBodyPlan,
    plans: &ConcretePlans,
) -> Result<MirFunc, CompilerError> {
    skull_mir::lower::lower_instance_with_plans(ws, &scalar_ctx(ws, g), plan, plans)
}

fn lower_global(
    ws: &Workspace,
    g: &InstanceGraph,
    plan: &ConcreteBodyPlan,
    plans: &ConcretePlans,
) -> Result<Option<MirFunc>, CompilerError> {
    skull_mir::lower::lower_global_with_plans(
        ws,
        &scalar_ctx(ws, g),
        plan,
        plans,
        Ustr::from("__test_ginit"),
    )
}

/// `plans` with the selected callee's own signature rewritten by `edit`.
fn with_callee_signature(
    bodies: &ConcreteBodies,
    callee: &Instance,
    edit: impl FnOnce(&mut skull_mir::concrete::ConcreteSignature),
) -> ConcretePlans {
    let mut plans = (*bodies.plans).clone();
    let plan = Arc::make_mut(
        plans
            .get_mut(callee)
            .expect("selected callee plan disappeared"),
    );
    edit(Arc::make_mut(&mut plan.signature));
    plans
}

const ID_CALL_SOURCE: &str = r#"
fn id<T>(x: T) -> T { return x }
fn main() {
    let out = id(1)
    print("{out}")
}
"#;

#[track_caller]
fn assert_int_and_string_default_domains(g: &InstanceGraph, plan: &ConcreteBodyPlan) {
    assert_eq!(plan.foreign_domains.len(), 2, "expected both Box domains");
    let rendered = plan
        .foreign_domains
        .values()
        .flat_map(|tables| tables.expr_types.values())
        .map(|id| g.interner().render(*id))
        .collect::<Vec<_>>();
    assert!(
        rendered.iter().any(|ty| ty == "int?"),
        "missing int default table: {rendered:?}"
    );
    assert!(
        rendered.iter().any(|ty| ty == "string?"),
        "missing string default table: {rendered:?}"
    );
}

fn expanded_with_prefix(ws: &Workspace, g: &InstanceGraph, prefix: &str) -> Instance {
    g.expanded()
        .find(|i| g.identity(ws, i).starts_with(prefix))
        .cloned()
        .unwrap_or_else(|| panic!("no expanded instance of `{prefix}`"))
}

fn collect_nested_type_nodes(
    ws: &Workspace,
    inf: &bone_semantic::inference::BodyInference,
    ty: TyId,
    out: &mut Vec<TyId>,
) {
    let node = match ty {
        TyId::Static(mid, idx) => ws.module(mid).ty_arena[idx],
        TyId::Local(idx) => inf.ty_arena[idx],
    };
    let children: Vec<_> = match node {
        TypeExpr::Nullable(inner) => vec![inner],
        TypeExpr::App { params, .. } => params.into_iter().collect(),
        TypeExpr::Function { params, ret } => params.into_iter().chain([ret]).collect(),
        TypeExpr::Tuple { elements } => elements.into_iter().collect(),
        TypeExpr::Builtin(_) | TypeExpr::Generic(..) | TypeExpr::Infer(..) => Vec::new(),
    };
    for child in children {
        let child = ty.with_idx(child);
        out.push(child);
        collect_nested_type_nodes(ws, inf, child, out);
    }
}

const BOX_DEFAULT_WITH_CALL: &str = r#"
fn seed<T>() -> T? { return null }
struct Box<T> { value: T? = seed() }
fn main() {
    let a: Box<int> = Box {}
    let b: Box<string> = Box {}
    print("{a.value} {b.value}")
}
"#;

const BOX_DEFAULT_WITHOUT_CALL: &str = r#"
struct Box<T> { value: T? = null }
fn main() {
    let a: Box<int> = Box {}
    let b: Box<string> = Box {}
    print("{a.value} {b.value}")
}
"#;

type PlanCheck = fn(&Workspace, &InstanceGraph, &Instance, &ConcreteBodyPlan);

/// Every concrete entry production lowering reads is load-bearing: the intact
/// plan lowers, and the same plan with one table emptied is rejected instead
/// of being reconstructed from the source.
#[test]
fn lowering_rejects_a_plan_missing_a_concrete_entry() {
    struct Case {
        label: &'static str,
        src: &'static str,
        owner: &'static str,
        check: PlanCheck,
        poison: fn(&mut ConcreteBodyPlan),
        message: Option<&'static str>,
        intact_lowers: bool,
    }
    let no_check: PlanCheck = |_, _, _, _| {};
    let cases = [
        Case {
            label: "nested type-node entry",
            src: r#"
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
            owner: "main",
            check: |ws, _, main, plan| {
                let inf = ws
                    .inference(main.func.module)
                    .and_then(|ir| ir.per_fn.get(&BodyOwner::Function(main.func.func)))
                    .expect("main has no inference");
                let mut nested = Vec::new();
                for ty in inf.expr_types.values().chain(inf.binding_types.values()) {
                    collect_nested_type_nodes(ws, inf, *ty, &mut nested);
                }
                assert!(!nested.is_empty(), "fixture exposed no nested types");
                for ty in nested {
                    assert!(
                        plan.types.ty_types.contains_key(&ty),
                        "concrete plan omitted nested type node {ty:?}"
                    );
                }
            },
            poison: |plan| {
                let first = *plan
                    .types
                    .ty_types
                    .keys()
                    .next()
                    .expect("concrete plan has no type nodes");
                plan.types.ty_types.remove(&first);
            },
            message: None,
            intact_lowers: true,
        },
        // Replacing the plan's child list with a source-shaped adapter fails
        // at the inner application.
        Case {
            label: "expression table of a nested application",
            src: r#"
fn keep<T>(x: T) -> T { return x }
fn main() {
    let xs: [[int]] = []
    let out = keep(xs)
}
"#,
            owner: "keep",
            check: |_, g, _, plan| {
                let nested = plan
                    .types
                    .expr_types
                    .values()
                    .chain(plan.types.binding_types.values())
                    .map(|id| g.interner().get(*id))
                    .any(|ty| matches!(ty, MonoTy::App { params, .. } if matches!(params.first(), Some(MonoTy::App { .. }))));
                assert!(nested, "the concrete plan lost the nested application tree");
            },
            poison: |plan| plan.types.expr_types = ArenaMap::default(),
            message: None,
            intact_lowers: true,
        },
        Case {
            label: "callee site map",
            src: ID_CALL_SOURCE,
            owner: "main",
            check: |_, _, _, plan| {
                assert!(!plan.callees.is_empty(), "fixture has no concrete sites");
            },
            poison: |plan| plan.callees.clear(),
            message: None,
            intact_lowers: true,
        },
        Case {
            label: "explicitly untyped slot marker",
            src: r#"
fn main() {
    let value = null
    print("{value}")
}
"#,
            owner: "main",
            check: |ws, _, main, plan| {
                let body =
                    &ws.module(main.func.module).bodies[&BodyOwner::Function(main.func.func)];
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
            },
            poison: |plan| plan.types.untyped_exprs.clear(),
            message: Some("missing concrete type node"),
            // The intact plan fails later, on printing a null; the poisoned
            // one must fail earlier, on the missing marker.
            intact_lowers: false,
        },
        Case {
            label: "struct-default type nodes",
            src: r#"
struct Config { scale: float = 2.0, count: int = 3 }
fn main() {
    let c = Config { count: 4 }
    print("{c.scale}")
}
"#,
            owner: "main",
            check: no_check,
            poison: |plan| {
                plan.foreign_domains
                    .values_mut()
                    .next()
                    .expect("struct literal entered no default domain")
                    .ty_types
                    .clear();
            },
            message: Some("concrete struct-default plan is missing concrete type node"),
            intact_lowers: true,
        },
        Case {
            label: "struct-default domain reached through a call",
            src: BOX_DEFAULT_WITH_CALL,
            owner: "main",
            check: |_, g, _, plan| assert_int_and_string_default_domains(g, plan),
            poison: |plan| {
                let domain = plan.foreign_domains.keys().next().cloned().expect("domain");
                plan.foreign_domains.remove(&domain);
            },
            message: None,
            intact_lowers: true,
        },
        Case {
            label: "struct-default domain of a default without calls",
            src: BOX_DEFAULT_WITHOUT_CALL,
            owner: "main",
            check: |_, g, _, plan| assert_int_and_string_default_domains(g, plan),
            poison: |plan| {
                let domain = plan.foreign_domains.keys().next().cloned().expect("domain");
                plan.foreign_domains.remove(&domain);
            },
            message: None,
            intact_lowers: true,
        },
    ];
    for case in cases {
        let (ws, g, bodies) = bodies_of(case.src);
        let (inst, plan) = plan_named(&ws, &bodies, case.owner);
        (case.check)(&ws, &g, inst, plan);
        if case.intact_lowers {
            lower(&ws, &g, plan, &bodies.plans).unwrap_or_else(|err| {
                panic!("{}: the intact plan failed to lower: {err:?}", case.label)
            });
        }
        let mut poisoned = (**plan).clone();
        (case.poison)(&mut poisoned);
        let err = lower(&ws, &g, &poisoned, &bodies.plans).expect_err(&format!(
            "{}: lowering accepted the poisoned plan",
            case.label
        ));
        if let Some(message) = case.message {
            assert!(err.message.contains(message), "{}: {err:?}", case.label);
        }
    }
}

/// A type the source declares but the plan lacks is a lowering error, not a
/// type re-derived from the declaration.
#[test]
fn lowering_rejects_a_source_type_missing_from_the_plan() {
    type Edit = fn(&mut Workspace, ModuleId, &Instance, TyId);
    let cases: [(&str, &str, &str, Edit); 2] = [
        (
            "declared binding type",
            "fn main() {\n    let value: int = 1\n    print(\"{value}\")\n}",
            "main",
            |ws, mid, inst, extra| {
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
                body.binding_arena[binding].declared_ty = Some(extra);
            },
        ),
        (
            "signature parameter type",
            "fn id(value: int) -> int { return value }\nfn main() { print(\"{id(1)}\") }",
            "id",
            |ws, mid, inst, extra| {
                ws.modules[mid.index()].functions[inst.func.func].param_decls[0].ty = extra;
            },
        ),
    ];
    for (label, src, owner, edit) in cases {
        let (mut ws, g, bodies) = bodies_of(src);
        let mid = module_id(&ws, "<test>");
        let (inst, plan) = plan_named(&ws, &bodies, owner);
        let (inst, plan) = (inst.clone(), plan.clone());
        let extra = ws.modules[mid.index()]
            .ty_arena
            .alloc(TypeExpr::Builtin(bone_semantic::hir::BuiltinTy::Float));
        edit(&mut ws, mid, &inst, TyId::Static(mid, extra));
        let err = lower(&ws, &g, &plan, &bodies.plans).expect_err(&format!(
            "{label}: lowering accepted a type absent from the plan"
        ));
        assert!(
            err.message.contains("missing concrete type node"),
            "{label}: {err:?}"
        );
    }
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
    let (_, plan) = plan_named(&ws, &bodies, "make");
    let mir =
        lower(&ws, &g, plan, &bodies.plans).expect("plan-driven struct literal lowering failed");
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
    let (_, main_plan) = plan_named(&ws, &bodies, "main");
    let callee = callee_named(&ws, main_plan, "id");
    lower(&ws, &g, main_plan, &bodies.plans)
        .expect("call lowering rejected the intact selected callee plan");
    let mut plans = (*bodies.plans).clone();
    assert!(
        plans.remove(&callee).is_some(),
        "the fixture has no selected id plan"
    );
    assert!(
        lower(&ws, &g, main_plan, &plans).is_err(),
        "call lowering fell back after the selected callee ABI plan was removed"
    );
}

/// A scalar-ABI call whose selected parameter slots are gone rejects instead
/// of boxing, for a direct call and for a safe-nav UFCS receiver alike.
#[test]
fn production_scalar_call_rejects_a_missing_selected_parameter_slot() {
    let cases = [
        (
            "direct",
            r#"
nobox value V2 { x: float, y: float }
@scalar_abi fn id(v: V2) -> V2 { return v }
fn main() {
    let v = id(V2 { x: 1.0, y: 2.0 })
    print("{v.x}")
}
"#,
            "no ABI slot",
            false,
        ),
        (
            "ufcs",
            r#"
nobox value V2 { x: float, y: float }
@scalar_abi fn id(v: V2) -> V2 { return v }
fn main() {
    let v: V2? = V2 { x: 1.0, y: 2.0 }
    let out = v?.id()
    print("{out?.x ?? 0.0}")
}
"#,
            "UFCS receiver has no ABI slot 0",
            true,
        ),
    ];
    for (label, src, message, spanned) in cases {
        let (ws, g, bodies) = bodies_of(src);
        let (_, main_plan) = plan_named(&ws, &bodies, "main");
        let callee = callee_named(&ws, main_plan, "id");
        lower(&ws, &g, main_plan, &bodies.plans)
            .unwrap_or_else(|err| panic!("{label}: the intact scalar call must lower: {err:?}"));

        let mut caller = (**main_plan).clone();
        let site = call_site(main_plan, &callee);
        let recorded = caller.callees.get_mut(site).expect("call disappeared");
        let signature = Arc::make_mut(&mut recorded.signature);
        signature.params.clear();
        signature.param_tuple_spelled.clear();
        let plans = with_callee_signature(&bodies, &callee, |signature| {
            signature.params.clear();
            signature.param_tuple_spelled.clear();
        });
        let err = lower(&ws, &g, &caller, &plans)
            .expect_err(&format!("{label}: a missing ABI slot must reject"));
        assert!(err.message.contains(message), "{label}: {err:?}");
        if spanned {
            assert!(
                !err.labels.is_empty(),
                "{label}: the diagnostic lost its span"
            );
        }
    }
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
    let (_, main_plan) = plan_named(&ws, &bodies, "main");
    let callee = callee_named(&ws, main_plan, "variadic");
    let variadic_sites: Vec<_> = main_plan
        .callees
        .values()
        .filter(|recorded| recorded.instance == callee)
        .collect();
    assert_eq!(
        variadic_sites.len(),
        2,
        "fixture lost its direct or UFCS call"
    );
    for recorded in variadic_sites {
        assert_eq!(
            recorded.signature.params.len(),
            3,
            "fixture did not leave surplus arguments to the declared tail slot"
        );
    }
    lower(&ws, &g, main_plan, &bodies.plans).expect("direct and UFCS variadic calls did not lower");
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
    let (main, plan) = plan_named(&ws, &bodies, "main");
    let (main, plan) = (main.clone(), plan.clone());
    let body = ws.modules[main.func.module.index()]
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

    let err = lower(&ws, &g, &plan, &bodies.plans)
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
    let (ws, g, bodies) = bodies_of(ID_CALL_SOURCE);
    let (_, main_plan) = plan_named(&ws, &bodies, "main");
    let callee = callee_named(&ws, main_plan, "id");
    assert_eq!(
        bodies.plans[&callee].signature.params.len(),
        1,
        "callee signature has no parameter population"
    );
    let float = g.interner().intern(MonoTy::Float);
    let mut caller = (**main_plan).clone();
    let recorded = caller
        .callees
        .get_mut(call_site(main_plan, &callee))
        .expect("id call disappeared");
    Arc::make_mut(&mut recorded.signature).params[0] = float;
    let plans = with_callee_signature(&bodies, &callee, |signature| signature.params[0] = float);
    let mir = lower(&ws, &g, &caller, &plans).expect("plan-driven call lowering failed");
    assert!(
        count_int_to_float(&mir) > 0,
        "call lowering ignored the selected callee's concrete parameter signature"
    );
}

#[test]
fn production_call_rejects_a_callee_signature_body_disagreement() {
    let (ws, g, bodies) = bodies_of(ID_CALL_SOURCE);
    let (_, main_plan) = plan_named(&ws, &bodies, "main");
    let callee = callee_named(&ws, main_plan, "id");
    let float = g.interner().intern(MonoTy::Float);
    let plans = with_callee_signature(&bodies, &callee, |signature| signature.params[0] = float);
    let err = lower(&ws, &g, main_plan, &plans)
        .expect_err("lowering accepted a mismatched callee body plan");
    assert!(
        err.message
            .contains("concrete callee signature disagrees with its body plan"),
        "unexpected mismatch diagnostic: {err:?}"
    );
}

#[test]
fn production_return_coercion_consumes_the_body_signature() {
    let (ws, g, bodies) = bodies_of(ID_CALL_SOURCE);
    let (_, plan) = plan_named(&ws, &bodies, "id");
    let mut plan = (**plan).clone();
    assert!(
        plan.signature.result.is_some(),
        "body signature has no result population"
    );
    Arc::make_mut(&mut plan.signature).result = Some(g.interner().intern(MonoTy::Float));
    let mir = lower(&ws, &g, &plan, &bodies.plans).expect("plan-driven return lowering failed");
    assert!(
        count_int_to_float(&mir) > 0,
        "return lowering ignored the body's concrete result signature"
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
    let plan = user_always_plan(&ws, &bodies, false);
    assert!(
        plan.signature.result.is_some(),
        "global plan lost its concrete result type"
    );
    assert_eq!(
        plan.callees.len(),
        1,
        "always-emitted plan lost its direct call population"
    );
    lower_global(&ws, &g, plan, &bodies.plans).expect("plan-driven global lowering failed");
    let mut poisoned = plan.clone();
    poisoned.types.expr_types = ArenaMap::default();
    assert!(
        lower_global(&ws, &g, &poisoned, &bodies.plans).is_err(),
        "global lowering ignored its concrete type plan"
    );
    let mut missing_callee = plan.clone();
    missing_callee.callees.clear();
    assert!(
        lower_global(&ws, &g, &missing_callee, &bodies.plans).is_err(),
        "global lowering ignored its concrete callee plan"
    );
    let callee = plan
        .callees
        .values()
        .next()
        .expect("global call site")
        .instance
        .clone();
    let float = g.interner().intern(MonoTy::Float);
    let mismatched = with_callee_signature(&bodies, &callee, |signature| {
        signature.result = Some(float);
    });
    let err = lower_global(&ws, &g, plan, &mismatched)
        .expect_err("global lowering accepted a mismatched callee body plan");
    assert!(
        err.message
            .contains("concrete callee signature disagrees with its body plan"),
        "unexpected always-emitted mismatch diagnostic: {err:?}"
    );
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
    assert!(
        g.contributions.iter().any(|(owner, contribution)| {
            matches!(owner, instance_graph::Owner::AlwaysEmitted(..))
                && !contribution.struct_default_domains.is_empty()
        }),
        "fixture did not put a struct-default domain on an always-emitted contribution"
    );
    assert_eq!(
        user_always_plan(&ws, &bodies, false).foreign_domains.len(),
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
    let plan = user_always_plan(&ws, &bodies, true);
    assert!(
        plan.signature.params.len() >= 2,
        "fixture did not publish target and decorator parameter slots"
    );
    let mir = skull_mir::lower::lower_decorator_with_plans(
        &ws,
        &scalar_ctx(&ws, &g),
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
    let plan = user_always_plan(&ws, &bodies, false);
    let intact = lower_global(&ws, &g, plan, &bodies.plans)
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
    Arc::make_mut(&mut poisoned.signature).result = Some(g.interner().intern(MonoTy::Float));
    let lowered = lower_global(&ws, &g, &poisoned, &bodies.plans)
        .expect("poisoned global plan failed")
        .expect("poisoned global initializer disappeared");
    assert_eq!(
        return_arity(&lowered),
        1,
        "global lowering reconstructed its declared result shape"
    );
}

#[test]
fn always_emitted_plan_retains_a_failed_callee_site() {
    let (ws, g, bodies) = project_bodies_of(
        r#"
fn pick<T>() -> int {
    let unused: T = 1
    return 1
}
global G: int = pick()
fn main() { let x = G }
"#,
    );
    let plan = user_always_plan(&ws, &bodies, false);
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
    let err = lower_global(&ws, &g, plan, &bodies.plans)
        .expect_err("lowering ignored the graph's failed always-emitted site");
    assert!(
        err.message.contains("unresolved callee site"),
        "failed site lost its lowering diagnostic: {err:?}"
    );
}

/// One generic HIR body needs different concrete type tables for each instance.
#[test]
fn every_expanded_instance_has_a_fully_concrete_per_instance_body() {
    let (ws, g, bodies) = bodies_of(
        r#"
fn ident<T>(x: T) -> T { return x }
fn main() {
    let a = ident(1)
    let b = ident("s")
}
"#,
    );
    for i in g.expanded() {
        assert!(
            bodies.plans.contains_key(i),
            "expanded instance `{}` has no ConcreteBody",
            g.identity(&ws, i)
        );
    }

    let insts: Vec<&Instance> = bodies
        .plans
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
    // Named, not merely different: the slots carry `int` on one side and
    // `string` on the other, which is what the substitution was supposed to
    // supply.
    let rendered: Vec<(String, String)> = shared
        .iter()
        .filter(|(_, x, y)| x != y)
        .map(|(_, x, y)| (g.interner().render(*x), g.interner().render(*y)))
        .collect();
    assert!(
        !rendered.is_empty(),
        "both instances of one generic body agree on every slot, so the tables are \
         not instance-specific: {shared:?}"
    );
    assert!(
        rendered
            .iter()
            .any(|(x, y)| (x == "int" && y == "string") || (x == "string" && y == "int")),
        "the disagreement is not the instantiation: {rendered:?}"
    );
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
    let owner = BodyOwner::Function(inst.func.func);
    ws.module_mut(inst.func.module)
        .bodies
        .get_mut(&owner)
        .expect("plain has no body")
        .generation += 1;

    let stale_graph = graph_of(&ws);
    assert!(
        stale_graph.expanded().all(|i| i.func != inst.func),
        "the instance graph expanded a body from stale inference"
    );

    let bodies = concrete_body::build(&ws, &g);

    assert!(
        !bodies.plans.contains_key(&inst),
        "stale inference produced a ConcreteBody"
    );
}

/// Perturbing only the CONSUMER's
/// substitution -- the table builder's, never the walk's -- leaves a generic
/// body's slots unbound. A symmetric perturbation could not distinguish this
/// arm from a dead one.
#[test]
fn an_empty_substitution_leaves_the_generic_slots_unbound() {
    let ws = ws_with_stdlib(
        r#"
fn ident<T>(x: T) -> T { return x }
fn main() { let a = ident(1) }
"#,
    );
    let g = graph_of(&ws);
    let inst = expanded_with_prefix(&ws, &g, "ident");
    let owner = BodyOwner::Function(inst.func.func);
    let inf = ws
        .inference(inst.func.module)
        .and_then(|ir| ir.per_fn.get(&owner))
        .expect("ident has no inference");
    let body = &ws.module(inst.func.module).bodies[&owner];

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
    assert!(with.expr_types.values().next().is_some());

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
    assert!(
        without
            .failures
            .iter()
            .any(|f| matches!(f.slot, Slot::Expr(_))),
        "no expression slot failed, so only bindings were examined"
    );
}

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
    print("{r.x}")
}
"#;

/// Workers finish in whatever order the scheduler hands them, so the published
/// plans must not depend on it: a build on one worker and builds on several
/// agree on every ordinary and always-emitted plan.
#[test]
fn concrete_tables_are_identical_across_worker_counts() {
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
    for src in [src, RICH] {
        let ws = ws_with_stdlib(src);
        // One graph for every build: raw type IDs are arena-local.
        let graph = graph_of(&ws);
        let build_with = |threads| {
            rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .expect("test pool")
                .install(|| concrete_body::build(&ws, &graph))
        };
        let serial = build_with(1);
        assert!(serial.plans.len() > 1, "fixture exposed too few plans");
        assert!(
            !serial.always_plans.is_empty(),
            "fixture exposed no always-emitted plans"
        );
        for threads in [2, 8] {
            let parallel = build_with(threads);
            assert_eq!(*serial.plans, *parallel.plans, "{threads} workers");
            assert_eq!(serial.always_plans, parallel.always_plans);
        }
    }
}

#[test]
fn fresh_publication_publishes_the_authoritative_candidate_index() {
    let ws = ws_with_stdlib(RICH);
    let bodies = concrete_body::build(&ws, &graph_of(&ws));
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
        .find(|instance| fn_name(&ws1, instance.func) == "bad")
        .cloned()
        .expect("bad instance");
    assert!(
        graph.index.subst.remove(&bad).is_some(),
        "fixture did not expose bad's substitution"
    );
    let mut local = concrete_body::build(&ws1, &graph);
    assert!(local.first_plan_failure().is_some());
    assert!(!local.plans.contains_key(&bad));
    assert!(local.first_always_plan_failure().is_some());

    let ws2 = ws_with_stdlib(source_v2);
    let main = body_id(&ws2, "main");
    let owners = [
        main,
        (main.0, BodyOwner::Global(Idx::from_raw(0u32.into()))),
    ];
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
    let fresh = concrete_body::build(&ws2, &graph);
    assert_eq!(local.plans, fresh.plans);
    assert_eq!(local.always_plans, fresh.always_plans);
    assert!(local.first_plan_failure().is_none());
    assert!(local.first_always_plan_failure().is_none());
    assert!(fresh.first_plan_failure().is_none());
    assert!(fresh.first_always_plan_failure().is_none());
}

#[test]
fn local_plans_replace_a_retracted_generic_callee() {
    let source = |argument| {
        format!(
            "fn echo<T>(x: T) -> T {{ return x }}\n\
             fn main() {{ let value = echo({argument}); print(\"{{value}}\") }}"
        )
    };
    let ws1 = ws_with_stdlib(&source("1"));
    let mut graph = graph_of(&ws1);
    let mut local = concrete_body::build(&ws1, &graph);
    let old_echo = local
        .plans
        .keys()
        .find(|instance| fn_name(&ws1, instance.func) == "echo")
        .expect("echo<int> plan")
        .clone();

    let ws2 = ws_with_stdlib(&source("\"changed\""));
    let main = body_id(&ws2, "main");
    let update = graph.update_bodies_with_root_delta(&ws2, &[], &[], &[main], &[]);
    assert!(update.removed.contains(&old_echo));
    local.update_local(
        &ws2,
        &graph,
        &update.revisited,
        &update.removed,
        &update.revisited_owners,
    );
    let fresh = concrete_body::build(&ws2, &graph);
    assert_eq!(local.plans, fresh.plans);
    assert!(!local.plans.contains_key(&old_echo));
    assert_eq!(
        local
            .plans
            .keys()
            .filter(|instance| fn_name(&ws2, instance.func) == "echo")
            .count(),
        1
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
        .plans
        .keys()
        .filter(|i| g.identity(&ws, i).starts_with("wrap"))
        .collect();
    assert_eq!(
        wraps.len(),
        2,
        "expected two `wrap` instances, got {wraps:?}"
    );
    let (a, b) = (&bodies.plans[wraps[0]], &bodies.plans[wraps[1]]);
    let shared = a
        .callees
        .iter()
        .filter_map(|(site, x)| b.callees.get(site).map(|y| (&x.instance, &y.instance)))
        .collect::<Vec<_>>();
    assert!(!shared.is_empty(), "the two wrap plans share no call site");
    let named = shared
        .iter()
        .filter(|(x, y)| x != y)
        .map(|(x, y)| (g.identity(&ws, x), g.identity(&ws, y)))
        .collect::<Vec<_>>();
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
        .plans
        .keys()
        .find(|i| g.identity(&ws, i).starts_with("pull"))
        .expect("no concrete `pull` instance");
    let pull_calls = bodies.plans[pull]
        .callees
        .iter()
        .filter(|(site, _)| matches!(site.op, SiteOp::DirectCall))
        .map(|(_, callee)| g.identity(&ws, &callee.instance))
        .collect::<Vec<_>>();
    assert!(
        pull_calls
            .iter()
            .any(|n| n.starts_with("next_VecIter") && n.contains("int")),
        "the explicit bounded call did not resolve inside `pull`: {pull_calls:?}"
    );

    let sum_it = bodies
        .plans
        .keys()
        .find(|i| g.identity(&ws, i).starts_with("sum_it"))
        .expect("no concrete `sum_it` instance");
    let loop_nexts = bodies.plans[sum_it]
        .callees
        .iter()
        .filter(|(site, _)| matches!(site.op, SiteOp::ForNext(_)))
        .map(|(_, callee)| g.identity(&ws, &callee.instance))
        .collect::<Vec<_>>();
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
    let mut g = graph_of(&ws);
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
                ModuleId::builtin(),
                BodyOwner::Global(Idx::from_raw(0u32.into())),
            ),
            instance_graph::InstanceBoundTree::default(),
        );

    let bodies = concrete_body::build(&ws, &g);
    assert!(
        !bodies.plans.contains_key(&inst),
        "an ambiguous body was still built"
    );
}

/// A `for` head resolves to a concrete `iter` AND a concrete `next`, and a
/// chained iterator's nested witnesses each get their own entry. Both live at
/// one expression, which is why the table is keyed by SITE.
#[test]
fn iterator_operations_resolve_to_concrete_witnesses() {
    let (ws, g, bodies) = bodies_of(RICH);
    let main = bodies
        .plans
        .keys()
        .find(|i| g.identity(&ws, i).starts_with("main@"))
        .expect("no `main` body");
    let ops = g
        .call_sites()
        .find(|(instance, _)| *instance == main)
        .map(|(_, sites)| sites)
        .expect("main has no graph sites");
    let named = ops
        .iter()
        .filter_map(|(site, record)| {
            let SiteOutcome::Resolved(instance) = &record.outcome else {
                return None;
            };
            Some((&site.op, g.identity(&ws, instance)))
        })
        .collect::<Vec<_>>();
    // Named, not merely present: the `for` over `[int]` resolves `next` at
    // `VecIter<int>`, so the ELEMENT type is in the callee's identity.
    assert!(
        named.iter().any(
            |(k, n)| matches!(k, SiteOp::ForIter | SiteOp::ImplicitIter) && n.contains("p_int")
        ),
        "no concrete `iter` at the int element: {named:?}"
    );
    assert!(
        named
            .iter()
            .any(|(k, n)| matches!(k, SiteOp::ForNext(_)) && n.starts_with("next_VecIter")),
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
