//! skull backend integration for `rock-cli` (selected via `--backend skull`).
//!
//! skull is the Claude-managed codegen on top of `bone`'s HIR (see the `skull`
//! crate + `docs/skull-plan.md`). Where `--backend bone` stops at type-checking,
//! `--backend skull` goes one step further: it emits Lua from bone's HIR and
//! (for `test`/`run`) executes it via luajit.
//!
//! A unit "passes" skull today iff: bone accepts it AND skull emits Lua that
//! luajit loads without error. True `@test`-output execution is the next step
//! and lands once bone has a prelude (`print`/`assert`) — until then most files
//! are rejected at the bone stage, exactly like `--backend bone`. The long-term
//! goal is `rock test tests --backend skull` at 100%.

use crate::Result;
use crate::bone_backend::{categorize, short_path};
use anyhow::Context;
use colored::Colorize;
use rock_core::CompilerError;
use rock_core::SourceFiles;
use rock_core::source_file_from_path_and_contents;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

// Both compile caches share this identity and storage path so invalidation and
// publication stay identical for sim receipts and frame artifacts.
// Compile-related prefixes default to keyed; these are runner and diagnostics.
const COMPILE_CACHE_KEYED_ENV_PREFIXES: &[&str] = &["ROCK_", "SKULL_", "BONE_"];
const COMPILE_CACHE_KEYED_ENV_NAMES: &[&str] = &["GAME_ART_VAULT", "GAME_SOURCE_DIR", "CARGO_TARGET_DIR", "HOME"];
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
    "ROCK_MASK_WALL_CLOCK",
    "ROCK_DETERMINISTIC_IDS",
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
    crate::compile_slots::SLOTS_ENV,
    crate::compile_slots::CLASS_ENV,
    crate::compile_slots::SLOT_DIR_ENV,
    "ROCK_JOB_ID",
    "ROCK_INVOCATION_LOG",
    crate::invocation_log::PARENT_ENV,
    "ROCK_CHANGE_ID",
    "ROCK_LOG_EPOCH_MS",
    "ROCK_SIM_COMPILE_TIMING",
    "ROCK_SIM_ENTRY_PLAN",
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
    (COMPILE_CACHE_KEYED_ENV_PREFIXES.iter().any(|prefix| name.starts_with(prefix))
        || COMPILE_CACHE_KEYED_ENV_NAMES.contains(&name))
        && !COMPILE_CACHE_UNKEYED_ENV.contains(&name)
        && !COMPILE_CACHE_UNKEYED_ENV_PREFIXES.iter().any(|prefix| name.starts_with(prefix))
}

fn compile_cache_environment_sha256() -> String {
    let keyed = std::env::vars_os()
        .filter_map(|(name, value)| {
            let name = name.to_string_lossy().into_owned();
            compile_cache_environment_is_keyed(&name).then(|| (name, value.to_string_lossy().into_owned()))
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    cache_sha256_bytes(serde_json::json!(keyed).to_string().as_bytes())
}

fn compile_cache_build_vars_sha256(inputs: &bone_semantic::inference::ComptimeInputs) -> String {
    let mut volatile = inputs.volatile_vars.iter().map(|name| name.as_str()).collect::<Vec<_>>();
    volatile.sort_unstable();
    let mut vars = inputs
        .vars
        .iter()
        .filter(|(name, _)| !inputs.volatile_vars.contains(*name))
        .map(|(name, var)| (name.as_str(), var.value.as_str(), var.secret))
        .collect::<Vec<_>>();
    vars.sort_unstable();
    let canonical = serde_json::json!({
        "channel": inputs.channel,
        "modules_mode": inputs.modules_mode,
        "embed_root": inputs.embed_root.as_ref().map(|root| root.to_string_lossy().into_owned()),
        "volatile": volatile,
        "vars": vars,
    });
    cache_sha256_bytes(canonical.to_string().as_bytes())
}

fn compile_cache_stdlib_digest() -> serde_json::Value {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    for file in rock_host::stdlib_loader::stdlib_files() {
        hasher.update(file.name.as_bytes());
        hasher.update([0]);
        hasher.update((file.contents.len() as u64).to_le_bytes());
        hasher.update(file.contents.as_bytes());
    }
    serde_json::json!({
        "dir": rock_host::stdlib_loader::stdlib_dir().to_string_lossy(),
        "sha256": format!("{:x}", hasher.finalize()),
    })
}

/// This `rock` binary, for compiles that run in this process or in a child of it.
pub(crate) fn current_rock_exe() -> Result<std::path::PathBuf> {
    std::env::current_exe().context("locating the rock binary")
}

/// SHA-256 of the compiler `rock_exe` names, or `None` when it cannot be
/// pinned. Compile caches are off without it: a key must name the compiler
/// that produced the artifact, not whatever is at the path now. This process's
/// own binary is pinned to its running image, refused when replaced after the
/// process started; another binary (a test harness keying the `rock` it spawns)
/// is hashed from its path, refused when it changes during the read. Memoized
/// per path.
pub(crate) fn compile_cache_compiler_sha256(rock_exe: &std::path::Path) -> Option<String> {
    static HASHES: std::sync::OnceLock<std::sync::Mutex<HashMap<std::path::PathBuf, Option<String>>>> =
        std::sync::OnceLock::new();
    let hashes = HASHES.get_or_init(Default::default);
    if let Some(hash) = hashes.lock().unwrap().get(rock_exe) {
        return hash.clone();
    }
    let running = std::env::current_exe().is_ok_and(|exe| exe == rock_exe);
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
    hashes.lock().unwrap().insert(rock_exe.to_path_buf(), hash.clone());
    hash
}

#[cfg(target_os = "linux")]
fn running_image_sha256() -> Result<Option<String>> {
    // The kernel resolves this to the mapped image even after its path is replaced.
    cache_sha256_file(std::path::Path::new("/proc/self/exe")).map(Some)
}

#[cfg(target_os = "macos")]
fn running_image_sha256() -> Result<Option<String>> {
    let exe = std::env::current_exe().context("locate the rock binary to key the compile")?;
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: `info` is a writable proc_bsdinfo of exactly `size` bytes.
    let written = unsafe {
        libc::proc_pidinfo(
            std::process::id() as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    if written != size {
        return Err(std::io::Error::last_os_error()).context("read this process's start time");
    }
    let started = std::time::UNIX_EPOCH
        + Duration::from_secs(info.pbi_start_tvsec)
        + Duration::from_micros(info.pbi_start_tvusec);
    file_sha256_if_unchanged_since(&exe, started)
}

/// Hash `path` if it is the same file before and after the read and was last
/// written before `started`; `None` otherwise.
#[cfg(unix)]
fn file_sha256_if_unchanged_since(path: &std::path::Path, started: std::time::SystemTime) -> Result<Option<String>> {
    use std::os::unix::fs::MetadataExt;
    let stamp = |meta: &std::fs::Metadata| (meta.dev(), meta.ino(), meta.len(), meta.mtime(), meta.mtime_nsec());
    let before = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    let hash = cache_sha256_file(path)?;
    let after = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    let written = before.modified().with_context(|| format!("mtime of {}", path.display()))?;
    Ok((stamp(&before) == stamp(&after) && written <= started).then_some(hash))
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
    let preamble = skull_game_preamble(&project.to_string_lossy())
        .context("resolve the game preamble for the compile cache key")?;
    Ok(serde_json::json!({
        "compiler_sha256": compiler_sha256,
        "backend": backend.name(),
        "stdlib": compile_cache_stdlib_digest(),
        "preamble_sha256": cache_sha256_bytes(preamble.as_bytes()),
        "build_vars_sha256": compile_cache_build_vars_sha256(comptime),
        "environment_sha256": compile_cache_environment_sha256(),
    }))
}

pub(crate) fn compile_cache_lock_key(
    root: &std::path::Path,
    key: &str,
    log_target: &'static str,
    cache_kind: &'static str,
) -> Result<std::fs::File> {
    use std::io::{Seek, Write};
    use std::fs::File;
    use std::fs::TryLockError;
    let path = root.join(format!("{key}.lock"));
    let mut file = File::options().read(true).write(true).create(true).truncate(false)
        .open(&path).with_context(|| format!("open compile cache lock {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            crate::compile_slots::report_spawn_phase("cache-wait");
            let holder = std::fs::read_to_string(&path)
                .with_context(|| format!("read compile cache lock {}", path.display()))?;
            log::info!(target: log_target, "waiting for {} compiling the same {} (cache key {})", holder.trim(), cache_kind, &key[..key.len().min(16)]);
            file.lock().with_context(|| format!("wait for compile cache lock {key}"))?;
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

pub(crate) fn compile_cache_publish_dir(staged: &std::path::Path, entry: &std::path::Path, manifest: &str) -> Result<()> {
    match std::fs::rename(staged, entry) {
        Ok(()) => Ok(()),
        Err(_) if entry.join(manifest).is_file() => Ok(()),
        Err(error) => Err(error).with_context(|| format!("publish {} -> {}", staged.display(), entry.display())),
    }
}

pub(crate) fn compile_cache_mark_used(entry: &std::path::Path, manifest: &str) -> Result<()> {
    std::fs::File::options().write(true).open(entry.join(manifest))
        .with_context(|| format!("open {}", entry.join(manifest).display()))?
        .set_modified(std::time::SystemTime::now())
        .context("touch compile cache manifest")
}

pub(crate) fn compile_cache_evict(root: &std::path::Path, entry: &std::path::Path, evicting_prefix: &str) -> Result<()> {
    let doomed = tempfile::Builder::new().prefix(evicting_prefix).tempdir_in(root)
        .context("create compile cache eviction dir")?.keep();
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
                std::fs::remove_dir_all(&path)
                    .with_context(|| format!("remove stale compile cache staging dir {}", path.display()))?;
            }
            continue;
        }
        // An entry another run is evicting can vanish between listing and stat.
        let Ok(used) = std::fs::metadata(path.join(manifest)).and_then(|meta| meta.modified()) else { continue };
        let Ok(bytes) = compile_cache_dir_bytes(&path) else { continue };
        total += bytes;
        entries.push((used, bytes, path));
    }
    entries.sort();
    for (_, bytes, path) in entries {
        if total <= budget { break; }
        if path == keep { continue; }
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
        let Some(key) = path.file_name().and_then(|name| name.to_str()).and_then(|name| name.strip_suffix(".lock")) else {
            continue;
        };
        if root.join(key).exists() {
            continue;
        }
        let file = match std::fs::File::options().read(true).write(true).open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).with_context(|| format!("open compile cache lock {}", path.display())),
        };
        match file.try_lock() {
            Ok(()) => match std::fs::remove_file(&path) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    return Err(error).with_context(|| format!("remove compile cache lock {}", path.display()));
                }
                _ => {}
            },
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(error)) => {
                return Err(error).with_context(|| format!("probe compile cache lock {}", path.display()));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod compile_cache_compiler_tests {
    use super::*;

    #[test]
    fn compiler_hash_refuses_a_binary_written_after_the_process_started() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("rock");
        std::fs::write(&exe, b"compiler image").unwrap();
        let written = std::fs::metadata(&exe).unwrap().modified().unwrap();
        assert_eq!(
            file_sha256_if_unchanged_since(&exe, written).unwrap().as_deref(),
            Some(cache_sha256_bytes(b"compiler image").as_str()),
        );
        assert_eq!(
            file_sha256_if_unchanged_since(&exe, written - Duration::from_secs(1)).unwrap(),
            None,
            "a binary replaced after the process started is not the running compiler",
        );
        assert!(running_image_sha256().unwrap().is_some(), "the test binary predates its own process");
    }

    #[test]
    fn compiler_hash_pins_this_process_and_hashes_another_binary_by_path() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("rock");
        std::fs::write(&exe, b"harness rock").unwrap();
        assert_eq!(compile_cache_compiler_sha256(&exe), Some(cache_sha256_bytes(b"harness rock")));
        let running = current_rock_exe().unwrap();
        assert_eq!(compile_cache_compiler_sha256(&running), running_image_sha256().unwrap());
    }
}

fn compile_cache_dir_bytes(dir: &std::path::Path) -> Result<u64> {
    let mut total = 0;
    for file in std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        total += file?.metadata()?.len();
    }
    Ok(total)
}

/// Parse `src`, type-check it through bone WITH the stdlib injected (so
/// operators/`print`/`assert` resolve, exactly like `--backend bone`), then emit
/// Lua for the user module via skull. The stdlib module isn't emitted — its
/// symbols are referenced by name (resolved at runtime by a real stdlib
/// prelude, or simply not called under the load-only validity gate).
/// Returns the emitted Lua and the emitter's own `emits_main`, which decides
/// whether the caller appends the entry call. Not recovered from the text: the
/// emitted spelling of a definition is the emitter's business and changes with
/// `ROCK_SKULL_CALLEE_BINDING`.
fn skull_emit(
    file: &str,
    src: &str,
) -> std::result::Result<(String, bool), Vec<CompilerError>> {
    let (lua, _, emits_main) = skull_emit_full(file, src).map_err(EmitFullErr::into_errs)?;
    Ok((lua, emits_main))
}

/// A [`skull_emit_full`] failure. `Discovery` (a malformed `@test` annotation on
/// a file bone ACCEPTED) is split from `Compile` so the expect-compile-failure
/// escape in [`skull_build`] never absorbs it -- `@test(expect_failure = true,
/// returns = 42)` must reject, not count as an expected compile failure.
enum EmitFullErr {
    Compile(Vec<CompilerError>),
    Discovery(CompilerError),
}

impl EmitFullErr {
    fn into_errs(self) -> Vec<CompilerError> {
        match self {
            EmitFullErr::Compile(errs) => errs,
            EmitFullErr::Discovery(e) => vec![e],
        }
    }
}

/// Emit the workspace and also discover the user module's `@test` functions
/// (for execution). Returns `(lua, tests, emits_main)`. `file` is stamped as
/// the source-file id so spans (and `__SITE__`) carry the real path.
fn skull_emit_full(
    file: &str,
    src: &str,
) -> std::result::Result<(String, Vec<skull::SkullTest>, bool), EmitFullErr> {
    let stats = crate::compile_measurements::Scope::begin(file);
    crate::compile_measurements::source(file, src.len());
    let user = skull_parser::parse_module(src, ustr::Ustr::from(file))
        .map_err(|e| EmitFullErr::Compile(vec![e]))?;
    let user_file = user.file;
    let mut modules = crate::bone_backend::with_stdlib([user]);
    crate::compile_measurements::phase("parse+stdlib");
    let mut ws = crate::bone_backend::check_modules(&mut modules).map_err(EmitFullErr::Compile)?;
    crate::compile_measurements::phase("semantic");
    crate::compile_measurements::timings("bone", &ws.phase_timings);
    drop(modules);
    // Single-file emission resolves from the file's enclosing project
    // (walk-up), matching the JS single-file path -- the same source must
    // agree across backends (crates/rock-cli/tests/js_run_gate.rs).
    crate::comptime::stamp_workspace(&mut ws, std::path::Path::new(file)).map_err(|e| {
        EmitFullErr::Compile(vec![CompilerError::new_no_location(format!(
            "build var resolution failed: {e:#}"
        ))])
    })?;
    let ws = ws;
    // The build lint pass (`docs/lint-future-plan.md`): soft findings report,
    // `Error` findings refuse the program through the same path a type error
    // takes.
    let lint_errs = crate::lint_pass::run_build_lints_single(&ws, file, src);
    if !lint_errs.is_empty() {
        return Err(EmitFullErr::Compile(lint_errs));
    }
    crate::compile_measurements::phase("prepare");
    let user_mid = crate::bone_backend::entry_module_id(&ws, user_file);
    let emit = skull::mir_emit::emit_workspace_mir(&ws, user_mid)
        .map_err(|e| EmitFullErr::Compile(vec![e]))?;
    crate::compile_measurements::emission(&emit);
    let emits_main = emit.emits_main;
    let (lua, skipped) = (emit.lua, emit.skipped);
    if std::env::var("SKULL_DEBUG_SKIPPED").is_ok() && !skipped.is_empty() {
        eprintln!("skull: {} stdlib fns skipped", skipped.len());
        for (name, reason) in &skipped {
            eprintln!("  {name}: {reason}");
        }
    }
    let tests = skull::module_tests(&ws, user_mid).map_err(EmitFullErr::Discovery)?;
    stats.success();
    Ok((lua, tests, emits_main))
}

