//! Cross-run cache of sim compile receipts.
//!
//! A `rock sim` run compiles in a child `rock sim --list` that publishes a
//! receipt (`main.lua` plus its manifest). This keeps that receipt under a key
//! of everything that can change the emitted chunk or the sim registry, so a
//! later run with the same inputs loads it instead of compiling. A wrong hit is
//! worse than a miss: the key covers every input the compile reads, a hit is
//! revalidated like any receipt, and any doubt recompiles.
//!
//! Key: the compiler binary's hash, backend, receipt context (project, renderer,
//! callee shape, codegen environment), normalized sim selection, stdlib dir and contents, the game
//! preamble, every non-volatile `[build.vars]` value, the environment the
//! compile can read, and every discovered source and package manifest.
//! `embed_string` files are known only after a compile, so a hit rehashes the
//! ones its receipt names. Volatile build vars (build clocks) are excluded by
//! their own contract: they may vary without changing the artifact's identity,
//! so a hit keeps the clock of the compile that produced it.
//!
//! Concurrency: a miss locks its key before taking a compile slot, rechecks
//! the cache, then compiles into a private staging dir and publishes with one
//! directory rename; readers only see complete entries. Eviction
//! renames an entry away before deleting it, and a reader that loses its entry
//! mid-load falls back to compiling.

use super::*;
use std::fs::File;

const CACHE_ENV: &str = "ROCK_SIM_COMPILE_CACHE";
const CACHE_DIR_ENV: &str = "ROCK_SIM_COMPILE_CACHE_DIR";
const CACHE_MAX_MB_ENV: &str = "ROCK_SIM_COMPILE_CACHE_MAX_MB";
const DEFAULT_MAX_MB: u64 = 4096;
const KEY_SCHEMA: u64 = 2;
const STAGING_PREFIX: &str = ".staging-";
const EVICTING_PREFIX: &str = ".evicting-";
/// A staging dir older than this belongs to a run that died mid-compile.
const STALE_STAGING: Duration = Duration::from_secs(6 * 60 * 60);

/// The cache root, or `None` when `ROCK_SIM_COMPILE_CACHE=0` turns it off or
/// `rock_exe`, the compiler that keys and fills it, cannot be hashed.
/// Defaults to the host work root's `tmp/rock-sim-compile-cache`, sharing one bounded
/// store on disk rather than the small `/tmp`.
pub(super) fn root(rock_exe: &Path) -> Result<Option<PathBuf>> {
    if std::env::var_os(CACHE_ENV).is_some_and(|value| value == "0")
        || crate::compile_cache::compile_cache_compiler_sha256(rock_exe).is_none()
    {
        return Ok(None);
    }
    if let Some(dir) = std::env::var_os(CACHE_DIR_ENV).filter(|value| !value.is_empty()) {
        return Ok(Some(PathBuf::from(dir)));
    }
    let home = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow!("no HOME for the sim compile cache; set {CACHE_DIR_ENV} or {CACHE_ENV}=0")
        })?;
    Ok(Some(
        crate::host_paths::local_work_root(Path::new(&home))?
            .join("tmp")
            .join("rock-sim-compile-cache"),
    ))
}

fn budget_bytes() -> Result<u64> {
    let mb = match std::env::var(CACHE_MAX_MB_ENV) {
        Ok(value) => value
            .parse::<u64>()
            .with_context(|| format!("{CACHE_MAX_MB_ENV}={value:?} is not a whole number of MB"))?,
        Err(std::env::VarError::NotPresent) => DEFAULT_MAX_MB,
        Err(error) => return Err(anyhow!("{CACHE_MAX_MB_ENV}: {error}")),
    };
    Ok(mb * 1024 * 1024)
}

/// What the compile read beyond the receipt's context and source inputs. The
/// compile child records it with the build vars it stamped; a lookup computes
/// it from the current state.
pub(super) fn identity(
    project: &Path,
    backend: crate::Backend,
    comptime: &bone_semantic::inference::ComptimeInputs,
    rock_exe: &Path,
) -> Result<serde_json::Value> {
    crate::compile_cache::compile_cache_identity(project, backend, comptime, rock_exe)
}

