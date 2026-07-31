//! Stage 0d properties for the shadow instance graph
//! (`docs/monomorphization-plan.md` S 0d, S 12, S 20g.4).

use std::sync::LazyLock;

use bone_semantic::inference::Workspace;
use skull::instance_graph::{self, EdgeKind};
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

fn graph_of(src: &str) -> (Workspace, instance_graph::InstanceGraph) {
    let ws = check(src);
    let roots = instance_graph::emission_roots(&ws, None);
    let g = instance_graph::build(&ws, roots, false);
    (ws, g)
}

/// One generic function called at two element types is TWO instances. Today's
/// `compute_call_spec` produces at most one specialization here (and usually
/// none, because `any_value` does not hold for primitives).
#[test]
fn one_generic_two_arguments_is_two_instances() {
    let (ws, g) = graph_of(
        r#"
fn ident<T>(x: T) -> T { return x }
fn main() {
    let a = ident(1)
    let b = ident("s")
}
"#,
    );
    let syms = g.sorted_symbols(&ws);
    let idents: Vec<&String> = syms.iter().filter(|s| s.contains("ident")).collect();
    assert_eq!(
        idents.len(),
        2,
        "expected ident<int> and ident<string> as distinct instances, got {idents:?}"
    );
    assert!(idents.iter().any(|s| s.ends_with("<int>")), "{idents:?}");
    assert!(idents.iter().any(|s| s.ends_with("<string>")), "{idents:?}");
}

/// Transitive instantiation: a generic caller passes its OWN binding down, so
/// the callee's instance is decided by the caller's substitution rather than by
/// the inner call site alone.
#[test]
fn instantiation_flows_through_a_generic_caller() {
    let (ws, g) = graph_of(
        r#"
fn inner<T>(x: T) -> T { return x }
fn outer<T>(x: T) -> T { return inner(x) }
fn main() {
    let a = outer(1)
    let b = outer(true)
}
"#,
    );
    let syms = g.sorted_symbols(&ws);
    for want in ["<int>", "<bool>"] {
        assert!(
            syms.iter().any(|s| s.contains("inner") && s.ends_with(want)),
            "inner was not instantiated at {want}: {:?}",
            syms.iter().filter(|s| s.contains("inner")).collect::<Vec<_>>()
        );
    }
}

/// The manifest must not depend on the order roots are discovered in
/// (plan S 20g.4).
///
/// HONEST SCOPE: this pins the property, and it does NOT reproduce the defect
/// that found it. The expansion cap was originally keyed on PATH length, which
/// made membership depend on which root reached an instance first;
/// `games/little-knights` produced two different manifests, and re-applying the
/// path cap still turns that project's `order-stable` column to NO. Every
/// synthetic fixture tried here stays green under the path cap, because the
/// orders have to diverge over a real library's transitive depth. **The corpus
/// run (`rock mono-manifest`, the per-project `order-stable` column) is the
/// oracle for this property; this test is a cheap lower bound.** Do not read it
/// as coverage of the cap.
#[test]
fn manifest_is_independent_of_discovery_order() {
    // The fixture MUST reach the expansion cap, or it proves nothing: a shallow
    // program is order-independent whatever the cap is keyed on. `grow<T>`
    // instantiates at ever-deeper types, and two roots reach it by paths of
    // different lengths, so a path-length cap expands a different set depending
    // on which root is seeded first.
    let src = r#"
fn grow<T>(x: T, n: int) -> int {
    if n <= 0 { return 0 }
    let wrapped: [T] = [x]
    return 1 + grow(wrapped, n - 1)
}
fn near() -> int { return grow(1, 20) }
fn hop1() -> int { return near() }
fn hop2() -> int { return hop1() }
fn hop3() -> int { return hop2() }
fn hop4() -> int { return hop3() }
fn hop5() -> int { return hop4() }
fn far() -> int { return hop5() }
fn main() { near(); far() }
"#;
    let ws = check(src);
    // `Some("<test>")` makes the STDLIB a library, so its generic bodies are
    // reached only transitively and at path lengths that depend on which root
    // got there first. With every module a root (the `None` form) path depth
    // cannot vary, and this test would pass under any cap -- the corpus found
    // the defect precisely because real projects have library modules.
    let roots = instance_graph::emission_roots(&ws, Some("<test>"));
    let fwd = instance_graph::build(&ws, roots.clone(), false);
    let rev = instance_graph::build(&ws, roots, true);
    assert_eq!(
        instance_graph::render_manifest(&ws, &fwd),
        instance_graph::render_manifest(&ws, &rev),
        "reversing root discovery order changed the manifest"
    );
    assert_eq!(
        instance_graph::manifest_digest(&ws, &fwd),
        instance_graph::manifest_digest(&ws, &rev)
    );
}

