//! A program that dies at runtime must make `rock run` exit non-zero.
//!
//! It used to exit 0. `run_lua_code` collapsed the child's `ExitStatus` into an
//! in-band string marker (`"STDERR: "` prepended to the captured output), so
//! `rock run` received the crash as ORDINARY OUTPUT, printed it, and returned
//! `Ok(())`. Anything gating on exit status -- a justfile recipe, CI, a shell
//! `&&` chain -- read a dead program as a successful one, and CLAUDE.md had to
//! carry the warning "`rock run` exits 0 while printing a LuaJIT crash" as a
//! standing trap for humans to remember instead.
//!
//! The marker is gone: the runner returns stdout/stderr/status separately and
//! every caller reads the status.

use std::process::Command;

fn rock() -> Command {
    crate::command::rock_bin()
}

fn write_source(name: &str, body: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("rock_run_exit_status_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let path = dir.join(name);
    std::fs::write(&path, body).expect("write source");
    path
}

const DIES_AT_RUNTIME: &str = r#"
inline(lua) fn boom() = """error("deliberate runtime death")"""

fn main() {
    print("before")
    boom()
    print("after")
}
"#;

const RUNS_CLEAN: &str = r#"
fn main() {
    print("clean")
}
"#;

/// Also asserts the crash is still REPORTED. A fix that swallowed the message to
/// get the status right would trade one silent failure for another.
#[test]
fn a_runtime_error_makes_rock_run_exit_nonzero() {
    let path = write_source("dies.rock", DIES_AT_RUNTIME);
    let out = rock().args(["run"]).arg(&path).output().expect("spawn rock run");

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success(),
        "`rock run` exited 0 for a program that died at runtime:\n{combined}"
    );
    assert!(
        combined.contains("deliberate runtime death"),
        "the runtime error was not reported:\n{combined}"
    );
    assert!(
        combined.contains("before"),
        "output printed before the error was lost:\n{combined}"
    );
}

/// The control: a clean program still exits 0, so the fix rejects failures
/// rather than everything.
#[test]
fn a_clean_program_still_exits_zero() {
    let path = write_source("clean.rock", RUNS_CLEAN);
    let out = rock().args(["run"]).arg(&path).output().expect("spawn rock run");

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "clean program did not exit 0:\n{combined}");
    assert!(combined.contains("clean"), "output missing:\n{combined}");
}

/// A successful program that writes to stderr must NOT be treated as failed.
/// The old marker keyed on `!stderr.is_empty()` as well as the exit status, so
/// any warning on stderr made a passing run look like a crash.
#[test]
fn stderr_output_from_a_successful_program_is_not_a_failure() {
    let path = write_source(
        "noisy.rock",
        r#"
inline(lua) fn warn() = """io.stderr:write("a warning\n")"""

fn main() {
    warn()
    print("done")
}
"#,
    );
    let out = rock().args(["run"]).arg(&path).output().expect("spawn rock run");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.success(),
        "a program that merely wrote to stderr was treated as failed:\n{combined}"
    );
    assert!(combined.contains("done"), "stdout missing:\n{combined}");
}
