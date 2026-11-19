use crate::support::{invoke, repo_root, rock, success, text, write_files};

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

/// A package's `rock test` program reflects a `nobox` global as its logical
/// value through the global directory, like a game build does.
const NOBOX_MAIN: &str = r#"
nobox value Probe { x: int, y: int }
global SPOT: Probe = Probe { x: 3, y: 4 }
global counter: int = 0
pub fn bump() -> int {
    counter = counter + 1
    return counter
}
fn main() { print(bump()) }
"#;

const NOBOX_TEST: &str = r#"
inline(lua) fn logical_probe_x() -> int = """
local entry = assert(__rock_global_entry("SPOT"))
return __rock_global_get(entry).x
"""

inline(lua) fn logical_probe_set_x(value: int) = """
local entry = assert(__rock_global_entry("SPOT"))
local probe = __rock_global_get(entry)
probe.x = value
__rock_global_set(entry, probe)
"""

@test fn package_test_program_reflects_the_logical_nobox_value() {
    assert(bump() == 1, "fixture state")
    assert(logical_probe_x() == SPOT.x, "package tests expose the logical nobox value")
    let next = SPOT.x + SPOT.y
    logical_probe_set_x(next)
    assert(SPOT.x == next, "package tests write the logical value through its compiler adapter")
}
"#;

#[test]
fn package_test_program_reflects_nobox_globals_by_logical_value() {
    let dir = tempfile::tempdir().expect("fixture package");
    let project = dir.path().join("project");
    write_files(
        &project,
        &[
            (
                "stick.toml",
                "[package]\nname = \"test-program-shape\"\ntype = \"bin\"\n\
                 [language]\nmodules = \"v1\"\n",
            ),
            ("main.rock", NOBOX_MAIN),
            ("tests/shape.rock", NOBOX_TEST),
        ],
    );

    let test = rock(&repo_root())
        .arg("test")
        .arg(project.join("tests"))
        .env("ROCK_TEST_VERBOSE", "1")
        .output()
        .expect("run fixture package tests");
    let text = text(&test);
    assert!(test.status.success(), "package test failed:\n{text}");
    assert!(
        text.contains("1/1 @tests passed"),
        "expected one passing test:\n{text}"
    );
}
