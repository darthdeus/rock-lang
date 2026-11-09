//! Shared identity and storage for the sim and frame compile caches.

use crate::registry::{ProgramKind, RuntimeServices};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::time::Duration;

// Both compile caches share this identity and storage path so invalidation and
// publication stay identical for sim receipts and frame artifacts.
// Compile-related prefixes default to keyed; these are runner and diagnostics.
const COMPILE_CACHE_KEYED_ENV_PREFIXES: &[&str] = &["ROCK_", "SKULL_", "BONE_"];
const COMPILE_CACHE_KEYED_ENV_NAMES: &[&str] = &[
    "GAME_ART_VAULT",
    "GAME_SOURCE_DIR",
    "CARGO_TARGET_DIR",
    "HOME",
];
const COMPILE_CACHE_UNKEYED_ENV: &[&str] = &[
    "ROCK_SIM_COMPILE_CACHE",
    "ROCK_SIM_COMPILE_CACHE_DIR",
    "ROCK_SIM_COMPILE_CACHE_MAX_MB",
    "ROCK_SIM_COMPILE_RECEIPT_DIR",
    "ROCK_SIM_COMPILE_RECEIPT_PREPARE",
    "ROCK_SIM_COMPILE_SELECTION",
    "ROCK_BUILD_COMMAND",
    "ROCK_SKULL_ROUTE_PROFILE",
    "ROCK_FRAME_COMPILE_CACHE",
    "ROCK_FRAME_COMPILE_CACHE_DIR",
    "ROCK_FRAME_COMPILE_CACHE_MAX_MB",
    "ROCK_FRAME_COMPILE_CACHE_STATS",
    "ROCK_FRAME_TIMEOUT_SECS",
    "ROCK_FRAME_WATCHDOG",
    "ROCK_PERF_STARTUP_WATCHDOG",
    "ROCK_RUN_SIM",
    "ROCK_TEST_SEED",
    "ROCK_HEADLESS_SCOPE",
    "ROCK_LUAJIT",
    "ROCK_AGENT_DAEMON_BIN",
    "ROCK_AGENT_DAEMON_SOCK",
    "ROCK_ENGINE_BUILD_RECEIPT",
    "ROCK_SKIP_ENGINE_BUILD",
    "ROCK_NUM_THREADS",
    "ROCK_TEST_TMP",
    "ROCK_TEST_FILTER",
    "ROCK_TEST_SHARDS",
    "ROCK_TEST_SHARD_TESTS",
    "ROCK_TEST_TIMINGS",
    "ROCK_TEST_VERBOSE",
    "ROCK_TEST_GC_COLLECT_MB",
    "ROCK_TEST_RECEIPT_RELEASE_HELPER",
    "ROCK_COMPILE_SLOT_TEST_CHILD_DIR",
    "ROCK_VERBOSE_TIMING",
    "ROCK_UI_TRACE_DIR",
    "ROCK_COMPILE_STATS",
    crate::compile_slots::SPAWN_PHASE_FILE_ENV,
    crate::compile_slots::ADMISSION_ENV,
    "ROCK_INVOCATION_CONTEXT_KEYS",
    "ROCK_COMPILE_SLOTS",
    "ROCK_COMPILE_CLASS",
    "ROCK_COMPILE_SLOT_DIR",
    "ROCK_JOB_ID",
    "ROCK_INVOCATION_LOG",
    crate::invocation_log::PARENT_ENV,
    "ROCK_CHANGE_ID",
    "ROCK_LOG_EPOCH_MS",
    "ROCK_SIM_COMPILE_TIMING",
    "ROCK_SIM_KEEP_BUILD",
    "ROCK_SIM_LIFECYCLE_LOG",
    "ROCK_SIM_LOG_DIR",
    "ROCK_SIM_MEMORY_MB",
    "ROCK_SIM_SLOT_LOCK",
    "ROCK_SIM_SPAWN_BARRIER",
    "ROCK_SIM_TIMEOUT_SECS",
    "ROCK_SIM_WORKER_SLOTS",
    "ROCK_SIM_WORKER_SLOT_DIR",
    "ROCK_SIM_WORKER_WAIT_LOG",
];
const COMPILE_CACHE_UNKEYED_ENV_PREFIXES: &[&str] = &["ROCK_JOBS_", "ROCK_SIM_WARN_"];

pub(crate) fn cache_sha256_bytes(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}

