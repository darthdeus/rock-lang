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
    let project = Project::new(&[
        ("decl.rock", "const X: float = 0.0\n"),
        ("user.rock", "fn w() {\n    X = 1.0\n}\n"),
    ]);
    let (decl, user) = (project.path("decl.rock"), project.path("user.rock"));
    let mut bone = BoneWorkspace::new();
    bone.update(&decl, project.read("decl.rock"));
    bone.update(&user, project.read("user.rock"));
    let before = messages(&bone, &user);
    assert!(
        before.iter().any(|m| m.contains("cannot assign to const")),
        "{before:?}"
    );

    std::fs::write(&decl, "global X: float = 0.0\n").unwrap();
    bone.update(&decl, "global X: float = 0.0\n".to_string());
    assert!(
        messages(&bone, &user).is_empty(),
        "{:?}",
        messages(&bone, &user)
    );
}
