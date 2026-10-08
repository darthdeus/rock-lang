//! P1 of docs/replay-scrub-plan.md: record a headless run, replay it, and
//! assert the replayed `--dump-state` reproduces the recorded run's game +
//! engine state exactly.
//!
//! Determinism scope: the plan's contract carves out wall-clock DIAGNOSTICS
//! (`stone_get_time()` "is fine in diagnostics") from the determinism
//! guarantee. Those live under `stone.PROFILER` and its frame series
//! (`FRAME_*`, `PROFILE_HEAP_KB`) and are inherently non-reproducible; the
//! dump names them in `__stone_nonreproducible`. We strip those and assert
//! byte-identity of everything else. This matches the repo's existing dump
//! tests (game_snapshot_tests), which all compare curated game-state subtrees
//! and never the profiler. Everything else is dt-derived and replay
//! reproduces it from the logged dt.
//!
//! Heavy: spawns `rock frame` against a compiled game. Excluded from the
//! default nextest profile (see `.config/nextest.toml`); runs under `-P full`.

use std::path::Path;
use std::process::Command;
use crate::stone_harness::{HeavyCommandExt, SpawnBudget};

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
        "frame", game, "-n", &frames_s, "--headless", "--dump-state", "--output", &scratch_path,
    ];
    args.extend_from_slice(extra);

    let output = Command::new(rock_bin)
        .args(&args)
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(workspace_root())
        .output_with_deadline()
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
        .output_with_deadline()
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
        .output_with_deadline()
        .expect("failed to spawn rock frame")
}

/// Like `run_frame` but with environment variables set on the child.
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

    let output = Command::new(rock_bin)
        .args(&args)
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .envs(env.iter().copied())
        .current_dir(workspace_root())
        .output_with_deadline()
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

/// Remove the wall-clock diagnostics carve-out (see module docs). This is the
/// BYTE-IDENTITY contract: record vs a RENDERED replay must match on everything
/// except non-reproducible wall-clock diagnostics (PROFILER), INCLUDING all
/// render state -- so this is deliberately NOT the sim-state predicate (which
/// the render-skipped seek test uses). Both runs render here, so render state
/// is identical and must be compared.
fn strip_diagnostics(v: &mut serde_json::Value) {
    crate::stone_harness::strip_nonreproducible(v);
}

/// Piece 2 of P3: a window resize during recording is captured as an `r`
/// line and re-applied at the matching tick on replay (so screen_to_world
/// reproduces). Headless never resizes, so we inject a resize line into a
/// recorded log and assert replay adopts it. `tonumber` parses the decimal
/// coords fine (records write %a hex; both round-trip).
#[test]
fn replay_applies_recorded_window_resize() {
    let _spawn_budget = SpawnBudget::for_test(120);
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
    let _spawn_budget = SpawnBudget::for_test(120);
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
    let _spawn_budget = SpawnBudget::for_test(120);
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
    let _spawn_budget = SpawnBudget::for_test(120);
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
    let _spawn_budget = SpawnBudget::for_test(120);
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
    let _spawn_budget = SpawnBudget::for_test(120);
    let dir = tempfile::tempdir().expect("replay-log dir");
    let log = dir.path().join("v9-renderer.replay");
    let log_s = log.to_string_lossy().into_owned();

    let _ = run_frame("bitgun", 2, &["--record", &log_s]);
    let text = std::fs::read_to_string(&log).expect("read v9 record");
    assert!(text.starts_with("STONE_REPLAY 17\n"), "recording must use the v17 header: {text}");
    let tick = text.lines().find(|line| line.starts_with("t ")).expect("tick");
    assert_eq!(tick.split(' ').count(), 10, "v9 tick has nine payload fields");
    assert!(tick.ends_with(" - -"), "empty live pad state has explicit empty fields: {tick}");
}

