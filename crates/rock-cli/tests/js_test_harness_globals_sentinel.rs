//! The `--backend js` test executor emits ONE program for a file, so a global
//! derived from shared mutable state is computed once -- the same model luajit
//! and `rock run --backend js` use.
//!
//! This was a sentinel: the executor used to emit each `@test` in isolation and
//! CONCATENATE, giving every block its own copy of the global initializers, so
//! the counter below restarted and `g2` came out 1 instead of 2. Per-test
//! emission is still what CLASSIFIES a test as supported (so an unsupported one
//! skips alone rather than taking the file down), but assembly is now always the
//! whole program.
//!
//! All three arms assert the SAME answer across the two backends and the two JS
//! entry points, so a regression that made any one of them disagree fails here
//! -- including one that "fixed" luajit to match a broken JS.

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

/// Both globals see one run of the initializers, so the second reads `2`.
#[test]
fn js_executor_keeps_shared_global_state() {
    let (ok, text) = rock("test", FIXTURE, &["--backend", "js"]);
    assert!(
        ok,
        "the JS test executor must compute a shared-state global ONCE per file; \
         `second_global` reading 1 is the per-test-block restart this file \
         exists to catch:\n{text}"
    );
    assert!(
        !text.contains("got <1>"),
        "`second_global` read a restarted counter:\n{text}"
    );
    // Denominator: both tests must actually have RUN. A file whose tests all
    // skipped would satisfy the assertions above while measuring nothing.
    assert!(
        text.contains("2/2"),
        "both tests must run, not skip -- got:\n{text}"
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
