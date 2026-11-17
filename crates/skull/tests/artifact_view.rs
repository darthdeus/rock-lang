use crate::common::{fn_name, graph_for, key, module_id, name_matches, ws_with_stdlib};
use bone_semantic::hir::{BodyOwner, FnBackend};
use bone_semantic::inference::Workspace;
use bone_semantic::prelude::ModuleId;
use skull::artifact::RealizedCallee;
use skull::artifact::{self, ArtifactMappings, ArtifactRequest, Exclusion, Unrealizable};
use skull::instance_graph::{self, Instance, InstanceGraph};
use skull::target_projection::{
    JAVASCRIPT_WEB, LUAJIT_DESKTOP, NATIVE_C64_DESKTOP, TargetSpec, WASM32_WASI,
};
use skull_mir::intrinsics::BackendRealization;
use skull_mir::mir::FuncRef;

struct Prefixed(&'static str);
impl artifact::SymbolPlanner for Prefixed {
    fn symbol(
        &self,
        ws: &Workspace,
        g: &InstanceGraph,
        i: &Instance,
        _realized: artifact::Realized,
    ) -> String {
        format!("{}${}", self.0, g.logical_symbol(ws, i))
    }
    fn describe(&self) -> &'static str {
        "prefixed"
    }
}

struct MainRightCollision;

impl artifact::SymbolPlanner for MainRightCollision {
    fn symbol(
        &self,
        ws: &Workspace,
        g: &InstanceGraph,
        i: &Instance,
        _realized: artifact::Realized,
    ) -> String {
        let name = fn_name(ws, i.func);
        if matches!(name, "main" | "right") {
            "shared".to_string()
        } else {
            g.logical_symbol(ws, i)
        }
    }

    fn describe(&self) -> &'static str {
        "main-right-collision"
    }
}

static MAIN_RIGHT_COLLISION: MainRightCollision = MainRightCollision;

fn realize(
    ws: &Workspace,
    g: &InstanceGraph,
    spec: TargetSpec,
    roots: &[FuncRef],
) -> artifact::ArtifactView {
    artifact::realize(ws, g, &ArtifactRequest::new(spec, roots))
}

fn backend(ws: &Workspace, f: FuncRef) -> FnBackend {
    ws.module(f.module).functions[f.func].backend
}

fn is_nomangle(f: &bone_semantic::hir::Function) -> bool {
    f.annotations
        .iter()
        .any(|a| a.name.name.as_str() == "nomangle")
}

/// A realization whose `backend` mappings are supplied by `supply`.
fn realize_supplied(
    ws: &Workspace,
    g: &InstanceGraph,
    spec: TargetSpec,
    roots: &[FuncRef],
    backend: FnBackend,
    supply: &Supplies,
) -> artifact::ArtifactView {
    artifact::realize(
        ws,
        g,
        &ArtifactRequest::new(spec, roots)
            .with_mappings(ArtifactMappings::for_backend(backend).with_supply(supply)),
    )
}

/// `roots` narrowed to `main`, so the view walks from it alone while the
/// shared graph keeps every emission root.
fn main_roots(ws: &Workspace, roots: &[FuncRef]) -> Vec<FuncRef> {
    roots
        .iter()
        .copied()
        .filter(|r| fn_name(ws, *r) == "main")
        .collect()
}

/// How `view` realizes the retained instance of the declaration `name`.
fn realized_kind(
    ws: &Workspace,
    view: &artifact::ArtifactView,
    name: &str,
) -> Option<artifact::Realization> {
    view.realization
        .iter()
        .find(|(i, _)| fn_name(ws, i.func) == name)
        .map(|(_, r)| r.kind)
}

/// The cause `view` severs calls to the declaration `name` under.
fn severed_cause(
    ws: &Workspace,
    view: &artifact::ArtifactView,
    name: &str,
) -> Option<Unrealizable> {
    view.severed
        .iter()
        .find(|site| fn_name(ws, site.callee.func) == name)
        .map(|site| site.cause)
}

/// A backend realization oracle that supplies exactly the named declarations --
/// the stand-in for a real backend's own (`skull_codegen_js::js_supply`). One
/// oracle, so an `@intrinsic` mapping and a hand-written host binding differ
/// only in whether the DECLARATION carries the marker.
struct Supplies(&'static [&'static str]);
impl BackendRealization for Supplies {
    fn backend(&self) -> &'static str {
        "toy"
    }
    fn supplies(&self, _ws: &Workspace, f: &bone_semantic::hir::Function) -> bool {
        self.0
            .contains(&bone_core::modules::display_name(f.name.as_str()))
    }
}

#[test]
fn an_artifact_realizes_the_shared_instance_to_the_sibling_that_serves_it() {
    let ws = ws_with_stdlib("fn main() { let b: NativeBuffer<int> = nb_new(4); let x = b[0] }");
    let (g, roots) = graph_for(&ws);
    let decls: Vec<FuncRef> = g
        .instances
        .iter()
        .filter(|i| fn_name(&ws, i.func) == "__nb_checked_index")
        .map(|i| i.func)
        .collect();
    assert_eq!(
        decls.len(),
        1,
        "the shared graph holds more than one declaration of one sibling group: {:?}",
        decls.iter().map(|d| backend(&ws, *d)).collect::<Vec<_>>()
    );
    // NON-VACUITY: the group really has two members, so collapsing to one is a
    // decision and not the only thing that could have happened.
    let group: Vec<FnBackend> = ws.functions_by_name[&key(&ws, "__nb_checked_index")]
        .iter()
        .map(|&(m, f)| ws.module(m).functions[f].backend)
        .collect();
    assert!(
        group.contains(&FnBackend::Lua) && group.contains(&FnBackend::Js),
        "the fixture no longer declares both siblings: {group:?}"
    );

    let lua = realize(&ws, &g, LUAJIT_DESKTOP, &roots);
    let js = realize(&ws, &g, JAVASCRIPT_WEB, &roots);
    assert_eq!(
        lua.shared_instances, js.shared_instances,
        "the two views were taken over different populations"
    );

    let selected = |view: &artifact::ArtifactView| -> Vec<(Instance, FnBackend)> {
        view.realization
            .iter()
            .filter(|(i, _)| fn_name(&ws, i.func) == "__nb_checked_index")
            .map(|(i, r)| (i.clone(), backend(&ws, r.decl)))
            .collect()
    };
    let (l, j) = (selected(&lua), selected(&js));
    assert_eq!(
        l.len(),
        1,
        "luajit retained {} instances of the entry",
        l.len()
    );
    assert_eq!(
        j.len(),
        1,
        "javascript retained {} instances of the entry",
        j.len()
    );
    assert_eq!(
        l[0].0, j[0].0,
        "the two artifacts realized DIFFERENT instances"
    );
    assert_eq!(
        l[0].1,
        FnBackend::Lua,
        "luajit did not select the Lua sibling"
    );
    assert_eq!(
        j[0].1,
        FnBackend::Js,
        "javascript did not select the JS sibling"
    );

    // And each artifact SPELLS it through the one boundary-id helper, so the
    // two contracts are distinguishable even though `@nomangle` makes the
    // symbol identical.
    let (lid, jid) = (&lua.boundaries[&l[0].0], &js.boundaries[&j[0].0]);
    assert_eq!(
        lid.symbol, jid.symbol,
        "the fixture no longer shares an emitted symbol"
    );
    assert_ne!(
        lid.declaration, jid.declaration,
        "one declaration served both artifacts"
    );
    assert_ne!(
        lid, jid,
        "target-distinct boundary contracts collapsed to one id"
    );
}

