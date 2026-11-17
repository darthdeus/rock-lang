//! The bone "fill in missing match arms" code action (`compute_code_actions_workspace`).
//!
//! When the cursor is in a `match` over an enum that's missing variants, the
//! bone backend offers an action whose edit inserts `EnumName::Variant => {}`
//! arms for the uncovered variants. Empty `match x {}`, partial matches, and the
//! no-op cases (exhaustive / wildcard) are all exercised here -- there's no
//! `.rock.lsp` fixture type for code actions.

use crate::common::*;
use lsp_types::*;
use rock_lsp::compute_code_actions_workspace;

const COLOR: &str = "enum Color { Red, Green, Blue }";

fn match_source(declaration: &str, value: &str, arms: &str) -> String {
    if arms.is_empty() {
        format!("{declaration}\n\nfn main() {{\n    let c = {value}\n    match c {{}}\n}}\n")
    } else {
        format!(
            "{declaration}\n\nfn main() {{\n    let c = {value}\n    match c {{\n{arms}\n    }}\n}}\n"
        )
    }
}

fn new_text(action: &CodeAction) -> String {
    let edit = action.edit.as_ref().expect("action should carry an edit");
    #[expect(
        clippy::mutable_key_type,
        reason = "WorkspaceEdit Uri keys use immutable text for equality and hashing"
    )]
    let changes = edit.changes.as_ref().expect("edit should have changes");
    changes.values().next().expect("one file's edits")[0]
        .new_text
        .clone()
}

#[test]
fn color_match_fill_and_no_action_cases() {
    for (arms, missing) in [
        ("", &["Red", "Green", "Blue"][..]),
        ("        Color::Red => {}", &["Green", "Blue"][..]),
        (
            "        Color::Red => {}\n        Color::Green => {}\n        Color::Blue => {}",
            &[][..],
        ),
        ("        Color::Red => {}\n        _ => {}", &[][..]),
    ] {
        let src = match_source(COLOR, "Color::Red", arms);
        let actions = run(&src, 4, 4);
        if missing.is_empty() {
            assert!(actions.is_empty(), "{arms}: {actions:?}");
            continue;
        }
        assert_eq!(actions.len(), 1, "{arms}: {actions:?}");
        assert!(
            actions[0]
                .title
                .contains(&format!("{} missing match arms", missing.len())),
            "{}",
            actions[0].title
        );
        let inserted = new_text(&actions[0]);
        for variant in missing {
            assert!(
                inserted.contains(&format!("Color::{variant} => {{}}")),
                "{inserted:?}"
            );
        }
        if !arms.is_empty() {
            assert!(
                !inserted.contains("Color::Red"),
                "covered variant re-added: {inserted:?}"
            );
        }
    }
}

#[test]
fn generates_payload_patterns_for_tuple_and_record_variants() {
    let src = match_source(
        "enum Shape { Circle(float), Rect(float, float), Square { side: float }, Dot }",
        "Shape::Dot",
        "",
    );
    let actions = run(&src, 4, 4);
    assert_eq!(actions.len(), 1, "{actions:?}");
    let inserted = new_text(&actions[0]);
    for pattern in [
        "Shape::Circle(_) => {}",
        "Shape::Rect(_, _) => {}",
        "Shape::Square { .. } => {}",
        "Shape::Dot => {}",
    ] {
        assert!(
            inserted.contains(pattern),
            "missing {pattern:?}: {inserted:?}"
        );
    }
}

#[test]
fn no_match_arm_action_when_cursor_is_outside_a_match() {
    let src = match_source(COLOR, "Color::Red", "");
    // The `let` can offer an inline-local action, but not a match-arms action.
    let actions = run(&src, 3, 8);
    assert!(
        !actions.iter().any(|a| a.title.contains("match arm")),
        "no match-arms action outside a match: {actions:?}"
    );
}

// The return annotation is wrong, while the parameter still fixes the match
// type. The code action must remain available through that inference error.
#[test]
fn fills_arms_even_when_body_mistypes_return() {
    let src = "\
enum TargetError { TooFar, NoLos }

fn message(e: TargetError) -> string {
    match e {
    }
}
";
    let actions = run(src, 3, 4);
    assert_eq!(
        actions.len(),
        1,
        "expected the fill action despite the type error: {actions:?}"
    );
    let inserted = new_text(&actions[0]);
    assert!(
        inserted.contains("TargetError::TooFar => {}"),
        "{inserted:?}"
    );
    assert!(
        inserted.contains("TargetError::NoLos => {}"),
        "{inserted:?}"
    );
}

// The fill action must carry the diagnostic it resolves, as an editor quick-fix.
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

    let diag = ws
        .bone
        .diagnostics_for(path.to_str().unwrap())
        .into_iter()
        .find(|d| d.message.contains("Non-exhaustive match"))
        .expect("non-exhaustive diagnostic should exist");
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