/// Per-phase wall-clock for one skull run over a single source. `parse` and
/// `semantic` mirror `BonePhaseTimes` (skull shares bone's front half: same
/// `parse_rock` + `semantic_pass`), so they're directly comparable; `emit` is
/// the codegen skull adds on top (`skull::emit_workspace`). Consumed by
/// `backend-stats` for the bone-vs-skull comparison.
#[derive(Default, Clone)]
pub struct SkullPhaseTimes {
    pub parse: Duration,
    pub semantic: Duration,
    /// Comptime/build-variable preparation after semantic checking and before
    /// code generation. This is setup work, not semantic or emit self-time.
    pub workspace_setup: Duration,
    pub emit: Duration,
    /// The last front-end/setup/emit failure, retained so measurement callers
    /// cannot report a contradictory successful-looking zero population.
    pub failure: Option<CompilerError>,
    pub setup_failed: bool,
    pub emit_failed: bool,
    /// skull's emit broken into coarse stages (lower / cleanup / emit-lua /
    /// scaffold+misc), from `MirEmit::phases`. Empty if bone rejected the
    /// workspace (emit never ran). Summed element-wise across timed runs.
    pub emit_phases: Vec<(&'static str, Duration)>,
    /// skull's ABI-plan memo counters, from `MirEmit::abi_plan_stats`.
    pub abi_plan_stats: skull_mir::lower::AbiPlanMemoStats,
    /// Shared-arena traffic split at the setup/fanout boundary, from
    /// `MirEmit::arena_traffic`.
    pub arena_traffic: skull::mir_emit::ArenaTraffic,
    /// Body-oriented lowering and transform attribution from `MirEmit`.
    pub attribution: skull::mir_emit::EmitAttribution,
    /// Setup-lane populations and observed execution overlap from `MirEmit`.
    pub setup_lanes: skull::mir_emit::SetupLaneAttribution,
    /// Graph operation census from `MirEmit`.
    pub graph_census: skull::instance_graph::RunStats,
    /// Concrete-plan ownership census from `MirEmit`.
    pub concrete_stats: skull::concrete_body::ConcreteBuildStats,
}

/// Whole-project skull timing: parse all user files, type-check them together
/// with the stdlib as ONE workspace (bone's front half), then emit Lua for the
/// entry module. Returns whether Lua was emitted (the skull "pass" condition,
/// same gate as `skull_emit`) and the per-phase timing. `parse`/`semantic`
/// mirror `bone_workspace_timed`; `emit` is skull's codegen on top. Used by
/// `backend-stats` — a per-file loop would never reach emit because bone rejects
/// any file that references a sibling (so emit would always read ~0).
pub fn skull_workspace_timed(sources: &SourceFiles) -> (bool, SkullPhaseTimes) {
    let stats = crate::compile_measurements::Scope::for_sources(sources);
    let mut times = SkullPhaseTimes::default();
    let (user_modules, entry_file, parse) = match crate::bone_backend::parse_user_modules(sources) {
        Ok(v) => v,
        Err(mut errors) => {
            times.failure = errors.drain(..).next();
            return (false, times);
        }
    };
    times.parse = parse;

    let t = Instant::now();
    // Timed path: assemble via the shared helper but keep the `semantic_pass`
    // call explicit here (the verdict is a bool, not a gate, and the timer must
    // bracket exactly this call).
    let mut modules = crate::bone_backend::with_stdlib(user_modules);
    crate::compile_measurements::phase("parse+stdlib");
    let aliases = match crate::bone_backend::aliases_for_modules(&modules) {
        Ok(aliases) => aliases,
        Err(error) => {
            times.semantic = t.elapsed();
            times.failure = Some(error);
            return (false, times);
        }
    };
    let mut ws = match bone_semantic::semantic_passes::semantic_pass_with_aliases(&mut modules, &aliases) {
        Ok(ws) => ws,
        Err(error) => {
            times.semantic = t.elapsed();
            times.failure = Some(error);
            return (false, times);
        }
    };
    times.semantic = t.elapsed();
    crate::compile_measurements::phase("semantic");
    crate::compile_measurements::timings("bone", &ws.phase_timings);
    let body_errors = crate::bone_backend::body_errors(&ws);
    if let Some(error) = body_errors.into_iter().next() {
        times.failure = Some(error);
        return (false, times);
    }

    // Production emission stamps declared [build.vars] once after the checked
    // workspace exists. Keep that preparation visible as its own timed lane;
    // the emit stopwatch remains codegen-only.
    let t = Instant::now();
    if let Err(error) = crate::comptime::stamp_workspace(
        &mut ws,
        std::path::Path::new(entry_file.as_str()),
    ) {
        times.workspace_setup = t.elapsed();
        times.setup_failed = true;
        times.failure = Some(CompilerError::new_no_location(format!(
            "build var resolution failed: {error:#}"
        )));
        return (false, times);
    }
    times.workspace_setup = t.elapsed();

    crate::compile_measurements::phase("prepare");
    let entry_mid = crate::bone_backend::entry_module_id(&ws, entry_file);

    let t = Instant::now();
    let emit = skull::mir_emit::emit_workspace_mir(&ws, entry_mid);
    times.emit = t.elapsed();
    let emit_ok = match emit {
        Ok(e) => {
            crate::compile_measurements::emission(&e);
            times.emit_phases = e.phases;
            times.abi_plan_stats = e.abi_plan_stats;
            times.arena_traffic = e.arena_traffic;
            times.attribution = e.attribution;
            times.setup_lanes = e.setup_lanes;
            times.graph_census = e.graph_census;
            times.concrete_stats = e.concrete_stats;
            true
        }
        Err(e) => {
            times.failure = Some(e);
            times.emit_failed = true;
            false
        }
    };

    if emit_ok { stats.success(); }
    (emit_ok, times)
}

/// `rock iter-codegen <project> --iters N` — build the bone workspace ONCE
/// (parse + semantic), then re-run only skull's `emit_workspace` (the full Lua
/// codegen: lower MIR -> inline -> regalloc -> stringify) N times. The loop body
/// is pure codegen, so a sampling profiler (`samply record -- rock iter-codegen
/// <project> --iters N`) attributes nearly every sample to the codegen stage.
/// Build with `--features sequential` to make skull's emit single-threaded for
/// clean, thread-attribution-free flamegraphs. The counterpart to bone's
/// `iter-compile` (which loops inference) and `iter-parse` (the front end).
pub fn skull_iter_codegen(path: &str, iters: usize) -> Result<()> {
    let source_spec = crate::common::path_to_source_spec(path);
    let sources = crate::unified_compile::discover_sources(&source_spec)?;
    let iters = iters.max(1);

    // Build the workspace once: parse + bone semantic_pass. Bail clearly if bone
    // can't accept the project (skull emits only what bone lowers).
    let (mut modules, entry_file) = match skull_assemble_project(&sources) {
        Ok(v) => v,
        Err(errs) => {
            eprintln!("{}", render_project_errs(&sources, &errs));
            crate::ablation_report::exit_with_census(1);
        }
    };
    let mut ws = match crate::bone_backend::check_modules(&mut modules) {
        Ok(ws) => ws,
        Err(errs) => {
            eprintln!("{}", render_project_errs(&sources, &errs));
            crate::ablation_report::exit_with_census(1);
        }
    };
    if let Err(e) = crate::comptime::stamp_workspace(&mut ws, std::path::Path::new(path)) {
        eprintln!("Error: build var resolution failed for {path}: {e:#}");
        crate::ablation_report::exit_with_census(1);
    }
    let entry_mid = crate::bone_backend::entry_module_id(&ws, entry_file);

    // Warm-up emit outside the timed loop (catch any gap, prime the allocator).
    if let Err(e) = skull::mir_emit::emit_workspace_mir(&ws, entry_mid) {
        eprintln!("{}", render_project_err(&sources, &e));
        crate::ablation_report::exit_with_census(1);
    }

    let t = Instant::now();
    let mut bytes = 0usize;
    for _ in 0..iters {
        match skull::mir_emit::emit_workspace_mir(&ws, entry_mid) {
            Ok(e) => bytes = e.lua.len(),
            Err(e) => {
                eprintln!("{}", render_project_err(&sources, &e));
                crate::ablation_report::exit_with_census(1);
            }
        }
    }
    let elapsed = t.elapsed();
    let per = elapsed.as_secs_f64() * 1000.0 / iters as f64;
    let mode = if cfg!(feature = "sequential") { "sequential" } else { "parallel" };
    log::info!(target: "PROFILE",
        "skull iter-codegen {}: {} iters x {} modules ({} KB Lua), {} mode, {:.3}ms total, {:.4}ms/iter",
        path, iters, ws.modules.len(), bytes / 1024, mode,
        elapsed.as_secs_f64() * 1000.0, per);
    Ok(())
}

/// `rock skull-inline-stats <project>` — DIAGNOSTIC: replay skull's function
/// inliner over the whole workspace and report what gets inlined and how much
/// code each splice copies, so we can see whether the emit blow-up comes from a
/// few large callees, one small callee duplicated into many callers, or deep
/// transitive accumulation. Writes a full per-callee table to
/// `skull-inline-stats.txt` (for further analysis) and prints a summary.
///
/// `callee_insts` = reachable inst count of the (cleaned) callee body — the MIR
/// it copies at each site. `total copies` = `callee_insts x times inlined`, a
/// proxy for how much emitted code that callee is responsible for.
pub fn skull_inline_stats(path: &str) -> Result<()> {
    let source_spec = crate::common::path_to_source_spec(path);
    let sources = crate::unified_compile::discover_sources(&source_spec)?;
    let (mut modules, _entry_file) = match skull_assemble_project(&sources) {
        Ok(v) => v,
        Err(errs) => {
            eprintln!("{}", render_project_errs(&sources, &errs));
            crate::ablation_report::exit_with_census(1);
        }
    };
    let mut ws = match crate::bone_backend::check_modules(&mut modules) {
        Ok(ws) => ws,
        Err(errs) => {
            eprintln!("{}", render_project_errs(&sources, &errs));
            crate::ablation_report::exit_with_census(1);
        }
    };
    if let Err(e) = crate::comptime::stamp_workspace(&mut ws, std::path::Path::new(path)) {
        eprintln!("Error: build var resolution failed for {path}: {e:#}");
        crate::ablation_report::exit_with_census(1);
    }

    let r = skull::inline_stats::report(&ws);
    let out = "skull-inline-stats.txt";
    std::fs::write(out, &r.text).map_err(|e| anyhow::anyhow!("writing {out}: {e}"))?;

    println!("{} ({path})", "skull inline stats".bold());
    println!(
        "  {} fns scanned, {} inlined into, {} splices, {} inst-copies",
        r.funcs_scanned, r.callers_inlined, r.total_splices, r.total_copies
    );
    println!("  {} distinct callees", r.distinct_callees);
    if let Some((name, body, times, copies)) = &r.top_callee {
        println!("  most-copied callee: {name} ({body} insts x {times} = {copies} copies)");
    }
    println!("  full report -> {}", out.bold());
    Ok(())
}

pub fn skull_mem2reg_stats(path: &str) -> Result<()> {
    let source_spec = crate::common::path_to_source_spec(path);
    let sources = crate::unified_compile::discover_sources(&source_spec)?;
    let sources = sources
        .production_only(std::path::Path::new(path))
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let (mut modules, entry_file) = match skull_assemble_project(&sources) {
        Ok(v) => v,
        Err(errs) => {
            eprintln!("{}", render_project_errs(&sources, &errs));
            crate::ablation_report::exit_with_census(1);
        }
    };
    let mut ws = match crate::bone_backend::check_modules(&mut modules) {
        Ok(ws) => ws,
        Err(errs) => {
            eprintln!("{}", render_project_errs(&sources, &errs));
            crate::ablation_report::exit_with_census(1);
        }
    };
    if let Err(e) = crate::comptime::stamp_workspace(&mut ws, std::path::Path::new(path)) {
        eprintln!("Error: build var resolution failed for {path}: {e:#}");
        crate::ablation_report::exit_with_census(1);
    }

    let entry_mid = crate::bone_backend::entry_module_id(&ws, entry_file);
    let r = match skull::mem2reg_stats::report_in(&ws, entry_mid, Some(path)) {
        Ok(report) => report,
        Err(error) => {
            eprintln!("{}", render_project_err(&sources, &error));
            crate::ablation_report::exit_with_census(1);
        }
    };
    let out = "skull-mem2reg-stats.txt";
    std::fs::write(out, &r.text).map_err(|e| anyhow::anyhow!("writing {out}: {e}"))?;

    println!("{} ({path})", "skull mem2reg residual census".bold());
    println!(
        "  {} bodies, {} surviving MIR loads ({} cross-block direct)",
        r.bodies_scanned, r.totals.loads, r.totals.cross_block_promotable_loads,
    );
    println!(
        "  {} parameter loads folded by Lua emission; {} residual Store loads",
        r.totals.parameter_promotable_loads, r.totals.store_promotable_loads,
    );
    println!(
        "  {} MIR cell colors; {} residual removable Store colors",
        r.totals.current_cell_slots, r.totals.guaranteed_removable_cell_slots,
    );
    println!("  full report -> {}", out.bold());
    Ok(())
}

/// `rock iter-cook <project> --iters N` — benchmark the front-end COOK
/// (`skull_parser::cook` = macro-expand + per-module desugar), the work bone's
/// `expand_and_desugar` does on every compile. Parses stdlib+project to raw
/// ASTs once, then loops `clone + cook`.
///
/// NOTE: the per-iter `clone` is a HARNESS RESET, not a real compile cost. `cook`
/// mutates the AST in place, so the loop must restore an uncooked copy each
/// iteration. The batch compile path (`semantic_pass` -> `expand_and_desugar` ->
/// `cook`) operates on `&mut [Ast]` in place and clones NOTHING; only the
/// incremental Session clones (`combined_unexpanded`, full-rebuild paths). So
/// read `cook-only` as the real per-compile cost and `clone-only` as harness
/// overhead, not an `expand_and_desugar` `.to_vec()` (no such copy exists today).
pub fn skull_iter_cook(path: &str, iters: usize) -> Result<()> {
    let source_spec = crate::common::path_to_source_spec(path);
    let sources = crate::unified_compile::discover_sources(&source_spec)?;
    let iters = iters.max(1);

    let (raw, _entry_file) = match skull_assemble_project(&sources) {
        Ok(v) => v,
        Err(errs) => {
            eprintln!("{}", render_project_errs(&sources, &errs));
            crate::ablation_report::exit_with_census(1);
        }
    };

    // Warm-up (prime allocator, catch any panic) outside the timed loop.
    {
        let mut c = raw.clone();
        skull_parser::cook(&mut c);
    }

    // Clone-only loop: isolates the harness-reset clone (NOT a batch-compile
    // cost — see fn doc). Skippable via ITER_COOK_NO_CLONE_LOOP=1.
    let mut acc = 0usize;
    let clone_ms = if std::env::var("ITER_COOK_NO_CLONE_LOOP").is_ok() {
        0.0
    } else {
        let tc = Instant::now();
        for _ in 0..iters {
            let c = raw.clone();
            acc = acc.wrapping_add(c.len());
        }
        tc.elapsed().as_secs_f64() * 1000.0 / iters as f64
    };

    // Clone + cook loop: the full `expand_and_desugar` body.
    let t = Instant::now();
    for _ in 0..iters {
        let mut c = raw.clone();
        skull_parser::cook(&mut c);
        acc = acc.wrapping_add(c.len());
    }
    let elapsed = t.elapsed();
    let per = elapsed.as_secs_f64() * 1000.0 / iters as f64;
    let _ = acc;
    log::info!(target: "PROFILE",
        "skull iter-cook {}: {} iters x {} modules, {:.4}ms/iter (clone+cook), {:.4}ms/iter clone-only, {:.4}ms/iter cook-only",
        path, iters, raw.len(), per, clone_ms, (per - clone_ms).max(0.0));

    // Split cook into its two sub-phases (macro-expand vs syntax desugar), each
    // timed on a freshly cloned AST so the desugar pass sees post-expand input
    // exactly as `cook` runs it. Clone time is excluded from both sub-totals.
    let (mut exp_ns, mut des_ns) = (0u128, 0u128);
    for _ in 0..iters {
        let mut c = raw.clone();
        let te = Instant::now();
        skull_parser::macro_expand::expand_macros(&mut c);
        exp_ns += te.elapsed().as_nanos();
        let td = Instant::now();
        for ast in c.iter_mut() {
            skull_parser::desugar::desugar(ast);
        }
        des_ns += td.elapsed().as_nanos();
        acc = acc.wrapping_add(c.len());
    }
    let _ = acc;
    let exp_ms = exp_ns as f64 / 1e6 / iters as f64;
    let des_ms = des_ns as f64 / 1e6 / iters as f64;
    log::info!(target: "PROFILE",
        "  cook split: {:.4}ms/iter macro-expand, {:.4}ms/iter desugar",
        exp_ms, des_ms);
    Ok(())
}

/// Assemble a whole *project* directory (a dir with a `stick.toml`) into bone
/// modules: every project + dependency `.rock` file (via the normal compiler's
/// dependency resolver) plus the bone stdlib. Returns the parsed modules and the
/// entry-module file path (`main.rock`, else the first project file); resolve it
/// to a `ModuleId` via `bone_backend::entry_module_id` once the workspace exists.
///
/// Unlike the single-file path, the embedded compiler stdlib is NOT pulled from
/// `discover_sources` — bone supplies its own via `bone_stdlib_modules()`.
pub(crate) fn skull_assemble_project(
    sources: &SourceFiles,
) -> std::result::Result<(Vec<skull_parser::Ast>, ustr::Ustr), Vec<CompilerError>> {
    let (user_modules, entry_file, _parse) = crate::bone_backend::parse_user_modules(sources)?;
    Ok((crate::bone_backend::with_stdlib(user_modules), entry_file))
}

/// A Lua harness appended after the emitted module: it runs each `@test` fn with
/// its `print` AND `io.write` captured (both restored before the markers are
/// written), and writes one `__SKULL__<TAB>PASS|FAIL<TAB>detail` line and one
/// wall-time `TIME` line per test. Before each test it writes a
/// `ROCK_TEST_START` marker with this run's `token` to stderr; the token and
/// ordinal also trail the TIME line so the host can count completed tests as
/// progress. `ROCK_TEST_SHARD_TESTS` (1-based indices)
/// restricts a process to one shard's tests; start ordinals count within it.
/// Capturing `io.write` too is required: tests that emit via `io.write` (raw,
/// no trailing newline) would otherwise bypass the buffer and read as empty.
fn build_test_harness(tests: &[skull::SkullTest], token: &str) -> String {
    fn lua_str(s: &str) -> String {
        let mut o = String::from("\"");
        for c in s.chars() {
            match c {
                '"' => o.push_str("\\\""),
                '\\' => o.push_str("\\\\"),
                '\n' => o.push_str("\\n"),
                '\r' => o.push_str("\\r"),
                '\t' => o.push_str("\\t"),
                _ => o.push(c),
            }
        }
        o.push('"');
        o
    }

    let mut h = format!("\nlocal __skull_token = {}\nlocal __skull_T = {{\n", lua_str(token));
    for t in tests {
        let out = match &t.output {
            Some(s) => lua_str(s),
            None => "nil".to_string(),
        };
        // `f` is the global test fn by its EMITTED symbol; `out` is the expected
        // stdout (or nil). `name` labels the FAIL line and stays the logical
        // DefKey, which is what a human reading the report wants (a package unit
        // runs many tests together, and two modules may declare the same name).
        // `val` marks a `@test(returns = ...)` test: the harness emits a VALUE
        // line carrying the serialized return value; the HOST compares it.
        h.push_str(&format!(
            "  {{ f = {}, out = {}, name = {}, val = {} }},\n",
            t.symbol,
            out,
            lua_str(&t.name),
            t.returns.is_some()
        ));
    }
    h.push_str("}\n");
    h.push_str(
        r#"local function __esc(s) return (s:gsub("\\", "\\\\"):gsub("\n", "\\n"):gsub("\t", "\\t")) end
-- os.clock is CPU time on POSIX and misses sleeps and waits, so tests are timed
-- with the monotonic clock; MSVC's clock() is already wall time.
local __skull_now = function() return os.clock() * 1e3 end
do
  local ffi = require("ffi")
  if ffi.os ~= "Windows" then
    ffi.cdef[[struct __skull_timespec { long tv_sec; long tv_nsec; };
      int __skull_clock_gettime(int, struct __skull_timespec *) __asm__("clock_gettime");]]
    local ts = ffi.new("struct __skull_timespec")
    local monotonic = ffi.os == "OSX" and 6 or 1
    __skull_now = function()
      assert(ffi.C.__skull_clock_gettime(monotonic, ts) == 0, "clock_gettime failed")
      return tonumber(ts.tv_sec) * 1e3 + tonumber(ts.tv_nsec) / 1e6
    end
  end
end
io.stdout:setvbuf("no")
local __skull_gc_mb = tonumber(os.getenv("ROCK_TEST_GC_COLLECT_MB") or "512")
assert(__skull_gc_mb and __skull_gc_mb >= 0 and __skull_gc_mb < math.huge,
  "ROCK_TEST_GC_COLLECT_MB must be a finite non-negative number (0 disables)")
local __skull_gc_limit_kb = __skull_gc_mb * 1024
local __skull_run = {}
local __skull_shard = os.getenv("ROCK_TEST_SHARD_TESTS")
if __skull_shard then
  for i in __skull_shard:gmatch("%d+") do __skull_run[#__skull_run + 1] = __skull_T[tonumber(i)] end
else
  __skull_run = __skull_T
end
for n, t in ipairs(__skull_run) do
  io.stderr:write("\nROCK_TEST_START\t", __skull_token, "\t", n - 1, "\n")
  io.stderr:flush()
  local buf = {}
  local rp = print
  local rw = io.write
  -- Model real stdout byte-faithfully: `print` joins args with TAB and adds a
  -- trailing newline; `io.write` appends raw fragments. One accumulator; only
  -- TRAILING whitespace stripped (eats print's `\n` + trailing-space separators
  -- like `{i} ` loops). Leading whitespace is significant so a right-aligned /
  -- space-padded `{x:5}` -> `   42` is testable via @test(output=..).
  print = function(...)
    local p = {}
    for i = 1, select('#', ...) do p[i] = tostring((select(i, ...))) end
    buf[#buf + 1] = table.concat(p, "\t") .. "\n"
  end
  io.write = function(...)
    for i = 1, select('#', ...) do buf[#buf + 1] = tostring((select(i, ...))) end
  end
  local start = __skull_now()
  local ok, ret = pcall(t.f)
  local ms = __skull_now() - start
  print = rp
  io.write = rw
  local got = table.concat(buf, ""):gsub("%s+$", "")
  local result = "fail"
  if not ok then
    io.write("\n__SKULL__\tFAIL\t", t.name, ": error: ", __esc(tostring(ret)), "\n")
  elseif t.out ~= nil and got ~= t.out then
    io.write("\n__SKULL__\tFAIL\t", t.name, ": output mismatch: expected <", __esc(t.out), "> got <", __esc(got), ">\n")
  elseif t.val then
    result = "value"
    -- %.17g round-trips any f64; booleans via tostring. The host does the
    -- comparison (test_protocol::compare_value), never this harness.
    local s
    if type(ret) == "number" then s = string.format("%.17g", ret) else s = tostring(ret) end
    io.write("\n__SKULL__\tVALUE\t", t.name, "\t", s, "\n")
  else
    result = "pass"
    io.write("\n__SKULL__\tPASS\t\n")
  end
  io.write("\n__SKULL__\tTIME\t", t.name, "\t", string.format("%.3f", ms), "\t", result,
    "\t", __skull_token, "\t", n - 1, "\n")
  -- Reclaim floating test garbage without changing the collector's pacing.
  if __skull_gc_limit_kb > 0 and collectgarbage("count") > __skull_gc_limit_kb then
    collectgarbage("collect")
  end
end
"#,
    );
    h
}

/// Render single-file errors against `src`. `skull_emit_full` parses with the
/// real `file` path as the span file id, so the source must be registered under
/// that same id -- registering under a placeholder makes every span lookup fail
/// ("Unable to fetch source") and silently drops the snippet + labels.
pub(crate) fn render_file_errs(file: &str, src: &str, errs: &[CompilerError]) -> String {
    let mut sf = SourceFiles::empty();
    sf.add_or_update_file(source_file_from_path_and_contents(
        file.to_string(),
        file.to_string(),
        src.to_string(),
    ));
    crate::bone_backend::render_errs(&sf, errs)
}

/// `map_err` for the `skull::user_module_id` lookup — the same message at every
/// call site.
fn no_user_module(e: CompilerError) -> anyhow::Error {
    anyhow::anyhow!("no user module: {}", e.message)
}

/// A skull failure: a short bucketed reason + the full rendered detail.
#[derive(Debug)]
struct SkullFailure {
    reason: String,
    rendered: String,
}

/// Outcome of building + running one file. `Pass` means it emitted valid Lua
/// AND every `@test` passed (or it had none). `TestFail` means it emitted valid
/// Lua but some `@test` produced the wrong output or errored — these still count
/// toward "emit valid Lua" but not toward execution. Both variants carry the
/// number of individual `@test` functions involved so the runner can report
/// per-`@test` totals, not just per-file ones.
#[derive(Debug)]
enum SkullOutcome {
    /// All `@test`s passed (or the file had none). `tests` = number that ran
    /// green (0 for a load-only file with no `@test`s).
    Pass { tests: usize, skipped: usize, timings: Vec<TestTiming> },
    /// Some `@test`s failed. `passed` ran green; `fails` holds the rest's detail.
    /// Abnormal shard stops also carry a concise diagnostic for the default CLI
    /// summary, independent of `ROCK_TEST_VERBOSE`.
    TestFail {
        passed: usize,
        fails: Vec<String>,
        skipped: usize,
        timings: Vec<TestTiming>,
        diagnostics: Vec<String>,
    },
}

/// Wall time of one `@test` that finished, for the slowest-tests report and
/// the `ROCK_TEST_TIMINGS` file.
#[derive(Debug, Clone)]
struct TestTiming {
    name: String,
    ms: f64,
    status: &'static str,
}

fn child_exit_status(status: &std::process::ExitStatus) -> String {
    if let Some(code) = status.code() {
        return format!("exit code {code}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            let name = match signal {
                libc::SIGABRT => "SIGABRT",
                libc::SIGBUS => "SIGBUS",
                libc::SIGFPE => "SIGFPE",
                libc::SIGILL => "SIGILL",
                libc::SIGINT => "SIGINT",
                libc::SIGKILL => "SIGKILL",
                libc::SIGPIPE => "SIGPIPE",
                libc::SIGSEGV => "SIGSEGV",
                libc::SIGTERM => "SIGTERM",
                _ => "unknown signal",
            };
            return format!("signal {signal} ({name})");
        }
    }
    "unknown exit status".to_string()
}

fn stderr_excerpt(stderr: &[u8]) -> String {
    const MAX_BYTES: usize = 4096;
    let text = String::from_utf8_lossy(stderr);
    let text = text.trim();
    if text.is_empty() {
        return "<empty>".to_string();
    }
    if text.len() <= MAX_BYTES {
        return text.to_string();
    }
    let mut start = text.len() - MAX_BYTES;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("[earlier stderr omitted]\n{}", &text[start..])
}

fn timeout_cause(run: &crate::common::ProcessResult, limit: Duration) -> String {
    match run.timeout_phase {
        Some(crate::common::TimeoutPhase::TestCpu) => format!(
            "timed out: child CPU-time budget reached ({:.3}s)",
            limit.as_secs_f64()
        ),
        Some(crate::common::TimeoutPhase::StartupCpu) => format!(
            "timed out: emitted-program loading used the child CPU-time budget ({:.3}s)",
            limit.as_secs_f64()
        ),
        Some(crate::common::TimeoutPhase::TestWall) => format!(
            "timed out: wall-clock backstop reached ({}s)",
            crate::common::test_watchdog_wall_backstop(limit).as_secs_f64()
        ),
        Some(crate::common::TimeoutPhase::StartupWall) => format!(
            "timed out: emitted-program loading reached its wall-clock backstop ({}s)",
            crate::common::test_watchdog_wall_backstop(limit).as_secs_f64()
        ),
        _ => format!("timed out: watchdog stopped the child ({:.3}s configured)", limit.as_secs_f64()),
    }
}

/// Run a source string through the full skull pipeline (bone type-check + skull
/// codegen), confirm the emitted Lua loads, and execute its `@test` functions
/// against the real (emitted) stdlib. `Err` = emit/load failure; `Ok` carries
/// the pass/test-fail outcome. `filter` selects which `@test`s run.
fn skull_build(
    file: &str,
    src: &str,
    filter: Option<&crate::test::TestFilter>,
) -> std::result::Result<SkullOutcome, SkullFailure> {
    let _stats = crate::compile_measurements::Scope::begin(file);
    crate::package::prepare_test_sound_bank(file).map_err(|e| SkullFailure {
        reason: "sound bank preparation failed".to_string(),
        rendered: format!("sound bank preparation failed for {file}: {e:#}"),
    })?;
    // bone type-check + skull emit. Errors here are bone's (most files) or a
    // skull codegen gap; both carry a `CompilerError`.
    let (lua, tests, _emits_main) = match skull_emit_full(file, src) {
        Ok(v) => v,
        Err(EmitFullErr::Compile(errs)) => {
            // A file whose `@test`s ALL expect failure is supposed to fail to
            // compile — a bone rejection here is the expected (passing) outcome.
            // (Mirrors the real rock runner's compile-Err branch.)
            if let Some(tests) = all_tests_expect_compile_failure(file, src) {
                let selected = if let Some(filter) = filter {
                    let mut sources = SourceFiles::empty();
                    sources.add_or_update_file(source_file_from_path_and_contents(
                        file.to_string(),
                        file.to_string(),
                        src.to_string(),
                    ));
                    crate::test::find_test_functions_from_sources(&sources)
                        .iter()
                        .filter(|test| filter.matches(&test.name, file))
                        .count()
                } else {
                    tests
                };
                return Ok(SkullOutcome::Pass {
                    tests: selected,
                    skipped: tests - selected,
                    timings: Vec::new(),
                });
            }
            return Err(SkullFailure {
                reason: categorize(&errs[0].message),
                rendered: render_file_errs(file, src, &errs),
            });
        }
        Err(EmitFullErr::Discovery(e)) => {
            return Err(SkullFailure {
                reason: categorize(&e.message),
                rendered: render_file_errs(file, src, &[e]),
            });
        }
    };

    let lua = match configured_stone_dir(
        std::path::Path::new(file)
            .parent()
            .unwrap_or_else(|| std::path::Path::new(".")),
    ) {
        Ok(Some(stone_dir)) => TestProgram {
            preamble: stone_root_preamble(&stone_dir),
            lua,
            setup: "",
        },
        Ok(None) => lua.into(),
        Err(error) => {
            return Err(SkullFailure {
                reason: "Stone root resolution failed".to_string(),
                rendered: error.to_string(),
            })
        }
    };
    let files = vec![file.to_owned(); tests.len()];
    run_emitted_tests(lua, &tests, &files, TEST_CPU_LIMIT, 1, filter, false)
}

/// `Some(count)` when `file` declares `@test` fns and EVERY one carries
/// `expect_failure`/`@ignore` -- a compile rejection is then the expected
/// (passing) outcome and each counts as a passing test. Shared by the luajit
/// and JS executors (both bone-type-check first, so the rule is identical).
pub(crate) fn all_tests_expect_compile_failure(file: &str, src: &str) -> Option<usize> {
    let mut sf = SourceFiles::empty();
    sf.add_or_update_file(source_file_from_path_and_contents(
        file.to_string(),
        file.to_string(),
        src.to_string(),
    ));
    let test_fns = crate::test::find_test_functions_from_sources(&sf);
    if !test_fns.is_empty() && test_fns.iter().all(|t| t.expect_failure || t.ignored) {
        Some(test_fns.len())
    } else {
        None
    }
}

/// Given emitted workspace Lua and the `@test` fns discovered in it, run them
/// via the harness (or the load-only validity gate when there are none) and
/// classify the outcome. Shared by the per-file path ([`skull_build`]) and the
/// whole-package path ([`skull_build_package`]). `filter` selects the tests
/// that run; `shards` LuaJIT processes split the selected tests and their
/// results merge into one outcome. `release_memory` returns the freed program
/// text to the OS before the tests start, for a host that then only waits.
fn run_emitted_tests(
    lua: impl Into<TestProgram>,
    tests: &[skull::SkullTest],
    files: &[String],
    limit: std::time::Duration,
    shards: usize,
    filter: Option<&crate::test::TestFilter>,
    release_memory: bool,
) -> std::result::Result<SkullOutcome, SkullFailure> {
    let lua = lua.into();
    assert_eq!(tests.len(), files.len());

    let mut selected_tests = Vec::with_capacity(tests.len());
    let mut selected_files = Vec::with_capacity(files.len());
    for (test, file) in tests.iter().zip(files) {
        if filter.map_or(true, |filter| filter.matches(&test.name, file)) {
            selected_tests.push(test.clone());
            selected_files.push(file.clone());
        }
    }
    let skipped = tests.len() - selected_tests.len();

    // Failure reports quote the program's head; the whole text is not kept
    // while the test process runs (mag's is about 170 MB).
    // Files without @tests still have a bounded load-only validity gate.
    if tests.is_empty() {
        let (script, excerpt) = write_test_program(lua, "").map_err(|e| SkullFailure {
            reason: "invalid lua emitted (skull bug)".to_string(),
            rendered: e.to_string(),
        })?;
        let run = run_test_program(script.path(), limit, run_token(), 0, None).map_err(|e| SkullFailure {
            reason: "invalid lua emitted (skull bug)".to_string(),
            rendered: e.to_string(),
        })?;
        if run.timed_out || !run.output.status.success() {
            return Err(SkullFailure {
                reason: "invalid lua emitted (skull bug)".to_string(),
                rendered: if run.timed_out {
                    format!("loading emitted program: timed out after {}s", limit.as_secs_f64())
                } else {
                    format!("{}\n\n--- emitted lua (truncated) ---\n{}",
                        format!("{}{}", String::from_utf8_lossy(&run.output.stdout),
                            String::from_utf8_lossy(&run.output.stderr)), excerpt)
                },
            });
        }
        return Ok(SkullOutcome::Pass { tests: 0, skipped: 0, timings: Vec::new() });
    }
    if selected_tests.is_empty() {
        crate::compile_measurements::finish("ok");
        return Ok(SkullOutcome::Pass { tests: 0, skipped, timings: Vec::new() });
    }
    // Only the selected tests exist from here on: the harness, the shard
    // indices and the never-reported accounting all count them alone.
    let (tests, files) = (selected_tests.as_slice(), selected_files.as_slice());

    // Run the @tests: emit + harness, capture each test's PASS/FAIL line.
    // A stable key keeps existing tests in place when declarations are inserted.
    let token = run_token();
    let shards = resolve_test_shards(shards, tests.len(), lua.len(),
        std::env::var_os("ROCK_TEST_SHARDS").is_some());
    let harness = build_test_harness(tests, &token);
    let (script, excerpt) = write_test_program(lua, &harness).map_err(|e| SkullFailure {
        reason: "test run errored (skull bug)".to_string(),
        rendered: e.to_string(),
    })?;
    drop(harness);
    // Per-file runs keep freed pages for the next compile; package runs only wait.
    if release_memory {
        crate::allocator::release_freed_memory();
    }
    let requested_shards = shards.clamp(1, tests.len());
    let mut assignment: Vec<Vec<usize>> = (0..requested_shards).map(|_| Vec::new()).collect();
    for (index, test) in tests.iter().enumerate() {
        assignment[stable_test_shard(&test.name, requested_shards)].push(index);
    }
    assignment.retain(|indices| !indices.is_empty());
    let shards = assignment.len();
    let runs: Vec<Result<Vec<ShardRun>>> = std::thread::scope(|scope| {
        let handles: Vec<_> = assignment.iter().map(|indices| {
            let (script, token) = (script.path(), token.clone());
            scope.spawn(move || run_shard(script, limit, token, indices, shards > 1))
        }).collect();
        handles.into_iter()
            .map(|handle| handle.join().unwrap_or_else(|_| Err(anyhow::anyhow!("test shard thread panicked"))))
            .collect()
    });
    drop(script);

    let expects: HashMap<&str, &skull::ValueExpect> = tests
        .iter()
        .filter_map(|t| t.returns.as_ref().map(|v| (t.name.as_str(), v)))
        .collect();
    let mut passed = 0;
    let mut fails = Vec::new();
    let mut diagnostics = Vec::new();
    let mut timings = Vec::new();
    let mut segments = Vec::new();
    for (shard, run) in runs.into_iter().enumerate() {
        let run = run.map_err(|e| SkullFailure {
            reason: "test run errored (skull bug)".to_string(),
            rendered: e.to_string(),
        })?;
        for (process, segment) in run.into_iter().enumerate() {
            let label = if process == 0 {
                format!("shard {}/{shards}: ", shard + 1)
            } else {
                format!("shard {}/{shards} process {}: ", shard + 1, process + 1)
            };
            segments.push((label, segment));
        }
    }
    for (label, ShardRun { indices, run }) in segments {
        let indices = indices.as_slice();
        // The harness `pcall`s each test, so an ordinary assertion failure is a
        // FAIL marker and the process still exits 0. A non-zero status means
        // the run DIED -- `os.exit`, a C-level abort, an OOM -- and every test
        // after that point never ran in this process. A truncated run still
        // reports what it got; `run_shard` reruns the tests after a stopped one.
        let stdout = String::from_utf8_lossy(&run.output.stdout);
        let stderr = String::from_utf8_lossy(&run.output.stderr);
        let died = !run.output.status.success();

        // No result lines and no test started => the module didn't even load
        // (a skull bug), not a test failure.
        let markers = crate::test_protocol::parse_skull_markers(&stdout);
        if markers.is_none() && !run.timed_out && !died && run.running_test.is_none() {
            return Err(SkullFailure {
                reason: "invalid lua emitted (skull bug)".to_string(),
                rendered: format!("{label}{stdout}\n{stderr}\n\n--- emitted lua (truncated) ---\n{excerpt}"),
            });
        }
        let markers = markers.unwrap_or(crate::test_protocol::SkullMarkers {
            passed: 0, fails: Vec::new(), values: Vec::new(), times: Vec::new(),
        });
        let reported = markers.passed + markers.fails.len() + markers.values.len();
        passed += markers.passed;
        fails.extend(markers.fails);

        // VALUE lines: compare the serialized return value HOST-side against
        // the pinned `@test(returns = ...)` literal.
        let mut value_fails = HashSet::new();
        for (name, val) in &markers.values {
            let Some(expect) = expects.get(name.as_str()) else {
                return Err(SkullFailure {
                    reason: "harness invariant breach (skull bug)".to_string(),
                    rendered: format!("VALUE line for `{name}` without a returns expectation"),
                });
            };
            match crate::test_protocol::compare_value(expect, val, crate::test_protocol::FLOAT_REL_TOL)
            {
                Ok(()) => passed += 1,
                Err(detail) => {
                    value_fails.insert(name.as_str());
                    fails.push(format!("{name}: {detail}"));
                }
            }
        }
        // A shard's TIME lines follow its run order, so the j-th is test
        // `indices[j]`; keying by it restores declaration order after merging.
        timings.extend(markers.times.iter().zip(indices).map(|(time, &index)| (index, TestTiming {
            name: time.name.clone(),
            ms: time.ms,
            status: match time.result.as_str() {
                "pass" => "pass",
                "value" if !value_fails.contains(time.name.as_str()) => "pass",
                _ => "fail",
            },
        })));

        // Every selected test owes exactly one marker. Counting only the
        // markers that ARRIVED makes a truncated run self-consistent -- three
        // tests, one marker, `1/1 passed` -- so each test that never reported
        // is a named failure. This is the only signal for a premature
        // `os.exit(0)`, which exits success and emits no marker at all.
        let stop = if run.timed_out {
            timeout_cause(&run, limit)
        } else if died {
            "child process stopped".to_string()
        } else {
            "process exited 0 before all tests reported".to_string()
        };
        let missing = indices.iter().copied().skip(reported).collect::<Vec<_>>();
        if run.timed_out || died || !missing.is_empty() {
            let state = if let Some(position) = run.running_test {
                indices.get(position).map(|&index| format!(
                    "running test `{}` ({})",
                    tests[index].name,
                    files[index],
                )).unwrap_or_else(|| format!("running shard test ordinal {position}"))
            } else if let Some(position) = run.last_completed_test {
                indices.get(position).map(|&index| format!(
                    "between tests after `{}` ({})",
                    tests[index].name,
                    files[index],
                )).unwrap_or_else(|| "between tests".to_string())
            } else {
                "before the first test started".to_string()
            };
            let missing = missing.iter().map(|&index| format!(
                "{}: never reported ({})",
                tests[index].name,
                files[index],
            )).collect::<Vec<_>>().join(", ");
            diagnostics.push(format!(
                "{label}stopped: {stop}; exit status: {}; {state}; never reported: {}; stderr:\n{}",
                child_exit_status(&run.output.status),
                if missing.is_empty() { "<none>" } else { &missing },
                stderr_excerpt(&run.output.stderr),
            ));
        }
        for (position, &index) in indices.iter().enumerate().skip(reported) {
            let reason = if run.running_test == Some(position) {
                format!("{stop} while it ran")
            } else if run.running_test.is_none() && run.last_completed_test.is_some() {
                format!("{stop} between tests")
            } else {
                "its process stopped before it ran".to_string()
            };
            fails.push(format!("{}: never reported ({}; {label}{reason})", tests[index].name, files[index]));
        }
        if reported >= indices.len() && died {
            fails.push(format!(
                "{label}every @test reported, but the harness process exited non-zero -- \
                 something after the tests failed"
            ));
        }
    }
    timings.sort_by_key(|(index, _)| *index);
    let timings = timings.into_iter().map(|(_, timing)| timing).collect();

    if fails.is_empty() {
        Ok(SkullOutcome::Pass { tests: passed, skipped, timings })
    } else {
        Ok(SkullOutcome::TestFail { passed, fails, skipped, timings, diagnostics })
    }
}

/// Child CPU time one `@test` (or program loading) may use: a hang backstop,
/// not a speed bound. A loaded host inflates a test's CPU time (memory
/// reclaim, E-cores, busy SMT siblings): the slowest `mag` test used 21 s on
/// host-a at load 70 with little free memory, and 16 s under an all-core
/// busy-loop burner.
const TEST_CPU_LIMIT: std::time::Duration = std::time::Duration::from_secs(120);

/// One test process of a shard and the tests it was given.
struct ShardRun {
    indices: Vec<usize>,
    run: crate::common::ProcessResult,
}

/// Processes a shard may start after the first. A stopped test is charged by
/// name and the tests after it run in a fresh process; the cap bounds a
/// defect that stops every test to a few process starts and budgets.
const SHARD_RESTARTS: usize = 3;

/// Run one shard's tests. When the process stops while a test runs (watchdog,
/// crash, early exit), that test keeps the stopped run and the tests after it
/// rerun in a fresh process, so one stopped test never takes the rest of the
/// shard with it. `select` passes the indices even for a whole-program run.
fn run_shard(
    script: &std::path::Path,
    limit: std::time::Duration,
    token: String,
    indices: &[usize],
    select: bool,
) -> Result<Vec<ShardRun>> {
    let mut runs = Vec::new();
    let mut rest = indices;
    loop {
        let selection = (select || !runs.is_empty()).then(|| {
            rest.iter().map(|i| (i + 1).to_string()).collect::<Vec<_>>().join(",")
        });
        let run = run_test_program(script, limit, token.clone(), rest.len(), selection.as_deref())?;
        // A test still running when the process ended is the one that stopped it.
        let resume_at = run.running_test
            .filter(|_| runs.len() < SHARD_RESTARTS)
            .map(|position| position + 1)
            .filter(|&next| next < rest.len());
        let Some(next) = resume_at else {
            runs.push(ShardRun { indices: rest.to_vec(), run });
            return Ok(runs);
        };
        runs.push(ShardRun { indices: rest[..next].to_vec(), run });
        rest = &rest[next..];
    }
}

/// A per-run token for the harness's start markers, only distinct from
/// accidental test output. Lead decision: the per-test limit is a hang guard
/// for this repo's own tests, not a security boundary, so deliberate
/// in-process forgery by Lua code (debug locals, readable harness source,
/// intercepted harness writes) is out of scope.
fn run_token() -> String {
    use std::hash::{BuildHasher, Hasher};
    let half = || {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u128(std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
        hasher.write_u32(std::process::id());
        hasher.finish()
    };
    format!("{:016x}{:016x}", half(), half())
}

struct TestProgram {
    preamble: String,
    lua: String,
    setup: &'static str,
}

impl TestProgram {
    fn len(&self) -> usize {
        self.preamble.len() + self.lua.len() + self.setup.len()
    }
}

impl From<String> for TestProgram {
    fn from(lua: String) -> Self {
        Self { preamble: String::new(), lua, setup: "" }
    }
}

fn program_excerpt(parts: &[&str]) -> String {
    let bytes: usize = parts.iter().map(|part| part.len()).sum();
    let mut head = String::with_capacity(bytes.min(4000));
    for part in parts {
        let end = part.floor_char_boundary(4000 - head.len());
        head.push_str(&part[..end]);
        if end < part.len() {
            break;
        }
    }
    if bytes > 4000 {
        head.push_str(&format!("\n... ({} more bytes)", bytes - head.len()));
    }
    head
}

/// A file avoids blocking on stdin before the startup watchdog is running, and
/// lets every shard load the same program.
fn write_test_program(program: TestProgram, harness: &str) -> Result<(tempfile::NamedTempFile, String)> {
    use std::io::Write;
    let parts = [&*program.preamble, &*program.lua, program.setup, harness];
    let excerpt = program_excerpt(&parts);
    let mut script = tempfile::NamedTempFile::new()?;
    for part in parts {
        script.write_all(part.as_bytes())?;
    }
    crate::compile_measurements::phase("write");
    crate::compile_measurements::finish("ok");
    Ok((script, excerpt))
}

/// Run `script` under the per-test watchdog. `selection` restricts the run to
/// one shard's tests (`ROCK_TEST_SHARD_TESTS`); `tests` counts the tests run.
fn run_test_program(
    script: &std::path::Path,
    limit: std::time::Duration,
    token: String,
    tests: usize,
    selection: Option<&str>,
) -> Result<crate::common::ProcessResult> {
    use std::process::Stdio;
    use rock_host::process_management::{register_child_process, unregister_child_process};

    let mut command = rock_host::project::build_luajit_command(script, &[]);
    crate::common::apply_test_rock_dir(&mut command);
    // Shards and concurrent runs share one worktree; each process gets its own
    // scratch root, removed when this returns.
    let _scratch = crate::common::test_scratch_dir(&mut command)?;
    // An inherited selection would silently narrow an unsharded run.
    match selection {
        Some(selection) => command.env("ROCK_TEST_SHARD_TESTS", selection),
        None => command.env_remove("ROCK_TEST_SHARD_TESTS"),
    };
    let mut child = command
        .env("STONE_HEADLESS", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let pid = child.id();
    register_child_process(pid);
    let result = crate::common::ProcessManager::wait_with_test_watchdog(
        &mut child, limit, crate::common::TestWatch { token, tests });
    if result.is_err() {
        if let Err(error) = child.kill() {
            log::error!(target: "CLI", "failed to kill Lua test process: {error}");
        }
        if let Err(error) = child.wait() {
            log::error!(target: "CLI", "failed to reap Lua test process: {error}");
        }
    }
    unregister_child_process(pid);
    result
}

/// A directory that is a `stick.toml` package -- the unit `rock test` builds as
/// ONE whole workspace (all its files + deps + its own `tests/`) rather than
/// file-by-file. A `type=lib` split across files can only be tested this way:
/// per-file, a file referencing a sibling's symbol fails to compile. Root
/// `tests/` (no stick.toml, hundreds of independent colliding programs) is NOT
/// a package and stays on the per-file path; a package NESTED under it is one,
/// and [`walk_test_units`] finds it.
fn is_test_package_dir(path: &str) -> bool {
    let p = std::path::Path::new(path);
    p.is_dir() && p.join("stick.toml").is_file()
}

pub(crate) fn is_package_tests_dir(path: &str) -> Option<String> {
    let p = std::path::Path::new(path);
    if p.is_dir() {
        let name = p.file_name().and_then(|n| n.to_str());
        if name == Some("tests") || name == Some("tests/") {
            let parent = p.parent().unwrap_or(std::path::Path::new(""));
            let candidate = if parent.as_os_str().is_empty() {
                std::path::Path::new(".")
            } else {
                parent
            };
            if candidate.join("stick.toml").is_file() {
                return Some(candidate.to_string_lossy().to_string());
            }
        }
    }
    None
}

/// Split the `.rock` files under `path` into loose-file units and PACKAGE
/// units. A directory holding a `stick.toml` is a package: it becomes one unit
/// compiled through its own manifest, and the walk does not descend into it.
///
/// Building a package's sources as loose files drops the manifest that supplies
/// their dialect and their dependencies, so a v1 source is parsed in the legacy
/// dialect and fails with `unsupported item start` -- an emit failure that says
/// nothing about the package it came from. Spec 6.3 makes this the ownership
/// rule: a nested manifest starts a different package and is not absorbed as
/// source of the tree above it.
///
/// A path naming a single file stays that one file, and the `~` and extension
/// filters match `rock_host::project::find_rock_files_at_path`, which this
/// replaces on the `rock test` route only.
fn walk_test_units(
    path: &str,
    files: &mut Vec<String>,
    package_dirs: &mut Vec<String>,
) -> Result<()> {
    let p = std::path::Path::new(path);
    if !p.exists() {
        anyhow::bail!("Path does not exist: {}", path);
    }
    if p.is_file() {
        files.push(path.to_string());
        return Ok(());
    }
    let base = fs_err::canonicalize(p)?;
    let mut walk = walkdir::WalkDir::new(&base).sort_by_file_name().into_iter();
    while let Some(entry) = walk.next() {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                eprintln!("rock: skipping path under `{}` while scanning for tests: {e}", base.display());
                continue;
            }
        };
        let entry_path = entry.path();
        if entry.file_type().is_dir() {
            if entry_path != base && entry_path.join("stick.toml").is_file() {
                package_dirs.push(entry_path.to_string_lossy().into_owned());
                walk.skip_current_dir();
            }
            continue;
        }
        let name = entry_path.to_string_lossy();
        if entry_path.extension().is_some_and(|e| e == "rock") && !name.ends_with('~') {
            files.push(name.into_owned());
        }
    }
    Ok(())
}

/// Assemble + type-check + emit a whole package (tests INCLUDED), then run its
/// `@test`s. The package analogue of [`skull_build`]. Cross-file references
/// resolve because every file is in one workspace.
fn skull_build_package(
    dir: &str,
    shards: usize,
    filter: Option<&crate::test::TestFilter>,
) -> std::result::Result<SkullOutcome, SkullFailure> {
    skull_build_package_with_limit(dir, TEST_CPU_LIMIT, shards, filter)
}

fn skull_build_package_with_limit(
    dir: &str,
    limit: std::time::Duration,
    shards: usize,
    filter: Option<&crate::test::TestFilter>,
) -> std::result::Result<SkullOutcome, SkullFailure> {
    let _stats = crate::compile_measurements::Scope::begin(dir);
    // Like a game run, so tests never read a bank an earlier tree compiled.
    crate::package::prepare_test_sound_bank(dir).map_err(|e| SkullFailure {
        reason: "sound bank preparation failed".to_string(),
        rendered: format!("sound bank preparation failed for {dir}: {e:#}"),
    })?;
    let slot = crate::compile_slots::acquire(dir).map_err(|e| SkullFailure {
        reason: "no compile slot".to_string(),
        rendered: format!("{e:#}"),
    })?;
    crate::compile_measurements::slot(slot.acquisition());
    let spec = crate::unified_compile::SourceSpec::Directory(std::path::PathBuf::from(dir));
    let sources = crate::unified_compile::discover_sources(&spec).map_err(|e| SkullFailure {
        reason: "cannot load project".to_string(),
        rendered: e.to_string(),
    })?;
    let sources = crate::package_test_sources::select(&sources, std::path::Path::new(dir))
        .map_err(|error| SkullFailure {
            reason: "cannot select package test sources".to_string(),
            rendered: format!("{error:#}"),
        })?;
    crate::compile_measurements::phase("discovery");
    let (mut modules, entry_file) = skull_assemble_project(&sources).map_err(|errs| SkullFailure {
        reason: categorize(&errs[0].message),
        rendered: render_project_errs(&sources, &errs),
    })?;
    crate::compile_measurements::phase("parse+stdlib");
    let mut ws = crate::bone_backend::check_modules(&mut modules).map_err(|errs| SkullFailure {
        reason: categorize(&errs[0].message),
        rendered: render_project_errs(&sources, &errs),
    })?;
    crate::compile_measurements::phase("semantic");
    crate::compile_measurements::timings("bone", &ws.phase_timings);
    // The workspace owns everything emission reads; the parse trees (about
    // 1 GB for mag) would otherwise stay live through the emit peak.
    drop(modules);
    crate::comptime::stamp_workspace(&mut ws, std::path::Path::new(dir)).map_err(|e| {
        SkullFailure {
            reason: "build var resolution failed".to_string(),
            rendered: format!("build var resolution failed for {dir}: {e:#}"),
        }
    })?;
    let entry_mid = crate::bone_backend::entry_module_id(&ws, entry_file);
    let test_fns = collect_package_test_fns(&ws, dir).map_err(|e| SkullFailure {
        reason: categorize(&e.message),
        rendered: render_project_err(&sources, &e),
    })?;
    let roots = test_fns.iter().map(|(func, _)| *func).collect::<Vec<_>>();
    let lua = match skull::mir_emit::emit_workspace_mir_for_package_test(&ws, entry_mid, &roots) {
        Ok(e) => {
            crate::compile_measurements::emission(&e);
            e.lua
        },
        Err(e) => {
            return Err(SkullFailure {
                reason: categorize(&e.message),
                rendered: render_project_err(&sources, &e),
            });
        }
    };
    let files = test_fns.iter().map(|(func, _)| {
        ws.modules[func.module.index()].file.as_str().to_owned()
    }).collect::<Vec<_>>();
    let tests = test_fns.into_iter().map(|(_, test)| test).collect::<Vec<_>>();
    let needs_libstone = needs_libstone(&ws);
    // The compiler state is dead once the program is emitted. Free it before
    // the program text is copied and the test process runs: mag's peaks near
    // 5 GB, and keeping it resident doubled the memory a `rock test` needed
    // while its luajit child ran.
    drop((ws, sources));
    crate::allocator::release_freed_memory();
    drop(slot);
    let lua = prepend_package_preamble(lua, dir, needs_libstone)?;
    crate::compile_measurements::phase("prepare");
    run_emitted_tests(lua, &tests, &files, limit, shards, filter, true)
}

/// Package shard limit; the default resolves against the selected tests and
/// program size in `run_emitted_tests`, before the program text is freed.
fn test_shard_count() -> Result<usize> {
    let value = match std::env::var("ROCK_TEST_SHARDS") {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => None,
        Err(error) => anyhow::bail!("ROCK_TEST_SHARDS: {error}"),
    };
    test_shard_count_from(value.as_deref(),
        std::thread::available_parallelism().map_or(1, |cores| cores.get()))
}

fn test_shard_count_from(value: Option<&str>, cores: usize) -> Result<usize> {
    match value {
        Some(value) => match value.trim().parse::<usize>() {
            Ok(shards) if shards > 0 => Ok(shards),
            _ => anyhow::bail!("ROCK_TEST_SHARDS must be a positive integer, got `{value}`"),
        },
        None => Ok(cores.clamp(1, 2)),
    }
}

fn stable_test_shard(name: &str, shards: usize) -> usize {
    let hash = name.as_bytes().iter().fold(0xcbf29ce484222325u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    });
    (hash % shards as u64) as usize
}

fn resolve_test_shards(shards: usize, tests: usize, program_bytes: usize, overridden: bool) -> usize {
    if !overridden && (tests < 60 || program_bytes < 8 * 1024 * 1024) {
        1
    } else {
        shards
    }
}

#[cfg(test)]
#[path = "../tests/test_shards/policy.rs"]
mod test_shard_policy;

/// Whether the checked program declares a native binding into libstone. The
/// resolved graph can supply native bindings without the full Stone host; this
/// reads the checked FFI namespace, independent of package names and aliases.
fn needs_libstone(ws: &bone_semantic::inference::Workspace) -> bool {
    ws.modules.iter().any(|module| {
        module.functions.iter().any(|(_, function)| {
            function.is_extern
                && function.backend != bone_semantic::hir::FnBackend::Js
                && function.ffi_lib.is_some_and(|library| library.as_str() == "stone")
        })
    })
}

/// Package tests stage native Stone bindings and keep configured asset roots.
/// Full Stone dependencies also load its CPU host. Lifecycle hooks and the
/// frame runner are not invoked: no SDL, window, GL or audio device is started.
fn prepend_package_preamble(
    lua: String,
    dir: &str,
    needs_libstone: bool,
) -> std::result::Result<TestProgram, SkullFailure> {
    let path = std::path::Path::new(dir);
    let stone_dir = configured_stone_dir(path).map_err(|error| SkullFailure {
        reason: "Stone root resolution failed".to_string(),
        rendered: error.to_string(),
    })?;
    let is_game = rock_host::project::is_game_project(path).map_err(|error| SkullFailure {
        reason: "test host resolution failed".to_string(),
        rendered: error.to_string(),
    })?;
    if is_game || stone_dir.is_some() || needs_libstone {
        // Core libstone only: the CPU host does not use optional game cdylibs.
        rock_host::engine_build::build_libstone().map_err(|error| SkullFailure {
            reason: "engine library staging failed".to_string(),
            rendered: format!(
                "`rock test {dir}` could not stage libstone for the test process: {error:#}"
            ),
        })?;
    }
    let preamble = if is_game {
        skull_game_preamble(dir).map_err(|error| SkullFailure {
            reason: "game preamble resolution failed".to_string(),
            rendered: error.to_string(),
        })?
    } else if let Some(stone_dir) = &stone_dir {
        stone_root_preamble(stone_dir)
    } else {
        String::new()
    };
    // The CPU host starts after definitions and before the harness.
    let setup = if stone_dir.is_some() { "\nsetup_stone()\n" } else { "" };
    Ok(TestProgram { preamble, lua, setup })
}

/// The `@test` fns defined in the package rooted at `dir` -- its own source
/// files and its `tests/`, but NOT stdlib or dependency packages (which live at
/// other canonical paths). Module file paths are canonicalized absolute, so a
/// prefix match against the canonicalized `dir` selects exactly this package.
/// Carry each test's `FuncRef` so executors retain its source file.
pub(crate) fn collect_package_test_fns(
    ws: &bone_semantic::inference::Workspace,
    dir: &str,
) -> std::result::Result<Vec<(skull_mir::mir::FuncRef, skull::SkullTest)>, CompilerError> {
    use bone_semantic::prelude::ModuleId;
    let root = std::path::Path::new(dir)
        .canonicalize()
        .unwrap_or_else(|_| std::path::PathBuf::from(dir));
    let mut out = Vec::new();
    for m in ws.modules.iter() {
        if m.id == ModuleId::builtin() {
            continue;
        }
        let under = std::path::Path::new(m.file.as_str())
            .canonicalize()
            .map(|c| c.starts_with(&root))
            .unwrap_or(false);
        if under {
            out.extend(
                skull::module_tests_with_ids(ws, m.id)?
                    .into_iter()
                    .map(|(fid, t)| (skull_mir::mir::FuncRef { module: m.id, func: fid }, t)),
            );
        }
    }
    Ok(out)
}

/// Keep emitted-Lua dumps in error reports manageable.
#[cfg(test)]
fn truncate(s: &str) -> String {
    if s.len() > 4000 {
        let end = s.floor_char_boundary(4000);
        format!("{}\n... ({} more bytes)", &s[..end], s.len() - end)
    } else {
        s.to_string()
    }
}

/// Parse the `--profile` value for the `rock shader` entry forms -- the only
/// command that selects a profile (in-game source is emitted once and adapted
/// to the live GL context by the engine). No fallback: an unknown value rejects
/// listing the valid ones.
pub(crate) fn parse_glsl_profile(s: Option<&str>) -> Result<skull_codegen_glsl::GlslProfile> {
    use skull_codegen_glsl::GlslProfile;
    match s {
        None | Some("330core") => Ok(GlslProfile::Core330),
        Some("300es") => Ok(GlslProfile::Es300),
        Some(other) => {
            anyhow::bail!("rock shader: unknown --profile `{other}` (valid: 330core, 300es)")
        }
    }
}

fn emit_shader_program_with_view(
    ws: &bone_semantic::inference::Workspace,
    program: skull::shader_artifact::ShaderProgram,
    profile: skull_codegen_glsl::GlslProfile,
) -> std::result::Result<skull_codegen_glsl::ShaderEmission, skull_parser::CompilerError> {
    let roots: Vec<skull_mir::mir::FuncRef> = program
        .vert
        .into_iter()
        .chain(std::iter::once(program.frag))
        .collect();
    let graph = skull::instance_graph::build(ws, roots, false);
    let shader_scx = skull_mir::lower::ScalarCtx::new_values_first_class(ws)
        .sharing(std::sync::Arc::clone(graph.interner()));
    let programs = [program];
    let view = skull::shader_artifact::realize_shader_with_context(
        ws,
        &graph,
        &skull::shader_artifact::ShaderArtifactRequest {
            spec: skull::target_projection::TargetSpec {
                shader: Some(skull::target_projection::ShaderTargetSpec::Glsl(profile)),
                ..skull::target_projection::LUAJIT_DESKTOP
            },
            programs: &programs,
        },
        &shader_scx,
    )
    .ok_or_else(|| skull_parser::CompilerError::new_no_location("GLSL shader view is unavailable".to_string()))?;
    let program_view = view
        .program_views
        .first()
        .ok_or_else(|| skull_parser::CompilerError::new_no_location("GLSL shader view has no program".to_string()))?;
    match program.vert {
        Some(_) => skull_codegen_glsl::emit_shader_pipeline_with_view(
            ws,
            &shader_scx,
            program_view,
            profile,
        ),
        None => skull_codegen_glsl::emit_shader_entry_with_view(
            ws,
            &shader_scx,
            program_view,
            profile,
        ),
    }
}

/// `rock shader <file.rock> <fn> [fn...] [--output <file.glsl>]` -- GLSL emission.
/// Three forms:
///   - fn library (route B): plain roots, no `#version` line -- `--profile`
///     rejects here (nothing for it to select).
///   - single `@shader(fragment)` root (A1 fullscreen) and
///     `<vert_fn> <frag_fn>` (A2 pipeline, selected when a root is
///     `@shader(vertex)`): both take `[--profile 330core|300es]`.
/// A profile violation renders spanned against the source and exits non-zero.
pub fn skull_shader(files: &[String], output: Option<String>, profile: Option<String>) -> Result<()> {
    let Some((path, roots)) = files.split_first() else {
        anyhow::bail!("rock shader: usage: rock shader <file.rock> <fn> [fn...] --output <file.glsl>");
    };
    if roots.is_empty() {
        anyhow::bail!("rock shader: need at least one root fn name");
    }
    if let Some(out) = output.as_deref() {
        invalidate_shader_output(path, out)?;
    }
    let src = match fs_err::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            log::error!(target: "SHADER", "cannot read {}: {}", path, e);
            crate::ablation_report::exit_with_census(1);
        }
    };
    let user = match skull_parser::parse_module(&src, ustr::Ustr::from(path.as_str())) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("{}", render_file_errs(path, &src, &[e]));
            crate::ablation_report::exit_with_census(1);
        }
    };
    let user_file = user.file;
    let ws = match crate::bone_backend::check_with_stdlib([user]) {
        Ok(ws) => ws,
        Err(errs) => {
            eprintln!("{}", render_file_errs(path, &src, &errs));
            crate::ablation_report::exit_with_census(1);
        }
    };
    let user_mid = crate::bone_backend::entry_module_id(&ws, user_file);
    let roots: Vec<String> = roots.to_vec();

    // Pipeline form: any root is a `@shader(vertex)` entry.
    let is_pipeline = roots
        .iter()
        .any(|r| skull_codegen_glsl::is_vertex_shader_root(&ws, user_mid, r));

    let glsl = if is_pipeline {
        if roots.len() != 2 {
            anyhow::bail!("rock shader: the vertex+fragment pipeline form takes exactly two roots: <vert_fn> <frag_fn>");
        }
        let prof = parse_glsl_profile(profile.as_deref())?;
        let vert = match skull_codegen_glsl::resolve_root(&ws, user_mid, &roots[0]) {
            Ok(root) => root,
            Err(e) => {
                eprintln!("{}", render_file_errs(path, &src, &[e]));
                crate::ablation_report::exit_with_census(1);
            }
        };
        let frag = match skull_codegen_glsl::resolve_root(&ws, user_mid, &roots[1]) {
            Ok(root) => root,
            Err(e) => {
                eprintln!("{}", render_file_errs(path, &src, &[e]));
                crate::ablation_report::exit_with_census(1);
            }
        };
        match emit_shader_program_with_view(
            &ws,
            skull::shader_artifact::ShaderProgram { vert: Some(vert), frag },
            prof,
        )
        .map(|emission| emission.source)
        {
            Ok(g) => g,
            Err(e) => {
                eprintln!("{}", render_file_errs(path, &src, &[e]));
                crate::ablation_report::exit_with_census(1);
            }
        }
    } else {
        // The route-B fn library emits no `#version` line, so a profile cannot
        // apply to it; the single-root fullscreen entry takes one like the
        // pipeline form does.
        let is_entry = roots.len() == 1
            && skull_codegen_glsl::is_fragment_shader_root(&ws, user_mid, &roots[0]);
        if profile.is_some() && !is_entry {
            anyhow::bail!("rock shader: --profile applies to shader entries (pipeline or @shader(fragment) fullscreen), not the route-B fn library (it emits no #version line)");
        }
        let prof = parse_glsl_profile(profile.as_deref())?;
        let glsl = if is_entry {
            let frag = match skull_codegen_glsl::resolve_root(&ws, user_mid, &roots[0]) {
                Ok(root) => root,
                Err(e) => {
                    eprintln!("{}", render_file_errs(path, &src, &[e]));
                    crate::ablation_report::exit_with_census(1);
                }
            };
            emit_shader_program_with_view(
                &ws,
                skull::shader_artifact::ShaderProgram { vert: None, frag },
                prof,
            )
            .map(|emission| emission.source)
        } else {
            let root_refs = roots
                .iter()
                .map(|root| skull_codegen_glsl::resolve_root(&ws, user_mid, root))
                .collect::<std::result::Result<Vec<_>, _>>();
            let root_refs = match root_refs {
                Ok(root_refs) => root_refs,
                Err(e) => {
                    eprintln!("{}", render_file_errs(path, &src, &[e]));
                    crate::ablation_report::exit_with_census(1);
                }
            };
            let session = match skull_codegen_glsl::shader_library_session(&ws, root_refs) {
                Ok(session) => session,
                Err(e) => {
                    eprintln!("{}", render_file_errs(path, &src, &[e]));
                    crate::ablation_report::exit_with_census(1);
                }
            };
            skull_codegen_glsl::emit_shader_library_with_view(&ws, &session.scx, &session.view)
        };
        match glsl {
            Ok(g) => g,
            Err(e) => {
                eprintln!("{}", render_file_errs(path, &src, &[e]));
                crate::ablation_report::exit_with_census(1);
            }
        }
    };

    match output {
        Some(out) => {
            // Keep the fn-library header text stable: utils_math.glsl byte-identity.
            let mode = if glsl.starts_with("#version") { "shader entry" } else { "route B" };
            let profile_flag = profile.as_deref().map(|p| format!(" --profile {p}")).unwrap_or_default();
            let header = format!(
                "// GENERATED by skull-codegen-glsl ({mode}) -- DO NOT EDIT.\n\
                 // Regenerate: rock shader {} {}{} --output {}\n\n",
                path,
                roots.join(" "),
                profile_flag,
                out
            );
            // A `#version` output puts the header AFTER the version line: the ES
            // profile requires `#version` to be the very first token (a leading
            // comment is a hard error), and the engine loader keeps the header
            // in-source, so a header-before-#version breaks the mobile profile.
            let file = if let Some(rest) = glsl.strip_prefix("#version") {
                let nl = rest.find('\n').map(|p| p + 1).unwrap_or(rest.len());
                let (version_tail, body) = rest.split_at(nl);
                let body = body.strip_prefix('\n').unwrap_or(body);
                format!("#version{version_tail}{header}{body}")
            } else {
                format!("{header}{glsl}")
            };
            rock_host::project::write_atomic(&out, &file)?;
            log::info!(target: "SHADER", "{} [{}] -> {}", path, roots.join(", "), out);
        }
        None => print!("{glsl}"),
    }
    Ok(())
}

