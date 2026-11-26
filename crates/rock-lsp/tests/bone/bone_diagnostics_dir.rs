//! `rock lsp diagnostics <dir>` and `rock lsp lint <dir>` over every `.rock`
//! file under a directory.

use rock_lsp::server_cli::{diagnostics_report, lint_report, print_report};

#[test]
fn a_hard_parse_error_does_not_rebuild_every_project_prefix() {
    use std::cell::RefCell;

    thread_local! {
        static BUILDS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    }

    struct CountBuilds;
    impl log::Log for CountBuilds {
        fn enabled(&self, meta: &log::Metadata<'_>) -> bool {
            meta.target() == "bone_incremental::session"
        }

        fn log(&self, record: &log::Record<'_>) {
            if self.enabled(record.metadata()) {
                BUILDS.with_borrow_mut(|builds| builds.push(record.args().to_string()));
            }
        }

        fn flush(&self) {}
    }

    log::set_logger(&CountBuilds).unwrap();
    log::set_max_level(log::LevelFilter::Debug);

    let dir = tempfile::tempdir().unwrap();
    let write = |name, source| std::fs::write(dir.path().join(name), source).unwrap();
    write(
        "stick.toml",
        "[package]\nname = \"report\"\ntype = \"lib\"\n",
    );
    for i in 0..16 {
        std::fs::write(
            dir.path().join(format!("a{i:02}.rock")),
            format!("fn helper_{i}() -> int {{ {i} }}\n"),
        )
        .unwrap();
    }
    write("b.rock", "fn broken() { let text = \"unterminated");
    write("c.rock", "fn caller() { helper_15(); missing() }\n");

    let report = diagnostics_report(dir.path().to_str().unwrap()).unwrap();
    let files = report["files"].as_array().unwrap();
    assert_eq!(files.len(), 2, "{report}");
    for (file, (name, message)) in files
        .iter()
        .zip([("b.rock", "string"), ("c.rock", "missing")])
    {
        assert!(file["file"].as_str().unwrap().ends_with(name), "{file}");
        assert_eq!(file["diagnostics"].as_array().unwrap().len(), 1, "{file}");
        assert!(
            file["diagnostics"][0]["message"]
                .as_str()
                .unwrap()
                .contains(message),
            "{file}"
        );
    }
    let take_builds = || BUILDS.with_borrow_mut(std::mem::take);
    assert_eq!(take_builds(), ["session build: 17 user modules"]);

    let a = dir.path().join("a15.rock");
    let b = dir.path().join("b.rock");
    let mut workspace = bone_query::BoneWorkspace::new();
    workspace.load_project(a.to_str().unwrap());
    assert_eq!(take_builds(), ["session build: 17 user modules"]);
    workspace.update(a.to_str().unwrap(), "fn helper_15() -> int { 42 }\n".into());
    assert!(take_builds().is_empty(), "a body edit rebuilt the project");
    assert_eq!(workspace.diagnostics_for(b.to_str().unwrap()).len(), 1);
    workspace.update(b.to_str().unwrap(), "fn repaired() {}\n".into());
    assert_eq!(take_builds(), ["session build: 18 user modules"]);
    assert!(workspace.diagnostics_for(b.to_str().unwrap()).is_empty());
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
