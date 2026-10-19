//! The LSP must treat the stdlib as a first-class part of the workspace:
//! opening a stdlib file must not collide with the warm copy, source-based
//! queries (default values) must work on stdlib functions, and stdlib symbols
//! must be searchable / navigable (workspace-symbols, ctags). The session keys
//! the warm stdlib by the loader's stdlib ids (which is what makes it the std
//! package to bone), the editor sees its REAL canonical paths, and its source
//! is seeded into the project's `texts`, so it behaves like any other file.

use bone_query::BoneWorkspace;
use lsp_types::{HoverContents, Position};

/// Canonical on-disk path of a stdlib file: what the editor sends and sees.
fn stdlib_path(name: &str) -> String {
    let dir = match rock_stdlib::stdlib_origin() {
        rock_stdlib::StdlibSource::Dir(dir) => dir,
        rock_stdlib::StdlibSource::Embedded => panic!("the stdlib is built in; this test needs a stdlib directory"),
    };
    bone_query::canonicalize_lenient(&dir.join(name).to_string_lossy())
}

/// A scratch project (no stick.toml) that calls a stdlib function.
fn project_calling_stdlib(src: &str) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("stick.toml"), "[package]\nname = \"st\"\n").unwrap();
    std::fs::write(dir.path().join("main.rock"), src).unwrap();
    let main = bone_query::canonicalize_lenient(&dir.path().join("main.rock").to_string_lossy());
    (dir, main)
}

#[test]
fn opening_a_stdlib_file_has_no_self_collision() {
    // Opening the real stdlib file (e.g. after goto-def jumps there) must NOT
    // report `duplicate <ty> across modules` for every stdlib type -- the opened
    // buffer IS the warm module, not a second copy.
    let path = stdlib_path("unified.rock");
    let src = std::fs::read_to_string(&path).expect("stdlib readable");
    let mut bone = BoneWorkspace::new();
    let per_file = bone.update(&path, src);
    let dups: Vec<_> = per_file
        .iter()
        .flat_map(|(_, d)| d)
        .filter(|d| d.message.contains("across modules"))
        .collect();
    assert!(dups.is_empty(), "opening stdlib self-collided: {dups:?}");
    assert!(
        !bone.diagnostics_for(&path).iter().any(|d| d.message.contains("across modules")),
        "diagnostics_for on stdlib self-collided: {:?}",
        bone.diagnostics_for(&path)
    );
}

#[test]
fn goto_def_into_stdlib_still_resolves() {
    // `xs.push(..)` must resolve to the stdlib file's real path (a regression
    // guard for the warm-module path change).
    let (_dir, main) = project_calling_stdlib("fn main() -> int {\n  let xs = [1]\n  xs.push(2)\n  xs.len()\n}\n");
    let mut bone = BoneWorkspace::new();
    let src = std::fs::read_to_string(&main).unwrap();
    bone.update(&main, src);
    // cursor on `push` (line 2, char 5).
    let def = bone.definition(&main, Position::new(2, 5)).expect("push resolves");
    let uri = def.uri.as_str();
    assert!(uri.ends_with("unified.rock"), "push must resolve into stdlib: {uri}");
}

#[test]
fn stdlib_fn_default_value_renders_in_hover() {
    // `assert_true(condition, message: string = "Expected true")` -- hovering the
    // call must show the real default value, not the `= ..` fallback, because the
    // stdlib source is now available for slicing.
    let (_dir, main) = project_calling_stdlib("fn main() {\n  assert_true(true)\n}\n");
    let mut bone = BoneWorkspace::new();
    let s = std::fs::read_to_string(&main).unwrap();
    bone.update(&main, s);
    // cursor on `assert_true` (line 1, char 4).
    let h = match bone.hover(&main, Position::new(1, 4)).map(|h| h.contents) {
        Some(HoverContents::Markup(m)) => m.value,
        _ => String::new(),
    };
    assert!(
        h.contains("message: string = \"Expected true\""),
        "stdlib default value must render, got: {h:?}"
    );
}

#[test]
fn stdlib_symbols_appear_in_workspace_symbols() {
    // A stdlib function (`vec_push`) must be findable by name so Ctrl-T / ctags
    // can jump to it.
    let (_dir, main) = project_calling_stdlib("fn main() -> int {\n  let xs = [1]\n  xs.push(2)\n  xs.len()\n}\n");
    let mut bone = BoneWorkspace::new();
    let s = std::fs::read_to_string(&main).unwrap();
    bone.update(&main, s);
    let hits = bone.workspace_symbols("vec_push");
    assert!(
        hits.iter().any(|s| s.name == "vec_push"),
        "vec_push must be a workspace symbol; got {:?}",
        hits.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
}

#[test]
fn stdlib_functions_are_not_flagged_unused_in_a_bin_project() {
    // The `unused_fn` lint runs whole-project for a bin. Stdlib functions have no
    // in-project caller, so a naive walk would flag hundreds of them. They must
    // be excluded (via warm_mids, not the old file-id prefix).
    let (_dir, main) = project_calling_stdlib("fn main() -> int {\n  let xs = [1]\n  xs.push(2)\n  xs.len()\n}\n");
    let mut bone = BoneWorkspace::new();
    let s = std::fs::read_to_string(&main).unwrap();
    let per_file = bone.update(&main, s);
    let stdlib_unused: Vec<_> = per_file
        .iter()
        .filter(|(f, _)| f.ends_with("unified.rock") || f.ends_with("vec.rock"))
        .flat_map(|(f, d)| d.iter().map(move |x| (f, &x.message)))
        .filter(|(_, m)| m.contains("unused"))
        .collect();
    assert!(
        stdlib_unused.is_empty(),
        "stdlib functions must not be flagged unused: {stdlib_unused:?}"
    );
}

#[test]
fn the_stdlib_builds_clean_in_the_editor() {
    // The editor must read the stdlib as the stdlib: std-only primitives
    // (`@frame_ref_source` in arena.rock) are admitted by the file's stdlib
    // identity, so a session that keys the stdlib by any other id reports its
    // own stdlib as broken.
    let (_dir, main) = project_calling_stdlib("fn main() -> int {\n  let xs = [1]\n  xs.push(2)\n  xs.len()\n}\n");
    let mut bone = BoneWorkspace::new();
    let s = std::fs::read_to_string(&main).unwrap();
    let per_file = bone.update(&main, s);
    let errors: Vec<_> = per_file
        .iter()
        .flat_map(|(f, d)| d.iter().map(move |x| (f, x)))
        .filter(|(_, d)| d.severity == Some(lsp_types::DiagnosticSeverity::ERROR))
        .map(|(f, d)| (f.clone(), d.message.clone()))
        .collect();
    assert!(errors.is_empty(), "a legacy project over the stdlib must build clean: {errors:?}");
}

#[test]
fn stdlib_diagnostics_are_published_under_the_stdlib_file_path() {
    // Whatever id the session keys the stdlib by, the editor sees the
    // stdlib's real path: every file `update` reports is a real file.
    let (_dir, main) = project_calling_stdlib("fn main() {}\n");
    let mut bone = BoneWorkspace::new();
    let s = std::fs::read_to_string(&main).unwrap();
    let per_file = bone.update(&main, s);
    let unified = stdlib_path("unified.rock");
    assert!(per_file.iter().any(|(f, _)| f == &unified), "unified.rock reported under its real path");
    for (f, _) in &per_file {
        assert!(std::path::Path::new(f).exists(), "`{f}` is not a file the editor can open");
    }
}
