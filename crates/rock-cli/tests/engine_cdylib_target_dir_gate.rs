//! A game command must stage the engine cdylib from the Cargo target
//! directory Cargo actually built it into.
//!
//! `stage_cdylib` used to hardcode `<rock_dir>/target/debug` and ignore
//! `CARGO_TARGET_DIR`. With the override set that is not merely a wrong
//! path: in a tree that happens to hold an old `target/debug` cdylib it
//! silently stages the stale one, and in a fresh tree it fails with a message
//! about a path cargo never wrote. The benchmark's receipt step hit both
//! (`30eb5a0d0`).
//!
//! This gate drives the production `build_libstone` with `CARGO_TARGET_DIR`
//! pointed at a scratch directory, and deletes the artifact at every end
//! before the build: the scratch artifact, the staged copy, and the worktree
//! `target/debug` copy today's code would fall back to. A mere "staged file
//! exists" check would pass on the bug by staging that stale fallback, so
//! deleting every potential source is what makes the surviving staged file
//! necessarily the one Cargo produced in the scratch directory. A content
//! hash is not used because `stage_cdylib` rewrites the macOS install name
//! and re-signs the copy, so the bytes differ even when the copy is correct.
//!
//! `#[ignore]`d: it rebuilds `stone-rs`, replaces the shared staged
//! `tmp/libstone`, and removes the worktree `target/debug` core cdylib. Run
//! deliberately:
//!
//!   cargo test -p rock-cli --test rock_cli_heavy -- --ignored --nocapture \
//!     engine_cdylib_target_dir

use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn cdylib_name(stem: &str) -> String {
    #[cfg(target_os = "windows")]
    {
        format!("{stem}.dll")
    }
    #[cfg(target_os = "macos")]
    {
        format!("lib{stem}.dylib")
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        format!("lib{stem}.so")
    }
}

#[test]
#[ignore = "rebuilds stone-rs and replaces shared target/tmp artifacts; run deliberately"]
fn the_staged_core_cdylib_comes_from_the_cargo_target_directory_cargo_used() {
    let root = workspace_root();
    let home = std::env::var_os("HOME").expect("HOME");
    let scratch = PathBuf::from(home).join("tmp/rock-engine-cargo-target-dir-gate");
    let scratch_lib = scratch.join("debug").join(cdylib_name("stone_rs"));
    let staged = root.join("tmp").join(cdylib_name("stone"));
    let worktree_lib = root.join("target/debug").join(cdylib_name("stone_rs"));

    // Dependencies cached in the scratch dir survive; only the final artifact
    // is removed, so each run must (re)produce it and the first run pays the
    // one-time cost of building the dependency graph into the scratch dir.
    fs_err::create_dir_all(scratch.join("debug")).expect("create scratch target");
    for path in [&scratch_lib, &staged, &worktree_lib] {
        if path.exists() {
            fs_err::remove_file(path).expect("clear pre-existing cdylib");
        }
    }

    let prev_receipt = std::env::var_os("ROCK_ENGINE_BUILD_RECEIPT");
    let prev_target = std::env::var_os("CARGO_TARGET_DIR");
    unsafe {
        std::env::remove_var("ROCK_ENGINE_BUILD_RECEIPT");
        std::env::set_var("CARGO_TARGET_DIR", &scratch);
    }

    let result = rock_host::engine_build::build_libstone();

    unsafe {
        match prev_target {
            Some(value) => std::env::set_var("CARGO_TARGET_DIR", value),
            None => std::env::remove_var("CARGO_TARGET_DIR"),
        }
        if let Some(value) = prev_receipt {
            std::env::set_var("ROCK_ENGINE_BUILD_RECEIPT", value);
        }
    }

    result.expect("build_libstone must build and stage from the resolved target directory");
    assert!(
        scratch_lib.exists(),
        "cargo did not build the core cdylib into CARGO_TARGET_DIR: {}",
        scratch_lib.display()
    );
    assert!(
        staged.exists(),
        "the core cdylib built into CARGO_TARGET_DIR was not staged into tmp/: {}",
        staged.display()
    );
}