pub(crate) fn cache_sha256_file(path: &std::path::Path) -> Result<String> {
    use anyhow::Context;
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    std::io::copy(
        &mut std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?,
        &mut hasher,
    )
    .with_context(|| format!("hash {}", path.display()))?;
    Ok(format!("{:x}", hasher.finalize()))
}

pub(crate) fn compile_cache_environment_is_keyed(name: &str) -> bool {
    (COMPILE_CACHE_KEYED_ENV_PREFIXES
        .iter()
        .any(|prefix| name.starts_with(prefix))
        || COMPILE_CACHE_KEYED_ENV_NAMES.contains(&name))
        && !COMPILE_CACHE_UNKEYED_ENV.contains(&name)
        && !COMPILE_CACHE_UNKEYED_ENV_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
}

fn compile_cache_environment_sha256() -> String {
    let keyed = std::env::vars_os()
        .filter_map(|(name, value)| {
            let name = name.to_string_lossy().into_owned();
            compile_cache_environment_is_keyed(&name)
                .then(|| (name, value.to_string_lossy().into_owned()))
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    cache_sha256_bytes(serde_json::json!(keyed).to_string().as_bytes())
}

fn compile_cache_build_vars_sha256(inputs: &bone_semantic::inference::ComptimeInputs) -> String {
    let mut volatile = inputs
        .volatile_vars
        .iter()
        .map(|name| name.as_str())
        .collect::<Vec<_>>();
    volatile.sort_unstable();
    let mut vars = inputs
        .vars
        .iter()
        .filter(|(name, _)| {
            !inputs.volatile_vars.contains(*name) && !inputs.source_fingerprints.contains_key(*name)
        })
        .map(|(name, var)| (name.as_str(), var.value.as_str(), var.secret))
        .collect::<Vec<_>>();
    vars.sort_unstable();
    let mut source_fingerprints = inputs
        .source_fingerprints
        .iter()
        .map(|(name, fingerprint)| {
            let mut roots = fingerprint
                .roots
                .iter()
                .map(|root| root.as_str())
                .collect::<Vec<_>>();
            let mut files = fingerprint
                .files
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>();
            let mut fingerprint_vars = fingerprint
                .vars
                .iter()
                .map(|var| var.as_str())
                .collect::<Vec<_>>();
            roots.sort_unstable();
            files.sort_unstable();
            fingerprint_vars.sort_unstable();
            (
                name.as_str(),
                fingerprint.secret,
                roots,
                files,
                fingerprint_vars,
            )
        })
        .collect::<Vec<_>>();
    source_fingerprints.sort_unstable_by(|left, right| left.0.cmp(right.0));
    let canonical = serde_json::json!({
        "channel": inputs.channel,
        "modules_mode": inputs.modules_mode,
        "embed_root": inputs.embed_root.as_ref().map(|root| root.to_string_lossy().into_owned()),
        "volatile": volatile,
        "vars": vars,
        "source_fingerprints": source_fingerprints,
    });
    cache_sha256_bytes(canonical.to_string().as_bytes())
}

fn compile_cache_source_fingerprint_support_sha256(
    inputs: &bone_semantic::inference::ComptimeInputs,
) -> Result<String> {
    use sha2::{Digest, Sha256};
    use std::borrow::Cow;

    let mut files = inputs
        .source_fingerprints
        .values()
        .flat_map(|fingerprint| fingerprint.files.iter().map(String::as_str))
        .collect::<Vec<_>>();
    files.sort_unstable();
    files.dedup();

    let mut hasher = Sha256::new();
    for file in files {
        let contents: Cow<'_, [u8]> = if let Some(name) = file
            .strip_prefix("stdlib/")
            .or_else(|| rock_stdlib::stdlib_file_name(file))
        {
            let stdlib_file = rock_stdlib::stdlib_files()
                .iter()
                .find(|stdlib_file| stdlib_file.name == name)
                .with_context(|| format!("source fingerprint support file stdlib/{name} is not in the selected stdlib"))?;
            Cow::Borrowed(stdlib_file.contents.as_bytes())
        } else {
            let canonical_support = crate::comptime::resolve_source_fingerprint_support_path(
                file,
                inputs.embed_root.as_deref(),
            )?;
            Cow::Owned(std::fs::read(&canonical_support).with_context(|| {
                format!(
                    "read source fingerprint support file {}",
                    canonical_support.display()
                )
            })?)
        };
        hasher.update((file.len() as u64).to_le_bytes());
        hasher.update(file.as_bytes());
        hasher.update((contents.len() as u64).to_le_bytes());
        hasher.update(contents.as_ref());
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn compile_cache_stdlib_digest() -> serde_json::Value {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    for file in rock_stdlib::stdlib_files() {
        hasher.update(file.name.as_bytes());
        hasher.update([0]);
        hasher.update((file.contents.len() as u64).to_le_bytes());
        hasher.update(file.contents.as_bytes());
    }
    let origin = match rock_stdlib::stdlib_origin() {
        rock_stdlib::StdlibSource::Dir(dir) => dir.to_string_lossy().into_owned(),
        rock_stdlib::StdlibSource::Embedded => "<embedded-stdlib>".to_owned(),
    };
    serde_json::json!({
        "origin": origin,
        "sha256": format!("{:x}", hasher.finalize()),
    })
}

/// SHA-256 of the compiler `rock_exe` names, or `None` when it cannot be
/// pinned. Compile caches are off without it: a key must name the compiler
/// that produced the artifact, not whatever is at the path now. This process's
/// own binary is pinned to its mapped image when available; otherwise a
/// pathname replaced after process start is refused. Another binary (a test
/// harness keying the `rock` it spawns) is hashed from its path, refused when
/// it changes during the read. Memoized
/// per path.
pub(crate) fn compile_cache_compiler_sha256(rock_exe: &std::path::Path) -> Option<String> {
    static HASHES: std::sync::OnceLock<
        std::sync::Mutex<HashMap<std::path::PathBuf, Option<String>>>,
    > = std::sync::OnceLock::new();
    let hashes = HASHES.get_or_init(Default::default);
    if let Some(hash) = hashes.lock().unwrap().get(rock_exe) {
        return hash.clone();
    }
    let running =
        rock_host::process_management::current_executable_image().is_ok_and(|exe| exe == rock_exe);
    let hash = if running {
        running_image_sha256()
    } else {
        file_sha256_if_unchanged_since(rock_exe, std::time::SystemTime::now())
    };
    let hash = match hash {
        Ok(Some(hash)) => Some(hash),
        Ok(None) => {
            log::warn!(target: "CLI", "{} changed while it was keyed; compile caches are off", rock_exe.display());
            None
        }
        Err(error) => {
            log::warn!(target: "CLI", "cannot hash {}; compile caches are off: {error:#}", rock_exe.display());
            None
        }
    };
    hashes
        .lock()
        .unwrap()
        .insert(rock_exe.to_path_buf(), hash.clone());
    hash
}

fn running_image_sha256() -> Result<Option<String>> {
    crate::comptime::compiler_identity().map(Some)
}

/// Compiler-image identity of `path` if it is the same file before and after
/// the read and was last written before `started`; `None` otherwise. Uses
/// the shared compiler-image definition, not a plain file hash, so a binary
/// keyed by path names the same identity as the running compiler.
#[cfg(unix)]
fn file_sha256_if_unchanged_since(
    path: &std::path::Path,
    started: std::time::SystemTime,
) -> Result<Option<String>> {
    use std::os::unix::fs::MetadataExt;
    let stamp = |meta: &std::fs::Metadata| {
        (
            meta.dev(),
            meta.ino(),
            meta.len(),
            meta.mtime(),
            meta.mtime_nsec(),
        )
    };
    let before = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    let hash = crate::comptime::compiler_image_identity(path)?;
    let after = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    let written = before
        .modified()
        .with_context(|| format!("mtime of {}", path.display()))?;
    Ok((stamp(&before) == stamp(&after) && written <= started).then_some(hash))
}

#[cfg(windows)]
fn file_sha256_if_unchanged_since(
    path: &std::path::Path,
    started: std::time::SystemTime,
) -> Result<Option<String>> {
    use std::os::windows::fs::OpenOptionsExt;
    // Keep the pathname and contents fixed while the shared image reader opens it.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(winapi::um::winnt::FILE_SHARE_READ)
        .open(path)?;
    if file.metadata()?.modified()? > started {
        return Ok(None);
    }
    crate::comptime::compiler_image_identity(path).map(Some)
}

pub(crate) fn compile_cache_identity(
    project: &std::path::Path,
    backend: crate::Backend,
    comptime: &bone_semantic::inference::ComptimeInputs,
    rock_exe: &std::path::Path,
) -> Result<serde_json::Value> {
    use anyhow::Context;
    // Null only where caching is off; a child that cannot pin its compiler keys
    // its receipt to null, which no parent lookup matches.
    let compiler_sha256 = compile_cache_compiler_sha256(rock_exe);
    let preamble = (RuntimeServices::registered().program_preamble)(
        &project.to_string_lossy(),
        ProgramKind::Game,
    )
    .context("resolve the game preamble for the compile cache key")?;
    Ok(serde_json::json!({
        "compiler_sha256": compiler_sha256,
        "backend": backend.name(),
        "stdlib": compile_cache_stdlib_digest(),
        "preamble_sha256": cache_sha256_bytes(preamble.as_bytes()),
        "build_vars_sha256": compile_cache_build_vars_sha256(comptime),
        "source_fingerprint_support_sha256": compile_cache_source_fingerprint_support_sha256(comptime)?,
        "environment_sha256": compile_cache_environment_sha256(),
    }))
}

pub(crate) fn compile_cache_lock_key(
    root: &std::path::Path,
    key: &str,
    log_target: &'static str,
    cache_kind: &'static str,
) -> Result<std::fs::File> {
    use std::fs::File;
    use std::fs::TryLockError;
    use std::io::{Seek, Write};
    let path = root.join(format!("{key}.lock"));
    let mut file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("open compile cache lock {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            rock_host::process_management::report_spawn_phase("cache-wait");
            let holder = std::fs::read_to_string(&path)
                .with_context(|| format!("read compile cache lock {}", path.display()))?;
            log::info!(target: log_target, "waiting for {} compiling the same {} (cache key {})", holder.trim(), cache_kind, &key[..key.len().min(16)]);
            file.lock()
                .with_context(|| format!("wait for compile cache lock {key}"))?;
        }
        Err(error) => return Err(error).with_context(|| format!("lock compile cache key {key}")),
    }
    // Keep the inode: unlinking it can strand an existing waiter on the old
    // lock. Kernel locks die with the holder; replace its stale PID.
    file.set_len(0).context("clear compile cache lock holder")?;
    file.rewind().context("rewind compile cache lock holder")?;
    writeln!(file, "{}", std::process::id()).context("record compile cache lock holder")?;
    Ok(file)
}

pub(crate) fn compile_cache_publish_dir(
    staged: &std::path::Path,
    entry: &std::path::Path,
    manifest: &str,
) -> Result<()> {
    match std::fs::rename(staged, entry) {
        Ok(()) => Ok(()),
        Err(_) if entry.join(manifest).is_file() => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("publish {} -> {}", staged.display(), entry.display())),
    }
}

pub(crate) fn compile_cache_mark_used(entry: &std::path::Path, manifest: &str) -> Result<()> {
    std::fs::File::options()
        .write(true)
        .open(entry.join(manifest))
        .with_context(|| format!("open {}", entry.join(manifest).display()))?
        .set_modified(std::time::SystemTime::now())
        .context("touch compile cache manifest")
}

pub(crate) fn compile_cache_evict(
    root: &std::path::Path,
    entry: &std::path::Path,
    evicting_prefix: &str,
) -> Result<()> {
    let doomed = tempfile::Builder::new()
        .prefix(evicting_prefix)
        .tempdir_in(root)
        .context("create compile cache eviction dir")?
        .keep();
    let target = doomed.join("entry");
    match std::fs::rename(entry, &target) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            let moved = Err(error).with_context(|| format!("move {} aside", entry.display()));
            return remove_compile_cache_eviction(&doomed).and(moved);
        }
    }
    remove_compile_cache_eviction(&doomed)
}

