//! The cached JS build keeps one `MonoInterner`, `InstanceGraph`, and
//! `ArtifactView` across full emission and overlay edits. The graph delta arms
//! below prove addition, removal, and reintroduction against a nonzero
//! population.

use crate::common::ws_with;
use bone_semantic::hir::BodyOwner;
use skull_mir::mir::FuncRef;

fn function(ws: &bone_semantic::inference::Workspace, name: &str) -> FuncRef {
    let mid = *ws
        .module_ids
        .get(&ustr::Ustr::from("<test>"))
        .expect("test module");
    let (fid, _) = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == name)
        .expect("function");
    FuncRef {
        module: mid,
        func: fid,
    }
}

fn instance_count(cache: &skull_codegen_js::emit::JsEmitCache, func: FuncRef) -> usize {
    cache
        .graph()
        .instances
        .iter()
        .filter(|instance| instance.func == func)
        .count()
}

/// The cached session keeps one arena while the retained graph is rebuilt.
#[test]
fn the_cache_keeps_one_arena_across_an_overlay_edit() {
    const BEFORE: &str = "\
fn pick<T>(x: T, y: T) -> T { x }\n\
fn other() -> float { pick(1.0, 2.0) }\n\
@nomangle fn hot() -> float { 1.0 }\n\
fn main() { print(hot() + other()) }\n";
    const AFTER: &str = "\
fn pick<T>(x: T, y: T) -> T { x }\n\
fn other() -> float { pick(1.0, 2.0) }\n\
@nomangle fn hot() -> float { 3.0 }\n\
fn main() { print(hot() + other()) }\n";

    let before = ws_with(BEFORE);
    let after = ws_with(AFTER);
    let mid = function(&before, "main").module;
    let pick = function(&before, "pick");
    let (_, mut cache) = skull_codegen_js::emit::emit_program_cached(&before, mid).unwrap();

    let handle = cache.interner_handle();
    assert!(
        handle.len() > 0,
        "the shared arena has no interned concrete types"
    );
    assert_eq!(
        instance_count(&cache, pick),
        1,
        "one generic instantiation was retained"
    );
    let type_population = handle.len();

    let hot = function(&after, "hot");
    skull_codegen_js::emit::emit_overlay(
        &after,
        mid,
        &mut cache,
        &[(hot.module, BodyOwner::Function(hot.func))],
    )
    .unwrap()
    .expect_overlay();

    assert!(
        std::sync::Arc::ptr_eq(&handle, &cache.interner_handle()),
        "the cache must keep ONE arena across the edit"
    );
    assert_eq!(
        cache.interner_handle().len(),
        type_population,
        "the edit added no type"
    );
    assert_eq!(
        instance_count(&cache, pick),
        1,
        "the edit kept one generic instantiation"
    );
}

