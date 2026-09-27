use serde_json::{json, Value};
use std::path::Path;
use std::process::Command;

fn frame(run: usize, ordinal: usize, recorded_us: u64, playback_us: u64) -> Value {
    json!({
        "format": "rock.replay-profile.v1",
        "kind": "frame",
        "run": run,
        "status": "complete",
        "identity": {
            "replay_version": 13,
            "game": "MAG",
            "source": "mag",
            "replay_revision": "abc123",
            "playback_revision": "abc123",
            "replay_build": "0123456789abcdef",
            "playback_build": "0123456789abcdef",
            "exact_artifact": true,
            "same_revision": true
        },
        "ordinal": ordinal,
        "tick": ordinal,
        "recorded": {
            "total_us": recorded_us,
            "logic_us": recorded_us / 4,
            "render_us": recorded_us / 2,
            "present_us": recorded_us / 4
        },
        "playback": {
            "total_us": playback_us,
            "logic_us": playback_us / 4,
            "render_us": playback_us / 2,
            "present_us": playback_us / 4
        },
        "input": {
            "pressed_keys": if ordinal == 10 { json!(["E"]) } else { json!([]) },
            "released_keys": [],
            "pressed_buttons": [],
            "released_buttons": [],
            "pressed_pad_buttons": [],
            "released_pad_buttons": [],
            "mouse": { "x_hex": "0", "y_hex": "0", "wheel": 0, "imgui_captured": false },
            "pad_axes": []
        },
        "recorded_profile": if ordinal == 10 { json!({
            "heap_bytes": 2000,
            "heap_growth_bytes": 500,
            "gc": false,
            "dropped": 0,
            "sections": [
                { "name": "recorded_targeting", "inclusive_us": 7000, "calls": 1 }
            ]
        }) } else { Value::Null },
        "playback_profile": {
            "heap_bytes": 1000 + run,
            "heap_growth_bytes": run * 10,
            "gc": false,
            "dropped": 0,
            "allocation_traffic": { "available": false, "bytes": null },
            "sections": [
                { "name": "update_targeting", "inclusive_us": playback_us / 3, "calls": 1 }
            ]
        },
        "graphics": {
            "draw_calls": 20 + run,
            "text_draw_calls": 3,
            "stream_upload_calls": 2,
            "stream_upload_bytes": 64,
            "streamed_vertices": 12
        }
    })
}

fn terminal(run: usize, status: &str, observed: usize) -> Value {
    json!({
        "format": "rock.replay-profile.v1",
        "kind": "run",
        "run": run,
        "status": status,
        "identity": {
            "replay_version": 13,
            "game": "MAG",
            "source": "mag",
            "replay_revision": "abc123",
            "playback_revision": if status == "complete" { "abc123" } else { "" },
            "replay_build": "0123456789abcdef",
            "playback_build": "0123456789abcdef",
            "exact_artifact": true,
            "same_revision": status == "complete"
        },
        "range": { "from": 10, "to": 11 },
        "warmup_frames": 10,
        "requested_frames": 2,
        "observed_frames": observed,
        "observed_ordinals": if observed == 2 { json!([10, 11]) } else { json!([]) },
        "exit_code": if status == "complete" { json!(0) } else { json!(3) },
        "timed_out": false,
        "error": if status == "complete" { Value::Null } else { json!("fixture failure") },
        "stderr_tail": if status == "complete" { "" } else { "fixture failed" }
    })
}

fn write_jsonl(path: &Path) {
    let playback = [
        [10_000, 20_000],
        [11_000, 21_000],
        [40_000, 20_000],
        [42_000, 21_000],
    ];
    let mut rows = Vec::new();
    for (run_index, values) in playback.into_iter().enumerate() {
        let run = run_index + 1;
        rows.push(frame(run, 10, 60_000, values[0]));
        rows.push(frame(run, 11, 19_000, values[1]));
        rows.push(terminal(run, "complete", 2));
    }
    rows.push(terminal(5, "process_failure", 0));
    std::fs::write(
        path,
        rows.into_iter()
            .map(|row| serde_json::to_string(&row).unwrap())
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .unwrap();
}

#[test]
fn replay_profile_report_joins_runs_and_preserves_failures() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("profile.jsonl");
    write_jsonl(&input);
    let output = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "replay-profile-report",
            input.to_str().unwrap(),
            "--top",
            "2",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "report failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["format"], "rock.replay-profile-report.v1");
    assert_eq!(report["runs"]["complete"], 4);
    assert_eq!(report["runs"]["incomplete"], 1);
    assert_eq!(report["incomplete_runs"][0]["status"], "process_failure");
    assert_eq!(report["top_frames"][0]["ordinal"], 10);
    assert_eq!(report["top_frames"][0]["samples"], 4);
    assert_eq!(report["top_frames"][0]["recorded"]["total_us"], 60_000);
    assert_eq!(
        report["top_frames"][0]["playback"]["total"]["median_us"],
        25_500
    );
    assert_eq!(
        report["top_frames"][0]["repeatability"],
        "bimodal_candidate"
    );
    assert_eq!(
        report["top_frames"][0]["recording_relation"],
        "recorded_slower"
    );
    assert_eq!(report["top_frames"][0]["input"]["pressed_keys"][0], "E");
    assert_eq!(
        report["top_frames"][0]["sections"][0]["name"],
        "update_targeting"
    );
    assert_eq!(
        report["top_frames"][0]["recorded_profile"]["sections"][0]["name"],
        "recorded_targeting"
    );
    assert_eq!(report["top_frames"][1]["repeatability"], "consistent");

    let pretty = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "replay-profile-report",
            input.to_str().unwrap(),
            "--top",
            "1",
        ])
        .output()
        .unwrap();
    assert!(pretty.status.success());
    let pretty = String::from_utf8(pretty.stdout).unwrap();
    assert!(pretty.contains("Runs: 4 complete, 1 incomplete"));
    assert!(pretty.contains("#10"));
    assert!(pretty.contains("+key E"));
    assert!(
        !pretty.contains("#11"),
        "--top 1 must retain one ranked frame"
    );
}