fn remove_compile_cache_eviction(doomed: &std::path::Path) -> Result<()> {
    match std::fs::remove_dir_all(doomed) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            Err(error).with_context(|| format!("remove {}", doomed.display()))
        }
        _ => Ok(()),
    }
}

pub(crate) fn compile_cache_evict_to_budget(
    root: &std::path::Path,
    budget: u64,
    keep: &std::path::Path,
    manifest: &str,
    staging_prefix: &str,
    evicting_prefix: &str,
    stale_staging: Duration,
) -> Result<()> {
    let now = std::time::SystemTime::now();
    let mut entries = Vec::new();
    let mut total = 0u64;
    for item in std::fs::read_dir(root).with_context(|| format!("read {}", root.display()))? {
        let item = item?;
        let path = item.path();
        let name = item.file_name().to_string_lossy().into_owned();
        if name.starts_with(evicting_prefix) {
            remove_compile_cache_eviction(&path)?;
            continue;
        }
        if name.starts_with(staging_prefix) {
            let age = item.metadata()?.modified()?;
            if now.duration_since(age).unwrap_or_default() > stale_staging {
                std::fs::remove_dir_all(&path).with_context(|| {
                    format!("remove stale compile cache staging dir {}", path.display())
                })?;
            }
            continue;
        }
        // An entry another run is evicting can vanish between listing and stat.
        let Ok(used) = std::fs::metadata(path.join(manifest)).and_then(|meta| meta.modified())
        else {
            continue;
        };
        let Ok(bytes) = compile_cache_dir_bytes(&path) else {
            continue;
        };
        total += bytes;
        entries.push((used, bytes, path));
    }
    entries.sort();
    for (_, bytes, path) in entries {
        if total <= budget {
            break;
        }
        if path == keep {
            continue;
        }
        compile_cache_evict(root, &path, evicting_prefix)?;
        total -= bytes;
    }
    sweep_compile_cache_locks(root)
}