#[test]
fn an_artifact_symbol_follows_the_declaration_it_selected() {
    let ws = ws_with_stdlib(
        r#"
@nomangle inline(lua) fn sibling_probe(x: int) -> int = "return x"
inline(js) fn sibling_probe(x: int) -> int = "return x"
fn main() { let x = sibling_probe(1) }
"#,
    );
    let (g, roots) = graph_for(&ws);
    let js = artifact::realize(
        &ws,
        &g,
        &ArtifactRequest::new(JAVASCRIPT_WEB, &roots).with_symbols(&artifact::REALIZED_SYMBOLS),
    );
    let (instance, realized) = js
        .realization
        .iter()
        .find(|(i, r)| fn_name(&ws, i.func) == "sibling_probe" && r.decl != i.func)
        .expect("the fixture did not select a JS sibling");
    let selected = Instance {
        func: realized.decl,
        args: instance.args.clone(),
    };
    assert_eq!(
        js.symbols[instance],
        selected.symbol(&ws, g.interner()),
        "the artifact symbol came from the target-neutral representative"
    );
}

const NATIVE_REQUIRED: &str = r#"
@intrinsic @native_required
fn fast_op(x: int) -> int { return x * 2 }
@intrinsic
fn portable_op(x: int) -> int { return x * 2 }
fn caller(x: int) -> int { return fast_op(x) }
fn main() {
    let a = caller(1)
    let b = portable_op(1)
}
"#;

/// A `@native_required` intrinsic needs the artifact's mapping, and its missing
/// mapping severs the call while retaining the caller. A portable intrinsic is
/// the control for the fallback path.
#[test]
fn native_required_mapping_controls_fallback_and_severed_calls() {
    let ws = ws_with_stdlib(NATIVE_REQUIRED);
    let (g, roots) = graph_for(&ws);
    let f = g
        .instances
        .iter()
        .map(|i| &ws.module(i.func.module).functions[i.func.func])
        .find(|f| name_matches(f.name, "fast_op"))
        .expect("`fast_op` is not in the graph");
    assert!(
        instance_graph::serves(f.backend, FnBackend::Lua),
        "the FnBackend test already rejects it"
    );
    assert!(
        f.inline_body.is_none(),
        "the fixture has a native mapping after all"
    );
    assert!(
        skull_mir::intrinsics::is_native_required(f),
        "the fixture is not `@native_required`"
    );

    let named = |view: &artifact::ArtifactView| -> Option<Exclusion> {
        view.excluded
            .iter()
            .find(|e| fn_name(&ws, e.instance.func) == "fast_op")
            .map(|e| e.cause)
    };

    let bare = realize(&ws, &g, LUAJIT_DESKTOP, &roots);
    assert_eq!(
        named(&bare),
        Some(Exclusion::Unrealizable(Unrealizable::NativeMappingMissing)),
        "a `@native_required` intrinsic with only a Rock fallback was realized"
    );
    assert!(
        bare.retained()
            .any(|i| fn_name(&ws, i.func) == "portable_op"),
        "a portable intrinsic was dropped: {:?}",
        bare.excluded
    );
    assert!(
        bare.excluded
            .iter()
            .all(|e| fn_name(&ws, e.instance.func) != "portable_op"),
        "the portable rung did not fire"
    );
    assert!(
        bare.sites_walked > 0,
        "no site was walked, so 0 severed says nothing: {:?}",
        bare.severed
    );
    let named_severed: Vec<_> = bare
        .severed
        .iter()
        .filter(|site| fn_name(&ws, site.callee.func) == "fast_op")
        .collect();
    assert_eq!(
        named_severed.len(),
        1,
        "expected exactly the `fast_op` call to be severed, got {:?}",
        bare.severed
    );
    assert_eq!(
        bare.severed.len(),
        1,
        "unexpected additional severed calls: {:?}",
        bare.severed
    );
    assert!(
        named_severed[0].cause == Unrealizable::NativeMappingMissing,
        "the severed call lost its cause: {named_severed:?}"
    );
    assert!(
        bare.instances
            .iter()
            .any(|i| fn_name(&ws, i.func) == "caller"),
        "the caller was dropped, so nothing would have emitted the call anyway"
    );

    // The same graph and target with one mapping added serves the call.
    let table = Supplies(&["fast_op"]);
    let mapped = realize_supplied(&ws, &g, LUAJIT_DESKTOP, &roots, FnBackend::Lua, &table);
    assert_eq!(
        named(&mapped),
        None,
        "a mapped `@native_required` intrinsic was still excluded"
    );
    assert!(
        mapped.severed.is_empty(),
        "mapping the callee left the call severed"
    );
}