fn read_rows(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn write_rows(path: &Path, rows: &[Value]) {
    std::fs::write(
        path,
        rows.iter()
            .map(|row| serde_json::to_string(row).unwrap())
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .unwrap();
}

fn report(path: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_rock"))
        .args(["replay-profile-report", path.to_str().unwrap()])
        .output()
        .unwrap()
}

#[test]
fn replay_profile_report_rejects_false_denominators_and_mixed_replay_rows() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("profile.jsonl");
    write_jsonl(&input);

    let mut rows = read_rows(&input);
    let terminal = rows
        .iter_mut()
        .find(|row| row["kind"] == "run" && row["run"] == 4)
        .unwrap();
    terminal["observed_frames"] = json!(1);
    let bad_denominator = dir.path().join("bad-denominator.jsonl");
    write_rows(&bad_denominator, &rows);
    let output = report(&bad_denominator);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("terminal denominator disagrees"));

    let mut rows = read_rows(&input);
    let frame = rows
        .iter_mut()
        .find(|row| row["kind"] == "frame" && row["run"] == 4 && row["ordinal"] == 10)
        .unwrap();
    frame["input"]["pressed_keys"] = json!(["Q"]);
    let mixed_input = dir.path().join("mixed-input.jsonl");
    write_rows(&mixed_input, &rows);
    let output = report(&mixed_input);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("input transitions disagree"));

    let mut rows = read_rows(&input);
    let frame = rows
        .iter_mut()
        .find(|row| row["kind"] == "frame" && row["run"] == 4 && row["ordinal"] == 10)
        .unwrap();
    frame["identity"]["playback_revision"] = json!("different-revision");
    frame["identity"]["same_revision"] = json!(true);
    let mixed_revision = dir.path().join("mixed-revision.jsonl");
    write_rows(&mixed_revision, &rows);
    let output = report(&mixed_revision);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("playback revisions disagree"));
}

/// A drifted run is reportable -- divergence is the accepted default -- but it
/// must never read as an exact run. The label appears in the JSON root, and the
/// pretty output carries it BOTH above the run summary and immediately above
/// the measurements, so no number is far from it.
#[test]
fn replay_profile_report_labels_a_drifted_run_and_names_its_divergent_tick() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("profile.jsonl");
    write_jsonl(&input);
    let mut rows = read_rows(&input);
    for row in &mut rows {
        row["identity"]["playback_build"] = json!("fedcba9876543210");
        row["identity"]["exact_artifact"] = json!(false);
        if row["kind"] == "run" {
            row["divergence"] = json!({
                "checkpoint_interval": 10,
                "detectable": true,
                "first_divergent_tick": 40,
            });
        }
    }
    let drifted = dir.path().join("drifted.jsonl");
    write_rows(&drifted, &rows);

    let output = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args(["replay-profile-report", drifted.to_str().unwrap(), "--format", "json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "a drifted profile must still report:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json_report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json_report["exact_artifact"], false);
    assert_eq!(json_report["divergence"]["first_divergent_tick"], 40);
    assert_eq!(json_report["divergence"]["detectable"], true);
    assert_eq!(json_report["runs"]["complete"], 4, "the measurements survive the label");

    let pretty = report(&drifted);
    assert!(pretty.status.success());
    let pretty = String::from_utf8(pretty.stdout).unwrap();
    assert_eq!(
        pretty.matches("NOT EXACT").count(),
        2,
        "the label must sit above the summary AND above the numbers:\n{pretty}"
    );
    assert!(pretty.contains("diverged at tick 40"), "{pretty}");
    assert!(pretty.contains("fedcba9876543210"), "the playback identity must be named:\n{pretty}");
}

/// The exactness label is not decorative: a row cannot claim an exactness its
/// own two fingerprints contradict, in either direction.
#[test]
fn replay_profile_report_rejects_a_dishonest_exactness_label() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("profile.jsonl");
    write_jsonl(&input);

    let mut rows = read_rows(&input);
    for row in &mut rows {
        row["identity"]["playback_build"] = json!("fedcba9876543210");
    }
    let forged_exact = dir.path().join("forged-exact.jsonl");
    write_rows(&forged_exact, &rows);
    let output = report(&forged_exact);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("claims an exact Artifact"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let mut rows = read_rows(&input);
    for row in &mut rows {
        row["identity"]["exact_artifact"] = json!(false);
    }
    let forged_drift = dir.path().join("forged-drift.jsonl");
    write_rows(&forged_drift, &rows);
    let output = report(&forged_drift);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("claims drift"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn replay_profile_report_retains_a_failed_population_with_no_complete_run() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("failed.jsonl");
    write_rows(&input, &[terminal(1, "process_failure", 0)]);
    let output = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "replay-profile-report",
            input.to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["runs"]["complete"], 0);
    assert_eq!(report["runs"]["incomplete"], 1);
    assert_eq!(report["top_frames"].as_array().unwrap().len(), 0);
    assert_eq!(report["incomplete_runs"][0]["status"], "process_failure");
}
