use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use crate::stone_harness::{HeavyCommandExt, SpawnBudget};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn events(root: &Path) -> Vec<Value> {
    let path = std::fs::read_dir(root)
        .expect("history root")
        .map(|entry| entry.expect("history entry").path())
        .find(|path| path.is_dir())
        .expect("history directory")
        .join("events.jsonl");
    std::fs::read_to_string(path)
        .expect("history events")
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid JSONL event"))
        .collect()
}

#[test]
fn ordinary_shared_worker_run_writes_validated_lifecycle() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let root = tempfile::tempdir().expect("record root");
    let output = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "sim",
            "testbed",
            "testbed::co_join2_tuple",
            "testbed::co_join3_tuple",
            "--workers",
            "-j",
            "1",
        ])
        .current_dir(workspace_root())
        .env("ROCK_SIM_LOG_DIR", root.path())
        .env("ROCK_SIM_TIMEOUT_SECS", "180")
        .output_with_deadline()
        .expect("run shared worker witness");
    assert!(
        output.status.success(),
        "shared worker witness failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let records = events(root.path());
    assert_eq!(records.first().and_then(|event| event["event"].as_str()), Some("run_start"));
    assert_eq!(records.last().and_then(|event| event["event"].as_str()), Some("run_end"));
    assert_eq!(records.last().and_then(|event| event["status"].as_str()), Some("PASS"));
    assert_eq!(records.last().and_then(|event| event["complete"].as_bool()), Some(true));

    let worker_spawns = records.iter().filter(|event| event["event"] == "worker_spawn").collect::<Vec<_>>();
    assert_eq!(worker_spawns.len(), 1, "the two entries share one booted worker");
    assert_eq!(worker_spawns[0]["kind"], "shared");
    assert_eq!(worker_spawns[0]["entries"].as_array().map(Vec::len), Some(2));
    assert_eq!(records.iter().filter(|event| event["event"] == "worker_first_entry").count(), 1);
    assert_eq!(records.iter().filter(|event| event["event"] == "entry_start").count(), 2);
    assert_eq!(records.iter().filter(|event| event["event"] == "entry_end").count(), 2);
    assert_eq!(records.iter().filter(|event| event["event"] == "entry_result").count(), 2);
    assert_eq!(records.iter().filter(|event| event["event"] == "artifact_check_start").count(), 2);
    assert_eq!(records.iter().filter(|event| event["event"] == "artifact_check_end").count(), 2);
}

#[test]
fn single_name_preserves_requested_and_effective_mode_metadata() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let root = tempfile::tempdir().expect("record root");
    let output = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args(["sim", "testbed", "testbed::co_join2_tuple", "--workers", "-j", "4"])
        .current_dir(workspace_root())
        .env("ROCK_SIM_LOG_DIR", root.path())
        .env("ROCK_SIM_TIMEOUT_SECS", "180")
        .output_with_deadline()
        .expect("run single-name witness");
    assert!(
        output.status.success(),
        "single-name witness failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let records = events(root.path());
    let config = records
        .iter()
        .find(|event| event["event"] == "run_config")
        .expect("run configuration event");
    assert_eq!(config["requested_jobs"], 4);
    assert_eq!(config["requested_mode"], "workers");
    assert_eq!(config["effective_jobs"], 1);
    assert_eq!(config["effective_mode"], "single");
}

#[test]
fn recovery_record_keeps_actual_worker_failure_and_occurrences() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let root = tempfile::tempdir().expect("record root");
    let output = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "sim",
            "testbed",
            "testbed::b2_crash_head,testbed::b2_crash_tail",
            "testbed::b0_leak_probe",
            "--workers",
            "-j",
            "1",
        ])
        .current_dir(workspace_root())
        .env("ROCK_SIM_LOG_DIR", root.path())
        .env("ROCK_SIM_TIMEOUT_SECS", "180")
        .output_with_deadline()
        .expect("run recovery witness");
    assert!(!output.status.success(), "crash fixture must fail its invocation");

    let records = events(root.path());
    let recovery_index = records
        .iter()
        .position(|event| event["event"] == "worker_recovery")
        .expect("worker recovery event");
    let recovery = &records[recovery_index];
    assert!(
        recovery["cause"]
            .as_str()
            .is_some_and(|cause| cause.contains("exit_code")),
        "recovery cause must preserve the worker failure: {recovery}"
    );
    assert_eq!(recovery["prior_worker_attempt"].as_u64(), Some(1));
    assert_eq!(recovery["responsible_occurrence_id"].as_u64(), Some(1));
    assert_eq!(recovery["successors"].as_array().map(Vec::len), Some(1));
    assert_eq!(recovery["successors"][0]["occurrence_id"], 2);
    assert!(
        records
            .iter()
            .position(|event| event["event"] == "worker_end")
            .expect("worker end event")
            < recovery_index,
        "recovery follows the reaped worker"
    );
    assert_eq!(
        records
            .iter()
            .filter(|event| event["event"] == "entry_result")
            .count(),
        2
    );
    assert_eq!(records.last().and_then(|event| event["status"].as_str()), Some("FAIL"));
    assert_eq!(records.last().and_then(|event| event["complete"].as_bool()), Some(true));
}

