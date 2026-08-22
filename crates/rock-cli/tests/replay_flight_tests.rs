//! P3.5 of docs/replay-scrub-plan.md: the flight-recorder log-format additions
//! (v4) -- a `build <id>` header field and `reload <tick>` markers -- plus the
//! determinism warnings replay emits when a log was recorded against code that
//! has since changed. The always-on `rock stone` recorder wiring is
//! author-verified in a real window; the log format + warnings are testable
//! headless via the STONE_BUILD_ID / STONE_RELOAD_AT_FRAME hooks.
//!
//! Heavy: spawns `rock frame` against a compiled game. Excluded from the
//! default nextest profile; runs under `-P full`.

use std::path::Path;
use std::process::Command;

fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
}

/// Record a log with the given env, returning (temp dir, log path).
fn record_with_env(game: &str, frames: u32, env: &[(&str, &str)]) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().expect("dir");
    let log = dir.path().join("flight.replay");
    let log_s = log.to_string_lossy().into_owned();
    let scratch = tempfile::tempdir().expect("scratch");
    let out = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame", game, "-n", &frames.to_string(), "--record", &log_s,
            "--output", &scratch.path().to_string_lossy(),
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .envs(env.iter().copied())
        .current_dir(workspace_root())
        .output()
        .expect("spawn record");
    assert!(out.status.success(), "record failed: {}", String::from_utf8_lossy(&out.stderr));
    (dir, log_s)
}

/// Replay a log with the given env; return the process stderr.
fn replay_stderr(game: &str, log: &str, env: &[(&str, &str)]) -> String {
    let scratch = tempfile::tempdir().expect("scratch");
    let out = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame", game, "-n", "15", "--replay", log,
            "--output", &scratch.path().to_string_lossy(),
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .envs(env.iter().copied())
        .current_dir(workspace_root())
        .output()
        .expect("spawn replay");
    assert!(out.status.success(), "replay must exit 0\nstderr:\n{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// The v7 header stamps the build id, and a hot-reload during recording writes a
/// `reload <tick>` marker.
#[test]
fn record_stamps_build_id_and_reload_marker() {
    let (_dir, log) = record_with_env(
        "bitgun", 15,
        &[("STONE_BUILD_ID", "buildAAA"), ("STONE_RELOAD_AT_FRAME", "5")],
    );
    let text = std::fs::read_to_string(&log).expect("read log");
    assert!(text.starts_with("STONE_REPLAY 7"), "must be a v7 log\n{text}");
    assert!(
        text.lines().any(|l| l == "build buildAAA"),
        "header must carry the build id\n{text}"
    );
    assert!(
        text.lines().any(|l| l.starts_with("reload ")),
        "a mid-recording reload must write a marker\n{text}"
    );
}

/// Replaying a reload-marked log warns that determinism ends at the reload.
#[test]
fn replay_warns_on_reload_marked_log() {
    let (_dir, log) = record_with_env("bitgun", 15, &[("STONE_RELOAD_AT_FRAME", "5")]);
    let stderr = replay_stderr("bitgun", &log, &[]);
    assert!(
        stderr.contains("hot-reload marker") && stderr.to_lowercase().contains("determinism"),
        "replay must warn on a reload-marked log\nstderr:\n{stderr}"
    );
}

/// The F9 "save repro" action promotes the always-on recording to a permanent
/// tick-stamped `repro-<tick>.replay` next to the source log, and that file is a
/// valid from-boot log that replays cleanly. Exercised headless via the
/// STONE_SAVE_REPRO_AUTO_AT_FRAME hook (F9 itself is author-verified).
#[test]
fn f9_save_repro_writes_a_replayable_tick_stamped_file() {
    let dir = tempfile::tempdir().expect("dir");
    let log = dir.path().join("last.replay");
    let scratch = tempfile::tempdir().expect("scratch");

    let rec = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame", "bitgun", "-n", "15", "--record", &log.to_string_lossy(),
            "--output", &scratch.path().to_string_lossy(),
        ])
        .env("STONE_SAVE_REPRO_AUTO_AT_FRAME", "8")
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(workspace_root())
        .output()
        .expect("spawn record");
    assert!(rec.status.success(), "record failed: {}", String::from_utf8_lossy(&rec.stderr));

    // The derived dest is repro-<tick>.replay in the log's dir.
    let repro = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("repro-")))
        .expect("F9 must write a repro-<tick>.replay file");
    let text = std::fs::read_to_string(&repro).expect("read repro");
    assert!(text.starts_with("STONE_REPLAY"), "repro must be a valid log\n{text}");

    let rep = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame", "bitgun", "-n", "1000", "--replay", &repro.to_string_lossy(),
            "--output", &scratch.path().to_string_lossy(),
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(workspace_root())
        .output()
        .expect("spawn replay");
    assert!(
        rep.status.success(),
        "the saved repro must replay cleanly\nstderr:\n{}",
        String::from_utf8_lossy(&rep.stderr)
    );
}

/// Replaying a log against a DIFFERENT build id warns; the same build id does
/// not.
#[test]
fn replay_warns_on_build_id_mismatch_only() {
    let (_dir, log) = record_with_env("bitgun", 15, &[("STONE_BUILD_ID", "buildAAA")]);

    let mismatched = replay_stderr("bitgun", &log, &[("STONE_BUILD_ID", "buildBBB")]);
    assert!(
        mismatched.contains("built against buildAAA") && mismatched.contains("buildBBB"),
        "a build mismatch must warn\nstderr:\n{mismatched}"
    );

    let matched = replay_stderr("bitgun", &log, &[("STONE_BUILD_ID", "buildAAA")]);
    assert!(
        !matched.contains("built against"),
        "a matching build id must NOT warn\nstderr:\n{matched}"
    );
}
