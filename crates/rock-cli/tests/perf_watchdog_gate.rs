//! Perf startup and frame execution have separate budgets and diagnostics.

use std::path::{Path, PathBuf};
use std::process::Command;
use crate::stone_harness::{HeavyCommandExt, SpawnBudget};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap().to_path_buf()
}

fn run_perf(startup_ms: &str, frame_ms: &str) -> (String, bool, Option<serde_json::Value>) {
    let output = tempfile::tempdir().expect("perf output");
    let result = Command::new(env!("CARGO_BIN_EXE_rock"))
        .current_dir(repo_root())
        .args(["perf", "crates/rock-cli/test-data/perf-watchdog", "--frames", "3",
            "--warmup-seconds", "0", "--format", "json"])
        .arg("--output").arg(output.path())
        .env_remove("STONE_HOT_RELOAD")
        .env("ROCK_FRAME_TIMEOUT_SECS", "8")
        .env("PERF_TEST_STARTUP_MS", startup_ms)
        .env("STONE_FRAME_DELAY_MS", frame_ms)
        .output_with_deadline().expect("spawn perf run");
    (format!("{}{}", String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)), result.status.success(),
        serde_json::from_slice(&result.stdout).ok())
}

#[test]
fn startup_does_not_consume_the_progressing_frame_budget() {
    let _spawn_budget = SpawnBudget::for_test(120);
    // Each phase fits 8s with scheduling margin; their 9.5s sum does not.
    let (text, ok, summary) = run_perf("3500", "2000");
    assert!(ok, "startup plus progressing frames exceed the old total budget:\n{text}");
    assert_eq!(summary.expect("completed perf JSON")["frames"].as_u64(), Some(3),
        "all requested frames must complete:\n{text}");
    assert!(!text.contains("timed out"), "progress must survive the total-budget crossing:\n{text}");
}

#[test]
fn startup_timeout_identifies_the_phase_and_requested_frames() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let (text, ok, _) = run_perf("16000", "0");
    assert!(!ok, "startup must remain bounded:\n{text}");
    assert!(text.contains("phase=startup"), "identify startup timeout:\n{text}");
    assert!(text.contains("completed/requested=0/3"), "retain the requested denominator:\n{text}");
    assert!(text.contains("progress_stopped=false"), "no frame has progressed yet:\n{text}");
}

#[test]
fn frame_budget_timeout_identifies_the_phase_and_first_frame() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let (text, ok, _) = run_perf("0", "16000");
    assert!(!ok, "stalled frames must remain bounded:\n{text}");
    assert!(text.contains("phase=frame budget"), "identify frame budget timeout:\n{text}");
    assert!(text.contains("renderer=gl, completed/requested=1/3"), "retain real progress:\n{text}");
    assert!(text.contains("progress_stopped=false"), "the total frame budget does not infer inactivity:\n{text}");
}