/// Remove key locks that nobody holds and whose entry is gone. A process that
/// opened a lock but has not locked it yet can lose its inode here; it then
/// compiles beside the next locker, which costs a duplicate compile, not a
/// wrong entry (publication tolerates an existing entry).
fn sweep_compile_cache_locks(root: &std::path::Path) -> Result<()> {
    use std::fs::TryLockError;
    for item in std::fs::read_dir(root).with_context(|| format!("read {}", root.display()))? {
        let path = item?.path();
        let Some(key) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".lock"))
        else {
            continue;
        };
        if root.join(key).exists() {
            continue;
        }
        let file = match std::fs::File::options().read(true).write(true).open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("open compile cache lock {}", path.display()));
            }
        };
        match file.try_lock() {
            Ok(()) => match std::fs::remove_file(&path) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    return Err(error)
                        .with_context(|| format!("remove compile cache lock {}", path.display()));
                }
                _ => {}
            },
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(error)) => {
                return Err(error)
                    .with_context(|| format!("probe compile cache lock {}", path.display()));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod compile_cache_compiler_tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn self_spawn_keeps_the_running_compiler_after_its_path_is_deleted() {
        const TEST: &str = "compile_cache::compile_cache_compiler_tests::self_spawn_keeps_the_running_compiler_after_its_path_is_deleted";
        const PHASE: &str = "ROCK_SELF_SPAWN_PROBE_PHASE";
        const EXIT: &str = "ROCK_SELF_SPAWN_PROBE_EXIT";
        const HASH: &str = "ROCK_SELF_SPAWN_PROBE_HASH";
        match std::env::var(PHASE).as_deref() {
            Ok("unlinked") => {
                let path = std::env::current_exe().unwrap();
                let expected = crate::comptime::compiler_image_identity(&path).unwrap();
                std::fs::remove_file(&path).unwrap();
                assert!(!std::env::current_exe().unwrap().exists());
                assert_eq!(crate::comptime::compiler_identity().unwrap(), expected);
                let executable = rock_host::process_management::current_executable_image().unwrap();
                assert_eq!(
                    compile_cache_compiler_sha256(&executable),
                    Some(expected.clone())
                );
                let status = std::process::Command::new(executable)
                    .args(["--exact", TEST, "--nocapture"])
                    .env(PHASE, "respawned")
                    .env(HASH, expected)
                    .status()
                    .expect("spawn the running compiler after unlink");
                std::process::exit(status.code().expect("child exit code"));
            }
            Ok("respawned") => {
                rock_host::compiler_thp::enter_compiler_process()
                    .expect("restart the unlinked compiler with its allocator default");
                assert_eq!(std::env::var("MIMALLOC_ALLOW_THP").as_deref(), Ok("0"));
                assert_eq!(
                    running_image_sha256().unwrap(),
                    Some(std::env::var(HASH).unwrap())
                );
                std::process::exit(std::env::var(EXIT).unwrap().parse().unwrap());
            }
            _ => {}
        }
        let home = std::env::var_os("HOME").unwrap();
        let storage = crate::host_paths::local_work_root(std::path::Path::new(&home))
            .unwrap()
            .join("tmp");
        std::fs::create_dir_all(&storage).unwrap();
        for expected_exit in [0, 42] {
            let scratch = tempfile::Builder::new()
                .prefix("rock-self-exe-")
                .tempdir_in(&storage)
                .unwrap();
            let root = scratch.path().to_path_buf();
            let executable = root.join("runner");
            std::fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
            let output = std::process::Command::new(&executable)
                .args(["--exact", TEST, "--nocapture"])
                .env(PHASE, "unlinked")
                .env(EXIT, expected_exit.to_string())
                .env_remove("MIMALLOC_ALLOW_THP")
                .env_remove("mimalloc_allow_thp")
                .output()
                .unwrap();
            assert_eq!(
                output.status.code(),
                Some(expected_exit),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            scratch.close().unwrap();
            assert!(
                !root.exists(),
                "the probe must clean up after success and intentional failure"
            );
        }
    }

    #[test]
    fn compiler_hash_refuses_a_binary_written_after_the_process_started() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("rock");
        std::fs::write(&exe, b"compiler image").unwrap();
        let written = std::fs::metadata(&exe).unwrap().modified().unwrap();
        let expected = crate::comptime::compiler_image_identity(&exe).unwrap();
        assert_eq!(
            file_sha256_if_unchanged_since(&exe, written)
                .unwrap()
                .as_deref(),
            Some(expected.as_str()),
        );
        assert_eq!(
            file_sha256_if_unchanged_since(&exe, written - std::time::Duration::from_secs(1))
                .unwrap(),
            None,
            "a binary replaced after the process started is not the running compiler",
        );
        assert!(
            running_image_sha256().unwrap().is_some(),
            "the test binary predates its own process"
        );
    }

    #[test]
    fn compiler_hash_pins_this_process_and_hashes_another_binary_by_path() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("rock");
        std::fs::write(&exe, b"harness rock").unwrap();
        let expected = crate::comptime::compiler_image_identity(&exe).unwrap();
        assert_eq!(compile_cache_compiler_sha256(&exe), Some(expected));
        let running = rock_host::process_management::current_executable_image().unwrap();
        assert_eq!(
            compile_cache_compiler_sha256(&running),
            running_image_sha256().unwrap()
        );
    }
}

