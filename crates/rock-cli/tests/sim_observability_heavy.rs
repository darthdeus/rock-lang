use serde_json::Value;
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};
use crate::stone_harness::{fake_free_gpu_mib, HeavyCommandExt, SpawnBudget};
#[cfg(target_os = "linux")]
use crate::kill_harness::{cmdline, wait_gone, RunnerGuard};

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
    let output = crate::command::rock_bin()
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
    let mut command = crate::command::rock_bin();
    command.args(["sim", "testbed", "testbed::co_join2_tuple", "--workers", "-j", "4"])
        .current_dir(workspace_root())
        .env("ROCK_SIM_LOG_DIR", root.path())
        .env("ROCK_SIM_TIMEOUT_SECS", "180");
    // A one-lane GPU budget: one name runs in one lane, so `-j 4` must not
    // be held against it.
    let gpu = tempfile::tempdir().expect("fake GPU dir");
    fake_free_gpu_mib(&mut command, gpu.path(), 6144);
    let output = command
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
    let output = crate::command::rock_bin()
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
    let output = crate::command::rock_bin()
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
    let output = crate::command::rock_bin()
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

/// Read the live events of the single run under `root`, skipping a torn tail
/// line: the recorder flushes per event, so only the last line can be partial,
/// and it parses once complete on a later poll.
#[cfg(target_os = "linux")]
fn poll_events(root: &Path) -> Vec<Value> {
    let mut dirs: Vec<_> = std::fs::read_dir(root)
        .map(|read| {
            read.filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .filter(|path| path.is_dir())
                .collect()
        })
        .unwrap_or_default();
    dirs.sort();
    let Some(run) = dirs.first() else { return Vec::new() };
    let Ok(text) = std::fs::read_to_string(run.join("events.jsonl")) else { return Vec::new() };
    text.lines().filter_map(|line| serde_json::from_str(line).ok()).collect()
}

#[test]
fn concurrent_invocations_keep_independent_histories() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let root = tempfile::tempdir().expect("record root");
    let first = crate::command::rock_bin()
        .args(["sim", "testbed", "testbed::co_join2_tuple"])
        .current_dir(workspace_root())
        .env("ROCK_SIM_LOG_DIR", root.path())
        .env("ROCK_SIM_TIMEOUT_SECS", "180")
        .spawn_with_deadline()
        .expect("spawn first invocation");
    let second = crate::command::rock_bin()
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

