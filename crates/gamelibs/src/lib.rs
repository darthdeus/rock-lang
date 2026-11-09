//! The versioned native game-library cache shared by every checkout on a host.

use std::env;
use std::path::{Path, PathBuf};

pub const VERSION: &str = include_str!("../../../gamelibs.version");

/// Name of the explicit work-root override. The per-host table lives with the
/// private tooling, which exports this variable for child builds.
pub(crate) const WORK_ROOT_ENV: &str = "ROCK_WORK_ROOT";

/// Shared host storage policy: an explicit absolute `ROCK_WORK_ROOT`, else the
/// home directory. Host identity never enters the public crates. A root that
/// is not an existing directory is an error, never created: an unmounted work
/// volume must not fill the system disk.
pub fn local_work_root(home: &Path) -> Result<PathBuf, String> {
    if let Some(dir) = env::var_os(WORK_ROOT_ENV).filter(|value| !value.is_empty()) {
        let dir = PathBuf::from(dir);
        if !dir.is_absolute() {
            return Err(format!(
                "{WORK_ROOT_ENV} must be absolute, got {}",
                dir.display()
            ));
        }
        if !dir.is_dir() {
            return Err(format!(
                "{WORK_ROOT_ENV} {} is not an existing directory (is its volume mounted?)",
                dir.display()
            ));
        }
        return Ok(dir);
    }
    Ok(home.to_path_buf())
}

/// The version directory. An override is also a version directory, containing
/// `gamelibs-<platform>` children; all overrides must be absolute.
pub(crate) fn gamelibs_root() -> PathBuf {
    if let Some(dir) = env::var_os("ROCK_GAMELIBS_DIR").filter(|value| !value.is_empty()) {
        let dir = PathBuf::from(dir);
        assert!(
            dir.is_absolute(),
            "ROCK_GAMELIBS_DIR must be an absolute version directory"
        );
        return dir;
    }
    let home = env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .expect("HOME or USERPROFILE is required for the gamelibs cache");
    local_work_root(&home)
        .unwrap_or_else(|error| panic!("{error}"))
        .join("tmp/rock-gamelibs")
        .join(VERSION.trim())
}

/// Resolve one installed platform. Readers never download or write the cache.
pub(crate) fn platform_dir(platform: &str) -> PathBuf {
    assert!(
        matches!(platform, "linux-x86_64" | "macos-arm64" | "windows-x86_64"),
        "no gamelibs for {platform}"
    );
    let path = gamelibs_root().join(format!("gamelibs-{platform}"));
    assert!(
        path.is_dir(),
        "gamelibs cache missing at {}; install the matching native bundle or set ROCK_GAMELIBS_DIR",
        path.display()
    );
    path
}

fn target_platform(os: &str, arch: &str) -> &'static str {
    match (os, arch) {
        ("macos", "aarch64") => "macos-arm64",
        ("windows", "x86_64") => "windows-x86_64",
        ("linux", "x86_64") => "linux-x86_64",
        // BSD builds use the Linux native library set.
        ("freebsd" | "openbsd" | "netbsd", _) => "linux-x86_64",
        (os, arch) => panic!("no prebuilt gamelibs for {os}/{arch}"),
    }
}

/// The native prefix for the running host, independent of Cargo's target env.
pub fn host_dir() -> PathBuf {
    platform_dir(target_platform(env::consts::OS, env::consts::ARCH))
}

/// The prefix for Cargo's compilation target. Track cache overrides and the
/// manifest for relocated builds.
pub fn prebuilt_dir() -> PathBuf {
    println!("cargo:rerun-if-env-changed=ROCK_GAMELIBS_DIR");
    println!("cargo:rerun-if-env-changed=ROCK_WORK_ROOT");
    println!("cargo:rerun-if-env-changed=HOME");
    println!("cargo:rerun-if-env-changed=USERPROFILE");
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    println!(
        "cargo:rerun-if-changed={}",
        manifest.join("Cargo.toml").display()
    );
    let os = env::var("CARGO_CFG_TARGET_OS").expect("CARGO_CFG_TARGET_OS");
    let arch = env::var("CARGO_CFG_TARGET_ARCH").expect("CARGO_CFG_TARGET_ARCH");
    let dir = platform_dir(target_platform(&os, &arch));
    println!("cargo:rerun-if-changed={}", dir.display());
    dir
}

pub(crate) fn is_source_root(path: &Path) -> bool {
    path.join("Cargo.toml").is_file() && path.join("gamelibs.version").is_file()
}

/// Source workspace discovery for Fire's exported-symbol inventory. Native
/// libraries are resolved separately and never identify a source checkout.
pub fn source_root() -> PathBuf {
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    manifest
        .ancestors()
        .find(|dir| is_source_root(dir))
        .unwrap_or_else(|| panic!("no Rock source workspace above {}", manifest.display()))
        .to_path_buf()
}

pub fn link_search_and_rpath() {
    let lib = prebuilt_dir().join("lib");
    println!("cargo:rustc-link-search=native={}", lib.display());
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", lib.display());
    println!("cargo:rerun-if-changed=build.rs");
}
