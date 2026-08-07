//! The Stage-0e ablation COLLECTS rather than aborting
//! (`docs/monomorphization-plan.md` S 0e, `skull_mir::ablation`), so its census
//! is evidence about a whole run and must print however the build ended.
//!
//! `std::process::exit` runs no destructors and returns to no caller, so a path
//! that calls it skips the drain in `main`. The paths that call it are the ones
//! a build under the mode DIES on, which inverted the mode: a project that
//! happened to compile reported a full taxonomy, and a project that failed
//! reported nothing at all. The `Iterator`-bounded surface is entirely in the
//! second group, so its category read zero on every project in the corpus while
//! the fatal was being recorded correctly the whole time.
//!
//! Drives the real CLI, because the defect is in how the process exits and
//! nothing below that layer can observe it.

use std::process::Command;

fn rock_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_rock"))
}

fn write_tmp(tag: &str, src: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("rock_ablation_census_{tag}"));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("a.rock");
    std::fs::write(&f, src).unwrap();
    f
}

/// Compile under the mode and return (exit_ok, combined output).
fn compile_ablated(path: &std::path::Path) -> (bool, String) {
    let out = Command::new(rock_bin())
        .args(["compile", path.to_str().unwrap(), "--output", "/dev/null"])
        .env("ROCK_MONO_ABLATION", "1")
        .output()
        .unwrap();
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), s)
}

/// A bounded call has no standalone emission, so with inlining disabled the
/// build FAILS -- and the census must still print, naming the mechanism.
///
/// This fixture is the whole point: it is the shape whose build dies. A fixture
/// that compiled would pass against the defect, because the drain in `main` runs
/// on that path.
const BOUNDED: &str = r#"
fn total<I: Iterator<int>>(it: I) -> int {
    let s = 0
    for v in it { s = s + v }
    return s
}

fn main() {
    let xs: [int] = [1, 2, 3]
    print("{total(xs.iter())}")
}
"#;

/// A generic call that specializes cleanly: the build SUCCEEDS under the mode.
/// The admission control -- it says the census prints on the surviving path too,
/// so a failure above is about the exit route and not about the mode being off.
const CLEAN: &str = r#"
fn ident<T>(x: T) -> T { return x }

fn main() {
    let v = Vec2 { x: 1, y: 2 }
    let a = ident(v)
    print("{a.x}")
}
"#;

#[test]
fn a_failed_build_still_prints_its_census() {
    let path = write_tmp("bounded", BOUNDED);
    let (ok, out) = compile_ablated(&path);
    assert!(
        !ok,
        "the fixture must FAIL to build under the mode, or it cannot witness the \
         exiting path; output:\n{out}"
    );
    assert!(
        out.contains("Stage-0e totality ablation"),
        "a build that died printed no census, so every fatal it recorded is lost; \
         output:\n{out}"
    );
    assert!(
        out.contains("bounded body has no standalone emission"),
        "the census printed but does not name the `Iterator`-bounded mechanism the \
         run recorded; output:\n{out}"
    );
}

#[test]
fn a_surviving_build_prints_its_census() {
    let path = write_tmp("clean", CLEAN);
    let (_, out) = compile_ablated(&path);
    assert!(
        out.contains("Stage-0e totality ablation"),
        "the mode printed no census on a build that completed; output:\n{out}"
    );
    assert!(
        !out.contains("bounded body has no standalone emission"),
        "a fixture with no bounded call reported the bounded mechanism, so the \
         assertion above cannot distinguish; output:\n{out}"
    );
}
