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

/// `rock frame <game> -n N --dump-state` with extra env; returns parsed JSON.
fn run_frame_env(game: &str, frames: u32, env: &[(&str, &str)]) -> serde_json::Value {
    let rock_bin = env!("CARGO_BIN_EXE_rock");
    let scratch = tempfile::tempdir().expect("scratch dir");
    let scratch_path = scratch.path().to_string_lossy().into_owned();
    let frames_s = frames.to_string();
    let out = Command::new(rock_bin)
        .args(["frame", game, "-n", &frames_s, "--dump-state", "--output", &scratch_path])
        .envs(env.iter().copied())
        .current_dir(workspace_root())
        .output()
        .expect("failed to spawn rock frame");
    assert!(
        out.status.success(),
        "rock frame {game} env={env:?} exited {:?}\nstderr:\n{}",
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
    let s = run_frame_env("bitgun", 30, &[("STONE_START_PAUSED", "1")]);
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
    let s = run_frame_env("bitgun", 40, &[("STONE_RUN_TO_TICK", "10")]);
    assert_eq!(
        current_tick(&s),
        10,
        "run-to-tick must advance to the target then pause"
    );
}
