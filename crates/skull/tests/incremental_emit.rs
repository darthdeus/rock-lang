//! Oracle for skull incremental codegen (`docs/skull-incremental-emit-plan.md`):
//! after re-emitting only the bodies a (simulated) edit re-inferred, the assembled
//! Lua must be BYTE-IDENTICAL to a fresh full emit of the edited workspace. The
//! interesting case is editing an INLINED callee — its callers' chunks contain its
//! body, so they must re-emit too (the inline codegen-dep), even though a body edit
//! never re-infers the callers.

use bone_semantic::hir::BodyOwner;
use bone_semantic::inference::Workspace;
use bone_semantic::prelude::ModuleId;
use skull::mir_emit::{emit_full, emit_incremental, emit_overlay};
use crate::common::ws_of;

/// The `(ModuleId, BodyOwner)` reinferred-id for the named function in `ws`.
fn body_id(ws: &Workspace, name: &str) -> (ModuleId, BodyOwner) {
    let mid = skull::user_module_id(ws).unwrap();
    for (fid, f) in ws.module(mid).functions.iter() {
        if f.name.as_str() == name {
            return (mid, BodyOwner::Function(fid));
        }
    }
    panic!("no fn named {name}");
}

/// Drive one body edit through the cache and assert incremental == fresh.
fn check_edit(v1: &str, v2: &str, edited: &[&str]) {
    let ws1 = ws_of(v1);
    let mid = skull::user_module_id(&ws1).unwrap();
    let (_full1, mut cache) = emit_full(&ws1, mid).expect("emit_full v1");

    let ws2 = ws_of(v2);
    let mid2 = skull::user_module_id(&ws2).unwrap();
    let reinferred: Vec<_> = edited.iter().map(|n| body_id(&ws2, n)).collect();

    let incremental = emit_incremental(&ws2, &mut cache, &reinferred).expect("emit_incremental");
    let fresh = emit_full(&ws2, mid2).expect("emit_full v2").0;
    assert_eq!(
        incremental.lua, fresh.lua,
        "incremental assembly != fresh emit after editing {edited:?}"
    );
}

#[test]
fn body_edit_to_inlined_callee_reemits_callers() {
    // `helper` is a tiny straight callee -> inlined into `use_it`. Editing its
    // body must re-emit `use_it` (its chunk holds the inlined value), not just
    // `helper`. `other` is untouched and reused from cache.
    let v1 = "@inline\nfn helper() -> int { 3 }\n\
              fn use_it() -> int { helper() }\n\
              fn other() -> int { 7 }";
    let v2 = "@inline\nfn helper() -> int { 5 }\n\
              fn use_it() -> int { helper() }\n\
              fn other() -> int { 7 }";
    check_edit(v1, v2, &["helper"]);
}

#[test]
fn body_edit_to_leaf_only_reemits_itself() {
    let v1 = "@inline\nfn helper() -> int { 3 }\n\
              fn use_it() -> int { helper() }\n\
              fn other() -> int { 7 }";
    let v2 = "@inline\nfn helper() -> int { 3 }\n\
              fn use_it() -> int { helper() }\n\
              fn other() -> int { 9 }"; // only `other` changes
    check_edit(v1, v2, &["other"]);
}

#[test]
fn value_struct_ctor_edit_reemits_inliner() {
    // `mk` returns a `value` struct -> inlined into `use_v` as a scalarized
    // CallMulti (deps must capture CallMulti callees, not just plain Call).
    // Editing the ctor body must re-emit `use_v` with the new inlined leaves.
    let v1 = "value V2 { x: int, y: int }\n\
              @inline\nfn mk() -> V2 { V2 { x: 1, y: 2 } }\n\
              fn use_v() -> int { let v = mk()\n v.x }";
    let v2 = "value V2 { x: int, y: int }\n\
              @inline\nfn mk() -> V2 { V2 { x: 9, y: 2 } }\n\
              fn use_v() -> int { let v = mk()\n v.x }";
    check_edit(v1, v2, &["mk"]);
}

