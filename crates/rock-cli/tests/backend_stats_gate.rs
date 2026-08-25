use std::process::Command;

#[test]
fn backend_stats_returns_the_timed_emit_failure() {
    let dir = tempfile::tempdir().expect("temporary project");
    std::fs::write(
        dir.path().join("main.rock"),
        "fn main() { print(build_var(\"MISSING\")) }\n",
    )
    .expect("main.rock");

    let output = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args(["backend-stats", dir.path().to_str().expect("utf-8 project path")])
        .output()
        .expect("run backend-stats");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!output.status.success(), "a timed setup failure must fail the command");
    assert!(
        stdout.contains("emit           : FAILED") && stdout.contains("no [build.vars]"),
        "the report must expose the real emit failure:\n{stdout}\n{stderr}"
    );
    assert!(
        stderr.contains("backend-stats skull emit failed"),
        "the command must return a failure with its stage:\n{stdout}\n{stderr}"
    );
}
