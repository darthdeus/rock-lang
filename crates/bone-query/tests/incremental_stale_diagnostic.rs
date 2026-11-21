//! Diagnostics from an intermediate edit state must clear once the buffer is valid again.

use crate::fixture::{PATH, Project};
use bone_query::BoneWorkspace;

fn messages(bone: &BoneWorkspace, path: &str) -> Vec<String> {
    bone.diagnostics_for(path)
        .into_iter()
        .map(|d| d.message)
        .collect()
}

/// Flipping const to global must re-infer the dependent module that writes the global.
#[test]
fn const_to_global_clears_assign_error_cross_file() {
    let manifest = (
        "stick.toml",
        "[package]\nname = \"stale_diagnostic\"\ntype = \"lib\"\n\n[language]\nmodules = \"v1\"\n",
    );
    let decl_src = "pub mod decl;\npub const X: float = 0.0\n";
    let user_src = "pub mod user;\nuse super::decl::X;\nfn w() {\n    X = 1.0\n}\n";
    let project = Project::new(&[
        manifest,
        ("main.rock", "fn main() -> int { 0 }\n"),
        ("decl.rock", decl_src),
        ("user.rock", user_src),
    ]);
    let (decl, user) = (project.path("decl.rock"), project.path("user.rock"));
    let mut bone = BoneWorkspace::new();
    bone.update(&decl, decl_src.to_string());
    bone.update(&user, user_src.to_string());
    let before = messages(&bone, &user);
    assert!(
        before.iter().any(|m| m.contains("cannot assign to const")),
        "{before:?}"
    );

    let global_src = "pub mod decl;\npub global X: float = 0.0\n";
    std::fs::write(&decl, global_src).unwrap();
    bone.update(&decl, global_src.to_string());
    assert!(
        messages(&bone, &user).is_empty(),
        "{:?}",
        messages(&bone, &user)
    );
}

/// One update that edits a body and text before it (a comment, recovered junk)
/// must leave the same diagnostics as a fresh check of the final buffer.
#[test]
fn body_edit_with_shifted_prefix_matches_fresh_check() {
    let located = |bone: &BoneWorkspace| {
        bone.diagnostics_for(PATH)
            .into_iter()
            .map(|d| (d.range.start.line, d.message))
            .collect::<Vec<_>>()
    };
    let main = "fn main() { let s: string = a() }\n";
    let cases = [
        vec![
            format!("// cccc\nfn a() -> int {{ 1 }}\nfn b() -> int {{ 2 }}\n{main}"),
            format!("// c\nfn a() -> int {{ 1 }}\nfn b() -> int {{ 3 }}\n{main}"),
        ],
        // The first two buffers carry recovery errors, so the third takes the
        // full path with a single changed body.
        vec![
            "fn a(x: int) { a(x) }\nenum E {\n, fn o(x: E) {\n}\nfn b() { }\n".to_string(),
            "fn a(x: int) { a(x) }\nenum E {\nfn o(x: E) {\n}\nfn b() { print} (1) }\n".to_string(),
            "fn a(x: int) { a(1) }\nenum E {\nfn o(x: E) {\n}\nfn b() { }\n".to_string(),
        ],
        // Recovery ends the first body early, so fixing it lengthens that
        // function's span while the text after it stays in place.
        vec![
            "fn a(n: int) {\n    i while < n {\n        t = t + 1\n}\n}\nfn b(x: int) {\n}\nfn c(x: float) {\n}\n".to_string(),
            "fn a(n: int) {\n    while i < n {\n        t = t + 1\n}\n}\nfn b(x: int) {\n}\nfn c(x: float) {\n}\n".to_string(),
            "fn a(n: int) {\n    while i < n {\n        t = t + 1\n}\n}\nfn b(x: int) {\n}\nfn c(x: float) {\nlet w = x\n}\n".to_string(),
        ],
    ];
    for states in cases {
        let mut bone = BoneWorkspace::new();
        for text in &states {
            bone.update(PATH, text.clone());
        }
        let fresh = crate::fixture::workspace(states.last().unwrap());
        assert_eq!(located(&bone), located(&fresh), "{states:#?}");
    }
}
