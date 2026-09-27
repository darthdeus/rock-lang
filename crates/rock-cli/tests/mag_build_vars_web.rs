use std::path::PathBuf;
use std::process::Command;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap().to_path_buf()
}

fn rock_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_rock"))
}

#[test]
fn mag_web_build_carries_declared_identity() {
    let root = repo_root();
    let dir = tempfile::tempdir().unwrap();
    let sha = String::from_utf8(
        Command::new("git")
            .current_dir(&root)
            .args(["rev-parse", "--short", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    let web_dir = dir.path().join("web");
    let out = Command::new(rock_bin())
        .current_dir(&root)
        .args(["web", "mag", "--out", web_dir.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "web mag build failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let js = std::fs::read_to_string(web_dir.join("game.js")).unwrap();
    assert!(js.contains(&sha), "web JS did not carry GIT_SHA={sha}");
    assert!(js.contains("web"), "web JS did not carry the web channel/platform");
    assert!(!js.contains("__ROCK_BUILD_SHA"), "web JS still carries the old build global");
    assert!(!js.contains("__ROCK_CHANNEL"), "web JS still carries the old channel global");
}
