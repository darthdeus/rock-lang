//! `rock lsp diagnostics <dir>` and `rock lsp lint <dir>` over every `.rock`
//! file under a directory.

use crate::fixture::Project;
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
    // The orphan needs whole-project reachability; b also has a type error.
    let project = Project::new(&[
        ("stick.toml", "[package]\nname = \"g\"\ntype = \"bin\"\n"),
        ("a.rock", "fn main() {}\nfn orphan() {}\n"),
        ("b.rock", "fn b() -> int {\n  \"nope\"\n}\n"),
    ]);
    let dir = project.dir.path().to_str().unwrap();
    let report = diagnostics_report(dir).unwrap();

    assert!(report["dir"].is_string(), "dir report shape");
    assert_eq!(
        report["file_count"].as_u64(),
        Some(2),
        "walked both .rock files"
    );

    let msgs = messages(&report);
    let lint_msgs = messages(&lint_report(dir).unwrap());
    for (report, msgs, type_error) in [("diagnostics", &msgs, true), ("lint", &lint_msgs, false)] {
        assert!(
            msgs.iter().any(|m| m.contains("unused function `orphan`")),
            "{report}: whole-project unused_fn: {msgs:?}"
        );
        assert_eq!(
            msgs.iter().any(|m| m.contains("type mismatch")),
            type_error,
            "{report}: b.rock's type error: {msgs:?}"
        );
    }
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
            .any(|m| m.contains("cannot store identity struct `example_lib::Actor`"));
        let mut server = rock_lsp::workspace::LspWorkspace::new();
        server.bone.update(path, source.to_owned());
        let in_server = server.bone.diagnostics_for(path).iter().any(|d| {
            d.message
                .contains("cannot store identity struct `example_lib::Actor`")
        });
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
    // Silently dropping the file would report a clean project.
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
