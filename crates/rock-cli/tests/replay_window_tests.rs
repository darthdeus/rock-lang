//! P3.5 of docs/replay-scrub-plan.md: the `rock replay <game> <file>` windowed
//! playback command. The VISIBLE window + timeline bar are author-verified; what
//! is testable headless is the command wiring and the runner's freeze-at-log-end
//! behavior. The core contract: unlike `rock frame --replay` (batch headless,
//! exits when the log is spent), windowed replay keeps the loop alive past the
//! log end, frozen at the last tick. We drive that headless with STONE_HEADLESS
//! (no visible window) + a FRAME_COUNT bound so the run terminates.
//!
//! Heavy: spawns `rock replay`/`rock frame` against a compiled game. Excluded
//! from the default nextest profile; runs under `-P full`.

use std::path::Path;
use std::process::Command;

fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
}

/// Record a short log and return its path + the temp dir keeping it alive.
fn record_log(game: &str, frames: u32) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().expect("dir");
    let log = dir.path().join("win.replay");
    let log_s = log.to_string_lossy().into_owned();
    let scratch = tempfile::tempdir().expect("scratch");
    let out = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame", game, "-n", &frames.to_string(), "--record", &log_s,
            "--output", &scratch.path().to_string_lossy(),
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(workspace_root())
        .output()
        .expect("spawn record");
    assert!(out.status.success(), "record failed: {}", String::from_utf8_lossy(&out.stderr));
    (dir, log_s)
}

/// Parse the "overall avg=... (N frames in ...)" count from the engine's
/// end-of-run FPS summary on stderr.
fn frames_run(stderr: &str) -> u32 {
    for line in stderr.lines() {
        if let Some(i) = line.find("avg=") {
            if let Some(j) = line[i..].find('(') {
                let rest = &line[i + j + 1..];
                if let Some(n) = rest.split_whitespace().next() {
                    if let Ok(v) = n.parse::<u32>() {
                        return v;
                    }
                }
            }
        }
    }
    panic!("no FPS summary line in stderr:\n{stderr}");
}

/// `rock replay <game> <file>` opens windowed playback that freezes at the last
/// recorded tick and keeps looping (window stays open) instead of exiting at log
/// end. Driven headless (STONE_HEADLESS) and bounded (FRAME_COUNT) so it
/// terminates: the run must execute MORE frames than the ~11-tick log, proving
/// it did not stop when the log was spent.
#[test]
fn windowed_replay_keeps_looping_past_log_end() {
    let (_dir, log) = record_log("bitgun", 10);

    let out = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args(["replay", "bitgun", &log])
        .env("STONE_HEADLESS", "1")
        .env("FRAME_COUNT", "30")
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(workspace_root())
        .output()
        .expect("spawn replay");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "rock replay must exit 0\nstderr:\n{stderr}");
    assert!(
        stderr.contains("replaying") && stderr.contains("ticks"),
        "must announce the replay\nstderr:\n{stderr}"
    );
    let ran = frames_run(&stderr);
    assert!(
        ran >= 20,
        "windowed replay must keep looping past the ~11-tick log (ran {ran} frames)\nstderr:\n{stderr}"
    );
}

/// Control: `rock frame --replay` (batch headless) exits exactly when the log is
/// spent, however large FRAME_COUNT is -- the opposite of windowed playback.
#[test]
fn headless_frame_replay_exits_at_log_end() {
    let (_dir, log) = record_log("bitgun", 10);
    let scratch = tempfile::tempdir().expect("scratch");

    let out = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame", "bitgun", "-n", "30", "--replay", &log,
            "--output", &scratch.path().to_string_lossy(),
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(workspace_root())
        .output()
        .expect("spawn frame replay");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "frame replay must exit 0\nstderr:\n{stderr}");
    let ran = frames_run(&stderr);
    assert!(
        ran <= 15,
        "headless frame replay must stop at the ~11-tick log, not run to FRAME_COUNT=30 (ran {ran})\nstderr:\n{stderr}"
    );
}

/// `rock replay` needs both a game and a log path.
#[test]
fn replay_command_rejects_missing_args() {
    let out = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args(["replay", "bitgun"])
        .current_dir(workspace_root())
        .output()
        .expect("spawn");
    assert!(!out.status.success(), "missing log arg must fail");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("usage: rock replay"),
        "must print usage\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A `--replay` pointing at a nonexistent log aborts (reject, don't fall back).
#[test]
fn replay_command_rejects_missing_log() {
    let out = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args(["replay", "bitgun", "/nonexistent/no.replay"])
        .current_dir(workspace_root())
        .output()
        .expect("spawn");
    assert!(!out.status.success(), "missing log must fail");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("not found"),
        "must report the missing log\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
