//! P4 of docs/replay-scrub-plan.md: keyframe snapshot + restore. This is the
//! Phase-A RESTORE ORACLE, written RED before any restore machinery exists so
//! Phase B's restore path turns it green (TDD, per the arc's discipline).
//!
//! Contract: snapshotting the registry at tick T and restoring it later must
//! return the game EXACTLY to tick-T sim state -- same `current_tick`, and every
//! simulation-state global byte-identical to a plain run-to-T. The equality
//! oracle is the engine's own `__replay_is_sim_state` predicate output (the
//! `c <tick> <name> <hash>` checkpoint names via `sim_state_names`); we compare
//! those globals, not a duplicated Rust exclusion list.
//!
//! WHICH globals is engine-owned, and so is which LEAVES inside one are the
//! host's realization rather than the state: a field whose declared type is a
//! handle newtype serializes as a host projection
//! (`stone/engine/host_projection.rock`). Nothing here duplicates that.
//!
//! Intended Phase-B hooks (do NOT exist yet -- that is why this is RED and
//! #[ignore]d so it never gates `just t`): `STONE_SNAPSHOT_AT_TICK=T` captures a
//! keyframe; `STONE_RESTORE_AT_TICK=U` restores the most recent keyframe. With
//! neither implemented, the restore is a no-op: the run advances to U and
//! `current_tick` stays U (!= T), so the first assertion fails. Remove #[ignore]
//! when the restore path lands.
//!
//! Heavy: spawns `rock frame`. -P full tier (excluded from the default profile).

use std::path::Path;
use std::process::Command;

fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("workspace root")
}

/// Sim-state global names = the engine predicate's output (its `c`-line names),
/// consumed rather than re-listed in Rust.
fn sim_state_names(game: &str, frames: u32) -> std::collections::HashSet<String> {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("names.replay");
    let scratch = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame", game, "-n", &frames.to_string(),
            "--record", &log.to_string_lossy(), "--checkpoint-interval", "1",
            "--output", &scratch.path().to_string_lossy(),
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(workspace_root())
        .output()
        .expect("spawn record");
    assert!(out.status.success(), "record failed: {}", String::from_utf8_lossy(&out.stderr));
    let text = std::fs::read_to_string(&log).unwrap();
    text.lines()
        .filter_map(|l| l.strip_prefix("c "))
        .filter_map(|rest| rest.split(' ').nth(1))
        .map(|s| s.to_string())
        .collect()
}

/// `rock frame <game> -n N --replay <log> --dump-state [env]` -> parsed JSON.
fn run_replay_dump(game: &str, frames: u32, log: &str, env: &[(&str, &str)]) -> serde_json::Value {
    let scratch = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame", game, "-n", &frames.to_string(), "--replay", log,
            "--dump-state", "--output", &scratch.path().to_string_lossy(),
        ])
        .envs(env.iter().copied())
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(workspace_root())
        .output()
        .expect("spawn replay");
    assert!(out.status.success(), "replay failed: {}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).expect("dump-state JSON")
}

/// `rock frame <game> -n N --record <log> --dump-state [env]` -> parsed JSON.
/// Unlike `run_replay_dump`, this keeps the process in live record mode so a
/// test can prove a rewind happened without a supervisor relaunch.
fn run_record_dump(game: &str, frames: u32, log: &str, env: &[(&str, &str)]) -> serde_json::Value {
    let scratch = tempfile::tempdir().unwrap();
    let frames_s = frames.to_string();
    let out = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame", game, "-n", &frames_s, "--record", log,
            "--dump-state", "--output", &scratch.path().to_string_lossy(),
        ])
        .envs(env.iter().copied())
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(workspace_root())
        .output()
        .expect("spawn live record");
    assert!(out.status.success(), "live record failed: {}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).expect("live record dump-state JSON")
}

fn current_tick(s: &serde_json::Value) -> i64 {
    s["stone"]["current_tick"].as_i64().expect("current_tick")
}

#[test]
fn restore_returns_to_snapshot_tick_state() {
    let game = "bitgun";
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("restore.replay");
    let log_s = log.to_string_lossy().into_owned();
    let scratch = tempfile::tempdir().unwrap();

    // A log to replay deterministically.
    let rec = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame", game, "-n", "30", "--record", &log_s,
            "--output", &scratch.path().to_string_lossy(),
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(workspace_root())
        .output()
        .expect("spawn record");
    assert!(rec.status.success(), "record failed: {}", String::from_utf8_lossy(&rec.stderr));

    // Reference: replay run stepped to exactly tick 10 -> the true tick-10 state.
    let reference = run_replay_dump(
        game, 30, &log_s,
        &[("STONE_START_PAUSED", "1"), ("STONE_STEP_FRAMES", "10")],
    );
    assert_eq!(current_tick(&reference), 10, "reference must sit at tick 10");

    // Restore run: snapshot at tick 10, advance to 20, restore the keyframe.
    // A correct restore returns to tick-10 state.
    let restored = run_replay_dump(
        game, 30, &log_s,
        &[("STONE_SNAPSHOT_AT_TICK", "10"), ("STONE_RESTORE_AT_TICK", "20")],
    );

    assert_eq!(
        current_tick(&restored), 10,
        "after restoring the tick-10 keyframe, current_tick must be 10 (got {}); \
         restore path missing today",
        current_tick(&restored)
    );

    // Every simulation-state global must match the tick-10 reference exactly.
    let sim = sim_state_names(game, 30);
    assert_sim_state_matches(&restored, &reference, &sim, "tick-10");
}

/// Assert every sim-state global (`sim`) in `got` matches `want`, and at least
/// one was compared.
///
/// Resource ids are excluded by the engine's serializer, which renders a leaf
/// whose declared type is a handle newtype as a host projection
/// (`stone/engine/host_projection.rock`): these two dumps come from two `rock
/// frame` processes that created a different number of images and meshes on the
/// way to the tick they report. The count here is the exclusion's denominator.
fn assert_sim_state_matches(
    got: &serde_json::Value,
    want: &serde_json::Value,
    sim: &std::collections::HashSet<String>,
    label: &str,
) {
    let mut compared = 0;
    let mut stripped = 0;
    for (pkg, globals) in got.as_object().expect("dump object") {
        let want_pkg = &want[pkg];
        for (name, val) in globals.as_object().expect("pkg object") {
            if !sim.contains(name) {
                continue;
            }
            let (a, b) = (val.clone(), want_pkg[name].clone());
            stripped += crate::stone_harness::count_host_projections(&a);
            stripped += crate::stone_harness::count_host_projections(&b);
            assert_eq!(a, b, "sim-state {pkg}.{name} must match {label}");
            compared += 1;
        }
    }
    assert!(compared > 0, "must compare at least one sim-state global");
    // The denominator for the exclusion: there WAS something to exclude, so a
    // green here is not "there were no handles to disagree about".
    assert!(stripped > 0, "no sim-state global carried a host-projection leaf");
}

fn assert_sim_state_matches_exact(
    got: &serde_json::Value,
    want: &serde_json::Value,
    sim: &std::collections::HashSet<String>,
    label: &str,
) {
    let mut compared = 0;
    for (pkg, globals) in got.as_object().expect("dump object") {
        let want_pkg = &want[pkg];
        for (name, val) in globals.as_object().expect("pkg object") {
            if !sim.contains(name) {
                continue;
            }
            assert_eq!(val, &want_pkg[name], "sim-state {pkg}.{name} must match {label}");
            compared += 1;
        }
    }
    assert!(compared > 0, "must compare at least one sim-state global");
}

fn assert_f32_state_matches(got: &serde_json::Value, want: &serde_json::Value, path: &str) {
    match (got, want) {
        (serde_json::Value::Number(a), serde_json::Value::Number(b)) => {
            if let (Some(a), Some(b)) = (a.as_i64(), b.as_i64()) {
                assert_eq!(a, b, "integer state must match at {path}");
            } else {
                let a = a.as_f64().expect("JSON number");
                let b = b.as_f64().expect("JSON number");
                assert_eq!((a as f32).to_bits(), (b as f32).to_bits(), "f32 state must match at {path}");
            }
        }
        (serde_json::Value::Array(a), serde_json::Value::Array(b)) => {
            assert_eq!(a.len(), b.len(), "array length must match at {path}");
            for (i, (a, b)) in a.iter().zip(b).enumerate() {
                assert_f32_state_matches(a, b, &format!("{path}[{i}]"));
            }
        }
        (serde_json::Value::Object(a), serde_json::Value::Object(b)) => {
            assert_eq!(a.len(), b.len(), "object shape must match at {path}");
            for (name, a) in a {
                let b = b.get(name).unwrap_or_else(|| panic!("missing {path}.{name}"));
                assert_f32_state_matches(a, b, &format!("{path}.{name}"));
            }
        }
        _ => assert_eq!(got, want, "state must match at {path}"),
    }
}

fn assert_mag_seek_state_matches(
    got: &serde_json::Value,
    want: &serde_json::Value,
    sim: &std::collections::HashSet<String>,
    label: &str,
) {
    let mut exact = sim.clone();
    for name in ["DROP_PARTICLES", "WISPS"] {
        assert!(exact.remove(name), "{name} must be admitted as simulation state");
        assert_f32_state_matches(&got["mag"][name], &want["mag"][name], &format!("mag.{name}"));
    }
    assert_sim_state_matches(got, want, &exact, label);
}

/// P4 item 2: scrub to an ARBITRARY earlier tick. Cadence keyframes (every
/// `STONE_KEYFRAME_INTERVAL` ticks during replay) let a backward scrub restore
/// the nearest keyframe <= T and render-skipped-resim forward to T. Landing at
/// T must be byte-identical to a plain run-to-T -- the same determinism contract
/// the restore oracle proves, but exercising nearest-keyframe selection (T=25
/// with interval 10 restores the tick-20 keyframe, not tick 0).
#[test]
fn scrub_to_earlier_tick_matches_plain_run() {
    let game = "bitgun";
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("scrub.replay");
    let log_s = log.to_string_lossy().into_owned();
    let scratch = tempfile::tempdir().unwrap();

    let rec = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame", game, "-n", "60", "--record", &log_s,
            "--output", &scratch.path().to_string_lossy(),
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(workspace_root())
        .output()
        .expect("spawn record");
    assert!(rec.status.success(), "record failed: {}", String::from_utf8_lossy(&rec.stderr));

    // Reference: the true tick-25 state.
    let reference = run_replay_dump(
        game, 60, &log_s,
        &[("STONE_START_PAUSED", "1"), ("STONE_STEP_FRAMES", "25")],
    );
    assert_eq!(current_tick(&reference), 25, "reference must sit at tick 25");

    // Scrub run: play forward to tick 40, then scrub BACK to 25. With interval
    // 10 the nearest keyframe <= 25 is tick 20, so this restores tick 20 and
    // resims 20->25 (not a trivial restore-to-exact-tick).
    let scrubbed = run_replay_dump(
        game, 60, &log_s,
        &[
            ("STONE_KEYFRAME_INTERVAL", "10"),
            ("STONE_SCRUB_AT_TICK", "40"),
            ("STONE_SCRUB_TO_TICK", "25"),
        ],
    );

    assert_eq!(
        current_tick(&scrubbed), 25,
        "after scrubbing back to 25, current_tick must be 25 (got {})",
        current_tick(&scrubbed)
    );

    let sim = sim_state_names(game, 60);
    assert_sim_state_matches(&scrubbed, &reference, &sim, "tick-25");
}