#[cfg(test)]
mod compile_cache_source_fingerprint_tests {
    use super::*;
    use bone_core::workspace::{ComptimeVarValue, SourceFingerprint};
    use std::path::PathBuf;
    use ustr::Ustr;

    fn deferred_inputs() -> bone_semantic::inference::ComptimeInputs {
        let mut inputs = bone_semantic::inference::ComptimeInputs {
            channel: "release".to_string(),
            modules_mode: "v1".to_string(),
            embed_root: Some(PathBuf::from("/project")),
            ..Default::default()
        };
        inputs.source_fingerprints.insert(
            Ustr::from("REVISION"),
            SourceFingerprint {
                secret: false,
                roots: vec![Ustr::from("src")],
                files: vec!["release/revision.txt".to_string()],
                vars: vec![Ustr::from("DECODER")],
            },
        );
        inputs
    }

    #[test]
    fn build_vars_digest_is_stable_across_deferred_resolution() {
        let before = deferred_inputs();
        let mut after = before.clone();
        after.vars.insert(
            Ustr::from("REVISION"),
            ComptimeVarValue {
                value: "source-v1:abc123".to_string(),
                secret: false,
            },
        );

        assert_eq!(
            compile_cache_build_vars_sha256(&before),
            compile_cache_build_vars_sha256(&after),
            "resolving the deferred value must not move the lookup key",
        );
    }