fn invalidate_shader_output(input: &str, output: &str) -> Result<()> {
    let input = std::path::Path::new(input);
    let output = std::path::Path::new(output);
    let aliases = input == output
        || match (fs_err::canonicalize(input), fs_err::canonicalize(output)) {
            (Ok(input), Ok(output)) => input == output,
            _ => false,
        };
    if aliases {
        anyhow::bail!("rock shader: --output must not refer to the input file");
    }
    match fs_err::remove_file(output) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

pub fn skull_compile_single(path: &str, output: Option<String>) -> Result<()> {
    if std::path::Path::new(path).is_dir() {
        return skull_compile_project(path, output);
    }
    let stats = crate::compile_measurements::Scope::begin(path);
    let src = match fs_err::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            log::error!(target: "SKULL", "cannot read {}: {}", path, e);
            crate::ablation_report::exit_with_census(1);
        }
    };
    match skull_emit(path, &src) {
        Ok((lua, _emits_main)) => {
            match output {
                Some(out) => {
                    rock_host::project::write_atomic(&out, &lua)?;
                    log::info!(target: "SKULL", "{} -> {}", path, out);
                }
                None => {
                    // Result output (the compiled Lua), not logging — pipeable.
                    print!("{}", lua);
                }
            }
            crate::compile_measurements::phase("write");
            stats.success();
            Ok(())
        }
        Err(errs) => {
            eprintln!("{}", render_file_errs(path, &src, &errs));
            crate::ablation_report::exit_with_census(1);
        }
    }
}

