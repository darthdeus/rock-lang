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
//! wipes the sibling overlay. This guards the `sim` entry end to end.
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
fn sim_wipes_stale_overlay() {
    let root = repo_root();
    let tmp = root.join("tmp");
    std::fs::create_dir_all(&tmp).expect("mkdir tmp");
    let overlay = tmp.join("incremental.lua");

    // Poison: if the watcher loads it, the print proves the leak; either way a
    // fresh-main write must remove it first.
    std::fs::write(&overlay, "print('STALE_OVERLAY_LOADED')\n").expect("plant overlay");

    let out = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args(["sim", "mag", "mob_attack_windup"])
        .current_dir(root)
        .output()
        .expect("run rock sim");

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        !combined.contains("STALE_OVERLAY_LOADED"),
        "stale overlay was loaded by `rock sim` (must be wiped before the run)\n{combined}"
    );
    assert!(
        !overlay.exists(),
        "writing a fresh main.lua must wipe the sibling incremental.lua"
    );
    assert!(
        combined.contains("mob_attack_windup: PASS"),
        "sim should pass against the fresh main\n{combined}"
    );
}
