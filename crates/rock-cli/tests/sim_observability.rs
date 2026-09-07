use rock_cli::sim_observability::SimRunRecorder;
use serde_json::Value;
use std::path::Path;

fn events(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .expect("sim recorder events")
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid JSONL event"))
        .collect()
}

#[test]
fn persistent_records_keep_runs_and_validated_occurrences() {
    let root = tempfile::tempdir().expect("record root");
    let first = SimRunRecorder::begin_at(root.path(), "testbed", "a a", false, Some(2), "workers");
    let ids = first.plan_entries(&["a".into(), "a".into()]);
    assert_eq!(ids.len(), 2);
    assert_ne!(ids[0], ids[1], "repeated names need distinct occurrence IDs");
    let worker = first.worker_spawn(&[(ids[0], "a".into())], &[], "shared");
    first.worker_started(worker, 123);
    first.entry_start(worker, ids[0], "a", 0);
    first.entry_end(worker, ids[0], "a", 0);
    first.entry_result(ids[0], worker, "PASS", Some(1), 1, false, None);
    first.finish(true, None);

    let first_path = first.events_path().expect("first event path");
    let first_events = events(&first_path);
    assert_eq!(first_events.first().and_then(|e| e["event"].as_str()), Some("run_start"));
    assert_eq!(
        first_events.first().and_then(|e| e["source"]["status"].as_str()),
        Some("available"),
        "source metadata follows the invocation workspace, not ROCK_SIM_LOG_DIR"
    );
    assert!(first_events.iter().any(|e| e["event"] == "entry_start"));
    assert!(first_events.iter().any(|e| e["event"] == "entry_end"));
    assert_eq!(first_events.last().and_then(|e| e["event"].as_str()), Some("run_end"));
    assert_eq!(first_events.last().and_then(|e| e["status"].as_str()), Some("INCOMPLETE"));
    assert_eq!(first_events.last().and_then(|e| e["complete"].as_bool()), Some(false));

    let second = SimRunRecorder::begin_at(root.path(), "testbed", "a", true, None, "single");
    second.finish(false, Some("sim failed"));
    let second_path = second.events_path().expect("second event path");
    assert_ne!(first_path.parent(), second_path.parent(), "each invocation needs its own history");
    assert!(first_path.exists(), "a later invocation must retain earlier history");
    assert_eq!(events(&second_path).last().unwrap()["status"], "FAIL");
}

#[test]
fn concurrent_recorders_keep_each_jsonl_stream_parseable() {
    let root = tempfile::tempdir().expect("record root");
    let left = SimRunRecorder::begin_at(root.path(), "testbed", "left", false, Some(1), "single");
    let right = SimRunRecorder::begin_at(root.path(), "testbed", "right", false, Some(1), "single");
    let left_path = left.events_path().expect("left event path");
    let right_path = right.events_path().expect("right event path");
    std::thread::scope(|scope| {
        scope.spawn(|| {
            for _ in 0..8 {
                left.phase_start("compile");
                left.phase_end("compile", true);
            }
            left.finish(true, None);
        });
        scope.spawn(|| {
            for _ in 0..8 {
                right.phase_start("compile");
                right.phase_end("compile", true);
            }
            right.finish(true, None);
        });
    });
    assert_eq!(events(&left_path).last().unwrap()["event"], "run_end");
    assert_eq!(events(&right_path).last().unwrap()["event"], "run_end");
}

#[test]
fn interrupted_recorder_keeps_partial_history_without_success_claim() {
    let root = tempfile::tempdir().expect("record root");
    let recorder = SimRunRecorder::begin_at(root.path(), "testbed", "a", true, None, "single");
    let ids = recorder.plan_entries(&["a".into()]);
    recorder.worker_spawn(&[(ids[0], "a".into())], &[], "isolated");
    let path = recorder.events_path().expect("partial event path");
    drop(recorder);
    let partial = events(&path);
    assert_ne!(partial.last().and_then(|event| event["event"].as_str()), Some("run_end"));
    let later = SimRunRecorder::begin_at(root.path(), "testbed", "b", false, None, "single");
    let later_path = later.events_path().expect("later event path");
    later.finish(true, None);
    assert!(path.exists(), "a later invocation must retain partial history");
    assert_ne!(path.parent(), later_path.parent());
}
