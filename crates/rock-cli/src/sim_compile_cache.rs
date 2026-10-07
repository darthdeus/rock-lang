//! Cross-run cache of sim compile receipts (docs/sim-memory-plan.md).
//!
//! A `rock sim` run compiles in a child `rock sim --list` that publishes a
//! receipt (`main.lua` plus its manifest). This keeps that receipt under a key
//! of everything that can change the emitted chunk or the sim registry, so a
//! later run with the same inputs loads it instead of compiling. A wrong hit is
//! worse than a miss: the key covers every input the compile reads, a hit is
//! revalidated like any receipt, and any doubt recompiles.
//!
//! Key: the compiler binary's hash, backend, receipt context (project, renderer,
//! callee shape, codegen environment), stdlib dir and contents, the game
//! preamble, every non-volatile `[build.vars]` value, the environment the
//! compile can read, and every discovered source and package manifest.
//! `embed_string` files are known only after a compile, so a hit rehashes the
//! ones its receipt names. Volatile build vars (build clocks) are excluded by
//! their own contract: they may vary without changing the artifact's identity,
//! so a hit keeps the clock of the compile that produced it.
//!
//! Concurrency: a miss compiles into a private staging dir under the cache
//! root and publishes with one directory rename, which fails when another run
//! already published the key; readers only see complete entries. Eviction
//! renames an entry away before deleting it, and a reader that loses its entry
//! mid-load falls back to compiling.

use super::*;

const CACHE_ENV: &str = "ROCK_SIM_COMPILE_CACHE";
const CACHE_DIR_ENV: &str = "ROCK_SIM_COMPILE_CACHE_DIR";
const CACHE_MAX_MB_ENV: &str = "ROCK_SIM_COMPILE_CACHE_MAX_MB";
const DEFAULT_MAX_MB: u64 = 4096;
const KEY_SCHEMA: u64 = 1;
const STAGING_PREFIX: &str = ".staging-";
const EVICTING_PREFIX: &str = ".evicting-";
/// A staging dir older than this belongs to a run that died mid-compile.
const STALE_STAGING: Duration = Duration::from_secs(6 * 60 * 60);

/// Environment the compile can read: the prefixes cover every variable the
/// compiler crates and the game preamble read. Unknown names under them are
/// keyed, so a new compile setting cannot produce a wrong hit. `STONE_` is the
/// engine's runtime namespace (`--jv`, `STONE_LOG`, `STONE_HEADLESS`); the
/// compile reads only `STONE_LOG`, to time its own passes.
const KEYED_ENV_PREFIXES: &[&str] = &["ROCK_", "SKULL_", "BONE_"];
const KEYED_ENV_NAMES: &[&str] = &["GAME_ART_VAULT", "GAME_SOURCE_DIR", "CARGO_TARGET_DIR", "HOME"];
/// Variables under those prefixes that only the sim runner, its reports or
/// job tooling read; never the compile.
const UNKEYED_ENV: &[&str] = &[
    CACHE_ENV,
    CACHE_DIR_ENV,
    CACHE_MAX_MB_ENV,
    SIM_COMPILE_RECEIPT_DIR_ENV,
    SIM_COMPILE_RECEIPT_PREPARE_ENV,
    "ROCK_JOB_ID",
    // Compile records describe the compile; they never change its output.
    "ROCK_COMPILE_STATS",
    // Slot settings only decide when the compile starts, not what it emits;
    // keying them made interactive and batch runs miss each other's entries.
    crate::compile_slots::SLOTS_ENV,
    crate::compile_slots::CLASS_ENV,
    crate::compile_slots::SLOT_DIR_ENV,
    // Invocation-log settings and tags; never read by the compile.
    "ROCK_INVOCATION_LOG",
    crate::invocation_log::PARENT_ENV,
    "ROCK_CHANGE_ID",
    // rock_log's per-invocation clock origin, inherited by children.
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
];
const UNKEYED_ENV_PREFIXES: &[&str] = &["ROCK_JOBS_", "ROCK_SIM_WARN_"];

