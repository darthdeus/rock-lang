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
use skull::artifact::RealizedCallee;
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
///
/// There is ONE graph. It takes no target, and the same value serves every
/// artifact -- selecting the backend sibling is `realize_decl`'s job.
fn graph_for(ws: &Workspace) -> (InstanceGraph, Vec<skull_mir::mir::FuncRef>) {
    let roots = instance_graph::emission_roots(ws, None);
    (instance_graph::build(ws, roots.clone(), false), roots)
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
// PREMISE 1 -- the shared graph retains one backend-group representative.
// ---------------------------------------------------------------------------

/// `__nb_checked_index` is declared twice: `inline(lua)` in `unified.rock` and
/// `inline(js)` in `js.rock`. Both raw declarations are emission roots, but the
/// target-neutral graph canonicalizes them to one logical instance.
#[test]
fn premise_the_shared_graph_keeps_one_representative_per_sibling_group() {
    let ws = check("fn main() { let b: NativeBuffer<int> = nb_new(4); let x = b[0] }");
    let (g, _) = graph_for(&ws);
    let decls: Vec<skull_mir::mir::FuncRef> = g
        .instances
        .iter()
        .filter(|i| {
            ws.module(i.func.module).functions[i.func.func].name.as_str()
                == "__nb_checked_index"
        })
        .map(|i| i.func)
        .collect();
    assert_eq!(
        decls.len(),
        1,
        "the shared graph holds more than one declaration of one sibling group: {:?}",
        decls
            .iter()
            .map(|d| ws.module(d.module).functions[d.func].backend)
            .collect::<Vec<_>>()
    );
    // NON-VACUITY: the group really has two members, so collapsing to one is a
    // decision and not the only thing that could have happened.
    let group: Vec<FnBackend> = ws.functions_by_name[&Ustr::from("__nb_checked_index")]
        .iter()
        .map(|&(m, f)| ws.module(m).functions[f].backend)
        .collect();
    assert!(
        group.contains(&FnBackend::Lua) && group.contains(&FnBackend::Js),
        "the fixture no longer declares both siblings: {group:?}"
    );
}

/// ONE shared instance, realized to the sibling each artifact serves. The
/// luajit artifact selects the Lua declaration and the javascript artifact the
/// JS one -- the same `Instance` key in both views, a different `Realized::decl`
/// in each.
///
/// Neither is decided through `InstanceGraph::boundaries`, which is keyed by
/// mangled symbol and therefore cannot tell the two apart at all.
#[test]
fn an_artifact_realizes_the_shared_instance_to_the_sibling_that_serves_it() {
    let ws = check("fn main() { let b: NativeBuffer<int> = nb_new(4); let x = b[0] }");
    let (g, roots) = graph_for(&ws);
    let lua = artifact::realize(&ws, &g, &ArtifactRequest::new(LUAJIT, &roots));
    let js = artifact::realize(&ws, &g, &ArtifactRequest::new(JAVASCRIPT, &roots));

    let selected = |view: &artifact::ArtifactView| -> Vec<(Instance, FnBackend)> {
        view.realization
            .iter()
            .filter(|(i, _)| {
                ws.module(i.func.module).functions[i.func.func].name.as_str()
                    == "__nb_checked_index"
            })
            .map(|(i, r)| {
                (i.clone(), ws.module(r.decl.module).functions[r.decl.func].backend)
            })
            .collect()
    };
    let (l, j) = (selected(&lua), selected(&js));
    assert_eq!(l.len(), 1, "luajit retained {} instances of the entry", l.len());
    assert_eq!(j.len(), 1, "javascript retained {} instances of the entry", j.len());
    assert_eq!(l[0].0, j[0].0, "the two artifacts realized DIFFERENT instances");
    assert_eq!(l[0].1, FnBackend::Lua, "luajit did not select the Lua sibling");
    assert_eq!(j[0].1, FnBackend::Js, "javascript did not select the JS sibling");

    // And each artifact SPELLS it through the one boundary-id helper, so the
    // two contracts are distinguishable even though `@nomangle` makes the
    // symbol identical.
    let (lid, jid) = (&lua.boundaries[&l[0].0], &js.boundaries[&j[0].0]);
    assert_eq!(lid.symbol, jid.symbol, "the fixture no longer shares an emitted symbol");
    assert_ne!(lid.declaration, jid.declaration, "one declaration served both artifacts");
    assert_ne!(lid, jid, "target-distinct boundary contracts collapsed to one id");
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
    let (g, _) = graph_for(&ws);
    let f = g
        .instances
        .iter()
        .map(|i| &ws.module(i.func.module).functions[i.func.func])
        .find(|f| f.name.as_str() == "fast_op")
        .expect("`fast_op` is not in the graph");
    assert!(
        instance_graph::serves(f.backend, FnBackend::Lua),
        "the FnBackend test already rejects it"
    );
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
    let (g, roots) = graph_for(&ws);
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
    let (g, roots) = graph_for(&ws);
    let view = artifact::realize(&ws, &g, &ArtifactRequest::new(LUAJIT, &roots));
    assert!(
        view.retained().any(|i| {
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
    let (g, _) = graph_for(&ws);
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
    let (g, roots) = graph_for(&ws);
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
            RealizedCallee::Boundary(b) => {
                assert_eq!(b.target, FnBackend::Lua, "the boundary names another target");
                assert!(
                    view.realization.values().any(|r| r.decl == b.declaration),
                    "a dependency resolved to a declaration outside the artifact"
                );
                // The SAME id the artifact minted, not a second construction of
                // one that happens to compare equal.
                assert!(
                    view.boundaries.values().any(|k| k == b),
                    "a boundary dependency is not in the artifact's boundary set"
                );
                assert!(
                    view.symbols.values().any(|s| *s == b.symbol),
                    "the boundary's symbol is not one the artifact assigned"
                );
            }
            RealizedCallee::Rock(i) => assert!(
                view.instances.contains(i),
                "a dependency resolved to an instance outside the artifact"
            ),
        }
    }
}

/// Dependency metadata follows the declaration the artifact SELECTED. The
/// target-neutral representative's body is different text and names no runtime
/// entry, so scanning only that declaration silently loses the JS dependency.
#[test]
fn inline_body_dependencies_follow_the_selected_sibling() {
    let ws = check(
        r#"
@nomangle inline(lua) fn selected_dep_probe(x: int) -> int = "return x"
@nomangle inline(js) fn selected_dep_probe(x: int) -> int = "return __nb_checked_index({ len: 1 }, x);"
fn main() { let n = selected_dep_probe(1) }
"#,
    );
    let (g, roots) = graph_for(&ws);
    let lua = artifact::realize(&ws, &g, &ArtifactRequest::new(LUAJIT, &roots));
    let js = artifact::realize(&ws, &g, &ArtifactRequest::new(JAVASCRIPT, &roots));

    let selected = |view: &artifact::ArtifactView| {
        view.realization
            .values()
            .find(|r| {
                ws.module(r.decl.module).functions[r.decl.func].name.as_str()
                    == "selected_dep_probe"
            })
            .copied()
            .expect("the artifact did not retain the probe")
    };
    let lua_probe = selected(&lua);
    let js_probe = selected(&js);
    assert_eq!(
        ws.module(lua_probe.decl.module).functions[lua_probe.decl.func].backend,
        FnBackend::Lua
    );
    assert_eq!(
        ws.module(js_probe.decl.module).functions[js_probe.decl.func].backend,
        FnBackend::Js
    );
    assert!(
        g.inline_body_deps
            .get(&instance_graph::DeclKey(js_probe.decl))
            .is_some_and(|deps| deps.contains("__nb_checked_index")),
        "the shared graph did not record the selected sibling's metadata: {:?}",
        g.inline_body_deps
    );
    assert!(
        lua.inline_deps.iter().all(|d| d.owner != lua_probe.decl),
        "the Lua body acquired a dependency it does not name"
    );
    assert!(
        js.inline_deps
            .iter()
            .any(|d| d.owner == js_probe.decl && d.entry == "__nb_checked_index"),
        "the selected JS body's runtime dependency was lost: {:?}",
        js.render_unresolved_inline_deps(&ws)
    );
}

// ---------------------------------------------------------------------------
// PREMISE 4 -- one deterministic, injective symbol per (artifact, instance).
// ---------------------------------------------------------------------------

/// Every retained instance gets exactly one symbol, and no two share one.
///
/// The `@nomangle` sibling pair that used to collide here is one instance in
/// the shared graph
/// (`premise_the_shared_graph_keeps_one_representative_per_sibling_group`), so
/// injectivity is not being bought by retention dropping a duplicate.
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
    let (g, roots) = graph_for(&ws);
    let view = artifact::realize(&ws, &g, &ArtifactRequest::new(LUAJIT, &roots));
    let c = view.census();
    assert!(c.instances > 0, "the artifact retained nothing");
    assert_eq!(
        view.symbols.len(),
        c.retained,
        "the symbol table is not total over the retained instances"
    );
    assert_eq!(
        c.retained,
        c.instances + c.boundaries,
        "the Rock/boundary split does not partition the retained set: {c:?}"
    );
    assert!(c.boundaries > 0, "the fixture retained no boundary at all: {c:?}");
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
        let (g, roots) = graph_for(&ws);
        let view = artifact::realize(&ws, &g, &ArtifactRequest::new(spec, &roots));
        for i in view.retained() {
            assert!(
                g.instances.contains(i),
                "`{}` realized an instance the shared graph does not have: {}",
                spec.name,
                g.identity(&ws, i)
            );
        }
        // The accounting is a PARTITION: every shared instance is a retained
        // Rock instance, a retained boundary, unrealizable, or unreachable --
        // never quietly absent from all four.
        let c = view.census();
        assert_eq!(
            c.instances + c.boundaries + view.excluded.len(),
            g.instances.len(),
            "`{}` lost instances between the graph and the view: {c:?}",
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
    let (g, all_roots) = graph_for(&ws);
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
        wide.retained().any(|i| {
            ws.module(i.func.module).functions[i.func.func].name.as_str()
                == "never_called_from_main"
        }),
        "the uncalled declaration is unreachable even as a root, so the fixture \
         cannot distinguish"
    );
    // ...and the walk genuinely reaches transitively, not just its roots.
    assert!(
        c.retained > view.roots.len(),
        "the walk retained nothing beyond its roots: {c:?}"
    );
    assert!(
        view.retained().any(|i| {
            ws.module(i.func.module).functions[i.func.func].name.as_str() == "ident"
        }),
        "the walk did not reach a called generic"
    );
    // Only ROCK bodies are traversed. A boundary is retained and never queued,
    // which is the same rule the graph walk follows when it stops at one.
    for i in &view.instances {
        assert!(
            view.realization[i].kind.boundary_class().is_none(),
            "a boundary was enqueued as a Rock instance"
        );
    }
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
    let (g, roots) = graph_for(&ws);
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

/// ONE graph, realized by every artifact. What differs is the DECLARATION each
/// selects and the boundary identity it mints; the logical `Instance` is shared.
///
/// The consequence is measured, not asserted from the signature: the two views
/// are taken over the same `g.instances`, and the divergence is located in the
/// realization rather than in the population.
#[test]
fn one_shared_instance_realizes_to_a_different_declaration_per_artifact() {
    let ws = check(REACH);
    let (g, roots) = graph_for(&ws);

    let lua = artifact::realize(&ws, &g, &ArtifactRequest::new(LUAJIT, &roots));
    let js = artifact::realize(&ws, &g, &ArtifactRequest::new(JAVASCRIPT, &roots));
    assert_eq!(
        lua.shared_instances, js.shared_instances,
        "the two views were taken over different populations"
    );

    // The population that can witness it: instances BOTH artifacts retain.
    let both: Vec<&Instance> =
        lua.realization.keys().filter(|i| js.retains(i)).collect();
    assert!(!both.is_empty(), "no instance is retained by both artifacts");

    // Sibling selection HAPPENED -- otherwise every agreement below is vacuous.
    let diverged: Vec<&Instance> = both
        .iter()
        .copied()
        .filter(|i| lua.realization[i].decl != js.realization[i].decl)
        .collect();
    assert!(
        !diverged.is_empty(),
        "no shared instance selected a different declaration, so this fixture \
         exercises no backend variant at all"
    );
    // ...and it is SELECTION, not a second instance: the key is one `Instance`
    // and at least one artifact chose something other than the representative.
    for i in &diverged {
        assert!(
            lua.realization[i].decl != i.func || js.realization[i].decl != i.func,
            "the declarations differ but neither is a selected sibling"
        );
    }

    // The boundary identity follows the declaration, so two artifacts name two
    // contracts for one logical identity -- plan S 0h's `InstanceId` is
    // target-neutral, the ARTIFACT owns the spelling.
    let boundary_pairs: Vec<&Instance> = diverged
        .iter()
        .copied()
        .filter(|i| lua.boundaries.contains_key(*i) && js.boundaries.contains_key(*i))
        .collect();
    assert!(
        !boundary_pairs.is_empty(),
        "no diverged instance is a boundary on both artifacts, so the id \
         comparison below would be zero over zero"
    );
    for i in boundary_pairs {
        assert_ne!(
            lua.boundaries[i], js.boundaries[i],
            "one `BoundaryId` served two targets"
        );
    }
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
    let (g, roots) = graph_for(&ws);
    let lua_p = Prefixed("lua");
    let js_p = Prefixed("js");
    let lua =
        artifact::realize(&ws, &g, &ArtifactRequest::new(LUAJIT, &roots).with_symbols(&lua_p));
    let js = artifact::realize(
        &ws,
        &g,
        &ArtifactRequest::new(JAVASCRIPT, &roots).with_symbols(&js_p),
    );

    // The shared population is now `Instance` itself: one graph, one arena, so
    // a `MonoTyId` means the same type in both views and there is nothing to
    // key around.
    let shared: Vec<&Instance> = lua.retained().filter(|i| js.retains(i)).collect();
    assert!(!shared.is_empty(), "the two artifacts share no logical instance at all");

    // The hostile planners spell every shared instance DIFFERENTLY, and that is
    // legal. If this test asserted equality it would be asserting the opposite
    // of the signed decision (plan S 0h: the ARTIFACT owns the spelling).
    for i in &shared {
        assert_ne!(
            lua.symbols[*i], js.symbols[*i],
            "the hostile planners agree on `{}`, so the fixture cannot distinguish",
            g.identity(&ws, i)
        );
    }

    // A BOUNDARY is spelled by the same planner. Boundary admission that
    // reached for `mangle_fn` instead would silently ignore the request's
    // planner, and every id below would come out target-neutral.
    let boundaries: Vec<&Instance> =
        shared.iter().copied().filter(|i| lua.boundaries.contains_key(*i)).collect();
    assert!(
        !boundaries.is_empty(),
        "no shared instance is a boundary, so the planner claim below is zero over zero"
    );
    for i in boundaries {
        assert_eq!(
            lua.boundaries[i].symbol, lua.symbols[i],
            "a boundary id was spelled by something other than the artifact's planner"
        );
        assert_ne!(
            lua.boundaries[i].symbol,
            js.boundaries[i].symbol,
            "the two planners spelled one boundary alike"
        );
    }

    // CONTROL: with the DEFAULT planner the same two artifacts spell the shared
    // instances alike. Without this, `assert_ne!` above could be passing
    // because the two views share nothing meaningful.
    let d_lua = artifact::realize(&ws, &g, &ArtifactRequest::new(LUAJIT, &roots));
    let d_js = artifact::realize(&ws, &g, &ArtifactRequest::new(JAVASCRIPT, &roots));
    let agreed = shared.iter().filter(|i| d_lua.symbols.get(**i) == d_js.symbols.get(**i)).count();
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
    let (g, roots) = graph_for(&ws);
    let lua = artifact::realize(&ws, &g, &ArtifactRequest::new(LUAJIT, &roots));
    let js = artifact::realize(&ws, &g, &ArtifactRequest::new(JAVASCRIPT, &roots));

    let only_lua = lua.retained().filter(|i| !js.retains(i)).count();
    let only_js = js.retained().filter(|i| !lua.retains(i)).count();
    assert!(
        only_lua > 0 || only_js > 0,
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
    let (g, roots) = graph_for(&ws);
    let view = artifact::realize(&ws, &g, &ArtifactRequest::new(NATIVE_C64, &roots));
    let c = view.census();
    assert!(c.excluded > 0, "the hostile target excluded nothing: {c:?}");
    assert!(c.instances > 0, "the hostile target excluded everything: {c:?}");
    for (i, r) in &view.realization {
        // Asked of the DECLARATION the artifact selected, not of the shared
        // representative: selection is what a portable-only target must not be
        // able to use to reach a foreign body.
        let f = &ws.module(r.decl.module).functions[r.decl.func];
        assert!(
            f.inline_body.is_none() && !f.is_extern,
            "`{}` has a foreign body but survived a target with no foreign backend",
            g.identity(&ws, i)
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
    let (g, roots) = graph_for(&ws);
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
            RealizedCallee::Boundary(b) => b.declaration,
            RealizedCallee::Rock(i) => view.realization[i].decl,
        };
        let n = view.realization.values().filter(|r| r.decl == decl).count();
        assert_eq!(n, 1, "a resolved dependency had {n} candidate instances");
    }
}

/// A boundary this artifact cannot serve is a SEVERED call at the caller's own
/// site, not a callee table entry. Emitting a call to a body that does not exist
/// on the target is exactly the fallback plan S 10 forbids.
#[test]
fn a_boundary_the_artifact_cannot_serve_severs_the_call() {
    let ws = check(
        r#"
inline(lua) fn only_lua(x: int) -> int = "return x + 1"
fn main() { let a = only_lua(1) }
"#,
    );
    let (g, roots) = graph_for(&ws);

    // CONTROL: on luajit the same call is an ordinary retained boundary, so the
    // severing below is a property of the ARTIFACT and not of the fixture.
    let lua = artifact::realize(&ws, &g, &ArtifactRequest::new(LUAJIT, &roots));
    fn mine<'a>(ws: &Workspace, view: &'a artifact::ArtifactView) -> Vec<&'a Instance> {
        view.boundaries
            .keys()
            .filter(|i| {
                ws.module(i.func.module).functions[i.func.func].name.as_str() == "only_lua"
            })
            .collect()
    }
    assert_eq!(mine(&ws, &lua).len(), 1, "luajit did not retain `only_lua` as a boundary");
    assert!(
        !lua.render_severed(&ws, &g).iter().any(|s| s.contains("only_lua")),
        "luajit severed its own boundary: {:?}",
        lua.render_severed(&ws, &g)
    );

    let js = artifact::realize(&ws, &g, &ArtifactRequest::new(JAVASCRIPT, &roots));
    // The fixture's OWN call, NAMED -- the stdlib supplies a background
    // population of Lua-only boundaries on this artifact, and a total count
    // would pass whether or not `only_lua` was among them.
    let severed: Vec<&artifact::Severed> = js
        .severed
        .iter()
        .filter(|s| {
            ws.module(s.callee.func.module).functions[s.callee.func.func].name.as_str()
                == "only_lua"
        })
        .collect();
    assert_eq!(
        severed.len(),
        1,
        "expected `only_lua` to be severed once, among {:?}",
        js.render_severed(&ws, &g)
    );
    assert_eq!(severed[0].cause, Unrealizable::ForeignToArtifact);
    // Located: a rejection with no span cannot become a compile error at the
    // Stage-3 cutover. The span is the walk's, read back at the severed SITE.
    let span = g.call_sites[&severed[0].caller][&severed[0].site].span;
    assert!(!instance_graph::is_spanless(&span), "the severed call carries no source span");
    // And it is NOT retained: a consumer must not be able to read a callable
    // entry for it.
    assert!(mine(&ws, &js).is_empty(), "the severed boundary is still retained on JS");
}
