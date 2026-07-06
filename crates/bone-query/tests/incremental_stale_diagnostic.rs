//! A transient syntax/recovery error from an intermediate edit state must clear
//! once the buffer is valid again. The incremental body-edit fast path carried
//! the file's syntax errors forward unchanged, so a `... as in` -> `... as int`
//! keystroke (where the intermediate `as in` is a parse-recovery error) left a
//! phantom "unsupported type" diagnostic stuck until an LSP restart.

use bone_query::BoneWorkspace;

const PATH: &str = "/tmp/rock_lsp_bone_stale_diag.rock";

fn diag_msgs(bone: &BoneWorkspace) -> Vec<String> {
    bone.diagnostics_for(PATH).into_iter().map(|d| d.message).collect()
}

#[test]
fn intermediate_unsupported_type_clears_after_edit() {
    let valid = "fn f(v: float) -> int {\n    return v as int\n}\n";
    // `as in` -- `in` is a keyword; the type parser recovers with an error.
    let broken = "fn f(v: float) -> int {\n    return v as in\n}\n";

    let mut bone = BoneWorkspace::new();
    bone.update(PATH, valid.to_string());
    assert!(diag_msgs(&bone).is_empty(), "valid start: {:?}", diag_msgs(&bone));

    bone.update(PATH, broken.to_string());
    assert!(!diag_msgs(&bone).is_empty(), "broken state should report something");

    // Edit back to valid -- every diagnostic MUST clear.
    bone.update(PATH, valid.to_string());
    assert!(
        diag_msgs(&bone).is_empty(),
        "stale diagnostic survived the fix-up edit: {:?}",
        diag_msgs(&bone)
    );
}

#[test]
fn typing_as_int_char_by_char_ends_clean() {
    // The user's exact scenario: type the type annotation one char at a time
    // through broken intermediate states, then land on the valid `int`.
    let head = "fn g(v: float) -> int {\n    return v as ";
    let tail = "\n}\n";
    let states = ["i", "in", "int"];

    let mut bone = BoneWorkspace::new();
    // Seed with a complete valid buffer first (as if the line existed).
    bone.update(PATH, format!("{head}int{tail}"));
    for s in states {
        bone.update(PATH, format!("{head}{s}{tail}"));
    }
    assert!(
        diag_msgs(&bone).is_empty(),
        "landing on `as int` must be diagnostic-free: {:?}",
        diag_msgs(&bone)
    );
}

#[test]
fn const_to_global_clears_assign_error() {
    let with_const =
        "const X: float = 0.0\nfn w() {\n    X = 1.0\n}\n";
    let with_global =
        "global X: float = 0.0\nfn w() {\n    X = 1.0\n}\n";

    let mut bone = BoneWorkspace::new();
    bone.update(PATH, with_const.to_string());
    assert!(
        diag_msgs(&bone).iter().any(|m| m.contains("cannot assign to const")),
        "const start should error: {:?}",
        diag_msgs(&bone)
    );

    bone.update(PATH, with_global.to_string());
    assert!(
        diag_msgs(&bone).is_empty(),
        "stale const-assign diagnostic survived const->global edit: {:?}",
        diag_msgs(&bone)
    );
}

#[test]
fn const_to_global_clears_assign_error_cross_file() {
    // The real scenario: the global is declared in one file, the assigning fn
    // lives in another. Flipping const->global must re-infer the DEPENDENT
    // module's body (it writes the global) and clear its diagnostic.
    let dir = std::env::temp_dir().join("rock_lsp_bone_const_xfile");
    let _ = std::fs::create_dir_all(&dir);
    let decl = dir.join("decl.rock");
    let user = dir.join("user.rock");
    let dp = decl.to_str().unwrap();
    let up = user.to_str().unwrap();

    std::fs::write(&decl, "const X: float = 0.0\n").unwrap();
    std::fs::write(&user, "fn w() {\n    X = 1.0\n}\n").unwrap();

    let mut bone = BoneWorkspace::new();
    bone.update(dp, "const X: float = 0.0\n".to_string());
    bone.update(up, "fn w() {\n    X = 1.0\n}\n".to_string());
    let user_diags = |b: &BoneWorkspace| {
        b.diagnostics_for(up).into_iter().map(|d| d.message).collect::<Vec<_>>()
    };
    assert!(
        user_diags(&bone).iter().any(|m| m.contains("cannot assign to const")),
        "const start should error in the user file: {:?}",
        user_diags(&bone)
    );

    // Flip the DECL file const -> global.
    std::fs::write(&decl, "global X: float = 0.0\n").unwrap();
    bone.update(dp, "global X: float = 0.0\n".to_string());
    assert!(
        user_diags(&bone).is_empty(),
        "stale const-assign diagnostic survived in the dependent file: {:?}",
        user_diags(&bone)
    );
}
