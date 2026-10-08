//! Regression guard for the deferred-quit teardown (stone_quit vs
//! stone_teardown split, stone-rs/src/lib.rs).
//!
//! `stone_quit()` used to destroy the SDL window + GL context + ImGui
//! synchronously. Called from an `@late_draw` system (the pause-menu "Quit"),
//! that runs INSIDE the render block, so the rest of the frame
//! (render_ui_batches, imgui render, buffer swap) then ran against a torn-down
//! context and segfaulted. `stone_quit()` is a deferred flag now; the loop ends
//! after the current frame and stone_teardown() runs once post-loop.
//!
//! `demos/quit_from_late_draw` calls stone_quit() from @late_draw at frame 3.
//! A regression makes `rock frame` die by signal -> non-success exit.
//!
//! Gated out of the default nextest profile (spawns a windowed game); runs
//! under `cargo nextest run -P full`. See .config/nextest.toml.

use std::path::Path;
use std::process::Command;
use crate::stone_harness::{HeavyCommandExt, SpawnBudget};

fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .parent()
        .expect("repo root")
}

#[test]
fn quit_from_late_draw_exits_cleanly() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let rock_bin = env!("CARGO_BIN_EXE_rock");
    let output = Command::new(rock_bin)
        .args(["frame", "demos/quit_from_late_draw", "-n", "30"])
        // Generous watchdog: init + a few frames on a HiDPI box under CI load.
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(workspace_root())
        .output_with_deadline()
        .expect("failed to spawn rock frame");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // A segfault from tearing the window down mid-frame exits by signal
    // (status.success() == false, no exit code).
    assert!(
        output.status.success(),
        "rock frame on demos/quit_from_late_draw exited with {:?} \
         (a mid-frame teardown crash exits by signal)\nstdout: {stdout}\nstderr: {stderr}",
        output.status,
    );

    // Prove the @late_draw quit path was actually exercised (guards against a
    // false green where the demo never reached the quit call).
    assert!(
        stdout.contains("requesting quit from @late_draw") || stderr.contains("requesting quit from @late_draw"),
        "demo did not log the @late_draw quit -- the crash path was not exercised\nstdout: {stdout}\nstderr: {stderr}",
    );
}