fn key(
    identity: &serde_json::Value,
    context: &serde_json::Value,
    inputs: &[SimReceiptInput],
    selection: &[String],
) -> String {
    let keyed = inputs
        .iter()
        .filter(|input| input.kind != "embedded")
        .cloned()
        .collect::<Vec<_>>();
    let canonical = serde_json::json!([
        KEY_SCHEMA,
        identity,
        context,
        receipt_inputs_json(&keyed),
        selection
    ]);
    crate::compile_cache::cache_sha256_bytes(canonical.to_string().as_bytes())
}

/// The key a stored receipt records, with the identity it was compiled under.
fn manifest_key(receipt_dir: &Path) -> Result<(String, serde_json::Value)> {
    let path = receipt_dir.join("receipt.json");
    let manifest: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&path).with_context(|| format!("read {}", path.display()))?,
    )
    .with_context(|| format!("parse {}", path.display()))?;
    let field = |name: &str| {
        manifest
            .get(name)
            .ok_or_else(|| anyhow!("sim compile receipt has no `{name}`"))
    };
    let inputs = parse_receipt_inputs(field("inputs")?)?;
    let identity = field("cache_identity")?;
    let selection: Vec<String> = serde_json::from_value(field("selection")?.clone())
        .context("sim compile receipt selection is malformed")?;
    Ok((
        key(identity, field("context")?, &inputs, &selection),
        identity.clone(),
    ))
}

/// The current state's key, with the context and inputs a hit validates against.
struct Lookup {
    key: String,
    identity: serde_json::Value,
    current: CurrentReceiptState,
}

impl Lookup {
    fn current(
        backend: crate::Backend,
        project: &Path,
        rock_exe: &Path,
        selection: &[String],
    ) -> Result<Self> {
        // Resolve the build vars as the `rock sim --list` child will, even
        // from a test harness that is no `rock sim` invocation.
        let comptime = crate::comptime::inputs_for_project_dir_as(
            project,
            Some(crate::Operation::Sim.command().cli_name()),
        )?;
        let identity = identity(project, backend, &comptime, rock_exe)?;
        let current = CurrentReceiptState::of(project)?;
        let key = key(&identity, &current.context, &current.inputs, selection);
        Ok(Self {
            key,
            identity,
            current,
        })
    }
}