#[test]
fn sequential_edits_stay_consistent() {
    // Apply a sequence of edits to ONE cache (like a watch session): after each
    // incremental update the assembly must still equal a fresh emit of that state.
    // Exercises cache reuse across multiple updates, not just the first.
    let ws0 = ws_of(
        "@inline\nfn c() -> int { 1 }\n@inline\nfn b() -> int { c() }\nfn a() -> int { b() }\nfn z() -> int { 9 }",
    );
    let mid = skull::user_module_id(&ws0).unwrap();
    let (_f0, mut cache) = emit_full(&ws0, mid).unwrap();

    let edits = [
        ("@inline\nfn c() -> int { 2 }\n@inline\nfn b() -> int { c() }\nfn a() -> int { b() }\nfn z() -> int { 9 }", "c"),
        ("@inline\nfn c() -> int { 2 }\n@inline\nfn b() -> int { c() }\nfn a() -> int { b() }\nfn z() -> int { 5 }", "z"),
        ("@inline\nfn c() -> int { 2 }\n@inline\nfn b() -> int { c() }\nfn a() -> int { b() }\nfn z() -> int { 7 }", "z"),
    ];
    for (src, edited) in edits {
        let ws = ws_of(src);
        let m = skull::user_module_id(&ws).unwrap();
        let reinferred = vec![body_id(&ws, edited)];
        let inc = emit_incremental(&ws, &mut cache, &reinferred).unwrap();
        let fresh = emit_full(&ws, m).unwrap().0;
        assert_eq!(inc.lua, fresh.lua, "incremental drifted from fresh after editing {edited}");
    }
}

#[test]
fn transitive_inline_edit_reemits_root() {
    // c() inlined into b() inlined into a(): a's chunk holds c's body. Editing c
    // must re-emit a (transitive inline dep).
    let v1 = "@inline\nfn c() -> int { 1 }\n\
              @inline\nfn b() -> int { c() }\n\
              fn a() -> int { b() }";
    let v2 = "@inline\nfn c() -> int { 2 }\n\
              @inline\nfn b() -> int { c() }\n\
              fn a() -> int { b() }";
    check_edit(v1, v2, &["c"]);
}

/// A SPECIALIZED body is a cached emitted body like any other, so editing the
/// generic it specializes must re-emit it.
///
/// Nothing else names that connection: a specialization is not reached through
/// its declaration's own chunk, so without the declaration edge in its deps a
/// stale copy is reused and the incremental output keeps the OLD body. The
/// global initializer is what forces a standalone specialized copy -- it never
/// runs an inliner, so the call cannot be served by a splice
/// (`vec_value_columns.rs` documents the same route).
#[test]
fn body_edit_to_a_generic_reemits_its_specializations() {
    let v1 = "value V2 { x: float, y: float }\n\
              fn first<T>(xs: [T]) -> T { return xs[0] }\n\
              global G: float = first([V2 { x: 1.0, y: 2.0 }]).x\n\
              fn use_g() -> float { return G }";
    let v2 = "value V2 { x: float, y: float }\n\
              fn first<T>(xs: [T]) -> T { return xs[1] }\n\
              global G: float = first([V2 { x: 1.0, y: 2.0 }]).x\n\
              fn use_g() -> float { return G }";
    check_edit(v1, v2, &["first"]);
}

/// The control: the specialized body is REALLY there, and it is what the edit
/// above moves. Without this the test could pass over a program that emits no
/// specialization at all, which is the shape every arm of it would satisfy.
#[test]
fn the_generic_fixture_emits_a_standalone_specialization() {
    let v1 = "value V2 { x: float, y: float }\n\
              fn first<T>(xs: [T]) -> T { return xs[0] }\n\
              global G: float = first([V2 { x: 1.0, y: 2.0 }]).x\n\
              fn use_g() -> float { return G }";
    let ws = crate::common::ws_of(v1);
    let mid = skull::user_module_id(&ws).unwrap();
    let lua = emit_full(&ws, mid).expect("emit_full").0.lua;
    assert!(
        lua.contains("function first_arr_T_ret_T_spec_a_n2_V2_0_z("),
        "fixture must emit a standalone specialized body"
    );
}

