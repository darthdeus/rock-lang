//! Space is a completion trigger character (so `let x = |` opens the menu on an
//! empty prefix). But `<CR>` auto-indentation inserts spaces, which would
//! trigger the menu on a fresh line -- so `<CR><CR>` accepts item 1 instead of
//! making two newlines. A space trigger whose line-prefix is only whitespace is
//! that indentation case and must be suppressed; a space after a token, and any
//! non-space (manual) invoke, must still complete.

use bone_query::BoneWorkspace;
use lsp_types::Position;

const PATH: &str = "/tmp/rock_completion_blank_line.rock";

fn complete(src: &str, pos: Position, space: bool) -> usize {
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src.to_string());
    bone.completion_with_trigger(PATH, pos, space).len()
}

#[test]
fn space_trigger_on_blank_indented_line_is_suppressed() {
    // `fn f() {` / `    let count = 1` / `    <cursor>` -- the third line is
    // indentation only, mimicking `<CR>` autoindent.
    let src = "fn f() {\n    let count = 1\n    \n}\n";
    let n = complete(src, Position::new(2, 4), true);
    assert_eq!(
        n, 0,
        "space trigger on a whitespace-only line must yield nothing"
    );
}

#[test]
fn space_trigger_after_a_token_still_completes() {
    // `    let x = ` -- cursor right after the space that follows `=`.
    let src = "fn f() {\n    let count = 1\n    let x = \n}\n";
    let n = complete(src, Position::new(2, 12), true);
    assert!(
        n > 0,
        "space after a token is a real trigger context: got {n}"
    );
}

#[test]
fn manual_invoke_on_blank_line_is_not_suppressed() {
    // Same blank line, but not a space trigger (manual `<C-x><C-o>` / Invoked).
    let src = "fn f() {\n    let count = 1\n    \n}\n";
    let n = complete(src, Position::new(2, 4), false);
    assert!(
        n > 0,
        "manual invoke on a blank line must still complete: got {n}"
    );
}