/// Two nominals with identical leaves must NOT collapse into one instance --
/// the S 20g.2 adversary, carried through from `MonoTy` into instance identity.
#[test]
fn same_shape_nominals_produce_distinct_instances() {
    let (ws, g) = graph_of(
        r#"
value P { x: float, y: float }
value Q { x: float, y: float }
fn ident<T>(v: T) -> T { return v }
fn main() {
    let p = ident(P { x: 1.0, y: 2.0 })
    let q = ident(Q { x: 1.0, y: 2.0 })
}
"#,
    );
    let syms = g.sorted_symbols(&ws);
    let idents: Vec<&String> = syms.iter().filter(|s| s.contains("ident")).collect();
    assert_eq!(idents.len(), 2, "P and Q must not share an instance: {idents:?}");
    assert!(idents.iter().any(|s| s.ends_with("<P>")), "{idents:?}");
    assert!(idents.iter().any(|s| s.ends_with("<Q>")), "{idents:?}");
}

/// A `for` over a user collection contributes an `iterator-next` edge with a
/// CONCRETE witness instance (plan S 12). Without reading `next_generics` the
/// witness has no instantiation and the edge is lost entirely -- which is how
/// the first version of this walk reported zero iterator edges over the whole
/// corpus while looking healthy.
#[test]
fn iterator_next_witness_is_instantiated() {
    let (_ws, g) = graph_of(
        r#"
fn main() {
    let xs: [int] = [1, 2, 3]
    let total = 0
    for v in xs.iter().map(_ + 1) { total = total + v }
}
"#,
    );
    let nexts = g.edges.iter().filter(|e| e.kind == EdgeKind::IteratorNext).count();
    assert!(nexts > 0, "no iterator-next edge was recorded");
    assert!(
        g.edges
            .iter()
            .filter(|e| e.kind == EdgeKind::IteratorNext)
            .all(|e| !e.to.args.is_empty()),
        "an iterator-next witness reached a generic callee with no instantiation"
    );
}

/// Ordinary same-instance recursion is finite and must be reported as such,
/// not as a polymorphic-recursion candidate.
#[test]
fn same_instance_recursion_is_not_polymorphic() {
    let (ws, g) = graph_of(
        r#"
fn countdown(n: int) -> int {
    if n <= 0 { return 0 }
    return countdown(n - 1)
}
fn main() { countdown(3) }
"#,
    );
    let census = g.recursion_census(&ws);
    assert!(
        census.direct_same_instance.iter().any(|s| s.contains("countdown")),
        "direct recursion not detected: {:?}",
        census.direct_same_instance
    );
    assert!(
        census.changing_instance.is_empty(),
        "same-instance recursion was misfiled as polymorphic: {:?}",
        census.changing_instance
    );
}

/// Polymorphic recursion (`f<T> -> f<[T]>`) grows the instantiation TYPE
/// without bound. It must terminate as a capped report rather than diverging --
/// and it must be filed as changing-instance, distinct from the case above.
/// The plan (S 7) records that this shape compiles and RUNS today.
#[test]
fn changing_instance_recursion_is_capped_and_reported() {
    let (ws, g) = graph_of(
        r#"
fn depth<T>(x: T, n: int) -> int {
    if n <= 0 { return 0 }
    let wrapped: [T] = [x]
    return 1 + depth(wrapped, n - 1)
}
fn main() { depth(1, 3) }
"#,
    );
    let census = g.recursion_census(&ws);
    assert!(
        !census.changing_instance.is_empty(),
        "polymorphic recursion was not reported as changing-instance"
    );
    // The walk terminated, which is the point of the cap.
    assert!(g.instances.len() < 500, "instance explosion: {}", g.instances.len());
}

