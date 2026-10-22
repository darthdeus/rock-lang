//! Oracle for skull incremental codegen:
//! after re-emitting only the bodies a (simulated) edit re-inferred, the assembled
//! Lua must be BYTE-IDENTICAL to a fresh full emit of the edited workspace. The
//! interesting case is editing an INLINED callee — its callers' chunks contain its
//! body, so they must re-emit too (the inline codegen-dep), even though a body edit
//! never re-infers the callers.

use bone_semantic::hir::BodyOwner;
use bone_semantic::inference::Workspace;
use bone_semantic::prelude::ModuleId;
use skull::mir_emit::{emit_full, emit_incremental, emit_overlay as emit_overlay_result, MirEmit};
use crate::common::{ws_of, ws_with_stdlib};

fn emit_overlay(
    ws: &Workspace,
    cache: &mut skull::mir_emit::SkullEmitCache,
    reinferred: &[(ModuleId, BodyOwner)],
) -> MirEmit {
    emit_overlay_result(ws, cache, reinferred).expect("emit_overlay")
}

/// Explicit global-shape coverage. Bound overlays are also exercised by the
/// callee-binding runtime tests and the production CLI overlay harness.
fn emit_full_global(
    ws: &Workspace,
    mid: ModuleId,
) -> Result<(MirEmit, skull::mir_emit::SkullEmitCache), skull_parser::CompilerError> {
    skull::mir_emit::emit_full_with_callee_binding(
        ws,
        mid,
        skull_codegen_lua::CalleeBindingMode::Global,
    )
}

/// The `(ModuleId, BodyOwner)` reinferred-id for the named function in `ws`.
fn body_id(ws: &Workspace, name: &str) -> (ModuleId, BodyOwner) {
    // Every module, not `user_module_id`: that returns the FIRST non-builtin
    // module, which over a stdlib workspace is a stdlib module rather than the
    // fixture's own.
    for module in ws.modules.iter() {
        for (fid, f) in module.functions.iter() {
            if f.name.as_str() == name {
                return (module.id, BodyOwner::Function(fid));
            }
        }
    }
    panic!("no fn named {name}");
}

/// Drive one body edit through the cache and assert incremental == fresh.
fn check_edit(v1: &str, v2: &str, edited: &[&str]) {
    check_edit_with(ws_of, v1, v2, edited)
}

#[test]
fn scalar_constant_edits_refresh_full_incremental_and_overlay_emission() {
    let v1 = "const BASE: float = 1.5\nconst ALIAS: float = BASE\n@nomangle fn choose() -> float { ALIAS }";
    let v2 = v1.replace("1.5", "-0.0");
    let ws1 = ws_of(v1);
    let mid = skull::user_module_id(&ws1).unwrap();
    let (full, mut incremental_cache) = emit_full_global(&ws1, mid).unwrap();
    let (_, mut overlay_cache) = emit_full_global(&ws1, mid).unwrap();
    assert!(crate::common::function_body(&full.lua, "choose").contains("1.5"));
    let ws2 = ws_of(&v2);
    let gid = ws2.module(mid).globals.iter().find(|(_, g)| g.name.as_str() == "BASE").unwrap().0;
    let edited = [(mid, BodyOwner::Global(gid))];
    let incremental = emit_incremental(&ws2, &mut incremental_cache, &edited).unwrap();
    let fresh = emit_full_global(&ws2, mid).unwrap().0;
    assert_eq!(incremental.lua, fresh.lua);
    let overlay = emit_overlay(&ws2, &mut overlay_cache, &edited);
    let body = crate::common::function_body(&overlay.lua, "choose");
    assert!(body.contains("-0.0") && !body.contains("ALIAS"), "{body}");
}

#[test]
fn numeric_selection_updates_in_full_incremental_and_overlay_emission() {
    let v1 = "@nomangle fn choose(a: float, b: float) -> float { return if a < b { a } else { b } }";
    let v2 = v1.replace("a < b", "a > b");
    let ws1 = ws_of(v1);
    let (full, mut cache) = emit_full_global(&ws1, skull::user_module_id(&ws1).unwrap()).unwrap();
    assert!(full.lua.contains("math.min("), "{}", full.lua);
    let ws2 = ws_of(&v2);
    let overlay = emit_overlay(&ws2, &mut cache, &[body_id(&ws2, "choose")]);
    assert!(overlay.lua.contains("math.max("), "{}", overlay.lua);
    assert!(!overlay.lua.contains("math.min("), "{}", overlay.lua);
    check_edit(v1, &v2, &["choose"]);
}

/// [`check_edit`] over a chosen workspace builder. A fixture needing the stdlib
/// (any `+`, any collection method) passes `common::ws_with_stdlib`; the bare
/// `ws_of` keeps the cheap fixtures cheap.
fn check_edit_with(build: fn(&str) -> Workspace, v1: &str, v2: &str, edited: &[&str]) {
    let ws1 = build(v1);
    let mid = skull::user_module_id(&ws1).unwrap();
    let (_full1, mut cache) = emit_full(&ws1, mid).expect("emit_full v1");

    let ws2 = build(v2);
    let mid2 = skull::user_module_id(&ws2).unwrap();
    let reinferred: Vec<_> = edited.iter().map(|n| body_id(&ws2, n)).collect();

    let incremental = emit_incremental(&ws2, &mut cache, &reinferred).expect("emit_incremental");
    let fresh = emit_full(&ws2, mid2).expect("emit_full v2").0;
    assert_eq!(incremental.lua, fresh.lua, "incremental assembly != fresh emit after editing {edited:?}");
}

fn residual_inline_budget_fixture(helper_value: i32, caller_locals: usize) -> String {
    let mut src = format!(
        "@inline @nomangle fn helper() -> int {{ {helper_value} }}\n\
         @nomangle fn caller() -> int {{\n"
    );
    for i in 0..caller_locals {
        src.push_str(&format!("let unused_{i} = {i}\n"));
    }
    src.push_str("return helper()\n}\n@nomangle fn other() -> int { 7 }\n");
    src
}

fn inline_budget_eligibility_flip_fixture(large_helper: bool) -> String {
    let mut src = String::from("@inline @nomangle fn helper(flag: bool) -> int {\n");
    if large_helper {
        for i in 0..64 {
            src.push_str(&format!(
                "let value_{i} = {i}\nif flag {{ return value_{i} }}\n"
            ));
        }
        src.push_str("return 17\n}\n");
    } else {
        src.push_str("if flag { return 3 }\nreturn 5\n}\n");
    }
    src.push_str("@nomangle fn caller() -> int {\n");
    for i in 0..380 {
        src.push_str(&format!("let unused_{i} = {i}\n"));
    }
    src.push_str("return helper(false)\n}\n@nomangle fn other() -> int { 7 }\n");
    src
}

#[test]
fn body_edit_to_inlined_callee_reemits_callers() {
    // `helper` is a tiny straight callee -> inlined into `use_it`. Editing its
    // body must re-emit `use_it` (its chunk holds the inlined value), not just
    // `helper`. `other` is untouched and reused from cache.
    let v1 = "@inline @nomangle fn helper() -> int { 3 }\n\
              @nomangle fn use_it() -> float { helper() }\n\
              @nomangle fn other() -> int { 7 }";
    let v2 = "@inline @nomangle fn helper() -> int { 5 }\n\
              @nomangle fn use_it() -> float { helper() }\n\
              @nomangle fn other() -> int { 7 }";
    let ws1 = ws_of(v1);
    let (full1, mut cache) = emit_full_global(&ws1, skull::user_module_id(&ws1).unwrap())
        .expect("emit_full v1");
    let caller_v1 = crate::common::function_body(&full1.lua, "use_it");
    assert!(!caller_v1.contains("helper("), "the fixture's call must be inlined:\n{caller_v1}");

    let ws2 = ws_of(v2);
    let incremental = emit_incremental(&ws2, &mut cache, &[body_id(&ws2, "helper")])
        .expect("emit_incremental");
    let stats = cache.repair_stats();
    assert_eq!(stats.chunks_rebuilt, 2, "the edited helper and its inliner must rebuild: {stats:?}");
    assert_eq!(stats.chunk_rebuild_requests, 2, "the untouched `other` chunk must stay cached: {stats:?}");
    assert!(incremental.lua.contains("function other("), "the reused sibling must remain in the assembly");
    let caller_v2 = crate::common::function_body(&incremental.lua, "use_it");
    assert!(!caller_v2.contains("helper("), "the updated call must still be inlined:\n{caller_v2}");
    let fresh = emit_full_global(&ws2, skull::user_module_id(&ws2).unwrap())
        .expect("emit_full v2")
        .0;
    assert_eq!(incremental.lua, fresh.lua, "incremental assembly != fresh emit after editing helper");
}

