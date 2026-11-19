//! A non-canonical path spelling (Windows `\\?\` vs `C:\`, or `/./`) must resolve to one module.

use bone_query::BoneWorkspace;
use std::path::Path;

#[test]
fn noncanonical_spelling_resolves_to_the_canonically_keyed_module() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/noncanonical_path");
    let canon = fs_err::canonicalize(dir.join("camera.rock"))
        .expect("fixture camera canonicalizes")
        .to_string_lossy()
        .to_string();
    let noncanon = format!("{}/./camera.rock", dir.display());
    let text = std::fs::read_to_string(&canon).expect("fixture camera is readable");

    // Editing through either spelling; read queries go through both.
    for edited in [&noncanon, &canon] {
        let mut bone = BoneWorkspace::new();
        bone.load_project(&canon);
        let diags = bone.update(edited, text.clone());
        let dup: Vec<_> = diags
            .iter()
            .flat_map(|(_, d)| d)
            .filter(|d| d.message.contains("duplicate struct"))
            .collect();
        assert!(dup.is_empty(), "edited via {edited}: {dup:?}");
        let via_canon = bone.semantic_tokens(&canon).data;
        assert!(
            !via_canon.is_empty(),
            "edited via {edited}: canonical tokens"
        );
        assert_eq!(
            bone.semantic_tokens(&noncanon).data,
            via_canon,
            "edited via {edited}"
        );
    }
}