    #[test]
    fn build_vars_digest_keys_canonical_deferred_declarations() {
        let mut original = deferred_inputs();
        let declaration = original
            .source_fingerprints
            .get_mut(&Ustr::from("REVISION"))
            .unwrap();
        declaration.roots = vec![Ustr::from("generated"), Ustr::from("src")];
        declaration.files.push("release/lock.json".to_string());
        declaration.vars.push(Ustr::from("PLATFORM"));
        let mut reordered = original.clone();
        let declaration = reordered
            .source_fingerprints
            .get_mut(&Ustr::from("REVISION"))
            .unwrap();
        declaration.roots.reverse();
        declaration.files.reverse();
        declaration.vars.reverse();
        let mut changed = original.clone();
        changed
            .source_fingerprints
            .get_mut(&Ustr::from("REVISION"))
            .unwrap()
            .secret = true;

        assert_eq!(
            compile_cache_build_vars_sha256(&original),
            compile_cache_build_vars_sha256(&reordered),
            "declaration list order is canonicalized",
        );
        assert_ne!(
            compile_cache_build_vars_sha256(&original),
            compile_cache_build_vars_sha256(&changed),
            "deferred declarations are part of the cache identity",
        );
    }

    #[test]
    fn identity_keys_explicit_support_file_contents_from_project_and_stdlib() {
        let project = tempfile::tempdir().unwrap();
        let support = project.path().join("release/revision.txt");
        std::fs::create_dir_all(support.parent().unwrap()).unwrap();
        std::fs::write(&support, b"revision one").unwrap();
        let stdlib_name = rock_stdlib::stdlib_files()[0].name;
        let mut inputs = bone_semantic::inference::ComptimeInputs {
            embed_root: Some(project.path().to_path_buf()),
            ..Default::default()
        };
        inputs.source_fingerprints.insert(
            Ustr::from("REVISION"),
            SourceFingerprint {
                secret: false,
                roots: vec![Ustr::from("src")],
                files: vec![
                    "release/revision.txt".to_string(),
                    format!("stdlib/{stdlib_name}"),
                ],
                vars: Vec::new(),
            },
        );
        let rock_exe = std::env::current_exe().unwrap();
        let first =
            compile_cache_identity(project.path(), crate::Backend::Skull, &inputs, &rock_exe)
                .unwrap();
        assert!(
            first["source_fingerprint_support_sha256"]
                .as_str()
                .is_some()
        );

        std::fs::write(&support, b"revision two").unwrap();
        let second =
            compile_cache_identity(project.path(), crate::Backend::Skull, &inputs, &rock_exe)
                .unwrap();
        assert_ne!(
            first["source_fingerprint_support_sha256"], second["source_fingerprint_support_sha256"],
            "support bytes must participate in compile cache identity",
        );
    }

