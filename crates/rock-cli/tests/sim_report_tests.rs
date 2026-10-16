//! `rock sim-report` reads the persistent `rock sim` history in aggregate.
//!
//! Every fixture here is a real record shape: `events.jsonl` lines exactly as
//! `SimRunRecorder` writes them (compact, sorted keys, `schema_version` 1,
//! monotonic `elapsed_ms`) plus the matching `timings.json`. The cases cover a
//! complete run, an interrupted run with no `run_end`, a run with recoveries,
//! a run whose `timings.json` was never written, and the documented ordering
//! where a reaped worker's buffered entry lines land after its `worker_end`.

use serde_json::Value;
use std::path::{Path, PathBuf};

/// Newest-first resolution is by run id, so fixture ids keep the real
/// `<unix-ms>-<pid>-<counter>` shape.
const COMPLETE: &str = "1700000000000-100-0";
const INTERRUPTED: &str = "1700000001000-101-0";
const RECOVERED: &str = "1700000002000-102-0";
const NO_TIMINGS: &str = "1700000003000-103-0";
const DRAINED_LATE: &str = "1700000004000-104-0";

fn write_run(root: &Path, run_id: &str, events: &[String], timings: Option<&str>) -> PathBuf {
    let dir = root.join(run_id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("events.jsonl"), format!("{}\n", events.join("\n"))).unwrap();
    if let Some(body) = timings {
        std::fs::write(dir.join("timings.json"), body).unwrap();
    }
    dir
}

fn line(run_id: &str, body: &str) -> String {
    format!("{{{body},\"run_id\":\"{run_id}\",\"schema_version\":1}}")
}

