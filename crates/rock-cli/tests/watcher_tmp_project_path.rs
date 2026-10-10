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

// Event-led: wait for the event the test expects, keeping every event that
// arrives before it, then collect trailers until QUIESCENCE passes quietly.
// Events arrive in write order, so a suppressed path written before the
// awaited one would already be in the result. HANG_BACKSTOP only bounds a
// lost event.
const QUIESCENCE: Duration = Duration::from_millis(500);

fn drain_until(
    rx: &std::sync::mpsc::Receiver<rock_cli::FileChangeEvent>,
    expected: impl Fn(&rock_cli::FileChangeEvent) -> bool,
) -> Vec<rock_cli::FileChangeEvent> {
    let mut got = Vec::new();
    let hard = Instant::now() + test_utils::HANG_BACKSTOP;
    while !got.iter().any(&expected) {
        match rx.recv_timeout(hard.saturating_duration_since(Instant::now())) {
            Ok(ev) => got.push(ev),
            Err(_) => return got,
        }
    }
    while Instant::now() < hard {
        match rx.recv_timeout(QUIESCENCE) {
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

    let got = drain_until(&rx, |e| e.paths.iter().any(|p| p.ends_with("a.rock")));
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

    let got = drain_until(&rx, |e| e.is_restart_trigger);
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
