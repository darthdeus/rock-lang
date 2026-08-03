//! The first TARGET ARTIFACT VIEW (`docs/monomorphization-plan.md` S 9, S 11).
//!
//! An artifact view is a target's REALIZATION of the one shared
//! [`InstanceGraph`]: which declarations it retains, how each boundary is
//! actually supplied, and what it spells each retained instance. It may drop
//! and it may reject, but it may never DISCOVER -- plan S 20g.1 asks whether
//! logical identity survives realization differing, so a view that invented an
//! instance would be answering its own question.
//!
//! The four premises below are what made a count-only projection insufficient.
//! Each is asserted against the SHARED graph first, so the view's job is a
//! measured defect rather than a design preference.

use std::sync::LazyLock;

use bone_semantic::hir::FnBackend;
use bone_semantic::inference::Workspace;
use skull::artifact::{self, ArtifactMappings, ArtifactRequest, Exclusion, Unrealizable};
use skull::concrete_body::Callee;
use skull::instance_graph::{self, Instance, InstanceGraph};
use skull::target_projection::{JAVASCRIPT, LUAJIT, NATIVE_C64};
use skull_mir::intrinsics::{Emission, IntrinsicEntry, IntrinsicTable};
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

/// The graph AND the roots it was built from. An artifact's roots are an input
/// to `realize` (plan S 11: "the target may change roots"), so a helper that
/// dropped them would make every view root-less.
fn graph_for(ws: &Workspace, target: FnBackend) -> (InstanceGraph, Vec<skull_mir::mir::FuncRef>) {
    let roots = instance_graph::emission_roots(ws, None);
    (instance_graph::build_for_target(ws, roots.clone(), false, target), roots)
}

/// The artifact's own graph, built for ITS backend -- not a Lua graph filtered
/// afterwards. `build_for_target` selects backend variants per target, so
/// realizing a Lua graph for a JS artifact would answer JS's question with
/// Lua's edges.
fn view_of<'a>(
    ws: &'a Workspace,
    spec: skull::target_projection::ExecutionTargetSpec,
) -> (InstanceGraph, Vec<skull_mir::mir::FuncRef>) {
    graph_for(ws, spec.backend.unwrap_or(FnBackend::Any))
}

/// A planner that spells everything with a target prefix. It exists so
/// "both artifacts spell this instance alike" cannot be mistaken for the
/// identity property: plan S 0h puts the spelling under the ARTIFACT.
struct Prefixed(&'static str);
impl artifact::SymbolPlanner for Prefixed {
    fn symbol(&self, ws: &Workspace, g: &InstanceGraph, i: &Instance) -> String {
        format!("{}${}", self.0, g.logical_symbol(ws, i))
    }
    fn describe(&self) -> &'static str {
        "prefixed"
    }
}

/// A backend table that maps exactly one name -- the stand-in for a real
/// backend's `IntrinsicTable`.
struct OneName(&'static str);
impl IntrinsicTable for OneName {
    fn backend(&self) -> &'static str {
        "toy"
    }
    fn lookup(&self, f: &bone_semantic::hir::Function) -> Option<Emission> {
        (f.name.as_str() == self.0).then_some(Emission::Template("$1"))
    }
    fn entries(&self) -> Vec<IntrinsicEntry> {
        vec![IntrinsicEntry { name: self.0, arity: 1 }]
    }
}

// ---------------------------------------------------------------------------
// PREMISE 1 -- the shared graph retains BOTH `@nomangle` backend siblings.
// ---------------------------------------------------------------------------

/// `__nb_checked_index` is declared twice: `inline(lua)` in `unified.rock` and
/// `inline(js)` in `js.rock`. Both are `@nomangle`, so both are emission roots,
/// so BOTH are instances of the Lua graph -- a JS-only body is reachable in a
/// Lua walk, and the two share one mangled symbol.
///
/// This is the premise the artifact view exists to fix, asserted here so the
/// fix has a measured defect to point at rather than a design preference.
#[test]
fn premise_the_shared_graph_keeps_both_backend_siblings() {
    let ws = check("fn main() { let b: NativeBuffer<int> = nb_new(4); let x = b[0] }");
    let (g, _) = graph_for(&ws, FnBackend::Lua);
    let backends: Vec<FnBackend> = g
        .instances
        .iter()
        .filter(|i| {
            ws.module(i.func.module).functions[i.func.func].name.as_str()
                == "__nb_checked_index"
        })
        .map(|i| ws.module(i.func.module).functions[i.func.func].backend)
        .collect();
    assert!(
        backends.contains(&FnBackend::Lua) && backends.contains(&FnBackend::Js),
        "the fixture no longer has both siblings in the Lua graph: {backends:?}"
    );
}

