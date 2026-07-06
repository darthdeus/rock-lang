//! The bone "Fill struct fields" code action (`compute_code_actions_workspace`).
//!
//! When the cursor is inside a struct literal `Name { ... }` that's missing
//! required fields (declared, no default, not already provided), the bone
//! backend offers an action whose edit inserts `field: ?` for each missing
//! field. Fields with a default are not added (omitting them is legal). The
//! generated `?` is a Rock expression hole, so the result type-checks and the
//! user can hover each `?` for its expected type. There's no `.rock.lsp`
//! fixture type for code actions, so this is a Rust end-to-end test.

use lsp_types::*;
use rock_lsp::{compute_code_actions_workspace, LspWorkspace};
use std::str::FromStr;

fn bone_ws(path: &std::path::Path, src: &str) -> LspWorkspace {
    std::fs::write(path, src).unwrap();
    let mut ws = LspWorkspace::new();
    ws.bone.update(path.to_str().unwrap(), src.to_string());
    ws
}

fn params_at(path: &std::path::Path, line: u32, ch: u32) -> CodeActionParams {
    CodeActionParams {
        text_document: TextDocumentIdentifier {
            uri: Uri::from_str(&format!("file://{}", path.display())).unwrap(),
        },
        range: Range {
            start: Position::new(line, ch),
            end: Position::new(line, ch),
        },
        context: CodeActionContext {
            diagnostics: vec![],
            only: None,
            trigger_kind: None,
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    }
}

fn run(src: &str, line: u32, ch: u32) -> Vec<CodeAction> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.rock");
    let mut ws = bone_ws(&path, src);
    compute_code_actions_workspace(&mut ws, &params_at(&path, line, ch))
        .unwrap()
        .unwrap_or_default()
}

/// The single action whose title mentions struct fields, or None.
fn fill_action(actions: &[CodeAction]) -> Option<&CodeAction> {
    actions.iter().find(|a| a.title.contains("struct field"))
}

/// Apply the action's one TextEdit to `src` and return the result. ASCII-only
/// (LSP UTF-16 columns == byte columns for these fixtures).
fn apply(src: &str, action: &CodeAction) -> String {
    let edit = action.edit.as_ref().expect("action carries an edit");
    let changes = edit.changes.as_ref().expect("edit has changes");
    let edits = changes.values().next().expect("one file's edits");
    let te = &edits[0];
    let s = pos_to_offset(src, te.range.start);
    let e = pos_to_offset(src, te.range.end);
    format!("{}{}{}", &src[..s], te.new_text, &src[e..])
}

fn pos_to_offset(src: &str, pos: Position) -> usize {
    let mut off = 0usize;
    for (l, line) in src.split_inclusive('\n').enumerate() {
        if l as u32 == pos.line {
            let mut c = 0u32;
            for (b, _) in line.char_indices() {
                if c == pos.character {
                    return off + b;
                }
                c += 1;
            }
            return off + line.len();
        }
        off += line.len();
    }
    off
}

#[test]
fn fills_all_fields_of_an_empty_literal() {
    let src = "\
struct Point { x: int, y: int }

fn main() {
    let p = Point {}
}
";
    // cursor inside `Point {}` (line 3)
    let actions = run(src, 3, 16);
    let action = fill_action(&actions).expect("expected a fill-struct-fields action");
    let out = apply(src, action);
    assert!(
        out.contains("Point { x: ?, y: ? }"),
        "both fields filled inline: {out:?}"
    );
}

#[test]
fn fills_only_the_missing_field_of_a_partial_literal() {
    let src = "\
struct Point { x: int, y: int }

fn main() {
    let p = Point { x: 1 }
}
";
    let actions = run(src, 3, 20);
    let action = fill_action(&actions).expect("expected a fill action");
    let out = apply(src, action);
    assert!(
        out.contains("Point { x: 1, y: ? }"),
        "only y added, comma inserted: {out:?}"
    );
    assert!(!out.contains("x: ?"), "must not re-add the present x: {out:?}");
}

#[test]
fn respects_a_trailing_comma() {
    let src = "\
struct Point { x: int, y: int }

fn main() {
    let p = Point { x: 1, }
}
";
    let actions = run(src, 3, 20);
    let action = fill_action(&actions).expect("expected a fill action");
    let out = apply(src, action);
    assert!(
        out.contains("Point { x: 1, y: ? }"),
        "trailing comma not duplicated: {out:?}"
    );
}

#[test]
fn does_not_add_fields_that_have_a_default() {
    // `b` has a default, so `Bar { a: 1 }` is already complete -> no action.
    let src = "\
struct Bar { a: int, b: int = 5 }

fn main() {
    let v = Bar { a: 1 }
}
";
    let actions = run(src, 3, 18);
    assert!(
        fill_action(&actions).is_none(),
        "defaulted field missing is legal -> no fill action: {actions:?}"
    );
}

#[test]
fn fills_only_required_fields_in_empty_literal_with_a_default() {
    // `Bar {}` is missing required `a` (fill) and defaulted `b` (skip).
    let src = "\
struct Bar { a: int, b: int = 5 }

fn main() {
    let v = Bar {}
}
";
    let actions = run(src, 3, 17);
    let action = fill_action(&actions).expect("expected a fill action for required `a`");
    let out = apply(src, action);
    assert!(out.contains("Bar { a: ? }"), "only required a: {out:?}");
    assert!(!out.contains("b: ?"), "defaulted b not added: {out:?}");
}

#[test]
fn no_action_when_all_fields_present() {
    let src = "\
struct Point { x: int, y: int }

fn main() {
    let p = Point { x: 1, y: 2 }
}
";
    assert!(
        fill_action(&run(src, 3, 20)).is_none(),
        "complete literal offers no fill action"
    );
}

#[test]
fn no_action_when_cursor_not_in_a_struct_literal() {
    let src = "\
struct Point { x: int, y: int }

fn main() {
    let p = Point { x: 1, y: 2 }
}
";
    // cursor on the `struct` decl line (line 0)
    assert!(fill_action(&run(src, 0, 8)).is_none());
}

#[test]
fn fills_a_multiline_literal_with_matching_indent() {
    let src = "\
struct Point { x: int, y: int }

fn main() {
    let p = Point {
        x: 1
    }
}
";
    // cursor on the `x: 1` line (line 4)
    let actions = run(src, 4, 8);
    let action = fill_action(&actions).expect("expected a fill action");
    let out = apply(src, action);
    // y goes on its own line, indented like x, with a comma closing x.
    assert!(
        out.contains("        x: 1,\n        y: ?,\n"),
        "multiline fill keeps indentation: {out:?}"
    );
}