    #[test]
    fn identity_rejects_missing_explicit_support_files() {
        let project = tempfile::tempdir().unwrap();
        let mut inputs = bone_semantic::inference::ComptimeInputs {
            embed_root: Some(project.path().to_path_buf()),
            ..Default::default()
        };
        inputs.source_fingerprints.insert(
            Ustr::from("REVISION"),
            SourceFingerprint {
                secret: false,
                roots: vec![Ustr::from("src")],
                files: vec!["release/missing.txt".to_string()],
                vars: Vec::new(),
            },
        );
        let error = compile_cache_identity(
            project.path(),
            crate::Backend::Skull,
            &inputs,
            &std::env::current_exe().unwrap(),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("release/missing.txt"));
    }

    #[test]
    fn support_digest_allows_declared_sibling_and_absolute_dependency_files() {
        let fixture = tempfile::tempdir().unwrap();
        let project = fixture.path().join("project");
        let sibling = fixture.path().join("decoder/metadata.json");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(sibling.parent().unwrap()).unwrap();
        std::fs::write(&sibling, b"decoder one").unwrap();
        let mut inputs = bone_semantic::inference::ComptimeInputs {
            embed_root: Some(project.clone()),
            ..Default::default()
        };
        inputs.source_fingerprints.insert(
            Ustr::from("REVISION"),
            SourceFingerprint {
                secret: false,
                roots: vec![Ustr::from("../decoder")],
                files: vec![
                    "../decoder/metadata.json".to_string(),
                    sibling.to_string_lossy().into_owned(),
                ],
                vars: Vec::new(),
            },
        );

        let first = compile_cache_source_fingerprint_support_sha256(&inputs).unwrap();
        std::fs::write(&sibling, b"decoder two").unwrap();
        let second = compile_cache_source_fingerprint_support_sha256(&inputs).unwrap();
        assert_ne!(
            first, second,
            "explicit sibling and absolute dependency bytes are keyed"
        );

        inputs.embed_root = None;
        inputs
            .source_fingerprints
            .get_mut(&Ustr::from("REVISION"))
            .unwrap()
            .files = vec![sibling.to_string_lossy().into_owned()];
        assert!(compile_cache_source_fingerprint_support_sha256(&inputs).is_ok());
    }
}

fn compile_cache_dir_bytes(dir: &std::path::Path) -> Result<u64> {
    let mut total = 0;
    for file in std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        total += file?.metadata()?.len();
    }
    Ok(total)
}
