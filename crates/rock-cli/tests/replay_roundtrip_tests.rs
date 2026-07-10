//! P1 of docs/replay-scrub-plan.md: record a headless run, replay it, and
//! assert the replayed `--dump-state` reproduces the recorded run's game +
//! engine state exactly.
//!
//! Determinism scope: the plan's contract carves out wall-clock DIAGNOSTICS
//! (`stone_get_time()` "is fine in diagnostics") from the determinism
//! guarantee. Those live entirely under `stone.PROFILER` (per-frame CPU/GPU
//! timing metrics + `frame_t*` timestamps) and are inherently
//! non-reproducible. We strip that one subtree and assert byte-identity of
//! everything else. This matches the repo's existing dump tests
//! (game_snapshot_tests), which all compare curated game-state subtrees and
//! never the profiler. Everything outside PROFILER is dt-derived and replay
//! reproduces it from the logged dt.
//!
//! Heavy: spawns `rock frame` against a compiled game. Excluded from the
//! default nextest profile (see `.config/nextest.toml`); runs under `-P full`.

use std::path::Path;
use std::process::Command;

fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
}

/// Run `rock frame <game> -n N --dump-state [extra…]` and return parsed JSON.
/// Each call gets its own --output scratch so concurrent runs don't race on
/// `<output>/main.lua`. No ROCK_TEST_SEED: the record run seeds from the
/// prelude's default and writes the seed into the replay header, so replay
/// must restore it — exercising the RNG-restore path, not just dt.
fn run_frame(game: &str, frames: u32, extra: &[&str]) -> serde_json::Value {
    let rock_bin = env!("CARGO_BIN_EXE_rock");
    let scratch = tempfile::tempdir().expect("scratch dir");
    let scratch_path = scratch.path().to_string_lossy().into_owned();
    let frames_s = frames.to_string();
    let mut args: Vec<&str> = vec![
        "frame", game, "-n", &frames_s, "--dump-state", "--output", &scratch_path,
    ];
    args.extend_from_slice(extra);

    let output = Command::new(rock_bin)
        .args(&args)
        .current_dir(workspace_root())
        .output()
        .expect("failed to spawn rock frame");
    assert!(
        output.status.success(),
        "rock frame {game} {extra:?} exited with {:?}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "dump-state stdout was not valid JSON ({e})\nstdout:\n{}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

/// Like `run_frame` but returns the raw Output without asserting success —
/// for tests that expect the run to be rejected.
fn run_frame_raw(game: &str, frames: u32, extra: &[&str]) -> std::process::Output {
    let rock_bin = env!("CARGO_BIN_EXE_rock");
    let scratch = tempfile::tempdir().expect("scratch dir");
    let scratch_path = scratch.path().to_string_lossy().into_owned();
    let frames_s = frames.to_string();
    let mut args: Vec<&str> = vec!["frame", game, "-n", &frames_s, "--output", &scratch_path];
    args.extend_from_slice(extra);
    Command::new(rock_bin)
        .args(&args)
        .current_dir(workspace_root())
        .output()
        .expect("failed to spawn rock frame")
}

/// Remove the wall-clock diagnostics carve-out (see module docs).
fn strip_diagnostics(v: &mut serde_json::Value) {
    if let Some(stone) = v.get_mut("stone").and_then(|s| s.as_object_mut()) {
        stone.remove("PROFILER");
    }
}

#[test]
fn replay_reproduces_dump_state_byte_identical() {
    let game = "bitgun";
    let frames = 30;
    let dir = tempfile::tempdir().expect("replay-log dir");
    let log = dir.path().join("run.replay");
    let log_s = log.to_string_lossy().into_owned();

    // Control: two plain runs must DIFFER even after stripping diagnostics —
    // proves the deterministic dump is dt-sensitive, so the identity check
    // below is not trivially green.
    let mut plain_a = run_frame(game, frames, &[]);
    let mut plain_b = run_frame(game, frames, &[]);
    strip_diagnostics(&mut plain_a);
    strip_diagnostics(&mut plain_b);
    assert_ne!(
        plain_a, plain_b,
        "control: two plain runs should differ (dt-derived state), else the \
         identity assertion is meaningless"
    );

    // Record a run, then replay its log. The replayed dump must reproduce the
    // recorded run's state exactly (diagnostics excluded).
    let mut record_dump = run_frame(game, frames, &["--record", &log_s]);
    let mut replay_dump = run_frame(game, frames, &["--replay", &log_s]);
    strip_diagnostics(&mut record_dump);
    strip_diagnostics(&mut replay_dump);
    assert_eq!(
        record_dump, replay_dump,
        "replay must reproduce the recorded run's state byte-for-byte \
         (diagnostics excluded)"
    );
}

/// Headless runs never receive real input, so the record/replay round-trip
/// above only exercises empty key/button ticks. Cover the non-empty
/// parse+apply path directly: record to get a valid header, then rewrite the
/// last tick to hold a key + a mouse button and assert replay applies both.
#[test]
fn replay_applies_recorded_keys_and_buttons() {
    let game = "bitgun";
    let frames = 10;
    let dir = tempfile::tempdir().expect("replay-log dir");
    let log = dir.path().join("keys.replay");
    let log_s = log.to_string_lossy().into_owned();

    let _ = run_frame(game, frames, &["--record", &log_s]);

    // Tick line: `t <dt> <mx> <my> <wheel> <imgui> <keys> <buttons>`.
    let text = std::fs::read_to_string(&log).expect("read replay log");
    let mut lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();
    let last = lines
        .iter()
        .rposition(|l| l.starts_with("t "))
        .expect("a tick line");
    let mut fields: Vec<String> = lines[last].split(' ').map(|s| s.to_string()).collect();
    fields[6] = "space".to_string();
    fields[7] = "1".to_string();
    lines[last] = fields.join(" ");
    std::fs::write(&log, lines.join("\n") + "\n").expect("rewrite replay log");

    let state = run_frame(game, frames, &["--replay", &log_s]);
    assert_eq!(
        state["stone"]["keys_down"]["space"],
        serde_json::json!(true),
        "replay must apply a recorded held key to keys_down"
    );
    // Button 1 → mouse_down = {[1]=true}; a contiguous-from-1 int-keyed map
    // renders as an array, so `[true]`.
    assert_eq!(
        state["stone"]["mouse_down"],
        serde_json::json!([true]),
        "replay must apply a recorded held mouse button to mouse_down"
    );
}

/// `--replay` with an unopenable log must abort the run, not silently fall
/// back to a live (non-replayed) run whose output looks plausible.
#[test]
fn replay_rejects_missing_log() {
    let out = run_frame_raw("bitgun", 5, &["--replay", "/nonexistent/no.replay"]);
    assert!(
        !out.status.success(),
        "a missing replay log must abort the run, got success\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("failed to open replay log"),
        "stderr should name the failure\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A file that isn't a replay log (bad magic) and a log with malformed tick
/// lines must both be rejected — a truncated/corrupt log silently replaying
/// short reads as a shorter, wrong repro.
#[test]
fn replay_rejects_corrupt_log() {
    let dir = tempfile::tempdir().expect("dir");

    let not_a_log = dir.path().join("not_a_log.replay");
    std::fs::write(&not_a_log, "hello world\n").expect("write");
    let out = run_frame_raw("bitgun", 5, &["--replay", &not_a_log.to_string_lossy()]);
    assert!(
        !out.status.success(),
        "a non-replay file must abort the run\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let corrupt = dir.path().join("corrupt.replay");
    std::fs::write(
        &corrupt,
        "STONE_REPLAY 1\nseed 42\nwindow 0x1p9 0x1p9\ngame bitgun\nt garbage line\n",
    )
    .expect("write");
    let out = run_frame_raw("bitgun", 5, &["--replay", &corrupt.to_string_lossy()]);
    assert!(
        !out.status.success(),
        "malformed tick lines must abort the run\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("malformed"),
        "stderr should report the malformed tick count\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
