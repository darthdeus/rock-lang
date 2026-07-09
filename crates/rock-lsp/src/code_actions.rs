use lsp_types::{
    CodeAction, CodeActionKind, CodeActionParams, Diagnostic, Range, TextEdit, WorkspaceEdit,
};
use std::collections::HashMap;

use crate::workspace::LspWorkspace;

pub fn compute_code_actions_workspace(
    workspace: &mut LspWorkspace,
    params: &CodeActionParams,
) -> anyhow::Result<Option<Vec<CodeAction>>> {
    let uri = &params.text_document.uri;
    let uri_str = uri.as_str();
    // Plain `file://` strip to match the key bone.update registers.
    let path = uri_str.strip_prefix("file://").unwrap_or(uri_str);

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

    Ok((!actions.is_empty()).then_some(actions))
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
    bone_multi_edit_action(title, uri, vec![TextEdit { range, new_text }], CodeActionKind::QUICKFIX)
}

/// Build a multi-edit `CodeAction` of `kind` for the bone backend: several
/// `TextEdit`s on one `uri`, wrapped in a `WorkspaceEdit`.
fn bone_multi_edit_action(
    title: String,
    uri: &lsp_types::Uri,
    edits: Vec<TextEdit>,
    kind: CodeActionKind,
) -> CodeAction {
    let changes = [(uri.clone(), edits)].into_iter().collect::<HashMap<_, _>>();
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

