//! Workspace-symbol queries across packages, through the server adapter.

use crate::fixture::Project;
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

fn package_workspace() -> (Project, rock_lsp::LspWorkspace) {
    let app_src = "use util::heal_first;\n\nfn main() {\n    heal_first()\n}\n";
    let project = Project::new(&[
        (
            "app/stick.toml",
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\ntype = \"bin\"\n\n\
             [language]\nmodules = \"v1\"\n\n\
             [dependencies]\nutil = { path = \"../util\" }\n",
        ),
        (
            "util/stick.toml",
            "[package]\nname = \"util\"\nversion = \"0.1.0\"\ntype = \"lib\"\n\n\
             [language]\nmodules = \"v1\"\n",
        ),
        (
            "util/lib.rock",
            "pub @test fn heal_case_one() {}\n\
             pub fn heal_first() {}\n\
             pub @test fn heal_case_two() {}\n\
             pub fn heal_second() {}\n\
             pub struct Shape { pub size: int, }\n\
             pub enum Mode { Ready }\n\
             pub newtype Id(int)\n\
             pub global LIMIT: int = 1\n",
        ),
        ("app/main.rock", app_src),
    ]);
    let mut ws = rock_lsp::LspWorkspace::new();
    let errors: Vec<_> = ws
        .bone
        .update(&project.path("app/main.rock"), app_src.to_string())
        .into_iter()
        .flat_map(|(_, diagnostics)| diagnostics)
        .filter(|diagnostic| diagnostic.severity == Some(DiagnosticSeverity::ERROR))
        .collect();
    assert!(errors.is_empty(), "fixture must build cleanly: {errors:?}");
    (project, ws)
}

#[test]
fn workspace_symbols_match_qualified_names_and_rank_tests_last() {
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
    assert_eq!(
        workspace_names(&mut ws, "UTil::hEaL_FiRsT"),
        vec!["util::heal_first".to_string()]
    );

    let util_names: Vec<_> = all
        .iter()
        .filter(|name| name.starts_with("util::"))
        .cloned()
        .collect();
    assert_eq!(workspace_names(&mut ws, "UTIL::"), util_names);
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
