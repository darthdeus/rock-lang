use bone_query::FileUriExt;
use lsp_types::*;
use rock_lsp::{LspWorkspace, compute_code_actions_workspace, do_completion_workspace};

/// Write `src` to `path` and open it on a fresh bone workspace.
pub fn bone_ws(path: &std::path::Path, src: &str) -> LspWorkspace {
    std::fs::write(path, src).unwrap();
    let mut ws = LspWorkspace::new();
    ws.bone.update(path.to_str().unwrap(), src.to_string());
    ws
}

pub fn uri(path: &std::path::Path) -> Uri {
    Uri::from_file_path(path.to_str().unwrap()).unwrap()
}

pub fn at(path: &std::path::Path, line: u32, ch: u32) -> TextDocumentPositionParams {
    TextDocumentPositionParams::new(
        TextDocumentIdentifier { uri: uri(path) },
        Position::new(line, ch),
    )
}

/// The code actions for a zero-width range at `(line, ch)` with `diagnostics`
/// in the request context.
pub fn actions_at(
    ws: &mut LspWorkspace,
    path: &std::path::Path,
    line: u32,
    ch: u32,
    diagnostics: Vec<Diagnostic>,
) -> Vec<CodeAction> {
    let params = CodeActionParams {
        text_document: TextDocumentIdentifier { uri: uri(path) },
        range: Range::new(Position::new(line, ch), Position::new(line, ch)),
        context: CodeActionContext {
            diagnostics,
            ..CodeActionContext::default()
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    };
    compute_code_actions_workspace(ws, &params)
        .unwrap()
        .unwrap_or_default()
}

/// The code actions offered for `src` with the cursor at `(line, ch)`.
pub fn run(src: &str, line: u32, ch: u32) -> Vec<CodeAction> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.rock");
    actions_at(&mut bone_ws(&path, src), &path, line, ch, vec![])
}

pub fn completion(
    ws: &mut LspWorkspace,
    path: &std::path::Path,
    line: u32,
    ch: u32,
) -> CompletionList {
    let params = CompletionParams {
        text_document_position: at(path, line, ch),
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
        context: None,
    };
    do_completion_workspace(ws, &params)
        .unwrap()
        .expect("completion response")
}

/// Apply all of a code action's single-file edits.
pub fn apply_all(src: &str, action: &CodeAction) -> String {
    let edit = action.edit.as_ref().expect("action carries an edit");
    #[expect(
        clippy::mutable_key_type,
        reason = "WorkspaceEdit Uri keys use immutable text for equality and hashing"
    )]
    let changes = edit.changes.as_ref().expect("edit has changes");
    let edits = changes.values().next().expect("one file's edits");
    crate::fixture::apply_edits(src, edits.to_vec())
}
