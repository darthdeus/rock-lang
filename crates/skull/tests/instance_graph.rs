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
    let syms = g.manifest_lines(&ws);
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
    let syms = g.manifest_lines(&ws);
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
    let syms = g.manifest_lines(&ws);
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
/// key. Two views that resolve genuinely different `FuncRef`s must still produce
/// one manifest, because the variants share a `(name, signature)` and therefore
/// one mangling.
///
/// The fixture carries an EXPLICIT `inline(lua)`/`inline(js)` pair. It used to
/// rely on the stdlib chain its iterator loop pulls in, and the claim that the
/// chain resolved different `FuncRef`s was never measured -- measured, it is
/// FALSE: 89 instances on each side and ZERO resolving differently, so the
/// manifests agreed because nothing had forked. `variant_pairs` below is the
/// non-vacuity assertion, and it is what makes the equality mean anything.
#[test]
fn logical_identity_is_stable_across_targets() {
    use bone_semantic::hir::FnBackend;
    let ws = check(
        r#"
value P { x: float, y: float }
fn ident<T>(v: T) -> T { return v }
@inline inline(lua) fn variant_probe<T>(v: T) -> int = "return 1"
@inline inline(js) fn variant_probe<T>(v: T) -> int = "return 1;"
fn main() {
    let a = ident(P { x: 1.0, y: 2.0 })
    let b = ident(1)
    let p = variant_probe(1)
    let xs: [int] = [1, 2, 3]
    for v in xs.iter().map(_ + 1) { let _ = v }
}
"#,
    );
    let roots = instance_graph::emission_roots(&ws, None);
    let lua = instance_graph::build_for_target(&ws, roots.clone(), false, FnBackend::Lua);
    let js = instance_graph::build_for_target(&ws, roots, false, FnBackend::Js);

    let by_symbol = |g: &instance_graph::InstanceGraph| {
        g.instances
            .iter()
            .map(|i| (g.symbol(&ws, i), i.func))
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    let (l, j) = (by_symbol(&lua), by_symbol(&js));
    let variant_pairs =
        l.iter().filter(|(sym, lf)| j.get(*sym).is_some_and(|jf| jf != *lf)).count();
    assert!(
        variant_pairs > 0,
        "no symbol resolved to a different FuncRef across the two targets, so the manifest \
         equality below is vacuous -- the fixture is not exercising backend variants at all"
    );

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

/// The identity adversary: one generic instantiated at `(fn(int) -> int)?` and
/// at `fn(int) -> int?`. Those are different types that RENDER alike -- a
/// postfix `?` on an unbracketed arrow type is ambiguous -- so every set, map
/// and digest over the graph has to key on the canonical encoding.
///
/// The equal-renderings assertion is what makes this adversarial: if the two
/// ever displayed differently, the pair would no longer be the collision and
/// the separation below would prove nothing.
#[test]
fn two_instances_whose_arguments_render_alike_stay_distinct() {
    let (ws, g) = graph_of(
        r#"
fn twice(n: int) -> int { return n * 2 }
fn maybe(n: int) -> int? { return n * 2 }
fn ident<T>(x: T) -> T { return x }
fn main() {
    let f: fn(int) -> int? = maybe
    let g: (fn(int) -> int)? = twice
    let a = ident(f)
    let b = ident(g)
}
"#,
    );

    let idents: Vec<&instance_graph::Instance> = g
        .instances
        .iter()
        .filter(|i| ws.module(i.func.module).functions[i.func.func].name.as_str() == "ident")
        .collect();
    assert_eq!(idents.len(), 2, "expected two `ident` instances, got {}", idents.len());

    let display: Vec<String> = idents.iter().map(|i| g.symbol(&ws, i)).collect();
    assert_eq!(
        display[0], display[1],
        "the pair is no longer the ambiguity this test exists for: {display:?}"
    );

    let identity: Vec<String> = idents.iter().map(|i| g.identity(&ws, i)).collect();
    assert_ne!(
        identity[0], identity[1],
        "two distinct instances share one identity: {identity:?}"
    );

    // ...and the manifest keeps both, rather than deduplicating them into one.
    let lines = g.manifest_lines(&ws);
    let ident_lines: Vec<&String> =
        lines.iter().filter(|l| l.contains("ident")).collect();
    assert_eq!(
        ident_lines.len(),
        2,
        "the manifest collapsed two distinct instances: {ident_lines:?}"
    );
}

// ---------------------------------------------------------------------------
// Stage 2: the three decided rejections at instance construction (plan S 0h).
// Each has a control that must NOT be rejected beside it -- a rejection whose
// only evidence is the rejected input proves the gate fires, not that it
// discriminates.
// ---------------------------------------------------------------------------

fn rejections_of(
    ws: &Workspace,
    g: &instance_graph::InstanceGraph,
    kind: instance_graph::RejectionKind,
) -> Vec<String> {
    g.rejections
        .iter()
        .filter(|r| r.kind == kind)
        .map(|r| format!("{} at {:?}: {}", g.identity(ws, &r.at), r.span, r.message))
        .collect()
}

/// `f<T> -> f<[T]>` cannot be monomorphized: the instantiation type grows
/// without bound. Plan S 7 records that it compiles and RUNS today, so this is
/// a deliberate language-surface rejection, not a bug fix.
#[test]
fn changing_instance_recursion_is_rejected() {
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
    let r = rejections_of(&ws, &g, instance_graph::RejectionKind::ChangingInstanceRecursion);
    assert_eq!(r.len(), 1, "expected exactly one polymorphic-recursion rejection, got {r:?}");
    assert!(r[0].contains("depth"), "{r:?}");
    assert!(
        !g.rejections.iter().any(|x| x.span.file.as_str() == "<unknown>"),
        "a rejection carried no span: {:?}",
        g.rejections
    );
    // The WALK still had to terminate to produce that rejection -- detection is
    // a post-pass, so `MAX_INSTANTIATION_DEPTH` is what stops the expansion.
    assert!(g.instances.len() < 500, "instance explosion: {}", g.instances.len());
    assert_eq!(
        g.recursion_census(&ws).changing_instance.len(),
        1,
        "the census disagrees with the rejection it is derived from"
    );
}

/// Mutual polymorphic recursion. The type grows around a two-function cycle, so
/// neither edge on its own changes a function's own arguments -- a self-edge
/// check alone cannot see this.
#[test]
fn mutual_changing_instance_recursion_is_rejected() {
    let (ws, g) = graph_of(
        r#"
fn ping<T>(x: T, n: int) -> int {
    if n <= 0 { return 0 }
    return pong(x, n - 1)
}
fn pong<T>(x: T, n: int) -> int {
    let wrapped: [T] = [x]
    return 1 + ping(wrapped, n - 1)
}
fn main() { ping(1, 3) }
"#,
    );
    let r = rejections_of(&ws, &g, instance_graph::RejectionKind::ChangingInstanceRecursion);
    assert!(!r.is_empty(), "mutual polymorphic recursion was not rejected");
}

/// CONTROL: ordinary same-instance recursion, direct and mutual, is finite and
/// must survive. It is the shape the corpus actually contains (11 direct, 9
/// mutual), so a gate that rejected it would reject every real program.
#[test]
fn same_instance_recursion_is_not_rejected() {
    let (ws, g) = graph_of(
        r#"
fn countdown(n: int) -> int {
    if n <= 0 { return 0 }
    return countdown(n - 1)
}
fn even(n: int) -> bool { if n == 0 { return true } return odd(n - 1) }
fn odd(n: int) -> bool { if n == 0 { return false } return even(n - 1) }
fn hold<T>(x: T, n: int) -> int {
    if n <= 0 { return 0 }
    return hold(x, n - 1)
}
fn main() {
    countdown(3)
    even(4)
    hold(1, 2)
    hold("s", 2)
}
"#,
    );
    let r = rejections_of(&ws, &g, instance_graph::RejectionKind::ChangingInstanceRecursion);
    assert!(r.is_empty(), "finite recursion was rejected: {r:?}");
    // ...and the fixture really did produce the recursion it claims to.
    let census = g.recursion_census(&ws);
    assert!(
        census.changing_instance.is_empty(),
        "same-instance recursion was misfiled as polymorphic: {:?}",
        census.changing_instance
    );
    assert!(
        census.direct_same_instance.iter().any(|s| s.contains("countdown")),
        "the fixture produced no direct recursion to control against"
    );
    assert!(
        !census.mutual_same_instance.is_empty(),
        "the fixture produced no mutual recursion to control against"
    );
    // `hold` recurs at two distinct instantiations, neither reachable from the
    // other. Two instances of one function inside a recursive SCC is NOT
    // polymorphic recursion.
    assert_eq!(
        g.instances
            .iter()
            .filter(|i| ws.module(i.func.module).functions[i.func.func].name.as_str() == "hold")
            .count(),
        2,
        "the fixture lost the two-instantiation arm of the control"
    );
}

/// THE WATCH ITEM (plan S 0h). A GENERIC function taken as a value has no call
/// site to key an instantiation on -- structurally the same gap as the `iter`
/// witness, decided the other way. It must be REJECTED with a real diagnostic,
/// not instantiated at an invented type and not silently dropped.
#[test]
fn a_generic_fn_value_is_rejected() {
    let (ws, g) = graph_of(
        r#"
fn ident<T>(x: T) -> T { return x }
fn apply(f: fn(int) -> int, v: int) -> int { return f(v) }
fn main() {
    apply(ident, 1)
}
"#,
    );
    let r = rejections_of(&ws, &g, instance_graph::RejectionKind::GenericFnValue);
    assert_eq!(r.len(), 1, "expected one generic-fn-value rejection, got {r:?}");
    assert!(r[0].contains("ident"), "the rejection does not name the function: {r:?}");
}

/// CONTROL: a concrete function taken as a value is ordinary and must survive.
#[test]
fn a_concrete_fn_value_is_not_rejected() {
    let (ws, g) = graph_of(
        r#"
fn bump(x: int) -> int { return x + 1 }
fn apply(f: fn(int) -> int, v: int) -> int { return f(v) }
fn main() {
    let h = bump
    apply(bump, 1)
}
"#,
    );
    assert!(
        rejections_of(&ws, &g, instance_graph::RejectionKind::GenericFnValue).is_empty(),
        "a concrete fn value was rejected"
    );
    assert!(
        g.edges.iter().any(|e| e.kind == EdgeKind::FnValue),
        "the fixture produced no fn-value edge to control against"
    );
}

/// A type argument no constraint reaches. `make()`'s `T` is bound by nothing at
/// the call, in the body, or at the binding -- there is no concrete type to
/// instantiate, and today it compiles and runs. Decided REJECT (plan S 0h).
#[test]
fn an_unconstrained_type_variable_is_rejected() {
    let (ws, g) = graph_of(
        r#"
fn make<T>() -> [T] { return [] }
fn main() {
    let xs = make()
    let n = xs.len()
}
"#,
    );
    // Every call that carries the unbound variable is rejected, not only the
    // one that introduced it: `len` is called on the `[T]` it returned.
    let r = rejections_of(&ws, &g, instance_graph::RejectionKind::UnconstrainedTypeVar);
    assert_eq!(r.len(), 2, "expected the `make` and `len` call sites, got {r:?}");
    assert!(r.iter().any(|s| s.contains("make")), "{r:?}");
    assert!(r.iter().any(|s| s.contains("len")), "{r:?}");
}

/// CONTROL: a type variable constrained anywhere in the body -- not at the call
/// -- still unifies to a concrete type and must NOT be rejected. This is the
/// arm that decides whether the gate reads "unconstrained" or merely "not
/// constrained by the call's own arguments".
#[test]
fn a_body_constrained_type_variable_is_not_rejected() {
    let (ws, g) = graph_of(
        r#"
fn make<T>() -> [T] { return [] }
fn main() {
    let xs = make()
    xs.push(7)
}
"#,
    );
    let r = rejections_of(&ws, &g, instance_graph::RejectionKind::UnconstrainedTypeVar);
    assert!(r.is_empty(), "a body-constrained type variable was rejected: {r:?}");
    let makes: Vec<String> = g
        .instances
        .iter()
        .filter(|i| ws.module(i.func.module).functions[i.func.func].name.as_str() == "make")
        .map(|i| g.symbol(&ws, i))
        .collect();
    assert_eq!(makes.len(), 1, "expected one concrete `make` instance, got {makes:?}");
    assert!(makes[0].ends_with("<int>"), "T did not unify to int: {makes:?}");
}

/// A valid program reaches instance construction with NOTHING rejected. Without
/// this, every assertion above could be satisfied by a gate that rejects
/// everything.
#[test]
fn an_ordinary_generic_program_is_not_rejected() {
    let (_ws, g) = graph_of(
        r#"
fn ident<T>(x: T) -> T { return x }
fn main() {
    let a = ident(1)
    let b = ident("s")
    let xs = [1, 2, 3]
    for v in xs { ident(v) }
}
"#,
    );
    assert!(g.rejections.is_empty(), "a valid program was rejected: {:?}", g.rejections);
}

// ---------------------------------------------------------------------------
// Stage 2: synthesized `iter` witnesses resolve to concrete instances.
//
// A synthesized `iter` call has no call expr, so it can never be keyed in
// `call_generic_bindings`. bone now retains the substitution it already
// computed while resolving the witness -- symmetric to `next_generics` -- and
// the walk instantiates from that record. The two routes are independent: the
// `for` head (`for_iter_protocol`) and an iterable argument coerced at an
// `Iterator`-bounded param (`implicit_iter_args`).
// ---------------------------------------------------------------------------

/// The `for` head over a type whose `iter` is GENERIC. `iter<T>(Arena<T>)` is
/// the stdlib's own, so the fixture does not invent a protocol.
#[test]
fn a_generic_for_head_iter_witness_is_instantiated() {
    let (ws, g) = graph_of(
        r#"
fn main() {
    let a: Arena<int> = arena_new()
    let total = 0
    for (e, v) in a { total = total + v }
}
"#,
    );
    let iters: Vec<String> = g
        .edges
        .iter()
        .filter(|e| e.kind == EdgeKind::IteratorIter)
        .map(|e| g.symbol(&ws, &e.to))
        .collect();
    assert!(!iters.is_empty(), "the `for` head recorded no iterator-iter edge");
    assert!(
        iters.iter().any(|s| s.contains("iter") && s.ends_with("<int>")),
        "the generic `iter` witness was not instantiated at `int`: {iters:?}"
    );
    assert!(
        !g.unresolved.iter().any(|u| u.reason == "iter-witness-no-instantiation-record"),
        "a witness was still filed as having no instantiation record"
    );
}

/// The call-argument route. `count<I: Iterator<T>, T>` takes an `Iterator`, so
/// passing an `Arena<int>` coerces through the same generic `iter` -- a
/// DIFFERENT bone map (`implicit_iter_args`) reached by a different code path.
#[test]
fn a_generic_implicit_iter_witness_is_instantiated() {
    let (ws, g) = graph_of(
        r#"
fn main() {
    let a: Arena<int> = arena_new()
    let n = a.count()
}
"#,
    );
    let iters: Vec<String> = g
        .edges
        .iter()
        .filter(|e| e.kind == EdgeKind::ImplicitIter)
        .map(|e| g.symbol(&ws, &e.to))
        .collect();
    assert!(!iters.is_empty(), "the coerced argument recorded no implicit-iter edge");
    assert!(
        iters.iter().any(|s| s.contains("iter") && s.ends_with("<int>")),
        "the generic `iter` witness was not instantiated at `int`: {iters:?}"
    );
    assert!(
        !g.unresolved
            .iter()
            .any(|u| u.reason == "implicit-iter-witness-no-instantiation-record"),
        "a witness was still filed as having no instantiation record"
    );
}

/// CONTROL: a NON-generic `iter` witness needs no record and must keep working
/// through the empty-substitution path.
#[test]
fn a_concrete_iter_witness_still_resolves() {
    let (ws, g) = graph_of(
        r#"
struct Counter { i: int, n: int }
fn iter(c: Counter) -> Counter { return c }
fn next(c: Counter) -> (bool, int) {
    if c.i >= c.n { return (false, 0) }
    c.i = c.i + 1
    return (true, c.i)
}
fn main() {
    let c = Counter { i: 0, n: 3 }
    let total = 0
    for v in c { total = total + v }
}
"#,
    );
    let iters: Vec<String> = g
        .edges
        .iter()
        .filter(|e| e.kind == EdgeKind::IteratorIter)
        .map(|e| g.symbol(&ws, &e.to))
        .collect();
    assert!(!iters.is_empty(), "a concrete `iter` witness produced no edge");
    assert!(
        g.edges
            .iter()
            .filter(|e| e.kind == EdgeKind::IteratorIter)
            .all(|e| e.to.args.is_empty()),
        "a non-generic witness was given instance arguments: {iters:?}"
    );
}

/// CONTROL: the `next` witness's own record (`next_generics`) is untouched by
/// the `iter` one. Both witnesses of one loop must be concrete, and they are
/// resolved from two separate records.
#[test]
fn the_next_witness_record_is_unchanged() {
    let (_ws, g) = graph_of(
        r#"
fn main() {
    let a: Arena<int> = arena_new()
    let total = 0
    for (e, v) in a { total = total + v }
}
"#,
    );
    let nexts: Vec<&instance_graph::Edge> =
        g.edges.iter().filter(|e| e.kind == EdgeKind::IteratorNext).collect();
    assert!(!nexts.is_empty(), "no iterator-next edge was recorded");
    assert!(
        nexts.iter().all(|e| !e.to.args.is_empty()),
        "a `next` witness lost its instantiation"
    );
}

/// HOSTILE: the witness record is keyed by the RESOLVED declaration's `VarId`s,
/// but the walk instantiates the declaration the TARGET selects. Sibling
/// declarations are separate functions with separate `VarId`s, so reading the
/// record against the selected variant's generics finds nothing and the witness
/// silently degrades to `*-generic-unbound` on the non-default target only.
///
/// The corpus cannot catch this: it has no generic backend-sibling `iter`. The
/// fixture supplies one, and the Lua view is the control -- the same witness
/// must be instantiated identically on both targets, because instance identity
/// is target-neutral (plan S 20a).
#[test]
fn a_generic_witness_survives_backend_variant_selection() {
    let src = r#"
struct Bag<T> { items: [T] }
struct BagIter<T> { items: [T], i: int }

fn iter<T>(b: Bag<T>) -> BagIter<T> {
    return BagIter { items: b.items, i: 0 }
}
inline(js) fn iter<T>(b: Bag<T>) -> BagIter<T> = "return { items: b.items, i: 0 };"

fn next<T>(it: BagIter<T>) -> (bool, T) {
    if it.i >= it.items.len() { return (false, it.items[0]) }
    it.i = it.i + 1
    return (true, it.items[it.i - 1])
}

fn main() {
    let b = Bag { items: [1, 2, 3] }
    let total = 0
    for v in b { total = total + v }
}
"#;
    let ws = check(src);
    let roots = instance_graph::emission_roots(&ws, None);

    let witness_args = |target: bone_semantic::hir::FnBackend| -> Vec<String> {
        let g = instance_graph::build_for_target(&ws, roots.clone(), false, target);
        assert!(
            !g.unresolved.iter().any(|u| u.reason.contains("unbound")),
            "{target:?}: a witness degraded to unbound: {:?}",
            g.unresolved
                .iter()
                .map(|u| (u.reason, ws.module(u.callee.module).functions[u.callee.func].name))
                .collect::<Vec<_>>()
        );
        let mut v: Vec<String> = g
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::IteratorIter)
            .map(|e| {
                e.to.args.iter().map(|a| g.arena.canonical(*a)).collect::<Vec<_>>().join(",")
            })
            .collect();
        v.sort();
        assert!(!v.is_empty(), "{target:?}: no iterator-iter edge");
        v
    };

    let lua = witness_args(bone_semantic::hir::FnBackend::Lua);
    let js = witness_args(bone_semantic::hir::FnBackend::Js);
    assert_eq!(
        lua, js,
        "the same logical witness instantiated differently once the target selected a sibling"
    );
}
