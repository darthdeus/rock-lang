//! A texture slot drawn with but never marked filled must fail a bounded run.
//!
//! The injection preserves the real texture bindings and suppresses only their
//! filled stamps, so pixel output cannot make the failure pass accidentally.

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

fn run_frame(force_unfilled: bool) -> (String, bool) {
    let out_dir = tempfile::tempdir().expect("tempdir");
    let mut cmd = Command::new(rock_bin());
    cmd.current_dir(repo_root())
        .args(["frame", "games/match3", "-n", "5"])
        .arg("--output")
        .arg(out_dir.path().join("main.lua"))
        .env("ROCK_FRAME_TIMEOUT_SECS", "120");
    if force_unfilled {
        cmd.env("STONE_TEXTURE_SLOT_FORCE_UNFILLED", "1");
    }
    let out = cmd.output().expect("spawn rock frame");
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (text, out.status.success())
}

fn run_sim(force_unfilled: bool) -> (String, bool) {
    let mut cmd = Command::new(rock_bin());
    cmd.current_dir(repo_root())
        .args(["sim", "bitgun", "tonemap_input_order"])
        .env("ROCK_FRAME_TIMEOUT_SECS", "120");
    if force_unfilled {
        cmd.env("STONE_TEXTURE_SLOT_FORCE_UNFILLED", "1");
    }
    let out = cmd.output().expect("spawn rock sim");
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (text, out.status.success())
}

#[test]
fn unfilled_texture_slot_fails_a_bounded_run() {
    let (control, control_ok) = run_frame(false);
    assert!(control_ok, "control run must succeed.\n{control}");
    assert!(
        !control.contains("texture slot(s) drawn with but never filled"),
        "control run reported an unfilled texture slot.\n{control}"
    );

    let (injected, injected_ok) = run_frame(true);
    assert!(
        !injected_ok,
        "a bounded run with an unfilled texture slot must exit nonzero.\n{injected}"
    );
    assert!(
        injected.contains("texture slot(s) drawn with but never filled"),
        "the run must report the texture-slot failure count.\n{injected}"
    );
}

#[test]
fn unfilled_texture_slot_fails_a_sim() {
    let (control, control_ok) = run_sim(false);
    assert!(control_ok, "control sim must succeed.\n{control}");

    let (injected, injected_ok) = run_sim(true);
    assert!(
        !injected_ok,
        "a sim with an unfilled texture slot must fail.\n{injected}"
    );
    assert!(
        injected.contains("texture slots drawn with but never filled"),
        "stop_sim must report its texture-slot invariant.\n{injected}"
    );
}
