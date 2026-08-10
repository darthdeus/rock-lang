//! `ConcreteBody` type tables (`docs/monomorphization-plan.md` S 9, S 15a).
//!
//! The property under test is not "a table exists". It is that the table is a
//! function of the INSTANCE: one generic body reached at two instantiations has
//! two tables that disagree on the same slot, and neither carries a free
//! generic. A table built under an empty substitution would fail every generic
//! slot instead, which is what the mutation arm at the bottom asserts.

use std::sync::LazyLock;

use bone_semantic::hir::{BindingInfo, BodyOwner, Expr, TypeExpr};
use bone_semantic::inference::Workspace;
use comfy_la_arena::{ArenaMap, Idx};
use skull::concrete_body::{self, Callee, ConcreteBodies, ConcreteBody, IteratorOpKind, Slot};
use skull::instance_graph::{self, InstanceGraph, Site, SiteOp, SiteOutcome};
use skull_mir::monoty::{MonoTy, MonoTyId, MonoTyReason};
use skull_mir::mir::{Inst, MirFunc};
use ustr::Ustr;

static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
    rock_host::stdlib_loader::stdlib_files()
        .iter()
        .map(|f| {
            let id = Ustr::from(format!("<stdlib:{}>", f.name).as_str());
            skull_parser::parse_module(f.contents, id).expect("stdlib parse")
        })
        .collect()
});

fn check(src: &str) -> Workspace {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module(src, Ustr::from("<test>")).expect("parse"));
    bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass")
}

fn bodies_of(src: &str) -> (Workspace, InstanceGraph, ConcreteBodies) {
    let ws = check(src);
    let roots = skull::mir_emit::emission_roots(&ws, None);
    let mut g = instance_graph::build(&ws, roots, false);
    let bodies = concrete_body::build(&ws, &mut g);
    (ws, g, bodies)
}