#[test]
fn body_edit_to_still_residual_inline_callee_rebuilds_only_the_callee() {
    // The caller starts over the 400-cell splice budget, so the @inline call
    // stays residual both before and after this literal-only helper edit. The
    // caller's emitted text is unchanged and must be reused; `other` is a second
    // untouched chunk to keep the reuse population explicit.
    let v1 = residual_inline_budget_fixture(3, 401);
    let v2 = residual_inline_budget_fixture(5, 401);
    let ws1 = ws_of(&v1);
    let (full1, mut cache) = emit_full_global(&ws1, skull::user_module_id(&ws1).unwrap())
        .expect("emit_full v1");
    let caller_v1 = crate::common::function_body(&full1.lua, "caller");
    assert!(caller_v1.contains("helper("), "the over-budget call must remain residual:\n{caller_v1}");
    assert!(full1.lua.contains("function other("), "the fixture must retain its reuse-control chunk");

    let ws2 = ws_of(&v2);
    let incremental = emit_incremental(&ws2, &mut cache, &[body_id(&ws2, "helper")])
        .expect("emit_incremental");
    let stats = cache.repair_stats();
    assert_eq!(stats.chunks_rebuilt, 1, "a still-residual caller should be reused: {stats:?}");
    let caller_v2 = crate::common::function_body(&incremental.lua, "caller");
    assert!(caller_v2.contains("helper("), "the call should remain residual:\n{caller_v2}");
    let fresh = emit_full_global(&ws2, skull::user_module_id(&ws2).unwrap())
        .expect("emit_full v2")
        .0;
    assert_eq!(incremental.lua, fresh.lua, "incremental assembly != fresh emit after editing helper");
}

#[test]
fn shrinking_inline_callee_flips_residual_call_to_inline_and_rebuilds_caller() {
    let v1 = inline_budget_eligibility_flip_fixture(true);
    let v2 = inline_budget_eligibility_flip_fixture(false);
    let ws1 = ws_of(&v1);
    let (full1, mut cache) = emit_full_global(&ws1, skull::user_module_id(&ws1).unwrap())
        .expect("emit_full v1");
    let caller_v1 = crate::common::function_body(&full1.lua, "caller");
    assert!(caller_v1.contains("helper("), "the large helper must remain residual:\n{caller_v1}");

    let ws2 = ws_of(&v2);
    let incremental = emit_incremental(&ws2, &mut cache, &[body_id(&ws2, "helper")])
        .expect("emit_incremental");
    let stats = cache.repair_stats();
    assert_eq!(stats.chunks_rebuilt, 2, "the smaller helper should make its caller newly eligible: {stats:?}");
    assert_eq!(stats.chunk_rebuild_requests, 2, "the untouched `other` chunk should stay cached: {stats:?}");
    let caller_v2 = crate::common::function_body(&incremental.lua, "caller");
    assert!(!caller_v2.contains("helper("), "the smaller helper should now inline:\n{caller_v2}");
    let fresh = emit_full_global(&ws2, skull::user_module_id(&ws2).unwrap())
        .expect("emit_full v2")
        .0;
    assert_eq!(incremental.lua, fresh.lua, "incremental assembly != fresh emit after shrinking helper");
}

#[test]
fn exact_body_edit_has_no_full_concrete_or_artifact_rebuild() {
    let v1 = "fn helper() -> int { 3 }\nfn main() { let x = helper() }";
    let v2 = "fn helper() -> int { 5 }\nfn main() { let x = helper() }";
    let ws1 = ws_with_stdlib(v1);
    let workspace_functions: usize = ws1.modules.iter().map(|module| module.functions.len()).sum();
    assert!(workspace_functions > 2, "root locality fixture has no unrelated function population");
    let mid = skull::user_module_id(&ws1).unwrap();
    let (_full, mut cache) = emit_full_global(&ws1, mid).expect("emit_full v1");
    let ws2 = ws_with_stdlib(v2);

    let _overlay = emit_overlay(&ws2, &mut cache, &[body_id(&ws2, "helper")]);
    let stats = cache.repair_stats();
    assert_eq!(stats.body_only_edits, 1, "the production route did not record a body edit");
    assert!(stats.owners_revisited > 0, "no graph owner was revisited: {stats:?}");
    assert!(stats.instances_revisited > 0, "no graph delta subject was visited: {stats:?}");
    assert!(stats.graph_contributions_retracted > 0, "no graph contribution was retracted: {stats:?}");
    assert!(stats.graph_contributions_installed > 0, "no graph contribution was installed: {stats:?}");
    assert!(stats.concrete_entries_derived > 0, "no concrete entry was derived: {stats:?}");
    assert!(stats.concrete_entries_reused > 0, "no unchanged concrete entry was reused: {stats:?}");
    assert!(stats.concrete_entries_retracted > 0, "no concrete entry was retracted: {stats:?}");
    assert!(stats.artifact_chunks_revisited > 0, "no artifact subject was revisited: {stats:?}");
    assert_eq!(stats.full_graph_rebuilds, 0, "body-only route rebuilt the graph: {stats:?}");
    assert_eq!(
        stats.full_concrete_rebuilds, 0,
        "a body-only edit rebuilt all concrete plans: {stats:?}"
    );
    assert_eq!(
        stats.full_artifact_view_realizations, 0,
        "a body-only edit rebuilt the full ArtifactView: {stats:?}"
    );
    assert_eq!(stats.broad_owner_scans, 0, "body-only route scanned all owners: {stats:?}");
    assert_eq!(stats.broad_concrete_scans, 0, "body-only route scanned all concrete entries: {stats:?}");
    assert_eq!(stats.artifact_scalar_context_builds, 0, "artifact repair rebuilt the maintained scalar context: {stats:?}");
    assert_eq!(stats.broad_artifact_scans, 0, "body-only route scanned all artifact chunks: {stats:?}");
    assert_eq!(stats.broad_candidate_scans, 0, "body-only route scanned all candidate keys: {stats:?}");
    assert!(stats.root_requests > 0, "root locality has no request denominator: {stats:?}");
    assert_eq!(
        stats.root_functions_examined, 0,
        "body-only route enumerated workspace roots: {stats:?}"
    );
    assert_eq!(stats.root_reseeds, 0, "body-only route reseeded all roots: {stats:?}");
    assert_eq!(
        stats.root_modules_examined, 0,
        "body-only route enumerated root modules: {stats:?}"
    );
    assert_eq!(
        stats.root_instances_checked, 0,
        "body-only route scanned retained root instances: {stats:?}"
    );
    assert!(
        stats.shader_delta_requests > 0,
        "shader-root locality has no body request denominator: {stats:?}"
    );
    assert_eq!(
        stats.artifact_realize_invocations, 0,
        "body-only route realized a full artifact: {stats:?}"
    );
    assert!(
        stats.artifact_iteration_requests > 0,
        "artifact locality has no changed-subject denominator: {stats:?}"
    );
    assert!(
        stats.artifact_index_rows_examined > 0,
        "artifact locality did not count its connected index rows: {stats:?}"
    );
    assert!(
        stats.artifact_iterations >= stats.artifact_index_rows_examined,
        "artifact iteration census dropped indexed rows: {stats:?}"
    );
    assert!(
        stats.declaration_lookup_requests > 0,
        "declaration lookup locality has no request denominator: {stats:?}"
    );
    assert!(
        stats.candidate_entries_requested > 0,
        "candidate locality has no requested-entry denominator: {stats:?}"
    );
    assert!(
        stats.chunk_rebuild_requests > 0,
        "chunk locality has no rebuild-request denominator: {stats:?}"
    );
    assert!(
        stats.dynamic_state_chunks_compared > 0,
        "state locality has no changed-chunk denominator: {stats:?}"
    );
    assert_eq!(
        stats.dynamic_state_full_scans, 0,
        "an unchanged body-local state shape scanned every emitted chunk: {stats:?}"
    );
    assert_eq!(
        stats.dynamic_state_static_rows_scanned, 0,
        "an unchanged body-local state shape rebuilt the static inventory: {stats:?}"
    );
}

