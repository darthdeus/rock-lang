//! `rock lsp diagnostics <dir>`: whole-project diagnostics over every `.rock`
//! under a directory, exercising the directory branch of `diagnostics_report`.

use rock_lsp::cli::bone_lsp::{diagnostics_report, lint_report};

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
    std::fs::write(
        dir.path().join("stick.toml"),
        "[package]\nname = \"g\"\ntype = \"bin\"\n",
    )
    .unwrap();
    // a.rock: an uncalled non-root fn -> whole-project unused_fn (needs the
    // directory load to see there's no caller anywhere).
    std::fs::write(dir.path().join("a.rock"), "fn main() {}\nfn orphan() {}\n").unwrap();
    // b.rock: a plain type error, file-local.
    std::fs::write(
        dir.path().join("b.rock"),
        "fn b() -> int {\n  \"nope\"\n}\n",
    )
    .unwrap();

    let report = diagnostics_report(dir.path().to_str().unwrap()).unwrap();

    assert_eq!(report["dir"].is_string(), true, "dir report shape");
    assert_eq!(
        report["file_count"].as_u64(),
        Some(2),
        "walked both .rock files"
    );

    let msgs = messages(&report);
    assert!(
        msgs.iter().any(|m| m.contains("unused function `orphan`")),
        "whole-project unused_fn must appear: {msgs:?}"
    );
    assert!(
        msgs.iter()
            .any(|m| m.to_lowercase().contains("int") || m.to_lowercase().contains("type")),
        "b.rock's type error must appear: {msgs:?}"
    );
}

#[test]
fn lint_report_excludes_typecheck_errors() {
    // `rock lsp lint` reports only lints -- the type error that `diagnostics`
    // surfaces must NOT appear, but the unused_fn/unused_var findings must.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("stick.toml"),
        "[package]\nname = \"g\"\ntype = \"bin\"\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("a.rock"), "fn main() {}\nfn orphan() {}\n").unwrap();
    std::fs::write(
        dir.path().join("b.rock"),
        "fn b() -> int {\n  \"nope\"\n}\n",
    )
    .unwrap();

    let diag_msgs = messages(&diagnostics_report(dir.path().to_str().unwrap()).unwrap());
    let lint_msgs = messages(&lint_report(dir.path().to_str().unwrap()).unwrap());

    // Sanity: diagnostics DOES carry the type error.
    assert!(
        diag_msgs
            .iter()
            .any(|m| m.contains("unify") || m.to_lowercase().contains("int")),
        "diagnostics should include the type error: {diag_msgs:?}"
    );
    // Lint report: the unused_fn lint is present, the type error is not.
    assert!(
        lint_msgs
            .iter()
            .any(|m| m.contains("unused function `orphan`")),
        "lint report must include unused_fn: {lint_msgs:?}"
    );
    assert!(
        !lint_msgs.iter().any(|m| m.contains("unify")),
        "lint report must exclude typecheck errors: {lint_msgs:?}"
    );
}

#[test]
fn dir_stays_silent_for_lib() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("stick.toml"),
        "[package]\nname = \"l\"\ntype = \"lib\"\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("a.rock"), "fn public_api() {}\n").unwrap();

    let report = diagnostics_report(dir.path().to_str().unwrap()).unwrap();
    let msgs = messages(&report);
    assert!(
        !msgs.iter().any(|m| m.contains("unused function")),
        "lib package must not run unused_fn: {msgs:?}"
    );
}

// The LSP reads a package's identity-storage state from its own manifest, in
// the one-shot report and in the server workspace alike.
#[test]
fn identity_storage_diagnostics_follow_the_package_declaration() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("main.rock");
    let source = "struct Actor { hp: int }\nstruct Holder { actor: Actor }\nfn main() {}\n";
    std::fs::write(&file, source).unwrap();
    let path = file.to_str().unwrap();
    let reported = |manifest: &str| {
        std::fs::write(dir.path().join("stick.toml"), manifest).unwrap();
        let report = diagnostics_report(dir.path().to_str().unwrap()).unwrap();
        let in_report = messages(&report)
            .iter()
            .any(|m| m.contains("cannot store identity struct `Actor`"));
        let mut server = rock_lsp::workspace::LspWorkspace::new();
        server.bone.update(path, source.to_owned());
        let in_server = server
            .bone
            .diagnostics_for(path)
            .iter()
            .any(|d| d.message.contains("cannot store identity struct `Actor`"));
        assert_eq!(in_report, in_server, "{manifest}: {report}");
        in_report
    };
    assert!(
        reported("[package]\nname = \"example-lib\"\n"),
        "a package that declares nothing is enforced"
    );
    assert!(
        !reported("[package]\nname = \"example-lib\"\n[language]\nidentity_storage = \"exempt\"\n"),
        "a package declared exempt gets no identity-storage diagnostics"
    );
}

#[test]
fn unreadable_file_fails_the_dir_report_by_name() {
    // A .rock file that is not UTF-8 cannot be read; the walk must name it and
    // fail rather than report a clean project with the file silently dropped.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rock"), "fn main() {}\n").unwrap();
    std::fs::write(dir.path().join("bad.rock"), b"fn main() {}\n\xff\xfe\n").unwrap();
    let path = dir.path().to_str().unwrap();
    for (what, res) in [
        ("diagnostics", rock_lsp::cli::bone_lsp::diagnostics(path)),
        ("lint", rock_lsp::cli::bone_lsp::lint(path)),
    ] {
        let err = res.expect_err(what).to_string();
        assert!(
            err.contains("bad.rock") && !err.contains("a.rock,"),
            "{what}: {err}"
        );
    }
}