/// `rock watch --backend skull` — live recompile loop: on each change,
/// re-discover sources, bone-check, and emit the full `.lua` via the new
/// (cell-model) path to `output` (default `tmp/main.lua`). A parse/type error
/// renders and serves the previous good output; the loop keeps running. (The old
/// IncrementalEmitter was retired with the old IR; a batch re-emit is fast —
/// a game project assembles in a few ms.)
pub fn skull_watch(path: &str, output: Option<String>) -> Result<()> {
    let out_file = output
        .map(std::path::PathBuf::from)
        .unwrap_or_else(rock_host::project::default_main_lua_path);
    if let Some(parent) = out_file.parent() {
        if !parent.exists() {
            fs_err::create_dir_all(parent)?;
        }
    }

    // Live recompile backed by bone's incremental `Session` (warm stdlib,
    // per-body re-inference) + skull's per-body emit cache: a body edit re-emits
    // only the re-inferred chunks (+ their inline-dependent callers), reusing the
    // rest. An interface/structural change (signature, add/remove file) shifts
    // `Idx<Function>`, so the cache is rebuilt with a full emit. See
    // `docs/skull-incremental-emit-split.md`.
    let mut session: Option<bone_incremental::session::Session> = None;
    let mut cache: Option<skull::mir_emit::SkullEmitCache> = None;

    let build = |label: &str,
                 session: &mut Option<bone_incremental::session::Session>,
                 cache: &mut Option<skull::mir_emit::SkullEmitCache>| {
        let stats = crate::compile_measurements::Scope::begin(path);
        let source_spec = crate::common::path_to_source_spec(path);
        let sources = match crate::unified_compile::discover_sources(&source_spec) {
            Ok(s) => s,
            Err(e) => {
                log::error!(target: "WATCH", "skull: source discovery failed: {e}");
                return;
            }
        };
        // Game build: drop test-only files (tests/ dirs) so a broken sim/test
        // can't break the watched game output -- parity with the rock backend's
        // is_game gate. See `docs/sim-layering.md`.
        let path_ref = std::path::Path::new(path);
        let sources = if path_ref.is_dir()
            && rock_host::project::is_game_project(path_ref).unwrap_or(false)
        {
            match sources.production_only(path_ref) {
                Ok(sources) => sources,
                Err(error) => {
                    log::error!(target: "WATCH", "skull: production source selection failed: {error}");
                    return;
                }
            }
        } else {
            sources
        };
        crate::compile_measurements::phase("discovery");
        let files = crate::bone_backend::session_files(&sources);

        let t = Instant::now();
        let mut reinferred: Vec<(bone_semantic::prelude::ModuleId, bone_semantic::hir::BodyOwner)> = Vec::new();
        let mut full = false; // interface/structural change -> rebuild cache
        let mut update_failed = false;

        match session {
            None => match crate::bone_backend::bone_session_for_emission(
                files.clone(),
                std::path::Path::new(path),
            ) {
                Ok(s) => {
                    *session = Some(s);
                    full = true; // first build: populate the cache
                }
                Err(e) => {
                    eprintln!("{}", e.render(&sources));
                    log::error!(target: "WATCH", "skull {label}: session init failed");
                    return;
                }
            },
            Some(s) => {
                for f in &files {
                    match s.update_file(&f.path, &f.text) {
                        Ok(result) => {
                            if matches!(result.tier, bone_incremental::session::UpdateTier::Interface | bone_incremental::session::UpdateTier::Scope | bone_incremental::session::UpdateTier::FullUser) {
                                full = true;
                            }
                            reinferred.extend(result.reinferred);
                        }
                        Err(e) => {
                            update_failed = true;
                            eprintln!("{}", e.render(&sources));
                        }
                    }
                }
                let gone: Vec<String> = s
                    .tracked_paths()
                    .into_iter()
                    .filter(|p| !files.iter().any(|f| f.path == *p))
                    .map(|p| p.to_string())
                    .collect();
                for file_path in gone {
                    full = true; // removed file shifts module ids
                    if let Err(e) = s.remove_file(&file_path) {
                        update_failed = true;
                        eprintln!("{}", e.render(&sources));
                    }
                }
            }
        }
        crate::compile_measurements::phase("session: parse+stdlib+semantic");
        let check_ms = t.elapsed().as_secs_f64() * 1000.0;

        let Some(s) = session else { return };
        // A broken edit still COMMITS its partial state to the session (the parse
        // is total now), so the incremental cache -- which reflects the last good
        // emit -- can no longer be diffed against it. Drop the cache on any error
        // so the next clean recompile does a correct full rebuild rather than an
        // incremental emit across the broken intermediate. The reload itself is
        // skipped (last good output keeps running) until the code is clean.
        if update_failed {
            *cache = None;
            log::error!(target: "WATCH", "skull {label}: update failed in {check_ms:.1}ms - serving last good output");
            return;
        }
        let errs = s.diagnostics();
        if !errs.is_empty() {
            *cache = None;
            for e in &errs {
                eprintln!("{}", e.render(&sources));
            }
            log::error!(target: "WATCH", "skull {label}: {} diagnostic(s) in {check_ms:.1}ms - serving last good output", errs.len());
            return;
        }

        // A recompile is a build, so the lint pass runs here too; an `Error`
        // finding refuses the reload the same way a type error does.
        let lint_errs = crate::lint_pass::run_build_lints(
            s.workspace(),
            &sources,
            std::path::Path::new(path),
            crate::lint_pass::Scope::WholeProject,
        );
        if !lint_errs.is_empty() {
            *cache = None;
            for e in &lint_errs {
                eprintln!("{}", render_project_err(&sources, e));
            }
            log::error!(target: "WATCH", "skull {label}: {} lint error(s) - serving last good output", lint_errs.len());
            return;
        }

        let ws = s.workspace();
        let Ok(mid) = skull::user_module_id(ws) else {
            log::error!(target: "WATCH", "skull {label}: no user module");
            return;
        };
        let te = Instant::now();
        let emit = if full || cache.is_none() {
            match skull::mir_emit::emit_full(ws, mid) {
                Ok((e, c)) => {
                    *cache = Some(c);
                    e
                }
                Err(e) => {
                    eprintln!("{}", render_project_err(&sources, &e));
                    log::error!(target: "WATCH", "skull {label}: emit failed - serving last good output");
                    return;
                }
            }
        } else {
            match skull::mir_emit::emit_incremental(ws, cache.as_mut().unwrap(), &reinferred) {
                Ok(e) => e,
                Err(e) => {
                    eprintln!("{}", render_project_err(&sources, &e));
                    log::error!(target: "WATCH", "skull {label}: incremental emit failed - serving last good output");
                    return;
                }
            }
        };
        crate::compile_measurements::emission(&emit);
        let emit_ms = te.elapsed().as_secs_f64() * 1000.0;
        let mode = if full || reinferred.is_empty() { "full" } else { "incremental" };
        match rock_host::project::write_atomic(&out_file, &emit.lua) {
            Ok(()) => {
                crate::compile_measurements::phase("write");
                stats.success();
                log::info!(target: "WATCH",
                "skull {label}: {} files -> {} ({mode}, {} re-inferred; check {check_ms:.1}ms, emit {emit_ms:.1}ms)",
                sources.files.len(), out_file.display(), reinferred.len());
            },
            Err(err) => log::error!(target: "WATCH", "skull {label}: write failed: {err}"),
        }
    };

    build("initial", &mut session, &mut cache);

    crate::run_compile_watch_loop(
        vec![std::path::PathBuf::from(path)],
        &format!("skull: watching {path} for changes"),
        |_event| build("rebuild", &mut session, &mut cache),
    )
}

/// `rock run <file> --backend skull` — emit Lua, append a `main()` call if the
/// file defines one, and execute it.
pub fn skull_run(path: &str) -> Result<()> {
    if std::path::Path::new(path).is_dir() {
        return skull_run_project(path);
    }
    let src = match fs_err::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            log::error!(target: "SKULL", "cannot read {}: {}", path, e);
            crate::ablation_report::exit_with_census(1);
        }
    };
    let (lua, emits_main) = match skull_emit(path, &src) {
        Ok(l) => l,
        Err(errs) => {
            eprintln!("{}", render_file_errs(path, &src, &errs));
            crate::ablation_report::exit_with_census(1);
        }
    };
    let program = if emits_main {
        format!("{lua}\nmain()\n")
    } else {
        lua
    };
    match rock_host::lua_runner::run_lua_code(&program) {
        Ok(run) => {
            print!("{}", run.stdout);
            if !run.stderr.is_empty() {
                eprint!("{}", run.stderr);
            }
            // The program's own exit status is the answer. Printing a crash and
            // returning Ok is how `rock run` used to exit 0 on a dead program.
            if !run.success {
                log::error!(target: "SKULL", "program exited with a runtime error");
                crate::ablation_report::exit_with_census(1);
            }
            Ok(())
        }
        Err(e) => {
            log::error!(target: "SKULL", "runtime error: {}", e.message);
            crate::ablation_report::exit_with_census(1);
        }
    }
}

/// Render a bone `CompilerError` against a whole project's source set, so spans
/// resolve to the right file. Every discovered source is registered under its
/// id-path (the same id used when parsing into a module).
pub(crate) fn render_project_err(sources: &SourceFiles, e: &CompilerError) -> String {
    render_project_errs(sources, std::slice::from_ref(e))
}

/// [`render_project_err`] over every collected diagnostic, joined into one
/// report — the CLI prints ALL of them, matching the LSP.
pub(crate) fn render_project_errs(sources: &SourceFiles, errs: &[CompilerError]) -> String {
    let mut sf = SourceFiles::empty();
    for file in &sources.files {
        let f = file.borrow();
        let id = f.file.to_id_path();
        sf.add_or_update_file(source_file_from_path_and_contents(
            id.clone(),
            id,
            f.contents.clone(),
        ));
    }
    crate::bone_backend::render_errs(&sf, errs)
}

/// `rock run <project-dir> --backend skull` — assemble the whole project (all
/// project + dependency `.rock` files + bone stdlib), type-check it through
/// bone, emit Lua via skull, and execute it.
///
/// A game project also needs the stone engine runtime (libstone FFI + the frame
/// loop): the `is_game` branch below stages libstone and execs luajit on
/// `tmp/main.lua`, where stone's global initializer sets up cimgui, reusing the
/// rock backend's run recipe
/// (`build_engine_libs`, `exec_luajit_file`). All games run this way (match3,
/// mag, bitgun, ...). Non-game projects keep the captured-output pipe. See
/// `docs/skull-plan.md` — whole-game milestone (reached).
/// Discover + assemble + bone-typecheck + skull-emit a whole project directory
/// into one Lua program string. The shared core of `run <dir>` and `compile
/// <dir>` (the single-file forms share [`skull_emit_full`]); only the tail
/// differs — `run` wraps `main()` and execs, `compile` writes the string out.
/// Renders against the project sources and exits 1 on any discovery / type /
/// emit failure. The `bool` is the emitter's own `emits_main`, which decides
/// whether the caller appends the `__rock_main_wrapper(main)` entry.
fn skull_emit_project(dir: &str) -> Result<(GameProgram, bool)> {
    let inputs = match crate::comptime::inputs_for_project_dir(std::path::Path::new(dir)) {
        Ok(inputs) => inputs,
        Err(e) => {
            log::error!(target: "SKULL", "build var resolution failed for project {}: {:#}", dir, e);
            crate::ablation_report::exit_with_census(1);
        }
    };
    skull_emit_project_with_inputs(dir, inputs)
}

fn skull_emit_project_with_inputs(
    dir: &str,
    inputs: bone_semantic::inference::ComptimeInputs,
) -> Result<(GameProgram, bool)> {
    use std::path::PathBuf;
    let stats = crate::compile_measurements::Scope::begin(dir);
    let slot = crate::compile_slots::acquire(dir)?;
    crate::compile_measurements::slot(slot.acquisition());
    let renderer_target = match skull_renderer_target(dir) {
        Ok(target) => target,
        Err(e) => {
            log::error!(target: "SKULL", "invalid renderer selection for {}: {}", dir, e);
            crate::invocation_log::exit(1);
        }
    };
    let spec = crate::unified_compile::SourceSpec::Directory(PathBuf::from(dir));
    let sources = match crate::unified_compile::discover_sources(&spec) {
        Ok(s) => s,
        Err(e) => {
            log::error!(target: "SKULL", "cannot load project {}: {}", dir, e);
            crate::ablation_report::exit_with_census(1);
        }
    };
    // Game build: drop test-only files (tests/ dirs) so a compile error in a
    // sim/test can't break the running game. See
    // `docs/sim-layering.md`.
    let sources = sources
        .production_only(std::path::Path::new(dir))
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;

    crate::compile_measurements::phase("discovery");
    // Compile core: parse(assemble) + bone check + skull codegen. Excludes
    // discovery (disk read) and the caller's output write, matching the bone
    // backend's `compile` figure so loc/sec lines up across backends.
    let t = Instant::now();
    let (mut modules, entry_file) = match skull_assemble_project(&sources) {
        Ok(v) => v,
        Err(errs) => {
            eprintln!("{}", render_project_errs(&sources, &errs));
            crate::ablation_report::exit_with_census(1);
        }
    };

    crate::compile_measurements::phase("parse+stdlib");
    let mut ws = match crate::bone_backend::check_modules(&mut modules) {
        Ok(ws) => ws,
        Err(errs) => {
            eprintln!("{}", render_project_errs(&sources, &errs));
            log::error!(target: "SKULL", "bone type-check failed for project {}", dir);
            crate::ablation_report::exit_with_census(1);
        }
    };
    crate::compile_measurements::phase("semantic");
    crate::compile_measurements::timings("bone", &ws.phase_timings);
    // The workspace owns everything lint, emission and metadata read; the
    // parse trees would otherwise stay live through the emit peak.
    drop(modules);
    ws.comptime_inputs = inputs;
    let user_mid = crate::bone_backend::entry_module_id(&ws, entry_file);

    // Lints are a compiler pass, not an editor accessory: they run on the same
    // typechecked output every build produces, and an `Error` finding refuses
    // the program (`docs/lint-future-plan.md`). The game build dropped `tests/`
    // above, hence ProductionOnly.
    let lint_errs = crate::lint_pass::run_build_lints(
        &ws,
        &sources,
        std::path::Path::new(dir),
        crate::lint_pass::Scope::ProductionOnly,
    );
    if !lint_errs.is_empty() {
        eprintln!("{}", render_project_errs(&sources, &lint_errs));
        log::error!(target: "SKULL", "{} lint error(s) in project {}", lint_errs.len(), dir);
        crate::ablation_report::exit_with_census(1);
    }

    crate::compile_measurements::phase("prepare");
    // Project build: modules outside `dir` (the `stone` dependency, stdlib) are
    // libraries and prune to what the project reaches.
    let emit = match skull::mir_emit::emit_workspace_mir_in_for_renderer(
        &ws,
        user_mid,
        Some(dir),
        renderer_target,
    ) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("{}", render_project_err(&sources, &e));
            log::error!(target: "SKULL", "skull emit failed for project {}", dir);
            crate::ablation_report::exit_with_census(1);
        }
    };
    crate::compile_measurements::emission(&emit);
    let allocation_report_path = allocation_report_path_from_env();
    if let Err(e) =
        write_allocation_report(allocation_report_path.as_deref(), &emit.allocation_report)
    {
        log::error!(target: "SKULL", "cannot write allocation report for {}: {:#}", dir, e);
        crate::ablation_report::exit_with_census(1);
    }
    let emits_main = emit.emits_main;
    let (lua, skipped) = (emit.lua, emit.skipped);
    let compile = t.elapsed();
    crate::bone_backend::log_compile_stats(&sources, &ws, compile, compile, None);
    if std::env::var("SKULL_DEBUG_SKIPPED").is_ok() && !skipped.is_empty() {
        eprintln!("skull: {} fns skipped", skipped.len());
        for (name, reason) in &skipped {
            eprintln!("  {name}: {reason}");
        }
    }

    let lua = match install_rock_state_metadata(lua, &ws, &spec) {
        Ok(lua) => lua,
        Err(error) => {
            eprintln!("{}", render_project_err(&sources, &error));
            log::error!(target: "SKULL", "state metadata planning failed for {}", dir);
            crate::ablation_report::exit_with_census(1);
        }
    };
    // The bone Workspace is dead once `lua` is built -- this is a terminal
    // compile path (`compile`/`run` of a project; never looped in tests or
    // backend-stats), so the only thing after this is the output write / a
    // luajit exec / process exit. Hand the drop to the rayon pool so it overlaps
    // that tail. `sources` (rock SourceFiles, RefCell-backed and ~0.01ms to
    // drop) stays inline. `ws` is owned (bone has no Rc/RefCell), hence Send.
    // A `sequential` build drops inline.
    #[cfg(not(feature = "sequential"))]
    rayon::spawn(move || drop(ws));
    #[cfg(feature = "sequential")]
    drop(ws);
    stats.success();
    Ok((lua, emits_main))
}

fn write_allocation_report(
    path: Option<&std::ffi::OsStr>,
    report: &skull::allocation_report::AllocationReport,
) -> Result<()> {
    let Some(path) = path else { return Ok(()) };
    if path.is_empty() {
        anyhow::bail!("SKULL_ALLOCATION_REPORT must name an output file");
    }
    let path = std::path::Path::new(path);
    rock_host::project::write_atomic(path, &report.render_text())
        .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
}

fn allocation_report_path_from_env() -> Option<std::ffi::OsString> {
    std::env::var_os("SKULL_ALLOCATION_REPORT").or_else(|| {
        let mut path = std::path::PathBuf::from(std::env::var_os(
            "SKULL_ALLOCATION_ACTIVATION",
        )?);
        path.set_extension("manifest.txt");
        Some(path.into_os_string())
    })
}

