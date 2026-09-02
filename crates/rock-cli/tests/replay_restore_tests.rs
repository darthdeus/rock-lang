//! P4 of docs/replay-scrub-plan.md: keyframe snapshot + restore. This is the
//! Phase-A RESTORE ORACLE, written RED before any restore machinery exists so
//! Phase B's restore path turns it green (TDD, per the arc's discipline).
//!
//! Contract: snapshotting the registry at tick T and restoring it later must
//! return the game EXACTLY to tick-T sim state -- same `current_tick`, and every
//! simulation-state global byte-identical to a plain run-to-T. The equality
//! oracle is the engine's own StateImagePlan world-membership output (the
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

/// Sim-state global names = the engine predicate's output, resolved to the
/// logical names used by dump-state rather than re-listed in Rust.
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
        .map(|identity| identity.rsplit_once("::").map_or(identity, |(_, name)| name).to_string())
        .collect()
}

#[test]
fn replay_snapshot_oracle_includes_effective_pad_state_only() {
    let names = sim_state_names("bitgun", 3);
    assert!(names.contains("pad_down"), "effective pad buttons must be replay state: {names:?}");
    assert!(names.contains("pad_axes"), "effective pad axes must be replay state: {names:?}");
    assert!(names.contains("ACTION_RUNTIME"), "action runtime must be replay state: {names:?}");
    assert!(!names.contains("physical_pad_down"), "physical pad lane must not be snapshotted: {names:?}");
    assert!(!names.contains("physical_pad_active_instance"), "controller handles must not be snapshotted: {names:?}");
    assert!(!names.contains("replay_pad_down"), "replay scratch must not be snapshotted: {names:?}");
    assert!(!names.contains("replay_scancodes_down"), "replay keyboard scratch must not be snapshotted: {names:?}");
    assert!(!names.contains("pad_axis_motion_this_frame"), "capture scratch must not be snapshotted: {names:?}");
    assert!(!names.contains("ACTIONS"), "binding configuration must not be snapshotted: {names:?}");
    assert!(!names.contains("ACTION_NAMES"), "binding registry scratch must not be snapshotted: {names:?}");
    assert!(!names.contains("BINDING_RAW_ACTIONS"), "unknown binding scratch must not be snapshotted: {names:?}");
    for name in [
        "BINDING_RAW_ACTION_BYTES",
        "BINDING_RAW_ACTION_KEYS",
        "REBIND_CAPTURE",
        "REBIND_CAPTURE_CANCELLED",
        "REBIND_SNAPSHOT_SCANCODES_DOWN",
        "REBIND_SNAPSHOT_SCANCODES_PRESSED",
        "REBIND_SNAPSHOT_MOUSE_DOWN",
        "REBIND_SNAPSHOT_MOUSE_PRESSED",
        "REBIND_SNAPSHOT_PAD_DOWN",
        "REBIND_SNAPSHOT_PAD_PRESSED",
        "REBIND_SNAPSHOT_AXES",
        "REBIND_SNAPSHOT_AXIS_MOTION",
    ] {
        assert!(!names.contains(name), "input host projection {name} was snapshotted: {names:?}");
    }
    assert!(!names.contains("LAST_INPUT_DEVICE"), "glyph modality must not be snapshotted: {names:?}");
    assert!(!names.contains("UI_INPUT_CAPTURE"), "capture mode must not be snapshotted: {names:?}");
    for name in [
        "RENDER_BATCH_SORT_KEYS",
        "UI_RENDER_SORT_KEYS",
        "UI_BATCH_KEY_BUILDS",
        "WORLD_BATCH_KEY_BUILDS",
    ] {
        assert!(!names.contains(name), "renderer flush scratch {name} was snapshotted: {names:?}");
    }
    assert!(
        !names.contains("TEXT_MISSING_GLYPH_POLICY"),
        "process-derived TEXT_MISSING_GLYPH_POLICY was snapshotted: {names:?}"
    );
}

#[test]
fn mag_audio_and_vfx_projections_are_not_world_image_state() {
    let names = sim_state_names("mag", 3);
    assert!(!names.is_empty(), "the Mag checkpoint census must examine a nonzero population");
    assert!(names.contains("pad_down"), "the Mag census must include effective input state: {names:?}");
    for name in [
        "SFX_SPELL_FLAMESTRIKE_HIT",
        "SFX_CAST_LIGHTNING",
        "AMBIENCE_VOICE",
        "STONE_BUILD_SHA",
        "ui_effect_composite_shader",
        "UI_RASTER_CANVAS",
        "UI_OUTPUT_CANVAS",
        "UI_EFFECT_COMPOSITE_PIPELINE",
        "RENDER_BATCH_SORT_KEYS",
        "UI_RENDER_SORT_KEYS",
        "UI_BATCH_KEY_BUILDS",
        "WORLD_BATCH_KEY_BUILDS",
    ] {
        assert!(!names.contains(name), "host projection {name} leaked into WorldImage: {names:?}");
    }
    assert!(
        !names.contains("TEXT_MISSING_GLYPH_POLICY"),
        "process-derived TEXT_MISSING_GLYPH_POLICY leaked into WorldImage: {names:?}"
    );
}

#[test]
fn mag_seek_census_admits_aggregate_owners_not_superseded_direct_globals() {
    let names = sim_state_names("mag", 3);
    assert!(!names.is_empty(), "the Mag checkpoint census must examine a nonzero population");
    assert_mag_seek_census_owners(&names);
}

fn assert_mag_panel_plan_census(sim: &std::collections::HashSet<String>) {
    assert!(
        !sim.contains("PANEL_DRAW_PLAN"),
        "PANEL_DRAW_PLAN is a rebuildable catalog cache, not WorldImage state"
    );
    assert!(
        !sim.contains("PANEL_DRAW_PLAN_BUILDS"),
        "PANEL_DRAW_PLAN_BUILDS is a diagnostic rebuild counter, not WorldImage state"
    );
    assert!(
        sim.contains("PANEL_STATES"),
        "PANEL_STATES must remain semantic panel ownership"
    );
    assert!(
        sim.contains("PANEL_SEQ"),
        "PANEL_SEQ must remain semantic panel ownership"
    );
}

#[test]
fn mag_panel_draw_plan_is_derived_not_world_image() {
    let names = sim_state_names("mag", 3);
    assert!(!names.is_empty(), "the Mag panel-plan census must examine a nonzero population");
    assert_mag_panel_plan_census(&names);
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
    let observed = run_record_dump_observed(game, frames, log, env);
    assert!(
        observed.status_success,
        "live record failed: {}",
        observed.stderr
    );
    observed.state
}

struct ObservedRecordRun {
    status_success: bool,
    stderr: String,
    state: serde_json::Value,
}

fn run_record_dump_observed(
    game: &str,
    frames: u32,
    log: &str,
    env: &[(&str, &str)],
) -> ObservedRecordRun {
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
    let state = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "live record dump-state JSON ({e}); stderr:\n{}",
            String::from_utf8_lossy(&out.stderr)
        )
    });
    ObservedRecordRun {
        status_success: out.status.success(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        state,
    }
}

fn current_tick(s: &serde_json::Value) -> i64 {
    s["stone"]["current_tick"].as_i64().expect("current_tick")
}

