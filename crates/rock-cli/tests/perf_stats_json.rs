//! `backend-stats` and `incremental-stats` report their timings as JSON.
use crate::support::invoke;
use std::process::Output;

fn project() -> tempfile::TempDir {
    crate::support::two_module_package("perfstats")
}

fn json_stdout(out: &Output) -> serde_json::Value {
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "exit {:?}\nstdout:\n{stdout}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("stdout is not JSON ({e}):\n{stdout}"))
}

#[test]
fn backend_stats_reports_phase_means_as_json() {
    let root = project();
    let out = invoke(
        root.path(),
        &[
            "backend-stats",
            ".",
            "--iters",
            "2",
            "--warmup",
            "0",
            "--format",
            "json",
        ],
    );
    let report = json_stdout(&out);
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

#[test]
fn incremental_stats_times_body_and_signature_edits_as_json() {
    let root = project();
    let out = invoke(
        root.path(),
        &[
            "incremental-stats",
            ".",
            "--iters",
            "2",
            "--warmup",
            "1",
            "--format",
            "json",
        ],
    );
    let report = json_stdout(&out);
    assert!(report["init_ms"].as_f64().unwrap() > 0.0);
    let edits = report["edits"].as_array().unwrap();
    let body = edits
        .iter()
        .find(|e| e["kind"] == "body" && e["name"].as_str().unwrap().ends_with("::bump"))
        .unwrap_or_else(|| panic!("no body edit of bump: {edits:?}"));
    assert_eq!(body["reload"], "incremental");
    assert_eq!(body["samples_ms"].as_array().unwrap().len(), 2);
    assert!(body["mean_ms"].as_f64().unwrap() > 0.0);
    assert!(body["emit_mean_ms"].as_f64().unwrap() >= 0.0);
    assert!(body["overlay_bytes"].as_u64().unwrap() > 0);
    let signature = edits
        .iter()
        .find(|e| e["kind"] == "signature" && e["name"].as_str().unwrap().ends_with("::bump"))
        .unwrap_or_else(|| panic!("no signature edit of bump: {edits:?}"));
    assert_eq!(signature["reload"], "full");
    assert!(signature["mean_ms"].as_f64().unwrap() > 0.0);
}

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

/// The reloaded project is the production build: a test-only file is no
/// source of it, however large its bodies are.
#[test]
fn incremental_stats_edits_only_sources_of_the_project_it_reloads() {
    let root = project();
    std::fs::create_dir(root.path().join("tests")).unwrap();
    std::fs::write(root.path().join("tests/sim.rock"), SIM).unwrap();
    let out = invoke(
        root.path(),
        &[
            "incremental-stats",
            ".",
            "--iters",
            "1",
            "--warmup",
            "0",
            "--format",
            "json",
        ],
    );
    let report = json_stdout(&out);
    let edits = report["edits"].as_array().unwrap();
    assert!(edits.iter().any(|e| e["kind"] == "body"), "{edits:?}");
    for edit in edits {
        assert!(edit["error"].is_null(), "{edit:?}");
        assert!(
            !edit["file"].as_str().unwrap().ends_with("tests/sim.rock"),
            "{edit:?}"
        );
    }
}
