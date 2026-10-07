use std::process::Command;

fn fixture() -> (tempfile::TempDir, Command) {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap();
    let project = dir.path().join("project");
    let bin = dir.path().join("bin");
    std::fs::create_dir(&project).unwrap();
    std::fs::create_dir(&bin).unwrap();
    std::fs::write(project.join("stick.toml"), format!(
        "[package]\nname = \"jobs-fixture\"\nversion = \"0.1.0\"\ntype = \"bin\"\n[dependencies]\nstone = {{ path = {:?} }}\n[build.vars]\nGIT_SHA = \"jobs\"\nBUILD_UNIX = \"0\"\nBUILD_DATE = \"test\"\nCHANNEL = {{ builtin = \"channel\" }}\nPLATFORM = {{ builtin = \"platform\" }}\n", workspace.join("stone")
    )).unwrap();
    std::fs::write(project.join("main.rock"), "fn main() {}\n@sim\nco fn jobs_probe() -> Coroutine<Wait> { stop_sim() }\n").unwrap();
    let nvidia = bin.join("nvidia-smi");
    std::fs::write(&nvidia, "#!/usr/bin/env bash\nprintf '6144\\n'\n").unwrap();
    std::fs::set_permissions(&nvidia, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut paths = vec![bin];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let mut command = Command::new(env!("CARGO_BIN_EXE_rock"));
    command.arg("sim").arg(&project).current_dir(workspace).env("ROCK_DIR", workspace)
        .env("ROCK_SIM_COMPILE_CACHE_DIR", dir.path().join("cache"))
        .env("PATH", std::env::join_paths(paths).unwrap());
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
fn single_named_sim_rejects_over_budget_before_compilation() {
    let (dir, mut command) = fixture();
    let output = command.args(["jobs_probe", "--jobs", "2"]).output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "over-budget request unexpectedly passed: {stderr}");
    assert!(stderr.contains("memory budget of 1") && stderr.contains("--jobs-force"), "{stderr}");
    assert!(!dir.path().join("cache").exists(), "rejection happened after compilation");
}
