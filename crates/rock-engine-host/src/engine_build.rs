//! Core engine build, staging and source receipts. Project libraries are
//! built separately from their package-owned Cargo manifests.

pub use crate::native_libraries::{build_optional_engine_lib, game_engine_libs};
use anyhow::{Context, Result};
use rock_host::cargo::run_cargo;
use sha1::{Digest, Sha1};
use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};

pub const ENGINE_BUILD_RECEIPT_ENV: &str = "ROCK_ENGINE_BUILD_RECEIPT";
/// Interactive launches select immutable tools; source builds are opt-in.
pub fn prebuilt_game() -> bool {
    std::env::var("ROCK_PREBUILT_GAME").as_deref() == Ok("1")
}

pub fn require_source_build() -> Result<()> {
    if prebuilt_game() {
        anyhow::bail!(
            "prebuilt game toolchain cannot rebuild native/compiler inputs; select a source toolchain to rebuild them"
        );
    }
    Ok(())
}

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

/// The Cargo target directory for `rock_root`. Cargo resolves a relative
/// `CARGO_TARGET_DIR` against the directory it was invoked from, and
/// `run_cargo` runs cargo with `current_dir(rock_host::rock_dir())`; resolve it the
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

pub(crate) fn cdylib_name(stem: &str) -> String {
    #[cfg(target_os = "linux")]
    return format!("lib{stem}.so");
    #[cfg(target_os = "macos")]
    return format!("lib{stem}.dylib");
    #[cfg(target_os = "windows")]
    return format!("{stem}.dll");
}