/// Attach host-only package grouping and reflection descriptions to the
/// compiler-emitted StateSchema. This is metadata, not a second state-membership
/// oracle: consumers enumerate `_G.__ROCK_STATE_SCHEMA` and read the active
/// Session through its session keys.
fn build_rock_state_metadata(
    ws: &bone_semantic::inference::Workspace,
    spec: &crate::unified_compile::SourceSpec,
) -> std::result::Result<String, CompilerError> {
    use bone_semantic::prelude::ModuleId;
    use std::collections::BTreeMap;

    let package_map = crate::unified_compile::build_package_map(spec);
    let mut metadata: BTreeMap<String, String> = BTreeMap::new();
    for m in ws.modules.iter().filter(|m| m.id != ModuleId::builtin()) {
        let package = package_map.as_ref().and_then(|(graph, names)| {
            let owner = graph.package_of_file(std::path::Path::new(m.file.as_str()))?;
            names.get(&owner).cloned()
        });
        for (_gid, g) in m.globals.iter() {
            let identity = skull_mir::state_schema::global_identity(m.file, g.name);
            let description = skull::type_desc_lua(ws, g.ty);
            let fields = [
                package.clone().map(|p| format!("package={p:?}")),
                description.map(|d| format!("description={d}")),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
            if !fields.is_empty() {
                metadata.insert(identity, format!("{{{}}}", fields.join(",")));
            }
        }
    }
    let mut out = String::from("\n_G.__ROCK_STATE_METADATA = {");
    for (identity, fields) in metadata {
        out.push_str(&format!("[{identity:?}]={fields},"));
    }
    out.push_str("}\n");
    out.push_str("for _, entry in ipairs(_G.__ROCK_STATE_SCHEMA or {}) do\n");
    out.push_str("  local metadata = _G.__ROCK_STATE_METADATA[entry.identity]\n");
    out.push_str("  if metadata then\n");
    out.push_str("    entry.package = metadata.package\n");
    out.push_str("    entry.description = metadata.description\n");
    out.push_str("  end\nend\n");
    Ok(out)
}

struct GameProgram {
    lua: String,
    metadata: String,
    metadata_at: usize,
}

impl GameProgram {
    fn with_metadata(lua: String, metadata: String) -> std::result::Result<Self, CompilerError> {
        let marker = "__rock_state_install_global_bridge()\n";
        let at = lua.find(marker).ok_or_else(|| CompilerError::new_no_location(
            "compiler-owned StateSchema is missing its global bridge".to_string(),
        ))?;
        Ok(Self { lua, metadata, metadata_at: at + marker.len() })
    }

    fn parts(&self) -> [&str; 3] {
        let (head, tail) = self.lua.split_at(self.metadata_at);
        [head, &self.metadata, tail]
    }
}

fn install_rock_state_metadata(
    lua: String,
    ws: &bone_semantic::inference::Workspace,
    spec: &crate::unified_compile::SourceSpec,
) -> std::result::Result<GameProgram, CompilerError> {
    let mut metadata = build_rock_state_metadata(ws, spec)?;
    let scx = skull_mir::lower::ScalarCtx::new(ws);
    metadata.push_str(&build_state_nobox_accessors(ws, &scx)?);
    GameProgram::with_metadata(lua, metadata)
}

/// Build `_G.__ROCK_STATE_NOBOX`, the rebuild/take-apart pair per `nobox`
/// global. A `nobox` global's storage is its leaf globals, so reflection cannot
/// reach it as one Session value; these closures are the physical adapter for
/// the logical StateSchema entry.
///
/// Both directions consume one canonical accessor plan
/// (`nobox_global_accessor_shape`), so they cannot drift from each other or from the
/// storage: `get` is the positional leaf ctor over the plan's paths, and
/// `set` reads the same paths in order. Nested cdata uses the flattened `_lct`
/// representation; ordinary cdata and GC-table values retain `_ct` and `_vt`.
///
fn build_state_nobox_accessors(
    ws: &bone_semantic::inference::Workspace,
    scx: &skull_mir::lower::ScalarCtx,
) -> std::result::Result<String, CompilerError> {
    use bone_semantic::prelude::ModuleId;

    let mut rows: Vec<String> = Vec::new();
    let state_slots = skull::mir_emit::lua_global_state_slots(ws, scx)?;
    for m in ws.modules.iter().filter(|m| m.id != ModuleId::builtin()) {
        for (_gid, g) in m.globals.iter().filter(|(_, global)| !global.is_const) {
            let Some(acc) = skull_mir::lower::nobox_global_accessor_shape(ws, scx, g.ty)
                .map_err(|e| CompilerError::new_no_location(e.message))?
            else { continue };
            let skull_mir::lower::NoboxGlobalAccessorShape {
                ty_name, paths, nullable, storage, ctor_suffix: suffix,
                presence_path: _, unbuildable,
            } = acc;
            let ty_symbol = skull_mir::mangle::type_symbol(ws, ty_name);
            let plan = skull_mir::lower::GlobalPlan::for_declared_type(ws, scx, g.ty)
                .map_err(|e| CompilerError::new_no_location(e.message))?;
            // The leaves are emitted symbols, matching the Session slot map.
            let all_leaves = plan
                .leaf_names(skull_mir::mangle::symbol_of_key(g.name))
                .ok_or_else(|| {
                    CompilerError::new_no_location(format!(
                        "nobox global `{}` has an accessor shape but no leaf-name plan",
                        g.name
                    ))
                })?;
            // A nullable global leads with a presence leaf that has no path: it is
            // not a field, so it is branched on rather than read. Everything below
            // pairs the REMAINING leaves with the paths.
            let (present, leaves) = if nullable {
                let Some((p, rest)) = all_leaves.split_first() else {
                    return Err(CompilerError::new_no_location(format!(
                        "nullable nobox global `{}` has no presence leaf",
                        g.name
                    )));
                };
                (Some(*p), rest.to_vec())
            } else {
                (None, all_leaves)
            };
            if leaves.len() != paths.len() {
                return Err(CompilerError::new_no_location(format!(
                    "nobox global `{}` has {} leaves but {} leaf paths",
                    g.name,
                    leaves.len(),
                    paths.len()
                )));
            }
            // Both accessor directions are POSITIONAL over the leaf sequence, so
            // they are sound only where the ctor is too. `nobox_global_accessor_shape`
            // makes that call; guessing is how a leak becomes a miscompile, so an
            // unbuildable shape rejects rather than emit a wrong accessor.
            if let Some(why) = &unbuildable {
                return Err(CompilerError::new_no_location(format!(
                    "nobox global `{}` of type `{ty_name}` cannot be rebuilt: {why}",
                    g.name
                )));
            }
            let all_leaf_names = if let Some(present) = present {
                std::iter::once(present).chain(leaves.iter().copied()).collect::<Vec<_>>()
            } else {
                leaves.clone()
            };
            let state_keys = all_leaf_names
                .iter()
                .map(|leaf| {
                    state_slots.get(leaf).copied().ok_or_else(|| {
                        CompilerError::new_no_location(format!(
                            "nobox global `{}` leaf `{leaf}` has no Lua Session slot",
                            g.name
                        ))
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            let (present_key, value_keys) = if nullable {
                let (present, values) = state_keys
                    .split_first()
                    .expect("a nullable shape has a presence Session slot");
                (Some(*present), values.to_vec())
            } else {
                (None, state_keys)
            };
            let args = value_keys
                .iter()
                .map(|key| format!("__rock_state_get({key})"))
                .collect::<Vec<_>>()
                .join(", ");
            let writes = leaves
                .iter()
                .zip(&paths)
                // Table boxes (leaf-table, and a GC table's `nobox` fields) store
                // dotted paths as literal keys; cdata fields remain ordinary
                // nested field reads.
                .zip(&value_keys)
                .map(|((_, p), key)| {
                    let value = if storage != skull_mir::lower::ValueStorage::CData {
                        format!("v[{:?}]", p.as_str())
                    } else {
                        format!("v.{}", p.as_str())
                    };
                    format!("__rock_state_set({key}, {value})")
                })
                .collect::<Vec<_>>()
                .join("; ");
            let (get, set) = match present {
                // Presence gates BOTH directions: rebuilding an absent value would
                // hand reflection a struct of nils, and a write that left the leaves
                // behind would let a later read resurrect the old value.
                Some(_) => (
                    format!(
                        "if not __rock_state_get({}) then return nil end return {ty_symbol}{suffix}({args})",
                        present_key.as_ref().expect("nullable accessor has a presence key")
                    ),
                    format!(
                        "__rock_state_set({}, v ~= nil); if v ~= nil then {writes} else {} end",
                        present_key.as_ref().expect("nullable accessor has a presence key"),
                        leaves
                            .iter()
                            .zip(&value_keys)
                            .map(|(_, key)| format!("__rock_state_clear({key})"))
                            .collect::<Vec<_>>()
                            .join("; ")
                    ),
                ),
                None => (format!("return {ty_symbol}{suffix}({args})"), writes),
            };
            rows.push(format!(
                "[{:?}]={{get=function() {get} end,set=function(v) {set} end}},",
                skull_mir::state_schema::global_identity(m.file, g.name)
            ));
        }
    }
    // Assigned even when EMPTY. A full reload re-executes the chunk in the live
    // process, so what the new chunk omits it inherits: emitting nothing here
    // would leave the previous build's accessors installed, and changing the last
    // `nobox` global to an ordinary one would keep reflection reading leaf globals
    // that no longer back it.
    Ok(format!("\n_G.__ROCK_STATE_NOBOX = {{{}}}\n", rows.concat()))
}

/// Collect `@sim` info from the bone workspace -- the skull analog of rock's
/// `sim_registry::collect_sims`, over bone HIR. Returns each sim's wire name
/// (the `@sim("name")` arg, else the fn name) + its pre-spawn `@sim_env` pairs,
/// with `@sim_render` sugar (`SIM_SKIP_RENDER=0`) applied first so an explicit
/// `@sim_env` can override. Workspace/source order; the runner applies each
/// pair via `Command::env` (later wins). Same shape that drives the emitted
/// `register_sim` registry, so the wire set the runner sees matches what the
/// game can dispatch.
/// The headless window sims that never read pixels boot in. 320x180 is 16:9
/// like the 1920x1080 game default at a thirty-sixth of the pixels, so
/// aspect-locked logic (camera framing,
/// `gameplay_visible_world_width(1920/1080)`) is unaffected while scene render
/// targets shrink to a few megabytes. Four capture sims already run this size
/// by explicit choice (`frame_stream_capture`, `low_fps_nudge`,
/// `meme_capture_sound`), so it is a blessed sim size, not a new extreme.
pub const SIM_SMALL_WINDOW: &str = "320x180";

/// Declared-name prefixes of the engine's async pixel-readback family and its
/// FFI, plus the imgui test-geometry and synthetic-input families: every
/// `pixel_readback_*` wrapper, `stone_pixel_readback_*` call, `imgui_test_*`
/// UI automation helper (scroll/click/drag by presented position), and
/// `imgui_inject_*` synthetic input call (mouse buttons, text), present and
/// future. The families are large, uniformly prefixed, and call no shared
/// funnel, so a prefix covers their growth; over-matching stays on the safe
/// side (a lifecycle call keeps the window). The imgui families read or drive
/// presented geometry (item rects, visibility, widget input) rather than
/// pixels, but that geometry is window-derived exactly like a readback: the
/// seam proof showed a palette-click sim failing at every size below 1080p
/// while its siblings pass at 320x180, and the full proof showed a
/// synthetic-mouse toggle sim failing 6/20 at 320x180.
pub static SIM_PIXEL_PREFIXES: &[&str] =
    &["pixel_readback", "stone_pixel_readback", "imgui_test_", "imgui_inject_"];

/// True when the declared callee name reads pixels: an exact root, a MAG
/// hover-pick reader, or a readback-family prefix.
fn sim_name_reads_pixels(declared: &str) -> bool {
    SIM_PIXEL_ROOTS.iter().any(|root| *root == declared)
        || SIM_MAG_PICK_ROOTS.iter().any(|root| *root == declared)
        || SIM_PIXEL_PREFIXES.iter().any(|prefix| declared.starts_with(prefix))
}

/// Declared names of the engine's pixel readers: captures, canvas, window and
/// depth readback and frame streams, plus the FFI
/// roots sims may call directly and `sim_submit_guard`, which every guarded
/// reader funnels through. The tail of the list reads presented-output-derived
/// state rather than pixels, which the full proof showed is equally
/// window-derived: the window size (`stone_get_window_size`, asserted 1280x800
/// by an agent sim), the live view scale (`scene_scale` reads 64 at 1080p but
/// 6.3 at 320x180; `resolve_view_for` resolves it), the presented UI geometry
/// census (`imgui_bodies_with_prefix`, `imgui_body_ran`, `imgui_total_vtx_count`;
/// a prop grid draws zero cards at 320x180), the synthetic mouse position
/// (`imgui_set_mouse_pos`, the non-inject-prefixed half of the input pair),
/// and the render-target capacity readers: the native storage queries
/// (`stone_canvas_capacity_*`, `stone_canvas_boot_*`, `stone_canvas_active_*`,
/// `stone_canvas_bound_as_target`), the pyramid step (`pyramid_active_step`
/// derives pyramid geometry from the capacity), and the capacity
/// re-specification (`ensure_scene_capacity`). Existence and the Rock
/// active-rect readers are deliberately not listed: `canvas_is_live` funnels
/// through `stone_canvas_exists`, and `canvas_active` is called by shared
/// helpers, so matching either keeps hundreds of sims that own fixed-size
/// canvases. Each name is unique workspace-wide (D-form methods desugar to
/// bare names, so no method is listed: a sim reading probe bytes created its
/// probe through `window_probe_new` in the same entry). The async readback,
/// imgui test-geometry and imgui synthetic-input families are covered by
/// [`SIM_PIXEL_PREFIXES`]; the scene capacity globals are covered by
/// [`SIM_CAPACITY_GLOBALS`]. A sim whose transitive callees name one of these
/// keeps the game's window; anything else boots in [`SIM_SMALL_WINDOW`]
/// unless it declares its own `STONE_WINDOW`.
pub static SIM_PIXEL_ROOTS: &[&str] = &[
    "take_screenshot",
    "window_probe_new",
    "read_canvas_pixel",
    "canvas_read_pixels",
    "canvas_read_region",
    "canvas_read_rgba8",
    "canvas_read_region_i32",
    "frame_stream_begin",
    "frame_stream_record",
    "sim_submit_guard",
    "stone_read_window_rect",
    "stone_save_screenshot",
    "stone_save_screenshot_sized",
    "stone_read_canvas_pixel",
    "stone_read_canvas_pixel_channel",
    "read_depth_pixel",
    "stone_read_depth_pixel",
    "stone_get_window_size",
    "imgui_set_mouse_pos",
    "scene_scale",
    "resolve_view_for",
    "imgui_total_vtx_count",
    "imgui_body_ran",
    "imgui_bodies_with_prefix",
    "pyramid_active_step",
    "ensure_scene_capacity",
    "stone_canvas_capacity_w",
    "stone_canvas_capacity_h",
    "stone_canvas_boot_w",
    "stone_canvas_boot_h",
    "stone_canvas_active_w",
    "stone_canvas_active_h",
    "stone_canvas_active_is_creation",
    "stone_canvas_bound_as_target",
];

/// MAG's hover-pick readers (mag/click.rock): picking reads the ID buffer (a
/// readback the engine performs per frame), and sims reach the answer through
/// these globals readers (`hover_name_target` funnels through
/// `current_click_target`; the engine hover updaters funnel through
/// `read_canvas_pixel`). Listed separately from [`SIM_PIXEL_ROOTS`] because
/// they resolve in MAG's workspace, not in stone's; the matcher consults both.
pub static SIM_MAG_PICK_ROOTS: &[&str] = &[
    "pick_at_cursor",
    "pick_from_id",
    "current_click_target",
    "pick_selection_count",
    "pick_selection_id",
];

/// Declared names of the scene capacity globals: the bloom pyramid levels and
/// the capacity the scene target set was built for
/// (`stone/engine/rendering/draw.rock`). Both follow the boot window: testbed
/// boots 6 bloom levels at 1280x720 but 4 at 320x180, so a sim asserting a
/// level count or a capacity panics or fails small. A sim whose transitive
/// bodies reference one of these keeps the game's window, like a pixel reader.
/// Matched by declared (bare) name. The drawable size is matched by call
/// (`drawable_size`, `stone_get_drawable_size` in [`SIM_PIXEL_ROOTS`]), not by
/// its `DRAWABLE_SIZE` global, which shared helpers read: matching the global
/// keeps hundreds of sims that never query the size. Liveness and ownership
/// queries (`canvas_is_live`, `new_canvas`, `destroy_canvas`) do not match and
/// stay small.
pub static SIM_CAPACITY_GLOBALS: &[&str] = &["BLOOM_LEVELS", "SCENE_CAPACITY"];

/// Callee edges of one body: resolved calls and protocol dispatch from its
/// inference, plus function-valued references from a body scan (a named fn
/// passed as a value lowers to `Expr::Global`, never a call). Mirrors
/// `bone_lints`' edge walk; bodies without inference (extern decls, opaque
/// bodies) are leaves, matched by name at their callers instead.
fn sim_callee_edges(
    ws: &bone_semantic::inference::Workspace,
    mid: bone_core::prelude::ModuleId,
    fid: bone_core::prelude::Idx<bone_core::hir::Function>,
    name_map: &std::collections::HashMap<
        ustr::Ustr,
        Vec<(bone_core::prelude::ModuleId, bone_core::prelude::Idx<bone_core::hir::Function>)>,
    >,
) -> Vec<(bone_core::prelude::ModuleId, bone_core::prelude::Idx<bone_core::hir::Function>)> {
    use bone_core::hir::{BodyOwner, Callee, Expr};
    let owner = BodyOwner::Function(fid);
    let mut out = Vec::new();
    if let Some(bi) = ws.inference(mid).and_then(|inf| inf.per_fn.get(&owner)) {
        out.extend(bi.resolved_overload_calls.values().copied());
        out.extend(bi.implicit_iter_args.values().map(|w| w.func));
        for info in bi.for_iter_protocol.values() {
            if let Some(w) = &info.iter {
                out.push(w.func);
            }
            sim_collect_bound_next(&info.next, &mut out);
        }
        for tree in bi.bound_call_trees.values() {
            for bn in tree.next.values() {
                sim_collect_bound_next(bn, &mut out);
            }
        }
    }
    if let Some(body) = ws.module(mid).bodies.get(&owner) {
        for (_, expr) in body.expr_arena.iter() {
            let name = match expr {
                Expr::Global(n) | Expr::SiblingRef(n) => Some(n),
                Expr::Call { callee: Callee::Unresolved { name, .. }, .. } => Some(name),
                _ => None,
            };
            if let Some(fns) = name.map(|n| name_map.get(n)).flatten() {
                out.extend(fns.iter().copied());
            }
        }
    }
    out
}

fn sim_collect_bound_next(
    bn: &bone_core::workspace::BoundNext,
    out: &mut Vec<(bone_core::prelude::ModuleId, bone_core::prelude::Idx<bone_core::hir::Function>)>,
) {
    if let bone_core::workspace::BoundNext::Concrete { func, sub, .. } = bn {
        out.push(*func);
        for bn in sub.next.values() {
            sim_collect_bound_next(bn, out);
        }
    }
}

/// True when the sim at `(mid, fid)` can read pixels: one of
/// [`SIM_PIXEL_ROOTS`] is among its transitive callees (memoized across sims,
/// so one compile walks the shared call graph once). A new sim that captures
/// is classified by its own calls, never by a name list.
fn sim_reads_pixels(
    ws: &bone_semantic::inference::Workspace,
    mid: bone_core::prelude::ModuleId,
    fid: bone_core::prelude::Idx<bone_core::hir::Function>,
    memo: &mut std::collections::HashMap<
        (bone_core::prelude::ModuleId, bone_core::prelude::Idx<bone_core::hir::Function>),
        bool,
    >,
    visiting: &mut std::collections::HashSet<
        (bone_core::prelude::ModuleId, bone_core::prelude::Idx<bone_core::hir::Function>),
    >,
    name_map: &std::collections::HashMap<
        ustr::Ustr,
        Vec<(bone_core::prelude::ModuleId, bone_core::prelude::Idx<bone_core::hir::Function>)>,
    >,
) -> bool {
    if let Some(&known) = memo.get(&(mid, fid)) {
        return known;
    }
    if !visiting.insert((mid, fid)) {
        return false;
    }
    let callee = &ws.module(mid).functions[fid];
    let mut reads = sim_name_reads_pixels(skull_mir::mangle::declared_name(callee.name.as_str()));
    if !reads {
        for (callee_mid, callee_fid) in sim_callee_edges(ws, mid, fid, name_map) {
            if sim_reads_pixels(ws, callee_mid, callee_fid, memo, visiting, name_map) {
                reads = true;
                break;
            }
        }
    }
    visiting.remove(&(mid, fid));
    memo.insert((mid, fid), reads);
    reads
}

/// True when the sim at `(mid, fid)` references a scene capacity global: one
/// of [`SIM_CAPACITY_GLOBALS`] is named by its transitive bodies (memoized
/// like [`sim_reads_pixels`]). Data globals lower to `Expr::Global` exactly
/// like function-valued references; a name that resolves to no function is a
/// data reference and is matched by declared (bare) name.
fn sim_reads_capacity(
    ws: &bone_semantic::inference::Workspace,
    mid: bone_core::prelude::ModuleId,
    fid: bone_core::prelude::Idx<bone_core::hir::Function>,
    memo: &mut std::collections::HashMap<
        (bone_core::prelude::ModuleId, bone_core::prelude::Idx<bone_core::hir::Function>),
        bool,
    >,
    visiting: &mut std::collections::HashSet<
        (bone_core::prelude::ModuleId, bone_core::prelude::Idx<bone_core::hir::Function>),
    >,
    name_map: &std::collections::HashMap<
        ustr::Ustr,
        Vec<(bone_core::prelude::ModuleId, bone_core::prelude::Idx<bone_core::hir::Function>)>,
    >,
) -> bool {
    use bone_core::hir::{BodyOwner, Expr};
    if let Some(&known) = memo.get(&(mid, fid)) {
        return known;
    }
    if !visiting.insert((mid, fid)) {
        return false;
    }
    let mut reads = false;
    if let Some(body) = ws.module(mid).bodies.get(&BodyOwner::Function(fid)) {
        for (_, expr) in body.expr_arena.iter() {
            let name = match expr {
                Expr::Global(n) | Expr::SiblingRef(n) => Some(n),
                _ => None,
            };
            let Some(name) = name else { continue };
            if name_map.contains_key(name) {
                continue;
            }
            let declared = name.as_str().rsplit("::").next().unwrap_or(name.as_str());
            if SIM_CAPACITY_GLOBALS.iter().any(|root| *root == declared) {
                reads = true;
                break;
            }
        }
    }
    if !reads {
        for (callee_mid, callee_fid) in sim_callee_edges(ws, mid, fid, name_map) {
            if sim_reads_capacity(ws, callee_mid, callee_fid, memo, visiting, name_map) {
                reads = true;
                break;
            }
        }
    }
    visiting.remove(&(mid, fid));
    memo.insert((mid, fid), reads);
    reads
}

fn collect_skull_sims(
    ws: &bone_semantic::inference::Workspace,
) -> Vec<(String, String, Vec<(String, String)>)> {
    use bone_core::prelude::ModuleId;
    use bone_semantic::hir::AnnoValue;

    // The `@sim` walk + wire derivation is shared with skull's emitted registry
    // (`skull::mir_emit::collect_sim_fns`) so the runner's wire names match what
    // the game registered: same module filter, same `sim_wire_name`. It is
    // spelled out here rather than reused so the walk also keeps each sim's
    // `(ModuleId, Idx<Function>)`, which the small-window rule needs to read
    // the sim's resolved calls. Here we additionally read each sim's `@sim_env`
    // pairs (a runner-only concern): `@sim_render` sugar first so an explicit
    // `@sim_env` can override.
    let mut name_map: std::collections::HashMap<
        ustr::Ustr,
        Vec<(ModuleId, bone_core::prelude::Idx<bone_core::hir::Function>)>,
    > = std::collections::HashMap::new();
    for module in ws.modules.iter() {
        if module.id == ModuleId::builtin() {
            continue;
        }
        for (fid, f) in module.functions.iter() {
            name_map.entry(f.name).or_default().push((module.id, fid));
        }
    }
    let mut pixel_memo = std::collections::HashMap::new();
    let mut capacity_memo = std::collections::HashMap::new();
    let mut visiting = std::collections::HashSet::new();
    let mut sims = Vec::new();
    for module in ws.modules.iter() {
        if module.id == ModuleId::builtin() {
            continue;
        }
        for (fid, f) in module.functions.iter() {
            let Some(wire) = skull_mir::lifecycle::sim_wire_name(f) else {
                continue;
            };
            let mut env_pairs = Vec::new();
            if f.annotations.iter().any(|a| a.is_builtin("sim_render")) {
                env_pairs.push(("SIM_SKIP_RENDER".to_string(), "0".to_string()));
            }
            for ann in &f.annotations {
                if !ann.is_builtin("sim_env") {
                    continue;
                }
                let (Some(AnnoValue::String(k)), Some(AnnoValue::String(v))) = (
                    ann.args.first().map(|a| &a.value),
                    ann.args.get(1).map(|a| &a.value),
                ) else {
                    continue;
                };
                env_pairs.push((k.to_string(), v.to_string()));
            }
            // Skipped when the sim declares any `STONE_WINDOW` (an explicit
            // empty keeps the game's own size); prepended so the pair order
            // reads default-first. `STONE_WINDOW` already travels per entry,
            // so a shared worker resizes for each entry like an explicit size
            // today.
            let explicit_window = env_pairs.iter().any(|(k, _)| k == "STONE_WINDOW");
            if !explicit_window
                && !sim_reads_pixels(ws, module.id, fid, &mut pixel_memo, &mut visiting, &name_map)
                && !sim_reads_capacity(ws, module.id, fid, &mut capacity_memo, &mut visiting, &name_map)
            {
                env_pairs.insert(
                    0,
                    ("STONE_WINDOW".to_string(), SIM_SMALL_WINDOW.to_string()),
                );
            }
            sims.push((wire.as_str().to_string(), module.file.as_str().to_string(), env_pairs));
        }
    }
    sims
}

fn relative_sim_source_path(
    workspace_root: &std::path::Path,
    project_dir: &std::path::Path,
    source_file: &str,
) -> Result<String> {
    use std::path::Component;

    let source_path = std::path::Path::new(source_file);
    let source_path = if source_path.is_absolute() {
        source_path.to_path_buf()
    } else {
        project_dir.join(source_path)
    };
    let source_path = source_path
        .canonicalize()
        .with_context(|| format!("resolve @sim source file `{source_file}`"))?;
    let root_components = workspace_root.components().collect::<Vec<_>>();
    let source_components = source_path.components().collect::<Vec<_>>();
    let common = root_components
        .iter()
        .zip(&source_components)
        .take_while(|(root, source)| root == source)
        .count();
    if common == 0 || root_components.first() != source_components.first() {
        anyhow::bail!(
            "@sim source file `{source_file}` is on a different filesystem root from {}",
            workspace_root.display()
        );
    }

    let mut relative = std::path::PathBuf::new();
    for component in root_components.iter().skip(common) {
        if matches!(component, Component::Normal(_)) {
            relative.push("..");
        }
    }
    for component in source_components.iter().skip(common) {
        relative.push(component.as_os_str());
    }
    let normalized = relative.to_string_lossy().replace('\\', "/");
    if normalized.is_empty() {
        anyhow::bail!("@sim source file `{source_file}` resolves to the workspace root");
    }
    Ok(normalized)
}

pub(crate) fn source_root_for_sim_project(project_dir: &std::path::Path) -> Result<std::path::PathBuf> {
    let project_dir = project_dir
        .canonicalize()
        .with_context(|| format!("canonicalize sim project {}", project_dir.display()))?;
    let mut current = project_dir.as_path();
    loop {
        // A stray empty .git in an ancestor is not a source repository.
        let marker = current.join(".git");
        let git_dir = if marker.is_dir() {
            Some(marker)
        } else {
            std::fs::read_to_string(&marker).ok().and_then(|text| {
                text.trim().strip_prefix("gitdir: ").map(|dir| current.join(dir))
            })
        };
        if git_dir.is_some_and(|dir| dir.join("HEAD").is_file()) {
            return Ok(current.to_path_buf());
        }
        let Some(parent) = current.parent() else { break };
        current = parent;
    }
    if let Some(root) = rock_host::project::find_source_root(&project_dir) {
        if let Ok(root) = root.canonicalize() {
            if project_dir.starts_with(&root) {
                return Ok(root);
            }
        }
    }
    Ok(project_dir)
}

/// Build the skull game program for `dir`, write the game-shaped `main.lua`
/// (preamble + entry + the `@sim` registry) to `main_path`, and collect the
/// project's `@sim` wire names, source paths and env pairs from the SAME
/// workspace build. The skull counterpart of rock's `compile_sims` -- one
/// type-check feeds both the emitted registry and the runner's wire/env list,
/// so they cannot drift.
pub fn skull_compile_game_and_collect_sims(
    dir: &str,
    main_path: &std::path::Path,
) -> Result<(Vec<String>, HashMap<String, Vec<(String, String)>>, HashMap<String, String>, SkullStone)> {
    skull_compile_game_and_collect_sims_with_allocation(dir, main_path, None, &[])
}

pub fn skull_compile_game_and_collect_sims_with_allocation(
    dir: &str,
    main_path: &std::path::Path,
    allocation_manifest: Option<&std::path::Path>,
    selection: &[String],
) -> Result<(
    Vec<String>,
    HashMap<String, Vec<(String, String)>>,
    HashMap<String, String>,
    SkullStone,
)> {
    // Session-based build (same machinery as `rock stone`, tests included so the
    // `@sim` bodies are in the program). The live SkullStone is returned so the
    // runner can serve mid-sim `reload_edit` ops through the REAL incremental
    // path (`docs/sim-reload-harness-plan.md`).
    let incremental_path = main_path.with_file_name("incremental.lua");
    let _activation = allocation_manifest.map(|_| skull::allocation_report::activation_scope(true));
    let stone = SkullStone::init_impl(
        dir, main_path, &incremental_path, true, allocation_manifest, selection,
    )
    .map_err(|e| anyhow::anyhow!("cannot compile project {dir}: {e}"))?;

    let project_dir = std::path::Path::new(dir)
        .canonicalize()
        .with_context(|| format!("canonicalize sim project {dir}"))?;
    let workspace_root = source_root_for_sim_project(&project_dir)?;
    let mut sims = collect_skull_sims(stone.project.workspace());
    if let Some(selected) = &stone.selected_sim_names {
        sims.retain(|(wire, _, _)| selected.contains(wire));
    }
    let mut wire_names = Vec::with_capacity(sims.len());
    let mut env_by_wire = HashMap::with_capacity(sims.len());
    let mut source_files = HashMap::with_capacity(sims.len());
    let mut relative_by_source = HashMap::new();
    for (wire, source_file, env) in sims {
        wire_names.push(wire.clone());
        let relative_source = match relative_by_source.entry(source_file.clone()) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => entry.insert(
                relative_sim_source_path(&workspace_root, &project_dir, &source_file)
                    .with_context(|| format!("record source for @sim `{wire}`"))?,
            ),
        };
        source_files.insert(wire.clone(), relative_source.clone());
        env_by_wire.insert(wire, env);
    }
    Ok((wire_names, env_by_wire, source_files, stone))
}

#[cfg(test)]
mod sim_source_path_tests {
    use super::{relative_sim_source_path, source_root_for_sim_project};

    #[test]
    fn sim_source_paths_are_workspace_relative_across_projects_and_dependencies() {
        let temp = tempfile::tempdir().expect("tempdir");
        let workspace = temp.path().join("workspace");
        let project = workspace.join("mag");
        let source = project.join("tests/sims/example.rock");
        std::fs::create_dir_all(source.parent().unwrap()).expect("create project source dir");
        std::fs::write(&source, "@sim\nco fn example() {}\n").expect("write project source");

        let workspace = workspace.canonicalize().expect("canonical workspace");
        let project = project.canonicalize().expect("canonical project");
        assert_eq!(
            relative_sim_source_path(&workspace, &project, source.to_str().unwrap()).unwrap(),
            "mag/tests/sims/example.rock"
        );

        let dependency = temp.path().join("dependency/tests/sims/example.rock");
        std::fs::create_dir_all(dependency.parent().unwrap()).expect("create dependency source dir");
        std::fs::write(&dependency, "@sim\nco fn example() {}\n").expect("write dependency source");
        assert_eq!(
            relative_sim_source_path(&workspace, &project, dependency.to_str().unwrap()).unwrap(),
            "../dependency/tests/sims/example.rock"
        );
    }

    #[test]
    fn source_root_comes_from_project_when_sdk_root_is_separate() {
        let temp = tempfile::tempdir().expect("tempdir");
        let source_root = temp.path().join("source-worktree");
        let project = source_root.join("mag");
        let sdk_root = temp.path().join("sdk");
        std::fs::create_dir_all(source_root.join(".git")).expect("create source repository marker");
        std::fs::write(source_root.join(".git/HEAD"), "ref: refs/heads/main\n")
            .expect("write repository HEAD");
        std::fs::create_dir_all(&project).expect("create project");
        std::fs::create_dir_all(&sdk_root).expect("create separate sdk root");

        let found = source_root_for_sim_project(&project).unwrap();
        assert_eq!(found, source_root.canonicalize().unwrap());
        assert_ne!(found, sdk_root.canonicalize().unwrap());
    }

    #[test]
    fn standalone_sim_project_uses_its_own_source_root() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(temp.path().join(".git")).expect("create empty non-repository marker");
        let project = temp.path().join("standalone-project");
        let source = project.join("tests/sims/probe.rock");
        std::fs::create_dir_all(source.parent().unwrap()).expect("create source directory");
        std::fs::write(&source, "@sim\nco fn probe() {}\n").expect("write source");

        let project_root = project.canonicalize().expect("canonical project");
        assert_eq!(source_root_for_sim_project(&project).unwrap(), project_root);
        assert_eq!(
            relative_sim_source_path(&project_root, &project_root, source.to_str().unwrap()).unwrap(),
            "tests/sims/probe.rock"
        );
    }

    #[test]
    fn standalone_sim_project_ignores_invalid_git_files() {
        let temp = tempfile::tempdir().expect("tempdir");
        let project = temp.path().join("standalone-project");
        std::fs::create_dir(&project).expect("create project");
        let project_root = project.canonicalize().unwrap();
        for marker in ["not a gitdir file\n", "gitdir: missing-metadata\n"] {
            std::fs::write(temp.path().join(".git"), marker).expect("write invalid git marker");
            assert_eq!(source_root_for_sim_project(&project).unwrap(), project_root);
        }
    }

    #[test]
    fn source_root_recognizes_worktree_git_files() {
        let temp = tempfile::tempdir().expect("tempdir");
        let source_root = temp.path().join("worktree");
        let project = source_root.join("game");
        let metadata = temp.path().join("git-metadata");
        std::fs::create_dir_all(&project).expect("create project");
        std::fs::create_dir(&metadata).expect("create git metadata");
        std::fs::write(metadata.join("HEAD"), "ref: refs/heads/main\n").expect("write HEAD");
        for gitdir in ["../git-metadata".to_string(), metadata.to_string_lossy().into_owned()] {
            std::fs::write(source_root.join(".git"), format!("gitdir: {gitdir}\n"))
                .expect("write worktree git file");
            assert_eq!(source_root_for_sim_project(&project).unwrap(), source_root.canonicalize().unwrap());
        }
    }
}

#[cfg(test)]
mod sim_small_window_tests {
    use super::skull_compile_game_and_collect_sims;
    use std::collections::HashMap;
    use std::sync::OnceLock;

    fn fixture_env_by_wire() -> HashMap<String, Vec<(String, String)>> {
        let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("test-data/sim-small-window");
        let out = tempfile::tempdir().expect("small-window output dir");
        let main = out.path().join("main.lua");
        let (_wires, env_by_wire, _sources, _stone) =
            skull_compile_game_and_collect_sims(fixture.to_str().unwrap(), &main)
                .expect("the small-window fixture must compile");
        env_by_wire
    }

    fn env_by_wire() -> &'static HashMap<String, Vec<(String, String)>> {
        static CACHE: OnceLock<HashMap<String, Vec<(String, String)>>> = OnceLock::new();
        CACHE.get_or_init(fixture_env_by_wire)
    }

    /// The env of the sim declared as `short`: the collect key is the wire
    /// name, which under modules v1 is the sim's DefKey.
    fn env_for<'a>(
        env_by_wire: &'a HashMap<String, Vec<(String, String)>>,
        short: &str,
    ) -> &'a Vec<(String, String)> {
        let mut found = None;
        for (wire, env) in env_by_wire {
            let declared = wire.rsplit("::").next().unwrap_or(wire.as_str());
            if declared == short {
                assert!(found.is_none(), "two sims share the short name `{short}`");
                found = Some(env);
            }
        }
        found.unwrap_or_else(|| {
            panic!(
                "no sim named `{short}`; wires: {:?}",
                env_by_wire.keys().collect::<Vec<_>>()
            )
        })
    }

    /// Effective STONE_WINDOW: last declaration wins, like the runner.
    fn stone_window(env: &[(String, String)]) -> Option<&str> {
        env.iter().rev().find(|(k, _)| k == "STONE_WINDOW").map(|(_, v)| v.as_str())
    }

    #[test]
    fn sims_that_read_no_pixels_get_the_small_window() {
        for short in [
            "small_window_plain_sim",
            "small_window_canvas_owner",
            "small_window_native_reader",
            "small_window_active_reader",
            "small_window_drawable_reader",
            "small_window_drawable_ffi_reader",
        ] {
            assert_eq!(
                stone_window(env_for(env_by_wire(), short)),
                Some("320x180"),
                "`{short}` calls no window-derived helper, so it boots in the small window"
            );
        }
    }

    #[test]
    fn sims_that_capture_keep_the_game_default() {
        for short in [
            "small_window_pixel_reader",
            "small_window_indirect_reader",
            "small_window_readback_reader",
            "small_window_ui_reader",
            "small_window_pick_reader",
            "small_window_window_reader",
            "small_window_mouse_reader",
            "small_window_inject_reader",
            "small_window_scale_reader",
            "small_window_view_reader",
            "small_window_geometry_reader",
            "small_window_capacity_reader",
            "small_window_pyramid_reader",
            "small_window_ensure_reader",
            "small_window_bloom_reader",
            "small_window_scene_capacity_reader",
        ] {
            assert_eq!(
                stone_window(env_for(env_by_wire(), short)),
                None,
                "`{short}` reads window-derived state, so the harness must not override its window"
            );
        }
    }

    #[test]
    fn an_explicit_window_size_wins_over_the_small_default() {
        assert_eq!(
            stone_window(env_for(env_by_wire(), "small_window_explicit_size")),
            Some("800x600"),
            "an explicit @sim_env STONE_WINDOW wins"
        );
    }

    #[test]
    fn every_pixel_root_name_resolves_in_a_stone_workspace() {
        let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("test-data/sim-small-window");
        let project = crate::live_project::LiveProject::init(fixture.to_str().unwrap(), true)
            .expect("the small-window fixture must compile");
        let mut names = std::collections::HashSet::new();
        for module in project.workspace().modules.iter() {
            for (_fid, f) in module.functions.iter() {
                let declared = f.name.as_str().rsplit("::").next().unwrap_or(f.name.as_str());
                names.insert(declared.to_string());
            }
        }
        for root in super::SIM_PIXEL_ROOTS {
            assert!(
                names.contains(*root),
                "pixel root `{root}` resolves to no function; the set drifted from the engine"
            );
        }
        for prefix in super::SIM_PIXEL_PREFIXES {
            assert!(
                names.iter().any(|n| n.starts_with(prefix)),
                "pixel prefix `{prefix}` matches no function; the set drifted from the engine"
            );
        }
    }

    #[test]
    fn every_capacity_global_name_resolves_in_a_stone_workspace() {
        let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("test-data/sim-small-window");
        let project = crate::live_project::LiveProject::init(fixture.to_str().unwrap(), true)
            .expect("the small-window fixture must compile");
        let mut names = std::collections::HashSet::new();
        for module in project.workspace().modules.iter() {
            for (_gid, g) in module.globals.iter() {
                let declared = g.name.as_str().rsplit("::").next().unwrap_or(g.name.as_str());
                names.insert(declared.to_string());
            }
        }
        for root in super::SIM_CAPACITY_GLOBALS {
            assert!(
                names.contains(*root),
                "capacity global `{root}` resolves to no global; the set drifted from the engine"
            );
        }
    }

    #[test]
    fn mag_pick_root_names_resolve_in_mag() {
        let mag = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../mag");
        let project = crate::live_project::LiveProject::init(mag.to_str().unwrap(), true)
            .expect("mag must compile");
        let mut names = std::collections::HashSet::new();
        for module in project.workspace().modules.iter() {
            for (_fid, f) in module.functions.iter() {
                let declared = f.name.as_str().rsplit("::").next().unwrap_or(f.name.as_str());
                names.insert(declared.to_string());
            }
        }
        for root in super::SIM_MAG_PICK_ROOTS {
            assert!(
                names.contains(*root),
                "pick root `{root}` resolves to no MAG function; the set drifted from the game"
            );
        }
    }
}