#[test]
fn an_unrelated_body_overlay_does_not_restage_the_complete_code_artifact() {
    let v1 = r#"
@lifecycle(phase="update", owner="session")
deco init tick {}
@tick fn first() {}
@tick fn second() {}
fn helper() -> int { return 1 }
fn main() { let _ = helper() }
"#;
    let v2 = v1.replace("return 1", "return 2");
    let ws1 = ws_of(v1);
    let mid = skull::user_module_id(&ws1).unwrap();
    let (full, mut cache) = emit_full_global(&ws1, mid).expect("emit_full v1");
    let escaping = full.code_artifact.entries.values().filter(|entry| entry.escaping).count();
    assert!(escaping >= 2, "fixture must expose a nonzero CodeRef population: {escaping}");

    let ws2 = ws_of(&v2);
    let overlay = emit_overlay(&ws2, &mut cache, &[body_id(&ws2, "helper")]);
    let staged = overlay
        .lua
        .lines()
        .filter(|line| line.contains("__rock_code_slot_stage("))
        .count();
    assert_eq!(
        staged, 0,
        "an unrelated body edit must not restage all {escaping} CodeRefs"
    );
    assert!(
        overlay.lua.contains("__rock_code_slot_publish()"),
        "the overlay must retain the transactional publication boundary"
    );
}

#[test]
fn a_changed_decorated_body_stages_its_logical_code_ref() {
    let v1 = r#"
@lifecycle(phase="update", owner="session")
deco init tick {}
@tick fn first() {}
@tick fn second() {}
fn main() {}
"#;
    let v2 = v1.replace("@tick fn first() {}", "@tick fn first() { let x = 1 }");
    let ws1 = ws_of(v1);
    let mid = skull::user_module_id(&ws1).unwrap();
    let (full, mut cache) = emit_full_global(&ws1, mid).expect("emit_full v1");
    let escaping = full.code_artifact.entries.values().filter(|entry| entry.escaping).count();
    assert!(escaping >= 2, "fixture must expose a nonzero CodeRef population: {escaping}");

    let ws2 = ws_of(&v2);
    let overlay = emit_overlay(&ws2, &mut cache, &[body_id(&ws2, "first")]);
    let staged: Vec<_> = overlay
        .lua
        .lines()
        .filter(|line| line.contains("__rock_code_slot_stage("))
        .collect();
    assert_eq!(staged.len(), 1, "only the changed decorated body should be staged: {staged:?}");
    assert!(
        staged[0].contains("named:<t>::first<>"),
        "the staged slot must retain logical identity: {}",
        staged[0]
    );
    let stage_offset = overlay
        .lua
        .find("__rock_code_slot_stage(")
        .expect("the changed CodeRef must be staged");
    let body_offset = overlay.lua.find("function first").expect("changed body emission");
    assert!(
        body_offset < stage_offset,
        "CodeRef publication must observe the newly emitted body: {body_offset} !< {stage_offset}"
    );
}

#[test]
fn a_changed_caller_can_restage_an_unchanged_callback() {
    let v1 = r#"
fn apply<T>(value: T, callback: fn(T) -> T) -> T { return callback(value) }
fn unchanged(value: int) -> int { return value }
fn caller() -> int { return apply(1, unchanged) }
"#;
    let v2 = v1.replace("apply(1, unchanged)", "apply(2, unchanged)");
    let ws1 = ws_of(v1);
    let mid = skull::user_module_id(&ws1).unwrap();
    let (full, mut cache) = emit_full_global(&ws1, mid).expect("emit_full v1");
    let callback = full
        .code_artifact
        .entries
        .values()
        .find(|entry| {
            entry.escaping
                && entry
                    .body_name
                    .is_some_and(|name| name.as_str().starts_with("unchanged"))
        })
        .expect("fixture must publish the unchanged callback");
    assert!(callback.body_name.is_some(), "fresh emission must realize the callback body");

    let ws2 = ws_of(&v2);
    let overlay = emit_overlay_result(&ws2, &mut cache, &[body_id(&ws2, "caller")])
        .expect("an unchanged callback referenced by the changed caller must retain its body");
    let staged: Vec<_> = overlay
        .lua
        .lines()
        .filter(|line| line.contains("__rock_code_slot_stage(") && line.contains("unchanged"))
        .collect();
    assert_eq!(staged.len(), 1, "the changed caller must stage its unchanged callback: {staged:?}");
    assert!(
        staged[0].contains("unchanged_int"),
        "the callback stage must point at the body already loaded from the full artifact: {}",
        staged[0]
    );
}

#[test]
fn an_unchanged_state_shape_uses_incremental_schema_validation() {
    let v1 = "@lifecycle(phase=\"update\", owner=\"session\")\nfn first() {}\n";
    let v2 = v1.replace("first", "second");
    let ws1 = ws_of(v1);
    let mid = skull::user_module_id(&ws1).unwrap();
    let (full, mut cache) = emit_full_global(&ws1, mid).expect("emit_full v1");
    let ws2 = ws_of(&v2);
    let overlay = emit_overlay(&ws2, &mut cache, &[body_id(&ws2, "second")]);
    assert_eq!(
        cache.state_schema().unwrap().schema_id,
        full.state_schema.schema_id,
        "a body-only edit changed the state schema"
    );
    let full_collect = overlay
        .phases
        .iter()
        .find(|(name, _)| *name == "  StateSchema full collection")
        .expect("schema phase census");
    assert_eq!(
        full_collect.1.as_nanos(),
        0,
        "unchanged state shape took the full declaration walk: {full_collect:?}"
    );
}

#[test]
fn a_static_initializer_edit_refreshes_dynamic_state_facts() {
    let v1 = "fn helper() -> int { static n = 1 return n }\nfn main() { let _ = helper() }";
    let v2 = "fn helper() -> int { static n = 2 return n }\nfn main() { let _ = helper() }";
    let ws1 = ws_of(v1);
    let mid = skull::user_module_id(&ws1).unwrap();
    let (_full, mut cache) = emit_full_global(&ws1, mid).expect("emit_full v1");
    let ws2 = ws_of(v2);
    let _overlay = emit_overlay(&ws2, &mut cache, &[body_id(&ws2, "helper")]);
    let stats = cache.repair_stats();
    assert!(
        stats.dynamic_state_chunks_compared > 0,
        "missing comparison population: {stats:?}"
    );
    assert!(
        stats.dynamic_state_facts_changed,
        "the initializer change was classified as state-neutral"
    );
    assert_eq!(
        stats.dynamic_state_full_scans, 1,
        "the changed state facts were not validated"
    );
    assert!(
        stats.dynamic_state_static_rows_scanned > 0,
        "the changed static did not reach validation: {stats:?}"
    );
}

#[test]
fn removing_the_last_edge_retracts_concrete_and_artifact_entries_locally() {
    let v1 = "fn leaf<T>(x: T) -> T { x }\nfn main() { let x = leaf(3) }";
    let v2 = "fn leaf<T>(x: T) -> T { x }\nfn main() { let x = 0 }";
    let ws1 = ws_of(v1);
    let mid = skull::user_module_id(&ws1).unwrap();
    let (full, mut cache) = emit_full(&ws1, mid).expect("emit_full v1");
    assert!(full.lua.contains("leaf"), "the removal fixture must emit the callee");

    let ws2 = ws_of(v2);
    let incremental = emit_incremental(&ws2, &mut cache, &[body_id(&ws2, "main")])
        .expect("emit_incremental v2");
    let stats = cache.repair_stats();
    assert!(stats.instances_revisited > 0, "the changed owner was not revisited: {stats:?}");
    assert!(stats.concrete_entries_retracted > 0, "the removed callee was not retracted: {stats:?}");
    assert!(stats.artifact_chunks_removed > 0, "the removed callee chunk was not retracted: {stats:?}");
    assert_eq!(stats.full_concrete_rebuilds, 0, "local removal rebuilt concrete state: {stats:?}");
    assert_eq!(stats.full_artifact_view_realizations, 0, "local removal realized a full view: {stats:?}");
    let fresh = emit_full(&ws2, skull::user_module_id(&ws2).unwrap()).expect("emit_full v2").0;
    assert_eq!(incremental.lua, fresh.lua, "local removal diverged from fresh emission");
}

#[test]
fn body_edit_to_leaf_only_reemits_itself() {
    let v1 = "@inline\nfn helper() -> int { 3 }\n\
              fn use_it() -> float { helper() }\n\
              fn other() -> int { 7 }";
    let v2 = "@inline\nfn helper() -> int { 3 }\n\
              fn use_it() -> float { helper() }\n\
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
        lua.contains("function first_arr_T_spec_a_n2_V2_0_z("),
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
    let (full, mut cache) = emit_full_global(&ws1, mid).expect("emit_full v1");
    let spec = "function first_arr_T_spec_a_n2_V2_0_z(";
    assert!(full.lua.contains(spec), "control: the full build defines the specialization");

    let ws2 = crate::common::ws_of(v2);
    let overlay = emit_overlay(&ws2, &mut cache, &[body_id(&ws2, "first")]);
    assert!(
        overlay.lua.contains(spec),
        "the overlay must redefine the specialization the edit moved:\n{}",
        overlay.lua
    );
}