/// Plan S 15a: every specialization today's conditional machinery produces must
/// appear in the shadow graph as a complete instance. Equality is NOT expected
/// -- the shadow side is strictly larger, because the old mechanism is
/// conditional -- but an absence in THAT direction means the shadow walk cannot
/// see an edge production already takes.
///
/// A miss whose CALLING body the walk never reached is classified separately:
/// that is a reachability difference (dead code, or a shared generic body no
/// instance reaches), not a missing instance. Over the whole closed world the
/// unexplained count is 0 and the classified count is 569.
#[test]
fn every_current_specialization_appears_in_the_shadow_graph() {
    let (ws, g) = graph_of(
        r#"
value P { x: float, y: float }
@inline fn ident<T>(v: T) -> T { return v }
fn main() {
    let a = ident(P { x: 1.0, y: 2.0 })
    let b = ident(1)
    let xs: [P] = [P { x: 1.0, y: 2.0 }]
    xs.push(P { x: 3.0, y: 4.0 })
}
"#,
    );
    let r = instance_graph::reconcile(&ws, &g, bone_semantic::hir::FnBackend::Lua);
    assert!(r.today > 0, "the fixture produced no specializations to reconcile");
    assert!(
        r.missing.is_empty(),
        "current specializations absent from the shadow graph: {:?}",
        r.missing
    );
    assert_eq!(r.rejected_sites, 0, "a Stage-0b invariant fired on a valid program");
    // The expected direction: the shadow graph is strictly larger.
    assert!(r.shadow_only > 0, "shadow graph added nothing over today's specializer");
}

/// A Rock function taken as a VALUE is an edge. The known-positive fixture
/// `bbc06d767` asked for: the first attempt at this edge kind searched
/// `resolved_overload_calls` and measured zero, which is indistinguishable from
/// "the corpus has none" without a fixture that MUST produce one.
///
/// bone records nothing in that map here -- `apply` is not overloaded -- which
/// is why the discriminant is skull's own `Expr::Global`-names-a-function scan
/// (`lower/expr.rs:75`).
#[test]
fn a_function_taken_as_a_value_is_an_edge() {
    let (ws, g) = graph_of(
        r#"
fn bump(x: int) -> int { return x + 1 }
fn apply(f: fn(int) -> int, v: int) -> int { return f(v) }
fn main() {
    let g = bump
    apply(bump, 1)
}
"#,
    );
    let vals: Vec<&str> = g
        .edges
        .iter()
        .filter(|e| e.kind == EdgeKind::FnValue)
        .map(|e| g.symbol(&ws, &e.to))
        .collect::<Vec<_>>()
        .leak()
        .iter()
        .map(|s| s.as_str())
        .collect();
    assert!(
        vals.iter().any(|s| s.contains("bump")),
        "`bump` is taken as a value twice and produced no fn-value edge: {vals:?}"
    );
}

/// THE WATCH ITEM (plan S 0h). A GENERIC function taken as a value has no call
/// site, so there are no bindings to key an instantiation on -- structurally the
/// same gap as the `iter` witness. It must be REPORTED, not silently instantiated
/// at some invented type and not silently dropped.
#[test]
fn a_generic_function_taken_as_a_value_has_no_instantiation() {
    let (_ws, g) = graph_of(
        r#"
fn ident<T>(x: T) -> T { return x }
fn apply(f: fn(int) -> int, v: int) -> int { return f(v) }
fn main() {
    apply(ident, 1)
}
"#,
    );
    let n = g
        .unresolved
        .iter()
        .filter(|u| u.reason == "fn-value-generic-no-instantiation")
        .count();
    assert!(n > 0, "a generic fn value was not reported as uninstantiable");
}

