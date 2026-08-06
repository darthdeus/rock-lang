//! SENTINEL for a `--backend js` TEST-EXECUTOR bug (not a language one).
//!
//! With no decorator-dependent test in a file, `js_test.rs` emits each `@test`
//! in isolation and CONCATENATES the results. Every block carries its own copy
//! of the global initializers, so at load they run once per test block. A global
//! whose value derives from SHARED MUTABLE state therefore restarts: the counter
//! below is reset by the second block before `g2` is computed, and `g2` comes
//! out 1 instead of 2.
//!
//! This is the emission model, not a backend semantics divergence: luajit runs
//! the file as one program and gets 1 and 2, and `rock run --backend js` -- which
//! also emits one program -- gets 1 and 2 as well. Both arms are asserted below,
//! so a fix that made luajit agree with the buggy JS answer would fail here too.
//!
//! Measured on the pristine tree at `84edfd16b`, so it predates the Session
//! Arena registry; the registry is simply the first stdlib state to depend on
//! cross-global sharing. It is why four arena iteration tests fail under
//! `rock test tests/ --backend js` (`docs/known-failing-tests.md`).
//!
//! PINNED BUG: the JS executor should report `2`. When the harness emits globals
//! once, `js_executor_restarts_shared_global_state` goes red -- flip its
//! expectation to a pass and update this comment in the same commit.

use std::path::Path;
use std::process::Command;

/// Two globals derived from one shared counter. Nothing here is Arena-specific;
/// the counter is the whole mechanism.
const FIXTURE: &str = r#"
global SHARED_COUNTER: int = 0

fn sentinel_next_id() -> int {
    SHARED_COUNTER = SHARED_COUNTER + 1
    return SHARED_COUNTER
}

global sentinel_g1: int = sentinel_next_id()
global sentinel_g2: int = sentinel_next_id()

@test(output = "1")
fn first_global() { print(sentinel_g1) }

@test(output = "2")
fn second_global() { print(sentinel_g2) }
"#;

const RUN_FIXTURE: &str = r#"
global SHARED_COUNTER: int = 0

fn sentinel_next_id() -> int {
    SHARED_COUNTER = SHARED_COUNTER + 1
    return SHARED_COUNTER
}

global sentinel_g1: int = sentinel_next_id()
global sentinel_g2: int = sentinel_next_id()

fn main() {
    print(sentinel_g1)
    print(sentinel_g2)
}
"#;

fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("workspace root")
}

/// Writes `src` to a temp `.rock` and runs `rock <cmd> <file> [args]`.
/// The fixture is never left in `tests/`, which `rock test tests/` would scan.
fn rock(cmd: &str, src: &str, args: &[&str]) -> (bool, String) {
    let dir = tempfile::tempdir().expect("fixture dir");
    let file = dir.path().join("sentinel.rock");
    std::fs::write(&file, src).expect("write fixture");
    let out = Command::new(env!("CARGO_BIN_EXE_rock"))
        .arg(cmd)
        .arg(&file)
        .args(args)
        .current_dir(workspace_root())
        .output()
        .expect("spawn rock");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

/// PINNED BUG: the second test reads a restarted counter, so `second_global`
/// fails with `1`. Flip to an all-pass assertion when the harness is fixed.
#[test]
fn js_executor_restarts_shared_global_state() {
    let (ok, text) = rock("test", FIXTURE, &["--backend", "js"]);
    assert!(
        !ok,
        "PINNED BEHAVIOR (BUG): the JS test executor is expected to fail this \
         fixture because it re-runs global initializers per test block. It \
         passed -- the harness fix landed; flip this assertion.\n{text}"
    );
    assert!(
        text.contains("second_global") && text.contains("got <1>"),
        "the failure must be the shared-counter restart (`second_global` got 1), \
         not some other error -- otherwise this sentinel is pinning the wrong \
         thing:\n{text}"
    );
    assert!(
        !text.contains("FAIL first_global"),
        "the first block's initializers ran last before it, so `first_global` is \
         the control and must still pass:\n{text}"
    );
}

/// The control that makes the sentinel meaningful: luajit runs the file as one
/// program and gets the right answers.
#[test]
fn luajit_executor_keeps_shared_global_state() {
    let (ok, text) = rock("test", FIXTURE, &[]);
    assert!(ok, "luajit must evaluate the shared counter once:\n{text}");
}

/// The other control: emitting ONE JS program -- what `rock run --backend js`
/// and `rock web` do -- gives the same answers as luajit. This is what makes the
/// bug a test-executor property rather than a JS-semantics divergence.
#[test]
fn single_js_program_keeps_shared_global_state() {
    let (ok, text) = rock("run", RUN_FIXTURE, &["--backend", "js"]);
    assert!(ok, "`rock run --backend js` failed:\n{text}");
    let body: String = text.lines().filter(|l| !l.contains("]")).collect::<Vec<_>>().join("\n");
    assert!(
        body.contains('1') && body.contains('2'),
        "one JS program must print 1 and 2 like luajit:\n{text}"
    );
}