#[test]
fn scrub_reports_seek_cost_and_resimulation_population() {
    let game = "bitgun";
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("seek-metrics.replay");
    let log_s = log.to_string_lossy().into_owned();
    let scratch = tempfile::tempdir().unwrap();

    let rec = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame", game, "-n", "60", "--record", &log_s,
            "--output", &scratch.path().to_string_lossy(),
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(workspace_root())
        .output()
        .expect("spawn record");
    assert!(rec.status.success(), "record failed: {}", String::from_utf8_lossy(&rec.stderr));

    let scrubbed = run_replay_dump(
        game,
        60,
        &log_s,
        &[
            ("STONE_KEYFRAME_INTERVAL", "10"),
            ("STONE_SCRUB_AT_TICK", "40"),
            ("STONE_SCRUB_TO_TICK", "25"),
        ],
    );
    let diag = &scrubbed["__stone_diag"];
    assert_eq!(diag["STONE_SEEK_COUNT"].as_i64(), Some(1));
    assert_eq!(diag["STONE_SEEK_FROM_TICK"].as_i64(), Some(40));
    assert_eq!(diag["STONE_SEEK_TO_TICK"].as_i64(), Some(25));
    assert_eq!(diag["STONE_SEEK_RESIM_FRAMES"].as_i64(), Some(5));
    assert!(
        diag["STONE_SEEK_MS"].as_f64().is_some_and(|ms| ms > 0.0),
        "seek timing must be reported as a positive duration: {diag}"
    );
    assert!(
        diag["STONE_SEEK_RESIM_MS"].as_f64().is_some_and(|ms| ms > 0.0),
        "resimulation timing must be reported as a positive duration: {diag}"
    );
}

#[test]
fn refused_scrub_does_not_leave_seek_active_or_complete() {
    let game = "bitgun";
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("refused-scrub.replay");
    let log_s = log.to_string_lossy().into_owned();
    let scratch = tempfile::tempdir().unwrap();

    let rec = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame", game, "-n", "60", "--record", &log_s,
            "--output", &scratch.path().to_string_lossy(),
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(workspace_root())
        .output()
        .expect("spawn record");
    assert!(rec.status.success(), "record failed: {}", String::from_utf8_lossy(&rec.stderr));

    // Interval zero disables every cadence capture, so this deliberately has
    // no keyframe <= the backward target. A refused scrub must leave playback
    // untouched and must not publish a completed seek.
    let refused = run_replay_dump(
        game,
        60,
        &log_s,
        &[
            ("STONE_KEYFRAME_INTERVAL", "0"),
            ("STONE_SCRUB_AT_TICK", "40"),
            ("STONE_SCRUB_TO_TICK", "25"),
        ],
    );
    assert_eq!(current_tick(&refused), 61, "a refused scrub must not pause or rewind playback");
    let diag = &refused["__stone_diag"];
    assert_eq!(diag["STONE_SEEK_ACTIVE"].as_i64(), Some(0));
    assert!(diag.get("STONE_SEEK_COUNT").is_none(), "a refused scrub must not complete a seek: {diag}");
    assert!(diag.get("STONE_SEEK_RESIM_FRAMES").is_none(), "a refused scrub must not count resimulation: {diag}");
    assert!(diag.get("STONE_SEEK_MS").is_none(), "a refused scrub must not report seek time: {diag}");
}

#[test]
fn live_record_rewind_matches_plain_replay_at_target() {
    let game = "bitgun";
    let history: u32 = 40;
    let target: i64 = 25;
    let runtime_frames = 80;
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("live-rewind.replay");
    let log_s = log.to_string_lossy().into_owned();

    let rewound = run_record_dump(
        game,
        runtime_frames,
        &log_s,
        &[
            ("STONE_LIVE_REWIND", "1"),
            ("STONE_TIMELINE_SEEK_AT_FRAME", "40"),
            ("STONE_TIMELINE_SEEK_TO_TICK", "25"),
            ("STONE_AUDIO_PROBE", "1"),
        ],
    );
    assert_eq!(current_tick(&rewound), target, "live rewind must pause at the requested target");
    let diag = &rewound["__stone_diag"];
    assert_eq!(diag["STONE_LIVE_REWIND_SUPPORTED"].as_i64(), Some(1));
    assert_eq!(diag["STONE_LIVE_REWIND_REFUSED"].as_i64(), Some(0));
    assert_eq!(diag["STONE_LIVE_REWIND_COUNT"].as_i64(), Some(1));
    assert_eq!(diag["STONE_LIVE_REWIND_RESTORE_TICK"].as_i64(), Some(0));
    assert_eq!(diag["STONE_LIVE_REWIND_FUTURE_TICKS"].as_i64(), Some(history as i64 - target));
    assert_eq!(diag["STONE_SEEK_COUNT"].as_i64(), Some(1));
    assert_eq!(diag["STONE_SEEK_FROM_TICK"].as_i64(), Some(history as i64));
    assert_eq!(diag["STONE_SEEK_TO_TICK"].as_i64(), Some(target));
    assert_eq!(diag["STONE_SEEK_RESIM_FRAMES"].as_i64(), Some(target));
    assert_eq!(diag["STONE_SEEK_ACTIVE"].as_i64(), Some(0));
    assert!(
        diag["AUDIO_SEEK_MUTES"].as_i64().is_some_and(|n| n > 0),
        "live rewind must use the seek audio mute gate: {diag}"
    );
    assert!(
        diag["STONE_RESTORE_MS"].as_f64().is_some_and(|ms| ms > 0.0),
        "live rewind must reach the existing snapshot restore path: {diag}"
    );
    assert!(
        diag["STONE_TRANSIENT_RESET_CALLS"].as_i64().unwrap_or(0) > 0,
        "live rewind must fire the existing transient-reset seam: {diag}"
    );
    let record_lines = std::fs::read_to_string(&log_s)
        .unwrap()
        .lines()
        .filter(|line| line.starts_with("t "))
        .count();
    assert_eq!(record_lines, history as usize, "the live future must remain one canonical input stream");

    let reference = run_replay_dump(
        game,
        history,
        &log_s,
        &[("STONE_START_PAUSED", "1"), ("STONE_STEP_FRAMES", &target.to_string())],
    );
    assert_eq!(current_tick(&reference), target, "reference must sit at tick {target}");
    let sim = sim_state_names(game, history);
    assert_sim_state_matches(&rewound, &reference, &sim, "live-rewind tick-25");
}

