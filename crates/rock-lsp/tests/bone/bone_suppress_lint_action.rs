//! The bone "Suppress with `// rock: allow(<lint>)`" code action.
//!
//! When the cursor sits on a lint diagnostic (identified by the `Diagnostic.code`
//! field, not by message-sniffing), the backend offers a quick-fix that inserts
//! `// rock: allow(<lint>)` on a new line directly above the offender, copying
//! the offender's leading whitespace so the comment lines up.

use crate::common::*;
use lsp_types::*;
use rock_lsp::compute_code_actions_workspace;

const UNUSED_VAR_SRC: &str = "\
fn main() {
    let unused_thing = 5
}
";

fn suppress_action(actions: &[CodeAction]) -> Option<&CodeAction> {
    actions.iter().find(|a| a.title.contains("Suppress"))
}

// The lint diagnostic carries its lint name in `Diagnostic.code`, so the action
// can identify it robustly instead of parsing the human-readable message.
#[test]
fn lint_diagnostic_carries_its_name_in_the_code_field() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.rock");
    let ws = bone_ws(&path, UNUSED_VAR_SRC);
    let diag = ws
        .bone
        .lint_diagnostics(path.to_str().unwrap())
        .into_iter()
        .find(|d| d.message.contains("unused variable"))
        .expect("unused_var diagnostic should exist");
    assert_eq!(
        diag.code,
        Some(NumberOrString::String("unused_var".to_string())),
        "lint diagnostic must tag its name in `code`: {diag:?}"
    );
}

#[test]
fn offers_suppress_action_on_an_unused_var_diagnostic() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.rock");
    let mut ws = bone_ws(&path, UNUSED_VAR_SRC);

    let diag = ws
        .bone
        .lint_diagnostics(path.to_str().unwrap())
        .into_iter()
        .find(|d| d.message.contains("unused variable"))
        .expect("unused_var diagnostic should exist");

    let mut params = params_at(&path, diag.range.start.line, diag.range.start.character);
    params.context.diagnostics = vec![diag.clone()];

    let actions = compute_code_actions_workspace(&mut ws, &params)
        .unwrap()
        .unwrap_or_default();
    let action = suppress_action(&actions).expect("suppress action offered");

    assert!(
        action.title.contains("unused_var"),
        "title names the lint: {}",
        action.title
    );
    assert_eq!(
        action.diagnostics.as_deref(),
        Some(&[diag][..]),
        "the action must carry the lint diagnostic it resolves"
    );

    // The edit inserts the magic comment above the offender, indentation matched.
    let out = apply(UNUSED_VAR_SRC, action);
    assert_eq!(
        out, "fn main() {\n    // rock: allow(unused_var)\n    let unused_thing = 5\n}\n",
        "got:\n{out}"
    );
}

// No lint diagnostic in context -> no suppress action (the action keys off the
// diagnostics the client passes, not a re-scan).
#[test]
fn no_suppress_action_without_a_lint_diagnostic_in_context() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.rock");
    let mut ws = bone_ws(&path, UNUSED_VAR_SRC);
    // Cursor on the `let`, but no diagnostics supplied in the request context.
    let actions = compute_code_actions_workspace(&mut ws, &params_at(&path, 1, 8))
        .unwrap()
        .unwrap_or_default();
    assert!(
        suppress_action(&actions).is_none(),
        "no suppress action: {actions:?}"
    );
}

// A non-lint bone diagnostic (no known lint `code`) must not trigger the action.
#[test]
fn ignores_non_lint_diagnostics() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.rock");
    let mut ws = bone_ws(&path, UNUSED_VAR_SRC);
    let fake = Diagnostic {
        range: Range::new(Position::new(1, 8), Position::new(1, 20)),
        source: Some("bone".to_string()),
        code: Some(NumberOrString::String("not_a_lint".to_string())),
        message: "something else".to_string(),
        ..Default::default()
    };
    let mut params = params_at(&path, 1, 8);
    params.context.diagnostics = vec![fake];
    let actions = compute_code_actions_workspace(&mut ws, &params)
        .unwrap()
        .unwrap_or_default();
    assert!(
        suppress_action(&actions).is_none(),
        "unknown code must not trigger: {actions:?}"
    );
}
