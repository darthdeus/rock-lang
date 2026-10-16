//! Deterministic macOS WGPU surface lifecycle evidence.

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

#[test]
fn selected_wgpu_survives_resize_in_both_directions() {
    if !cfg!(target_os = "macos") {
        return;
    }
    let output_dir = tempfile::tempdir().expect("temporary output directory");
    let mut command = rock_command();
    command
        .current_dir(repo_root())
        .args(["frame", "mag", "--renderer", "wgpu", "-n", "30"])
        .arg("--output")
        .arg(output_dir.path().join("main.lua"))
        .env("ROCK_FRAME_TIMEOUT_SECS", "120")
        .env("STONE_RESIZE_AT_FRAME", "4")
        .env("STONE_RESIZE_W", "960")
        .env("STONE_RESIZE_H", "540")
        .env("STONE_RESIZE_AT_FRAME_2", "12")
        .env("STONE_RESIZE_W_2", "1920")
        .env("STONE_RESIZE_H_2", "1080");
    let result = command.output().expect("spawn resize lifecycle run");
    let mut text = String::from_utf8_lossy(&result.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&result.stderr));

    assert!(
        result.status.success(),
        "selected WGPU must survive both deterministic resizes:\n{text}"
    );
    assert_eq!(
        text.matches("test hook: resizing to").count(),
        2,
        "both resize directions must reach the real window hook:\n{text}"
    );
    assert!(
        text.contains("frame_errors=0")
            && text.contains("wgpu_errors=0")
            && text.contains("wgpu_presents=")
            && text.contains("gl_swaps=0"),
        "the bounded lifecycle summary must report clean renderer state:\n{text}"
    );
}
