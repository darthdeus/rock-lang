//! Proves that core engine compilation is scoped to `stone-rs` rather than
//! building the entire workspace.
//!
//! We choose a real-mechanism gate in the style of `engine_cdylib_freshness_gate`:
//! after a staged engine build via `build_libstone`, it asserts that an artifact
//! for a crate `stone-rs` does not depend on (`rock-cli`) is absent in `target/debug/`.
//! This proves real Cargo and filesystem behavior rather than asserting syntax or
//! arguments by construction.
//!
//! `#[ignore]`d for the same reason `engine_cdylib_freshness_gate` is: it removes
//! and rebuilds shared workspace artifacts in `target/debug/` and `tmp/`. Run
//! deliberately:
//!
//!   cargo test -p rock-cli --test rock_cli_heavy -- --ignored --nocapture engine_build_scope

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
#[ignore = "deletes shared workspace artifacts and rebuilds a crate; run deliberately"]
fn core_engine_build_does_not_compile_unrelated_workspace_crates() {
    let root = workspace_root();

    // The unrelated crate artifact that stone-rs does NOT depend on.
    // When cargo build --lib runs with no -p, rock-cli's lib target is compiled
    // and produces target/debug/librock_cli.rlib. When scoped with -p stone-rs,
    // cargo only builds stone-rs and its dependencies, leaving this artifact absent.
    let unrelated_artifact = root.join("target/debug/librock_cli.rlib");
    if unrelated_artifact.exists() {
        fs_err::remove_file(&unrelated_artifact).expect("remove unrelated artifact before test");
    }
    assert!(
        !unrelated_artifact.exists(),
        "fixture starts without unrelated artifact: {}",
        unrelated_artifact.display()
    );

    // Also clear staged libstone so we verify the staged artifact is actually produced
    let staged_stone = root.join("tmp").join(cdylib_name("stone"));
    if staged_stone.exists() {
        fs_err::remove_file(&staged_stone).expect("remove staged libstone");
    }

    // Ensure no receipt in the environment bypasses the build
    let prev_receipt = std::env::var_os("ROCK_ENGINE_BUILD_RECEIPT");
    unsafe {
        std::env::remove_var("ROCK_ENGINE_BUILD_RECEIPT");
    }

    let result = rock_host::engine_build::build_libstone();

    // Restore env
    if let Some(val) = prev_receipt {
        unsafe {
            std::env::set_var("ROCK_ENGINE_BUILD_RECEIPT", val);
        }
    }

    result.expect("build_libstone should succeed");

    assert!(
        staged_stone.exists(),
        "build_libstone must produce staged libstone at {}",
        staged_stone.display()
    );

    assert!(
        !unrelated_artifact.exists(),
        "build_libstone built unrelated crate artifact {}, proving cargo was not package-scoped",
        unrelated_artifact.display()
    );
}
