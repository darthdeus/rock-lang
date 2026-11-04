use crate::common::ws_with_stdlib;
use skull::mir_emit::{emit_full, emit_incremental, emit_workspace_mir};

const SOURCE: &str =
    "fn leaf() -> int { static value: int = 7; value } @nomangle fn main() -> int { leaf() }";

#[test]
fn one_shot_releases_graph_and_chunks_with_identical_lua() {
    let ws = ws_with_stdlib(SOURCE);
    let mid = *ws.module_ids.get(&ustr::Ustr::from("<test>")).unwrap();
    let one_shot = emit_workspace_mir(&ws, mid).expect("one shot");
    let (live, cache) = emit_full(&ws, mid).expect("live build");
    assert_eq!(one_shot.lua, live.lua);
    assert_eq!(one_shot.artifact_fingerprint, live.artifact_fingerprint);
    assert_eq!(one_shot.compile_counts.chunks, live.compile_counts.chunks);
    assert_eq!(
        one_shot.compile_counts.dependency_edges,
        live.compile_counts.dependency_edges
    );
    assert!(one_shot.release_stats.graph_instances > 0);
    assert!(one_shot.release_stats.chunks > 0);
    assert_eq!(one_shot.release_stats.retained_dependency_edges, 0);
    assert_eq!(one_shot.release_stats.retained_static_keys, 0);
    assert_eq!(live.release_stats.graph_instances, 0);
    assert_eq!(live.release_stats.chunks, 0);
    assert!(live.release_stats.retained_dependency_edges > 0);
    assert!(live.release_stats.retained_static_keys > 0);
    assert!(!cache.graph().instances.is_empty());
}

#[test]
fn live_build_keeps_cache_for_reassembly_after_a_body_edit() {
    let ws = ws_with_stdlib(SOURCE);
    let mid = *ws.module_ids.get(&ustr::Ustr::from("<test>")).unwrap();
    let (_, mut cache) = emit_full(&ws, mid).expect("live build");
    let edited = ws_with_stdlib(&SOURCE.replace("= 7", "= 9"));
    let module = edited.module(mid);
    let (fid, _) = module
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == "leaf")
        .unwrap();
    let emit = emit_incremental(
        &edited,
        &mut cache,
        &[(mid, bone_semantic::hir::BodyOwner::Function(fid))],
    )
    .expect("edit");
    let fresh = emit_full(&edited, mid).expect("fresh").0;
    assert_eq!(emit.lua, fresh.lua);
}

#[test]
fn one_shot_releases_chunk_storage_after_definitions() {
    let emit = crate::common::emit_mir(SOURCE);
    assert!(emit.compile_counts.chunks > 0);
    assert_eq!(emit.release_stats.chunks, emit.compile_counts.chunks);
}

#[test]
fn one_shot_releases_concrete_plans_and_live_build_keeps_them() {
    let ws = ws_with_stdlib(SOURCE);
    let mid = *ws.module_ids.get(&ustr::Ustr::from("<test>")).unwrap();
    let one_shot = emit_workspace_mir(&ws, mid).expect("one shot");
    let live = emit_full(&ws, mid).expect("live build").0;
    assert!(one_shot.compile_counts.concrete_plans > 0);
    assert_eq!(
        one_shot.release_stats.concrete_plans,
        one_shot.compile_counts.concrete_plans
    );
    assert_eq!(live.release_stats.concrete_plans, 0);
    assert_eq!(one_shot.lua, live.lua);
}
