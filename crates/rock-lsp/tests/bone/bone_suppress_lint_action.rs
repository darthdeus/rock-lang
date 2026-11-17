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

#[test]
fn lint_codes_gate_suppression_and_actions_carry_the_resolved_diagnostic() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.rock");
    let mut ws = bone_ws(&path, UNUSED_VAR_SRC);

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
    let fake = Diagnostic {
        range: Range::new(Position::new(1, 8), Position::new(1, 20)),
        source: Some("bone".to_string()),
        code: Some(NumberOrString::String("not_a_lint".to_string())),
        message: "something else".to_string(),
        ..Default::default()
    };
    for diagnostics in [vec![], vec![fake]] {
        let mut params = params_at(&path, 1, 8);
        params.context.diagnostics = diagnostics;
        let actions = compute_code_actions_workspace(&mut ws, &params)
            .unwrap()
            .unwrap_or_default();
        assert!(
            suppress_action(&actions).is_none(),
            "only known lint codes trigger suppression: {actions:?}"
        );
    }
}
