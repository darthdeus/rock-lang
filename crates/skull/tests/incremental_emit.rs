//! Oracle for skull incremental codegen (`docs/skull-incremental-emit-plan.md`):
//! after re-emitting only the bodies a (simulated) edit re-inferred, the assembled
//! Lua must be BYTE-IDENTICAL to a fresh full emit of the edited workspace. The
//! interesting case is editing an INLINED callee — its callers' chunks contain its
//! body, so they must re-emit too (the inline codegen-dep), even though a body edit
//! never re-infers the callers.

use bone_semantic::hir::BodyOwner;
use bone_semantic::inference::Workspace;
use bone_semantic::prelude::ModuleId;
use skull::mir_emit::{emit_full, emit_incremental};

mod common;
use common::ws_of;

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
