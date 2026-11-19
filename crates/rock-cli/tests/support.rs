//! Process helpers shared by the rock-cli integration tests.
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The checkout this crate was built from.
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

/// The built binary rooted at `root`, on one compiler thread (tests run side by
/// side), without the ambient settings that would change what it compiles or
/// where it looks.
pub fn rock(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rock"));
    command
        .current_dir(root)
        .env("ROCK_DIR", root)
        .env("ROCK_INVOCATION_LOG", "off")
        .env("ROCK_NUM_THREADS", "1");
    for key in [
        "ROCK_STDLIB_DIR",
        "ROCK_SKULL_LINE_DIRECTIVES",
        "ROCK_TEST_FILTER",
        "LUA_PATH",
        "LUA_CPATH",
    ] {
        command.env_remove(key);
    }
    command
}

/// `rock <args>` in `root`, waited for.
pub fn invoke(root: &Path, args: &[&str]) -> Output {
    rock(root).args(args).output().expect("start rock")
}

/// Stdout then stderr, without colour codes.
pub fn text(out: &Output) -> String {
    test_utils::strip_ansi_sgr(&format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    ))
}

/// Asserts the command succeeded and returns its stdout.
#[track_caller]
pub fn success(out: Output) -> String {
    assert!(out.status.success(), "{}", text(&out));
    String::from_utf8(out.stdout).unwrap()
}

/// Asserts the command failed and its stderr names `expected`.
#[track_caller]
pub fn failure(out: Output, expected: &str) {
    let stderr = test_utils::strip_ansi_sgr(&String::from_utf8_lossy(&out.stderr));
    assert!(!out.status.success(), "{}", text(&out));
    assert!(
        stderr.contains(expected),
        "expected {expected:?}: {}",
        text(&out)
    );
}

/// Writes `files` (paths relative to `root`), creating parent directories.
pub fn write_files(root: &Path, files: &[(&str, &str)]) {
    for (name, contents) in files {
        let path = root.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
}

/// `rock test <src as one file> [args]` from the checkout: (passed, text).
pub fn rock_test_source(src: &str, args: &[&str]) -> (bool, String) {
    let dir = tempfile::tempdir().expect("fixture dir");
    let file = dir.path().join("probe.rock");
    std::fs::write(&file, src).expect("write fixture");
    let out = rock(&repo_root())
        .arg("test")
        .arg(&file)
        .args(args)
        .env("ROCK_TEST_VERBOSE", "1")
        .output()
        .expect("start rock");
    (out.status.success(), text(&out))
}

/// The entry module of [`two_module_package`].
pub const PACKAGE_MAIN: &str = r#"
mod main;

use super::util::bump;

fn main() {
    print("{bump(41)}")
}
"#;

const PACKAGE_UTIL: &str = r#"
mod util;

pub fn bump(n: int) -> int {
    let step = 1
    return n + step
}

pub fn twice(n: int) -> int {
    return bump(bump(n))
}
"#;

/// A modules-v1 package named `name` whose `main` calls into a sibling module.
pub fn two_module_package(name: &str) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    write_files(
        root.path(),
        &[
            (
                "stick.toml",
                &format!("[package]\nname = \"{name}\"\n\n[language]\nmodules = \"v1\"\n"),
            ),
            ("main.rock", PACKAGE_MAIN),
            ("util.rock", PACKAGE_UTIL),
        ],
    );
    root
}

/// True when `node` runs. When it does not, panics unless
/// `ROCK_SKIP_NODE_TESTS` is set (CI has node, so a missing binary there must
/// not read as a pass), in which case it logs the skip and returns false.
#[cfg(feature = "backend-js")]
pub fn require_node() -> bool {
    const SKIP_NODE_ENV: &str = "ROCK_SKIP_NODE_TESTS";
    if Command::new("node")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        return true;
    }
    assert!(
        std::env::var_os(SKIP_NODE_ENV).is_some(),
        "`node` is not on PATH; install it or set {SKIP_NODE_ENV}=1 to skip the tests that run JS"
    );
    eprintln!("SKIP: `node` not on PATH ({SKIP_NODE_ENV} is set)");
    false
}
