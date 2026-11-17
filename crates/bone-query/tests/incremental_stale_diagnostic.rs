//! Diagnostics from an intermediate edit state must clear once the buffer is
//! valid again. The incremental body-edit fast path once carried syntax errors
//! forward, so typing `as in` -> `as int` (where `as in` is a parse-recovery
//! error) left a phantom "unsupported type" until an LSP restart.

use crate::fixture::{PATH, Project};
use bone_query::BoneWorkspace;

fn messages(bone: &BoneWorkspace, path: &str) -> Vec<String> {
    bone.diagnostics_for(path)
        .into_iter()
        .map(|d| d.message)
        .collect()
}

#[test]
fn edits_through_broken_states_end_clean() {
    let cast = |ty: &str| format!("fn f(v: float) -> int {{\n    return v as {ty}\n}}\n");
    let assign = |kw: &str| format!("{kw} X: float = 0.0\nfn w() {{\n    X = 1.0\n}}\n");
    // (case, buffer states with the fragment each reports; None is clean)
    let cases = [
        (
            "`as int` broken to `as in`",
            vec![
                (cast("int"), None),
                (cast("in"), Some("")),
                (cast("int"), None),
            ],
        ),
        (
            "typing `as int` char by char",
            vec![
                (cast("int"), None),
                (cast("i"), Some("")),
                (cast("in"), Some("")),
                (cast("int"), None),
            ],
        ),
        (
            "const flipped to global",
            vec![
                (assign("const"), Some("cannot assign to const")),
                (assign("global"), None),
            ],
        ),
    ];
    for (case, states) in cases {
        let mut bone = BoneWorkspace::new();
        for (i, (text, expected)) in states.into_iter().enumerate() {
            bone.update(PATH, text);
            let msgs = messages(&bone, PATH);
            match expected {
                None => assert!(msgs.is_empty(), "{case}: state {i}: {msgs:?}"),
                Some(f) => assert!(
                    msgs.iter().any(|m| m.contains(f)),
                    "{case}: state {i}: {msgs:?}"
                ),
            }
        }
    }
}

/// Flipping const to global in one file must re-infer the dependent module
/// that writes the global and clear its diagnostic.
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
