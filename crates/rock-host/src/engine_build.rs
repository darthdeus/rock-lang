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
use sha1::{Digest, Sha1};
use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const ENGINE_BUILD_RECEIPT_ENV: &str = "ROCK_ENGINE_BUILD_RECEIPT";
const ENGINE_BUILD_RECEIPT_VERSION: u32 = 1;

#[derive(Debug)]
struct EngineBuildReceipt {
    version: u32,
    workspace: String,
    libs: Vec<ReceiptLib>,
}

#[derive(Clone, Debug)]
struct ReceiptLib {
    src_stem: String,
    dst_stem: String,
    inputs: Vec<String>,
    inputs_sha1: String,
    staged_sha1: String,
}

/// Run `cargo <args>` with inherited stdio, bailing with `fail_msg`
/// if cargo exits non-zero. Single place for the build-invoke +
/// status-check the engine/compiler builders all need.
pub fn run_cargo(args: &[&str], fail_msg: &str) -> Result<()> {
    let status = Command::new("cargo")
        .args(args)
        .current_dir(crate::rock_dir())
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
    EngineLib {
        need: "3d",
        crate_name: "stone-3d",
        src_stem: "stone_3d",
        dst_stem: "stone_3d",
    },
    EngineLib {
        need: "agent",
        crate_name: "stone-agent",
        src_stem: "stone_agent",
        dst_stem: "stone_agent",
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
    cfg.validate_renderer()?;
    cfg.validate_ios()?;
    let renderer = cfg.renderer_backend()?;
    Ok(OPTIONAL_ENGINE_LIBS
        .iter()
        .filter(|lib| {
            cfg.needs_engine_lib(lib.need)
                || (lib.need == "wgpu" && renderer == crate::stick::RendererBackend::Wgpu)
        })
        .collect())
}

/// The Cargo target directory for `rock_root`. Cargo resolves a relative
/// `CARGO_TARGET_DIR` against the directory it was invoked from, and
/// `run_cargo` runs cargo with `current_dir(crate::rock_dir())`; resolve it the
/// same way here. Without the override Cargo uses `<rock_root>/target`.
fn cargo_target_dir(rock_root: &Path) -> PathBuf {
    match std::env::var_os("CARGO_TARGET_DIR").filter(|value| !value.is_empty()) {
        Some(value) => {
            let dir = PathBuf::from(value);
            if dir.is_absolute() {
                dir
            } else {
                rock_root.join(dir)
            }
        }
        None => rock_root.join("target"),
    }
}

fn dep_info_path(target_dir: &Path, src_stem: &str) -> PathBuf {
    let debug = target_dir.join("debug");
    #[cfg(target_os = "windows")]
    return debug.join(format!("{src_stem}.d"));
    #[cfg(not(target_os = "windows"))]
    debug.join(format!("lib{src_stem}.d"))
}

fn staged_cdylib_path(root: &Path, dst_stem: &str) -> PathBuf {
    #[cfg(target_os = "linux")]
    return root.join("tmp").join(format!("lib{dst_stem}.so"));
    #[cfg(target_os = "macos")]
    return root.join("tmp").join(format!("lib{dst_stem}.dylib"));
    #[cfg(target_os = "windows")]
    return root.join("tmp").join(format!("{dst_stem}.dll"));
}

fn makefile_words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('\n') => {}
                Some('\r') if chars.peek() == Some(&'\n') => {
                    chars.next();
                }
                Some(next) => word.push(next),
                None => word.push(ch),
            }
        } else if ch.is_whitespace() {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
        } else {
            word.push(ch);
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

fn local_dep_inputs(root: &Path, target_dir: &Path, src_stem: &str) -> Result<Vec<String>> {
    let root = fs_err::canonicalize(root)?;
    let dep_info = dep_info_path(target_dir, src_stem);
    let text = fs_err::read_to_string(&dep_info).map_err(|error| {
        anyhow::anyhow!(
            "engine build receipt cannot read Cargo dep-info `{}`: {error}",
            dep_info.display()
        )
    })?;
    let text = text.replace("\\\r\n", "").replace("\\\n", "");
    let mut inputs = BTreeSet::new();
    for line in text.lines() {
        let Some((_, dependencies)) = line.split_once(": ") else {
            continue;
        };
        for dependency in makefile_words(dependencies) {
            let path = PathBuf::from(dependency);
            let path = if path.is_absolute() { path } else { root.join(path) };
            if path.is_file() {
                let path = fs_err::canonicalize(path)?;
                if let Ok(relative) = path.strip_prefix(&root) {
                    inputs.insert(relative.to_string_lossy().into_owned());
                }
            }
        }
    }
    for entry in walkdir::WalkDir::new(&root).into_iter().filter_entry(|entry| {
        let name = entry.file_name().to_string_lossy();
        !matches!(name.as_ref(), ".git" | "target" | "tmp" | "prebuilt")
    }) {
        let entry = entry?;
        if entry.file_type().is_file() && entry.file_name() == "Cargo.toml" {
            let relative = entry.path().strip_prefix(&root)?;
            inputs.insert(relative.to_string_lossy().into_owned());
        }
    }
    for extra in ["Cargo.lock", ".cargo/config.toml"] {
        if root.join(extra).is_file() {
            inputs.insert(extra.to_string());
        }
    }
    if inputs.is_empty() {
        anyhow::bail!(
            "engine build receipt found no local inputs in `{}`",
            dep_info.display()
        );
    }
    Ok(inputs.into_iter().collect())
}

fn sha1_file(path: &Path) -> Result<String> {
    let mut file = fs_err::File::open(path)?;
    let mut hash = Sha1::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn inputs_sha1(root: &Path, inputs: &[String]) -> Result<String> {
    let mut hash = Sha1::new();
    for relative in inputs {
        let path = root.join(relative);
        let bytes = fs_err::read(&path).map_err(|error| {
            anyhow::anyhow!("engine build input `{}` is unavailable: {error}", path.display())
        })?;
        hash.update((relative.len() as u64).to_le_bytes());
        hash.update(relative.as_bytes());
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(&bytes);
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn capture_receipt_lib(
    root: &Path,
    target_dir: &Path,
    src_stem: &str,
    dst_stem: &str,
) -> Result<ReceiptLib> {
    let root = fs_err::canonicalize(root)?;
    let inputs = local_dep_inputs(&root, target_dir, src_stem)?;
    let inputs_sha1 = inputs_sha1(&root, &inputs)?;
    let staged = staged_cdylib_path(&root, dst_stem);
    let staged_sha1 = sha1_file(&staged).map_err(|error| {
        anyhow::anyhow!(
            "engine build receipt cannot fingerprint staged library `{}`: {error}",
            staged.display()
        )
    })?;
    Ok(ReceiptLib {
        src_stem: src_stem.to_string(),
        dst_stem: dst_stem.to_string(),
        inputs,
        inputs_sha1,
        staged_sha1,
    })
}

fn validate_receipt_lib(root: &Path, receipt: &ReceiptLib) -> Result<()> {
    let root = fs_err::canonicalize(root)?;
    let current_inputs = inputs_sha1(&root, &receipt.inputs).map_err(|error| {
        anyhow::anyhow!(
            "engine build receipt for `{}` rejected because its inputs changed: {error}",
            receipt.dst_stem
        )
    })?;
    if current_inputs != receipt.inputs_sha1 {
        anyhow::bail!(
            "engine build receipt for `{}` rejected because its inputs changed",
            receipt.dst_stem
        );
    }
    let staged = staged_cdylib_path(&root, &receipt.dst_stem);
    let current_stage = sha1_file(&staged).map_err(|error| {
        anyhow::anyhow!(
            "engine build receipt for `{}` rejected because its staged library changed: {error}",
            receipt.dst_stem
        )
    })?;
    if current_stage != receipt.staged_sha1 {
        anyhow::bail!(
            "engine build receipt for `{}` rejected because its staged library changed",
            receipt.dst_stem
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // In-process callers (the sim differential's threads) stage the same
    // library at once; a shared staging path lets one rename the other's
    // half-written copy.
    #[test]
    fn concurrent_stagings_in_one_process_use_distinct_paths() {
        let dst = Path::new("/tmp/libstone.dylib");
        assert_ne!(staging_path(dst), staging_path(dst));
    }

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

    #[test]
    fn a_declared_3d_need_resolves_to_its_crate() {
        let dir = game_with("\"3d\"");
        let resolved = game_engine_libs(dir.path().to_str().unwrap()).unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].crate_name, "stone-3d");
        assert_eq!(resolved[0].src_stem, "stone_3d");
        assert_eq!(resolved[0].dst_stem, "stone_3d");
    }

    #[test]
    fn renderer_selection_resolves_wgpu_without_a_compute_need() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs_err::write(
            dir.path().join("stick.toml"),
            "[package]\nname = \"g\"\n[renderer]\nbackend = \"wgpu\"\n",
        ).expect("write manifest");
        assert_eq!(libs(&dir), vec!["wgpu"]);
    }

    /// The negative side: a game asking for one lib must not drag in the other.
    /// Without this, a resolver returning the whole table passes the test above.
    #[test]
    fn an_undeclared_lib_is_not_resolved() {
        assert_eq!(libs(&game_with("\"wgpu\"")), vec!["wgpu"]);
        assert_eq!(libs(&game_with("\"video\"")), vec!["video"]);
        assert_eq!(libs(&game_with("\"3d\"")), vec!["3d"]);
        assert_eq!(libs(&game_with("")), Vec::<&str>::new());
    }

    #[test]
    fn engine_libs_resolve_together_in_table_order() {
        assert_eq!(
            libs(&game_with("\"3d\", \"wgpu\", \"video\"")),
            vec!["video", "wgpu", "3d"]
        );
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

    #[cfg(target_os = "macos")]
    #[test]
    fn staged_cdylib_uses_its_stable_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("probe.c");
        let dylib = dir.path().join("libsource.dylib");
        fs_err::write(&source, "int probe(void) { return 1; }\n").expect("write source");
        let compile = Command::new("clang")
            .args(["-dynamiclib"])
            .arg(&source)
            .arg("-o")
            .arg(&dylib)
            .status()
            .expect("run clang");
        assert!(compile.success(), "compile probe dylib");

        set_macos_install_name(&dylib, "stone").expect("set install name");
        let output = Command::new("otool")
            .arg("-D")
            .arg(&dylib)
            .output()
            .expect("run otool");
        assert!(output.status.success(), "read install name");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.lines().any(|line| line == "@rpath/libstone.dylib"), "{stdout}");
    }

    fn receipt_fixture() -> (tempfile::TempDir, ReceiptLib) {
        let root = tempfile::tempdir().expect("tempdir");
        let target_dir = root.path().join("target");
        let source = root.path().join("stone-rs/src/lib.rs");
        fs_err::create_dir_all(source.parent().unwrap()).expect("source dir");
        fs_err::create_dir_all(target_dir.join("debug")).expect("target dir");
        fs_err::create_dir_all(root.path().join("tmp")).expect("stage dir");
        fs_err::write(&source, "pub fn version() -> u32 { 1 }\n").expect("source");
        fs_err::write(root.path().join("Cargo.toml"), "[workspace]\n").expect("manifest");
        fs_err::write(
            dep_info_path(&target_dir, "probe"),
            format!("target/debug/libprobe.rlib: {}\n", source.display()),
        )
        .expect("dep info");
        fs_err::write(staged_cdylib_path(root.path(), "probe"), b"built artifact")
            .expect("staged dylib");
        let receipt =
            capture_receipt_lib(root.path(), &target_dir, "probe", "probe").expect("capture");
        (root, receipt)
    }

    #[test]
    fn receipt_fixture_writes_dep_info_under_its_workspace_root() {
        let (root, _) = receipt_fixture();
        let debug = root.path().join("target/debug");
        #[cfg(target_os = "windows")]
        let dep_info = debug.join("probe.d");
        #[cfg(not(target_os = "windows"))]
        let dep_info = debug.join("libprobe.d");
        assert!(
            dep_info.is_file(),
            "receipt fixture dep-info escaped `{}`",
            root.path().display()
        );
    }

    #[test]
    fn a_shared_engine_receipt_rejects_an_input_changed_after_capture() {
        let (root, receipt) = receipt_fixture();
        validate_receipt_lib(root.path(), &receipt).expect("unchanged receipt is valid");

        fs_err::write(root.path().join("stone-rs/src/lib.rs"), "pub fn version() -> u32 { 2 }\n")
            .expect("mutate source");
        let error = validate_receipt_lib(root.path(), &receipt).expect_err("stale input must reject");
        assert!(error.to_string().contains("inputs changed"), "{error:#}");
    }

    #[test]
    fn a_shared_engine_receipt_rejects_a_replaced_staged_library() {
        let (root, receipt) = receipt_fixture();
        fs_err::write(staged_cdylib_path(root.path(), "probe"), b"different artifact")
            .expect("replace stage");
        let error = validate_receipt_lib(root.path(), &receipt).expect_err("wrong dylib must reject");
        assert!(error.to_string().contains("staged library changed"), "{error:#}");
    }

    #[test]
    fn a_shared_engine_receipt_round_trips_its_workspace_and_library_identity() {
        let (root, lib) = receipt_fixture();
        let root_path = fs_err::canonicalize(root.path()).expect("canonical root");
        let receipt = EngineBuildReceipt {
            version: ENGINE_BUILD_RECEIPT_VERSION,
            workspace: root_path.to_string_lossy().into_owned(),
            libs: vec![lib],
        };
        let path = root.path().join("receipt.json");
        fs_err::write(&path, engine_build_receipt_json(&receipt).expect("encode")).expect("receipt");

        let loaded = load_engine_build_receipt(&root_path, &path).expect("load");
        assert_eq!(loaded.version, ENGINE_BUILD_RECEIPT_VERSION);
        assert_eq!(loaded.workspace, receipt.workspace);
        assert_eq!(loaded.libs.len(), 1);
        validate_receipt_lib(&root_path, &loaded.libs[0]).expect("loaded identity remains valid");
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
    if validate_receipted_lib(dst_stem)? {
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
/// The path a `stage_cdylib` call copies and signs into before its rename.
fn staging_path(dst: &Path) -> String {
    format!("{}.{}", dst.display(), std::process::id())
}

pub fn stage_cdylib(src_stem: &str, dst_stem: &str) -> Result<()> {
    // A receipt owns the destination even for callers that bypass the builder.
    if validate_receipted_lib(dst_stem)? {
        return Ok(());
    }
    let rock_root = PathBuf::from(crate::rock_dir());
    let target_debug = cargo_target_dir(&rock_root).join("debug");
    let dev_tmp = rock_root.join("tmp");
    fs_err::create_dir_all(&dev_tmp)?;

    // Platform cdylib naming: Unix prefixes `lib`, Windows doesn't;
    // extension is .so / .dylib / .dll.
    #[cfg(target_os = "linux")]
    let (src, dst) = (
        target_debug.join(format!("lib{src_stem}.so")),
        dev_tmp.join(format!("lib{dst_stem}.so")),
    );
    #[cfg(target_os = "macos")]
    let (src, dst) = (
        target_debug.join(format!("lib{src_stem}.dylib")),
        dev_tmp.join(format!("lib{dst_stem}.dylib")),
    );
    #[cfg(target_os = "windows")]
    let (src, dst) = (
        target_debug.join(format!("{src_stem}.dll")),
        dev_tmp.join(format!("{dst_stem}.dll")),
    );

    let staging = staging_path(&dst);

    // `cargo build -p <pkg>` can exit 0 without producing the cdylib at
    // `target/debug/lib<stem>.dylib`: a cleaned/foreign target dir
    // (multi-worktree sharing), a crate-type that no longer includes
    // `cdylib`, or a build that touched only dependents. A bare
    // `fs_err::copy` then fails with a cryptic "No such file or
    // directory (os error 2)" that points at the *destination*, hiding
    // the real problem. Surface it.
    if !src.exists() {
        let pkg = src_stem.replace('_', "-");
        anyhow::bail!(
            "cargo build reported success but the cdylib was not produced at `{src}`.\n\
             Likely: `CARGO_TARGET_DIR` selects a stale/cleaned/foreign target \
             dir (worktrees sharing a target), `{pkg}` `crate-type` missing \
             `cdylib`, or the build only touched dependents.\n\
             Recover: `cargo build -p {pkg}` from this worktree (with the same \
             `CARGO_TARGET_DIR`, if set) and confirm `{src}` exists, then re-run.",
            src = src.display(),
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
        if let Err(error) = set_macos_install_name(Path::new(&staging), dst_stem) {
            let _ = fs_err::remove_file(&staging);
            return Err(error);
        }
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

#[cfg(target_os = "macos")]
fn set_macos_install_name(path: &Path, dst_stem: &str) -> Result<()> {
    let status = Command::new("install_name_tool")
        .args(["-id", &format!("@rpath/lib{dst_stem}.dylib")])
        .arg(path)
        .status()?;
    if !status.success() {
        anyhow::bail!("install_name_tool -id failed for {}", path.display());
    }
    Ok(())
}

/// Build stone-rs and stage it as libstone in tmp/. Core engine only —
/// optional cdylibs (stone-video) are gated on `[engine] needs = [...]`
/// in the game's stick.toml. Use `build_engine_libs(game_path)` when you
/// have a specific game in scope; this raw form is fine for callers that
/// have no game (REPL, library compiles).
pub fn build_libstone() -> Result<()> {
    if crate::project::is_release_layout() {
        return Ok(());
    }
    cargo_build_and_stage_lib("stone-rs", &["build", "-p", "stone-rs"], "stone_rs", "stone")?;
    Ok(())
}

/// Staged stem of the ImGui-free Stone cdylib that native C artifacts link.
/// It is a different library from the Lua engine's `libstone`, so it must not
/// share that path (receipts pin `tmp/libstone.*`; Lua processes load it).
pub const C_LIBSTONE_STEM: &str = "stone_c";

/// The Stone library name a native C artifact links (`-l<name>`): the
/// separately staged variant in a dev layout, the SDK's `stone` in a release
/// layout where nothing is staged.
pub fn c_libstone_link_name() -> &'static str {
    if crate::project::is_release_layout() { "stone" } else { C_LIBSTONE_STEM }
}

/// Build the ImGui-free Stone cdylib used by native C artifacts. The C
/// backend maps the tool bindings to their imgui-absent contract, so linking
/// the default-featured cdylib would leave those provider symbols undefined.
pub fn build_libstone_for_c() -> Result<()> {
    if crate::project::is_release_layout() {
        return Ok(());
    }
    // The separate C provider does not detach this consumer from its receipt.
    validate_receipted_lib("stone")?;
    cargo_build_and_stage_lib(
        "stone-rs for the C backend",
        &["build", "-p", "stone-rs", "--lib", "--no-default-features"],
        "stone_rs",
        C_LIBSTONE_STEM,
    )?;
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
    if validate_receipted_lib(lib.dst_stem)? {
        return Ok(());
    }
    build_optional_engine_lib_fresh(lib)
}

fn build_optional_engine_lib_fresh(lib: &EngineLib) -> Result<()> {
    let crate_path = PathBuf::from(crate::rock_dir()).join(lib.crate_name);
    if !crate_path.exists() {
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

/// Build and stage the core engine plus the optional libraries declared by
/// `game_paths`, then write the content-bound receipt inherited by a test
/// session. Ordinary commands do not create receipts and retain their usual
/// build-before-run behavior.
pub fn prepare_engine_build_receipt(game_paths: &[String], path: &Path) -> Result<()> {
    if crate::project::is_release_layout() {
        anyhow::bail!("engine build receipts are only meaningful in a source workspace");
    }
    let root = fs_err::canonicalize(crate::rock_dir())?;
    let inherited = engine_build_receipt_from_env()?.map(|(_, _, receipt)| receipt);
    cargo_build_and_stage_lib("stone-rs", &["build", "-p", "stone-rs"], "stone_rs", "stone")?;

    let mut optional = BTreeSet::new();
    for game_path in game_paths {
        for lib in game_engine_libs(game_path)? {
            optional.insert(lib.need);
        }
    }
    for lib in OPTIONAL_ENGINE_LIBS.iter().filter(|lib| optional.contains(lib.need)) {
        build_optional_engine_lib_fresh(lib)?;
    }

    let target_dir = cargo_target_dir(&root);
    let capture = |src_stem: &str, dst_stem: &str| -> Result<ReceiptLib> {
        if let Some(lib) = inherited.as_ref().and_then(|receipt| receipt_lib(receipt, dst_stem)) {
            // Cargo dep-info may describe a later C build, not this owned library.
            validate_receipt_lib(&root, lib)?;
            return Ok(lib.clone());
        }
        capture_receipt_lib(&root, &target_dir, src_stem, dst_stem)
    };
    let mut libs = vec![capture("stone_rs", "stone")?];
    for lib in OPTIONAL_ENGINE_LIBS.iter().filter(|lib| optional.contains(lib.need)) {
        libs.push(capture(lib.src_stem, lib.dst_stem)?);
    }
    let receipt = EngineBuildReceipt {
        version: ENGINE_BUILD_RECEIPT_VERSION,
        workspace: root.to_string_lossy().into_owned(),
        libs,
    };
    crate::project::write_atomic(path, engine_build_receipt_json(&receipt)?)?;
    Ok(())
}

fn engine_build_receipt_json(receipt: &EngineBuildReceipt) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec_pretty(&serde_json::json!({
        "version": receipt.version,
        "workspace": receipt.workspace,
        "libs": receipt.libs.iter().map(|lib| serde_json::json!({
            "src_stem": lib.src_stem,
            "dst_stem": lib.dst_stem,
            "inputs": lib.inputs,
            "inputs_sha1": lib.inputs_sha1,
            "staged_sha1": lib.staged_sha1,
        })).collect::<Vec<_>>(),
    }))?)
}

fn load_engine_build_receipt(root: &Path, path: &Path) -> Result<EngineBuildReceipt> {
    let bytes = fs_err::read(path).map_err(|error| {
        anyhow::anyhow!("cannot read engine build receipt `{}`: {error}", path.display())
    })?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| {
        anyhow::anyhow!("invalid engine build receipt `{}`: {error}", path.display())
    })?;
    let object = value.as_object().ok_or_else(|| {
        anyhow::anyhow!("invalid engine build receipt `{}`: root is not an object", path.display())
    })?;
    let required_string = |object: &serde_json::Map<String, serde_json::Value>, field: &str| {
        object
            .get(field)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| anyhow::anyhow!("invalid engine build receipt: `{field}` is not a string"))
    };
    let version = object.get("version").and_then(serde_json::Value::as_u64).ok_or_else(|| {
        anyhow::anyhow!("invalid engine build receipt `{}`: `version` is not an integer", path.display())
    })? as u32;
    let workspace_name = required_string(object, "workspace")?;
    let lib_values = object.get("libs").and_then(serde_json::Value::as_array).ok_or_else(|| {
        anyhow::anyhow!("invalid engine build receipt `{}`: `libs` is not an array", path.display())
    })?;
    let mut libs = Vec::with_capacity(lib_values.len());
    for value in lib_values {
        let lib = value.as_object().ok_or_else(|| {
            anyhow::anyhow!("invalid engine build receipt `{}`: a library is not an object", path.display())
        })?;
        let input_values = lib.get("inputs").and_then(serde_json::Value::as_array).ok_or_else(|| {
            anyhow::anyhow!("invalid engine build receipt `{}`: `inputs` is not an array", path.display())
        })?;
        let inputs = input_values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| anyhow::anyhow!("invalid engine build receipt: an input is not a string"))
            })
            .collect::<Result<Vec<_>>>()?;
        libs.push(ReceiptLib {
            src_stem: required_string(lib, "src_stem")?,
            dst_stem: required_string(lib, "dst_stem")?,
            inputs,
            inputs_sha1: required_string(lib, "inputs_sha1")?,
            staged_sha1: required_string(lib, "staged_sha1")?,
        });
    }
    let receipt = EngineBuildReceipt { version, workspace: workspace_name, libs };
    if receipt.version != ENGINE_BUILD_RECEIPT_VERSION {
        anyhow::bail!(
            "engine build receipt `{}` has version {}, expected {}",
            path.display(),
            receipt.version,
            ENGINE_BUILD_RECEIPT_VERSION
        );
    }
    let workspace = fs_err::canonicalize(&receipt.workspace).map_err(|error| {
        anyhow::anyhow!("engine build receipt workspace `{}` is unavailable: {error}", receipt.workspace)
    })?;
    if workspace != root {
        anyhow::bail!(
            "engine build receipt `{}` belongs to `{}`, not `{}`",
            path.display(),
            workspace.display(),
            root.display()
        );
    }
    Ok(receipt)
}

fn receipt_lib<'a>(receipt: &'a EngineBuildReceipt, dst_stem: &str) -> Option<&'a ReceiptLib> {
    receipt.libs.iter().find(|lib| lib.dst_stem == dst_stem)
}

