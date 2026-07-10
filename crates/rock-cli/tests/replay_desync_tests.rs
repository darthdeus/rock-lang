//! P2 of docs/replay-scrub-plan.md: the desync detector. While recording,
//! every K ticks the engine hashes each registered global (canonical %a
//! serialization) and writes per-global `c <tick> <name> <hash>` checkpoint
//! lines into the replay log. On replay it recomputes and compares; the FIRST
//! divergent tick + which global diverged is reported and the run exits
//! nonzero (reject-don't-fall-back, same policy as the log-load errors).
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

/// Run `rock frame <game> -n N [extra…]` without asserting success; returns
/// the raw Output so tests can inspect exit status + stderr.
fn run_frame_raw(game: &str, frames: u32, extra: &[&str]) -> std::process::Output {
    let rock_bin = env!("CARGO_BIN_EXE_rock");
    let scratch = tempfile::tempdir().expect("scratch dir");
    let scratch_path = scratch.path().to_string_lossy().into_owned();
    let frames_s = frames.to_string();
    let mut args: Vec<&str> = vec!["frame", game, "-n", &frames_s, "--output", &scratch_path];
    args.extend_from_slice(extra);
    Command::new(rock_bin)
        .args(&args)
        // The default 3s Stone watchdog is too tight for a HEAVY game (mag)
        // under nextest's parallel process load: init + cargo-lock waits can
        // eat >3s before a single frame runs, so the watchdog kills the record
        // with a header-only log and no `c ` lines (a false failure, not a
        // divergence). This was the note-6 flake; the game records fine given
        // wall-clock. 60s absorbs the contention without hiding a real hang.
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(workspace_root())
        .output()
        .expect("failed to spawn rock frame")
}