/// Every level of a combinator chain gets a CONCRETE witness instance.
///
/// HONEST SCOPE: this does not test the `bound_call_trees` walk or the nested
/// `sub` recursion, and it was written believing it did. Disabling either one
/// leaves it green -- measured, one mutation at a time. The inner witnesses
/// arrive by a third route: the walk instantiates `next_FilterIter<..>`, whose
/// BODY calls `next` on its inner iterator as an ordinary generic call, and
/// that route already existed in round 1.
///
/// The corpus is the oracle for the other two routes, and it prices them small:
/// `bound-next-witness` is 5 edges across all 23 projects, of which the `sub`
/// recursion contributes 1 of mag's 3.
#[test]
fn a_chained_iterator_records_its_inner_next_witness() {
    let (ws, g) = graph_of(
        r#"
fn main() {
    let xs: [int] = [1, 2, 3]
    let total = 0
    for v in xs.iter().map(_ + 1).filter(_ > 0) { total = total + v }
}
"#,
    );
    let witnesses: Vec<String> = g
        .edges
        .iter()
        .filter(|e| {
            e.kind == EdgeKind::IteratorNext || e.kind == EdgeKind::BoundNextWitness
        })
        .map(|e| g.symbol(&ws, &e.to))
        .collect();
    // `next` is one overload per iterator type, so the mangling carries the
    // receiver: `next_VecIter_T_ret_...`. The three levels of the chain must
    // each appear, at their own concrete instantiation.
    for want in ["next_VecIter", "next_MapIter", "next_FilterIter"] {
        assert!(
            witnesses.iter().any(|s| s.contains(want)),
            "no witness edge for `{want}`; got {witnesses:?}"
        );
    }
}

/// Logical instance identity is target-neutral (plan S 20a): the target decides
/// which backend VARIANT serves a call and which boundaries exist, never the
/// key. The two views resolve genuinely different `FuncRef`s -- a same-name
/// `inline(lua)`/`inline(js)` pair is two declarations -- and must still produce
/// one manifest, because the variants share a `(name, signature)` and therefore
/// one mangling.
#[test]
fn logical_identity_is_stable_across_targets() {
    use bone_semantic::hir::FnBackend;
    let ws = check(
        r#"
value P { x: float, y: float }
fn ident<T>(v: T) -> T { return v }
fn main() {
    let a = ident(P { x: 1.0, y: 2.0 })
    let b = ident(1)
    let xs: [int] = [1, 2, 3]
    for v in xs.iter().map(_ + 1) { let _ = v }
}
"#,
    );
    let roots = instance_graph::emission_roots(&ws, None);
    let lua = instance_graph::build_for_target(&ws, roots.clone(), false, FnBackend::Lua);
    let js = instance_graph::build_for_target(&ws, roots, false, FnBackend::Js);
    assert_eq!(
        instance_graph::render_manifest(&ws, &lua),
        instance_graph::render_manifest(&ws, &js),
        "the target changed logical instance identity"
    );
}

/// Plan S 20g.1: feed the SAME shadow graph to every capability fixture and
/// require logical instance identity to be unchanged while realization
/// obligations differ. The projections must not re-derive identity -- `project`
/// takes `&InstanceGraph`, so it structurally cannot, and this asserts the
/// consequence.
#[test]
fn a_projection_never_changes_logical_identity() {
    use skull::target_projection as tp;
    let (ws, g) = graph_of(
        r#"
value P { x: float, y: float }
extern(ffi, "stone") fn host_tick(n: int) -> int
fn ident<T>(v: T) -> T { return v }
fn main() {
    let a = ident(P { x: 1.0, y: 2.0 })
    let b = ident(1)
    host_tick(3)
}
"#,
    );
    let projections: Vec<tp::Projection> =
        tp::ALL.iter().map(|s| tp::project(&ws, &g, *s)).collect();
    assert_eq!(projections.len(), 4, "all four fixtures must project");
    let first = projections[0].manifest_digest;
    for p in &projections {
        assert_eq!(
            p.manifest_digest, first,
            "projection `{}` changed the manifest digest",
            p.target
        );
        assert_eq!(p.instances, projections[0].instances, "`{}`", p.target);
    }
    // ...while realization genuinely differs, or the equality above is vacuous.
    let luajit = projections.iter().find(|p| p.target == "luajit").unwrap();
    let wasm = projections.iter().find(|p| p.target == "wasm32").unwrap();
    assert!(
        wasm.boundaries_available < luajit.boundaries_available,
        "wasm32 must lose boundaries luajit has, or the fixtures are not distinct"
    );
    assert!(
        !wasm.deferred.is_empty() && luajit.deferred.is_empty(),
        "the two fixtures must differ in deferred obligations"
    );
}

