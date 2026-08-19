//! Deterministic Lost/Outdated recovery evidence on macOS WGPU.

use std::path::{Path, PathBuf};
use std::process::Command;

fn rock_command() -> Command {
    if cfg!(target_os = "macos") {
        let mut command = Command::new("caffeinate");
        command.args(["-du"]);
        command.arg(rock_bin());
        command
    } else {
        Command::new(rock_bin())
    }
}

fn rock_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_rock"))
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

#[test]
fn injected_lost_and_outdated_each_recover_before_the_next_present() {
    if !cfg!(target_os = "macos") {
        return;
    }
    for injected in ["lost", "outdated"] {
        let output_dir = tempfile::tempdir().expect("temporary output directory");
        let mut command = rock_command();
        command
            .current_dir(repo_root())
            .args(["frame", "mag", "--renderer", "wgpu", "-n", "30"])
            .arg("--output")
            .arg(output_dir.path().join("main.lua"))
            .env("ROCK_FRAME_TIMEOUT_SECS", "120")
            .env("STONE_WGPU_TEST_SURFACE_ERROR", injected);
        let result = command.output().expect("spawn deterministic recovery run");
        let mut text = String::from_utf8_lossy(&result.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&result.stderr));
        let summary = text
            .lines()
            .find(|line| line.contains("[RUN] bounded"))
            .unwrap_or("<missing bounded summary>");
        assert!(
            result.status.success(),
            "injected {injected} must recover and finish:
{text}"
        );
        assert!(
            summary.contains("completed/requested=30/30")
                && summary.contains("frame_errors=0")
                && summary.contains("wgpu_errors=0")
                && summary.contains("wgpu_presents=30")
                && summary.contains("wgpu_recoveries=1")
                && summary.contains("gl_swaps=0"),
            "injected {injected} must be visible in the clean final summary: {summary}"
        );
    }
}
