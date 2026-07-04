//! Regression: a Rock project whose path contains a `tmp` component
//! (every `tempfile::tempdir()` on Linux is `/tmp/...`, and scratch
//! projects often live under `/tmp`) must still get hot-reload events.
//!
//! `is_tmp_scratch_file` is meant to skip the compiler's *project-relative*
//! `tmp/` output dir (`<project>/tmp/main.lua`, etc.) while still passing
//! `<project>/tmp/restart.txt`. It must NOT treat the system `/tmp` (or any
//! `tmp` component that is part of the project root itself) as scratch —
//! doing so silently drops every source change and kills hot reload with
//! no error or log. Platform-independent: the project root is forced under
//! a `tmp/` component explicitly rather than relying on `$TMPDIR`.

use rock_cli::FileWatcher;
use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

// Quiescence-based: wait up to HARD_DEADLINE for the first event, then
// keep collecting until QUIESCENCE elapses with no new arrivals. Old
// version unconditionally slept the full 3s ceiling on every call,
// adding ~3s per test for no coverage gain (the assertions only care
// about whether specific events appeared or didn't appear, not about
// a fixed-window absence).
const HARD_DEADLINE: Duration = Duration::from_secs(3);
const QUIESCENCE: Duration = Duration::from_millis(500);

fn drain(rx: &std::sync::mpsc::Receiver<rock_cli::FileChangeEvent>) -> Vec<rock_cli::FileChangeEvent> {
    let mut got = Vec::new();
    let hard = Instant::now() + HARD_DEADLINE;

    // Block up to the hard deadline waiting for the first event. If
    // nothing arrives, return empty -- assertions handle the "expected
    // but missing" failure mode.
    let first_wait = hard.saturating_duration_since(Instant::now());
    match rx.recv_timeout(first_wait) {
        Ok(ev) => got.push(ev),
        Err(_) => return got,
    }

    // Drain follow-ons with a quiescence window. 500ms is well above
    // the 5ms file_watcher debounce, so a correctly-suppressed event
    // can't sneak in after we return. The hard deadline still bounds
    // pathological event storms.
    loop {
        let now = Instant::now();
        if now >= hard {
            break;
        }
        let remaining = hard.saturating_duration_since(now).min(QUIESCENCE);
        match rx.recv_timeout(remaining) {
            Ok(ev) => got.push(ev),
            Err(_) => break,
        }
    }
    got
}

#[test]
fn source_change_reported_when_project_root_has_tmp_component() {
    let td = tempfile::tempdir().unwrap();
    // Force a `tmp` component into the project root regardless of $TMPDIR.
    let root = td.path().join("tmp").join("proj");
    std::fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();

    let (tx, rx) = channel();
    let _deb = FileWatcher::new()
        .with_metadata_filter(true)
        .watch(vec![root.clone()], tx)
        .unwrap();

    std::thread::sleep(Duration::from_millis(300));
    std::fs::write(root.join("a.rock"), b"fn main() { let x = 1 }\n").unwrap();

    let got = drain(&rx);
    assert!(
        got.iter()
            .any(|e| e.paths.iter().any(|p| p.ends_with("a.rock"))),
        "source edit under a tmp-component project root was dropped \
         (is_tmp_scratch_file misclassified it); events={got:?}"
    );
}

#[test]
fn project_relative_tmp_dir_is_still_scratch() {
    // The genuine intent must survive the fix: `<project>/tmp/main.lua`
    // is compiler output and must NOT trigger a reload, while
    // `<project>/tmp/restart.txt` still must.
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("tmp").join("proj");
    std::fs::create_dir_all(root.join("tmp")).unwrap();
    let root = root.canonicalize().unwrap();

    let (tx, rx) = channel();
    let _deb = FileWatcher::new()
        .with_metadata_filter(true)
        .with_restart_trigger(true)
        .watch(vec![root.clone()], tx)
        .unwrap();

    std::thread::sleep(Duration::from_millis(300));
    // Compiler scratch output: must be ignored.
    std::fs::write(root.join("tmp").join("main.lua"), b"-- generated\n").unwrap();
    std::thread::sleep(Duration::from_millis(200));
    // Manual restart signal: must get through.
    std::fs::write(root.join("tmp").join("restart.txt"), b"1\n").unwrap();

    let got = drain(&rx);
    assert!(
        got.iter().any(|e| e.is_restart_trigger),
        "project tmp/restart.txt must still trigger; events={got:?}"
    );
    assert!(
        !got.iter().any(|e| e
            .paths
            .iter()
            .any(|p| p.ends_with("main.lua"))),
        "project tmp/main.lua is compiler scratch and must not reload; events={got:?}"
    );
}