/// The artifact resolves each dependency to a declaration IT retains, and says
/// so per dependency rather than as a total.
#[test]
fn inline_body_dependencies_resolve_to_targets_in_the_artifact() {
    let ws = ws_with_stdlib("fn main() { let v: [Vec2] = []; let l = v.to_lua_array() }");
    let (g, roots) = graph_for(&ws);
    let deps: Vec<&str> = g
        .inline_body_deps
        .values()
        .flat_map(|s| s.iter())
        .copied()
        .collect();
    assert!(
        !deps.is_empty(),
        "the fixture records no inline-body dependency at all"
    );
    // A NAME, and nothing that names a declaration: no `FuncRef`, no instance.
    assert!(
        deps.iter().all(|d| d.starts_with("__")),
        "the dependencies are no longer bare runtime-entry names: {deps:?}"
    );

    let view = realize(&ws, &g, LUAJIT_DESKTOP, &roots);
    assert!(
        view.unresolved_inline_deps.is_empty(),
        "unresolved inline-body dependencies: {:?}",
        view.unresolved_inline_deps
    );
    // Each dependency terminates in a CALLEE, not a bare declaration: a
    // runtime entry is a `Callee::Boundary` carrying its class and the symbol
    // emission must produce, and anything else names a concrete instance.
    assert!(
        view.inline_deps
            .iter()
            .any(|dep| matches!(&dep.callee, RealizedCallee::Boundary(_))),
        "no dependency resolved to a boundary: {:?}",
        view.inline_deps
    );
    for dep in &view.inline_deps {
        match &dep.callee {
            RealizedCallee::Boundary(b) => {
                assert_eq!(
                    b.target.execution.backend,
                    Some(FnBackend::Lua),
                    "the boundary names another execution backend"
                );
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
    let ws = ws_with_stdlib(
        r#"
@nomangle inline(lua) fn selected_dep_probe(x: int) -> int = "return x"
@nomangle inline(js) fn selected_dep_probe(x: int) -> int = "return __nb_checked_index({ len: 1 }, x);"
fn main() { let n = selected_dep_probe(1) }
"#,
    );
    let (g, roots) = graph_for(&ws);
    let lua = realize(&ws, &g, LUAJIT_DESKTOP, &roots);
    let js = realize(&ws, &g, JAVASCRIPT_WEB, &roots);

    let selected = |view: &artifact::ArtifactView| {
        view.realization
            .values()
            .find(|r| fn_name(&ws, r.decl) == "selected_dep_probe")
            .copied()
            .expect("the artifact did not retain the probe")
    };
    let lua_probe = selected(&lua);
    let js_probe = selected(&js);
    assert_eq!(backend(&ws, lua_probe.decl), FnBackend::Lua);
    assert_eq!(backend(&ws, js_probe.decl), FnBackend::Js);
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
        js.unresolved_inline_deps
    );
}

#[test]
fn an_artifact_symbol_is_total_and_injective_over_what_it_retains() {
    let ws = ws_with_stdlib(
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
    let view = realize(&ws, &g, LUAJIT_DESKTOP, &roots);
    assert!(!view.instances.is_empty(), "the artifact retained nothing");
    assert_eq!(
        view.symbols.len(),
        view.realization.len(),
        "the symbol table is not total over the retained instances"
    );
    assert_eq!(
        view.realization.len(),
        view.instances.len() + view.boundaries.len(),
        "the Rock/boundary split does not partition the retained set"
    );
    assert!(
        !view.boundaries.is_empty(),
        "the fixture retained no boundary at all"
    );
    assert!(
        view.symbol_collisions.is_empty(),
        "two retained instances share one symbol: {:?}",
        view.symbol_collisions
    );
    // Non-vacuous: the generic instances must actually be spelled apart, so
    // injectivity is not satisfied by there being nothing to collide.
    let idents: Vec<&String> = view
        .symbols
        .iter()
        .filter(|(i, _)| fn_name(&ws, i.func) == "ident")
        .map(|(_, s)| s)
        .collect();
    assert_eq!(
        idents.len(),
        2,
        "the fixture has no two-instance generic: {idents:?}"
    );
    assert_ne!(idents[0], idents[1], "two instantiations spell alike");

    // Deterministic: realizing the same graph twice gives the same table.
    let again = realize(&ws, &g, LUAJIT_DESKTOP, &roots);
    assert_eq!(
        view.symbols, again.symbols,
        "the symbol assignment is not deterministic"
    );
}

#[test]
fn realization_never_invents_an_instance() {
    let ws = ws_with_stdlib(REACH);
    let (g, roots) = graph_for(&ws);
    for spec in [LUAJIT_DESKTOP, JAVASCRIPT_WEB, NATIVE_C64_DESKTOP] {
        let view = realize(&ws, &g, spec, &roots);
        assert_instance_partition(&view, &g);
    }
}

fn assert_instance_partition(view: &artifact::ArtifactView, graph: &InstanceGraph) {
    use std::collections::BTreeSet;

    let name = view.spec.name;
    let retained: BTreeSet<_> = view.retained().cloned().collect();
    let excluded: BTreeSet<_> = view
        .excluded
        .iter()
        .map(|row| row.instance.clone())
        .collect();
    assert_eq!(
        excluded.len(),
        view.excluded.len(),
        "{name}: duplicate excluded instance"
    );
    assert!(
        retained.is_disjoint(&excluded),
        "{name}: retained and excluded the same instance"
    );
    assert_eq!(
        retained.union(&excluded).cloned().collect::<BTreeSet<_>>(),
        graph.instances,
        "{name}: did not account for every shared instance"
    );
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

/// Give the view fewer roots than the shared graph, so it has realizable
/// instances that it cannot reach.
#[test]
fn retention_is_reachability_not_a_declaration_filter() {
    let ws = ws_with_stdlib(REACH);
    let (g, all_roots) = graph_for(&ws);
    let main_only = main_roots(&ws, &all_roots);
    assert_eq!(main_only.len(), 1, "the fixture has no unique `main` root");

    let view = realize(&ws, &g, LUAJIT_DESKTOP, &main_only);
    // EXACTLY the roots it was given, plus the always-emitted callees, which
    // carry no incoming edge and would otherwise read as unreachable. Stated as
    // a set equality, so a view that quietly seeded from the whole graph could
    // not pass by having "enough" roots.
    let expected: std::collections::BTreeSet<&Instance> = g
        .instances
        .iter()
        .filter(|i| (i.func == main_only[0] && i.args.is_empty()) || g.is_always_emitted_target(i))
        .collect();
    assert_eq!(
        view.roots.iter().collect::<std::collections::BTreeSet<_>>(),
        expected,
        "the artifact's roots are not the ones it was given: {:?}",
        view.roots
    );
    assert!(
        view.sites_walked > 0,
        "the walk traversed no call site: {:?}",
        view.severed
    );
    // Both causes have a population, so neither reads as the other.
    assert!(
        view.excluded
            .iter()
            .any(|entry| entry.cause == Exclusion::Unreachable),
        "nothing was excluded as unreachable, so retention is still a filter: {:?}",
        view.excluded
    );
    assert!(
        view.excluded.iter().any(|entry| {
            entry.cause == Exclusion::Unrealizable(Unrealizable::ForeignToArtifact)
        }),
        "nothing was excluded as unrealizable: {:?}",
        view.excluded
    );
    // Named, not merely counted: the fixture's uncalled declaration is
    // REALIZABLE here, which is what makes its exclusion a reachability fact
    // and not a realization one.
    let uncalled = view
        .excluded
        .iter()
        .find(|e| fn_name(&ws, e.instance.func) == "never_called_from_main");
    assert_eq!(
        uncalled.map(|e| e.cause),
        Some(Exclusion::Unreachable),
        "an uncalled but compilable declaration was not excluded as unreachable"
    );
    // CONTROL: the same declaration IS retained when it is a root, so the
    // exclusion above is a property of reachability and not of the declaration.
    let wide = realize(&ws, &g, LUAJIT_DESKTOP, &all_roots);
    assert!(
        wide.retained()
            .any(|i| { fn_name(&ws, i.func) == "never_called_from_main" }),
        "the uncalled declaration is unreachable even as a root, so the fixture \
         cannot distinguish"
    );
    // ...and the walk genuinely reaches transitively, not just its roots.
    assert!(
        view.retained().count() > view.roots.len(),
        "the walk retained nothing beyond its roots: {:?}",
        view.roots
    );
    assert!(
        view.retained().any(|i| { fn_name(&ws, i.func) == "ident" }),
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

/// Use different symbol planners to exercise target-local names.
#[test]
fn cross_artifact_identity_is_the_instance_and_not_the_spelling() {
    let ws = ws_with_stdlib(REACH);
    let (g, roots) = graph_for(&ws);
    let lua_p = Prefixed("lua");
    let js_p = Prefixed("js");
    let lua = artifact::realize(
        &ws,
        &g,
        &ArtifactRequest::new(LUAJIT_DESKTOP, &roots).with_symbols(&lua_p),
    );
    let js = artifact::realize(
        &ws,
        &g,
        &ArtifactRequest::new(JAVASCRIPT_WEB, &roots).with_symbols(&js_p),
    );

    // The shared population is now `Instance` itself: one graph, one arena, so
    // a `MonoTyId` means the same type in both views and there is nothing to
    // key around.
    let shared: Vec<&Instance> = lua.retained().filter(|i| js.retains(i)).collect();
    assert!(
        !shared.is_empty(),
        "the two artifacts share no logical instance at all"
    );

    // The hostile planners spell every shared instance DIFFERENTLY, and that is
    // legal. If this test asserted equality it would be asserting the opposite
    // of the signed decision (plan S 0h: the ARTIFACT owns the spelling).
    for i in &shared {
        assert_ne!(
            lua.symbols[*i],
            js.symbols[*i],
            "the hostile planners agree on `{}`, so the fixture cannot distinguish",
            g.identity(&ws, i)
        );
    }

    // A BOUNDARY is spelled by the same planner. Boundary admission that
    // reached for `mangle_fn` instead would silently ignore the request's
    // planner, and every id below would come out target-neutral.
    let boundaries: Vec<&Instance> = shared
        .iter()
        .copied()
        .filter(|i| lua.boundaries.contains_key(*i))
        .collect();
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
            lua.boundaries[i].symbol, js.boundaries[i].symbol,
            "the two planners spelled one boundary alike"
        );
    }

    // CONTROL: with the DEFAULT planner the same two artifacts spell the shared
    // instances alike. Without this, `assert_ne!` above could be passing
    // because the two views share nothing meaningful.
    let d_lua = realize(&ws, &g, LUAJIT_DESKTOP, &roots);
    let d_js = realize(&ws, &g, JAVASCRIPT_WEB, &roots);
    let agreed = shared
        .iter()
        .filter(|i| d_lua.symbols.get(**i) == d_js.symbols.get(**i))
        .count();
    assert_eq!(
        agreed,
        shared.len(),
        "the default planner already disagrees, so the hostile one proves nothing"
    );
}

/// A synthetic target with no backend of its own realizes the PORTABLE
/// subset. It is the hostile control: if retention were reading nothing, this
/// would keep every foreign body.
#[test]
fn a_target_with_no_backend_keeps_only_portable_bodies() {
    let ws = ws_with_stdlib("fn main() { let b: NativeBuffer<int> = nb_new(4); let x = b[0] }");
    let (g, roots) = graph_for(&ws);
    let view = realize(&ws, &g, NATIVE_C64_DESKTOP, &roots);
    assert!(
        !view.excluded.is_empty(),
        "the hostile target excluded nothing"
    );
    assert!(
        !view.instances.is_empty(),
        "the hostile target excluded everything"
    );
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

#[test]
fn synthetic_targets_share_instances_and_diverge_in_physical_obligations() {
    let ws = ws_with_stdlib(
        r#"
extern(ffi, "host") fn host_tick(n: int) -> int
fn main() { let x = 1; host_tick(x) }
"#,
    );
    let (g, roots) = graph_for(&ws);
    assert!(
        g.instances.len() > 1,
        "the synthetic-target graph has no nonzero population"
    );

    let c64 = realize(&ws, &g, NATIVE_C64_DESKTOP, &roots);
    let wasm = realize(&ws, &g, WASM32_WASI, &roots);
    let shared = c64.retained().filter(|i| wasm.retains(i)).count();
    let c64_only = c64.retained().filter(|i| !wasm.retains(i)).count();
    let wasm_only = wasm.retained().filter(|i| !c64.retains(i)).count();

    assert!(shared > 0, "the two projections share no logical instances");
    assert_eq!(
        c64_only, 0,
        "C64 changed the shared logical realization population"
    );
    assert_eq!(
        wasm_only, 0,
        "Wasm32 changed the shared logical realization population"
    );
    assert_eq!(c64.realization, wasm.realization);
    assert_eq!(c64.excluded, wasm.excluded);
    assert_eq!(c64.severed, wasm.severed);
    assert_ne!(
        c64.deferred, wasm.deferred,
        "the physical target obligations are identical"
    );
    assert!(
        c64.deferred.iter().any(|d| d.contains("pointer width")),
        "C64 has no target-specific physical obligation"
    );
    assert!(
        c64.deferred.iter().any(|d| d.contains("64-bit")),
        "C64 float width is not projected"
    );
    assert!(
        wasm.deferred.iter().any(|d| d.contains("import entry")),
        "Wasm32 has no target-specific boundary obligation"
    );
    assert_eq!(c64.shared_instances, g.instances.len());
    assert_eq!(wasm.shared_instances, g.instances.len());
    for view in [&c64, &wasm] {
        assert_instance_partition(view, &g);
    }
}

/// Erased runtime entries are non-generic and therefore have at most one instance.
#[test]
fn every_inline_dependency_target_has_at_most_one_instance() {
    let ws = ws_with_stdlib(
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
    let view = realize(&ws, &g, LUAJIT_DESKTOP, &roots);
    assert!(
        !view.inline_deps.is_empty(),
        "the fixture resolved no dependency, so the count below is vacuous"
    );
    for name in instance_graph::ERASED_RUNTIME_ENTRIES {
        let decls = ws.functions_by_name.get(&key(&ws, name));
        for &(module, func) in decls.into_iter().flatten() {
            assert!(
                ws.module(module).functions[func].generics.is_empty(),
                "runtime entry `{name}` is generic, so it can have several instances \
                 and the ambiguity arm is live -- give it a fixture"
            );
        }
    }
    // The entry the fixture names from both a synthesized call and opaque text
    // resolves to its one base instance.
    let entry: Vec<&Instance> = g
        .instances
        .iter()
        .filter(|i| fn_name(&ws, i.func) == "__rock_col_sort")
        .collect();
    assert_eq!(
        entry.len(),
        1,
        "`__rock_col_sort` should have exactly its base instance: {entry:?}"
    );
    assert!(entry[0].args.is_empty());
    assert!(
        view.inline_deps.iter().any(|dep| match &dep.callee {
            RealizedCallee::Boundary(b) => b.declaration == entry[0].func,
            RealizedCallee::Rock(i) => i == entry[0],
        }),
        "the opaque body's `__rock_col_sort` dependency did not resolve to its instance"
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
    let ws = ws_with_stdlib(
        r#"
inline(lua) fn only_lua(x: int) -> int = "return x + 1"
fn main() { let a = only_lua(1) }
"#,
    );
    let (g, roots) = graph_for(&ws);

    // CONTROL: on luajit the same call is an ordinary retained boundary, so the
    // severing below is a property of the ARTIFACT and not of the fixture.
    let lua = realize(&ws, &g, LUAJIT_DESKTOP, &roots);
    fn mine<'a>(ws: &Workspace, view: &'a artifact::ArtifactView) -> Vec<&'a Instance> {
        view.boundaries
            .keys()
            .filter(|i| fn_name(ws, i.func) == "only_lua")
            .collect()
    }
    assert_eq!(
        mine(&ws, &lua).len(),
        1,
        "luajit did not retain `only_lua` as a boundary"
    );
    let lua_severed: Vec<_> = lua
        .severed
        .iter()
        .filter(|s| fn_name(&ws, s.callee.func) == "only_lua")
        .collect();
    assert!(
        lua_severed.is_empty(),
        "luajit severed its own boundary: {lua_severed:?}"
    );

    let js = realize(&ws, &g, JAVASCRIPT_WEB, &roots);
    // The fixture's OWN call, NAMED -- the stdlib supplies a background
    // population of Lua-only boundaries on this artifact, and a total count
    // would pass whether or not `only_lua` was among them.
    let severed: Vec<&artifact::Severed> = js
        .severed
        .iter()
        .filter(|s| fn_name(&ws, s.callee.func) == "only_lua")
        .collect();
    assert_eq!(
        severed.len(),
        1,
        "expected `only_lua` to be severed once, among {:?}",
        js.severed
    );
    assert_eq!(severed[0].cause, Unrealizable::ForeignToArtifact);
    // Located: a rejection with no span cannot become a compile error at the
    // Stage-3 cutover. The span is the walk's, read back at the severed SITE.
    let span = g.sites_of(&severed[0].caller).expect("the caller's sites")[&severed[0].site].span;
    assert!(
        !instance_graph::is_spanless(&span),
        "the severed call carries no source span"
    );
    // And it is NOT retained: a consumer must not be able to read a callable
    // entry for it.
    assert!(
        mine(&ws, &js).is_empty(),
        "the severed boundary is still retained on JS"
    );
}

/// The platform axis participates in boundary admission and in the boundary
/// contract's identity, even where the execution backend is the same.
#[test]
fn platform_decides_boundary_admission_and_contract_identity() {
    use skull::target_projection as tp;

    let ws = ws_with_stdlib(
        r#"
extern(ffi, "host") fn host_tick(n: int) -> int
fn main() { let n = host_tick(1) }
"#,
    );
    let (g, roots) = graph_for(&ws);
    let target = |platform| tp::TargetSpec {
        name: "platform-admission-probe",
        platform,
        ..tp::LUAJIT_DESKTOP
    };
    let desktop = realize(&ws, &g, target(tp::DESKTOP), &roots);
    let web = realize(&ws, &g, target(tp::WEB), &roots);
    let host_tick_severed = |view: &artifact::ArtifactView| {
        view.severed
            .iter()
            .find_map(|s| (fn_name(&ws, s.callee.func) == "host_tick").then_some(s.cause))
    };
    assert!(
        host_tick_severed(&desktop).is_none(),
        "desktop rejected a host symbol"
    );
    assert_eq!(
        host_tick_severed(&web),
        Some(Unrealizable::ClassUnsupported(
            instance_graph::BoundaryClass::Extern,
        )),
        "the web platform admitted a host symbol solely because luajit can call one"
    );

    let android = target(tp::PlatformSpec {
        kind: tp::PlatformKind::Android,
        ..tp::DESKTOP
    });
    let android = realize(&ws, &g, android, &roots);
    let host_tick = |view: &artifact::ArtifactView| {
        view.boundaries
            .iter()
            .find(|(i, _)| fn_name(&ws, i.func) == "host_tick")
            .map(|(_, id)| id.clone())
            .expect("host_tick boundary")
    };
    assert_ne!(
        host_tick(&desktop),
        host_tick(&android),
        "two platform contracts collapsed to one execution-backend identity"
    );
}

#[test]
fn shader_packaging_uses_the_desktop_source_profile() {
    use skull::target_projection as tp;

    assert_eq!(
        tp::LUAJIT_DESKTOP.shader,
        Some(tp::ShaderTargetSpec::Glsl(
            skull_codegen_glsl::GlslProfile::Core330,
        )),
        "shader packaging must carry Core330 source"
    );
    assert_eq!(
        tp::JAVASCRIPT_WEB.shader,
        tp::LUAJIT_DESKTOP.shader,
        "shader packaging must use the supported source profile"
    );
}

/// A portable representative with a Lua sibling makes Lua select a sibling too.
const AXES: &str = r#"
extern(ffi, "host") fn host_tick(n: int) -> int
inline(lua) fn only_lua(x: int) -> int = "return x + 1"
fn portable_op(x: int) -> int { return x * 3 }
inline(lua) fn portable_op(x: int) -> int = "return x * 3"
fn ident<T>(x: T) -> T { return x }
fn main() {
    let b: NativeBuffer<int> = nb_new(4)
    let x = b[0]
    let v: [Vec2] = []
    let l = v.to_lua_array()
    host_tick(only_lua(portable_op(ident(1))))
    let s = ident("s")
}
"#;

#[test]
fn shader_packaging_and_target_name_do_not_decide_cpu_realization() {
    use skull::target_projection as tp;
    let ws = ws_with_stdlib(AXES);
    let (g, roots) = graph_for(&ws);
    let view = |spec: tp::TargetSpec| realize(&ws, &g, spec, &roots);

    let base = view(tp::LUAJIT_DESKTOP);

    // NON-VACUITY, first: the thing being compared has to be able to differ.
    // A view that decided nothing would satisfy every equality below.
    assert!(
        !base.instances.is_empty(),
        "the fixture retained no Rock instance"
    );
    assert!(
        !base.boundaries.is_empty(),
        "the fixture retained no boundary"
    );
    assert!(
        !base.inline_deps.is_empty(),
        "the fixture resolved no opaque dependency"
    );
    assert!(
        base.realization.iter().any(|(i, r)| r.decl != i.func),
        "no instance selected a backend sibling, so declaration equality is vacuous"
    );

    // ...and a DIFFERENT execution axis really does change the answer. Without
    // this, "packaging changes nothing" could just mean nothing changes
    // anything.
    let js = view(tp::JAVASCRIPT_WEB);
    assert!(
        !base.same_cpu_decisions(&js),
        "changing the EXECUTION axis changed nothing, so the equalities below \
         are measuring an inert function"
    );
    assert!(
        !js.severed.is_empty(),
        "the fixture severs nothing on JS, so severed classifications are not \
         part of what is being compared"
    );

    // Composed here rather than declared as fixtures: a packaging variation is
    // a field change, not a new target variant.
    let variants = [
        (
            "dropping the shader axis",
            tp::TargetSpec {
                name: "luajit-desktop-no-shader",
                shader: None,
                ..tp::LUAJIT_DESKTOP
            },
        ),
        (
            "renaming the artifact",
            tp::TargetSpec {
                name: "\u{0}not-a-target",
                ..tp::LUAJIT_DESKTOP
            },
        ),
    ];
    for (label, spec) in variants {
        assert!(
            base.same_cpu_decisions(&view(spec)),
            "{label} changed a CPU decision"
        );
    }
}

const REALIZATION_KINDS: &str = r#"
@intrinsic inline(lua) fn mapped_probe(x: int) -> int = "return x * 2"
@struct_expand inline(lua) fn expand_probe(x: int) -> int = "return 0"
inline(lua) fn bound_probe(x: int) -> int = "return x + 1"
extern(ffi, "host") fn host_probe(n: int) -> int
fn rock_bound(x: int) -> int { return rock_bound_callee(x) }
fn rock_bound_callee(x: int) -> int { return x + 1 }
fn main() {
    let a = mapped_probe(1)
    let b = expand_probe(2)
    let c = bound_probe(3)
    let d = host_probe(4)
    let e = rock_bound(5)
}
"#;

/// Target realization accounts for native mappings, compile-time expansion,
/// and host bindings before deciding which Rock bodies to traverse.
#[test]
fn target_realization_uses_mappings_and_stops_at_boundaries() {
    let ws = ws_with_stdlib(REALIZATION_KINDS);
    let (g, all_roots) = graph_for(&ws);
    let roots = main_roots(&ws, &all_roots);
    assert_eq!(roots.len(), 1, "the fixture has no unique `main` root");
    let kind = |view: &artifact::ArtifactView, name: &str| realized_kind(&ws, view, name);
    let severed = |view: &artifact::ArtifactView, name: &str| severed_cause(&ws, view, name);

    let js = realize(&ws, &g, JAVASCRIPT_WEB, &roots);
    for name in ["mapped_probe", "bound_probe", "host_probe"] {
        assert_eq!(
            severed(&js, name),
            Some(Unrealizable::ForeignToArtifact),
            "unmapped JS call to `{name}` was not severed"
        );
    }
    assert_eq!(kind(&js, "mapped_probe"), None);
    assert_eq!(
        kind(&js, "expand_probe"),
        Some(artifact::Realization::CompileTimeExpansion),
        "the JS artifact asked for a backend body for an expansion that has none"
    );
    assert_eq!(
        severed(&js, "expand_probe"),
        None,
        "an expansion the artifact performs is still a severed call"
    );
    assert_eq!(
        kind(&js, "rock_bound"),
        Some(artifact::Realization::RockBody)
    );
    assert_eq!(
        kind(&js, "rock_bound_callee"),
        Some(artifact::Realization::RockBody),
        "the unbound Rock body did not reach its callee"
    );

    let bindings = Supplies(&["mapped_probe", "bound_probe", "host_probe", "rock_bound"]);
    let js_bound = realize_supplied(&ws, &g, JAVASCRIPT_WEB, &roots, FnBackend::Js, &bindings);
    for (name, expected) in [
        ("mapped_probe", artifact::Realization::NativeMapping),
        ("expand_probe", artifact::Realization::CompileTimeExpansion),
        ("bound_probe", artifact::Realization::HostBinding),
        ("host_probe", artifact::Realization::HostBinding),
        ("rock_bound", artifact::Realization::HostBinding),
    ] {
        assert_eq!(
            kind(&js_bound, name),
            Some(expected),
            "wrong realization for `{name}`"
        );
    }
    for name in ["mapped_probe", "bound_probe", "host_probe", "rock_bound"] {
        assert_eq!(
            severed(&js_bound, name),
            None,
            "JS binding for `{name}` left the call severed"
        );
    }

    for name in ["bound_probe", "rock_bound"] {
        let instance = js_bound
            .realization
            .keys()
            .find(|i| fn_name(&ws, i.func) == name)
            .unwrap_or_else(|| panic!("{name} was not retained"))
            .clone();
        assert!(
            js_bound.boundaries.contains_key(&instance),
            "{name} is not a boundary"
        );
        assert!(
            !js_bound.instances.contains(&instance),
            "{name} was traversed as Rock"
        );
    }
    assert_eq!(
        kind(&js_bound, "rock_bound_callee"),
        None,
        "the walk entered a body the artifact's own binding replaces"
    );

    let lua = realize(&ws, &g, LUAJIT_DESKTOP, &roots);
    assert_eq!(
        kind(&lua, "mapped_probe"),
        Some(artifact::Realization::NativeMapping)
    );
    assert_eq!(
        kind(&lua, "expand_probe"),
        Some(artifact::Realization::CompileTimeExpansion)
    );
    assert_eq!(
        kind(&lua, "bound_probe"),
        Some(artifact::Realization::ForeignBody(
            skull::instance_graph::BoundaryClass::InlineBody
        )),
        "without a binding the Lua artifact must select the declaration's body"
    );

    let lua_bound = realize_supplied(&ws, &g, LUAJIT_DESKTOP, &roots, FnBackend::Lua, &bindings);
    assert_eq!(
        kind(&lua_bound, "bound_probe"),
        Some(artifact::Realization::HostBinding),
        "the artifact emitted a body its own binding replaces"
    );
}

#[test]
fn the_js_view_retains_reachable_inline_js_bodies_with_their_lanes() {
    let ws = ws_with_stdlib(
        r#"
inline(js) fn reached_pair(v: float) -> (bool, float) = "return __rock_publish(false, v);"
inline(js) fn reached_scalar(v: float) -> float = "return v;"
inline(js) fn never_reached(v: float) -> (bool, float) = "return __rock_publish(false, v);"
fn main() {
    let (ok, f) = reached_pair(1.0)
    let s = reached_scalar(2.0)
}
"#,
    );
    let (g, all_roots) = graph_for(&ws);
    // The artifact's roots are narrower than the graph's, which is plan S 11's
    // "the target may change roots": every declaration is an emission root of
    // the shared graph, so an unreachable control needs the view to walk from
    // `main` alone.
    let roots = main_roots(&ws, &all_roots);
    let js = realize(&ws, &g, JAVASCRIPT_WEB, &roots);
    let lanes = |name: &str| {
        js.publisher_lanes
            .iter()
            .find(|(i, _)| fn_name(&ws, i.func) == name)
            .map(|(_, n)| *n)
    };

    assert_eq!(
        lanes("reached_pair"),
        Some(2),
        "a reachable 2-lane publisher lost its width"
    );
    assert_eq!(lanes("reached_scalar"), Some(1));
    // The UNREACHABLE control: retention is a walk, so a body no root reaches
    // carries no obligation -- and its absence here is what says the two lane
    // counts above are a property of reachability rather than of declaration.
    assert_eq!(
        lanes("never_reached"),
        None,
        "an unreachable body was given a lane count"
    );
    assert!(
        js.excluded.iter().any(|e| {
            fn_name(&ws, e.instance.func) == "never_reached" && e.cause == Exclusion::Unreachable
        }),
        "the unreachable body is missing from the exclusion partition"
    );

    // The LUAJIT artifact over the same graph selects Lua bodies, so these JS
    // declarations are not what it measures -- the lanes belong to the artifact,
    // not to the shared instance.
    let lua = realize(&ws, &g, LUAJIT_DESKTOP, &roots);
    assert!(
        !lua.publisher_lanes
            .keys()
            .any(|i| { fn_name(&ws, i.func) == "reached_pair" }),
        "the luajit artifact claimed a JS-only body's result wire"
    );
}

/// Use both a scalar and a tuple to require two widths from the same declaration.
#[test]
fn a_generic_foreign_body_takes_its_publisher_width_from_its_instance() {
    let ws = ws_with_stdlib(
        r#"
inline(js) fn wired<T>(v: T) -> T = "return v;"
fn main() {
    let pair = wired((true, 1.0))
    let one = wired(7)
}
"#,
    );
    let (g, all_roots) = graph_for(&ws);
    let roots = main_roots(&ws, &all_roots);
    let js = realize(&ws, &g, JAVASCRIPT_WEB, &roots);

    // Keyed by the instance's own type argument, so the assertion names WHICH
    // instantiation got which width rather than only that two numbers appeared.
    let widths: std::collections::BTreeMap<String, usize> = js
        .publisher_lanes
        .iter()
        .filter(|(i, _)| fn_name(&ws, i.func) == "wired")
        .map(|(i, n)| (g.interner().render(i.args[0]), *n))
        .collect();

    // The DENOMINATOR: the fixture has a real generic population, so a zero
    // below would be an unanswered question and not a clean one.
    let generic_instances = js
        .realization
        .keys()
        .filter(|i| fn_name(&ws, i.func) == "wired")
        .count();
    assert_eq!(
        generic_instances, 2,
        "the fixture lost one of its two instantiations"
    );

    assert_eq!(
        widths,
        [
            ("(bool, float)".to_string(), 2usize),
            ("int".to_string(), 1usize)
        ]
        .into_iter()
        .collect::<std::collections::BTreeMap<_, _>>(),
        "the two instances of one generic foreign body did not state their own widths"
    );
    assert!(
        js.missing_substitutions.is_empty(),
        "a generic instance went unanswered for a reason other than its own width"
    );
}

const PUBLICATION_FIXTURE: &str = r#"
@nomangle fn portable(x: int) -> int { return x + 1 }
inline(lua) fn portable(x: int) -> int = """
    return x + 2
"""
fn caller() -> int { return portable(5) }

fn plain(x: int) -> int { return x + 1 }
inline(lua) fn plain(x: int) -> int = """
    return x + 2
"""
fn other_caller() -> int { return plain(5) }

fn main() { print("{caller()} {other_caller()}") }
"#;

/// The compact fixture: an `@nomangle` portable declaration with a same-name
/// `inline(lua)` sibling, plus an ordinary call, plus a sibling control (the
/// SAME shape with no `@nomangle`) that must not acquire a publication.
#[test]
fn a_nomangle_root_publishes_its_own_body_beside_the_calls_sibling_serves() {
    let ws = ws_with_stdlib(PUBLICATION_FIXTURE);
    let (g, roots) = graph_for(&ws);
    let lua = realize(&ws, &g, LUAJIT_DESKTOP, &roots);

    // Nonzero ordinary-call population: the artifact retains real Rock work.
    assert!(
        !lua.instances.is_empty(),
        "the fixture retained no ordinary Rock instances at all"
    );
    // Nonzero exact-publication population: the `@nomangle` root's promise is
    // modeled at all.
    assert!(
        !lua.publications.is_empty(),
        "the `@nomangle` root produced no publication obligation"
    );
    // `portable` specifically is represented (the stdlib carries its own
    // `@nomangle` declarations, some of them GENERIC -- a generic declaration
    // has no concrete base `Instance` at all in this model, which is a real,
    // separate, expected member of `unrepresented_publications` and not what
    // this fixture is testing).
    assert!(
        !lua.unrepresented_publications
            .iter()
            .any(|r| fn_name(&ws, *r) == "portable"),
        "portable's own declaration should be its group's representative (Any \
         is the lowest backend rank): {:?}",
        lua.unrepresented_publications
    );

    // The ordinary call resolves through the Lua sibling: `caller`'s realized
    // callee is the `inline(lua)` declaration, not the `@nomangle` one.
    let nomangle_decl = ws
        .modules
        .iter()
        .find_map(|m| {
            m.functions
                .iter()
                .find(|(_, f)| name_matches(f.name, "portable") && is_nomangle(f))
                .map(|(func, _)| FuncRef { module: m.id, func })
        })
        .expect("the @nomangle declaration exists");
    let portable_canonical = lua
        .realization
        .keys()
        .find(|i| fn_name(&ws, i.func) == "portable")
        .cloned()
        .expect("the canonical `portable` instance is retained");
    assert_eq!(
        backend(&ws, lua.realization[&portable_canonical].decl),
        FnBackend::Lua,
        "the ordinary call to `portable` must be served by the Lua sibling, not the \
         @nomangle Any-backed declaration"
    );
    assert_ne!(
        lua.realization[&portable_canonical].decl, nomangle_decl,
        "call-serving realization selected the @nomangle declaration itself, not its sibling"
    );

    // The bare `@nomangle` Rock definition remains a publication obligation for
    // its OWN declaration -- the canonical Instance IS the @nomangle instance
    // here (Any is always the lowest backend rank), so this is the SAME key as
    // `portable_canonical` above, naming a DIFFERENT thing (its own body, not
    // whichever declaration serves calls).
    assert_eq!(
        lua.publications
            .get(&portable_canonical)
            .map(String::as_str),
        Some("portable"),
        "the @nomangle root's own bare-name publication is missing or misnamed: {:?}",
        lua.publications
    );

    // The sibling control (same shape, no `@nomangle`) must NOT acquire a
    // publication obligation -- ordinary same-name siblings stay exactly as
    // sibling-collapse always treated them.
    let plain_canonical = lua
        .realization
        .keys()
        .find(|i| fn_name(&ws, i.func) == "plain")
        .cloned()
        .expect("the canonical `plain` instance is retained");
    assert!(
        !lua.publications.contains_key(&plain_canonical),
        "a same-name sibling pair with no @nomangle root acquired a publication obligation"
    );
    assert!(
        lua.publication_collisions.is_empty(),
        "the compact fixture has no colliding publication pair: {:?}",
        lua.publication_collisions
    );
}

/// Two INDEPENDENT `@nomangle` declarations sharing one bare name (different
/// signatures, so `backend_variant_group` never groups them -- this collision
/// is not routed through sibling selection at all) must REJECT rather than let
/// one publication silently overwrite the other.
const PUBLICATION_COLLISION_FIXTURE: &str = r#"
@nomangle fn dup(x: int) -> int { return x }
@nomangle fn dup(x: string) -> string { return x }
fn main() { print("{dup(1)} {dup(\"s\")}") }
"#;

/// The collision rows then retract and reinstall with the root that causes
/// them, matching a fresh view each time.
#[test]
fn two_nomangle_roots_sharing_a_bare_name_collide_and_retract_locally() {
    let ws = ws_with_stdlib(PUBLICATION_COLLISION_FIXTURE);
    let (g, roots) = graph_for(&ws);
    let mut local = realize(&ws, &g, LUAJIT_DESKTOP, &roots);
    assert_eq!(
        local.publication_collisions.len(),
        1,
        "two independent @nomangle roots named `dup` did not collide: {:?}",
        local.publication_collisions
    );
    let collision = &local.publication_collisions[0];
    assert_eq!(collision.symbol, "dup");
    assert_eq!(
        collision.instances.len(),
        2,
        "the collision did not name both instances"
    );
    // Neither colliding instance is silently admitted as a resolved publication
    // -- a reject beats a last-writer-wins guess.
    for i in &collision.instances {
        assert!(
            !local.publications.contains_key(i),
            "a colliding publication was admitted anyway: {i:?}"
        );
    }

    let removed_root = roots
        .iter()
        .copied()
        .find(|root| {
            let f = &ws.module(root.module).functions[root.func];
            name_matches(f.name, "dup") && is_nomangle(f) && f.generics.is_empty()
        })
        .expect("fixture's @nomangle root");
    let narrowed: Vec<_> = roots
        .iter()
        .copied()
        .filter(|root| *root != removed_root)
        .collect();
    let fresh = realize(&ws, &g, LUAJIT_DESKTOP, &narrowed);
    local.update_local(
        &ws,
        &g,
        &ArtifactRequest::new(LUAJIT_DESKTOP, &narrowed),
        &Default::default(),
        &Default::default(),
        &Default::default(),
    );
    assert_eq!(
        local.publications, fresh.publications,
        "publication rows were not retracted locally"
    );
    assert_eq!(
        local.publication_collisions, fresh.publication_collisions,
        "collision rows went stale"
    );
    assert_eq!(
        local.unrepresented_publications,
        fresh.unrepresented_publications
    );
    assert_eq!(
        local.symbol_collisions, fresh.symbol_collisions,
        "symbol rows changed under a publication delta"
    );
    let restored = realize(&ws, &g, LUAJIT_DESKTOP, &roots);
    local.update_local(
        &ws,
        &g,
        &ArtifactRequest::new(LUAJIT_DESKTOP, &roots),
        &Default::default(),
        &Default::default(),
        &Default::default(),
    );
    assert_eq!(
        local.publications, restored.publications,
        "publication installation was lost"
    );
    assert_eq!(
        local.publication_collisions, restored.publication_collisions,
        "collision installation was lost"
    );
}

/// The user-module body named `name`: a function, else a global.
fn user_body(ws: &Workspace, name: &str) -> (ModuleId, BodyOwner) {
    let module = module_id(ws, "<test>");
    let m = ws.module(module);
    m.functions
        .iter()
        .find(|(_, f)| name_matches(f.name, name))
        .map(|(fid, _)| BodyOwner::Function(fid))
        .or_else(|| {
            m.globals
                .iter()
                .find(|(_, g)| name_matches(g.name, name))
                .map(|(gid, _)| BodyOwner::Global(gid))
        })
        .map(|owner| (module, owner))
        .unwrap_or_else(|| panic!("no user body `{name}`"))
}

struct LocalUpdate {
    ws: Workspace,
    local: artifact::ArtifactView,
    fresh: artifact::ArtifactView,
}

/// `v1`'s view, updated locally after the user body `changed` is edited to
/// `v2`, beside the view realized fresh from `v2`. Both views use the request
/// roots `pick` selects from the emission roots for `project`, which the edit
/// must leave unchanged.
fn update_locally(
    v1: &str,
    v2: &str,
    changed: &str,
    project: Option<&str>,
    pick: fn(&Workspace, &[FuncRef]) -> Vec<FuncRef>,
    symbols: Option<&'static dyn artifact::SymbolPlanner>,
    check_v1: fn(&Workspace, &artifact::ArtifactView),
) -> LocalUpdate {
    let request = |roots| {
        let request = ArtifactRequest::new(LUAJIT_DESKTOP, roots);
        match symbols {
            Some(planner) => request.with_symbols(planner),
            None => request,
        }
    };
    let ws1 = ws_with_stdlib(v1);
    let roots1 = skull::mir_emit::emission_roots(&ws1, project);
    let request_roots = pick(&ws1, &roots1);
    let mut graph = instance_graph::build(&ws1, roots1, false);
    let mut local = artifact::realize(&ws1, &graph, &request(&request_roots));
    check_v1(&ws1, &local);

    let ws2 = ws_with_stdlib(v2);
    local.prepare_update(&graph);
    let update =
        graph.update_bodies_with_root_delta(&ws2, &[], &[], &[user_body(&ws2, changed)], &[]);
    let roots2 = skull::mir_emit::emission_roots(&ws2, project);
    assert_eq!(
        pick(&ws2, &roots2),
        request_roots,
        "the edit moved a request root"
    );
    let fresh_graph = instance_graph::build(&ws2, roots2, false);
    let fresh = artifact::realize(&ws2, &fresh_graph, &request(&request_roots));
    local.update_local(
        &ws2,
        &graph,
        &request(&request_roots),
        &update.revisited,
        &update.added,
        &update.removed,
    );
    LocalUpdate {
        ws: ws2,
        local,
        fresh,
    }
}

fn all_roots(_: &Workspace, roots: &[FuncRef]) -> Vec<FuncRef> {
    roots.to_vec()
}

#[test]
fn local_symbol_collision_retraction_matches_fresh_view() {
    let LocalUpdate { local, fresh, .. } = update_locally(
        "fn left() -> int { return 1 }\nfn right() -> int { return 2 }\n\
         fn main() { let a = left()\n let b = right()\n print(\"{a}{b}\") }",
        "fn left() -> int { return 1 }\nfn right() -> int { return 2 }\n\
         fn main() { let a = left()\n print(\"{a}\") }",
        "main",
        None,
        main_roots,
        Some(&MAIN_RIGHT_COLLISION),
        |_, view| {
            assert_eq!(
                view.symbol_collisions.len(),
                1,
                "fixture did not populate a symbol collision"
            );
        },
    );
    assert_eq!(
        local.symbol_collisions, fresh.symbol_collisions,
        "symbol collision rows went stale"
    );
    assert!(
        local.symbol_collisions.is_empty(),
        "the removed owner kept a collision row"
    );
}

#[test]
fn local_inline_foreign_dependency_retraction_matches_fresh_view() {
    let LocalUpdate { local, fresh, .. } = update_locally(
        r#"
inline(lua) fn foreign_dep(x: int) -> int = """
    return __rock_vec_column_elements({ len = 1 }, x)
"""
fn caller() -> int {
    return foreign_dep(1)
}
fn main() { let x = caller() }
"#,
        r#"
inline(lua) fn foreign_dep(x: int) -> int = """
    return __rock_vec_column_elements({ len = 1 }, x)
"""
fn caller() -> string { return "unchanged" }
fn main() { let x = caller() }
"#,
        "caller",
        None,
        main_roots,
        None,
        |_, view| {
            assert!(
                !view.inline_deps.is_empty() || !view.unresolved_inline_deps.is_empty(),
                "inline foreign-dependency fixture had no retained dependency"
            );
        },
    );
    assert_eq!(
        local.inline_deps, fresh.inline_deps,
        "inline dependency rows went stale"
    );
    assert_eq!(
        local.unresolved_inline_deps, fresh.unresolved_inline_deps,
        "inline unresolved rows went stale"
    );
}

// The update rows describe the graph the view was realized from, so deriving
// them from a graph that has since moved is refused rather than guessed.
#[test]
#[should_panic(expected = "call `prepare_update` before updating the graph")]
fn artifact_update_rows_refuse_a_graph_updated_first() {
    let ws1 = ws_with_stdlib(GLOBAL_FN_VALUE_V1);
    let roots = skull::mir_emit::emission_roots(&ws1, Some("/project"));
    let mut graph = instance_graph::build(&ws1, roots.clone(), false);
    let request = ArtifactRequest::new(LUAJIT_DESKTOP, &roots);
    let mut local = artifact::realize(&ws1, &graph, &request);
    let ws2 = ws_with_stdlib(GLOBAL_FN_VALUE_V2);
    let update = graph.update_bodies_with_root_delta(&ws2, &[], &[], &[user_body(&ws2, "F")], &[]);
    local.update_local(
        &ws2,
        &graph,
        &request,
        &update.revisited,
        &update.added,
        &update.removed,
    );
}

fn fn_value_target_names(ws: &Workspace, view: &artifact::ArtifactView) -> Vec<String> {
    let mut names: Vec<String> = view
        .fn_value_targets
        .iter()
        .map(|i| fn_name(ws, i.func).to_string())
        .collect();
    names.sort();
    names
}

const GLOBAL_FN_VALUE_V1: &str = "fn inc(x: int) -> int { return x + 1 }\n\
fn dec(x: int) -> int { return x - 1 }\n\
global F: fn(int) -> int = inc\n\
fn main() { print(\"{F(1)} {dec(1)}\") }";
const GLOBAL_FN_VALUE_V2: &str = "fn inc(x: int) -> int { return x + 1 }\n\
fn dec(x: int) -> int { return x - 1 }\n\
global F: fn(int) -> int = dec\n\
fn main() { print(\"{F(1)} {dec(1)}\") }";

// A value taken by a global initializer is recorded without a caller instance.
#[test]
fn a_global_initializer_fn_value_target_follows_a_local_retarget() {
    let LocalUpdate {
        ws, local, fresh, ..
    } = update_locally(
        GLOBAL_FN_VALUE_V1,
        GLOBAL_FN_VALUE_V2,
        "F",
        Some("/project"),
        all_roots,
        None,
        |ws, view| assert_eq!(fn_value_target_names(ws, view), ["inc"]),
    );
    assert_eq!(fn_value_target_names(&ws, &fresh), ["dec"]);
    assert_eq!(local.fn_value_targets, fresh.fn_value_targets);
}

#[test]
fn repeated_fn_value_sites_retarget_without_losing_other_callers() {
    for shared in [false, true] {
        let source = |target| {
            format!(
                r#"
fn inc(x: int) -> int {{ return x + 1 }}
fn dec(x: int) -> int {{ return x - 1 }}
fn keep() -> int {{ let f = inc; return f(0) }}
fn caller() {{
    let f = {target}
    let g = {target}
    print("{{f(1)}} {{g(2)}}")
}}
fn main() {{
    caller()
    print("{{inc(0)}} {{dec(0)}}")
    {}
}}
"#,
                if shared { "print(\"{keep()}\")" } else { "" }
            )
        };
        let LocalUpdate {
            ws, local, fresh, ..
        } = update_locally(
            &source("inc"),
            &source("dec"),
            "caller",
            None,
            main_roots,
            None,
            |ws, view| assert_eq!(fn_value_target_names(ws, view), ["inc"]),
        );
        let expected = if shared {
            vec!["dec", "inc"]
        } else {
            vec!["dec"]
        };
        assert_eq!(fn_value_target_names(&ws, &fresh), expected);
        assert_eq!(
            local.fn_value_targets, fresh.fn_value_targets,
            "shared={shared}"
        );
    }
}

#[test]
fn successive_fn_value_updates_match_fresh_views_and_bindings() {
    let workspace = |source: &str| {
        crate::common::ws_with_stdlib_module(
            r#"
@pure @inline
inline(lua) fn inc(x: int) -> int = "return x + 1"
@pure @inline
inline(lua) fn dec(x: int) -> int = "return x - 1"
"#,
            source,
        )
    };
    for initializer in [false, true] {
        let source = |keep: &str, caller: &str, dependency: &str, argument: usize| {
            format!(
                r#"
fn dependency() -> int {{ {dependency} }}
fn keep() -> int {{ let f = {keep}; return f(dependency()) }}
fn caller() -> int {{ let f = {caller}; let g = {caller}; return f({argument}) + g(2) }}
{}
fn main() {{ print("{{keep()}} {{caller()}} {{inc(0)}} {{dec(0)}}") }}
"#,
                if initializer {
                    "global F: fn(int) -> int = inc"
                } else {
                    ""
                }
            )
        };
        let ws = workspace(&source("inc", "inc", "return 0", 1));
        let roots = skull::mir_emit::emission_roots(&ws, None);
        let request_roots = main_roots(&ws, &roots);
        let mut graph = instance_graph::build(&ws, roots, false);
        let request = ArtifactRequest::new(LUAJIT_DESKTOP, &request_roots);
        let mut local = artifact::realize(&ws, &graph, &request);
        assert_eq!(fn_value_target_names(&ws, &local), ["inc"]);
        let mid = module_id(&ws, "<test>");
        let (_, mut cache) = skull::mir_emit::emit_full(&ws, mid).expect("initial emit");
        for (keep, caller, dependency, argument, changed) in [
            ("inc", "inc", "return dec(0)", 1, "dependency"),
            ("inc", "dec", "return dec(0)", 1, "caller"),
            ("dec", "dec", "return dec(0)", 1, "keep"),
            ("dec", "dec", "return dec(0)", 3, "caller"),
            ("dec", "dec", "return dec(0)", 4, "caller"),
            ("dec", "inc", "return dec(0)", 1, "caller"),
            ("inc", "inc", "return dec(0)", 1, "keep"),
            ("inc", "inc", "return 1", 1, "dependency"),
        ] {
            let ws = workspace(&source(keep, caller, dependency, argument));
            let edited = [user_body(&ws, changed)];
            local.prepare_update(&graph);
            let update = graph.update_bodies_with_root_delta(&ws, &[], &[], &edited, &[]);
            local.update_local(
                &ws,
                &graph,
                &request,
                &update.revisited,
                &update.added,
                &update.removed,
            );
            let fresh_graph =
                instance_graph::build(&ws, skull::mir_emit::emission_roots(&ws, None), false);
            let fresh = artifact::realize(&ws, &fresh_graph, &request);
            let expected = if initializer || keep == "inc" || caller == "inc" {
                if keep == "dec" || caller == "dec" {
                    vec!["dec", "inc"]
                } else {
                    vec!["inc"]
                }
            } else {
                vec!["dec"]
            };
            assert_eq!(fn_value_target_names(&ws, &fresh), expected);
            let incremental = skull::mir_emit::emit_incremental(&ws, &mut cache, &edited)
                .expect("incremental emit");
            let (fresh_emit, _) = skull::mir_emit::emit_full(&ws, mid).expect("fresh emit");
            assert_eq!(
                incremental.lua, fresh_emit.lua,
                "initializer={initializer}, changed={changed}"
            );
            assert_eq!(
                local.fn_value_targets, fresh.fn_value_targets,
                "initializer={initializer}, changed={changed}, keep={keep}, caller={caller}, argument={argument}"
            );
        }
    }
}
