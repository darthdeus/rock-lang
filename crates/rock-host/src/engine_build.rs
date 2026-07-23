//! Engine cdylib build + staging — the shared substrate that every game
//! command (`run`/`frame`/`perf`/`sim`/`stone`) uses to get `libstone` (and
//! optionally `libstone_video`) into `tmp/` before luajit `ffi.load`s them.
//!
//! In release layout these are no-ops: the prebuilt libraries already live in
//! `<rock_dir>/lib/` and the runtime loader finds them via the lib-path env.
//! In dev layout we shell out to cargo, then copy the cdylib from
//! `target/debug/` into `tmp/`, renaming `libstone_rs` -> `libstone` etc. so
//! the FFI loader sees the same name on both layouts.

use anyhow::Result;
use std::path::Path;
use std::process::{Command, Stdio};

/// Run `cargo <args>` with inherited stdio, bailing with `fail_msg`
/// if cargo exits non-zero. Single place for the build-invoke +
/// status-check the engine/compiler builders all need.
pub fn run_cargo(args: &[&str], fail_msg: &str) -> Result<()> {
    let status = Command::new("cargo")
        .args(args)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;
    if !status.success() {
        anyhow::bail!("{}", fail_msg);
    }
    Ok(())
}

/// Parse the game's stick.toml (if present), validate `[engine]
/// needs` early so typos fail fast, and report whether the game
/// requests the video engine lib. A missing manifest is core-only
/// (not an error); a malformed one *is* surfaced via
/// `from_path_optional` (silent-errors audit, 2026-05-02).
pub fn game_requests_video(game_path: &str) -> Result<bool> {
    let manifest = Path::new(game_path).join("stick.toml");
    let cfg = crate::stick::StickConfig::from_path_optional(&manifest)?;
    if let Some(ref cfg) = cfg {
        cfg.validate_engine_needs()?;
    }
    Ok(cfg.as_ref().map_or(false, |c| c.needs_engine_lib("video")))
}

/// Build a cargo crate and stage its cdylib into tmp/ under the given name.
/// Release layout is a no-op (prebuilt libs already staged); dev layout shells
/// out to cargo then copies + renames the cdylib via `stage_cdylib`.
fn cargo_build_and_stage_lib(
    display_name: &str,
    cargo_args: &[&str],
    src_stem: &str,
    dst_stem: &str,
) -> Result<()> {
    if crate::project::is_release_layout() {
        return Ok(());
    }
    run_cargo(cargo_args, &format!("Failed to build {}", display_name))?;
    stage_cdylib(src_stem, dst_stem)
}

