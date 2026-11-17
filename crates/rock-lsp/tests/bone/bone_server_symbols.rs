//! Workspace-symbol queries across packages, through the server adapter.

use crate::fixture::{Project, assert_no_errors};
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

#[test]
fn workspace_symbols_order_by_name_file_and_position() {
    let files = [
        ("z/main.rock", "fn series_same() {}\nfn series_z() {}"),
        (
            "a/main.rock",
            "fn series_same() {}\nfn series_same(n: int) {}\nfn series_a() {}",
        ),
    ];
    let project = Project::new(&files);
    let mut ws = rock_lsp::LspWorkspace::new();
    for (file, text) in files {
        assert_no_errors(&ws.bone.update(&project.path(file), text.into()));
    }
    let hits = do_workspace_symbols_workspace(
        &mut ws,
        &WorkspaceSymbolParams {
            query: "series_".into(),
        },
    )
    .unwrap()
    .unwrap();
    let keys: Vec<_> = hits
        .iter()
        .map(|hit| {
            let file = if hit.location.uri.as_str().ends_with("/a/main.rock") {
                'a'
            } else {
                'z'
            };
            (hit.name.as_str(), file, hit.location.range.start.line)
        })
        .collect();
    assert_eq!(
        keys,
        [
            ("series_a", 'a', 2),
            ("series_same", 'a', 0),
            ("series_same", 'a', 1),
            ("series_same", 'z', 0),
            ("series_z", 'z', 1),
        ]
    );
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
    let diagnostics = ws
        .bone
        .update(&project.path("app/main.rock"), app_src.to_string());
    assert_no_errors(&diagnostics);
    (project, ws)
}

#[test]
fn workspace_symbols_match_bare_and_qualified_names() {
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
            "util::heal_case_one".to_string(),
            "util::heal_case_two".to_string(),
            "util::heal_first".to_string(),
            "util::heal_second".to_string(),
        ]
    );

    let heal_names: Vec<_> = all
        .iter()
        .filter(|name| name.to_lowercase().contains("heal_"))
        .cloned()
        .collect();
    assert_eq!(matches, heal_names);
}
