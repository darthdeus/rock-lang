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

/// A project root whose `tmp/` is private and whose every other top-level
/// entry links to the real workspace. `build_libstone` stages into
/// `<ROCK_DIR>/tmp`, and `tools/test-desktop.sh full` pins the sha1 of the
/// real `<root>/tmp/libstone.*` in an engine build receipt for the whole run,
/// so a test that deletes or restages that file fails every later heavy test.
/// Cargo still builds the real sources into the real `target/`.
///
/// Sets `ROCK_DIR`, which `rock_dir()` caches on first use: call it before any
/// production call, once per test process (nextest runs one test per process).
pub(crate) fn private_engine_root(label: &str) -> PathBuf {
    let real = workspace_root();
    let home = std::env::var_os("HOME").expect("HOME");
    let private = PathBuf::from(home).join("tmp").join(format!("rock-engine-gate-root-{label}"));
    if private.exists() {
        fs_err::remove_dir_all(&private).expect("clear previous private root");
    }
    fs_err::create_dir_all(private.join("tmp")).expect("create private root");
    for entry in fs_err::read_dir(&real).expect("list workspace root") {
        let name = entry.expect("workspace entry").file_name();
        if matches!(name.to_str(), Some("tmp" | ".git")) {
            continue;
        }
        std::os::unix::fs::symlink(real.join(&name), private.join(&name))
            .expect("link workspace entry into private root");
    }
    unsafe { std::env::set_var("ROCK_DIR", &private) };
    private
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
    let root = private_engine_root("scope");

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