fn staged_cdylib_path(root: &Path, dst_stem: &str) -> PathBuf {
    root.join("tmp").join(cdylib_name(dst_stem))
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
            let path = if path.is_absolute() {
                path
            } else {
                root.join(path)
            };
            if path.is_file() {
                let path = fs_err::canonicalize(path)?;
                if let Ok(relative) = path.strip_prefix(&root) {
                    inputs.insert(relative.to_string_lossy().into_owned());
                }
            }
        }
    }
    let mut manifests = BTreeSet::new();
    for input in &inputs {
        let mut directory = root.join(input);
        while directory.pop() && directory.starts_with(&root) {
            let manifest = directory.join("Cargo.toml");
            if manifest.is_file() {
                manifests.insert(manifest.strip_prefix(&root)?.to_string_lossy().into_owned());
                break;
            }
        }
    }
    inputs.extend(manifests);
    for extra in ["Cargo.toml", "Cargo.lock", ".cargo/config.toml"] {
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
            anyhow::anyhow!(
                "engine build input `{}` is unavailable: {error}",
                path.display()
            )
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

    #[test]
    fn identical_staging_preserves_transformed_publication_and_receipts() {
        let (root, _) = receipt_fixture();
        let target = root.path().join("target");
        let src = target.join("artifact");
        let dst = staged_cdylib_path(root.path(), "probe");
        fs_err::write(&src, b"unsigned artifact").unwrap();
        let transform = |path: &Path| -> Result<()> {
            fs_err::write(path, b"signed artifact with stable install name")?;
            Ok(())
        };
        stage_cdylib_from(&src, &dst, transform).unwrap();
        let first = fs_err::metadata(&dst).unwrap();
        let receipt = capture_receipt_lib(root.path(), &target, "probe", "probe").unwrap();

        stage_cdylib_from(&src, &dst, |_| {
            panic!("identical artifact was transformed again")
        })
        .unwrap();
        let second = fs_err::metadata(&dst).unwrap();
        assert_eq!(first.modified().unwrap(), second.modified().unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(first.ino(), second.ino(), "publication was replaced");
        }
        validate_receipt_lib(root.path(), &receipt).unwrap();
        let other = capture_receipt_lib(root.path(), &target, "probe", "probe").unwrap();
        validate_receipt_lib(root.path(), &other).unwrap();
        assert_eq!(receipt.staged_sha1, other.staged_sha1);
    }

    fn stage_fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let src = root.path().join("cargo-artifact");
        let dst = root.path().join("staged-artifact");
        fs_err::write(&src, b"first artifact").unwrap();
        (root, src, dst)
    }

    #[test]
    fn changed_source_and_damaged_stage_are_republished() {
        let (_root, src, dst) = stage_fixture();
        stage_cdylib_from(&src, &dst, |_| Ok(())).unwrap();
        fs_err::write(&src, b"changed Cargo artifact").unwrap();
        stage_cdylib_from(&src, &dst, |_| Ok(())).unwrap();
        assert_eq!(fs_err::read(&dst).unwrap(), fs_err::read(&src).unwrap());
        fs_err::write(&dst, b"damaged published artifact").unwrap();
        stage_cdylib_from(&src, &dst, |_| Ok(())).unwrap();
        assert_eq!(fs_err::read(&dst).unwrap(), fs_err::read(&src).unwrap());
        fs_err::remove_file(&dst).unwrap();
        stage_cdylib_from(&src, &dst, |_| Ok(())).unwrap();
        assert_eq!(fs_err::read(&dst).unwrap(), fs_err::read(&src).unwrap());
        fs_err::remove_file(&src).unwrap();
        assert!(stage_cdylib_from(&src, &dst, |_| Ok(())).is_err());
    }

    #[test]
    fn absent_damaged_or_old_identity_requires_staging() {
        let (_root, src, dst) = stage_fixture();
        fs_err::copy(&src, &dst).unwrap();
        let mut preparations = 0;
        let mut stage = || {
            stage_cdylib_from(&src, &dst, |_| {
                preparations += 1;
                Ok(())
            })
            .unwrap();
        };
        stage();
        fs_err::write(stage_identity_path(&dst), b"broken record").unwrap();
        stage();
        let mut identity: serde_json::Value =
            serde_json::from_slice(&fs_err::read(stage_identity_path(&dst)).unwrap()).unwrap();
        identity["version"] = serde_json::json!(STAGE_IDENTITY_VERSION + 1);
        fs_err::write(
            stage_identity_path(&dst),
            serde_json::to_vec(&identity).unwrap(),
        )
        .unwrap();
        stage();
        assert_eq!(
            preparations, 3,
            "unproven publications must not skip staging"
        );
    }

    #[test]
    fn concurrent_identical_staging_publishes_once() {
        let (root, _) = receipt_fixture();
        let src = root.path().join("target/artifact");
        let dst = staged_cdylib_path(root.path(), "probe");
        let target = root.path().join("target");
        fs_err::write(&src, b"concurrent artifact").unwrap();
        let starts = std::sync::Barrier::new(8);
        let preparations = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|scope| {
            let mut threads = Vec::new();
            for _ in 0..8 {
                threads.push(scope.spawn(|| {
                    starts.wait();
                    stage_cdylib_from(&src, &dst, |_| {
                        // Probe the OS lock while production is preparing the
                        // publication, independent of thread scheduling.
                        let probe = std::fs::OpenOptions::new()
                            .read(true)
                            .write(true)
                            .open(stage_identity_path(&dst).with_extension("lock"))
                            .unwrap();
                        assert!(matches!(
                            probe.try_lock(),
                            Err(std::fs::TryLockError::WouldBlock)
                        ));
                        preparations.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        Ok(())
                    })
                    .unwrap();
                    capture_receipt_lib(root.path(), &target, "probe", "probe").unwrap()
                }));
            }
            for thread in threads {
                validate_receipt_lib(root.path(), &thread.join().unwrap()).unwrap();
            }
        });
        assert_eq!(preparations.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(fs_err::read(&dst).unwrap(), fs_err::read(&src).unwrap());
    }

    #[test]
    fn failed_staging_preserves_publication_and_cleans_scratch_files() {
        let (root, src, dst) = stage_fixture();
        stage_cdylib_from(&src, &dst, |_| Ok(())).unwrap();
        let before = fs_err::read(&dst).unwrap();
        let entries = || -> BTreeSet<PathBuf> {
            fs_err::read_dir(root.path())
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect()
        };
        let paths = entries();
        let identity = fs_err::read(stage_identity_path(&dst)).unwrap();
        fs_err::write(&src, b"new artifact").unwrap();
        let result = stage_cdylib_from(&src, &dst, |path| {
            fs_err::write(path, b"partial transformation")?;
            anyhow::bail!("intentional preparation failure")
        });
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("intentional preparation failure")
        );
        assert_eq!(fs_err::read(&dst).unwrap(), before);
        assert_eq!(entries(), paths, "failed stage leaked scratch files");
        assert_eq!(fs_err::read(stage_identity_path(&dst)).unwrap(), identity);
        let panic = std::panic::catch_unwind(|| {
            let _ = stage_cdylib_from(&src, &dst, |_| panic!("intentional preparation panic"));
        });
        assert!(panic.is_err());
        assert_eq!(fs_err::read(&dst).unwrap(), before);
        assert_eq!(entries(), paths, "panicked stage leaked scratch files");
        assert_eq!(fs_err::read(stage_identity_path(&dst)).unwrap(), identity);
        stage_cdylib_from(&src, &dst, |_| Ok(())).unwrap();
        assert_eq!(fs_err::read(&dst).unwrap(), fs_err::read(&src).unwrap());
        assert_eq!(entries(), paths, "successful stage leaked scratch files");
    }

    #[test]
    fn package_native_declaration_resolves_without_a_registry() {
        let dir = tempfile::tempdir().unwrap();
        fs_err::write(dir.path().join("stick.toml"),
            "[package]\nname = \"probe\"\n[native]\nmanifest = \"Cargo.toml\"\nlibrary = \"probe\"\n").unwrap();
        fs_err::write(dir.path().join("Cargo.toml"),
            "[package]\nname = \"probe\"\nversion = \"0.1.0\"\n[lib]\npath = \"lib.rs\"\ncrate-type = [\"cdylib\"]\n[workspace]\n").unwrap();
        fs_err::write(dir.path().join("lib.rs"), "pub fn probe() {}\n").unwrap();
        let libs = game_engine_libs(dir.path().to_str().unwrap()).unwrap();
        assert_eq!(libs.len(), 1);
        assert_eq!(libs[0].dst_stem, "probe");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn staged_cdylib_uses_its_stable_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("probe.c");
        let dylib = dir.path().join("libsource.dylib");
        fs_err::write(&source, "int probe(void) { return 1; }\n").expect("write source");
        let compile = std::process::Command::new("clang")
            .args(["-dynamiclib"])
            .arg(&source)
            .arg("-o")
            .arg(&dylib)
            .status()
            .expect("run clang");
        assert!(compile.success(), "compile probe dylib");

        set_macos_install_name(&dylib, "stone").expect("set install name");
        let output = std::process::Command::new("otool")
            .arg("-D")
            .arg(&dylib)
            .output()
            .expect("run otool");
        assert!(output.status.success(), "read install name");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.lines().any(|line| line == "@rpath/libstone.dylib"),
            "{stdout}"
        );
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

        fs_err::write(
            root.path().join("stone-rs/src/lib.rs"),
            "pub fn version() -> u32 { 2 }\n",
        )
        .expect("mutate source");
        let error =
            validate_receipt_lib(root.path(), &receipt).expect_err("stale input must reject");
        assert!(error.to_string().contains("inputs changed"), "{error:#}");
    }

    #[test]
    fn a_shared_engine_receipt_rejects_a_replaced_staged_library() {
        let (root, receipt) = receipt_fixture();
        fs_err::write(
            staged_cdylib_path(root.path(), "probe"),
            b"different artifact",
        )
        .expect("replace stage");
        let error =
            validate_receipt_lib(root.path(), &receipt).expect_err("wrong dylib must reject");
        assert!(
            error.to_string().contains("staged library changed"),
            "{error:#}"
        );
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
        fs_err::write(&path, engine_build_receipt_json(&receipt).expect("encode"))
            .expect("receipt");

        let advertised: serde_json::Value =
            serde_json::from_slice(&fs_err::read(&path).expect("read receipt"))
                .expect("receipt JSON");
        assert_eq!(advertised["prebuilt_game_protocol"], 1);
        let loaded = load_engine_build_receipt(&root_path, &path).expect("load");
        assert_eq!(loaded.version, ENGINE_BUILD_RECEIPT_VERSION);
        assert_eq!(loaded.workspace, receipt.workspace);
        assert_eq!(loaded.libs.len(), 1);
        validate_receipt_lib(&root_path, &loaded.libs[0]).expect("loaded identity remains valid");
    }

    // A budgeting parent caps compile work by phase; a cold engine build
    // billed to `spawn` was cut off under load.
    #[test]
    fn an_engine_build_reports_its_phase_before_cargo_runs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let phase = dir.path().join("phase");
        // SAFETY: nextest runs each test in its own process. Restored below.
        let prev = std::env::var_os(rock_host::process_management::SPAWN_PHASE_FILE_ENV);
        unsafe { std::env::set_var(rock_host::process_management::SPAWN_PHASE_FILE_ENV, &phase) };
        // An unknown flag makes cargo fail before it touches any lock.
        let built = cargo_build_and_stage_lib(
            "phase probe",
            &["--rock-phase-probe-unknown-flag"],
            "x",
            "phase_probe",
        );
        unsafe {
            match prev {
                Some(v) => {
                    std::env::set_var(rock_host::process_management::SPAWN_PHASE_FILE_ENV, v)
                }
                None => std::env::remove_var(rock_host::process_management::SPAWN_PHASE_FILE_ENV),
            }
        }
        assert!(built.is_err(), "cargo must reject the probe flag");
        assert_eq!(
            fs_err::read_to_string(&phase).ok().as_deref(),
            Some("engine-build")
        );
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
    if rock_host::project::is_release_layout() {
        return Ok(());
    }
    if validate_receipted_lib(dst_stem)? {
        return Ok(());
    }
    require_source_build()?;
    rock_host::process_management::report_spawn_phase("engine-build");
    run_cargo(cargo_args, &format!("Failed to build {}", display_name))?;
    stage_cdylib(src_stem, dst_stem)
}

