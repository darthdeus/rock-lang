//! A log carries the exact Artifact fingerprint it was recorded under, and a
//! playback under a DIFFERENT fingerprint is named as such before the first
//! game tick -- and then plays anyway. Admission used to reject here; a replay
//! is re-run to be measured, and a rejected replay measures nothing.

use std::path::Path;
use std::process::Command;
use crate::stone_harness::{HeavyCommandExt, SpawnBudget};

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
        .output_with_deadline()
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
        .output_with_deadline()
        .expect("replay")
}

#[test]
fn v8_names_a_changed_artifact_and_plays_it_anyway() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let (_dir, log) = record();
    let original = std::fs::read_to_string(&log).expect("read replay");
    let artifact_a = original
        .lines()
        .find_map(|line| line.strip_prefix("build "))
        .expect("recorded Artifact A fingerprint")
        .to_string();
    let changed = "1111111111111111";
    assert_ne!(artifact_a, changed, "fixture must observe the compiler fingerprint");

    let admitted = replay(&log);
    assert!(admitted.status.success(), "Artifact A must admit its own log: {}", String::from_utf8_lossy(&admitted.stderr));
    assert!(
        !String::from_utf8_lossy(&admitted.stderr).contains("built against"),
        "an identical Artifact must not warn about drift"
    );

    replace_build(&log, changed);
    let played = replay(&log);
    let stderr = String::from_utf8_lossy(&played.stderr);
    assert!(played.status.success(), "changed Artifact {changed} must still play: {stderr}");
    assert!(
        stderr.contains(&format!("built against {changed}")) && stderr.contains("state may diverge"),
        "the changed Artifact must be named: {stderr}"
    );
    // The identity is still resolved in replay init: the warning precedes
    // the watchdog's first completed-frame marker.
    let first_frame = stderr
        .find("completed=1 ")
        .expect("the watchdog marks the first completed frame");
    assert!(
        stderr.find("built against").is_some_and(|warned| warned < first_frame),
        "the drift must be named before the first game tick: {stderr}"
    );
}