/// The OVERLAY carries a specialization the graph added for the edit
/// (graph-delta overlay construction).
///
/// The sibling above edits a body whose specialization already exists, so the
/// overlay only has to REDEFINE it. Here the edit adds the first call to a new
/// instantiation, so the graph delta has to add its definition -- and a reload
/// dofiles the overlay after `main.lua`, which carries no such definition. The edited
/// caller's call spells `..._spec_...`, so omitting it is a nil global at
/// reload rather than a stale body.
///
/// The two populations are asserted in the same test: the P specialization must
/// already exist (so this is about the graph addition) and the Q one must not.
#[test]
fn an_overlay_carries_a_graph_added_specialization() {
    let types = "value P { x: float, y: float }\n\
                 value Q { a: float, b: float, c: float }\n\
                 fn tally_of<T>(xs: [T]) -> T { return xs[0] }\n";
    let v1 = types.to_string()
        + "global G1: float = tally_of([P { x: 1.0, y: 0.0 }]).x\n\
           fn use_it() -> float { return G1 }";
    let v2 = types.to_string()
        + "global G1: float = tally_of([P { x: 1.0, y: 0.0 }]).x\n\
           fn use_it() -> float { return tally_of([Q { a: 1.0, b: 2.0, c: 3.0 }]).a }";

    let ws1 = crate::common::ws_of(&v1);
    let mid = skull::user_module_id(&ws1).unwrap();
    let (full, mut cache) = emit_full_global(&ws1, mid).expect("emit_full v1");
    let spec_of = |lua: &str, ty: &str| {
        lua.lines().any(|l| l.starts_with("function tally_of_") && l.contains(ty))
    };
    assert!(spec_of(&full.lua, "P"), "control: v1 already defines the P specialization");
    assert!(!spec_of(&full.lua, "Q"), "control: v1 must NOT define the Q one yet");

    let ws2 = crate::common::ws_of(&v2);
    let overlay = emit_overlay(&ws2, &mut cache, &[body_id(&ws2, "use_it")]);
    assert!(
        spec_of(&overlay.lua, "Q"),
        "the overlay must ADD the specialization the edit made reachable, or the \
         reload calls a nil global:\n{}",
        overlay.lua
    );
}

/// **A SECOND edit to a generic re-emits the instantiation a FIRST edit added**
/// -- which is only true if the graph is MAINTAINED across edits rather than
/// frozen at the last full build (mono cut C+D, E1 graph ownership).
///
/// `update_chunk_cache` maps a changed declaration to its live instances
/// through `InstanceGraph::instances_of`. A graph left at its full-build state
/// still answers, and answers plausibly -- it just does not know about `Q`,
/// which the first edit introduced. So the second edit re-emits `P` and leaves
/// the reload running the OLD `Q` body: a stale specialization, silently, with
/// every existing arm green.
///
/// Both populations are asserted: the first overlay must actually add `Q` (or
/// the second overlay has nothing to be stale about), and the second must carry
/// the edited body at BOTH instantiations.
#[test]
fn a_later_edit_to_a_generic_reaches_an_instantiation_an_earlier_edit_added() {
    let types = "value P { x: float, y: float }\n\
                 value Q { a: float, b: float, c: float }\n";
    let generic = |marker: &str| {
        format!("fn tally_of<T>(xs: [T]) -> T {{ print(\"{marker}\") return xs[0] }}\n")
    };
    let v1 = types.to_string()
        + &generic("MARK1")
        + "global G1: float = tally_of([P { x: 1.0, y: 0.0 }]).x\n\
           fn use_it() -> float { return G1 }";
    // Edit 1: `use_it` gains the first call at `Q`.
    let v2 = types.to_string()
        + &generic("MARK1")
        + "global G1: float = tally_of([P { x: 1.0, y: 0.0 }]).x\n\
           fn use_it() -> float { return tally_of([Q { a: 1.0, b: 2.0, c: 3.0 }]).a }";
    // Edit 2: the GENERIC's own body changes. Both instantiations must move.
    let v3 = types.to_string()
        + &generic("MARK2")
        + "global G1: float = tally_of([P { x: 1.0, y: 0.0 }]).x\n\
           fn use_it() -> float { return tally_of([Q { a: 1.0, b: 2.0, c: 3.0 }]).a }";

    let ws1 = crate::common::ws_with_stdlib(&v1);
    let mid = skull::user_module_id(&ws1).unwrap();
    let (_full, mut cache) = emit_full_global(&ws1, mid).expect("emit_full v1");

    let ws2 = crate::common::ws_with_stdlib(&v2);
    let o1 = emit_overlay(&ws2, &mut cache, &[body_id(&ws2, "use_it")]);
    let defines_q = |lua: &str| {
        lua.lines().any(|l| l.starts_with("function tally_of_") && l.contains("Q"))
    };
    assert!(defines_q(&o1.lua), "control: edit 1 must add the Q instantiation:\n{}", o1.lua);

    let ws3 = crate::common::ws_with_stdlib(&v3);
    let o2 = emit_overlay(&ws3, &mut cache, &[body_id(&ws3, "tally_of")]);
    // The marker is what makes "re-emitted" observable; a name-only check would
    // pass on the stale definition.
    let bodies: Vec<&str> =
        o2.lua.lines().filter(|l| l.starts_with("function tally_of_")).collect();
    assert_eq!(bodies.len(), 2, "both instantiations must be redefined, got {bodies:?}");
    assert_eq!(
        o2.lua.matches("MARK2").count(),
        2,
        "each redefined body must carry the EDITED marker, or one instantiation \
         was served from a chunk the stale graph never invalidated:\n{}",
        o2.lua
    );
}

/// **A pruned instance's chunk is FORGOTTEN, so a later edit that brings the
/// instantiation back rebuilds it instead of serving the stale body.**
///
/// Removal is the direction emission cannot see on its own: nothing on the
/// chunk side knows an instance was stranded, so without
/// `SkullEmitCache::forget_instance` the entry simply stays. That is harmless
/// for a full reassembly -- the liveness walk does not reach it -- which is
/// exactly why it needs its own arm: the damage only appears when the
/// instantiation comes BACK, because a stale chunk would otherwise survive the
/// graph removal and the reload then run a definition built
/// from the pre-edit source.
///
/// The three populations are asserted in order: `Q` present, `Q` stranded, `Q`
/// back and carrying the EDITED body.
#[test]
fn an_instantiation_that_returns_after_being_pruned_is_rebuilt_not_reused() {
    let types = "value P { x: float, y: float }\n\
                 value Q { a: float, b: float, c: float }\n";
    let generic = |marker: &str| {
        format!("fn tally_of<T>(xs: [T]) -> T {{ print(\"{marker}\") return xs[0] }}\n")
    };
    let keep_alive = "global G1: float = tally_of([P { x: 1.0, y: 0.0 }]).x\n";
    let calls_q = "fn use_it() -> float { return tally_of([Q { a: 1.0, b: 2.0, c: 3.0 }]).a }";
    let no_q = "fn use_it() -> float { return G1 }";

    let v1 = types.to_string() + &generic("MARK1") + keep_alive + calls_q;
    let v2 = types.to_string() + &generic("MARK1") + keep_alive + no_q; // strands Q
    let v3 = types.to_string() + &generic("MARK2") + keep_alive + no_q; // edits the generic
    let v4 = types.to_string() + &generic("MARK2") + keep_alive + calls_q; // Q returns

    let ws1 = crate::common::ws_with_stdlib(&v1);
    let mid = skull::user_module_id(&ws1).unwrap();
    let (full, mut cache) = emit_full_global(&ws1, mid).expect("emit_full v1");
    let q_bodies = |lua: &str| -> Vec<String> {
        lua.lines()
            .filter(|l| l.starts_with("function tally_of_") && l.contains("Q"))
            .map(str::to_string)
            .collect()
    };
    assert_eq!(q_bodies(&full.lua).len(), 1, "control: v1 defines the Q instantiation");

    let ws2 = crate::common::ws_with_stdlib(&v2);
    let removal = emit_overlay(&ws2, &mut cache, &[body_id(&ws2, "use_it")]);
    assert_eq!(
        q_bodies(&removal.lua).len(),
        0,
        "the removal overlay must remove the stranded Q specialization:\n{}",
        removal.lua
    );
    let ws3 = crate::common::ws_with_stdlib(&v3);
    let _ = emit_overlay(&ws3, &mut cache, &[body_id(&ws3, "tally_of")]);

    let ws4 = crate::common::ws_with_stdlib(&v4);
    let back = emit_overlay(&ws4, &mut cache, &[body_id(&ws4, "use_it")]);
    assert_eq!(
        q_bodies(&back.lua).len(),
        1,
        "control: the returning instantiation must be defined again:\n{}",
        back.lua
    );
    let text = &back.lua;
    let start = text.find("function tally_of_").map(|i| &text[i..]).unwrap_or(text);
    let q_start = text
        .find(q_bodies(text)[0].as_str())
        .map(|i| &text[i..])
        .unwrap_or(start);
    let q_body: String = q_start.lines().take_while(|l| *l != "end").collect();
    assert!(
        q_body.contains("MARK2"),
        "the returning instantiation must be REBUILT from the edited generic, not \
         served from the chunk it had before it was pruned:\n{q_body}"
    );
}

