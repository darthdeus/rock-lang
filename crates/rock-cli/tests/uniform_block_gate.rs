//! A uniform block drawn with but never filled must FAIL a bounded run.
//!
//! `unfilled_block_gate.rock` proves the COUNTER, but it fills the block before
//! `stop_sim()` so the run ends clean -- it cannot show that a non-zero count
//! actually fails anything. This asserts the process-level contract:
//! `runner.rock` exits 3 on `stone_unfilled_uniform_block_count() > 0`, the same
//! shape as `shader_failure_gate.rs`.
//!
//! `STONE_UNIFORM_BLOCK_FORCE_UNFILLED=1` drops the `uploaded` stamp, so the
//! REAL reporting path runs over blocks that are genuinely filled -- a missing
//! call site produces exactly this state. The control run (same command, no
//! injection) is what keeps this from passing for the wrong reason: match3 fills
//! every block it registers, so it must exit 0.

use std::process::Command;

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

/// `rock frame games/match3` for 5 frames, optionally with the unfilled
/// injection. Returns (combined output, exit-status-success).
fn run_frame(force_unfilled: bool) -> (String, bool) {
    let out_dir = tempfile::tempdir().expect("tempdir");
    let mut cmd = Command::new(rock_bin());
    cmd.current_dir(repo_root())
        .args(["frame", "games/match3", "-n", "5"])
        .arg("--output")
        .arg(out_dir.path().join("main.lua"))
        .env("ROCK_FRAME_TIMEOUT_SECS", "120");
    if force_unfilled {
        cmd.env("STONE_UNIFORM_BLOCK_FORCE_UNFILLED", "1");
    }
    let out = cmd.output().expect("spawn rock frame");
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (text, out.status.success())
}

#[test]
fn unfilled_uniform_block_fails_a_bounded_run() {
    let (control, control_ok) = run_frame(false);
    assert!(
        control_ok,
        "control run must succeed -- match3 fills every block it registers.\n{control}"
    );
    assert!(
        !control.contains("never filled"),
        "control run reported an unfilled block, so the injected run below proves nothing.\n{control}"
    );

    let (injected, injected_ok) = run_frame(true);
    assert!(
        !injected_ok,
        "a bounded run with an unfilled uniform block must exit nonzero.\n{injected}"
    );
    assert!(
        injected.contains("never filled"),
        "the run must say WHICH block went unfilled, not just fail.\n{injected}"
    );
}
