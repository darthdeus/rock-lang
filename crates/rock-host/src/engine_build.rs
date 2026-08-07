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

/// One optional engine cdylib: what a game asks for, what cargo builds, and
/// what the FFI loader expects to find in `tmp/`.
///
/// The single source of truth for optional libs. Every surface that used to
/// name `stone-video` by hand reads this instead -- the build, the staging,
/// `KNOWN_ENGINE_NEEDS`, `rock stone`'s watch list and its rebuild trigger.
/// Those are separate READS of the same fact, and a per-site copy leaves the
/// ones you did not think of quietly on the old behaviour.
#[derive(Debug)]
pub struct EngineLib {
    /// The `[engine] needs = [...]` entry that requests it.
    pub need: &'static str,
    /// Cargo package name, which is also its top-level directory.
    pub crate_name: &'static str,
    /// cdylib stem cargo produces, `target/debug/lib<src_stem>`.
    pub src_stem: &'static str,
    /// cdylib stem the FFI loader wants, `tmp/lib<dst_stem>`.
    pub dst_stem: &'static str,
}

/// Optional engine cdylibs, in `[engine] needs` order. `stone-rs` is NOT here:
/// it is the core and is always built.
pub const OPTIONAL_ENGINE_LIBS: &[EngineLib] = &[
    EngineLib {
        need: "video",
        crate_name: "stone-video",
        src_stem: "stone_video",
        dst_stem: "stone_video",
    },
    EngineLib {
        need: "wgpu",
        crate_name: "stone-wgpu",
        src_stem: "stone_wgpu",
        dst_stem: "stone_wgpu",
    },
];

/// Parse the game's stick.toml (if present), validate `[engine] needs` early so
/// typos fail fast, and return the optional libs it asks for. A missing
/// manifest is core-only (not an error); a malformed one *is* surfaced via
/// `from_path_optional` (silent-errors audit, 2026-05-02).
///
/// Reports what the manifest DECLARES. Whether the crate can actually be built
/// is `build_optional_engine_lib`'s business, and it rejects rather than skips.
pub fn game_engine_libs(game_path: &str) -> Result<Vec<&'static EngineLib>> {
    let manifest = Path::new(game_path).join("stick.toml");
    let cfg = crate::stick::StickConfig::from_path_optional(&manifest)?;
    let Some(cfg) = cfg else {
        return Ok(Vec::new());
    };
    cfg.validate_engine_needs()?;
    Ok(OPTIONAL_ENGINE_LIBS
        .iter()
        .filter(|lib| cfg.needs_engine_lib(lib.need))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn game_with(needs: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        fs_err::write(
            dir.path().join("stick.toml"),
            format!("[package]\nname = \"g\"\n[engine]\nneeds = [{needs}]\n"),
        )
        .expect("write manifest");
        dir
    }

    fn libs(dir: &tempfile::TempDir) -> Vec<&'static str> {
        game_engine_libs(dir.path().to_str().unwrap())
            .expect("resolve")
            .iter()
            .map(|l| l.need)
            .collect()
    }

    #[test]
    fn a_declared_need_resolves_to_its_crate() {
        let dir = game_with("\"wgpu\"");
        let resolved = game_engine_libs(dir.path().to_str().unwrap()).unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].crate_name, "stone-wgpu");
        assert_eq!(resolved[0].src_stem, "stone_wgpu");
        assert_eq!(resolved[0].dst_stem, "stone_wgpu");
    }

    /// The negative side: a game asking for one lib must not drag in the other.
    /// Without this, a resolver returning the whole table passes the test above.
    #[test]
    fn an_undeclared_lib_is_not_resolved() {
        assert_eq!(libs(&game_with("\"wgpu\"")), vec!["wgpu"]);
        assert_eq!(libs(&game_with("\"video\"")), vec!["video"]);
        assert_eq!(libs(&game_with("")), Vec::<&str>::new());
    }

    #[test]
    fn both_libs_resolve_together_in_table_order() {
        assert_eq!(libs(&game_with("\"wgpu\", \"video\"")), vec!["video", "wgpu"]);
    }

    /// No manifest at all is core-only, not an error: most games have none.
    #[test]
    fn a_game_without_a_manifest_needs_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(game_engine_libs(dir.path().to_str().unwrap()).unwrap().is_empty());
    }

    /// A declared lib with no crate must REJECT. It used to skip silently --
    /// a leftover from when `build_stone_video` was called unconditionally and
    /// the existence check was what made that safe (`f7d570723`). Once
    /// `[engine] needs` became the gate, skipping only meant the game asked for
    /// a lib, did not get it, and failed later inside ffi.load.
    #[test]
    fn a_declared_lib_with_no_crate_is_rejected_not_skipped() {
        let bogus = EngineLib {
            need: "nope",
            crate_name: "definitely-not-a-crate-dir",
            src_stem: "nope",
            dst_stem: "nope",
        };
        let err = build_optional_engine_lib(&bogus).expect_err("absent crate must reject");
        assert!(
            err.to_string().contains("definitely-not-a-crate-dir"),
            "the error must name the crate: {err}"
        );
    }

    /// A typo must reach the caller rather than silently resolving to nothing,
    /// which would skip the cdylib and fail later inside ffi.load.
    #[test]
    fn an_unknown_need_is_rejected_not_dropped() {
        let dir = game_with("\"wgpuu\"");
        let err = game_engine_libs(dir.path().to_str().unwrap())
            .expect_err("typo must be rejected");
        assert!(err.to_string().contains("wgpuu"), "unhelpful error: {err}");
    }
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

/// Build one optional engine cdylib and stage it into tmp/.
///
/// A declared lib whose crate is absent is an ERROR, not a skip. This is a
/// monorepo: the crate is always here, so its absence means the workspace is
/// broken or the need is misspelled past the `KNOWN_ENGINE_NEEDS` check.
/// Skipping produced a game that asked for a lib, silently did not get it, and
/// failed later inside `ffi.load` with nothing pointing back here.
pub fn build_optional_engine_lib(lib: &EngineLib) -> Result<()> {
    if crate::project::is_release_layout() {
        return Ok(());
    }
    if !Path::new(lib.crate_name).exists() {
        anyhow::bail!(
            "stick.toml declares `[engine] needs = [\"{}\"]`, but the crate \
             `{}` is not in this workspace.",
            lib.need,
            lib.crate_name,
        );
    }
    cargo_build_and_stage_lib(
        lib.crate_name,
        &["build", "-p", lib.crate_name],
        lib.src_stem,
        lib.dst_stem,
    )
}

/// Build the engine cdylibs required for a specific game. Always builds
/// stone-rs; additionally builds each optional lib the game's stick.toml
/// declares under `[engine] needs`. Missing/unparseable stick.toml is treated
/// as "core only" — we don't fail the run, the caller will hit a clearer error
/// during compile if the game is misconfigured.
pub fn build_engine_libs(game_path: &str) -> Result<()> {
    build_libstone()?;
    for lib in game_engine_libs(game_path)? {
        build_optional_engine_lib(lib)?;
    }
    Ok(())
}
