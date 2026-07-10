//! P3 of docs/replay-scrub-plan.md: timeline controls (pause / step /
//! run-to-tick) and the auto-record ring buffer. The ImGui timeline bar
//! itself is visual and left to the author; here we drive the testable core
//! headless via env (STONE_START_PAUSED / STONE_STEP_FRAMES / STONE_RUN_TO_TICK)
//! and observe the effect through --dump-state (current_tick / time frozen or
//! advanced by the exact amount).
//!
//! Heavy: spawns `rock frame`. Excluded from the default nextest profile
//! (see `.config/nextest.toml`); runs under `-P full`.

use std::path::Path;
use std::process::Command;

fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
}

/// `rock frame <game> -n N --dump-state [extra…]` with extra env; parsed JSON.
fn run_frame_env(
    game: &str,
    frames: u32,
    extra: &[&str],
    env: &[(&str, &str)],
) -> serde_json::Value {
    let rock_bin = env!("CARGO_BIN_EXE_rock");
    let scratch = tempfile::tempdir().expect("scratch dir");
    let scratch_path = scratch.path().to_string_lossy().into_owned();
    let frames_s = frames.to_string();
    let mut args: Vec<&str> = vec![
        "frame", game, "-n", &frames_s, "--dump-state", "--output", &scratch_path,
    ];
    args.extend_from_slice(extra);
    let out = Command::new(rock_bin)
        .args(&args)
        .envs(env.iter().copied())
        .current_dir(workspace_root())
        .output()
        .expect("failed to spawn rock frame");
    assert!(
        out.status.success(),
        "rock frame {game} {extra:?} env={env:?} exited {:?}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "dump-state not valid JSON ({e})\nstdout:\n{}",
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

fn current_tick(state: &serde_json::Value) -> i64 {
    state["stone"]["current_tick"].as_i64().expect("current_tick")
}

/// A run started paused advances no game tick and no game time, however many
/// frames render.
#[test]
fn start_paused_freezes_tick_and_time() {
    let s = run_frame_env("bitgun", 30, &[], &[("STONE_START_PAUSED", "1")]);
    assert_eq!(current_tick(&s), 0, "paused run must not advance the tick counter");
    assert_eq!(
        s["stone"]["time"].as_f64(),
        Some(0.0),
        "paused run must not accumulate game time\ntime={}",
        s["stone"]["time"]
    );
}

/// Queued step-frames advance exactly that many ticks, then re-freeze.
#[test]
fn step_frames_advance_exactly_n_ticks() {
    let s = run_frame_env(
        "bitgun",
        30,
        &[],
        &[("STONE_START_PAUSED", "1"), ("STONE_STEP_FRAMES", "3")],
    );
    assert_eq!(
        current_tick(&s),
        3,
        "3 queued steps must advance exactly 3 ticks, then freeze"
    );
}

/// Run-to-tick fast-forwards to the target tick, then pauses there.
#[test]
fn run_to_tick_stops_at_target() {
    let s = run_frame_env("bitgun", 40, &[], &[("STONE_RUN_TO_TICK", "10")]);
    assert_eq!(
        current_tick(&s),
        10,
        "run-to-tick must advance to the target then pause"
    );
}

/// Piece 4 of P3: fast-forward (run-to-tick) skips the render pass on the
/// intermediate ticks, but must reach the SAME simulation state as stepping
/// there with rendering. Record a log, then replay it two ways to tick 8 --
/// seek (render-skipped) vs step (rendered).
///
/// Scope: compare the game package (`gameproject`) + the deterministic engine
/// clock (`stone.time`/`dt`). Render OUTPUT legitimately differs -- skipping
/// render leaves the batch pools / shadow scratch / graphics-stats counters
/// (`RENDER_BATCHES`, `SHADOW_*`, `stats_history_*`, `screenshot_*`, `world`,
/// `gizmo`) unbuilt, which is the whole point of skipping. Those are rebuilt
/// the moment rendering resumes; they are not simulation state. This is the
/// same carve-out principle as PROFILER, extended to rendered-vs-unrendered.
#[test]
fn seek_reaches_same_state_as_stepping() {
    let rock_bin = env!("CARGO_BIN_EXE_rock");
    let dir = tempfile::tempdir().expect("dir");
    let log = dir.path().join("seek.replay");
    let log_s = log.to_string_lossy().into_owned();
    let scratch = tempfile::tempdir().expect("rec scratch");

    // Record a log to replay.
    let rec = Command::new(rock_bin)
        .args([
            "frame", "bitgun", "-n", "30", "--record", &log_s,
            "--output", &scratch.path().to_string_lossy(),
        ])
        .current_dir(workspace_root())
        .output()
        .expect("spawn record");
    assert!(rec.status.success(), "record failed: {}", String::from_utf8_lossy(&rec.stderr));

    // Seek (render-skipped) to tick 8 vs step (rendered) to tick 8.
    let seek = run_frame_env("bitgun", 30, &["--replay", &log_s], &[("STONE_RUN_TO_TICK", "8")]);
    let step = run_frame_env(
        "bitgun",
        30,
        &["--replay", &log_s],
        &[("STONE_START_PAUSED", "1"), ("STONE_STEP_FRAMES", "8")],
    );
    assert_eq!(current_tick(&seek), 8, "seek must reach tick 8");
    assert_eq!(current_tick(&step), 8, "step must reach tick 8");
    assert_eq!(
        seek["gameproject"], step["gameproject"],
        "render-skipped seek must reach identical GAME state to a rendered step"
    );
    assert_eq!(
        seek["stone"]["time"], step["stone"]["time"],
        "the deterministic clock must match too"
    );
}

/// Piece 5 of P3: the auto-record "save repro" action snapshots the live
/// from-boot log to a permanent file that replays cleanly. (Record-from-boot
/// is the always-on ring buffer that ships now; a bounded "last N seconds"
/// window needs P4 keyframes -- see stone/engine/replay.rock.) Records while
/// saving a repro at frame 10, then replays the saved repro.
#[test]
fn save_repro_writes_a_replayable_from_boot_log() {
    let rock_bin = env!("CARGO_BIN_EXE_rock");
    let dir = tempfile::tempdir().expect("dir");
    let log = dir.path().join("live.replay");
    let repro = dir.path().join("saved.replay");
    let scratch = tempfile::tempdir().expect("scratch");

    let rec = Command::new(rock_bin)
        .args([
            "frame", "bitgun", "-n", "30", "--record", &log.to_string_lossy(),
            "--output", &scratch.path().to_string_lossy(),
        ])
        .env("STONE_SAVE_REPRO_AT_FRAME", "10")
        .env("STONE_SAVE_REPRO_PATH", repro.to_string_lossy().as_ref())
        .current_dir(workspace_root())
        .output()
        .expect("spawn record+save");
    assert!(rec.status.success(), "record failed: {}", String::from_utf8_lossy(&rec.stderr));

    let text = std::fs::read_to_string(&repro).expect("saved repro must exist");
    assert!(text.starts_with("STONE_REPLAY"), "repro must have the header\n{text}");
    let ticks = text.lines().filter(|l| l.starts_with("t ")).count();
    assert!(ticks >= 5, "repro must contain the from-boot ticks; got {ticks}");

    let rep = Command::new(rock_bin)
        .args([
            "frame", "bitgun", "-n", "1000", "--replay", &repro.to_string_lossy(),
            "--output", &scratch.path().to_string_lossy(),
        ])
        .current_dir(workspace_root())
        .output()
        .expect("spawn replay");
    assert!(
        rep.status.success(),
        "the saved repro must replay cleanly\nstderr:\n{}",
        String::from_utf8_lossy(&rep.stderr)
    );
}
