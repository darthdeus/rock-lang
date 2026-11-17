use crate::fixture::{Project, apply_edits, pos};
use bone_query::BoneWorkspace;
use lsp_types::DiagnosticSeverity;

#[test]
fn inlining_distinguishes_macro_default_and_call_site_spans() {
    // The loop-default use in defs overlaps the caller's loop byte span;
    // only an explicit caller argument is eligible for a caller edit.
    for (defs, src, expected) in [
        (
            "macro looped(rrrrrrr: expr = y) {\n    for i in 0..3 {\n        print(rrrrrrr)\n    }\n}\n",
            "fn f(v: int) { let y = g(v)\nlooped()\n}\nfn g(v: int) -> int { v }\n",
            None,
        ),
        (
            "macro read_default_value(arg: expr = y) -> int { arg + 2 }\n",
            "fn f() -> int {\n    let y = 1\n    return read_default_value()\n}\n",
            None,
        ),
        (
            "macro read_explicit_value(arg: expr) -> int { arg + 2 }\n",
            "fn f() -> int {\n    let y = 1\n    return read_explicit_value(y)\n}\n",
            Some("fn f() -> int {\n    return read_explicit_value(1)\n}\n"),
        ),
    ] {
        let project = Project::new(&[("defs.rock", defs), ("main.rock", src)]);
        let main = project.path("main.rock");
        let mut bone = BoneWorkspace::new();
        bone.update(&project.path("defs.rock"), defs.to_string());
        let diagnostics = bone.update(&main, src.to_string());
        assert!(
            diagnostics
                .iter()
                .flat_map(|(_, d)| d)
                .all(|d| { d.severity != Some(DiagnosticSeverity::ERROR) }),
            "{src}: {diagnostics:?}"
        );
        let cursor = pos(src, "let y", 4);
        assert!(
            bone.hover(&main, cursor).is_some(),
            "the body must type-check"
        );
        let action = bone.inline_local(&main, cursor);
        match expected {
            None => assert!(
                action.is_none(),
                "macro definition spans must not be inlined: {src}"
            ),
            Some(expected) => assert_eq!(apply_edits(src, action.unwrap().edits), expected),
        }
    }
}
