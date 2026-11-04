//! A package's `rock test` program reflects a `nobox` global as its logical
//! value through the global directory, like a game build does.
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent().unwrap().parent().unwrap().canonicalize().unwrap()
}

const MAIN: &str = r#"
nobox value Probe { x: int, y: int }
global SPOT: Probe = Probe { x: 3, y: 4 }
global counter: int = 0
pub fn bump() -> int {
    counter = counter + 1
    return counter
}
fn main() { print(bump()) }
"#;

const TEST: &str = r#"
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
    std::fs::create_dir_all(project.join("tests")).unwrap();
    std::fs::write(project.join("stick.toml"),
        "[package]\nname = \"test-program-shape\"\ntype = \"bin\"\n\
         [language]\nmodules = \"v1\"\n").unwrap();
    std::fs::write(project.join("main.rock"), MAIN).unwrap();
    std::fs::write(project.join("tests/shape.rock"), TEST).unwrap();

    let test = std::process::Command::new(env!("CARGO_BIN_EXE_rock"))
        .arg("test").arg(project.join("tests"))
        .current_dir(root())
        .env("ROCK_TEST_VERBOSE", "1")
        .env_remove("ROCK_TEST_FILTER")
        .output().expect("run fixture package tests");
    let text = test_utils::strip_ansi_sgr(&format!(
        "{}{}",
        String::from_utf8_lossy(&test.stdout),
        String::from_utf8_lossy(&test.stderr)
    ));
    assert!(test.status.success(), "package test failed:\n{text}");
    assert!(text.contains("1/1 @tests passed"), "expected one passing test:\n{text}");
}
