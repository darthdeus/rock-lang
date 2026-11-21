//! The stdlib behaves like any other workspace file: real canonical paths, seeded texts.

use bone_query::BoneWorkspace;

/// Canonical on-disk path of a stdlib file: what the editor sends and sees.
fn stdlib_path(name: &str) -> String {
    let dir = match rock_stdlib::stdlib_origin() {
        rock_stdlib::StdlibSource::Dir(dir) => dir,
        rock_stdlib::StdlibSource::Embedded => {
            panic!("the stdlib is built in; this test needs a stdlib directory")
        }
    };
    bone_query::canonicalize_lenient(&dir.join(name).to_string_lossy())
}

/// The opened buffer is the warm module, not a second copy that would report
/// `duplicate <ty> across modules` for every stdlib type.
#[test]
fn opening_a_stdlib_file_has_no_self_collision() {
    let path = stdlib_path("unified.rock");
    let src = std::fs::read_to_string(&path).expect("stdlib readable");
    let mut bone = BoneWorkspace::new();
    let per_file = bone.update(&path, src);
    let collides = |d: &lsp_types::Diagnostic| d.message.contains("across modules");
    let dups: Vec<_> = per_file
        .iter()
        .flat_map(|(_, d)| d)
        .filter(|d| collides(d))
        .collect();
    assert!(dups.is_empty(), "update: {dups:?}");
    let pulled = bone.diagnostics_for(&path);
    assert!(!pulled.iter().any(collides), "diagnostics_for: {pulled:?}");
}
