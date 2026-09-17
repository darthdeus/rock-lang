//! modules-v1 emission (plan M4). A v1 definition's identity is its DefKey
//! (`<package>::<module path>::<name>`), which is not a Lua identifier: it must
//! never reach an identifier position in the emitted artifact. Before M4 the
//! mangler passed the key through, so `rock compile` on a v1 package exited 0
//! and wrote a file LuaJIT could not parse (`function v1emit::bump_...`).
//!
//! These run the program rather than grepping the artifact: a name that is
//! wrong in either the definition or the call site fails to execute.

use std::path::{Path, PathBuf};
use std::process::Command;

fn rock_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_rock"))
}

const STICK: &str = r#"
[package]
name = "v1emit"
type = "bin"

[language]
modules = "v1"

[build]
channels = ["dev"]
default_channel = "dev"
"#;

const MAIN: &str = r#"
pub struct Counter {
    pub n: int,
}

pub fn bump(c: Counter) -> Counter {
    Counter { n: c.n + 1 }
}

fn main() {
    let c = Counter { n: 1 }
    let d = bump(c)
    print("n={d.n}")
}
"#;

/// Two logical modules each own a `value` of the same signature. Their DefKeys
/// differ only in the module path, so the emitted names must differ too -- this
/// is what the mangling digest is for (plan 2.1).
const SAME_NAME_MAIN: &str = r#"
fn main() {
    print("a={a::value()} b={b::value()}")
}
"#;

const SAME_NAME_A: &str = r#"
mod a;

pub fn value() -> int {
    1
}
"#;

const SAME_NAME_B: &str = r#"
mod b;

pub fn value() -> int {
    2
}
"#;

/// `pkg::m7906` and `pkg::m56555` are a real collision of the 32-bit module
/// path digest (found by search over `DefaultHasher`, which is stable across
/// processes and builds). Two definitions would then share an emitted
/// identifier, so the compile must be refused rather than renamed (plan 2.1).
const COLLIDE_STICK: &str = r#"
[package]
name = "pkg"
type = "bin"

[language]
modules = "v1"

[build]
channels = ["dev"]
default_channel = "dev"
"#;

const COLLIDE_MAIN: &str = r#"
fn main() {
    print("hi")
}
"#;

const COLLIDE_A: &str = r#"
mod m7906;

pub fn value() -> int {
    1
}
"#;

const COLLIDE_B: &str = r#"
mod m56555;

pub fn other() -> int {
    2
}
"#;

fn project(tag: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rock_modules_emit_{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("stick.toml"), STICK).unwrap();
    for (name, body) in files {
        std::fs::write(dir.join(name), body).unwrap();
    }
    dir
}

fn run(dir: &Path) -> (String, String, bool) {
    let out = Command::new(rock_bin())
        .arg("run")
        .arg(dir)
        .output()
        .expect("spawn rock");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

#[test]
fn v1_package_emits_runnable_lua() {
    let dir = project("basic", &[("main.rock", MAIN)]);
    let (stdout, stderr, ok) = run(&dir);
    assert!(ok, "rock run failed\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(
        stdout.contains("n=2"),
        "expected the v1 program to run and print `n=2`\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn colliding_module_digests_are_rejected() {
    let dir = std::env::temp_dir().join("rock_modules_emit_collide");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("stick.toml"), COLLIDE_STICK).unwrap();
    std::fs::write(dir.join("main.rock"), COLLIDE_MAIN).unwrap();
    std::fs::write(dir.join("a.rock"), COLLIDE_A).unwrap();
    std::fs::write(dir.join("b.rock"), COLLIDE_B).unwrap();
    let out = Command::new(rock_bin())
        .arg("compile")
        .arg(&dir)
        .arg("--output")
        .arg(dir.join("out.lua"))
        .output()
        .expect("spawn rock");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "colliding module digests must refuse the compile
stderr:
{stderr}"
    );
    assert!(
        stderr.contains("module path digest") && stderr.contains("pkg::m7906"),
        "the rejection must name the colliding prefixes
stderr:
{stderr}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn v1_same_name_in_two_modules_emits_distinct_symbols() {
    let dir = project(
        "same_name",
        &[
            ("main.rock", SAME_NAME_MAIN),
            ("a.rock", SAME_NAME_A),
            ("b.rock", SAME_NAME_B),
        ],
    );
    let (stdout, stderr, ok) = run(&dir);
    assert!(ok, "rock run failed\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(
        stdout.contains("a=1 b=2"),
        "same-named fns in two modules must emit distinct symbols\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