/// The luajit artifact retains the Lua declaration and DROPS the JS one; the
/// javascript artifact does the reverse. Neither is decided through
/// `InstanceGraph::boundaries`, which is keyed by mangled symbol and therefore
/// cannot tell the two apart at all.
#[test]
fn an_artifact_retains_only_the_sibling_that_serves_it() {
    let ws = check("fn main() { let b: NativeBuffer<int> = nb_new(4); let x = b[0] }");
    let sibling_backends = |view: &artifact::ArtifactView| -> Vec<FnBackend> {
        view.instances
            .iter()
            .filter(|i| {
                ws.module(i.func.module).functions[i.func.func].name.as_str()
                    == "__nb_checked_index"
            })
            .map(|i| ws.module(i.func.module).functions[i.func.func].backend)
            .collect()
    };

    // Each artifact realizes ITS OWN graph.
    let (lg, lr) = view_of(&ws, LUAJIT);
    let (jg, jr) = view_of(&ws, JAVASCRIPT);
    let lua = artifact::realize(&ws, &lg, &ArtifactRequest::new(LUAJIT, &lr));
    let js = artifact::realize(&ws, &jg, &ArtifactRequest::new(JAVASCRIPT, &jr));
    assert!(!lua.census().graph_mismatch && !js.census().graph_mismatch);
    assert_eq!(
        sibling_backends(&lua),
        vec![FnBackend::Lua],
        "the luajit artifact did not drop the `inline(js)` sibling"
    );
    assert_eq!(
        sibling_backends(&js),
        vec![FnBackend::Js],
        "the javascript artifact did not drop the `inline(lua)` sibling"
    );
    // And the drop is RECORDED, not silent: a declaration that leaves the
    // artifact is a named exclusion with a cause.
    assert!(
        lua.excluded.iter().any(|e| {
            ws.module(e.instance.func.module).functions[e.instance.func.func].name.as_str()
                == "__nb_checked_index"
                && e.cause == Exclusion::Unrealizable(Unrealizable::ForeignToArtifact)
        }),
        "the dropped sibling has no recorded cause: {:?}",
        lua.census().excluded_by_cause
    );
}

// ---------------------------------------------------------------------------
// PREMISE 2 -- `@native_required` availability from an actual native mapping.
// ---------------------------------------------------------------------------

const NATIVE_REQUIRED: &str = r#"
@intrinsic @native_required
fn fast_op(x: int) -> int { return x * 2 }
fn main() { let a = fast_op(1) }
"#;

/// A `@native_required` intrinsic whose only body is a portable Rock fallback.
/// `serves()` says the declaration is compatible with every target, and it HAS
/// a Rock body, so both of the cheap tests answer "available" -- which is the
/// exact failure `@native_required` exists to prevent.
#[test]
fn premise_a_compatible_backend_and_a_rock_fallback_both_say_available() {
    let ws = check(NATIVE_REQUIRED);
    let (g, _) = graph_for(&ws, FnBackend::Lua);
    let f = g
        .instances
        .iter()
        .map(|i| &ws.module(i.func.module).functions[i.func.func])
        .find(|f| f.name.as_str() == "fast_op")
        .expect("`fast_op` is not in the graph");
    assert!(!g.boundary_invalid_on_target(f), "the FnBackend test already rejects it");
    assert!(f.inline_body.is_none(), "the fixture has a native mapping after all");
    assert!(
        skull_mir::intrinsics::is_native_required(f),
        "the fixture is not `@native_required`"
    );
}

