//! **The JS build session is DURABLE: full emit and overlay share one arena.**
//!
//! An `Instance` is an identity only relative to the `MonoInterner` that minted
//! its type ids. A cache that survives an edit while the overlay interns into a
//! FRESH arena therefore holds keys that mean nothing to the run answering, and
//! every `Instance`-keyed structure the emitter carries -- `CalleeNames`,
//! `spec_trees`, and the graph and artifact view Phase 3 puts here next --
//! would silently be two populations wearing one type.
//!
//! **What is observable today, and what is not.** Reuse alone has no emitted
//! consequence: re-interning a type is idempotent, so a shared arena and a
//! fresh one produce identical output. Nor can the two arms this property
//! really wants be written yet, and the sentinel below records why -- JS's
//! overlay puts specialized bodies in its STABLE section, so every edit that
//! changes the instance population, and every edit that flips a scalar ABI
//! (which changes a definition the stable section derives from), returns
//! `FullRequired` before an overlay exists to assert against. That is what
//! Phase 3C changes; until then this file pins the arena identity and the
//! reason the stronger arms are absent.

mod common;
use bone_semantic::hir::BodyOwner;
use common::ws_with;
use skull_mir::mir::FuncRef;

fn function(ws: &bone_semantic::inference::Workspace, name: &str) -> FuncRef {
    let mid = *ws.module_ids.get(&ustr::Ustr::from("<test>")).expect("test module");
    let (fid, _) = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == name)
        .expect("function");
    FuncRef { module: mid, func: fid }
}

/// The session arena survives the edit as ONE object.
///
/// **Stated with its own weakness**: this arm cannot fail on a fresh overlay
/// arena, because the cache's handle is unchanged whatever the overlay does. It
/// asserts that the cache OWNS a non-empty arena and keeps it across an edit;
/// that the overlay USES it is enforced by construction (`emit_overlay`
/// constructs no `ScalarCtx` of its own, so removing the reuse fails to
/// compile) rather than by this assertion. It becomes a real arm in Phase 3C,
/// when the graph and view this arena keys live in the cache beside it and the
/// sentinel below flips.
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
    let (_, mut cache) = skull_codegen_js::emit::emit_program_cached(&before, mid).unwrap();

    let handle = cache.interner_handle();
    // Non-vacuous: the arena holds the `pick<float>` instantiation, so this is a
    // real population and not an empty one any two arenas would share.
    assert_eq!(handle.len(), 1, "one generic instantiation was interned");

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
    assert_eq!(cache.interner_handle().len(), 1, "and the population it already had");
}

/// **SENTINEL -- flip when Phase 3C makes the JS overlay instance-aware.**
///
/// The natural discriminant for arena continuity is that an edit demanding a
/// NEW instantiation interns it in the arena the cache owns. That cannot be
/// written today: JS puts specialized bodies in the overlay's STABLE section,
/// so any edit changing the instance population fails the `stable` comparison
/// and returns `FullRequired` before an overlay is produced at all.
///
/// Asserting `FullRequired` pins the reason. When the overlay is driven by the
/// artifact view's added/removed instances rather than by a whole-program text
/// diff, this becomes `Overlay`, the arm above can assert real arena growth,
/// and the OTHER arm this file wants becomes writable too: an edit that flips a
/// `value` struct's scalar ABI, proving `emit_overlay`'s `update_body` loop
/// re-scans the edited body rather than reusing the full build's scan.
#[test]
fn an_instance_adding_edit_is_a_full_reload_today_sentinel() {
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
    let (_, mut cache) = skull_codegen_js::emit::emit_program_cached(&before, mid).unwrap();
    assert_eq!(cache.interner_handle().len(), 1, "one instantiation before the edit");

    let hot = function(&after, "hot");
    let reload = skull_codegen_js::emit::emit_overlay(
        &after,
        mid,
        &mut cache,
        &[(hot.module, BodyOwner::Function(hot.func))],
    )
    .unwrap();
    assert!(
        matches!(reload, skull_codegen_js::emit::JsReload::FullRequired),
        "specialized bodies live in the overlay's stable section, so an edit that \
         adds an instantiation is a full reload -- flip when Phase 3C changes that"
    );
}