#[test]
fn replay_rejects_malformed_v6_pad_axis_records() {
    let _spawn_budget = SpawnBudget::for_test(120);
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

/// Job-publication rows are ordered observations, not optional diagnostics: a
/// replay that never consumes one must stop at that tick instead of silently
/// accepting a different scheduler path.
#[test]
fn replay_rejects_unconsumed_job_decision_records() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let dir = tempfile::tempdir().expect("dir");
    let log = dir.path().join("unconsumed-job-decision.replay");
    let log_s = log.to_string_lossy().into_owned();
    let _ = run_frame("bitgun", 2, &["--record", &log_s]);

    let text = std::fs::read_to_string(&log).expect("read replay log");
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let tick = lines
        .iter()
        .position(|line| line.starts_with("t "))
        .expect("a tick line");
    lines.insert(tick + 1, "j 0 flow 1 0".to_string());
    std::fs::write(&log, lines.join("\n") + "\n").expect("rewrite replay log");

    let out = run_frame_raw("bitgun", 2, &["--replay", &log_s]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "an unconsumed job decision must abort replay\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("job decision count mismatch") && stderr.contains("tick 0"),
        "the rejection must name the observation and tick\nstderr:\n{stderr}"
    );
}

fn write_job_decision_fixture() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().expect("fixture dir");
    let repo = workspace_root()
        .parent()
        .expect("repo root");
    std::fs::write(
        dir.path().join("stick.toml"),
        format!(
            "[package]\nname = \"replay_job_fixture\"\nversion = \"0.1.0\"\ntype = \"bin\"\n\n[dependencies]\nstone = {{ path = \"{}\" }}\n\n[build.vars]\nGIT_SHA = \"fixture\"\n",
            repo.join("stone").display()
        ),
    )
    .expect("write fixture stick.toml");
    std::fs::write(
        dir.path().join("main.rock"),
        r#"
global FLOW_CALLS: int = 0
global FLOW_ACCEPTED: int = 0

@update
fn replay_job_probe() {
    if FLOW_CALLS >= 8 { return }
    FLOW_CALLS = FLOW_CALLS + 1
    if replay_job_ready(current_tick, "fixture", current_tick + 1, os_getenv("FLOW_TEST_READY") == "1") {
        FLOW_ACCEPTED = FLOW_ACCEPTED + 1
    }
    if FLOW_CALLS == 8 {
        stone_diag_set("FLOW_TEST_CALLS", FLOW_CALLS)
        stone_diag_set("FLOW_TEST_ACCEPTED", FLOW_ACCEPTED)
        if FLOW_ACCEPTED != 8 { error("flow fixture accepted fewer than 8 decisions") }
        stone_quit()
    }
}

fn main() {
    stone_run_game(GameConfig { title: "replay job fixture", width: 64, height: 64 })
}
"#,
    )
    .expect("write fixture main.rock");
    let project = dir.path().to_string_lossy().into_owned();
    (dir, project)
}