fn assert_ok(out: &std::process::Output, ctx: &str) {
    assert!(
        out.status.success(),
        "{ctx}: expected success, got {:?}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Clean roundtrip with checkpoints enabled: record bitgun with a checkpoint
/// cadence that actually fires within the run, replay, expect no divergence
/// and a clean exit.
#[test]
fn clean_roundtrip_with_checkpoints_reports_no_divergence() {
    let dir = tempfile::tempdir().expect("dir");
    let log = dir.path().join("ck.replay");
    let log_s = log.to_string_lossy().into_owned();

    let rec = run_frame_raw("bitgun", 30, &["--record", &log_s, "--checkpoint-interval", "10"]);
    assert_ok(&rec, "record");

    // The log must actually contain checkpoint lines, else this proves nothing.
    let text = std::fs::read_to_string(&log).expect("read log");
    assert!(
        text.lines().any(|l| l.starts_with("c ")),
        "record must emit checkpoint lines at K=10 over 30 frames; log:\n{text}"
    );

    let rep = run_frame_raw("bitgun", 30, &["--replay", &log_s]);
    assert_ok(&rep, "replay");
    assert!(
        !String::from_utf8_lossy(&rep.stderr).contains("desync"),
        "clean replay must report no desync\nstderr:\n{}",
        String::from_utf8_lossy(&rep.stderr)
    );
}

/// Injected divergence: record, flip one recorded checkpoint hash in the log,
/// replay. The recomputed hash won't match the tampered one, so replay must
/// exit nonzero and name the tick + global.
#[test]
fn injected_checkpoint_divergence_is_detected_and_named() {
    let dir = tempfile::tempdir().expect("dir");
    let log = dir.path().join("inj.replay");
    let log_s = log.to_string_lossy().into_owned();

    let rec = run_frame_raw("bitgun", 30, &["--record", &log_s, "--checkpoint-interval", "10"]);
    assert_ok(&rec, "record");

    // Corrupt the first checkpoint line's hash. Line: `c <tick> <name> <hash>`.
    let text = std::fs::read_to_string(&log).expect("read log");
    let mut lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();
    let ci = lines
        .iter()
        .position(|l| l.starts_with("c "))
        .expect("a checkpoint line");
    let fields: Vec<&str> = lines[ci].split(' ').collect();
    let (tick, name) = (fields[1].to_string(), fields[2].to_string());
    lines[ci] = format!("c {tick} {name} deadbeefdeadbeef");
    std::fs::write(&log, lines.join("\n") + "\n").expect("rewrite log");

    let rep = run_frame_raw("bitgun", 30, &["--replay", &log_s]);
    assert!(
        !rep.status.success(),
        "an injected checkpoint mismatch must abort the replay\nstderr:\n{}",
        String::from_utf8_lossy(&rep.stderr)
    );
    let stderr = String::from_utf8_lossy(&rep.stderr);
    assert!(
        stderr.contains("desync") && stderr.contains(&name) && stderr.contains(&format!("tick={tick}")),
        "desync report must name the tick ({tick}) and global ({name})\nstderr:\n{stderr}"
    );
}

/// Regression (P3.5): checkpoints x fast-forward must NOT false-positive. A
/// seek (`STONE_RUN_TO_TICK`) skips the render pass on the intermediate ticks,
/// so render-derived globals differ from a rendered run. Two categories, two
/// mechanisms (both exercised here):
///   - engine render/accumulative globals (`RENDER_BATCHES`, `stats_history_*`)
///     and game render handles (Materials, `RetainedMesh`) are excluded from
///     hashing entirely by the sim-state predicate -- they would stay divergent
///     even AFTER the seek;
///   - game scalars written in @draw systems (mag's `MAG_LIGHTS_ADDED`) are
///     transiently stale only DURING the seek and self-heal when render resumes,
///     so the checkpoint compare is skipped while actively seeking.
/// bitgun exercises the first, mag the second.
fn assert_seek_across_checkpoints_clean(game: &str) {
    let dir = tempfile::tempdir().expect("dir");
    let log = dir.path().join("seek_ck.replay");
    let log_s = log.to_string_lossy().into_owned();

    let rec = run_frame_raw(game, 30, &["--record", &log_s, "--checkpoint-interval", "5"]);
    assert_ok(&rec, "record");
    let text = std::fs::read_to_string(&log).expect("read log");
    let t_lines = text.lines().filter(|l| l.starts_with("t ")).count();
    assert!(
        text.lines().any(|l| l.starts_with("c ")),
        "[{game}] record must emit checkpoint lines at K=5 over 30 frames \
         (got {t_lines} tick lines). A header-only log means the Stone \
         watchdog killed the run before any frame -- bump ROCK_FRAME_TIMEOUT_SECS.\
         \nlog:\n{text}"
    );

    // Fast-forward across several checkpoint ticks (5, 10, 15, 20, 25). The
    // render-skipped seek must reach the same SIMULATION state the recording
    // hashed, so no divergence fires.
    let scratch = tempfile::tempdir().expect("scratch");
    let rep = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args(["frame", game, "-n", "30", "--replay", &log_s,
               "--output", &scratch.path().to_string_lossy()])
        .env("STONE_RUN_TO_TICK", "25")
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(workspace_root())
        .output()
        .expect("spawn seek replay");
    assert!(
        rep.status.success(),
        "[{game}] seek across checkpoint ticks must not false-positive\nstderr:\n{}",
        String::from_utf8_lossy(&rep.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&rep.stderr).contains("desync"),
        "[{game}] seek across checkpoints must report no desync\nstderr:\n{}",
        String::from_utf8_lossy(&rep.stderr)
    );
}

#[test]
fn seek_across_checkpoints_does_not_false_positive_bitgun() {
    assert_seek_across_checkpoints_clean("bitgun");
}

/// mag has a game global written in a @draw system (`MAG_LIGHTS_ADDED`) plus a
/// `RetainedMesh` global (`AO_MESH`); this locks both the render-handle __ty
/// exclusion and the skip-compare-while-seeking path.
#[test]
fn seek_across_checkpoints_does_not_false_positive_mag() {
    assert_seek_across_checkpoints_clean("mag");
}

/// False-positive guard: hash EVERY tick (K=1) over 30 frames and replay
/// clean. Proves the hashed state has no per-tick nondeterministic input —
/// if any hashed global reads the wall clock or depends on Map iteration
/// order, this fails and names it.
#[test]
fn per_tick_checkpoints_have_no_false_positives() {
    let dir = tempfile::tempdir().expect("dir");
    let log = dir.path().join("k1.replay");
    let log_s = log.to_string_lossy().into_owned();

    let rec = run_frame_raw("bitgun", 30, &["--record", &log_s, "--checkpoint-interval", "1"]);
    assert_ok(&rec, "record");

    let rep = run_frame_raw("bitgun", 30, &["--replay", &log_s]);
    assert_ok(&rep, "replay (K=1 clean)");
    assert!(
        !String::from_utf8_lossy(&rep.stderr).contains("desync"),
        "per-tick replay must be divergence-free\nstderr:\n{}",
        String::from_utf8_lossy(&rep.stderr)
    );
}
