//! A non-stone game command must never load a stale hot-reload overlay.
//!
//! The engine's per-frame hot-reload watcher (`stone/engine/hot_reload.rock`)
//! runs in every command's frame loop and `dofile`s `tmp/incremental.lua` on
//! its first appearance. Only `rock stone` wipes that overlay on a full build,
//! so a stale `incremental.lua` left by a prior stone session used to be loaded
//! over the fresh `main.lua` by `rock sim`/`run` -- shadowing functions with
//! pre-edit definitions (e.g. a 1-arg call to a now-2-arg fn -> nil param).
//!
//! Fix: every full-`main.lua` writer routes through `write_full_main`, which
//! wipes the sibling overlay. This guards the `frame` entry end to end.
//!
//! Driven through `rock frame` rather than `rock sim`: sim invocations compile
//! into a private per-invocation directory, so their overlay is a sibling of
//! THAT `main.lua` and a workspace `tmp/incremental.lua` can never reach them
//! (`tests/sim_concurrent_invocations.rs`). `frame` still writes the shared
//! `tmp/`, so it is where this gap remains open and worth guarding.
//!
//! Gated (compiles + runs a game): excluded from the default nextest profile.

use std::path::Path;
use std::process::Command;

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
    let root = repo_root();
    let tmp = root.join("tmp");
    std::fs::create_dir_all(&tmp).expect("mkdir tmp");
    let overlay = tmp.join("incremental.lua");

    // Poison: if the watcher loads it, the print proves the leak; either way a
    // fresh-main write must remove it first.
    std::fs::write(&overlay, "print('STALE_OVERLAY_LOADED')\n").expect("plant overlay");

    let out = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args(["frame", "-n", "2", "mag"])
        .current_dir(root)
        .output()
        .expect("run rock frame");

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        !combined.contains("STALE_OVERLAY_LOADED"),
        "stale overlay was loaded by `rock frame` (must be wiped before the run)\n{combined}"
    );
    assert!(
        !overlay.exists(),
        "writing a fresh main.lua must wipe the sibling incremental.lua"
    );
    assert!(
        combined.contains("frames executed successfully"),
        "frame run should succeed against the fresh main\n{combined}"
    );
}
