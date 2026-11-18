use crate::support::{invoke, rock, success, text, write_files};

fn project(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    write_files(
        dir.path(),
        &[(
            "stick.toml",
            "[package]\nname = \"sim-selection\"\ntype = \"lib\"\n",
        )],
    );
    write_files(dir.path(), files);
    dir
}

#[test]
fn package_tests_keep_reexports_and_their_transitive_helpers() {
    let dir = project(&[
        (
            "tests/check.rock",
            "@test fn check() { assert(api::answer() == 42) }",
        ),
        (
            "tests/sims/api.rock",
            "mod api; pub use ::helpers::value as answer; fn unrelated() {}",
        ),
        (
            "tests/sims/helpers.rock",
            "mod helpers; pub fn value() -> int { 42 }",
        ),
        (
            "tests/sims/unused.rock",
            "fn unused() { missing_sim_name() }",
        ),
    ]);
    let stdout = success(invoke(dir.path(), &["test", "."]));
    assert!(stdout.contains("1/1 @tests passed"), "{stdout}");
}

#[test]
fn package_tests_report_syntax_errors_in_unreferenced_sim_files() {
    let dir = project(&[
        ("tests/check.rock", "@test fn check() {}"),
        ("tests/sims/unused.rock", "fn unused() { let = 1; }"),
    ]);
    let rejected = rock(dir.path())
        .args(["test", "."])
        .env("ROCK_TEST_VERBOSE", "1")
        .output()
        .unwrap();
    let report = text(&rejected);
    assert!(!rejected.status.success(), "{report}");
    assert!(report.contains("unused.rock:1:"), "{report}");
    assert!(report.contains("unsupported pattern: Assign"), "{report}");
}