/// `rock compile <project-dir> --backend skull` — assemble + type-check + emit
/// the whole project to Lua and write it to `output` (or stdout). The directory
/// analogue of the single-file [`skull_compile_single`] path; shares
/// [`skull_emit_project`] with `run`.
fn skull_compile_project(dir: &str, output: Option<String>) -> Result<()> {
    let inputs = crate::comptime::inputs_for_project_dir(std::path::Path::new(dir))?;
    skull_compile_project_with_inputs(dir, output, inputs)
}

pub fn skull_compile_project_with_inputs(
    dir: &str,
    output: Option<String>,
    inputs: bone_semantic::inference::ComptimeInputs,
) -> Result<()> {
    use std::path::PathBuf;
    let stats = crate::compile_measurements::Scope::begin(dir);
    let (emit, emits_main) = skull_emit_project_with_inputs(dir, inputs)?;
    // A game's runnable main.lua is the same shape run/stone produce via
    // write_full_main: preamble (game source dir) + emit +
    // __rock_main_wrapper(main). Without the preamble game asset paths are
    // wrong; without the wrapper it never starts.
    let preamble = if rock_host::project::is_game_project(&PathBuf::from(dir)).unwrap_or(false) {
        skull_game_preamble(dir)?
    } else {
        String::new()
    };
    let parts = skull_game_main(&preamble, &emit, emits_main);
    match output {
        Some(out) => {
            rock_host::project::write_atomic_streamed(&out, |file| write_program_parts(file, &parts))?;
            log::info!(target: "SKULL", "{} -> {}", dir, out);
        }
        None => {
            write_program_parts(&mut std::io::stdout().lock(), &parts)?;
        }
    }
    crate::compile_measurements::phase("write");
    stats.success();
    Ok(())
}

fn skull_run_project(dir: &str) -> Result<()> {
    use std::path::PathBuf;
    let stats = crate::compile_measurements::Scope::begin(dir);
    ensure_renderer_feature(dir)?;
    let is_game = rock_host::project::is_game_project(&PathBuf::from(dir)).unwrap_or(false);
    if is_game {
        crate::package::prepare_sound_bank(dir)?;
    }
    let (lua, emits_main) = skull_emit_project(dir)?;

    // Game projects need the rock backend's run recipe, not a bare stdin
    // pipe: stage libstone into tmp/ (build_engine_libs), run luajit on a
    // FILE with LD_LIBRARY_PATH set (build_luajit_command) so
    // `ffi.load("libstone.so")` resolves, and inherit stdio so the
    // window/input work. Non-game projects keep the captured-output pipe.
    if is_game {
        if !std::env::var("ROCK_SKIP_ENGINE_BUILD").is_ok_and(|v| v == "1") {
            if let Err(e) = rock_host::engine_build::build_engine_libs(dir) {
                log::error!(target: "SKULL", "engine build failed: {}", e);
                crate::ablation_report::exit_with_census(1);
            }
        }
        // Game preamble + `__rock_main_wrapper(main)` entry, built by the same
        // helpers the stone two-file watch path uses (so both write an
        // identical tmp/main.lua), and wipe any stale overlay so the engine's
        // hot-reload watcher can't load a leftover incremental.lua over the
        // fresh baseline.
        let lua_file = rock_host::project::default_main_lua_path();
        if let Err(e) = write_full_main(
            &skull_game_preamble(dir)?,
            &lua,
            emits_main,
            &lua_file,
            &lua_file.with_file_name("incremental.lua"),
        ) {
            log::error!(target: "SKULL", "cannot write {}: {}", lua_file.display(), e);
            crate::ablation_report::exit_with_census(1);
        }
        crate::compile_measurements::phase("write");
        stats.success();
        // No timeout: games run an infinite frame loop.
        return crate::compile_cmd::exec_luajit_file(&lua_file, None, &[]);
    }

    // The captured stdin evaluator requires contiguous input.
    let program = skull_game_main("", &lua, emits_main).concat();
    stats.success();
    match rock_host::lua_runner::run_lua_code(&program) {
        Ok(run) => {
            print!("{}", run.stdout);
            if !run.stderr.is_empty() {
                eprint!("{}", run.stderr);
            }
            // The program's own exit status is the answer. Printing a crash and
            // returning Ok is how `rock run` used to exit 0 on a dead program.
            if !run.success {
                log::error!(target: "SKULL", "program exited with a runtime error");
                crate::ablation_report::exit_with_census(1);
            }
            Ok(())
        }
        Err(e) => {
            log::error!(target: "SKULL", "runtime error: {}", e.message);
            crate::ablation_report::exit_with_census(1);
        }
    }
}

/// One JSON line per finished `@test`: `{unit, name, ms, status}`.
fn write_test_timings(path: &std::path::Path, timings: &[(String, TestTiming)]) -> Result<()> {
    let mut text = String::new();
    for (unit, t) in timings {
        let row = serde_json::json!({ "unit": unit, "name": t.name, "ms": t.ms, "status": t.status });
        text.push_str(&row.to_string());
        text.push('\n');
    }
    fs_err::write(path, text)?;
    Ok(())
}

fn print_slowest_tests(timings: &[(String, TestTiming)]) {
    if timings.is_empty() {
        return;
    }
    let mut slowest: Vec<&TestTiming> = timings.iter().map(|(_, t)| t).collect();
    slowest.sort_by(|a, b| b.ms.total_cmp(&a.ms));
    println!("\n{}", "=== slowest @tests (skull) ===".bold());
    for t in slowest.iter().take(10) {
        println!("  {:>10.1} ms  {}", t.ms, t.name);
    }
}

/// `rock test <paths> --backend skull` — for every `.rock` file, run the skull
/// pipeline and the luajit validity gate; report pass/fail + a bucketed
/// failure-reason histogram (the skull analogue of the bone test histogram).
/// Split `paths` into test UNITS: isolated files (root `tests/`: independent
/// programs that would collide in one workspace) and whole `stick.toml`
/// packages built as ONE workspace each. A cross-file lib can only be tested
/// the second way -- per-file, any file referencing a sibling's symbol fails to
/// compile. Shared by every test executor (luajit + JS).
pub(crate) fn collect_test_units(paths: &[String]) -> Result<(Vec<String>, Vec<String>)> {
    let mut files = Vec::new();
    let mut package_dirs = Vec::new();
    for path in paths {
        if is_test_package_dir(path) {
            package_dirs.push(path.clone());
        } else if let Some(pkg) = is_package_tests_dir(path) {
            package_dirs.push(pkg);
        } else {
            walk_test_units(path, &mut files, &mut package_dirs)?;
        }
    }
    files.sort();
    files.dedup();
    package_dirs.sort();
    // `mag`, `mag/`, `./mag` and `mag/tests` all name one package: dedup by
    // its canonical root, keeping the first spelling as the unit label.
    let mut seen = HashSet::new();
    let mut unique = Vec::with_capacity(package_dirs.len());
    for dir in package_dirs {
        if seen.insert(fs_err::canonicalize(&dir)?) {
            unique.push(dir);
        }
    }
    Ok((files, unique))
}

pub(crate) fn skull_test(paths: &[String], filter: Option<&crate::test::TestFilter>) -> Result<()> {
    crate::lint_pass::set_corpus_mode();
    let verbose = std::env::var("ROCK_TEST_VERBOSE").is_ok();
    let timings_path = std::env::var_os("ROCK_TEST_TIMINGS");

    let (files, package_dirs) = collect_test_units(paths)?;
    let shards = test_shard_count()?;

    let start = Instant::now();
    let mut emit_ok = 0usize; // units: emit valid Lua (passes or test-fails)
    let mut exec_pass = 0usize; // units: emit + all @tests pass
    let mut failed = 0usize; // units: emit/load failure
    let mut tests_total = 0usize; // individual @test functions executed
    let mut tests_passed = 0usize; // individual @test functions that passed
    let mut tests_skipped = 0usize;
    let mut reason_counts: HashMap<String, usize> = HashMap::new();
    let mut details: Vec<(String, String)> = Vec::new();
    let mut timings: Vec<(String, TestTiming)> = Vec::new();

    // bone can panic during inference on some inputs (a bone bug, not ours).
    // Silence the default panic print during the sweep and catch each unit's
    // panic so one bad unit doesn't abort the whole measurement — it's reported
    // as a `bone panic` failure instead.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));

    // Run every unit (files first, then packages), collecting labelled outcomes
    // so both kinds fold through one reporting path.
    type UnitResult = std::thread::Result<std::result::Result<SkullOutcome, SkullFailure>>;
    let mut outcomes: Vec<(String, UnitResult)> = Vec::new();
    for file in &files {
        let outcome: UnitResult = match fs_err::read_to_string(file) {
            Ok(src) => {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| skull_build(file, &src, filter)))
            }
            Err(e) => Ok(Err(SkullFailure { reason: e.to_string(), rendered: e.to_string() })),
        };
        outcomes.push((short_path(file).to_string(), outcome));
    }
    for dir in &package_dirs {
        let outcome: UnitResult =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| skull_build_package(dir, shards, filter)));
        outcomes.push((short_path(dir).to_string(), outcome));
    }

    for (label, outcome) in outcomes {
        match outcome {
            Ok(Ok(SkullOutcome::Pass { tests, skipped, timings: unit_timings })) => {
                timings.extend(unit_timings.into_iter().map(|t| (label.clone(), t)));
                emit_ok += 1;
                exec_pass += 1;
                tests_total += tests;
                tests_passed += tests;
                tests_skipped += skipped;
                if verbose {
                    println!("{:.<60} {}", label, "PASS".green());
                }
            }
            Ok(Ok(SkullOutcome::TestFail {
                passed,
                fails,
                skipped,
                timings: unit_timings,
                diagnostics,
            })) => {
                timings.extend(unit_timings.into_iter().map(|t| (label.clone(), t)));
                emit_ok += 1;
                tests_total += passed + fails.len();
                tests_passed += passed;
                tests_skipped += skipped;
                println!(
                    "{:.<60} {}  {}",
                    label,
                    "TESTFAIL".yellow(),
                    format!("{} @test(s) failed", fails.len()).dimmed()
                );
                for diagnostic in diagnostics {
                    println!("  {diagnostic}");
                }
                *reason_counts
                    .entry("@test execution failed".to_string())
                    .or_insert(0) += 1;
                if verbose {
                    details.push((label, fails.join("\n")));
                }
            }
            Ok(Err(SkullFailure { reason, rendered })) => {
                failed += 1;
                println!("{:.<60} {}  {}", label, "FAILED".red(), reason.dimmed());
                *reason_counts.entry(reason).or_insert(0) += 1;
                if verbose {
                    details.push((label, rendered));
                }
            }
            Err(panic) => {
                failed += 1;
                let msg = panic
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "unknown panic".to_string());
                let reason = "bone panic (inference)".to_string();
                println!("{:.<60} {}  {}", label, "PANIC".red().bold(), msg.dimmed());
                *reason_counts.entry(reason).or_insert(0) += 1;
                if verbose {
                    details.push((label, msg));
                }
            }
        }
    }

    std::panic::set_hook(prev_hook);

    if !reason_counts.is_empty() {
        let mut rows: Vec<(String, usize)> = reason_counts.into_iter().collect();
        rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        println!("\n{}", "=== failure reasons (skull) ===".bold());
        let shown = rows.len().min(25);
        for (reason, count) in rows.iter().take(shown) {
            println!("  {:>4}  {}", count.to_string().yellow(), reason);
        }
        if rows.len() > shown {
            println!("  {:>4}  ({} more distinct reasons)", "...", rows.len() - shown);
        }
    }

    if verbose && !details.is_empty() {
        println!("\n{}", "=== failure details ===".bold());
        for (file, rendered) in &details {
            println!("\n--- {} ---\n{}", file, rendered);
        }
    } else if failed > 0 || emit_ok > exec_pass {
        println!(
            "\n{}",
            "(re-run with ROCK_TEST_VERBOSE=1 for full per-file errors)".dimmed()
        );
    }

    if let Some(path) = &timings_path {
        write_test_timings(std::path::Path::new(path), &timings)?;
    }
    print_slowest_tests(&timings);

    let total = emit_ok + failed;
    let test_fails = emit_ok - exec_pass;
    let tests_failed = tests_total - tests_passed;
    let filter_summary = filter.map_or_else(String::new, |filter| {
        format!(
            "; filter '{}': {} selected, {} skipped",
            filter.display(),
            tests_total,
            tests_skipped
        )
    });
    crate::invocation_log::note_phase("units", start.elapsed().as_millis() as u64);
    crate::invocation_log::note_phase(
        "tests_run",
        timings.iter().map(|(_, t)| t.ms).sum::<f64>() as u64,
    );
    crate::invocation_log::note_counts(&[
        ("units_selected", total),
        ("units_failed", failed),
        ("units_test_failed", test_fails),
        ("tests_total", tests_total),
        ("tests_passed", tests_passed),
        ("tests_failed", tests_failed),
    ]);
    println!(
        "\nskull backend: {}/{} @tests passed ({} failed) across {} files ({:.2}s){}",
        tests_passed.to_string().green(),
        tests_total,
        if tests_failed == 0 {
            tests_failed.to_string().green()
        } else {
            tests_failed.to_string().red()
        },
        total,
        start.elapsed().as_secs_f64(),
        filter_summary,
    );
    println!(
        "  files: {} all-pass, {} emit valid Lua ({} with test failures), {} emit-fail, {} total",
        exec_pass.to_string().green(),
        emit_ok.to_string().green(),
        test_fails.to_string().yellow(),
        if failed == 0 {
            failed.to_string().green()
        } else {
            failed.to_string().red()
        },
        total,
    );
    if !package_dirs.is_empty() {
        println!("  @test shards: {shards} LuaJIT processes per package (ROCK_TEST_SHARDS)");
    }

    // LSP fixtures run through the bone-backed LSP (skull's language server),
    // via the same dispatch helper rock's `test` uses. Failures count toward
    // the non-zero exit alongside @test failures.
    let suites_start = Instant::now();
    let mut lsp_failed = 0usize;
    for path in paths {
        let (_t, _p, f, _i) = crate::lsp_test::run_lsp_suite(
            std::path::Path::new(path),
            crate::Backend::Skull,
            verbose,
        )?;
        lsp_failed += f;
    }

    // Error-test fixtures (`.rock.err`) verify rejection, which happens in the
    // shared bone front end before codegen -- so skull folds them in too.
    crate::invocation_log::note_phase("lsp_suite", suites_start.elapsed().as_millis() as u64);
    let err_start = Instant::now();
    let mut err_failed = 0usize;
    for path in paths {
        let (_t, _p, f, _i) = crate::error_test::run_error_suite(
            std::path::Path::new(path),
            crate::Backend::Skull,
            verbose,
        )?;
        err_failed += f;
    }

    // The Stage-0e census, before the exit below: this path leaves the process
    // with `process::exit`, which runs no destructor and never returns to
    // `main`, so a report installed only there would silently never print.
    crate::invocation_log::note_phase("error_suite", err_start.elapsed().as_millis() as u64);
    crate::invocation_log::note_counts(&[("lsp_failed", lsp_failed), ("error_failed", err_failed)]);
    let ablation_fatals = crate::ablation_report::report("rock test");

    if let Some(filter) = filter.filter(|_| tests_total == 0) {
        anyhow::bail!(
            "rock test filter '{}' matched no @tests (0 selected, {} skipped)",
            filter.display(),
            tests_skipped
        );
    }

    if tests_failed > 0
        || test_fails > 0
        || failed > 0
        || lsp_failed > 0
        || err_failed > 0
        || ablation_fatals
    {
        // Plain exit: the census above already drained the registry, so routing
        // this through `exit_with_census` would print an empty second one.
        crate::invocation_log::exit(1);
    }
    Ok(())
}

// luajit run + stdout-normalization helpers, shared by the hot-reload gates below.

/// Per-test execution outcome (clean stdout vs errored), as the gates read it.
#[cfg(test)]
#[path = "skull_backend_tests.rs"]
mod tests;

// ---------------------------------------------------------------------------
// skull-incremental-profile
// ---------------------------------------------------------------------------

struct SkullIncrRow {
    name: String,
    update_ms: f64,
    emit_ms: f64,
    reinferred: usize,
    matches: bool,
}

pub(crate) fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

/// Force a body re-cook by injecting a space at the start of the first top-level
/// `fn`'s first body line. Whitespace INSIDE the body's block span changes the
/// body hash (so the Session re-cooks + re-infers that body) but not the lowered
/// HIR, so skull re-emits a byte-identical chunk -- the touch exercises the
/// incremental path while the output stays comparable to the fresh baseline.
///
/// The space must land inside the block span `[bs..be]`, NOT in the gap right
/// after `{` (which classifies as LayoutOnly -> 0 bodies re-inferred), so anchor
/// it after the first newline following the brace. `None` for files with no
/// touchable top-level `fn` body.
pub(crate) fn body_touch(text: &str) -> Option<String> {
    let fn_pos = if text.starts_with("fn ") {
        0
    } else {
        text.find("\nfn ")? + 1
    };
    let brace = text[fn_pos..].find('{')? + fn_pos;
    let nl = text[brace..].find('\n')? + brace; // first newline after the brace
    let mut out = String::with_capacity(text.len() + 1);
    out.push_str(&text[..=nl]); // through the newline
    out.push(' '); // start of the first body line -> inside the block span
    out.push_str(&text[nl + 1..]);
    Some(out)
}

/// A REAL semantic body edit: flip the first single ASCII digit inside the body
/// of `fn <name>` (d -> d-1, '0'->'1'). Unlike `body_touch`'s whitespace, this
/// is a genuine 1-char constant change like editing mag `update_mobs`
/// `tick_anim(mob.anim, 3, ..)` 3->2 (alternated back to 3 by re-applying the
/// ORIGINAL text). Returns None if the fn or a body digit isn't found.
pub(crate) fn body_digit_flip(text: &str, name: &str) -> Option<String> {
    let fn_pos = text.find(&format!("fn {name}"))?;
    let brace = text[fn_pos..].find('{')? + fn_pos;

    // Bound the body by brace-matching so we only touch THIS function's digits.
    let mut depth = 0i32;
    let mut end = text.len();
    for (i, c) in text[brace..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    end = brace + i;
                    break;
                }
            }
            _ => {}
        }
    }

    let rel = text[brace..end].find(|c: char| c.is_ascii_digit())?;
    let abs = brace + rel;
    let d = text.as_bytes()[abs];
    let flipped = if d == b'0' { b'1' } else { d - 1 };

    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..abs]);
    out.push(flipped as char);
    out.push_str(&text[abs + 1..]);
    Some(out)
}

/// `rock skull-incremental-profile <project>` -- profile skull's INCREMENTAL
/// recompile path (bone `Session` per-file cook + `emit_incremental`, what `rock
/// watch --backend skull` runs). Per project file: force a body re-cook, time
/// `update_file` + `emit_incremental`, verify the incremental output is
/// byte-identical to the fresh `emit_full` baseline, then revert so the next
/// file measures from the same baseline. The skull analogue of
/// `incremental-profile` (which is rock-CompilationStorage-only). Exits 1 on any
/// byte divergence (an incremental-emit soundness bug).
pub fn skull_incremental_profile(path: &str) -> Result<()> {
    use std::path::PathBuf;

    let renderer_target = skull_renderer_target(path)?;
    let spec = crate::unified_compile::SourceSpec::Directory(PathBuf::from(path));
    let sources = crate::unified_compile::discover_sources(&spec)?;
    let files = crate::bone_backend::session_files(&sources);

    eprintln!("=== Skull Incremental Profile: {path} ===\n");

    let t_warm = Instant::now();
    let mut session =
        crate::bone_backend::bone_session_for_emission(files.clone(), std::path::Path::new(path))
            .map_err(|e| anyhow::anyhow!("session warm failed: {}", e.message))?;
    let warm_ms = ms(t_warm);

    let (baseline_lua, mut cache, full_ms) = {
        let ws = session.workspace();
        let mid = skull::user_module_id(ws)
            .map_err(no_user_module)?;
        let t_full = Instant::now();
        let (emit, c) = skull::mir_emit::emit_full_in_for_renderer(
            ws,
            mid,
            Some(path),
            renderer_target,
        )
            .map_err(|e| anyhow::anyhow!("{}", render_project_err(&sources, &e)))?;
        (emit.lua, c, ms(t_full))
    };

    eprintln!(
        "warm session {warm_ms:.1}ms | fresh emit_full {full_ms:.1}ms ({} lines of Lua)\n",
        baseline_lua.lines().count()
    );

    // Profile only the project's own files (skip the stone engine + other deps
    // discovery pulls in): match the project directory's basename as a path
    // component, e.g. `mag` -> only `.../mag/*.rock`.
    let proj_dir = format!(
        "{}/",
        std::path::Path::new(path)
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string())
    );

    let mut rows: Vec<SkullIncrRow> = Vec::new();
    let mut skipped = 0usize;

    for file in &files {
        let (fpath, text) = (&file.path, &file.text);
        if !fpath.contains(&proj_dir) {
            continue;
        }
        let Some(touched) = body_touch(text) else {
            skipped += 1;
            continue;
        };

        let t_u = Instant::now();
        let upd = session
            .update_file(fpath, &touched)
            .map_err(|e| anyhow::anyhow!("update_file({fpath}): {}", e.message))?;
        let update_ms = ms(t_u);

        let (emit_ms, incr_lua) = {
            let ws = session.workspace();
            let t_e = Instant::now();
            let incr = skull::mir_emit::emit_incremental(ws, &mut cache, &upd.reinferred)
                .map_err(|e| anyhow::anyhow!("emit_incremental({fpath}): {}", e.message))?;
            (ms(t_e), incr.lua)
        };

        rows.push(SkullIncrRow {
            name: std::path::Path::new(fpath)
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_else(|| fpath.clone()),
            update_ms,
            emit_ms,
            reinferred: upd.reinferred.len(),
            matches: incr_lua == baseline_lua,
        });

        // Revert so the next file starts from the baseline session + cache.
        let rev = session
            .update_file(fpath, text)
            .map_err(|e| anyhow::anyhow!("revert({fpath}): {}", e.message))?;
        let ws = session.workspace();
        skull::mir_emit::emit_incremental(ws, &mut cache, &rev.reinferred)
            .map_err(|e| anyhow::anyhow!("revert emit({fpath}): {}", e.message))?;
    }

    // --- table ---
    let max_name = rows.iter().map(|r| r.name.len()).max().unwrap_or(8).max(8);
    eprintln!(
        "{:<w$}  {:>9}  {:>8}  {:>10}  {:>8}  {:>6}",
        "Module", "Update", "Emit", "Reinferred", "vs Full", "Status",
        w = max_name
    );
    eprintln!("{}", "-".repeat(max_name + 50));
    let mut sorted: Vec<&SkullIncrRow> = rows.iter().collect();
    sorted.sort_by(|a, b| {
        (b.update_ms + b.emit_ms)
            .partial_cmp(&(a.update_ms + a.emit_ms))
            .unwrap()
    });
    for r in &sorted {
        let total = r.update_ms + r.emit_ms;
        let ratio = if full_ms > 0.0 {
            format!("{:.0}%", total / full_ms * 100.0)
        } else {
            "-".into()
        };
        eprintln!(
            "{:<w$}  {:>7.2}ms  {:>6.2}ms  {:>10}  {:>8}  {:>6}",
            r.name,
            r.update_ms,
            r.emit_ms,
            r.reinferred,
            ratio,
            if r.matches { "OK" } else { "DIFF!" },
            w = max_name
        );
    }

    // --- summary ---
    let n = rows.len().max(1) as f64;
    let avg_update: f64 = rows.iter().map(|r| r.update_ms).sum::<f64>() / n;
    let avg_emit: f64 = rows.iter().map(|r| r.emit_ms).sum::<f64>() / n;
    let diffs: Vec<&SkullIncrRow> = rows.iter().filter(|r| !r.matches).collect();
    eprintln!("\nSummary:");
    eprintln!("   Fresh emit_full:        {full_ms:.1}ms");
    eprintln!(
        "   Avg incremental:        {:.2}ms ({:.0}% of full)  [update {:.2}ms + emit {:.2}ms]",
        avg_update + avg_emit,
        (avg_update + avg_emit) / full_ms * 100.0,
        avg_update,
        avg_emit
    );
    eprintln!("   Modules profiled:       {}  ({} skipped: no touchable fn body)", rows.len(), skipped);

    if diffs.is_empty() {
        eprintln!(
            "\nAll {} modules: incremental emit byte-identical to fresh. Incremental is correct.",
            rows.len()
        );
        Ok(())
    } else {
        eprintln!("\n{} INCREMENTAL EMIT DIVERGENCES (incremental != fresh):", diffs.len());
        for r in &diffs {
            eprintln!("   {}", r.name);
        }
        crate::ablation_report::exit_with_census(1);
    }
}

