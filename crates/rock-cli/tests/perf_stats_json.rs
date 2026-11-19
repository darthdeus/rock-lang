//! `backend-stats` and `incremental-stats` report their timings as JSON.
use crate::support::{invoke, success};
use std::path::Path;

fn project() -> tempfile::TempDir {
    crate::support::two_module_package("perfstats")
}

fn json_report(root: &Path, command: &str, iters: &str, warmup: &str) -> serde_json::Value {
    let stdout = success(invoke(
        root,
        &[
            command, ".", "--iters", iters, "--warmup", warmup, "--format", "json",
        ],
    ));
    serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("stdout is not JSON ({e}):\n{stdout}"))
}

#[test]
fn backend_stats_reports_phase_means_as_json() {
    let root = project();
    let report = json_report(root.path(), "backend-stats", "2", "0");
    assert_eq!(report["iters"], 2);
    assert_eq!(report["files"], 2);
    assert_eq!(report["front"]["ok"], true);
    assert_eq!(report["emit"]["ok"], true);
    assert!(report["front"]["semantic_ms"].as_f64().unwrap() > 0.0);
    assert!(report["emit"]["emit_ms"].as_f64().unwrap() > 0.0);
    let phases = report["front"]["phases"].as_object().unwrap();
    assert!(phases.contains_key("infer functions"), "{phases:?}");
    let setup = report["emit"]["setup_phases"].as_object().unwrap();
    assert!(setup.contains_key("graph"), "{setup:?}");
    let emit = report["emit"]["emit_phases"].as_object().unwrap();
    assert!(emit.contains_key("lower hir->mir"), "{emit:?}");
}

/// Larger than any body of the project, so an edit that picked test-only
/// sources would pick this one.
const SIM: &str = r#"
@test fn the_largest_body_of_the_project() {
    let a = 1
    let b = a + 1
    let c = b + 1
    let d = c + 1
    let e = d + 1
    let f = e + 1
    assert(f == 6, "six")
}
"#;

#[test]
fn incremental_stats_times_body_and_signature_edits_of_production_sources_as_json() {
    let root = project();
    crate::support::write_files(root.path(), &[("tests/sim.rock", SIM)]);
    let report = json_report(root.path(), "incremental-stats", "2", "1");
    assert!(report["init_ms"].as_f64().unwrap() > 0.0);
    let edits = report["edits"].as_array().unwrap();
    for edit in edits {
        assert!(edit["error"].is_null(), "{edit:?}");
        assert!(
            !edit["file"].as_str().unwrap().ends_with("tests/sim.rock"),
            "{edit:?}"
        );
    }
    let edit_of_bump = |kind: &str| {
        edits
            .iter()
            .find(|e| e["kind"] == kind && e["name"].as_str().unwrap().ends_with("::bump"))
            .unwrap_or_else(|| panic!("no {kind} edit of bump: {edits:?}"))
    };
    let body = edit_of_bump("body");
    assert_eq!(body["reload"], "incremental");
    assert_eq!(body["samples_ms"].as_array().unwrap().len(), 2);
    assert!(body["mean_ms"].as_f64().unwrap() > 0.0);
    assert!(body["emit_mean_ms"].as_f64().unwrap() >= 0.0);
    assert!(body["overlay_bytes"].as_u64().unwrap() > 0);
    let signature = edit_of_bump("signature");
    assert_eq!(signature["reload"], "full");
    assert!(signature["mean_ms"].as_f64().unwrap() > 0.0);
}