/// A complete two-worker run with hand-computable totals:
///
/// * wall 2500 ms; three envelopes summing to 1700 ms.
/// * worker 1 lives 100..2300 (2200 ms), boots for 500 ms, runs two entries
///   (1000 + 500 ms), leaving 200 ms of remainder.
/// * worker 2 lives 150..1400 (1250 ms), boots for 1000 ms, runs one entry
///   (200 ms), leaving 50 ms of remainder.
/// * so lifetimes sum to 3450 ms = 1700 inside + 1500 startup + 250 remainder.
fn complete_events() -> Vec<String> {
    let id = COMPLETE;
    vec![
        line(id, "\"elapsed_ms\":10,\"event\":\"run_start\",\"project_arg\":\"mag\",\"quiet\":false,\"requested_jobs\":2,\"requested_mode\":\"default\",\"selection\":\"<all>\",\"source\":{\"dirty\":true,\"revision\":\"abc1234def\",\"status\":\"available\"},\"started_at_unix_ms\":1700000000000"),
        line(id, "\"effective_jobs\":2,\"effective_mode\":\"default\",\"elapsed_ms\":20,\"event\":\"run_config\",\"project_path\":\"mag\",\"requested_jobs\":2,\"requested_mode\":\"default\",\"update_snapshots\":false"),
        line(id, "\"elapsed_ms\":30,\"event\":\"entry_planned\",\"occurrence_id\":1,\"parts\":[\"alpha\"],\"sequence\":0,\"spec\":\"alpha\""),
        line(id, "\"elapsed_ms\":31,\"event\":\"entry_planned\",\"occurrence_id\":2,\"parts\":[\"beta\"],\"sequence\":1,\"spec\":\"beta\""),
        line(id, "\"elapsed_ms\":32,\"event\":\"entry_planned\",\"occurrence_id\":3,\"parts\":[\"gamma\"],\"sequence\":2,\"spec\":\"gamma\""),
        line(id, "\"boot_signature\":[],\"elapsed_ms\":40,\"entries\":[{\"name\":\"alpha\",\"occurrence_id\":1},{\"name\":\"beta\",\"occurrence_id\":2}],\"event\":\"lane_planned\",\"isolation_reason\":null,\"kind\":\"shared\",\"lane_id\":0"),
        line(id, "\"boot_signature\":[[\"SIM_SKIP_RENDER\",\"0\"]],\"elapsed_ms\":41,\"entries\":[{\"name\":\"gamma\",\"occurrence_id\":3}],\"event\":\"lane_planned\",\"isolation_reason\":\"declares @sim_env(SIM_WORKER_ISOLATED)\",\"kind\":\"isolated\",\"lane_id\":1"),
        line(id, "\"boot_signature\":[],\"elapsed_ms\":95,\"entries\":[{\"name\":\"alpha\",\"occurrence_id\":1},{\"name\":\"beta\",\"occurrence_id\":2}],\"event\":\"worker_spawn\",\"kind\":\"shared\",\"worker_attempt\":1"),
        line(id, "\"elapsed_ms\":100,\"event\":\"worker_started\",\"pid\":501,\"worker_attempt\":1"),
        line(id, "\"boot_signature\":[[\"SIM_SKIP_RENDER\",\"0\"]],\"elapsed_ms\":145,\"entries\":[{\"name\":\"gamma\",\"occurrence_id\":3}],\"event\":\"worker_spawn\",\"kind\":\"isolated\",\"worker_attempt\":2"),
        line(id, "\"elapsed_ms\":150,\"event\":\"worker_started\",\"pid\":502,\"worker_attempt\":2"),
        line(id, "\"elapsed_ms\":600,\"event\":\"worker_first_entry\",\"name\":\"alpha\",\"occurrence_id\":1,\"part\":0,\"worker_attempt\":1"),
        line(id, "\"elapsed_ms\":600,\"event\":\"entry_start\",\"name\":\"alpha\",\"occurrence_id\":1,\"part\":0,\"worker_attempt\":1"),
        line(id, "\"elapsed_ms\":1150,\"event\":\"worker_first_entry\",\"name\":\"gamma\",\"occurrence_id\":3,\"part\":0,\"worker_attempt\":2"),
        line(id, "\"elapsed_ms\":1150,\"event\":\"entry_start\",\"name\":\"gamma\",\"occurrence_id\":3,\"part\":0,\"worker_attempt\":2"),
        line(id, "\"elapsed_ms\":1350,\"event\":\"entry_end\",\"frame_count\":8,\"name\":\"gamma\",\"occurrence_id\":3,\"part\":0,\"worker_attempt\":2"),
        line(id, "\"elapsed_ms\":1360,\"event\":\"entry_result\",\"exit_code\":null,\"expects_pass\":1,\"frame_count\":8,\"occurrence_id\":3,\"timed_out\":false,\"verdict\":\"PASS\",\"worker_attempt\":2"),
        line(id, "\"elapsed_ms\":1400,\"event\":\"worker_end\",\"exit_code\":0,\"signal\":null,\"status\":\"success\",\"timed_out\":false,\"worker_attempt\":2"),
        line(id, "\"elapsed_ms\":1600,\"event\":\"entry_end\",\"frame_count\":20,\"name\":\"alpha\",\"occurrence_id\":1,\"part\":0,\"worker_attempt\":1"),
        line(id, "\"elapsed_ms\":1610,\"event\":\"entry_result\",\"exit_code\":null,\"expects_pass\":1,\"frame_count\":20,\"occurrence_id\":1,\"timed_out\":false,\"verdict\":\"PASS\",\"worker_attempt\":1"),
        line(id, "\"elapsed_ms\":1700,\"event\":\"entry_start\",\"name\":\"beta\",\"occurrence_id\":2,\"part\":0,\"worker_attempt\":1"),
        line(id, "\"elapsed_ms\":2200,\"event\":\"entry_end\",\"frame_count\":6,\"name\":\"beta\",\"occurrence_id\":2,\"part\":0,\"worker_attempt\":1"),
        line(id, "\"elapsed_ms\":2210,\"event\":\"entry_result\",\"exit_code\":null,\"expects_pass\":0,\"frame_count\":6,\"occurrence_id\":2,\"timed_out\":false,\"verdict\":\"FAIL\",\"worker_attempt\":1"),
        line(id, "\"elapsed_ms\":2300,\"event\":\"worker_end\",\"exit_code\":0,\"signal\":null,\"status\":\"success\",\"timed_out\":false,\"worker_attempt\":1"),
        line(id, "\"complete\":true,\"elapsed_ms\":2500,\"error\":\"1 of 3 sims failed in `mag`\",\"event\":\"run_end\",\"finalized_entries\":3,\"planned_entries\":3,\"status\":\"FAIL\""),
    ]
}

const COMPLETE_TIMINGS: &str = r#"{"accepted":false,"entries":[
{"completed":true,"duration_ms":1000,"identity":"a1","name":"alpha","occurrence_id":1,"part":0,"passed":true},
{"completed":true,"duration_ms":500,"identity":"b2","name":"beta","occurrence_id":2,"part":0,"passed":false},
{"completed":true,"duration_ms":200,"identity":"c3","name":"gamma","occurrence_id":3,"part":0,"passed":true}
],"host":{"arch":"aarch64","logical_cpus":14},"schema_version":1,"warning_policy":{"absolute_ms":10000,"minimum_samples":5,"regression_delta_ms":1000,"regression_factor":2.0}}"#;

