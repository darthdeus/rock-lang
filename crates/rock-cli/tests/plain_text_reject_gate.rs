//! Plain family-aware text rejects one invalid admission per bounded process.
//!
//! A late missing codepoint must reject before any earlier glyph is emitted,
//! and the API must not silently choose one of `font:` / `family:` when both
//! are supplied. An empty call still validates every family handle, including
//! a stale fallback.

use std::process::Command;
use crate::stone_harness::{HeavyCommandExt, SpawnBudget};

fn rock_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_rock"))
}

fn repo_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn run_frame(subject: Option<&str>) -> (String, bool) {
    let out_dir = tempfile::tempdir().expect("tempdir");
    let mut cmd = Command::new(rock_bin());
    cmd.current_dir(repo_root())
        .args(["frame", "games/testbed", "-n", "3"])
        .arg("--output")
        .arg(out_dir.path().join("main.lua"))
        .env("ROCK_FRAME_TIMEOUT_SECS", "120");
    if let Some(s) = subject {
        cmd.env("PLAIN_REJECT", s);
    }
    let out = cmd.output_with_deadline().expect("spawn rock frame");
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (text, out.status.success())
}

fn assert_rejects(subject: &str, needle: &str) {
    let (text, ok) = run_frame(Some(subject));
    assert!(
        text.contains(needle),
        "{subject}: the rejection did not name its cause (wanted {needle:?}):\n{text}"
    );
    assert!(!ok, "{subject}: the rejected plain call must fail the run:\n{text}");
}

#[test]
fn supplying_font_and_family_rejects() {
    let _spawn_budget = SpawnBudget::for_test(120);
    assert_rejects("both_font_family", "either font or family");
}

#[test]
fn a_late_missing_codepoint_rejects_before_partial_emission() {
    let _spawn_budget = SpawnBudget::for_test(120);
    assert_rejects("missing_codepoint_late", "codepoint 128512 at offset 1");
}

#[test]
fn a_stale_fallback_rejects_for_empty_plain_text() {
    let _spawn_budget = SpawnBudget::for_test(120);
    assert_rejects("empty_stale_fallback", "stale font handle");
}

#[test]
fn an_uninjected_run_still_passes() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let (text, ok) = run_frame(None);
    assert!(ok, "the uninjected control run must pass:\n{text}");
    assert!(
        !text.contains("frame(s) raised during run"),
        "the control raised, so the injected arms prove nothing:\n{text}"
    );
}