#[test]
fn replay_roundtrips_produced_job_decision_rows() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let (_fixture, project) = write_job_decision_fixture();
    let dir = tempfile::tempdir().expect("dir");
    let log = dir.path().join("job-decisions.replay");
    let log_s = log.to_string_lossy().into_owned();

    let recorded = run_frame_env(
        &project,
        20,
        &["--record", &log_s],
        &[("FLOW_TEST_READY", "1")],
    );
    assert_eq!(recorded["__stone_diag"]["FLOW_TEST_CALLS"], serde_json::json!(8));
    assert_eq!(
        recorded["__stone_diag"]["FLOW_TEST_ACCEPTED"],
        serde_json::json!(8)
    );

    let text = std::fs::read_to_string(&log).expect("read recorded replay log");
    let rows: Vec<Vec<&str>> = text
        .lines()
        .filter(|line| line.starts_with("j "))
        .map(|line| line.split_whitespace().collect())
        .collect();
    assert_eq!(rows.len(), 8, "recording must contain one j row per update: {text}");
    for row in &rows {
        assert_eq!(row.len(), 5, "job row must have five fields: {row:?}");
        assert_eq!(row[2], "fixture", "fixture kind must survive recording: {row:?}");
        assert!(
            row[3].parse::<u64>().expect("positive job key") > 0,
            "job key must be positive: {row:?}"
        );
        assert_eq!(row[4], "1", "recorded readiness must be true: {row:?}");
    }

    let replayed = run_frame_env(
        &project,
        20,
        &["--replay", &log_s],
        &[("FLOW_TEST_READY", "0")],
    );
    assert_eq!(replayed["__stone_diag"]["FLOW_TEST_CALLS"], serde_json::json!(8));
    assert_eq!(
        replayed["__stone_diag"]["FLOW_TEST_ACCEPTED"],
        serde_json::json!(8),
        "replay must return the recorded ready=1 result even with the live flag disabled"
    );

    let first = text
        .lines()
        .position(|line| line.starts_with("j "))
        .expect("a recorded job row");
    let mut mismatch_lines: Vec<String> = text.lines().map(str::to_string).collect();
    let mut mismatch_fields: Vec<String> = mismatch_lines[first]
        .split_whitespace()
        .map(str::to_string)
        .collect();
    mismatch_fields[3] = "9007199254740991".to_string();
    if mismatch_fields[3] == rows[0][3] {
        mismatch_fields[3] = "9007199254740990".to_string();
    }
    mismatch_lines[first] = mismatch_fields.join(" ");
    let mismatch = dir.path().join("job-decisions-mismatch.replay");
    let mismatch_s = mismatch.to_string_lossy().into_owned();
    std::fs::write(&mismatch, mismatch_lines.join("\n") + "\n").expect("write mismatch log");
    let out = run_frame_raw_env(
        &project,
        20,
        &["--replay", &mismatch_s],
        &[("FLOW_TEST_READY", "0")],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success() && stderr.contains("job decision mismatch"),
        "a changed key must reject replay\nstderr:\n{stderr}"
    );

    let missing = dir.path().join("job-decisions-missing.replay");
    let missing_s = missing.to_string_lossy().into_owned();
    let missing_text = text
        .lines()
        .enumerate()
        .filter_map(|(index, line)| (index != first).then_some(line))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&missing, missing_text + "\n").expect("write missing-row log");
    let out = run_frame_raw_env(
        &project,
        20,
        &["--replay", &missing_s],
        &[("FLOW_TEST_READY", "0")],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success() && stderr.contains("missing job decision"),
        "a missing key row must reject replay\nstderr:\n{stderr}"
    );
}

