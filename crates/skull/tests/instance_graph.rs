//! Stage 0d properties for the shadow instance graph
//!.

use crate::common::{body_id, emit_full_of, fn_name, fn_ref, ws_and_graph, ws_of, ws_with_stdlib};
use bone_semantic::inference::Workspace;
use skull::instance_graph::{self, Instance, Site, SiteOp, SiteOutcome};
use ustr::Ustr;

fn resolved_sites(
    graph: &instance_graph::InstanceGraph,
) -> impl Iterator<Item = (&Site, &Instance)> {
    graph.call_sites().flat_map(|(_, sites)| {
        sites
            .iter()
            .filter_map(|(site, record)| match &record.outcome {
                SiteOutcome::Resolved(target) => Some((site, target)),
                SiteOutcome::Failed(_) => None,
            })
    })
}

#[test]
fn concrete_enum_coroutine_consumers_use_the_frame_channel() {
    let ws = ws_with_stdlib(
        r#"
enum Ev { Quiet, Hit(float, float), Text(string, int) }
struct Task<R> { coroutine: Coroutine<Ev, R> }
co fn source() -> Coroutine<Ev> {
    yield Ev::Quiet
    yield Ev::Hit(3.0, 4.0)
}
fn resume_one<Y, R>(c: Coroutine<Y, R, ()>) -> Y? { return c.resume() }
fn poll<R>(t: Task<R>) -> Ev? { return resume_one(t.coroutine) }
co fn delegate(c: Coroutine<Ev>) -> Coroutine<Ev> { await c }
fn main() {
    let t = Task { coroutine: source() }
    let a = poll(t)
    let b = resume_one(delegate(source()))
    let c = source()
    let d = next(c)
    print("{a} {b} {d}")
}
"#,
    );
    let (emitted, _) = emit_full_of(&ws, "enum coroutine consumers");
    assert!(
        emitted.lua.matches(".__advance").count() >= 4,
        "generic resume, task poll, iterator next, and await must use the channel"
    );
    assert!(
        !emitted.lua.contains(".__resume"),
        "a concrete enum consumer reached the boxed wrapper"
    );
}

