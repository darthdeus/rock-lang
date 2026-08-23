//! RED admission fixture: a v8 log is tied to the exact Artifact fingerprint.

use std::path::Path;
use std::process::Command;

fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("workspace root")
}

fn record() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().expect("replay dir");
    let log = dir.path().join("flight.replay");
    let scratch = tempfile::tempdir().expect("scratch");
    let out = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame", "bitgun", "-n", "15", "--record", &log.to_string_lossy(),
            "--output", &scratch.path().to_string_lossy(),
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(workspace_root())
        .output()
        .expect("record");
    assert!(out.status.success(), "record failed: {}", String::from_utf8_lossy(&out.stderr));
    (dir, log.to_string_lossy().into_owned())
}

fn replace_build(log: &str, build: &str) {
    let text = std::fs::read_to_string(log).expect("read replay");
    let mut lines = text.lines().map(ToOwned::to_owned).collect::<Vec<_>>();
    let index = lines.iter().position(|line| line.starts_with("build ")).expect("build header");
    lines[index] = format!("build {build}");
    std::fs::write(log, format!("{}\n", lines.join("\n"))).expect("write replay");
}

fn replay(log: &str) -> std::process::Output {
    let scratch = tempfile::tempdir().expect("scratch");
    Command::new(env!("CARGO_BIN_EXE_rock"))
        .args(["frame", "bitgun", "-n", "15", "--replay", log, "--output", &scratch.path().to_string_lossy()])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(workspace_root())
        .output()
        .expect("replay")
}

#[test]
fn v8_admits_artifact_a_and_rejects_each_changed_artifact_before_tick() {
    let (_dir, log) = record();
    let original = std::fs::read_to_string(&log).expect("read replay");
    let artifact_a = original
        .lines()
        .find_map(|line| line.strip_prefix("build "))
        .expect("recorded Artifact A fingerprint")
        .to_string();

    let admitted = replay(&log);
    assert!(admitted.status.success(), "Artifact A must admit its own log: {}", String::from_utf8_lossy(&admitted.stderr));

    for changed in ["1111111111111111", "2222222222222222"] {
        if changed == artifact_a {
            continue;
        }
        replace_build(&log, changed);
        let rejected = replay(&log);
        let stderr = String::from_utf8_lossy(&rejected.stderr);
        assert!(!rejected.status.success(), "changed Artifact {changed} must reject");
        assert!(stderr.contains("before first game tick"), "admission must precede the first tick: {stderr}");
    }
}
