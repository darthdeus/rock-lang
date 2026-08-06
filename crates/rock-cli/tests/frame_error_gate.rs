//! A caught frame error must FAIL a bounded run.
//!
//! The frame loop wraps `run_one_frame` in xpcall and continues on a raise,
//! which is right for a dev session and wrong for a gate. Measured: with
//! `Map.get`'s `@nomangle` dropped, a smoke run printed `attempt to call global
//! 'get'` with a full traceback on all 31 frames, then reported `30 frames
//! executed successfully`, exit 0, and `just test-games` said `ok`. The runner
//! now exits 3 on `FRAME_ERROR_COUNT > 0` for bounded runs, beside the GL,
//! shader and uniform-block counts it already gates on.
//!
//! `STONE_FRAME_FORCE_ERROR=<n>` raises inside `run_one_frame` itself, so the
//! REAL catch-and-continue path runs rather than a hand-faked count. The
//! control run (same command, no injection) is what keeps this from passing for
//! the wrong reason.

use std::process::Command;

fn rock_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_rock"))
}

fn repo_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

/// `rock frame games/match3` for 5 frames, optionally raising from frame 2 on.
/// Returns (combined output, exit-status-success).
fn run_frame(force_error_at: Option<i32>) -> (String, bool) {
    let out_dir = tempfile::tempdir().expect("tempdir");
    let mut cmd = Command::new(rock_bin());
    cmd.current_dir(repo_root())
        .args(["frame", "games/match3", "-n", "5"])
        .arg("--output")
        .arg(out_dir.path().join("main.lua"))
        .env("ROCK_FRAME_TIMEOUT_SECS", "120");
    if let Some(n) = force_error_at {
        cmd.env("STONE_FRAME_FORCE_ERROR", n.to_string());
    }
    let out = cmd.output().expect("spawn rock frame");
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (text, out.status.success())
}

#[test]
fn a_caught_frame_error_fails_a_bounded_run() {
    let (text, ok) = run_frame(Some(2));
    assert!(
        text.contains("forced frame error"),
        "injection did not reach the real frame body:\n{text}"
    );
    assert!(
        text.contains("Error in frame"),
        "the raise did not go through the xpcall traceback path:\n{text}"
    );
    assert!(
        text.contains("frame(s) raised during run"),
        "the run did not report the frame-error count:\n{text}"
    );
    assert!(!ok, "a caught frame error must fail the run:\n{text}");
    // The precise regression: the run reached the end of the loop and would
    // have announced success. The count is what makes `successfully` false.
    assert!(
        !text.contains("frames executed successfully"),
        "the run still announced success over raised frames:\n{text}"
    );
}

/// The control. Without the injection the same command must pass, so the test
/// above cannot be passing because `rock frame games/match3` is broken.
#[test]
fn a_clean_bounded_run_still_passes() {
    let (text, ok) = run_frame(None);
    assert!(ok, "the uninjected control run must pass:\n{text}");
    assert!(
        text.contains("frames executed successfully"),
        "the control did not reach the success line:\n{text}"
    );
    assert!(
        !text.contains("frame(s) raised during run"),
        "the control reported frame errors, so the injected arm proves nothing:\n{text}"
    );
}