/// An invocation killed mid-run: no `run_end`, one envelope still open, one
/// worker never reaped.
fn interrupted_events() -> Vec<String> {
    let id = INTERRUPTED;
    vec![
        line(id, "\"elapsed_ms\":10,\"event\":\"run_start\",\"project_arg\":\"mag\",\"quiet\":false,\"requested_jobs\":1,\"requested_mode\":\"default\",\"selection\":\"alpha,beta\",\"source\":{\"status\":\"unavailable\"},\"started_at_unix_ms\":1700000001000"),
        line(id, "\"effective_jobs\":1,\"effective_mode\":\"default\",\"elapsed_ms\":20,\"event\":\"run_config\",\"project_path\":\"mag\",\"requested_jobs\":1,\"requested_mode\":\"default\",\"update_snapshots\":false"),
        line(id, "\"elapsed_ms\":30,\"event\":\"entry_planned\",\"occurrence_id\":1,\"parts\":[\"alpha\"],\"sequence\":0,\"spec\":\"alpha\""),
        line(id, "\"elapsed_ms\":31,\"event\":\"entry_planned\",\"occurrence_id\":2,\"parts\":[\"beta\"],\"sequence\":1,\"spec\":\"beta\""),
        line(id, "\"boot_signature\":[],\"elapsed_ms\":40,\"entries\":[{\"name\":\"alpha\",\"occurrence_id\":1},{\"name\":\"beta\",\"occurrence_id\":2}],\"event\":\"lane_planned\",\"isolation_reason\":null,\"kind\":\"shared\",\"lane_id\":0"),
        line(id, "\"boot_signature\":[],\"elapsed_ms\":95,\"entries\":[{\"name\":\"alpha\",\"occurrence_id\":1},{\"name\":\"beta\",\"occurrence_id\":2}],\"event\":\"worker_spawn\",\"kind\":\"shared\",\"worker_attempt\":1"),
        line(id, "\"elapsed_ms\":100,\"event\":\"worker_started\",\"pid\":601,\"worker_attempt\":1"),
        line(id, "\"elapsed_ms\":300,\"event\":\"worker_first_entry\",\"name\":\"alpha\",\"occurrence_id\":1,\"part\":0,\"worker_attempt\":1"),
        line(id, "\"elapsed_ms\":300,\"event\":\"entry_start\",\"name\":\"alpha\",\"occurrence_id\":1,\"part\":0,\"worker_attempt\":1"),
        line(id, "\"elapsed_ms\":700,\"event\":\"entry_end\",\"name\":\"alpha\",\"occurrence_id\":1,\"part\":0,\"worker_attempt\":1"),
        line(id, "\"elapsed_ms\":710,\"event\":\"entry_result\",\"exit_code\":null,\"expects_pass\":1,\"frame_count\":null,\"occurrence_id\":1,\"timed_out\":false,\"verdict\":\"PASS\",\"worker_attempt\":1"),
        line(id, "\"elapsed_ms\":800,\"event\":\"entry_start\",\"name\":\"beta\",\"occurrence_id\":2,\"part\":0,\"worker_attempt\":1"),
    ]
}