/// Instance identity: one generic called at two types is two instances; a
/// generic caller passes its OWN binding down; and two nominals with
/// identical leaves -- the S 20g.2 adversary -- do not collapse.
#[test]
fn each_distinct_instantiation_is_its_own_instance() {
    let cases = [
        (
            "ordinary generic loop",
            r#"
fn ident<T>(x: T) -> T { return x }
fn main() {
    let a = ident(1)
    let b = ident("s")
    let xs = [1, 2, 3]
    for v in xs { ident(v) }
}
"#,
            "ident",
            ["<int>", "<string>"],
        ),
        (
            "through a generic caller",
            r#"
fn inner<T>(x: T) -> T { return x }
fn outer<T>(x: T) -> T { return inner(x) }
fn main() {
    let a = outer(1)
    let b = outer(true)
}
"#,
            "inner",
            ["<int>", "<bool>"],
        ),
        (
            "same-shape nominals",
            r#"
value P { x: float, y: float }
value Q { x: float, y: float }
fn ident<T>(v: T) -> T { return v }
fn main() {
    let p = ident(P { x: 1.0, y: 2.0 })
    let q = ident(Q { x: 1.0, y: 2.0 })
}
"#,
            "ident",
            ["<P>", "<Q>"],
        ),
    ];
    for (label, src, name, args) in cases {
        let (ws, g) = ws_and_graph(src);
        let rejected: Vec<String> = g.rejections().map(|r| r.message.clone()).collect();
        assert!(
            rejected.is_empty(),
            "{label}: valid generic program rejected: {rejected:?}"
        );
        let lines = g.manifest_lines(&ws);
        let found: Vec<&String> = lines.iter().filter(|s| s.contains(name)).collect();
        assert_eq!(found.len(), args.len(), "{label}: {found:?}");
        for arg in args {
            assert!(
                found.iter().any(|s| s.ends_with(arg)),
                "{label}: `{name}` was not instantiated at {arg}: {found:?}"
            );
        }
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
    let ws = ws_with_stdlib(src);
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

/// Every complete generic call identity recovered from typed bindings must
/// appear in the instance graph. A miss whose CALLING body the graph never
/// reached is a reachability difference, not a missing instance.
#[test]
fn every_concrete_call_identity_appears_in_the_instance_graph() {
    use bone_semantic::hir::BodyOwner;
    use skull_mir::lower::SpecSubst;
    use skull_mir::mir::FuncRef;
    use skull_mir::monoty::{MonoTyReason, convert};
    use std::collections::HashSet;

    let (ws, g) = ws_and_graph(
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
    // Both sides encode with `MonoTy::canonical`, so a pretty-printing
    // ambiguity cannot match two different types to each other.
    let instances: HashSet<(FuncRef, Vec<String>)> = g
        .instances
        .iter()
        .map(|i| {
            let args = i.args.iter().map(|a| g.interner().canonical(*a));
            (i.func, args.collect())
        })
        .collect();
    let reached: HashSet<FuncRef> = g.instances.iter().map(|i| i.func).collect();
    let mut calls = 0;
    let mut unresolved = Vec::new();
    let mut missing = Vec::new();
    let mut matched = HashSet::new();
    for module in ws.source_modules() {
        let Some(ir) = ws.inference(module.id) else {
            continue;
        };
        for (owner, inf) in ir.per_fn.iter() {
            let BodyOwner::Function(caller) = owner else {
                continue;
            };
            let caller = FuncRef {
                module: module.id,
                func: *caller,
            };
            for (e, (m, f)) in inf.resolved_overload_calls.iter() {
                let func = FuncRef {
                    module: *m,
                    func: *f,
                };
                let Some(bindings) = inf.call_generic_bindings.get(e) else {
                    continue;
                };
                // An empty enclosing substitution reads a call inside a generic
                // body as the source states it, independently of the walk.
                let key = bindings
                    .iter()
                    .map(|b| convert(&ws, inf, *b, &SpecSubst::default()))
                    .collect::<Result<Vec<_>, _>>();
                let key = match key {
                    Ok(key) => key,
                    Err(MonoTyReason::UnboundGeneric(_)) => continue,
                    Err(_) => {
                        unresolved.push(fn_name(&ws, func));
                        continue;
                    }
                };
                calls += 1;
                let identity = (func, key.iter().map(|t| t.canonical()).collect::<Vec<_>>());
                if instances.contains(&identity) {
                    matched.insert(identity);
                } else if reached.contains(&caller) {
                    missing.push(format!(
                        "{}<{}> (called from `{}`)",
                        fn_name(&ws, func),
                        identity.1.join(", "),
                        fn_name(&ws, caller)
                    ));
                }
            }
        }
    }
    assert!(
        calls > 0,
        "the fixture produced no concrete calls to reconcile"
    );
    assert!(
        missing.is_empty(),
        "concrete call identities absent from the instance graph: {missing:?}"
    );
    assert!(
        unresolved.is_empty(),
        "a valid call identity could not be stated: {unresolved:?}"
    );
    assert!(
        instances.len() > matched.len(),
        "the graph had no identities beyond direct calls"
    );
}

/// Every level of a combinator chain gets a concrete `next` instance. Without
/// reading `next_generics` a witness has no instantiation and its edge is lost
/// entirely -- which is how the first version of this walk reported zero
/// iterator edges over the whole corpus while looking healthy.
#[test]
fn a_chained_iterator_reaches_each_next_instance() {
    let (ws, g) = ws_and_graph(
        r#"
fn main() {
    let xs: [int] = [1, 2, 3]
    let total = 0
    for v in xs.iter().map(_ + 1).filter(_ > 0) { total = total + v }
}
"#,
    );
    let witnesses: Vec<String> = resolved_sites(&g)
        .filter(|(site, _)| matches!(site.op, SiteOp::ForNext(_) | SiteOp::BoundNext(_)))
        .map(|(_, target)| g.symbol(&ws, target))
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
    assert!(
        resolved_sites(&g)
            .filter(|(site, _)| matches!(site.op, SiteOp::ForNext(_)))
            .all(|(_, target)| !target.args.is_empty()),
        "an iterator-next witness reached a generic callee with no instantiation"
    );
}

/// A repeated backend makes the whole same-signature set a genuine overload
/// ambiguity. Bone deliberately leaves every candidate distinct; the root-side
/// inverse must not manufacture a representative that bone never chose.
#[test]
fn canonical_variant_preserves_a_repeated_backend_group() {
    let ws = ws_with_stdlib(
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
    let ws = ws_with_stdlib(src);
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

/// A coroutine instance OWNS helper symbols, recorded with the owning
/// instance's identity so emission can name them per instance. The graph's
/// accounting must match the concrete coroutine plan's declared helpers.
#[test]
fn coroutine_graph_helpers_match_the_concrete_signature_plan() {
    let cases = [
        (
            r#"
co fn ticker(n: int) {
    let i = 0
    while i < n { yield; i = i + 1 }
}
fn main() { let c = ticker(3) }
"#,
            &["__step", "__resume"][..],
        ),
        (
            r#"
nobox value Payload { x: float, y: float }
enum Event { Tick(Payload) }
co fn ticker() -> Coroutine<Event> {
    yield Event::Tick(Payload { x: 1.0, y: 2.0 })
}
fn main() { let c = ticker(); let _ = c.resume() }
"#,
            &["__advance"][..],
        ),
    ];
    for (src, required) in cases {
        let (ws, g) = ws_and_graph(src);
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
            for suffix in required {
                assert!(actual.contains(suffix), "no `{suffix}` helper: {actual:?}");
            }
            compared += 1;
        }
        assert!(compared > 0, "fixture produced no concrete coroutine plan");
    }
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
    let ws = ws_with_stdlib(src);
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
    let (ws, g) = ws_and_graph(
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
    let ws = ws_with_stdlib(
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

/// The other decided rejections: mutual polymorphic recursion, where the type
/// grows around a two-function cycle so no single edge changes a function's
/// own arguments; a GENERIC function taken as a value, which has no call site
/// to key an instantiation on (THE WATCH ITEM, plan S 0h) and must be rejected
/// rather than instantiated at an invented type or dropped; and a type argument
/// no constraint reaches, where every site carrying the unbound variable is
/// rejected -- `len` is called on the `[T]` that `make` returned.
#[test]
fn decided_rejections_name_their_sites() {
    use instance_graph::RejectionKind::*;
    let cases = [
        (
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
            ChangingInstanceRecursion,
            &["ping", "pong"][..],
        ),
        (
            r#"
fn ident<T>(x: T) -> T { return x }
fn apply(f: fn(int) -> int, v: int) -> int { return f(v) }
fn main() {
    apply(ident, 1)
}
"#,
            GenericFnValue,
            &["ident"][..],
        ),
        (
            r#"
fn make<T>() -> [T] { return [] }
fn main() {
    let xs = make()
    let n = xs.len()
}
"#,
            UnconstrainedTypeVar,
            &["make", "len_arr_T"][..],
        ),
    ];
    for (src, kind, names) in cases {
        let (ws, g) = ws_and_graph(src);
        let r = rejections_of(&ws, &g, kind);
        assert_eq!(r.len(), names.len(), "{kind:?}: {r:?}");
        for name in names {
            assert!(r.iter().any(|s| s.contains(name)), "{kind:?}: {r:?}");
        }
    }
}
/// CONTROL: ordinary same-instance recursion, direct and mutual, is finite and
/// must survive. It is the shape the corpus actually contains (11 direct, 9
/// mutual), so a gate that rejected it would reject every real program.
#[test]
fn same_instance_recursion_is_not_rejected() {
    let (ws, g) = ws_and_graph(
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
    let calls = |from: &str, to: &str| {
        g.edges()
            .any(|(source, e)| fn_name(&ws, source.func) == from && fn_name(&ws, e.to.func) == to)
    };
    assert!(
        calls("countdown", "countdown"),
        "the fixture produced no direct recursion to control against"
    );
    assert!(
        calls("even", "odd") && calls("odd", "even"),
        "the fixture produced no mutual recursion to control against"
    );
    // `hold` recurs at two distinct instantiations, neither reachable from the
    // other. Two instances of one function inside a recursive SCC is NOT
    // polymorphic recursion.
    assert_eq!(
        g.instances
            .iter()
            .filter(|i| fn_name(&ws, i.func) == "hold")
            .count(),
        2,
        "the fixture lost the two-instantiation arm of the control"
    );
}

/// CONTROL: a concrete function taken as a value is ordinary and must survive.
#[test]
fn a_concrete_fn_value_is_not_rejected() {
    let (ws, g) = ws_and_graph(
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
    // A function taken as a value is an edge. bone records nothing in
    // `resolved_overload_calls` here -- `apply` is not overloaded -- so this is
    // skull's own `Expr::Global`-names-a-function scan.
    assert!(
        resolved_sites(&g)
            .filter(|(site, _)| site.op == SiteOp::FnValue)
            .any(|(_, target)| fn_name(&ws, target.func) == "bump"),
        "`bump` is taken as a value twice and produced no fn-value edge"
    );
}

/// CONTROL: a type variable constrained anywhere in the body -- not at the call
/// -- still unifies to a concrete type and must NOT be rejected. This is the
/// arm that decides whether the gate reads "unconstrained" or merely "not
/// constrained by the call's own arguments".
#[test]
fn a_body_constrained_type_variable_is_not_rejected() {
    let (ws, g) = ws_and_graph(
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
        .filter(|i| fn_name(&ws, i.func) == "make")
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

/// A synthesized `iter` witness over a GENERIC `iter` (the stdlib's own
/// `iter<T>(Arena<T>)`) is instantiated from bone's retained record, through
/// both routes: the `for` head (`for_iter_protocol`) and an iterable argument
/// coerced at an `Iterator`-bounded param (`implicit_iter_args`), from a
/// function body and from an always-emitted global initializer. The `next`
/// witness's own record is separate and must stay concrete beside it.
#[test]
fn generic_iter_witnesses_are_instantiated() {
    let for_head = "let a: Arena<int> = arena_new()\n    let total = 0\n    for (e, v) in a { total = total + v }";
    let implicit = "let a: Arena<int> = arena_new()\n    let n = a.count()";
    for (label, src, kind, no_record) in [
        (
            "for head",
            format!("fn main() {{\n    {for_head}\n}}\n"),
            SiteOp::ForIter,
            "iter-witness-no-instantiation-record",
        ),
        (
            "implicit iter",
            format!("fn main() {{\n    {implicit}\n}}\n"),
            SiteOp::ImplicitIter,
            "implicit-iter-witness-no-instantiation-record",
        ),
    ] {
        let (ws, g) = ws_and_graph(&src);
        let iters: Vec<String> = resolved_sites(&g)
            .filter(|(site, _)| site.op == kind)
            .map(|(_, target)| g.symbol(&ws, target))
            .collect();
        assert!(
            iters
                .iter()
                .any(|s| s.contains("iter") && s.ends_with("<int>")),
            "{label}: the generic `iter` witness was not instantiated at `int`: {iters:?}"
        );
        assert!(
            !g.unresolved().any(|u| u.reason == no_record),
            "{label}: a witness was still filed as having no instantiation record"
        );
        let nexts: Vec<&Instance> = resolved_sites(&g)
            .filter(|(site, _)| matches!(site.op, SiteOp::ForNext(_) | SiteOp::BoundNext(_)))
            .map(|(_, target)| target)
            .collect();
        assert!(!nexts.is_empty(), "{label}: no next witness edge");
        assert!(
            nexts.iter().all(|target| !target.args.is_empty()),
            "{label}: a `next` witness lost its instantiation"
        );
    }
    for (label, body, names) in [
        (
            "for head",
            format!("{for_head}\n    total"),
            &["iter", "next"][..],
        ),
        (
            "implicit iter",
            implicit.replace("let n = ", ""),
            &["count", "iter", "next"][..],
        ),
    ] {
        let (ws, g) = ws_and_graph(&format!(
            "global TOTAL: int = {{\n    {body}\n}}\nfn main() {{ print(\"{{TOTAL}}\") }}\n"
        ));
        let symbols: Vec<String> = g.instances.iter().map(|i| g.symbol(&ws, i)).collect();
        for name in names {
            assert!(
                symbols
                    .iter()
                    .any(|s| s.contains(name) && s.contains("int")),
                "{label}: global initializer did not seed concrete `{name}` reachability: {symbols:?}"
            );
        }
    }
}
/// CONTROL: a NON-generic `iter` witness needs no record and must keep working
/// through the empty-substitution path.
#[test]
fn a_concrete_iter_witness_still_resolves() {
    let (ws, g) = ws_and_graph(
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
    let iters: Vec<String> = resolved_sites(&g)
        .filter(|(site, _)| site.op == SiteOp::ForIter)
        .map(|(_, target)| g.symbol(&ws, target))
        .collect();
    assert!(
        !iters.is_empty(),
        "a concrete `iter` witness produced no edge"
    );
    assert!(
        resolved_sites(&g)
            .filter(|(site, _)| site.op == SiteOp::ForIter)
            .all(|(_, target)| target.args.is_empty()),
        "a non-generic witness was given instance arguments: {iters:?}"
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
    let ws = ws_with_stdlib(src);
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
    let witnesses: Vec<&Instance> = resolved_sites(&g)
        .filter(|(site, _)| site.op == SiteOp::ForIter)
        .map(|(_, target)| target)
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
    for target in &witnesses {
        let (Some(l), Some(j)) = (lua.realization.get(target), js.realization.get(target)) else {
            panic!("an artifact did not retain the witness at all");
        };
        // Same instance -- it is the same key in both maps -- with the JS
        // sibling chosen only on JS.
        assert_eq!(
            l.decl, target.func,
            "luajit did not select the representative itself"
        );
        selected += usize::from(j.decl != target.func);
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

/// Every reached instance is EITHER expanded or a boundary and never both --
/// the partition the closure gate's arithmetic depends on -- nothing was cut by
/// the depth cap, and no two distinct instances encode to one logical symbol.
///
/// NOT a closure assertion. `is_closed()` also requires that no generic emission
/// root went uninstantiated, and a program built against the whole stdlib always
/// leaves some -- the corpus answers `closed: NO` for that reason. This pins the
/// three properties that do hold; the depth-cap arm gets its positive from
/// `changing_instance_recursion_is_rejected`.
#[test]
fn every_instance_is_expanded_or_a_boundary() {
    let (ws, g) = ws_and_graph(
        r#"
value P { x: float, y: float }
fn ident<T>(x: T) -> T { return x }
@intrinsic fn portable_op(x: int) -> int { return x + 1 }
fn main() {
    let xs: [int] = [1, 2, 3]
    let ys: [string] = ["a"]
    xs.push(ident(4))
    ys.push(ident("b"))
    let total = 0
    for v in xs { total = total + v }
    let ps: [P] = [P { x: 1.0, y: 2.0 }]
    ps.push(P { x: 3.0, y: 4.0 })
    portable_op(1)
    let s = "{ps.len()}"
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
            instance_graph::boundary_class_for(&ws.module(i.func.module).functions[i.func.func])
                .is_some()
        })
        .map(|i| g.identity(&ws, i))
        .collect();
    assert!(
        both.is_empty(),
        "instances counted as BOTH expanded and boundary: {both:?}"
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
    let (ws, g) = ws_and_graph(
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
        .filter(|i| fn_name(&ws, i.func) == "ident")
        .collect();
    assert_eq!(idents.len(), 2);
    let display: Vec<String> = idents.iter().map(|i| g.symbol(&ws, i)).collect();
    assert_eq!(
        display[0], display[1],
        "the fixture is no longer the collision it tests"
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
    let (ws, g) = ws_and_graph(
        r#"
fn bump(x: int) -> int { return x + 1 }
fn main() { bump(1) }
"#,
    );
    let bump = g
        .instances
        .iter()
        .find(|i| fn_name(&ws, i.func) == "bump")
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
/// can arise; this fixture is one level deeper than the generic-caller case of
/// `each_distinct_instantiation_is_its_own_instance` -- the inner call binds a
/// collection over the caller's own generic rather than the generic itself.
#[test]
fn a_callee_binding_inside_a_generic_body_is_concrete() {
    let (ws, g) = ws_and_graph(
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
        .filter(|i| fn_name(&ws, i.func) == "push")
        .map(|i| g.symbol(&ws, i))
        .collect();
    assert!(
        pushes.iter().any(|s| s.contains("int")) && pushes.iter().any(|s| s.contains("string")),
        "`push` was not instantiated at both of `hold`'s instantiations: {pushes:?}"
    );
    assert!(
        g.instances
            .iter()
            .filter(|i| fn_name(&ws, i.func) == "hold")
            .all(|i| !i.args.is_empty()),
        "a `hold` instance carries no arguments"
    );
}

// ---------------------------------------------------------------------------
// Stage 2: `@intrinsic` is not unconditionally a boundary.
// ---------------------------------------------------------------------------

/// HOSTILE: an `@intrinsic` whose body is a portable ROCK fallback. Emit
/// resolution is `backend mapping > portable fallback > reject`, so the shared
/// graph must EXPAND that fallback -- it is the body a target with no mapping
/// runs, and its callees are real edges. `@native_required` SKIPS the fallback
/// rung, and an intrinsic with only a FOREIGN body has nothing to expand: both
/// stay boundaries with no outgoing edges.
///
/// Each helper is GENERIC so that only an instantiation puts it in the graph:
/// a non-generic callee is an emission root in its own right, so its presence
/// would prove nothing about whether the fallback body was walked.
#[test]
fn an_intrinsic_expands_only_a_portable_rock_fallback() {
    let cases = [
        (
            r#"
fn helper_called_only_by_the_fallback<T>(x: T) -> T { return x }
@intrinsic fn portable_op(x: int) -> int {
    return helper_called_only_by_the_fallback(x)
}
fn main() { portable_op(1) }
"#,
            "portable_op",
            true,
            Some("helper_called_only_by_the_fallback"),
        ),
        (
            r#"
fn unreachable_through_a_native_required_body<T>(x: T) -> T { return x }
@intrinsic @native_required fn pinned_op(x: int) -> int {
    return unreachable_through_a_native_required_body(x)
}
fn main() { pinned_op(1) }
"#,
            "pinned_op",
            false,
            Some("unreachable_through_a_native_required_body"),
        ),
        (
            r#"
@intrinsic inline(lua) fn mapped_op(x: int) -> int = "return x + 1"
fn main() { mapped_op(1) }
"#,
            "mapped_op",
            false,
            None,
        ),
    ];
    for (src, op, expands, helper) in cases {
        let (ws, g) = ws_and_graph(src);
        assert_eq!(
            g.expanded().any(|i| fn_name(&ws, i.func) == op),
            expands,
            "`{op}` expansion"
        );
        assert_eq!(
            g.boundaries.keys().any(|k| k.contains(op)),
            !expands,
            "`{op}` boundary classification: {:?}",
            g.boundaries.keys().collect::<Vec<_>>()
        );
        if let Some(helper) = helper {
            assert_eq!(
                g.instances.iter().any(|i| fn_name(&ws, i.func) == helper),
                expands,
                "`{helper}` is reachable exactly when `{op}`'s body is walked"
            );
        }
        if !expands {
            let out: Vec<String> = g
                .edges()
                .filter(|(from, _)| fn_name(&ws, from.func) == op)
                .map(|(_, e)| g.symbol(&ws, &e.to))
                .collect();
            assert!(out.is_empty(), "boundary `{op}` produced edges: {out:?}");
        }
    }
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
    let (ws, g) = ws_and_graph(
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
    let (_, bare) = ws_and_graph(&src("@nomangle"));
    assert!(
        bare.uninstantiated_roots.contains_key("keep"),
        "a `@nomangle` generic root IS a bare-name obligation: {:?}",
        bare.uninstantiated_roots.keys()
    );

    let (_, mangled) = ws_and_graph(&src(""));
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
    let plain = crate::common::graph_of(&ws_of(entry));
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

    let shadowed = crate::common::graph_of(&ws_of(&format!(
        "{entry}@nomangle fn __rock_col_sort<T>(x: T, y: T) -> int {{ return 2 }}\n"
    )));
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
    let (_ws2, g2) = ws_and_graph(
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
/// boundaries." Every edge must carry its own span so any failed boundary can
/// produce a located diagnostic.
///
/// The fixture must produce edges so the span check is not vacuous.
#[test]
fn every_edge_carries_a_source_span() {
    let (ws, g) = ws_and_graph(
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
    print("{1.5}")
}
"#,
    );
    assert!(g.edges().next().is_some(), "fixture produced no edges");
    let spanless: Vec<String> = g
        .edges()
        .filter(|(_, edge)| instance_graph::is_spanless(&edge.span))
        .map(|(from, edge)| format!("{} -> {}", g.symbol(&ws, from), g.symbol(&ws, &edge.to)))
        .collect();
    assert!(
        spanless.is_empty(),
        "{} of {} edges carry no source span: {spanless:#?}",
        g.edges()
            .filter(|(_, e)| instance_graph::is_spanless(&e.span))
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
    let (ws, g) = ws_and_graph(
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
    // CONTROL: the scan does not simply report the whole catalogue.
    assert!(
        !deps.iter().any(|d| d.contains("__map_col_slot_checked")),
        "the dependency scan reported an entry the body never names: {deps:?}"
    );
}

/// The other rejection an always-emitted body can hold: a generic function
/// taken as a VALUE by a global initializer. `GenericFnValue` is a decided S 0h
/// rejection, and it stays one when there is no `from` instance to file it
/// against.
#[test]
fn a_generic_fn_value_in_an_always_emitted_body_keeps_its_rejection_kind() {
    let ws = ws_with_stdlib(
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

/// Each recorder's own `Unresolved` write: `record_call`, `record_witness` and
/// `record_bound_next` push independently, so covering one says nothing about
/// the others. `record_call` answers `no-bindings` when a generic call has no
/// `call_generic_bindings` record, which the plan measured 126 times on valid
/// corpus code before the graph fix -- so each fixture removes a RECORD from a
/// successfully inferred workspace rather than corrupting the source.
/// `BodyInference` is public result data, so this reaches the recorder without
/// inventing an invalid program, and it is what makes a `Span::unknown`
/// mutation in a recorder visible.
///
/// The two witness halves both fail through `instantiate_witness` with
/// `witness-generic-unbound`, so each case requires every site of its reason to
/// name its own callee, and keeps the OTHER half's edge as a control.
#[test]
fn a_call_or_witness_with_no_generic_record_is_unresolved_with_a_span() {
    let for_loop = r#"
fn main() {
    let xs: [int] = [1, 2, 3]
    let t = 0
    for x in xs { t = t + x }
}
"#;
    let cases: [(
        &str,
        fn(&mut Workspace, &str) -> usize,
        &str,
        &str,
        Option<SiteOp>,
    ); 3] = [
        (
            "fn ident<T>(x: T) -> T { return x }\nfn main() { let a = ident(1) }\n",
            drop_generic_binding_records,
            "no-bindings",
            "ident",
            None,
        ),
        (
            for_loop,
            drop_witness_generic_records,
            "witness-generic-unbound",
            "iter",
            Some(SiteOp::ForNext(Box::new([]))),
        ),
        (
            for_loop,
            drop_bound_next_generic_records,
            "witness-generic-unbound",
            "next",
            Some(SiteOp::ForIter),
        ),
    ];
    for (src, drop, reason, callee, control) in cases {
        let mut ws = ws_with_stdlib(src);
        // EXACT: each fixture has one such record in `main`, and a helper that
        // removed a same-index stdlib record too would report 2.
        assert_eq!(drop(&mut ws, "main"), 1, "{callee}: records dropped");
        let g = crate::common::graph_of(&ws);
        let sites: Vec<&instance_graph::Unresolved> =
            g.unresolved().filter(|u| u.reason == reason).collect();
        let callees: Vec<&str> = sites.iter().map(|u| fn_name(&ws, u.callee)).collect();
        assert_eq!(
            callees,
            vec![callee],
            "expected exactly the `{callee}` site to lose its instantiation"
        );
        assert!(
            sites.iter().all(|u| !instance_graph::is_spanless(&u.span)),
            "an unresolved `{callee}` site carries no span: {sites:?}"
        );
        if let Some(kind) = control {
            assert!(
                resolved_sites(&g).any(|(site, _)| site.op == kind),
                "{callee}: the fixture disturbed the other witness half too"
            );
        }
    }
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
    let (ws, g) = ws_and_graph(
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
    let missing: Vec<String> = g
        .instances
        .iter()
        .filter(|i| g.substitution(i).is_none())
        .map(|i| g.identity(&ws, i))
        .collect();
    assert!(
        missing.is_empty(),
        "instances with no recorded substitution: {missing:?}"
    );
    assert!(
        g.substitution(&instance_graph::Instance {
            func: fn_ref(&ws, "main"),
            args: Vec::new(),
        })
        .is_some(),
        "`main`, a non-generic root, has no recorded substitution"
    );
}

/// Run `f` over exactly one body's inference record.
fn with_body_inference<R>(
    ws: &mut Workspace,
    fn_name: &str,
    f: impl FnOnce(&mut bone_semantic::inference::BodyInference) -> R,
) -> R {
    let (mid, owner) = body_id(ws, fn_name);
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
