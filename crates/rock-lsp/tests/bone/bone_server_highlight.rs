//! Interactive-server document-highlight handler on the bone backend.
//!
//! Highlights every occurrence of the symbol under the cursor within the
//! current file (LSP `textDocument/documentHighlight`), reusing bone's
//! symbol-aware references query. Drives the adapter directly with a bone-backed
//! LspWorkspace, the same way the server does on a request.

use crate::common::bone_ws;
use lsp_types::*;
use rock_lsp::{LspWorkspace, do_document_highlight_workspace};
use std::str::FromStr;

// `count` is declared on line 1 and used on lines 2 and 3 -> three occurrences.
const SRC: &str = "\
fn main() {
    let count = 0
    let bumped = count + 1
    print(count)
}
";

fn highlight_at(
    ws: &mut LspWorkspace,
    path: &std::path::Path,
    line: u32,
    ch: u32,
) -> Vec<DocumentHighlight> {
    let params = DocumentHighlightParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: Uri::from_str(&format!("file://{}", path.display())).unwrap(),
            },
            position: Position::new(line, ch),
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    };
    do_document_highlight_workspace(ws, &params)
        .unwrap()
        .unwrap_or_default()
}

#[test]
fn highlights_all_occurrences_of_a_local() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.rock");
    let mut ws = bone_ws(&path, SRC);

    // Cursor on the `count` declaration (line 1, the `c` at char 8).
    let hl = highlight_at(&mut ws, &path, 1, 8);

    let mut lines: Vec<u32> = hl.iter().map(|h| h.range.start.line).collect();
    lines.sort();
    assert_eq!(
        lines,
        vec![1, 2, 3],
        "expected `count` highlighted on lines 1/2/3, got {hl:?}"
    );
    // Each highlight covers the 5-char identifier `count`.
    for h in &hl {
        assert_eq!(
            h.range.end.character - h.range.start.character,
            5,
            "range: {h:?}"
        );
    }
}

#[test]
fn highlight_from_a_use_site_finds_the_same_set() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.rock");
    let mut ws = bone_ws(&path, SRC);

    // Cursor on the `count` use inside print() (line 3, char 10).
    let hl = highlight_at(&mut ws, &path, 3, 10);
    let mut lines: Vec<u32> = hl.iter().map(|h| h.range.start.line).collect();
    lines.sort();
    assert_eq!(lines, vec![1, 2, 3], "from a use site, got {hl:?}");
}
