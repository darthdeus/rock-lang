//! DEFER ON ERROR end to end: `rock test` runs a package's tests in one Lua
//! state, so a fixture whose `defer` restores state must restore it when an
//! `assert` fails inside the fixture, or the next test sees the leaked state.
use std::path::{Path, PathBuf};
use std::process::Command;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent().unwrap().parent().unwrap().canonicalize().unwrap()
}

#[test]
fn failed_assert_in_fixture_runs_its_defer_before_the_next_test() {
    let dir = tempfile::tempdir().expect("defer fixture package");
    let project = dir.path().join("project");
    std::fs::create_dir_all(project.join("tests")).unwrap();
    std::fs::write(project.join("stick.toml"),
        "[package]\nname = \"test-defer-on-error\"\ntype = \"lib\"\n\
         [language]\nmodules = \"v1\"\n").unwrap();
    std::fs::write(project.join("lib.rock"), r#"
pub inline(lua) fn fixture_mark() -> int = "return _G.__defer_fixture_mark or 0"
inline(lua) fn set_fixture_mark(v: int) = "_G.__defer_fixture_mark = v"

pub fn with_fixture(check: fn() -> ()) {
    set_fixture_mark(1)
    defer { set_fixture_mark(0) }
    check()
}
"#).unwrap();
    std::fs::write(project.join("tests/fixture_rules.rock"), r#"
@test
fn a_fixture_check_fails() {
    with_fixture(() => {
        assert(fixture_mark() == 1, "the fixture is installed inside the check")
        assert(false, "deliberate failure inside the fixture")
    })
}

@test
fn b_next_test_sees_the_fixture_restored() {
    assert_eq(fixture_mark(), 0)
}
"#).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_rock"))
        .arg("test").arg(project.join("tests"))
        .current_dir(root())
        .env("ROCK_TEST_VERBOSE", "1")
        .output().expect("run defer fixture package test");
    let text = format!("{}{}", String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr));
    assert!(text.contains("deliberate failure inside the fixture"),
        "the failing check must report its own assert:\n{text}");
    assert!(text.contains("1/2 @tests passed"),
        "only the deliberately failing test may fail:\n{text}");
    assert!(!text.contains("b_next_test_sees_the_fixture_restored:"),
        "the fixture's defer must have restored the mark:\n{text}");
}