/// The cache root, or `None` when `ROCK_SIM_COMPILE_CACHE=0` turns it off.
/// Defaults to `~/tmp/rock-sim-compile-cache` so worktrees share one bounded
/// store on disk rather than the small `/tmp`.
pub(super) fn root() -> Result<Option<PathBuf>> {
    if std::env::var_os(CACHE_ENV).is_some_and(|value| value == "0") {
        return Ok(None);
    }
    if let Some(dir) = std::env::var_os(CACHE_DIR_ENV).filter(|value| !value.is_empty()) {
        return Ok(Some(PathBuf::from(dir)));
    }
    let home = std::env::var_os("HOME").filter(|value| !value.is_empty()).ok_or_else(|| {
        anyhow!("no HOME for the sim compile cache; set {CACHE_DIR_ENV} or {CACHE_ENV}=0")
    })?;
    Ok(Some(PathBuf::from(home).join("tmp").join("rock-sim-compile-cache")))
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

fn compiler_sha256() -> Result<String> {
    static HASH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    if let Some(hash) = HASH.get() {
        return Ok(hash.clone());
    }
    let exe = std::env::current_exe().context("locating the rock binary to key the sim compile")?;
    let mut hasher = Sha256::new();
    std::io::copy(
        &mut std::fs::File::open(&exe).with_context(|| format!("open {}", exe.display()))?,
        &mut hasher,
    )
    .with_context(|| format!("hash {}", exe.display()))?;
    Ok(HASH.get_or_init(|| format!("{:x}", hasher.finalize())).clone())
}

/// The stdlib is read from disk and joins no project's source set.
fn stdlib_digest() -> serde_json::Value {
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

/// Values are hashed, never written: a var may be `secret`.
fn build_vars_sha256(inputs: &bone_semantic::inference::ComptimeInputs) -> String {
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
    sha256_bytes(canonical.to_string().as_bytes())
}

fn environment_is_keyed(name: &str) -> bool {
    (KEYED_ENV_PREFIXES.iter().any(|prefix| name.starts_with(prefix)) || KEYED_ENV_NAMES.contains(&name))
        && !UNKEYED_ENV.contains(&name)
        && !UNKEYED_ENV_PREFIXES.iter().any(|prefix| name.starts_with(prefix))
}

/// Hashed, never written: the environment can hold credentials.
fn environment_sha256() -> String {
    let keyed = std::env::vars_os()
        .filter_map(|(name, value)| {
            let name = name.to_string_lossy().into_owned();
            environment_is_keyed(&name).then(|| (name, value.to_string_lossy().into_owned()))
        })
        .collect::<BTreeMap<_, _>>();
    sha256_bytes(serde_json::json!(keyed).to_string().as_bytes())
}

/// What the compile read beyond the receipt's context and source inputs. The
/// compile child records it with the build vars it stamped; a lookup computes
/// it from the current state.
pub(super) fn identity(
    project: &Path,
    backend: crate::Backend,
    comptime: &bone_semantic::inference::ComptimeInputs,
) -> Result<serde_json::Value> {
    let preamble = crate::skull_backend::skull_game_preamble(&project.to_string_lossy())
        .context("resolve the game preamble for the sim compile key")?;
    Ok(serde_json::json!({
        "compiler_sha256": compiler_sha256()?,
        "backend": backend.name(),
        "stdlib": stdlib_digest(),
        "preamble_sha256": sha256_bytes(preamble.as_bytes()),
        "build_vars_sha256": build_vars_sha256(comptime),
        "environment_sha256": environment_sha256(),
    }))
}

fn key(
    identity: &serde_json::Value,
    context: &serde_json::Value,
    inputs: &[SimReceiptInput],
) -> String {
    let keyed = inputs.iter().filter(|input| input.kind != "embedded").cloned().collect::<Vec<_>>();
    let canonical = serde_json::json!([KEY_SCHEMA, identity, context, receipt_inputs_json(&keyed)]);
    sha256_bytes(canonical.to_string().as_bytes())
}

/// The key a stored receipt records, with the identity it was compiled under.
fn manifest_key(receipt_dir: &Path) -> Result<(String, serde_json::Value)> {
    let path = receipt_dir.join("receipt.json");
    let manifest: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&path).with_context(|| format!("read {}", path.display()))?,
    )
    .with_context(|| format!("parse {}", path.display()))?;
    let field = |name: &str| {
        manifest.get(name).ok_or_else(|| anyhow!("sim compile receipt has no `{name}`"))
    };
    let inputs = parse_receipt_inputs(field("inputs")?)?;
    let identity = field("cache_identity")?;
    Ok((key(identity, field("context")?, &inputs), identity.clone()))
}

