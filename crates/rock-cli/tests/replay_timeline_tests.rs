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

/// The engine-owned simulation-state predicate's OUTPUT: record `game` with a
/// per-tick checkpoint so every `c <tick> <name> <hash>` line names exactly the
/// globals the engine hashes as simulation state, and collect that name set.
/// Consuming this (rather than duplicating the exclusion list in Rust) is the
/// P3.5 unification -- the Lua `__replay_is_sim_state` predicate is the single
/// source of truth.
fn sim_state_names(game: &str, frames: u32) -> std::collections::HashSet<String> {
    let rock_bin = env!("CARGO_BIN_EXE_rock");
    let dir = tempfile::tempdir().expect("dir");
    let log = dir.path().join("simnames.replay");
    let scratch = tempfile::tempdir().expect("scratch");
    let out = Command::new(rock_bin)
        .args([
            "frame", game, "-n", &frames.to_string(),
            "--record", &log.to_string_lossy(),
            "--checkpoint-interval", "1",
            "--output", &scratch.path().to_string_lossy(),
        ])
        .current_dir(workspace_root())
        .output()
        .expect("spawn record");
    assert!(out.status.success(), "record failed: {}", String::from_utf8_lossy(&out.stderr));
    let text = std::fs::read_to_string(&log).expect("read log");
    let names: std::collections::HashSet<String> = text
        .lines()
        .filter_map(|l| l.strip_prefix("c "))
        .filter_map(|rest| rest.split(' ').nth(1))
        .map(|s| s.to_string())
        .collect();
    assert!(!names.is_empty(), "record must emit checkpoint lines naming sim-state globals");
    names
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
/// there with rendering. Record a log, then replay it two ways to tick 25 --
/// seek (render-skipped) vs step (rendered).
///
/// Scope is the engine's simulation-state predicate (P3.5): compare every
/// global the engine hashes as sim state (from `sim_state_names`), across all
/// packages. Render OUTPUT legitimately differs -- skipping render leaves the
/// batch pools / shadow scratch / graphics-stats counters and game Materials'
/// `cached_batch` unbuilt (`RENDER_BATCHES`, `SHADOW_*`, `stats_history_*`,
/// `tileset_mat`, ...); those are excluded by the predicate, not compared here.
/// Tick 25 is far enough that the render-only material batches have grown, so
/// this would catch a predicate that failed to exclude them.
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

    // Seek (render-skipped) to tick 25 vs step (rendered) to tick 25.
    let seek = run_frame_env("bitgun", 30, &["--replay", &log_s], &[("STONE_RUN_TO_TICK", "25")]);
    let step = run_frame_env(
        "bitgun",
        30,
        &["--replay", &log_s],
        &[("STONE_START_PAUSED", "1"), ("STONE_STEP_FRAMES", "25")],
    );
    assert_eq!(current_tick(&seek), 25, "seek must reach tick 25");
    assert_eq!(current_tick(&step), 25, "step must reach tick 25");

    // Compare every simulation-state global (the predicate's output), across
    // packages, restricted to the names the engine hashes.
    let sim = sim_state_names("bitgun", 30);
    let mut compared = 0;
    for (pkg, globals) in seek.as_object().expect("dump is an object") {
        let step_pkg = &step[pkg];
        for (name, sv) in globals.as_object().expect("package is an object") {
            if !sim.contains(name) {
                continue;
            }
            assert_eq!(
                sv, &step_pkg[name],
                "sim-state global {pkg}.{name} must match between render-skipped seek and rendered step"
            );
            compared += 1;
        }
    }
    assert!(compared > 0, "must have compared at least one sim-state global");
}

/// Record a short log for the keyboard-shortcut tests. Returns the temp dir
/// (keeps the file alive) + the log path.
fn record_short_log(game: &str, frames: u32) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().expect("dir");
    let log = dir.path().join("kb.replay");
    let log_s = log.to_string_lossy().into_owned();
    let scratch = tempfile::tempdir().expect("scratch");
    let out = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame", game, "-n", &frames.to_string(), "--record", &log_s,
            "--output", &scratch.path().to_string_lossy(),
        ])
        .current_dir(workspace_root())
        .output()
        .expect("spawn record");
    assert!(out.status.success(), "record failed: {}", String::from_utf8_lossy(&out.stderr));
    (dir, log_s)
}

/// P3.5: the replay timeline SPACE shortcut toggles play/pause by reading ImGui
/// key state, NOT the replayed `keys_down` (the log owns that during replay).
/// We inject a Space key event into ImGui at frame 3 of a windowed replay (the
/// recorded log has no keys down, so keys_down never carries Space) and assert
/// the run freezes: current_tick stops advancing well before the ~20-tick log
/// end. Only the ImGui-IO path could cause that.
#[test]
fn space_toggles_pause_via_imgui_not_replayed_input() {
    let (_dir, log) = record_short_log("bitgun", 20);
    let frozen = run_frame_env(
        "bitgun",
        40,
        &["--replay", &log],
        &[
            ("STONE_REPLAY_WINDOW", "1"),
            ("STONE_HEADLESS", "1"),
            ("STONE_TIMELINE_KEY_AT", "3"),
            ("STONE_TIMELINE_KEY", "space"),
        ],
    );
    let ct = current_tick(&frozen);
    assert!(
        ct < 10,
        "Space must pause the replay via ImGui IO (froze at tick {ct}, expected < 10, log is ~20)"
    );
}

/// P3.5: the RIGHT shortcut steps exactly one tick, again via ImGui IO. Start
/// paused (tick frozen at 0), inject Right at frame 3, and assert the timeline
/// advanced exactly one tick.
#[test]
fn right_steps_one_tick_via_imgui_not_replayed_input() {
    let (_dir, log) = record_short_log("bitgun", 20);
    let stepped = run_frame_env(
        "bitgun",
        40,
        &["--replay", &log],
        &[
            ("STONE_REPLAY_WINDOW", "1"),
            ("STONE_HEADLESS", "1"),
            ("STONE_START_PAUSED", "1"),
            ("STONE_TIMELINE_KEY_AT", "3"),
            ("STONE_TIMELINE_KEY", "right"),
        ],
    );
    assert_eq!(
        current_tick(&stepped), 1,
        "Right must step exactly one tick from a paused start"
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
