//! Interactive-server document-highlight handler on the bone backend.
//!
//! Highlights every occurrence of the symbol under the cursor within the
//! current file (LSP `textDocument/documentHighlight`), reusing bone's
//! symbol-aware references query. Drives the adapter directly with a bone-backed
//! LspWorkspace, the same way the server does on a request.

use crate::common::bone_ws;
use lsp_types::*;
use rock_lsp::{LspWorkspace, do_document_highlight_workspace};

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
    let params = TextDocumentPositionParams {
        text_document: TextDocumentIdentifier {
            uri: Uri::from_file_path(path.to_str().unwrap()).unwrap(),
        },
        position: Position::new(line, ch),
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

    for (line, column) in [(1, 8), (3, 10)] {
        let highlights = highlight_at(&mut ws, &path, line, column);
        let mut lines: Vec<_> = highlights.iter().map(|h| h.range.start.line).collect();
        lines.sort();
        assert_eq!(lines, [1, 2, 3], "cursor at {line}:{column}");
        for h in &highlights {
            assert_eq!(h.range.end.character - h.range.start.character, 5, "{h:?}");
        }
    }
}