/// `--replay` with an unopenable log must abort the run, not silently fall
/// back to a live (non-replayed) run whose output looks plausible.
#[test]
fn replay_rejects_missing_log() {
    let _spawn_budget = SpawnBudget::for_test(120);
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
    let _spawn_budget = SpawnBudget::for_test(120);
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
    let _spawn_budget = SpawnBudget::for_test(120);
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
    let _spawn_budget = SpawnBudget::for_test(120);
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

#[test]
fn replay_preserves_a_physical_tap_completed_between_frames() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let (fixture, project) = write_job_decision_fixture();
    std::fs::write(fixture.path().join("main.rock"), r#"
global TAP_OBSERVED: bool = false
global MOUSE_TAP_OBSERVED: bool = false

inline(lua) fn queue_mouse_tap() -> bool = """
    local ffi = require("ffi")
    ffi.cdef("int SDL_PushEvent(void *event);")
    local sdl = ffi.load("SDL2")
    local event = ffi.new("uint32_t[14]")
    event[0] = 0x401
    ffi.cast("uint8_t *", event)[16] = 1
    ffi.cast("uint8_t *", event)[17] = 1
    if sdl.SDL_PushEvent(event) ~= 1 then return false end
    event[0] = 0x402
    ffi.cast("uint8_t *", event)[17] = 0
    if sdl.SDL_PushEvent(event) ~= 1 then return false end
    ffi.fill(event, 56)
    event[0] = 0x300
    ffi.cast("uint8_t *", event)[12] = 1
    ffi.cast("uint8_t *", event)[13] = 1
    event[4] = 4
    event[5] = 113
    ffi.cast("uint16_t *", event)[12] = 1
    return sdl.SDL_PushEvent(event) == 1
"""

@late_update
fn enqueue_mouse_tap() {
    if current_tick == 0 && replay_is_record() {
        assert(queue_mouse_tap(), "enqueue SDL mouse tap")
    }
}

@draw
fn register_button() {
    ui_mouse_item(12345, ui_full_screen())
}

@late_init
fn enqueue_tap() {
    if replay_is_record() {
        assert(stone_test_push_key_event("right", true), "enqueue key down")
        assert(stone_test_push_key_event("right", false), "enqueue key up")
    }
}

@update
fn consume_tap() {
    if is_key_pressed("right") {
        assert(!is_key_down("right"), "tap must finish before input capture")
        assert(is_key_released("right"), "tap must preserve its release")
        assert(is_scancode_pressed("right"), "physical binding must see the tap")
        TAP_OBSERVED = replay_job_ready(current_tick, "tap", 1, true)
    }
    if is_mouse_pressed(MOUSE_BUTTON_LEFT) {
        assert(!is_mouse_down(MOUSE_BUTTON_LEFT), "mouse tap must finish before capture")
        assert(is_mouse_released(MOUSE_BUTTON_LEFT), "mouse tap release")
        assert(UI_MOUSE.clicked == 12345, "UI must receive the recorded press latch")
        MOUSE_TAP_OBSERVED = replay_job_ready(current_tick, "mouse-tap", 1, true)
    }
    if current_tick == 1 {
        assert(is_key_down("q"), "repeat must preserve logical hold")
        assert(!is_key_pressed("q"), "repeat must not become a fresh press")
        assert(is_scancode_down("a"), "physical binding must retain its distinct identity")
        assert(!is_scancode_pressed("a"), "physical repeat must not become a press")
        assert(scancode_repeated("a"), "physical repeat survives replay")
        assert(scancode_modifiers("a") == 2, "physical modifier survives replay")
    }
    if current_tick == 2 {
        assert(MOUSE_TAP_OBSERVED, "recorded mouse tap must reach its consumer")
        assert(TAP_OBSERVED, "recorded tap must reach its consumer")
    }
}

fn main() {
    stone_run_game(GameConfig { title: "replay tap", width: 64, height: 64 })
}
"#).expect("write tap fixture");
    let log = fixture.path().join("tap.replay");
    let recorded = run_frame_raw(&project, 3, &["--record", log.to_str().unwrap()]);
    assert!(recorded.status.success(), "recording failed: {}",
        String::from_utf8_lossy(&recorded.stderr));
    let wire = std::fs::read_to_string(&log).unwrap();
    assert_eq!(wire.lines().filter(|line| line.starts_with("j ")).count(), 2,
        "the taps must reach a publication decision while recording");
    let replayed = run_frame_raw(&project, 3, &["--replay", log.to_str().unwrap()]);
    assert!(replayed.status.success(), "tap playback failed: {}",
        String::from_utf8_lossy(&replayed.stderr));
    let publication = wire.lines().find(|line| line.starts_with("u 0 ")).unwrap();
    for (label, bad) in [
        ("missing", String::new()),
        ("duplicate", format!("{publication}\n{publication}")),
        ("orphan", publication.replacen("u 0", "u 1", 1)),
        ("malformed", "u 0 7b7d".to_string()),
    ] {
        let path = fixture.path().join(format!("image-{label}.replay"));
        std::fs::write(&path, wire.replace(publication, &bad)).unwrap();
        let rejected = run_frame_raw(&project, 3, &["--replay", path.to_str().unwrap()]);
        assert!(!rejected.status.success(), "accepted {label} image journal");
        assert!(String::from_utf8_lossy(&rejected.stderr).contains("malformed replay records"));
    }
    let detail = wire.lines().find(|line| line.starts_with("a 0 ")).unwrap();
    for (name, replacement) in [
        ("missing", String::new()),
        ("duplicate", format!("{detail}\n{detail}")),
        ("orphan", detail.replacen("a 0", "a 1", 1)),
        ("flag", detail.replacen("=1", "=2", 1)),
    ] {
        let bad_log = fixture.path().join(format!("{name}.replay"));
        std::fs::write(&bad_log, wire.replace(detail, &replacement)).unwrap();
        let bad = run_frame_raw(&project, 3, &["--replay", bad_log.to_str().unwrap()]);
        assert!(!bad.status.success() && String::from_utf8_lossy(&bad.stderr)
            .contains("malformed replay records"), "must reject {name}: {}",
            String::from_utf8_lossy(&bad.stderr));
    }
    let output_dir = fixture.path().join("profile");
    let profiled = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args(["replay-profile", &project, log.to_str().unwrap(), "--range", "0-2",
            "--runs", "2", "--output", output_dir.to_str().unwrap()])
        .current_dir(workspace_root()).output_with_deadline().expect("profile taps");
    assert!(profiled.status.success(), "tap profiling failed: {}",
        String::from_utf8_lossy(&profiled.stderr));
    let rows: Vec<serde_json::Value> = String::from_utf8(profiled.stdout).unwrap()
        .lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    assert_eq!(rows.iter().filter(|row| row["kind"] == "frame").count(), 6);
    assert_eq!(rows.iter().filter(|row| row["kind"] == "run"
        && row["status"] == "complete").count(), 2);
}

#[test]
fn mag_replay_profiles_through_admission_after_a_short_splash_tap() {
    let _spawn_budget = SpawnBudget::for_test(120);
    const FRAMES: u32 = 420;
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("mag.replay");
    let settings = dir.path().join("settings.json");
    let progress = dir.path().join("progress.json");
    let bindings = dir.path().join("bindings.json");
    let binding_history = dir.path().join("bindings-history.jsonl");
    let layout_history = dir.path().join("layout-history.jsonl");
    std::fs::write(&settings, r#"{"privacy":{"analytics":1,"analytics_version":3}}"#).unwrap();
    let recorded = run_frame_env("mag", FRAMES, &["--headless", "--record", log.to_str().unwrap(),
        "--checkpoint-interval", "0"], &[
        ("PERF_MODE", "1"), ("STONE_WINDOW", "1280x720"),
        ("MAG_SETTINGS_PATH_OVERRIDE", settings.to_str().unwrap()),
        ("MAG_PROGRESS_PATH_OVERRIDE", progress.to_str().unwrap()),
        ("MAG_BINDINGS_PATH_OVERRIDE", bindings.to_str().unwrap()),
        ("MAG_BINDINGS_HISTORY_PATH_OVERRIDE", binding_history.to_str().unwrap()),
        ("MAG_LAYOUT_HISTORY_PATH_OVERRIDE", layout_history.to_str().unwrap()),
        ("STONE_PHYSICAL_MOUSE_PRESS_AT", "2"),
        ("STONE_PHYSICAL_MOUSE_RELEASE_AT", "2"),
        ("STONE_PHYSICAL_KEY_PRESS_AT", "130"),
        ("STONE_PHYSICAL_KEY_RELEASE_AT", "131"),
        ("STONE_PHYSICAL_KEY", "down"), ("STONE_PHYSICAL_SCANCODE", "down"),
        ("STONE_PHYSICAL_KEY_PRESS_AT_2", "135"),
        ("STONE_PHYSICAL_KEY_RELEASE_AT_2", "136"),
        ("STONE_PHYSICAL_KEY_2", "return"), ("STONE_PHYSICAL_SCANCODE_2", "return"),
        ("STONE_PHYSICAL_MOUSE_PRESS_AT_2", "140"),
        ("STONE_PHYSICAL_MOUSE_RELEASE_AT_2", "141"),
        ("STONE_PHYSICAL_MOUSE_X_2", "640"),
        ("STONE_PHYSICAL_MOUSE_Y_2", "247"),
    ]);
    assert_eq!(recorded["mag"]["MAG_RUN_STARTS"], 1,
        "fixture must enter gameplay: screen={} save_slots_open={} focus_scope={} first_focus_rect={} prep_ticks={} admission_ticks={} play_effects={}",
        recorded["mag"]["SCREEN"], recorded["mag"]["SAVE_SLOTS_OPEN"],
        recorded["stone"]["UI_FOCUS"]["scope"],
        recorded["stone"]["UI_FOCUS"]["cur"]["data"][0]["rect"],
        recorded["mag"]["MAG_RUN_PREPARE_ADVANCE_TICKS"],
        recorded["mag"]["MAG_RUN_ADMISSION_TICKS"], recorded["__stone_diag"]["MAG_PLAY_EFFECT_COUNT"]);
    let wire = std::fs::read_to_string(&log).unwrap();
    assert!(wire.lines().any(|line| line.starts_with("j ") && line.contains(" flow ")),
        "the run must exercise actual flow publication");
    let output_dir = dir.path().join("profile");
    let profile_range = format!("0-{}", FRAMES - 1);
    let output = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args(["replay-profile", "mag", log.to_str().unwrap(), "--range", &profile_range,
            "--runs", "2", "--output", output_dir.to_str().unwrap()])
        .env("PERF_MODE", "1").env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(workspace_root()).output_with_deadline().expect("profile MAG replay");
    assert!(output.status.success(), "MAG profiling failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    let rows: Vec<serde_json::Value> = String::from_utf8(output.stdout).unwrap()
        .lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    assert_eq!(rows.iter().filter(|row| row["kind"] == "frame").count(),
        FRAMES as usize * 2);
    let mut image_work = std::collections::HashMap::new();
    for line in wire.lines().filter(|line| line.starts_with("i ")) {
        let f: Vec<_> = line.split_whitespace().collect();
        image_work.insert(f[1].parse::<u64>().unwrap(), [
            f[3].parse::<u64>().unwrap(), f[7].parse().unwrap(), f[8].parse().unwrap()]);
    }
    assert!(!image_work.is_empty(), "the admission fixture must perform image work");
    for row in rows.iter().filter(|row| row["kind"] == "frame") {
        let tick = row["ordinal"].as_u64().unwrap();
        let actual = &row["playback"];
        assert_eq!([
            actual["image_load_calls"].as_u64().unwrap(),
            actual["image_upload_calls"].as_u64().unwrap(),
            actual["image_upload_bytes"].as_u64().unwrap(),
        ], *image_work.get(&tick).unwrap_or(&[0,0,0]), "image work at tick {tick}");
    }
    let rewound = run_frame_env("mag", FRAMES, &["--headless", "--replay", log.to_str().unwrap()], &[
        ("STONE_KEYFRAME_INTERVAL", "30"),
        ("STONE_TIMELINE_SEEK_AT_FRAME", "150"),
        ("STONE_TIMELINE_SEEK_TO_TICK", "75"),
    ]);
    assert_eq!(rewound["stone"]["current_tick"], 75);
    // Dump-state follows run_shutdown, which clears the admission flags.
    assert!(rewound["__stone_diag"]["STONE_SEEK_RESIM_FRAMES"].as_u64().unwrap() > 0);
    let runs: Vec<_> = rows.iter().filter(|row| row["kind"] == "run").collect();
    assert_eq!(runs.len(), 2);
    for run in runs {
        assert_eq!(run["status"], "complete");
        assert_eq!(run["observed_frames"], FRAMES);
        assert_eq!(run["identity"]["exact_artifact"], true);
    }
}