/// The OVERLAY carries a changed specialization, not just its declaration.
///
/// A reload dofiles the overlay after `main.lua`, so a definition the overlay
/// omits keeps running the version main.lua carries. Redefining only the
/// declaration's shared body would leave the specialized copy -- which is what
/// the unspliceable call site actually calls -- pinned at the pre-edit body,
/// and nothing about the reload would look wrong.
#[test]
fn an_overlay_redefines_a_changed_specialization() {
    let v1 = "value V2 { x: float, y: float }\n\
              fn first<T>(xs: [T]) -> T { return xs[0] }\n\
              global G: float = first([V2 { x: 1.0, y: 2.0 }]).x\n\
              fn use_g() -> float { return G }";
    let v2 = "value V2 { x: float, y: float }\n\
              fn first<T>(xs: [T]) -> T { return xs[1] }\n\
              global G: float = first([V2 { x: 1.0, y: 2.0 }]).x\n\
              fn use_g() -> float { return G }";
    let ws1 = crate::common::ws_of(v1);
    let mid = skull::user_module_id(&ws1).unwrap();
    let (full, mut cache) = emit_full(&ws1, mid).expect("emit_full v1");
    let spec = "function first_arr_T_ret_T_spec_a_n2_V2_0_z(";
    assert!(full.lua.contains(spec), "control: the full build defines the specialization");

    let ws2 = crate::common::ws_of(v2);
    let overlay = emit_overlay(&ws2, &mut cache, &[body_id(&ws2, "first")]);
    assert!(
        overlay.lua.contains(spec),
        "the overlay must redefine the specialization the edit moved:\n{}",
        overlay.lua
    );
}

/// ONE liveness answer decides both kinds of emitted body.
///
/// The renderer no longer discovers or builds: it orders text the walk already
/// reached. So a specialized body reaching a FURTHER specialization has to come
/// out of the walk, and if the walk stopped carrying that edge the build fails
/// loudly (`no liveness walk reached it`) rather than quietly rebuilding it in a
/// second place. Measured on bitgun: cutting the edge fires that rejection.
///
/// Here the property is observable as output: the specialized body is emitted at
/// all, and it is emitted ONCE -- two deciders would each be entitled to render
/// it.
#[test]
fn one_liveness_walk_decides_the_specialized_bodies() {
    let src = "value V2 { x: float, y: float }\n\
               fn first<T>(xs: [T]) -> T { return xs[0] }\n\
               global G: float = first([V2 { x: 1.0, y: 2.0 }]).x\n\
               fn use_g() -> float { return G }";
    let ws = crate::common::ws_of(src);
    let mid = skull::user_module_id(&ws).unwrap();
    let lua = emit_full(&ws, mid).expect("emit_full").0.lua;
    let def = "function first_arr_T_ret_T_spec_a_n2_V2_0_z(";
    assert_eq!(
        lua.matches(def).count(),
        1,
        "the specialized body is decided once and emitted once"
    );
}

/// Editing a generic `co fn` must re-emit the SPECIALIZED coroutines of it.
///
/// A specialization is not reached through its declaration's chunk, so the only
/// thing naming the connection is the declaration edge the specialized chunk
/// records in its deps. Byte identity cannot see this at all -- it compares
/// fresh full builds and never performs an edit -- so the arm lives here.
///
/// The element is column-stored on purpose: that is what makes the call
/// specialize on the default path, so the fixture has a specialized coroutine to
/// invalidate rather than one shared body.
#[test]
fn a_body_edit_to_a_generic_coroutine_reemits_its_specializations() {
    let v1 = "nobox value P { x: float, y: float }\n\
              co fn walk<T>(v: [T]) -> Coroutine<int, int, ()> {\n\
              \x20   let n = v.len\n\
              \x20   yield n\n\
              \x20   return 1\n\
              }\n\
              fn use_it() -> Coroutine<int, int, ()> {\n\
              \x20   let xs: [P] = [P { x: 1.0, y: 0.0 }]\n\
              \x20   return walk(xs)\n\
              }";
    let v2 = v1.replace("return 1", "return 2");
    // The population, asserted in the same test: without a specialized
    // coroutine in v1 there is nothing for the edit to invalidate and the
    // comparison would pass over an empty set.
    let ws = crate::common::ws_of(v1);
    let mid = skull::user_module_id(&ws).unwrap();
    let lua = emit_full(&ws, mid).expect("emit_full v1").0.lua;
    let spec_steps = lua
        .match_indices("__step(")
        .map(|(i, _)| lua[..i].rsplit("function ").next().unwrap_or("").to_string())
        .filter(|n| n.contains("_spec_"))
        .count();
    assert_eq!(spec_steps, 1, "the fixture must produce a SPECIALIZED coroutine step:\n{lua}");
    check_edit(v1, &v2, &["walk"]);
}
