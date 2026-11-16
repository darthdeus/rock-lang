//! Workspace-symbol queries across packages, through the server adapter.

use lsp_types::*;
use rock_lsp::do_workspace_symbols_workspace;

fn workspace_names(ws: &mut rock_lsp::LspWorkspace, query: &str) -> Vec<String> {
    let params = WorkspaceSymbolParams {
        query: query.to_string(),
    };
    do_workspace_symbols_workspace(ws, &params)
        .unwrap()
        .expect("workspace symbols")
        .into_iter()
        .map(|symbol| symbol.name)
        .collect()
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