/// The same ABI-width flip on the `emit_incremental` path: taking `foo` by
/// value flips `taken_by_ref`, which invalidates every memoized ABI plan, so
/// the reassembled program must equal a fresh full emit rather than serve
/// plans derived under the old ABI state.
#[test]
fn an_incremental_emit_matches_fresh_after_a_taken_by_ref_flip() {
    let v1 = "value V2 { x: float, y: float }\n\
              fn foo(v: V2) -> float { v.x }\n\
              fn other() -> float { foo(V2 { x: 2.0, y: 3.0 }) }\n\
              fn use_ref() -> float { other() }";
    let v2 = "value V2 { x: float, y: float }\n\
              fn foo(v: V2) -> float { v.x }\n\
              fn other() -> float { foo(V2 { x: 2.0, y: 3.0 }) }\n\
              fn use_ref() -> float {\n\
                let f = foo\n\
                f(V2 { x: 1.0, y: 2.0 })\
              }";
    check_edit_with(crate::common::ws_with_stdlib, v1, v2, &["use_ref"]);
}

#[test]
fn an_overlay_reemits_a_definition_after_a_scalar_abi_width_flip() {
    let v1 = "value V2 { x: float, y: float }\n\
              fn foo(v: V2) -> float { v.x }\n\
              fn other() -> float { foo(V2 { x: 2.0, y: 3.0 }) }\n\
              fn use_ref() -> float { other() }";
    let v2 = "value V2 { x: float, y: float }\n\
              fn foo(v: V2) -> float { v.x }\n\
              fn other() -> float { foo(V2 { x: 2.0, y: 3.0 }) }\n\
              fn use_ref() -> float {\n\
                let f = foo\n\
                f(V2 { x: 1.0, y: 2.0 })\
              }";
    let ws1 = crate::common::ws_with_stdlib(v1);
    let mid = skull::user_module_id(&ws1).unwrap();
    let (full1, mut cache) = emit_full_global(&ws1, mid).expect("emit_full v1");
    assert!(full1.lua.contains("function foo_"), "v1 must emit foo");

    let ws2 = crate::common::ws_with_stdlib(v2);
    let overlay = emit_overlay(&ws2, &mut cache, &[body_id(&ws2, "use_ref")]);
    assert!(
        overlay.lua.contains("function foo_") && overlay.lua.contains("function use_ref"),
        "the ABI-width flip must re-emit foo and its caller in the overlay:\n{}",
        overlay.lua
    );
}

/// The overlay carries a NON-GENERIC LIBRARY body the graph added for the edit
/// (graph-delta overlay construction).
///
/// Its two siblings above both add a SPECIALIZATION, and those reach the overlay
/// through the graph-added chunk path, which builds the chunk and files it in
/// `changed_since_full`. This is the arm that has no other route.
///
/// The subject must live in a LIBRARY module, and that is the whole reason this
/// fixture uses a stdlib fn: `is_library_module` makes the compiled unit's own
/// sources roots, so a fn in the fixture text is emitted whether or not anything
/// calls it and could never be graph-added. `is_ascii_ws` is a plain
/// `@pure` stdlib fn -- with inlining off it is emitted standalone -- and no
/// trivial program reaches it.
///
/// Both populations are asserted in the same run: it must be ABSENT from the
/// full build, or "the overlay adds it" is satisfied by a definition that was
/// already there.
#[test]
fn an_overlay_carries_a_graph_added_library_body() {
    let v1 = "fn use_it(c: int) -> bool { return c == 32 }\n\
              fn main() { print(\"{use_it(9)}\") }";
    let v2 = "fn use_it(c: int) -> bool { return is_ascii_ws(c) }\n\
              fn main() { print(\"{use_it(9)}\") }";

    let ws1 = crate::common::ws_with_stdlib(v1);
    let mid = skull::user_module_id(&ws1).unwrap();
    let (full, mut cache) = emit_full_global(&ws1, mid).expect("emit_full v1");
    let defines =
        |lua: &str| lua.lines().any(|l| l.starts_with("function is_ascii_ws_int("));
    assert!(
        !defines(&full.lua),
        "control: v1 must not define `is_ascii_ws`, or the overlay assertion below \
         is about a definition that already existed"
    );

    let ws2 = crate::common::ws_with_stdlib(v2);
    let overlay = emit_overlay(&ws2, &mut cache, &[body_id(&ws2, "use_it")]);
    assert!(
        overlay.lua.contains("is_ascii_ws_int("),
        "control: the edited body must actually call it:\n{}",
        overlay.lua
    );
    assert!(
        defines(&overlay.lua),
        "the overlay must DEFINE the library body the edit made reachable, or the \
         reload calls a nil global:\n{}",
        overlay.lua
    );
}

/// A graph-added specialization whose user body fails construction must make
/// both reload paths fail closed.
#[test]
fn an_overlay_rejects_a_failed_graph_added_body() {
    let common = "nobox value NV2 { x: float, y: float }\n\
                  fn bad<T>(xs: [T]) -> int {\n\
                  \x20   let t = (xs[0], 1.0)\n\
                  \x20   print(\"{t}\")\n\
                  \x20   return 0\n\
                  }\n";
    let v1 = common.to_string() + "fn use_it() -> int { return 0 }";
    let v2 = common.to_string()
        + "fn use_it() -> int {\n\
           \x20   let xs: [NV2] = [NV2 { x: 1.0, y: 2.0 }]\n\
           \x20   return bad(xs)\n\
           }";

    let ws1 = crate::common::ws_with_stdlib(&v1);
    let mid = skull::user_module_id(&ws1).unwrap();
    let (full, mut cache) = emit_full_global(&ws1, mid).expect("emit_full v1");
    assert!(!full.lua.contains("function bad_"), "v1 must not emit the specialization");

    let ws2 = crate::common::ws_with_stdlib(&v2);
    let err = match emit_overlay_result(&ws2, &mut cache, &[body_id(&ws2, "use_it")]) {
        Ok(_) => panic!("a failed graph-added construction must trigger FullRequired"),
        Err(err) => err,
    };
    assert!(
        err.message.contains("nobox") && err.message.contains("bad"),
        "the overlay failure must identify the graph-added construction: {}",
        err.message
    );
}

#[test]
fn graph_added_construction_failure_is_propagated_by_incremental() {
    let common = "nobox value NV2 { x: float, y: float }\n\
                  fn bad<T>(xs: [T]) -> int {\n\
                  \x20   let t = (xs[0], 1.0)\n\
                  \x20   print(\"{t}\")\n\
                  \x20   return 0\n\
                  }\n";
    let v1 = common.to_string() + "fn use_it() -> int { return 0 }";
    let v2 = common.to_string()
        + "fn use_it() -> int {\n\
           \x20   let xs: [NV2] = [NV2 { x: 1.0, y: 2.0 }]\n\
           \x20   return bad(xs)\n\
           }";

    let ws1 = crate::common::ws_with_stdlib(&v1);
    let mid = skull::user_module_id(&ws1).unwrap();
    let ws2 = crate::common::ws_with_stdlib(&v2);
    let full_err = match emit_full(&ws2, skull::user_module_id(&ws2).unwrap()) {
        Ok(_) => panic!("a full build must reject the failed graph-added construction"),
        Err(err) => err,
    };
    assert!(
        full_err.message.contains("nobox") && full_err.message.contains("bad"),
        "the full-build failure must identify the rejected specialization: {}",
        full_err.message
    );
    let (_, mut cache) = emit_full(&ws1, mid).expect("emit_full v1");
    let err = match emit_incremental(&ws2, &mut cache, &[body_id(&ws2, "use_it")]) {
        Ok(_) => panic!("a failed graph-added construction must not be printed and ignored"),
        Err(err) => err,
    };
    assert!(
        err.message.contains("nobox") && err.message.contains("bad"),
        "the propagated error must identify the failed graph-added construction: {}",
        err.message
    );
}

