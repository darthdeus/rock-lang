//! Every dev-loop command appends one JSON line to the invocation log, so
//! edit-to-feedback times can be mined (`tools/loop-stats.py`). Drives the real
//! CLI: the record is written at process exit, and `rock test` leaves through
//! `process::exit` on failure, which nothing below the binary can observe.

use std::path::Path;
use std::process::Command;

fn run_test(dir: &Path, fixture: &Path, log_env: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_rock"))
        .args(["test", fixture.to_str().unwrap()])
        .current_dir(dir)
        .env("ROCK_INVOCATION_LOG", log_env)
        .env("ROCK_CHANGE_ID", "fixture-change")
        .env("ROCK_JOB_ID", "fixture-job")
        .env_remove("ROCK_INVOCATION_PARENT")
        .output()
        .expect("run rock test")
}

fn fixture(dir: &Path, failing: bool) -> std::path::PathBuf {
    let path = dir.join(if failing { "fails.rock" } else { "passes.rock" });
    let body = if failing {
        "@test(returns = 4) fn four() -> int { return 4 }\n@test(returns = 5) fn wrong() -> int { return 6 }\n"
    } else {
        "@test(returns = 4) fn four() -> int { return 4 }\n@test(returns = 9) fn nine() -> int { return 9 }\n"
    };
    std::fs::write(&path, body).expect("write fixture");
    path
}

fn records(log: &Path) -> Vec<serde_json::Value> {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    text.lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("bad line {line:?}: {e}")))
        .collect()
}

#[test]
fn rock_test_writes_exactly_one_well_formed_record() {
    let dir = tempfile::tempdir().expect("tempdir");
    let log = dir.path().join("state").join("invocations.jsonl");
    let pass = fixture(dir.path(), false);
    let out = run_test(dir.path(), &pass, log.to_str().unwrap());
    assert!(out.status.success(), "stdout:{}\nstderr:{}",
        String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));

    let recs = records(&log);
    assert_eq!(recs.len(), 1, "one record per invocation: {recs:?}");
    let r = &recs[0];
    assert_eq!(r["schema"], 1);
    assert_eq!(r["kind"], "test");
    assert_eq!(r["exit_code"], 0);
    assert_eq!(r["argv"][1], "test");
    assert!(r["pid"].as_u64().is_some());
    assert!(r["ppid"].as_u64().is_some());
    assert!(r["host"].as_str().is_some_and(|h| !h.is_empty()));
    assert!(r["start_utc"].as_str().is_some_and(|s| s.ends_with('Z')));
    assert!(r["end_utc"].as_str().is_some_and(|s| s.ends_with('Z')));
    assert!(r["wall_ms"].as_u64().is_some());
    assert!(r["cwd"].as_str().is_some());
    for key in ["repo_root", "head", "branch", "dirty"] {
        assert!(r.get(key).is_some(), "missing {key}: {r}");
    }
    assert_eq!(r["env"]["ROCK_CHANGE_ID"], "fixture-change");
    assert_eq!(r["env"]["ROCK_JOB_ID"], "fixture-job");
    assert_eq!(r["counts"]["tests_total"], 2);
    assert_eq!(r["counts"]["tests_passed"], 2);
    assert_eq!(r["counts"]["tests_failed"], 0);
    assert_eq!(r["counts"]["units_selected"], 1);
    assert!(r["phases_ms"]["units"].as_u64().is_some(), "{r}");

    // A failing run leaves through `process::exit(1)`; it still records once,
    // with its exit code and failure counts.
    let fail = fixture(dir.path(), true);
    let out = run_test(dir.path(), &fail, log.to_str().unwrap());
    assert!(!out.status.success());
    let recs = records(&log);
    assert_eq!(recs.len(), 2, "{recs:?}");
    assert_eq!(recs[1]["exit_code"], 1);
    assert_eq!(recs[1]["counts"]["tests_failed"], 1);
}

#[test]
fn invocation_log_off_writes_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pass = fixture(dir.path(), false);
    let out = run_test(dir.path(), &pass, "off");
    assert!(out.status.success());
    // `off` is a switch, not a relative file name.
    assert!(!dir.path().join("off").exists());
    let out = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args(["test", pass.to_str().unwrap()])
        .current_dir(dir.path())
        .env("ROCK_INVOCATION_LOG", "off")
        .env("XDG_STATE_HOME", dir.path().join("xdg"))
        .env_remove("ROCK_INVOCATION_PARENT")
        .output()
        .expect("run rock test");
    assert!(out.status.success());
    assert!(!dir.path().join("xdg").exists(), "off must not touch the default log");
}

#[test]
fn default_log_respects_xdg_state_home() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pass = fixture(dir.path(), false);
    let out = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args(["test", pass.to_str().unwrap()])
        .current_dir(dir.path())
        .env_remove("ROCK_INVOCATION_LOG")
        .env_remove("ROCK_INVOCATION_PARENT")
        .env("XDG_STATE_HOME", dir.path().join("xdg"))
        .output()
        .expect("run rock test");
    assert!(out.status.success());
    let recs = records(&dir.path().join("xdg/rock/invocations.jsonl"));
    assert_eq!(recs.len(), 1, "{recs:?}");
}