/// The recovery record for a boot-owned native resource an entry destroyed.
/// The fixture destroys a canvas it created itself at boot: the engine's scene
/// target reallocation is a resize under stable handles now, so the identity
/// contract cannot ride on it.
#[test]
fn native_refusal_recovery_keeps_cause_and_responsible_entry() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let root = tempfile::tempdir().expect("record root");
    let output = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "sim",
            "testbed",
            "testbed::b3_canvas_head",
            "testbed::b3_canvas_seed",
            "testbed::b3_canvas_probe",
            "--workers",
            "-j",
            "1",
        ])
        .current_dir(workspace_root())
        .env("ROCK_SIM_LOG_DIR", root.path())
        .env("ROCK_SIM_TIMEOUT_SECS", "180")
        .output_with_deadline()
        .expect("run native-refusal witness");
    assert!(
        output.status.success(),
        "native refusal is a scheduling event and all entries pass:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let records = events(root.path());
    let recovery = records
        .iter()
        .find(|event| event["event"] == "worker_recovery")
        .expect("native refusal recovery event");
    assert!(
        recovery["cause"]
            .as_str()
            .is_some_and(|cause| cause.contains("destroyed native resources")),
        "native refusal must survive in the recovery record: {recovery}"
    );
    assert_eq!(recovery["responsible_occurrence_id"].as_u64(), Some(2));
    assert_eq!(recovery["successors"][0]["occurrence_id"], 3);
}

#[test]
fn quiet_handled_error_still_writes_terminal_record() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let root = tempfile::tempdir().expect("record root");
    let output = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args(["sim", "testbed", "no_such_sim", "--quiet"])
        .current_dir(workspace_root())
        .env("ROCK_SIM_LOG_DIR", root.path())
        .output_with_deadline()
        .expect("run handled-error witness");
    assert!(!output.status.success(), "unknown sim must fail");
    let records = events(root.path());
    assert_eq!(records.first().and_then(|event| event["quiet"].as_bool()), Some(true));
    assert_eq!(records.last().and_then(|event| event["event"].as_str()), Some("run_end"));
    assert_eq!(records.last().and_then(|event| event["status"].as_str()), Some("FAIL"));
    assert_eq!(records.last().and_then(|event| event["complete"].as_bool()), Some(true));
}

#[test]
fn concurrent_invocations_keep_independent_histories() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let root = tempfile::tempdir().expect("record root");
    let first = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args(["sim", "testbed", "testbed::co_join2_tuple"])
        .current_dir(workspace_root())
        .env("ROCK_SIM_LOG_DIR", root.path())
        .env("ROCK_SIM_TIMEOUT_SECS", "180")
        .spawn_with_deadline()
        .expect("spawn first invocation");
    let second = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args(["sim", "testbed", "testbed::co_join3_tuple"])
        .current_dir(workspace_root())
        .env("ROCK_SIM_LOG_DIR", root.path())
        .env("ROCK_SIM_TIMEOUT_SECS", "180")
        .spawn_with_deadline()
        .expect("spawn second invocation");
    let first = first.wait_with_output().expect("wait first invocation");
    let second = second.wait_with_output().expect("wait second invocation");
    assert!(
        first.status.success() && second.status.success(),
        "concurrent invocations failed:\n{}\n{}\n{}\n{}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr),
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );

    let run_dirs = std::fs::read_dir(root.path())
        .expect("history root")
        .map(|entry| entry.expect("history entry").path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    assert_eq!(run_dirs.len(), 2, "concurrent invocations need separate histories");
    for run_dir in run_dirs {
        let records = std::fs::read_to_string(run_dir.join("events.jsonl"))
            .expect("history events")
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("valid JSONL event"))
            .collect::<Vec<_>>();
        assert_eq!(records.first().and_then(|event| event["event"].as_str()), Some("run_start"));
        assert_eq!(records.last().and_then(|event| event["event"].as_str()), Some("run_end"));
    }
}
