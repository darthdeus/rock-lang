//! A `@test` run that stops early must not read as a complete green run.
//!
//! The harness `pcall`s each test, so an ordinary assertion failure becomes a
//! FAIL marker and the process still exits 0. That made a non-zero status look
//! redundant, and it was dropped. It is not redundant: it is the only signal
//! left when a test kills the process outright (`os.exit`, a C-level abort, an
//! OOM), because then the remaining tests never run and never emit a marker.
//!
//! The denominator is the sharper half. Counting only the markers that arrived
//! makes a truncated run self-consistent -- three declared tests, one marker,
//! reported `1/1 passed` -- so the tests that never ran vanish from the total
//! instead of failing it. A premature `os.exit(0)` is the same shape with no
//! status to catch it.

use std::process::Command;

fn rock() -> Command {
    Command::new(env!("CARGO_BIN_EXE_rock"))
}

fn write_source(name: &str, body: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("scratch dir");
    let path = dir.path().join(name);
    std::fs::write(&path, body).expect("write source");
    (dir, path)
}

fn run_test(path: &std::path::Path) -> (bool, String) {
    let out = rock().args(["test"]).arg(path).output().expect("spawn rock test");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), combined)
}

fn declared_test_count(source: &str) -> usize {
    source
        .lines()
        .filter(|line| line.trim_start().starts_with("@test fn "))
        .count()
}

fn reported_test_total(output: &str) -> usize {
    let summary = output
        .lines()
        .find(|line| line.contains("skull backend:"))
        .unwrap_or_else(|| panic!("missing skull test summary:\n{output}"));
    let (_, counts) = summary.split_once("skull backend:").expect("summary marker");
    let (_, denominator) = counts.split_once('/').expect("passed/total summary");
    let digits = denominator
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>();
    digits.parse().unwrap_or_else(|_| panic!("invalid test total in summary:\n{summary}"))
}

/// `b` kills the process, so `c` -- which would FAIL -- never runs.
const HARD_EXIT_MIDWAY: &str = r#"
inline(lua) fn die() = """ os.exit(7) """

@test fn a_runs_first() { assert(true, "ok") }
@test fn b_exits_hard() { die() }
@test fn c_never_runs() { assert(false, "this test must be counted as not-passed") }
"#;

/// The same truncation with a SUCCESS status, so nothing but the marker count
/// can catch it.
const CLEAN_EXIT_MIDWAY: &str = r#"
inline(lua) fn quit() = """ os.exit(0) """

@test fn a_runs_first() { assert(true, "ok") }
@test fn b_exits_clean() { quit() }
@test fn c_never_runs() { assert(false, "this test must be counted as not-passed") }
"#;

const ALL_PASS: &str = r#"
@test fn a() { assert(true, "ok") }
@test fn b() { assert(true, "ok") }
"#;

const ONE_ORDINARY_FAILURE: &str = r#"
@test fn a() { assert(true, "ok") }
@test fn b() { assert(false, "deliberate") }
"#;

#[test]
fn a_test_that_kills_the_process_fails_the_run() {
    let (_dir, path) = write_source("hard_exit.rock", HARD_EXIT_MIDWAY);
    let (success, combined) = run_test(&path);
    assert!(
        !success,
        "`rock test` exited 0 for a run that died midway; two of three tests \
         never ran and one of those would have failed:\n{combined}"
    );
    assert_eq!(
        reported_test_total(&combined),
        declared_test_count(HARD_EXIT_MIDWAY),
        "the summary total must include declared tests that never reported:\n{combined}"
    );
}

/// The status is `success` here, so this case is carried entirely by comparing
/// the marker count against the number of declared tests.
#[test]
fn a_premature_clean_exit_fails_the_run() {
    let (_dir, path) = write_source("clean_exit.rock", CLEAN_EXIT_MIDWAY);
    let (success, combined) = run_test(&path);
    assert!(
        !success,
        "`rock test` exited 0 for a run truncated by `os.exit(0)`; the status \
         cannot see this one, only the missing markers can:\n{combined}"
    );
    assert_eq!(
        reported_test_total(&combined),
        declared_test_count(CLEAN_EXIT_MIDWAY),
        "the summary total must include tests omitted by an early clean exit:\n{combined}"
    );
}

/// The control. A fix that failed every run would satisfy both tests above.
#[test]
fn an_ordinary_run_still_passes() {
    let (_dir, path) = write_source("all_pass.rock", ALL_PASS);
    let (success, combined) = run_test(&path);
    assert!(success, "a clean two-test file must still pass:\n{combined}");
    assert!(
        combined.contains("2/2"),
        "expected both tests counted:\n{combined}"
    );
}

/// The other control: an ordinary assertion failure must still be reported as a
/// test failure rather than a harness error, and must still fail the run.
#[test]
fn an_ordinary_assertion_failure_is_still_a_test_failure() {
    let (_dir, path) = write_source("one_fail.rock", ONE_ORDINARY_FAILURE);
    let (success, combined) = run_test(&path);
    assert!(!success, "a failing @test must fail the run:\n{combined}");
    assert!(
        combined.contains("1/2") || combined.contains("(1 failed)"),
        "expected a per-test failure tally, not a harness error:\n{combined}"
    );
}
