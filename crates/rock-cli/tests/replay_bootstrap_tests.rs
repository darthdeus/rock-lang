//! Cross-process replay bootstrap coverage for MAG's file-backed progress and
//! spell-layout history.
//!
//! Each fixture runs the real MAG boot consumer in one process, lets a
//! production-shaped action persist B, and then starts a second process with
//! B still on disk. The replay must restore the recording-time input A before
//! the action runs again, and playback must not write B's file.

use std::path::{Path, PathBuf};
use std::process::Command;

fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
}

fn run_mag(
    frames: u32,
    record: Option<&Path>,
    replay: Option<&Path>,
    env: &[(&str, &str)],
) -> serde_json::Value {
    let scratch = tempfile::tempdir().expect("scratch dir");
    let mut args = vec![
        "frame".to_string(),
        "mag".to_string(),
        "-n".to_string(),
        frames.to_string(),
        "--dump-state".to_string(),
        "--output".to_string(),
        scratch.path().to_string_lossy().into_owned(),
    ];
    if let Some(path) = record {
        args.push("--record".to_string());
        args.push(path.to_string_lossy().into_owned());
    }
    if let Some(path) = replay {
        args.push("--replay".to_string());
        args.push(path.to_string_lossy().into_owned());
    }

    let output = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args(&args)
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .envs(env.iter().copied())
        .current_dir(workspace_root())
        .output()
        .expect("spawn MAG frame");
    assert!(
        output.status.success(),
        "MAG frame failed: {:?}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
        panic!(
            "MAG dump-state was not JSON ({err})\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn run_mag_raw(
    frames: u32,
    record: Option<&Path>,
    replay: Option<&Path>,
    env: &[(&str, &str)],
) -> std::process::Output {
    let scratch = tempfile::tempdir().expect("scratch dir");
    let mut args = vec![
        "frame".to_string(),
        "mag".to_string(),
        "-n".to_string(),
        frames.to_string(),
        "--output".to_string(),
        scratch.path().to_string_lossy().into_owned(),
    ];
    if let Some(path) = record {
        args.push("--record".to_string());
        args.push(path.to_string_lossy().into_owned());
    }
    if let Some(path) = replay {
        args.push("--replay".to_string());
        args.push(path.to_string_lossy().into_owned());
    }
    Command::new(env!("CARGO_BIN_EXE_rock"))
        .args(&args)
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .envs(env.iter().copied())
        .current_dir(workspace_root())
        .output()
        .expect("spawn MAG frame")
}

fn fixture_env(
    kind: &str,
    progress: &Path,
    layout: &Path,
    settings: &Path,
) -> [(&'static str, String); 5] {
    [
        ("MAG_BOOT_PLAYING", "1".to_string()),
        ("MAG_REPLAY_BOOTSTRAP_FIXTURE", kind.to_string()),
        (
            "MAG_PROGRESS_PATH_OVERRIDE",
            progress.to_string_lossy().into_owned(),
        ),
        (
            "MAG_LAYOUT_HISTORY_PATH_OVERRIDE",
            layout.to_string_lossy().into_owned(),
        ),
        (
            "MAG_SETTINGS_PATH_OVERRIDE",
            settings.to_string_lossy().into_owned(),
        ),
    ]
}

fn env_refs<'a>(env: &'a [(&'static str, String)]) -> Vec<(&'static str, &'a str)> {
    env.iter().map(|(key, value)| (*key, value.as_str())).collect()
}

fn progress_projection(state: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "bank_gold": state["__stone_diag"]["MAG_REPLAY_BOOTSTRAP_BANK_GOLD"],
        "event_count": state["__stone_diag"]["MAG_REPLAY_BOOTSTRAP_EVENT_COUNT"],
        "quest_count": state["__stone_diag"]["MAG_REPLAY_BOOTSTRAP_QUEST_COUNT"],
        "quest_collapsed": state["__stone_diag"]["MAG_REPLAY_BOOTSTRAP_QUEST_COLLAPSED"],
        "player_x": state["__stone_diag"]["MAG_REPLAY_BOOTSTRAP_PLAYER_X"],
        "player_y": state["__stone_diag"]["MAG_REPLAY_BOOTSTRAP_PLAYER_Y"],
        "inventory_gold": state["__stone_diag"]["MAG_REPLAY_BOOTSTRAP_INVENTORY_GOLD"],
        "reagent_0": state["__stone_diag"]["MAG_REPLAY_BOOTSTRAP_REAGENT_0"],
    })
}

fn layout_projection(state: &serde_json::Value) -> serde_json::Value {
    state["mag"]["SPELL_STRIPS"].clone()
}

fn empty_progress_path(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("progress.json")
}

fn known_progress_a() -> Vec<u8> {
    serde_json::to_vec_pretty(&serde_json::json!({
        "version": 3,
        "bank_gold": 41,
        "events": {
            "replay_bootstrap_seed": {"c": 2, "f": 11, "l": 22}
        },
        "quests": {
            "runs": [],
            "tracked": [],
            "collapsed": {},
            "quarantined": []
        },
        "player": {
            "grid": [5, 5],
            "dead": false,
            "health_cur": 80.0,
            "mana_cur": 70.0,
            "stamina_cur": 15.0,
            "inventory": {"gold": 7, "bandage": 2, "bloody_bandage": 1},
            "reagents": [1, 2, 3, 4, 5, 6]
        },
        "spellbook": {"favorites": []}
    }))
    .expect("serialize known progress A")
}

#[test]
fn settings_bootstrap_replays_a_real_ui_setting_without_touching_disk_b() {
    let dir = tempfile::tempdir().expect("fixture dir");
    let progress = empty_progress_path(&dir);
    let layout = dir.path().join("spellbook-layout.jsonl");
    let settings = dir.path().join("settings.json");
    let replay = dir.path().join("settings.replay");
    let settings_a = br#"{"graphics":{"world_scale":0.75}}"#;
    let settings_b = br#"{"graphics":{"world_scale":2.0}}"#;
    std::fs::write(&settings, settings_a).expect("write settings A");

    let env_owned = fixture_env("settings", &progress, &layout, &settings);
    let env = env_refs(&env_owned);
    let recorded = run_mag(2, Some(&replay), None, &env);
    std::fs::write(&settings, settings_b).expect("mutate settings to B");
    let persisted_b = std::fs::read(&settings).expect("read settings B");
    let replayed = run_mag(2, None, Some(&replay), &env);

    assert_eq!(
        recorded["mag"]["SETTINGS"]["graphics"]["world_scale"],
        serde_json::json!(0.75),
        "recording must reach the real MAG settings consumer"
    );
    assert_eq!(
        replayed["mag"]["SETTINGS"]["graphics"]["world_scale"],
        serde_json::json!(0.75),
        "playback must use captured settings A rather than live disk B"
    );
    assert_eq!(
        std::fs::read(&settings).expect("settings B remains present"),
        persisted_b,
        "settings playback must leave B byte-identical"
    );
    assert!(
        replayed["__stone_diag"]["MAG_SETTINGS_PLAYBACK_SAVE_SKIPS"]
            .as_i64()
            .is_some_and(|n| n > 0),
        "playback must route settings writes through the no-op authority"
    );
    let log = std::fs::read_to_string(&replay).expect("read settings replay");
    assert!(
        log.lines().any(|line| line.starts_with("boot ")),
        "settings must be captured through the generic bootstrap wire\n{log}"
    );
}

#[test]
fn repeated_settings_consumer_reuses_the_first_bootstrap_read() {
    let dir = tempfile::tempdir().expect("fixture dir");
    let progress = empty_progress_path(&dir);
    let layout = dir.path().join("spellbook-layout.jsonl");
    let settings = dir.path().join("settings.json");
    let replay = dir.path().join("settings-repeat.replay");
    std::fs::write(&settings, br#"{"graphics":{"world_scale":0.75}}"#)
        .expect("write settings");

    let env_owned = fixture_env("settings", &progress, &layout, &settings);
    let mut env = env_refs(&env_owned);
    env.push(("STONE_QUIET", "0"));
    let state = run_mag(2, Some(&replay), None, &env);
    let diag = &state["__stone_diag"];
    assert_eq!(
        diag["MAG_SETTINGS_CONSUMER_CALLS"].as_i64(),
        Some(2),
        "the pre-window and non-quiet audio settings consumers must both run: {diag}"
    );
    assert!(
        diag["STONE_REPLAY_BOOTSTRAP_REUSES"].as_i64().is_some_and(|n| n > 0),
        "the audio-init consumer must reuse the settings captured before window creation"
    );
    assert_eq!(
        diag["STONE_REPLAY_BOOTSTRAP_DISK_PROBES"].as_i64(),
        Some(3),
        "recording must probe each logical file once, including the absent progress and layout files: {diag}"
    );
    assert_eq!(
        diag["STONE_REPLAY_BOOTSTRAP_FILE_READS"].as_i64(),
        Some(1),
        "only the present settings file may be read from disk: {diag}"
    );

    let settings_key = "6d61672f73657474696e67732e6a736f6e";
    let text = std::fs::read_to_string(&replay).expect("read replay");
    assert_eq!(
        text.lines()
            .filter(|line| line.starts_with(&format!("boot {settings_key} ")))
            .count(),
        1,
        "one logical bootstrap key must have one wire entry\n{text}"
    );
}

#[test]
fn repeated_settings_consumer_reuses_absent_bootstrap_without_second_disk_probe() {
    let dir = tempfile::tempdir().expect("fixture dir");
    let progress = empty_progress_path(&dir);
    let layout = dir.path().join("spellbook-layout.jsonl");
    let settings = dir.path().join("settings.json");
    let replay = dir.path().join("settings-absent-repeat.replay");
    assert!(!settings.exists(), "the absent-settings arm must start without a file");

    let env_owned = fixture_env("settings", &progress, &layout, &settings);
    let mut env = env_refs(&env_owned);
    env.push(("STONE_QUIET", "0"));
    let state = run_mag(2, Some(&replay), None, &env);
    let diag = &state["__stone_diag"];
    assert_eq!(
        diag["MAG_SETTINGS_CONSUMER_CALLS"].as_i64(),
        Some(2),
        "both settings consumers must execute for an absent file too: {diag}"
    );
    assert_eq!(
        diag["STONE_REPLAY_BOOTSTRAP_REUSES"].as_i64(),
        Some(1),
        "the second consumer must reuse the current settings absence: {diag}"
    );
    assert_eq!(
        diag["STONE_REPLAY_BOOTSTRAP_DISK_PROBES"].as_i64(),
        Some(3),
        "each absent logical file must be probed only once: {diag}"
    );
    assert_eq!(
        diag["STONE_REPLAY_BOOTSTRAP_FILE_READS"].as_i64().unwrap_or(0),
        0,
        "absence must not turn into a disk read on the repeated consumer: {diag}"
    );

    let text = std::fs::read_to_string(&replay).expect("read replay");
    let settings_key = "6d61672f73657474696e67732e6a736f6e";
    assert_eq!(
        text.lines()
            .filter(|line| line.starts_with(&format!("boot {settings_key} 0 -")))
            .count(),
        1,
        "one authoritative settings absence entry must be emitted\n{text}"
    );
    assert!(
        !text.lines().any(|line| line.starts_with("boot 6d61672f6c65676163792d73657474696e67732e6a736f6e ")),
        "a temporary settings override must not consult or capture the author's legacy path\n{text}"
    );
}

#[test]
fn progress_bootstrap_captures_absence_and_keeps_b_read_only() {
    let dir = tempfile::tempdir().expect("fixture dir");
    let progress = empty_progress_path(&dir);
    let layout = dir.path().join("spellbook-layout.jsonl");
    let settings = dir.path().join("settings.json");
    let replay = dir.path().join("progress.replay");
    assert!(!progress.exists(), "recording arm must begin with an absent save");

    let env_owned = fixture_env("progress", &progress, &layout, &settings);
    let env = env_refs(&env_owned);
    let recorded = run_mag(1, Some(&replay), None, &env);
    let persisted_b = std::fs::read(&progress).expect("record action must create B");
    let log = std::fs::read_to_string(&replay).expect("read replay");
    assert!(log.starts_with("STONE_REPLAY 8\n"), "recording must use v8\n{log}");
    let boot_lines: Vec<&str> = log.lines().filter(|line| line.starts_with("boot ")).collect();
    assert_eq!(boot_lines.len(), 3, "the temporary current-settings path plus progress and layout must capture keys");
    let first_tick = log.lines().position(|line| line.starts_with("t ")).expect("tick");
    let first_boot = log.lines().position(|line| line.starts_with("boot ")).expect("bootstrap");
    assert!(first_boot < first_tick, "bootstrap records must precede tick records");

    let replayed = run_mag(1, None, Some(&replay), &env);

    assert_eq!(
        progress_projection(&replayed),
        progress_projection(&recorded),
        "replay must apply the progress action once to captured absence"
    );
    assert_ne!(
        recorded["__stone_diag"]["MAG_REPLAY_BOOTSTRAP_BANK_GOLD"], 0,
        "the production-shaped progress action must reach the consumer"
    );
    assert!(
        recorded["__stone_diag"]["MAG_REPLAY_BOOTSTRAP_SAVE_WRITES"]
            .as_i64()
            .expect("record write witness") > 0,
        "recording must use the production progress write seam"
    );
    assert_eq!(
        replayed["__stone_diag"]["MAG_REPLAY_BOOTSTRAP_SAVE_WRITES"],
        0,
        "playback must not enter the progress write seam"
    );
    assert_eq!(
        std::fs::read(&progress).expect("B must remain present"),
        persisted_b,
        "progress playback must not rewrite the persisted B file"
    );
}

#[test]
fn progress_bootstrap_captures_existing_save_bytes_and_applies_them_once() {
    let dir = tempfile::tempdir().expect("fixture dir");
    let progress = empty_progress_path(&dir);
    let layout = dir.path().join("spellbook-layout.jsonl");
    let settings = dir.path().join("settings.json");
    let replay = dir.path().join("progress-existing.replay");
    std::fs::write(&progress, known_progress_a()).expect("write progress A");
    let persisted_a = std::fs::read(&progress).expect("read progress A");

    let env_owned = fixture_env("progress", &progress, &layout, &settings);
    let env = env_refs(&env_owned);
    let recorded = run_mag(1, Some(&replay), None, &env);
    let persisted_b = std::fs::read(&progress).expect("record action must persist B");
    assert_ne!(persisted_b, persisted_a, "record action must change progress A into B");

    let replayed = run_mag(1, None, Some(&replay), &env);

    assert_eq!(
        progress_projection(&replayed),
        progress_projection(&recorded),
        "replay must apply the captured progress JSON before the action"
    );
    assert_eq!(
        replayed["__stone_diag"]["MAG_REPLAY_BOOTSTRAP_SAVE_WRITES"],
        0,
        "existing-save playback must not enter the progress write seam"
    );
    assert_eq!(
        std::fs::read(&progress).expect("B must remain present"),
        persisted_b,
        "existing-save playback must not rewrite the persisted B file"
    );
}

#[test]
fn progress_bootstrap_can_be_captured_when_play_starts_after_menu_ticks() {
    let dir = tempfile::tempdir().expect("fixture dir");
    let progress = empty_progress_path(&dir);
    let layout = dir.path().join("spellbook-layout.jsonl");
    let settings = dir.path().join("settings.json");
    let replay = dir.path().join("menu-progress.replay");
    std::fs::write(&progress, known_progress_a()).expect("write progress A");

    let mut env_owned = fixture_env("delayed_progress", &progress, &layout, &settings);
    env_owned[0].1 = "0".to_string();
    let mut env = env_refs(&env_owned);
    env.push(("MAG_BOOT_MENU", "1"));
    let recorded = run_mag(4, Some(&replay), None, &env);

    assert_eq!(
        recorded["mag"]["MAG_RUN_STARTS"],
        1,
        "the fixture must enter the same run_start path as Play after menu ticks"
    );
    let text = std::fs::read_to_string(&replay).expect("read replay");
    let first_tick = text.lines().position(|line| line.starts_with("t ")).expect("tick");
    let progress_key = "6d61672f70726f67726573732e6a736f6e";
    let progress_boot = text
        .lines()
        .position(|line| line.starts_with(&format!("boot {progress_key} ")))
        .expect("progress bootstrap");
    assert!(
        progress_boot > first_tick,
        "the production menu path must capture progress at first use after earlier menu ticks"
    );

    let replayed = run_mag(4, None, Some(&replay), &env);
    assert_eq!(
        replayed["mag"]["MAG_RUN_STARTS"],
        1,
        "playback must reach the delayed run using the captured progress input"
    );
}

#[test]
fn new_replay_fails_closed_when_a_declared_bootstrap_is_missing() {
    let dir = tempfile::tempdir().expect("fixture dir");
    let progress = empty_progress_path(&dir);
    let layout = dir.path().join("spellbook-layout.jsonl");
    let settings = dir.path().join("settings.json");
    let replay = dir.path().join("missing-bootstrap.replay");
    let env_owned = fixture_env("progress", &progress, &layout, &settings);
    let env = env_refs(&env_owned);
    let _ = run_mag(1, Some(&replay), None, &env);

    let text = std::fs::read_to_string(&replay).expect("read replay");
    let progress_key = "6d61672f70726f67726573732e6a736f6e";
    let without_bootstrap = text
        .lines()
        .filter(|line| !line.starts_with(&format!("boot {progress_key} ")))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(&replay, without_bootstrap).expect("remove bootstrap records");
    let out = run_mag_raw(1, None, Some(&replay), &env);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "missing v8 bootstrap must abort\n{stderr}");
    assert!(
        stderr.contains("missing required bootstrap input")
            && stderr.contains("mag/progress.json"),
        "the rejection must name the missing contract\n{stderr}"
    );
}

#[test]
fn duplicate_bootstrap_keys_are_rejected() {
    let dir = tempfile::tempdir().expect("fixture dir");
    let progress = empty_progress_path(&dir);
    let layout = dir.path().join("spellbook-layout.jsonl");
    let settings = dir.path().join("settings.json");
    let replay = dir.path().join("duplicate-bootstrap.replay");
    let env_owned = fixture_env("progress", &progress, &layout, &settings);
    let env = env_refs(&env_owned);
    let _ = run_mag(1, Some(&replay), None, &env);

    let text = std::fs::read_to_string(&replay).expect("read replay");
    let duplicate = text.lines().find(|line| line.starts_with("boot "))
        .expect("recording must contain a bootstrap line");
    let mut lines: Vec<&str> = text.lines().collect();
    let tick = lines.iter().position(|line| line.starts_with("t ")).expect("tick");
    lines.insert(tick, duplicate);
    std::fs::write(&replay, lines.join("\n") + "\n").expect("duplicate bootstrap");

    let out = run_mag_raw(1, None, Some(&replay), &env);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "duplicate bootstrap must abort\n{stderr}");
    assert!(stderr.contains("malformed replay records"), "duplicate must use the parser rejection\n{stderr}");
}

#[test]
fn legacy_replay_warns_about_each_missing_bootstrap_contract() {
    let dir = tempfile::tempdir().expect("fixture dir");
    let progress = empty_progress_path(&dir);
    let layout = dir.path().join("spellbook-layout.jsonl");
    let settings = dir.path().join("settings.json");
    let replay = dir.path().join("legacy.replay");
    let env_owned = fixture_env("progress", &progress, &layout, &settings);
    let env = env_refs(&env_owned);
    let _ = run_mag(1, Some(&replay), None, &env);

    let text = std::fs::read_to_string(&replay).expect("read replay");
    let legacy = text
        .lines()
        .filter(|line| !line.starts_with("boot "))
        .enumerate()
        .map(|(i, line)| if i == 0 { "STONE_REPLAY 6" } else { line })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(&replay, legacy).expect("write legacy replay");
    let out = run_mag_raw(1, None, Some(&replay), &env);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "legacy replay remains runnable\n{stderr}");
    assert!(
        stderr.contains("legacy replay v6")
            && stderr.contains("mag/progress.json")
            && stderr.contains("mag/spellbook-layout.jsonl"),
        "legacy playback must name both missing contracts\n{stderr}"
    );
}

