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

#[test]
fn replay_playback_cannot_emit_live_telemetry() {
    let repository = workspace_root().parent().expect("repository root");
    let source = std::fs::read_to_string(repository.join("stone/engine/systems/telemetry.rock"))
        .expect("telemetry source");
    let record = source
        .split("fn telemetry_record(")
        .nth(1)
        .expect("telemetry_record")
        .split("inline(lua) fn telemetry_record_native")
        .next()
        .expect("telemetry_record body");
    let native = source
        .split("inline(lua) fn telemetry_record_native")
        .nth(1)
        .expect("native telemetry wrapper")
        .split("inline(lua) fn telemetry_close")
        .next()
        .expect("native telemetry body");

    assert!(
        record.contains("if replay_is_replay() { return true }"),
        "ordinary replay playback still reaches the live telemetry sink"
    );
    assert!(
        !native.contains("__playback_seeking"),
        "the native wrapper retains a second seek-only telemetry policy"
    );

    let mag_policy = std::fs::read_to_string(repository.join("mag/telemetry.rock"))
        .expect("MAG telemetry policy");
    let can_record = mag_policy
        .split("fn mag_telemetry_can_record()")
        .nth(1)
        .expect("mag_telemetry_can_record")
        .split("fn mag_telemetry_apply")
        .next()
        .expect("mag_telemetry_can_record body");
    assert!(
        can_record.contains("&& !replay_is_replay()"),
        "replay playback still enables eager MAG telemetry construction"
    );

    let events = std::fs::read_to_string(repository.join("mag/telemetry_events.rock"))
        .expect("MAG telemetry events");
    let spell_phase = events
        .split("fn telemetry_spell_phase(")
        .nth(1)
        .expect("telemetry_spell_phase")
        .split("fn telemetry_source_cohort_name")
        .next()
        .expect("telemetry_spell_phase body");
    assert!(
        spell_phase
            .find("if !mag_telemetry_can_record() { return }")
            .is_some_and(|guard| guard < spell_phase.find("telemetry_data()").unwrap()),
        "spell telemetry constructs its payload before replay policy can reject it"
    );
}

fn record_raw(game: &str, frames: u32, log: &str, env: &[(&str, &str)]) -> Output {
    let scratch = tempfile::tempdir().expect("record scratch");
    Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame",
            game,
            "-n",
            &frames.to_string(),
            "--record",
            log,
            "--output",
            &scratch.path().to_string_lossy(),
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .envs(env.iter().copied())
        .current_dir(workspace_root())
        .output()
        .expect("spawn record")
}

