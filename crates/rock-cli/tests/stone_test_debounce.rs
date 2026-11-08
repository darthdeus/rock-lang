//! Debounce and recovery coverage through the real headless watcher.

use crate::stone_harness::StoneHarness;
use std::time::Duration;

const BASIC_PROGRAM: &str = "fn main() { let x = 1 }\n";
const QUIESCENT: Duration = Duration::from_millis(400);
/// Backstop for one settled wait. The wait itself is completion-led (it ends
/// on the event or on child death); this only catches hangs and lost events.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(180);

/// Write `content` to `rel` as one atomic rename so the watcher observes a
/// single complete change.
///
/// A direct write of a new file emits CREATE then MODIFY kernel events; under
/// load those can straddle the watcher's 5ms debounce window and surface as
/// two reloads (an empty-file compile plus the content compile) for one
/// logical write. Staging outside the watched extensions and renaming keeps
/// the "exactly one reload" property deterministic.
fn write_file_atomic(h: &StoneHarness, rel: &str, content: &str) {
    let dest = h.root().join(rel);
    let staging = dest.with_extension("staging");
    std::fs::write(&staging, content).expect("staging write");
    std::fs::rename(&staging, &dest).expect("atomic rename");
}

#[test]
fn smoke_initial_write_triggers_one_reload() {
    let h = StoneHarness::spawn(&[]);
    let baseline = h.event_count();
    write_file_atomic(&h, "a.rock", BASIC_PROGRAM);
    let log = h.await_settled_after(baseline, QUIESCENT, SETTLE_TIMEOUT);
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
    // We assert the *settled* count is much smaller than the write count --
    // exact matching is host-dependent.
    let h = StoneHarness::spawn(&[]);
    let baseline = h.event_count();
    for i in 0..10 {
        h.write_file("a.rock", &format!("fn main() {{ let x = {} }}\n", i));
    }
    let log = h.await_settled_after(baseline, QUIESCENT, SETTLE_TIMEOUT);
    assert!(
        log.len() <= 3,
        "10 rapid writes should coalesce to <=3 reloads, got {}: {:?}",
        log.len(),
        log,
    );
    assert!(!log.is_empty(), "burst should produce at least one reload");
}

#[test]
fn separated_writes_each_reload() {
    // Writes spaced wider than the debounce window get distinct reloads --
    // the inverse of `rapid_burst_coalesces`. We assert `>= 3` rather than
    // `== 3` because the underlying FS layer (FSEvents on macOS, inotify
    // on Linux) is allowed to emit multiple kernel events for one
    // `std::fs::write`, and under workspace-wide nextest load they can
    // straddle the 5ms debounce window. The meaningful property is
    // "each separated write fires at least once"; over-firing is a
    // host artifact, under-firing would be a real bug.
    let h = StoneHarness::spawn(&[]);
    let baseline = h.event_count();
    h.write_file("a.rock", "fn main() { let x = 1 }\n");
    h.await_settled_after(baseline, QUIESCENT, SETTLE_TIMEOUT);

    let baseline = h.event_count();
    h.write_file("a.rock", "fn main() { let x = 2 }\n");
    h.await_settled_after(baseline, QUIESCENT, SETTLE_TIMEOUT);

    let baseline = h.event_count();
    h.write_file("a.rock", "fn main() { let x = 3 }\n");
    let log = h.await_settled_after(baseline, QUIESCENT, SETTLE_TIMEOUT);
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
    let baseline = h.event_count();
    h.write_file("a.rock", "fn main() { this is not valid rock");
    let _ = h.await_settled_after(baseline, QUIESCENT, SETTLE_TIMEOUT);
    let errors = h.error_log();
    assert!(
        !errors.is_empty(),
        "bad source should produce a compile_error event, got: reloads={:?} errors={:?}",
        h.reload_log(),
        errors,
    );
}
