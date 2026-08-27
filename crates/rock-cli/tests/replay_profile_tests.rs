use std::path::Path;
use std::process::Command;

fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
}

fn record_fixture(dir: &Path) -> std::path::PathBuf {
    let replay = dir.join("profile.replay");
    let compile_out = dir.join("record-out");
    let recorded = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame",
            "bitgun",
            "-n",
            "6",
            "--record",
            replay.to_str().unwrap(),
            "--output",
            compile_out.to_str().unwrap(),
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .env("STONE_REPLAY_TIMING_US", "10000,11000,12000,13000,14000,15000")
        .env(
            "STONE_REPLAY_PHASE_TIMING_US",
            "1000:2000:3000,1001:2001:3001,1002:2002:3002,1003:2003:3003,1004:2004:3004,1005:2005:3005",
        )
        .current_dir(workspace_root())
        .output()
        .expect("record replay");
    assert!(
        recorded.status.success(),
        "record failed:\n{}",
        String::from_utf8_lossy(&recorded.stderr)
    );
    replay
}

#[test]
fn replay_profile_emits_exact_repeated_range_as_json_lines() {
    let dir = tempfile::tempdir().expect("replay profile scratch");
    let replay = record_fixture(dir.path());

    let profile_out = dir.path().join("profile-out");
    let output = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "replay-profile",
            "bitgun",
            replay.to_str().unwrap(),
            "--range",
            "2-4",
            "--runs",
            "2",
            "--output",
            profile_out.to_str().unwrap(),
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .env("STONE_REPLAY_SLOW_PROFILE_MS", "0.001")
        .current_dir(workspace_root())
        .output()
        .expect("profile replay");
    assert!(
        output.status.success(),
        "profile failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let profile_jsonl = dir.path().join("profile.jsonl");
    std::fs::write(&profile_jsonl, &output.stdout).unwrap();

    let rows = String::from_utf8(output.stdout)
        .expect("UTF-8 profile output")
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("JSONL row"))
        .collect::<Vec<_>>();
    let frames = rows
        .iter()
        .filter(|row| row["kind"] == "frame")
        .collect::<Vec<_>>();
    let runs = rows
        .iter()
        .filter(|row| row["kind"] == "run")
        .collect::<Vec<_>>();
    assert_eq!(frames.len(), 6, "two complete 3-frame ranges: {rows:#?}");
    assert_eq!(runs.len(), 2, "one terminal row per run: {rows:#?}");
    for run in 1..=2 {
        let ordinals = frames
            .iter()
            .filter(|row| row["run"] == run)
            .map(|row| row["ordinal"].as_u64().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(ordinals, [2, 3, 4]);
        let terminal = runs.iter().find(|row| row["run"] == run).unwrap();
        assert_eq!(terminal["status"], "complete");
        assert_eq!(terminal["warmup_frames"], 2);
        assert_eq!(terminal["requested_frames"], 3);
        assert_eq!(terminal["observed_frames"], 3);
    }
    assert_eq!(frames[0]["recorded"]["total_us"], 12_000);
    assert!(frames[0]["playback"]["total_us"].as_u64().unwrap() > 0);
    assert_eq!(frames[0]["tick"], frames[0]["ordinal"]);
    assert_eq!(
        frames[0]["identity"]["replay_build"],
        frames[0]["identity"]["playback_build"]
    );
    assert!(!frames[0]["identity"]["replay_revision"]
        .as_str()
        .unwrap()
        .is_empty());
    assert_eq!(
        frames[0]["identity"]["replay_revision"],
        frames[0]["identity"]["playback_revision"]
    );
    assert_eq!(
        frames[0]["playback_profile"]["allocation_traffic"]["available"],
        false
    );
    assert!(frames[0]["playback_profile"]["allocation_traffic"]["bytes"].is_null());
    assert!(
        !frames[0]["playback_profile"]["sections"]
            .as_array()
            .unwrap()
            .is_empty(),
        "the measured frame must execute real profiler sections"
    );
    assert!(frames[0]["graphics"]["draw_calls"].as_u64().unwrap() > 0);
    assert_eq!(
        frames[2]["ordinal"], 4,
        "the inclusive final boundary must be captured"
    );

    let report = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "replay-profile-report",
            profile_jsonl.to_str().unwrap(),
            "--format",
            "json",
        ])
        .current_dir(workspace_root())
        .output()
        .expect("summarize replay profile");
    assert!(
        report.status.success(),
        "profile report failed:\n{}",
        String::from_utf8_lossy(&report.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&report.stdout).unwrap();
    assert_eq!(report["runs"]["complete"], 2);
    assert_eq!(report["runs"]["incomplete"], 0);
    assert_eq!(report["observations"], 3);
    assert_eq!(report["top_frames"].as_array().unwrap().len(), 3);
    assert_eq!(report["top_frames"][0]["samples"], 2);
}

#[test]
fn replay_profile_preserves_an_incomplete_run_instead_of_ranking_it_as_zero() {
    let dir = tempfile::tempdir().expect("replay profile failure scratch");
    let replay = record_fixture(dir.path());
    let output = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "replay-profile",
            "bitgun",
            replay.to_str().unwrap(),
            "--range",
            "2-4",
            "--output",
            dir.path().join("profile-out").to_str().unwrap(),
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .env("STONE_FRAME_FORCE_ERROR", "4")
        .current_dir(workspace_root())
        .output()
        .expect("profile failing replay");
    assert!(
        !output.status.success(),
        "an incomplete range must fail the command"
    );
    let rows = String::from_utf8(output.stdout)
        .expect("UTF-8 profile output")
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("JSONL row"))
        .collect::<Vec<_>>();
    let terminal = rows
        .iter()
        .find(|row| row["kind"] == "run")
        .expect("terminal row");
    assert_ne!(terminal["status"], "complete");
    assert_eq!(terminal["requested_frames"], 3);
    assert!(terminal["observed_frames"].as_u64().unwrap() < 3);
    assert!(
        !terminal["stderr_tail"].as_str().unwrap().is_empty(),
        "the failed process must retain diagnostic evidence"
    );
}

#[test]
fn replay_profile_rejects_a_different_artifact_before_spawning_playback() {
    let dir = tempfile::tempdir().expect("replay profile identity scratch");
    let replay = record_fixture(dir.path());
    let text = std::fs::read_to_string(&replay).unwrap();
    let build_line = text
        .lines()
        .find(|line| line.starts_with("build "))
        .unwrap();
    let mismatched = text.replacen(build_line, "build ffffffffffffffff", 1);
    std::fs::write(&replay, mismatched).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "replay-profile",
            "bitgun",
            replay.to_str().unwrap(),
            "--range",
            "2-4",
            "--output",
            dir.path().join("profile-out").to_str().unwrap(),
        ])
        .current_dir(workspace_root())
        .output()
        .expect("profile mismatched replay");
    assert!(!output.status.success());
    assert!(
        output.stdout.is_empty(),
        "mismatch must reject before a measured row exists"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("replay Artifact mismatch"),
        "mismatch must name both identities: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