fn record_dump(game: &str, frames: u32, log: &str, env: &[(&str, &str)]) -> serde_json::Value {
    let scratch = tempfile::tempdir().expect("record scratch");
    let out = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame",
            game,
            "-n",
            &frames.to_string(),
            "--record",
            log,
            "--dump-state",
            "--output",
            &scratch.path().to_string_lossy(),
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .envs(env.iter().copied())
        .current_dir(workspace_root())
        .output()
        .expect("spawn record dump");
    assert!(
        out.status.success(),
        "record dump failed: {:?}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap_or_else(|error| {
        panic!(
            "record dump was not JSON ({error})\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

fn replay_raw(game: &str, frames: u32, log: &str, env: &[(&str, &str)]) -> Output {
    let scratch = tempfile::tempdir().expect("replay scratch");
    Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame",
            game,
            "-n",
            &frames.to_string(),
            "--replay",
            log,
            "--output",
            &scratch.path().to_string_lossy(),
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
            "frame",
            game,
            "-n",
            &frames.to_string(),
            "--replay",
            log,
            "--dump-state",
            "--output",
            &scratch.path().to_string_lossy(),
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

fn exact_phase_samples(n: usize, first: u64) -> String {
    (0..n)
        .map(|i| {
            let base = first + i as u64;
            format!("{}:{}:{}", base, base + 100, base + 200)
        })
        .collect::<Vec<_>>()
        .join(",")
}

#[test]
fn replay_profile_range_emits_every_requested_observation_and_the_final_boundary() {
    let dir = tempfile::tempdir().expect("replay profile range");
    let log = dir.path().join("range.replay");
    let timings = exact_samples(8, 10_000);
    let phases = exact_phase_samples(8, 1_000);
    let recorded = record_raw(
        "bitgun",
        8,
        log.to_str().unwrap(),
        &[
            ("STONE_REPLAY_TIMING_US", timings.as_str()),
            ("STONE_REPLAY_PHASE_TIMING_US", phases.as_str()),
        ],
    );
    assert!(
        recorded.status.success(),
        "record failed: {}",
        String::from_utf8_lossy(&recorded.stderr)
    );

    let playback = replay_raw(
        "bitgun",
        5,
        log.to_str().unwrap(),
        &[
            ("STONE_REPLAY_PROFILE_FROM", "2"),
            ("STONE_REPLAY_PROFILE_TO", "4"),
            ("STONE_REPLAY_PROFILE_WIRE", "1"),
        ],
    );
    assert!(
        playback.status.success(),
        "profile playback failed: {}",
        String::from_utf8_lossy(&playback.stderr)
    );
    let stdout = String::from_utf8_lossy(&playback.stdout);
    let rows = stdout
        .lines()
        .filter(|line| line.starts_with("ROCK_REPLAY_PROFILE_ROW "))
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 3, "requested 2-4 inclusive:\n{stdout}");
    for (row, ordinal) in rows.iter().zip(2..=4) {
        assert!(
            row.starts_with(&format!("ROCK_REPLAY_PROFILE_ROW {ordinal} ")),
            "range rows must be exact and ordered: {rows:?}"
        );
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TimingRow {
    ordinal: usize,
    tick: usize,
    total_us: u64,
    logic_us: u64,
    render_us: u64,
    present_us: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SlowSection {
    name: String,
    inclusive_us: u64,
    calls: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SlowFrameRow {
    ordinal: usize,
    tick: usize,
    heap_bytes: u64,
    heap_growth_bytes: u64,
    gc: bool,
    dropped: u64,
    sections: Vec<SlowSection>,
}

fn decode_hex_string(text: &str) -> String {
    assert!(
        !text.is_empty() && text.len() % 2 == 0,
        "invalid hex string: {text}"
    );
    let bytes = (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex byte"))
        .collect::<Vec<_>>();
    String::from_utf8(bytes).expect("UTF-8 section name")
}

fn slow_frame_lines(text: &str) -> Vec<SlowFrameRow> {
    text.lines()
        .filter_map(|line| {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            if fields.first().copied() != Some("s") {
                return None;
            }
            assert!(fields.len() >= 8, "incomplete slow-frame row: {line}");
            let section_count = fields[7].parse::<usize>().expect("section count");
            assert_eq!(
                fields.len(),
                8 + section_count * 3,
                "slow-frame section population must match its payload: {line}"
            );
            let sections = fields[8..]
                .chunks_exact(3)
                .map(|section| SlowSection {
                    name: decode_hex_string(section[0]),
                    inclusive_us: section[1].parse().expect("section microseconds"),
                    calls: section[2].parse().expect("section calls"),
                })
                .collect();
            Some(SlowFrameRow {
                ordinal: fields[1].parse().expect("slow-frame ordinal"),
                tick: fields[2].parse().expect("slow-frame tick"),
                heap_bytes: fields[3].parse().expect("heap bytes"),
                heap_growth_bytes: fields[4].parse().expect("heap growth bytes"),
                gc: match fields[5] {
                    "0" => false,
                    "1" => true,
                    other => panic!("invalid GC marker: {other}"),
                },
                dropped: fields[6].parse().expect("dropped profiler events"),
                sections,
            })
        })
        .collect()
}

fn timing_lines(text: &str) -> Vec<TimingRow> {
    text.lines()
        .filter_map(|line| {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            if fields.first().copied() != Some("p") {
                return None;
            }
            assert_eq!(
                fields.len(),
                7,
                "v10 timing row must carry the fixed phase tuple: {line}"
            );
            Some(TimingRow {
                ordinal: fields[1].parse().expect("timing ordinal"),
                tick: fields[2].parse().expect("timing tick"),
                total_us: fields[3].parse().expect("total microseconds"),
                logic_us: fields[4].parse().expect("logic microseconds"),
                render_us: fields[5].parse().expect("render microseconds"),
                present_us: fields[6].parse().expect("present microseconds"),
            })
        })
        .collect()
}

fn tick_count(text: &str) -> usize {
    text.lines().filter(|line| line.starts_with("t ")).count()
}

fn image_resource_lines(text: &str) -> Vec<Vec<&str>> {
    text.lines()
        .filter(|line| line.starts_with("i "))
        .map(|line| line.split_whitespace().collect())
        .collect()
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
    if let Some(stone) = v
        .get_mut("stone")
        .and_then(serde_json::Value::as_object_mut)
    {
        stone.remove("PROFILER");
    }
}

#[test]
fn timing_samples_align_exactly_with_observation_ordinals_and_ticks() {
    let dir = tempfile::tempdir().expect("log dir");
    let log = dir.path().join("aligned.replay");
    let log_s = log.to_string_lossy().into_owned();
    let samples = exact_samples(8, 101);
    let phases = exact_phase_samples(8, 11);
    let recorded = record_raw(
        "bitgun",
        8,
        &log_s,
        &[
            ("STONE_REPLAY_TIMING_US", &samples),
            ("STONE_REPLAY_PHASE_TIMING_US", &phases),
        ],
    );
    assert!(
        recorded.status.success(),
        "record failed: {}",
        String::from_utf8_lossy(&recorded.stderr)
    );

    let text = std::fs::read_to_string(&log).expect("read replay");
    assert!(
        text.starts_with(&format!("{}\n", rock_replay_protocol::replay_header())),
        "phase timing logs must use v{}\n{text}", rock_replay_protocol::REPLAY_VERSION
    );
    assert_eq!(tick_count(&text), 8, "fixture must have eight observations");
    assert_eq!(
        timing_lines(&text),
        (0..8)
            .map(|i| TimingRow {
                ordinal: i,
                tick: i,
                total_us: 101 + i as u64,
                logic_us: 11 + i as u64,
                render_us: 111 + i as u64,
                present_us: 211 + i as u64,
            })
            .collect::<Vec<_>>(),
        "each exact sample must retain its observation ordinal, tick, and phase tuple"
    );
}

#[test]
fn a_runtime_image_miss_is_attached_to_its_completed_replay_observation() {
    let dir = tempfile::tempdir().expect("log dir");
    let log = dir.path().join("image-load.replay");
    let log_s = log.to_string_lossy().into_owned();
    let recorded = record_raw(
        "bitgun",
        6,
        &log_s,
        &[
            ("STONE_TEST_IMAGE_LOAD_AT_FRAME", "3"),
            ("STONE_TEST_IMAGE_LOAD_PATH", "assets/Sprites/error.png"),
        ],
    );
    assert!(
        recorded.status.success(),
        "record failed: {}",
        String::from_utf8_lossy(&recorded.stderr)
    );

    let text = std::fs::read_to_string(&log).expect("read replay");
    let rows = image_resource_lines(&text);
    assert_eq!(rows.len(), 1, "one forced cache miss must emit one image-resource row\n{text}");
    assert_eq!(rows[0][1], "2", "the third frame is observation ordinal two");
    assert_eq!(rows[0][2], "2", "resource ordinal and tick must agree");
    assert_eq!(rows[0][3], "1", "one native image load succeeds");
    assert_eq!(rows[0][4], "0", "the fixture path must not fail");
    assert_eq!(rows[0][11], "1", "the row must carry one exact path");
    assert_eq!(decode_hex_string(rows[0][13]), "assets/Sprites/error.png");

    let playback = replay_dump("bitgun", 6, &log_s, &[]);
    assert_eq!(
        playback["__stone_diag"]["STONE_REPLAY_RECORDED_IMAGE_RESOURCE_COUNT"],
        1,
        "playback must retain the recorded image row on its observation"
    );
}

#[test]
fn opted_in_slow_frames_carry_bounded_named_profiler_attribution() {
    let dir = tempfile::tempdir().expect("log dir");
    let plain = dir.path().join("plain.replay");
    let profiled = dir.path().join("profiled.replay");
    let plain_s = plain.to_string_lossy().into_owned();
    let profiled_s = profiled.to_string_lossy().into_owned();

    let ordinary = record_raw("bitgun", 8, &plain_s, &[("STONE_FRAME_DELAY_MS", "2")]);
    assert!(ordinary.status.success(), "ordinary record failed");
    let ordinary_text = std::fs::read_to_string(&plain).expect("read ordinary replay");
    assert!(
        slow_frame_lines(&ordinary_text).is_empty(),
        "ordinary recording must not emit profiler-section attribution"
    );

    let profiled_state = record_dump(
        "bitgun",
        8,
        &profiled_s,
        &[
            ("STONE_FRAME_DELAY_MS", "2"),
            ("STONE_REPLAY_SLOW_PROFILE_MS", "1"),
        ],
    );
    assert_eq!(
        profiled_state["__stone_diag"]["STONE_REPLAY_PROFILE_LIGHTWEIGHT_FRAMES"], 8,
        "opt-in recording must use the replay-only capture path on every frame"
    );
    let text = std::fs::read_to_string(&profiled).expect("read profiled replay");
    assert!(
        text.starts_with(&format!("{}\n", rock_replay_protocol::replay_header())),
        "slow-frame attribution requires the versioned v{} grammar\n{text}",
        rock_replay_protocol::REPLAY_VERSION
    );
    let rows = slow_frame_lines(&text);
    assert_eq!(
        rows.len(),
        tick_count(&text),
        "every forced-slow frame needs attribution"
    );
    assert!(rows.iter().all(|row| row.ordinal == row.tick));
    assert!(rows.iter().all(|row| !row.sections.is_empty()));
    assert!(rows.iter().all(|row| row.sections.len() <= 12));
    assert!(
        rows.iter().any(|row| row.heap_bytes > 0),
        "heap evidence must be nondegenerate"
    );
    assert!(
        rows.iter()
            .flat_map(|row| &row.sections)
            .any(|section| section.inclusive_us > 0 && section.calls > 0),
        "named section attribution must contain measured work"
    );

    let playback = replay_dump("bitgun", 8, &profiled_s, &[]);
    assert_eq!(
        playback["__stone_diag"]["STONE_REPLAY_RECORDED_SLOW_PROFILE_COUNT"],
        rows.len() as u64,
        "playback must parse the same aligned attribution population"
    );
}

#[test]
fn playback_profiles_only_recorded_slow_ordinals_without_mutating_the_source() {
    let dir = tempfile::tempdir().expect("log dir");
    let log = dir.path().join("playback-profile.replay");
    let log_s = log.to_string_lossy().into_owned();
    let timings = "1000,1000,9000,1000,1000,7000,1000,1000";
    let recorded = record_raw("bitgun", 8, &log_s, &[("STONE_REPLAY_TIMING_US", timings)]);
    assert!(recorded.status.success(), "record failed");
    let before = std::fs::read(&log).expect("read source replay");
    assert!(
        slow_frame_lines(&String::from_utf8_lossy(&before)).is_empty(),
        "the source must contain only cheap timing"
    );

    let playback = replay_dump(
        "bitgun",
        8,
        &log_s,
        &[("STONE_REPLAY_SLOW_PROFILE_MS", "5")],
    );
    assert_eq!(
        playback["__stone_diag"]["STONE_REPLAY_PLAYBACK_SLOW_PROFILE_COUNT"], 2,
        "only the two recorded frames at or above 5ms should be profiled"
    );
    assert_eq!(
        playback["__stone_diag"]["STONE_REPLAY_PROFILE_LIGHTWEIGHT_FRAMES"], 2,
        "section collection must be inactive on the other playback frames"
    );
    assert_eq!(
        std::fs::read(&log).expect("reread source replay"),
        before,
        "playback profiling must remain in memory"
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
    assert!(
        !timings.is_empty(),
        "the product recorder must emit measured samples"
    );
    assert!(timings.iter().all(|row| row.tick < tick_count(&text)));
    assert!(
        timings
            .iter()
            .any(|row| row.logic_us + row.render_us + row.present_us > 0),
        "the product recorder must attach a nondegenerate fixed phase tuple"
    );
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
    assert!(
        text.starts_with(&format!("{}\n", rock_replay_protocol::replay_header())),
        "crash log must retain v{}\n{text}", rock_replay_protocol::REPLAY_VERSION
    );
    assert!(ticks > 0, "crash log must retain observations");
    assert_eq!(
        timings.len() + 1,
        ticks,
        "only the crash-truncated final observation may lack timing"
    );
    assert_eq!(timings.last().map(|row| row.ordinal), Some(ticks - 2));
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
    let text = std::fs::read_to_string(&log).expect("read replay");
    std::fs::write(&log, crate::stone_harness::downgrade_replay(&text, 8))
        .expect("write legacy replay");

    let state = replay_dump("bitgun", 8, &log_s, &[]);
    assert_eq!(
        state["__stone_diag"]["STONE_REPLAY_RECORDED_TIMING_COUNT"], 0,
        "legacy playback must expose an empty recorded timing track"
    );
}

#[test]
fn legacy_v9_total_timing_remains_playable_without_phase_attribution() {
    let dir = tempfile::tempdir().expect("log dir");
    let log = dir.path().join("legacy-v9.replay");
    let log_s = log.to_string_lossy().into_owned();
    let recorded = record_raw("bitgun", 8, &log_s, &[]);
    assert!(recorded.status.success(), "record failed");
    let text = std::fs::read_to_string(&log).expect("read replay");
    std::fs::write(&log, crate::stone_harness::downgrade_replay(&text, 9))
        .expect("write legacy replay");

    let state = replay_dump("bitgun", 8, &log_s, &[]);
    assert_eq!(
        state["__stone_diag"]["STONE_REPLAY_RECORDED_TIMING_COUNT"],
        8
    );
    assert_eq!(
        state["__stone_diag"]["STONE_REPLAY_RECORDED_TIMING_LAST_LOGIC_US"], -1,
        "v9 total timing must not invent phase attribution"
    );
}

#[test]
fn legacy_v10_phase_timing_remains_playable_without_slow_profiles() {
    let dir = tempfile::tempdir().expect("log dir");
    let log = dir.path().join("legacy-v10.replay");
    let log_s = log.to_string_lossy().into_owned();
    let recorded = record_raw("bitgun", 8, &log_s, &[]);
    assert!(recorded.status.success(), "record failed");
    let text = std::fs::read_to_string(&log).expect("read replay");
    std::fs::write(&log, crate::stone_harness::downgrade_replay(&text, 10))
        .expect("write v10 replay");

    let state = replay_dump("bitgun", 8, &log_s, &[]);
    assert_eq!(
        state["__stone_diag"]["STONE_REPLAY_RECORDED_TIMING_COUNT"],
        8
    );
    assert_eq!(
        state["__stone_diag"]["STONE_REPLAY_RECORDED_SLOW_PROFILE_COUNT"], 0,
        "v10 must not invent slow-frame attribution"
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
        (
            "duplicate",
            source.replacen(first_timing, &format!("{first_timing}\n{first_timing}"), 1),
        ),
        (
            "orphan",
            source
                .lines()
                .chain(std::iter::once("p 99 99 1 1 1 1"))
                .collect::<Vec<_>>()
                .join("\n")
                + "\n",
        ),
        (
            "negative",
            source.replacen(first_timing, "p 0 0 -1 1 1 1", 1),
        ),
        (
            "non-finite",
            source.replacen(first_timing, "p 0 0 nan 1 1 1", 1),
        ),
        (
            "overflow",
            source.replacen(
                first_timing,
                "p 0 0 999999999999999999999999999999999999 1 1 1",
                1,
            ),
        ),
        (
            "missing-phase",
            source.replacen(first_timing, "p 0 0 1 1 1", 1),
        ),
        (
            "negative-phase",
            source.replacen(first_timing, "p 0 0 1 -1 1 1", 1),
        ),
    ];
    for (name, text) in cases {
        let path = dir.path().join(format!("{name}.replay"));
        std::fs::write(&path, text).expect("write malformed replay");
        let path_s = path.to_string_lossy().into_owned();
        assert_load_rejected(&replay_raw("bitgun", 8, &path_s, &[]), name);
    }
}

#[test]
fn malformed_duplicate_and_orphan_slow_profiles_are_rejected() {
    let dir = tempfile::tempdir().expect("log dir");
    let base = dir.path().join("profile-base.replay");
    let base_s = base.to_string_lossy().into_owned();
    let recorded = record_raw(
        "bitgun",
        4,
        &base_s,
        &[
            ("STONE_FRAME_DELAY_MS", "2"),
            ("STONE_REPLAY_SLOW_PROFILE_MS", "1"),
        ],
    );
    assert!(recorded.status.success(), "record failed");
    let source = std::fs::read_to_string(&base).expect("read base replay");
    let first = source
        .lines()
        .find(|line| line.starts_with("s "))
        .expect("slow-profile population");
    let fields = first.split_whitespace().collect::<Vec<_>>();
    assert!(
        fields.len() >= 14,
        "fixture needs at least two sections: {first}"
    );

    let mutate_field = |index: usize, value: &str| {
        let mut changed = fields.clone();
        changed[index] = value;
        source.replacen(first, &changed.join(" "), 1)
    };

    let duplicate_name = {
        let mut changed = fields.clone();
        changed[8 + 3] = changed[8];
        changed.join(" ")
    };
    let cases = [
        (
            "duplicate-profile",
            source.replacen(first, &format!("{first}\n{first}"), 1),
        ),
        (
            "orphan-profile",
            source.clone() + "s 99 99 1 1 0 0 1 78 1 1\n",
        ),
        ("wrong-tick", mutate_field(2, "1")),
        ("zero-calls", mutate_field(10, "0")),
        ("bad-name", mutate_field(8, "zz")),
        ("duplicate-name", source.replacen(first, &duplicate_name, 1)),
        ("wrong-count", mutate_field(7, "13")),
    ];
    for (name, text) in cases {
        let path = dir.path().join(format!("{name}.replay"));
        std::fs::write(&path, text).expect("write malformed replay");
        let path_s = path.to_string_lossy().into_owned();
        assert_load_rejected(&replay_raw("bitgun", 8, &path_s, &[]), name);
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
    let record_env = [
        ("STONE_REPLAY_TIMING_US", samples.as_str()),
        ("STONE_CHECKPOINT_INTERVAL", "1"),
        ("STONE_FRAME_DELAY_MS", "2"),
        ("STONE_REPLAY_SLOW_PROFILE_MS", "1"),
    ];
    let recorded = record_raw("bitgun", 12, &original_s, &record_env);
    assert!(recorded.status.success(), "record failed");
    let original_bytes = std::fs::read(&original).expect("original replay bytes");
    let changed_text = std::fs::read_to_string(&original)
        .expect("original replay")
        .lines()
        .map(|line| {
            if let Some(rest) = line.strip_prefix("p ") {
                let fields = rest.split_whitespace().collect::<Vec<_>>();
                format!(
                    "p {} {} 9000000 8000000 7000000 6000000",
                    fields[0], fields[1]
                )
            } else if line.starts_with("s ") {
                let mut fields = line.split_whitespace().collect::<Vec<_>>();
                fields[3] = "900000000";
                fields[4] = "800000000";
                fields[5] = if fields[5] == "0" { "1" } else { "0" };
                for i in (9..fields.len()).step_by(3) {
                    fields[i] = "7000000";
                }
                fields.join(" ")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(&changed, changed_text).expect("changed replay");

    let replay_env = [("STONE_CHECKPOINT_INTERVAL", "1")];
    let mut state_a = replay_dump("bitgun", 12, &original_s, &replay_env);
    let mut state_b = replay_dump("bitgun", 12, &changed_s, &replay_env);
    assert_eq!(
        state_a["__stone_diag"]["STONE_REPLAY_RECORDED_TIMING_COUNT"],
        12
    );
    assert_eq!(
        state_b["__stone_diag"]["STONE_REPLAY_RECORDED_TIMING_COUNT"],
        12
    );
    assert_eq!(
        state_a["__stone_diag"]["STONE_REPLAY_RECORDED_TIMING_LAST_US"],
        1011
    );
    assert_eq!(
        state_b["__stone_diag"]["STONE_REPLAY_RECORDED_TIMING_LAST_US"],
        9000000
    );
    assert_eq!(
        state_b["__stone_diag"]["STONE_REPLAY_RECORDED_TIMING_LAST_LOGIC_US"],
        8000000
    );
    assert_eq!(
        state_a["__stone_diag"]["STONE_REPLAY_RECORDED_SLOW_PROFILE_COUNT"],
        12
    );
    assert_eq!(
        state_b["__stone_diag"]["STONE_REPLAY_RECORDED_SLOW_PROFILE_COUNT"],
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
    let phases = exact_phase_samples(40, 1000);
    let recorded = record_raw(
        "bitgun",
        40,
        &log_s,
        &[
            ("STONE_REPLAY_TIMING_US", &samples),
            ("STONE_REPLAY_PHASE_TIMING_US", &phases),
            ("STONE_FRAME_DELAY_MS", "2"),
            ("STONE_REPLAY_SLOW_PROFILE_MS", "1"),
        ],
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
            ("STONE_REPLAY_SLOW_PROFILE_MS", "1"),
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
        diag["REPLAY_TIMELINE_TIMING_TICK"], 12,
        "the displayed timing sample must name its observation tick"
    );
    assert_eq!(diag["REPLAY_TIMELINE_TIMING_RECORDED_LOGIC_US"], 1012);
    assert_eq!(diag["REPLAY_TIMELINE_TIMING_RECORDED_RENDER_US"], 1112);
    assert_eq!(diag["REPLAY_TIMELINE_TIMING_RECORDED_PRESENT_US"], 1212);
    assert_eq!(
        diag["REPLAY_TIMELINE_SLOW_PROFILE_PRESENT"], 1,
        "the selected slow observation must expose its named profiler snapshot"
    );
    assert_eq!(
        diag["REPLAY_TIMELINE_PLAYBACK_SLOW_PROFILE_PRESENT"], 1,
        "the selected observation must expose replay-time attribution separately"
    );
    assert!(
        diag["REPLAY_TIMELINE_TIMING_PLAYBACK_LOGIC_US"]
            .as_i64()
            .is_some_and(|us| us >= 0),
        "ordinary playback phase timing must remain separate and observable: {diag}"
    );
}

#[test]
fn resume_live_here_drops_abandoned_timing_and_appends_fresh_branch_samples() {
    let dir = tempfile::tempdir().expect("log dir");
    let log = dir.path().join("branch-timing.replay");
    let log_s = log.to_string_lossy().into_owned();
    let samples = (0..160)
        .map(|i| {
            if i < 20 {
                (100 + i).to_string()
            } else {
                (9000 + i).to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(",");
    let phases = (0..160)
        .map(|i| {
            let base = if i < 20 { 200 + i } else { 20000 + i };
            format!("{base}:{}:{}", base + 100, base + 200)
        })
        .collect::<Vec<_>>()
        .join(",");
    let live = record_raw(
        "bitgun",
        120,
        &log_s,
        &[
            ("STONE_REPLAY_TIMING_US", &samples),
            ("STONE_REPLAY_PHASE_TIMING_US", &phases),
            ("STONE_FRAME_DELAY_MS", "2"),
            ("STONE_REPLAY_SLOW_PROFILE_MS", "1"),
            ("STONE_LIVE_REWIND", "1"),
            ("STONE_TIMELINE_SEEK_AT_FRAME", "20"),
            ("STONE_TIMELINE_SEEK_TO_TICK", "10"),
            ("STONE_BRANCH_AT_FRAME", "40"),
        ],
    );
    assert!(
        live.status.success(),
        "branch record failed: {}",
        String::from_utf8_lossy(&live.stderr)
    );
    let text = std::fs::read_to_string(&log).expect("branched replay");
    let timings = timing_lines(&text);
    let profiles = slow_frame_lines(&text);
    assert_eq!(
        profiles.len(),
        tick_count(&text),
        "branch rewrite must retain prefix attribution and append fresh branch attribution"
    );
    assert!(profiles.iter().all(|row| row.ordinal == row.tick));
    assert_eq!(
        timings.iter().find(|row| row.ordinal == 5).copied(),
        Some(TimingRow {
            ordinal: 5,
            tick: 5,
            total_us: 105,
            logic_us: 205,
            render_us: 305,
            present_us: 405,
        }),
        "the branch rewrite must preserve each retained phase through the shared record"
    );
    assert!(
        timings
            .iter()
            .any(|row| row.tick >= 10 && row.total_us >= 9000),
        "new branch frames must record fresh timing: {timings:?}"
    );
    assert!(
        timings
            .iter()
            .all(|row| !(row.tick >= 10 && row.tick < 20 && row.total_us < 9000)),
        "abandoned future timing must be discarded rather than retained: {timings:?}"
    );
    assert!(
        timings
            .iter()
            .any(|row| row.tick >= 10 && row.logic_us >= 20000),
        "new branch frames must carry fresh phase attribution: {timings:?}"
    );
    assert!(
        timings
            .iter()
            .all(|row| !(row.tick >= 10 && row.tick < 20 && row.logic_us < 20000)),
        "abandoned future phase attribution must be discarded: {timings:?}"
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
            ("STONE_FRAME_DELAY_MS", "2"),
            ("STONE_REPLAY_SLOW_PROFILE_MS", "1"),
            ("STONE_SAVE_REPRO_AT_FRAME", "10"),
            ("STONE_SAVE_REPRO_PATH", &repro_s),
        ],
    );
    assert!(
        recorded.status.success(),
        "record failed: {}",
        String::from_utf8_lossy(&recorded.stderr)
    );
    let repro_text = std::fs::read_to_string(&repro).expect("saved repro");
    assert!(
        timing_lines(&repro_text).len() > 0,
        "save repro must retain the timing population already completed"
    );
    assert!(
        timing_lines(&repro_text).len() == tick_count(&repro_text),
        "save repro must flush the pending sample before copying"
    );
    assert_eq!(
        slow_frame_lines(&repro_text).len(),
        tick_count(&repro_text),
        "save repro must flush and preserve the aligned slow-frame snapshots"
    );

    let seeked = replay_dump("bitgun", 40, &source_s, &[("STONE_RUN_TO_TICK", "25")]);
    let seek_diag = &seeked["__stone_diag"];
    assert_eq!(
        seek_diag["STONE_REPLAY_PLAYBACK_TIMING_COUNT"]
            .as_i64()
            .unwrap_or(0),
        0,
        "hidden resimulation must not be reported as ordinary playback-frame timing"
    );
    assert!(
        seek_diag["STONE_REPLAY_RECORDED_TIMING_COUNT"]
            .as_i64()
            .is_some_and(|n| n > 0),
        "hidden resimulation must still have recorded timing available for inspection"
    );
}