fn strip_scheduler_frame_stamps(value: &mut serde_json::Value) -> usize {
    match value {
        serde_json::Value::Array(values) => values.iter_mut().map(strip_scheduler_frame_stamps).sum(),
        serde_json::Value::Object(fields) => {
            let removed = usize::from(fields.remove("last_tick_frame").is_some());
            removed + fields.values_mut().map(strip_scheduler_frame_stamps).sum::<usize>()
        }
        _ => 0,
    }
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
            let mut a = val.clone();
            let mut b = want_pkg[name].clone();
            strip_scheduler_frame_stamps(&mut a);
            strip_scheduler_frame_stamps(&mut b);
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

/// Current MAG seek census owners, plus the superseded direct globals they
/// replaced. Both MAG seek census consumers must call this rather than
/// re-listing the names.
fn assert_mag_seek_census_owners(sim: &std::collections::HashSet<String>) {
    for name in [
        "MAG_ACTIVE_WORLD",
        "MAG_PROGRESS",
        "MAG_JOURNAL",
        "MAG_RUN_UI",
        "history",
    ] {
        assert!(sim.contains(name), "{name} must be admitted as simulation state");
    }
    for (name, owner) in [
        ("bank_gold", "MAG_PROGRESS"),
        ("journal", "MAG_JOURNAL"),
        ("quest_log_open", "MAG_RUN_UI"),
    ] {
        assert!(
            !sim.contains(name),
            "{name} is a superseded direct global; sim-state belongs to {owner}"
        );
    }
    assert!(
        !sim.contains("player"),
        "Player ownership must remain inside MAG_ACTIVE_WORLD, not a duplicate global"
    );
}

fn assert_mag_seek_state_matches(
    got: &serde_json::Value,
    want: &serde_json::Value,
    sim: &std::collections::HashSet<String>,
    label: &str,
) {
    assert_mag_seek_census_owners(sim);
    let mut exact = sim.clone();
    assert!(
        exact.remove("WISPS"),
        "WISPS must be admitted as simulation state"
    );
    assert_f32_state_matches(&got["mag"]["WISPS"], &want["mag"]["WISPS"], "mag.WISPS");
    assert_mag_seek_populations_match(got, want, label);
    assert_sim_state_matches(got, want, &exact, label);
}

fn assert_mag_seek_populations_match(
    got: &serde_json::Value,
    want: &serde_json::Value,
    label: &str,
) {
    for key in [
        "MAG_REPLAY_DROP_PARTICLES",
        "MAG_REPLAY_DROP_FIRE",
        "MAG_REPLAY_DROP_MEND",
        "MAG_REPLAY_WISPS",
        "MAG_REPLAY_WISP_TRAIL",
    ] {
        assert_eq!(
            got["__stone_diag"][key],
            want["__stone_diag"][key],
            "Mag replay population {key} must match {label}: got={} want={}",
            got["__stone_diag"],
            want["__stone_diag"]
        );
    }
}

fn assert_mag_seek_populations_are_nonzero(state: &serde_json::Value) {
    let diag = &state["__stone_diag"];
    for (key, label) in [
        ("MAG_REPLAY_DROP_PARTICLES", "drop-particle arena"),
        ("MAG_REPLAY_DROP_FIRE", "Fire drop particles"),
        ("MAG_REPLAY_DROP_MEND", "Mend drop particles"),
        ("MAG_REPLAY_WISPS", "wisp arena"),
        ("MAG_REPLAY_WISP_TRAIL", "wisp trail history"),
    ] {
        assert!(
            diag[key].as_i64().is_some_and(|n| n > 0),
            "the reference must populate {label}: {diag}"
        );
    }
}

fn mag_seek_equivalence_state_names(
    game: &str,
    frames: u32,
) -> std::collections::HashSet<String> {
    let mut sim = sim_state_names(game, frames);
    for name in ["WORLD_MODE", "MOTES", "WISPS"] {
        assert!(sim.contains(name), "{name} must be admitted as simulation state");
    }
    assert_mag_seek_census_owners(&sim);
    assert_mag_panel_plan_census(&sim);
    for name in ["TELEMETRY_SESSION_OPEN", "TELEMETRY_LAST_FRAME_T"] {
        assert!(!sim.contains(name), "{name} is telemetry side-channel state");
    }
    assert!(!sim.contains("DROP_PARTICLES"), "WorldState owns drop-particle storage inside MAG_ACTIVE_WORLD");
    assert!(!sim.contains("ENGINE"), "render-only ENGINE settings must not be checkpointed");
    for name in [
        "MAG_BUILD_INFO_DATE",
        "MAG_BUILD_INFO_UNIX",
        "MAG_DEATH_AUDIO_WRITES",
        "MAG_DEATH_GRAYSCALE_PASSES",
        "MAG_GAMEPLAY_PANEL_DRAWS",
        "MAG_GAMEPLAY_WORLD_DRAWS",
        "MAG_LIGHTS_ADDED",
        "MAG_RESURRECTION_UI_DRAWS",
        "MAG_RUN_STARTS",
        "MAG_RUN_STOPS",
        "UI_MOUSE",
        "MENU_ATMOSPHERE_BATCHES",
        "MENU_ATMOSPHERE_SUBMISSIONS",
        "MENU_BOUNDED_FLOOR_SUBMISSIONS",
        "MENU_BOUNDED_GLYPH_SUBMISSIONS",
        "MENU_LABEL_CACHE_KEYS",
        "MENU_LABEL_OUTPUT_ANCHORS",
        "MENU_LABEL_OUTPUT_CALLS",
        "MENU_LABEL_ROUTES",
        "MENU_LABEL_SCENE_CALLS",
        "MENU_LOGO_REFLECTION_ADDITIONAL_INPUTS",
        "MENU_LOGO_REFLECTION_FACE_PIXELS",
        "MENU_LOGO_REFLECTION_LAST_GENERATION",
        "MENU_LOGO_REFLECTION_LAST_SOURCE_HANDLE",
        "MENU_LOGO_REFLECTION_MATERIAL_CALLS",
        "MENU_LOGO_TEXT_ART",
        "MENU_LOGO_TEXT_STATE",
        "MENU_LOGO_TEXT_OUTPUT_CALLS",
        "MENU_OUTPUT_TEXT_ART",
        "MENU_OUTPUT_TEXT_TARGET_RECT",
    ] {
        assert!(sim.remove(name), "{name} must be excluded as a presentation or lifecycle diagnostic");
    }
    sim
}

const MAG_RECOVERY_RECORDED_AT: &str = "1000";
const MAG_RECOVERY_REPLAYED_AT: &str = "1004";

fn mag_recovery_last_observed_at(state: &serde_json::Value) -> i64 {
    state["mag"]["RECOVERY_RESERVOIR"]["last_observed_at"]
        .as_i64()
        .expect("RECOVERY_RESERVOIR.last_observed_at")
}

fn mag_recovery_record_replay_arms(progress: &serde_json::Value) -> (serde_json::Value, serde_json::Value) {
    let dir = tempfile::tempdir().unwrap();
    let fixture = dir.path().join("recovery-clock.replay");
    let fixture_s = fixture.to_string_lossy().into_owned();
    let progress_path = dir.path().join("progress.json");
    let progress_s = progress_path.to_string_lossy().into_owned();
    let layout_s = dir.path().join("spellbook-layout.jsonl").to_string_lossy().into_owned();
    let settings_s = dir.path().join("settings.json").to_string_lossy().into_owned();
    let bindings_s = dir.path().join("bindings.json").to_string_lossy().into_owned();
    std::fs::write(&progress_path, serde_json::to_vec_pretty(progress).unwrap()).unwrap();

    let record_env = [
        ("MAG_BOOT_PLAYING", "1"),
        ("MAG_PROGRESS_PATH_OVERRIDE", progress_s.as_str()),
        ("MAG_LAYOUT_HISTORY_PATH_OVERRIDE", layout_s.as_str()),
        ("MAG_SETTINGS_PATH_OVERRIDE", settings_s.as_str()),
        ("MAG_BINDINGS_PATH_OVERRIDE", bindings_s.as_str()),
        ("STONE_TEST_UNIX_SECONDS", MAG_RECOVERY_RECORDED_AT),
    ];
    let recorded = run_record_dump("mag", 3, &fixture_s, &record_env);

    let replayed = run_replay_dump(
        "mag",
        3,
        &fixture_s,
        &[
            ("MAG_BOOT_PLAYING", "1"),
            ("MAG_PROGRESS_PATH_OVERRIDE", progress_s.as_str()),
            ("MAG_LAYOUT_HISTORY_PATH_OVERRIDE", layout_s.as_str()),
            ("MAG_SETTINGS_PATH_OVERRIDE", settings_s.as_str()),
            ("MAG_BINDINGS_PATH_OVERRIDE", bindings_s.as_str()),
            ("STONE_TEST_UNIX_SECONDS", MAG_RECOVERY_REPLAYED_AT),
        ],
    );
    (recorded, replayed)
}

fn mag_current_progress_with_recovery(measures: i64, last_observed_at: i64) -> serde_json::Value {
    serde_json::json!({
        "version": 4,
        "bank_gold": 0,
        "events": {},
        "quests": {"runs": [], "tracked": [], "collapsed": {}, "quarantined": []},
        "player": {
            "level": {"namespace": "official", "name": "crypt"},
            "grid": [25, 8],
            "dead": false,
            "health_cur": 100.0,
            "mana_cur": 100.0,
            "stamina_cur": 20.0,
            "inventory": {"gold": 0, "bandage": 0, "bloody_bandage": 0},
            "reagents": [50, 50, 50, 50, 50, 50]
        },
        "spellbook": {"favorites": []},
        "recovery": {
            "measures": measures,
            "last_observed_at": last_observed_at,
            "packet_cursor": 0,
            "pending_measures": 0
        }
    })
}

#[test]
fn mag_full_recovery_replay_uses_each_process_wall_clock_sentinel() {
    let (recorded, replayed) =
        mag_recovery_record_replay_arms(&mag_current_progress_with_recovery(10, 100));
    let recorded_at = mag_recovery_last_observed_at(&recorded);
    let replayed_at = mag_recovery_last_observed_at(&replayed);
    assert_eq!(recorded_at, 1000, "recorded reservoir last_observed_at={recorded_at}");
    // PINNED BUG: replay reads the new process wall clock (1004) instead of the
    // recorded observation (1000). This line and the assert_ne! below both encode
    // today's buggy value; a fix that makes replay preserve the recorded clock
    // must flip BOTH in the same commit, or the pair contradicts itself:
    //   - this line: 1004 -> 1000
    //   - the assert_ne! below: assert_ne! -> assert_eq!
    assert_eq!(replayed_at, 1004, "replayed reservoir last_observed_at={replayed_at}");
    assert_ne!(
        recorded_at, replayed_at,
        "replay now preserves the recorded wall clock; flip this sentinel to assert_eq! \
         (and the assert_eq!(replayed_at, ...) above from 1004 to 1000)"
    );
}

#[test]
fn mag_cast_seek_restores_scheduler_owner() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = dir.path().join("mag-cast-seek.replay");
    let fixture_s = fixture.to_string_lossy().into_owned();
    let progress = dir.path().join("progress.json");
    let progress_s = progress.to_string_lossy().into_owned();
    let layout_s = dir.path().join("spellbook-layout.jsonl").to_string_lossy().into_owned();
    let settings_s = dir.path().join("settings.json").to_string_lossy().into_owned();
    let bindings_s = dir.path().join("bindings.json").to_string_lossy().into_owned();
    std::fs::write(
        &progress,
        serde_json::to_vec_pretty(&serde_json::json!({
            "version": 3,
            "bank_gold": 0,
            "events": {},
            "quests": {"runs": [], "tracked": [], "collapsed": {}, "quarantined": []},
            "player": {
                "grid": [25, 8],
                "dead": false,
                "health_cur": 100.0,
                "mana_cur": 100.0,
                "stamina_cur": 20.0,
                "inventory": {"gold": 0, "bandage": 0, "bloody_bandage": 0},
                "reagents": [50, 50, 50, 50, 50, 50]
            },
            "spellbook": {"favorites": []}
        }))
        .unwrap(),
    )
    .unwrap();

    let _ = run_record_dump(
        "mag",
        40,
        &fixture_s,
        &[
            ("MAG_BOOT_PLAYING", "1"),
            ("MAG_PROGRESS_PATH_OVERRIDE", &progress_s),
            ("MAG_LAYOUT_HISTORY_PATH_OVERRIDE", &layout_s),
            ("MAG_SETTINGS_PATH_OVERRIDE", &settings_s),
            ("MAG_BINDINGS_PATH_OVERRIDE", &bindings_s),
        ],
    );
    let mut tick = 0;
    let fixture_text = std::fs::read_to_string(&fixture).expect("current Mag replay fixture");
    assert!(fixture_text.starts_with("STONE_REPLAY 13\n"), "fixture must use the exact-Artifact replay format");
    let fixture_text = fixture_text
        .lines()
        .map(|line| {
            if !line.starts_with("t ") {
                return line.to_string();
            }
            let mut fields: Vec<&str> = line.split_whitespace().collect();
            assert_eq!(fields.len(), 10, "v9 tick shape changed: {line}");
            fields[1] = "0x1.999999999999ap-5";
            fields[6] = if tick == 6 { "1" } else { "-" };
            tick += 1;
            fields.join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(tick, 40, "fixture must rewrite every recorded tick");
    std::fs::write(&fixture, fixture_text + "\n").expect("write deterministic Mag cast replay");

    let target = 12;
    let common_env = [
        ("MAG_BOOT_PLAYING", "1"),
        ("MAG_REPLAY_CAST_SCHEDULER_PROBE", "1"),
        ("MAG_REPLAY_CAST_SCHEDULER_TICK", "11"),
        ("MAG_PROGRESS_PATH_OVERRIDE", progress_s.as_str()),
        ("MAG_LAYOUT_HISTORY_PATH_OVERRIDE", layout_s.as_str()),
        ("MAG_SETTINGS_PATH_OVERRIDE", settings_s.as_str()),
        ("MAG_BINDINGS_PATH_OVERRIDE", bindings_s.as_str()),
    ];
    let mut ordinary_env = common_env.to_vec();
    ordinary_env.extend([
        ("STONE_START_PAUSED", "1"),
        ("STONE_STEP_FRAMES", "12"),
        ("STONE_KEYFRAME_INTERVAL", "10"),
    ]);

    let ordinary = run_replay_dump("mag", 20, &fixture_s, &ordinary_env);
    assert_eq!(current_tick(&ordinary), target);
    let ordinary_probe = &ordinary["__stone_diag"];
    assert_eq!(ordinary_probe["MAG_CAST_PROBE_HITS"], 1, "ordinary arm must observe the target once: {ordinary_probe}");
    assert_eq!(ordinary_probe["MAG_CAST_PROBE_DIRECT_STATE"], 1, "ordinary arm must have live direct state: {ordinary_probe}");
    assert!(ordinary_probe["MAG_CAST_PROBE_SCENE"].as_i64().is_some_and(|id| id > 0),
        "ordinary arm must resolve a live Scene root: {ordinary_probe}");
    assert_eq!(ordinary_probe["MAG_CAST_PROBE_CASTING"], 1, "ordinary arm must be mid-cast: {ordinary_probe}");
    assert_eq!(ordinary_probe["MAG_CAST_PROBE_LIGHTNING"], 1, "ordinary arm must cast Lightning: {ordinary_probe}");
    let cast_timer = ordinary_probe["MAG_CAST_PROBE_TIMER"].as_f64().expect("cast timer");
    let cast_duration = ordinary_probe["MAG_CAST_PROBE_DURATION"].as_f64().expect("cast duration");
    assert!(cast_timer > 0.0 && cast_timer < cast_duration,
        "ordinary arm must observe a progressing cast: {ordinary_probe}");
    assert_eq!(ordinary_probe["MAG_CAST_PROBE_TASK"], 1, "ordinary arm must own a casting Task: {ordinary_probe}");
    assert!(ordinary_probe["MAG_CAST_PROBE_POLLER_ROWS"].as_i64().is_some_and(|n| n > 0),
        "ordinary arm must have a nonempty POLLER: {ordinary_probe}");
    assert_eq!(ordinary_probe["MAG_CAST_PROBE_TASK_FRAME"], target,
        "ordinary casting Task must be pumped on the target tick: {ordinary_probe}");
    assert!(ordinary_probe["MAG_CAST_PROBE_MANA"].as_f64().is_some_and(|mana| mana > 0.0 && mana < 100.0),
        "ordinary cast must have spent mana: {ordinary_probe}");
    assert_eq!(ordinary_probe["MAG_CAST_PROBE_REAGENT_1"], 49);
    assert_eq!(ordinary_probe["MAG_CAST_PROBE_REAGENT_4"], 49);

    let mut sought_env = common_env.to_vec();
    sought_env.extend([
        ("STONE_KEYFRAME_INTERVAL", "10"),
        ("STONE_TIMELINE_SEEK_AT_FRAME", "30"),
        ("STONE_TIMELINE_SEEK_TO_TICK", "12"),
    ]);

    let sought = run_replay_dump("mag", 40, &fixture_s, &sought_env);
    assert_eq!(current_tick(&sought), target);
    let sought_probe = &sought["__stone_diag"];
    assert_eq!(sought_probe["MAG_CAST_PROBE_HITS"], 2,
        "the target must be observed before and after the backward seek: {sought_probe}");
    for key in [
        "MAG_CAST_PROBE_DIRECT_STATE", "MAG_CAST_PROBE_SCENE", "MAG_CAST_PROBE_CASTING",
        "MAG_CAST_PROBE_LIGHTNING", "MAG_CAST_PROBE_TIMER", "MAG_CAST_PROBE_DURATION",
        "MAG_CAST_PROBE_TASK", "MAG_CAST_PROBE_POLLER_ROWS", "MAG_CAST_PROBE_MANA",
        "MAG_CAST_PROBE_REAGENT_1", "MAG_CAST_PROBE_REAGENT_4",
    ] {
        assert_eq!(sought_probe[key], ordinary_probe[key],
            "seek landing must reproduce ordinary casting state for {key}");
    }
    assert!(sought_probe["MAG_CAST_PROBE_TASK_FRAME"].as_i64().unwrap() > 30,
        "the restored task must be pumped after the seek, not left at its keyframe frame");
    let sim = mag_seek_equivalence_state_names("mag", 60);
    assert!(sim.contains("POLLER"), "POLLER must be in the admitted sim-state oracle");
    assert_mag_seek_state_matches(&sought, &ordinary, &sim, "Mag scheduler seek");
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
fn exact_recorded_replay_supports_two_backward_scene_image_scrubs() {
    let game = "bitgun";
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("exact-two-scrubs.replay");
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
    let text = std::fs::read_to_string(&log).expect("read exact replay");
    assert!(text.starts_with("STONE_REPLAY 13\n"), "the scrub fixture must exercise the new format\n{text}");

    let ordinary = run_replay_dump(
        game,
        80,
        &log_s,
        &[("STONE_START_PAUSED", "1"), ("STONE_STEP_FRAMES", "35")],
    );
    assert_eq!(current_tick(&ordinary), 35, "ordinary playback must reach the comparison tick");

    let sought = run_replay_dump(
        game,
        150,
        &log_s,
        &[
            ("STONE_KEYFRAME_INTERVAL", "10"),
            ("STONE_TIMELINE_SEEK_AT_FRAME", "60"),
            ("STONE_TIMELINE_SEEK_TO_TICK", "25"),
            ("STONE_LIVE_PLAY_AT_FRAME", "80"),
            ("STONE_TIMELINE_SEEK_AT_FRAME_2", "120"),
            ("STONE_TIMELINE_SEEK_TO_TICK_2", "35"),
            ("STONE_AUDIO_PROBE", "1"),
        ],
    );
    assert_eq!(current_tick(&sought), 35, "the second backward seek must land at tick 35");
    let diag = &sought["__stone_diag"];
    assert_eq!(diag["STONE_SEEK_COUNT"].as_i64(), Some(2), "both backward seeks must complete: {diag}");
    assert!(
        diag["STONE_KEYFRAME_GLOBALS"].as_i64().is_some_and(|n| n > 0)
            && diag["STONE_KEYFRAME_NODES"].as_i64().is_some_and(|n| n > 0),
        "both restore arms must materialize nonempty WorldImage/SceneImage state: {diag}"
    );
    assert!(
        diag["AUDIO_SEEK_MUTES"].as_i64().is_some_and(|n| n > 0),
        "audio must stay behind the catch-up gate: {diag}"
    );
    assert_eq!(
        diag["STONE_PRESENTATION_LATE_DRAW_DURING_SEEK"].as_i64().unwrap_or(0),
        0,
        "presentation must stay suppressed during catch-up: {diag}"
    );
    assert_eq!(
        diag["STONE_PRESENTED_VIEW_PROMOTED_DURING_SEEK"].as_i64().unwrap_or(0),
        0,
        "the seek tool must not publish a presented view: {diag}"
    );
    assert_eq!(diag["STONE_SEEK_FIRST_FRAMES"].as_i64(), Some(2), "each seek needs a real post-seek frame: {diag}");
    assert!(
        diag["STONE_STATE_IMAGE_IMAGES_BEFORE_SHUTDOWN"].as_i64().is_some_and(|n| n > 0)
            && diag["STONE_STATE_IMAGE_IMAGES_AFTER_SHUTDOWN"].as_i64() == Some(0),
        "shutdown must release every lazy StateImage owner: {diag}"
    );
    let sim = sim_state_names(game, 80);
    assert_sim_state_matches(&sought, &ordinary, &sim, "the exact two-scrub target");
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

    // The boot anchor remains available even when viewer cadence is disabled.
    // Use an invalid future target for the refusal arm; a valid backward scrub
    // must now be able to resimulate from boot.
    let refused = run_replay_dump(
        game,
        60,
        &log_s,
        &[
            ("STONE_KEYFRAME_INTERVAL", "0"),
            ("STONE_SCRUB_AT_TICK", "40"),
            ("STONE_SCRUB_TO_TICK", "1000"),
        ],
    );
    assert_eq!(current_tick(&refused), 60, "a refused scrub must not pause or rewind playback");
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

#[test]
fn live_rewind_builds_lazy_index_without_recording_periodic_captures() {
    let game = "bitgun";
    let target: i64 = 55;
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("live-lazy.replay");
    let log_s = log.to_string_lossy().into_owned();

    let rewound = run_record_dump(
        game,
        180,
        &log_s,
        &[
            ("STONE_KEYFRAME_INTERVAL", "10"),
            ("STONE_TIMELINE_SEEK_AT_FRAME", "100"),
            ("STONE_TIMELINE_SEEK_TO_TICK", "55"),
        ],
    );
    assert_eq!(current_tick(&rewound), target, "live rewind must land at the requested target");
    let diag = &rewound["__stone_diag"];
    assert_eq!(diag["STONE_LIVE_REWIND_RESTORE_TICK"].as_i64(), Some(0), "the first live seek has only its boot source: {diag}");
    assert!(
        diag["STONE_SEEK_RESIM_FRAMES"].as_i64().is_some_and(|n| n == target),
        "the first live seek must resimulate from boot: {diag}"
    );
    assert!(
        diag["STONE_KEYFRAME_SEEK_CAPTURES"].as_i64().is_some_and(|n| n >= 5),
        "hidden live resimulation must create intermediate lazy keyframes: {diag}"
    );
    assert!(
        diag["STONE_KEYFRAME_COUNT"].as_i64().is_some_and(|n| n >= 2),
        "the live keyframe population must retain the boot and generated boundaries: {diag}"
    );
    assert_eq!(
        diag["STONE_KEYFRAME_BOOT_CAPTURES"].as_i64(), Some(1),
        "live rewind must start from one boot capture: {diag}"
    );
    assert_eq!(
        diag["STONE_KEYFRAME_RECORDING_PERIODIC_CAPTURES"].as_i64(), Some(0),
        "the live recording itself must not capture before the seek: {diag}"
    );

    let reference = run_replay_dump(
        game,
        180,
        &log_s,
        &[("STONE_START_PAUSED", "1"), ("STONE_STEP_FRAMES", "55")],
    );
    assert_eq!(current_tick(&reference), target);
    let sim = sim_state_names(game, 180);
    assert_sim_state_matches(&rewound, &reference, &sim, "live-lazy tick-55");
}

#[test]
fn live_rewind_ignores_superseded_wall_clock_window() {
    let game = "bitgun";
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("live-no-window.replay");
    let log_s = log.to_string_lossy().into_owned();
    let runtime_frames = 120;

    let rewound = run_record_dump(
        game,
        runtime_frames,
        &log_s,
        &[
            ("STONE_LIVE_REWIND_CADENCE_SECONDS", "0.001"),
            ("STONE_LIVE_REWIND_WINDOW_SECONDS", "0.001"),
            ("STONE_TIMELINE_SEEK_AT_FRAME", "100"),
            ("STONE_TIMELINE_SEEK_TO_TICK", "1"),
        ],
    );
    let diag = &rewound["__stone_diag"];
    assert_eq!(current_tick(&rewound), 1, "the old wall-clock window must not reject a valid target: {diag}");
    assert_eq!(diag["STONE_LIVE_REWIND_SUPPORTED"].as_i64(), Some(1));
    assert_eq!(diag["STONE_LIVE_REWIND_REFUSED"].as_i64(), Some(0));
    assert_eq!(diag["STONE_LIVE_REWIND_COUNT"].as_i64(), Some(1));
    assert_eq!(diag["STONE_LIVE_REWIND_HISTORY_FLOOR_AT_SEEK"].as_i64(), Some(0));
    assert!(
        diag["STONE_KEYFRAME_COUNT"].as_i64().is_some_and(|n| n >= 1 && n <= 4),
        "the boot anchor and any on-demand images must remain: {diag}"
    );
    assert_eq!(recorded_tick_lines(&log_s), 100, "the live horizon must retain one canonical input stream");
}

#[test]
fn live_rewind_after_body_reload_uses_boot_only() {
    let game = "bitgun";
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("live-reload.replay");
    let log_s = log.to_string_lossy().into_owned();

    let live = run_record_dump(
        game,
        140,
        &log_s,
        &[
            ("STONE_RELOAD_AT_FRAME", "60"),
            ("STONE_TIMELINE_SEEK_AT_FRAME", "100"),
            ("STONE_TIMELINE_SEEK_TO_TICK", "20"),
        ],
    );
    let diag = &live["__stone_diag"];
    assert_eq!(diag["STONE_REPLAY_RELOADS"].as_i64(), Some(1));
    assert_eq!(diag["STONE_LIVE_REWIND_COUNT"].as_i64(), Some(1), "the post-reload boot anchor must remain usable: {diag}");
    assert_eq!(diag["STONE_LIVE_REWIND_RESTORE_TICK"].as_i64(), Some(0), "pre-reload keyframes must be invalidated: {diag}");
    assert_eq!(current_tick(&live), 20);
}

#[test]
fn live_full_reload_refuses_after_invalidating_all_keyframes() {
    let game = "bitgun";
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("live-full-reload.replay");
    let log_s = log.to_string_lossy().into_owned();

    let live = run_record_dump(
        game,
        120,
        &log_s,
        &[
            ("STONE_RELOAD_AT_FRAME", "60"),
            ("STONE_RELOAD_FULL", "1"),
            ("STONE_TIMELINE_SEEK_AT_FRAME", "90"),
            ("STONE_TIMELINE_SEEK_TO_TICK", "20"),
        ],
    );
    let diag = &live["__stone_diag"];
    assert_eq!(diag["STONE_REPLAY_RELOADS"].as_i64(), Some(1));
    assert_eq!(diag["STONE_LIVE_REWIND_REFUSED"].as_i64(), Some(1));
    assert!(diag.get("STONE_LIVE_REWIND_COUNT").is_none(), "full reload must not restore stale state: {diag}");
    assert!(
        diag["STONE_LIVE_REWIND_REFUSAL"].as_str().is_some_and(|s| s.contains("full reload")),
        "the refusal must name the invalidating reload: {diag}"
    );
    assert_eq!(recorded_tick_lines(&log_s), 120, "refusal must leave the live input history intact");
}

#[test]
fn long_live_bitgun_rewind_reports_lazy_index_costs() {
    let game = "bitgun";
    let seek_frame: u32 = 200;
    let target: i64 = 105;
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("long-live.replay");
    let log_s = log.to_string_lossy().into_owned();

    let rewound = run_record_dump(
        game,
        seek_frame + 150,
        &log_s,
        &[
            ("STONE_KEYFRAME_INTERVAL", "10"),
            ("STONE_TIMELINE_SEEK_AT_FRAME", "200"),
            ("STONE_TIMELINE_SEEK_TO_TICK", "105"),
        ],
    );
    let diag = &rewound["__stone_diag"];
    assert_eq!(current_tick(&rewound), target, "long live rewind must land at its requested target: {diag}");
    assert_eq!(diag["STONE_LIVE_REWIND_REFUSED"].as_i64(), Some(0), "the in-window target must be admitted: {diag}");
    assert!(
        diag["STONE_LIVE_REWIND_HISTORY_FLOOR_AT_SEEK"].as_i64() == Some(0),
        "the live history must remain from boot: {diag}"
    );
    let resim = diag["STONE_SEEK_RESIM_FRAMES"].as_i64().expect("resim frame count");
    assert_eq!(resim, target, "the first live seek must use the boot source: {diag}");
    assert!(diag["STONE_KEYFRAME_SEEK_CAPTURES"].as_i64().is_some_and(|n| n >= 9), "{diag}");
    assert!(diag["STONE_KEYFRAME_COUNT"].as_i64().is_some_and(|n| n >= 10), "{diag}");
    let cadence_ms = diag["STONE_KEYFRAME_CADENCE_MS"].as_f64().expect("lazy capture cost");
    let capture_max_ms = diag["STONE_KEYFRAME_CAPTURE_MAX_MS"].as_f64().expect("capture hitch");
    assert!(cadence_ms > 0.0, "{diag}");
    assert!(capture_max_ms > 0.0, "{diag}");
    println!(
        "[live-lazy-costs] target={target} resim={resim} keyframes={} captures={} lazy_ms={cadence_ms:.2} capture_max_ms={capture_max_ms:.2}",
        diag["STONE_KEYFRAME_COUNT"],
        diag["STONE_KEYFRAME_SEEK_CAPTURES"]
    );
    assert_eq!(recorded_tick_lines(&log_s), seek_frame as usize, "the retained future must remain the one recorded input stream");
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
    let progress_s = dir.path().join("progress.json").to_string_lossy().into_owned();
    let layout_s = dir.path().join("spellbook-layout.jsonl").to_string_lossy().into_owned();
    let settings_s = dir.path().join("settings.json").to_string_lossy().into_owned();
    let seek_settings_s = dir.path().join("seek-settings.json").to_string_lossy().into_owned();

    let _ = run_record_dump(
        game,
        frames,
        &log_s,
        &[
            ("MAG_BOOT_MENU", "1"),
            ("MAG_BOOT_PLAYING", "1"),
            ("MAG_REPLAY_BOOTSTRAP_FIXTURE", "progress"),
            ("MAG_PROGRESS_PATH_OVERRIDE", progress_s.as_str()),
            ("MAG_LAYOUT_HISTORY_PATH_OVERRIDE", layout_s.as_str()),
            ("MAG_SETTINGS_PATH_OVERRIDE", settings_s.as_str()),
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
            assert!(fields.len() == 8 || fields.len() == 10, "recorded tick shape changed: {line}");
            fields[1] = "0x1.999999999999ap-4";
            fields[2] = match tick {
                30 => "960",
                32 => "1100",
                _ => "960",
            };
            fields[3] = match tick {
                30 => "590",
                32 => "700",
                _ => "432",
            };
            fields[6] = match tick {
                20 => "o",
                28 => "down",
                35 => "o",
                50 => "escape",
                _ => "-",
            };
            fields[7] = if tick == 30 || tick == 32 { "1" } else { "-" };
            tick += 1;
            fields.join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(tick, frames as usize, "fixture must rewrite every requested tick");
    std::fs::write(&log, lines + "\n").expect("write deterministic Mag replay");

    let reference = run_replay_dump(
        game,
        frames,
        &log_s,
        &[
            ("MAG_BOOT_MENU", "1"),
            ("MAG_SCREENSHOT", "1"),
            ("MAG_PROGRESS_PATH_OVERRIDE", progress_s.as_str()),
            ("MAG_LAYOUT_HISTORY_PATH_OVERRIDE", layout_s.as_str()),
            ("MAG_SETTINGS_PATH_OVERRIDE", settings_s.as_str()),
            ("STONE_START_PAUSED", "1"),
            ("STONE_STEP_FRAMES", &target.to_string()),
        ],
    );
    assert_eq!(current_tick(&reference), i64::from(target));
    assert_eq!(reference["__stone_diag"]["MAG_PLAY_EFFECT_SCREEN"].as_i64(), Some(2), "ordinary playback must leave the menu for Playing");
    assert_eq!(reference["__stone_diag"]["MAG_PLAY_EFFECT_COUNT"].as_i64(), Some(1), "ordinary playback must perform one Play transition");
    assert!(reference["__stone_diag"]["MAG_PLAY_EFFECT_SCENE"].as_i64().is_some_and(|id| id >= 0), "ordinary playback must activate a fresh Scene: {}", reference["__stone_diag"]);
    assert_eq!(reference["mag"]["MAG_RUN_STARTS"].as_i64(), Some(1), "ordinary playback must reach the production run-start seam");
    // The recorded 'o' at tick 35 reaches the GAME and no longer reaches the
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
    let sought = run_replay_dump(
        game,
        frames + target + 10,
        &log_s,
        &[
            ("MAG_BOOT_MENU", "1"),
            ("MAG_SCREENSHOT", "1"),
            ("MAG_PROGRESS_PATH_OVERRIDE", progress_s.as_str()),
            ("MAG_LAYOUT_HISTORY_PATH_OVERRIDE", layout_s.as_str()),
            ("MAG_SETTINGS_PATH_OVERRIDE", seek_settings_s.as_str()),
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
        sought["__stone_diag"]["MAG_PLAY_EFFECT_SCREEN"], reference["__stone_diag"]["MAG_PLAY_EFFECT_SCREEN"],
        "seek resimulation must preserve Mag's actual Play screen transition"
    );
    assert!(sought["__stone_diag"]["MAG_PLAY_EFFECT_SCENE"].as_i64().is_some_and(|id| id >= 0), "seek resimulation must activate a fresh Scene: {}", sought["__stone_diag"]);
    assert_eq!(sought["__stone_diag"]["MAG_PLAY_EFFECT_COUNT"].as_i64(), Some(2), "hidden seek resimulation must perform its own Play transition, not reuse the visible run: {}", sought["__stone_diag"]);
    assert!(sought["__stone_diag"]["MAG_MENU_ACTION_STATE_TICKS"].as_i64().is_some_and(|n| n > 0), "seek resimulation must execute the classified state consumer: {}", sought["__stone_diag"]);
    assert!(sought["__stone_diag"]["MAG_CONSENT_ACTION_STATE_TICKS"].as_i64().is_some_and(|n| n > 0), "fresh-settings seek must consume the consent action in the state-role path: {}", sought["__stone_diag"]);
    assert!(sought["__stone_diag"]["MAG_SETTINGS_PLAYBACK_SAVE_SKIPS"].as_i64().is_some_and(|n| n > 0), "seek resimulation must not persist the consent choice as an external effect: {}", sought["__stone_diag"]);
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
    let cadence_sought = run_replay_dump(
        game,
        frames + target + 10,
        &log_s,
        &[
            ("MAG_BOOT_MENU", "1"),
            ("MAG_SCREENSHOT", "1"),
            ("MAG_PROGRESS_PATH_OVERRIDE", progress_s.as_str()),
            ("MAG_LAYOUT_HISTORY_PATH_OVERRIDE", layout_s.as_str()),
            ("MAG_SETTINGS_PATH_OVERRIDE", seek_settings_s.as_str()),
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
        cadence_sought["__stone_diag"]["MAG_PLAY_EFFECT_SCREEN"], reference["__stone_diag"]["MAG_PLAY_EFFECT_SCREEN"],
        "a cadence-keyframe seek must preserve Mag's actual Play screen transition"
    );
    assert!(cadence_sought["__stone_diag"]["MAG_PLAY_EFFECT_SCENE"].as_i64().is_some_and(|id| id >= 0), "cadence seek must activate a fresh Scene: {}", cadence_sought["__stone_diag"]);
    assert_eq!(cadence_sought["__stone_diag"]["MAG_PLAY_EFFECT_COUNT"].as_i64(), Some(2), "cadence seek must perform its own Play transition: {}", cadence_sought["__stone_diag"]);
    assert_eq!(
        cadence_sought["stone"]["WORLD_MODE"]["game_paused"],
        reference["stone"]["WORLD_MODE"]["game_paused"],
        "a cadence-keyframe seek must restore engine-owned pause state"
    );

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
            assert!(fields.len() == 8 || fields.len() == 10, "recorded tick shape changed: {line}");
            fields[1] = "0x1.999999999999ap-4";
            fields[2] = if tick == 24 { "1100" } else { "0" };
            fields[3] = if tick == 24 { "700" } else { "0" };
            fields[6] = match tick {
                20 => "o",
                23 | 28 | 30 | 50 | 52 => "down",
                32 => "return",
                45 => "escape",
                _ => "-",
            };
            fields[7] = if tick == 24 { "1" } else { "-" };
            tick += 1;
            fields.join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(tick, frames as usize, "fixture must rewrite every requested tick");
    std::fs::write(&keyboard_log, lines + "\n").expect("write keyboard Mag replay");

    let keyboard_reference = run_replay_dump(
        game,
        frames,
        &keyboard_log_s,
        &[
            ("MAG_BOOT_MENU", "1"),
            ("MAG_SCREENSHOT", "1"),
            ("MAG_PROGRESS_PATH_OVERRIDE", progress_s.as_str()),
            ("MAG_LAYOUT_HISTORY_PATH_OVERRIDE", layout_s.as_str()),
            ("MAG_SETTINGS_PATH_OVERRIDE", settings_s.as_str()),
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
            ("MAG_PROGRESS_PATH_OVERRIDE", progress_s.as_str()),
            ("MAG_LAYOUT_HISTORY_PATH_OVERRIDE", layout_s.as_str()),
            ("MAG_SETTINGS_PATH_OVERRIDE", settings_s.as_str()),
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
fn mag_main_menu_seek_state_equivalence_survives_resimulation() {
    let game = "mag";
    let frames = 80;
    let target: u32 = 40;
    let census_tick = (target - 1).to_string();
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("menu-state-equivalence.replay");
    let log_s = log.to_string_lossy().into_owned();
    let progress_s = dir.path().join("progress.json").to_string_lossy().into_owned();
    let layout_s = dir.path().join("spellbook-layout.jsonl").to_string_lossy().into_owned();
    let settings_s = dir.path().join("preconsented-settings.json").to_string_lossy().into_owned();
    std::fs::write(
        &settings_s,
        r#"{"privacy":{"analytics":1,"analytics_version":3}}"#,
    )
    .expect("seed the production settings file with an answered consent");

    let _ = run_record_dump(
        game,
        frames,
        &log_s,
        &[
            ("MAG_BOOT_MENU", "1"),
            ("MAG_BOOT_PLAYING", "1"),
            ("MAG_REPLAY_BOOTSTRAP_FIXTURE", "progress"),
            ("MAG_PROGRESS_PATH_OVERRIDE", progress_s.as_str()),
            ("MAG_LAYOUT_HISTORY_PATH_OVERRIDE", layout_s.as_str()),
            ("MAG_SETTINGS_PATH_OVERRIDE", settings_s.as_str()),
            ("MAG_SCREENSHOT", "1"),
            ("STONE_CHECKPOINT_INTERVAL", "0"),
        ],
    );
    let text = std::fs::read_to_string(&log).expect("read Mag state-equivalence replay");
    let mut tick = 0;
    let lines = text
        .lines()
        .map(|line| {
            if !line.starts_with("t ") {
                return line.to_string();
            }
            let mut fields: Vec<&str> = line.split_whitespace().collect();
            assert!(fields.len() == 8 || fields.len() == 10, "recorded tick shape changed: {line}");
            fields[1] = "0x1.999999999999ap-4";
            fields[2] = "960";
            fields[3] = if tick == 30 { "590" } else { "432" };
            fields[6] = "-";
            fields[7] = if tick == 30 { "1" } else { "-" };
            tick += 1;
            fields.join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(tick, frames as usize, "fixture must rewrite every requested tick");
    std::fs::write(&log, lines + "\n").expect("write deterministic state-equivalence replay");

    let reference = run_replay_dump(
        game,
        frames,
        &log_s,
        &[
            ("MAG_BOOT_MENU", "1"),
            ("MAG_SCREENSHOT", "1"),
            ("MAG_PROGRESS_PATH_OVERRIDE", progress_s.as_str()),
            ("MAG_LAYOUT_HISTORY_PATH_OVERRIDE", layout_s.as_str()),
            ("MAG_SETTINGS_PATH_OVERRIDE", settings_s.as_str()),
            ("STONE_START_PAUSED", "1"),
            ("STONE_STEP_FRAMES", &target.to_string()),
            ("MAG_REPLAY_STATE_CENSUS", "1"),
            ("MAG_REPLAY_STATE_CENSUS_TICK", &census_tick),
        ],
    );
    assert_eq!(current_tick(&reference), i64::from(target));
    assert_eq!(reference["__stone_diag"]["MAG_PLAY_EFFECT_SCREEN"].as_i64(), Some(2));
    assert_mag_seek_populations_are_nonzero(&reference);
    let sim = mag_seek_equivalence_state_names(game, frames);

    let sought = run_replay_dump(
        game,
        frames + target + 10,
        &log_s,
        &[
            ("MAG_BOOT_MENU", "1"),
            ("MAG_SCREENSHOT", "1"),
            ("MAG_PROGRESS_PATH_OVERRIDE", progress_s.as_str()),
            ("MAG_LAYOUT_HISTORY_PATH_OVERRIDE", layout_s.as_str()),
            ("MAG_SETTINGS_PATH_OVERRIDE", settings_s.as_str()),
            ("STONE_KEYFRAME_INTERVAL", "1000"),
            ("STONE_TIMELINE_SEEK_AT_FRAME", &frames.to_string()),
            ("STONE_TIMELINE_SEEK_TO_TICK", &target.to_string()),
            ("MAG_REPLAY_STATE_CENSUS", "1"),
            ("MAG_REPLAY_STATE_CENSUS_TICK", &census_tick),
        ],
    );
    assert_eq!(current_tick(&sought), i64::from(target));
    assert_eq!(sought["__stone_diag"]["STONE_SEEK_RESIM_FRAMES"].as_i64(), Some(i64::from(target)));
    assert_mag_seek_state_matches(&sought, &reference, &sim, "Mag from-boot state equivalence");

    let cadence_sought = run_replay_dump(
        game,
        frames + target + 10,
        &log_s,
        &[
            ("MAG_BOOT_MENU", "1"),
            ("MAG_SCREENSHOT", "1"),
            ("MAG_PROGRESS_PATH_OVERRIDE", progress_s.as_str()),
            ("MAG_LAYOUT_HISTORY_PATH_OVERRIDE", layout_s.as_str()),
            ("MAG_SETTINGS_PATH_OVERRIDE", settings_s.as_str()),
            ("STONE_KEYFRAME_INTERVAL", "30"),
            ("STONE_TIMELINE_SEEK_AT_FRAME", &frames.to_string()),
            ("STONE_TIMELINE_SEEK_TO_TICK", &target.to_string()),
            ("MAG_REPLAY_STATE_CENSUS", "1"),
            ("MAG_REPLAY_STATE_CENSUS_TICK", &census_tick),
        ],
    );
    assert_eq!(cadence_sought["__stone_diag"]["STONE_SEEK_RESIM_FRAMES"].as_i64(), Some(10));
    assert_mag_seek_state_matches(&cadence_sought, &reference, &sim, "Mag cadence state equivalence");
}

#[test]
fn mag_second_live_rewind_reflective_sample_is_rebuilt_before_hidden_late_draw() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("second-rewind-reflection.replay");
    let log_s = log.to_string_lossy().into_owned();
    let run = run_record_dump_observed(
        "mag",
        120,
        &log_s,
        &[
            ("PERF_MODE", "1"),
            ("PERF_NO_SECTIONS", "1"),
            ("PERF_DT", "0.1"),
            ("MAG_BOOT_MENU", "1"),
            ("STONE_LIVE_REWIND", "1"),
            ("STONE_KEYFRAME_INTERVAL", "10"),
            ("STONE_TIMELINE_SEEK_AT_FRAME", "40"),
            ("STONE_TIMELINE_SEEK_TO_TICK", "10"),
            ("STONE_TIMELINE_SEEK_AT_FRAME_2", "100"),
            ("STONE_TIMELINE_SEEK_TO_TICK_2", "27"),
        ],
    );
    assert!(
        run.status_success,
        "the bounded run must not raise a frame error; the pre-fix RED arm reports:\n{}",
        run.stderr
    );
    assert!(
        !run.stderr.contains("reflective TextArt requested a presented scene sample"),
        "the reflective sample failure must be gone:\n{}",
        run.stderr
    );

    let diag = &run.state["__stone_diag"];
    assert_eq!(diag["STONE_LIVE_REWIND_COUNT"].as_i64(), Some(1), "{diag}");
    assert_eq!(diag["STONE_SEEK_COUNT"].as_i64(), Some(2), "exactly two admitted seeks: {diag}");
    assert_eq!(diag["STONE_SEEK_FROM_TICK"].as_i64(), Some(10), "the second seek must be distinguishable: {diag}");
    assert_eq!(diag["STONE_SEEK_TO_TICK"].as_i64(), Some(27), "{diag}");
    assert!(
        diag["STONE_SEEK_RESIM_FRAMES"].as_i64().is_some_and(|n| n > 0),
        "the second seek must resimulate: {diag}"
    );
    assert!(
        diag["STONE_KEYFRAME_GLOBALS"].as_i64().is_some_and(|n| n > 0)
            && diag["STONE_KEYFRAME_NODES"].as_i64().is_some_and(|n| n > 0)
            && diag["STONE_RESTORE_MS"].as_f64().is_some_and(|n| n > 0.0),
        "the fixture must materialize a non-empty SceneImage/StateImage restore: {diag}"
    );
    assert!(
        diag["STONE_SEEK_TOOL_RENDER_FRAMES"].as_i64().is_some_and(|n| n > 0),
        "the seek-tool render population must be nonzero: {diag}"
    );
    assert_eq!(
        diag["STONE_HIDDEN_LATE_DRAW_FRAMES"].as_i64(),
        diag["STONE_SEEK_TOOL_RENDER_FRAMES"].as_i64(),
        "the hidden late-draw population must match the active seek frames: {diag}"
    );
    assert!(
        diag["MAG_MENU_ACTION_STATE_TICKS"].as_i64().is_some_and(|n| n > 0),
        "the gameplay-relevant menu action consumer must run during hidden resimulation: {diag}"
    );
    assert_eq!(diag["STONE_PRESENTATION_LATE_DRAW_DURING_SEEK"].as_i64().unwrap_or(0), 0, "reflective gameplay presentation must stay out of active seek frames: {diag}");
    assert_eq!(diag["STONE_PRESENTED_VIEW_PROMOTED_DURING_SEEK"].as_i64().unwrap_or(0), 0, "the restored host projection must not be published by a tool-only seek frame: {diag}");
    assert!(
        diag["MAG_MENU_LOGO_TEXT_SAMPLE_REQUESTS"].as_i64().is_some_and(|n| n > 0)
            && run.state["mag"]["MENU_LOGO_TEXT_OUTPUT_CALLS"]
                .as_i64()
                .is_some_and(|n| n > 0),
        "the reflective menu TextArt consumer must be exercised: {diag}"
    );
}

#[test]
fn mag_first_live_rewind_is_the_nearest_working_reflection_control() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("first-rewind-reflection-control.replay");
    let log_s = log.to_string_lossy().into_owned();
    let run = run_record_dump_observed(
        "mag",
        70,
        &log_s,
        &[
            ("PERF_MODE", "1"),
            ("PERF_NO_SECTIONS", "1"),
            ("PERF_DT", "0.1"),
            ("MAG_BOOT_MENU", "1"),
            ("STONE_LIVE_REWIND", "1"),
            ("STONE_KEYFRAME_INTERVAL", "10"),
            ("STONE_TIMELINE_SEEK_AT_FRAME", "40"),
            ("STONE_TIMELINE_SEEK_TO_TICK", "10"),
        ],
    );
    assert!(
        run.status_success,
        "the first-seek control must remain clean; stderr:\n{}",
        run.stderr
    );
    assert!(
        !run.stderr.contains("reflective TextArt requested a presented scene sample"),
        "the control must not request a scene sample before its first capture:\n{}",
        run.stderr
    );
    let diag = &run.state["__stone_diag"];
    assert_eq!(diag["STONE_LIVE_REWIND_COUNT"].as_i64(), Some(1), "{diag}");
    assert_eq!(diag["STONE_SEEK_COUNT"].as_i64(), Some(1), "the control has one admitted seek: {diag}");
    assert!(
        diag["MAG_MENU_LOGO_TEXT_SAMPLE_REQUESTS"].as_i64().is_some_and(|n| n > 0),
        "the nearest working control must reach the reflective menu consumer: {diag}"
    );
}

#[test]
fn live_record_rewind_refuses_unsupported_state_without_mutating_world() {
    let game = "survivors";
    let runtime_frames = 80;
    let dir = tempfile::tempdir().unwrap();
    for (mode, global, kind) in [
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

/// Boot admission describes boot. Unsupported raw/native state created AFTER
/// boot but before H must still refuse the rewind at H -- before mode
/// promotion and before any restore -- so the live world is never partially
/// rolled back. The distinguishing evidence against the boot-refusal test is
/// `STONE_LIVE_REWIND_SUPPORTED == 1`: boot WAS admissible here.
#[test]
fn live_record_rewind_refuses_late_created_unsupported_state() {
    let game = "survivors";
    let runtime_frames = 80;
    let dir = tempfile::tempdir().unwrap();
    for (mode, global, kind) in [
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
        // The bounded runner's log population is the denominator for the live
        // stream; the refusal must not switch the session into replay.
        assert_eq!(
            recorded_tick_lines(&log_s), runtime_frames as usize,
            "{mode} refusal must leave the session recording (no promotion to replay)"
        );
        assert_eq!(current_tick(&refused), current_tick(&control), "{mode} refusal must keep live time moving");
        let sim = sim_state_names(game, runtime_frames);
        assert_sim_state_matches_exact(&refused, &control, &sim, &format!("late-refused live-rewind {mode}"));
    }
}

#[test]
fn live_admission_failure_clears_partial_history() {
    let game = "survivors";
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("live-cadence-refused.replay");
    let log_s = log.to_string_lossy().into_owned();

    let refused = run_record_dump(
        game,
        100,
        &log_s,
        &[
            ("ROCK_TEST_SEED", "42"),
            ("STONE_LIVE_REWIND_UNSUPPORTED", "foreign"),
            ("STONE_LIVE_REWIND_UNSUPPORTED_AT_TICK", "30"),
            ("STONE_TIMELINE_SEEK_AT_FRAME", "70"),
            ("STONE_TIMELINE_SEEK_TO_TICK", "25"),
        ],
    );
    let diag = &refused["__stone_diag"];
    assert_eq!(diag["STONE_LIVE_REWIND_SUPPORTED"].as_i64(), Some(1));
    assert!(diag["STONE_LIVE_REWIND_REFUSALS"].as_i64().is_some_and(|n| n >= 1), "{diag}");
    assert!(diag.get("STONE_LIVE_REWIND_COUNT").is_none(), "a failed live admission must not promote: {diag}");
    assert_eq!(diag["STONE_KEYFRAME_COUNT"].as_i64(), Some(1), "partial cadence frames must be discarded: {diag}");
    assert_eq!(recorded_tick_lines(&log_s), 100, "the failed admission must leave the input history intact");
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

/// The author-facing surface: the explicit replay-picker action opens the
/// timeline over the LIVE recording and spans the retained history. A plain
/// recording stays closed, so it pays nothing. After a rewind the bar's span is
/// the whole recorded log -- the stretch past `current_tick` is exactly the
/// retained future the author can still play into.
///
/// The bar's control wiring is what is asserted here; placement and styling are
/// author-verified, per the timeline bar's own convention.
#[test]
fn live_timeline_bar_exposes_retained_history_while_recording() {
    let game = "bitgun";
    let dir = tempfile::tempdir().unwrap();

    let armed_log = dir.path().join("bar-opened.replay").to_string_lossy().into_owned();
    let armed = run_record_dump(game, 30, &armed_log, &[("STONE_OPEN_LIVE_TIMELINE_AT_FRAME", "5")]);
    let diag = &armed["__stone_diag"];
    assert!(
        diag["REPLAY_TIMELINE_DRAWS"].as_i64().is_some_and(|n| n > 0),
        "the explicit live-timeline action must open the timeline bar: {diag}"
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
            ("STONE_OPEN_LIVE_TIMELINE_AT_FRAME", "5"),
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

#[test]
fn ordinary_recording_keeps_one_boot_anchor_without_timeline_arming() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("default-anchor.replay").to_string_lossy().into_owned();
    let state = run_record_dump(
        "bitgun",
        30,
        &log,
        &[("STONE_KEYFRAME_INTERVAL", "1")],
    );
    let diag = &state["__stone_diag"];
    assert_eq!(diag["STONE_LIVE_REWIND_SUPPORTED"].as_i64(), Some(1), "default recording must admit live rewind: {diag}");
    assert_eq!(diag["STONE_KEYFRAME_BOOT_CAPTURES"].as_i64(), Some(1), "boot admission must have one capture: {diag}");
    assert_eq!(diag["STONE_KEYFRAME_COUNT"].as_i64(), Some(1), "ordinary recording must retain only its boot anchor: {diag}");
    assert_eq!(diag.get("STONE_KEYFRAME_CADENCE_CAPTURES").and_then(serde_json::Value::as_i64).unwrap_or(0), 0, "live recording must not pay periodic capture cost: {diag}");
    assert_eq!(diag.get("STONE_KEYFRAME_RECORDING_PERIODIC_CALLS").and_then(serde_json::Value::as_i64).unwrap_or(0), 0, "the live runner must not call the periodic authority: {diag}");
    assert!(diag.get("REPLAY_TIMELINE_DRAWS").is_none(), "the unopened timeline must not draw: {diag}");
}

#[test]
fn live_recording_without_timeline_has_zero_periodic_captures_even_with_old_cadence_env() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("no-periodic.replay").to_string_lossy().into_owned();
    let state = run_record_dump(
        "bitgun",
        60,
        &log,
        &[
            ("STONE_LIVE_REWIND", "1"),
            ("STONE_LIVE_REWIND_CADENCE_SECONDS", "0.001"),
            ("STONE_LIVE_REWIND_WINDOW_SECONDS", "2.0"),
        ],
    );
    let diag = &state["__stone_diag"];
    assert_eq!(diag["STONE_KEYFRAME_COUNT"].as_i64(), Some(1), "old live cadence settings must not create a second mechanism: {diag}");
    assert_eq!(diag.get("STONE_KEYFRAME_CADENCE_CAPTURES").and_then(serde_json::Value::as_i64).unwrap_or(0), 0, "no timeline use means no periodic WorldImage captures: {diag}");
}

#[test]
fn replay_picker_live_action_opens_history_without_indexing_it() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("picker-live.replay").to_string_lossy().into_owned();
    let state = run_record_dump(
        "bitgun",
        30,
        &log,
        &[("STONE_OPEN_LIVE_TIMELINE_AT_FRAME", "5")],
    );
    let diag = &state["__stone_diag"];
    assert_eq!(diag["STONE_LIVE_TIMELINE_OPEN"].as_i64(), Some(1), "the replay picker action must open the live timeline: {diag}");
    assert!(diag["REPLAY_TIMELINE_DRAWS"].as_i64().is_some_and(|n| n > 0), "opening the live timeline must expose its retained history: {diag}");
    assert_eq!(diag["STONE_KEYFRAME_COUNT"].as_i64(), Some(1), "opening the timeline must not pre-index the recording: {diag}");
}

#[test]
fn far_seek_builds_intermediate_lazy_keyframes_and_matches_plain_playback() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("lazy-far-seek.replay").to_string_lossy().into_owned();
    let _ = run_record_dump("bitgun", 80, &log, &[]);
    let sought = run_replay_dump(
        "bitgun",
        100,
        &log,
        &[
            ("STONE_START_PAUSED", "1"),
            ("STONE_KEYFRAME_INTERVAL", "10"),
            ("STONE_TIMELINE_SEEK_AT_FRAME", "1"),
            ("STONE_TIMELINE_SEEK_TO_TICK", "55"),
        ],
    );
    assert_eq!(current_tick(&sought), 55, "the immediate seek must land at its target");
    let reference = run_replay_dump(
        "bitgun",
        100,
        &log,
        &[("STONE_START_PAUSED", "1"), ("STONE_STEP_FRAMES", "55")],
    );
    let sim = sim_state_names("bitgun", 80);
    assert_sim_state_matches(&sought, &reference, &sim, "lazy seek target");
    let diag = &sought["__stone_diag"];
    assert_eq!(diag["STONE_KEYFRAME_INIT_COUNT"].as_i64(), Some(1), "a loaded replay must begin with only its boot anchor: {diag}");
    assert!(diag["STONE_KEYFRAME_COUNT"].as_i64().is_some_and(|n| n >= 6), "a far seek must retain boot plus intermediate cadence boundaries: {diag}");
    assert!(diag["STONE_KEYFRAME_SEEK_CAPTURES"].as_i64().is_some_and(|n| n >= 5), "hidden resimulation must use the shared cadence capture authority: {diag}");
    assert!(diag["STONE_KEYFRAME_FIRST_SEEK_TICK"].as_i64().is_some_and(|n| n > 0 && n < 55), "the first seek capture must be an intermediate boundary, not only the target: {diag}");
    assert!(diag["STONE_STATE_IMAGE_IMAGES_BEFORE_SHUTDOWN"].as_i64().is_some_and(|n| n >= 6), "each retained lazy keyframe must own one StateImage before shutdown: {diag}");
}

#[test]
fn second_seek_restores_from_the_nearest_lazily_generated_keyframe() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("lazy-nearest.replay").to_string_lossy().into_owned();
    let _ = run_record_dump("bitgun", 80, &log, &[]);
    let sought = run_replay_dump(
        "bitgun",
        120,
        &log,
        &[
            ("STONE_START_PAUSED", "1"),
            ("STONE_KEYFRAME_INTERVAL", "10"),
            ("STONE_TIMELINE_SEEK_AT_FRAME", "1"),
            ("STONE_TIMELINE_SEEK_TO_TICK", "55"),
            ("STONE_TIMELINE_SEEK_AT_FRAME_2", "80"),
            ("STONE_TIMELINE_SEEK_TO_TICK_2", "35"),
        ],
    );
    assert_eq!(current_tick(&sought), 35, "the second seek must land at its target");
    let diag = &sought["__stone_diag"];
    assert_eq!(diag["STONE_SEEK_COUNT"].as_i64(), Some(2), "both seek requests must complete: {diag}");
    assert_eq!(diag["STONE_LAST_RESTORE_TICK"].as_i64(), Some(30), "the second seek must restore its nearest lazy keyframe: {diag}");
    assert_eq!(diag["STONE_SEEK_RESIM_FRAMES"].as_i64(), Some(5), "only the span after the nearest keyframe should resimulate: {diag}");
    assert!(diag["STONE_STATE_IMAGE_SESSION_OWNERS"].as_i64().is_some_and(|n| n >= 1), "the restored StateImage session must remain owned by the active runtime: {diag}");
}

#[test]
fn boot_image_refusal_does_not_stop_default_recording() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("boot-refusal-recording.replay").to_string_lossy().into_owned();
    let frames = 40;
    let state = run_record_dump(
        "survivors",
        frames,
        &log,
        &[("ROCK_TEST_SEED", "42"), ("STONE_LIVE_REWIND_UNSUPPORTED", "foreign")],
    );
    let diag = &state["__stone_diag"];
    assert_eq!(diag["STONE_LIVE_REWIND_SUPPORTED"].as_i64(), Some(0), "the unsupported boot anchor must report unavailable rewind: {diag}");
    assert_eq!(diag["STONE_LIVE_REWIND_REFUSED"].as_i64(), Some(1), "boot refusal must be observable: {diag}");
    let recorded = recorded_tick_lines(&log);
    assert_eq!(current_tick(&state), recorded as i64, "live time and streamed input must agree after boot refusal");
    assert!(recorded >= frames as usize, "the refusal must not stop the streamed recording");
}

#[test]
fn replay_file_contains_observations_and_bootstrap_but_no_world_image_payload() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("observations-only.replay").to_string_lossy().into_owned();
    let _ = run_record_dump("bitgun", 20, &log, &[]);
    let bytes = std::fs::read_to_string(&log).unwrap();
    assert!(bytes.lines().any(|line| line.starts_with("t ")), "the replay must contain input observations");
    assert!(bytes.contains("STONE_REPLAY 13"), "the replay must retain its versioned header");
    for payload in ["WorldImage", "StateImage", "keyframe", "snapshot"] {
        assert!(!bytes.contains(payload), "the replay wire must not serialize {payload}: {bytes}");
    }
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
    assert_eq!(diag["STONE_BRANCH_REQUESTS"].as_i64(), Some(1), "the post-frame hook must use the request path: {diag}");
    assert_eq!(diag["STONE_BRANCH_INPUT_HANDOFFS"].as_i64(), Some(1), "input authority must return only after the branch transaction commits: {diag}");
    assert_eq!(diag["STONE_BRANCH_INPUT_HANDOFF_EARLY"].as_i64().unwrap_or(0), 0, "input authority must not return before the branch transaction commits: {diag}");
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
        diag["STONE_BRANCH_IMAGES_AFTER"].as_i64(), Some(1),
        "branch invalidation must release discarded keyframe images: {diag}"
    );
    assert_eq!(
        diag["STONE_BRANCH_RECORDING_AFTER"].as_i64(), Some(1),
        "a branch must hand the session back to the recorder: {diag}"
    );
    assert_eq!(
        diag["STONE_BRANCH_HORIZON_AFTER"].as_i64(), Some(-1),
        "a branched session is no longer a rewound one, so it has no horizon: {diag}"
    );
    assert_eq!(
        diag["STONE_BRANCH_CURSOR_AFTER"].as_i64(), Some(branch_tick),
        "the branched cursor must be rebased to the retained prefix: {diag}"
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
    // state in a fresh process, including a resize retained before the branch.
    let replayable_s = dir.path().join("branch-replayable.replay").to_string_lossy().into_owned();
    let live2 = run_record_dump(
        game, 120, &replayable_s,
        &[
            ("STONE_LIVE_REWIND", "1"),
            ("STONE_CHECKPOINT_INTERVAL", "5"),
            ("STONE_RESIZE_AT_FRAME", "10"),
            ("STONE_RESIZE_W", "1000"),
            ("STONE_RESIZE_H", "700"),
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
    assert!(
        replay_lines(&replayable_s).iter().any(|l| l.starts_with("r ")),
        "the retained branch prefix must carry its resize record"
    );
    let replayed = run_replay_dump(game, after2 as u32 + 5, &replayable_s, &[]);
    assert_eq!(
        current_tick(&replayed), current_tick(&live2),
        "the branched log must replay to the live tick"
    );
    let mut sim = sim_state_names(game, 40);
    sim.remove("dt");
    assert_sim_state_matches(&live2, &replayed, &sim, "branched from-boot replay");
    for name in ["WINDOW_SIZE", "DRAWABLE_SIZE", "SCENE_CAPACITY"] {
        assert_eq!(
            live2["stone"][name], replayed["stone"][name],
            "branch replay must reproduce the retained resize's {name}"
        );
    }
}

/// The real Resume Live Here button runs during late draw, before the native
/// frame-end replay cleanup and tick advancement. Keep this arm separate from
/// STONE_BRANCH_AT_FRAME, whose hook runs after `run_one_frame` and therefore
/// cannot expose the production phase ordering.
#[test]
fn draw_phase_branch_while_paused_commits_at_displayed_tick() {
    let game = "bitgun";
    let branch_tick: i64 = 25;
    let dir = tempfile::tempdir().unwrap();
    let log_s = dir.path().join("draw-paused.replay").to_string_lossy().into_owned();

    let state = run_record_dump(
        game,
        120,
        &log_s,
        &[
            ("STONE_LIVE_REWIND", "1"),
            ("STONE_TIMELINE_SEEK_AT_FRAME", "40"),
            ("STONE_TIMELINE_SEEK_TO_TICK", "25"),
            ("STONE_BRANCH_DRAW_AT_FRAME", "70"),
        ],
    );
    let diag = &state["__stone_diag"];
    assert_eq!(diag["STONE_BRANCH_DRAW_HOOK_SEEN"].as_i64(), Some(70), "paused arm must request from late draw: {diag}");
    assert_eq!(diag["STONE_BRANCH_DRAW_ACTIVE"].as_i64(), Some(1), "paused draw request must see the rewound session: {diag}");
    assert_eq!(diag["STONE_BRANCH_COUNT"].as_i64(), Some(1), "draw-phase branch must commit: {diag}");
    assert_eq!(diag["STONE_BRANCH_REQUEST_TICK"].as_i64(), Some(branch_tick), "request sees the displayed tick: {diag}");
    assert_eq!(diag["STONE_BRANCH_COMMIT_TICK"].as_i64(), Some(branch_tick), "paused branch must not add a tick: {diag}");
    assert_eq!(diag["STONE_BRANCH_TICK_DELTA"].as_i64(), Some(0), "paused request must commit at the same boundary: {diag}");
    assert_eq!(diag["STONE_BRANCH_REPLAY_CLEANUP_DELTA"].as_i64(), Some(0), "paused branch has no replay tick to clean up: {diag}");
    assert_eq!(current_tick(&state), recorded_tick_lines(&log_s) as i64, "world tick and retained log prefix must agree");
    assert_eq!(diag["STONE_BRANCH_FIRST_RECORD_TICK"].as_i64(), Some(branch_tick), "the first new tick must follow the retained prefix: {diag}");
    assert_eq!(diag["STONE_BRANCH_BOUNDARY_MISMATCHES"].as_i64().unwrap_or(0), 0, "cursor/current_tick must agree at every boundary: {diag}");
    assert_eq!(state["stone"]["sim_mouse_override"], serde_json::Value::Null, "replay mouse override must be cleared after handoff");
    assert_eq!(state["stone"]["IMGUI_MOUSE_CAPTURE_OVERRIDE"], serde_json::Value::Null, "replay capture override must be cleared after handoff");
}

#[test]
fn draw_phase_branch_while_playing_includes_the_request_frame_tick() {
    let game = "bitgun";
    let dir = tempfile::tempdir().unwrap();
    let log_s = dir.path().join("draw-playing.replay").to_string_lossy().into_owned();

    let state = run_record_dump(
        game,
        120,
        &log_s,
        &[
            ("STONE_LIVE_REWIND", "1"),
            ("STONE_TIMELINE_SEEK_AT_FRAME", "40"),
            ("STONE_TIMELINE_SEEK_TO_TICK", "25"),
            ("STONE_LIVE_PLAY_AT_FRAME", "70"),
            ("STONE_BRANCH_DRAW_AT_FRAME", "80"),
        ],
    );
    let diag = &state["__stone_diag"];
    assert_eq!(diag["STONE_BRANCH_DRAW_HOOK_SEEN"].as_i64(), Some(80), "playing arm must request from late draw: {diag}");
    assert_eq!(diag["STONE_BRANCH_DRAW_ACTIVE"].as_i64(), Some(1), "playing draw request must see the rewound session: {diag}");
    let request_tick = diag["STONE_BRANCH_REQUEST_TICK"].as_i64().expect("playing request tick");
    let commit_tick = diag["STONE_BRANCH_COMMIT_TICK"].as_i64().expect("playing commit tick");
    assert_eq!(diag["STONE_BRANCH_COUNT"].as_i64(), Some(1), "draw-phase branch must commit: {diag}");
    assert!(request_tick > 25, "the playing arm must advance into the retained future before requesting: {diag}");
    assert_eq!(commit_tick, request_tick + 1, "the request-frame replay tick must run exactly once before commit: {diag}");
    assert_eq!(diag["STONE_BRANCH_TICK_DELTA"].as_i64(), Some(1), "playing branch must include the request-frame tick: {diag}");
    assert_eq!(diag["STONE_BRANCH_REPLAY_CLEANUP_DELTA"].as_i64(), Some(1), "playing branch must clean up while replay still owns the frame: {diag}");
    assert_eq!(current_tick(&state), recorded_tick_lines(&log_s) as i64, "world tick and retained log prefix must agree");
    assert_eq!(diag["STONE_BRANCH_FIRST_RECORD_TICK"].as_i64(), Some(commit_tick), "the first new tick must follow the retained prefix: {diag}");
    assert_eq!(diag["STONE_BRANCH_BOUNDARY_MISMATCHES"].as_i64().unwrap_or(0), 0, "cursor/current_tick must agree at every boundary: {diag}");
    assert_eq!(state["stone"]["sim_mouse_override"], serde_json::Value::Null, "replay mouse override must be cleared after handoff");
    assert_eq!(state["stone"]["IMGUI_MOUSE_CAPTURE_OVERRIDE"], serde_json::Value::Null, "replay capture override must be cleared after handoff");
}

#[test]
fn branch_projects_game_input_authority_and_drops_replay_latches() {
    let run_probe = |log_s: &str, fresh_mouse: bool| {
        let mut env = vec![
            ("STONE_INPUT_PROBE_KEY", "input_probe"),
            ("STONE_INPUT_PROBE_MOUSE_BUTTON", "1"),
            ("STONE_LIVE_REWIND", "1"),
            ("STONE_TIMELINE_SEEK_AT_FRAME", "40"),
            ("STONE_TIMELINE_SEEK_TO_TICK", "25"),
            ("STONE_LIVE_PLAY_AT_FRAME", "70"),
            ("STONE_BRANCH_DRAW_AT_FRAME", "80"),
            // These raw events arrive while replay owns the game lane.
            ("STONE_PHYSICAL_KEY_PRESS_AT", "75"),
            ("STONE_PHYSICAL_KEY_RELEASE_AT", "76"),
            ("STONE_PHYSICAL_KEY_PRESS_AT_2", "82"),
            ("STONE_PHYSICAL_KEY_RELEASE_AT_2", "83"),
            ("STONE_PHYSICAL_MOUSE_PRESS_AT", "80"),
            ("STONE_PHYSICAL_MOUSE_CAPTURED_AT", "80"),
            // Keep the captured Resume Live Here press physically held for
            // four live frames after handoff before releasing it.
            ("STONE_PHYSICAL_MOUSE_RELEASE_AT", "85"),
        ];
        if fresh_mouse {
            env.push(("STONE_PHYSICAL_MOUSE_PRESS_AT_2", "87"));
            env.push(("STONE_PHYSICAL_MOUSE_RELEASE_AT_2", "89"));
        }
        run_record_dump("bitgun", 120, log_s, &env)
    };

    let captured_dir = tempfile::tempdir().unwrap();
    let captured_log = captured_dir.path().join("captured-hold.replay").to_string_lossy().into_owned();
    let captured = run_probe(&captured_log, false);
    let captured_diag = &captured["__stone_diag"];
    assert_eq!(captured_diag["STONE_BRANCH_COUNT"].as_i64(), Some(1), "input handoff needs a successful branch: {captured_diag}");
    assert_eq!(captured_diag["STONE_INPUT_CAPTURED_MOUSE_INJECTED"].as_i64(), Some(1), "the captured-click control must run: {captured_diag}");
    assert_eq!(captured_diag["STONE_INPUT_PROBE_REPLAY_KEY_DOWN"].as_i64().unwrap_or(0), 0, "physical held input must not reach replay game logic: {captured_diag}");
    assert_eq!(captured_diag["STONE_INPUT_PROBE_REPLAY_KEY_PRESSED"].as_i64().unwrap_or(0), 0, "physical key latches must not reach replay game logic: {captured_diag}");
    assert_eq!(captured_diag["STONE_INPUT_PROBE_LIVE_KEY_PRESSED"].as_i64(), Some(1), "fresh keyboard input must reach the first eligible live tick: {captured_diag}");
    assert_eq!(captured_diag["STONE_INPUT_PROBE_REPLAY_MOUSE_DOWN"].as_i64().unwrap_or(0), 0, "physical mouse input must not reach replay game logic: {captured_diag}");
    assert_eq!(captured_diag["STONE_INPUT_PROBE_REPLAY_MOUSE_PRESSED"].as_i64().unwrap_or(0), 0, "a captured UI click must not reach replay game logic: {captured_diag}");
    assert_eq!(captured_diag["STONE_INPUT_PROBE_LIVE_MOUSE_DOWN"].as_i64().unwrap_or(0), 0, "the captured physical hold must stay out of live game input: {captured_diag}");
    assert_eq!(captured_diag["STONE_INPUT_PROBE_LIVE_MOUSE_PRESSED"].as_i64().unwrap_or(0), 0, "the captured physical press must not create a live press edge: {captured_diag}");
    assert_eq!(captured_diag["STONE_INPUT_PROBE_LIVE_MOUSE_RELEASED"].as_i64().unwrap_or(0), 0, "releasing the unseen captured press must not create a live release edge: {captured_diag}");

    let fresh_dir = tempfile::tempdir().unwrap();
    let fresh_log = fresh_dir.path().join("fresh-after-capture.replay").to_string_lossy().into_owned();
    let fresh = run_probe(&fresh_log, true);
    let diag = &fresh["__stone_diag"];
    assert_eq!(diag["STONE_BRANCH_COUNT"].as_i64(), Some(1), "fresh input arm needs a successful branch: {diag}");
    assert_eq!(diag["STONE_INPUT_PROBE_REPLAY_MOUSE_DOWN"].as_i64().unwrap_or(0), 0, "physical mouse input must not reach replay game logic: {diag}");
    assert_eq!(diag["STONE_INPUT_PROBE_REPLAY_MOUSE_PRESSED"].as_i64().unwrap_or(0), 0, "a captured UI click must not reach replay game logic: {diag}");
    assert_eq!(diag["STONE_INPUT_PROBE_LIVE_MOUSE_PRESSED"].as_i64(), Some(1), "a fresh post-release mouse press must create exactly one live press edge: {diag}");
    assert!(diag["STONE_INPUT_PROBE_LIVE_MOUSE_DOWN"].as_i64().is_some_and(|n| n > 0), "the fresh mouse press must reach live game logic as down: {diag}");
    assert_eq!(diag["STONE_INPUT_PROBE_LIVE_MOUSE_RELEASED"].as_i64(), Some(1), "only the fresh release may reach live game logic: {diag}");
    assert_eq!(fresh["stone"]["sim_mouse_override"], serde_json::Value::Null, "handoff clears replay mouse authority");
    assert_eq!(fresh["stone"]["IMGUI_MOUSE_CAPTURE_OVERRIDE"], serde_json::Value::Null, "handoff clears replay UI capture authority");
}

#[test]
fn resume_live_here_persists_the_branched_mag_world_not_the_discarded_future() {
    let dir = tempfile::tempdir().unwrap();
    let log_s = dir.path().join("mag-branch-persistence.replay").to_string_lossy().into_owned();
    let progress = dir.path().join("progress.json");
    let progress_s = progress.to_string_lossy().into_owned();
    let settings = dir.path().join("settings.json");
    let settings_s = settings.to_string_lossy().into_owned();
    let layout_s = dir.path().join("spellbook-layout.jsonl").to_string_lossy().into_owned();
    let bindings_s = dir.path().join("bindings.json").to_string_lossy().into_owned();
    let bindings_history = dir.path().join("spellbook-bindings.jsonl");
    let bindings_history_s = bindings_history.to_string_lossy().into_owned();
    std::fs::write(
        &progress,
        serde_json::to_vec_pretty(&serde_json::json!({
            "version": 3,
            "bank_gold": 111,
            "events": {},
            "quests": {"runs": [], "tracked": [], "collapsed": {}, "quarantined": []},
            "player": {
                "grid": [25, 8], "dead": false,
                "health_cur": 100.0, "mana_cur": 100.0, "stamina_cur": 20.0,
                "inventory": {"gold": 0, "bandage": 0, "bloody_bandage": 0},
                "reagents": [0, 0, 0, 0, 0, 0]
            },
            "spellbook": {"favorites": []}
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(&settings, br#"{"audio":{"master":0.25}}"#).unwrap();

    let state = run_record_dump(
        "mag",
        100,
        &log_s,
        &[
            ("MAG_BOOT_PLAYING", "1"),
            ("MAG_REPLAY_BRANCH_PERSISTENCE_FIXTURE", "1"),
            ("MAG_PROGRESS_PATH_OVERRIDE", &progress_s),
            ("MAG_LAYOUT_HISTORY_PATH_OVERRIDE", &layout_s),
            ("MAG_SETTINGS_PATH_OVERRIDE", &settings_s),
            ("MAG_BINDINGS_PATH_OVERRIDE", &bindings_s),
            ("MAG_BINDINGS_HISTORY_PATH_OVERRIDE", &bindings_history_s),
            ("STONE_LIVE_REWIND", "1"),
            ("STONE_TIMELINE_SEEK_AT_FRAME", "40"),
            ("STONE_TIMELINE_SEEK_TO_TICK", "10"),
            ("STONE_BRANCH_AT_FRAME", "70"),
        ],
    );
    let diag = &state["__stone_diag"];
    assert_eq!(diag["MAG_REPLAY_BRANCH_FUTURE_PROGRESS_WRITE"].as_i64(), Some(1),
        "the discarded future must first reach the real progress writer: {diag}");
    assert_eq!(diag["MAG_REPLAY_BRANCH_FUTURE_SETTINGS_WRITE"].as_i64(), Some(1),
        "the discarded future must first reach the real settings writer: {diag}");
    assert_eq!(diag["MAG_REPLAY_BRANCH_FUTURE_BINDINGS_WRITE"].as_i64(), Some(1),
        "the discarded future must first reach the real binding/history writer: {diag}");
    assert_eq!(diag["STONE_BRANCH_COUNT"].as_i64(), Some(1),
        "the fixture must take the production Resume Live Here path: {diag}");
    assert_eq!(diag["MAG_REPLAY_BRANCH_PERSISTENCE_RECONCILES"].as_i64(), Some(1),
        "the branched world must pass through one persistence reconciliation: {diag}");

    let persisted_progress: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&progress).unwrap()).unwrap();
    let persisted_settings: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&settings).unwrap()).unwrap();
    assert_eq!(persisted_progress["bank_gold"], 111,
        "the discarded future must not survive in progress.json");
    assert_eq!(persisted_settings["audio"]["master"], 0.25,
        "the discarded future must not survive in settings.json");
    let persisted_bindings_history = std::fs::read_to_string(&bindings_history).unwrap();
    assert!(!persisted_bindings_history.contains("f12"),
        "the discarded binding-history future must be replaced by the branched graph:\n{persisted_bindings_history}");
}

#[test]
fn literal_temporary_path_mag_product_smoke() {
    let dir = tempfile::tempdir().unwrap();
    let replay = dir.path().join("literal-product.replay");
    let replay_s = replay.to_string_lossy().into_owned();
    let first_progress = dir.path().join("first-progress.json");
    let first_progress_s = first_progress.to_string_lossy().into_owned();
    let second_progress = dir.path().join("second-progress.json");
    let second_progress_s = second_progress.to_string_lossy().into_owned();
    let settings = dir.path().join("settings.json");
    let settings_s = settings.to_string_lossy().into_owned();
    let bindings = dir.path().join("bindings.json");
    let bindings_s = bindings.to_string_lossy().into_owned();
    let binding_history = dir.path().join("bindings-history.jsonl");
    let binding_history_s = binding_history.to_string_lossy().into_owned();
    let layout_history = dir.path().join("layout-history.jsonl");
    let layout_history_s = layout_history.to_string_lossy().into_owned();
    let editor_history = dir.path().join("editor-history.jsonl");
    let editor_history_s = editor_history.to_string_lossy().into_owned();
    let editor_history_v1 = dir.path().join("editor-history.json");
    let editor_history_v1_s = editor_history_v1.to_string_lossy().into_owned();
    std::fs::write(&settings, r#"{"privacy":{"analytics":1,"analytics_version":3}}"#).unwrap();

    let real_paths = [
        workspace_root().join("mag/saves/progress.json"),
        workspace_root().join("mag/saves/history.jsonl"),
        workspace_root().join("mag/saves/history.json"),
    ];
    let real_before = real_paths.iter().map(|path| std::fs::read(path).ok()).collect::<Vec<_>>();

    let run = run_record_dump_observed(
        "mag",
        540,
        &replay_s,
        &[
            ("MAG_BOOT_MENU", "1"),
            ("MAG_LITERAL_PRODUCT_SMOKE", "1"),
            ("MAG_PROGRESS_PATH_OVERRIDE", &first_progress_s),
            ("MAG_LITERAL_PRODUCT_SMOKE_SECOND_PATH", &second_progress_s),
            ("MAG_SETTINGS_PATH_OVERRIDE", &settings_s),
            ("MAG_BINDINGS_PATH_OVERRIDE", &bindings_s),
            ("MAG_BINDINGS_HISTORY_PATH_OVERRIDE", &binding_history_s),
            ("MAG_LAYOUT_HISTORY_PATH_OVERRIDE", &layout_history_s),
            ("MAG_LITERAL_PRODUCT_SMOKE_HISTORY_PATH", &editor_history_s),
            ("MAG_LITERAL_PRODUCT_SMOKE_HISTORY_V1_PATH", &editor_history_v1_s),
            ("STONE_LIVE_REWIND", "1"),
            ("STONE_KEYFRAME_INTERVAL", "10"),
            ("STONE_PHYSICAL_MOUSE_PRESS_AT", "2"),
            ("STONE_PHYSICAL_MOUSE_RELEASE_AT", "3"),
            ("STONE_PHYSICAL_KEY_PRESS_AT", "100"),
            ("STONE_PHYSICAL_KEY_RELEASE_AT", "101"),
            ("STONE_PHYSICAL_KEY", "down"),
            ("STONE_PHYSICAL_SCANCODE", "down"),
            ("STONE_PHYSICAL_KEY_PRESS_AT_2", "105"),
            ("STONE_PHYSICAL_KEY_RELEASE_AT_2", "106"),
            ("STONE_PHYSICAL_KEY_2", "return"),
            ("STONE_PHYSICAL_SCANCODE_2", "return"),
            ("STONE_TIMELINE_SEEK_AT_FRAME", "260"),
            ("STONE_TIMELINE_SEEK_TO_TICK", "150"),
            ("STONE_TIMELINE_SEEK_AT_FRAME_2", "430"),
            ("STONE_TIMELINE_SEEK_TO_TICK_2", "180"),
            ("STONE_BRANCH_DRAW_AT_FRAME", "480"),
        ],
    );
    assert!(run.status_success, "literal product run failed:\n{}", run.stderr);
    assert!(
        run.stderr.contains("completed/requested=540/540 frame_errors=0"),
        "the literal bounded run must report zero frame errors:\n{}",
        run.stderr
    );
    let diag = &run.state["__stone_diag"];
    assert_eq!(diag["MAG_LITERAL_SMOKE_MENU"].as_i64(), Some(1), "{diag}");
    assert_eq!(diag["MAG_LITERAL_SMOKE_FIRST_START"].as_i64(), Some(1), "{diag}");
    assert!(diag["MAG_LITERAL_SMOKE_FIRST_START_FRAME"].as_i64().is_some_and(|n| n < 150), "the first seek target must contain the live run: {diag}");
    assert_eq!(diag["STONE_SEEK_COUNT"].as_i64(), Some(2), "{diag}");
    assert_eq!(diag["STONE_SEEK_FROM_TICK"].as_i64(), Some(150), "the second scrub must start at the backward target: {diag}");
    assert_eq!(diag["STONE_SEEK_TO_TICK"].as_i64(), Some(180), "the second scrub must move forward: {diag}");
    assert_eq!(diag["STONE_BRANCH_DRAW_ACTIVE"].as_i64(), Some(1), "Resume Live Here must run from the timeline draw path: {diag}");
    assert_eq!(diag["STONE_BRANCH_COUNT"].as_i64(), Some(1), "Resume Live Here must commit: {diag}");
    assert_eq!(diag["MAG_LITERAL_SMOKE_FIRST_SURVIVED"].as_i64(), Some(1), "{diag}");
    for field in ["BANK", "POSITION", "INVENTORY", "REAGENTS", "EVENTS", "QUEST", "JOURNAL", "HISTORY", "PANEL", "TARGET"] {
        assert_eq!(diag[format!("MAG_LITERAL_SMOKE_FIRST_{field}")].as_i64(), Some(1), "first-run {field} state did not survive replay: {diag}");
    }
    assert_eq!(diag["MAG_LITERAL_SMOKE_SECOND_CLEAN"].as_i64(), Some(1), "{diag}");
    for field in ["BANK", "POSITION", "INVENTORY", "REAGENTS", "EVENTS", "QUEST", "JOURNAL", "HISTORY", "PANEL", "TARGET", "RETIRED", "ARENA"] {
        assert_eq!(diag[format!("MAG_LITERAL_SMOKE_SECOND_{field}")].as_i64(), Some(1), "second-run {field} state was not clean: {diag}");
    }
    assert_eq!(diag["MAG_LITERAL_SMOKE_FINAL_MENU"].as_i64(), Some(1), "{diag}");
    assert_eq!(diag["MAG_LITERAL_SMOKE_TEMP_PATHS"].as_i64(), Some(1), "every writable product path must be temporary: {diag}");
    assert_eq!(diag["MAG_LITERAL_SMOKE_STARTS_BEFORE_SECOND"].as_i64(), Some(1), "the first product run must remain active through the replay scrub: {diag}");
    assert_eq!(diag["MAG_LITERAL_SMOKE_STARTS"].as_i64(), Some(2), "exactly two product runs must start: {diag}");
    assert_eq!(diag["MAG_LITERAL_SMOKE_STOPS"].as_i64(), Some(2), "{diag}");
    assert!(recorded_tick_lines(&replay_s) > 0, "the literal product run must record a replay");
    for path in [&first_progress, &settings, &bindings, &binding_history, &layout_history] {
        assert!(path.exists(), "the product path must write only inside the temporary root: {}", path.display());
    }
    let real_after = real_paths.iter().map(|path| std::fs::read(path).ok()).collect::<Vec<_>>();
    assert_eq!(real_after, real_before, "the literal smoke must not write MAG's real persistence files");
}

#[test]
fn recorded_game_input_is_projected_into_replay_authority() {
    let dir = tempfile::tempdir().unwrap();
    let log_s = dir.path().join("recorded-input.replay").to_string_lossy().into_owned();
    let _ = run_record_dump("bitgun", 40, &log_s, &[]);
    let text = std::fs::read_to_string(&log_s).unwrap();
    let mut tick = 0;
    let rewritten = text.lines().map(|line| {
        if !line.starts_with("t ") { return line.to_string() }
        let mut fields: Vec<&str> = line.split_whitespace().collect();
        if tick == 10 { fields[6] = "input_probe" }
        tick += 1;
        fields.join(" ")
    }).collect::<Vec<_>>().join("\n");
    std::fs::write(&log_s, rewritten + "\n").unwrap();

    let replayed = run_replay_dump("bitgun", 40, &log_s, &[("STONE_INPUT_PROBE_KEY", "input_probe")]);
    let diag = &replayed["__stone_diag"];
    assert!(diag["STONE_INPUT_PROBE_REPLAY_KEY_DOWN"].as_i64().unwrap_or(0) > 0, "recorded held input must reach replay game logic: {diag}");
    assert!(diag["STONE_INPUT_PROBE_REPLAY_KEY_PRESSED"].as_i64().unwrap_or(0) > 0, "recorded input must produce its replay press edge: {diag}");
}

#[test]
fn replay_of_a_log_with_a_midrun_resize_matches_recording() {
    let game = "bitgun";
    let dir = tempfile::tempdir().unwrap();
    let log_s = dir.path().join("resize.replay").to_string_lossy().into_owned();
    let recorded = run_record_dump(
        game,
        60,
        &log_s,
        &[
            ("STONE_CHECKPOINT_INTERVAL", "1"),
            ("STONE_RESIZE_AT_FRAME", "30"),
            ("STONE_RESIZE_W", "1000"),
            ("STONE_RESIZE_H", "700"),
        ],
    );
    assert!(
        replay_lines(&log_s).iter().any(|l| l.starts_with("r ")),
        "the fixture must actually carry a resize line"
    );
    assert_eq!(recorded["stone"]["WINDOW_SIZE"]["x"], 1000);
    assert_eq!(recorded["stone"]["WINDOW_SIZE"]["y"], 700);
    assert_eq!(recorded["stone"]["DRAWABLE_SIZE"]["x"], 1000);
    assert_eq!(recorded["stone"]["DRAWABLE_SIZE"]["y"], 700);

    let replayed = run_replay_dump(game, 70, &log_s, &[]);
    assert_eq!(
        current_tick(&replayed),
        current_tick(&recorded),
        "ordinary from-boot replay must reach the recorded horizon"
    );
    for name in ["WINDOW_SIZE", "DRAWABLE_SIZE", "SCENE_CAPACITY"] {
        assert_eq!(
            replayed["stone"][name], recorded["stone"][name],
            "replay must reproduce the real {name} after the resize"
        );
    }
    let sim = sim_state_names(game, 60);
    assert_sim_state_matches(&replayed, &recorded, &sim, "ordinary mid-run resize replay");
}

#[test]
fn seek_across_midrun_resize_matches_plain_playback() {
    let dir = tempfile::tempdir().unwrap();
    let log_s = dir.path().join("resize-seek.replay").to_string_lossy().into_owned();
    let _recorded = run_record_dump(
        "bitgun",
        60,
        &log_s,
        &[
            ("STONE_CHECKPOINT_INTERVAL", "1"),
            ("STONE_RESIZE_AT_FRAME", "30"),
            ("STONE_RESIZE_W", "1000"),
            ("STONE_RESIZE_H", "700"),
        ],
    );
    let reference = run_replay_dump(
        "bitgun",
        70,
        &log_s,
        &[("STONE_START_PAUSED", "1"), ("STONE_STEP_FRAMES", "45")],
    );
    let sought = run_replay_dump(
        "bitgun",
        120,
        &log_s,
        &[
            ("STONE_KEYFRAME_INTERVAL", "60"),
            ("STONE_TIMELINE_SEEK_AT_FRAME", "60"),
            ("STONE_TIMELINE_SEEK_TO_TICK", "45"),
        ],
    );
    assert_eq!(current_tick(&reference), 45);
    assert_eq!(current_tick(&sought), 45);
    assert_eq!(reference["stone"]["WINDOW_SIZE"]["x"], 1000);
    assert_eq!(reference["stone"]["WINDOW_SIZE"]["y"], 700);
    assert_eq!(reference["stone"]["DRAWABLE_SIZE"]["x"], 1000);
    assert_eq!(reference["stone"]["DRAWABLE_SIZE"]["y"], 700);
    for name in ["WINDOW_SIZE", "DRAWABLE_SIZE", "SCENE_CAPACITY"] {
        assert_eq!(
            sought["stone"][name], reference["stone"][name],
            "seek across resize must retain the real {name}"
        );
    }
    let sim = sim_state_names("bitgun", 60);
    assert_sim_state_matches(&sought, &reference, &sim, "seek across mid-run resize");
}

#[test]
fn screenshot_after_render_skipped_seek_reaches_its_frame_budget() {
    let dir = tempfile::tempdir().unwrap();
    let log_s = dir.path().join("screenshot-seek.replay").to_string_lossy().into_owned();
    let state = run_record_dump(
        "bitgun",
        75,
        &log_s,
        &[
            ("STONE_LIVE_REWIND", "1"),
            ("STONE_TIMELINE_SEEK_AT_FRAME", "40"),
            ("STONE_TIMELINE_SEEK_TO_TICK", "25"),
        ],
    );
    assert_eq!(
        state["stone"]["screenshot_taken_via_env"], true,
        "TAKE_SCREENSHOT must complete after the hidden seek"
    );
    assert!(
        state["stone"]["screenshot_frame_counter"].as_i64().unwrap_or(0) >= 75,
        "the screenshot counter must reach the FRAME_COUNT budget: {}",
        state["stone"]["screenshot_frame_counter"]
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
    assert_eq!(diag["STONE_BRANCH_FAILURE_SURFACES_UNCHANGED"].as_i64(), Some(1), "a failed rewrite must prove every future surface and owner stayed intact: {diag}");
    assert_eq!(diag["STONE_BRANCH_INPUT_HANDOFFS"].as_i64().unwrap_or(0), 0, "a failed rewrite must not hand input back: {diag}");
    assert_eq!(diag["STONE_BRANCH_INPUT_HANDOFF_EARLY"].as_i64().unwrap_or(0), 0, "a failed rewrite must not hand input back before commit: {diag}");
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
        game, 200, &log_s,
        &[
            ("STONE_TIMELINE_SEEK_AT_FRAME", "40"),
            ("STONE_TIMELINE_SEEK_TO_TICK", "25"),
            ("STONE_BRANCH_AT_FRAME", "70"),
            ("STONE_TIMELINE_SEEK_AT_FRAME_2", "110"),
            ("STONE_TIMELINE_SEEK_TO_TICK_2", "55"),
        ],
    );
    let diag = &live["__stone_diag"];
    assert_eq!(diag["STONE_BRANCH_COUNT"].as_i64(), Some(1), "{diag}");
    assert_eq!(diag["STONE_LIVE_REWIND_BRANCH_INDEX_REBASED"].as_i64(), Some(1), "branch must rebase the lazy index: {diag}");
    assert_eq!(diag["STONE_LIVE_REWIND_HISTORY_FLOOR_TICK"].as_i64(), Some(0), "the simplified history has no wall-clock floor: {diag}");
    assert_eq!(diag.get("STONE_LIVE_REWIND_POST_BRANCH_CAPTURES").and_then(serde_json::Value::as_i64), None, "live recording must not reacquire a periodic capture loop: {diag}");
    assert_eq!(diag["STONE_LIVE_REWIND_COUNT"].as_i64(), Some(2), "both rewinds must fire: {diag}");
    let horizon = diag["STONE_LIVE_REWIND_HORIZON"].as_i64().expect("horizon");
    assert!(horizon > 25, "the branched session must have recorded past T (got {horizon})");
    assert_eq!(
        horizon, recorded_tick_lines(&log_s) as i64,
        "the second rewind's horizon must be the branched history, not the discarded future: {diag}"
    );
    assert_eq!(diag["STONE_LIVE_REWIND_RESTORE_TICK"].as_i64(), Some(0), "without a prior live seek the second rewind restores from boot: {diag}");
    assert_eq!(current_tick(&live), 55, "the second rewind must land at its target: {diag}");
}

#[test]
fn branch_rebases_live_rewind_floor_before_an_immediate_second_seek() {
    let dir = tempfile::tempdir().unwrap();
    let log_s = dir.path().join("rebranch-floor.replay").to_string_lossy().into_owned();
    let live = run_record_dump(
        "bitgun",
        90,
        &log_s,
        &[
            ("STONE_TIMELINE_SEEK_AT_FRAME", "40"),
            ("STONE_TIMELINE_SEEK_TO_TICK", "25"),
            ("STONE_BRANCH_AT_FRAME", "70"),
            ("STONE_TIMELINE_SEEK_AT_FRAME_2", "72"),
            ("STONE_TIMELINE_SEEK_TO_TICK_2", "10"),
        ],
    );
    let diag = &live["__stone_diag"];
    let floor = diag["STONE_LIVE_REWIND_HISTORY_FLOOR_TICK"].as_i64().unwrap_or_else(|| panic!("branch history floor: {diag}"));
    assert_eq!(floor, 0, "the simplified branch keeps the from-boot history: {diag}");
    assert_eq!(diag["STONE_LIVE_REWIND_HISTORY_FLOOR_AT_SEEK"].as_i64(), Some(floor), "the live seek must use the same from-boot history floor: {diag}");
    assert_eq!(diag["STONE_LIVE_REWIND_COUNT"].as_i64(), Some(2), "the immediate second seek must be admitted: {diag}");
    assert_eq!(diag["STONE_LIVE_REWIND_REFUSED"].as_i64(), Some(0), "floor rebasing must not refuse the surviving boundary: {diag}");
    assert_eq!(current_tick(&live), 10, "the immediate second rewind must land at its target: {diag}");
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
        diag["STONE_KEYFRAME_CAPTURE_MAX_MS"].as_f64().is_some_and(|v| v > 0.0),
        "the run must report its worst observed capture hitch: {diag}"
    );
    assert!(
        diag["STONE_KEYFRAME_GLOBALS"].as_i64().is_some_and(|n| n > 0)
            && diag["STONE_KEYFRAME_NODES"].as_i64().is_some_and(|n| n > 0),
        "a keyframe must report the population it copied: {diag}"
    );
    assert!(
        diag["STONE_KEYFRAME_SHUTDOWN_BEFORE"].as_i64().is_some_and(|n| n > 0)
            && diag["STONE_KEYFRAME_SHUTDOWN_AFTER"].as_i64() == Some(0),
        "shutdown must release every retained keyframe owner: {diag}"
    );
    assert!(
        diag["STONE_STATE_IMAGE_IMAGES_BEFORE_SHUTDOWN"].as_i64().is_some_and(|n| n > 0)
            && diag["STONE_STATE_IMAGE_IMAGES_AFTER_SHUTDOWN"].as_i64() == Some(0),
        "shutdown must release the StateImage images owned by keyframes: {diag}"
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

    // Keyframe BYTES are only honest where real memory was imaged. Bitgun and
    // bopt hold game buffers in addition to Stone's unavoidable storage floor;
    // a minimal game proves that floor is measured rather than reported as a
    // fictitious zero.
    let seek_diag = &seeked["__stone_diag"];
    let bitgun_bytes = seek_diag["STONE_KEYFRAME_STORAGE_BYTES"].as_i64().expect("bitgun storage bytes");
    assert!(
        bitgun_bytes > 0,
        "bitgun holds buffer storage in sim state, so its keyframe must report imaged bytes: {seek_diag}"
    );

    let minimal_game = tempfile::tempdir().unwrap();
    std::fs::write(
        minimal_game.path().join("stick.toml"),
        format!(
            "[package]\nname = \"replay_storage_floor\"\nversion = \"0.1.0\"\ntype = \"bin\"\n\n\
             [dependencies]\nstone = {{ path = \"{}\" }}\n\n\
             [build.vars]\nGIT_SHA = \"test\"\n",
            workspace_root().parent().expect("repo root").join("stone").display()
        ),
    )
    .unwrap();
    std::fs::write(
        minimal_game.path().join("main.rock"),
        "fn main() {\n    stone_run_game(GameConfig { title: \"replay storage floor\", width: 64, height: 64 })\n}\n",
    )
    .unwrap();
    let minimal_game_s = minimal_game.path().to_string_lossy().into_owned();
    let minimal_log = dir.path().join("minimal-storage.replay").to_string_lossy().into_owned();
    run_record_dump(&minimal_game_s, 25, &minimal_log, &[]);
    let minimal = run_replay_dump(&minimal_game_s, 30, &minimal_log, &[("STONE_KEYFRAME_INTERVAL", "10")]);
    let minimal_diag = &minimal["__stone_diag"];
    let engine_floor = minimal_diag["STONE_KEYFRAME_STORAGE_BYTES"]
        .as_i64().expect("minimal Stone storage bytes");
    assert!(
        engine_floor > 0 && bitgun_bytes > engine_floor,
        "Bitgun storage must exceed the nonzero Stone engine floor: floor={engine_floor} \
         bitgun={bitgun_bytes} diag={minimal_diag}"
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
        bytes > engine_floor,
        "bopt buffer storage must exceed the Stone engine floor {engine_floor}: {}",
        bopt["__stone_diag"]
    );
    println!(
        "[replay-costs] keyframe buffer storage: engine floor {engine_floor}B, \
         bitgun {bitgun_bytes}B, bopt {bytes}B"
    );
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

/// The engine keyframe walk (`dc` in stone/engine/snapshot.rock) must copy a
/// CAPTURING CodeRef env record held in a registered global, keep its captures
/// isolated across a restore, and leave its logical slot shared with the live
/// program. The probe publishes a same-ABI body edit after the capture, so a
/// restored record that copied a backend body fails this test.
/// The bitgun probe (`lambda_snapshot_probe.rock`, gated by BITGUN_LAMBDA_STATE)
/// plants such a lambda plus a plain int counter it drives each tick.
///
/// Oracle is the COUNTER, never the serialized lambda: dump-state renders env
/// records as "<function>" (capture-blind), so a state-comparison on the lambda
/// itself passes even when the fix is absent. The counter's post-restore
/// trajectory observes the lambda's behavior instead.
///
/// Vehicle is SCRUB, not plain RESTORE_AT: a backward scrub restores the nearest
/// keyframe AND resims forward, invoking the restored lambda after the edit.
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

    // Reference: the true tick-35 state, including the compatible body edit at
    // tick 30.
    let reference = run_replay_dump(
        game, 60, &log_s,
        &[("BITGUN_LAMBDA_STATE", "1"), ("BITGUN_LAMBDA_STATE_EDIT", "1"),
          ("STONE_START_PAUSED", "1"), ("STONE_STEP_FRAMES", "35")],
    );
    assert_eq!(current_tick(&reference), 35, "reference must sit at tick 35");

    // Scrub run: play to 50, scrub back to 35 -> restore keyframe 20 and resim
    // through the post-capture body edit at tick 30.
    let scrubbed = run_replay_dump(
        game, 60, &log_s,
        &[
            ("BITGUN_LAMBDA_STATE", "1"),
            ("BITGUN_LAMBDA_STATE_EDIT", "1"),
            ("STONE_KEYFRAME_INTERVAL", "10"),
            ("STONE_SCRUB_AT_TICK", "50"),
            ("STONE_SCRUB_TO_TICK", "35"),
        ],
    );
    assert_eq!(current_tick(&scrubbed), 35, "after scrubbing back to 35, current_tick must be 35");

    // The oracle: the lambda-driven counter must be non-trivial (the lambda
    // actually ran) and match the plain run-to-35 exactly -- the restored lambda
    // was callable, its capture isolated, and its slot saw the edit. NOT a
    // comparison of the serialized lambda global (dump-state renders it
    // "<function>" either way).
    let ref_counter = lambda_counter(&reference);
    let scrub_counter = lambda_counter(&scrubbed);
    assert!(ref_counter > 0, "reference lambda must have driven the counter (got {ref_counter})");
    assert_eq!(
        scrub_counter, ref_counter,
        "post-scrub counter must match the plain run-to-35 (got {scrub_counter} vs {ref_counter}); \
         a stalled counter means the restored CodeRef was uncallable or stale"
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
