//! `rock lsp diagnostics <dir>` and `rock lsp lint <dir>` over every `.rock`
//! file under a directory.

use crate::fixture::Project;
use rock_lsp::server_cli::{diagnostics_report, lint_report, print_report};

fn messages(report: &serde_json::Value) -> Vec<String> {
    report["files"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|f| f["diagnostics"].as_array().unwrap())
        .map(|d| d["message"].as_str().unwrap().to_string())
        .collect()
}

// The LSP reads a package's identity-storage state from its own manifest, in
// the one-shot report and in the server workspace alike.
#[test]
fn unreadable_file_fails_the_dir_report_by_name() {
    // Silently dropping the file would report a clean project.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rock"), "fn main() {}\n").unwrap();
    std::fs::write(dir.path().join("bad.rock"), b"fn main() {}\n\xff\xfe\n").unwrap();
    let path = dir.path().to_str().unwrap();
    for (what, res) in [
        ("diagnostics", diagnostics_report(path)),
        ("lint", lint_report(path)),
    ] {
        let err = print_report(res.unwrap()).expect_err(what).to_string();
        assert!(
            err.contains("bad.rock") && !err.contains("a.rock,"),
            "{what}: {err}"
        );
    }
}
