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

/// End to end: a drifted run names the tick it drifted at. The recording keeps
/// its desync checkpoints, one recorded hash is tampered with (standing in for
/// a simulation that actually drifts), and the build id is moved so the run is
/// labelled inexact. The profiler must still emit its range AND report the
/// divergent tick, because a benchmark number from a drifted run is only
/// readable next to where it drifted.
#[test]
fn replay_profile_reports_the_tick_a_drifted_run_diverged_at() {
    let dir = tempfile::tempdir().expect("replay divergence scratch");
    let replay = dir.path().join("divergent.replay");
    let recorded = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame", "bitgun", "-n", "30",
            "--record", replay.to_str().unwrap(),
            "--checkpoint-interval", "10",
            "--output", dir.path().join("record-out").to_str().unwrap(),
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(workspace_root())
        .output()
        .expect("record replay");
    assert!(
        recorded.status.success(),
        "record failed:\n{}",
        String::from_utf8_lossy(&recorded.stderr)
    );

    let text = std::fs::read_to_string(&replay).unwrap();
    let checkpoint = text
        .lines()
        .find(|line| line.starts_with("c "))
        .expect("K=10 over 30 frames must emit checkpoints")
        .to_string();
    let fields = checkpoint.split(' ').collect::<Vec<_>>();
    let (tick, name) = (fields[1].to_string(), fields[2].to_string());
    let build_line = text.lines().find(|line| line.starts_with("build ")).unwrap();
    let tampered = text
        .replacen(&checkpoint, &format!("c {tick} {name} deadbeefdeadbeef"), 1)
        .replacen(build_line, "build ffffffffffffffff", 1);
    std::fs::write(&replay, tampered).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "replay-profile", "bitgun", replay.to_str().unwrap(),
            "--range", "12-14",
            "--output", dir.path().join("profile-out").to_str().unwrap(),
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(workspace_root())
        .output()
        .expect("profile drifted replay");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "a drifted run must still complete its range\nstderr:\n{stderr}"
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
    assert_eq!(terminal["status"], "complete");
    assert_eq!(terminal["identity"]["exact_artifact"], false);
    assert_eq!(terminal["divergence"]["checkpoint_interval"], 10);
    assert_eq!(
        terminal["divergence"]["first_divergent_tick"].as_u64(),
        Some(tick.parse::<u64>().unwrap()),
        "the terminal row must name the tick the run drifted at: {terminal:#?}"
    );
    assert!(
        terminal["stderr_tail"].as_str().unwrap().contains("desync"),
        "a divergent run keeps its diagnostic evidence: {terminal:#?}"
    );
    assert!(
        stderr.contains(&format!("diverged from the recording at tick {tick}")),
        "the operator must be told at the terminal too: {stderr}"
    );
}

/// A recording whose Artifact has moved still profiles: divergence is the
/// accepted default and only the LABEL has to be honest, so the run warns,
/// measures its range, and stamps `exact_artifact: false` on every row it
/// emits. The old behavior -- rejecting before spawning playback -- cost every
/// benchmark run on a changed build.
#[test]
fn replay_profile_warns_and_labels_a_different_artifact_instead_of_rejecting() {
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
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(workspace_root())
        .output()
        .expect("profile mismatched replay");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "a moved Artifact must still profile:\nstdout:\n{}\nstderr:\n{stderr}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        stderr.contains("replay Artifact differs")
            && stderr.contains("ffffffffffffffff"),
        "the warning must name both identities: {stderr}"
    );
    let rows = String::from_utf8(output.stdout)
        .expect("UTF-8 profile output")
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("JSONL row"))
        .collect::<Vec<_>>();
    assert!(
        rows.iter().any(|row| row["kind"] == "frame"),
        "the drifted run must still produce measured rows: {rows:#?}"
    );
    let terminal = rows
        .iter()
        .find(|row| row["kind"] == "run")
        .expect("terminal row");
    assert_eq!(terminal["status"], "complete");
    for row in &rows {
        assert_eq!(
            row["identity"]["exact_artifact"], false,
            "every row of a drifted run must be labelled inexact: {row:#?}"
        );
        assert_eq!(row["identity"]["replay_build"], "ffffffffffffffff");
        assert_ne!(
            row["identity"]["replay_build"],
            row["identity"]["playback_build"]
        );
    }
    assert!(
        terminal["divergence"]["detectable"].as_bool().unwrap(),
        "the fixture records checkpoints, so drift is locatable: {terminal:#?}"
    );
}
