//! Shared bone LSP integration-test harness: workspace setup, code-action
//! params, and edit application. Pulled in via `mod common;` in main.rs; used by
//! the code-action and server-capability test modules.

use lsp_types::*;
use rock_lsp::{LspWorkspace, compute_code_actions_workspace};
use std::str::FromStr;

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

/// Compute the code actions offered for `src` with the cursor at `(line, ch)`.
pub fn run(src: &str, line: u32, ch: u32) -> Vec<CodeAction> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.rock");
    let mut ws = bone_ws(&path, src);
    compute_code_actions_workspace(&mut ws, &params_at(&path, line, ch))
        .unwrap()
        .unwrap_or_default()
}

/// Apply a code action's single-file edit to `src` and return the result.
pub fn apply(src: &str, action: &CodeAction) -> String {
    let edit = action.edit.as_ref().expect("action carries an edit");
    #[expect(
        clippy::mutable_key_type,
        reason = "WorkspaceEdit Uri keys use immutable text for equality and hashing"
    )]
    let changes = edit.changes.as_ref().expect("edit has changes");
    let edits = changes.values().next().expect("one file's edits");
    let te = &edits[0];
    let s = pos_to_offset(src, te.range.start);
    let e = pos_to_offset(src, te.range.end);
    format!("{}{}{}", &src[..s], te.new_text, &src[e..])
}

/// Apply ALL of a code action's single-file edits (descending by start, so
/// earlier offsets stay valid) and return the result. For multi-edit actions.
pub fn apply_all(src: &str, action: &CodeAction) -> String {
    let edit = action.edit.as_ref().expect("action carries an edit");
    #[expect(
        clippy::mutable_key_type,
        reason = "WorkspaceEdit Uri keys use immutable text for equality and hashing"
    )]
    let changes = edit.changes.as_ref().expect("edit has changes");
    let edits = changes.values().next().expect("one file's edits");
    let mut spans: Vec<(usize, usize, &str)> = edits
        .iter()
        .map(|te| {
            (
                pos_to_offset(src, te.range.start),
                pos_to_offset(src, te.range.end),
                te.new_text.as_str(),
            )
        })
        .collect();
    spans.sort_by_key(|x| std::cmp::Reverse(x.0));
    let mut s = src.to_string();
    for (start, end, text) in spans {
        s.replace_range(start..end, text);
    }
    s
}

/// Byte offset in `src` of an LSP `(line, character)` position.
#[expect(
    clippy::explicit_counter_loop,
    reason = "Preserve the u32 coordinate counter and its overflow behavior"
)]
pub fn pos_to_offset(src: &str, pos: Position) -> usize {
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
