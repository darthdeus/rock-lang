//! Interactive-server document/workspace-symbol handlers on the bone backend.
//!
//! The `.rock.lsp` fixtures only drive the one-shot CLI subcommands; these
//! exercise the server adapters directly -- an `LspWorkspace` with
//! `backend = Bone`, fed via `bone.update` exactly as didOpen/didChange do --
//! so the dispatch wiring in `lib.rs` can't silently rot.

use crate::common::bone_ws;
use lsp_types::*;
use rock_lsp::{do_document_symbols_workspace, do_workspace_symbols_workspace};
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

fn uri_of(path: &std::path::Path) -> Uri {
    Uri::from_str(&format!("file://{}", path.display())).unwrap()
}

fn workspace_names(ws: &mut rock_lsp::LspWorkspace, query: &str) -> Vec<String> {
    let params = WorkspaceSymbolParams {
        query: query.to_string(),
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    };
    match do_workspace_symbols_workspace(ws, &params).unwrap() {
        Some(WorkspaceSymbolResponse::Flat(symbols)) => {
            symbols.into_iter().map(|symbol| symbol.name).collect()
        }
        other => panic!("expected flat workspace symbols, got {other:?}"),
    }
}

fn package_workspace() -> (tempfile::TempDir, rock_lsp::LspWorkspace) {
    let dir = tempfile::tempdir().unwrap();
    let app_dir = dir.path().join("app");
    let util_dir = dir.path().join("util");
    std::fs::create_dir_all(&app_dir).unwrap();
    std::fs::create_dir_all(&util_dir).unwrap();
    std::fs::write(
        app_dir.join("stick.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\ntype = \"bin\"\n\n\
         [language]\nmodules = \"v1\"\n\n\
         [dependencies]\nutil = { path = \"../util\" }\n",
    )
    .unwrap();
    std::fs::write(
        util_dir.join("stick.toml"),
        "[package]\nname = \"util\"\nversion = \"0.1.0\"\ntype = \"lib\"\n\n\
         [language]\nmodules = \"v1\"\n",
    )
    .unwrap();
    std::fs::write(
        util_dir.join("lib.rock"),
        "pub @test fn heal_case_one() {}\n\
         pub fn heal_first() {}\n\
         pub @test fn heal_case_two() {}\n\
         pub fn heal_second() {}\n\
         pub struct Shape { pub size: int, }\n\
         pub enum Mode { Ready }\n\
         pub newtype Id(int)\n\
         pub global LIMIT: int = 1\n",
    )
    .unwrap();
    let app_main = app_dir.join("main.rock");
    let app_src = "use util::heal_first;\n\nfn main() {\n    heal_first()\n}\n";
    std::fs::write(&app_main, app_src).unwrap();
    let mut ws = rock_lsp::LspWorkspace::new();
    let errors: Vec<_> = ws
        .bone
        .update(app_main.to_str().unwrap(), app_src.to_string())
        .into_iter()
        .flat_map(|(_, diagnostics)| diagnostics)
        .filter(|diagnostic| diagnostic.severity == Some(DiagnosticSeverity::ERROR))
        .collect();
    assert!(errors.is_empty(), "fixture must build cleanly: {errors:?}");
    (dir, ws)
}

#[test]
fn document_symbols_returns_top_level_decls() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.rock");
    let mut ws = bone_ws(&path, SRC);

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
        assert!(
            names.iter().any(|n| n == want),
            "missing {want} in {names:?}"
        );
    }
}

#[test]
fn workspace_symbols_filters_by_query() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.rock");
    let mut ws = bone_ws(&path, SRC);

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
    assert_eq!(
        hits.len(),
        1,
        "query 'heal' should match exactly one: {hits:?}"
    );
    assert_eq!(hits[0].0, "heal");
    assert_eq!(hits[0].1, SymbolKind::FUNCTION);
}

#[test]
fn workspace_symbols_ignores_package_prefix() {
    let (_dir, mut ws) = package_workspace();
    let all = workspace_names(&mut ws, "");

    for want in [
        "util::heal_case_one",
        "util::heal_first",
        "util::heal_case_two",
        "util::heal_second",
        "util::Shape",
        "util::Mode",
        "util::Id",
        "util::LIMIT",
    ] {
        assert!(
            all.iter().any(|name| name == want),
            "missing {want}: {all:?}"
        );
    }

    let hits = workspace_names(&mut ws, "UTIL");
    assert!(
        hits.is_empty(),
        "package-only query must not match symbols: {hits:?}"
    );
}

#[test]
fn workspace_symbols_matches_qualified_query() {
    let (_dir, mut ws) = package_workspace();
    assert_eq!(
        workspace_names(&mut ws, "UTil::hEaL_FiRsT"),
        vec!["util::heal_first".to_string()]
    );

    let all = workspace_names(&mut ws, "");
    let util_names: Vec<_> = all
        .into_iter()
        .filter(|name| name.starts_with("util::"))
        .collect();
    assert_eq!(workspace_names(&mut ws, "UTIL::"), util_names);
}

#[test]
fn workspace_symbols_ranks_tests_last() {
    let (_dir, mut ws) = package_workspace();
    let matches = workspace_names(&mut ws, "hEaL_");
    assert_eq!(
        matches,
        vec![
            "util::heal_first".to_string(),
            "util::heal_second".to_string(),
            "util::heal_case_one".to_string(),
            "util::heal_case_two".to_string(),
        ]
    );

    let all = workspace_names(&mut ws, "");
    let heal_names: Vec<_> = all
        .iter()
        .filter(|name| name.to_lowercase().contains("heal_"))
        .cloned()
        .collect();
    assert_eq!(matches, heal_names);
    assert_eq!(
        &all[all.len() - 2..],
        &[
            "util::heal_case_one".to_string(),
            "util::heal_case_two".to_string(),
        ]
    );
}
