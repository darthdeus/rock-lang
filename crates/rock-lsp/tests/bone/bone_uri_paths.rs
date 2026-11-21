//! Document URIs are percent-decoded to the file they name, so a project in a
//! directory with a space or a non-ASCII file name resolves like any other, and
//! locations the server returns encode such paths back; a non-file URI is a
//! visible request error, never fed to bone as a path.

use crate::common::bone_ws;
use bone_query::FileUriExt;
use lsp_types::*;
use rock_lsp::semantic_tokens::compute_semantic_tokens_workspace;
use rock_lsp::{LspWorkspace, do_document_symbols_workspace, do_workspace_symbols_workspace};
use std::str::FromStr;

const SRC: &str = "fn heal_all(x: int) -> int {\n    return x + 1\n}\n\nfn main() {\n    let y = heal_all(2)\n}\n";

/// A workspace holding `<tmp>/my dir/caf\u{e9}.rock` plus the URI an editor sends
/// for it, spelled out by hand so the test does not trust the encoder.
fn spaced_project() -> (tempfile::TempDir, std::path::PathBuf, Uri, LspWorkspace) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("my dir")).unwrap();
    let path = dir.path().join("my dir").join("caf\u{e9}.rock");
    let ws = bone_ws(&path, SRC);
    let root_uri = Uri::from_file_path(dir.path().to_str().unwrap()).unwrap();
    let root = root_uri.as_str();
    assert!(
        root.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/:.%_-".contains(&b))
    );
    let uri = Uri::from_str(&format!("{root}/my%20dir/caf%C3%A9.rock")).unwrap();
    (dir, path, uri, ws)
}

fn symbol_params(uri: Uri) -> DocumentSymbolParams {
    DocumentSymbolParams {
        text_document: TextDocumentIdentifier { uri },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    }
}

#[test]
fn encoded_document_uri_resolves_to_the_file() {
    let (_dir, _path, uri, mut ws) = spaced_project();
    let names: Vec<String> = do_document_symbols_workspace(&mut ws, &symbol_params(uri.clone()))
        .unwrap()
        .expect("document symbols")
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert!(
        names.iter().any(|n| n == "heal_all"),
        "symbols of the decoded file: {names:?}"
    );

    let t = compute_semantic_tokens_workspace(&mut ws, &uri).unwrap();
    assert!(!t.data.is_empty(), "no tokens for decoded file");
}
