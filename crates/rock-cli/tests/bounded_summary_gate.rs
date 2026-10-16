//! The final bounded-run summary must describe exactly the requested work.

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

fn run_frame(renderer: &str, frames: usize) -> (String, bool) {
    let output_dir = tempfile::tempdir().expect("temporary output directory");
    let mut command = rock_command();
    command
        .current_dir(repo_root())
        .args([
            "frame",
            "games/wgpu_renderer",
            "--renderer",
            renderer,
            "-n",
            &frames.to_string(),
        ])
        .arg("--output")
        .arg(output_dir.path().join("main.lua"))
        .env_remove("ROCK_FRAME_TIMEOUT_SECS")
        .env("FPS_LOG", "0");
    let result = command.output().expect("spawn bounded frame run");
    let mut text = String::from_utf8_lossy(&result.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&result.stderr));
    (text, result.status.success())
}

#[test]
fn final_bounded_summaries_have_exact_30_and_600_frame_populations() {
    for renderer in ["gl", "wgpu"] {
        for frames in [30, 600] {
            let (text, ok) = run_frame(renderer, frames);
            let summary = text
                .lines()
                .find(|line| line.contains("[RUN] bounded"))
                .unwrap_or("<missing bounded summary>");
            assert!(
                ok,
                "{renderer} {frames}-frame run must succeed; summary={summary}\n{text}"
            );
            assert!(
                summary.contains(&format!(
                    "renderer={renderer} completed/requested={frames}/{frames}"
                )),
                "the final summary must report exactly the requested population; summary={summary}"
            );
            assert!(
                summary.contains("frame_errors=0") && summary.contains("wgpu_errors=0"),
                "the exact-count arm must also be clean; summary={summary}"
            );
            if renderer == "gl" {
                assert!(
                    summary.contains(&format!("gl_swaps={frames}")),
                    "GL must swap exactly once per requested frame; summary={summary}"
                );
            } else {
                assert!(
                    summary.contains(&format!("wgpu_presents={frames}")),
                    "WGPU must present exactly once per requested frame; summary={summary}"
                );
                assert!(
                    summary.contains("gl_swaps=0"),
                    "selected WGPU must not use GL swaps; summary={summary}"
                );
            }
        }
    }
}