/// `rock iter-incremental <project>` -- tight-loop sampling profiler for the REAL
/// stone incremental path. Warm a Session + `emit_full` once, then repeatedly
/// `update_file` + `emit_overlay` ONE body, alternating between two whitespace
/// states so every pass is a genuine change (not a content-hash no-op). Reports
/// the timing distribution (mean/p50/p99/min/max/stddev) + overlay size + the
/// per-phase breakdown of BOTH halves: `update_file` (clone/cook/lower/infer, from
/// bone's `phase_timings`) and `emit_overlay`, plus verdict lints. It calls the EXACT same
/// `session.update_file` + `skull::mir_emit::emit_overlay` that
/// `SkullStone::recompile` runs on a body edit (no reassembly -- the two-file
/// split's whole point), so the numbers are the live recompile path, just sampled
/// in a tight loop instead of off disk.
///
/// Post-rung-2, `update_file` re-lowers + re-infers ONLY the changed body (not the
/// whole module). `SKULL_ITER_FILE=<substr>` targets a file (whitespace touch of
/// its first fn); `SKULL_ITER_FN=<name>` profiles a REAL constant edit -- flips
/// the first digit in that fn's body, alternating with the original (e.g.
/// `SKULL_ITER_FN=update_mobs` flips mag's `tick_anim(.., 3, ..)` 3<->2).
/// `SKULL_ITER_WRITE=1` additionally measures the production atomic overlay
/// write into a temporary output path.
fn iter_incremental_sources(path: &str) -> Result<rock_core::SourceFiles> {
    let spec = crate::unified_compile::SourceSpec::Directory(std::path::PathBuf::from(path));
    crate::unified_compile::discover_sources(&spec)?
        .production_only(std::path::Path::new(path))
        .map_err(|error| anyhow::anyhow!(error.to_string()))
}