/// The current state's key, with the context and inputs a hit validates against.
struct Lookup {
    key: String,
    identity: serde_json::Value,
    current: CurrentReceiptState,
}

impl Lookup {
    fn current(backend: crate::Backend, project: &Path) -> Result<Self> {
        let comptime = crate::comptime::inputs_for_project_dir(project)?;
        let identity = identity(project, backend, &comptime)?;
        let current = CurrentReceiptState::of(project)?;
        let key = key(&identity, &current.context, &current.inputs);
        Ok(Self { key, identity, current })
    }
}

/// `prepare_sims` through the cache: load the entry for the current inputs,
/// or compile in a child and publish its receipt for later runs.
pub(super) fn prepare(
    backend: crate::Backend,
    project: &Path,
    output: &Path,
    root: &Path,
) -> Result<PreparedSims> {
    let budget = budget_bytes()?;
    std::fs::create_dir_all(root)
        .with_context(|| format!("create sim compile cache {}", root.display()))?;
    let lookup = timed_sim_compile_phase("compile_sims", "cache_lookup", project, Some(output), || {
        Lookup::current(backend, project)
    })?;
    let short = &lookup.key[..16];
    let entry = root.join(&lookup.key);
    if entry.is_dir() {
        let loaded = manifest_key(&entry).and_then(|(stored, _)| {
            if stored != lookup.key {
                return Err(anyhow!("its manifest keys to {stored}"));
            }
            consume_sim_receipt(&entry, project, output, Some(&lookup.current))
        });
        match loaded {
            Ok(prepared) => {
                if let Err(error) = mark_used(&entry) {
                    ::log::warn!(target: "SIM", "compile cache: cannot mark {short} used: {error:#}");
                }
                ::log::info!(target: "SIM", "compile cache hit {short}");
                return Ok(prepared);
            }
            Err(error) => {
                ::log::warn!(target: "SIM", "compile cache entry {short} is unusable, recompiling: {error:#}");
                if let Err(error) = evict(root, &entry) {
                    ::log::warn!(target: "SIM", "compile cache: cannot evict {short}: {error:#}");
                }
            }
        }
    } else {
        ::log::info!(target: "SIM", "compile cache miss {short}");
    }

    let staging = tempfile::Builder::new()
        .prefix(STAGING_PREFIX)
        .tempdir_in(root)
        .with_context(|| format!("stage a sim compile under {}", root.display()))?;
    compile_receipt_in_child(backend, project, staging.path())?;
    let receipt = sim_receipt_project_dir(staging.path(), project);
    let prepared = consume_sim_receipt(&receipt, project, output, None)?;
    match publish(&receipt, &entry, &lookup) {
        Ok(()) => {
            if let Err(error) = evict_to_budget(root, budget, &entry) {
                ::log::warn!(target: "SIM", "compile cache: eviction failed: {error:#}");
            }
        }
        Err(error) => ::log::info!(target: "SIM", "compile cache: not storing {short}: {error:#}"),
    }
    Ok(prepared)
}

/// Store a freshly compiled receipt under `key` only if what it records
/// matches what the lookup saw: an input that changed during the compile
/// leaves this run correct but uncached.
fn publish(receipt: &Path, entry: &Path, lookup: &Lookup) -> Result<()> {
    let (compiled, identity) = manifest_key(receipt)?;
    if compiled != lookup.key {
        const FIELDS: [&str; 5] =
            ["compiler_sha256", "stdlib", "preamble_sha256", "build_vars_sha256", "environment_sha256"];
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
        let changed = if changed.is_empty() { "sources or context".to_string() } else { changed.join(", ") };
        return Err(anyhow!("inputs changed while compiling ({changed})"));
    }
    match std::fs::rename(receipt, entry) {
        Ok(()) => Ok(()),
        // Another run published the same key first; both compiled the same inputs.
        Err(_) if entry.join("receipt.json").is_file() => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("publish {} -> {}", receipt.display(), entry.display()))
        }
    }
}