#[test]
fn save_repro_retains_bootstrap_entries() {
    let dir = tempfile::tempdir().expect("fixture dir");
    let progress = empty_progress_path(&dir);
    let layout = dir.path().join("spellbook-layout.jsonl");
    let settings = dir.path().join("settings.json");
    let replay = dir.path().join("source.replay");
    let env_owned = fixture_env("progress", &progress, &layout, &settings);
    let mut env = env_refs(&env_owned);
    env.push(("STONE_SAVE_REPRO_AUTO_AT_FRAME", "1"));
    let _ = run_mag(3, Some(&replay), None, &env);

    let repro = std::fs::read_dir(dir.path())
        .expect("replay dir")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.file_name().and_then(|name| name.to_str()).is_some_and(|name| name.starts_with("repro-")))
        .expect("auto save repro must create a sibling");
    let text = std::fs::read_to_string(repro).expect("read saved repro");
    assert!(text.starts_with("STONE_REPLAY 8\n"), "repro must retain v8\n{text}");
    assert_eq!(text.lines().filter(|line| line.starts_with("boot ")).count(), 3,
        "save repro must retain all bootstrap entries");
}

#[test]
fn branch_rewrite_retains_bootstrap_entries_in_canonical_key_order() {
    let dir = tempfile::tempdir().expect("fixture dir");
    let progress = empty_progress_path(&dir);
    let layout = dir.path().join("spellbook-layout.jsonl");
    let settings = dir.path().join("settings.json");
    let replay = dir.path().join("branch.replay");
    let env_owned = fixture_env("progress", &progress, &layout, &settings);
    let mut env = env_refs(&env_owned);
    env.extend([
        ("STONE_LIVE_REWIND", "1"),
        ("STONE_CHECKPOINT_INTERVAL", "5"),
        ("STONE_TIMELINE_SEEK_AT_FRAME", "40"),
        ("STONE_TIMELINE_SEEK_TO_TICK", "25"),
        ("STONE_BRANCH_AT_FRAME", "70"),
    ]);
    let state = run_mag(120, Some(&replay), None, &env);
    assert_eq!(state["__stone_diag"]["STONE_BRANCH_COUNT"], 1,
        "branch fixture must actually rewrite the log");

    let text = std::fs::read_to_string(&replay).expect("read branched replay");
    let boot_lines: Vec<&str> = text.lines().filter(|line| line.starts_with("boot ")).collect();
    assert_eq!(boot_lines.len(), 3, "branch rewrite must retain all bootstrap entries");
    let mut sorted = boot_lines.clone();
    sorted.sort_unstable();
    assert_eq!(boot_lines, sorted, "branch rewrite must emit bootstrap keys canonically");
    let lines: Vec<&str> = text.lines().collect();
    let first_tick = lines.iter().position(|line| line.starts_with("t ")).expect("tick");
    let last_boot = lines.iter().rposition(|line| line.starts_with("boot ")).expect("bootstrap");
    assert!(last_boot < first_tick, "rewritten bootstrap entries must precede ticks");
}