/// Stage a freshly built cdylib under its stable loader name. Accepted build
/// receipts retain ownership; otherwise Cargo's source artifact and the final
/// transformed publication must both match the recorded staging identity.
pub fn stage_cdylib(src_stem: &str, dst_stem: &str) -> Result<()> {
    // A receipt owns the destination even for callers that bypass the builder.
    if validate_receipted_lib(dst_stem)? {
        return Ok(());
    }
    let rock_root = PathBuf::from(rock_host::rock_dir());
    let target_debug = cargo_target_dir(&rock_root).join("debug");
    let dev_tmp = rock_root.join("tmp");
    fs_err::create_dir_all(&dev_tmp)?;

    let src = target_debug.join(cdylib_name(src_stem));
    let dst = staged_cdylib_path(&rock_root, dst_stem);

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
    stage_cdylib_from(&src, &dst, |staging| {
        prepare_staged_cdylib(staging, dst_stem)
    })
}

// Bump when the install-name/signing contract changes. Source bytes and final
// bytes differ on macOS, so neither identity can substitute for the other.
const STAGE_IDENTITY_VERSION: u32 = 1;

#[derive(serde::Deserialize, serde::Serialize)]
struct StageIdentity {
    version: u32,
    source_sha1: String,
    staged_sha1: String,
}