fn mark_used(entry: &Path) -> Result<()> {
    std::fs::File::options()
        .write(true)
        .open(entry.join("receipt.json"))
        .context("open receipt manifest")?
        .set_modified(std::time::SystemTime::now())
        .context("touch receipt manifest")
}

/// Rename first so a concurrent lookup sees the entry gone, never half-deleted.
fn evict(root: &Path, entry: &Path) -> Result<()> {
    let doomed = tempfile::Builder::new()
        .prefix(EVICTING_PREFIX)
        .tempdir_in(root)
        .context("create eviction dir")?
        .keep();
    let target = doomed.join("entry");
    match std::fs::rename(entry, &target) {
        Ok(()) => {}
        // Another run evicted it first.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            let moved = Err(error).with_context(|| format!("move {} aside", entry.display()));
            return remove_evicted(&doomed).and(moved);
        }
    }
    remove_evicted(&doomed)
}

/// Two runs can clear the same leftover eviction dir at once.
fn remove_evicted(doomed: &Path) -> Result<()> {
    match std::fs::remove_dir_all(doomed) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            Err(error).with_context(|| format!("remove {}", doomed.display()))
        }
        _ => Ok(()),
    }
}

fn dir_bytes(dir: &Path) -> Result<u64> {
    let mut total = 0;
    for file in std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        total += file?.metadata()?.len();
    }
    Ok(total)
}

/// Least recently used entries go first; `keep` (the entry this run just
/// published) never does. Also clears staging dirs of runs that died.
fn evict_to_budget(root: &Path, budget: u64, keep: &Path) -> Result<()> {
    let now = std::time::SystemTime::now();
    let mut entries = Vec::new();
    let mut total = 0u64;
    for item in std::fs::read_dir(root).with_context(|| format!("read {}", root.display()))? {
        let item = item?;
        let path = item.path();
        let name = item.file_name().to_string_lossy().into_owned();
        if name.starts_with(EVICTING_PREFIX) {
            remove_evicted(&path)?;
            continue;
        }
        if name.starts_with(STAGING_PREFIX) {
            let age = item.metadata()?.modified()?;
            if now.duration_since(age).unwrap_or_default() > STALE_STAGING {
                std::fs::remove_dir_all(&path)
                    .with_context(|| format!("remove stale staging {}", path.display()))?;
            }
            continue;
        }
        // An entry another run is evicting can vanish between listing and stat.
        let Ok(used) = std::fs::metadata(path.join("receipt.json")).and_then(|meta| meta.modified())
        else {
            continue;
        };
        let Ok(bytes) = dir_bytes(&path) else { continue };
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
        evict(root, &path)?;
        total -= bytes;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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

        assert!(!oldest.exists() && !older.exists(), "two least recently used entries go");
        assert!(newest_used.exists() && just_published.exists());
        assert!(live_staging.exists(), "a running compile's staging dir stays");
        let names = std::fs::read_dir(root)
            .unwrap()
            .map(|item| item.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(names, ["c", "d", ".staging-live"].map(String::from).into_iter().collect());
    }

    #[test]
    fn runner_only_environment_is_unkeyed_and_compile_settings_are_keyed() {
        for name in ["ROCK_SIM_TIMEOUT_SECS", "ROCK_JOB_ID", "ROCK_COMPILE_STATS", "ROCK_COMPILE_SLOTS", "ROCK_COMPILE_CLASS", "ROCK_COMPILE_SLOT_DIR", "ROCK_JOBS_SELF", "ROCK_SIM_WARN_FACTOR", "STONE_JV", CACHE_DIR_ENV] {
            assert!(!environment_is_keyed(name), "{name}");
        }
        for name in ["ROCK_SKULL_TAIL_CALLS", "SKULL_INLINE_OFF", "ROCK_STDLIB_DIR", "GAME_ART_VAULT", "ROCK_SIM_NEW"] {
            assert!(environment_is_keyed(name), "{name}");
        }
        assert!(!environment_is_keyed("PATH"));
    }
}
