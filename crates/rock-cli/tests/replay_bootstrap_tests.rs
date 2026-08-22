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
