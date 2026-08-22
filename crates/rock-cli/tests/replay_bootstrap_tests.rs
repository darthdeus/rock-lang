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
        "bank_gold": state["mag"]["bank_gold"],
        "events": state["mag"]["events"],
        "player": state["mag"]["player"],
    })
}

fn layout_projection(state: &serde_json::Value) -> serde_json::Value {
    state["mag"]["SPELL_STRIPS"].clone()
}

fn empty_progress_path(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("progress.json")
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
    assert!(log.starts_with("STONE_REPLAY 7\n"), "recording must use v7\n{log}");
    let boot_lines: Vec<&str> = log.lines().filter(|line| line.starts_with("boot ")).collect();
    assert_eq!(boot_lines.len(), 2, "both MAG bootstrap consumers must capture a key");
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
        recorded["mag"]["bank_gold"], 0,
        "the production-shaped progress action must reach the consumer"
    );
    assert_eq!(
        std::fs::read(&progress).expect("B must remain present"),
        persisted_b,
        "progress playback must not rewrite the persisted B file"
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
    let without_bootstrap = text
        .lines()
        .filter(|line| !line.starts_with("boot "))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(&replay, without_bootstrap).expect("remove bootstrap records");
    let out = run_mag_raw(1, None, Some(&replay), &env);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "missing v7 bootstrap must abort\n{stderr}");
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
    assert!(text.starts_with("STONE_REPLAY 7\n"), "repro must retain v7\n{text}");
    assert_eq!(text.lines().filter(|line| line.starts_with("boot ")).count(), 2,
        "save repro must retain both bootstrap entries");
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
    assert_eq!(boot_lines.len(), 2, "branch rewrite must retain both bootstrap entries");
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
    assert_eq!(
        std::fs::read(&layout).expect("B must remain present"),
        persisted_b,
        "spell-layout playback must not rewrite the persisted B file"
    );
}