/// The artifact decides from its OWN mapping. luajit's mapping is the body slot
/// (an `inline(lua)` body), so a Rock-only `@native_required` intrinsic is
/// unavailable there -- and a table that maps it makes it available, which is
/// what says the arm reads the mapping rather than rejecting everything.
#[test]
fn native_required_availability_comes_from_the_artifact_mapping() {
    let ws = check(NATIVE_REQUIRED);
    let (g, roots) = graph_for(&ws, FnBackend::Lua);
    let named = |view: &artifact::ArtifactView| -> Option<Exclusion> {
        view.excluded
            .iter()
            .find(|e| {
                ws.module(e.instance.func.module).functions[e.instance.func.func].name.as_str()
                    == "fast_op"
            })
            .map(|e| e.cause)
    };

    let bare = artifact::realize(&ws, &g, &ArtifactRequest::new(LUAJIT, &roots));
    assert_eq!(
        named(&bare),
        Some(Exclusion::Unrealizable(Unrealizable::NativeMappingMissing)),
        "a `@native_required` intrinsic with only a Rock fallback was realized"
    );

    // CONTROL: the same graph, the same target, one table entry different. If
    // the arm rejected every intrinsic this would still be excluded.
    let table = OneName("fast_op");
    let mapped = artifact::realize(
        &ws,
        &g,
        &ArtifactRequest::new(LUAJIT, &roots).with_mappings(
            ArtifactMappings::for_backend(FnBackend::Lua).with_table(&table),
        ),
    );
    assert_eq!(
        named(&mapped),
        None,
        "a mapped `@native_required` intrinsic was still excluded"
    );
}

/// An ORDINARY intrinsic is the nearest working control: it takes the portable
/// fallback rung, so it stays realized with no mapping at all. Without this the
/// test above could pass because every intrinsic is excluded.
#[test]
fn an_ordinary_intrinsic_falls_back_to_its_rock_body() {
    let ws = check(
        r#"
@intrinsic
fn portable_op(x: int) -> int { return x * 2 }
fn main() { let a = portable_op(1) }
"#,
    );
    let (g, roots) = graph_for(&ws, FnBackend::Lua);
    let view = artifact::realize(&ws, &g, &ArtifactRequest::new(LUAJIT, &roots));
    assert!(
        view.instances.iter().any(|i| {
            ws.module(i.func.module).functions[i.func.func].name.as_str() == "portable_op"
        }),
        "a portable intrinsic was dropped: {:?}",
        view.census().excluded_by_cause
    );
    assert!(
        view.excluded.iter().all(|e| {
            ws.module(e.instance.func.module).functions[e.instance.func.func].name.as_str()
                != "portable_op"
        }),
        "the portable rung did not fire"
    );
}

// ---------------------------------------------------------------------------
// PREMISE 3 -- opaque `inline(lua)` dependencies are name metadata.
// ---------------------------------------------------------------------------

/// The graph records that an opaque body NAMES a runtime entry. It does not
/// resolve which declaration that is, so nothing downstream can retain the
/// right one -- and on an artifact where the entry has a different serving
/// declaration, the name alone is ambiguous.
#[test]
fn premise_inline_body_dependencies_are_unresolved_names() {
    let ws = check("fn main() { let v: [Vec2] = []; let l = v.to_lua_array() }");
    let (g, _) = graph_for(&ws, FnBackend::Lua);
    let deps: Vec<&str> = g
        .inline_body_deps
        .values()
        .flat_map(|s| s.iter())
        .copied()
        .collect();
    assert!(!deps.is_empty(), "the fixture records no inline-body dependency at all");
    // A NAME, and nothing that names a declaration: no `FuncRef`, no instance.
    assert!(
        deps.iter().all(|d| d.starts_with("__")),
        "the dependencies are no longer bare runtime-entry names: {deps:?}"
    );
}