/// `prepare_sims` through the cache: load the entry for the current inputs,
/// or compile in a child `rock_exe` and publish its receipt for later runs.
pub(super) fn prepare(
    backend: crate::Backend,
    project: &Path,
    output: &Path,
    root: &Path,
    rock_exe: &Path,
    selection: &[String],
) -> Result<PreparedSims> {
    let budget = budget_bytes()?;
    std::fs::create_dir_all(root)
        .with_context(|| format!("create sim compile cache {}", root.display()))?;
    let selection = normalized_sim_selection(selection);
    let full_lookup = timed_sim_compile_phase(
        "compile_sims",
        "cache_lookup",
        project,
        Some(output),
        || Lookup::current(backend, project, rock_exe, &[]),
    )?;
    if !selection.is_empty() {
        match load_cached(&root.join(&full_lookup.key), &full_lookup, project, output) {
            Ok(Some(prepared)) => return Ok(prepared),
            Ok(None) => {}
            Err(error) => {
                ::log::warn!(target: "SIM", "full sim compile cache entry is unusable: {error:#}")
            }
        }
    }
    let lookup = Lookup {
        key: key(
            &full_lookup.identity,
            &full_lookup.current.context,
            &full_lookup.current.inputs,
            &selection,
        ),
        identity: full_lookup.identity.clone(),
        current: full_lookup.current.clone(),
    };
    let short = &lookup.key[..16];
    let entry = root.join(&lookup.key);
    match load_cached(&entry, &lookup, project, output) {
        Ok(Some(prepared)) => return Ok(prepared),
        Ok(None) => ::log::info!(target: "SIM", "compile cache miss {short}"),
        Err(error) => {
            ::log::warn!(target: "SIM", "compile cache entry {short} is unusable, recompiling: {error:#}")
        }
    }

    // The child takes its compile slot only after this lock; same-key waiters
    // must never occupy slots needed by the holder.
    let _key_lock = lock_key(root, &lookup.key)?;
    if !selection.is_empty() {
        match load_cached(&root.join(&full_lookup.key), &full_lookup, project, output) {
            Ok(Some(prepared)) => return Ok(prepared),
            Ok(None) => {}
            Err(error) => {
                ::log::warn!(target: "SIM", "full sim compile cache entry is unusable: {error:#}")
            }
        }
    }
    match load_cached(&entry, &lookup, project, output) {
        Ok(Some(prepared)) => return Ok(prepared),
        Ok(None) => {}
        Err(error) => {
            ::log::warn!(target: "SIM", "compile cache entry {short} is unusable, recompiling: {error:#}");
            evict(root, &entry).with_context(|| format!("evict unusable sim compile {short}"))?;
        }
    }

    let staging = tempfile::Builder::new()
        .prefix(STAGING_PREFIX)
        .tempdir_in(root)
        .with_context(|| format!("stage a sim compile under {}", root.display()))?;
    compile_receipt_in_child(backend, project, rock_exe, staging.path(), &selection)?;
    let receipt = sim_receipt_project_dir(staging.path(), project);
    let prepared = consume_sim_receipt(&receipt, project, output, None)?;
    let (compiled_key, _) = manifest_key(&receipt)?;
    let (entry, publish_lookup) = if compiled_key == full_lookup.key {
        (root.join(&full_lookup.key), &full_lookup)
    } else {
        (entry, &lookup)
    };
    match publish(&receipt, &entry, publish_lookup) {
        Ok(()) => {
            if let Err(error) = evict_to_budget(root, budget, &entry) {
                ::log::warn!(target: "SIM", "compile cache: eviction failed: {error:#}");
            }
        }
        Err(error) => ::log::info!(target: "SIM", "compile cache: not storing {short}: {error:#}"),
    }
    Ok(prepared)
}

fn load_cached(
    entry: &Path,
    lookup: &Lookup,
    project: &Path,
    output: &Path,
) -> Result<Option<PreparedSims>> {
    if !entry.is_dir() {
        return Ok(None);
    }
    let (stored, _) = manifest_key(entry)?;
    if stored != lookup.key {
        return Err(anyhow!("its manifest keys to {stored}"));
    }
    let prepared = consume_sim_receipt(entry, project, output, Some(&lookup.current))?;
    let short = &lookup.key[..16];
    if let Err(error) = mark_used(entry) {
        ::log::warn!(target: "SIM", "compile cache: cannot mark {short} used: {error:#}");
    }
    ::log::info!(target: "SIM", "compile cache hit {short}");
    Ok(Some(prepared))
}

fn lock_key(root: &Path, key: &str) -> Result<File> {
    crate::compile_cache::compile_cache_lock_key(root, key, "SIM", "sim program")
}

/// Store a freshly compiled receipt under `key` only if what it records
/// matches what the lookup saw: an input that changed during the compile
/// leaves this run correct but uncached.
fn publish(receipt: &Path, entry: &Path, lookup: &Lookup) -> Result<()> {
    let (compiled, identity) = manifest_key(receipt)?;
    if compiled != lookup.key {
        const FIELDS: [&str; 5] = [
            "compiler_sha256",
            "stdlib",
            "preamble_sha256",
            "build_vars_sha256",
            "environment_sha256",
        ];
        let changed = FIELDS
            .into_iter()
            .filter(|field| identity.get(field) != lookup.identity.get(field))
            .collect::<Vec<_>>();
        if changed == ["build_vars_sha256"] {
            return Err(anyhow!(
                "[build.vars] values changed while compiling (a clock var not declared \
                 `volatile = true` changes on every run)"
            ));
        }
        let changed = if changed.is_empty() {
            "sources or context".to_string()
        } else {
            changed.join(", ")
        };
        return Err(anyhow!("inputs changed while compiling ({changed})"));
    }
    crate::compile_cache::compile_cache_publish_dir(receipt, entry, "receipt.json")
}

