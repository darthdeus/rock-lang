//! A file reached via a non-canonical path spelling must resolve to the SAME
//! module as the canonical one -- not a second copy.
//!
//! On Windows this is the load-bearing case: project discovery canonicalizes
//! file paths to the `\\?\C:\...` extended-length form, but an LSP client sends
//! the URI-decoded `C:\...` spelling to `update`/queries. Keyed verbatim, the
//! same file lands under two keys -> the session loads `struct Camera` twice ->
//! "duplicate struct `Camera` across modules", and every read query misses the
//! module. (Found by the `test-windows` CI tier via `repro_camera_crash`.)
//!
//! Reproduced cross-platform here by injecting a `/./` segment, which
//! `canonicalize` collapses but a verbatim string key preserves.

use bone_query::BoneWorkspace;
use std::path::Path;

fn camera_fixture() -> (String, String, String) {
    let project = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/noncanonical_path");
    let camera = project.join("camera.rock");
    let canon = fs_err::canonicalize(&camera)
        .expect("fixture camera canonicalizes")
        .to_string_lossy()
        .to_string();
    let parent = camera.parent().expect("camera has a parent");
    let name = camera.file_name().expect("camera has a file name").to_string_lossy();
    let noncanon = format!("{}/./{name}", parent.display());
    let text = std::fs::read_to_string(camera).expect("fixture camera is readable");
    (canon, noncanon, text)
}

#[test]
fn noncanonical_path_does_not_duplicate_module() {
    let (canon, noncanon, text) = camera_fixture();

    let mut bone = BoneWorkspace::new();
    bone.load_project(&canon); // session keyed by the canonical path
    let diags = bone.update(&noncanon, text); // must reuse that module, not add a copy

    let dup: Vec<_> = diags
        .iter()
        .flat_map(|(_, d)| d)
        .filter(|d| d.message.contains("duplicate struct"))
        .collect();
    assert!(dup.is_empty(), "non-canonical path duplicated the module: {dup:?}");
    let via_canon = bone.semantic_tokens(&canon).data.len();
    let via_noncanon = bone.semantic_tokens(&noncanon).data.len();
    assert!(via_canon > 0, "canonical path should yield tokens");
    assert_eq!(via_noncanon, via_canon, "non-canonical path must resolve to the same module");
}

#[test]
fn read_queries_resolve_via_noncanonical_path() {
    // A read query (semantic_tokens) issued with a non-canonical path spelling
    // must find the module discovery keyed canonically -- else Windows LSP
    // hover/completion/tokens all silently return empty.
    let (canon, noncanon, text) = camera_fixture();

    let mut bone = BoneWorkspace::new();
    bone.load_project(&canon);
    let _ = bone.update(&canon, text);

    let via_canon = bone.semantic_tokens(&canon).data.len();
    let via_noncanon = bone.semantic_tokens(&noncanon).data.len();
    assert!(via_canon > 0, "canonical path should yield tokens");
    assert_eq!(via_noncanon, via_canon, "non-canonical path must resolve to the same module");
}
