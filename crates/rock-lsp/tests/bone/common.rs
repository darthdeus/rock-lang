//! Shared bone LSP integration-test harness: workspace setup, code-action
//! params, and edit application. Pulled in via `mod common;` in main.rs; used by
//! the code-action and server-capability test modules.

use bone_query::FileUriExt;
use lsp_types::*;
use rock_lsp::{LspWorkspace, compute_code_actions_workspace};

/// Write `src` to `path` and open it on a fresh bone workspace.
pub fn bone_ws(path: &std::path::Path, src: &str) -> LspWorkspace {
    std::fs::write(path, src).unwrap();
    let mut ws = LspWorkspace::new();
    ws.bone.update(path.to_str().unwrap(), src.to_string());
    ws
}

/// A zero-width `CodeActionParams` at `(line, ch)` for `path`.
pub fn params_at(path: &std::path::Path, line: u32, ch: u32) -> CodeActionParams {
    CodeActionParams {
        text_document: TextDocumentIdentifier {
            uri: Uri::from_file_path(path.to_str().unwrap()).unwrap(),
        },
        range: Range {
            start: Position::new(line, ch),
            end: Position::new(line, ch),
        },
        context: CodeActionContext {
            diagnostics: vec![],
            ..CodeActionContext::default()
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    }
}

/// Compute the code actions offered for `src` with the cursor at `(line, ch)`.
pub fn run(src: &str, line: u32, ch: u32) -> Vec<CodeAction> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.rock");
    let mut ws = bone_ws(&path, src);
    compute_code_actions_workspace(&mut ws, &params_at(&path, line, ch))
        .unwrap()
        .unwrap_or_default()
}

/// The edits a code action applies to its single file.
fn action_edits(action: &CodeAction) -> &[TextEdit] {
    let edit = action.edit.as_ref().expect("action carries an edit");
    #[expect(
        clippy::mutable_key_type,
        reason = "WorkspaceEdit Uri keys use immutable text for equality and hashing"
    )]
    let changes = edit.changes.as_ref().expect("edit has changes");
    changes.values().next().expect("one file's edits")
}

/// Apply a code action's first single-file edit.
pub fn apply(src: &str, action: &CodeAction) -> String {
    crate::fixture::apply_edits(src, vec![action_edits(action)[0].clone()])
}

/// Apply all single-file edits, descending by start so offsets stay valid.
pub fn apply_all(src: &str, action: &CodeAction) -> String {
    crate::fixture::apply_edits(src, action_edits(action).to_vec())
}
