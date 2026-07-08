//! `rock lsp diagnostics <dir>`: whole-project diagnostics over every `.rock`
//! under a directory, exercising the directory branch of `diagnostics_report`.

use rock_lsp::cli::bone_lsp::diagnostics_report;

fn messages(report: &serde_json::Value) -> Vec<String> {
    report["files"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|f| f["diagnostics"].as_array().unwrap())
        .map(|d| d["message"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn dir_reports_diagnostics_across_files() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("stick.toml"), "[package]\nname = \"g\"\ntype = \"bin\"\n").unwrap();
    // a.rock: an uncalled non-root fn -> whole-project unused_fn (needs the
    // directory load to see there's no caller anywhere).
    std::fs::write(dir.path().join("a.rock"), "fn main() {}\nfn orphan() {}\n").unwrap();
    // b.rock: a plain type error, file-local.
    std::fs::write(dir.path().join("b.rock"), "fn b() -> int {\n  \"nope\"\n}\n").unwrap();

    let report = diagnostics_report(dir.path().to_str().unwrap()).unwrap();

    assert_eq!(report["dir"].is_string(), true, "dir report shape");
    assert_eq!(report["file_count"].as_u64(), Some(2), "walked both .rock files");

    let msgs = messages(&report);
    assert!(
        msgs.iter().any(|m| m.contains("unused function `orphan`")),
        "whole-project unused_fn must appear: {msgs:?}"
    );
    assert!(
        msgs.iter().any(|m| m.to_lowercase().contains("int") || m.to_lowercase().contains("type")),
        "b.rock's type error must appear: {msgs:?}"
    );
}

#[test]
fn dir_stays_silent_for_lib() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("stick.toml"), "[package]\nname = \"l\"\ntype = \"lib\"\n").unwrap();
    std::fs::write(dir.path().join("a.rock"), "fn public_api() {}\n").unwrap();

    let report = diagnostics_report(dir.path().to_str().unwrap()).unwrap();
    let msgs = messages(&report);
    assert!(
        !msgs.iter().any(|m| m.contains("unused function")),
        "lib package must not run unused_fn: {msgs:?}"
    );
}
