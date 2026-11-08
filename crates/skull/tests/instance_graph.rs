//! Stage 0d properties for the shadow instance graph
//!.

use std::sync::LazyLock;

use bone_semantic::inference::Workspace;
use skull::instance_graph::{self, EdgeKind};
use ustr::Ustr;

static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
    rock_stdlib::stdlib_files()
        .iter()
        .map(|f| {
            let id = Ustr::from(rock_stdlib::stdlib_file_id(f.name).as_str());
            skull_parser::parse_module_clean(f.contents, id).expect("stdlib parse")
        })
        .collect()
});

fn check(src: &str) -> Workspace {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(src, Ustr::from("<test>")).expect("parse"));
    bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass")
}

fn graph_of(src: &str) -> (Workspace, instance_graph::InstanceGraph) {
    let ws = check(src);
    let roots = skull::mir_emit::emission_roots(&ws, None);
    let g = instance_graph::build(&ws, roots, false);
    (ws, g)
}

#[test]
fn fresh_graph_does_not_seed_incremental_delta_state() {
    let (_ws, graph) = graph_of(
        r#"
fn main() {
    print("ok")
}
"#,
    );
    assert_eq!(
        graph.incremental_state_population(),
        (0, 0, 0),
        "fresh construction retained update-only owner or instance deltas"
    );
}

/// One generic function called at two element types is TWO instances, even
/// when neither argument requires a representation-specific lowering.
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
            syms.iter()
                .any(|s| s.contains("inner") && s.ends_with(want)),
            "inner was not instantiated at {want}: {:?}",
            syms.iter()
                .filter(|s| s.contains("inner"))
                .collect::<Vec<_>>()
        );
    }
}

/// The manifest must not depend on the order roots are discovered in
/// (plan S 20g.4).
///
/// This fixture is a lower bound. The per-project `order-stable` column from
/// `rock mono-manifest` is the expansion-cap oracle because it exercises real
/// library depth.
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
    let roots = skull::mir_emit::emission_roots(&ws, Some("<test>"));
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
    assert_eq!(
        idents.len(),
        2,
        "P and Q must not share an instance: {idents:?}"
    );
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
    let nexts = g
        .edges()
        .filter(|e| e.kind == EdgeKind::IteratorNext)
        .count();
    assert!(nexts > 0, "no iterator-next edge was recorded");
    assert!(
        g.edges()
            .filter(|e| e.kind == EdgeKind::IteratorNext)
            .all(|e| !e.to.args.is_empty()),
        "an iterator-next witness reached a generic callee with no instantiation"
    );
}

/// Every complete generic call identity recovered from typed bindings must
/// appear in the instance graph. A miss whose CALLING body the graph never
/// reached is classified separately from an identity missing in a reached
/// body.
#[test]
fn every_concrete_call_identity_appears_in_the_instance_graph() {
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
    let r = instance_graph::reconcile_call_identities(&ws, &g);
    assert!(
        r.calls > 0,
        "the fixture produced no concrete calls to reconcile"
    );
    assert!(
        r.missing.is_empty(),
        "concrete call identities absent from the instance graph: {:?}",
        r.missing
    );
    assert_eq!(
        r.unresolved_sites, 0,
        "a valid call identity could not be stated"
    );
    assert!(
        r.graph_only > 0,
        "the graph had no identities beyond direct calls"
    );
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
        .edges()
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

/// Every level of a combinator chain gets a concrete `next` instance.
#[test]
fn a_chained_iterator_reaches_each_next_instance() {
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
        .edges()
        .filter(|e| e.kind == EdgeKind::IteratorNext || e.kind == EdgeKind::BoundNextWitness)
        .map(|e| g.symbol(&ws, &e.to))
        .collect();
    // `next` is one overload per iterator type, so the mangling carries the
    // receiver: `next_VecIter_T`. The three levels of the chain must
    // each appear, at their own concrete instantiation.
    for want in ["next_VecIter", "next_MapIter", "next_FilterIter"] {
        assert!(
            witnesses.iter().any(|s| s.contains(want)),
            "no witness edge for `{want}`; got {witnesses:?}"
        );
    }
}

/// Logical instance identity is target-neutral (plan S 20a), and now so is the
/// GRAPH: one build, one arena, one manifest, whatever artifact realizes it.
///
/// The fixture carries an EXPLICIT `inline(lua)`/`inline(js)` pair, which is
/// what makes the equality mean anything: the two artifacts genuinely select
/// different declarations for `variant_probe`, and `sibling_selections` below is
/// the non-vacuity assertion for that. It used to rely on the stdlib chain its
/// iterator loop pulls in, and the claim that the chain resolved different
/// `FuncRef`s was never measured -- measured, it was FALSE.
#[test]
fn the_shared_graph_is_target_neutral() {
    use skull::artifact::{self, ArtifactRequest};
    use skull::target_projection as tp;
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
    let roots = skull::mir_emit::emission_roots(&ws, None);
    let g = instance_graph::build(&ws, roots.clone(), false);

    // The property, formerly a sentinel asserting the opposite: there is no
    // second graph to disagree with. Rebuilding must be the same manifest --
    // the walk selects no variant, so nothing it records can depend on a target.
    let again = instance_graph::build(&ws, roots.clone(), false);
    assert_eq!(
        instance_graph::render_manifest(&ws, &g),
        instance_graph::render_manifest(&ws, &again)
    );

    // NON-VACUITY: the two artifacts really do select different declarations
    // for one shared instance. Without this the equality above would hold
    // because nothing ever forked.
    let lua = artifact::realize(&ws, &g, &ArtifactRequest::new(tp::LUAJIT_DESKTOP, &roots));
    let js = artifact::realize(&ws, &g, &ArtifactRequest::new(tp::JAVASCRIPT_WEB, &roots));
    let probe = |v: &artifact::ArtifactView| {
        v.realization
            .iter()
            .filter(|(i, _)| {
                ws.module(i.func.module).functions[i.func.func]
                    .name
                    .as_str()
                    == "variant_probe"
            })
            .map(|(i, r)| (i.clone(), r.decl))
            .collect::<Vec<_>>()
    };
    let (lp, jp) = (probe(&lua), probe(&js));
    assert_eq!(
        lp.len(),
        1,
        "the probe was not retained exactly once on luajit"
    );
    assert_eq!(
        jp.len(),
        1,
        "the probe was not retained exactly once on javascript"
    );
    assert_eq!(lp[0].0, jp[0].0, "one shared instance, not two");
    assert_ne!(
        lp[0].1, jp[0].1,
        "both artifacts selected the same declaration, so this fixture is not \
         exercising backend variants at all and the manifest equality is vacuous"
    );
}

/// A repeated backend makes the whole same-signature set a genuine overload
/// ambiguity. Bone deliberately leaves every candidate distinct; the root-side
/// inverse must not manufacture a representative that bone never chose.
#[test]
fn canonical_variant_preserves_a_repeated_backend_group() {
    let ws = check(
        r#"
inline(lua) fn __canonical_duplicate_probe(x: int) -> int = "return x"
inline(js) fn __canonical_duplicate_probe(x: int) -> int = "return x;"
inline(js) fn __canonical_duplicate_probe(x: int) -> int = "return x + 1;"
fn main() {}
"#,
    );
    let decls: Vec<skull_mir::mir::FuncRef> = ws.functions_by_name
        [&Ustr::from("__canonical_duplicate_probe")]
        .iter()
        .map(|&(module, func)| skull_mir::mir::FuncRef { module, func })
        .collect();
    let mut representatives: Vec<skull_mir::mir::FuncRef> = decls
        .iter()
        .map(|d| skull_mir::lower::canonical_variant(&ws, *d))
        .collect();
    representatives.sort_by_key(|r| (r.module.raw(), r.func.into_raw().into_u32()));
    representatives.dedup();
    assert_eq!(
        representatives.len(),
        decls.len(),
        "a repeated-backend group was collapsed: {representatives:?}"
    );
    assert_eq!(
        instance_graph::stdlib_entry_representative(&ws, "__canonical_duplicate_probe"),
        None,
        "a residual overload ambiguity resolved to one compiler-generated entry"
    );
}

/// An `extern` is available exactly where the target supplies foreign symbols.
/// The `Invalid boundary` outcome (plan S 10) is per-target, so this is what
/// makes an artifact's capability read load-bearing rather than decorative.
///
/// Cross-artifact IDENTITY is not tested here -- it belongs with the artifact
/// view and is asserted over the shared `Instance` in
/// `artifact_view::cross_artifact_identity_is_the_instance_and_not_the_spelling`.
#[test]
fn extern_availability_distinguishes_backend_owned_js_boundary() {
    use skull::artifact::{self, ArtifactRequest, Exclusion, Unrealizable};
    use skull::target_projection as tp;
    // TWO externs, because the two causes are different questions. The
    // Lua-hosted one is foreign to any non-Lua artifact before its CLASS is
    // ever asked; only a JS-hosted extern on the JS artifact reaches the
    // capability arm.
    let src = r#"
extern(ffi, "host") fn host_tick(n: int) -> int
extern(js) fn web_tick(n: int) -> int
fn main() { host_tick(3) }
"#;
    let ws = check(src);
    let roots = skull::mir_emit::emission_roots(&ws, None);
    assert!(
        instance_graph::build(&ws, roots.clone(), false)
            .instances
            .iter()
            .any(|i| ws.module(i.func.module).functions[i.func.func].is_extern),
        "the extern is not in the graph at all"
    );
    // Every artifact realizes THE SAME graph; the declaration each selects is
    // `realize_decl`'s answer, not a per-target walk's.
    let g = instance_graph::build(&ws, roots.clone(), false);
    let cause_of = |spec: tp::TargetSpec, name: &str| -> Option<Exclusion> {
        artifact::realize(&ws, &g, &ArtifactRequest::new(spec, &roots))
            .excluded
            .iter()
            .find(|e| {
                ws.module(e.instance.func.module).functions[e.instance.func.func]
                    .name
                    .as_str()
                    == name
            })
            .map(|e| e.cause)
    };
    assert_eq!(
        cause_of(tp::LUAJIT_DESKTOP, "host_tick"),
        None,
        "luajit supplies foreign symbols, and `main` calls this one"
    );
    // `extern(js)` is the browser/runtime boundary owned by the JS artifact;
    // the generic foreign-symbol capability describes host symbols, not this
    // backend-owned runtime surface. Unreachability cannot answer for it -- an
    // `extern` is an emission ROOT.
    assert_eq!(
        cause_of(tp::JAVASCRIPT_WEB, "web_tick"),
        None,
        "the JS artifact owns its `extern(js)` runtime boundary"
    );
    // The BACKEND arm: a Lua-hosted extern names a host these artifacts do not
    // have, which is answered before the class question. `NATIVE_C64` declares
    // `foreign_symbols: true`, and that is exactly why this must not be decided
    // by the capability -- no declaration in this workspace names a C host.
    for spec in [tp::JAVASCRIPT_WEB, tp::NATIVE_C64_DESKTOP, tp::WASM32_WASI] {
        assert_eq!(
            cause_of(spec, "host_tick"),
            Some(Exclusion::Unrealizable(Unrealizable::ForeignToArtifact)),
            "a Lua-hosted extern survived `{}` on the strength of a capability flag",
            spec.name
        );
    }
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
    assert!(
        helpers.contains(&"__step"),
        "no `__step` helper: {helpers:?}"
    );
    assert!(
        helpers.contains(&"__resume"),
        "no `__resume` helper: {helpers:?}"
    );
}

#[test]
fn coroutine_graph_helpers_match_the_concrete_signature_plan() {
    let (ws, g) = graph_of(
        r#"
nobox value Payload { x: float, y: float }
enum Event { Tick(Payload) }
co fn ticker() -> Coroutine<Event> {
    yield Event::Tick(Payload { x: 1.0, y: 2.0 })
}
fn main() { let c = ticker(); let _ = c.resume() }
"#,
    );
    let bodies = skull::concrete_body::build(&ws, &g);
    let mut compared = 0;
    for (inst, concrete) in bodies.plans.iter() {
        let f = &ws.module(inst.func.module).functions[inst.func.func];
        if !f.is_coroutine {
            continue;
        }
        let plan = skull_mir::lower::CoroutinePlan::from_concrete_plan(
            &ws,
            g.interner(),
            concrete,
            inst.symbol(&ws, g.interner()),
        )
        .expect("concrete coroutine plan");
        let expected: std::collections::BTreeSet<&str> =
            plan.declared_suffixes().into_iter().collect();
        let actual: std::collections::BTreeSet<&str> = g
            .coroutine_helpers
            .iter()
            .filter(|helper| helper.owner_identity == g.identity(&ws, inst))
            .map(|helper| helper.suffix)
            .collect();
        assert_eq!(
            actual, expected,
            "graph helper accounting drifted from the plan"
        );
        assert!(
            expected.contains("__advance"),
            "fixture did not produce a yield channel"
        );
        compared += 1;
    }
    assert!(compared > 0, "fixture produced no concrete coroutine plan");
}

/// S 0g: the manifest must be stable with inlining DISABLED, as well as under a
/// perturbed discovery order.
///
/// This holds BY CONSTRUCTION -- `instance_graph::build` walks bone's HIR and
/// inference and never runs the inliner -- and "by construction" is a claim
/// about code that can be edited, which is what this pins.
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
    let roots = skull::mir_emit::emission_roots(&ws, None);

    let (normal, normal_text) = {
        let normal = instance_graph::build(&ws, roots.clone(), false);
        let normal_text = instance_graph::render_manifest(&ws, &normal);
        (normal, normal_text)
    };
    let (no_inline_text, no_inline_rev) = {
        let _mode = crate::common::InlineOffGuard::on();
        let no_inline = instance_graph::build(&ws, roots.clone(), false);
        let no_inline_text = instance_graph::render_manifest(&ws, &no_inline);
        // Perturbed order AND inlining disabled, the two S 0g knobs together.
        let no_inline_rev = instance_graph::build(&ws, roots, true);
        (no_inline_text, no_inline_rev)
    };

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
    assert!(
        !normal_text.is_empty(),
        "the fixture produced an empty manifest"
    );
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
        .filter(|i| {
            ws.module(i.func.module).functions[i.func.func]
                .name
                .as_str()
                == "ident"
        })
        .collect();
    assert_eq!(
        idents.len(),
        2,
        "expected two `ident` instances, got {}",
        idents.len()
    );

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
    let ident_lines: Vec<&String> = lines.iter().filter(|l| l.contains("ident")).collect();
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
    // A rejection becomes a compile error at the Stage-3 cutover, so a spanless
    // one is an error with nowhere to point (plan S 2). Asserted HERE rather
    // than per fixture: every rejection kind reaches the property this way, and
    // the corpus measures 0 rejections, so these fixtures are its only evidence.
    let spanless: Vec<&instance_graph::Rejection> = g
        .rejections()
        .filter(|r| instance_graph::is_spanless(&r.span))
        .collect();
    assert!(
        spanless.is_empty(),
        "a rejection carried no source span: {spanless:?}"
    );
    g.rejections()
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
    let r = rejections_of(
        &ws,
        &g,
        instance_graph::RejectionKind::ChangingInstanceRecursion,
    );
    assert_eq!(
        r.len(),
        1,
        "expected exactly one polymorphic-recursion rejection, got {r:?}"
    );
    assert!(r[0].contains("depth"), "{r:?}");
    // The WALK still had to terminate to produce that rejection -- detection is
    // a post-pass, so `MAX_INSTANTIATION_DEPTH` is what stops the expansion.
    assert!(
        g.instances.len() < 500,
        "instance explosion: {}",
        g.instances.len()
    );
    assert_eq!(
        g.recursion_census(&ws).changing_instance.len(),
        1,
        "the census disagrees with the rejection it is derived from"
    );
    // ...and the graph is NOT closed, which is what makes `is_closed` a test
    // rather than a constant: the capped expansion left callees undiscovered.
    let c = g.closure(&ws);
    assert!(
        !c.depth_capped.is_empty(),
        "a capped expansion was not reported"
    );
    assert!(
        !c.is_closed(),
        "a graph with a cut expansion reported itself closed"
    );
}