fn assert_arena_tables(body: &ConcreteBody) {
    let _: &ArenaMap<Idx<Expr>, MonoTyId> = &body.expr_types;
    let _: &ArenaMap<Idx<BindingInfo>, MonoTyId> = &body.binding_types;
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
    assert!(!unique.is_empty(), "fixture exposed no nested type population");
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
    let mut poisoned = plan.clone();
    poisoned.types.ty_types.remove(&missing);
    let scx = skull_mir::lower::ScalarCtx::new(&ws).sharing(g.interner().clone());
    assert!(
        skull_mir::lower::lower_instance_with_plan(&ws, &scx, &poisoned).is_err(),
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
    assert!(nested.is_some(), "the concrete plan lost the nested application tree");

    let scx = skull_mir::lower::ScalarCtx::new(&ws).sharing(g.interner().clone());
    skull_mir::lower::lower_instance_with_plan(&ws, &scx, plan)
        .expect("plan-driven lowering failed");

    let mut poisoned = plan.clone();
    poisoned.types.expr_types = ArenaMap::default();
    assert!(
        skull_mir::lower::lower_instance_with_plan(&ws, &scx, &poisoned).is_err(),
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
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("test module");
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
    body.binding_arena[binding].declared_ty = Some(bone_semantic::prelude::TyId::Static(mid, extra));

    let scx = skull_mir::lower::ScalarCtx::new(&ws).sharing(g.interner().clone());
    let err = skull_mir::lower::lower_instance_with_plan(&ws, &scx, &plan)
        .expect_err("lowering accepted a declaration type absent from its concrete plan");
    assert!(err.message.contains("missing concrete type node"), "{err:?}");
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
        .find(|inst| ws.module(inst.func.module).functions[inst.func.func].name.as_str() == "make")
        .cloned()
        .expect("make has no concrete plan");
    let plan = bodies.plans.get(&make).expect("make plan disappeared");
    let scx = skull_mir::lower::ScalarCtx::new(&ws).sharing(g.interner().clone());
    let mir = skull_mir::lower::lower_instance_with_plan(&ws, &scx, plan)
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
    let main_plan = bodies
        .plans
        .iter()
        .find(|(inst, _)| ws.module(inst.func.module).functions[inst.func.func].name.as_str() == "main")
        .map(|(_, plan)| plan)
        .expect("main has no concrete plan");
    let callee = main_plan
        .callees
        .values()
        .find(|inst| ws.module(inst.func.module).functions[inst.func.func].name.as_str() == "id")
        .cloned()
        .expect("main has no concrete id call");
    let scx = skull_mir::lower::ScalarCtx::new(&ws).sharing(g.interner().clone());
    skull_mir::lower::lower_instance_with_plans(&ws, &scx, main_plan, &bodies.plans)
        .expect("call lowering rejected the intact selected callee plan");
    let mut plans = bodies.plans.clone();
    assert!(plans.remove(&callee).is_some(), "the fixture has no selected id plan");
    assert!(
        skull_mir::lower::lower_instance_with_plans(&ws, &scx, main_plan, &plans).is_err(),
        "call lowering fell back after the selected callee ABI plan was removed"
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
    assert!(!main_plan.callees.is_empty(), "fixture has no concrete site population");
    let mut poisoned = main_plan.clone();
    poisoned.callees.clear();
    let scx = skull_mir::lower::ScalarCtx::new(&ws).sharing(g.interner().clone());
    assert!(
        skull_mir::lower::lower_instance_with_plans(&ws, &scx, &poisoned, &bodies.plans).is_err(),
        "lowering reconstructed a callee identity after its plan site was removed"
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
    let callee = main_plan
        .callees
        .values()
        .find(|inst| ws.module(inst.func.module).functions[inst.func.func].name.as_str() == "id")
        .cloned()
        .expect("main has no concrete id call");
    let callee_plan = bodies.plans.get(&callee).expect("id has no concrete plan");
    assert_eq!(
        callee_plan.signature.params.len(),
        1,
        "callee signature has no parameter population"
    );
    let float = g.interner().intern(MonoTy::Float);
    let mut plans = bodies.plans.clone();
    plans
        .get_mut(&callee)
        .expect("selected callee plan disappeared")
        .signature
        .params[0] = float;
    let scx = skull_mir::lower::ScalarCtx::new(&ws).sharing(g.interner().clone());
    let mir = skull_mir::lower::lower_instance_with_plans(&ws, &scx, main_plan, &plans)
        .expect("plan-driven call lowering failed");
    assert!(
        count_int_to_float(&mir) > 0,
        "call lowering ignored the selected callee's concrete parameter signature"
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
        .find(|inst| ws.module(inst.func.module).functions[inst.func.func].name.as_str() == "id")
        .cloned()
        .expect("id has no concrete plan");
    let mut plan = bodies.plans.get(&callee).expect("id plan disappeared").clone();
    assert!(plan.signature.result.is_some(), "body signature has no result population");
    plan.signature.result = Some(g.interner().intern(MonoTy::Float));
    let scx = skull_mir::lower::ScalarCtx::new(&ws).sharing(g.interner().clone());
    let mir = skull_mir::lower::lower_instance_with_plan(&ws, &scx, &plan)
        .expect("plan-driven return lowering failed");
    assert!(
        count_int_to_float(&mir) > 0,
        "return lowering ignored the body's concrete result signature"
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
    let main = bodies
        .plans
        .iter()
        .find(|(inst, _)| g.identity(&ws, inst).starts_with("main"))
        .map(|(_, plan)| plan)
        .expect("main has no concrete plan");
    assert_eq!(main.foreign_domains.len(), 2, "expected both Box instantiation domains");
    let mut rendered = main
        .foreign_domains
        .values()
        .flat_map(|tables| tables.expr_types.values())
        .map(|id| g.interner().render(*id))
        .collect::<Vec<_>>();
    rendered.sort();
    assert!(rendered.iter().any(|ty| ty == "int?"), "missing int default table: {rendered:?}");
    assert!(rendered.iter().any(|ty| ty == "string?"), "missing string default table: {rendered:?}");

    let mut poisoned = main.clone();
    let domain = poisoned.foreign_domains.keys().next().cloned().expect("domain");
    poisoned.foreign_domains.remove(&domain);
    let scx = skull_mir::lower::ScalarCtx::new(&ws).sharing(g.interner().clone());
    assert!(
        skull_mir::lower::lower_instance_with_plan(&ws, &scx, &poisoned).is_err(),
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
    assert!(rendered.iter().any(|ty| ty == "int?"), "missing int default table: {rendered:?}");
    assert!(
        rendered.iter().any(|ty| ty == "string?"),
        "missing string default table: {rendered:?}"
    );
    let domain = main.foreign_domains.keys().next().cloned().expect("default domain");
    let mut poisoned = main.clone();
    poisoned.foreign_domains.remove(&domain);
    let scx = skull_mir::lower::ScalarCtx::new(&ws).sharing(g.interner().clone());
    assert!(
        skull_mir::lower::lower_instance_with_plan(&ws, &scx, &poisoned).is_err(),
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
    let user_mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("test module");
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
    assert!(plan.signature.result.is_some(), "global plan lost its concrete result type");
    assert_eq!(
        plan.callees.len(),
        1,
        "always-emitted plan lost its direct call population"
    );
    let scx = skull_mir::lower::ScalarCtx::new(&ws).sharing(g.interner().clone());
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
    let _ = owner;
}

#[test]
fn always_emitted_plan_retains_a_failed_callee_site() {
    let ws = check(
        r#"
fn pick<T>() -> int { return 1 }
global G: int = pick()
fn main() { let x = G }
"#,
    );
    let user_mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("test module");
    let roots = skull::mir_emit::emission_roots(&ws, Some("<project>"));
    let mut g = instance_graph::build(&ws, roots, false);
    let bodies = concrete_body::build(&ws, &mut g);
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
    assert_eq!(plan.callees.len(), 0, "failed call was fabricated as resolved");
    assert_eq!(plan.failed_sites.len(), 1, "failed site was dropped from the plan");
    assert_eq!(plan.failed_sites.values().next(), Some(&"unconstrained-type-var"));
    let scx = skull_mir::lower::ScalarCtx::new(&ws).sharing(g.interner().clone());
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
    assert!(c.expr_examined > 0, "no expression slots were converted: {c:?}");
    assert!(c.binding_examined > 0, "no binding slots were converted: {c:?}");
    assert_eq!(
        bodies.free_types.len(),
        0,
        "free types in supposedly concrete bodies, over {} expr and {} binding slots: {:?}",
        c.expr_examined,
        c.binding_examined,
        bodies.render_free_types(&ws, &g).iter().take(10).collect::<Vec<_>>()
    );
    assert_eq!(bodies.gaps.len(), 0, "expanded instances with no body: {:?}", c.gaps);
    // The zero above is only as strong as the space it was measured over: a
    // body that never consults its substitution reports no free types for the
    // trivial reason. These say the substitution was actually load-bearing.
    assert!(c.generic_bodies > 0, "no body was built under a non-empty substitution: {c:?}");
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
    assert_eq!(insts.len(), 2, "expected two ident instances, got {insts:?}");
    let (a, b) = (&bodies.bodies[insts[0]], &bodies.bodies[insts[1]]);
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
        rendered.iter().any(|(x, y)| (x == "int" && y == "string") || (x == "string" && y == "int")),
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
        .bodies
        .keys()
        .find(|i| g.identity(&ws, i).starts_with("plain"))
        .map(|i| &bodies.bodies[i])
        .expect("`plain` has no ConcreteBody");
    assert_arena_tables(found);
    assert!(found.expr_types.values().next().is_some(), "`plain`'s table is empty");
    assert_eq!(bodies.free_types.len(), 0);
}

#[test]
fn stale_inference_cannot_build_a_concrete_body() {
    let mut ws = check(
        r#"
fn plain(x: int) -> int { return x + 1 }
fn main() { let a = plain(1) }
"#,
    );
    let roots = skull::mir_emit::emission_roots(&ws, None);
    let mut g = instance_graph::build(&ws, roots, false);
    let inst = g
        .expanded()
        .find(|i| g.identity(&ws, i).starts_with("plain"))
        .cloned()
        .expect("no plain instance was expanded");
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

    let bodies = concrete_body::build(&ws, &mut g);

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
    let ws = check(src);
    let roots = skull::mir_emit::emission_roots(&ws, None);
    let g = instance_graph::build(&ws, roots, false);
    let inst = g
        .expanded()
        .find(|i| g.identity(&ws, i).starts_with("ident"))
        .cloned()
        .expect("no ident instance was expanded");

    let module = ws.module(inst.func.module);
    let inf = ws
        .inference(inst.func.module)
        .and_then(|ir| ir.per_fn.get(&bone_semantic::hir::BodyOwner::Function(inst.func.func)))
        .expect("ident has no inference");
    let body = module
        .bodies
        .get(&bone_semantic::hir::BodyOwner::Function(inst.func.func))
        .expect("ident has no body");

    let subst = g.substitution(&inst).expect("ident instance kept no substitution").clone();
    let with = g.interner().with_mut(|a| concrete_body::convert_body(a, &ws, inf, body, &subst));
    assert_eq!(with.failures.len(), 0, "the real substitution left slots unbound: {:?}", with.failures);
    assert!(with.examined() > 0);

    let without =
        g.interner().with_mut(|a| concrete_body::convert_body(a, &ws, inf, body, &Default::default()));
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
        without.failures.iter().map(|f| f.reason.bucket()).collect::<Vec<_>>()
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
        without.failures.iter().any(|f| matches!(f.slot, Slot::Expr(_))),
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
        .get(&bone_semantic::hir::BodyOwner::Function(body_inst.func.func))
        .expect("`body` has no HIR body");
    let mut annotated_targets = Vec::new();
    let mut inferred_targets = Vec::new();
    for (_, expr) in hir_body.expr_arena.iter() {
        let Expr::Assign { target, .. } = expr else { continue };
        let Expr::Var(binding) = hir_body.expr_arena[*target] else { continue };
        match hir_body.binding_arena[binding].name.as_str() {
            "annotated" => annotated_targets.push(*target),
            "inferred" => inferred_targets.push(*target),
            _ => {}
        }
    }
    assert_eq!(annotated_targets.len(), 2, "fixture has the wrong annotated targets");
    assert_eq!(inferred_targets.len(), 2, "fixture has the wrong inferred targets");
    assert!(
        annotated_targets
            .iter()
            .all(|target| !bodies.bodies[body_inst].expr_types.contains_idx(*target)),
        "annotated assignment targets are no longer untyped; flip this sentinel"
    );
    assert!(
        inferred_targets
            .iter()
            .all(|target| bodies.bodies[body_inst].expr_types.contains_idx(*target)),
        "the inferred assignment control became untyped"
    );
    assert!(
        bodies.bodies[body_inst].expr_types.values().count() > 20,
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

/// Every recorded site is accounted for EXACTLY once, and the accounting is
/// stated as an identity rather than as four independent non-zero counts: a
/// site that silently reached no table would leave the totals short while every
/// individual count still looked healthy.
#[test]
fn every_recorded_operation_becomes_one_entry_a_hole_or_a_rejection() {
    let (ws, g, bodies) = bodies_of(RICH);
    let c = bodies.census();
    assert!(c.sites_examined > 0, "no callee sites were examined at all: {c:?}");
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
    assert!(bodies.gaps.is_empty(), "an expanded instance got no body: {:?}", c.gaps);
    // Each table has a population, so the identity above is not satisfied by
    // one table carrying everything.
    assert!(c.direct_calls > 0, "no direct call was tabulated: {c:?}");
    assert!(c.fn_values > 0, "no function value was tabulated: {c:?}");
    assert!(c.synthesized_calls > 0, "no synthesized call was tabulated: {c:?}");
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
            let Callee::Boundary(inst, class) = callee else { continue };
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

/// The callee table is a function of the INSTANCE. `wrap<int>` and
/// `wrap<string>` share one HIR body, so the same `Idx<Expr>` must name two
/// DIFFERENT callee instances -- which is what makes a substitution unnecessary
/// downstream. A table built per FUNCTION fails exactly here.
#[test]
fn one_generic_body_names_a_different_callee_per_instance() {
    let (ws, g, bodies) = bodies_of(RICH);
    let wraps: Vec<&skull::instance_graph::Instance> =
        bodies.bodies.keys().filter(|i| g.identity(&ws, i).starts_with("wrap")).collect();
    assert_eq!(wraps.len(), 2, "expected two `wrap` instances, got {wraps:?}");
    let (a, b) = (&bodies.bodies[wraps[0]], &bodies.bodies[wraps[1]]);
    let shared: Vec<(skull::instance_graph::Site, &Callee, &Callee)> = a
        .direct_calls
        .iter()
        .filter_map(|(s, x)| b.direct_calls.get(s).map(|y| (s.clone(), x, y)))
        .collect();
    assert!(!shared.is_empty(), "the two `wrap` bodies share no call site");
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
        pull_calls.iter().any(|n| n.starts_with("next_VecIter") && n.contains("int")),
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
        loop_nexts.iter().any(|n| n.starts_with("next_VecIter") && n.contains("int")),
        "the caller-generic loop witness did not resolve inside `sum_it`: {loop_nexts:?}"
    );
}

#[test]
fn disagreeing_bound_trees_cannot_choose_a_body_by_discovery_order() {
    let ws = check(
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
    let inst = g
        .expanded()
        .find(|i| g.identity(&ws, i).starts_with("sum_it"))
        .cloned()
        .expect("no concrete `sum_it` instance");
    let recorded = g.bound_tree(&inst).cloned().expect("no witness tree");
    assert!(!recorded.next.is_empty(), "fixture has no bound witness tree");
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

    let bodies = concrete_body::build(&ws, &mut g);
    assert!(!bodies.bodies.contains_key(&inst), "an ambiguous body was still built");
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
        named.iter().any(|(k, n)| *k == IteratorOpKind::Iter && n.contains("p_int")),
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
    assert!(!nested.is_empty(), "no nested conformance witness was recorded: {:?}", ops.keys());
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
    let main = bodies.bodies.keys().find(|i| g.identity(&ws, i).starts_with("main@")).expect("no `main`");
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
    assert_eq!(boxed, vec!["__rock_col_sort"], "a boxed element took the leaf arm");
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
    let untyped: Vec<&str> =
        bodies.untyped.iter().filter(|(i, _)| i == inst).map(|(_, u)| u.kind).collect();
    for kind in ["Call", "IfLet", "Range", "Var"] {
        assert!(untyped.contains(&kind), "the fixture has no untyped `{kind}`: {untyped:?}");
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
        bodies.bodies[inst].callees().iter().all(|(s, _)| s.expr != header),
        "the template-lowered `for` header has a callee entry after all"
    );
    assert_eq!(g.storage_failures().count(), 0, "a synthesized receiver had no concrete type");
}
