//! Phase 4: a v9 recording carries the normalized binding overlay before its
//! first tick, so replay startup can select it independently of disk state.

use std::path::Path;
use std::process::Command;

fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
}

#[test]
fn mag_record_stamps_a_nonempty_binding_overlay_before_ticks() {
    let dir = tempfile::tempdir().expect("record dir");
    let log = dir.path().join("bindings.replay");
    let scratch = tempfile::tempdir().expect("scratch dir");
    let out = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame", "mag", "-n", "3", "--record", &log.to_string_lossy(),
            "--output", &scratch.path().to_string_lossy(),
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(workspace_root())
        .output()
        .expect("spawn record");
    assert!(out.status.success(), "record failed: {}", String::from_utf8_lossy(&out.stderr));

    let text = std::fs::read_to_string(log).expect("read replay");
    let lines: Vec<&str> = text.lines().collect();
    let overlay_index = lines.iter().position(|line| line.starts_with("bindings "))
        .expect("v9 header must contain a binding overlay");
    let first_tick = lines.iter().position(|line| line.starts_with("t "))
        .expect("record must contain a tick");
    assert!(overlay_index < first_tick, "overlay must precede the first tick");
    let encoded = lines[overlay_index].strip_prefix("bindings ").unwrap();
    assert!(!encoded.is_empty() && encoded.len() % 2 == 0, "overlay must be non-empty hex");
    assert!(encoded.bytes().all(|b| b.is_ascii_hexdigit()), "overlay must be hex");
    let decoded = (0..encoded.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&encoded[i..i + 2], 16).unwrap())
        .collect::<Vec<_>>();
    let json: serde_json::Value = serde_json::from_slice(&decoded).expect("overlay JSON");
    assert_eq!(json["version"], 1, "overlay schema version");
    assert!(json["actions"].as_object().is_some_and(|actions| !actions.is_empty()),
        "overlay must contain a non-empty action population");
}

#[test]
fn replay_uses_recorded_bindings_after_disk_changes() {
    let home = tempfile::tempdir().expect("record home");
    let data = home.path().join("data");
    let dir = tempfile::tempdir().expect("record dir");
    let log = dir.path().join("bindings-input.replay");
    let bindings_path = data.join("darth.games").join("mag").join("bindings.json");
    std::fs::create_dir_all(bindings_path.parent().unwrap()).expect("pref directory");
    let recorded = serde_json::json!({
        "version": 1,
        "actions": {
            "toggle_spells": [{
                "kind": "key", "scan": "x", "ctrl": false,
                "shift": false, "alt": false, "gui": false
            }]
        }
    });
    std::fs::write(&bindings_path, serde_json::to_vec(&recorded).unwrap())
        .expect("recorded bindings");
    let scratch = tempfile::tempdir().expect("record scratch");
    let record = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame", "mag", "-n", "8", "--record", &log.to_string_lossy(),
            "--output", &scratch.path().to_string_lossy(),
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .env("MAG_BOOT_PLAYING", "1")
        .env("MAG_BINDINGS_PATH_OVERRIDE", &bindings_path)
        .env("HOME", home.path())
        .env("XDG_DATA_HOME", &data)
        .current_dir(workspace_root())
        .output()
        .expect("spawn record");
    assert!(record.status.success(), "record failed: {}", String::from_utf8_lossy(&record.stderr));

    let text = std::fs::read_to_string(&log).expect("read replay");
    let mut ticks = 0;
    let rewritten = text.lines().map(|line| {
        if !line.starts_with("t ") { return line.to_string(); }
        let mut fields: Vec<&str> = line.split_whitespace().collect();
        assert_eq!(fields.len(), 10, "v9 tick must have ten fields: {line}");
        if ticks == 3 { fields[6] = "x"; }
        ticks += 1;
        fields.join(" ")
    }).collect::<Vec<_>>().join("\n") + "\n";
    assert!(ticks > 3, "rewritten log must have a non-zero tick population");
    std::fs::write(&log, rewritten).expect("rewrite input tick");

    let changed = serde_json::json!({
        "version": 1,
        "actions": {
            "toggle_spells": [{
                "kind": "key", "scan": "z", "ctrl": false,
                "shift": false, "alt": false, "gui": false
            }]
        }
    });
    std::fs::write(&bindings_path, serde_json::to_vec(&changed).unwrap()).expect("changed bindings");

    let changed_scratch = tempfile::tempdir().expect("changed replay scratch");
    let changed_run = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame", "mag", "-n", "8", "--replay", &log.to_string_lossy(), "--dump-state",
            "--output", &changed_scratch.path().to_string_lossy(),
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .env("MAG_BOOT_PLAYING", "1")
        .env("MAG_BINDINGS_PATH_OVERRIDE", &bindings_path)
        .env("HOME", home.path())
        .env("XDG_DATA_HOME", &data)
        .current_dir(workspace_root())
        .output()
        .expect("spawn changed replay");
    assert!(changed_run.status.success(), "changed replay failed: {}", String::from_utf8_lossy(&changed_run.stderr));
    let changed_state: serde_json::Value = serde_json::from_slice(&changed_run.stdout).expect("changed replay state");

    std::fs::remove_file(&bindings_path).expect("restore bindings disk");
    let clean_scratch = tempfile::tempdir().expect("clean replay scratch");
    let clean_run = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame", "mag", "-n", "8", "--replay", &log.to_string_lossy(), "--dump-state",
            "--output", &clean_scratch.path().to_string_lossy(),
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .env("MAG_BOOT_PLAYING", "1")
        .env("MAG_BINDINGS_PATH_OVERRIDE", &bindings_path)
        .env("HOME", home.path())
        .env("XDG_DATA_HOME", &data)
        .current_dir(workspace_root())
        .output()
        .expect("spawn clean replay");
    assert!(clean_run.status.success(), "clean replay failed: {}", String::from_utf8_lossy(&clean_run.stderr));
    let clean_state: serde_json::Value = serde_json::from_slice(&clean_run.stdout).expect("clean replay state");
    let changed_spellbook_open =
        &changed_state["mag"]["spell_panel_open"];
    let clean_spellbook_open =
        &clean_state["mag"]["spell_panel_open"];
    assert_eq!(changed_spellbook_open, &serde_json::Value::Bool(true),
        "the recorded x binding must open the initially closed spell panel");
    assert_eq!(changed_spellbook_open, clean_spellbook_open,
        "changing disk bindings must not change replay behavior");
}