/// ONE liveness answer decides both kinds of emitted body.
///
/// The renderer no longer discovers or builds: it orders text the walk already
/// reached. So a specialized body reaching a FURTHER specialization has to come
/// out of the walk, and if the walk stopped carrying that edge the build fails
/// loudly (`no liveness walk reached it`) rather than quietly rebuilding it in a
/// second place. Measured on application: cutting the edge fires that rejection.
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
    let def = "function first_arr_T_spec_a_n2_V2_0_z(";
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
    let v1 = "struct Vec<T> { _data: any, len: int }\n\
              nobox value P { x: float, y: float }\n\
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

/// An edit that introduces a NEW specialization demand must MATERIALIZE it
/// (mono cut C+D, the item B\* left open and D-1 absorbs).
///
/// Invalidation and graph addition are different failures. The sibling above edits a
/// callee whose specializations already exist, so the cache knows the key set and
/// only has to re-emit; here the edit is in the CALLER and produces a key the
/// cache has never seen. A cache that only re-emits what it already holds is
/// green on that one and silently short a definition on this one -- the caller
/// would then spell a specialized symbol nothing defines.
///
/// The graph update now reports the new instance and the cache builds it from
/// the graph's substitution and bound tree before overlay assembly. The same
/// `assemble` path still remains the incremental oracle for parity.
///
/// Asserted on the incremental output's own definition COUNT as well as against
/// a fresh build. Equality alone would be satisfied by two builds that both lack
/// the new body, and the count alone would not catch a stale one.
#[test]
fn an_edit_that_introduces_a_new_specialization_demand_builds_from_graph_delta() {
    let types = "value P { x: float, y: float }\n\
                 value Q { a: float, b: float, c: float }\n\
                 fn tally_of<T>(xs: [T]) -> T { return xs[0] }\n";
    let v1 = types.to_string()
        + "fn use_it() -> float {\n\
           \x20   let ps: [P] = [P { x: 1.0, y: 0.0 }]\n\
           \x20   return tally_of(ps).x\n\
           }";
    let v2 = types.to_string()
        + "fn use_it() -> float {\n\
           \x20   let ps: [P] = [P { x: 1.0, y: 0.0 }]\n\
           \x20   let qs: [Q] = [Q { a: 1.0, b: 2.0, c: 3.0 }]\n\
           \x20   return tally_of(ps).x + tally_of(qs).a\n\
           }";
    let specs_in = |src: &str| {
        let ws = crate::common::ws_with_stdlib(src);
        let mid = skull::user_module_id(&ws).unwrap();
        emit_full(&ws, mid)
            .expect("emit_full")
            .0
            .lua
            .lines()
            .filter(|l| l.starts_with("function tally_of_") && l.contains("_spec_"))
            .count()
    };
    assert_eq!(specs_in(&v1), 1, "v1 must hold exactly one SPECIALIZED `tally_of`");
    assert_eq!(
        specs_in(&v2),
        2,
        "v2 must hold TWO, or the edit introduces no new demand and this proves nothing"
    );
    check_edit_with(crate::common::ws_with_stdlib, &v1, &v2, &["use_it"]);

    // The direct arm: the INCREMENTAL assembly holds both specialized bodies.
    let ws1 = crate::common::ws_with_stdlib(&v1);
    let mid1 = skull::user_module_id(&ws1).unwrap();
    let (_, mut cache) = emit_full(&ws1, mid1).expect("emit_full v1");
    let ws2 = crate::common::ws_with_stdlib(&v2);
    let reinferred = vec![body_id(&ws2, "use_it")];
    let incremental = emit_incremental(&ws2, &mut cache, &reinferred).expect("emit_incremental");
    let defs = incremental
        .lua
        .lines()
        .filter(|l| l.starts_with("function tally_of_") && l.contains("_spec_"))
        .count();
    assert_eq!(
        defs, 2,
        "the incremental assembly must materialize the specialization the edit introduced"
    );
}

/// A standalone `Iterator`-bounded body survives an incremental REBUILD
/// (mono cut C+D step D-1).
///
/// Its placeholders are resolved from the demand's tree, not from the body, so a
/// rebuild that has the body and not the tree cannot re-emit it -- and that is
/// precisely what an incremental rebuild does: it re-lowers from the declaration
/// under a chunk key. The tree therefore lives in the cache beside the chunk
/// key, and this is the arm that reads it back.
///
/// Byte identity cannot see any of this. It compares fresh full builds and never
/// performs an edit, so the tree could be dropped on the cache path and every
/// artifact would still match.
///
/// Runs with inlining DISABLED, which is where a bounded body has no splice to
/// fall back on -- and is what E1 does by construction.
#[test]
fn a_standalone_bounded_body_survives_an_incremental_rebuild() {
    let _inline = crate::common::InlineOffGuard::on();
    let _g = crate::common::AblationGuard::on();
    let v1 = "fn total<I: Iterator<int>>(it: I) -> int {\n\
              \x20   let s = 0\n\
              \x20   for v in it { s = s + v }\n\
              \x20   return s\n\
              }\n\
              fn use_it() -> int {\n\
              \x20   let xs: [int] = [1, 2, 3]\n\
              \x20   return total(xs.iter())\n\
              }";
    let v2 = v1.replace("let s = 0", "let s = 100");

    let ws1 = crate::common::ws_with_stdlib(v1);
    let mid1 = skull::user_module_id(&ws1).unwrap();
    let (full1, mut cache) = emit_full(&ws1, mid1).expect("emit_full v1");
    // The population, in the same run: without a standalone bounded definition
    // in v1 the rebuild below has nothing to lose and the arm proves nothing.
    let standalone = |lua: &str| {
        crate::common::definitions_matching(lua, "_spec_")
            .into_iter()
            .filter(|name| name.starts_with("total_"))
            .count()
    };
    assert_eq!(standalone(&full1.lua), 1, "v1 must emit ONE standalone bounded body");

    let ws2 = crate::common::ws_with_stdlib(&v2);
    let reinferred = vec![body_id(&ws2, "total")];
    let incremental = emit_incremental(&ws2, &mut cache, &reinferred).expect("emit_incremental");
    assert_eq!(
        standalone(&incremental.lua),
        1,
        "the rebuilt bounded body is gone -- its demand tree did not survive the cache"
    );
    let fresh = emit_full(&ws2, skull::user_module_id(&ws2).unwrap()).expect("emit_full v2").0;
    assert_eq!(
        incremental.lua, fresh.lua,
        "incremental assembly != fresh emit after editing a bounded body"
    );
}

/// **An edit that REMOVES the last call to an instantiation must prune it**,
/// leaving no stale definition live -- the direction the graph-addition arms above do
/// not cover.
///
/// The sibling declaration instance is what makes it a per-INSTANCE question
/// rather than a per-declaration one: `tally_of<P>` stays reached throughout, so
/// an implementation that invalidated by DECLARATION would either keep both or
/// drop both, and only one is correct.
///
/// Both populations are asserted from fresh builds first, in the same run: v1
/// must hold two specializations and v2 exactly one, or the edit removes nothing
/// and the arm proves nothing.
#[test]
fn an_edit_that_removes_the_last_call_to_an_instantiation_prunes_it() {
    let types = "value P { x: float, y: float }\n\
                 value Q { a: float, b: float, c: float }\n\
                 fn tally_of<T>(xs: [T]) -> T { return xs[0] }\n";
    let with_both = types.to_string()
        + "fn use_it() -> float {\n\
           \x20   let ps: [P] = [P { x: 1.0, y: 0.0 }]\n\
           \x20   let qs: [Q] = [Q { a: 1.0, b: 2.0, c: 3.0 }]\n\
           \x20   return tally_of(ps).x + tally_of(qs).a\n\
           }";
    let with_one = types.to_string()
        + "fn use_it() -> float {\n\
           \x20   let ps: [P] = [P { x: 1.0, y: 0.0 }]\n\
           \x20   return tally_of(ps).x\n\
           }";
    let specs_in = |lua: &str| {
        lua.lines().filter(|l| l.starts_with("function tally_of_") && l.contains("_spec_")).count()
    };
    let fresh_specs = |src: &str| {
        let ws = crate::common::ws_with_stdlib(src);
        let mid = skull::user_module_id(&ws).unwrap();
        specs_in(&emit_full(&ws, mid).expect("emit_full").0.lua)
    };
    assert_eq!(fresh_specs(&with_both), 2, "v1 must hold TWO specialized `tally_of`");
    assert_eq!(
        fresh_specs(&with_one),
        1,
        "v2 must hold ONE -- the sibling instance stays reached, so this is per-instance"
    );

    check_edit_with(crate::common::ws_with_stdlib, &with_both, &with_one, &["use_it"]);

    // The direct arm. `check_edit_with` compares against a fresh emit, which is
    // the strongest oracle available, but it cannot say WHICH of the two
    // instances survived -- both counts moving together would still match.
    let ws1 = crate::common::ws_with_stdlib(&with_both);
    let mid1 = skull::user_module_id(&ws1).unwrap();
    let (_, mut cache) = emit_full(&ws1, mid1).expect("emit_full v1");
    let ws2 = crate::common::ws_with_stdlib(&with_one);
    let reinferred = vec![body_id(&ws2, "use_it")];
    let incremental = emit_incremental(&ws2, &mut cache, &reinferred).expect("emit_incremental");
    assert_eq!(
        specs_in(&incremental.lua),
        1,
        "the incremental assembly kept a definition nothing calls any more:\n{}",
        incremental.lua
    );
    // ...and it is the one still called, not merely one of the two.
    let ws_one = crate::common::ws_with_stdlib(&with_one);
    let mid_one = skull::user_module_id(&ws_one).unwrap();
    let fresh_lua = emit_full(&ws_one, mid_one).expect("emit_full v2").0.lua;
    let surviving = |lua: &str| {
        lua.lines()
            .filter(|l| l.starts_with("function tally_of_") && l.contains("_spec_"))
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        surviving(&incremental.lua),
        surviving(&fresh_lua),
        "the surviving instance is not the one a fresh build keeps"
    );
}