/// A runner SIGKILLed mid-sweep keeps every completed sim's verdict and names
/// its active child, and its worker dies with it instead of orphaning.
///
/// Linux-only: binding a child to its launcher's death needs the parent-death
/// signal, which macOS lacks (there TERM/INT/HUP forwarding still applies).
#[cfg(target_os = "linux")]
#[test]
fn killed_mid_run_sweep_keeps_completed_verdicts_and_names_its_active_child() {
    const FAST: &str = "sim_v1_names::unique_short_name";
    const SLOW: &str = "sim_v1_names::kill_window_slow";
    let _spawn_budget = SpawnBudget::for_test(240);
    let dir = tempfile::tempdir().expect("scratch");
    let log_dir = dir.path().join("history");
    let mut command = crate::command::rock_bin();
    command
        .args([
            "sim",
            "crates/rock-cli/test-data/sim-v1-names",
            "unique_short_name",
            "kill_window_slow",
            "--jobs",
            "1",
        ])
        .current_dir(workspace_root())
        .env("ROCK_SIM_LOG_DIR", &log_dir)
        .env("ROCK_SIM_TIMEOUT_SECS", "300")
        .stdin(std::process::Stdio::null())
        .stdout(std::fs::File::create(dir.path().join("runner.out")).expect("stdout log"))
        .stderr(std::fs::File::create(dir.path().join("runner.err")).expect("stderr log"));
    let mut runner = RunnerGuard::spawn(command);

    // Wait for the fast sim to complete and the slow sim to start. Killing
    // only after observing both pins the kill inside the slow sim's run: a
    // kill before the fast end (or after the slow end) fails these guards
    // loudly instead of passing vacuously.
    let deadline = Instant::now() + Duration::from_secs(240);
    let observed = loop {
        let records = poll_events(&log_dir);
        let fast_ended = records
            .iter()
            .any(|event| event["event"] == "entry_end" && event["name"] == FAST);
        let slow_started = records
            .iter()
            .any(|event| event["event"] == "entry_start" && event["name"] == SLOW);
        if fast_ended && slow_started {
            break records;
        }
        assert!(Instant::now() < deadline, "runner never reached the kill window");
        assert!(
            !records.iter().any(|event| event["event"] == "run_end"),
            "runner finished before the kill; the window needs a slower second sim"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    let worker_pid = observed
        .iter()
        .find(|event| event["event"] == "worker_started")
        .and_then(|event| event["pid"].as_u64())
        .expect("worker pid") as u32;
    assert!(
        cmdline(worker_pid).iter().any(|arg| arg.contains("luajit")),
        "worker {worker_pid} must be the luajit child: {:?}",
        cmdline(worker_pid)
    );
    runner.kill_runner();
    // The slow sim needs 21s+ even at full speed, so a worker gone within
    // 10s died with the runner; an orphan would still be running it.
    assert!(
        wait_gone(worker_pid as i32, Duration::from_secs(10)),
        "worker {worker_pid} outlived its SIGKILLed runner"
    );

    let records = poll_events(&log_dir);
    let of = |name: &str| records.iter().filter(|event| event["event"] == name).collect::<Vec<_>>();
    // One shared worker served both entries: two workers would record the
    // fast verdict at the first worker's exit and vacate this test.
    assert_eq!(of("worker_spawn").len(), 1, "both entries share one worker");
    assert!(of("worker_end").is_empty(), "the kill landed mid-worker, not after it");
    assert!(of("run_end").is_empty(), "the runner was killed, it never finished");
    // The completed sim's verdict survived as its live checkpoint, and only
    // there: no post-hoc result was ever recorded for it.
    let checkpoints = of("entry_checkpoint");
    assert_eq!(checkpoints.len(), 1, "exactly the completed sim checkpoints");
    assert_eq!(checkpoints[0]["name"].as_str(), Some(FAST));
    assert_eq!(checkpoints[0]["verdict"].as_str(), Some("PASS"));
    let fast_id = observed
        .iter()
        .find(|event| event["event"] == "entry_start" && event["name"] == FAST)
        .and_then(|event| event["occurrence_id"].as_u64())
        .expect("fast occurrence");
    assert!(
        of("entry_result").iter().all(|event| event["occurrence_id"].as_u64() != Some(fast_id)),
        "no post-hoc verdict may exist for the completed sim"
    );
    // The running child is named with its start time: started, never ended.
    let slow_starts = of("entry_start");
    assert!(
        slow_starts.iter().any(|event| event["name"] == SLOW),
        "the active child must be named"
    );
    assert!(
        of("entry_end").iter().all(|event| event["name"] != SLOW),
        "the active child never finished"
    );
    assert!(checkpoints.iter().all(|event| event["name"] != SLOW), "nothing completed checkpoints");
}

/// A worker the runner kills over budget still records every entry it had
/// completed: the completed verdicts are checkpointed, the over-budget entry
/// keeps its timeout record and names the deadline in the visible error.
#[test]
fn over_budget_worker_still_records_its_completed_entries() {
    let _spawn_budget = SpawnBudget::for_test(180);
    let root = tempfile::tempdir().expect("record root");
    let output = crate::command::rock_bin()
        .args([
            "sim",
            "crates/rock-cli/test-data/sim-v1-names",
            "unique_short_name",
            "kill_window_slow",
            "--jobs",
            "1",
        ])
        .current_dir(workspace_root())
        .env("ROCK_SIM_LOG_DIR", root.path())
        .env("ROCK_SIM_TIMEOUT_SECS", "8")
        .output_with_deadline()
        .expect("run over-budget witness");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!output.status.success(), "the over-budget sim must fail:\n{combined}");
    assert!(
        combined.contains("runner_killed=true"),
        "the runner must kill the over-budget worker:\n{combined}"
    );
    assert!(
        combined.contains("sim `sim_v1_names::kill_window_slow` exceeded its 8s wall-clock entry budget"),
        "the active sim must name its timeout budget, not just its killed process:\n{combined}"
    );
    assert!(
        combined.contains("[sim] 1/2 sims passed -- FAILED: sim_v1_names::kill_window_slow"),
        "the tally must name the over-budget sim:\n{combined}"
    );

    let records = events(root.path());
    let of = |name: &str| records.iter().filter(|event| event["event"] == name).collect::<Vec<_>>();
    let results = of("entry_result");
    assert_eq!(results.len(), 2, "both entries record verdicts");
    let verdict = |id: u64| {
        results
            .iter()
            .find(|event| event["occurrence_id"].as_u64() == Some(id))
            .expect("verdict")
    };
    assert_eq!(verdict(1)["verdict"].as_str(), Some("PASS"));
    assert_eq!(verdict(1)["timed_out"].as_bool(), Some(false));
    assert_eq!(verdict(2)["verdict"].as_str(), Some("FAIL"));
    assert_eq!(verdict(2)["timed_out"].as_bool(), Some(true));
    assert_eq!(of("worker_end").len(), 1);
    assert_eq!(of("worker_end")[0]["status"].as_str(), Some("timeout"));
    assert_eq!(records.last().and_then(|event| event["event"].as_str()), Some("run_end"));
}

/// A sim that declares its own short wall budget is killed at that budget
/// even under a long process-wide budget, and the failure names the sim
/// and the declared budget (not the process-wide number).
#[test]
fn declared_budget_kills_and_names_the_sim_at_its_declared_budget() {
    let _spawn_budget = SpawnBudget::for_test(180);
    let root = tempfile::tempdir().expect("record root");
    let output = crate::command::rock_bin()
        .args([
            "sim",
            "crates/rock-cli/test-data/sim-v1-names",
            "declared_timeout_slow",
            "--jobs",
            "1",
        ])
        .current_dir(workspace_root())
        .env("ROCK_SIM_LOG_DIR", root.path())
        .env("ROCK_SIM_TIMEOUT_SECS", "300")
        .output_with_deadline()
        .expect("run declared-budget witness");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!output.status.success(), "the over-budget sim must fail:\n{combined}");
    assert!(
        combined.contains("runner_killed=true"),
        "the runner must kill the over-budget worker:\n{combined}"
    );
    assert!(
        combined.contains(
            "sim `sim_v1_names::declared_timeout_slow` exceeded its 2s wall-clock entry budget"
        ),
        "the failure must name the sim and its declared 2s budget, not the 300s process-wide one:\n{combined}"
    );
    let records = events(root.path());
    let results = records
        .iter()
        .filter(|event| event["event"] == "entry_result")
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 1, "the single entry records a verdict");
    assert_eq!(results[0]["verdict"].as_str(), Some("FAIL"));
    assert_eq!(results[0]["timed_out"].as_bool(), Some(true));
    let worker_ends = records
        .iter()
        .filter(|event| event["event"] == "worker_end")
        .collect::<Vec<_>>();
    assert_eq!(worker_ends.len(), 1);
    assert_eq!(worker_ends[0]["status"].as_str(), Some("timeout"));
}

