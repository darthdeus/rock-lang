use std::process::Command;
use crate::stone_harness::fake_free_gpu_mib;

fn fixture() -> (tempfile::TempDir, Command) {
    let dir = tempfile::tempdir().unwrap();
    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap();
    let project = dir.path().join("project");
    std::fs::create_dir(&project).unwrap();
    std::fs::write(project.join("stick.toml"), format!(
        "[package]\nname = \"jobs-fixture\"\nversion = \"0.1.0\"\ntype = \"bin\"\n[dependencies]\nstone = {{ path = {:?} }}\n[build.vars]\nGIT_SHA = \"jobs\"\nBUILD_UNIX = \"0\"\nBUILD_DATE = \"test\"\nCHANNEL = {{ builtin = \"channel\" }}\nPLATFORM = {{ builtin = \"platform\" }}\n", workspace.join("stone")
    )).unwrap();
    std::fs::write(project.join("main.rock"), "fn main() {}\n@sim\nco fn jobs_probe() -> Coroutine<Wait> { stop_sim() }\n").unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_rock"));
    command.arg("sim").arg(&project).current_dir(workspace).env("ROCK_DIR", workspace)
        .env("ROCK_SIM_COMPILE_CACHE_DIR", dir.path().join("cache"));
    // Free GPU memory at the reserve: a free-GPU budget of one lane.
    fake_free_gpu_mib(&mut command, dir.path(), 6144);
    (dir, command)
}

/// Far above any host's RAM budget, which never exceeds its core count.
const OVER_RAM: &str = "1000";

#[test]
fn explicit_jobs_above_ram_budget_require_force() {
    let (dir, mut command) = fixture();
    let output = command.args(["--plan", "--jobs", OVER_RAM]).output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "over-budget request unexpectedly passed: {stderr}");
    assert!(stderr.contains("--jobs 1000 exceeds the RAM budget of"), "missing budget: {stderr}");
    assert!(stderr.contains("--jobs-force"), "missing override: {stderr}");
    assert!(!dir.path().join("cache").exists(), "rejection happened after compilation");
}

#[test]
fn explicit_jobs_above_only_the_free_gpu_budget_run_at_its_lane_count() {
    // Free VRAM is a load reading: another GPU user costs lanes, not the run.
    let (_dir, mut command) = fixture();
    let output = command.args(["--plan", "--jobs", "2"]).output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "a free-VRAM shortfall must not fail the run: {stderr}");
    assert!(stderr.contains("--jobs 2 exceeds the free-GPU budget of 1 lanes; running 1"), "missing warning: {stderr}");
    let plan: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(plan["jobs"], 1);
}

#[test]
fn forced_jobs_above_the_budget_keep_warning_and_requested_lanes() {
    let (_dir, mut command) = fixture();
    let output = command.args(["--plan", "--jobs", "2", "--jobs-force"]).output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "forced request failed: {stderr}");
    assert!(stderr.contains("--jobs 2 exceeds the default budget of 1 lanes"), "missing warning: {stderr}");
    let plan: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(plan["jobs"], 2);
}

#[test]
fn explicit_jobs_within_the_budget_are_accepted() {
    let (_dir, mut command) = fixture();
    let output = command.args(["--plan", "--jobs", "1"]).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let plan: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(plan["jobs"], 1);
}

#[test]
fn named_sims_narrow_jobs_to_their_entries_before_the_budget() {
    // A request for 1000 lanes over 64 entries is a request for 64, and only
    // those are held against the RAM budget, before compilation.
    let (dir, mut command) = fixture();
    let output = command.args(vec!["jobs_probe"; 64]).args(["--jobs", OVER_RAM]).output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "over-budget request unexpectedly passed: {stderr}");
    assert!(stderr.contains("--jobs 64 exceeds the RAM budget of"), "{stderr}");
    assert!(!dir.path().join("cache").exists(), "rejection happened after compilation");
}
