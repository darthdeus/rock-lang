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
use skull::artifact::{self, ArtifactMappings, Unrealizable};
use skull::instance_graph::{self, InstanceGraph};
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

fn graph_for(ws: &Workspace, target: FnBackend) -> InstanceGraph {
    let roots = instance_graph::emission_roots(ws, None);
    instance_graph::build_for_target(ws, roots, false, target)
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
    let g = graph_for(&ws, FnBackend::Lua);
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
    let g = graph_for(&ws, FnBackend::Lua);

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

    let lua = artifact::realize(&ws, &g, LUAJIT, ArtifactMappings::for_backend(FnBackend::Lua));
    let js = artifact::realize(&ws, &g, JAVASCRIPT, ArtifactMappings::for_backend(FnBackend::Js));
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
                && e.cause == Unrealizable::ForeignToArtifact
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
    let g = graph_for(&ws, FnBackend::Lua);
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
    let g = graph_for(&ws, FnBackend::Lua);
    let named = |view: &artifact::ArtifactView| -> Option<Unrealizable> {
        view.excluded
            .iter()
            .find(|e| {
                ws.module(e.instance.func.module).functions[e.instance.func.func].name.as_str()
                    == "fast_op"
            })
            .map(|e| e.cause)
    };

    let bare = artifact::realize(&ws, &g, LUAJIT, ArtifactMappings::for_backend(FnBackend::Lua));
    assert_eq!(
        named(&bare),
        Some(Unrealizable::NativeMappingMissing),
        "a `@native_required` intrinsic with only a Rock fallback was realized"
    );

    // CONTROL: the same graph, the same target, one table entry different. If
    // the arm rejected every intrinsic this would still be excluded.
    let table = OneName("fast_op");
    let mapped = artifact::realize(
        &ws,
        &g,
        LUAJIT,
        ArtifactMappings::for_backend(FnBackend::Lua).with_table(&table),
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
    let g = graph_for(&ws, FnBackend::Lua);
    let view = artifact::realize(&ws, &g, LUAJIT, ArtifactMappings::for_backend(FnBackend::Lua));
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
    let g = graph_for(&ws, FnBackend::Lua);
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
    let g = graph_for(&ws, FnBackend::Lua);
    let view = artifact::realize(&ws, &g, LUAJIT, ArtifactMappings::for_backend(FnBackend::Lua));
    let c = view.census();
    assert!(c.inline_deps_examined > 0, "no dependency was examined: {c:?}");
    assert_eq!(
        c.inline_deps_unresolved, 0,
        "unresolved inline-body dependencies: {:?}",
        view.render_unresolved_inline_deps(&ws)
    );
    // Each resolved target is a declaration the artifact RETAINS, so emission
    // cannot be asked for a symbol the artifact dropped.
    for dep in &view.inline_deps {
        assert!(
            view.instances.iter().any(|i| i.func == dep.target),
            "a dependency resolved to a declaration outside the artifact"
        );
        assert!(
            view.symbols.keys().any(|i| i.func == dep.target),
            "a resolved dependency has no artifact symbol"
        );
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
    let g = graph_for(&ws, FnBackend::Lua);
    let siblings: Vec<&skull::instance_graph::Instance> = g
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
    let g = graph_for(&ws, FnBackend::Lua);
    let view = artifact::realize(&ws, &g, LUAJIT, ArtifactMappings::for_backend(FnBackend::Lua));
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
    let again = artifact::realize(&ws, &g, LUAJIT, ArtifactMappings::for_backend(FnBackend::Lua));
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
    let ws = check(
        r#"
fn ident<T>(x: T) -> T { return x }
fn main() {
    let b: NativeBuffer<int> = nb_new(4)
    let x = b[0]
    let a = ident(1)
}
"#,
    );
    let g = graph_for(&ws, FnBackend::Lua);
    for (spec, mappings) in [
        (LUAJIT, ArtifactMappings::for_backend(FnBackend::Lua)),
        (JAVASCRIPT, ArtifactMappings::for_backend(FnBackend::Js)),
        (NATIVE_C64, ArtifactMappings::none()),
    ] {
        let view = artifact::realize(&ws, &g, spec, mappings);
        for i in &view.instances {
            assert!(
                g.instances.contains(i),
                "`{}` realized an instance the shared graph does not have: {}",
                spec.name,
                g.identity(&ws, i)
            );
        }
        // The accounting is exact: every shared instance is retained or
        // excluded, never quietly absent from both.
        assert_eq!(
            view.instances.len() + view.excluded.len(),
            g.instances.len(),
            "`{}` lost instances between the graph and the view",
            spec.name
        );
    }
}

/// The cross-target control. Two artifacts must SHARE logical instances --
/// otherwise "identity survives realization" is a claim about disjoint sets --
/// and must DIFFER in what they realize, otherwise the realization axis is
/// inert and every arm above could be dead.
#[test]
fn two_artifacts_share_logical_instances_and_differ_in_realization() {
    let ws = check(
        r#"
fn ident<T>(x: T) -> T { return x }
fn main() {
    let b: NativeBuffer<int> = nb_new(4)
    let x = b[0]
    let a = ident(1)
}
"#,
    );
    let g = graph_for(&ws, FnBackend::Lua);
    let lua = artifact::realize(&ws, &g, LUAJIT, ArtifactMappings::for_backend(FnBackend::Lua));
    let js = artifact::realize(&ws, &g, JAVASCRIPT, ArtifactMappings::for_backend(FnBackend::Js));

    let shared: Vec<&skull::instance_graph::Instance> =
        lua.instances.iter().filter(|i| js.instances.contains(*i)).collect();
    assert!(!shared.is_empty(), "the two artifacts share no logical instance at all");
    // Shared instances keep ONE logical identity across the two artifacts --
    // the S 20g.1 property, asserted over the shared population rather than
    // over a digest that could agree for the wrong reason.
    for i in &shared {
        assert_eq!(
            lua.symbols.get(*i),
            js.symbols.get(*i),
            "`{}` is spelled differently by the two artifacts",
            g.identity(&ws, i)
        );
    }

    let only_lua: Vec<&skull::instance_graph::Instance> =
        lua.instances.iter().filter(|i| !js.instances.contains(*i)).collect();
    let only_js: Vec<&skull::instance_graph::Instance> =
        js.instances.iter().filter(|i| !lua.instances.contains(*i)).collect();
    assert!(
        !only_lua.is_empty() && !only_js.is_empty(),
        "the two artifacts realize identical sets, so the realization axis is inert: \
         lua-only {}, js-only {}",
        only_lua.len(),
        only_js.len()
    );
}

/// A synthetic target with no backend of its own realizes the PORTABLE
/// subset. It is the hostile control: if retention were reading nothing, this
/// would keep every foreign body.
#[test]
fn a_target_with_no_backend_keeps_only_portable_bodies() {
    let ws = check("fn main() { let b: NativeBuffer<int> = nb_new(4); let x = b[0] }");
    let g = graph_for(&ws, FnBackend::Lua);
    let view = artifact::realize(&ws, &g, NATIVE_C64, ArtifactMappings::none());
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
