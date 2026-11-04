//! Macros are expanded away before bone lowering, so the workspace HIR has no
//! record of them -- the Session's parse-time capture (`UserFile.macros`) is the
//! only source. ctags and semantic tokens already read it; completion must too,
//! or every macro in the tree is invisible to the editor.

use bone_query::BoneWorkspace;
use lsp_types::Position;

const PATH: &str = "/tmp/rock_lsp_bone_completion_macros.rock";

fn labels(src: &str, line: u32, ch: u32) -> Vec<String> {
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src.to_string());
    bone.completion(PATH, Position::new(line, ch))
        .into_iter()
        .map(|c| c.label)
        .collect()
}

#[test]
fn macro_is_offered_at_an_expression_position() {
    let src = "macro twice(x: expr) {\n    x + x\n}\n\nfn r() {\n    tw\n}\n";
    let got = labels(src, 5, 6);
    assert!(
        got.iter().any(|l| l == "twice"),
        "macro `twice` missing from completion: {got:?}"
    );
}

#[test]
fn macro_and_fn_are_both_offered_under_a_shared_prefix() {
    // A macro must not shadow, nor be shadowed by, a same-prefix fn.
    let src = "macro log_once(msg: expr) {\n    log(msg)\n}\n\
               fn log_info(msg: string) {}\n\
               fn r() {\n    log_\n}\n";
    let got = labels(src, 5, 8);
    assert!(
        got.iter().any(|l| l == "log_once"),
        "macro missing: {got:?}"
    );
    assert!(got.iter().any(|l| l == "log_info"), "fn missing: {got:?}");
}

/// Stdlib macros live in the WARM modules, not `Session.files` -- a source that
/// reads user files alone passes every in-source test above while `log_once`
/// stays invisible in a real editor.
#[test]
fn stdlib_macro_is_offered() {
    let src = "fn r() {\n    log_o\n}\n";
    let got = labels(src, 1, 9);
    assert!(
        got.iter().any(|l| l == "log_once"),
        "stdlib macro `log_once` missing: {got:?}"
    );
}

#[test]
fn macro_is_not_offered_in_a_type_position() {
    // Macros are value-position constructs; a type slot offers the type
    // universe only (structs / enums / newtypes / builtins).
    let src = "macro twice(x: expr) {\n    x + x\n}\n\nfn r(a: tw) {}\n";
    // Column 10 == directly after `tw`, inside the param's TYPE slot.
    let got = labels(src, 4, 10);
    assert!(
        !got.iter().any(|l| l == "twice"),
        "macro leaked into a type slot: {got:?}"
    );
}
