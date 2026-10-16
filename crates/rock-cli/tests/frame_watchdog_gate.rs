//! A slow selected-WGPU child that keeps completing frames must not be killed
//! by a total wall-clock budget sized from the requested frame count.

use std::path::{Path, PathBuf};
use std::process::Command;

fn rock_command() -> Command {
    if cfg!(target_os = "macos") {
        let mut command = crate::command::rock("caffeinate");
        command.args(["-du"]);
        command.arg(std::path::PathBuf::from(env!("CARGO_BIN_EXE_rock")));
        command
    } else {
        crate::command::rock_bin()
    }
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn run_wgpu_frame(frames: &str, delay_ms: &str) -> (String, bool) {
    let output_dir = tempfile::tempdir().expect("temporary output directory");
    let mut command = rock_command();
    command
        .current_dir(repo_root())
        .args(["frame", "games/wgpu_renderer", "--renderer", "wgpu", "-n", frames])
        .arg("--output")
        .arg(output_dir.path().join("main.lua"))
        .env_remove("ROCK_FRAME_TIMEOUT_SECS")
        .env("STONE_FRAME_DELAY_MS", delay_ms)
        .env("FPS_LOG", "0");
    let result = command.output().expect("spawn selected-WGPU frame run");
    let mut text = String::from_utf8_lossy(&result.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&result.stderr));
    (text, result.status.success())
}

#[test]
fn a_slow_progressing_wgpu_run_is_not_killed_by_total_budget() {
    let output_dir = tempfile::tempdir().expect("temporary output directory");
    let mut command = rock_command();
    command
        .current_dir(repo_root())
        .args(["frame", "games/wgpu_renderer", "--renderer", "wgpu", "-n", "600"])
        .arg("--output")
        .arg(output_dir.path().join("main.lua"))
        .env_remove("ROCK_FRAME_TIMEOUT_SECS")
        .env("STONE_FRAME_DELAY_MS", "60")
        .env("FPS_LOG", "0");
    let result = command.output().expect("spawn selected-WGPU frame run");
    let mut text = String::from_utf8_lossy(&result.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&result.stderr));

    assert!(
        text.contains("ROCK_FRAME_PROGRESS renderer=wgpu"),
        "the child must report selected-WGPU progress before the watchdog verdict:\n{text}"
    );
    assert!(
        text.contains("completed=600 requested=600"),
        "the child must reach all requested frames:\n{text}"
    );
    assert!(
        result.status.success(),
        "a child making legitimate progress must survive its total-budget crossing:\n{text}"
    );
    assert!(
        text.contains("600 frames executed successfully"),
        "the CLI must report the completed denominator:\n{text}"
    );
}

#[test]
fn a_stalled_wgpu_run_reports_the_last_progress_and_still_times_out() {
    let (text, ok) = run_wgpu_frame("2", "6000");
    assert!(!ok, "a frame that stops progressing must fail:\n{text}");
    assert!(
        text.contains("renderer=wgpu, completed/requested=1/2"),
        "timeout diagnostics must identify the renderer and denominator:\n{text}"
    );
    assert!(
        text.contains("progress_stopped=true"),
        "timeout diagnostics must say that frame progress stopped:\n{text}"
    );
    assert!(
        text.contains("timed out after 5s"),
        "the default inactivity guard must remain bounded:\n{text}"
    );
}
