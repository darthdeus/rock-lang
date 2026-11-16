//! The stdlib is a first-class part of the workspace. The session keys the
//! warm stdlib by the loader's stdlib ids (which make it the std package to
//! bone), the editor sees its real canonical paths, and its source is seeded
//! into the project's texts, so it behaves like any other file.

use crate::fixture::{Project, hover_markup};
use bone_query::BoneWorkspace;
use lsp_types::{DiagnosticSeverity, Position};

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

#[test]
fn a_project_over_the_stdlib_navigates_and_lints_it_like_its_own_files() {
    let src =
        "fn main() -> int {\n  let xs = [1]\n  xs.push(2)\n  assert_true(true)\n  xs.len()\n}\n";
    let project = Project::new(&[
        ("stick.toml", "[package]\nname = \"st\"\n"),
        ("main.rock", src),
    ]);
    let main = project.path("main.rock");
    let (bone, per_file) = project.open("main.rock");

    let def = bone
        .definition(&main, Position::new(2, 5))
        .expect("push resolves");
    assert!(def.uri.as_str().ends_with("unified.rock"), "{:?}", def.uri);

    // The stdlib source is available to slice the real default value.
    let h = hover_markup(&bone, &main, Position::new(3, 4)).unwrap_or_default();
    assert!(h.contains("message: string = \"Expected true\""), "{h:?}");

    let hits = bone.workspace_symbols("vec_push");
    assert!(
        hits.iter().any(|s| s.name == "vec_push"),
        "workspace symbol"
    );

    // Std-only primitives (`@frame_ref_source` in arena.rock) are admitted by
    // the file's stdlib identity, and stdlib functions are excluded from the
    // bin's whole-project unused_fn walk.
    let problems: Vec<_> = per_file
        .iter()
        .flat_map(|(f, d)| d.iter().map(move |x| (f, x)))
        .filter(|(f, d)| {
            d.severity == Some(DiagnosticSeverity::ERROR)
                || (f.ends_with("unified.rock") || f.ends_with("vec.rock"))
                    && d.message.contains("unused")
        })
        .map(|(f, d)| (f.clone(), d.message.clone()))
        .collect();
    assert!(problems.is_empty(), "{problems:?}");

    // Whatever id the session keys the stdlib by, every file `update`
    // reports is a real file the editor can open.
    let unified = stdlib_path("unified.rock");
    assert!(
        per_file.iter().any(|(f, _)| f == &unified),
        "unified.rock under its real path"
    );
    for (f, _) in &per_file {
        assert!(std::path::Path::new(f).exists(), "`{f}` is not a file");
    }
}
