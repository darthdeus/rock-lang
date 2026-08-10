//! Phase A integration tests for `rock stone-test`.
//!
//! These spawn the real `rock` binary and exercise the watcher +
//! `unified_compile` round-trip end-to-end, so they're slow relative to
//! the in-process tests in `hot_reload_runtime_test.rs`. They run on the
//! default nextest profile today (the gate to `-P full` happens once we
//! also have Phase B/C runtime tests piling up here).

use std::time::Duration;
use crate::stone_harness::StoneHarness;

const BASIC_PROGRAM: &str = "fn main() { let x = 1 }\n";
const QUIESCENT: Duration = Duration::from_millis(400);
const SETTLE_TIMEOUT: Duration = Duration::from_secs(10);

#[test]
fn smoke_initial_write_triggers_one_reload() {
    let h = StoneHarness::spawn(&[]);
    h.write_file("a.rock", BASIC_PROGRAM);
    let log = h.await_write_settled(QUIESCENT, SETTLE_TIMEOUT);
    assert_eq!(
        log.len(),
        1,
        "single write should produce exactly one reload event, got {}: {:?}",
        log.len(),
        log,
    );
}

#[test]
fn rapid_burst_coalesces() {
    // 2j: 10 writes within ~1ms each, should coalesce into a single reload
    // (or at most a small handful, depending on the watcher's debounce window).
    // We assert the *settled* count is much smaller than the write count —
    // exact matching is host-dependent.
    let h = StoneHarness::spawn(&[]);
    for i in 0..10 {
        h.write_file("a.rock", &format!("fn main() {{ let x = {} }}\n", i));
    }
    let log = h.await_write_settled(QUIESCENT, SETTLE_TIMEOUT);
    assert!(
        log.len() <= 3,
        "10 rapid writes should coalesce to ≤3 reloads, got {}: {:?}",
        log.len(),
        log,
    );
    assert!(!log.is_empty(), "burst should produce at least one reload");
}

#[test]
fn separated_writes_each_reload() {
    // Writes spaced wider than the debounce window get distinct reloads —
    // the inverse of `rapid_burst_coalesces`. We assert `>= 3` rather than
    // `== 3` because the underlying FS layer (FSEvents on macOS, inotify
    // on Linux) is allowed to emit multiple kernel events for one
    // `std::fs::write`, and under workspace-wide nextest load they can
    // straddle the 5ms debounce window. The meaningful property is
    // "each separated write fires at least once"; over-firing is a
    // host artifact, under-firing would be a real bug.
    let h = StoneHarness::spawn(&[]);
    h.write_file("a.rock", "fn main() { let x = 1 }\n");
    h.await_write_settled(QUIESCENT, SETTLE_TIMEOUT);

    h.write_file("a.rock", "fn main() { let x = 2 }\n");
    h.await_write_settled(QUIESCENT, SETTLE_TIMEOUT);

    h.write_file("a.rock", "fn main() { let x = 3 }\n");
    let log = h.await_write_settled(QUIESCENT, SETTLE_TIMEOUT);
    assert!(
        log.len() >= 3,
        "three separated writes should produce at least three reloads, got {}: {:?}",
        log.len(),
        log,
    );
}

#[test]
fn compile_error_event_emitted_for_bad_source() {
    let h = StoneHarness::spawn(&[]);
    h.write_file("a.rock", "fn main() { this is not valid rock");
    // Wait for the watcher to settle.
    let _ = h.await_write_settled(QUIESCENT, SETTLE_TIMEOUT);
    let errors = h.error_log();
    assert!(
        !errors.is_empty(),
        "bad source should produce a compile_error event, got: reloads={:?} errors={:?}",
        h.reload_log(),
        errors,
    );
}