pub fn skull_iter_incremental(path: &str, iters: usize, warmup: usize) -> Result<()> {
    let renderer_target = skull_renderer_target(path)?;
    let sources = iter_incremental_sources(path)?;
    let files = crate::bone_backend::session_files(&sources);

    let mut session =
        crate::bone_backend::bone_session_for_emission(files.clone(), std::path::Path::new(path))
            .map_err(|e| anyhow::anyhow!("session warm failed: {}", e.message))?;

    let mut cache = {
        let ws = session.workspace();
        let mid = skull::user_module_id(ws)
            .map_err(no_user_module)?;
        let (_emit, c) = skull::mir_emit::emit_full_in_for_renderer(
            ws,
            mid,
            Some(path),
            renderer_target,
        )
            .map_err(|e| anyhow::anyhow!("{}", render_project_err(&sources, &e)))?;
        c
    };

    // Pick the first project file with a touchable top-level fn body, and build
    // two whitespace states (1 vs 2 spaces inside that body) so consecutive
    // updates always differ.
    let proj_dir = format!(
        "{}/",
        std::path::Path::new(path)
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string())
    );
    // SKULL_ITER_FN=<name>: profile a REAL constant edit (digit flip) in that
    // fn's body, alternating modified <-> original -- e.g.
    // `SKULL_ITER_FN=update_mobs rock iter-incremental mag` flips `tick_anim(..,
    // 3, ..)` 3<->2. Else: auto-pick a file and inject whitespace (body_touch).
    let (target, mut state_a, mut state_b) = if let Ok(fname) = std::env::var("SKULL_ITER_FN") {
        let f = files
            .iter()
            .find(|f| f.path.contains(&proj_dir) && f.text.contains(&format!("fn {fname}")))
            .ok_or_else(|| anyhow::anyhow!("no project file with `fn {fname}` in {path}"))?;
        let (p, t) = (&f.path, &f.text);
        let flipped = body_digit_flip(t, &fname)
            .ok_or_else(|| anyhow::anyhow!("no body digit to flip in `fn {fname}`"))?;
        // alternate flipped <-> original (a genuine 1-char body edit each pass)
        (p.clone(), flipped, t.clone())
    } else {
        // Pick the first project file whose touch actually re-cooks a body (>=1
        // re-inferred): some files' first fn lands the injected whitespace outside
        // the block span (LayoutOnly -> 0 bodies). Trial each via the live Session,
        // reverting after so the baseline is restored (no emit, so the cache is
        // untouched). Profiling a 0-reinferred file would only measure reassembly.
        let want_file = std::env::var("SKULL_ITER_FILE").ok();
        let mut chosen: Option<(String, String)> = None;
        for file in &files {
            let (p, t) = (&file.path, &file.text);
            if !p.contains(&proj_dir) {
                continue;
            }
            if let Some(w) = &want_file {
                if !p.contains(w.as_str()) {
                    continue;
                }
            }
            let Some(touched) = body_touch(t) else { continue };
            let upd = session
                .update_file(p, &touched)
                .map_err(|e| anyhow::anyhow!("trial update_file({p}): {}", e.message))?;
            let reinferred = upd.reinferred.len();
            session
                .update_file(p, t)
                .map_err(|e| anyhow::anyhow!("trial revert({p}): {}", e.message))?;
            if reinferred >= 1 {
                chosen = Some((p.clone(), t.clone()));
                break;
            }
        }
        let (target, orig) = chosen
            .ok_or_else(|| anyhow::anyhow!("no project file whose touch re-cooks a body in {path}"))?;
        let a = body_touch(&orig).unwrap();
        let b = body_touch(&a).unwrap();
        (target, a, b)
    };
    if std::env::var_os("SKULL_ITER_REVERSE").is_some() {
        std::mem::swap(&mut state_a, &mut state_b);
    }

    let mut lint_cache = bone_lints::IncrementalLintCache::default();
    let initial_lint_errors = crate::lint_pass::run_build_lints_cached(
        &mut lint_cache,
        session.workspace(),
        &sources,
        std::path::Path::new(path),
        crate::lint_pass::Scope::WholeProject,
        &bone_lints::LintCacheUpdate::full(),
    );
    if !initial_lint_errors.is_empty() {
        return Err(anyhow::anyhow!(
            "iter-incremental fixture produced {} initial lint error(s)",
            initial_lint_errors.len()
        ));
    }

    let iters = iters.max(1);
    let target_name = std::path::Path::new(&target)
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| target.clone());

    let write_disk = std::env::var_os("SKULL_ITER_WRITE").is_some();
    let write_dir = tempfile::tempdir().map_err(|e| anyhow::anyhow!("create iter output: {e}"))?;
    let write_path = write_dir.path().join("incremental.lua");
    let mut samples: Vec<f64> = Vec::with_capacity(iters);
    let mut disk_samples: Vec<f64> = Vec::with_capacity(iters);
    let mut last_update = 0.0;
    let mut last_diag = 0.0;
    let mut last_lint = 0.0;
    let mut last_emit = 0.0;
    let mut last_write = 0.0;
    let mut last_bytes = 0usize;
    let mut last_reinferred = 0usize;
    let mut last_body_meta = (0usize, false, 0usize, 0isize);
    let mut last_phases: Vec<(&'static str, std::time::Duration)> = Vec::new();
    let mut last_update_phases: Vec<(&'static str, std::time::Duration)> = Vec::new();
    let phase_stats = std::env::var_os("SKULL_ITER_PHASE_STATS").is_some();
    let mut phase_samples = std::collections::BTreeMap::<String, Vec<f64>>::new();

    for i in 0..(warmup + iters) {
        let text = if i % 2 == 0 { &state_a } else { &state_b };

        let t_u = Instant::now();
        let upd = session
            .update_file(&target, text)
            .map_err(|e| anyhow::anyhow!("update_file: {}", e.message))?;
        if let Some(source) = sources
            .files
            .iter()
            .find(|source| source.borrow().file.to_id_path() == target)
        {
            source.borrow_mut().contents = text.clone();
        }
        let update_ms = ms(t_u);
        // bone's per-edit phase breakdown (clone/cook/lower/infer), set by
        // rebuild_body_incremental inside the SAME update_file the stone loop calls.
        let update_phases = session.workspace().phase_timings.clone();
        let span_edits = upd
            .body_update
            .edit
            .as_ref()
            .map(|edit| {
                vec![skull::instance_graph::SpanEdit {
                    file: edit.file,
                    at: edit.at,
                    byte_delta: edit.byte_delta,
                    line_delta: edit.line_delta,
                }]
            })
            .unwrap_or_default();

        // `SkullStone::recompile` calls `diagnostics()` inside its `cook` timing --
        // an O(workspace) error scan + sort over every module. Measure it here too
        // so the iter-incremental "cook" matches the live one (the live cook ALSO
        // includes disk read of the changed file, which an in-memory loop can't).
        let t_d = Instant::now();
        let _ = session.diagnostics();
        let diag_ms = ms(t_d);

        let t_l = Instant::now();
        let lint_change = if matches!(upd.tier, bone_incremental::session::UpdateTier::BodyOnly) {
            bone_lints::LintCacheUpdate::body_only(
                [ustr::Ustr::from(target.as_str())],
                upd.reinferred.iter().copied(),
                upd.body_update.edit.iter().map(|edit| bone_lints::TextEditDelta {
                    file: edit.file,
                    at: edit.at,
                    byte_delta: edit.byte_delta,
                    line_delta: edit.line_delta,
                }),
            )
        } else {
            bone_lints::LintCacheUpdate::full()
        };
        let lint_errs = crate::lint_pass::run_build_lints_cached(
            &mut lint_cache,
            session.workspace(),
            &sources,
            std::path::Path::new(path),
            crate::lint_pass::Scope::WholeProject,
            &lint_change,
        );
        if !lint_errs.is_empty() {
            return Err(anyhow::anyhow!(
                "iter-incremental fixture produced {} lint error(s)",
                lint_errs.len()
            ));
        }
        let lint_ms = ms(t_l);

        // Profile the REAL stone path: `emit_overlay` (no reassembly), matching
        // what `SkullStone::recompile` writes on a body edit.
        let (emit_ms, overlay, phases) = {
            let ws = session.workspace();
            let t_e = Instant::now();
            let e = skull::mir_emit::emit_overlay_with_span_edits(
                ws,
                &mut cache,
                &upd.reinferred,
                &span_edits,
            )
                .map_err(|e| anyhow::anyhow!("emit_overlay: {}", e.message))?;
            (ms(t_e), e.lua, e.phases)
        };
        let overlay_bytes = overlay.len();
        let write_ms = if write_disk {
            let t_w = Instant::now();
            rock_host::project::write_atomic(&write_path, &overlay)
                .map_err(|e| anyhow::anyhow!("iter-incremental write: {e}"))?;
            ms(t_w)
        } else {
            0.0
        };

        if i >= warmup {
            if phase_stats {
                for (label, value) in [
                    ("session update", update_ms),
                    ("diagnostics", diag_ms),
                    ("lints", lint_ms),
                    ("emit overlay", emit_ms),
                    ("atomic write", write_ms),
                ] {
                    phase_samples.entry(label.to_string()).or_default().push(value);
                }
                for (prefix, timings) in [("session", &update_phases), ("emit", &phases)] {
                    for (label, duration) in timings {
                        phase_samples.entry(format!("{prefix}/{label}")).or_default()
                            .push(duration.as_secs_f64() * 1000.0);
                    }
                }
            }
            let backend_ms = update_ms + diag_ms + lint_ms + emit_ms;
            samples.push(backend_ms);
            if write_disk {
                disk_samples.push(backend_ms + write_ms);
            }
            last_update = update_ms;
            last_diag = diag_ms;
            last_lint = lint_ms;
            last_emit = emit_ms;
            last_bytes = overlay_bytes;
            last_write = write_ms;
            last_reinferred = upd.reinferred.len();
            last_body_meta = (
                upd.body_update.semantic_bodies.len(),
                upd.body_update.span_only,
                upd.body_update.edit.as_ref().map_or(0, |edit| edit.at),
                upd.body_update.edit.as_ref().map_or(0, |edit| edit.byte_delta),
            );
            last_phases = phases;
            last_update_phases = update_phases;
        }
    }

    let n = samples.len() as f64;
    let mean = samples.iter().sum::<f64>() / n;
    let var = samples.iter().map(|s| (s - mean).powi(2)).sum::<f64>() / n;
    let stddev = var.sqrt();
    let cv = if mean > 0.0 { stddev / mean * 100.0 } else { 0.0 };
    let mut sorted = samples.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pct = |p: f64| sorted[((p / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize];

    eprintln!("=== Skull Iter-Incremental: {path} ===");
    eprintln!(
        "touching `{target_name}` ({} reinferred/pass) | {iters} iters ({warmup} warmup)\n",
        last_reinferred
    );
    eprintln!(
        "  backend route per pass [ms] (update + diagnostics + lints + overlay):  mean {:.3}  p50 {:.3}  p99 {:.3}  min {:.3}  max {:.3}  stddev {:.3} (cv {:.1}%)",
        mean, pct(50.0), pct(99.0), sorted[0], sorted[sorted.len() - 1], stddev, cv
    );
    if write_disk {
        let mut disk_sorted = disk_samples.clone();
        disk_sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let disk_mean = disk_samples.iter().sum::<f64>() / n;
        let disk_var = disk_samples.iter().map(|s| (s - disk_mean).powi(2)).sum::<f64>() / n;
        let disk_pct = |p: f64| {
            disk_sorted[((p / 100.0) * (disk_sorted.len() as f64 - 1.0)).round() as usize]
        };
        eprintln!(
            "  disk-inclusive per pass [ms] (backend + atomic overlay write):  mean {:.3}  p50 {:.3}  p99 {:.3}  min {:.3}  max {:.3}  stddev {:.3} (cv {:.1}%)",
            disk_mean,
            disk_pct(50.0),
            disk_pct(99.0),
            disk_sorted[0],
            disk_sorted[disk_sorted.len() - 1],
            disk_var.sqrt(),
            if disk_mean > 0.0 { disk_var.sqrt() / disk_mean * 100.0 } else { 0.0 },
        );
    }
    if phase_stats {
        eprintln!("\n  per-phase samples [ms]: phase | n | mean | p50 | p99 | min");
        for (label, mut values) in phase_samples {
            let mean = values.iter().sum::<f64>() / values.len() as f64;
            values.sort_by(f64::total_cmp);
            let percentile = |p: f64| {
                values[((p / 100.0) * (values.len() as f64 - 1.0)).round() as usize]
            };
            eprintln!("    phase | {label} | {} | {mean:.6} | {:.6} | {:.6} | {:.6}",
                values.len(), percentile(50.0), percentile(99.0), values[0]);
        }
    }
    eprintln!("\n  last-pass breakdown (the two-file split path):");
    eprintln!(
        "    Session update_file:            {last_update:.3} ms  (re-lower+re-infer ONLY the {last_reinferred} changed body/bodies)"
    );
    for (lbl, d) in &last_update_phases {
        eprintln!("       {lbl}: {:.3} ms", d.as_secs_f64() * 1000.0);
    }
    let update_known: f64 = last_update_phases.iter().map(|(_, d)| d.as_secs_f64() * 1000.0).sum();
    eprintln!("       (parse + classify, untimed: {:.3} ms)", (last_update - update_known).max(0.0));
    eprintln!("    diagnostics (O(workspace) scan): {last_diag:.3} ms");
    eprintln!("    lints (whole-project verdict):  {last_lint:.3} ms");
    eprintln!("    emit_overlay:                   {last_emit:.3} ms  ({last_bytes} B overlay)");
    if write_disk {
        eprintln!("    incremental file write:         {last_write:.3} ms");
    }
    for (lbl, d) in &last_phases {
        eprintln!("       {lbl}: {:.3} ms", d.as_secs_f64() * 1000.0);
    }
    if std::env::var_os("SKULL_REPAIR_STATS").is_some() {
        let stats = cache.repair_stats();
        eprintln!(
            "    body metadata: semantic={} span_only={} edit_at={} byte_delta={}",
            last_body_meta.0, last_body_meta.1, last_body_meta.2, last_body_meta.3
        );
        eprintln!(
            "    repair counters: body={} owners={} instances={} graph_retract={} graph_install={} full_graph={} \
concrete_derived={} concrete_reused={} concrete_retracted={} full_concrete={} \
artifact_revisited={} artifact_added={} artifact_removed={} full_artifact={} \
broad_owner={} broad_concrete={} broad_artifact={} broad_candidate={}",
            stats.body_only_edits,
            stats.owners_revisited,
            stats.instances_revisited,
            stats.graph_contributions_retracted,
            stats.graph_contributions_installed,
            stats.full_graph_rebuilds,
            stats.concrete_entries_derived,
            stats.concrete_entries_reused,
            stats.concrete_entries_retracted,
            stats.full_concrete_rebuilds,
            stats.artifact_chunks_revisited,
            stats.artifact_chunks_added,
            stats.artifact_chunks_removed,
            stats.full_artifact_view_realizations,
            stats.broad_owner_scans,
            stats.broad_concrete_scans,
            stats.broad_artifact_scans,
            stats.broad_candidate_scans,
        );
        eprintln!(
            "    locality census: roots requested={} modules_examined={} functions_examined={} reseeded={} root-instances-checked={} \
shader requests={} roots examined={} derived={} declarations requested={} instances looked up={} \
candidates requested={} invalidated={} derived={} reused={} dependents requested={} visited={} \
chunks requested={} rebuilt={} artifact requests={} iterations={} index-rows={} root entries={} realizes={} \
root-hole names={} rows={} decls={} reason-buckets={} erased-catalogue={}",
            stats.root_requests,
            stats.root_modules_examined,
            stats.root_functions_examined,
            stats.root_reseeds,
            stats.root_instances_checked,
            stats.shader_delta_requests,
            stats.shader_roots_examined,
            stats.shader_roots_derived,
            stats.declaration_lookup_requests,
            stats.declaration_instance_lookups,
            stats.candidate_entries_requested,
            stats.candidate_entries_invalidated,
            stats.candidate_entries_derived,
            stats.candidate_entries_reused,
            stats.dependent_requests,
            stats.dependents_visited,
            stats.chunk_rebuild_requests,
            stats.chunks_rebuilt,
            stats.artifact_iteration_requests,
            stats.artifact_iterations,
            stats.artifact_index_rows_examined,
            stats.artifact_root_entries_examined,
            stats.artifact_realize_invocations,
            stats.root_hole_names_examined,
            stats.root_hole_rows_examined,
            stats.root_hole_decls_examined,
            stats.root_hole_reason_buckets_examined,
            stats.erased_catalogue_entries_examined,
        );
    }
    if let Some(census) = crate::lint_pass::take_census() {
        crate::lint_pass::print_census(&census);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// stone --backend skull: two-file incremental output (main.lua + incremental.lua)
// ---------------------------------------------------------------------------

pub(crate) fn configured_stone_dir(path: &std::path::Path) -> Result<Option<std::path::PathBuf>> {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let Some((config_path, _config)) = rock_host::stick::StickConfig::find_from_dir(&path)? else {
        return Ok(None);
    };
    let manifest_dir = config_path.parent().unwrap_or(&config_path);
    let graph = rock_host::stick::graph::PackageGraph::build(manifest_dir)?;
    let root = graph.root();
    // Stone testing itself: the root IS the stone package, so resolve its own
    // directory instead of reporting no Stone root (which fails global init).
    if graph.package(root).manifest_name == "stone" {
        return Ok(Some(graph.package(root).root.clone()));
    }
    Ok(graph
        .packages()
        .iter()
        .find(|package| package.id != root && package.manifest_name == "stone")
        .map(|package| package.root.clone()))
}

fn stone_root_preamble(stone_dir: &std::path::Path) -> String {
    let escaped = stone_dir.display().to_string().replace('\\', "\\\\").replace('"', "\\\"");
    format!("__ROCK_STONE_DIR = \"{escaped}\"\n")
}

/// The game preamble injected at the top of `main.lua`: `GAME_SOURCE_DIR` for
/// asset resolution. cimgui is initialized by stone's global initializer.
/// Factored so `skull_run_project` and the stone two-file path build it
/// identically.
pub(crate) fn skull_game_preamble(dir: &str) -> Result<String> {
    use std::path::PathBuf;
    let abs = PathBuf::from(dir).canonicalize().unwrap_or_else(|_| PathBuf::from(dir));
    let escaped = abs.display().to_string().replace('\\', "\\\\").replace('"', "\\\"");
    let config = match rock_host::stick::StickConfig::find_from_dir(&abs)? {
        Some((_, config)) => Some(config),
        None => None,
    };
    let overlay_defaults = rock_host::release::configured_asset_overlays_from(
        &abs,
        config.as_ref(),
        std::env::var_os(rock_host::release::ASSET_OVERLAYS_ENV).as_deref(),
    )?;
    let renderer = match skull_renderer_target(dir)? {
        skull::mir_emit::RendererTarget::Gl => "gl",
        skull::mir_emit::RendererTarget::Wgpu => "wgpu",
    };
    let mut full = format!("__ROCK_GAME_SOURCE_DIR = \"{escaped}\"\n");
    if let Some(stone_dir) = configured_stone_dir(&abs)? {
        let escaped_stone = stone_dir.display().to_string().replace('\\', "\\\\").replace('"', "\\\"");
        full.push_str(&format!("__ROCK_STONE_DIR = \"{escaped_stone}\"\n"));
    }
    full.push_str("__ROCK_ASSET_OVERLAY_DEFAULTS = {");
    for (index, path) in overlay_defaults.iter().enumerate() {
        if index > 0 {
            full.push_str(", ");
        }
        let path = path.display().to_string().replace('\\', "\\\\").replace('"', "\\\"");
        full.push('"');
        full.push_str(&path);
        full.push('"');
    }
    full.push_str("}\n");
    // Stamped for EVERY renderer, not only wgpu. The engine reads this global
    // per draw (`stone_renderer_is_wgpu` and friends compare it to "wgpu"),
    // and a global whose value is nil is ABSENT from `_G`, so every read of it
    // fell through to the state bridge's `__index` metamethod: 5.5M dispatches
    // per bench run, 98% of everything that reached the bridge. A present
    // value is a raw hit that constant-folds on a trace.
    full.push_str(&format!("__ROCK_RENDERER_BACKEND = \"{renderer}\"\n"));
    if !full.ends_with('\n') {
        full.push('\n');
    }
    Ok(full)
}

pub(crate) fn ensure_renderer_feature(dir: &str) -> Result<()> {
    skull_renderer_target(dir).map(|_| ())
}

fn skull_renderer_target(dir: &str) -> Result<skull::mir_emit::RendererTarget> {
    let backend = skull_renderer_backend(dir)?;
    if matches!(backend, rock_host::stick::RendererBackend::Wgpu)
        && !cfg!(feature = "renderer-wgpu")
    {
        anyhow::bail!(
            "the `wgpu` renderer was omitted from this build; enable the `renderer-wgpu` feature (for example, `just test-wgpu` or `cargo run --features renderer-wgpu --bin rock ... --renderer wgpu)"
        );
    }
    Ok(match backend {
        rock_host::stick::RendererBackend::Gl => skull::mir_emit::RendererTarget::Gl,
        rock_host::stick::RendererBackend::Wgpu => skull::mir_emit::RendererTarget::Wgpu,
    })
}

pub(crate) fn skull_renderer_backend(dir: &str) -> Result<rock_host::stick::RendererBackend> {
    let abs = std::path::Path::new(dir)
        .canonicalize()
        .unwrap_or_else(|_| std::path::PathBuf::from(dir));
    Ok(match rock_host::stick::StickConfig::find_from_dir(&abs)? {
        Some((_, cfg)) => cfg.renderer_backend()?,
        None => rock_host::stick::RendererBackend::Gl,
    })
}

/// Game-shaped `main.lua`: preamble + emit + `__rock_main_wrapper(main)` entry.
///
/// `emits_main` comes from the emitter, not from searching `emit_lua`: the
/// emitted spelling of a definition is the emitter's business and has already
/// changed once (`ROCK_SKULL_CALLEE_BINDING=upvalue` moves every body into a
/// factory, so `function main(` appears nowhere), which left a chunk that
/// defined the whole game and then ran nothing.
fn skull_game_main<'a>(preamble: &'a str, program: &'a GameProgram, emits_main: bool) -> [&'a str; 5] {
    let [head, metadata, tail] = program.parts();
    let entry = if emits_main { "\n__rock_main_wrapper(main)\n" } else { "" };
    [preamble, head, metadata, tail, entry]
}

fn write_program_parts(writer: &mut impl std::io::Write, parts: &[&str]) -> std::io::Result<()> {
    for part in parts {
        writer.write_all(part.as_bytes())?;
    }
    Ok(())
}

/// Remove the accumulated overlay. A missing file is the normal "no overlay yet"
/// case and is fine; ANY other error is surfaced -- a stale `incremental.lua`
/// left beside a fresh `main.lua` would reintroduce the overlay/main desync the
/// two-file split exists to avoid.
pub(crate) fn wipe_overlay(incremental_path: &std::path::Path) -> Result<()> {
    match std::fs::remove_file(incremental_path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(anyhow::anyhow!(
            "skull: failed to remove stale overlay {}: {e}",
            incremental_path.display()
        )),
    }
}

/// Write the full game-shaped `main.lua` and wipe any accumulated overlay -- the
/// shared tail of every FULL (re)build: `init`, the interface-rebuild branch of
/// `recompile`, and `force_full`. Returns the bytes written (the size the `Full`
/// reload reports).
fn write_full_main(
    preamble: &str,
    emit_lua: &GameProgram,
    emits_main: bool,
    main_path: &std::path::Path,
    incremental_path: &std::path::Path,
) -> Result<usize> {
    let main = skull_game_main(preamble, emit_lua, emits_main);
    let bytes = main.iter().map(|part| part.len()).sum();
    rock_host::project::write_atomic_streamed(main_path, |file| write_program_parts(file, &main))?;
    wipe_overlay(incremental_path)?;
    Ok(bytes)
}

/// What a stone-skull recompile produced. `cook_ms` is the front end (source
/// re-read + per-file incremental cook + re-typecheck via `Session.update_file`);
/// `emit_ms` is codegen + file write (`emit_overlay`/`emit_full`). `bytes` is the
/// written output size -- for `Incremental` that's the overlay size, the headline
/// signal for the overlay-size question.
#[derive(Debug)]
pub enum SkullReload {
    /// Full rebuild after an interface or structural edit.
    Full { bytes: usize, cook_ms: f64, emit_ms: f64, write_ms: f64 },
    /// Body edit: only `incremental.lua` rewritten (the overlay). `reinferred` is
    /// the number of bodies this edit re-inferred (the edit's churn). `emit_ms` is
    /// pure `emit_overlay` (directly comparable to `iter-incremental`); `write_ms`
    /// is the separate disk write of the overlay.
    Incremental { bytes: usize, reinferred: usize, cook_ms: f64, emit_ms: f64, write_ms: f64 },
    /// A full artifact is required before the current process can observe this
    /// update; no output was written on this path.
    FullRequired { reasons: Vec<String> },
    /// A type's runtime SHAPE changed (docs/hot-reload-schema-drift-plan.md):
    /// live state cannot be migrated in place. Nothing was written -- the
    /// supervisor must use the restart fallback. `types` names the drifted
    /// types for the
    /// why-log.
    ShapeChanged { types: Vec<String> },
    /// Nothing changed (content-hash no-op).
    NoChange,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SkullRouteProfile {
    pub session_snapshot_ns: u128,
    pub source_lookup_feed_ns: u128,
    pub session_update_ns: u128,
    pub diagnostics_ns: u128,
    pub lint_total_ns: u128,
    pub local_lint_derivation_ns: u128,
    pub global_lint_dependency_ns: u128,
    pub global_lint_derivation_ns: u128,
    pub suppression_filtering_ns: u128,
    pub suppression_filter_calls: usize,
    pub report_prepare_ns: u128,
    pub summary_sort_ns: u128,
    pub render_ns: u128,
    pub finding_aggregation_ns: u128,
    pub publication_shape_checks_ns: u128,
    pub transaction_checkpoint_ns: u128,
    pub body_transactions_started: usize,
    pub transaction_commit_ns: u128,
    pub affected_chunk_selection_ns: u128,
    pub emit_overlay_ns: u128,
    pub atomic_output_write_ns: u128,
    pub rollback_ns: u128,
    pub live_update_ns: u128,
    pub live_project_call_ns: u128,
    pub route_emit_call_ns: u128,
    pub live_project_unaccounted_ns: u128,
    pub route_unaccounted_ns: u128,
    pub total_ns: u128,
    pub unaccounted_ns: u128,
}

impl SkullRouteProfile {
    fn from_live(timing: crate::live_project::LiveRouteTiming) -> Self {
        Self {
            session_snapshot_ns: timing.session_snapshot_ns,
            source_lookup_feed_ns: timing.source_lookup_feed_ns,
            session_update_ns: timing.session_update_ns,
            diagnostics_ns: timing.diagnostics_ns,
            lint_total_ns: timing.lint_total_ns,
            local_lint_derivation_ns: timing.local_lint_derivation_ns,
            global_lint_dependency_ns: timing.global_lint_dependency_ns,
            global_lint_derivation_ns: timing.global_lint_derivation_ns,
            suppression_filtering_ns: timing.suppression_filtering_ns,
            suppression_filter_calls: timing.suppression_filter_calls,
            report_prepare_ns: timing.report_prepare_ns,
            summary_sort_ns: timing.summary_sort_ns,
            render_ns: timing.render_ns,
            finding_aggregation_ns: timing.finding_aggregation_ns,
            rollback_ns: timing.rollback_ns,
            live_update_ns: timing.live_update_ns,
            ..Self::default()
        }
    }

    fn finish(&mut self, total_ns: u128) {
        self.total_ns = total_ns;
        self.live_project_unaccounted_ns = self
            .live_project_call_ns
            .saturating_sub(self.session_snapshot_ns + self.live_update_ns);
        self.route_unaccounted_ns = self
            .route_emit_call_ns
            .saturating_sub(
                self.publication_shape_checks_ns
                    + self.transaction_checkpoint_ns
                    + self.transaction_commit_ns
                    + self.emit_overlay_ns
                    + self.atomic_output_write_ns,
            );
        let accounted = self.live_project_call_ns + self.route_emit_call_ns + self.rollback_ns;
        self.unaccounted_ns = total_ns.saturating_sub(accounted);
    }

    fn add_live(&mut self, timing: crate::live_project::LiveRouteTiming) {
        self.session_snapshot_ns += timing.session_snapshot_ns;
        self.source_lookup_feed_ns += timing.source_lookup_feed_ns;
        self.session_update_ns += timing.session_update_ns;
        self.diagnostics_ns += timing.diagnostics_ns;
        self.lint_total_ns += timing.lint_total_ns;
        self.local_lint_derivation_ns += timing.local_lint_derivation_ns;
        self.global_lint_dependency_ns += timing.global_lint_dependency_ns;
        self.global_lint_derivation_ns += timing.global_lint_derivation_ns;
        self.suppression_filtering_ns += timing.suppression_filtering_ns;
        self.suppression_filter_calls += timing.suppression_filter_calls;
        self.report_prepare_ns += timing.report_prepare_ns;
        self.summary_sort_ns += timing.summary_sort_ns;
        self.render_ns += timing.render_ns;
        self.finding_aggregation_ns += timing.finding_aggregation_ns;
        self.rollback_ns += timing.rollback_ns;
        self.live_update_ns += timing.live_update_ns;
    }
}

fn emit_phase_ns(phases: &[(&'static str, Duration)], label: &str) -> u128 {
    phases
        .iter()
        .find(|(name, _)| *name == label)
        .map_or(0, |(_, duration)| duration.as_nanos())
}

/// Persistent stone-skull compile state: a warm bone `Session` + skull emit cache
/// driving the two-file split (`docs/skull-incremental-emit-split.md`). A body
/// edit re-cooks just the edited file and writes a small `incremental.lua` the
/// running game light-reloads; an interface/structural edit rewrites `main.lua`
/// (full) and wipes the overlay.
pub struct SkullStone {
    selected_sim_names: Option<std::collections::HashSet<String>>,
    project: crate::live_project::LiveProject,
    cache: skull::mir_emit::SkullEmitCache,
    preamble: String,
    renderer_target: skull::mir_emit::RendererTarget,
    /// Per-type layout hashes of the last program the game process could have
    /// loaded. A full recompile whose hashes drifted returns
    /// `SkullReload::ShapeChanged` (full-reconstruction request) instead of writing
    /// (docs/hot-reload-schema-drift-plan.md).
    shape_hashes: skull::ShapeHashes,
    /// Full rebuilds retain the emitted shape of the original artifact.
    callee_binding: skull::CalleeBindingMode,
    route_profile_enabled: bool,
    last_route_profile: Option<SkullRouteProfile>,
    artifact_fingerprint: String,
}

impl SkullStone {
    /// Initial build: warm the Session, `emit_full` -> game-shaped `main.lua`,
    /// wipe `incremental.lua`. Errors render against the project sources.
    /// Game build: excludes test-only files (tests/ dirs) so a broken sim/test
    /// can't break the running game. See `docs/sim-layering.md`.
    pub fn init(dir: &str, main_path: &std::path::Path, incremental_path: &std::path::Path) -> Result<SkullStone> {
        Self::init_impl(dir, main_path, incremental_path, false, None, &[])
    }

    /// Sim-runner build: same as [`SkullStone::init`] but discovery KEEPS
    /// test-only files -- `@sim` bodies live in `tests/sims/` and must be part
    /// of the emitted program.
    pub fn init_with_tests(dir: &str, main_path: &std::path::Path, incremental_path: &std::path::Path) -> Result<SkullStone> {
        Self::init_impl(dir, main_path, incremental_path, true, None, &[])
    }

    pub fn init_with_tests_and_allocation_report(
        dir: &str,
        main_path: &std::path::Path,
        incremental_path: &std::path::Path,
        allocation_manifest: &std::path::Path,
    ) -> Result<SkullStone> {
        let _activation = skull::allocation_report::activation_scope(true);
        Self::init_impl(
            dir,
            main_path,
            incremental_path,
            true,
            Some(allocation_manifest),
            &[],
        )
    }

    fn init_impl(
        dir: &str,
        main_path: &std::path::Path,
        incremental_path: &std::path::Path,
        include_tests: bool,
        explicit_allocation_report: Option<&std::path::Path>,
        selection: &[String],
    ) -> Result<SkullStone> {
        let stats = crate::compile_measurements::Scope::begin(dir);
        let slot = crate::compile_slots::acquire(dir)?;
        crate::compile_measurements::slot(slot.acquisition());
        let renderer_target = skull_renderer_target(dir)?;
        let project = crate::live_project::LiveProject::init(dir, include_tests)?;
        let (lua, cache, artifact_fingerprint, emits_main, selected_sim_names) = {
            let ws = project.workspace();
            let mid = skull::user_module_id(ws)
                .map_err(no_user_module)?;
            let all_names = collect_skull_sims(ws).into_iter().map(|(name, _, _)| name).collect::<Vec<_>>();
            let selected = crate::run_sim::resolve_sim_specs(&all_names, selection)?;
            let selected = selected.into_iter().flat_map(|spec| spec.split(',').map(str::to_owned).collect::<Vec<_>>())
                .collect::<std::collections::BTreeSet<_>>();
            let narrow = !selection.is_empty() && selected.len() != all_names.len();
            let (emit, c) = if narrow {
                skull::mir_emit::emit_full_for_selected_sims(
                    ws, mid, Some(dir), renderer_target, &selected.into_iter().collect::<Vec<_>>(),
                )
            } else {
                skull::mir_emit::emit_full_in_for_renderer_with_callee_binding(
                    ws, mid, Some(dir), renderer_target, None,
                )
            }.map_err(|e| anyhow::anyhow!("{}", render_project_err(project.sources(), &e)))?;
            let selected_sim_names = if narrow { Some(c.registered_sim_names(ws)) } else { None };
            crate::compile_measurements::emission(&emit);
            let allocation_report_path = allocation_report_path_from_env();
            write_allocation_report(
                explicit_allocation_report
                    .map(std::path::Path::as_os_str)
                    .or(allocation_report_path.as_deref()),
                &emit.allocation_report,
            )?;
            // Host package/description metadata is appended to StateSchema.
            let spec = crate::unified_compile::SourceSpec::Directory(
                std::path::PathBuf::from(dir),
            );
            let artifact_fingerprint = emit.artifact_fingerprint.clone();
            let emits_main = emit.emits_main;
            let lua = install_rock_state_metadata(emit.lua, ws, &spec)
                .map_err(|e| anyhow::anyhow!(render_project_err(project.sources(), &e)))?;
            (lua, c, artifact_fingerprint, emits_main, selected_sim_names)
        };
        // Full rebuilds retain the shape of the session's original artifact.
        let callee_binding = cache.callee_binding();
        let preamble = skull_game_preamble(dir)?;
        crate::compile_measurements::phase("prepare");
        write_full_main(&preamble, &lua, emits_main, main_path, incremental_path)?;
        crate::compile_measurements::phase("write");
        let shape_hashes = skull::type_shape_hashes(project.workspace())
            .map_err(|e| anyhow::anyhow!(e.message))?;
        stats.success();
        Ok(SkullStone {
            project,
            cache,
            preamble,
            renderer_target,
            shape_hashes,
            callee_binding,
            route_profile_enabled: std::env::var_os("ROCK_SKULL_ROUTE_PROFILE").is_some(),
            last_route_profile: None,
            artifact_fingerprint,
            selected_sim_names,
        })
    }

    pub(crate) fn sim_selection_is_full(&self) -> bool { self.selected_sim_names.is_none() }

    pub fn enable_route_profile(&mut self) {
        self.route_profile_enabled = true;
        self.project.enable_route_profile();
    }

    pub fn take_last_route_profile(&mut self) -> Option<SkullRouteProfile> {
        self.last_route_profile.take()
    }

    fn finish_route_profile(
        &mut self,
        started: Option<Instant>,
        mut profile: SkullRouteProfile,
    ) {
        if let Some(started) = started {
            profile.finish(started.elapsed().as_nanos());
            self.last_route_profile = Some(profile);
        } else {
            self.last_route_profile = None;
        }
    }

    pub fn artifact_fingerprint(&self) -> &str {
        &self.artifact_fingerprint
    }

    /// Exact source texts and `embed_string` files that produced a sim
    /// artifact. The test-full compile receipt uses these as its invalidation
    /// boundary; it deliberately does not retain this live compile session.
    pub(crate) fn sim_receipt_inputs(
        &self,
    ) -> (Vec<(String, String)>, Vec<std::path::PathBuf>) {
        let sources = self
            .project
            .sources()
            .files
            .iter()
            .map(|source| {
                let source = source.borrow();
                (source.file.to_id_path(), source.contents.clone())
            })
            .collect();
        let comptime_files = self
            .cache
            .comptime_files()
            .into_iter()
            .map(|path| std::path::PathBuf::from(path.as_str()))
            .collect();
        (sources, comptime_files)
    }

    /// The `[build.vars]` selection this build stamped into its workspace.
    pub(crate) fn comptime_inputs(&self) -> &bone_semantic::inference::ComptimeInputs {
        &self.project.workspace().comptime_inputs
    }

    /// On a watch event: refresh the cached sources (re-read only `changed` off
    /// disk, or fully re-discover if the file SET changed), `update_file` every
    /// source (the Session content-hashes unchanged ones to a no-op), then route
    /// by the worst tier -- any interface/structural edit -> full `main.lua` +
    /// wipe overlay; otherwise the accumulated body edits -> `incremental.lua`
    /// overlay. `changed` is the watcher's changed-path set; pass empty to force a
    /// full re-discover.
    pub fn recompile(&mut self, changed: &[std::path::PathBuf], main_path: &std::path::Path, incremental_path: &std::path::Path) -> Result<SkullReload> {
        let stats = crate::compile_measurements::Scope::begin(self.project.dir());
        let started = self.route_profile_enabled.then(Instant::now);
        self.last_route_profile = None;
        let project_started = self.route_profile_enabled.then(Instant::now);
        let publication = self.project.recompile_for_publication(changed);
        crate::compile_measurements::phase("live-project");
        crate::compile_measurements::known_sources(self.project.sources().files.len());
        let project_call_ns = project_started.map_or(0, |started| started.elapsed().as_nanos());
        let (update, snapshot) = match publication {
            Ok(value) => value,
            Err(error) => {
                let mut profile = SkullRouteProfile::from_live(self.project.take_route_timing());
                profile.live_project_call_ns = project_call_ns;
                self.finish_route_profile(started, profile);
                return Err(error);
            }
        };
        let mut profile = SkullRouteProfile::from_live(self.project.take_route_timing());
        profile.live_project_call_ns = project_call_ns;
        let route_started = self.route_profile_enabled.then(Instant::now);
        let result = self.route_and_emit(
            update,
            main_path,
            incremental_path,
            self.route_profile_enabled.then_some(&mut profile),
        );
        if let Some(started) = route_started {
            profile.route_emit_call_ns += started.elapsed().as_nanos();
        }
        match result {
            Ok(reload) => {
                self.project.commit_body_transaction();
                self.finish_route_profile(started, profile);
                stats.success();
                Ok(reload)
            }
            Err(error) => {
                let rollback_started = self.route_profile_enabled.then(Instant::now);
                let restored = self.project.restore(snapshot);
                let cache_restored = if restored.is_ok() {
                    self.cache.rollback_body_edit(self.project.workspace())
                } else {
                    Ok(())
                };
                profile.add_live(self.project.take_route_timing());
                if let Some(started) = rollback_started {
                    profile.rollback_ns += started.elapsed().as_nanos();
                }
                self.finish_route_profile(started, profile);
                if let Err(restore) = restored {
                    return Err(anyhow::anyhow!(
                        "skull publication rollback failed after disk edit: {restore}; original error: {error}"
                    ));
                }
                if let Err(restore) = cache_restored {
                    return Err(anyhow::anyhow!(
                        "skull cache rollback failed after disk edit: {restore}; original error: {error}"
                    ));
                }
                Err(error)
            }
        }
    }

    /// In-memory edit: replace one source's contents WITHOUT touching disk and
    /// route/emit exactly like a watcher edit. The sim reload-injection path
    /// (`docs/sim-reload-harness-plan.md`): the runner edits a source in memory
    /// and writes only the overlay; the project tree is never mutated. Rejects
    /// a path the build didn't discover -- an edit to an unknown file would
    /// otherwise silently do nothing.
    pub fn apply_edit(&mut self, path: &std::path::Path, new_text: &str, main_path: &std::path::Path, incremental_path: &std::path::Path) -> Result<SkullReload> {
        let stats = crate::compile_measurements::Scope::begin(self.project.dir());
        let started = self.route_profile_enabled.then(Instant::now);
        self.last_route_profile = None;
        let project_started = self.route_profile_enabled.then(Instant::now);
        let publication = self.project.apply_edit_for_publication(path, new_text);
        crate::compile_measurements::phase("live-project");
        crate::compile_measurements::known_sources(self.project.sources().files.len());
        let project_call_ns = project_started.map_or(0, |started| started.elapsed().as_nanos());
        let (update, snapshot) = match publication {
            Ok(value) => value,
            Err(error) => {
                let mut profile = SkullRouteProfile::from_live(self.project.take_route_timing());
                profile.live_project_call_ns = project_call_ns;
                self.finish_route_profile(started, profile);
                return Err(error);
            }
        };
        let mut profile = SkullRouteProfile::from_live(self.project.take_route_timing());
        profile.live_project_call_ns = project_call_ns;
        let route_started = self.route_profile_enabled.then(Instant::now);
        let result = self.route_and_emit(
            update,
            main_path,
            incremental_path,
            self.route_profile_enabled.then_some(&mut profile),
        );
        if let Some(started) = route_started {
            profile.route_emit_call_ns += started.elapsed().as_nanos();
        }
        match result {
            Ok(reload) => {
                self.project.commit_body_transaction();
                self.finish_route_profile(started, profile);
                stats.success();
                Ok(reload)
            },
            Err(error) => {
                let rollback_started = self.route_profile_enabled.then(Instant::now);
                let restored = self.project.restore(snapshot);
                let cache_restored = if restored.is_ok() {
                    self.cache.rollback_body_edit(self.project.workspace())
                } else {
                    Ok(())
                };
                profile.add_live(self.project.take_route_timing());
                if let Some(started) = rollback_started {
                    profile.rollback_ns += started.elapsed().as_nanos();
                }
                self.finish_route_profile(started, profile);
                if let Err(restore) = restored {
                    return Err(anyhow::anyhow!(
                        "skull publication rollback failed after in-memory edit: {restore}; original error: {error}"
                    ));
                }
                if let Err(restore) = cache_restored {
                    return Err(anyhow::anyhow!(
                        "skull cache rollback failed after in-memory edit: {restore}; original error: {error}"
                    ));
                }
                Err(error)
            }
        }
    }

    /// The current in-session text of a source (reflects prior `apply_edit`s,
    /// which never touch disk). None if the path isn't a discovered source.
    pub fn source_text(&self, path: &std::path::Path) -> Option<String> {
        self.project.source_text(path)
    }

    /// Package roots retained by the source discovery used by this session.
    pub fn package_roots(&self) -> &[(u32, std::path::PathBuf)] {
        &self.project.sources().package_roots
    }

    /// Absolute paths of every `embed_string` input baked into the current
    /// program -- files the stone watcher must treat as sources: an edit to
    /// one routes as a changed non-source path, which forces the full rebuild
    /// that re-reads it (docs/comptime-plan.md).
    pub fn comptime_watch_files(&self) -> Vec<std::path::PathBuf> {
        // File ids are already the absolute canonical path the resolver read.
        self.cache
            .comptime_files()
            .into_iter()
            .map(|abs| std::path::PathBuf::from(abs.as_str()))
            .collect()
    }

    /// Feed source edits into the Session and route by the worst tier. Body-only
    /// edits write `incremental.lua`; a full artifact is either written or
    /// returned as `FullRequired` when a safety gate defers publication.
    /// Shared by the disk-driven watcher and in-memory sim reload paths.
    /// `t_cook` is the caller's front-end timer (includes its feed construction).
    fn route_and_emit(
        &mut self,
        update: crate::live_project::LiveUpdate,
        main_path: &std::path::Path,
        incremental_path: &std::path::Path,
        mut profile: Option<&mut SkullRouteProfile>,
    ) -> Result<SkullReload> {
        use std::path::PathBuf;
        let (full, reinferred, span_edits, cook_ms) = match update {
            crate::live_project::LiveUpdate::NoChange => return Ok(SkullReload::NoChange),
            crate::live_project::LiveUpdate::BodyOnly { reinferred, span_edits, cook_ms } => {
                (false, reinferred, span_edits, cook_ms)
            }
            crate::live_project::LiveUpdate::Full { cook_ms } => {
                (true, Vec::new(), Vec::new(), cook_ms)
            }
        };
        let t_emit = Instant::now();
        let shape_started = profile.is_some().then(Instant::now);
        let old_shader_contracts = self.cache.shader_registration_contracts();
        let shader_keys_changed = if full {
            self.cache
                .shader_registration_keys_changed(self.project.workspace())
                .map_err(|e| anyhow::anyhow!("{}", render_project_err(self.project.sources(), &e)))?
        } else {
            self.cache
                .shader_registration_keys_changed_for_bodies(
                    self.project.workspace(),
                    &reinferred,
                )
                .map_err(|e| anyhow::anyhow!("{}", render_project_err(self.project.sources(), &e)))?
        };
        if shader_keys_changed {
            if let (Some(started), Some(profile)) = (shape_started, profile.as_deref_mut()) {
                profile.publication_shape_checks_ns += started.elapsed().as_nanos();
            }
            return Ok(SkullReload::FullRequired {
                reasons: vec!["shader registration key set changed".to_string()],
            });
        }
        if full {
            // Schema-drift gate (docs/hot-reload-schema-drift-plan.md): live
            // data outlives every reload tier, so if any type's runtime layout
            // moved, hot-reloading the recompiled program would drive old-shape
            // state with new-shape readers. Request a complete reconstruction
            // and defer ALL writes until the restart path. Re-baseline the stored hashes now: the
            // delta is consumed by this one full publication.
            let new_shapes = skull::type_shape_hashes(self.project.workspace())
                .map_err(|e| anyhow::anyhow!(e.message))?;
            let drifted = skull::shape_diff(&self.shape_hashes, &new_shapes);
            if !drifted.is_empty() {
                self.shape_hashes = new_shapes;
                return Ok(SkullReload::ShapeChanged { types: drifted });
            }
            let lua = {
                let ws = self.project.workspace();
                let mid = skull::user_module_id(ws)
                    .map_err(no_user_module)?;
                let (emit, c) = skull::mir_emit::emit_full_in_for_renderer_with_callee_binding(
                    ws,
                    mid,
                    Some(self.project.dir()),
                    self.renderer_target,
                    Some(self.callee_binding),
                )
                    .map_err(|e| anyhow::anyhow!("{}", render_project_err(self.project.sources(), &e)))?;
                crate::compile_measurements::emission(&emit);
                if old_shader_contracts != c.shader_registration_contracts() {
                    return Ok(SkullReload::FullRequired {
                        reasons: vec!["shader registration ABI changed".to_string()],
                    });
                }
                if self
                    .cache
                    .state_schema()
                    .is_some_and(|old| {
                        c.state_schema()
                            .is_some_and(|new| old.schema_id != new.schema_id)
                    })
                {
                    return Ok(SkullReload::FullRequired {
                        reasons: vec!["StateSchema changed; fresh Session is required".to_string()],
                    });
                }
                if self.cache.lifecycle_plan().is_some_and(|old| {
                    old.removed_gated_wire(&emit.lifecycle_plan)
                }) {
                    return Ok(SkullReload::FullRequired {
                        reasons: vec![
                            "removing a gated @wire requires a fresh Session/HostProjection".to_string(),
                        ],
                    });
                }
                // StateSchema metadata is regenerated on a structural edit.
                let spec =
                    crate::unified_compile::SourceSpec::Directory(PathBuf::from(self.project.dir()));
                let emits_main = emit.emits_main;
                let lua = install_rock_state_metadata(emit.lua, ws, &spec)
                    .map_err(|e| anyhow::anyhow!(render_project_err(self.project.sources(), &e)))?;
                ((lua, emits_main), c)
            };
            self.preamble = skull_game_preamble(self.project.dir())?;
            let emit_ms = t_emit.elapsed().as_secs_f64() * 1000.0;
            let t_write = Instant::now();
            let ((lua, emits_main), staged_cache) = lua;
            let bytes =
                write_full_main(&self.preamble, &lua, emits_main, main_path, incremental_path)?;
            crate::compile_measurements::phase("write");
            self.cache = staged_cache;
            self.artifact_fingerprint = self
                .cache
                .artifact_fingerprint()
                .ok_or_else(|| anyhow::anyhow!("compiler did not publish an Artifact fingerprint"))?
                .to_string();
            let write_ms = t_write.elapsed().as_secs_f64() * 1000.0;
            Ok(SkullReload::Full { bytes, cook_ms, emit_ms, write_ms })
        } else {
            if let (Some(started), Some(profile)) = (shape_started, profile.as_deref_mut()) {
                profile.publication_shape_checks_ns += started.elapsed().as_nanos();
            }
            let checkpoint_started = profile.is_some().then(Instant::now);
            self.cache
                .begin_body_edit_transaction(&reinferred, &span_edits);
            if let Some(profile) = profile.as_deref_mut() {
                profile.body_transactions_started += 1;
            }
            if let (Some(started), Some(profile)) = (checkpoint_started, profile.as_deref_mut()) {
                profile.transaction_checkpoint_ns += started.elapsed().as_nanos();
            }
            let emit_started = profile.is_some().then(Instant::now);
            let overlay = {
                let ws = self.project.workspace();
                skull::mir_emit::emit_overlay_with_span_edits(
                    ws,
                    &mut self.cache,
                    &reinferred,
                    &span_edits,
                )
                    .map_err(|e| anyhow::anyhow!("{}", render_project_err(self.project.sources(), &e)))?
            };
            crate::compile_measurements::emission(&overlay);
            if let (Some(started), Some(profile)) = (emit_started, profile.as_deref_mut()) {
                profile.emit_overlay_ns += started.elapsed().as_nanos();
                profile.affected_chunk_selection_ns +=
                    emit_phase_ns(&overlay.phases, "  affected selection");
            }
            if old_shader_contracts != self.cache.shader_registration_contracts() {
                self.cache.commit_body_edit_transaction();
                return Ok(SkullReload::FullRequired {
                    reasons: vec!["shader registration ABI changed".to_string()],
                });
            }
            if self.cache.body_edit_baseline_lifecycle_plan().is_some_and(|old| {
                old.structure_changed(
                    self.cache
                        .lifecycle_plan()
                        .expect("overlay always carries a LifecyclePlan"),
                )
            }) {
                let reason = if self.cache.body_edit_baseline_lifecycle_plan().is_some_and(|old| {
                    old.removed_gated_wire(
                        self.cache
                            .lifecycle_plan()
                            .expect("overlay always carries a LifecyclePlan"),
                    )
                }) {
                    "removing a gated @wire requires a fresh Session/HostProjection"
                } else {
                    "LifecyclePlan structure changed; full artifact transition required"
                };
                self.cache.commit_body_edit_transaction();
                return Ok(SkullReload::FullRequired {
                    reasons: vec![reason.to_string()],
                });
            }
            let bytes = overlay.lua.len();
            let emit_ms = t_emit.elapsed().as_secs_f64() * 1000.0;
            let t_write = Instant::now();
            rock_host::project::write_atomic(incremental_path, &overlay.lua)?;
            crate::compile_measurements::phase("write");
            if let Some(profile) = profile.as_deref_mut() {
                profile.atomic_output_write_ns += t_write.elapsed().as_nanos();
            }
            self.artifact_fingerprint = self
                .cache
                .artifact_fingerprint()
                .ok_or_else(|| anyhow::anyhow!("compiler did not publish an Artifact fingerprint"))?
                .to_string();
            let commit_started = profile.is_some().then(Instant::now);
            self.cache.commit_body_edit_transaction();
            if let (Some(started), Some(profile)) = (commit_started, profile.as_deref_mut()) {
                profile.transaction_commit_ns += started.elapsed().as_nanos();
            }
            let write_ms = t_write.elapsed().as_secs_f64() * 1000.0;
            Ok(SkullReload::Incremental { bytes, reinferred: reinferred.len(), cook_ms, emit_ms, write_ms })
        }
    }

    /// Canonicalize to a single-file program: `emit_full` from the CURRENT
    /// session workspace -> `main.lua`, and wipe `incremental.lua`. Same effect
    /// as the full-rebuild branch of `recompile`, but driven by a caller rather
    /// than an edit tier and with no re-discovery (the session is already
    /// current).
    ///
    /// Used on the stone restart path: a process restart (libstone/compiler
    /// rebuild) re-spawns luajit, which loads `main.lua` only -- it never
    /// dual-loads the accumulated `incremental.lua` the way the live hot-reload
    /// watcher does. Folding the overlay back into `main.lua` before the restart
    /// means the new process gets the complete current program. The overlay
    /// stays a within-process, single-consumer artifact that never has to cross
    /// a process boundary (`docs/skull-incremental-emit-split.md`).
    pub fn force_full(&mut self, main_path: &std::path::Path, incremental_path: &std::path::Path) -> Result<()> {
        let stats = crate::compile_measurements::Scope::begin(self.project.dir());
        crate::compile_measurements::known_sources(self.project.sources().files.len());
        // A recompile that hit a type error bails AFTER `update_file` has already
        // mutated the session into the broken state, so the restart-path fold must
        // re-gate or it would write a partial `main.lua` to the respawned process.
        self.project.verdict("on restart fold")?;
        let (lua, cache, artifact_fingerprint, emits_main) = {
            let ws = self.project.workspace();
            let mid = skull::user_module_id(ws)
                .map_err(no_user_module)?;
            let (emit, c) = skull::mir_emit::emit_full_in_for_renderer_with_callee_binding(
                ws,
                mid,
                Some(self.project.dir()),
                self.renderer_target,
                Some(self.callee_binding),
            )
                .map_err(|e| anyhow::anyhow!("skull force_full emit: {}", e.message))?;
            crate::compile_measurements::emission(&emit);
            let artifact_fingerprint = emit.artifact_fingerprint.clone();
            let emits_main = emit.emits_main;
            // Host package/description metadata is rebuilt with the StateSchema.
            let spec = crate::unified_compile::SourceSpec::Directory(
                std::path::PathBuf::from(self.project.dir()),
            );
            let lua = install_rock_state_metadata(emit.lua, ws, &spec)
                .map_err(|e| anyhow::anyhow!(e.message))?;
            (lua, c, artifact_fingerprint, emits_main)
        };
        self.preamble = skull_game_preamble(self.project.dir())?;
        crate::compile_measurements::phase("prepare");
        write_full_main(&self.preamble, &lua, emits_main, main_path, incremental_path)?;
        crate::compile_measurements::phase("write");
        self.artifact_fingerprint = artifact_fingerprint;
        self.cache = cache; // resets changed_since_full -- the overlay is now folded in
        // This program is what the (re)spawned process loads -- re-baseline the
        // shape hashes to it, including drift that arrived via a recompile that
        // bailed on a type error after mutating the session.
        self.shape_hashes = skull::type_shape_hashes(self.project.workspace())
            .map_err(|e| anyhow::anyhow!(e.message))?;
        stats.success();
        Ok(())
    }
}

#[cfg(test)]
#[path = "../tests/stream_write/mod.rs"]
mod stream_write_tests;