fn stage_identity_path(dst: &Path) -> PathBuf {
    let mut path = dst.as_os_str().to_os_string();
    path.push(".stage.json");
    PathBuf::from(path)
}

pub(crate) fn stage_cdylib_from(
    src: &Path,
    dst: &Path,
    prepare: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    let identity_path = stage_identity_path(dst);
    // Keep one lock inode: replacing/unlinking it would let waiters publish
    // against different locks. File locks also serialize threads of one process.
    let lock_path = identity_path.with_extension("lock");
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("cannot open engine staging lock `{}`", lock_path.display()))?;
    lock.lock()
        .with_context(|| format!("cannot lock engine staging `{}`", dst.display()))?;
    let source_sha1 = sha1_file(src)?;
    let recorded = (|| -> Result<StageIdentity> {
        let identity: StageIdentity = serde_json::from_slice(&fs_err::read(&identity_path)?)?;
        anyhow::ensure!(
            identity.version == STAGE_IDENTITY_VERSION,
            "staging contract changed"
        );
        Ok(identity)
    })();
    match recorded {
        Ok(identity) if identity.source_sha1 == source_sha1 => match sha1_file(dst) {
            Ok(staged_sha1) if staged_sha1 == identity.staged_sha1 => {
                log::info!(
                    "Skipping engine staging for `{}`: source and staged content identities match",
                    dst.display()
                );
                return Ok(());
            }
            Ok(_) => log::debug!("Restaging `{}`: staged content changed", dst.display()),
            Err(error) => log::debug!(
                "Restaging `{}`: staged content unavailable: {error:#}",
                dst.display()
            ),
        },
        Ok(_) => log::debug!("Restaging `{}`: source content changed", dst.display()),
        // An absent, old or damaged record proves nothing. Reconstruct it only
        // by staging the current source, never by trusting an existing library.
        Err(error) => log::debug!(
            "Restaging `{}`: no valid staging identity: {error:#}",
            dst.display()
        ),
    }

    let staging =
        tempfile::NamedTempFile::new_in(dst.parent().context("staged cdylib has no parent")?)?
            .into_temp_path();
    fs_err::copy(src, &staging)?;
    anyhow::ensure!(
        sha1_file(&staging)? == source_sha1,
        "engine cdylib `{}` changed while staging; retry the build",
        src.display()
    );
    prepare(&staging)?;
    let identity = StageIdentity {
        version: STAGE_IDENTITY_VERSION,
        source_sha1,
        staged_sha1: sha1_file(&staging)?,
    };
    fs_err::rename(&staging, dst)?;
    rock_host::project::write_atomic(&identity_path, serde_json::to_vec(&identity)?)?;
    Ok(())
}

