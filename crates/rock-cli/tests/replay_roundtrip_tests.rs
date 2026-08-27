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
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
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
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(workspace_root())
        .output()
        .expect("failed to spawn rock frame")
}

/// Like `run_frame_raw` but with extra environment variables set on the child.
fn run_frame_raw_env(
    game: &str,
    frames: u32,
    extra: &[&str],
    env: &[(&str, &str)],
) -> std::process::Output {
    let rock_bin = env!("CARGO_BIN_EXE_rock");
    let scratch = tempfile::tempdir().expect("scratch dir");
    let scratch_path = scratch.path().to_string_lossy().into_owned();
    let frames_s = frames.to_string();
    let mut args: Vec<&str> = vec!["frame", game, "-n", &frames_s, "--output", &scratch_path];
    args.extend_from_slice(extra);
    Command::new(rock_bin)
        .args(&args)
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .envs(env.iter().copied())
        .current_dir(workspace_root())
        .output()
        .expect("failed to spawn rock frame")
}

/// Remove the wall-clock diagnostics carve-out (see module docs). This is the
/// BYTE-IDENTITY contract: record vs a RENDERED replay must match on everything
/// except non-reproducible wall-clock diagnostics (PROFILER), INCLUDING all
/// render state -- so this is deliberately NOT the sim-state predicate (which
/// the render-skipped seek test uses). Both runs render here, so render state
/// is identical and must be compared.
fn strip_diagnostics(v: &mut serde_json::Value) {
    if let Some(obj) = v.as_object_mut() {
        // The `__stone_diag` envelope is the non-dumped test-observability
        // readout (`_G.__stone_diag`), not registered game/engine state. It
        // carries replay-only counters (ghost draws, etc.) that legitimately
        // differ between a record run and its replay -- drop it wholesale.
        obj.remove("__stone_diag");
    }
    if let Some(stone) = v.get_mut("stone").and_then(|s| s.as_object_mut()) {
        stone.remove("PROFILER");
    }
}

/// Piece 2 of P3: a window resize during recording is captured as an `r`
/// line and re-applied at the matching tick on replay (so screen_to_world
/// reproduces). Headless never resizes, so we inject a resize line into a
/// recorded log and assert replay adopts it. `tonumber` parses the decimal
/// coords fine (records write %a hex; both round-trip).
#[test]
fn replay_applies_recorded_window_resize() {
    let dir = tempfile::tempdir().expect("dir");
    let log = dir.path().join("resize.replay");
    let log_s = log.to_string_lossy().into_owned();

    let _ = run_frame("bitgun", 30, &["--record", &log_s]);

    // Insert a resize to 800x600 at tick 5, after the 5 header lines.
    let text = std::fs::read_to_string(&log).expect("read log");
    let mut lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();
    lines.insert(5, "r 5 800 600".to_string());
    std::fs::write(&log, lines.join("\n") + "\n").expect("rewrite log");

    let state = run_frame("bitgun", 30, &["--replay", &log_s]);
    let ws = &state["stone"]["WINDOW_SIZE"];
    assert_eq!(
        ws["x"].as_f64(),
        Some(800.0),
        "replay must adopt the recorded resize width\nWINDOW_SIZE={ws}"
    );
    assert_eq!(
        ws["y"].as_f64(),
        Some(600.0),
        "replay must adopt the recorded resize height\nWINDOW_SIZE={ws}"
    );
}

