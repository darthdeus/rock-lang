//! `ConcreteBody` type tables (`docs/monomorphization-plan.md` S 9, S 15a).
//!
//! The property under test is not "a table exists". It is that the table is a
//! function of the INSTANCE: one generic body reached at two instantiations has
//! two tables that disagree on the same slot, and neither carries a free
//! generic. A table built under an empty substitution would fail every generic
//! slot instead, which is what the mutation arm at the bottom asserts.

use std::sync::LazyLock;

use bone_semantic::hir::{BindingInfo, Expr};
use bone_semantic::inference::Workspace;
use comfy_la_arena::{ArenaMap, Idx};
use skull::concrete_body::{self, ConcreteBodies, ConcreteBody, Slot};
use skull::instance_graph::{self, InstanceGraph};
use skull_mir::monoty::{MonoTyId, MonoTyReason};
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
    let roots = instance_graph::emission_roots(&ws, None);
    let mut g = instance_graph::build(&ws, roots, false);
    let bodies = concrete_body::build(&ws, &mut g);
    (ws, g, bodies)
}

fn assert_arena_tables(body: &ConcreteBody) {
    let _: &ArenaMap<Idx<Expr>, MonoTyId> = &body.expr_types;
    let _: &ArenaMap<Idx<BindingInfo>, MonoTyId> = &body.binding_types;
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
    for i in &g.expanded {
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
        .map(|(_, x, y)| (g.arena.render(*x), g.arena.render(*y)))
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
    let roots = instance_graph::emission_roots(&ws, None);
    let mut g = instance_graph::build(&ws, roots, false);
    let inst = g
        .expanded
        .iter()
        .find(|i| g.identity(&ws, i).starts_with("plain"))
        .cloned()
        .expect("no plain instance was expanded");
    let owner = bone_semantic::hir::BodyOwner::Function(inst.func.func);
    ws.module_mut(inst.func.module)
        .bodies
        .get_mut(&owner)
        .expect("plain has no body")
        .generation += 1;

    let stale_roots = instance_graph::emission_roots(&ws, None);
    let stale_graph = instance_graph::build(&ws, stale_roots, false);
    assert!(
        stale_graph.expanded.iter().all(|i| i.func != inst.func),
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
    let roots = instance_graph::emission_roots(&ws, None);
    let mut g = instance_graph::build(&ws, roots, false);
    let inst = g
        .expanded
        .iter()
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

    let with = concrete_body::convert_body(
        &mut g.arena,
        &ws,
        inf,
        body,
        g.substitutions.get(&inst).expect("ident instance kept no substitution"),
    );
    assert_eq!(with.failures.len(), 0, "the real substitution left slots unbound: {:?}", with.failures);
    assert!(with.examined() > 0);

    let without =
        concrete_body::convert_body(&mut g.arena, &ws, inf, body, &Default::default());
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