/// Two recoveries, both the documented destroyed-native-resources cause.
fn recovered_events() -> Vec<String> {
    let id = RECOVERED;
    vec![
        line(id, "\"elapsed_ms\":10,\"event\":\"run_start\",\"project_arg\":\"mag\",\"quiet\":false,\"requested_jobs\":1,\"requested_mode\":\"default\",\"selection\":\"<all>\",\"source\":{\"dirty\":false,\"revision\":\"deadbeef00\",\"status\":\"available\"},\"started_at_unix_ms\":1700000002000"),
        line(id, "\"effective_jobs\":1,\"effective_mode\":\"default\",\"elapsed_ms\":20,\"event\":\"run_config\",\"project_path\":\"mag\",\"requested_jobs\":1,\"requested_mode\":\"default\",\"update_snapshots\":false"),
        line(id, "\"elapsed_ms\":30,\"event\":\"entry_planned\",\"occurrence_id\":1,\"parts\":[\"alpha\"],\"sequence\":0,\"spec\":\"alpha\""),
        line(id, "\"elapsed_ms\":31,\"event\":\"entry_planned\",\"occurrence_id\":2,\"parts\":[\"beta\"],\"sequence\":1,\"spec\":\"beta\""),
        line(id, "\"boot_signature\":[],\"elapsed_ms\":40,\"entries\":[{\"name\":\"alpha\",\"occurrence_id\":1},{\"name\":\"beta\",\"occurrence_id\":2}],\"event\":\"lane_planned\",\"isolation_reason\":\"hot-reload lane: mutates the one session and overlay\",\"kind\":\"shared\",\"lane_id\":0"),
        line(id, "\"boot_signature\":[],\"elapsed_ms\":95,\"entries\":[{\"name\":\"alpha\",\"occurrence_id\":1},{\"name\":\"beta\",\"occurrence_id\":2}],\"event\":\"worker_spawn\",\"kind\":\"shared\",\"worker_attempt\":1"),
        line(id, "\"elapsed_ms\":100,\"event\":\"worker_started\",\"pid\":701,\"worker_attempt\":1"),
        line(id, "\"elapsed_ms\":300,\"event\":\"worker_first_entry\",\"name\":\"alpha\",\"occurrence_id\":1,\"part\":0,\"worker_attempt\":1"),
        line(id, "\"elapsed_ms\":300,\"event\":\"entry_start\",\"name\":\"alpha\",\"occurrence_id\":1,\"part\":0,\"worker_attempt\":1"),
        line(id, "\"elapsed_ms\":500,\"event\":\"entry_end\",\"name\":\"alpha\",\"occurrence_id\":1,\"part\":0,\"worker_attempt\":1"),
        line(id, "\"elapsed_ms\":510,\"event\":\"entry_result\",\"exit_code\":null,\"expects_pass\":1,\"frame_count\":null,\"occurrence_id\":1,\"timed_out\":false,\"verdict\":\"PASS\",\"worker_attempt\":1"),
        line(id, "\"elapsed_ms\":560,\"event\":\"worker_end\",\"exit_code\":0,\"signal\":null,\"status\":\"success\",\"timed_out\":false,\"worker_attempt\":1"),
        line(id, "\"cause\":\"this entry destroyed native resources the boot process created: cursor:1\",\"elapsed_ms\":570,\"event\":\"worker_recovery\",\"prior_worker_attempt\":1,\"responsible_occurrence_id\":1,\"round\":1,\"successors\":[{\"name\":\"beta\",\"occurrence_id\":2}]"),
        line(id, "\"boot_signature\":[],\"elapsed_ms\":575,\"entries\":[{\"name\":\"beta\",\"occurrence_id\":2}],\"event\":\"worker_spawn\",\"kind\":\"shared\",\"worker_attempt\":2"),
        line(id, "\"elapsed_ms\":580,\"event\":\"worker_started\",\"pid\":702,\"worker_attempt\":2"),
        line(id, "\"elapsed_ms\":900,\"event\":\"worker_first_entry\",\"name\":\"beta\",\"occurrence_id\":2,\"part\":0,\"worker_attempt\":2"),
        line(id, "\"elapsed_ms\":900,\"event\":\"entry_start\",\"name\":\"beta\",\"occurrence_id\":2,\"part\":0,\"worker_attempt\":2"),
        line(id, "\"elapsed_ms\":1000,\"event\":\"entry_end\",\"name\":\"beta\",\"occurrence_id\":2,\"part\":0,\"worker_attempt\":2"),
        line(id, "\"elapsed_ms\":1010,\"event\":\"entry_result\",\"exit_code\":null,\"expects_pass\":1,\"frame_count\":null,\"occurrence_id\":2,\"timed_out\":false,\"verdict\":\"PASS\",\"worker_attempt\":2"),
        line(id, "\"cause\":\"this entry destroyed native resources the boot process created: cursor:2\",\"elapsed_ms\":1020,\"event\":\"worker_recovery\",\"prior_worker_attempt\":2,\"responsible_occurrence_id\":2,\"round\":2,\"successors\":[]"),
        line(id, "\"elapsed_ms\":1050,\"event\":\"worker_end\",\"exit_code\":0,\"signal\":null,\"status\":\"success\",\"timed_out\":false,\"worker_attempt\":2"),
        line(id, "\"complete\":true,\"elapsed_ms\":1200,\"error\":null,\"event\":\"run_end\",\"finalized_entries\":2,\"planned_entries\":2,\"status\":\"PASS\""),
    ]
}

