//! End-to-end regression for the bone LSP crash: typing a malformed `global x =`
//! at the top of a stone-package file (`camera.rock`) used to panic the server
//! with `index out of bounds: the len is 1 but the index is N` inside
//! `Workspace::module`.
//!
//! Mechanism: each intermediate keystroke (`g`, `gl`, ... `global x =`) makes the
//! parser mis-consume the file's own `struct Camera`, so a full rebuild fails
//! (`Unknown type name Camera`). The incremental LayoutOnly path STORES some of
//! those broken states, so when a later keystroke fails and the session reverts
//! one step (to a stored-but-not-full-rebuildable state), the recovery
//! `rebuild_users` failed too -- and a failed `rebuild_users` left the workspace
//! half-built. The deterministic unit guard for the half-built workspace is
//! `bone-incremental/tests/rebuild_users_atomicity.rs`; this test pins the full
//! LSP path that surfaced it.
//!
//! Uses the real `stone/engine/systems/camera.rock` because the trigger depends
//! on the parser's recovery interaction with that file's exact shape (a synthetic
//! stand-in failed to reproduce the LayoutOnly-stores-broken-text quirk).

use bone_query::BoneWorkspace;
use lsp_types::{Position, Range};

#[test]
fn typing_broken_global_into_camera_never_crashes() {
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf();
    let cam = repo.join("stone/engine/systems/camera.rock");
    let cam_s = cam.to_string_lossy().to_string();
    let cam_text = std::fs::read_to_string(&cam).expect("camera.rock is readable");

    let mut bone = BoneWorkspace::new();
    bone.load_project(&cam_s);
    let diags = bone.update(&cam_s, cam_text.clone());
    let cam_diags: usize =
        diags.iter().filter(|(p, _)| *p == cam_s).map(|(_, d)| d.len()).sum();
    assert_eq!(cam_diags, 0, "camera.rock must start clean (else this test no longer reproduces): {diags:?}");

    // Type `global x = ` char by char as a new second line, firing the per-edit
    // refresh queries nvim sends. Reaching the end without panicking is the
    // assertion (the bug panicked partway through).
    let lines: Vec<&str> = cam_text.split('\n').collect();
    let target = "global x = ";
    for i in 1..=target.len() {
        let line2 = &target[..i];
        let mut ls = lines.clone();
        ls.insert(1, line2);
        let _ = bone.update(&cam_s, ls.join("\n"));
        let _ = bone.semantic_tokens(&cam_s);
        let _ = bone.completion(&cam_s, Position::new(1, line2.len() as u32));
        let _ = bone.inlay_hints(
            &cam_s,
            Range { start: Position::new(0, 0), end: Position::new(80, 0) },
        );
        let _ = bone.diagnostics_for(&cam_s);
    }

    // Fixing the file restores a clean, queryable session -- it was never wedged.
    let mut ls = lines.clone();
    ls.insert(1, "global x: float = 1.0");
    let _ = bone.update(&cam_s, ls.join("\n"));
    let _ = bone.semantic_tokens(&cam_s);
}
