//! A full frame compile must remove a stale hot-reload overlay.
//!
//! The live engine can `dofile` an `incremental.lua` beside its active
//! `main.lua`. Every full artifact therefore removes a stale sibling overlay,
//! even when the command producing it does not itself enable hot reload.
//!
//! Fix: every full-`main.lua` writer routes through `write_full_main`, which
//! wipes the sibling overlay. This guards the `frame` entry end to end.
//!
//! Driven through `rock frame` rather than `rock sim`, with the production hot
//! reload gate explicitly enabled. The second frame would therefore load a
//! surviving overlay and print its marker. A private output keeps parallel
//! full-lane processes from replacing this test's artifacts while it starts.
//!
//! Gated (compiles + runs a game): excluded from the default nextest profile.

use std::path::Path;
use crate::stone_harness::{HeavyCommandExt, SpawnBudget};

// CARGO_MANIFEST_DIR is `<repo>/crates/rock-cli`; the rock process resolves its
// `tmp/` output to the repo root regardless of launch cwd, so target that.
fn repo_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("rock-cli has a parent")
        .parent()
        .expect("crates has a parent")
}

#[test]
fn frame_wipes_stale_overlay() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let root = repo_root();
    let output = tempfile::tempdir().expect("frame output");
    let overlay = output.path().join("incremental.lua");

    // Plant a stale sibling that the fresh-main write must remove.
    std::fs::write(&overlay, "print('STALE_OVERLAY_LOADED')\n").expect("plant overlay");

    let out = crate::command::rock_bin()
        .args(["frame", "-n", "2", "mag"])
        .arg("--output")
        .arg(output.path())
        .env("STONE_SCREENSHOT_PATH", output.path().join("frame.png"))
        // `frame` normally leaves hot reload disabled. Opt in so this launch
        // exercises the same overlay loader as `rock stone`: frame one records
        // main.lua's modtime, frame two loads any surviving incremental.lua.
        .env("STONE_HOT_RELOAD", "1")
        // MAG startup can exceed the CLI's interactive five-second watchdog
        // when the full lane is also running other native GL children. This
        // gate is about the overlay contents, not cold-start latency.
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(root)
        .output_with_deadline()
        .expect("run rock frame");

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        !overlay.exists(),
        "writing a fresh main.lua must wipe the sibling incremental.lua"
    );
    assert!(
        !combined.contains("STALE_OVERLAY_LOADED"),
        "the hot-reload-capable launch loaded the stale overlay\n{combined}"
    );
    assert!(
        combined.contains("frames executed successfully"),
        "frame run should succeed against the fresh main\n{combined}"
    );
}