pub(crate) fn prepare_staged_cdylib(_staging: &Path, _dst_stem: &str) -> Result<()> {
    // macOS only: strip rustc's `linker-signed` flag by re-signing
    // adhoc. Sequoia 15.x's syspolicyd evaluates (adhoc |
    // linker-signed) through a stricter path than plain adhoc and may
    // SIGKILL dlopen with no userspace error for fresh cdhashes — see
    // XProtect data update pushed 2026-04-24. Plain `--sign -` is the
    // standard adhoc resign. Done in isolation on the per-call staging
    // path so concurrent stagers can't read a mid-resign file.
    #[cfg(target_os = "macos")]
    {
        let staging = _staging;
        set_macos_install_name(staging, _dst_stem)?;
        let status = std::process::Command::new("codesign")
            .args(["--force", "--sign", "-"])
            .arg(staging)
            .status()?;
        if !status.success() {
            anyhow::bail!("codesign --force --sign - failed for {}", staging.display());
        }
    }

    Ok(())
}

#[cfg(target_os = "macos")]
fn set_macos_install_name(path: &Path, dst_stem: &str) -> Result<()> {
    let status = std::process::Command::new("install_name_tool")
        .args(["-id", &format!("@rpath/lib{dst_stem}.dylib")])
        .arg(path)
        .status()?;
    if !status.success() {
        anyhow::bail!("install_name_tool -id failed for {}", path.display());
    }
    Ok(())
}

/// Use the installed core, or build and stage it in a source checkout.
pub fn build_libstone() -> Result<()> {
    if rock_host::project::is_release_layout() {
        return Ok(());
    }
    cargo_build_and_stage_lib(
        "stone-rs",
        &["build", "-p", "stone-rs"],
        "stone_rs",
        "stone",
    )?;
    Ok(())
}

