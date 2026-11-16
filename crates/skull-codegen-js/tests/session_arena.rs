//! The cached JS build keeps one `MonoInterner`, `InstanceGraph`, and
//! `ArtifactView` across full emission and overlay edits. The graph delta arms
//! below prove addition, removal, and reintroduction against a nonzero
//! population.

use crate::common::{replace_once, ws_with};
use bone_core::hir::BodyOwner;
use skull_mir::mir::FuncRef;

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
    const SOURCE: &str = "\
fn pick<T>(x: T, y: T) -> T { x }\n\
fn other() -> float { pick(1.0, 2.0) }\n\
@nomangle fn hot() -> float { 1.0 }\n\
fn main() { print(hot() + other()) }\n";

    let after_source = replace_once(SOURCE, "hot() -> float { 1.0 }", "hot() -> float { 3.0 }");
    let before = ws_with(SOURCE);
    let after = ws_with(&after_source);
    let mid = crate::common::test_fn_ref(&before, "main").module;
    let pick = crate::common::test_fn_ref(&before, "pick");
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

    let hot = crate::common::test_fn_ref(&after, "hot");
    crate::common::overlay_of_fn(&after, mid, &mut cache, hot);

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
    let mid = crate::common::test_fn_ref(&before, "main").module;
    let pick = crate::common::test_fn_ref(&before, "pick");
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
    let population = |cache: &skull_codegen_js::emit::JsEmitCache| cache.graph().instances.len();
    let initial_population = population(&cache);
    assert!(
        initial_population > 0,
        "the full build must retain graph instances"
    );

    let hot = crate::common::test_fn_ref(&after, "hot");
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
    let added_population = population(&cache);
    assert!(
        added_population > initial_population,
        "the edit must add a graph instance"
    );

    let removed_ws = ws_with(BEFORE);
    let removed_hot = crate::common::test_fn_ref(&removed_ws, "hot");
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
    assert_eq!(
        instance_count(&cache, pick),
        1,
        "the removed instance must leave the graph"
    );
    let removal_population = population(&cache);
    assert!(
        removal_population < added_population,
        "the removed instance must leave the graph"
    );

    let reintroduced_hot = crate::common::test_fn_ref(&after, "hot");
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
    assert_eq!(
        instance_count(&cache, pick),
        2,
        "reintroduction must restore the instance"
    );
    assert!(
        population(&cache) > removal_population,
        "reintroduction must grow the live graph population"
    );
}
