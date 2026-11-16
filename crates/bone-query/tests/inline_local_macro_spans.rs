use crate::fixture::{Project, apply_edits, pos};
use bone_query::BoneWorkspace;
use lsp_types::DiagnosticSeverity;

#[test]
fn does_not_inline_a_local_read_from_a_macro_default_inside_a_loop() {
    // The definition-file use at bytes 29..30 overlaps the caller's loop span
    // at bytes 28..36; its file still makes it ineligible for a caller edit.
    let defs = "macro looped(rrrrrrr: expr = y) {\n    for i in 0..3 {\n        print(rrrrrrr)\n    }\n}\n";
    let src = "fn f(v: int) { let y = g(v)\nlooped()\n}\nfn g(v: int) -> int { v }\n";
    let project = Project::new(&[("defs.rock", defs), ("main.rock", src)]);
    let (defs_path, main_path) = (project.path("defs.rock"), project.path("main.rock"));
    let mut bone = BoneWorkspace::new();
    bone.update(&defs_path, defs.to_string());
    bone.update(&main_path, src.to_string());

    let cursor = pos(src, "let y", 4);
    assert!(
        bone.hover(&main_path, cursor).is_some(),
        "the body must type-check"
    );
    assert!(
        bone.inline_local(&main_path, cursor).is_none(),
        "the impure initializer must not move into the expanded loop"
    );
}

#[test]
fn does_not_inline_a_pure_local_read_from_a_macro_default() {
    let defs = "macro read_default_value(arg: expr = y) -> int { arg + 2 }\n";
    let src = "fn f() -> int {\n    let y = 1\n    return read_default_value()\n}\n";
    let project = Project::new(&[("defs.rock", defs), ("main.rock", src)]);
    let (defs_path, main_path) = (project.path("defs.rock"), project.path("main.rock"));
    let mut bone = BoneWorkspace::new();
    bone.update(&defs_path, defs.to_string());
    let diagnostics = bone.update(&main_path, src.to_string());
    assert!(
        diagnostics
            .iter()
            .flat_map(|(_, d)| d)
            .all(|d| { d.severity != Some(DiagnosticSeverity::ERROR) })
    );

    assert!(
        bone.inline_local(&main_path, pos(src, "let y", 4))
            .is_none()
    );
}

#[test]
fn inlines_a_macro_argument_from_the_same_file_as_the_local() {
    let defs = "macro read_explicit_value(arg: expr) -> int { arg + 2 }\n";
    let src = "fn f() -> int {\n    let y = 1\n    return read_explicit_value(y)\n}\n";
    let project = Project::new(&[("defs.rock", defs), ("main.rock", src)]);
    let (defs_path, main_path) = (project.path("defs.rock"), project.path("main.rock"));
    let mut bone = BoneWorkspace::new();
    bone.update(&defs_path, defs.to_string());
    let diagnostics = bone.update(&main_path, src.to_string());
    assert!(
        diagnostics
            .iter()
            .flat_map(|(_, d)| d)
            .all(|d| { d.severity != Some(DiagnosticSeverity::ERROR) })
    );

    let action = bone.inline_local(&main_path, pos(src, "let y", 4)).unwrap();
    assert_eq!(
        apply_edits(src, action.edits),
        "fn f() -> int {\n    return read_explicit_value(1)\n}\n"
    );
}