#[test]
fn recursive_diagnostics_do_not_depend_on_type_interning_order() {
    let ws = check(
        r#"
fn grow<T>(x: T, n: int) -> int {
    if n <= 0 { return 0 }
    return grow([x], n - 1)
}
fn first() -> int { return grow(1, 3) }
fn second() -> int { return grow(true, 3) }
fn main() { first(); second() }
"#,
    );
    let roots = skull::mir_emit::emission_roots(&ws, Some("<test>"));
    let build = |types| {
        let interner = std::sync::Arc::new(skull_mir::monoty::MonoInterner::new());
        interner.intern_all(types);
        instance_graph::build_in(&ws, roots.clone(), false, interner)
    };
    use skull_mir::monoty::MonoTy;
    let forward = build([MonoTy::Int, MonoTy::Bool]);
    let reverse = build([MonoTy::Bool, MonoTy::Int]);
    let kind = instance_graph::RejectionKind::ChangingInstanceRecursion;
    let expected = rejections_of(&ws, &forward, kind);
    assert_eq!(
        expected.len(),
        1,
        "fixture produced no recursive diagnostic"
    );
    assert_eq!(expected, rejections_of(&ws, &reverse, kind));
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
    let r = rejections_of(
        &ws,
        &g,
        instance_graph::RejectionKind::ChangingInstanceRecursion,
    );
    assert!(
        !r.is_empty(),
        "mutual polymorphic recursion was not rejected"
    );
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
    let r = rejections_of(
        &ws,
        &g,
        instance_graph::RejectionKind::ChangingInstanceRecursion,
    );
    assert!(r.is_empty(), "finite recursion was rejected: {r:?}");
    // ...and the fixture really did produce the recursion it claims to.
    let census = g.recursion_census(&ws);
    assert!(
        census.changing_instance.is_empty(),
        "same-instance recursion was misfiled as polymorphic: {:?}",
        census.changing_instance
    );
    assert!(
        census
            .direct_same_instance
            .iter()
            .any(|s| s.contains("countdown")),
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
            .filter(|i| ws.module(i.func.module).functions[i.func.func]
                .name
                .as_str()
                == "hold")
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
    assert_eq!(
        r.len(),
        1,
        "expected one generic-fn-value rejection, got {r:?}"
    );
    assert!(
        r[0].contains("ident"),
        "the rejection does not name the function: {r:?}"
    );
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
        g.edges().any(|e| e.kind == EdgeKind::FnValue),
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
    // Every site that carries the unbound variable is rejected, not only the one
    // that introduced it: `len` is called on the `[T]` it returned, and the
    // synthesized-call scan cannot decide that receiver's STORAGE either -- an
    // unresolved receiver must be reported, never read as "boxed".
    let r = rejections_of(&ws, &g, instance_graph::RejectionKind::UnconstrainedTypeVar);
    assert_eq!(
        r.len(),
        3,
        "expected `make`, `len` and the storage query, got {r:?}"
    );
    assert!(r.iter().any(|s| s.contains("make")), "{r:?}");
    assert!(r.iter().any(|s| s.contains("len_arr_T")), "{r:?}");
    assert!(
        r.iter().any(|s| s.contains("storage cannot be decided")),
        "{r:?}"
    );
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
    assert!(
        r.is_empty(),
        "a body-constrained type variable was rejected: {r:?}"
    );
    let makes: Vec<String> = g
        .instances
        .iter()
        .filter(|i| {
            ws.module(i.func.module).functions[i.func.func]
                .name
                .as_str()
                == "make"
        })
        .map(|i| g.symbol(&ws, i))
        .collect();
    assert_eq!(
        makes.len(),
        1,
        "expected one concrete `make` instance, got {makes:?}"
    );
    assert!(
        makes[0].ends_with("<int>"),
        "T did not unify to int: {makes:?}"
    );
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
    let rejected: Vec<String> = g.rejections().map(|r| r.message.clone()).collect();
    assert!(
        rejected.is_empty(),
        "a valid program was rejected: {rejected:?}"
    );
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
        .edges()
        .filter(|e| e.kind == EdgeKind::IteratorIter)
        .map(|e| g.symbol(&ws, &e.to))
        .collect();
    assert!(
        !iters.is_empty(),
        "the `for` head recorded no iterator-iter edge"
    );
    assert!(
        iters
            .iter()
            .any(|s| s.contains("iter") && s.ends_with("<int>")),
        "the generic `iter` witness was not instantiated at `int`: {iters:?}"
    );
    assert!(
        !g.unresolved()
            .any(|u| u.reason == "iter-witness-no-instantiation-record"),
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
        .edges()
        .filter(|e| e.kind == EdgeKind::ImplicitIter)
        .map(|e| g.symbol(&ws, &e.to))
        .collect();
    assert!(
        !iters.is_empty(),
        "the coerced argument recorded no implicit-iter edge"
    );
    assert!(
        iters
            .iter()
            .any(|s| s.contains("iter") && s.ends_with("<int>")),
        "the generic `iter` witness was not instantiated at `int`: {iters:?}"
    );
    assert!(
        !g.unresolved()
            .any(|u| u.reason == "implicit-iter-witness-no-instantiation-record"),
        "a witness was still filed as having no instantiation record"
    );
}

#[test]
fn always_emitted_implicit_iterator_edges_are_instantiated() {
    let (ws, g) = graph_of(
        r#"
global TOTAL: int = {
    let a: Arena<int> = arena_new()
    a.count()
}
fn main() { print("{TOTAL}") }
"#,
    );
    let symbols: Vec<String> = g.instances.iter().map(|i| g.symbol(&ws, i)).collect();
    for name in ["count", "iter", "next"] {
        assert!(
            symbols
                .iter()
                .any(|s| s.contains(name) && s.contains("int")),
            "global initializer did not seed concrete `{name}` reachability: {symbols:?}"
        );
    }
}

#[test]
fn always_emitted_for_protocol_edges_are_instantiated() {
    let (ws, g) = graph_of(
        r#"
global TOTAL: int = {
    let a: Arena<int> = arena_new()
    let total = 0
    for (_, v) in a { total += v }
    total
}
fn main() { print("{TOTAL}") }
"#,
    );
    let symbols: Vec<String> = g.instances.iter().map(|i| g.symbol(&ws, i)).collect();
    for name in ["iter", "next"] {
        assert!(
            symbols
                .iter()
                .any(|s| s.contains(name) && s.contains("int")),
            "global initializer did not seed concrete `{name}` reachability: {symbols:?}"
        );
    }
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
        .edges()
        .filter(|e| e.kind == EdgeKind::IteratorIter)
        .map(|e| g.symbol(&ws, &e.to))
        .collect();
    assert!(
        !iters.is_empty(),
        "a concrete `iter` witness produced no edge"
    );
    assert!(
        g.edges()
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
    let nexts: Vec<&instance_graph::Edge> = g
        .edges()
        .filter(|e| e.kind == EdgeKind::IteratorNext)
        .collect();
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
    let roots = skull::mir_emit::emission_roots(&ws, None);
    let g = instance_graph::build(&ws, roots.clone(), false);
    assert!(
        !g.unresolved().any(|u| u.reason.contains("unbound")),
        "a witness degraded to unbound: {:?}",
        g.unresolved()
            .map(|u| (
                u.reason,
                ws.module(u.callee.module).functions[u.callee.func].name
            ))
            .collect::<Vec<_>>()
    );
    let witnesses: Vec<&instance_graph::Edge> = g
        .edges()
        .filter(|e| e.kind == EdgeKind::IteratorIter)
        .collect();
    assert!(!witnesses.is_empty(), "no iterator-iter edge");

    // The witness is ONE instance, and each artifact realizes it to its own
    // declaration. Selection is what used to fork the walk, and forking it is
    // what let a generic witness instantiate differently on one target only.
    use skull::artifact::{self, ArtifactRequest};
    use skull::target_projection as tp;
    let lua = artifact::realize(&ws, &g, &ArtifactRequest::new(tp::LUAJIT_DESKTOP, &roots));
    let js = artifact::realize(&ws, &g, &ArtifactRequest::new(tp::JAVASCRIPT_WEB, &roots));
    let mut selected = 0usize;
    for e in &witnesses {
        let (Some(l), Some(j)) = (lua.realization.get(&e.to), js.realization.get(&e.to)) else {
            panic!("an artifact did not retain the witness at all");
        };
        // Same instance -- it is the same key in both maps -- with the JS
        // sibling chosen only on JS.
        assert_eq!(
            l.decl, e.to.func,
            "luajit did not select the representative itself"
        );
        selected += usize::from(j.decl != e.to.func);
    }
    assert!(
        selected > 0,
        "no witness selected a different declaration on JS, so the shared-instance \
         claim is vacuous -- the fixture is not exercising backend variants"
    );
}

// ---------------------------------------------------------------------------
// Stage 2 gate: graph closure and symbol injectivity.
// ---------------------------------------------------------------------------

/// Every reached instance was expanded or is a boundary, nothing was cut by the
/// depth cap, and no two distinct instances encode to one logical symbol.
///
/// NOT a closure assertion. `is_closed()` also requires that no generic emission
/// root went uninstantiated, and a program built against the whole stdlib always
/// leaves some -- the corpus answers `closed: NO` for that reason. This pins the
/// three properties that do hold; the depth-cap arm gets its positive from
/// `changing_instance_recursion_is_rejected`.
#[test]
fn every_instance_is_expanded_or_a_boundary() {
    let (ws, g) = graph_of(
        r#"
fn ident<T>(x: T) -> T { return x }
fn main() {
    let xs: [int] = [1, 2, 3]
    let ys: [string] = ["a"]
    xs.push(ident(4))
    ys.push(ident("b"))
    let total = 0
    for v in xs { total = total + v }
}
"#,
    );
    let c = g.closure(&ws);
    assert!(
        c.unexpanded.is_empty(),
        "instances with a missing body: {:?}",
        c.unexpanded
    );
    assert!(
        c.depth_capped.is_empty(),
        "expansion was cut: {:?}",
        c.depth_capped
    );
    // The logical-symbol collisions a non-injective `identity` used to HIDE and
    // the target-per-graph walk then made real: `__nb_checked_index` is declared
    // `inline(lua)` and `inline(js)`, both `@nomangle`, both emission roots, and
    // both were separate instances. One target-neutral graph holds the group
    // REPRESENTATIVE only, so there is one instance and one symbol.
    //
    // Corpus-wide this is NOT yet empty: a same-BACKEND `@nomangle` pair has no
    // target to tell it apart and is a Stage-3 emit collision
    // (`skull_mir::mangle`'s `collision_errors`), not a graph artifact. This
    // fixture holds no such pair, which is why it can assert the property
    // directly.
    assert!(
        c.colliding_symbols.is_empty(),
        "two instances spell alike: {:?}",
        c.colliding_symbols.keys().collect::<Vec<_>>()
    );
    // Expansion is RECORDED, so the count is evidence rather than a restatement
    // of the instance count: the fixture reaches stdlib boundaries, which are
    // never expanded and are excused by class, not by silence.
    assert!(c.expanded > 0, "nothing was expanded");
    assert!(
        c.expanded < c.instances,
        "every reached instance counted as expanded ({}/{}), so the boundary arm of the \
         closure gate has no population and asserts nothing",
        c.expanded,
        c.instances
    );
}

/// Encoder injectivity is the property `symbol` CANNOT provide. The
/// render-collision adversary produces two instances that display alike; their
/// LOGICAL symbols must still differ. The final emitted spelling is an
/// artifact-ABI-plan property (plan S 0h) and is not what this asserts.
#[test]
fn instances_that_render_alike_get_distinct_logical_symbols() {
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
        .filter(|i| {
            ws.module(i.func.module).functions[i.func.func]
                .name
                .as_str()
                == "ident"
        })
        .collect();
    assert_eq!(idents.len(), 2);
    let display: Vec<String> = idents.iter().map(|i| g.symbol(&ws, i)).collect();
    assert_eq!(
        display[0], display[1],
        "the fixture is no longer the collision it tests"
    );
    let encoded: Vec<String> = idents.iter().map(|i| g.logical_symbol(&ws, i)).collect();
    assert_ne!(
        encoded[0], encoded[1],
        "two instances encode alike as `{}`",
        encoded[0]
    );
    // The ENCODER is injective over these two, which is what this test is
    // about. The graph's own collision set is not asserted here: it holds the
    // stdlib's backend-sibling collisions, pinned as a sentinel in
    // `every_instance_is_expanded_or_a_boundary`.
    assert!(
        !g.closure(&ws).colliding_symbols.contains_key(&encoded[0]),
        "the two `ident` instances collide after all"
    );
}

/// A non-generic instance keeps its base mangling -- the encoder must not rename
/// every function the moment instances exist.
#[test]
fn a_non_generic_instance_keeps_its_base_symbol() {
    let (ws, g) = graph_of(
        r#"
fn bump(x: int) -> int { return x + 1 }
fn main() { bump(1) }
"#,
    );
    let bump = g
        .instances
        .iter()
        .find(|i| {
            ws.module(i.func.module).functions[i.func.func]
                .name
                .as_str()
                == "bump"
        })
        .expect("no `bump` instance");
    assert_eq!(g.logical_symbol(&ws, bump), "bump_int");
}

/// A call INSIDE a shared generic body, walked at an instance, resolves through
/// the caller's substitution and reaches a CONCRETE callee.
///
/// Emission measurements price this population at 43 pairs / 46 sites
/// / 23 callees, but that is measured on the EMISSION path, which still lowers
/// shared generic bodies. In the graph every walked body is an instance, so the
/// binding always has a substitution to resolve against, and no `unbound-generic`
/// can arise; this fixture is one level deeper than
/// `instantiation_flows_through_a_generic_caller` -- the inner call binds a
/// collection over the caller's own generic rather than the generic itself.
#[test]
fn a_callee_binding_inside_a_generic_body_is_concrete() {
    let (ws, g) = graph_of(
        r#"
fn hold<T>(x: T) -> int {
    let xs: [T] = [x]
    xs.push(x)
    return xs.len()
}
fn main() {
    hold(1)
    hold("s")
}
"#,
    );
    assert!(
        !g.unresolved().any(|u| u.reason == "unbound-generic"),
        "a callee binding inside a generic body had no substitution: {:?}",
        g.unresolved().map(|u| u.reason).collect::<Vec<_>>()
    );
    let pushes: Vec<String> = g
        .instances
        .iter()
        .filter(|i| {
            ws.module(i.func.module).functions[i.func.func]
                .name
                .as_str()
                == "push"
        })
        .map(|i| g.symbol(&ws, i))
        .collect();
    assert!(
        pushes.iter().any(|s| s.contains("int")) && pushes.iter().any(|s| s.contains("string")),
        "`push` was not instantiated at both of `hold`'s instantiations: {pushes:?}"
    );
    assert!(
        g.instances
            .iter()
            .filter(|i| ws.module(i.func.module).functions[i.func.func]
                .name
                .as_str()
                == "hold")
            .all(|i| !i.args.is_empty()),
        "a `hold` instance carries no arguments"
    );
}

// ---------------------------------------------------------------------------
// Stage 2: synthesized column-intercept calls are gated on the SAME storage
// oracle lowering uses (`skull_mir::lower::ContainerPlan`), not on the trigger
// name alone.
// ---------------------------------------------------------------------------

fn generated_edges(ws: &Workspace, g: &instance_graph::InstanceGraph) -> Vec<String> {
    let mut v: Vec<String> = g
        .edges()
        .filter(|e| e.kind == EdgeKind::CompilerGenerated)
        .map(|e| {
            ws.module(e.to.func.module).functions[e.to.func.func]
                .name
                .to_string()
        })
        .collect();
    v.sort();
    v.dedup();
    v
}

/// A COLUMN-stored element takes the intercept: `[P]` over an all-scalar value
/// struct stores as flat columns, so `sort`/`clone`/`insert` route through the
/// column helpers.
#[test]
fn a_column_element_produces_the_synthesized_intercept() {
    let (ws, g) = graph_of(
        r#"
value P { x: float, y: float }
fn main() {
    let xs: [P] = [P { x: 1.0, y: 2.0 }]
    xs.push(P { x: 3.0, y: 4.0 })
    let c = xs.clone()
    xs.sort(_.x)
}
"#,
    );
    let names = generated_edges(&ws, &g);
    assert!(
        names.iter().any(|n| n.starts_with("__rock_col_")),
        "a column-stored `[P]` produced no column intercept: {names:?}"
    );
}

/// CONTROL, and the one that matters: a BOXED element must produce NONE of
/// them. The trigger names are identical -- `sort`, `clone`, `push` on a `[int]`
/// -- so a table keyed on the name alone reports edges emission never makes,
/// which is exactly the over-approximation this replaced.
#[test]
fn a_boxed_element_produces_no_synthesized_intercept() {
    let (ws, g) = graph_of(
        r#"
fn main() {
    let xs: [int] = [1, 2, 3]
    xs.push(4)
    let c = xs.clone()
    xs.sort(_ + 0)
}
"#,
    );
    let names = generated_edges(&ws, &g);
    assert!(
        !names
            .iter()
            .any(|n| n.starts_with("__rock_col_") || n.starts_with("__map_col_")),
        "a boxed `[int]` produced column intercepts: {names:?}"
    );
}

/// The Map sibling of the pair above: a column-stored VALUE takes the intercept,
/// a boxed one does not.
#[test]
fn map_column_intercepts_follow_the_value_type() {
    let (ws, col) = graph_of(
        r#"
value P { x: float, y: float }
fn main() {
    let m: Map<string, P> = map_new()
    m["a"] = P { x: 1.0, y: 2.0 }
    let n = m.len()
}
"#,
    );
    let (ws2, boxed) = graph_of(
        r#"
fn main() {
    let m: Map<string, string> = map_new()
    m["a"] = "b"
    let n = m.len()
}
"#,
    );
    assert!(
        generated_edges(&ws, &col)
            .iter()
            .any(|n| n.starts_with("__map_col_")),
        "a column-stored Map value produced no intercept: {:?}",
        generated_edges(&ws, &col)
    );
    assert!(
        !generated_edges(&ws2, &boxed)
            .iter()
            .any(|n| n.starts_with("__map_col_")),
        "a boxed Map value produced column intercepts: {:?}",
        generated_edges(&ws2, &boxed)
    );
}

/// The intercept resolves through the INSTANCE's substitution. The trigger is
/// not at the user's call site at all -- `xs.clone()` calls the stdlib's
/// generic `clone`, whose body holds the `__vec_data_clone` trigger -- so the
/// SAME body and the SAME site must intercept at `[P]` and not at `[int]`.
/// Keying on the trigger name cannot express that difference.
#[test]
fn the_storage_condition_follows_the_instantiation() {
    let src = r#"
value P { x: float, y: float }
fn dup<T>(xs: [T]) -> [T] { return xs.clone() }
fn main() {
    let a: [P] = [P { x: 1.0, y: 2.0 }]
    let b: [int] = [1, 2, 3]
    dup(a)
    dup(b)
}
"#;
    let (ws, g) = graph_of(src);
    // Both instantiations exist, so the fixture really does put one body under
    // two substitutions.
    let dups: Vec<String> = g
        .instances
        .iter()
        .filter(|i| {
            ws.module(i.func.module).functions[i.func.func]
                .name
                .as_str()
                == "dup"
        })
        .map(|i| g.symbol(&ws, i))
        .collect();
    assert_eq!(dups.len(), 2, "expected `dup<P>` and `dup<int>`: {dups:?}");

    // Every column intercept comes from an instance instantiated at `P`. None
    // comes from the `int` one, which is the discriminant.
    let sources: Vec<String> = g
        .edges()
        .filter(|e| {
            e.kind == EdgeKind::CompilerGenerated
                && ws.module(e.to.func.module).functions[e.to.func.func]
                    .name
                    .as_str()
                    .starts_with("__rock_col_")
        })
        .map(|e| g.symbol(&ws, &e.from))
        .collect();
    assert!(
        !sources.is_empty(),
        "no column intercept at all: the fixture proves nothing"
    );
    assert!(
        sources.iter().all(|s| s.contains("P")),
        "a column intercept came from a non-`P` instantiation: {sources:?}"
    );
    assert!(
        !sources.iter().any(|s| s.contains("<int>")),
        "the `int` instantiation gained an intercept it never emits: {sources:?}"
    );
}

// ---------------------------------------------------------------------------
// Stage 2: `@intrinsic` is not unconditionally a boundary.
// ---------------------------------------------------------------------------

fn is_expanded(ws: &Workspace, g: &instance_graph::InstanceGraph, name: &str) -> bool {
    g.expanded().any(|i| {
        ws.module(i.func.module).functions[i.func.func]
            .name
            .as_str()
            == name
    })
}

fn reached(ws: &Workspace, g: &instance_graph::InstanceGraph, name: &str) -> bool {
    g.instances.iter().any(|i| {
        ws.module(i.func.module).functions[i.func.func]
            .name
            .as_str()
            == name
    })
}

/// HOSTILE: an `@intrinsic` whose body is a portable ROCK fallback. Emit
/// resolution is `backend mapping > portable fallback > reject`, so the shared
/// graph must EXPAND that fallback -- it is the body a target with no mapping
/// runs, and its callees are real edges. Classifying every `@intrinsic` as
/// foreign made "expanded or boundary" a label rather than a partition: the walk
/// enumerated the body and recorded its edges while the closure gate excused the
/// instance as having none.
#[test]
fn a_rock_bodied_intrinsic_expands_its_fallback() {
    let (ws, g) = graph_of(
        r#"
fn helper_called_only_by_the_fallback<T>(x: T) -> T { return x }
@intrinsic fn portable_op(x: int) -> int {
    return helper_called_only_by_the_fallback(x)
}
fn main() { portable_op(1) }
"#,
    );
    assert!(
        is_expanded(&ws, &g, "portable_op"),
        "a Rock-bodied intrinsic was excused as a boundary instead of expanded"
    );
    // GENERIC on purpose: a non-generic callee is an emission root in its own
    // right, so its presence would prove nothing about whether the fallback body
    // was walked. A generic one has an INSTANCE only if some body instantiated
    // it.
    assert!(
        reached(&ws, &g, "helper_called_only_by_the_fallback"),
        "the fallback body's callee is missing from the graph"
    );
    assert!(
        !g.boundaries.keys().any(|k| k.contains("portable_op")),
        "a Rock-bodied intrinsic was classified as a foreign boundary: {:?}",
        g.boundaries.keys().collect::<Vec<_>>()
    );
}

/// CONTROL: `@native_required` SKIPS the fallback rung -- a missing backend
/// mapping is fatal rather than portable -- so its body is never the emitted one
/// and it stays a boundary even with a Rock body beside it.
#[test]
fn a_native_required_intrinsic_stays_a_boundary() {
    let (ws, g) = graph_of(
        r#"
fn unreachable_through_a_native_required_body<T>(x: T) -> T { return x }
@intrinsic @native_required fn pinned_op(x: int) -> int {
    return unreachable_through_a_native_required_body(x)
}
fn main() { pinned_op(1) }
"#,
    );
    assert!(
        !is_expanded(&ws, &g, "pinned_op"),
        "a native-required intrinsic expanded a fallback rung it skips"
    );
    assert!(
        g.boundaries.keys().any(|k| k.contains("pinned_op")),
        "a native-required intrinsic was not classified as a boundary"
    );
    // The fixture's callee is named for what must be true of it, and is GENERIC
    // so that only an instantiation puts it in the graph. Marking the instance
    // unexpanded while still enumerating its body left this reachable: a body
    // emission never routes through, contributing edges to the graph.
    assert!(
        !reached(&ws, &g, "unreachable_through_a_native_required_body"),
        "a callee reachable ONLY through a skipped fallback rung is in the graph"
    );
    let out: Vec<String> = g
        .edges()
        .filter(|e| {
            ws.module(e.from.func.module).functions[e.from.func.func]
                .name
                .as_str()
                == "pinned_op"
        })
        .map(|e| g.symbol(&ws, &e.to))
        .collect();
    assert!(
        out.is_empty(),
        "a boundary produced outgoing edges: {out:?}"
    );
}

/// CONTROL: an intrinsic with only a FOREIGN body has nothing to expand.
#[test]
fn a_foreign_bodied_intrinsic_stays_a_boundary() {
    let (ws, g) = graph_of(
        r#"
@intrinsic inline(lua) fn mapped_op(x: int) -> int = "return x + 1"
fn main() { mapped_op(1) }
"#,
    );
    assert!(
        !is_expanded(&ws, &g, "mapped_op"),
        "an inline-only intrinsic reported an expansion"
    );
    assert!(
        g.boundaries.keys().any(|k| k.contains("mapped_op")),
        "an inline-only intrinsic was not classified as a boundary"
    );
    assert!(
        !g.edges().any(|e| {
            ws.module(e.from.func.module).functions[e.from.func.func]
                .name
                .as_str()
                == "mapped_op"
        }),
        "a foreign-bodied boundary produced outgoing edges"
    );
}

/// Every instance is EITHER expanded or classified as a boundary -- the property
/// the closure gate's arithmetic depends on, asserted directly rather than
/// inferred from `unexpanded` being empty.
#[test]
fn expanded_and_boundary_partition_the_instances() {
    let (ws, g) = graph_of(
        r#"
value P { x: float, y: float }
@intrinsic fn portable_op(x: int) -> int { return x + 1 }
fn main() {
    let xs: [P] = [P { x: 1.0, y: 2.0 }]
    xs.push(P { x: 3.0, y: 4.0 })
    portable_op(1)
    let s = "{xs.len()}"
}
"#,
    );
    let c = g.closure(&ws);
    assert!(
        c.unexpanded.is_empty(),
        "neither expanded nor a boundary: {:?}",
        c.unexpanded
    );
    let both: Vec<String> = g
        .expanded()
        .filter(|i| {
            instance_graph::boundary_class_for(
                &ws.module(i.func.module).functions[i.func.func],
                &g.erased_entries,
            )
            .is_some()
        })
        .map(|i| g.identity(&ws, i))
        .collect();
    assert!(
        both.is_empty(),
        "instances counted as BOTH expanded and boundary: {both:?}"
    );
    assert!(c.expanded > 0 && c.expanded < c.instances);
}

/// Every name a synthesized-call table can produce belongs to the ONE
/// catalogue. Two lists that must agree will eventually not; this makes the
/// disagreement a test failure rather than a symbol emission never emits.
#[test]
fn synthesized_targets_belong_to_the_runtime_catalogue() {
    let catalogue = instance_graph::ERASED_RUNTIME_ENTRIES;
    let mut missing: Vec<&str> = Vec::new();
    for (_, _, names) in instance_graph::COMPILER_GENERATED_TRIGGERS {
        missing.extend(names.iter().filter(|n| !catalogue.contains(n)));
    }
    for (_, _, names) in instance_graph::INDEX_TRIGGERED {
        missing.extend(names.iter().filter(|n| !catalogue.contains(n)));
    }
    assert!(
        missing.is_empty(),
        "synthesized targets outside the catalogue: {missing:?}"
    );
}

/// A generic emission root can have concrete instances from other call sites
/// and STILL be a hole. Its obligation is to exist under a BARE name that
/// opaque Lua text can call; instances do not discharge that, a boundary
/// contract has to.
///
/// The root is recorded before the worklist runs, so at insertion time no
/// instance of it exists -- which is how a name that later calls fully
/// instantiated went on reading as having no concrete body at all. Both halves
/// are asserted here: the count is visible, and the hole does not close.
///
/// The fixture DECLARES its own `@nomangle` generic rather than borrowing a
/// stdlib one. A stdlib name is a live entry in the obligation matrix, so
/// closing that entry silently disarms this test while it stays green -- which
/// is what happened when `vec_push`'s last opaque caller was removed and its
/// `@nomangle` dropped. A declaration the test owns cannot be closed out from
/// under it.
#[test]
fn a_root_with_instances_reports_them_and_stays_a_hole() {
    let (ws, g) = graph_of(
        r#"
@nomangle fn keep<T>(x: T) -> T { return x }
fn main() {
    let a = keep(1)
    let b = keep("s")
}
"#,
    );
    let hole = g
        .uninstantiated_roots
        .get("keep")
        .unwrap_or_else(|| panic!("`keep` is not a root: {:?}", g.uninstantiated_roots.keys()));
    assert!(
        hole.generic_root,
        "`keep` was recorded under the wrong cause: {hole:?}"
    );
    assert!(
        hole.instances >= 2,
        "`keep` is called at two type arguments but reports {} instances",
        hole.instances
    );
    assert!(
        g.closure(&ws).uninstantiated_roots.contains("keep"),
        "a root with instances stopped counting as a hole -- the falsely-green reading"
    );
    assert!(
        !g.closure(&ws).is_closed(),
        "the fixture hole vanished from the closure gate"
    );
}

/// The narrowing in `generic_root_obligation`, both sides. A generic emission
/// root whose emitted symbol is MANGLED carries no bare-name obligation --
/// opaque text cannot spell a mangling the compiler owns -- while the same
/// declaration with `@nomangle` does.
///
/// Both arms come from ONE source differing only in the annotation, so a
/// difference cannot be an artifact of two fixtures.
#[test]
fn only_a_bare_symbol_carries_a_bare_name_obligation() {
    let src = |anno: &str| {
        format!(
            r#"
{anno} fn keep<T>(x: T) -> T {{ return x }}
fn main() {{ let a = keep(1) }}
"#
        )
    };
    let (_, bare) = graph_of(&src("@nomangle"));
    assert!(
        bare.uninstantiated_roots.contains_key("keep"),
        "a `@nomangle` generic root IS a bare-name obligation: {:?}",
        bare.uninstantiated_roots.keys()
    );

    let (_, mangled) = graph_of(&src(""));
    let excused: Vec<&String> = mangled
        .excused_generic_roots
        .get("mangled")
        .map(|s| s.iter().collect())
        .unwrap_or_default();
    assert!(
        excused.iter().any(|n| n.starts_with("keep_")),
        "the mangled twin must be EXCUSED, not silently absent: {excused:?}"
    );
    assert!(
        !mangled
            .uninstantiated_roots
            .keys()
            .any(|n| n.starts_with("keep")),
        "a mangled generic root is not an obligation: {:?}",
        mangled.uninstantiated_roots.keys()
    );
}

/// A graph over `src` alone, with no stdlib, so a fixture can supply the one
/// declaration of a runtime-entry name itself.
fn graph_of_without_stdlib(src: &str) -> instance_graph::InstanceGraph {
    let mut modules =
        vec![skull_parser::parse_module_clean(src, Ustr::from("<test>")).expect("parse")];
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    let roots = skull::mir_emit::emission_roots(&ws, None);
    instance_graph::build(&ws, roots, false)
}

/// The `stdlib-funcref` excusal is a DECLARATION property, never a name one.
/// `ERASED_RUNTIME_ENTRIES` holds NAMES, and membership alone is a proxy for
/// "this is the entry `stdlib_funcref` resolves to" -- a weaker question. A
/// `@nomangle` generic sharing a catalogue name is not that entry when another
/// declaration shares the name too.
///
/// The stdlib's own runtime entries are all non-generic, so the fixture builds
/// without the stdlib and declares the generic entry itself. A/B on exactly
/// that, one fixture differing only by a shadowing overload: alone, the entry
/// is what the resolver selects and is excused; with the overload the resolver
/// is ambiguous -- what `stdlib_funcref` itself rejects on -- so nothing is
/// excused and the obligation stands.
#[test]
fn the_funcref_excusal_follows_the_declaration_not_the_name() {
    let entry = "@nomangle fn __rock_col_sort<T>(x: T) -> int { return 1 }\nfn main() { }\n";
    let plain = graph_of_without_stdlib(entry);
    assert!(
        plain
            .excused_generic_roots
            .get("stdlib-funcref")
            .is_some_and(|s| s.contains("__rock_col_sort")),
        "the only `__rock_col_sort` IS the entry `stdlib_funcref` resolves to, so \
         it must be excused: {:?}",
        plain.excused_generic_roots
    );
    assert!(
        !plain.uninstantiated_roots.contains_key("__rock_col_sort"),
        "...and therefore not an obligation: {:?}",
        plain.uninstantiated_roots.keys()
    );

    let shadowed = graph_of_without_stdlib(&format!(
        "{entry}@nomangle fn __rock_col_sort<T>(x: T, y: T) -> int {{ return 2 }}\n"
    ));
    assert!(
        shadowed
            .uninstantiated_roots
            .contains_key("__rock_col_sort"),
        "a same-name overload makes the entry unresolvable, so the obligation \
         returns rather than riding the catalogue name: excused={:?}",
        shadowed.excused_generic_roots
    );
    assert!(
        !shadowed
            .excused_generic_roots
            .get("stdlib-funcref")
            .is_some_and(|s| s.contains("__rock_col_sort")),
        "nothing may be excused under a name the resolver cannot decide: {:?}",
        shadowed.excused_generic_roots
    );
}

/// The OTHER cause: a call from an always-emitted body (here a global
/// initializer) whose instantiation fails. It has no `from` instance to be
/// filed against as an [`Unresolved`] or a [`Rejection`], but it carries the
/// same reason those would.
///
/// The corpus has NO population for this cause -- all 14 of its holes are
/// generic emission roots -- so this fixture is its only evidence.
///
/// The fixture is built with a project root the test module does NOT sit under,
/// which makes every fn in it a library fn. Without that, `<test>` is the
/// project and `pick` is an emission root too, so the two causes coincide and
/// the fixture cannot show that this one fires on its own.
#[test]
fn an_always_emitted_call_hole_keeps_its_reason_and_span() {
    let ws = check(
        r#"
fn pick<T>() -> int {
    let unused: T = 1
    return 1
}
global G: int = pick()
fn main() { let x = G }
"#,
    );
    let roots = skull::mir_emit::emission_roots(&ws, Some("<project>"));
    let g = instance_graph::build(&ws, roots, false);
    let hole = g
        .uninstantiated_roots
        .get("pick_0")
        .unwrap_or_else(|| panic!("no hole recorded for `pick`: {:?}", g.uninstantiated_roots));
    assert_eq!(
        hole.always_emitted_sites.len(),
        1,
        "expected exactly the one failing site: {hole:?}"
    );
    let site = &hole.always_emitted_sites[0];
    // The CLASSIFICATION, not a reason string. `pick<T>()` reads `T` in its
    // body, but this call provides no binding for it, so it is an
    // `UnconstrainedTypeVar` rejection. The control below is the same call
    // from a function body, which becomes exactly that rejection.
    // Recording only a reason forced Stage 3 to recover this by parsing it.
    assert_eq!(
        site.outcome,
        instance_graph::AlwaysEmittedOutcome::Rejection(
            instance_graph::RejectionKind::UnconstrainedTypeVar
        ),
        "the always-emitted record lost the rejection classification: {hole:?}"
    );
    // The SPAN is the point: this fact becomes a compile error at the Stage-3
    // cutover, and every caller already had the call-site span while the
    // recorder dropped it.
    assert!(
        !instance_graph::is_spanless(&site.span),
        "the always-emitted hole carries no source span, so it can never produce a \
         located diagnostic: {hole:?}"
    );
    assert!(
        !hole.generic_root,
        "the fixture did not isolate the cause -- `pick` is an emission root here too: {hole:?}"
    );
    // CONTROL: the same failure from an INSTANTIATED body is the stronger
    // classification -- a rejection with a span -- and must not be filed here.
    let (_ws2, g2) = graph_of(
        r#"
fn pick<T>() -> int {
    let unused: T = 1
    return 1
}
fn main() { let x = pick() }
"#,
    );
    assert!(
        g2.rejections()
            .any(|r| r.kind == instance_graph::RejectionKind::UnconstrainedTypeVar),
        "the same call from a function body did not reject: {:?}",
        g2.rejections()
            .map(|r| r.message.clone())
            .collect::<Vec<_>>()
    );
}

/// Plan S 2: "Retain each edge's source span, for good errors at unresolved
/// boundaries." An edge whose span is unknown cannot produce one, so the
/// property is per-EDGE and not per-edge-kind: one spanless edge in a kind that
/// is otherwise well-spanned is still a rejection with no location.
///
/// The fixture's coverage is asserted first. Without that, a walk that emitted
/// no edges of a kind would pass this by having nothing to check.
#[test]
fn every_edge_carries_a_source_span() {
    let (ws, g) = graph_of(
        r#"
value P { x: float, y: float }
struct Hold<T> { v: T }
fn ident<T>(x: T) -> T { return x }
fn bump(x: int) -> int { return x + 1 }
fn apply(f: fn(int) -> int, v: int) -> int { return f(v) }
fn total(xs: [int]) -> int {
    let t = 0
    for x in xs { t = t + x }
    return t
}
fn main() {
    let a = ident(1)
    let b = bump(2)
    let c = apply(bump, 3)
    let xs: [int] = [1, 2, 3]
    let s = total(xs)
    let m = xs.map(_ * 2).sum()
    let mp: Map<string, P> = map_new()
    mp["a"] = P { x: 1.0, y: 2.0 }
    let n = mp.len()
    let h = Hold { v: 1 }
}
"#,
    );
    let kinds: std::collections::BTreeSet<&'static str> =
        g.edges().map(|e| e.kind.name()).collect();
    for want in [
        "direct-call",
        "generic-call",
        "iterator-iter",
        "iterator-next",
        "fn-value",
        "implicit-iter",
        "bound-next-witness",
        "compiler-generated",
    ] {
        assert!(
            kinds.contains(want),
            "fixture produced no `{want}` edge; covered: {kinds:?}"
        );
    }
    let mut spanless: std::collections::BTreeMap<&'static str, Vec<String>> = Default::default();
    for e in g.edges() {
        if instance_graph::is_spanless(&e.span) {
            let v = spanless.entry(e.kind.name()).or_default();
            if v.len() < 5 {
                v.push(format!(
                    "{} -> {}",
                    g.symbol(&ws, &e.from),
                    g.symbol(&ws, &e.to)
                ));
            }
        }
    }
    assert!(
        spanless.is_empty(),
        "{} of {} edges carry no source span: {spanless:#?}",
        g.edges()
            .filter(|e| instance_graph::is_spanless(&e.span))
            .count(),
        g.edges().count()
    );
}

/// HOSTILE: an opaque `inline(lua)` body that CALLS a compiler-owned runtime
/// entry. No Rock call site reaches it, no lowering arm synthesizes it, and the
/// walk cannot enter the body -- so without the dependency scan the symbol
/// vanishes from the graph while emission still has to emit it. That is exactly
/// how `__rock_vec_column_elements` disappeared once the name-keyed
/// over-approximation stopped covering for it.
#[test]
fn an_opaque_body_declares_its_runtime_dependency() {
    let (ws, g) = graph_of(
        r#"
@nomangle inline(lua) fn opaque_caller(v: any) -> any = """
    return __rock_vec_column_elements(v)
"""
fn main() { opaque_caller(1) }
"#,
    );
    let deps: Vec<String> = g
        .inline_body_deps
        .iter()
        .flat_map(|(owner, names)| {
            let name = ws.module(owner.0.module).functions[owner.0.func].name;
            names.iter().map(move |n| format!("{name} -> {n}"))
        })
        .collect();
    assert!(
        deps.iter()
            .any(|d| d.contains("opaque_caller") && d.contains("__rock_vec_column_elements")),
        "an opaque body's runtime dependency was not recorded: {deps:?}"
    );
    assert!(
        g.erased_entries.contains("__rock_vec_column_elements"),
        "the dependency did not reach the erased-entry set emission draws from"
    );
    // CONTROL: the scan does not simply report the whole catalogue.
    assert!(
        !g.erased_entries.contains("__map_col_slot_checked"),
        "the dependency scan reported an entry the body never names: {:?}",
        g.erased_entries
    );
}

/// The other rejection an always-emitted body can hold: a generic function
/// taken as a VALUE by a global initializer. `GenericFnValue` is a decided S 0h
/// rejection, and it stays one when there is no `from` instance to file it
/// against.
#[test]
fn a_generic_fn_value_in_an_always_emitted_body_keeps_its_rejection_kind() {
    let ws = check(
        r#"
fn ident<T>(x: T) -> T { return x }
global F: fn(int) -> int = ident
fn main() { let y = F(1) }
"#,
    );
    let roots = skull::mir_emit::emission_roots(&ws, Some("<project>"));
    let g = instance_graph::build(&ws, roots, false);
    let sites: Vec<&instance_graph::AlwaysEmittedFailure> = g
        .uninstantiated_roots
        .values()
        .flat_map(|h| h.always_emitted_sites.iter())
        .collect();
    assert!(
        !sites.is_empty(),
        "the fixture recorded no always-emitted failure"
    );
    assert!(
        sites.iter().any(|s| s.outcome
            == instance_graph::AlwaysEmittedOutcome::Rejection(
                instance_graph::RejectionKind::GenericFnValue
            )),
        "a generic fn value in a global initializer lost its rejection kind: {sites:?}"
    );
    assert!(
        sites.iter().all(|s| !instance_graph::is_spanless(&s.span)),
        "a generic-fn-value hole carries no span: {sites:?}"
    );
}

/// `record_call`'s own `Unresolved` write, reached the way the corpus reached
/// it: `instantiate` answers `no-bindings` when a generic call has no
/// `call_generic_bindings` record. That is not a poisoned program -- the plan
/// measured `no-bindings` 126 times on valid corpus code before the graph fix
/// -- so the fixture removes the RECORD from a successfully inferred workspace
/// rather than corrupting the source.
///
/// `BodyInference` is public result data, so this reaches the recorder without
/// inventing an invalid program, and it is what makes a `Span::unknown`
/// mutation in `record_call` visible.
#[test]
fn a_call_with_no_binding_record_is_unresolved_with_a_span() {
    let mut ws = check(
        r#"
fn ident<T>(x: T) -> T { return x }
fn main() { let a = ident(1) }
"#,
    );
    let removed = drop_generic_binding_records(&mut ws, "main");
    // EXACT: `main` has one generic call, so one record is dropped and one site
    // must appear. This equality only became assertable once the helper stopped
    // applying a module-local `BodyOwner` to every module -- before that it
    // removed a same-index stdlib `get` record too and reported 2.
    assert_eq!(
        removed, 1,
        "expected exactly `ident`'s binding record to be dropped"
    );
    let roots = skull::mir_emit::emission_roots(&ws, None);
    let g = instance_graph::build(&ws, roots, false);
    let sites: Vec<&instance_graph::Unresolved> = g
        .unresolved()
        .filter(|u| u.reason == "no-bindings")
        .collect();
    let callees: Vec<&str> = sites
        .iter()
        .map(|u| {
            ws.module(u.callee.module).functions[u.callee.func]
                .name
                .as_str()
        })
        .collect();
    assert_eq!(
        callees,
        vec!["ident"],
        "expected exactly the `ident` call to lose its instantiation, got {:?}",
        g.unresolved().map(|u| u.reason).collect::<Vec<_>>()
    );
    assert_eq!(
        sites.len(),
        removed,
        "one dropped record, one unresolved site"
    );
    assert!(
        sites.iter().all(|u| !instance_graph::is_spanless(&u.span)),
        "a `no-bindings` unresolved call carries no span: {sites:?}"
    );
}

/// The witness recorders' `Unresolved` write, same method: strip the
/// `for`-protocol witness generics that `instantiate_witness` reads.
#[test]
fn a_witness_with_no_generic_record_is_unresolved_with_a_span() {
    let mut ws = check(
        r#"
fn main() {
    let xs: [int] = [1, 2, 3]
    let t = 0
    for x in xs { t = t + x }
}
"#,
    );
    let removed = drop_witness_generic_records(&mut ws, "main");
    assert_eq!(
        removed, 1,
        "expected the one `for`-protocol ITER record to be dropped"
    );
    let roots = skull::mir_emit::emission_roots(&ws, None);
    let g = instance_graph::build(&ws, roots, false);
    // Discriminated by CALLEE, not by reason: both halves fail through
    // `instantiate_witness` and report `witness-generic-unbound`, so a
    // reason-only filter would accept the other recorder's site.
    let sites = unresolved_witnesses_of(&ws, &g, "iter");
    assert_eq!(
        sites.len(),
        removed,
        "expected exactly one unresolved `iter` witness, got {:?}",
        all_unresolved(&ws, &g)
    );
    assert!(
        sites.iter().all(|u| !instance_graph::is_spanless(&u.span)),
        "a witness unresolved site carries no span: {sites:?}"
    );
    // CONTROL: the NEXT half is untouched and still resolves. Without it this
    // fixture would also pass on a `record_bound_next` regression.
    assert!(
        g.edges().any(|e| e.kind == EdgeKind::IteratorNext),
        "the fixture disturbed the next half too, so it does not isolate `record_witness`"
    );
    assert!(
        unresolved_witnesses_of(&ws, &g, "next").is_empty(),
        "the next half went unresolved as well: {:?}",
        all_unresolved(&ws, &g)
    );
}

/// Plan S 9's precondition: a `ConcreteBody`'s type tables are built under the
/// instance's substitution, so that substitution must be a FUNCTION OF THE
/// INSTANCE. Nothing structural forces it -- the `VarId` keys come from
/// `generic_var_of` reading the CALLER's `BodyInference`, and the worklist
/// keeps whichever substitution arrived first -- so two callers of one instance
/// could disagree and the kept one would depend on discovery order.
///
/// The fixture reaches one instance from several callers, and the assertion is
/// on the COMPARISON count: an instance reached exactly once never tests
/// agreement, so a zero over recorded substitutions would say nothing.
#[test]
fn an_instance_substitution_does_not_depend_on_which_caller_reached_it() {
    let (ws, g) = graph_of(
        r#"
fn ident<T>(x: T) -> T { return x }
fn hold<T>(v: T) -> T { return ident(v) }
fn a() -> int { return ident(1) }
fn b() -> int { return hold(2) }
fn c() -> string { return ident("s") }
fn d() -> string { return hold("t") }
fn main() { a(); b(); c(); d() }
"#,
    );
    // The denominator must be tied to the CLAIMED instances. A global
    // comparison count is satisfied by any re-reached instance in the graph --
    // including stdlib ones the fixture says nothing about -- so it would not
    // prove that `ident<int>` and `ident<string>` were each reached through
    // both of their callers.
    let compared: std::collections::BTreeMap<String, usize> = g
        .instances
        .iter()
        .map(|i| (g.identity(&ws, i), g.subst_compared(i)))
        .filter(|(_, n)| *n > 0)
        .collect();
    // `ident<int>` is reached from `a` directly and from `hold<int>`
    // transitively, so its substitution is computed twice from two different
    // callers' inference; same for `string`. `hold` itself has ONE caller per
    // instantiation and is never re-reached -- measured, not assumed, and the
    // reason the claim names `ident` only.
    for arg in ["int", "string"] {
        let hits: usize = compared
            .iter()
            .filter(|(id, _)| id.starts_with("ident") && id.contains(arg))
            .map(|(_, n)| *n)
            .sum();
        assert!(
            hits > 0,
            "`ident` at `{arg}` was never reached from two callers, so its agreement was \
             never tested; compared: {compared:?}"
        );
    }
    let disagreements: Vec<String> = g
        .subst_disagreements()
        .map(|d| g.identity(&ws, &d.instance))
        .collect();
    assert!(
        disagreements.is_empty(),
        "{} of {} comparisons found an instance reached at two substitutions: {disagreements:?}",
        g.subst_disagreements().count(),
        g.subst_comparisons()
    );
    // The substitution is KEPT for EVERY instance, not merely checked --
    // `ConcreteBody` needs it, and the walk used to drop it on the floor. Not
    // restricted to generic instances: a non-generic one's substitution is
    // EMPTY, not absent, and skipping them hid that root seeding bypassed the
    // recorder entirely.
    let missing = g.instances_without_substitution(&ws);
    assert!(
        missing.is_empty(),
        "instances with no recorded substitution: {missing:?}"
    );
    assert!(
        g.substitution(&instance_graph::Instance {
            func: main_ref(&ws),
            args: Vec::new(),
        })
        .is_some(),
        "`main`, a non-generic root, has no recorded substitution"
    );
}

/// `main`'s `FuncRef`. Named directly so the assertion above is about the root
/// seeding path and not about whichever instance happens to sort first.
fn main_ref(ws: &Workspace) -> skull_mir::mir::FuncRef {
    for module in ws.modules.iter() {
        for (fid, f) in module.functions.iter() {
            if f.name.as_str() == "main" {
                return skull_mir::mir::FuncRef {
                    module: module.id,
                    func: fid,
                };
            }
        }
    }
    panic!("no `main`");
}

/// `record_bound_next`'s own `Unresolved` write. The three recorders do NOT
/// share one: `record_call`, `record_witness` and `record_bound_next` each push
/// independently, so covering the `iter` half of a `for` protocol says nothing
/// about the `next` half.
#[test]
fn a_bound_next_with_no_generic_record_is_unresolved_with_a_span() {
    let mut ws = check(
        r#"
fn main() {
    let xs: [int] = [1, 2, 3]
    let t = 0
    for x in xs { t = t + x }
}
"#,
    );
    let removed = drop_bound_next_generic_records(&mut ws, "main");
    assert_eq!(
        removed, 1,
        "expected the one `for`-protocol `next` record to be dropped"
    );
    let roots = skull::mir_emit::emission_roots(&ws, None);
    let g = instance_graph::build(&ws, roots, false);
    // Same discriminant as the iter fixture, pointed at the other callee.
    let sites = unresolved_witnesses_of(&ws, &g, "next");
    assert_eq!(
        sites.len(),
        removed,
        "expected exactly one unresolved `next` witness, got {:?}",
        all_unresolved(&ws, &g)
    );
    assert!(
        sites.iter().all(|u| !instance_graph::is_spanless(&u.span)),
        "a bound-next unresolved site carries no span: {sites:?}"
    );
    // CONTROL: the ITER half is untouched, so its witness still resolves. This
    // is what separates this fixture from the `record_witness` one -- without
    // it, either mutation would satisfy both tests.
    assert!(
        g.edges().any(|e| e.kind == EdgeKind::IteratorIter),
        "the fixture disturbed the iter half too, so it does not isolate `record_bound_next`"
    );
}

/// Unresolved witness sites whose CALLEE is `fn_name`. The `iter` and `next`
/// halves of a `for` protocol both fail through `instantiate_witness` with
/// `witness-generic-unbound`, so the callee is what separates the two
/// recorders.
fn unresolved_witnesses_of<'g>(
    ws: &Workspace,
    g: &'g instance_graph::InstanceGraph,
    fn_name: &str,
) -> Vec<&'g instance_graph::Unresolved> {
    g.unresolved()
        .filter(|u| {
            u.reason == "witness-generic-unbound"
                && ws.module(u.callee.module).functions[u.callee.func]
                    .name
                    .as_str()
                    == fn_name
        })
        .collect()
}

fn all_unresolved(ws: &Workspace, g: &instance_graph::InstanceGraph) -> Vec<(String, String)> {
    g.unresolved()
        .map(|u| {
            (
                u.reason.to_string(),
                ws.module(u.callee.module).functions[u.callee.func]
                    .name
                    .to_string(),
            )
        })
        .collect()
}

/// The one body to mutate, as `(module, owner)`.
///
/// A `BodyOwner::Function` holds a MODULE-LOCAL arena index, so applying one to
/// every module's inference table hits whatever stdlib function happens to share
/// that index. That is not hypothetical: it is what made
/// `drop_generic_binding_records` report two removals for a body with one
/// generic call, the second belonging to an unrelated stdlib `get`.
fn body_of(
    ws: &Workspace,
    fn_name: &str,
) -> (
    bone_semantic::prelude::ModuleId,
    bone_semantic::hir::BodyOwner,
) {
    for module in ws.modules.iter() {
        for (fid, f) in module.functions.iter() {
            if f.name.as_str() == fn_name {
                return (module.id, bone_semantic::hir::BodyOwner::Function(fid));
            }
        }
    }
    panic!("no fn named `{fn_name}`");
}

/// Run `f` over exactly one body's inference record.
fn with_body_inference<R>(
    ws: &mut Workspace,
    fn_name: &str,
    f: impl FnOnce(&mut bone_semantic::inference::BodyInference) -> R,
) -> R {
    let (mid, owner) = body_of(ws, fn_name);
    let inference = ws.inferences[mid.raw() as usize]
        .as_mut()
        .unwrap_or_else(|| panic!("`{fn_name}`'s module has no inference result"));
    let inf = inference
        .per_fn
        .get_mut(&owner)
        .unwrap_or_else(|| panic!("no inference record for `{fn_name}`"));
    f(inf)
}

/// Remove every `call_generic_bindings` entry in `fn_name`'s body, returning
/// how many were dropped. The COUNT is the denominator the caller asserts
/// against -- a helper that silently removed nothing would make its test vacuous.
fn drop_generic_binding_records(ws: &mut Workspace, fn_name: &str) -> usize {
    with_body_inference(ws, fn_name, |inf| {
        let n = inf.call_generic_bindings.len();
        inf.call_generic_bindings.clear();
        n
    })
}

/// Same, for the `for`-protocol witness generics `instantiate_witness` reads on
/// the ITER half. Feeds `record_witness`.
fn drop_witness_generic_records(ws: &mut Workspace, fn_name: &str) -> usize {
    with_body_inference(ws, fn_name, |inf| {
        let mut n = 0;
        for info in inf.for_iter_protocol.values_mut() {
            if let Some(w) = info.iter.as_mut() {
                if !w.generics.is_empty() {
                    w.generics.clear();
                    n += 1;
                }
            }
        }
        n
    })
}

/// The NEXT half: `BoundNext::Concrete::next_generics`. Feeds
/// `record_bound_next`, which pushes its own `Unresolved` -- the three
/// recorders do not share a write.
fn drop_bound_next_generic_records(ws: &mut Workspace, fn_name: &str) -> usize {
    with_body_inference(ws, fn_name, |inf| {
        let mut n = 0;
        for info in inf.for_iter_protocol.values_mut() {
            if let bone_semantic::prelude::BoundNext::Concrete { next_generics, .. } =
                &mut info.next
            {
                if !next_generics.is_empty() {
                    next_generics.clear();
                    n += 1;
                }
            }
        }
        n
    })
}

/// The positive fixture for the `Unresolved` axis of the span census.
///
/// The corpus records ZERO unresolved sites, so without this the census's
/// "0 spanless" for that axis is zero over zero and an `Unresolved` recorder
/// switched to `Span::unknown` would go unnoticed.
///
/// The population is a compiler-generated entry the walk cannot resolve to ONE
/// declaration. `stdlib_entry_representative` demands that every declaration of
/// the name canonicalize to one backend-variant group; a second `__map_col_len`
/// with a different signature is a genuine overload, so the group is not one and
/// the entry is refused -- the same residual ambiguity `Builder::stdlib_funcref`
/// refuses. Chosen because it is a permanent property of the lookup rather than
/// a live gap: closing anything cannot silently make this fixture resolve.
///
/// This covers the `compiler-generated-entry-unresolved` write in
/// `walk_body_edges`. The other three recorders each push their OWN
/// `Unresolved` and are covered one fixture apiece:
/// `a_call_with_no_binding_record_is_unresolved_with_a_span`,
/// `a_witness_with_no_generic_record_is_unresolved_with_a_span`,
/// `a_bound_next_with_no_generic_record_is_unresolved_with_a_span`. Mutating
/// any one recorder's span reddens only its own fixture -- measured.
#[test]
fn an_unresolved_call_site_keeps_its_span() {
    let program = |extra: &str| {
        format!(
            r#"
{extra}
value P {{ x: float, y: float }}
fn main() {{
    let m: Map<string, P> = map_new()
    m["a"] = P {{ x: 1.0, y: 2.0 }}
    let n = m.len()
}}
"#
        )
    };
    let ws = check(&program(
        r#"inline(lua) fn __map_col_len(n: int) -> int = "return n""#,
    ));
    let roots = skull::mir_emit::emission_roots(&ws, None);
    let g = instance_graph::build(&ws, roots.clone(), false);
    // The EXACT denominator and reason, not merely non-empty: a fixture that
    // silently drops to a different site still satisfies "non-zero" while
    // covering less than it claims. `m.len()` is the one site naming the
    // ambiguous entry; the store names `__map_col_slot`, which is unaffected.
    let reasons: Vec<&str> = g.unresolved().map(|u| u.reason).collect();
    assert_eq!(
        reasons,
        vec!["compiler-generated-entry-unresolved"; 1],
        "the unresolved population changed shape"
    );
    let spanless: Vec<&str> = g
        .unresolved()
        .filter(|u| instance_graph::is_spanless(&u.span))
        .map(|u| u.reason)
        .collect();
    assert!(
        spanless.is_empty(),
        "unresolved call sites with no source span: {spanless:?}"
    );
    let census = g.span_census();
    assert_eq!(
        census
            .unresolved
            .values()
            .map(|c| c.examined)
            .sum::<usize>(),
        g.unresolved().count(),
        "the census did not examine every unresolved site"
    );

    // CONTROL: the same program WITHOUT the ambiguating declaration resolves
    // every entry, so the population is a property of the ambiguity and not of
    // the fixture being malformed.
    let clean_ws = check(&program(""));
    let clean_roots = skull::mir_emit::emission_roots(&clean_ws, None);
    let clean = instance_graph::build(&clean_ws, clean_roots, false);
    assert!(
        clean.unresolved().next().is_none(),
        "the shared walk should resolve every synthesized entry: {:?}",
        clean.unresolved().map(|u| u.reason).collect::<Vec<_>>()
    );
}

/// **The recursion pass may not search from an instance whose declaration has
/// exactly one instance graph-wide.**
///
/// `detect_changing_instance_recursion` looks for `f<A>` reaching some `f<B>`
/// with `A != B`, and the reachable set only ever contains instances the graph
/// HOLDS -- so a declaration with a single instance cannot satisfy it and its
/// search is fruitless by construction.
///
/// This is structural on purpose. The pass's OUTPUT is empty on every program
/// without polymorphic recursion, so a rejection-output test passes whether the
/// search is bounded or exhaustive; it cannot be the red for a cost change.
/// `recursion_starts` can.
///
/// The population is counted over ALL instances, leaves included: a declaration's
/// second instance may have no outgoing edges of its own and so never appear
/// among the candidate starts, while still being exactly what makes the first
/// one's search worth running.
#[test]
fn the_recursion_search_skips_single_instance_declarations() {
    let (_ws, g) = graph_of(
        r#"
fn passthru<T>(x: T) -> T { return x }
// Two instances, and each has an outgoing edge of its own -- a leaf pair would
// never appear among the candidate starts at all.
fn ident<T>(x: T) -> T { return passthru(x) }

fn only_once(a: int) -> int { return a + 1 }
fn also_once(a: int) -> int { return only_once(a) + 1 }
fn third(a: int) -> int { return also_once(a) + 1 }

fn main() {
    print("{ident(1)} {ident("s")} {third(2)}")
}
"#,
    );

    // Declarations with more than one instance, over EVERY instance the graph
    // holds rather than only those with outgoing edges.
    let mut per_decl: std::collections::BTreeMap<(u32, u32), usize> = Default::default();
    for i in &g.instances {
        *per_decl
            .entry((i.func.module.raw(), i.func.func.into_raw().into_u32()))
            .or_default() += 1;
    }
    let multi: std::collections::BTreeSet<(u32, u32)> = per_decl
        .iter()
        .filter(|(_, n)| **n > 1)
        .map(|(k, _)| *k)
        .collect();

    // The candidate starts are instances with outgoing edges -- what the pass
    // would search from with no filter at all.
    let mut candidates: std::collections::BTreeSet<&instance_graph::Instance> = Default::default();
    for e in g.edges() {
        candidates.insert(&e.from);
    }
    let admissible = candidates
        .iter()
        .filter(|i| multi.contains(&(i.func.module.raw(), i.func.func.into_raw().into_u32())))
        .count();

    // The fixture must actually contain both populations, or this asserts
    // nothing: single-instance declarations to skip, and a multi-instance one
    // (`ident`) to keep.
    assert!(
        admissible < candidates.len(),
        "fixture has no single-instance declarations to skip ({admissible} of {})",
        candidates.len()
    );
    assert!(
        admissible > 0,
        "fixture has no multi-instance declaration to keep"
    );

    assert_eq!(
        g.recursion_starts,
        admissible,
        "the pass searched from {} instances but only {admissible} of {} candidates \
         belong to a declaration with more than one instance",
        g.recursion_starts,
        candidates.len()
    );
}