/// Build and stage the core engine plus the optional libraries declared by
/// `game_paths`, then write the content-bound receipt inherited by a test
/// session. Ordinary commands do not create receipts and retain their usual
/// build-before-run behavior.
pub fn prepare_engine_build_receipt(game_paths: &[String], path: &Path) -> Result<()> {
    if rock_host::project::is_release_layout() {
        anyhow::bail!("engine build receipts are only meaningful in a source workspace");
    }
    let root = fs_err::canonicalize(rock_host::rock_dir())?;
    let inherited = engine_build_receipt_from_env()?.map(|(_, _, receipt)| receipt);
    cargo_build_and_stage_lib(
        "stone-rs",
        &["build", "-p", "stone-rs"],
        "stone_rs",
        "stone",
    )?;

    for game_path in game_paths {
        for lib in game_engine_libs(game_path)? {
            build_optional_engine_lib(&lib)?;
        }
    }

    let target_dir = cargo_target_dir(&root);
    let capture = |src_stem: &str, dst_stem: &str| -> Result<ReceiptLib> {
        if let Some(lib) = inherited
            .as_ref()
            .and_then(|receipt| receipt_lib(receipt, dst_stem))
        {
            // Cargo dep-info may describe a later C build, not this owned library.
            validate_receipt_lib(&root, lib)?;
            return Ok(lib.clone());
        }
        capture_receipt_lib(&root, &target_dir, src_stem, dst_stem)
    };
    let libs = vec![capture("stone_rs", "stone")?];
    let receipt = EngineBuildReceipt {
        version: ENGINE_BUILD_RECEIPT_VERSION,
        workspace: root.to_string_lossy().into_owned(),
        libs,
    };
    rock_host::project::write_atomic(path, engine_build_receipt_json(&receipt)?)?;
    Ok(())
}

fn engine_build_receipt_json(receipt: &EngineBuildReceipt) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec_pretty(&serde_json::json!({
        "version": receipt.version,
        // Emitted by the compiler, so launchers can reject older binaries
        // that would ignore ROCK_PREBUILT_GAME and invoke Cargo anyway.
        "prebuilt_game_protocol": 1,
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
        anyhow::anyhow!(
            "cannot read engine build receipt `{}`: {error}",
            path.display()
        )
    })?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| {
        anyhow::anyhow!("invalid engine build receipt `{}`: {error}", path.display())
    })?;
    let object = value.as_object().ok_or_else(|| {
        anyhow::anyhow!(
            "invalid engine build receipt `{}`: root is not an object",
            path.display()
        )
    })?;
    let required_string = |object: &serde_json::Map<String, serde_json::Value>, field: &str| {
        object
            .get(field)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| {
                anyhow::anyhow!("invalid engine build receipt: `{field}` is not a string")
            })
    };
    let version = object
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "invalid engine build receipt `{}`: `version` is not an integer",
                path.display()
            )
        })? as u32;
    let workspace_name = required_string(object, "workspace")?;
    let lib_values = object
        .get("libs")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "invalid engine build receipt `{}`: `libs` is not an array",
                path.display()
            )
        })?;
    let mut libs = Vec::with_capacity(lib_values.len());
    for value in lib_values {
        let lib = value.as_object().ok_or_else(|| {
            anyhow::anyhow!(
                "invalid engine build receipt `{}`: a library is not an object",
                path.display()
            )
        })?;
        let input_values = lib
            .get("inputs")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "invalid engine build receipt `{}`: `inputs` is not an array",
                    path.display()
                )
            })?;
        let inputs = input_values
            .iter()
            .map(|value| {
                value.as_str().map(str::to_owned).ok_or_else(|| {
                    anyhow::anyhow!("invalid engine build receipt: an input is not a string")
                })
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
    let receipt = EngineBuildReceipt {
        version,
        workspace: workspace_name,
        libs,
    };
    if receipt.version != ENGINE_BUILD_RECEIPT_VERSION {
        anyhow::bail!(
            "engine build receipt `{}` has version {}, expected {}",
            path.display(),
            receipt.version,
            ENGINE_BUILD_RECEIPT_VERSION
        );
    }
    let workspace = fs_err::canonicalize(&receipt.workspace).map_err(|error| {
        anyhow::anyhow!(
            "engine build receipt workspace `{}` is unavailable: {error}",
            receipt.workspace
        )
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
    let Some(path) = std::env::var_os(ENGINE_BUILD_RECEIPT_ENV).filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    let root = fs_err::canonicalize(rock_host::rock_dir())?;
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
        // Uncaptured providers still build on demand.
        return Ok(false);
    };
    validate_receipt_lib(&root, lib)?;
    Ok(true)
}

pub fn build_engine_libs(game_path: &str) -> Result<()> {
    let optional = game_engine_libs(game_path)?;
    build_libstone()?;
    for lib in optional {
        build_optional_engine_lib(&lib)?;
    }
    Ok(())
}