/// Copy a freshly-built cdylib from `target/debug/` into `tmp/`, renaming
/// `libfoo_bar` -> `lib<dst_stem>` so the FFI loader sees a stable name on
/// both dev and release layouts.
///
/// **Concurrency.** Multiple `rock` invocations may stage the same cdylib
/// at the same time (canonical case: `cargo nextest run -P full` running
/// the json_serialize_tests in parallel — they all spawn `rock frame
/// match3` against the shared workspace `tmp/`). Direct copy + codesign
/// to the final path races: codesign A reads `tmp/libstone.dylib`, sees
/// an existing signature, and is mid-rewrite when copy B unlinks-and-
/// recreates the path; codesign A's path-based reopen then errors with
/// "No such file or directory". Pattern observed at `serializer_branch_
/// coverage` failure on 2026-05-04 under `-P full`.
///
/// Fix: stage to a unique per-process path, codesign there (in
/// isolation), then atomically `rename` to the final name. `rename(2)`
/// is atomic on macOS and Linux; concurrent dlopens see either the old
/// or the new fully-signed file but never a torn or mid-resign one.
pub fn stage_cdylib(src_stem: &str, dst_stem: &str) -> Result<()> {
    fs_err::create_dir_all("tmp")?;
    let pid = std::process::id();

    // Platform cdylib naming: Unix prefixes `lib`, Windows doesn't;
    // extension is .so / .dylib / .dll.
    #[cfg(target_os = "linux")]
    let (src, dst) = (
        format!("target/debug/lib{src_stem}.so"),
        format!("tmp/lib{dst_stem}.so"),
    );
    #[cfg(target_os = "macos")]
    let (src, dst) = (
        format!("target/debug/lib{src_stem}.dylib"),
        format!("tmp/lib{dst_stem}.dylib"),
    );
    #[cfg(target_os = "windows")]
    let (src, dst) = (
        format!("target/debug/{src_stem}.dll"),
        format!("tmp/{dst_stem}.dll"),
    );

    let staging = format!("{dst}.{pid}");

    // `cargo build -p <pkg>` can exit 0 without producing the cdylib at
    // `target/debug/lib<stem>.dylib`: a cleaned/foreign target dir
    // (multi-worktree sharing), a crate-type that no longer includes
    // `cdylib`, or a build that touched only dependents. A bare
    // `fs_err::copy` then fails with a cryptic "No such file or
    // directory (os error 2)" that points at the *destination*, hiding
    // the real problem. Surface it.
    if !Path::new(&src).exists() {
        let pkg = src_stem.replace('_', "-");
        anyhow::bail!(
            "cargo build reported success but the cdylib was not produced at `{src}`.\n\
             Likely: a stale/cleaned/foreign `target/` dir (worktrees sharing a \
             target), `{pkg}` `crate-type` missing `cdylib`, or the build only \
             touched dependents.\n\
             Recover: `cargo build -p {pkg}` from this worktree and confirm \
             `{src}` exists, then re-run.",
            src = src,
            pkg = pkg,
        );
    }
    fs_err::copy(&src, &staging)?;

    // macOS only: strip rustc's `linker-signed` flag by re-signing
    // adhoc. Sequoia 15.x's syspolicyd evaluates (adhoc |
    // linker-signed) through a stricter path than plain adhoc and may
    // SIGKILL dlopen with no userspace error for fresh cdhashes — see
    // XProtect data update pushed 2026-04-24. Plain `--sign -` is the
    // standard adhoc resign. Done in isolation on the per-pid staging
    // path so concurrent stagers can't read a mid-resign file.
    #[cfg(target_os = "macos")]
    {
        let status = Command::new("codesign")
            .args(["--force", "--sign", "-", &staging])
            .status()?;
        if !status.success() {
            let _ = fs_err::remove_file(&staging);
            anyhow::bail!("codesign --force --sign - failed for {}", staging);
        }
    }

    fs_err::rename(&staging, &dst)?;
    Ok(())
}

/// Build stone-rs and stage it as libstone in tmp/. Core engine only —
/// optional cdylibs (stone-video) are gated on `[engine] needs = [...]`
/// in the game's stick.toml. Use `build_engine_libs(game_path)` when you
/// have a specific game in scope; this raw form is fine for callers that
/// have no game (REPL, library compiles).
pub fn build_libstone() -> Result<()> {
    cargo_build_and_stage_lib("stone-rs", &["build", "--lib"], "stone_rs", "stone")?;
    Ok(())
}

/// Build stone-video and stage it as libstone_video in tmp/ (only if the crate exists).
pub fn build_stone_video() -> Result<()> {
    if crate::project::is_release_layout() {
        return Ok(());
    }
    if !std::path::Path::new("stone-video").exists() {
        return Ok(());
    }
    cargo_build_and_stage_lib(
        "stone-video",
        &["build", "-p", "stone-video"],
        "stone_video",
        "stone_video",
    )
}

/// Build the engine cdylibs required for a specific game. Always builds
/// stone-rs; additionally builds stone-video iff the game's stick.toml
/// declares `[engine] needs = ["video"]`. Missing/unparseable stick.toml is
/// treated as "core only" — we don't fail the run, the caller will hit a
/// clearer error during compile if the game is misconfigured.
pub fn build_engine_libs(game_path: &str) -> Result<()> {
    build_libstone()?;
    if game_requests_video(game_path)? {
        build_stone_video()?;
    }
    Ok(())
}
