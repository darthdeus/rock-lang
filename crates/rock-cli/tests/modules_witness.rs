//! modules-v1 witness (plan M6): three packages driven through the real `rock`
//! binary and real manifests, plus the two CLI defects that building it
//! exposed. `modules_emit.rs` is the M4 sibling; this file is the M6 one.

use std::path::{Path, PathBuf};
use std::process::Command;

fn rock_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_rock"))
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rock_modules_witness_{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, body).unwrap();
}

fn rock(args: &[&str]) -> (String, String, bool) {
    let out = Command::new(rock_bin()).args(args).output().expect("spawn rock");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

const V1_TESTED_PKG: &str = r#"
[package]
name = "v1tested"
type = "bin"

[language]
modules = "v1"

[build]
channels = ["dev"]
default_channel = "dev"
"#;

/// A v1 `@test` fn's DefKey is `v1tested::doubles` (root) or
/// `v1tested::math::scales` (a child module). `@test` is mangling-EXEMPT, so
/// the fn the artifact defines is the DECLARED segment -- the key itself is not
/// a Lua identifier. Before this was split, the harness wrote the KEY into the
/// `f = <global>` slot and luajit refused the chunk with
/// `'}' expected near '::'`, which the runner reported as "invalid lua emitted
/// (skull bug)" on every v1 package that owns a test.
#[test]
fn a_v1_packages_tests_run_under_the_cli() {
    let dir = scratch("v1_tests");
    write(&dir.join("stick.toml"), V1_TESTED_PKG);
    write(
        &dir.join("main.rock"),
        r#"
pub fn twice(n: int) -> int {
    n * 2
}

fn main() {
    print("ok")
}

@test
fn doubles() {
    assert(twice(21) == 42)
}
"#,
    );
    write(
        &dir.join("math.rock"),
        r#"
mod math;

pub fn scale(n: int) -> int {
    n * 3
}

@test
fn scales() {
    assert(scale(14) == 42)
}
"#,
    );
    let d = dir.to_string_lossy().into_owned();
    let (stdout, stderr, ok) = rock(&["test", &d]);
    assert!(ok, "rock test on a v1 package failed\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(
        stdout.contains("2/2 @tests passed"),
        "both v1 @tests must run\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
