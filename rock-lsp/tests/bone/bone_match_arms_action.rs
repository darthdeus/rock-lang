//! The bone "fill in missing match arms" code action (`compute_code_actions_workspace`).
//!
//! When the cursor is in a `match` over an enum that's missing variants, the
//! bone backend offers an action whose edit inserts `EnumName::Variant => {}`
//! arms for the uncovered variants. Empty `match x {}`, partial matches, and the
//! no-op cases (exhaustive / wildcard) are all exercised here -- there's no
//! `.rock.lsp` fixture type for code actions.

use lsp_types::*;
use rock_lsp::{compute_code_actions_workspace, LspBackend, LspWorkspace};
use std::str::FromStr;

fn bone_ws(path: &std::path::Path, src: &str) -> LspWorkspace {
    std::fs::write(path, src).unwrap();
    let mut ws = LspWorkspace::new();
    ws.backend = LspBackend::Bone;
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

fn new_text(action: &CodeAction) -> String {
    let edit = action.edit.as_ref().expect("action should carry an edit");
    let changes = edit.changes.as_ref().expect("edit should have changes");
    let edits = changes.values().next().expect("one file's edits");
    edits[0].new_text.clone()
}

const EMPTY: &str = "\
enum Color { Red, Green, Blue }

fn main() {
    let c = Color::Red
    match c {}
}
";

#[test]
fn fills_all_arms_for_empty_enum_match() {
    // cursor on `match c {}` (line 4)
    let actions = run(EMPTY, 4, 4);
    assert_eq!(actions.len(), 1, "exactly one action: {actions:?}");
    assert!(
        actions[0].title.contains("3 missing match arms"),
        "title: {}",
        actions[0].title
    );
    let nt = new_text(&actions[0]);
    for v in ["Color::Red => {}", "Color::Green => {}", "Color::Blue => {}"] {
        assert!(nt.contains(v), "new_text missing {v:?}: {nt:?}");
    }
}

#[test]
fn fills_only_the_missing_arms_for_a_partial_match() {
    let src = "\
enum Color { Red, Green, Blue }

fn main() {
    let c = Color::Red
    match c {
        Color::Red => {}
    }
}
";
    // cursor on the `match` line (line 4)
    let actions = run(src, 4, 4);
    assert_eq!(actions.len(), 1, "one action: {actions:?}");
    assert!(actions[0].title.contains("2 missing"), "title: {}", actions[0].title);
    let nt = new_text(&actions[0]);
    assert!(nt.contains("Color::Green => {}"), "{nt:?}");
    assert!(nt.contains("Color::Blue => {}"), "{nt:?}");
    assert!(!nt.contains("Color::Red"), "should not re-add the covered Red: {nt:?}");
}

#[test]
fn no_action_when_match_is_exhaustive() {
    let src = "\
enum Color { Red, Green, Blue }

fn main() {
    let c = Color::Red
    match c {
        Color::Red => {}
        Color::Green => {}
        Color::Blue => {}
    }
}
";
    assert!(run(src, 4, 4).is_empty(), "no action when all variants covered");
}

#[test]
fn no_action_with_a_wildcard_arm() {
    let src = "\
enum Color { Red, Green, Blue }

fn main() {
    let c = Color::Red
    match c {
        Color::Red => {}
        _ => {}
    }
}
";
    assert!(run(src, 4, 4).is_empty(), "a `_` arm makes the match exhaustive");
}

#[test]
fn generates_payload_patterns_for_tuple_and_record_variants() {
    let src = "\
enum Shape { Circle(float), Rect(float, float), Square { side: float }, Dot }

fn main() {
    let s = Shape::Dot
    match s {}
}
";
    let actions = run(src, 4, 4);
    assert_eq!(actions.len(), 1, "{actions:?}");
    let nt = new_text(&actions[0]);
    assert!(nt.contains("Shape::Circle(_) => {}"), "1-tuple: {nt:?}");
    assert!(nt.contains("Shape::Rect(_, _) => {}"), "2-tuple: {nt:?}");
    assert!(nt.contains("Shape::Square { .. } => {}"), "record: {nt:?}");
    assert!(nt.contains("Shape::Dot => {}"), "unit: {nt:?}");
}

#[test]
fn no_action_when_not_in_a_match() {
    // cursor on `let c = ...` (line 3) -- not inside a match
    assert!(run(EMPTY, 3, 8).is_empty());
}

// Regression: the match is in VALUE position and the body mistypes (`()` vs the
// declared `-> string`), so inference on the body errors. The fill-match-arms
// action must still resolve the scrutinee's enum from its annotated param type
// and offer the arms -- a type error elsewhere in the body must not suppress it.
#[test]
fn fills_arms_even_when_body_mistypes_return() {
    let src = "\
enum TargetError { TooFar, NoLos }

fn message(e: TargetError) -> string {
    match e {
    }
}
";
    // cursor on the `match e {` line (line 3, 0-based)
    let actions = run(src, 3, 4);
    assert_eq!(actions.len(), 1, "expected the fill action despite the type error: {actions:?}");
    let nt = new_text(&actions[0]);
    assert!(nt.contains("TargetError::TooFar => {}"), "{nt:?}");
    assert!(nt.contains("TargetError::NoLos => {}"), "{nt:?}");
}

// The fill action is wired to the non-exhaustive-match diagnostic: when that
// diagnostic is passed in the request context, the action carries it (so editors
// surface the fill as the quick-fix on the squiggle).
#[test]
fn fill_action_is_linked_to_the_non_exhaustive_diagnostic() {
    let src = "\
enum Color { Red, Green, Blue }

fn f(c: Color) {
    match c {
        Color::Red => {}
    }
}
";
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.rock");
    let mut ws = bone_ws(&path, src);

    // The diagnostic the LSP would publish for this file.
    let diag = ws
        .bone
        .diagnostics_for(path.to_str().unwrap())
        .into_iter()
        .find(|d| d.message.contains("Non-exhaustive match"))
        .expect("non-exhaustive diagnostic should exist");

    // Request a code action at the diagnostic, passing it in the context (as a
    // client does).
    let mut params = params_at(&path, diag.range.start.line, diag.range.start.character);
    params.context.diagnostics = vec![diag.clone()];

    let actions = compute_code_actions_workspace(&mut ws, &params)
        .unwrap()
        .unwrap_or_default();
    let fill = actions
        .iter()
        .find(|a| a.title.contains("missing match arm"))
        .expect("fill action offered");
    assert_eq!(
        fill.diagnostics.as_deref(),
        Some(&[diag][..]),
        "the fill action must carry the non-exhaustive diagnostic it resolves"
    );
}