/// The artifact resolves each dependency to a declaration IT retains, and says
/// so per dependency rather than as a total.
#[test]
fn inline_body_dependencies_resolve_to_targets_in_the_artifact() {
    let ws = check("fn main() { let v: [Vec2] = []; let l = v.to_lua_array() }");
    let (g, roots) = graph_for(&ws, FnBackend::Lua);
    let view = artifact::realize(&ws, &g, &ArtifactRequest::new(LUAJIT, &roots));
    let c = view.census();
    assert!(c.inline_deps_examined > 0, "no dependency was examined: {c:?}");
    assert_eq!(
        c.inline_deps_unresolved, 0,
        "unresolved inline-body dependencies: {:?}",
        view.render_unresolved_inline_deps(&ws)
    );
    // Each dependency terminates in a CALLEE, not a bare declaration: a
    // runtime entry is a `Callee::Boundary` carrying its class and the symbol
    // emission must produce, and anything else names a concrete instance.
    assert!(c.inline_deps_boundary > 0, "no dependency resolved to a boundary: {c:?}");
    for dep in &view.inline_deps {
        match &dep.callee {
            Callee::Boundary(b) => {
                assert_eq!(b.target, FnBackend::Lua, "the boundary names another target");
                assert!(
                    view.instances.iter().any(|i| i.func == b.declaration),
                    "a dependency resolved to a declaration outside the artifact"
                );
                assert!(
                    view.symbols.values().any(|s| *s == b.symbol),
                    "the boundary's symbol is not one the artifact assigned"
                );
            }
            Callee::Rock(i) => assert!(
                view.instances.contains(i),
                "a dependency resolved to an instance outside the artifact"
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// PREMISE 4 -- one deterministic, injective symbol per (artifact, instance).
// ---------------------------------------------------------------------------

/// Before retention, the two `@nomangle` siblings are distinct instances that
/// spell to ONE symbol. `logical_symbol` is the encoder, and the encoder is
/// injective over its own input; what it cannot do is answer for an artifact
/// holding both, which is what the shared graph holds.
#[test]
fn premise_two_retained_siblings_would_share_one_symbol() {
    let ws = check("fn main() { let b: NativeBuffer<int> = nb_new(4); let x = b[0] }");
    let (g, _) = graph_for(&ws, FnBackend::Lua);
    let siblings: Vec<&Instance> = g
        .instances
        .iter()
        .filter(|i| {
            ws.module(i.func.module).functions[i.func.func].name.as_str()
                == "__nb_checked_index"
        })
        .collect();
    assert_eq!(siblings.len(), 2, "the fixture lost a sibling");
    let symbols: std::collections::BTreeSet<String> =
        siblings.iter().map(|i| g.logical_symbol(&ws, i)).collect();
    assert_eq!(
        symbols.len(),
        1,
        "the two siblings no longer share one symbol, so the collision premise is gone"
    );
}

/// Every retained instance gets exactly one symbol, and no two share one. The
/// collision above is gone BECAUSE retention dropped the other sibling, which
/// is why the two decisions are one slice.
#[test]
fn an_artifact_symbol_is_total_and_injective_over_what_it_retains() {
    let ws = check(
        r#"
fn ident<T>(x: T) -> T { return x }
fn main() {
    let b: NativeBuffer<int> = nb_new(4)
    let x = b[0]
    let a = ident(1)
    let s = ident("s")
}
"#,
    );
    let (g, roots) = graph_for(&ws, FnBackend::Lua);
    let view = artifact::realize(&ws, &g, &ArtifactRequest::new(LUAJIT, &roots));
    let c = view.census();
    assert!(c.instances > 0, "the artifact retained nothing");
    assert_eq!(
        view.symbols.len(),
        view.instances.len(),
        "the symbol table is not total over the retained instances"
    );
    assert_eq!(
        c.symbol_collisions, 0,
        "two retained instances share one symbol: {:?}",
        view.render_symbol_collisions(&ws)
    );
    // Non-vacuous: the generic instances must actually be spelled apart, so
    // injectivity is not satisfied by there being nothing to collide.
    let idents: Vec<&String> = view
        .symbols
        .iter()
        .filter(|(i, _)| {
            ws.module(i.func.module).functions[i.func.func].name.as_str() == "ident"
        })
        .map(|(_, s)| s)
        .collect();
    assert_eq!(idents.len(), 2, "the fixture has no two-instance generic: {idents:?}");
    assert_ne!(idents[0], idents[1], "two instantiations spell alike");

    // Deterministic: realizing the same graph twice gives the same table.
    let again = artifact::realize(&ws, &g, &ArtifactRequest::new(LUAJIT, &roots));
    assert_eq!(view.symbols, again.symbols, "the symbol assignment is not deterministic");
}

// ---------------------------------------------------------------------------
// The two cross-cutting properties.
// ---------------------------------------------------------------------------

/// A view may DROP and it may REJECT; it may never DISCOVER. Asserted as a
/// subset relation over the actual instances, not over counts -- two sets of
/// equal size can still disagree.
#[test]
fn realization_never_invents_an_instance() {
    let ws = check(REACH);
    for spec in [LUAJIT, JAVASCRIPT, NATIVE_C64] {
        let (g, roots) = view_of(&ws, spec);
        let view = artifact::realize(&ws, &g, &ArtifactRequest::new(spec, &roots));
        assert!(!view.census().graph_mismatch, "`{}` got a foreign graph", spec.name);
        for i in &view.instances {
            assert!(
                g.instances.contains(i),
                "`{}` realized an instance the shared graph does not have: {}",
                spec.name,
                g.identity(&ws, i)
            );
        }
        // The accounting is a PARTITION: every shared instance is retained,
        // unrealizable, or unreachable, never quietly absent from all three.
        assert_eq!(
            view.instances.len() + view.excluded.len(),
            g.instances.len(),
            "`{}` lost instances between the graph and the view",
            spec.name
        );
    }
}

/// A fixture with a declaration nothing calls, so `unreachable` has a
/// population distinct from `unrealizable`.
const REACH: &str = r#"
fn ident<T>(x: T) -> T { return x }
@nomangle fn never_called_from_main(x: int) -> int { return x + 1 }
fn main() {
    let b: NativeBuffer<int> = nb_new(4)
    let x = b[0]
    let a = ident(1)
}
"#;

/// Retention is a WALK, not a declaration filter. A declaration this artifact
/// could compile but no root of ITS reaches is `Unreachable` -- a filter would
/// keep it, and the two causes must not be one bucket.
///
/// The graph is built from the FULL root set and the artifact is given a
/// narrower one, which is plan S 11's "the target may change roots" stated as a
/// fixture: the instance exists in the shared graph and is simply not this
/// artifact's program.
#[test]
fn retention_is_reachability_not_a_declaration_filter() {
    let ws = check(REACH);
    let (g, all_roots) = view_of(&ws, LUAJIT);
    let main_only: Vec<skull_mir::mir::FuncRef> = all_roots
        .iter()
        .copied()
        .filter(|r| ws.module(r.module).functions[r.func].name.as_str() == "main")
        .collect();
    assert_eq!(main_only.len(), 1, "the fixture has no unique `main` root");

    let view = artifact::realize(&ws, &g, &ArtifactRequest::new(LUAJIT, &main_only));
    let c = view.census();
    // EXACTLY the roots it was given, plus the always-emitted callees, which
    // carry no incoming edge and would otherwise read as unreachable. Stated as
    // a set equality, so a view that quietly seeded from the whole graph could
    // not pass by having "enough" roots.
    let expected: std::collections::BTreeSet<&Instance> = g
        .instances
        .iter()
        .filter(|i| {
            (i.func == main_only[0] && i.args.is_empty())
                || g.always_emitted_targets.contains(*i)
        })
        .collect();
    assert_eq!(
        view.roots.iter().collect::<std::collections::BTreeSet<_>>(),
        expected,
        "the artifact's roots are not the ones it was given: {c:?}"
    );
    assert!(c.sites_walked > 0, "the walk traversed no call site: {c:?}");
    // Both causes have a population, so neither reads as the other.
    assert!(
        c.excluded_by_cause.get("unreachable").copied().unwrap_or(0) > 0,
        "nothing was excluded as unreachable, so retention is still a filter: {:?}",
        c.excluded_by_cause
    );
    assert!(
        c.excluded_by_cause.get("foreign-to-artifact").copied().unwrap_or(0) > 0,
        "nothing was excluded as unrealizable: {:?}",
        c.excluded_by_cause
    );
    // Named, not merely counted: the fixture's uncalled declaration is
    // REALIZABLE here, which is what makes its exclusion a reachability fact
    // and not a realization one.
    let uncalled = view.excluded.iter().find(|e| {
        ws.module(e.instance.func.module).functions[e.instance.func.func].name.as_str()
            == "never_called_from_main"
    });
    assert_eq!(
        uncalled.map(|e| e.cause),
        Some(Exclusion::Unreachable),
        "an uncalled but compilable declaration was not excluded as unreachable"
    );
    // CONTROL: the same declaration IS retained when it is a root, so the
    // exclusion above is a property of reachability and not of the declaration.
    let wide = artifact::realize(&ws, &g, &ArtifactRequest::new(LUAJIT, &all_roots));
    assert!(
        wide.instances.iter().any(|i| {
            ws.module(i.func.module).functions[i.func.func].name.as_str()
                == "never_called_from_main"
        }),
        "the uncalled declaration is unreachable even as a root, so the fixture \
         cannot distinguish"
    );
    // ...and the walk genuinely reaches transitively, not just its roots.
    assert!(
        view.instances.len() > view.roots.len(),
        "the walk retained nothing beyond its roots: {c:?}"
    );
    assert!(
        view.instances.iter().any(|i| {
            ws.module(i.func.module).functions[i.func.func].name.as_str() == "ident"
        }),
        "the walk did not reach a called generic"
    );
}

/// A retained caller whose concrete callee this artifact cannot realize is a
/// SEVERED call -- the finding a declaration filter structurally cannot
/// produce, because it never looks at an edge.
#[test]
fn a_retained_caller_naming_an_unrealizable_callee_is_severed() {
    let ws = check(
        r#"
@intrinsic @native_required
fn fast_op(x: int) -> int { return x * 2 }
fn caller(x: int) -> int { return fast_op(x) }
fn main() { let a = caller(1) }
"#,
    );
    let (g, roots) = graph_for(&ws, FnBackend::Lua);
    let view = artifact::realize(&ws, &g, &ArtifactRequest::new(LUAJIT, &roots));
    let c = view.census();
    assert!(c.sites_walked > 0, "no site was walked, so 0 severed says nothing: {c:?}");
    let named: Vec<String> = view.render_severed(&ws, &g);
    assert_eq!(
        c.severed, 1,
        "expected exactly the `fast_op` call to be severed, got {named:?}"
    );
    assert!(named[0].contains("fast_op"), "the severed call is not the one expected: {named:?}");
    assert_eq!(
        c.severed_by_cause.get("native-mapping-missing").copied(),
        Some(1),
        "the severed call lost its cause: {:?}",
        c.severed_by_cause
    );
    // The CALLER survives -- which is precisely why the severed call matters:
    // emission would emit a call to a body that is not in the artifact.
    assert!(
        view.instances.iter().any(|i| {
            ws.module(i.func.module).functions[i.func.func].name.as_str() == "caller"
        }),
        "the caller was dropped, so nothing would have emitted the call anyway"
    );
    // CONTROL: mapping the intrinsic removes the severed call without touching
    // anything else.
    let table = OneName("fast_op");
    let mapped = artifact::realize(
        &ws,
        &g,
        &ArtifactRequest::new(LUAJIT, &roots).with_mappings(
            ArtifactMappings::for_backend(FnBackend::Lua).with_table(&table),
        ),
    );
    assert_eq!(mapped.census().severed, 0, "mapping the callee left the call severed");
}

/// Realizing a graph built for ANOTHER target is a recorded defect, not a
/// tolerated approximation: `build_for_target` resolves each callee through
/// that target's `backend_variant`, so a Lua graph answers JS's question with
/// Lua's edges.
///
/// The consequence is measured, not asserted from the signature. The two graphs
/// hold the SAME instances here -- both `@nomangle` siblings are emission roots
/// -- and differ in exactly one thing: which sibling a SITE resolves to. On the
/// wrong graph that site names a Lua body, which is unrealizable on JS, so the
/// call comes out SEVERED. An instance-set comparison would have found nothing.
#[test]
fn a_graph_built_for_another_target_is_a_recorded_mismatch() {
    let ws = check(REACH);
    let (lua_graph, roots) = graph_for(&ws, FnBackend::Lua);
    let (js_graph, _) = graph_for(&ws, FnBackend::Js);
    assert_eq!(
        lua_graph.instances, js_graph.instances,
        "the fixture's two graphs differ in instances, so the severed count \
         below could be explained by that instead"
    );

    let wrong = artifact::realize(&ws, &lua_graph, &ArtifactRequest::new(JAVASCRIPT, &roots));
    assert!(
        wrong.census().graph_mismatch,
        "a JS artifact realized a Lua graph without complaint"
    );
    assert_eq!(wrong.graph_target, FnBackend::Lua);

    let right = artifact::realize(&ws, &js_graph, &ArtifactRequest::new(JAVASCRIPT, &roots));
    assert!(!right.census().graph_mismatch, "the JS artifact rejected its own graph");
    assert_eq!(right.graph_target, FnBackend::Js);

    // The mismatch is not cosmetic: the wrong graph's variant selection severs
    // a call the right one resolves.
    assert!(
        wrong.census().severed > right.census().severed,
        "realizing the wrong graph severed no more calls than the right one \
         ({} vs {}), so the target's variant selection is not reaching the view",
        wrong.census().severed,
        right.census().severed
    );
    assert!(
        !wrong.render_severed(&ws, &lua_graph).is_empty(),
        "the severed call has no rendering"
    );
}

/// Identity across artifacts is the shared `Instance`, NOT the spelling.
///
/// Plan S 0h: `InstanceId` is target-neutral and the ARTIFACT owns the emitted
/// spelling. The default planner happens to be target-neutral; a hostile one
/// that renames deliberately must leave identity intact and each artifact's own
/// table total and injective. Asserting spelling equality across artifacts
/// would forbid exactly what the plan permits.
#[test]
fn cross_artifact_identity_is_the_instance_and_not_the_spelling() {
    let ws = check(REACH);
    let (lg, lr) = view_of(&ws, LUAJIT);
    let (jg, jr) = view_of(&ws, JAVASCRIPT);
    let lua_p = Prefixed("lua");
    let js_p = Prefixed("js");
    let lua = artifact::realize(
        &ws,
        &lg,
        &ArtifactRequest::new(LUAJIT, &lr).with_symbols(&lua_p),
    );
    let js = artifact::realize(
        &ws,
        &jg,
        &ArtifactRequest::new(JAVASCRIPT, &jr).with_symbols(&js_p),
    );

    // The shared population, keyed on the CANONICAL identity string -- never on
    // `Instance` itself. An `Instance` holds `MonoTyId`s, which index the
    // graph's OWN arena; two separately built graphs intern in whatever order
    // their walks reached types, so comparing raw ids across them compares
    // arena positions and not types.
    let ids_of = |v: &artifact::ArtifactView, ag: &InstanceGraph| {
        v.instances
            .iter()
            .map(|i| (ag.identity(&ws, i), v.symbols[i].clone()))
            .collect::<std::collections::BTreeMap<String, String>>()
    };
    let lua_ids = ids_of(&lua, &lg);
    let js_ids = ids_of(&js, &jg);
    let shared: Vec<&String> = lua_ids.keys().filter(|k| js_ids.contains_key(*k)).collect();
    assert!(!shared.is_empty(), "the two artifacts share no logical instance at all");

    // The hostile planners spell every shared instance DIFFERENTLY, and that is
    // legal. If this test asserted equality it would be asserting the opposite
    // of the signed decision (plan S 0h: the ARTIFACT owns the spelling).
    for id in &shared {
        assert_ne!(
            lua_ids[*id], js_ids[*id],
            "the hostile planners agree on `{id}`, so the fixture cannot distinguish"
        );
    }

    // What must hold is INSIDE each artifact: total and injective.
    for v in [&lua, &js] {
        assert_eq!(
            v.symbols.len(),
            v.instances.len(),
            "`{}`'s symbol table is not total",
            v.spec.name
        );
        assert_eq!(
            v.census().symbol_collisions,
            0,
            "`{}` spells two instances alike: {:?}",
            v.spec.name,
            v.render_symbol_collisions(&ws)
        );
    }

    // CONTROL: with the DEFAULT planner the same two artifacts spell the shared
    // instances alike. Without this, `assert_ne!` above could be passing
    // because the two views share nothing meaningful.
    let d_lua = artifact::realize(&ws, &lg, &ArtifactRequest::new(LUAJIT, &lr));
    let d_js = artifact::realize(&ws, &jg, &ArtifactRequest::new(JAVASCRIPT, &jr));
    let (dl, dj) = (ids_of(&d_lua, &lg), ids_of(&d_js, &jg));
    let agreed = shared.iter().filter(|id| dl.get(**id) == dj.get(**id)).count();
    assert_eq!(
        agreed,
        shared.len(),
        "the default planner already disagrees, so the hostile one proves nothing"
    );
}

/// The realization axis must not be inert: two artifacts must differ in what
/// they realize, or every arm above could be dead.
#[test]
fn two_artifacts_differ_in_realization() {
    let ws = check(REACH);
    let (lg, lr) = view_of(&ws, LUAJIT);
    let (jg, jr) = view_of(&ws, JAVASCRIPT);
    let lua = artifact::realize(&ws, &lg, &ArtifactRequest::new(LUAJIT, &lr));
    let js = artifact::realize(&ws, &jg, &ArtifactRequest::new(JAVASCRIPT, &jr));

    let only_lua = lua.instances.difference(&js.instances).count();
    let only_js = js.instances.difference(&lua.instances).count();
    assert!(
        only_lua > 0 && only_js > 0,
        "the two artifacts realize identical sets, so the realization axis is \
         inert: lua-only {only_lua}, js-only {only_js}"
    );
    // ...and they differ in HOW, not only in WHICH.
    assert_ne!(
        lua.census().realized_by_kind,
        js.census().realized_by_kind,
        "the two artifacts supply every retained declaration the same way"
    );
}

/// A synthetic target with no backend of its own realizes the PORTABLE
/// subset. It is the hostile control: if retention were reading nothing, this
/// would keep every foreign body.
#[test]
fn a_target_with_no_backend_keeps_only_portable_bodies() {
    let ws = check("fn main() { let b: NativeBuffer<int> = nb_new(4); let x = b[0] }");
    let (g, roots) = view_of(&ws, NATIVE_C64);
    let view = artifact::realize(&ws, &g, &ArtifactRequest::new(NATIVE_C64, &roots));
    let c = view.census();
    assert!(c.excluded > 0, "the hostile target excluded nothing: {c:?}");
    assert!(c.instances > 0, "the hostile target excluded everything: {c:?}");
    for i in &view.instances {
        let f = &ws.module(i.func.module).functions[i.func.func];
        assert!(
            f.inline_body.is_none() && !f.is_extern,
            "`{}` has a foreign body but survived a target with no foreign backend",
            f.name
        );
    }
}

/// The ambiguity arm's PRECONDITION, measured rather than assumed.
///
/// `InlineDep` must name one instance, not one declaration, so `realize` refuses
/// a declaration with several. That arm has no population today and a mutation
/// that picks the first candidate reddens nothing -- so this records WHY rather
/// than leaving a dead branch: every erased runtime entry that gets an instance
/// is non-generic, and the generic ones (`__rock_col_sort<T, K>`) get NO
/// instance at all, because a synthesized call to a generic entry has no
/// binding record (the `no-bindings` hole, `docs/parked/mono.md` backlog 4).
///
/// When that hole closes, a generic entry gains instances and this goes red at
/// the `<= 1` assertion -- which is the point at which the ambiguity arm starts
/// mattering.
#[test]
fn every_inline_dependency_target_has_at_most_one_instance() {
    let ws = check(
        r#"
value P { x: float, y: float }
value Q { a: float, b: float }
@nomangle inline(lua) fn opaque_sorter(v: any) -> any = """
    return __rock_col_sort(v)
"""
fn main() {
    let ps: [P] = []
    ps.sort(_.x)
    let qs: [Q] = []
    qs.sort(_.a)
    let v: [Vec2] = []
    let l = v.to_lua_array()
    opaque_sorter(1)
}
"#,
    );
    let (g, roots) = graph_for(&ws, FnBackend::Lua);
    let view = artifact::realize(&ws, &g, &ArtifactRequest::new(LUAJIT, &roots));
    assert!(
        !view.inline_deps.is_empty(),
        "the fixture resolved no dependency, so the count below is vacuous"
    );
    // The generic entry the fixture deliberately names is NOT instantiated --
    // that is the reason the ambiguity arm is unreachable, stated as a
    // measurement.
    let generic_entry = g
        .instances
        .iter()
        .filter(|i| {
            ws.module(i.func.module).functions[i.func.func].name.as_str() == "__rock_col_sort"
        })
        .count();
    assert_eq!(
        generic_entry, 0,
        "`__rock_col_sort` now has instances, so the ambiguity arm is live -- give \
         it a fixture"
    );
    for dep in &view.inline_deps {
        let decl = match &dep.callee {
            Callee::Boundary(b) => b.declaration,
            Callee::Rock(i) => i.func,
        };
        let n = view.instances.iter().filter(|i| i.func == decl).count();
        assert_eq!(n, 1, "a resolved dependency had {n} candidate instances");
    }
}
