//! Cross-process replay bootstrap coverage for MAG's file-backed progress,
//! spell-layout history, and binding history.
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
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "MAG frame failed: {:?}\nstderr:\n{stderr}",
        output.status,
    );
    assert!(
        stderr.contains("bounded renderer=") && stderr.contains("frame_errors=0"),
        "bounded MAG run must report completion with zero frame errors:\n{stderr}"
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

fn assert_layout_fixture_added_one(state: &serde_json::Value) {
    let diag = &state["__stone_diag"];
    let before = diag["MAG_REPLAY_LAYOUT_STRIPS_BEFORE"]
        .as_i64()
        .expect("layout fixture must report its starting strip population");
    let after = diag["MAG_REPLAY_LAYOUT_STRIPS_AFTER"]
        .as_i64()
        .expect("layout fixture must report its final strip population");
    assert!(before > 0, "empty history must seed the production strips: {diag}");
    assert_eq!(after, before + 1, "the production drag must add exactly one strip: {diag}");
    assert_eq!(state["mag"]["SPELL_STRIPS"]["len"], after,
        "the fixture census must describe the authoritative strip population");
}

fn fixture_env(
    kind: &str,
    progress: &Path,
    layout: &Path,
    settings: &Path,
) -> [(&'static str, String); 6] {
    let bindings_history = layout.with_file_name("spellbook-bindings.jsonl");
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
            "MAG_BINDINGS_HISTORY_PATH_OVERRIDE",
            bindings_history.to_string_lossy().into_owned(),
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

fn bindings_history_projection(state: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "nodes": state["__stone_diag"]["MAG_REPLAY_BINDINGS_HISTORY_NODES"],
        "current": state["__stone_diag"]["MAG_REPLAY_BINDINGS_HISTORY_CURRENT"],
        "can_undo": state["__stone_diag"]["MAG_REPLAY_BINDINGS_HISTORY_CAN_UNDO"],
    })
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

fn hex_bytes(value: &str) -> String {
    value
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn world_damage_snapshot_path(progress: &Path) -> PathBuf {
    PathBuf::from(format!(
        "{}.damage/chunks/{}/{}/x1-y0-g1.png",
        progress.display(),
        hex_bytes("official"),
        hex_bytes("crypt"),
    ))
}

fn add_world_damage_snapshot_fixture(progress: &Path) -> PathBuf {
    let mut root: serde_json::Value = serde_json::from_slice(
        &std::fs::read(progress).expect("read generated current progress"),
    )
    .expect("generated progress must be JSON");
    root["world_damage"] = serde_json::json!([{
        "level": {"namespace": "official", "name": "crypt"},
        "chunks": [{"x": 1, "y": 0, "face": 0, "generation": 1, "tail": []}],
    }]);
    std::fs::write(
        progress,
        serde_json::to_vec_pretty(&root).expect("serialize snapshot fixture manifest"),
    )
    .expect("write snapshot fixture manifest");

    let snapshot = world_damage_snapshot_path(progress);
    std::fs::create_dir_all(snapshot.parent().expect("snapshot parent"))
        .expect("create snapshot fixture directory");
    let image = image::RgbaImage::from_pixel(128, 128, image::Rgba([151, 19, 27, 211]));
    image
        .save_with_format(&snapshot, image::ImageFormat::Png)
        .expect("write snapshot fixture PNG");
    snapshot
}

#[test]
fn binding_history_bootstrap_ignores_mutated_and_deleted_live_history() {
    let dir = tempfile::tempdir().expect("fixture dir");
    let progress = empty_progress_path(&dir);
    let layout = dir.path().join("spellbook-layout.jsonl");
    let settings = dir.path().join("settings.json");
    let bindings = dir.path().join("bindings.json");
    let bindings_history = dir.path().join("spellbook-bindings.jsonl");
    let replay = dir.path().join("bindings-history.replay");

    std::fs::write(
        &bindings,
        br#"{"version":1,"actions":{"toggle_spells":[{"alt":false,"ctrl":false,"gui":false,"kind":"key","scan":"b","shift":false},{"button":"leftstick","kind":"pad"}]}}"#,
    )
    .expect("write non-default bindings A");

    let mut seed_owned = fixture_env("bindings_history_seed", &progress, &layout, &settings).to_vec();
    seed_owned.push(("MAG_BINDINGS_PATH_OVERRIDE", bindings.to_string_lossy().into_owned()));
    let seed_env = env_refs(&seed_owned);
    let seeded = run_mag(2, None, None, &seed_env);
    assert_eq!(
        bindings_history_projection(&seeded),
        serde_json::json!({"nodes": 2, "current": 1, "can_undo": 1}),
        "the seed must create a nondegenerate production binding history"
    );
    let parent_binding = seeded["__stone_diag"]["MAG_REPLAY_BINDINGS_HISTORY_PARENT_BINDING"]
        .as_str().expect("parent binding label");
    let current_binding = seeded["__stone_diag"]["MAG_REPLAY_BINDINGS_HISTORY_CURRENT_BINDING"]
        .as_str().expect("current binding label");
    assert!(!parent_binding.is_empty() && !current_binding.is_empty(),
        "both binding-history nodes must decode to real binding projections");
    assert_ne!(parent_binding, current_binding,
        "the undo parent must be semantically distinct from the current binding");
    let history_a = std::fs::read(&bindings_history).expect("read binding history A");
    assert!(!history_a.is_empty(), "the seed must persist binding history A");

    let mut record_owned = fixture_env("bindings_history_probe", &progress, &layout, &settings).to_vec();
    record_owned.push(("MAG_BINDINGS_PATH_OVERRIDE", bindings.to_string_lossy().into_owned()));
    let record_env = env_refs(&record_owned);
    let recorded = run_mag(2, Some(&replay), None, &record_env);
    assert_eq!(
        bindings_history_projection(&recorded),
        serde_json::json!({"nodes": 2, "current": 1, "can_undo": 1}),
        "recording must load the seeded history and expose an undo parent"
    );

    let mutated = b"mutated binding history on disk\n".to_vec();
    std::fs::write(&bindings_history, &mutated).expect("mutate binding history");
    let replayed_mutated = run_mag(2, None, Some(&replay), &record_env);
    assert_eq!(
        bindings_history_projection(&replayed_mutated),
        bindings_history_projection(&recorded),
        "playback must use captured binding history A instead of mutated disk"
    );
    assert_eq!(
        replayed_mutated["__stone_diag"]["STONE_SNAPSHOT_HISTORY_WRITES"]
            .as_i64().unwrap_or(0),
        0,
        "playback must not enter the binding-history write seam"
    );
    assert_eq!(
        std::fs::read(&bindings_history).expect("read mutated binding history"),
        mutated,
        "playback must leave mutated binding history byte-identical"
    );

    std::fs::remove_file(&bindings_history).expect("delete binding history");
    let replayed_deleted = run_mag(2, None, Some(&replay), &record_env);
    assert_eq!(
        bindings_history_projection(&replayed_deleted),
        bindings_history_projection(&recorded),
        "playback must preserve captured binding history when the live file is absent"
    );
    assert_eq!(
        replayed_deleted["__stone_diag"]["STONE_SNAPSHOT_HISTORY_WRITES"]
            .as_i64().unwrap_or(0),
        0,
        "deleted-file playback must remain read-only"
    );
    assert!(!bindings_history.exists(), "playback must not recreate the deleted live history");

    let text = std::fs::read_to_string(&replay).expect("read binding-history replay");
    let key = "6d61672f7370656c6c626f6f6b2d62696e64696e67732e6a736f6e6c";
    assert_eq!(
        text.lines().filter(|line| line.starts_with(&format!("boot {key} "))).count(),
        1,
        "binding history must have one authoritative bootstrap entry\n{text}"
    );
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
        Some(4),
        "recording must probe each logical file once, including the absent progress and histories: {diag}"
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
        Some(4),
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
fn record_mutate_all_bootstrap_files_then_replay_without_persistence_writes() {
    let dir = tempfile::tempdir().expect("fixture dir");
    let progress = empty_progress_path(&dir);
    let layout = dir.path().join("spellbook-layout.jsonl");
    let settings = dir.path().join("settings.json");
    let replay = dir.path().join("all-bootstrap.replay");
    std::fs::write(&progress, known_progress_a()).expect("write progress A");
    std::fs::write(&layout, b"").expect("write empty layout A");
    std::fs::write(&settings, br#"{"graphics":{"world_scale":0.75}}"#)
        .expect("write settings A");

    let env_owned = fixture_env("all", &progress, &layout, &settings);
    let mut env = env_refs(&env_owned);
    env.push(("STONE_QUIET", "0"));
    let recorded = run_mag(3, Some(&replay), None, &env);
    let progress_after_record = std::fs::read(&progress).expect("recorded progress B");
    let layout_after_record = std::fs::read(&layout).expect("recorded layout B");
    let settings_after_record = std::fs::read(&settings).expect("recorded settings B");

    let progress_mutated = b"mutated progress on disk".to_vec();
    let layout_mutated = b"mutated layout on disk\n".to_vec();
    let settings_mutated = br#"{"graphics":{"world_scale":2.0}}"#.to_vec();
    std::fs::write(&progress, &progress_mutated).expect("mutate progress B");
    std::fs::write(&layout, &layout_mutated).expect("mutate layout B");
    std::fs::write(&settings, &settings_mutated).expect("mutate settings B");

    let replayed = run_mag(3, None, Some(&replay), &env);
    assert_ne!(
        recorded["__stone_diag"]["MAG_REPLAY_BOOTSTRAP_BANK_GOLD"],
        serde_json::json!(0),
        "the combined fixture must exercise the production progress consumer"
    );
    assert_layout_fixture_added_one(&recorded);
    assert!(
        recorded["__stone_diag"]["MAG_SETTINGS_SUCCESSFUL_WRITES"]
            .as_i64()
            .is_some_and(|n| n > 0),
        "the combined fixture must exercise the production settings writer"
    );
    assert_eq!(
        progress_projection(&replayed),
        progress_projection(&recorded),
        "playback must apply captured progress once, not the mutated disk file"
    );
    assert_eq!(
        layout_projection(&replayed),
        layout_projection(&recorded),
        "playback must apply the captured spell layout once, not the mutated disk file"
    );
    assert_eq!(
        replayed["mag"]["SETTINGS"]["graphics"]["world_scale"],
        serde_json::json!(0.75),
        "playback must use recorded settings A rather than disk settings B"
    );
    assert_eq!(
        replayed["__stone_diag"]["MAG_REPLAY_BOOTSTRAP_SAVE_WRITES"].as_i64().unwrap_or(0),
        0,
        "playback must not enter the progress persistence writer"
    );
    assert_eq!(
        replayed["__stone_diag"]["STONE_SNAPSHOT_HISTORY_WRITES"].as_i64().unwrap_or(0),
        0,
        "playback must not enter the spell-layout persistence writer"
    );
    assert_eq!(
        replayed["__stone_diag"]["MAG_SETTINGS_SUCCESSFUL_WRITES"].as_i64().unwrap_or(0),
        0,
        "playback must not enter the settings persistence writer"
    );
    assert!(
        replayed["__stone_diag"]["MAG_SETTINGS_PLAYBACK_SAVE_SKIPS"]
            .as_i64()
            .is_some_and(|n| n > 0),
        "playback settings writes must reach the no-op authority"
    );
    assert_eq!(
        std::fs::read(&progress).expect("read mutated progress"),
        progress_mutated,
        "playback must leave mutated progress byte-identical"
    );
    assert_eq!(
        std::fs::read(&layout).expect("read mutated layout"),
        layout_mutated,
        "playback must leave mutated layout byte-identical"
    );
    assert_eq!(
        std::fs::read(&settings).expect("read mutated settings"),
        settings_mutated,
        "playback must leave mutated settings byte-identical"
    );
    assert_ne!(
        progress_after_record, progress_mutated,
        "recording must exercise progress persistence"
    );
    assert_ne!(
        layout_after_record, layout_mutated,
        "recording must exercise layout persistence"
    );
    assert_ne!(
        settings_after_record, settings_mutated,
        "recording must exercise settings persistence"
    );

    let text = std::fs::read_to_string(&replay).expect("read all-bootstrap replay");
    assert_eq!(
        text.lines().filter(|line| line.starts_with("boot ")).count(),
        4,
        "all four logical bootstrap inputs must be present exactly once\n{text}"
    );
    for key in [
        "6d61672f70726f67726573732e6a736f6e2f31",
        "6d61672f73657474696e67732e6a736f6e",
        "6d61672f7370656c6c626f6f6b2d6c61796f75742e6a736f6e6c",
        "6d61672f7370656c6c626f6f6b2d62696e64696e67732e6a736f6e6c",
    ] {
        assert_eq!(
            text.lines()
                .filter(|line| line.starts_with(&format!("boot {key} ")))
                .count(),
            1,
            "bootstrap key {key} must have one wire entry\n{text}"
        );
    }
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
    assert!(log.starts_with(&format!("{}\n", rock_replay_protocol::replay_header())),
        "recording must use v{}\n{log}", rock_replay_protocol::REPLAY_VERSION);
    let boot_lines: Vec<&str> = log.lines().filter(|line| line.starts_with("boot ")).collect();
    assert_eq!(boot_lines.len(), 4,
        "the temporary current-settings path plus progress and both histories must capture keys");
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
fn world_damage_snapshot_bootstrap_survives_deleted_live_blob_without_writes() {
    let dir = tempfile::tempdir().expect("fixture dir");
    let progress = empty_progress_path(&dir);
    let layout = dir.path().join("spellbook-layout.jsonl");
    let settings = dir.path().join("settings.json");
    let replay = dir.path().join("world-damage.replay");

    let setup_owned = fixture_env("progress", &progress, &layout, &settings);
    let setup_env = env_refs(&setup_owned);
    let _ = run_mag(1, None, None, &setup_env);
    let snapshot = add_world_damage_snapshot_fixture(&progress);
    let snapshot_key = format!(
        "mag.damage.snapshot/{}/{}/{}/1/0/1/load/1",
        hex_bytes(&progress.to_string_lossy()),
        hex_bytes("official"),
        hex_bytes("crypt"),
    );
    let wire_key = hex_bytes(&snapshot_key);

    let env_owned = fixture_env("world_damage", &progress, &layout, &settings);
    let env = env_refs(&env_owned);
    let recorded = run_mag(2, Some(&replay), None, &env);
    let recorded_snapshot = std::fs::read(&snapshot).expect("read recorded snapshot");
    let log = std::fs::read_to_string(&replay).expect("read world damage replay");
    // A chunk is two sidecars of one generation now, colour and material, so
    // it contributes two bootstrap inputs. The fixture writes only the colour
    // PNG, and the material one is captured as ABSENT -- which is what lets
    // playback derive it instead of probing disk.
    let material_key = format!("{snapshot_key}/m");
    let material_wire_key = hex_bytes(&material_key);
    assert_eq!(
        log.lines().filter(|line| line.starts_with("boot ")).count(),
        6,
        "the colour and material sidecars add one captured bootstrap input each\n{log}"
    );
    assert_eq!(
        log.lines()
            .filter(|line| line.starts_with(&format!("boot {wire_key} ")))
            .count(),
        1,
        "the snapshot identity has one collision-free bootstrap entry\n{log}"
    );
    assert_eq!(
        log.lines()
            .filter(|line| line.starts_with(&format!("boot {material_wire_key} ")))
            .count(),
        1,
        "the material sidecar has its own collision-free bootstrap entry\n{log}"
    );
    assert_eq!(recorded["__stone_diag"]["MAG_REPLAY_WORLD_DAMAGE_GENERATION"], 1);
    assert_eq!(recorded["__stone_diag"]["MAG_REPLAY_WORLD_DAMAGE_TAIL"], 0);
    assert_eq!(recorded["__stone_diag"]["MAG_REPLAY_WORLD_DAMAGE_SAVE_WRITES"], 1);

    std::fs::remove_file(&snapshot).expect("delete live snapshot before playback");
    let replayed = run_mag(2, None, Some(&replay), &env);
    assert_eq!(
        replayed["__stone_diag"]["MAG_REPLAY_WORLD_DAMAGE_GENERATION"],
        recorded["__stone_diag"]["MAG_REPLAY_WORLD_DAMAGE_GENERATION"]
    );
    assert_eq!(
        replayed["__stone_diag"]["MAG_REPLAY_WORLD_DAMAGE_TAIL"],
        recorded["__stone_diag"]["MAG_REPLAY_WORLD_DAMAGE_TAIL"]
    );
    assert_eq!(replayed["__stone_diag"]["MAG_REPLAY_WORLD_DAMAGE_SAVE_OK"], 1);
    assert_eq!(replayed["__stone_diag"]["MAG_REPLAY_WORLD_DAMAGE_SAVE_WRITES"], 0);
    assert!(
        replayed["__stone_diag"]["MAG_REPLAY_WORLD_DAMAGE_UPLOAD_CALLS"]
            .as_i64()
            .is_some_and(|n| n > 0),
        "playback must upload the captured sidecar bytes\n{replayed}"
    );
    assert!(!snapshot.exists(), "playback must not recreate the deleted blob");
    assert_ne!(recorded_snapshot.len(), 0, "the record must contain PNG bytes");
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
    let progress_key = "6d61672f70726f67726573732e6a736f6e2f31";
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
    let progress_key = "6d61672f70726f67726573732e6a736f6e2f31";
    let without_bootstrap = text
        .lines()
        .filter(|line| !line.starts_with(&format!("boot {progress_key} ")))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(&replay, without_bootstrap).expect("remove bootstrap records");
    let out = run_mag_raw(1, None, Some(&replay), &env);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "missing v9 bootstrap must abort\n{stderr}");
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
    let legacy = crate::stone_harness::downgrade_replay(&text, 6)
        .lines()
        .filter(|line| !line.starts_with("boot "))
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
            && stderr.contains("mag/spellbook-layout.jsonl")
            && stderr.contains("mag/spellbook-bindings.jsonl"),
        "legacy playback must name every missing bootstrap contract\n{stderr}"
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
    assert!(text.starts_with(&format!("{}\n", rock_replay_protocol::replay_header())),
        "repro must retain v{}\n{text}", rock_replay_protocol::REPLAY_VERSION);
    assert_eq!(text.lines().filter(|line| line.starts_with("boot ")).count(), 4,
        "save repro must retain all bootstrap entries");
}

#[test]
fn branch_rewrite_retains_bootstrap_entries_in_canonical_key_order() {
    let dir = tempfile::tempdir().expect("fixture dir");
    let progress = empty_progress_path(&dir);
    let layout = dir.path().join("spellbook-layout.jsonl");
    let settings = dir.path().join("settings.json");
    let replay = dir.path().join("branch.replay");
    let before_branch = dir.path().join("before-branch.replay");
    let before_branch_path = before_branch.to_string_lossy();
    let env_owned = fixture_env("progress", &progress, &layout, &settings);
    let mut env = env_refs(&env_owned);
    env.extend([
        ("STONE_LIVE_REWIND", "1"),
        ("STONE_KEYFRAME_INTERVAL", "10"),
        ("STONE_CHECKPOINT_INTERVAL", "5"),
        ("STONE_TIMELINE_SEEK_AT_FRAME", "40"),
        ("STONE_TIMELINE_SEEK_TO_TICK", "25"),
        ("STONE_SAVE_REPRO_AT_FRAME", "69"),
        ("STONE_SAVE_REPRO_PATH", before_branch_path.as_ref()),
        ("STONE_BRANCH_AT_FRAME", "70"),
    ]);
    // Stop at the rewrite: later gameplay may append new snapshot inputs.
    let state = run_mag(70, Some(&replay), None, &env);
    assert_eq!(state["__stone_diag"]["STONE_BRANCH_COUNT"], 1,
        "branch fixture must actually rewrite the log");

    let text = std::fs::read_to_string(&replay).expect("read branched replay");
    let boot_lines: Vec<&str> = text.lines().filter(|line| line.starts_with("boot ")).collect();
    let before = std::fs::read_to_string(&before_branch).expect("read pre-branch replay");
    let mut expected: Vec<&str> = before.lines().filter(|line| line.starts_with("boot ")).collect();
    // Damage snapshots can add inputs while the fixture runs; preserve their
    // captured bytes as well as the four inputs read at boot.
    for key in [
        "mag/progress.json/1",
        "mag/settings.json",
        "mag/spellbook-layout.jsonl",
        "mag/spellbook-bindings.jsonl",
    ] {
        let prefix = format!("boot {} ", hex_bytes(key));
        assert_eq!(expected.iter().filter(|line| line.starts_with(&prefix)).count(), 1,
            "pre-branch recording must contain bootstrap input {key} exactly once");
    }
    expected.sort_unstable();
    assert_eq!(boot_lines, expected, "branch rewrite must retain every captured bootstrap entry");
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
    assert_layout_fixture_added_one(&recorded);
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

#[test]
fn progress_bootstrap_records_each_reload_and_deletion() {
    let dir = tempfile::tempdir().expect("fixture dir");
    let progress = empty_progress_path(&dir);
    let layout = dir.path().join("spellbook-layout.jsonl");
    let settings = dir.path().join("settings.json");
    let replay = dir.path().join("progress-reload.replay");
    std::fs::write(&progress, known_progress_a()).unwrap();
    let env_owned = fixture_env("progress_reload", &progress, &layout, &settings);
    let env = env_refs(&env_owned);
    let recorded = run_mag(1, Some(&replay), None, &env);
    assert_eq!(recorded["__stone_diag"]["MAG_REPLAY_RELOAD_REMOVED"], 1);
    let sentinel = b"live save must not be read or rewritten by playback";
    std::fs::write(&progress, sentinel).unwrap();
    let replayed = run_mag(1, None, Some(&replay), &env);
    for state in [&recorded, &replayed] {
        assert_eq!(state["__stone_diag"]["MAG_REPLAY_RELOAD_LOADED"], 1);
        assert_eq!(state["__stone_diag"]["MAG_REPLAY_RELOAD_GOLD"], 765);
        assert_eq!(state["__stone_diag"]["MAG_REPLAY_RELOAD_DELETED_LOADED"], 0);
    }
    assert_eq!(std::fs::read(&progress).unwrap(), sentinel);
}