/// Piece 1 of P3: the record log is streamed to disk as ticks are captured,
/// so a crash before shutdown still leaves a valid, replayable partial log.
/// `STONE_CRASH_AT_FRAME` hard-exits the game with no shutdown flush — the
/// exact scenario incremental flush protects. Before streaming (buffer +
/// write-at-shutdown), the crash left an empty/absent log.
#[test]
fn record_log_survives_a_crash_via_incremental_flush() {
    let dir = tempfile::tempdir().expect("dir");
    let log = dir.path().join("crash.replay");
    let log_s = log.to_string_lossy().into_owned();

    let out = run_frame_raw_env(
        "bitgun",
        1000,
        &["--record", &log_s],
        &[("STONE_CRASH_AT_FRAME", "10")],
    );
    assert!(
        !out.status.success(),
        "the crash hook should exit nonzero\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let text = std::fs::read_to_string(&log).expect("partial log must exist after a crash");
    assert!(
        text.starts_with("STONE_REPLAY"),
        "partial log must still start with the header\n{text}"
    );
    let ticks = text.lines().filter(|l| l.starts_with("t ")).count();
    assert!(
        ticks >= 5,
        "crash-surviving log must contain streamed tick lines; got {ticks}\n{text}"
    );

    // The partial log is a valid prefix and replays cleanly.
    let rep = run_frame_raw("bitgun", 1000, &["--replay", &log_s]);
    assert!(
        rep.status.success(),
        "partial crash log must replay cleanly\nstderr:\n{}",
        String::from_utf8_lossy(&rep.stderr)
    );
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

    // Native resource ids are process-local allocation identity, not state, and
    // two processes are free to agree or disagree on them. The engine's
    // serializer already renders those leaves as host projections
    // (`stone/engine/host_projection.rock`); counting them here is the
    // DENOMINATOR, so a green comparison cannot mean "there was nothing to
    // exclude". An arm asserting the raw dumps DIFFER would be pinning a
    // coincidence, not a property.
    let projections = crate::stone_harness::count_host_projections(&record_dump)
        + crate::stone_harness::count_host_projections(&replay_dump);
    assert!(projections > 0, "the dumps carried no host-projection leaf");
    assert_eq!(
        record_dump, replay_dump,
        "replay must reproduce the recorded run's state byte-for-byte \
         (diagnostics and process-local resource ids excluded)"
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

#[test]
fn replay_applies_explicit_v6_pad_buttons_and_axes() {
    let dir = tempfile::tempdir().expect("replay-log dir");
    let log = dir.path().join("pad-v6.replay");
    let text = "STONE_REPLAY 6\nseed 42\nwindow 0x1p9 0x1p9\ngame bitgun\n"
        .to_string()
        + "t 0x0p+0 0x0p+0 0x0p+0 0 0 - - a leftx=0x1p-1\n";
    std::fs::write(&log, text).expect("write v6 pad log");

    let state = run_frame("bitgun", 1, &["--replay", &log.to_string_lossy()]);
    assert_eq!(
        state["stone"]["pad_down"]["a"],
        serde_json::json!(true),
        "v6 replay must project a recorded pad button into effective state"
    );
    assert_eq!(
        state["stone"]["pad_axes"]["leftx"],
        serde_json::json!(0.5),
        "v6 replay must project a recorded pad axis into effective state"
    );
}

#[test]
fn recording_and_branch_rewrite_use_the_v9_pad_record_renderer() {
    let dir = tempfile::tempdir().expect("replay-log dir");
    let log = dir.path().join("v9-renderer.replay");
    let log_s = log.to_string_lossy().into_owned();

    let _ = run_frame("bitgun", 2, &["--record", &log_s]);
    let text = std::fs::read_to_string(&log).expect("read v9 record");
    assert!(text.starts_with("STONE_REPLAY 13\n"), "recording must use the v13 header: {text}");
    let tick = text.lines().find(|line| line.starts_with("t ")).expect("tick");
    assert_eq!(tick.split(' ').count(), 10, "v9 tick has nine payload fields");
    assert!(tick.ends_with(" - -"), "empty live pad state has explicit empty fields: {tick}");
}

#[test]
fn replay_rejects_malformed_v6_pad_axis_records() {
    let dir = tempfile::tempdir().expect("replay-log dir");
    let log = dir.path().join("malformed-pad-v6.replay");
    let text = "STONE_REPLAY 6\nseed 42\nwindow 0x1p9 0x1p9\ngame bitgun\n"
        .to_string()
        + "t 0x0p+0 0x0p+0 0x0p+0 0 0 - - a leftx=not-a-number\n";
    std::fs::write(&log, text).expect("write malformed v6 log");

    let out = run_frame_raw("bitgun", 1, &["--replay", &log.to_string_lossy()]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "malformed v6 axis must refuse the log\nstderr:\n{stderr}");
    assert!(
        stderr.contains("malformed replay records"),
        "malformed v6 axis must use the replay load failure path\nstderr:\n{stderr}"
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

#[test]
fn replay_rejects_malformed_v5_environment_records() {
    let cases = [
        ("missing payload", "env STONE_QUIET 1"),
        ("extra field", "env STONE_QUIET 1 31 extra"),
        ("invalid presence", "env STONE_QUIET 2 31"),
        ("odd hex", "env STONE_QUIET 1 3"),
        ("non-hex payload", "env STONE_QUIET 1 gg"),
        ("value on absent record", "env STONE_QUIET 0 31"),
    ];

    for (label, env_line) in cases {
        let dir = tempfile::tempdir().expect("dir");
        let log = dir.path().join("malformed-env.replay");
        let text = format!(
            "STONE_REPLAY 5\nseed 42\nwindow 0x1p9 0x1p9\ngame bitgun\nbuild dev\ncheckpoint 60\n{env_line}\nt 0x0p+0 0x0p+0 0x0p+0 0 0 - -\n"
        );
        std::fs::write(&log, text).expect("write malformed env log");

        let out = run_frame_raw("bitgun", 1, &["--replay", &log.to_string_lossy()]);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{label} must refuse the log\nstderr:\n{stderr}");
        assert!(
            stderr.contains("malformed replay records"),
            "{label} must use the replay load failure path\nstderr:\n{stderr}"
        );
    }
}

#[test]
fn replay_preserves_v1_to_v5_load_compatibility() {
    for version in 1..=5 {
        let dir = tempfile::tempdir().expect("dir");
        let log = dir.path().join(format!("v{version}.replay"));
        let text = if version == 5 {
            format!(
                "STONE_REPLAY {version}\nseed 42\nwindow 0x1p9 0x1p9\ngame bitgun\nbuild dev\ncheckpoint 60\nenv STONE_QUIET 1 31\nt 0x0p+0 0x0p+0 0x0p+0 0 0 - -\n"
            )
        } else {
            format!(
                "STONE_REPLAY {version}\nseed 42\nwindow 0x1p9 0x1p9\ngame bitgun\nt 0x0p+0 0x0p+0 0x0p+0 0 0 - -\n"
            )
        };
        std::fs::write(&log, text).expect("write legacy log");

        let out = run_frame_raw("bitgun", 1, &["--replay", &log.to_string_lossy()]);
        assert!(
            out.status.success(),
            "v{version} replay must remain loadable\nstderr:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
