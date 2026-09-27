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
    match force_error_at {
        Some(n) => run_frame_with(&[("STONE_FRAME_FORCE_ERROR", n.to_string())]),
        None => run_frame_with(&[]),
    }
}

fn run_frame_with(envs: &[(&str, String)]) -> (String, bool) {
    let out_dir = tempfile::tempdir().expect("tempdir");
    let mut cmd = Command::new(rock_bin());
    cmd.current_dir(repo_root())
        .args(["frame", "games/match3", "-n", "5"])
        .arg("--output")
        .arg(out_dir.path().join("main.lua"))
        .env("ROCK_FRAME_TIMEOUT_SECS", "120");
    for (key, value) in envs {
        cmd.env(key, value);
    }
    let out = cmd.output().expect("spawn rock frame");
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (text, out.status.success())
}

#[test]
fn a_repeating_caught_frame_error_fails_once_with_its_total() {
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

    // The deterministic raise repeats on frames 2 through 5. It must be logged
    // once rather than burying the stream in tracebacks, while the shutdown
    // summary and bounded-run failure retain the total.
    // Count EMITTED lines: the shutdown summary echoes the stored message, so a
    // bare "Error in frame" also matches inside the summary and reads as a
    // second copy.
    let logged = text.matches("[GAME] Error in frame").count();
    assert_eq!(
        logged, 1,
        "a repeating frame error must log once, got {logged} copies:\n{text}"
    );
    // The denominator: without this, logging once is indistinguishable from
    // losing every repeat.
    assert!(
        text.contains("suppressed x4 at") && text.contains("4 frame(s) raised during run"),
        "the suppression and bounded-run summaries must report all four errors:\n{text}"
    );
    // The key must come from the ERROR, not the call site. Every frame error is
    // raised through one line in runner.rock, so a site key would collapse two
    // DIFFERENT faults into whichever happened first -- silently, and worse than
    // the spam this replaces. The summary prints its key, so this reads it.
    assert!(
        text.contains("forced frame error (STONE_FRAME_FORCE_ERROR)"),
        "the dedupe key is not derived from the error message:\n{text}"
    );
}

/// A raised frame must still publish the image uploads it queued.
///
/// The upload pump runs between update and render, so a raise in update used to
/// skip it. A fault whose condition is the queue itself then reproduced on every
/// later frame: MAG's run admission asserted an empty queue and raised on each
/// remaining frame of a session, with the queue never draining.
#[test]
fn a_frame_error_still_publishes_queued_image_uploads() {
    let image = repo_root().join("games/match3/tests/sims/smoke.snap/boot.png");
    let (text, ok) = run_frame_with(&[
        ("STONE_TEST_IMAGE_LOAD_AT_FRAME", "2".to_string()),
        ("STONE_TEST_IMAGE_LOAD_PATH", image.to_string_lossy().into_owned()),
        ("STONE_FRAME_FORCE_ERROR_WHILE_UPLOADS_PENDING", "1".to_string()),
    ]);
    assert!(
        text.contains("forced frame error while image uploads are pending"),
        "the queued load did not reach the queue-conditioned raise:\n{text}"
    );
    assert!(
        text.contains("1 frame(s) raised during run"),
        "the raised frame did not drain the queue, so the fault repeated:\n{text}"
    );
    assert!(!ok, "the one raised frame must still fail the bounded run:\n{text}");
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