#[test]
fn an_instance_adding_edit_is_served_by_the_overlay() {
    const BEFORE: &str = "\
fn pick<T>(x: T, y: T) -> T { x }\n\
@nomangle fn hot() -> float { pick(1.0, 2.0) }\n\
fn main() { print(hot()) }\n";
    const AFTER: &str = "\
fn pick<T>(x: T, y: T) -> T { x }\n\
@nomangle fn hot() -> float {\n  let n = pick(1, 2)\n  pick(1.0, 2.0) + int_to_float(n)\n}\n\
fn main() { print(hot()) }\n";

    let before = ws_with(BEFORE);
    let after = ws_with(AFTER);
    let mid = function(&before, "main").module;
    let pick = function(&before, "pick");
    let (_, mut cache) = skull_codegen_js::emit::emit_program_cached(&before, mid).unwrap();
    assert!(
        cache.interner_handle().len() > 0,
        "the shared arena has no concrete types"
    );
    assert_eq!(
        instance_count(&cache, pick),
        1,
        "one instantiation before the edit"
    );
    let initial_population = cache.graph_population();
    assert!(
        initial_population > 0,
        "the full build must retain graph instances"
    );

    let hot = function(&after, "hot");
    let reload = skull_codegen_js::emit::emit_overlay(
        &after,
        mid,
        &mut cache,
        &[(hot.module, BodyOwner::Function(hot.func))],
    )
    .unwrap();
    assert!(
        matches!(reload, skull_codegen_js::emit::JsReload::Overlay(_)),
        "an edit that adds a retained instance must produce an overlay"
    );
    assert_eq!(
        instance_count(&cache, pick),
        2,
        "the edit must add an instance"
    );
    let added = cache.graph_update().expect("graph update after addition");
    assert!(added.added > 0, "the edit must add a graph instance");
    assert_eq!(
        added.added_instances.len(),
        added.added,
        "the published addition population must carry the exact instances"
    );
    assert!(
        added
            .added_instances
            .iter()
            .all(|instance| cache.graph().instances.contains(instance)),
        "every published added instance must remain live in the session graph"
    );
    assert_eq!(
        added.removed, 0,
        "the addition arm must not remove the old instance"
    );

    const REMOVED: &str = "\
fn pick<T>(x: T, y: T) -> T { x }\n\
@nomangle fn hot() -> float { pick(1.0, 2.0) }\n\
fn main() { print(hot()) }\n";
    let removed_ws = ws_with(REMOVED);
    let removed_hot = function(&removed_ws, "hot");
    let removal = skull_codegen_js::emit::emit_overlay(
        &removed_ws,
        mid,
        &mut cache,
        &[(removed_hot.module, BodyOwner::Function(removed_hot.func))],
    )
    .unwrap();
    assert!(matches!(
        removal,
        skull_codegen_js::emit::JsReload::Overlay(_)
    ));
    let removed = cache.graph_update().expect("graph update after removal");
    assert!(removed.removed > 0, "the edit must remove the int instance");
    assert_eq!(
        removed.removed_instances.len(),
        removed.removed,
        "the published removal population must carry the exact instances"
    );
    assert!(
        removed
            .removed_instances
            .iter()
            .all(|instance| !cache.graph().instances.contains(instance)),
        "every published removed instance must leave the session graph"
    );
    assert_eq!(removed.added, 0, "the removal arm must not add an instance");
    assert_eq!(
        instance_count(&cache, pick),
        1,
        "the removed instance must leave the graph"
    );
    let removal_population = cache.graph_population();
    assert!(
        removal_population < added.after,
        "the removed instance must leave the graph"
    );

    let reintroduced_hot = function(&after, "hot");
    let reintroduced = skull_codegen_js::emit::emit_overlay(
        &after,
        mid,
        &mut cache,
        &[(
            reintroduced_hot.module,
            BodyOwner::Function(reintroduced_hot.func),
        )],
    )
    .unwrap();
    assert!(matches!(
        reintroduced,
        skull_codegen_js::emit::JsReload::Overlay(_)
    ));
    let readded = cache
        .graph_update()
        .expect("graph update after reintroduction");
    assert!(
        readded.added > 0,
        "reintroduction must rebuild the removed instance"
    );
    assert_eq!(readded.added_instances.len(), readded.added);
    assert!(readded.removed_instances.is_empty());
    assert_eq!(readded.removed, 0, "reintroduction must not hide a removal");
    assert_eq!(
        instance_count(&cache, pick),
        2,
        "reintroduction must restore the instance"
    );
    assert!(
        cache.graph_population() > removal_population,
        "reintroduction must grow the live graph population"
    );
}

#[test]
fn every_js_production_route_enters_the_shared_session() {
    let source = include_str!("../src/emit.rs");
    for (route, seam) in [
        ("pub fn emit_with_closure(", "new_js_session("),
        ("pub fn emit_program(", "build_program_session("),
        ("pub fn emit_program_cached(", "build_program_session("),
        ("pub fn emit_overlay(", "update_js_session("),
    ] {
        let start = source.find(route).expect("production JS route");
        let tail = &source[start..];
        assert!(tail.contains(seam), "{route} must enter {seam}");
    }
    assert_eq!(
        source.matches("fn js_scx(").count(),
        1,
        "one JS context constructor"
    );
    assert_eq!(
        source.matches("fn new_js_session(").count(),
        1,
        "one JS session constructor"
    );
    assert_eq!(
        source.matches("fn update_js_session(").count(),
        1,
        "one JS session updater"
    );
    assert!(
        source.contains("update_bodies_with_root_delta"),
        "the JS session must forward body edit metadata into graph repair"
    );
    assert!(
        source.contains("update.added") && source.contains("update.removed"),
        "the session must publish the graph's exact delta populations"
    );
    let overlay_start = source.find("pub fn emit_overlay(").unwrap();
    let overlay_end = source[overlay_start..]
        .find("\nfn collect_statics(")
        .map(|offset| overlay_start + offset)
        .expect("emit_overlay has a following private helper");
    let overlay = &source[overlay_start..overlay_end];
    assert!(
        overlay.contains("added_instances") && overlay.contains("removed_instances"),
        "the JS overlay must consume graph-added and graph-removed populations"
    );
    assert!(
        !overlay.contains("js_scx("),
        "overlay must retain the cached context"
    );
    assert!(
        !overlay.contains("new_js_session("),
        "overlay must retain the cached graph"
    );
}