/// Two entries with different declared budgets share one worker, and the
/// second entry runs on ITS budget: the first declares 60s and finishes
/// fast, the second declares 2s and must be killed at 2s. If the budget
/// were not republished at the second `entry_start`, the slow entry would
/// inherit 60s, run its kill window to completion and pass.
#[test]
fn per_entry_budget_is_republished_at_each_entry_start() {
    let _spawn_budget = SpawnBudget::for_test(180);
    let root = tempfile::tempdir().expect("record root");
    let output = crate::command::rock_bin()
        .args([
            "sim",
            "crates/rock-cli/test-data/sim-v1-names",
            "declared_long_fast",
            "declared_timeout_slow",
            "--jobs",
            "1",
        ])
        .current_dir(workspace_root())
        .env("ROCK_SIM_LOG_DIR", root.path())
        .env("ROCK_SIM_TIMEOUT_SECS", "300")
        .output_with_deadline()
        .expect("run per-entry budget witness");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!output.status.success(), "the second entry must fail at its budget:\n{combined}");
    assert!(
        combined.contains(
            "sim `sim_v1_names::declared_timeout_slow` exceeded its 2s wall-clock entry budget"
        ),
        "the second entry must be killed at its own 2s budget, not the first entry's 60s:\n{combined}"
    );
    let records = events(root.path());
    let of = |name: &str| records.iter().filter(|event| event["event"] == name).collect::<Vec<_>>();
    assert_eq!(of("worker_spawn").len(), 1, "both entries share one worker");
    let results = of("entry_result");
    assert_eq!(results.len(), 2, "both entries record verdicts");
    let verdict = |id: u64| {
        results
            .iter()
            .find(|event| event["occurrence_id"].as_u64() == Some(id))
            .expect("verdict")
    };
    assert_eq!(verdict(1)["verdict"].as_str(), Some("PASS"));
    assert_eq!(verdict(1)["timed_out"].as_bool(), Some(false));
    assert_eq!(verdict(2)["verdict"].as_str(), Some("FAIL"));
    assert_eq!(verdict(2)["timed_out"].as_bool(), Some(true));
    assert_eq!(of("worker_end")[0]["status"].as_str(), Some("timeout"));
}
