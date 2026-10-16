//! A shader that fails to compile must FAIL a bounded run.
//!
//! A failed compile sets no GL error, so `stone_gl_error_count()` stays 0 and
//! the frame loop draws on with a 0 handle: the run printed `[SHADER] ... failed`
//! 30 times and still reported `30 frames executed successfully` with exit 0, so
//! `just test-games` said `ok`. The runner now exits 3 on
//! `stone_shader_error_count() > 0` for bounded runs (frame/perf/sims).
//!
//! `STONE_SHADER_FORCE_FAIL=1` appends `#error` to every shader source, so the
//! REAL compile-status failure path runs rather than a hand-faked one. The
//! control run (same command, no injection) is what keeps this from passing for
//! the wrong reason.

use crate::stone_harness::{HeavyCommandExt, SpawnBudget};

fn repo_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

/// `rock frame games/match3` for 5 frames, optionally with the shader-failure
/// injection. Returns (combined output, exit-status-success).
fn run_frame(force_shader_failure: bool) -> (String, bool) {
    let out_dir = tempfile::tempdir().expect("tempdir");
    let mut cmd = crate::command::rock_bin();
    cmd.current_dir(repo_root())
        .args(["frame", "games/match3", "-n", "5"])
        .arg("--output")
        .arg(out_dir.path().join("main.lua"))
        .env("ROCK_FRAME_TIMEOUT_SECS", "120");
    if force_shader_failure {
        cmd.env("STONE_SHADER_FORCE_FAIL", "1");
    }
    let out = cmd.output_with_deadline().expect("spawn rock frame");
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (text, out.status.success())
}

#[test]
fn shader_compile_failure_fails_a_bounded_run() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let (text, ok) = run_frame(true);
    assert!(
        text.contains("shader compilation failed"),
        "injection did not reach the real compile path:\n{text}"
    );
    assert!(
        text.contains("shader compile/link failures during run"),
        "the run did not report the failure count:\n{text}"
    );
    assert!(!ok, "a failed shader compile must fail the run:\n{text}");
}

/// Without the injection the same command must pass -- otherwise the gate above
/// would hold on a broken game / missing GL rather than on the shader failure.
#[test]
fn clean_run_still_passes() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let (text, ok) = run_frame(false);
    assert!(ok, "the control run must pass:\n{text}");
    assert!(
        !text.contains("shader compile/link failures during run"),
        "a clean run must report no shader failures:\n{text}"
    );
}
