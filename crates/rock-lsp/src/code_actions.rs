use lsp_types::{
    CodeAction, CodeActionKind, CodeActionParams, Diagnostic, Position, Range, TextEdit,
    WorkspaceEdit,
};
use std::collections::HashMap;

use crate::workspace::LspWorkspace;

pub fn compute_code_actions_workspace(
    workspace: &mut LspWorkspace,
    params: &CodeActionParams,
) -> anyhow::Result<Option<Vec<CodeAction>>> {
    let uri = &params.text_document.uri;
    let path = &uri.to_file_path()?;

    let mut actions: Vec<CodeAction> = Vec::new();

    // Fill in missing enum match arms. When the non-exhaustive-match
    // diagnostic is under the cursor, attach it so the editor offers this as
    // the quick-fix for that error (lightbulb on the squiggle).
    if let Some(fill) = workspace.bone.fill_match_arms(path, params.range.start) {
        let mut action = bone_edit_action(
            format!(
                "Fill in {} missing match arm{}",
                fill.missing,
                if fill.missing == 1 { "" } else { "s" }
            ),
            uri,
            fill.range,
            fill.new_text,
        );
        let linked = matching_diagnostics(params, "Non-exhaustive match");
        if !linked.is_empty() {
            action.diagnostics = Some(linked);
        }
        actions.push(action);
    }

    // Fill in missing required struct-literal fields.
    if let Some(fill) = workspace.bone.fill_struct_fields(path, params.range.start) {
        actions.push(bone_edit_action(
            "Fill struct fields".to_string(),
            uri,
            fill.range,
            fill.new_text,
        ));
    }

    // Inline a local: replace its uses with its initializer and delete the `let`.
    if let Some(inline) = workspace.bone.inline_local(path, params.range.start) {
        actions.push(bone_multi_edit_action(
            format!("Inline local `{}`", inline.var),
            uri,
            inline.edits,
            CodeActionKind::REFACTOR_INLINE,
        ));
    }

    // Generate a stub for a call to an undeclared function.
    if let Some(stub) = workspace.bone.generate_function(path, params.range.start) {
        actions.push(bone_edit_action(
            format!("Generate function `{}`", stub.fn_name),
            uri,
            stub.range,
            stub.new_text,
        ));
    }

    // Suppress a lint: insert `// rock: allow(<lint>)` above the offending line.
    actions.extend(try_suppress_lint_actions(workspace, params, uri, path));

    Ok((!actions.is_empty()).then_some(actions))
}

/// One "Suppress with `// rock: allow(<lint>)`" action per lint diagnostic under
/// the cursor. Lint squiggles are identified by the `Diagnostic.code` field
/// (a known lint name), not by message-sniffing; the matched diagnostic is
/// attached so the editor offers this as THE fix for that squiggle. The insert
/// goes on a new line directly above the offender, copying its leading
/// whitespace so the comment lines up.
fn try_suppress_lint_actions(
    workspace: &LspWorkspace,
    params: &CodeActionParams,
    uri: &lsp_types::Uri,
    path: &str,
) -> Vec<CodeAction> {
    params
        .context
        .diagnostics
        .iter()
        .filter(|d| d.source.as_deref() == Some("bone") && ranges_overlap(d.range, params.range))
        .filter_map(|d| lint_name(d).map(|lint| (d, lint)))
        .map(|(d, lint)| {
            let line = d.range.start.line;
            let indent = workspace
                .bone
                .file_text(path)
                .and_then(|src| src.lines().nth(line as usize))
                .map(leading_whitespace)
                .unwrap_or_default();
            let insert = Range::new(Position::new(line, 0), Position::new(line, 0));
            let mut action = bone_edit_action(
                format!("Suppress with `// rock: allow({lint})`"),
                uri,
                insert,
                format!("{indent}// rock: allow({lint})\n"),
            );
            action.diagnostics = Some(vec![d.clone()]);
            action
        })
        .collect()
}

/// The lint name a bone diagnostic carries in its `code` field, iff it is one of
/// the known lints. `None` for a non-lint diagnostic (type error, hole, ...).
fn lint_name(d: &Diagnostic) -> Option<&'static str> {
    let lsp_types::NumberOrString::String(code) = d.code.as_ref()? else {
        return None;
    };
    bone_lints::KNOWN_LINTS
        .iter()
        .copied()
        .find(|k| *k == code.as_str())
}

/// The leading whitespace (spaces/tabs) of `line`.
fn leading_whitespace(line: &str) -> String {
    line.chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect()
}

/// The bone diagnostics from the request context whose message contains `needle`
/// and whose range overlaps the requested range -- the ones this quick-fix
/// resolves. Returned so they can be attached to the `CodeAction.diagnostics`.
fn matching_diagnostics(params: &CodeActionParams, needle: &str) -> Vec<Diagnostic> {
    params
        .context
        .diagnostics
        .iter()
        .filter(|d| {
            d.source.as_deref() == Some("bone")
                && d.message.contains(needle)
                && ranges_overlap(d.range, params.range)
        })
        .cloned()
        .collect()
}

/// LSP ranges (line/char, end-exclusive) overlap.
fn ranges_overlap(a: Range, b: Range) -> bool {
    a.start <= b.end && b.start <= a.end
}

/// Build a single-edit `QUICKFIX` `CodeAction` for the bone backend: one
/// `TextEdit` (range + new_text) on `uri`, wrapped in a `WorkspaceEdit`.
fn bone_edit_action(
    title: String,
    uri: &lsp_types::Uri,
    range: lsp_types::Range,
    new_text: String,
) -> CodeAction {
    bone_multi_edit_action(
        title,
        uri,
        vec![TextEdit { range, new_text }],
        CodeActionKind::QUICKFIX,
    )
}

/// Build a multi-edit `CodeAction` of `kind` for the bone backend: several
/// `TextEdit`s on one `uri`, wrapped in a `WorkspaceEdit`.
fn bone_multi_edit_action(
    title: String,
    uri: &lsp_types::Uri,
    edits: Vec<TextEdit>,
    kind: CodeActionKind,
) -> CodeAction {
    let changes = [(uri.clone(), edits)]
        .into_iter()
        .collect::<HashMap<_, _>>();
    CodeAction {
        title,
        kind: Some(kind),
        diagnostics: None,
        edit: Some(WorkspaceEdit::new(changes)),
        command: None,
        is_preferred: None,
        disabled: None,
        data: None,
    }
}
