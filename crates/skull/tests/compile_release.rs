use crate::common::{name_matches, ws_with_stdlib};
use skull::mir_emit::{emit_full, emit_incremental, emit_workspace_mir};

const SOURCE: &str =
    "fn leaf() -> int { static value: int = 7; value } @nomangle fn main() -> int { leaf() }";

#[test]
fn one_shot_and_live_builds_emit_the_same_program() {
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
        .find(|(_, f)| name_matches(f.name, "leaf"))
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
fn one_shot_can_skip_the_artifact_fingerprint_without_changing_the_program() {
    let ws = ws_with_stdlib(SOURCE);
    let mid = *ws.module_ids.get(&ustr::Ustr::from("<test>")).unwrap();
    let full = emit_workspace_mir(&ws, mid).expect("one shot");
    assert!(full.artifact_fingerprint.is_some());
    let (skipped, _) = skull::mir_emit::FullEmit::one_shot()
        .without_fingerprint()
        .run(&ws, mid)
        .expect("one shot without fingerprint");
    assert_eq!(skipped.artifact_fingerprint, None);
    assert_eq!(skipped.lua, full.lua);
}
