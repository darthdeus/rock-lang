//! Headless luajit children (`rock sim` workers, `rock frame`, ...) run in
//! scopes under `rock-headless.slice`, so the slice's limits bound all
//! concurrent headless runs together.
//!
//! Gated (compiles + runs a game): excluded from the default nextest profile.

// Every item here serves the Linux-only tests below.
#[cfg(target_os = "linux")]
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::process::Command;
#[cfg(target_os = "linux")]
use crate::stone_harness::{HeavyCommandExt, SpawnBudget};

#[cfg(target_os = "linux")]
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

#[cfg(target_os = "linux")]
#[test]
fn a_sim_worker_runs_in_the_headless_slice() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let out = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args(["sim", "crates/rock-cli/test-data/headless-scope", "sim_worker_runs_in_the_sim_slice"])
        .current_dir(workspace_root())
        .env_remove("ROCK_HEADLESS_SCOPE")
        .output_with_deadline()
        .expect("run rock sim");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "the worker must run in rock-headless.slice:\n{combined}");
}

#[cfg(target_os = "linux")]
fn frame_cgroup(scope_env: Option<&str>) -> String {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rock"));
    cmd.args(["frame", "-n", "2", "crates/rock-cli/test-data/headless-scope", "--dump-state"])
        .current_dir(workspace_root())
        .env_remove("ROCK_HEADLESS_SCOPE");
    if let Some(value) = scope_env {
        cmd.env("ROCK_HEADLESS_SCOPE", value);
    }
    let out = cmd.output_with_deadline().expect("run rock frame");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "rock frame failed:\n{stdout}{}", String::from_utf8_lossy(&out.stderr));
    let state: serde_json::Value = serde_json::from_str(stdout.trim()).expect("--dump-state prints JSON");
    state["headless-scope"]["WORKER_CGROUP"].as_str().expect("WORKER_CGROUP global").to_string()
}

#[cfg(target_os = "linux")]
#[test]
fn a_frame_run_uses_the_headless_slice_unless_opted_out() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let inherited = std::fs::read_to_string("/proc/self/cgroup").expect("caller cgroup");
    let scoped = frame_cgroup(None);
    assert!(scoped.contains("/rock-headless.slice/"), "rock frame child cgroup: {scoped}");
    assert_ne!(scoped.trim(), inherited.trim(), "the default must create a child scope");
    let direct = frame_cgroup(Some("0"));
    assert_eq!(direct.trim(), inherited.trim(), "ROCK_HEADLESS_SCOPE=0 must inherit the caller cgroup");
}