#[test]
fn changed_call_graph_reuses_the_maintained_scalar_context() {
    let v1 = "fn helper() -> int { 3 }\ninline(lua) fn extra() -> int = \"return 7\"\nfn main() { let x = helper() }";
    let v2 = v1.replace("{ 3 }", "{ extra() }");
    let ws1 = ws_with_stdlib(v1);
    let mid = skull::user_module_id(&ws1).unwrap();
    let (_, mut cache) = emit_full(&ws1, mid).expect("initial emit");
    let ws2 = ws_with_stdlib(&v2);
    let incremental = emit_incremental(&ws2, &mut cache, &[body_id(&ws2, "helper")]).expect("incremental emit");
    let stats = cache.repair_stats();
    assert!(cache.lua_view().publisher_lanes.keys().any(|i|
        ws2.module(i.func.module).functions[i.func.func].name.as_str() == "extra"),
        "the changed call graph must reach a foreign publisher");
    assert!(stats.artifact_index_rows_examined > 0, "no artifact repair: {stats:?}");
    assert_eq!(stats.artifact_scalar_context_builds, 0, "artifact repair rescanned the workspace: {stats:?}");
    let fresh = emit_full(&ws2, skull::user_module_id(&ws2).unwrap()).expect("fresh emit").0;
    assert_eq!(incremental.lua, fresh.lua);
}

#[test]
fn coroutine_frame_nil_stores_share_full_incremental_and_overlay_emission() {
    let v1 = r#"
enum FrameYield { Empty, Pair(int, int) }
co fn stream() -> Coroutine<FrameYield> {
    yield FrameYield::Pair(1, 2)
    yield FrameYield::Empty
}
inline(lua) fn slots_empty(c: Coroutine<FrameYield>) -> bool = """
    return c.__y1 == nil and c.__y2 == nil
"""
@nomangle fn sample(expected: int) -> bool {
    let c = stream()
    let first = c.resume() == FrameYield::Pair(expected, 2)
    let second = c.resume() == FrameYield::Empty
    return first && second && slots_empty(c)
}
"#;
    let v2 = v1.replace("Pair(1, 2)", "Pair(3, 2)");
    check_edit_with(ws_with_stdlib, v1, &v2, &["stream"]);
    let ws1 = ws_with_stdlib(v1);
    let (full, mut cache) = emit_full_global(&ws1, skull::user_module_id(&ws1).unwrap()).unwrap();
    let ws2 = ws_with_stdlib(&v2);
    let overlay = emit_overlay(&ws2, &mut cache, &[body_id(&ws2, "stream")]);
    assert!(full.lua.contains(" ~= nil or "), "full frame stores must guard absent nil writes");
    assert!(overlay.lua.contains(" ~= nil or "), "overlay frame stores must use the same guard");
    let run = format!("{}\nprint(sample(1))\n{}\nprint(sample(3))\n", full.lua, overlay.lua);
    assert_eq!(crate::common::run_luajit(&run, "overlay_frame_nil_stores"), "true\ntrue");
}

/// A `co fn` with lambdas in both its step region and a detached defer region
/// computes one lambda topology per region under a single owner. A body-only
/// edit elsewhere must still overlay: the per-owner topology the compatibility
/// gate compares has to be one value per owner, not one per region.
#[test]
fn body_only_edit_survives_multi_region_coroutine_topology() {
    let v1 = r#"
enum Ev { Tick(int) }
global LOG: int = 0
fn note(x: int) { LOG = x }
fn apply(f: fn(int) -> int, x: int) -> int { return f(x) }
co fn worker(seed: int) -> Coroutine<Ev> {
    defer {
        note(apply((v) => v + seed, 1))
    }
    let base = apply((v) => v + 1, seed)
    yield Ev::Tick(base)
}
fn probe() -> int { return 1 }
fn main() {
    let c = worker(0)
    let _ = c.resume()
    let _ = probe()
}
"#;
    let v2 = v1.replace("fn probe() -> int { return 1 }", "fn probe() -> int { return 2 }");
    let ws1 = ws_with_stdlib(v1);
    let mid = skull::user_module_id(&ws1).unwrap();
    let (_full, mut cache) = emit_full_global(&ws1, mid).expect("emit_full v1");
    let ws2 = ws_with_stdlib(&v2);
    emit_overlay(&ws2, &mut cache, &[body_id(&ws2, "probe")]);
}

/// A body-only edit in a defer region must overlay before adding a lambda to
/// that region refuses. Otherwise a gate rejecting every multi-region
/// coroutine would satisfy the negative check without comparing the edit.
#[test]
fn lambda_insertion_in_defer_region_still_refuses_overlay() {
    let v1 = r#"
enum Ev { Tick(int) }
global LOG: int = 0
fn note(x: int) { LOG = x }
fn apply(f: fn(int) -> int, x: int) -> int { return f(x) }
co fn worker(seed: int) -> Coroutine<Ev> {
    defer {
        note(apply((v) => v + seed, 1))
    }
    let base = apply((v) => v + 1, seed)
    yield Ev::Tick(base)
}
fn probe() -> int { return 1 }
fn main() {
    let c = worker(0)
    let _ = c.resume()
    let _ = probe()
}
"#;
    let compatible = v1.replace(
        "        note(apply((v) => v + seed, 1))",
        "        note(apply((v) => v + seed, 3))",
    );
    let v2 = compatible.replace(
        "        note(apply((v) => v + seed, 3))",
        "        note(apply((v) => v + seed, 3))\n        note(apply((v) => v - seed, 2))",
    );
    let ws1 = ws_with_stdlib(v1);
    let mid = skull::user_module_id(&ws1).unwrap();
    let (_full, mut cache) = emit_full_global(&ws1, mid).expect("emit_full v1");
    let ws_compatible = ws_with_stdlib(&compatible);
    emit_overlay_result(&ws_compatible, &mut cache, &[body_id(&ws_compatible, "worker")])
        .expect("a body-only defer edit must overlay before testing lambda insertion");
    let ws2 = ws_with_stdlib(&v2);
    let err = match emit_overlay_result(&ws2, &mut cache, &[body_id(&ws2, "worker")]) {
        Ok(_) => panic!("inserting a lambda into a region must refuse the overlay"),
        Err(err) => err,
    };
    assert!(
        err.message.contains("CodeRef compatibility") && err.message.contains("safe restart"),
        "the refusal must stay the compatibility restart: {}",
        err.message
    );
}

const SHARED_ABI_SLOTS: &str = r#"
@lifecycle(phase="update", owner="session")
deco init tick {}
@tick fn first() {}
@tick fn second() {}
@tick fn third() {}
fn main() {}
"#;

#[test]
fn code_slot_staging_names_each_distinct_call_abi_once() {
    let ws = ws_of(SHARED_ABI_SLOTS);
    let (full, _cache) = emit_full_global(&ws, skull::user_module_id(&ws).unwrap()).unwrap();
    let abis: std::collections::BTreeSet<&str> = full
        .code_artifact
        .entries
        .values()
        .filter(|entry| entry.escaping)
        .map(|entry| entry.compatibility.call_abi.as_str())
        .collect();
    let staged = full.lua.lines().filter(|line| line.contains("__rock_code_slot_stage(")).count();
    assert!(
        staged > abis.len(),
        "fixture must share a call ABI across slots: {staged} slots, {} ABIs",
        abis.len()
    );
    for abi in abis {
        let literal = skull_codegen_lua::lua_string(abi);
        assert_eq!(full.lua.matches(literal.as_str()).count(), 1, "ABI {literal} must be named once");
    }
}

