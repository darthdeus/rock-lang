use std::process::{Command, Output};

fn rock(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rock"))
        .args(args)
        .output()
        .expect("spawn rock")
}

fn assert_omitted(output: Output, feature: &str, command: &str) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{command} unexpectedly succeeded:\n{stderr}");
    assert!(stderr.contains("omitted"), "missing omitted-backend diagnostic:\n{stderr}");
    assert!(stderr.contains(feature), "missing enabling feature `{feature}`:\n{stderr}");
    assert!(stderr.contains(command), "missing command spelling `{command}`:\n{stderr}");
}

#[cfg(not(feature = "backend-js"))]
#[test]
fn web_without_backend_feature_rejects_before_project_compilation() {
    assert_omitted(
        rock(&["web", "this-project-does-not-exist"]),
        "backend-js",
        "rock web",
    );
}

#[cfg(not(feature = "backend-js"))]
#[test]
fn js_test_and_run_without_backend_feature_reject_before_emission() {
    for args in [
        &["test", "this-file-does-not-exist.rock", "--backend", "js"][..],
        &["run", "this-file-does-not-exist.rock", "--backend", "js"][..],
    ] {
        assert_omitted(rock(args), "backend-js", "--backend js");
    }
}

#[cfg(not(feature = "renderer-wgpu"))]
#[test]
fn wgpu_renderer_without_feature_rejects_before_project_resolution() {
    assert_omitted(
        rock(&["frame", "this-project-does-not-exist", "--renderer", "wgpu"]),
        "renderer-wgpu",
        "--renderer wgpu",
    );
}

#[cfg(not(feature = "renderer-wgpu"))]
#[test]
fn wgpu_manifest_without_feature_rejects_before_game_staging() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("stick.toml"),
        "[package]\nname = \"wgpu_feature_gate\"\ntype = \"bin\"\n\n[renderer]\nbackend = \"wgpu\"\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("main.rock"), "fn main() {}\n").unwrap();

    let path = dir.path().to_str().unwrap();
    let output = rock(&["frame", path, "--frames", "1"]);
    assert_omitted(output, "renderer-wgpu", "wgpu");
}
