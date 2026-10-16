//! The opt-in sim allocation census must retain every bounded fixture result.
//!
//! The fixture deliberately has one allocating step and one scalar control.
//! The test gates the reporting contract and exact frame population; it does
//! not turn the noisy allocation magnitudes into gameplay thresholds.

use serde_json::Value;
use std::path::{Path, PathBuf};
use crate::stone_harness::{HeavyCommandExt, SpawnBudget};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn run_census(output: &Path) -> std::process::Output {
    crate::command::rock_bin()
        .current_dir(repo_root())
        .args([
            "sim",
            "--allocation-census",
            output.to_str().expect("UTF-8 census path"),
            "testbed",
            "testbed::sim_allocation_allocating",
            "testbed::sim_allocation_control",
        ])
        .env("ROCK_SIM_TIMEOUT_SECS", "60")
        .output_with_deadline()
        .expect("spawn allocation census")
}

#[test]
fn census_reports_exact_fixture_frames_and_keeps_both_arms() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let dir = tempfile::tempdir().expect("census artifact directory");
    let output_path = dir.path().join("sim-allocation-census.json");
    let run = run_census(&output_path);
    let stdout = String::from_utf8_lossy(&run.stdout);
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(
        run.status.success(),
        "census command failed:\nstdout={stdout}\nstderr={stderr}"
    );

    let artifact: Value = serde_json::from_slice(
        &std::fs::read(&output_path).expect("census artifact exists"),
    )
    .expect("valid census JSON");
    assert_eq!(artifact["schema"], "rock.sim-allocation-census.v1");
    assert_eq!(artifact["examined"], 2);
    assert_eq!(artifact["sensor"]["accounting"], "gross_lua_allocator_requests");
    assert_eq!(artifact["sensor"]["jit"], true);
    assert_eq!(artifact["sensor"]["trace_observer"], false);
    assert_eq!(artifact["sensor"]["text_layout_sim_probes"], false);
    assert_eq!(artifact["sensor"]["imgui_sim_probes"], false);
    assert_eq!(artifact["sensor"]["final_frame"], "after_deferred_stop_sim");
    let records = artifact["records"].as_array().expect("records array");
    assert_eq!(records.len(), 2, "examined population must match records");

    let mut allocating_measured_kb = None;
    let mut allocating_calls_per_frame = None;
    let mut control_calls_per_frame = None;
    for record in records {
        assert_eq!(record["status"], "PASS", "fixture record: {record}");
        assert_eq!(record["measured_frames"], 4, "fixture denominator: {record}");
        assert_eq!(record["stop_sim_reached"], true);
        assert_eq!(record["rendering_enabled"], false);
        assert_eq!(record["rendering_skipped"], true);
        assert!(record["process_id"].as_u64().unwrap_or(0) > 0, "{record}");
        assert!(record["build_sha"].as_str().is_some_and(|sha| !sha.is_empty()), "{record}");
        assert!(record["allocation_calls_per_frame"].is_number(), "{record}");
        assert!(record["gross_kb_per_frame"].is_number(), "{record}");
        if record["sim_name"] == "testbed::sim_allocation_allocating" {
            allocating_measured_kb = record["measured_gross_kb"].as_f64();
            allocating_calls_per_frame = record["allocation_calls_per_frame"].as_f64();
        } else if record["sim_name"] == "testbed::sim_allocation_control" {
            control_calls_per_frame = record["allocation_calls_per_frame"].as_f64();
        }
    }
    assert!(
        allocating_measured_kb.is_some_and(|kb| kb > 0.0),
        "the allocating fixture must reach the gross allocator oracle"
    );
    let allocating_calls_per_frame = allocating_calls_per_frame.expect("allocating call rate");
    let control_calls_per_frame = control_calls_per_frame.expect("control call rate");
    assert!(
        allocating_calls_per_frame > control_calls_per_frame + 50.0,
        "vector boundary must clearly exceed scalar control: allocating={allocating_calls_per_frame} control={control_calls_per_frame}"
    );

    assert_eq!(artifact["rankings"]["by_gross_kb_per_frame"].as_array().unwrap().len(), 2);
    assert_eq!(
        artifact["rankings"]["by_allocation_calls_per_frame"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}