/// An older run predating `timings.json`. 472 of the 750 retained runs in the
/// author's history have the file; the reader must not need it.
fn no_timings_events() -> Vec<String> {
    let id = NO_TIMINGS;
    vec![
        line(id, "\"elapsed_ms\":10,\"event\":\"run_start\",\"project_arg\":\"mag\",\"quiet\":false,\"requested_jobs\":1,\"requested_mode\":\"default\",\"selection\":\"alpha\",\"source\":{\"dirty\":false,\"revision\":\"0e3ff677\",\"status\":\"available\"},\"started_at_unix_ms\":1700000003000"),
        line(id, "\"effective_jobs\":1,\"effective_mode\":\"default\",\"elapsed_ms\":20,\"event\":\"run_config\",\"project_path\":\"mag\",\"requested_jobs\":1,\"requested_mode\":\"default\",\"update_snapshots\":false"),
        line(id, "\"elapsed_ms\":30,\"event\":\"entry_planned\",\"occurrence_id\":1,\"parts\":[\"alpha\"],\"sequence\":0,\"spec\":\"alpha\""),
        line(id, "\"boot_signature\":[],\"elapsed_ms\":40,\"entries\":[{\"name\":\"alpha\",\"occurrence_id\":1}],\"event\":\"lane_planned\",\"isolation_reason\":\"@sim_env(\\\"MAG_TELEMETRY_ROOT\\\", \\\"$OUT/telemetry\\\") interpolates $OUT, which resolves before the process starts\",\"kind\":\"isolated\",\"lane_id\":0"),
        line(id, "\"boot_signature\":[],\"elapsed_ms\":95,\"entries\":[{\"name\":\"alpha\",\"occurrence_id\":1}],\"event\":\"worker_spawn\",\"kind\":\"isolated\",\"worker_attempt\":1"),
        line(id, "\"elapsed_ms\":100,\"event\":\"worker_started\",\"pid\":801,\"worker_attempt\":1"),
        line(id, "\"elapsed_ms\":400,\"event\":\"worker_first_entry\",\"name\":\"alpha\",\"occurrence_id\":1,\"part\":0,\"worker_attempt\":1"),
        line(id, "\"elapsed_ms\":400,\"event\":\"entry_start\",\"name\":\"alpha\",\"occurrence_id\":1,\"part\":0,\"worker_attempt\":1"),
        line(id, "\"elapsed_ms\":600,\"event\":\"entry_end\",\"name\":\"alpha\",\"occurrence_id\":1,\"part\":0,\"worker_attempt\":1"),
        line(id, "\"elapsed_ms\":610,\"event\":\"entry_result\",\"exit_code\":null,\"expects_pass\":1,\"frame_count\":null,\"occurrence_id\":1,\"timed_out\":false,\"verdict\":\"PASS\",\"worker_attempt\":1"),
        line(id, "\"elapsed_ms\":650,\"event\":\"worker_end\",\"exit_code\":0,\"signal\":null,\"status\":\"success\",\"timed_out\":false,\"worker_attempt\":1"),
        line(id, "\"complete\":true,\"elapsed_ms\":700,\"error\":null,\"event\":\"run_end\",\"finalized_entries\":1,\"planned_entries\":1,\"status\":\"PASS\""),
    ]
}

/// The documented drain ordering: the worker is reaped at 500 ms but its
/// reader had 250 ms of buffered envelope lines still to emit.
fn drained_late_events() -> Vec<String> {
    let id = DRAINED_LATE;
    vec![
        line(id, "\"elapsed_ms\":10,\"event\":\"run_start\",\"project_arg\":\"mag\",\"quiet\":false,\"requested_jobs\":1,\"requested_mode\":\"default\",\"selection\":\"alpha\",\"source\":{\"dirty\":false,\"revision\":\"0e3ff677\",\"status\":\"available\"},\"started_at_unix_ms\":1700000004000"),
        line(id, "\"effective_jobs\":1,\"effective_mode\":\"default\",\"elapsed_ms\":20,\"event\":\"run_config\",\"project_path\":\"mag\",\"requested_jobs\":1,\"requested_mode\":\"default\",\"update_snapshots\":false"),
        line(id, "\"elapsed_ms\":30,\"event\":\"entry_planned\",\"occurrence_id\":1,\"parts\":[\"alpha\"],\"sequence\":0,\"spec\":\"alpha\""),
        line(id, "\"boot_signature\":[],\"elapsed_ms\":40,\"entries\":[{\"name\":\"alpha\",\"occurrence_id\":1}],\"event\":\"lane_planned\",\"isolation_reason\":null,\"kind\":\"shared\",\"lane_id\":0"),
        line(id, "\"boot_signature\":[],\"elapsed_ms\":95,\"entries\":[{\"name\":\"alpha\",\"occurrence_id\":1}],\"event\":\"worker_spawn\",\"kind\":\"shared\",\"worker_attempt\":1"),
        line(id, "\"elapsed_ms\":100,\"event\":\"worker_started\",\"pid\":901,\"worker_attempt\":1"),
        line(id, "\"elapsed_ms\":200,\"event\":\"worker_first_entry\",\"name\":\"alpha\",\"occurrence_id\":1,\"part\":0,\"worker_attempt\":1"),
        line(id, "\"elapsed_ms\":200,\"event\":\"entry_start\",\"name\":\"alpha\",\"occurrence_id\":1,\"part\":0,\"worker_attempt\":1"),
        line(id, "\"elapsed_ms\":500,\"event\":\"worker_end\",\"exit_code\":0,\"signal\":null,\"status\":\"success\",\"timed_out\":false,\"worker_attempt\":1"),
        line(id, "\"elapsed_ms\":750,\"event\":\"entry_end\",\"name\":\"alpha\",\"occurrence_id\":1,\"part\":0,\"worker_attempt\":1"),
        line(id, "\"elapsed_ms\":760,\"event\":\"entry_result\",\"exit_code\":null,\"expects_pass\":1,\"frame_count\":null,\"occurrence_id\":1,\"timed_out\":false,\"verdict\":\"PASS\",\"worker_attempt\":1"),
        line(id, "\"complete\":true,\"elapsed_ms\":900,\"error\":null,\"event\":\"run_end\",\"finalized_entries\":1,\"planned_entries\":1,\"status\":\"PASS\""),
    ]
}

