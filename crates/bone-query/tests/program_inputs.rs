//! The editor loads a program's inputs the way a compile does: a stdlib that
//! does not parse and a package graph that does not build are diagnostics, not
//! a partial stdlib or an empty alias table that checks a different program.

use bone_query::BoneWorkspace;
use lsp_types::DiagnosticSeverity;
use rock_stdlib::StdlibFile;

fn errors(diags: &[(String, Vec<lsp_types::Diagnostic>)], path: &str) -> Vec<String> {
    diags
        .iter()
        .filter(|(file, _)| file == path)
        .flat_map(|(_, ds)| ds.iter())
        .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
        .map(|d| d.message.clone())
        .collect()
}

#[test]
fn a_stdlib_that_does_not_tokenize_is_a_diagnostic() {
    let stdlib = skull_parser::stdlib::parse(&[StdlibFile {
        name: "unified.rock",
        contents: "fn broken() {\n    let s = \"unterminated\n}\n",
        display_path: "<test-stdlib>/unified.rock",
    }]);
    let mut bone = BoneWorkspace::new().with_stdlib(stdlib);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.rock");
    let path = path.to_str().unwrap();
    let diags = bone.update(path, "fn main() {\n}\n".to_string());
    let errs = errors(&diags, path);
    assert!(
        errs.iter().any(|m| m.contains("unified.rock")),
        "expected a stdlib diagnostic, got {diags:?}"
    );
}

#[test]
fn a_stdlib_with_a_recovered_syntax_error_is_a_diagnostic() {
    let stdlib = skull_parser::stdlib::parse(&[StdlibFile {
        name: "unified.rock",
        contents: "fn ( {\n}\n",
        display_path: "<test-stdlib>/unified.rock",
    }]);
    assert!(stdlib.is_err(), "a recovered stdlib error must not load");
}

#[test]
fn a_package_graph_that_does_not_build_is_a_diagnostic() {
    let dir = tempfile::tempdir().unwrap();
    // The dependency does not exist, so the graph cannot be built. A script is
    // loaded without the graph, so only the alias table needs it.
    std::fs::write(
        dir.path().join("stick.toml"),
        "[package]\nname = \"app\"\n\n[dependencies]\nmissing = { path = \"../missing\" }\n",
    )
    .unwrap();
    let source = "#script\nfn main() {\n}\n";
    let file = dir.path().join("tool.rock");
    std::fs::write(&file, source).unwrap();
    let file = std::fs::canonicalize(file).unwrap();
    let path = file.to_str().unwrap();

    let mut bone = BoneWorkspace::new();
    let diags = bone.update(path, source.to_string());
    let errs = errors(&diags, path);
    assert!(
        errs.iter()
            .any(|m| m.contains("package graph") && !m.contains("Cannot load project sources")),
        "expected a package-graph diagnostic, got {diags:?}"
    );
}