#[test]
fn a_shared_call_abi_stays_compatible_across_an_overlay() {
    let v2 = SHARED_ABI_SLOTS.replace("@tick fn first() {}", "@tick fn first() { let x = 1 }");
    let ws1 = ws_of(SHARED_ABI_SLOTS);
    let (full, mut cache) = emit_full_global(&ws1, skull::user_module_id(&ws1).unwrap()).unwrap();
    let escaping: Vec<_> = full.code_artifact.entries.values().filter(|entry| entry.escaping).collect();
    let ws2 = ws_of(&v2);
    let overlay = emit_overlay(&ws2, &mut cache, &[body_id(&ws2, "first")]);
    assert!(overlay.lua.contains("__rock_code_slot_stage("), "the overlay must restage `first`");
    let run = format!(
        r#"{full}
local function snapshot()
  local rows = {{}}
  for key, slot in pairs(_G.__rock_code_slots) do rows[#rows + 1] = key .. "=" .. slot.abi end
  table.sort(rows)
  return table.concat(rows, "\n")
end
local before = snapshot()
{overlay}
assert(snapshot() == before, "the overlay changed a slot ABI")
print(before)
"#,
        full = full.lua,
        overlay = overlay.lua
    );
    let out = crate::common::run_luajit(&run, "shared_abi_overlay");
    assert_eq!(out.lines().count(), escaping.len(), "every escaping CodeRef has a slot:\n{out}");
    for entry in escaping {
        let abi = format!("={}", entry.compatibility.call_abi);
        assert!(out.lines().any(|row| row.ends_with(&abi)), "no slot carries ABI {abi}:\n{out}");
    }
}

#[test]
fn body_overlay_derives_only_edited_fingerprint_code_rows() {
    let mut source = String::from("fn helper() -> int { return 1 }\nfn main() { let _ = helper() }\n");
    for index in 0..40 {
        source.push_str(&format!("fn unrelated{index}() -> int {{ return {index} }}\n"));
    }
    let original = ws_of(&source);
    let (_, mut cache) = emit_full_global(&original, skull::user_module_id(&original).unwrap()).unwrap();
    for value in [2, 3, 4] {
        let changed = ws_of(&source.replacen("return 1 }", &format!("return {value} }}"), 1));
        let overlay = emit_overlay(&changed, &mut cache, &[body_id(&changed, "helper")]);
        let stats = cache.repair_stats();
        assert!(overlay.code_artifact.entries.len() > 40);
        assert!(stats.fingerprint_code_rows_derived > 0, "the edit must update its fingerprint row");
        assert!(stats.fingerprint_code_rows_derived < 4,
            "the fingerprint rederived unrelated code rows: {stats:?}");
        let fresh = emit_full_global(&changed, skull::user_module_id(&changed).unwrap()).unwrap().0;
        assert_eq!(overlay.artifact_fingerprint, fresh.artifact_fingerprint);
    }
}

#[test]
fn fingerprint_code_rows_include_new_body_specializations() {
    let original = "fn identity<T>(value: T) -> T { return value }\nfn main() { let _ = identity(1) }";
    let changed = original.replace("let _ = identity(1)", "let _ = identity(1)\nlet _ = identity(true)");
    let ws1 = ws_of(original);
    let (baseline, mut cache) = emit_full_global(&ws1, skull::user_module_id(&ws1).unwrap()).unwrap();
    let ws2 = ws_of(&changed);
    let overlay = emit_overlay(&ws2, &mut cache, &[body_id(&ws2, "main")]);
    assert!(overlay.code_artifact.entries.len() > baseline.code_artifact.entries.len());
    assert_ne!(overlay.artifact_fingerprint, baseline.artifact_fingerprint);
}

#[test]
fn fingerprint_code_rows_survive_rollback() {
    let original = "fn helper() -> int { return 1 }\nfn main() { let _ = helper() }";
    let ws1 = ws_of(original);
    let (_, mut cache) = emit_full_global(&ws1, skull::user_module_id(&ws1).unwrap()).unwrap();
    let ws2 = ws_of(&original.replace("return 1", "return 2"));
    let edited = [body_id(&ws2, "helper")];
    cache.begin_body_edit_transaction(&edited, &[]);
    let _ = emit_overlay(&ws2, &mut cache, &edited);
    cache.rollback_body_edit(&ws1).expect("rollback");
    for value in [3, 4] {
        let changed = ws_of(&original.replace("return 1", &format!("return {value}")));
        let overlay = emit_overlay(&changed, &mut cache, &[body_id(&changed, "helper")]);
        let fresh = emit_full_global(&changed, skull::user_module_id(&changed).unwrap()).unwrap().0;
        assert_eq!(overlay.artifact_fingerprint, fresh.artifact_fingerprint);
    }
}

#[test]
fn flat_ref_callee_body_overlay_updates_the_live_variant() {
    crate::common::require_luajit();
    let v1 = r#"
value Cell { v: int }
global xs: [Cell] = []
fn apply(c: ref Cell, delta: int) -> int { c.v += delta
return c.v }
@nomangle fn seed() { xs.push(Cell { v: 10 }) }
@nomangle fn snapshot() -> int { return xs[0].v }
@nomangle fn tick(delta: int) -> bool {
    let before = xs[0].v
    let seen = apply(xs[0], delta)
    return seen != before && xs[0].v == seen
}
"#;
    let v2 = v1.replace("c.v += delta", "c.v -= delta");
    let ws1 = ws_with_stdlib(v1);
    let (full, mut cache) = emit_full(&ws1, skull::user_module_id(&ws1).unwrap()).unwrap();
    let ws2 = ws_with_stdlib(&v2);
    let overlay = emit_overlay(&ws2, &mut cache, &[body_id(&ws2, "apply")]);
    let run = format!(
        "{}\nseed()\nassert(tick(2))\nlocal before = snapshot()\n{}\nassert(snapshot() == before)\nassert(tick(3))\nassert(snapshot() < before)\nprint('ok')\n",
        full.lua, overlay.lua,
    );
    assert_eq!(crate::common::run_luajit(&run, "flat_ref_body_overlay"), "ok");
}

#[test]
fn flat_ref_signature_change_refuses_an_overlay() {
    let v1 = r#"
value Cell { v: int }
value Outer { cell: Cell, w: int }
fn apply(c: ref Cell) -> int { c.v += 1
return c.v }
@nomangle fn tick() -> int {
    let xs: [Cell] = []
    xs.push(Cell { v: 10 })
    return apply(xs[0])
}
"#;
    let v2 = v1.replace("let xs: [Cell] = []", "let xs: [Outer] = []")
        .replace("xs.push(Cell { v: 10 })", "xs.push(Outer { cell: Cell { v: 10 }, w: 20 })")
        .replace("apply(xs[0])", "apply(xs[0].cell)");
    let ws1 = ws_with_stdlib(v1);
    let (_, mut cache) = emit_full(&ws1, skull::user_module_id(&ws1).unwrap()).unwrap();
    let ws2 = ws_with_stdlib(&v2);
    let err = match emit_overlay_result(&ws2, &mut cache, &[body_id(&ws2, "tick")]) {
        Ok(_) => panic!("changing a flat variant's element/prefix must request restart"),
        Err(err) => err,
    };
    assert!(err.message.contains("CodeRef compatibility") && err.message.contains("safe restart"), "{}", err.message);
}

#[test]
fn flat_ref_abi_uses_logical_layout() {
    let ws = ws_with_stdlib(r#"
value Cell { v: int }
fn bump(c: ref Cell) -> int { c.v += 1
return c.v }
fn sample() -> int {
    let xs: [Cell] = []
    xs.push(Cell { v: 10 })
    return bump(xs[0])
}
"#);
    let (full, _) = emit_full(&ws, skull::user_module_id(&ws).unwrap()).unwrap();
    let flat_abis: Vec<_> = full.code_artifact.entries.iter()
        .map(|(_, entry)| entry.compatibility.call_abi.as_str())
        .filter(|abi| abi.contains("|flat "))
        .collect();
    assert!(!flat_abis.is_empty(), "the production artifact must contain a flat variant");
    assert!(flat_abis.iter().all(|abi| !abi.contains("RepId")), "call ABI must not contain build-scoped arena handles: {flat_abis:?}");
}