fn mark_used(entry: &Path) -> Result<()> {
    crate::compile_cache::compile_cache_mark_used(entry, "receipt.json")
}

/// Rename first so a concurrent lookup sees the entry gone, never half-deleted.
fn evict(root: &Path, entry: &Path) -> Result<()> {
    crate::compile_cache::compile_cache_evict(root, entry, EVICTING_PREFIX)
}

/// Least recently used entries go first; `keep` (the entry this run just
/// published) never does. Also clears staging dirs of runs that died.
fn evict_to_budget(root: &Path, budget: u64, keep: &Path) -> Result<()> {
    crate::compile_cache::compile_cache_evict_to_budget(
        root,
        budget,
        keep,
        "receipt.json",
        STAGING_PREFIX,
        EVICTING_PREFIX,
        STALE_STAGING,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_storage_sim_cache_default() {
        crate::host_paths::assert_cache_default(
            "run_sim::sim_compile_cache::tests::host_storage_sim_cache_default",
            "rock-sim-compile-cache",
            || root(&std::env::current_exe().unwrap()),
        );
    }
    use std::fs::TryLockError;
    use std::io::Write;

    #[test]
    fn key_lock_holder_process() {
        let Some(root) = std::env::var_os("SIM_KEY_LOCK_TEST_ROOT") else {
            return;
        };
        let _held = lock_key(Path::new(&root), "test-key").unwrap();
        println!("locked");
        std::io::stdout().flush().unwrap();
        loop {
            std::thread::park();
        }
    }

    #[test]
    fn key_lock_recovers_after_holder_dies_without_unlinking_waiters() {
        use std::io::BufRead;
        let root = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "run_sim::sim_compile_cache::tests::key_lock_holder_process",
                "--exact",
                "--nocapture",
            ])
            .env("SIM_KEY_LOCK_TEST_ROOT", root.path())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        while !line.contains("locked") {
            line.clear();
            assert!(
                stdout.read_line(&mut line).unwrap() > 0,
                "holder exited before locking"
            );
        }
        let path = root.path().join("test-key.lock");
        let waiter = File::options().read(true).write(true).open(&path).unwrap();
        assert!(matches!(waiter.try_lock(), Err(TryLockError::WouldBlock)));
        let other_key = lock_key(root.path(), "other-key").unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        waiter.try_lock().expect("death releases the kernel lock");
        waiter.unlock().unwrap();
        let recovered = lock_key(root.path(), "test-key").unwrap();
        assert!(
            matches!(waiter.try_lock(), Err(TryLockError::WouldBlock)),
            "the existing waiter sees the same inode"
        );
        assert_eq!(
            std::fs::read_to_string(path).unwrap().trim(),
            std::process::id().to_string()
        );
        drop(recovered);
        drop(other_key);
    }

    fn entry(root: &Path, name: &str, bytes: usize, age_secs: u64) -> PathBuf {
        let path = root.join(name);
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("main.lua"), vec![b'x'; bytes]).unwrap();
        std::fs::write(path.join("receipt.json"), b"{}").unwrap();
        std::fs::File::options()
            .write(true)
            .open(path.join("receipt.json"))
            .unwrap()
            .set_modified(std::time::SystemTime::now() - Duration::from_secs(age_secs))
            .unwrap();
        path
    }

    #[test]
    fn eviction_removes_least_recently_used_entries_but_never_the_new_one() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let oldest = entry(root, "a", 1000, 300);
        let older = entry(root, "b", 1000, 200);
        let newest_used = entry(root, "c", 1000, 10);
        let just_published = entry(root, "d", 1000, 400);
        std::fs::create_dir(root.join(format!("{EVICTING_PREFIX}left"))).unwrap();
        let live_staging = root.join(format!("{STAGING_PREFIX}live"));
        std::fs::create_dir(&live_staging).unwrap();

        evict_to_budget(root, 2100, &just_published).unwrap();

        assert!(
            !oldest.exists() && !older.exists(),
            "two least recently used entries go"
        );
        assert!(newest_used.exists() && just_published.exists());
        assert!(
            live_staging.exists(),
            "a running compile's staging dir stays"
        );
        let names = std::fs::read_dir(root)
            .unwrap()
            .map(|item| item.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            names,
            ["c", "d", ".staging-live"]
                .map(String::from)
                .into_iter()
                .collect()
        );
    }

    #[test]
    fn eviction_sweeps_unheld_key_locks_without_entries() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let kept = entry(root, "a", 1000, 10);
        entry(root, "b", 1000, 300);
        for name in ["a", "b", "orphan"] {
            std::fs::write(root.join(format!("{name}.lock")), b"1\n").unwrap();
        }
        let held = lock_key(root, "held").unwrap();

        evict_to_budget(root, 1500, &kept).unwrap();

        assert!(root.join("a.lock").exists(), "a live entry keeps its lock");
        assert!(root.join("held.lock").exists(), "a held lock stays");
        assert!(
            !root.join("b.lock").exists(),
            "an evicted entry's lock goes"
        );
        assert!(
            !root.join("orphan.lock").exists(),
            "an unheld lock with no entry goes"
        );
        drop(held);
    }

    #[test]
    fn runner_only_environment_is_unkeyed_and_compile_settings_are_keyed() {
        for name in [
            "ROCK_SIM_TIMEOUT_SECS",
            "ROCK_FRAME_TIMEOUT_SECS",
            "ROCK_FRAME_WATCHDOG",
            "ROCK_PERF_STARTUP_WATCHDOG",
            "ROCK_FRAME_COMPILE_CACHE",
            "ROCK_FRAME_COMPILE_CACHE_DIR",
            "ROCK_FRAME_COMPILE_CACHE_MAX_MB",
            "ROCK_FRAME_COMPILE_CACHE_STATS",
            "ROCK_BUILD_COMMAND",
            "ROCK_SKULL_ROUTE_PROFILE",
            "ROCK_RUN_SIM",
            "ROCK_TEST_SEED",
            "ROCK_NUM_THREADS",
            "ROCK_HEADLESS_SCOPE",
            "FRAME_COUNT",
            "TAKE_SCREENSHOT",
            "STONE_HEADLESS",
            "ROCK_JOB_ID",
            "ROCK_COMPILE_STATS",
            "ROCK_COMPILE_ADMISSION",
            "ROCK_INVOCATION_CONTEXT_KEYS",
            "ROCK_COMPILE_SLOTS",
            "ROCK_COMPILE_CLASS",
            "ROCK_COMPILE_SLOT_DIR",
            "ROCK_JOBS_SELF",
            "ROCK_SIM_WARN_FACTOR",
            "ROCK_SIM_WORKER_SLOTS",
            "ROCK_SIM_WORKER_SLOT_DIR",
            "ROCK_SIM_WORKER_WAIT_LOG",
            "STONE_JV",
            CACHE_DIR_ENV,
        ] {
            assert!(
                !crate::compile_cache::compile_cache_environment_is_keyed(name),
                "{name}"
            );
        }
        for name in [
            "ROCK_SKULL_TAIL_CALLS",
            "SKULL_INLINE_OFF",
            "ROCK_STDLIB_DIR",
            "GAME_ART_VAULT",
            "ROCK_SIM_NEW",
        ] {
            assert!(
                crate::compile_cache::compile_cache_environment_is_keyed(name),
                "{name}"
            );
        }
        assert!(!crate::compile_cache::compile_cache_environment_is_keyed(
            "PATH"
        ));
    }
}
