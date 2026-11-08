//! The `perf/` tracker reads `backend-stats` and `incremental-stats` as JSON.
use std::path::Path;
use std::process::{Command, Output};

const MAIN: &str = r#"
mod main;

use super::util::bump;

fn main() {
    print("{bump(41)}")
}
"#;

const UTIL: &str = r#"
mod util;

pub fn bump(n: int) -> int {
    let step = 1
    return n + step
}

pub fn twice(n: int) -> int {
    return bump(bump(n))
}
"#;

fn project() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("stick.toml"),
        "[package]\nname = \"perfstats\"\n\n[language]\nmodules = \"v1\"\n",
    )
    .unwrap();
    std::fs::write(root.path().join("main.rock"), MAIN).unwrap();
    std::fs::write(root.path().join("util.rock"), UTIL).unwrap();
    root
}

fn invoke(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rock"))
        .current_dir(root)
        .args(args)
        .env("ROCK_DIR", root)
        .env("ROCK_INVOCATION_LOG", "off")
        .env_remove("ROCK_STDLIB_DIR")
        .output()
        .expect("start rock")
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
    assert_eq!(report["bone"]["ok"], true);
    assert_eq!(report["skull"]["ok"], true);
    assert!(report["bone"]["semantic_ms"].as_f64().unwrap() > 0.0);
    assert!(report["skull"]["emit_ms"].as_f64().unwrap() > 0.0);
    let phases = report["bone"]["phases"].as_object().unwrap();
    assert!(phases.contains_key("infer functions"), "{phases:?}");
    let setup = report["skull"]["setup_phases"].as_object().unwrap();
    assert!(setup.contains_key("graph"), "{setup:?}");
    let emit = report["skull"]["emit_phases"].as_object().unwrap();
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