fn history() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    write_run(root.path(), COMPLETE, &complete_events(), Some(COMPLETE_TIMINGS));
    write_run(root.path(), INTERRUPTED, &interrupted_events(), None);
    write_run(root.path(), RECOVERED, &recovered_events(), None);
    write_run(root.path(), NO_TIMINGS, &no_timings_events(), None);
    write_run(root.path(), DRAINED_LATE, &drained_late_events(), None);
    root
}

fn report(root: &Path, args: &[&str]) -> String {
    let mut command = crate::command::rock_bin();
    command.arg("sim-report").arg("--sim-log-dir").arg(root).args(args);
    let output = command.output().expect("run sim-report");
    assert!(
        output.status.success(),
        "sim-report {args:?} failed: {}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout),
    );
    String::from_utf8(output.stdout).expect("utf8 report")
}

fn report_json(root: &Path, args: &[&str]) -> Value {
    let mut all = vec!["--format", "json"];
    all.extend_from_slice(args);
    serde_json::from_str(&report(root, &all)).expect("machine-readable report")
}

#[test]
fn sim_report_decomposes_a_complete_run_into_envelopes_startup_and_remainder() {
    let root = history();
    let value = report_json(root.path(), &[COMPLETE]);

    let header = &value["run"];
    assert_eq!(header["run_id"], COMPLETE);
    assert_eq!(header["status"], "FAIL");
    assert_eq!(header["complete"], true);
    assert_eq!(header["wall_ms"], 2500);
    assert_eq!(header["planned_entries"], 3);
    assert_eq!(header["finalized_entries"], 3);
    assert_eq!(header["verdicts"]["PASS"], 2);
    assert_eq!(header["verdicts"]["FAIL"], 1);
    assert_eq!(header["revision"], "abc1234def");
    assert_eq!(header["dirty"], true);
    assert_eq!(header["requested_jobs"], 2);
    assert_eq!(header["effective_jobs"], 2);
    assert_eq!(header["boots"], 2);
    assert_eq!(header["recoveries"], 0);

    let time = &value["time"];
    assert_eq!(time["entries_with_envelope"], 3);
    assert_eq!(time["envelope_total_ms"], 1700);
    assert_eq!(time["worker_span_total_ms"], 3450);
    assert_eq!(time["inside_envelopes_ms"], 1700);
    assert_eq!(time["startup_latency_ms"], 1500);
    assert_eq!(time["remainder_ms"], 250);
    assert_eq!(time["startup_boots"], 2);
    assert_eq!(time["startup_median_ms"], 1000);
    assert_eq!(time["drain_overhang_ms"], 0);
    // 1700 / 2 jobs is the perfect-packing floor for the envelope work.
    assert_eq!(time["ideal_ms_at_effective_jobs"], 850);
    assert_eq!(time["frame_counts"]["known_entries"], 3);
    assert_eq!(time["frame_counts"]["missing_entries"], 0);
    assert_eq!(time["frame_counts"]["total"], 34);
    assert_eq!(time["frame_counts"]["min"], 6);
    assert_eq!(time["frame_counts"]["median"], 8);
    assert_eq!(time["frame_counts"]["max"], 20);
    // The envelope is never described as pure sim-body time.
    assert!(
        time["envelope_qualifier"]
            .as_str()
            .unwrap()
            .contains("native reset"),
        "the envelope qualifier must survive into the machine-readable report"
    );

    let slowest = value["slowest_entries"].as_array().unwrap();
    assert_eq!(slowest[0]["name"], "alpha");
    assert_eq!(slowest[0]["duration_ms"], 1000);
    assert_eq!(slowest[0]["frame_count"], 20);
    assert_eq!(slowest[1]["name"], "beta");
    assert_eq!(slowest[2]["name"], "gamma");
}

#[test]
fn sim_report_surfaces_the_single_entry_worker_class_and_worst_boot_ratios() {
    let root = history();
    let value = report_json(root.path(), &[COMPLETE]);

    let boot = &value["boot_amortization"];
    assert_eq!(boot["workers"], 2);
    assert_eq!(boot["entries_per_worker_max"], 2);
    // Upper median of [1, 2].
    assert_eq!(boot["entries_per_worker_median"], 2);
    assert_eq!(boot["single_entry_workers"]["workers"], 1);
    assert_eq!(boot["single_entry_workers"]["boot_ms"], 1000);
    assert_eq!(boot["single_entry_workers"]["work_ms"], 200);

    let worst = boot["worst_boot_to_work"].as_array().unwrap();
    assert_eq!(worst[0]["name"], "gamma");
    assert_eq!(worst[0]["boot_ms"], 1000);
    assert_eq!(worst[0]["work_ms"], 200);
    assert_eq!(worst[0]["entries"], 1);
    assert_eq!(worst[0]["ratio"].as_f64().unwrap(), 5.0);
}

