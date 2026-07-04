//! Interactive-server document/workspace-symbol handlers on the bone backend.
//!
//! The `.rock.lsp` fixtures only drive the one-shot CLI subcommands; these
//! exercise the server adapters directly -- an `LspWorkspace` with
//! `backend = Bone`, fed via `bone.update` exactly as didOpen/didChange do --
//! so the dispatch wiring in `lib.rs` can't silently rot.

use lsp_types::*;
use rock_lsp::{
    do_document_symbols_workspace, do_workspace_symbols_workspace, LspBackend, LspWorkspace,
};
use std::str::FromStr;

const SRC: &str = "\
struct Player {
    health: int,
}

fn heal(p: Player) -> Player {
    return Player { health: p.health + 10 }
}

fn main() {
    let p = Player { health: 100 }
}
";

/// A bone-backed workspace with `SRC` opened at `path`, written to disk so the
/// session's project-root resolution (`stick.toml` walk + canonicalize) is
/// stable.
fn bone_ws(path: &std::path::Path) -> LspWorkspace {
    std::fs::write(path, SRC).unwrap();
    let mut ws = LspWorkspace::new();
    ws.backend = LspBackend::Bone;
    ws.bone.update(path.to_str().unwrap(), SRC.to_string());
    ws
}

fn uri_of(path: &std::path::Path) -> Uri {
    Uri::from_str(&format!("file://{}", path.display())).unwrap()
}

#[test]
fn document_symbols_returns_top_level_decls() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.rock");
    let mut ws = bone_ws(&path);

    let params = DocumentSymbolParams {
        text_document: TextDocumentIdentifier { uri: uri_of(&path) },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    };
    let resp = do_document_symbols_workspace(&mut ws, &params).unwrap();
    let names: Vec<String> = match resp {
        Some(DocumentSymbolResponse::Nested(syms)) => syms.into_iter().map(|s| s.name).collect(),
        other => panic!("expected nested document symbols, got {other:?}"),
    };
    for want in ["Player", "heal", "main"] {
        assert!(names.iter().any(|n| n == want), "missing {want} in {names:?}");
    }
}

#[test]
fn workspace_symbols_filters_by_query() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.rock");
    let mut ws = bone_ws(&path);

    let params = WorkspaceSymbolParams {
        query: "heal".to_string(),
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    };
    let resp = do_workspace_symbols_workspace(&mut ws, &params).unwrap();
    let hits: Vec<(String, SymbolKind)> = match resp {
        Some(WorkspaceSymbolResponse::Flat(syms)) => {
            syms.into_iter().map(|s| (s.name, s.kind)).collect()
        }
        other => panic!("expected flat workspace symbols, got {other:?}"),
    };
    assert_eq!(hits.len(), 1, "query 'heal' should match exactly one: {hits:?}");
    assert_eq!(hits[0].0, "heal");
    assert_eq!(hits[0].1, SymbolKind::FUNCTION);
}