#[test]
fn spell_layout_bootstrap_captures_empty_file_and_keeps_b_read_only() {
    let dir = tempfile::tempdir().expect("fixture dir");
    let progress = empty_progress_path(&dir);
    let layout = dir.path().join("spellbook-layout.jsonl");
    let settings = dir.path().join("settings.json");
    let replay = dir.path().join("layout.replay");
    std::fs::write(&layout, b"").expect("recording arm must begin with an empty file");

    let env_owned = fixture_env("layout", &progress, &layout, &settings);
    let env = env_refs(&env_owned);
    let recorded = run_mag(1, Some(&replay), None, &env);
    let persisted_b = std::fs::read(&layout).expect("record action must persist B");

    let replayed = run_mag(1, None, Some(&replay), &env);

    assert_eq!(
        layout_projection(&replayed),
        layout_projection(&recorded),
        "replay must restore empty layout A before the recorded drag"
    );
    assert_eq!(
        recorded["mag"]["SPELL_STRIPS"]["len"],
        1,
        "the production-shaped drag must create exactly one strip"
    );
    assert!(
        recorded["__stone_diag"]["STONE_SNAPSHOT_HISTORY_WRITES"]
            .as_i64()
            .expect("record layout write witness") > 0,
        "recording must use the production snapshot-history write seam"
    );
    assert_eq!(
        replayed["__stone_diag"]["STONE_SNAPSHOT_HISTORY_WRITES"]
            .as_i64()
            .unwrap_or(0),
        0,
        "playback must not enter the snapshot-history write seam"
    );
    assert_eq!(
        std::fs::read(&layout).expect("B must remain present"),
        persisted_b,
        "spell-layout playback must not rewrite the persisted B file"
    );
}