#[test]
fn sim_report_censuses_lanes_boot_signatures_and_isolation_classes() {
    let root = history();
    let value = report_json(root.path(), &[COMPLETE]);

    let lanes = &value["lanes"];
    assert_eq!(lanes["lanes_planned"], 2);
    assert_eq!(lanes["single_entry_lanes"], 1);
    assert_eq!(lanes["distinct_boot_signatures"], 2);

    let by_signature = lanes["by_signature"].as_array().unwrap();
    assert_eq!(by_signature[0]["entries"], 2);
    assert_eq!(by_signature[0]["lanes"], 1);
    assert_eq!(by_signature[0]["signature"], "<default>");
    assert_eq!(by_signature[1]["signature"], "SIM_SKIP_RENDER=0");

    let by_reason = lanes["by_isolation_class"].as_array().unwrap();
    let classes: Vec<(&str, u64)> = by_reason
        .iter()
        .map(|row| {
            (
                row["class"].as_str().unwrap(),
                row["lanes"].as_u64().unwrap(),
            )
        })
        .collect();
    assert!(classes.contains(&("none", 1)), "got {classes:?}");
    assert!(
        classes.contains(&("SIM_WORKER_ISOLATED", 1)),
        "got {classes:?}"
    );
}

#[test]
fn sim_report_measures_worker_occupancy_and_the_under_occupied_tail() {
    let root = history();
    let value = report_json(root.path(), &[COMPLETE]);

    let occ = &value["occupancy"];
    assert_eq!(occ["wall_ms"], 2500);
    // 3450 ms of worker span over 2500 ms of wall.
    assert!((occ["mean_live_workers"].as_f64().unwrap() - 1.38).abs() < 1e-9);
    let hist: Vec<(u64, u64)> = occ["histogram"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["live_workers"].as_u64().unwrap(),
                row["ms"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(hist, vec![(0, 300), (1, 950), (2, 1250)]);
    // Saturation ends when worker 2 is reaped at 1400 ms.
    assert_eq!(occ["under_occupied_tail_ms"], 1100);
    assert_eq!(occ["ramp_to_full_ms"], 150);
}

#[test]
fn sim_report_joins_timings_json_when_present_and_reports_its_absence() {
    let root = history();

    let with = report_json(root.path(), &[COMPLETE]);
    let timings = &with["timings"];
    assert_eq!(timings["present"], true);
    assert_eq!(timings["entries"], 3);
    assert_eq!(timings["passed"], 2);
    assert_eq!(timings["completed"], 3);
    assert_eq!(timings["duration_total_ms"], 1700);
    assert_eq!(timings["accepted"], false);
    // timings.json duration_ms is the same envelope; disagreement is a defect.
    assert_eq!(timings["envelope_mismatches"], 0);

    let without = report_json(root.path(), &[NO_TIMINGS]);
    assert_eq!(without["timings"]["present"], false);
    // The envelope views still come out of events.jsonl alone.
    assert_eq!(without["time"]["envelope_total_ms"], 200);
    assert_eq!(without["run"]["status"], "PASS");
}

#[test]
fn sim_report_reports_an_interrupted_run_as_incomplete_without_a_wrong_denominator() {
    let root = history();
    let value = report_json(root.path(), &[INTERRUPTED]);

    let header = &value["run"];
    assert_eq!(header["status"], "INCOMPLETE");
    assert_eq!(header["complete"], false);
    assert_eq!(header["run_end_present"], false);
    // No `run_end` means no authoritative wall time; the observed span is
    // reported under its own name instead of masquerading as one.
    assert!(header["wall_ms"].is_null());
    assert_eq!(header["observed_span_ms"], 800);
    assert_eq!(header["planned_entries"], 2);
    assert_eq!(header["finalized_entries"], 1);
    assert_eq!(header["revision"], Value::Null);

    let time = &value["time"];
    assert_eq!(time["entries_with_envelope"], 1);
    assert_eq!(time["envelope_total_ms"], 400);
    assert_eq!(time["entries_started_without_end"], 1);
    assert_eq!(time["workers_without_end"], 1);

    let text = report(root.path(), &[INTERRUPTED]);
    assert!(
        text.contains("INCOMPLETE"),
        "the human report must say so: {text}"
    );
}

#[test]
fn sim_report_counts_recoveries_and_classifies_their_causes() {
    let root = history();
    let value = report_json(root.path(), &[RECOVERED]);

    assert_eq!(value["run"]["recoveries"], 2);
    assert_eq!(value["run"]["boots"], 2);
    let causes = value["recoveries"]["by_cause"].as_array().unwrap();
    assert_eq!(causes.len(), 1);
    assert_eq!(causes[0]["count"], 2);
    assert_eq!(causes[0]["cause"], "destroyed native resources");

    // A hot-reload lane is its own isolation class.
    let classes: Vec<&str> = value["lanes"]["by_isolation_class"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["class"].as_str().unwrap())
        .collect();
    assert_eq!(classes, vec!["hot-reload"]);
}

#[test]
fn sim_report_widens_a_worker_span_over_entry_lines_drained_after_it_was_reaped() {
    let root = history();
    let value = report_json(root.path(), &[DRAINED_LATE]);

    let time = &value["time"];
    // The child was reaped at 500 ms but its buffered `entry_end` is at 750 ms.
    assert_eq!(time["child_lifetime_total_ms"], 400);
    assert_eq!(time["drain_overhang_ms"], 250);
    assert_eq!(time["worker_span_total_ms"], 650);
    assert_eq!(time["envelope_total_ms"], 550);
    assert_eq!(time["startup_latency_ms"], 100);
    assert_eq!(time["remainder_ms"], 0);
    assert_eq!(time["entries_with_envelope"], 1);
}

#[test]
fn sim_report_defaults_to_the_most_recent_retained_run() {
    let root = history();
    let value = report_json(root.path(), &[]);
    assert_eq!(value["run"]["run_id"], DRAINED_LATE);
}

#[test]
fn sim_report_classifies_out_interpolation_lanes() {
    let root = history();
    let value = report_json(root.path(), &[NO_TIMINGS]);
    let classes: Vec<&str> = value["lanes"]["by_isolation_class"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["class"].as_str().unwrap())
        .collect();
    assert_eq!(classes, vec!["$OUT interpolation"]);
}

#[test]
fn sim_report_pretty_output_never_calls_an_envelope_pure_sim_body_time() {
    let root = history();
    let text = report(root.path(), &[COMPLETE]);
    assert!(text.contains("startup latency"), "{text}");
    assert!(
        text.contains("native reset"),
        "the envelope qualifier must be printed: {text}"
    );
    assert!(
        text.contains("not a measure of sim-body work alone"),
        "{text}"
    );
    // Guards against a future pass relabelling the envelope into the nicer
    // number the qualifier exists to prevent.
    let lowered = text.to_lowercase();
    assert!(
        !lowered.contains("sim-body time") && !lowered.contains("sim body time"),
        "the report must not relabel the envelope as sim-body time: {text}"
    );
}

#[test]
fn sim_report_compares_one_named_entry_across_retained_runs() {
    let root = history();
    let value = report_json(root.path(), &["--sim-entry", "alpha"]);
    let rows = value["rows"].as_array().unwrap();
    let ids: Vec<&str> = rows.iter().map(|r| r["run_id"].as_str().unwrap()).collect();
    assert_eq!(
        ids,
        vec![COMPLETE, INTERRUPTED, RECOVERED, NO_TIMINGS, DRAINED_LATE],
        "every retained run holding the name contributes one row, oldest first"
    );
    assert_eq!(rows[0]["entry_elapsed_ms"], 1000);
    assert_eq!(rows[0]["verdict"], "PASS");
    assert_eq!(rows[0]["status"], "FINAL");
    assert_eq!(rows[0]["source_revision"], "abc1234def");
    // The interrupted run started `alpha` and finished it; `beta` is the open
    // one, so `alpha` is still FINAL there.
    assert_eq!(rows[1]["entry_elapsed_ms"], 400);
    assert_eq!(rows[1]["status"], "FINAL");
    assert_eq!(rows[2]["entry_elapsed_ms"], 200);
    assert_eq!(rows[3]["entry_elapsed_ms"], 200);
    // The buffered envelope drained after the worker was reaped still measures.
    assert_eq!(rows[4]["entry_elapsed_ms"], 550);
}

#[test]
fn sim_report_rejects_an_unknown_run_id_without_touching_the_history() {
    let root = history();
    let output = crate::command::rock_bin()
        .arg("sim-report")
        .arg("--sim-log-dir")
        .arg(root.path())
        .arg("1600000000000-1-0")
        .output()
        .expect("run sim-report");
    assert!(!output.status.success(), "an unknown run id must not pass");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("1600000000000-1-0"), "{stderr}");
    // Read-only: the five fixture runs are untouched and nothing was added.
    let mut names: Vec<String> = std::fs::read_dir(root.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec![COMPLETE, INTERRUPTED, RECOVERED, NO_TIMINGS, DRAINED_LATE]
    );
}
