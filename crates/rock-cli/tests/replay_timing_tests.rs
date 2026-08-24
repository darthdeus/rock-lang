//! Diagnostic replay timing is observational metadata. These fixtures keep
//! the timing values exact where format/alignment is under test, and compare
//! authoritative state separately from the wall-clock diagnostics.

use std::path::Path;
use std::process::{Command, Output};

fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
}

fn record_raw(game: &str, frames: u32, log: &str, env: &[(&str, &str)]) -> Output {
    let scratch = tempfile::tempdir().expect("record scratch");
    Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame", game, "-n", &frames.to_string(), "--record", log,
            "--output", &scratch.path().to_string_lossy(),
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .envs(env.iter().copied())
        .current_dir(workspace_root())
        .output()
        .expect("spawn record")
}

fn replay_raw(game: &str, frames: u32, log: &str, env: &[(&str, &str)]) -> Output {
    let scratch = tempfile::tempdir().expect("replay scratch");
    Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame", game, "-n", &frames.to_string(), "--replay", log,
            "--output", &scratch.path().to_string_lossy(),
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .envs(env.iter().copied())
        .current_dir(workspace_root())
        .output()
        .expect("spawn replay")
}

fn replay_dump(game: &str, frames: u32, log: &str, env: &[(&str, &str)]) -> serde_json::Value {
    let scratch = tempfile::tempdir().expect("replay scratch");
    let out = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame", game, "-n", &frames.to_string(), "--replay", log,
            "--dump-state", "--output", &scratch.path().to_string_lossy(),
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .envs(env.iter().copied())
        .current_dir(workspace_root())
        .output()
        .expect("spawn replay dump");
    assert!(
        out.status.success(),
        "replay dump failed: {:?}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap_or_else(|error| {
        panic!(
            "replay dump was not JSON ({error})\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

fn exact_samples(n: usize, first: u64) -> String {
    (0..n)
        .map(|i| (first + i as u64).to_string())
        .collect::<Vec<_>>()
        .join(",")
}

fn timing_lines(text: &str) -> Vec<(usize, usize, u64)> {
    text.lines()
        .filter_map(|line| {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            if fields.first().copied() != Some("p") {
                return None;
            }
            Some((
                fields[1].parse().expect("timing ordinal"),
                fields[2].parse().expect("timing tick"),
                fields[3].parse().expect("timing microseconds"),
            ))
        })
        .collect()
}

fn tick_count(text: &str) -> usize {
    text.lines().filter(|line| line.starts_with("t ")).count()
}

fn assert_load_rejected(out: &Output, label: &str) {
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{label} must be rejected\n{stderr}");
    assert!(
        stderr.contains("malformed replay records") && stderr.contains("timing"),
        "{label} must use the timing-parser rejection path\n{stderr}"
    );
}

fn strip_observational_dump(v: &mut serde_json::Value) {
    if let Some(root) = v.as_object_mut() {
        root.remove("__stone_diag");
    }
    if let Some(stone) = v.get_mut("stone").and_then(serde_json::Value::as_object_mut) {
        stone.remove("PROFILER");
    }
}

#[test]
fn timing_samples_align_exactly_with_observation_ordinals_and_ticks() {
    let dir = tempfile::tempdir().expect("log dir");
    let log = dir.path().join("aligned.replay");
    let log_s = log.to_string_lossy().into_owned();
    let samples = exact_samples(8, 101);
    let recorded = record_raw(
        "bitgun",
        8,
        &log_s,
        &[("STONE_REPLAY_TIMING_US", &samples)],
    );
    assert!(
        recorded.status.success(),
        "record failed: {}",
        String::from_utf8_lossy(&recorded.stderr)
    );

    let text = std::fs::read_to_string(&log).expect("read replay");
    assert!(text.starts_with("STONE_REPLAY 9\n"), "new timing logs must use v9\n{text}");
    assert_eq!(tick_count(&text), 8, "fixture must have eight observations");
    assert_eq!(
        timing_lines(&text),
        (0..8).map(|i| (i, i, 101 + i as u64)).collect::<Vec<_>>(),
        "each exact sample must retain its observation ordinal and tick"
    );
}

#[test]
fn real_recording_produces_a_nonzero_timing_population() {
    let dir = tempfile::tempdir().expect("log dir");
    let log = dir.path().join("measured.replay");
    let log_s = log.to_string_lossy().into_owned();
    let recorded = record_raw("bitgun", 8, &log_s, &[]);
    assert!(recorded.status.success(), "record failed");
    let text = std::fs::read_to_string(&log).expect("read replay");
    let timings = timing_lines(&text);
    assert!(!timings.is_empty(), "the product recorder must emit measured samples");
    assert!(timings.iter().all(|(_, tick, _)| *tick < tick_count(&text)));
}

#[test]
fn crash_tail_can_omit_only_the_final_completed_timing_sample() {
    let dir = tempfile::tempdir().expect("log dir");
    let log = dir.path().join("crash-tail.replay");
    let log_s = log.to_string_lossy().into_owned();
    let samples = exact_samples(20, 200);
    let crashed = record_raw(
        "bitgun",
        20,
        &log_s,
        &[
            ("STONE_REPLAY_TIMING_US", &samples),
            ("STONE_CRASH_AT_FRAME", "8"),
        ],
    );
    assert!(!crashed.status.success(), "crash fixture must exit nonzero");

    let text = std::fs::read_to_string(&log).expect("crash log");
    let ticks = tick_count(&text);
    let timings = timing_lines(&text);
    assert!(text.starts_with("STONE_REPLAY 9\n"), "crash log must retain v9\n{text}");
    assert!(ticks > 0, "crash log must retain observations");
    assert_eq!(
        timings.len() + 1,
        ticks,
        "only the crash-truncated final observation may lack timing"
    );
    assert_eq!(timings.last().map(|row| row.0), Some(ticks - 2));
    assert!(
        replay_raw("bitgun", 20, &log_s, &[]).status.success(),
        "a crash-tail observation without timing must remain playable"
    );
}

#[test]
fn legacy_v8_without_timing_remains_playable_and_reports_an_empty_track() {
    let dir = tempfile::tempdir().expect("log dir");
    let log = dir.path().join("legacy.replay");
    let log_s = log.to_string_lossy().into_owned();
    let recorded = record_raw("bitgun", 8, &log_s, &[]);
    assert!(recorded.status.success(), "record failed");
    let legacy = std::fs::read_to_string(&log)
        .expect("read replay")
        .lines()
        .filter(|line| !line.starts_with("p "))
        .map(str::to_string)
        .collect::<Vec<_>>()
        .join("\n")
        .replacen("STONE_REPLAY 9", "STONE_REPLAY 8", 1)
        + "\n";
    std::fs::write(&log, legacy).expect("write legacy replay");

    let state = replay_dump("bitgun", 8, &log_s, &[]);
    assert_eq!(
        state["__stone_diag"]["STONE_REPLAY_RECORDED_TIMING_COUNT"],
        0,
        "legacy playback must expose an empty recorded timing track"
    );
}

#[test]
fn malformed_duplicate_and_orphan_timing_records_are_rejected() {
    let dir = tempfile::tempdir().expect("log dir");
    let base = dir.path().join("base.replay");
    let base_s = base.to_string_lossy().into_owned();
    let samples = exact_samples(8, 300);
    let recorded = record_raw(
        "bitgun",
        8,
        &base_s,
        &[("STONE_REPLAY_TIMING_US", &samples)],
    );
    assert!(recorded.status.success(), "record failed");
    let source = std::fs::read_to_string(&base).expect("read base replay");
    let first_timing = source
        .lines()
        .find(|line| line.starts_with("p "))
        .expect("timing population");

    let cases = [
        ("duplicate", format!("{source}\n{first_timing}\n")),
        (
            "orphan",
            source
                .lines()
                .chain(std::iter::once("p 99 99 1"))
                .collect::<Vec<_>>()
                .join("\n")
                + "\n",
        ),
        (
            "negative",
            source.replacen(first_timing, "p 0 0 -1", 1),
        ),
        (
            "non-finite",
            source.replacen(first_timing, "p 0 0 nan", 1),
        ),
        (
            "overflow",
            source.replacen(first_timing, "p 0 0 999999999999999999999999999999999999", 1),
        ),
    ];
    for (name, text) in cases {
        let path = dir.path().join(format!("{name}.replay"));
        std::fs::write(&path, text).expect("write malformed replay");
        let path_s = path.to_string_lossy().into_owned();
        assert_load_rejected(
            &replay_raw("bitgun", 8, &path_s, &[]),
            name,
        );
    }
}

#[test]
fn recorded_and_playback_tracks_are_distinct_and_timing_cannot_change_state() {
    let dir = tempfile::tempdir().expect("log dir");
    let original = dir.path().join("original.replay");
    let changed = dir.path().join("changed.replay");
    let original_s = original.to_string_lossy().into_owned();
    let changed_s = changed.to_string_lossy().into_owned();
    let samples = exact_samples(12, 1000);
    let recorded = record_raw(
        "bitgun",
        12,
        &original_s,
        &[("STONE_REPLAY_TIMING_US", &samples)],
    );
    assert!(recorded.status.success(), "record failed");
    let original_bytes = std::fs::read(&original).expect("original replay bytes");
    let changed_text = std::fs::read_to_string(&original)
        .expect("original replay")
        .lines()
        .map(|line| {
            if let Some(rest) = line.strip_prefix("p ") {
                let fields = rest.split_whitespace().collect::<Vec<_>>();
                format!("p {} {} 9000000", fields[0], fields[1])
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(&changed, changed_text).expect("changed replay");

    let mut state_a = replay_dump("bitgun", 12, &original_s, &[]);
    let mut state_b = replay_dump("bitgun", 12, &changed_s, &[]);
    assert_eq!(
        state_a["__stone_diag"]["STONE_REPLAY_RECORDED_TIMING_COUNT"],
        12
    );
    assert_eq!(
        state_b["__stone_diag"]["STONE_REPLAY_RECORDED_TIMING_COUNT"],
        12
    );
    assert!(
        state_a["__stone_diag"]["STONE_REPLAY_PLAYBACK_TIMING_COUNT"]
            .as_i64()
            .is_some_and(|n| n > 0),
        "ordinary playback must populate its own timing track: {}",
        state_a["__stone_diag"]
    );
    assert!(
        state_a["__stone_diag"].get("STONE_REPLAY_RECORDED_TIMING_LAST_US")
            != state_a["__stone_diag"].get("STONE_REPLAY_PLAYBACK_TIMING_LAST_US"),
        "recorded and playback values must have separate observability fields"
    );
    strip_observational_dump(&mut state_a);
    strip_observational_dump(&mut state_b);
    assert_eq!(
        state_a, state_b,
        "radically changing timing metadata must not change authoritative playback state"
    );
    assert_eq!(
        original_bytes,
        std::fs::read(&original).expect("source replay after playback"),
        "playback must not rewrite its source replay"
    );
}

#[test]
fn timeline_populates_timing_and_backward_then_forward_scrub_keeps_alignment() {
    let dir = tempfile::tempdir().expect("log dir");
    let log = dir.path().join("timeline.replay");
    let log_s = log.to_string_lossy().into_owned();
    let samples = exact_samples(40, 5000);
    let recorded = record_raw(
        "bitgun",
        40,
        &log_s,
        &[("STONE_REPLAY_TIMING_US", &samples)],
    );
    assert!(recorded.status.success(), "record failed");

    let state = replay_dump(
        "bitgun",
        80,
        &log_s,
        &[
            ("STONE_REPLAY_WINDOW", "1"),
            ("STONE_KEYFRAME_INTERVAL", "5"),
            ("STONE_TIMELINE_SEEK_AT_FRAME", "20"),
            ("STONE_TIMELINE_SEEK_TO_TICK", "5"),
            ("STONE_TIMELINE_SEEK_AT_FRAME_2", "40"),
            ("STONE_TIMELINE_SEEK_TO_TICK_2", "12"),
        ],
    );
    let diag = &state["__stone_diag"];
    assert!(
        diag["REPLAY_TIMELINE_TIMING_POPULATION"]
            .as_i64()
            .is_some_and(|n| n > 0),
        "the existing timeline must expose a nonzero timing population: {diag}"
    );
    assert_eq!(
        diag["REPLAY_TIMELINE_TIMING_RECORDED_US"],
        5000 + 12,
        "after backward then forward scrub, the strip must remain aligned to tick 12"
    );
    assert_eq!(
        diag["REPLAY_TIMELINE_TIMING_TICK"],
        12,
        "the displayed timing sample must name its observation tick"
    );
}

#[test]
fn resume_live_here_drops_abandoned_timing_and_appends_fresh_branch_samples() {
    let dir = tempfile::tempdir().expect("log dir");
    let log = dir.path().join("branch-timing.replay");
    let log_s = log.to_string_lossy().into_owned();
    let samples = (0..160)
        .map(|i| if i < 20 { (100 + i).to_string() } else { (9000 + i).to_string() })
        .collect::<Vec<_>>()
        .join(",");
    let live = record_raw(
        "bitgun",
        120,
        &log_s,
        &[
            ("STONE_REPLAY_TIMING_US", &samples),
            ("STONE_LIVE_REWIND", "1"),
            ("STONE_TIMELINE_SEEK_AT_FRAME", "20"),
            ("STONE_TIMELINE_SEEK_TO_TICK", "10"),
            ("STONE_BRANCH_AT_FRAME", "40"),
        ],
    );
    assert!(live.status.success(), "branch record failed: {}", String::from_utf8_lossy(&live.stderr));
    let text = std::fs::read_to_string(&log).expect("branched replay");
    let timings = timing_lines(&text);
    assert!(
        timings.iter().any(|(_, tick, us)| *tick >= 10 && *us >= 9000),
        "new branch frames must record fresh timing: {timings:?}"
    );
    assert!(
        timings.iter().all(|(_, tick, us)| !(*tick >= 10 && *tick < 20 && *us < 9000)),
        "abandoned future timing must be discarded rather than retained: {timings:?}"
    );
}

#[test]
fn save_repro_preserves_timing_and_hidden_resimulation_has_no_playback_samples() {
    let dir = tempfile::tempdir().expect("log dir");
    let source = dir.path().join("source.replay");
    let repro = dir.path().join("repro.replay");
    let source_s = source.to_string_lossy().into_owned();
    let repro_s = repro.to_string_lossy().into_owned();
    let samples = exact_samples(30, 12000);
    let recorded = record_raw(
        "bitgun",
        30,
        &source_s,
        &[
            ("STONE_REPLAY_TIMING_US", &samples),
            ("STONE_SAVE_REPRO_AT_FRAME", "10"),
            ("STONE_SAVE_REPRO_PATH", &repro_s),
        ],
    );
    assert!(recorded.status.success(), "record failed: {}", String::from_utf8_lossy(&recorded.stderr));
    let repro_text = std::fs::read_to_string(&repro).expect("saved repro");
    assert!(
        timing_lines(&repro_text).len() > 0,
        "save repro must retain the timing population already completed"
    );
    assert!(
        timing_lines(&repro_text).len() == tick_count(&repro_text),
        "save repro must flush the pending sample before copying"
    );

    let seeked = replay_dump(
        "bitgun",
        40,
        &source_s,
        &[("STONE_RUN_TO_TICK", "25")],
    );
    let seek_diag = &seeked["__stone_diag"];
    assert_eq!(
        seek_diag["STONE_REPLAY_PLAYBACK_TIMING_COUNT"].as_i64().unwrap_or(0),
        0,
        "hidden resimulation must not be reported as ordinary playback-frame timing"
    );
    assert!(
        seek_diag["STONE_REPLAY_RECORDED_TIMING_COUNT"].as_i64().is_some_and(|n| n > 0),
        "hidden resimulation must still have recorded timing available for inspection"
    );
}