/// Seek resimulation must run the same game semantics as ordinary playback.
/// Mag's main-menu interaction and Play action live in `@late_draw`, so this is the
/// concrete integration case that render-skipped resimulation used to lose.
#[test]
fn mag_main_menu_transition_survives_seek_resimulation() {
    let game = "mag";
    let frames = 80;
    let target: u32 = 40;
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("menu.replay");
    let log_s = log.to_string_lossy().into_owned();

    let _ = run_record_dump(
        game,
        frames,
        &log_s,
        &[
            ("MAG_BOOT_MENU", "1"),
            ("MAG_SCREENSHOT", "1"),
            ("STONE_CHECKPOINT_INTERVAL", "0"),
        ],
    );

    let text = std::fs::read_to_string(&log).expect("read Mag replay");
    let mut tick = 0;
    let lines = text
        .lines()
        .map(|line| {
            if !line.starts_with("t ") {
                return line.to_string();
            }
            let mut fields: Vec<&str> = line.split_whitespace().collect();
            assert_eq!(fields.len(), 8, "recorded tick shape changed: {line}");
            fields[1] = "0x1.999999999999ap-4";
            fields[2] = "960";
            fields[3] = "432";
            fields[6] = match tick {
                20 => "o",
                50 => "escape",
                _ => "-",
            };
            fields[7] = if tick == 30 { "1" } else { "-" };
            tick += 1;
            fields.join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(tick, frames as usize + 1, "fixture must rewrite every recorded tick");
    std::fs::write(&log, lines + "\n").expect("write deterministic Mag replay");

    let reference = run_replay_dump(
        game,
        frames,
        &log_s,
        &[
            ("MAG_BOOT_MENU", "1"),
            ("MAG_SCREENSHOT", "1"),
            ("STONE_START_PAUSED", "1"),
            ("STONE_STEP_FRAMES", &target.to_string()),
        ],
    );
    assert_eq!(current_tick(&reference), i64::from(target));
    assert_eq!(reference["mag"]["SCREEN"].as_i64(), Some(2), "ordinary playback must activate Play");
    // The recorded 'o' at tick 20 reaches the GAME and no longer reaches the
    // engine's debug toggles, which read tool input: whether a recorded key can
    // drive a tool is owned by
    // `replay_timeline_tests::recorded_keys_cannot_toggle_engine_debug_windows`,
    // which drives both sides. What stays asserted here is the classification
    // that made the divergence harmless in the first place.
    assert_eq!(
        reference["stone"]["WORLD_MODE"]["game_paused"].as_bool(),
        Some(false),
        "the reference target precedes the pause input"
    );
    let sim = sim_state_names(game, frames);
    for name in ["WORLD_MODE", "MOTES"] {
        assert!(sim.contains(name), "{name} must be admitted as simulation state");
    }
    assert!(!sim.contains("ENGINE"), "render-only ENGINE settings must not be checkpointed");
    let drops = reference["mag"]["DROP_PARTICLES"].as_array().expect("drop particle arena");
    for kind in ["Fire", "Mend"] {
        assert!(
            drops.iter().any(|p| p["kind"]["__variant"].as_str() == Some(kind)),
            "the reference must populate the {kind} producer moved out of @draw"
        );
    }
    let wisps = reference["mag"]["WISPS"]["data"].as_array().expect("wisp vector");
    assert!(!wisps.is_empty(), "the reference must contain gameplay Motes");
    assert!(
        wisps[0]["trail"]["depth_ys"]["data"]
            .as_array()
            .is_some_and(|points| !points.is_empty()),
        "the reference must populate trail history outside @draw"
    );

    let sought = run_replay_dump(
        game,
        frames + target + 10,
        &log_s,
        &[
            ("MAG_BOOT_MENU", "1"),
            ("MAG_SCREENSHOT", "1"),
            ("STONE_KEYFRAME_INTERVAL", "1000"),
            ("STONE_TIMELINE_SEEK_AT_FRAME", &frames.to_string()),
            ("STONE_TIMELINE_SEEK_TO_TICK", &target.to_string()),
        ],
    );
    assert_eq!(current_tick(&sought), i64::from(target), "seek must pause at the requested tick");
    assert_eq!(
        sought["__stone_diag"]["STONE_SEEK_RESIM_FRAMES"].as_i64(),
        Some(i64::from(target)),
        "the fixture must exercise from-boot resimulation"
    );
    assert_eq!(
        sought["mag"]["SCREEN"], reference["mag"]["SCREEN"],
        "seek resimulation must preserve Mag's @late_draw Play transition"
    );
    assert_eq!(
        sought["stone"]["WORLD_MODE"]["game_paused"],
        reference["stone"]["WORLD_MODE"]["game_paused"],
        "a from-boot seek must restore engine-owned pause state from before the horizon"
    );
    assert_eq!(sought["stone"]["PRESENTED_VIEW"], reference["stone"]["PRESENTED_VIEW"]);
    assert_eq!(sought["stone"]["CAMERA_TRAUMA"], reference["stone"]["CAMERA_TRAUMA"]);
    assert_eq!(
        sought["stone"]["CAMERA_SHAKE_OFFSET"],
        reference["stone"]["CAMERA_SHAKE_OFFSET"]
    );
    assert_mag_seek_state_matches(&sought, &reference, &sim, "Mag from-boot menu seek");

    let cadence_sought = run_replay_dump(
        game,
        frames + target + 10,
        &log_s,
        &[
            ("MAG_BOOT_MENU", "1"),
            ("MAG_SCREENSHOT", "1"),
            ("STONE_KEYFRAME_INTERVAL", "30"),
            ("STONE_TIMELINE_SEEK_AT_FRAME", &frames.to_string()),
            ("STONE_TIMELINE_SEEK_TO_TICK", &target.to_string()),
        ],
    );
    assert_eq!(
        cadence_sought["__stone_diag"]["STONE_SEEK_RESIM_FRAMES"].as_i64(),
        Some(10),
        "the fixture must restore immediately before the click"
    );
    assert_eq!(
        cadence_sought["mag"]["SCREEN"], reference["mag"]["SCREEN"],
        "a cadence-keyframe seek must restore Mag's retained mouse state"
    );
    assert_eq!(
        cadence_sought["stone"]["WORLD_MODE"]["game_paused"],
        reference["stone"]["WORLD_MODE"]["game_paused"],
        "a cadence-keyframe seek must restore engine-owned pause state"
    );
    assert_mag_seek_state_matches(&cadence_sought, &reference, &sim, "Mag cadence menu seek");

    let keyboard_log = dir.path().join("menu-keyboard.replay");
    let keyboard_log_s = keyboard_log.to_string_lossy().into_owned();
    let mut tick = 0;
    let lines = text
        .lines()
        .map(|line| {
            if !line.starts_with("t ") {
                return line.to_string();
            }
            let mut fields: Vec<&str> = line.split_whitespace().collect();
            assert_eq!(fields.len(), 8, "recorded tick shape changed: {line}");
            fields[1] = "0x1.999999999999ap-4";
            fields[2] = "0";
            fields[3] = "0";
            fields[6] = match tick {
                28 | 30 | 50 | 52 => "down",
                32 => "return",
                45 => "escape",
                _ => "-",
            };
            fields[7] = "-";
            tick += 1;
            fields.join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(tick, frames as usize + 1, "fixture must rewrite every recorded tick");
    std::fs::write(&keyboard_log, lines + "\n").expect("write keyboard Mag replay");

    let keyboard_reference = run_replay_dump(
        game,
        frames,
        &keyboard_log_s,
        &[
            ("MAG_BOOT_MENU", "1"),
            ("MAG_SCREENSHOT", "1"),
            ("STONE_START_PAUSED", "1"),
            ("STONE_STEP_FRAMES", &target.to_string()),
        ],
    );
    assert_eq!(
        keyboard_reference["mag"]["SETTINGS_OPEN"].as_bool(),
        Some(true),
        "ordinary keyboard playback must activate Settings"
    );
    let keyboard_sought = run_replay_dump(
        game,
        frames + target + 10,
        &keyboard_log_s,
        &[
            ("MAG_BOOT_MENU", "1"),
            ("MAG_SCREENSHOT", "1"),
            ("STONE_KEYFRAME_INTERVAL", "32"),
            ("STONE_TIMELINE_SEEK_AT_FRAME", &frames.to_string()),
            ("STONE_TIMELINE_SEEK_TO_TICK", &target.to_string()),
        ],
    );
    assert_eq!(
        keyboard_sought["mag"]["SETTINGS_OPEN"], keyboard_reference["mag"]["SETTINGS_OPEN"],
        "a cadence-keyframe seek must restore Mag's retained focus target"
    );
    assert_eq!(
        keyboard_sought["mag"]["CREDITS_OPEN"], keyboard_reference["mag"]["CREDITS_OPEN"],
        "restored focus must not activate a different menu item"
    );
}

#[test]
fn live_record_rewind_refuses_unsupported_state_without_mutating_world() {
    let game = "survivors";
    let runtime_frames = 80;
    let dir = tempfile::tempdir().unwrap();
    for (mode, global, kind) in [
        ("scene", "LIVE_REWIND_PROBE_SCENE", "Scene"),
        ("foreign", "LIVE_REWIND_PROBE_FOREIGN", "raw"),
        ("cdata", "LIVE_REWIND_PROBE_MUTABLE_CDATA", "cdata"),
    ] {
        let refused_log = dir.path().join(format!("{mode}-refused.replay"));
        let refused_log_s = refused_log.to_string_lossy().into_owned();
        let refused = run_record_dump(
            game,
            runtime_frames,
            &refused_log_s,
            &[
                ("ROCK_TEST_SEED", "42"),
                ("STONE_LIVE_REWIND", "1"),
                ("STONE_LIVE_REWIND_UNSUPPORTED", mode),
                ("STONE_TIMELINE_SEEK_AT_FRAME", "40"),
                ("STONE_TIMELINE_SEEK_TO_TICK", "25"),
            ],
        );
        let control = run_replay_dump(
            game,
            runtime_frames,
            &refused_log_s,
            &[("STONE_KEYFRAME_INTERVAL", "0"), ("STONE_LIVE_REWIND_UNSUPPORTED", mode)],
        );

        assert_eq!(current_tick(&refused), current_tick(&control), "{mode} refusal must keep live time moving");
        let diag = &refused["__stone_diag"];
        assert_eq!(diag["STONE_LIVE_REWIND_SUPPORTED"].as_i64(), Some(0), "{mode}");
        assert_eq!(diag["STONE_LIVE_REWIND_REFUSED"].as_i64(), Some(1), "{mode}");
        assert_eq!(diag["STONE_LIVE_REWIND_REFUSALS"].as_i64(), Some(1), "{mode}");
        assert!(diag.get("STONE_LIVE_REWIND_COUNT").is_none(), "{mode} refusal must not restore: {diag}");
        assert!(diag.get("STONE_SEEK_COUNT").is_none(), "{mode} refusal must not activate a seek: {diag}");
        assert!(diag.get("STONE_RESTORE_MS").is_none(), "{mode} refusal must not restore partial state: {diag}");
        assert_eq!(diag.get("STONE_TRANSIENT_RESET_CALLS").and_then(serde_json::Value::as_i64), None);
        let reason = diag["STONE_LIVE_REWIND_REFUSAL"].as_str().expect("refusal reason");
        assert!(reason.contains(global), "{mode} refusal must name its global: {reason}");
        assert!(reason.contains(kind), "{mode} refusal must name its unsupported type: {reason}");

        let sim = sim_state_names(game, runtime_frames);
        assert_sim_state_matches_exact(&refused, &control, &sim, &format!("refused live-rewind {mode} control"));
    }
}

/// Recorded input ticks in a from-boot log. A promotion to replay stops the
/// recorder, so this count is the direct oracle for "the session is still live".
fn recorded_tick_lines(path: &str) -> usize {
    std::fs::read_to_string(path)
        .expect("replay log")
        .lines()
        .filter(|l| l.starts_with("t "))
        .count()
}

/// Boot admission describes boot. Unsupported Scene/raw/native state created
/// AFTER boot but before H must still refuse the rewind at H -- before mode
/// promotion and before any restore -- so the live world is never partially
/// rolled back. The distinguishing evidence against the boot-refusal test is
/// `STONE_LIVE_REWIND_SUPPORTED == 1`: boot WAS admissible here.
#[test]
fn live_record_rewind_refuses_late_created_unsupported_state() {
    let game = "survivors";
    let runtime_frames = 80;
    let dir = tempfile::tempdir().unwrap();
    for (mode, global, kind) in [
        ("scene", "LIVE_REWIND_PROBE_SCENE", "Scene"),
        ("foreign", "LIVE_REWIND_PROBE_FOREIGN", "raw"),
        ("cdata", "LIVE_REWIND_PROBE_MUTABLE_CDATA", "cdata"),
    ] {
        let log = dir.path().join(format!("{mode}-late.replay"));
        let log_s = log.to_string_lossy().into_owned();
        let refused = run_record_dump(
            game,
            runtime_frames,
            &log_s,
            &[
                ("ROCK_TEST_SEED", "42"),
                ("STONE_LIVE_REWIND", "1"),
                ("STONE_LIVE_REWIND_UNSUPPORTED", mode),
                ("STONE_LIVE_REWIND_UNSUPPORTED_AT_TICK", "10"),
                ("STONE_TIMELINE_SEEK_AT_FRAME", "40"),
                ("STONE_TIMELINE_SEEK_TO_TICK", "25"),
            ],
        );
        let control = run_replay_dump(
            game,
            runtime_frames,
            &log_s,
            &[
                ("STONE_KEYFRAME_INTERVAL", "0"),
                ("STONE_LIVE_REWIND_UNSUPPORTED", mode),
                ("STONE_LIVE_REWIND_UNSUPPORTED_AT_TICK", "10"),
            ],
        );

        let diag = &refused["__stone_diag"];
        assert_eq!(
            diag["STONE_LIVE_REWIND_SUPPORTED"].as_i64(), Some(1),
            "{mode}: boot must be admissible, else this is the boot-refusal case: {diag}"
        );
        assert_eq!(diag["STONE_LIVE_REWIND_REFUSED"].as_i64(), Some(1), "{mode}");
        assert_eq!(diag["STONE_LIVE_REWIND_REFUSALS"].as_i64(), Some(1), "{mode}");
        assert!(diag.get("STONE_LIVE_REWIND_COUNT").is_none(), "{mode} refusal must not restore: {diag}");
        assert!(diag.get("STONE_SEEK_COUNT").is_none(), "{mode} refusal must not activate a seek: {diag}");
        assert!(diag.get("STONE_RESTORE_MS").is_none(), "{mode} refusal must not restore partial state: {diag}");
        assert_eq!(diag.get("STONE_TRANSIENT_RESET_CALLS").and_then(serde_json::Value::as_i64), None);
        let reason = diag["STONE_LIVE_REWIND_REFUSAL"].as_str().expect("refusal reason");
        assert!(reason.contains(global), "{mode} refusal must name its global: {reason}");
        assert!(reason.contains(kind), "{mode} refusal must name its unsupported type: {reason}");

        // No mode promotion: the recorder is still appending, so the log holds a
        // tick per run frame rather than stopping at the rewind frame (40).
        // `rock frame -n N` ticks N+1 frames -- the bound is checked after the
        // frame runs -- so that, not N, is the denominator.
        assert_eq!(
            recorded_tick_lines(&log_s), runtime_frames as usize + 1,
            "{mode} refusal must leave the session recording (no promotion to replay)"
        );
        assert_eq!(current_tick(&refused), current_tick(&control), "{mode} refusal must keep live time moving");
        let sim = sim_state_names(game, runtime_frames);
        assert_sim_state_matches_exact(&refused, &control, &sim, &format!("late-refused live-rewind {mode}"));
    }
}

/// The admission control: state created just as late, but SUPPORTED, must reach
/// the same gate and rewind. Without this arm the refusal test above cannot
/// distinguish "the gate classified the world" from "any late write refuses".
/// `LIVE_REWIND_PROBE_LATE_OK` is written at tick 10 and re-derived by the
/// resimulation, so the landing state has to carry it.
#[test]
fn live_record_rewind_admits_supported_late_state() {
    let game = "survivors";
    let history: u32 = 40;
    let target: i64 = 25;
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("late-supported.replay");
    let log_s = log.to_string_lossy().into_owned();

    let env: &[(&str, &str)] = &[
        ("ROCK_TEST_SEED", "42"),
        ("STONE_LIVE_REWIND", "1"),
        ("STONE_LIVE_REWIND_UNSUPPORTED", "supported"),
        ("STONE_LIVE_REWIND_UNSUPPORTED_AT_TICK", "10"),
        ("STONE_TIMELINE_SEEK_AT_FRAME", "40"),
        ("STONE_TIMELINE_SEEK_TO_TICK", "25"),
    ];
    let rewound = run_record_dump(game, 80, &log_s, env);
    let diag = &rewound["__stone_diag"];
    assert_eq!(diag["STONE_LIVE_REWIND_SUPPORTED"].as_i64(), Some(1));
    assert_eq!(diag["STONE_LIVE_REWIND_COUNT"].as_i64(), Some(1), "supported late state must rewind: {diag}");
    assert_eq!(diag.get("STONE_LIVE_REWIND_REFUSALS").and_then(serde_json::Value::as_i64), None);
    assert_eq!(current_tick(&rewound), target);
    assert_eq!(
        recorded_tick_lines(&log_s), history as usize,
        "the rewind must promote at H, leaving the recorded future intact"
    );

    let reference = run_replay_dump(
        game,
        history,
        &log_s,
        &[
            ("STONE_START_PAUSED", "1"),
            ("STONE_STEP_FRAMES", &target.to_string()),
            ("STONE_LIVE_REWIND_UNSUPPORTED", "supported"),
            ("STONE_LIVE_REWIND_UNSUPPORTED_AT_TICK", "10"),
        ],
    );
    assert_eq!(current_tick(&reference), target, "reference must sit at tick {target}");
    for s in [&rewound, &reference] {
        let v = s
            .as_object()
            .expect("dump object")
            .values()
            .find_map(|g| g.as_object().and_then(|o| o.get("LIVE_REWIND_PROBE_LATE_OK")))
            .expect("late probe global must be dumped");
        assert_eq!(v.as_i64(), Some(7), "the late-created supported value must be re-derived past tick 10");
    }
    let sim = sim_state_names(game, history);
    assert_sim_state_matches_exact(&rewound, &reference, &sim, "supported late-state live-rewind");
}

/// A rewound live session owns its recorded future, so reaching the horizon is
/// an inspection boundary, not the end of a batch job: it must PAUSE there and
/// stay interactive rather than taking the ordinary replay exit. Both arms play
/// T -> H, which consumes the retained records; arm B then scrubs backward again
/// from H, which is only reachable if the process is still alive at all.
///
/// The log-unchanged claim is checked WITHIN the process (a from-boot copy taken
/// one frame after the rewind, compared with the file at shutdown): two record
/// runs cannot be compared byte-for-byte because `dt` is real wall-clock time.
#[test]
fn live_rewind_pauses_at_horizon_and_keeps_the_future_inspectable() {
    let game = "bitgun";
    let horizon: i64 = 40;
    let dir = tempfile::tempdir().unwrap();

    let run = |name: &str, extra: &[(&str, &str)]| {
        let log = dir.path().join(format!("{name}.replay"));
        let log_s = log.to_string_lossy().into_owned();
        let copy = dir.path().join(format!("{name}-at-rewind.replay"));
        let copy_s = copy.to_string_lossy().into_owned();
        let mut env: Vec<(&str, &str)> = vec![
            ("STONE_LIVE_REWIND", "1"),
            ("STONE_TIMELINE_SEEK_AT_FRAME", "40"),
            ("STONE_TIMELINE_SEEK_TO_TICK", "25"),
            ("STONE_LIVE_PLAY_AT_FRAME", "70"),
            ("STONE_KEYFRAME_INTERVAL", "10"),
            ("STONE_SAVE_REPRO_AT_FRAME", "41"),
        ];
        env.push(("STONE_SAVE_REPRO_PATH", Box::leak(copy_s.clone().into_boxed_str())));
        env.extend_from_slice(extra);
        let dump = run_record_dump(game, 120, &log_s, &env);
        // The log must be untouched between the rewind and shutdown: no branch
        // was requested, so the retained future is still exactly what it was.
        let at_rewind = std::fs::read(&copy_s).expect("from-boot copy taken at the rewind");
        let at_exit = std::fs::read(&log_s).expect("final log");
        assert_eq!(
            recorded_tick_lines(&copy_s), horizon as usize,
            "{name}: the copy must hold the whole retained future, else the compare is vacuous"
        );
        assert_eq!(
            at_rewind, at_exit,
            "{name}: rewinding and inspecting must not rewrite the log"
        );
        assert_eq!(recorded_tick_lines(&log_s), horizon as usize, "{name}: the future must stay retained");
        dump
    };

    // Arm A: play T -> H and stop at the horizon.
    let at_horizon = run("horizon", &[]);
    let diag = &at_horizon["__stone_diag"];
    assert_eq!(
        current_tick(&at_horizon), horizon,
        "playing the retained future must reach the horizon: {diag}"
    );
    assert!(
        diag["STONE_REPLAY_HORIZON_PAUSES"].as_i64().is_some_and(|n| n > 0),
        "reaching the horizon must pause the session, not end it: {diag}"
    );
    assert_eq!(diag["STONE_LIVE_REWIND_HORIZON"].as_i64(), Some(horizon));

    // The consumed future must be the recorded one: landing at H matches a plain
    // replay stepped to H.
    let log_a = dir.path().join("horizon.replay").to_string_lossy().into_owned();
    let reference_h = run_replay_dump(
        game, horizon as u32, &log_a,
        &[("STONE_START_PAUSED", "1"), ("STONE_STEP_FRAMES", &horizon.to_string())],
    );
    assert_eq!(current_tick(&reference_h), horizon);
    // `dt` is the instantaneous frame delta, 0 on a frame that did not tick --
    // a phase artifact between a session frozen AT the horizon and a reference
    // stepped to it. Same carve-out the A-B loop comparisons make.
    let mut sim = sim_state_names(game, horizon as u32);
    sim.remove("dt");
    assert_sim_state_matches(&at_horizon, &reference_h, &sim, "live-rewind horizon tick-40");

    // Arm B: from the paused horizon, scrub backward again.
    let rescrubbed = run(
        "rescrub",
        &[("STONE_TIMELINE_SEEK_AT_FRAME_2", "100"), ("STONE_TIMELINE_SEEK_TO_TICK_2", "30")],
    );
    let diag = &rescrubbed["__stone_diag"];
    assert!(
        diag["STONE_REPLAY_HORIZON_PAUSES"].as_i64().is_some_and(|n| n > 0),
        "arm B must reach the horizon before scrubbing back: {diag}"
    );
    assert_eq!(
        current_tick(&rescrubbed), 30,
        "the user must be able to scrub backward again from the horizon: {diag}"
    );
    assert_eq!(diag["STONE_SEEK_COUNT"].as_i64(), Some(2), "rewind + re-scrub are two seeks: {diag}");
    assert_eq!(diag["STONE_SEEK_FROM_TICK"].as_i64(), Some(horizon));
    assert_eq!(diag["STONE_SEEK_TO_TICK"].as_i64(), Some(30));

    let log_b = dir.path().join("rescrub.replay").to_string_lossy().into_owned();
    let reference_30 = run_replay_dump(
        game, horizon as u32, &log_b,
        &[("STONE_START_PAUSED", "1"), ("STONE_STEP_FRAMES", "30")],
    );
    assert_eq!(current_tick(&reference_30), 30);
    let mut sim_b = sim_state_names(game, horizon as u32);
    sim_b.remove("dt");
    assert_sim_state_matches(&rescrubbed, &reference_30, &sim_b, "re-scrub tick-30");
}

/// The author-facing surface: with live rewind opted in and admitted, the
/// existing replay timeline bar opens over the LIVE recording and spans the
/// retained history. Without the opt-in the bar stays closed, so a plain
/// recording pays nothing. After a rewind the bar's span is the whole recorded
/// log -- the stretch past `current_tick` is exactly the retained future the
/// author can still play into.
///
/// The bar's control wiring is what is asserted here; placement and styling are
/// author-verified, per the timeline bar's own convention.
#[test]
fn live_timeline_bar_exposes_retained_history_while_recording() {
    let game = "bitgun";
    let dir = tempfile::tempdir().unwrap();

    let armed_log = dir.path().join("bar-armed.replay").to_string_lossy().into_owned();
    let armed = run_record_dump(game, 30, &armed_log, &[("STONE_LIVE_REWIND", "1")]);
    let diag = &armed["__stone_diag"];
    assert!(
        diag["REPLAY_TIMELINE_DRAWS"].as_i64().is_some_and(|n| n > 0),
        "an armed live recording must open the timeline bar: {diag}"
    );
    // The bar is drawn inside the frame, before the tick counter advances at
    // frame end, so its last span is one behind the dumped `current_tick`. The
    // point is that it TRACKS the live tick rather than a fixed log length.
    assert_eq!(
        diag["REPLAY_TIMELINE_TOTAL"].as_i64(), Some(current_tick(&armed) - 1),
        "over a live recording the bar spans the retained history: {diag}"
    );

    let plain_log = dir.path().join("bar-plain.replay").to_string_lossy().into_owned();
    let plain = run_record_dump(game, 30, &plain_log, &[]);
    assert!(
        plain["__stone_diag"].get("REPLAY_TIMELINE_DRAWS").is_none(),
        "a recording without the opt-in must not open the bar: {}",
        plain["__stone_diag"]
    );

    // After the rewind the session is paused at T with the future retained; the
    // bar must show that future, not stop at where the author is standing.
    let rewound_log = dir.path().join("bar-rewound.replay").to_string_lossy().into_owned();
    let rewound = run_record_dump(
        game, 80, &rewound_log,
        &[
            ("STONE_LIVE_REWIND", "1"),
            ("STONE_TIMELINE_SEEK_AT_FRAME", "40"),
            ("STONE_TIMELINE_SEEK_TO_TICK", "25"),
        ],
    );
    let diag = &rewound["__stone_diag"];
    assert_eq!(current_tick(&rewound), 25);
    assert_eq!(diag["STONE_LIVE_REWIND_COUNT"].as_i64(), Some(1), "the bar's seek action must rewind: {diag}");
    assert_eq!(
        diag["REPLAY_TIMELINE_TOTAL"].as_i64(), Some(40),
        "at the paused historical tick the bar must span the retained future: {diag}"
    );
}

/// Every line of a replay log, and the lines up to and including the `n`th
/// recorded tick. The prefix form is what proves a branch re-rendered the
/// retained history through the same wire machinery that streamed it: `c` and
/// `r` lines have to land in the same places, not merely be present.
fn replay_lines(path: &str) -> Vec<String> {
    std::fs::read_to_string(path).expect("replay log").lines().map(str::to_string).collect()
}

fn prefix_through_nth_tick(path: &str, n: usize) -> Vec<String> {
    let lines = replay_lines(path);
    let mut seen = 0;
    let mut out = Vec::new();
    for l in lines {
        let is_tick = l.starts_with("t ");
        out.push(l);
        if is_tick {
            seen += 1;
            if seen == n {
                return out;
            }
        }
    }
    panic!("{path} holds fewer than {n} recorded ticks");
}

fn nth_tick_line(path: &str, n: usize) -> Option<String> {
    replay_lines(path).into_iter().filter(|l| l.starts_with("t ")).nth(n - 1)
}

/// The branch oracle. One run populates EVERY future-bearing surface past the
/// branch point -- input records, desync checkpoints, a resize, a reload marker,
/// a keyframe, the live horizon and the streamed file -- then rewinds without
/// truncating anything, branches explicitly at T, and keeps recording.
///
/// Each class asserts a measured drop (a count taken before and after, so a
/// class that stops being cut reports zero) and the retained prefix is compared
/// byte-for-byte against a copy of the log taken before the branch. The final
/// check is end-to-end: replay the rewritten from-boot file in a fresh process
/// and compare its simulation state with the live branched state.
#[test]
fn explicit_branch_truncates_every_future_bearing_surface_together() {
    let game = "bitgun";
    let branch_tick: i64 = 25;
    let horizon: i64 = 40;
    let dir = tempfile::tempdir().unwrap();
    let log_s = dir.path().join("branch.replay").to_string_lossy().into_owned();
    let before_s = dir.path().join("branch-before.replay").to_string_lossy().into_owned();

    let live = run_record_dump(
        game, 120, &log_s,
        &[
            ("STONE_LIVE_REWIND", "1"),
            ("STONE_CHECKPOINT_INTERVAL", "5"),
            ("STONE_RESIZE_AT_FRAME", "30"),      // a resize event past T
            ("STONE_RESIZE_W", "1000"),
            ("STONE_RESIZE_H", "700"),
            ("STONE_RELOAD_AT_FRAME", "32"),      // a reload marker past T
            ("STONE_TIMELINE_SEEK_AT_FRAME", "40"),
            ("STONE_TIMELINE_SEEK_TO_TICK", "25"),
            ("STONE_SAVE_REPRO_AT_FRAME", "41"),  // the log as it stood, unbranched
            ("STONE_SAVE_REPRO_PATH", &before_s),
            ("STONE_BRANCH_AT_FRAME", "70"),
        ],
    );
    let diag = &live["__stone_diag"];

    // The rewind alone changed nothing: the copy taken after it still holds the
    // whole future, so every "gone" assertion below has a real denominator.
    assert_eq!(recorded_tick_lines(&before_s), horizon as usize, "the pre-branch log must hold the future");
    assert!(
        replay_lines(&before_s).iter().any(|l| l.starts_with("r 3")),
        "the pre-branch log must carry a resize past T"
    );
    assert!(
        replay_lines(&before_s).iter().any(|l| l.starts_with("reload 3")),
        "the pre-branch log must carry a reload marker past T"
    );

    assert_eq!(diag["STONE_BRANCH_COUNT"].as_i64(), Some(1), "{diag}");
    assert_eq!(diag["STONE_BRANCH_TICK"].as_i64(), Some(branch_tick), "{diag}");
    assert_eq!(
        diag["STONE_BRANCH_DROPPED_RECORDS"].as_i64(), Some(horizon - branch_tick),
        "input records after T must go: {diag}"
    );
    for class in ["CHECKPOINTS", "RESIZES", "RELOADS", "KEYFRAMES"] {
        let key = format!("STONE_BRANCH_DROPPED_{class}");
        assert!(
            diag[&key].as_i64().is_some_and(|n| n > 0),
            "{class} after T must go, and the population must be nonzero: {diag}"
        );
    }
    assert_eq!(
        diag["STONE_KEYFRAME_COUNT"].as_i64(), Some(1),
        "only the keyframes at or before T survive: {diag}"
    );
    assert_eq!(
        diag["STONE_BRANCH_RECORDING_AFTER"].as_i64(), Some(1),
        "a branch must hand the session back to the recorder: {diag}"
    );
    assert_eq!(
        diag["STONE_BRANCH_HORIZON_AFTER"].as_i64(), Some(-1),
        "a branched session is no longer a rewound one, so it has no horizon: {diag}"
    );

    // The rewritten prefix is byte-identical to what was streamed, `c`/`r`
    // interleaving included -- one wire-format writer, not two.
    assert_eq!(
        prefix_through_nth_tick(&log_s, branch_tick as usize),
        prefix_through_nth_tick(&before_s, branch_tick as usize),
        "the branch must re-render the retained prefix exactly as it was streamed"
    );
    // New recording appends, and what it records discriminates from the future
    // that was discarded.
    let after = recorded_tick_lines(&log_s);
    assert!(after > branch_tick as usize, "the branched session must keep recording (got {after} ticks)");
    assert_ne!(
        nth_tick_line(&log_s, branch_tick as usize + 1),
        nth_tick_line(&before_s, branch_tick as usize + 1),
        "the first tick after the branch must not be the discarded future's"
    );

    // End to end: the rewritten from-boot file must reproduce the live branched
    // state in a fresh process. This second run is the SAME scenario minus the
    // window resize, because replaying any log that carries a mid-run resize
    // already diverges today -- replay overrides `WINDOW_SIZE` but never resizes
    // the real window, so anything reading the drawable drifts. That is a
    // pre-existing defect, pinned by
    // `replay_of_a_log_with_a_midrun_resize_desyncs_sentinel`, not something the
    // branch introduces: the branch re-emits the resize at the branch tick and
    // the assertions above cover its truncation.
    let replayable_s = dir.path().join("branch-replayable.replay").to_string_lossy().into_owned();
    let live2 = run_record_dump(
        game, 120, &replayable_s,
        &[
            ("STONE_LIVE_REWIND", "1"),
            ("STONE_CHECKPOINT_INTERVAL", "5"),
            ("STONE_RELOAD_AT_FRAME", "32"),
            ("STONE_TIMELINE_SEEK_AT_FRAME", "40"),
            ("STONE_TIMELINE_SEEK_TO_TICK", "25"),
            ("STONE_BRANCH_AT_FRAME", "70"),
        ],
    );
    let diag2 = &live2["__stone_diag"];
    assert_eq!(diag2["STONE_BRANCH_COUNT"].as_i64(), Some(1), "{diag2}");
    for class in ["RECORDS", "CHECKPOINTS", "RELOADS", "KEYFRAMES"] {
        let key = format!("STONE_BRANCH_DROPPED_{class}");
        assert!(
            diag2[&key].as_i64().is_some_and(|n| n > 0),
            "the replayable arm must also have had a {class} population past T: {diag2}"
        );
    }
    let after2 = recorded_tick_lines(&replayable_s);
    let replayed = run_replay_dump(game, after2 as u32 + 5, &replayable_s, &[]);
    assert_eq!(
        current_tick(&replayed), current_tick(&live2),
        "the branched log must replay to the live tick"
    );
    let mut sim = sim_state_names(game, 40);
    sim.remove("dt");
    assert_sim_state_matches(&live2, &replayed, &sim, "branched from-boot replay");
}

/// PINNED BUG. A replay applies a recorded resize by overriding `WINDOW_SIZE`
/// only -- it never resizes the real window -- so any state derived from the
/// actual drawable drifts from the recorded run, and the desync detector aborts
/// with exit 5. Correct behavior is for a replay of a log carrying an `r` line
/// to reproduce the recorded run exactly.
///
/// This is inverted: it asserts the divergence that happens today. When the
/// replay path starts resizing the window (or otherwise reproducing the
/// recorded drawable), this test goes red -- flip it to assert success and
/// delete the carve-out in
/// `explicit_branch_truncates_every_future_bearing_surface_together`.
#[test]
fn replay_of_a_log_with_a_midrun_resize_desyncs_sentinel() {
    let game = "bitgun";
    let dir = tempfile::tempdir().unwrap();
    let log_s = dir.path().join("resize.replay").to_string_lossy().into_owned();
    let scratch = tempfile::tempdir().unwrap();

    let rec = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame", game, "-n", "60", "--record", &log_s,
            "--output", &scratch.path().to_string_lossy(),
        ])
        .envs([
            ("STONE_CHECKPOINT_INTERVAL", "1"),
            ("STONE_RESIZE_AT_FRAME", "30"),
            ("STONE_RESIZE_W", "1000"),
            ("STONE_RESIZE_H", "700"),
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(workspace_root())
        .output()
        .expect("spawn record");
    assert!(rec.status.success(), "record failed: {}", String::from_utf8_lossy(&rec.stderr));
    assert!(
        replay_lines(&log_s).iter().any(|l| l.starts_with("r ")),
        "the fixture must actually carry a resize line"
    );

    let out = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame", game, "-n", "70", "--replay", &log_s,
            "--output", &scratch.path().to_string_lossy(),
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(workspace_root())
        .output()
        .expect("spawn replay");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success() && stderr.contains("desync"),
        "PINNED BEHAVIOR (BUG): replaying a log with a mid-run resize must desync today. \
         If this replay now succeeds, the fix landed -- flip this assertion and drop the \
         carve-out in the branch oracle. stderr: {stderr}"
    );
}

/// Atomicity. With the rewrite forced to fail, the branch must leave the session
/// exactly where it was: paused in replay, the old file valid and complete, the
/// future still playable, and no in-memory collection cut.
#[test]
fn a_failed_branch_rewrite_leaves_the_future_intact() {
    let game = "bitgun";
    let horizon: i64 = 40;
    let dir = tempfile::tempdir().unwrap();
    let log_s = dir.path().join("failbranch.replay").to_string_lossy().into_owned();
    let before_s = dir.path().join("failbranch-before.replay").to_string_lossy().into_owned();

    let live = run_record_dump(
        game, 140, &log_s,
        &[
            ("STONE_LIVE_REWIND", "1"),
            ("STONE_BRANCH_FAIL", "rewrite"),
            ("STONE_TIMELINE_SEEK_AT_FRAME", "40"),
            ("STONE_TIMELINE_SEEK_TO_TICK", "25"),
            ("STONE_SAVE_REPRO_AT_FRAME", "41"),
            ("STONE_SAVE_REPRO_PATH", &before_s),
            ("STONE_BRANCH_AT_FRAME", "70"),
            ("STONE_LIVE_PLAY_AT_FRAME", "75"),   // the future must still play
        ],
    );
    let diag = &live["__stone_diag"];
    assert_eq!(diag["STONE_BRANCH_FAILURES"].as_i64(), Some(1), "{diag}");
    assert!(diag.get("STONE_BRANCH_COUNT").is_none(), "a failed rewrite must not branch: {diag}");
    for class in ["RECORDS", "CHECKPOINTS", "RESIZES", "RELOADS", "KEYFRAMES"] {
        let key = format!("STONE_BRANCH_DROPPED_{class}");
        assert!(diag.get(&key).is_none(), "a failed rewrite must cut nothing ({class}): {diag}");
    }

    // The old file survives untouched and is still a valid from-boot log.
    assert_eq!(replay_lines(&log_s), replay_lines(&before_s), "a failed rewrite must not touch the log");
    assert_eq!(recorded_tick_lines(&log_s), horizon as usize);

    // Still in replay with the future playable: it plays to the horizon and
    // pauses there rather than exiting.
    assert_eq!(current_tick(&live), horizon, "the retained future must still play: {diag}");
    assert!(
        diag["STONE_REPLAY_HORIZON_PAUSES"].as_i64().is_some_and(|n| n > 0),
        "the session must still be a paused live rewind: {diag}"
    );
    let reference = run_replay_dump(
        game, horizon as u32, &log_s,
        &[("STONE_START_PAUSED", "1"), ("STONE_STEP_FRAMES", &horizon.to_string())],
    );
    let mut sim = sim_state_names(game, horizon as u32);
    sim.remove("dt");
    assert_sim_state_matches(&live, &reference, &sim, "unbranched future after a failed rewrite");
}

/// The consequence of cutting the in-memory input history: a SECOND rewind after
/// a branch must see the branched history, not the discarded one. The horizon it
/// reports is `#records`, so a records/cursor truncation that did not happen
/// shows up here as a horizon longer than the file.
#[test]
fn a_second_rewind_after_a_branch_sees_only_the_branched_history() {
    let game = "bitgun";
    let dir = tempfile::tempdir().unwrap();
    let log_s = dir.path().join("rebranch.replay").to_string_lossy().into_owned();

    let live = run_record_dump(
        game, 160, &log_s,
        &[
            ("STONE_LIVE_REWIND", "1"),
            ("STONE_TIMELINE_SEEK_AT_FRAME", "40"),
            ("STONE_TIMELINE_SEEK_TO_TICK", "25"),
            ("STONE_BRANCH_AT_FRAME", "70"),
            ("STONE_TIMELINE_SEEK_AT_FRAME_2", "110"),
            ("STONE_TIMELINE_SEEK_TO_TICK_2", "30"),
        ],
    );
    let diag = &live["__stone_diag"];
    assert_eq!(diag["STONE_BRANCH_COUNT"].as_i64(), Some(1), "{diag}");
    assert_eq!(diag["STONE_LIVE_REWIND_COUNT"].as_i64(), Some(2), "both rewinds must fire: {diag}");
    let horizon = diag["STONE_LIVE_REWIND_HORIZON"].as_i64().expect("horizon");
    assert!(horizon > 25, "the branched session must have recorded past T (got {horizon})");
    assert_eq!(
        horizon, recorded_tick_lines(&log_s) as i64,
        "the second rewind's horizon must be the branched history, not the discarded future: {diag}"
    );
    assert_eq!(current_tick(&live), 30, "the second rewind must land at its target: {diag}");
}

/// Transient correctness against REAL state, not just the seam's call count.
/// The probe keeps Stone's world/UI/meat particle arenas and a Stone tween
/// populated every tick and a music stream playing, so the jump has something to
/// clear; the same run carries a persistent game value written once at init,
/// which the keyframe restores and nothing re-derives -- so a reset that fired
/// every `@reset` would show up as a zero there.
///
/// Both arms of the ONE shared implementation are driven: a live rewind and an
/// A-B loop jump.
#[test]
fn playback_jumps_clear_real_stone_transients_and_keep_persistent_state() {
    let game = "survivors";
    let dir = tempfile::tempdir().unwrap();

    let assert_reset = |dump: &serde_json::Value, label: &str| {
        let diag = &dump["__stone_diag"];
        assert!(
            diag["STONE_TRANSIENT_BEFORE_PARTICLES"].as_i64().is_some_and(|n| n > 0),
            "{label}: the particle arenas must be populated at the jump: {diag}"
        );
        assert!(
            diag["STONE_TRANSIENT_BEFORE_TWEENS"].as_i64().is_some_and(|n| n > 0),
            "{label}: a tween must be active at the jump: {diag}"
        );
        assert_eq!(
            diag["STONE_TRANSIENT_BEFORE_MUSIC"].as_i64(), Some(1),
            "{label}: music must be playing at the jump: {diag}"
        );
        assert_eq!(diag["STONE_TRANSIENT_AFTER_PARTICLES"].as_i64(), Some(0), "{label}: {diag}");
        assert_eq!(diag["STONE_TRANSIENT_AFTER_TWEENS"].as_i64(), Some(0), "{label}: {diag}");
        assert_eq!(diag["STONE_TRANSIENT_AFTER_MUSIC"].as_i64(), Some(0), "{label}: {diag}");
        // The GAME-owned pool: a static local no keyframe images, so only the
        // game's registered `on_loop_jump` callback can empty it.
        assert!(
            diag["TRANSIENT_PROBE_POOL_AT_JUMP"].as_i64().is_some_and(|n| n > 0),
            "{label}: the game pool must be populated at the jump: {diag}"
        );
        assert_eq!(diag["TRANSIENT_PROBE_POOL_AFTER_JUMP"].as_i64(), Some(0), "{label}: {diag}");
        let persistent = dump
            .as_object()
            .expect("dump object")
            .values()
            .find_map(|g| g.as_object().and_then(|o| o.get("TRANSIENT_PROBE_PERSISTENT")))
            .expect("the persistent probe global must be dumped");
        assert_eq!(
            persistent.as_i64(), Some(4242),
            "{label}: the reset must not touch persistent game state the keyframe restored"
        );
    };

    // Arm 1: a live rewind.
    let live_log = dir.path().join("transient-live.replay").to_string_lossy().into_owned();
    let rewound = run_record_dump(
        game, 80, &live_log,
        &[
            ("ROCK_TEST_SEED", "42"),
            ("STONE_TRANSIENT_PROBE", "1"),
            ("STONE_QUIET", "0"),
            ("STONE_AUDIO_STUB", "0"),
            ("STONE_LIVE_REWIND", "1"),
            ("STONE_TIMELINE_SEEK_AT_FRAME", "40"),
            ("STONE_TIMELINE_SEEK_TO_TICK", "25"),
        ],
    );
    assert_eq!(rewound["__stone_diag"]["STONE_LIVE_REWIND_COUNT"].as_i64(), Some(1));
    assert_reset(&rewound, "live rewind");

    // Arm 2: an A-B loop jump over a replay of the same recording.
    let looped = run_replay_dump(
        game, 120, &live_log,
        &[
            ("STONE_TRANSIENT_PROBE", "1"),
            ("STONE_QUIET", "0"),
            ("STONE_AUDIO_STUB", "0"),
            ("STONE_LOOP_A", "10"),
            ("STONE_LOOP_B", "20"),
            ("STONE_KEYFRAME_INTERVAL", "10"),
        ],
    );
    assert!(
        looped["__stone_diag"]["STONE_LOOP_JUMPS"].as_i64().is_some_and(|n| n > 0),
        "the loop must jump: {}", looped["__stone_diag"]
    );
    assert_reset(&looped, "A-B loop jump");
}

/// Replay observability. Every counter asserts its own population in the same
/// run; the timings assert that work was measured, never a budget -- these are
/// wall-clock numbers on a shared machine.
///
/// The cadence overhead is an A/B and is REPORTED, not gated: the difference
/// between a cadence-on and cadence-off run is well inside the run-to-run
/// spread of a `rock frame` process here, so a threshold on it would be a
/// measurement of the machine.
#[test]
fn replay_reports_its_own_costs() {
    let game = "bitgun";
    let dir = tempfile::tempdir().unwrap();

    // Record a log to replay for the seek/eviction/cadence arms.
    let log_s = dir.path().join("costs.replay").to_string_lossy().into_owned();
    let scratch = tempfile::tempdir().unwrap();
    let rec = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame", game, "-n", "100", "--record", &log_s,
            "--output", &scratch.path().to_string_lossy(),
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(workspace_root())
        .output()
        .expect("spawn record");
    assert!(rec.status.success(), "record failed: {}", String::from_utf8_lossy(&rec.stderr));

    // Seek: the first RENDERED frame at the target, which the seek total hides.
    // Eviction: window 30 over a 100-tick log drops keyframes as it goes.
    let seeked = run_replay_dump(
        game, 100, &log_s,
        &[
            ("STONE_KEYFRAME_INTERVAL", "10"),
            ("STONE_KEYFRAME_WINDOW", "30"),
            ("STONE_SCRUB_AT_TICK", "90"),
            ("STONE_SCRUB_TO_TICK", "55"),
        ],
    );
    let diag = &seeked["__stone_diag"];
    assert_eq!(diag["STONE_SEEK_COUNT"].as_i64(), Some(1), "{diag}");
    assert_eq!(
        diag["STONE_SEEK_FIRST_FRAMES"].as_i64(), Some(1),
        "exactly one first-rendered-frame must be reported per seek: {diag}"
    );
    let first_ms = diag["STONE_SEEK_FIRST_FRAME_MS"].as_f64().expect("first frame ms");
    assert!(first_ms > 0.0, "the target frame must be measured: {diag}");
    let evictions = diag["STONE_KEYFRAME_EVICTIONS"].as_i64().expect("eviction count");
    assert!(evictions > 0, "the bounded window must have evicted something: {diag}");
    assert!(
        diag["STONE_KEYFRAME_EVICT_MS"].as_f64().is_some_and(|v| v > 0.0),
        "eviction time must be measured where evictions happened: {diag}"
    );
    let cadence_ms = diag["STONE_KEYFRAME_CADENCE_MS"].as_f64().expect("cadence ms");
    let cadence_n = diag["STONE_KEYFRAME_CADENCE_CAPTURES"].as_i64().expect("cadence captures");
    assert!(cadence_n > 0 && cadence_ms > 0.0, "{diag}");
    assert!(
        diag["STONE_KEYFRAME_GLOBALS"].as_i64().is_some_and(|n| n > 0)
            && diag["STONE_KEYFRAME_NODES"].as_i64().is_some_and(|n| n > 0),
        "a keyframe must report the population it copied: {diag}"
    );
    println!(
        "[replay-costs] seek first rendered frame {first_ms:.2}ms; {evictions} evictions in \
         {:.2}ms; {cadence_n} cadence captures in {cadence_ms:.2}ms",
        diag["STONE_KEYFRAME_EVICT_MS"].as_f64().unwrap()
    );

    // Cadence overhead, A/B and REPORTED. The off arm must have no cadence
    // population at all -- otherwise the two arms are the same run.
    let t_on = std::time::Instant::now();
    let on = run_replay_dump(game, 100, &log_s, &[("STONE_KEYFRAME_INTERVAL", "10")]);
    let on_ms = t_on.elapsed().as_secs_f64() * 1000.0;
    let t_off = std::time::Instant::now();
    let off = run_replay_dump(game, 100, &log_s, &[("STONE_KEYFRAME_INTERVAL", "0")]);
    let off_ms = t_off.elapsed().as_secs_f64() * 1000.0;
    let on_n = on["__stone_diag"]["STONE_KEYFRAME_CADENCE_CAPTURES"].as_i64().unwrap_or(0);
    assert!(on_n > 0, "the cadence-on arm must capture: {}", on["__stone_diag"]);
    assert!(
        off["__stone_diag"].get("STONE_KEYFRAME_CADENCE_CAPTURES").is_none(),
        "the cadence-off arm must capture nothing, else the A/B compares one thing with itself: {}",
        off["__stone_diag"]
    );
    // Control for the seek counter above: a run that never seeks must report no
    // first-frame at all, so the "exactly one" is a count and not a constant.
    assert!(
        off["__stone_diag"].get("STONE_SEEK_FIRST_FRAMES").is_none(),
        "a run with no seek must report no first-rendered-frame: {}",
        off["__stone_diag"]
    );
    let in_engine = on["__stone_diag"]["STONE_KEYFRAME_CADENCE_MS"].as_f64().unwrap_or(0.0);
    println!(
        "[replay-costs] cadence A/B over 100 ticks: {on_n} captures, {in_engine:.2}ms in-engine; \
         process wall clock {on_ms:.0}ms on vs {off_ms:.0}ms off (reported, not gated -- process \
         spread here dwarfs the difference)"
    );

    // Save and branch rewrite: time and bytes, on a live rewound session.
    let branch_log = dir.path().join("costs-branch.replay").to_string_lossy().into_owned();
    let copy_s = dir.path().join("costs-copy.replay").to_string_lossy().into_owned();
    let branched = run_record_dump(
        game, 120, &branch_log,
        &[
            ("STONE_LIVE_REWIND", "1"),
            ("STONE_TIMELINE_SEEK_AT_FRAME", "40"),
            ("STONE_TIMELINE_SEEK_TO_TICK", "25"),
            ("STONE_SAVE_REPRO_AT_FRAME", "41"),
            ("STONE_SAVE_REPRO_PATH", &copy_s),
            ("STONE_BRANCH_AT_FRAME", "70"),
        ],
    );
    let diag = &branched["__stone_diag"];
    assert_eq!(diag["STONE_REPLAY_SAVES"].as_i64(), Some(1), "{diag}");
    let save_bytes = diag["STONE_REPLAY_SAVE_BYTES"].as_i64().expect("save bytes");
    assert_eq!(
        save_bytes as u64, std::fs::metadata(&copy_s).unwrap().len(),
        "the reported save size must be the file that was written"
    );
    assert!(diag["STONE_REPLAY_SAVE_MS"].as_f64().is_some_and(|v| v > 0.0), "{diag}");
    let rewrite_bytes = diag["STONE_BRANCH_REWRITE_BYTES"].as_i64().expect("rewrite bytes");
    assert!(rewrite_bytes > 0 && rewrite_bytes < save_bytes, "a branch must shrink the log: {diag}");
    let rewrite_ms = diag["STONE_BRANCH_REWRITE_MS"].as_f64().expect("rewrite ms");
    assert!(rewrite_ms > 0.0, "{diag}");
    println!(
        "[replay-costs] save {save_bytes}B in {:.2}ms; branch rewrite to {rewrite_bytes}B in \
         {rewrite_ms:.2}ms",
        diag["STONE_REPLAY_SAVE_MS"].as_f64().unwrap()
    );

    // Keyframe BYTES are only honest where real memory was imaged. bitgun holds
    // no buffer storage in sim state, so its figure is a truthful zero; bopt
    // holds a NativeBuffer, so its keyframe reports the bytes it copied.
    let seek_diag = &seeked["__stone_diag"];
    assert_eq!(
        seek_diag.get("STONE_KEYFRAME_STORAGE_BYTES").and_then(serde_json::Value::as_i64),
        Some(0),
        "bitgun images no buffer storage, so its byte figure must be a real zero: {seek_diag}"
    );
    let bopt_log = dir.path().join("bopt.replay").to_string_lossy().into_owned();
    let bopt_rec = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame", "bopt", "-n", "25", "--record", &bopt_log,
            "--output", &scratch.path().to_string_lossy(),
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(workspace_root())
        .output()
        .expect("spawn bopt record");
    assert!(bopt_rec.status.success(), "bopt record failed: {}", String::from_utf8_lossy(&bopt_rec.stderr));
    let bopt = run_replay_dump("bopt", 30, &bopt_log, &[("STONE_KEYFRAME_INTERVAL", "10")]);
    let bytes = bopt["__stone_diag"]["STONE_KEYFRAME_STORAGE_BYTES"].as_i64().expect("storage bytes");
    assert!(
        bytes > 0,
        "bopt holds buffer storage in sim state, so its keyframe must report imaged bytes: {}",
        bopt["__stone_diag"]
    );
    println!("[replay-costs] bopt keyframe imaged {bytes} bytes of buffer storage");
}

fn keyframe_count(s: &serde_json::Value) -> i64 {
    s["__stone_diag"]["STONE_KEYFRAME_COUNT"].as_i64().expect("STONE_KEYFRAME_COUNT")
}

/// The bitgun probe's lambda-driven counter, found by name across packages (the
/// dump groups by package; we don't hardcode the game package key). Panics if
/// absent -- the gate must be set for this global to be driven.
fn lambda_counter(s: &serde_json::Value) -> i64 {
    for (_pkg, globals) in s.as_object().expect("dump object") {
        if let Some(v) = globals.as_object().and_then(|o| o.get("lambda_state_counter")) {
            return v.as_i64().expect("lambda_state_counter must be an int");
        }
    }
    panic!("lambda_state_counter not present in dump-state");
}

/// docs/lambda-in-state-snapshot-plan.md T1: the engine keyframe walk (`dc` in
/// stone/engine/snapshot.rock) must deep-copy a CAPTURING lambda held in a
/// registered global -- an env record, `setmetatable({__fn=.., c<N>=..},
/// __rock_fn_mt)` -- and keep it callable AND capture-isolated across a restore.
/// The bitgun probe (`lambda_snapshot_probe.rock`, gated by BITGUN_LAMBDA_STATE)
/// plants such a lambda plus a plain int counter it drives each tick.
///
/// Oracle is the COUNTER, never the serialized lambda: dump-state renders env
/// records as "<function>" (capture-blind), so a state-comparison on the lambda
/// itself passes even when the fix is absent. The counter's post-restore
/// trajectory observes the lambda's behavior instead.
///
/// Vehicle is SCRUB, not plain RESTORE_AT: a backward scrub restores the nearest
/// keyframe AND resims forward, invoking the restored lambda each resim tick;
/// RESTORE_AT pauses immediately, so an uncallable copy (T2(a) without the
/// setmetatable re-attach) would never be called and the break-verify in the
/// plan's verification section could not fail. RED progression:
///   - today: snapshot_take errors on the metatable reject -> record/replay exit
///     nonzero -> run_replay_dump's success assert fails.
///   - after T2(a) alone: the resim calls an uncallable table -> @update aborts
///     mid-frame -> the counter stalls != reference.
///   - green: only once T2(b) re-attaches __rock_fn_mt on the copy.
#[test]
fn lambda_in_state_survives_keyframe_scrub() {
    let game = "bitgun";
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("lambda.replay");
    let log_s = log.to_string_lossy().into_owned();
    let scratch = tempfile::tempdir().unwrap();

    let rec = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame", game, "-n", "60", "--record", &log_s,
            "--output", &scratch.path().to_string_lossy(),
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(workspace_root())
        .output()
        .expect("spawn record");
    assert!(rec.status.success(), "record failed: {}", String::from_utf8_lossy(&rec.stderr));

    // Reference: the true tick-25 state with the lambda driving the counter from
    // boot. (Cadence keyframe at tick 0 walks the record here too -- so today
    // this run ALSO errors on the reject, which is fine: RED is RED.)
    let reference = run_replay_dump(
        game, 60, &log_s,
        &[
            ("BITGUN_LAMBDA_STATE", "1"),
            ("STONE_START_PAUSED", "1"), ("STONE_STEP_FRAMES", "25"),
        ],
    );
    assert_eq!(current_tick(&reference), 25, "reference must sit at tick 25");

    // Scrub run: play to 40, scrub back to 25 -> restore keyframe 20 and resim
    // 20..25 THROUGH the restored lambda record.
    let scrubbed = run_replay_dump(
        game, 60, &log_s,
        &[
            ("BITGUN_LAMBDA_STATE", "1"),
            ("STONE_KEYFRAME_INTERVAL", "10"),
            ("STONE_SCRUB_AT_TICK", "40"),
            ("STONE_SCRUB_TO_TICK", "25"),
        ],
    );
    assert_eq!(current_tick(&scrubbed), 25, "after scrubbing back to 25, current_tick must be 25");

    // The oracle: the lambda-driven counter must be non-trivial (the lambda
    // actually ran) and match the plain run-to-25 exactly -- the restored lambda
    // was callable and its capture isolated. NOT a comparison of the serialized
    // lambda global (dump-state renders it "<function>" either way).
    let ref_counter = lambda_counter(&reference);
    let scrub_counter = lambda_counter(&scrubbed);
    assert!(ref_counter > 0, "reference lambda must have driven the counter (got {ref_counter})");
    assert_eq!(
        scrub_counter, ref_counter,
        "post-scrub counter must match the plain run-to-25 (got {scrub_counter} vs {ref_counter}); \
         a stalled counter means the restored lambda was uncallable (lost __rock_fn_mt)"
    );
}

/// Loaded audio resources (MultiSound/Sound handles) are NOT simulation state:
/// the plan excludes audio from keyframes ("never rewinds; mute during scrub,
/// resume live"), the same carve-out render resources already get. bitgun's
/// `sfx_shotgun` is a `Sound` handle -- it must be absent from the sim-state
/// predicate's output (so it is neither snapshotted nor desync-hashed), while a
/// real gameplay global (`MOBS`) stays present.
#[test]
fn audio_resources_excluded_from_sim_state() {
    let sim = sim_state_names("bitgun", 3);
    assert!(
        sim.contains("MOBS"),
        "control: gameplay global MOBS must be sim state (got {} names)",
        sim.len()
    );
    assert!(
        !sim.contains("sfx_shotgun"),
        "audio Sound handle sfx_shotgun must be excluded from sim state"
    );
}

/// P5: a hot reload during replay invalidates keyframes (new code may have
/// changed state shapes, so old-code keyframes are unusable). We inject a reload
/// mid-replay (STONE_RELOAD_AT_FRAME, which during replay routes to the P5
/// handler) and assert the keyframe buffer is smaller than an identical run
/// without the reload, and that the reload was counted.
#[test]
fn reload_during_replay_invalidates_keyframes() {
    let game = "bitgun";
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("reload.replay");
    let log_s = log.to_string_lossy().into_owned();
    let scratch = tempfile::tempdir().unwrap();

    let rec = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame", game, "-n", "50", "--record", &log_s,
            "--output", &scratch.path().to_string_lossy(),
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(workspace_root())
        .output()
        .expect("spawn record");
    assert!(rec.status.success(), "record failed: {}", String::from_utf8_lossy(&rec.stderr));

    // Control: normal replay, cadence 10 -> keyframes accumulate to ~6.
    let control = run_replay_dump(game, 50, &log_s, &[("STONE_KEYFRAME_INTERVAL", "10")]);
    let control_kf = keyframe_count(&control);
    assert!(control_kf >= 5, "control should accumulate keyframes (got {control_kf})");
    assert_eq!(
        control["__stone_diag"]["STONE_REPLAY_RELOADS"].as_i64().unwrap_or(0), 0,
        "control run must not report a reload"
    );

    // Reloaded: same, but inject a reload at frame 48 -> keyframes invalidated,
    // so only the few captured after the reload remain.
    let reloaded = run_replay_dump(
        game, 50, &log_s,
        &[("STONE_KEYFRAME_INTERVAL", "10"), ("STONE_RELOAD_AT_FRAME", "48")],
    );
    assert!(
        reloaded["__stone_diag"]["STONE_REPLAY_RELOADS"].as_i64().unwrap_or(0) >= 1,
        "a reload during replay must be counted"
    );
    let reloaded_kf = keyframe_count(&reloaded);
    assert!(
        reloaded_kf < control_kf,
        "reload must invalidate keyframes: reloaded {reloaded_kf} should be < control {control_kf}"
    );
}

/// P6: the A-B loop. Enabling with A=10,B=20 keyframes A, plays A->B rendered,
/// and at B restores(A) and repeats -- each jump is a snapshot restore, not a
/// resim. Over 120 frames the loop must fire several times, current_tick must
/// stay inside [A,B], and -- the correctness point -- the landing state must be
/// byte-identical to a plain run-to that tick (the loop re-derives A->T
/// deterministically from the restored keyframe). The restore-ms timer must also
/// be populated (the A-jump seam measurement).
#[test]
fn ab_loop_cycles_and_restores_correct_state() {
    let game = "bitgun";
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("loop.replay");
    let log_s = log.to_string_lossy().into_owned();
    let scratch = tempfile::tempdir().unwrap();

    let rec = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame", game, "-n", "60", "--record", &log_s,
            "--output", &scratch.path().to_string_lossy(),
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(workspace_root())
        .output()
        .expect("spawn record");
    assert!(rec.status.success(), "record failed: {}", String::from_utf8_lossy(&rec.stderr));

    let looped = run_replay_dump(
        game, 120, &log_s,
        &[("STONE_LOOP_A", "10"), ("STONE_LOOP_B", "20"), ("STONE_KEYFRAME_INTERVAL", "10")],
    );
    let jumps = looped["__stone_diag"]["STONE_LOOP_JUMPS"].as_i64().unwrap_or(0);
    assert!(jumps >= 3, "the A-B loop must cycle several times over 120 frames (jumps={jumps})");
    let t = current_tick(&looped);
    assert!((10..=20).contains(&t), "loop must keep current_tick in [10,20] (got {t})");
    assert!(
        looped["__stone_diag"]["STONE_RESTORE_MS"].as_f64().unwrap_or(0.0) > 0.0,
        "the A-jump restore-ms timer must be populated"
    );

    // Correctness: the loop's landing state must match a plain run-to that tick.
    let reference = run_replay_dump(
        game, 60, &log_s,
        &[("STONE_START_PAUSED", "1"), ("STONE_STEP_FRAMES", &t.to_string())],
    );
    assert_eq!(current_tick(&reference), t, "reference must sit at the loop's tick {t}");
    // `dt` is the instantaneous frame delta -- 0 on the paused reference, the
    // replayed value on the mid-play loop; a phase artifact, not divergence
    // (accumulated `time` still matches). Exclude it, like the PROFILER carve-out.
    let mut sim = sim_state_names(game, 60);
    sim.remove("dt");
    assert_sim_state_matches(&looped, &reference, &sim, &format!("loop tick-{t}"));
}

/// P6: the loop-boundary reload gate. A body-edit reload during an active loop
/// invalidates all keyframes EXCEPT the boot anchor (tick 0), so the plain
/// restore(A) at the next boundary would fail; the loop instead re-derives A
/// (restore boot -> resim boot->A -> re-snapshot) and keeps cycling. We inject a
/// reload at frame 45 (well into the loop) and assert the loop keeps jumping and
/// stays in [A,B] with correct state -- without the re-derive it would wedge the
/// moment A's keyframe is gone.
#[test]
fn ab_loop_survives_incremental_reload_via_rederive() {
    let game = "bitgun";
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("loopreload.replay");
    let log_s = log.to_string_lossy().into_owned();
    let scratch = tempfile::tempdir().unwrap();

    let rec = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame", game, "-n", "80", "--record", &log_s,
            "--output", &scratch.path().to_string_lossy(),
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(workspace_root())
        .output()
        .expect("spawn record");
    assert!(rec.status.success(), "record failed: {}", String::from_utf8_lossy(&rec.stderr));

    let looped = run_replay_dump(
        game, 150, &log_s,
        &[
            ("STONE_LOOP_A", "10"), ("STONE_LOOP_B", "20"),
            ("STONE_KEYFRAME_INTERVAL", "10"),
            ("STONE_RELOAD_AT_FRAME", "45"),   // incremental (STONE_RELOAD_FULL unset)
        ],
    );
    assert_eq!(
        looped["__stone_diag"]["STONE_REPLAY_RELOADS"].as_i64().unwrap_or(0), 1,
        "the injected reload must have fired once"
    );
    let jumps = looped["__stone_diag"]["STONE_LOOP_JUMPS"].as_i64().unwrap_or(0);
    // ~3-4 jumps happen before the frame-45 reload; requiring >= 6 proves the
    // loop kept cycling AFTER it (via the re-derive), not just before.
    assert!(jumps >= 6, "loop must keep cycling across the reload (jumps={jumps})");
    let t = current_tick(&looped);
    assert!((10..=20).contains(&t), "loop must stay in [10,20] after a reload (got {t})");

    let reference = run_replay_dump(
        game, 80, &log_s,
        &[("STONE_START_PAUSED", "1"), ("STONE_STEP_FRAMES", &t.to_string())],
    );
    assert_eq!(current_tick(&reference), t, "reference must sit at tick {t}");
    // Exclude `dt` (phase artifact: paused reference 0 vs mid-play loop) as above.
    let mut sim = sim_state_names(game, 80);
    sim.remove("dt");
    assert_sim_state_matches(&looped, &reference, &sim, &format!("post-reload loop tick-{t}"));
}

/// Review fix 2: a loop wider than the keyframe window must NOT let eviction drop
/// its A keyframe (nor the tick-0 boot anchor). With A=10, B=90, window=30, at
/// tick ~90 the window floor is ~60, so a naive evict would drop A=10 and wedge
/// the next jump. A is exempt from eviction, so the loop keeps cycling in [A,B]
/// with correct state -- never silently diverging past B.
#[test]
fn wide_loop_keyframe_survives_eviction() {
    let game = "bitgun";
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("wideloop.replay");
    let log_s = log.to_string_lossy().into_owned();
    let scratch = tempfile::tempdir().unwrap();

    let rec = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame", game, "-n", "100", "--record", &log_s,
            "--output", &scratch.path().to_string_lossy(),
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(workspace_root())
        .output()
        .expect("spawn record");
    assert!(rec.status.success(), "record failed: {}", String::from_utf8_lossy(&rec.stderr));

    let looped = run_replay_dump(
        game, 250, &log_s,
        &[
            ("STONE_LOOP_A", "10"), ("STONE_LOOP_B", "90"),
            ("STONE_KEYFRAME_INTERVAL", "10"),
            ("STONE_KEYFRAME_WINDOW", "30"),   // narrower than the loop span (80)
        ],
    );
    let jumps = looped["__stone_diag"]["STONE_LOOP_JUMPS"].as_i64().unwrap_or(0);
    assert!(jumps >= 2, "wide loop must keep cycling despite eviction (jumps={jumps})");
    let t = current_tick(&looped);
    assert!(
        (10..=90).contains(&t),
        "wide loop must stay in [10,90]; escaping means A was evicted and the loop wedged (got {t})"
    );

    let reference = run_replay_dump(
        game, 100, &log_s,
        &[("STONE_START_PAUSED", "1"), ("STONE_STEP_FRAMES", &t.to_string())],
    );
    assert_eq!(current_tick(&reference), t, "reference must sit at tick {t}");
    let mut sim = sim_state_names(game, 100);
    sim.remove("dt");
    assert_sim_state_matches(&looped, &reference, &sim, &format!("wide loop tick-{t}"));
}

/// P4 item 4: a bounded keyframe ring buffer. STONE_KEYFRAME_WINDOW (ticks, 0 =
/// unbounded) evicts keyframes older than the floor -- the newest keyframe <=
/// (now - window) -- so a long session's keyframe memory is bounded while a tick
/// at (or just below) the window start stays restorable, because the ONE
/// keyframe below the window start is retained. With interval 10 + window 30, the
/// scrub fires at tick 90 where the last eviction floor was the tick-50 keyframe
/// (nearest <= 89-30); scrubbing to 55 restores exactly that floor keyframe
/// (nearest <= 55 == 50). Landing at 55 proves the floor was RETAINED --
/// over-eviction that dropped it would leave no keyframe <= 55, so the scrub
/// would not fire and current_tick would stay at 90.
#[test]
fn bounded_keyframe_window_evicts_but_keeps_window_restorable() {
    let game = "bitgun";
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("win.replay");
    let log_s = log.to_string_lossy().into_owned();
    let scratch = tempfile::tempdir().unwrap();

    let rec = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame", game, "-n", "100", "--record", &log_s,
            "--output", &scratch.path().to_string_lossy(),
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(workspace_root())
        .output()
        .expect("spawn record");
    assert!(rec.status.success(), "record failed: {}", String::from_utf8_lossy(&rec.stderr));

    let reference = run_replay_dump(
        game, 100, &log_s,
        &[("STONE_START_PAUSED", "1"), ("STONE_STEP_FRAMES", "55")],
    );
    assert_eq!(current_tick(&reference), 55, "reference must sit at tick 55");

    // Unbounded control: normal forward play (NOT a seek -- seeking skips cadence
    // capture) over the 100-tick log -> keyframes at 0,10,..,100 (~11).
    let unbounded = run_replay_dump(
        game, 100, &log_s,
        &[("STONE_KEYFRAME_INTERVAL", "10")],
    );
    let unbounded_kf = keyframe_count(&unbounded);
    assert!(unbounded_kf >= 8, "unbounded control should retain ~10 keyframes (got {unbounded_kf})");

    // Bounded + scrub: window 30, play to 90 then scrub back to 75.
    let bounded = run_replay_dump(
        game, 100, &log_s,
        &[
            ("STONE_KEYFRAME_INTERVAL", "10"),
            ("STONE_KEYFRAME_WINDOW", "30"),
            ("STONE_SCRUB_AT_TICK", "90"),
            ("STONE_SCRUB_TO_TICK", "55"),
        ],
    );
    assert_eq!(
        current_tick(&bounded), 55,
        "bounded scrub must land at tick 55 (the floor keyframe at 50 must survive eviction)"
    );
    let bounded_kf = keyframe_count(&bounded);
    assert!(
        bounded_kf <= 5,
        "window 30 must bound the keyframe buffer well below the unbounded {unbounded_kf} (got {bounded_kf})"
    );
    let sim = sim_state_names(game, 100);
    assert_sim_state_matches(&bounded, &reference, &sim, "tick-55");
}