fn engine_build_receipt_from_env() -> Result<Option<(PathBuf, PathBuf, EngineBuildReceipt)>> {
    let Some(path) = std::env::var_os(ENGINE_BUILD_RECEIPT_ENV).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let root = fs_err::canonicalize(crate::rock_dir())?;
    let path = PathBuf::from(path);
    let receipt = load_engine_build_receipt(&root, &path)?;
    Ok(Some((root, path, receipt)))
}

fn validate_receipted_lib(dst_stem: &str) -> Result<bool> {
    let Some((root, receipt_path, receipt)) = engine_build_receipt_from_env()? else {
        return Ok(false);
    };
    let Some(lib) = receipt_lib(&receipt, dst_stem) else {
        if dst_stem == "stone" {
            anyhow::bail!(
                "engine build receipt `{}` does not contain core libstone",
                receipt_path.display()
            );
        }
        // Uncaptured providers, including stone_c, still build on demand.
        return Ok(false);
    };
    validate_receipt_lib(&root, lib)?;
    Ok(true)
}

/// Build the engine cdylibs required for a specific game. Always builds
/// stone-rs; additionally builds each optional lib the game's stick.toml
/// declares under `[engine] needs`. A missing stick.toml is core-only; a
/// malformed manifest or unknown engine need is rejected here.
pub fn build_engine_libs(game_path: &str) -> Result<()> {
    let optional = game_engine_libs(game_path)?;
    build_libstone()?;
    for lib in optional {
        build_optional_engine_lib(lib)?;
    }
    Ok(())
}

/// Build the engine cdylibs needed by a native C artifact. Its core Stone
/// provider deliberately omits the optional cimgui dependency.
pub fn build_engine_libs_for_c(game_path: &str) -> Result<()> {
    build_libstone_for_c()?;
    for lib in game_engine_libs(game_path)? {
        build_optional_engine_lib(lib)?;
    }
    Ok(())
}