/// An `extern` is available exactly where the target supplies foreign symbols.
/// The `Invalid boundary` outcome (plan S 10) is per-target, so this is what
/// makes a boundary's `target` field load-bearing rather than decorative.
#[test]
fn extern_availability_follows_the_capability_not_the_target_name() {
    use skull::target_projection as tp;
    let (ws, g) = graph_of(
        r#"
extern(ffi, "stone") fn host_tick(n: int) -> int
fn main() { host_tick(3) }
"#,
    );
    assert!(
        g.boundaries.values().any(|b| b.class
            == skull::instance_graph::BoundaryClass::Extern),
        "the extern was not classified as a boundary at all"
    );
    let unsupported = |spec| {
        tp::project(&ws, &g, spec)
            .unsupported_by_class
            .get("extern")
            .copied()
            .unwrap_or(0)
    };
    assert_eq!(unsupported(tp::LUAJIT), 0, "luajit supplies foreign symbols");
    assert_eq!(unsupported(tp::NATIVE_C64), 0, "a C target supplies foreign symbols");
    assert!(unsupported(tp::JAVASCRIPT) > 0, "JS has no `ffi.load`");
    assert!(unsupported(tp::WASM32) > 0, "wasm imports are declared, not looked up");
}

/// A coroutine instance OWNS helper symbols, and today every one of them is
/// named from the SOURCE `FuncRef` (`coroutine_emit.rs:21`). Recording the
/// owning instance beside the name is what lets Stage 3 re-base them.
#[test]
fn a_coroutine_instance_owns_its_helper_symbols() {
    let (_ws, g) = graph_of(
        r#"
co fn ticker(n: int) {
    let i = 0
    while i < n { yield; i = i + 1 }
}
fn main() { let c = ticker(3) }
"#,
    );
    let helpers: Vec<&str> = g
        .coroutine_helpers
        .iter()
        .filter(|h| h.symbol_today.contains("ticker"))
        .map(|h| h.suffix)
        .collect();
    assert!(helpers.contains(&"__step"), "no `__step` helper: {helpers:?}");
    assert!(helpers.contains(&"__resume"), "no `__resume` helper: {helpers:?}");
}

/// S 0g: the manifest must be stable with inlining DISABLED, as well as under a
/// perturbed discovery order.
///
/// This holds BY CONSTRUCTION -- `instance_graph::build` walks bone's HIR and
/// inference and never runs the inliner -- and "by construction" is a claim
/// about code that can be edited, which is what this pins. The mode is toggled
/// rather than mocked: `ROCK_MONO_ABLATION` is what disables inlining in a real
/// build (`skull_transform::inline::classify`), so the test exercises the same
/// switch the corpus run does.
///
/// Corpus result recorded alongside it: all 23 project digests are identical
/// between a normal `rock mono-manifest .` and one under
/// `ROCK_MONO_ABLATION=1`.
#[test]
fn the_manifest_is_stable_with_inlining_disabled() {
    let src = r#"
value P { x: float, y: float }
@inline fn ident<T>(v: T) -> T { return v }
fn main() {
    let a = ident(P { x: 1.0, y: 2.0 })
    let b = ident(1)
    let xs: [int] = [1, 2, 3]
    for v in xs.iter().map(_ + 1) { let _ = v }
}
"#;
    let ws = check(src);
    let roots = instance_graph::emission_roots(&ws, None);

    skull_mir::ablation::set_enabled(false);
    let normal = instance_graph::build(&ws, roots.clone(), false);
    let normal_text = instance_graph::render_manifest(&ws, &normal);

    skull_mir::ablation::set_enabled(true);
    let no_inline = instance_graph::build(&ws, roots.clone(), false);
    let no_inline_text = instance_graph::render_manifest(&ws, &no_inline);
    // Perturbed order AND inlining disabled, the two S 0g knobs together.
    let no_inline_rev = instance_graph::build(&ws, roots, true);
    skull_mir::ablation::set_enabled(false);

    assert_eq!(
        normal_text, no_inline_text,
        "disabling inlining changed the instance manifest"
    );
    assert_eq!(
        no_inline_text,
        instance_graph::render_manifest(&ws, &no_inline_rev),
        "with inlining disabled, reversing root order changed the manifest"
    );
    assert_eq!(
        instance_graph::manifest_digest(&ws, &normal),
        instance_graph::manifest_digest(&ws, &no_inline_rev),
        "digest differs across the two knobs"
    );
    assert!(!normal_text.is_empty(), "the fixture produced an empty manifest");
}
