use std::process::Command;

fn rock_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_rock"))
}

fn node_available() -> bool {
    Command::new("node").arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

fn run_test(src: &str) -> (String, bool) {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("probe.rock");
    std::fs::write(&file, src).expect("write probe");
    let out = Command::new(rock_bin())
        .args(["test", file.to_str().unwrap(), "--backend", "js"])
        .output()
        .expect("spawn rock");
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (text, out.status.success())
}

const FAILING_ASSERT: &str = r#"
@test
fn deliberately_failing() {
    assert(1 == 2, "this test is supposed to fail")
}
"#;

const PASSING: &str = r#"
@test
fn genuinely_passing() {
    assert(1 == 1, "holds")
}
"#;

/// The two backends disagreed on this: js exited 1 while skull exited 0 for the
/// same failing input. Pin the agreement so a future divergence is loud.
#[test]
fn js_backend_agrees_on_exit_status() {
    if !node_available() {
        eprintln!("SKIP js_backend_agrees_on_exit_status: node not available");
        return;
    }
    let (text, ok) = run_test(FAILING_ASSERT);
    assert!(!ok, "js: a failing @test must exit nonzero:\n{text}");
    let (text, ok) = run_test(PASSING);
    assert!(ok, "js: a passing @test must exit zero:\n{text}");
}
