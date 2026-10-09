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
    // Free GPU memory at the reserve: a budget of one lane.
    fake_free_gpu_mib(&mut command, dir.path(), 6144);
    (dir, command)
}

#[test]
fn explicit_jobs_above_memory_budget_requires_force() {
    let (dir, mut command) = fixture();
    let output = command.args(["--plan", "--jobs", "2"]).output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "over-budget request unexpectedly passed: {stderr}");
    assert!(stderr.contains("memory budget of 1"), "missing budget: {stderr}");
    assert!(stderr.contains("--jobs-force"), "missing override: {stderr}");
    assert!(!dir.path().join("cache").exists(), "rejection happened after compilation");
}

#[test]
fn forced_jobs_above_memory_budget_keep_warning_and_requested_lanes() {
    let (_dir, mut command) = fixture();
    let output = command.args(["--plan", "--jobs", "2", "--jobs-force"]).output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "forced request failed: {stderr}");
    assert!(stderr.contains("--jobs 2 exceeds the default memory budget of 1 lanes"), "missing warning: {stderr}");
    let plan: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(plan["jobs"], 2);
}

#[test]
fn explicit_jobs_within_memory_budget_are_accepted() {
    let (_dir, mut command) = fixture();
    let output = command.args(["--plan", "--jobs", "1"]).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let plan: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(plan["jobs"], 1);
}

#[test]
fn named_sims_narrow_jobs_to_their_entries_before_the_budget() {
    // Four requested lanes for two entries is a request for two; only the two
    // are held against the one-lane budget, before compilation.
    let (dir, mut command) = fixture();
    let output = command.args(["jobs_probe", "jobs_probe", "--jobs", "4"]).output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "over-budget request unexpectedly passed: {stderr}");
    assert!(stderr.contains("--jobs 2 exceeds the default memory budget of 1 lanes"), "{stderr}");
    assert!(!dir.path().join("cache").exists(), "rejection happened after compilation");
}
