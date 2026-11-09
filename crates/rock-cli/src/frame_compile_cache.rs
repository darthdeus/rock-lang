//! One content-addressed full-compilation artifact cache for `rock frame` and
//! `rock stone`. Runtime frame counts, output directories and the private
//! per-session path select execution, never emitted Lua, so both commands share
//! one store, one key definition and one publication path.
//!
//! The key is `[CACHE_SCHEMA, compile_cache_identity, context, inputs]`:
//! - `compile_cache_identity` (`crate::compile_cache`) is the compiler image,
//!   backend, stdlib, game preamble, non-volatile `[build.vars]` values and the
//!   keyed environment;
//! - `context` is the canonical project dir;
//! - `inputs` is every discovered production source plus every `stick.toml` on
//!   the source path, by content.
//!
//! `embed_string` inputs are not in the key: a loaded entry re-hashes them and
//! is rejected when they changed.
//!
//! Volatile build vars (`project/stick.toml`: `BUILD_UNIX`, `BUILD_DATE`; declared
//! `volatile = true`) are identified in the key by NAME only. A hit therefore
//! keeps the values of the compile that produced it -- the same rule the sim
//! compile receipts use (the compile receipt contract) -- because a clock in the key
//! would make every entry miss. What that costs: The project's dev watermark reports the
//! age of that compile, which is the compile the running program IS; a truly
//! per-launch value belongs in the game's runtime, not in `[build.vars]`.
//!
//! `ROCK_FRAME_COMPILE_CACHE=0` or `ROCK_FRAME_COMPILE_CACHE_DIR` (and
//! `..._MAX_MB`, `..._STATS`) configure the one cache for both commands. A
//! requested `SKULL_ALLOCATION_REPORT` disables it, because a hit skips the
//! `IncrementalProject::init` that owns those report files.

use crate::Backend;
use crate::frame_runtime::CompiledGame;
use crate::game_compile::{GameArtifact, compile_game};
use crate::game_runtime::AdoptedArtifact;
use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

const CACHE_ENV: &str = "ROCK_FRAME_COMPILE_CACHE";
const CACHE_DIR_ENV: &str = "ROCK_FRAME_COMPILE_CACHE_DIR";
const CACHE_MAX_MB_ENV: &str = "ROCK_FRAME_COMPILE_CACHE_MAX_MB";
const CACHE_STATS_ENV: &str = "ROCK_FRAME_COMPILE_CACHE_STATS";
const DEFAULT_MAX_MB: u64 = 4096;
const CACHE_SCHEMA: u64 = 1;
const STAGING_PREFIX: &str = ".staging-frame-";
const EVICTING_PREFIX: &str = ".evicting-frame-";
const STALE_STAGING: Duration = Duration::from_secs(6 * 60 * 60);

#[derive(Clone)]
struct Lookup {
    key: String,
    identity: Value,
    context: Value,
    inputs: Value,
    project: PathBuf,
    backend: Backend,
}

/// What a `rock stone` session starts from.
pub(crate) enum SessionStart {
    /// The cache is off (`cache_root`): compile as before and store nothing.
    Off,
    /// The game starts from this artifact, installed as the session's
    /// `main.lua`; the session compiles nothing now.
    Hit(AdoptedArtifact),
    /// No entry: compile as before, then publish through `SessionCompile`.
    Miss(SessionCompile),
}

/// A session compile's place in the cache: the key its artifact would be stored
/// under, resolved before the compile from the same inputs the compile reads.
pub(crate) struct SessionCompile {
    lookup: Lookup,
}

impl Lookup {
    fn current(project: &Path, backend: Backend) -> Result<Self> {
        let comptime = crate::comptime::inputs_for_project_dir(project)?;
        Self::with_comptime(project, backend, &comptime)
    }

    /// The key of what a finished compile actually read: the build vars it
    /// stamped and the sources as they are now. A resolution between lookup and
    /// compile can differ (a `cmd` var), so only an equal key may be stored.
    fn after_compile(&self, artifact: &GameArtifact) -> Result<Self> {
        let comptime = artifact
            .comptime_inputs
            .as_ref()
            .ok_or_else(|| anyhow!("the compiler did not report the build vars it stamped"))?;
        Self::with_comptime(&self.project, self.backend, comptime)
    }

    fn with_comptime(
        project: &Path,
        backend: Backend,
        comptime: &bone_semantic::inference::ComptimeInputs,
    ) -> Result<Self> {
        let project = project
            .canonicalize()
            .with_context(|| format!("canonicalize {}", project.display()))?;
        let identity = crate::compile_cache::compile_cache_identity(
            &project,
            backend,
            comptime,
            &rock_host::process_management::current_executable_image()?,
        )?;
        let inputs = frame_source_inputs(&project)?;
        let context = json!({"project": project.to_string_lossy()});
        let key = cache_key(&identity, &context, &inputs);
        Ok(Self {
            key,
            identity,
            context,
            inputs,
            project,
            backend,
        })
    }
}

fn cache_key(identity: &Value, context: &Value, inputs: &Value) -> String {
    let canonical = json!([CACHE_SCHEMA, identity, context, inputs]);
    crate::compile_cache::cache_sha256_bytes(canonical.to_string().as_bytes())
}

fn nearest_manifest(path: &Path) -> Option<PathBuf> {
    let mut dir = if path.is_dir() { path } else { path.parent()? };
    loop {
        let candidate = dir.join("stick.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
        dir = dir.parent()?;
    }
}

fn frame_source_inputs(project: &Path) -> Result<Value> {
    let spec = crate::unified_compile::SourceSpec::Directory(project.to_path_buf());
    let discovered = crate::unified_compile::discover_sources(&spec)?;
    let sources = discovered
        .production_only(project)
        .map_err(|error| anyhow!(error.to_string()))?;
    let mut inputs = Vec::new();
    let mut manifests = std::collections::BTreeSet::new();
    if let Some(path) = nearest_manifest(project) {
        manifests.insert(path);
    }
    for source in &sources.files {
        let source = source.borrow();
        let source_path = PathBuf::from(source.file.to_id_path())
            .canonicalize()
            .with_context(|| {
                format!(
                    "canonicalize frame compile source {}",
                    source.file.to_id_path()
                )
            })?;
        if let Some(path) = nearest_manifest(&source_path) {
            manifests.insert(path);
        }
        inputs.push(json!({
            "kind": "source",
            "path": source_path.to_string_lossy(),
            "sha256": crate::compile_cache::cache_sha256_bytes(source.contents.as_bytes()),
        }));
    }
    for path in manifests {
        let canonical = path
            .canonicalize()
            .with_context(|| format!("canonicalize {}", path.display()))?;
        let bytes =
            std::fs::read(&canonical).with_context(|| format!("read {}", canonical.display()))?;
        inputs.push(json!({
            "kind": "manifest",
            "path": canonical.to_string_lossy(),
            "sha256": crate::compile_cache::cache_sha256_bytes(&bytes),
        }));
    }
    inputs.sort_by(|left, right| {
        (left["kind"].as_str(), left["path"].as_str())
            .cmp(&(right["kind"].as_str(), right["path"].as_str()))
    });
    Ok(Value::Array(inputs))
}

pub(crate) fn cache_root() -> Result<Option<PathBuf>> {
    // A hit skips IncrementalProject::init, which owns these requested report files.
    if std::env::var_os("SKULL_ALLOCATION_REPORT").is_some()
        || std::env::var_os("SKULL_ALLOCATION_ACTIVATION").is_some()
    {
        return Ok(None);
    }
    if std::env::var_os(CACHE_ENV).is_some_and(|value| value == "0")
        || crate::compile_cache::compile_cache_compiler_sha256(
            &rock_host::process_management::current_executable_image()?,
        )
        .is_none()
    {
        return Ok(None);
    }
    if let Some(dir) = std::env::var_os(CACHE_DIR_ENV).filter(|value| !value.is_empty()) {
        return Ok(Some(PathBuf::from(dir)));
    }
    let home = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow!("no HOME for the frame compile cache; set {CACHE_DIR_ENV} or {CACHE_ENV}=0")
        })?;
    Ok(Some(
        crate::host_paths::local_work_root(Path::new(&home))?
            .join("tmp")
            .join("rock-frame-compile-cache")
            .join("cache"),
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
    mb.checked_mul(1024 * 1024)
        .ok_or_else(|| anyhow!("{CACHE_MAX_MB_ENV} is too large"))
}

fn embedded_inputs(paths: &[PathBuf]) -> Result<Value> {
    let mut inputs = Vec::with_capacity(paths.len());
    for path in paths {
        let canonical = path
            .canonicalize()
            .with_context(|| format!("canonicalize embedded input {}", path.display()))?;
        inputs.push(json!({
            "path": canonical.to_string_lossy(),
            "sha256": crate::compile_cache::cache_sha256_file(&canonical)?,
        }));
    }
    inputs.sort_by(|left, right| left["path"].as_str().cmp(&right["path"].as_str()));
    Ok(Value::Array(inputs))
}

fn validated_embedded_inputs(value: &Value) -> Result<Vec<(PathBuf, u64)>> {
    let rows = value
        .as_array()
        .ok_or_else(|| anyhow!("frame compile manifest embedded_inputs is not an array"))?;
    let mut inputs = Vec::with_capacity(rows.len());
    for row in rows {
        let path = row
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("frame compile manifest embedded input has no path"))?;
        let expected = row
            .get("sha256")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("frame compile manifest embedded input has no sha256"))?;
        let path = Path::new(path);
        let canonical = path
            .canonicalize()
            .with_context(|| format!("resolve embedded input {}", path.display()))?;
        if canonical != path {
            return Err(anyhow!(
                "frame compile manifest embedded input path changed: {}",
                path.display()
            ));
        }
        let bytes = std::fs::read(&canonical)
            .with_context(|| format!("read embedded input {}", canonical.display()))?;
        let actual = crate::compile_cache::cache_sha256_bytes(&bytes);
        if actual != expected {
            return Err(anyhow!(
                "embedded input hash changed: {}",
                canonical.display()
            ));
        }
        let text = String::from_utf8(bytes)
            .with_context(|| format!("embedded input {} is not text", canonical.display()))?;
        // The hash the compile baked, recomputed from the bytes it read: a
        // session starting from this artifact seeds its watcher with it.
        inputs.push((canonical, skull_mir::mir::comptime_value_hash(&text)));
    }
    Ok(inputs)
}

fn manifest_key(manifest: &Value) -> Result<String> {
    let schema = manifest
        .get("schema_version")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("frame compile manifest has no schema_version"))?;
    if schema != CACHE_SCHEMA {
        return Err(anyhow!(
            "unsupported frame compile manifest schema {schema}"
        ));
    }
    let identity = manifest
        .get("compile_identity")
        .ok_or_else(|| anyhow!("frame compile manifest has no compile_identity"))?;
    let context = manifest
        .get("context")
        .ok_or_else(|| anyhow!("frame compile manifest has no context"))?;
    let inputs = manifest
        .get("inputs")
        .ok_or_else(|| anyhow!("frame compile manifest has no inputs"))?;
    Ok(cache_key(identity, context, inputs))
}

fn load_cached(
    entry: &Path,
    lookup: &Lookup,
    output_path: &Path,
) -> Result<Option<AdoptedArtifact>> {
    if !entry.exists() {
        return Ok(None);
    }
    let metadata = std::fs::symlink_metadata(entry)
        .with_context(|| format!("inspect frame compile cache entry {}", entry.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(anyhow!(
            "frame compile cache entry is not a regular directory"
        ));
    }
    let manifest_path = entry.join("manifest.json");
    let manifest_metadata = std::fs::symlink_metadata(&manifest_path)
        .with_context(|| format!("inspect {}", manifest_path.display()))?;
    if !manifest_metadata.file_type().is_file() {
        return Err(anyhow!("frame compile manifest is not a regular file"));
    }
    let manifest: Value = serde_json::from_slice(
        &std::fs::read(&manifest_path)
            .with_context(|| format!("read {}", manifest_path.display()))?,
    )
    .with_context(|| format!("parse {}", manifest_path.display()))?;
    let stored_manifest_sha256 = manifest
        .get("manifest_sha256")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("frame compile manifest has no manifest_sha256"))?;
    let mut unsigned_manifest = manifest.clone();
    unsigned_manifest
        .as_object_mut()
        .ok_or_else(|| anyhow!("frame compile manifest is not an object"))?
        .remove("manifest_sha256");
    let actual_manifest_sha256 = crate::compile_cache::cache_sha256_bytes(
        &serde_json::to_vec(&unsigned_manifest)
            .context("serialize frame compile manifest for verification")?,
    );
    if actual_manifest_sha256 != stored_manifest_sha256 {
        return Err(anyhow!(
            "frame compile manifest hash does not match its contents"
        ));
    }
    let stored_key = manifest
        .get("key")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("frame compile manifest has no key"))?;
    if manifest_key(&manifest)? != stored_key || stored_key != lookup.key {
        return Err(anyhow!(
            "frame compile manifest key does not match its entry"
        ));
    }
    if manifest.get("compile_identity") != Some(&lookup.identity)
        || manifest.get("context") != Some(&lookup.context)
        || manifest.get("inputs") != Some(&lookup.inputs)
    {
        return Err(anyhow!(
            "frame compile manifest inputs do not match the current compile"
        ));
    }
    let main_path = entry.join(rock_host::project::MAIN_LUA_FILENAME);
    let main_metadata = std::fs::symlink_metadata(&main_path)
        .with_context(|| format!("inspect {}", main_path.display()))?;
    if !main_metadata.file_type().is_file() {
        return Err(anyhow!("cached main.lua is not a regular file"));
    }
    let expected_hash = manifest
        .get("main_sha256")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("frame compile manifest has no main_sha256"))?;
    let actual_hash = crate::compile_cache::cache_sha256_file(&main_path)?;
    if actual_hash != expected_hash {
        return Err(anyhow!("cached main.lua hash does not match its manifest"));
    }
    let embedded = manifest
        .get("embedded_inputs")
        .ok_or_else(|| anyhow!("frame compile manifest has no embedded_inputs"))?;
    let embedded_inputs = validated_embedded_inputs(embedded)?;
    let artifact_fingerprint = manifest
        .get("artifact_fingerprint")
        .and_then(Value::as_str)
        .filter(|fingerprint| !fingerprint.is_empty())
        .ok_or_else(|| anyhow!("frame compile manifest has no artifact_fingerprint"))?
        .to_string();
    materialize(&main_path, output_path)?;
    if let Err(error) = crate::compile_cache::compile_cache_mark_used(entry, "manifest.json") {
        log::warn!(target: "FRAME", "cannot update frame compile cache use time: {error:#}");
    }
    Ok(Some(AdoptedArtifact {
        artifact_fingerprint,
        embedded_inputs,
    }))
}

/// Install `source` as the run's `main.lua` and wipe the overlay beside it, as
/// a full compile does.
pub(super) fn materialize(source: &Path, destination: &Path) -> Result<()> {
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    rock_host::project::write_atomic_streamed(destination, |file| {
        std::io::copy(&mut std::fs::File::open(source)?, file)?;
        Ok(())
    })
    .with_context(|| format!("materialize frame artifact at {}", destination.display()))?;
    crate::skull_backend::wipe_overlay(&destination.with_file_name("incremental.lua"))
}

fn write_manifest(
    artifact_dir: &Path,
    lookup: &Lookup,
    fingerprint: &str,
    embedded_files: &[PathBuf],
) -> Result<()> {
    let main_path = artifact_dir.join(rock_host::project::MAIN_LUA_FILENAME);
    let embedded_inputs = embedded_inputs(embedded_files)?;
    let manifest = json!({
        "schema_version": CACHE_SCHEMA,
        "key": lookup.key,
        "compile_identity": lookup.identity,
        "context": lookup.context,
        "inputs": lookup.inputs,
        "main_sha256": crate::compile_cache::cache_sha256_file(&main_path)?,
        "artifact_fingerprint": fingerprint,
        "embedded_inputs": embedded_inputs,
    });
    let mut manifest = manifest;
    let manifest_sha256 = crate::compile_cache::cache_sha256_bytes(
        &serde_json::to_vec(&manifest).context("serialize frame compile manifest for hashing")?,
    );
    manifest["manifest_sha256"] = json!(manifest_sha256);
    let mut bytes =
        serde_json::to_vec_pretty(&manifest).context("serialize frame compile manifest")?;
    bytes.push(b'\n');
    std::fs::write(artifact_dir.join("manifest.json"), bytes)
        .with_context(|| format!("write frame compile manifest in {}", artifact_dir.display()))
}

fn record_cache_event(
    event: &str,
    key: &str,
    frames: Option<usize>,
    context: &Value,
) -> Result<()> {
    let path = match std::env::var_os(CACHE_STATS_ENV).filter(|value| !value.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => {
            let Some(mut path) =
                std::env::var_os("ROCK_COMPILE_STATS").filter(|value| !value.is_empty())
            else {
                return Ok(());
            };
            path.push(".frame-cache.jsonl");
            PathBuf::from(path)
        }
    };
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    // Windows cannot lock a handle opened with append-only access.
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    file.lock()
        .with_context(|| format!("lock {}", path.display()))?;
    let row = json!({
        "schema": 1,
        "event": event,
        "key": &key[..key.len().min(16)],
        "command": std::env::var("ROCK_BUILD_COMMAND").ok(),
        "project": context["project"],
        "frame_count": frames,
        "pid": std::process::id(),
    });
    let mut bytes = serde_json::to_vec(&row).context("serialize frame compile cache event")?;
    bytes.push(b'\n');
    let write_result = file
        .write_all(&bytes)
        .with_context(|| format!("append {}", path.display()));
    let unlock_result = file
        .unlock()
        .with_context(|| format!("unlock {}", path.display()));
    write_result.and(unlock_result)
}

fn record_cache_event_best_effort(
    target: &'static str,
    event: &str,
    lookup: &Lookup,
    frames: Option<usize>,
) {
    if let Err(error) = record_cache_event(event, &lookup.key, frames, &lookup.context) {
        log::warn!(target: target, "cannot record compile cache event: {error:#}");
    }
}

/// Publish a staged artifact (a directory holding `main.lua`) under `lookup`'s
/// key, if the compile that wrote it read the inputs the lookup resolved.
/// Best effort by contract: every failure is a warning, because the command that
/// compiled already has its own artifact either way.
fn publish_entry(
    root: &Path,
    lookup: &Lookup,
    target: &'static str,
    compiled: Result<Lookup>,
    artifact_dir: &Path,
    fingerprint: &str,
    embedded: &[PathBuf],
    frames: Option<usize>,
) {
    let short = &lookup.key[..lookup.key.len().min(16)];
    let compiled = match compiled {
        Ok(compiled) => compiled,
        Err(error) => {
            log::warn!(target: target, "cannot recheck compile inputs after compiling {short}: {error:#}");
            return;
        }
    };
    if compiled.key != lookup.key {
        log::info!(target: target, "compile inputs changed while compiling {short}; artifact not cached");
        record_cache_event_best_effort(target, "input_changed", lookup, frames);
        return;
    }
    let entry = root.join(&lookup.key);
    let budget = match budget_bytes() {
        Ok(budget) => budget,
        Err(error) => {
            log::warn!(target: target, "cannot read the compile cache budget: {error:#}");
            return;
        }
    };
    if let Err(error) = write_manifest(artifact_dir, lookup, fingerprint, embedded) {
        log::warn!(target: target, "cannot write compile cache manifest {short}: {error:#}");
        return;
    }
    if let Err(error) =
        crate::compile_cache::compile_cache_publish_dir(artifact_dir, &entry, "manifest.json")
    {
        log::warn!(target: target, "cannot publish compile cache entry {short}: {error:#}");
        return;
    }
    if let Err(error) = crate::compile_cache::compile_cache_evict_to_budget(
        root,
        budget,
        &entry,
        "manifest.json",
        STAGING_PREFIX,
        EVICTING_PREFIX,
        STALE_STAGING,
    ) {
        log::warn!(target: target, "compile cache eviction failed: {error:#}");
    }
    record_cache_event_best_effort(target, "store", lookup, frames);
}

fn prepare_cached_artifact<F>(
    root: &Path,
    lookup: &Lookup,
    output_path: &Path,
    frame_count: usize,
    compile: F,
) -> Result<CompiledGame>
where
    F: FnOnce(&Path) -> Result<GameArtifact>,
{
    std::fs::create_dir_all(root)
        .with_context(|| format!("create frame compile cache {}", root.display()))?;
    let short = &lookup.key[..lookup.key.len().min(16)];
    let entry = root.join(&lookup.key);
    match load_cached(&entry, lookup, output_path) {
        Ok(Some(cached)) => {
            log::info!(target: "FRAME", "frame compile cache hit {short}");
            record_cache_event_best_effort("FRAME", "hit", lookup, Some(frame_count));
            return Ok(CompiledGame {
                lua_file: output_path.to_path_buf(),
                artifact_fingerprint: cached.artifact_fingerprint,
            });
        }
        Ok(None) => {}
        Err(error) => {
            log::warn!(target: "FRAME", "frame compile cache entry {short} is unusable: {error:#}");
            record_cache_event_best_effort("FRAME", "reject", lookup, Some(frame_count));
        }
    }
    let _key_lock =
        crate::compile_cache::compile_cache_lock_key(root, &lookup.key, "FRAME", "frame program")?;
    match load_cached(&entry, lookup, output_path) {
        Ok(Some(cached)) => {
            log::info!(target: "FRAME", "frame compile cache hit {short} after wait");
            record_cache_event_best_effort("FRAME", "hit", lookup, Some(frame_count));
            return Ok(CompiledGame {
                lua_file: output_path.to_path_buf(),
                artifact_fingerprint: cached.artifact_fingerprint,
            });
        }
        Ok(None) => {}
        Err(error) => {
            log::warn!(target: "FRAME", "frame compile cache entry {short} is unusable: {error:#}");
            if let Err(evict_error) =
                crate::compile_cache::compile_cache_evict(root, &entry, EVICTING_PREFIX)
            {
                log::warn!(target: "FRAME", "cannot evict unusable frame compile {short}: {evict_error:#}");
            }
            record_cache_event_best_effort("FRAME", "reject", lookup, Some(frame_count));
        }
    }
    log::info!(target: "FRAME", "frame compile cache miss {short}");
    record_cache_event_best_effort("FRAME", "miss", lookup, Some(frame_count));

    let staging = tempfile::Builder::new()
        .prefix(STAGING_PREFIX)
        .tempdir_in(root)
        .with_context(|| format!("stage frame compile under {}", root.display()))?;
    let artifact_dir = staging.path().join("artifact");
    std::fs::create_dir(&artifact_dir)
        .with_context(|| format!("create {}", artifact_dir.display()))?;
    let artifact = compile(&artifact_dir)?;
    let compiled_inputs = lookup.after_compile(&artifact);
    let artifact_path = artifact_dir.join(rock_host::project::MAIN_LUA_FILENAME);
    let compiled = CompiledGame {
        lua_file: output_path.to_path_buf(),
        artifact_fingerprint: artifact.artifact_fingerprint.clone(),
    };
    materialize(&artifact_path, output_path)?;
    record_cache_event_best_effort("FRAME", "compile", lookup, Some(frame_count));
    publish_entry(
        root,
        lookup,
        "FRAME",
        compiled_inputs,
        &artifact_dir,
        &artifact.artifact_fingerprint,
        &artifact.embedded_files,
        Some(frame_count),
    );
    Ok(compiled)
}

/// The compile a `rock stone` session starts from. `root` is [`cache_root`];
/// `None` means the cache is off. A hit installs the cached artifact as
/// `output_path` (and wipes any overlay beside it), so the caller compiles
/// nothing before starting the game. A miss hands back the key the session's
/// first compile will be stored under.
pub(crate) fn session_start(
    root: Option<&Path>,
    backend: Backend,
    path: &str,
    output_path: &Path,
) -> Result<SessionStart> {
    let Some(root) = root else {
        return Ok(SessionStart::Off);
    };
    let project = Path::new(path)
        .canonicalize()
        .with_context(|| format!("canonicalize game {path}"))?;
    let lookup = Lookup::current(&project, backend)?;
    let short = &lookup.key[..lookup.key.len().min(16)];
    let entry = root.join(&lookup.key);
    match load_cached(&entry, &lookup, output_path) {
        Ok(Some(cached)) => {
            log::info!(target: "STONE", "compile cache hit {short}: starting from the cached Lua");
            record_cache_event_best_effort("STONE", "hit", &lookup, None);
            Ok(SessionStart::Hit(cached))
        }
        Ok(None) => Ok(SessionStart::Miss(SessionCompile { lookup })),
        Err(error) => {
            log::warn!(target: "STONE", "compile cache entry {short} is unusable: {error:#}");
            record_cache_event_best_effort("STONE", "reject", &lookup, None);
            if let Err(evict_error) =
                crate::compile_cache::compile_cache_evict(root, &entry, EVICTING_PREFIX)
            {
                log::warn!(target: "STONE", "cannot evict unusable compile {short}: {evict_error:#}");
            }
            Ok(SessionStart::Miss(SessionCompile { lookup }))
        }
    }
}

/// Store a session's first successful compile so the next session starts from
/// it. `comptime` is the `[build.vars]` selection the compile stamped: a
/// resolution that drifted (a `cmd` var) resolves a different key, and nothing
/// is stored under the stale one.
pub(crate) fn store_session_artifact(
    root: &Path,
    pending: &SessionCompile,
    main_lua: &Path,
    fingerprint: &str,
    comptime: Option<&bone_semantic::inference::ComptimeInputs>,
    embedded: &[PathBuf],
) -> Result<()> {
    let Some(comptime) = comptime else {
        return Err(anyhow!(
            "the compile did not report the build vars it stamped"
        ));
    };
    std::fs::create_dir_all(root)
        .with_context(|| format!("create compile cache {}", root.display()))?;
    let staging = tempfile::Builder::new()
        .prefix(STAGING_PREFIX)
        .tempdir_in(root)
        .with_context(|| format!("stage session compile under {}", root.display()))?;
    let artifact_dir = staging.path().join("artifact");
    std::fs::create_dir(&artifact_dir)
        .with_context(|| format!("create {}", artifact_dir.display()))?;
    std::fs::copy(
        main_lua,
        artifact_dir.join(rock_host::project::MAIN_LUA_FILENAME),
    )
    .with_context(|| format!("copy {} into the compile cache", main_lua.display()))?;
    let lookup = &pending.lookup;
    let _key_lock =
        crate::compile_cache::compile_cache_lock_key(root, &lookup.key, "STONE", "stone program")?;
    let compiled = Lookup::with_comptime(&lookup.project, lookup.backend, comptime);
    publish_entry(
        root,
        lookup,
        "STONE",
        compiled,
        &artifact_dir,
        fingerprint,
        embedded,
        None,
    );
    Ok(())
}

pub fn compile_game_for_frame(
    backend: Backend,
    path: &str,
    output: &str,
    frame_count: usize,
) -> Result<CompiledGame> {
    let project = Path::new(path)
        .canonicalize()
        .with_context(|| format!("canonicalize game {path}"))?;
    let project_path = project.to_string_lossy();
    let Some(root) = cache_root()? else {
        return crate::frame_runtime::compile_game_for_run(backend, &project_path, output);
    };
    crate::frame_runtime::prepare_game_compile(&project_path)?;
    // The lookup resolves what the compile would: discovery, build vars.
    let lookup = Lookup::current(&project, backend)?;
    let output_path = PathBuf::from(output).join(rock_host::project::MAIN_LUA_FILENAME);
    let compiled =
        prepare_cached_artifact(&root, &lookup, &output_path, frame_count, |artifact_dir| {
            compile_game(backend, &project_path, artifact_dir)
        })?;
    Ok(compiled)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_storage_frame_cache_default() {
        crate::host_paths::assert_cache_default(
            "frame_compile_cache::tests::host_storage_frame_cache_default",
            "rock-frame-compile-cache/cache",
            cache_root,
        );
    }

    fn write_project(name: &str, build_vars: &str, main: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("stick.toml"),
            format!("[package]\nname = \"{name}\"\n{build_vars}"),
        )
        .unwrap();
        std::fs::write(dir.path().join("main.rock"), main).unwrap();
        let root = dir.path().canonicalize().unwrap();
        (dir, root)
    }

    fn project() -> (tempfile::TempDir, PathBuf) {
        write_project("frame-cache-fixture", "", "fn main() { print(1) }\n")
    }

    fn emitted_source_revision(lua: &[u8]) -> String {
        let source = String::from_utf8_lossy(lua);
        let start = source
            .find("source-v1:")
            .expect("compiled Lua contains its deferred build variable");
        source[start..]
            .chars()
            .take_while(|character| {
                character.is_ascii_alphanumeric() || matches!(character, ':' | '-')
            })
            .collect()
    }

    fn main_lua_in(dir: &Path) -> Vec<u8> {
        std::fs::read(dir.join(rock_host::project::MAIN_LUA_FILENAME)).unwrap()
    }

    fn event_names(stats: &Path) -> Vec<String> {
        std::fs::read_to_string(stats)
            .unwrap()
            .lines()
            .map(|line| {
                serde_json::from_str::<Value>(line).unwrap()["event"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect()
    }

    /// A fresh production compile, the expected answer for a cached one.
    struct Oracle {
        lua: Vec<u8>,
        fingerprint: String,
    }

    impl Oracle {
        fn of(project: &Path) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let artifact =
                compile_game(Backend::Skull, &project.to_string_lossy(), dir.path()).unwrap();
            Self {
                lua: main_lua_in(dir.path()),
                fingerprint: artifact.artifact_fingerprint.as_str().to_string(),
            }
        }

        #[track_caller]
        fn assert_matches(&self, compiled: &CompiledGame, why: &str) {
            assert_eq!(
                std::fs::read(&compiled.lua_file).unwrap(),
                self.lua,
                "{why}"
            );
            assert_eq!(
                compiled.artifact_fingerprint.as_str(),
                self.fingerprint,
                "{why}"
            );
        }
    }

    /// Resolve `lookup` through the cache, compiling for real on a miss.
    fn compile_cached(
        cache: &Path,
        lookup: &Lookup,
        project: &Path,
    ) -> (tempfile::TempDir, CompiledGame) {
        let output = tempfile::tempdir().unwrap();
        let compiled =
            prepare_cached_artifact(cache, lookup, &output.path().join("main.lua"), 1, |dir| {
                compile_game(Backend::Skull, &project.to_string_lossy(), dir)
            })
            .unwrap();
        (output, compiled)
    }

    /// Re-run this test binary as the worker test `name`, in `project`.
    fn worker(name: &str, project: &Path) -> std::process::Command {
        let rock_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .unwrap();
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                &format!("frame_compile_cache::tests::{name}"),
                "--nocapture",
            ])
            .current_dir(project)
            .env("ROCK_DIR", rock_dir);
        command
    }

    #[test]
    fn same_compile_input_uses_one_artifact_across_frame_counts_and_output_dirs() {
        for (command, frame_counts) in [("frame", [1, 600]), ("perf", [600, 600])] {
            let (_project_dir, project) = project();
            let cache = tempfile::tempdir().unwrap();
            let stats = cache.path().join("events.jsonl");
            for (run, frames) in frame_counts.into_iter().enumerate() {
                let output = cache.path().join(format!("outputs/{run}"));
                run_frame_cache_worker(command, &project, cache.path(), &stats, frames, &output);
            }
            assert_eq!(
                event_names(&stats),
                ["miss", "compile", "store", "hit"],
                "the second {command} process reuses the first one's compile"
            );
        }
    }

    #[test]
    fn deferred_source_fingerprint_keys_fresh_process_frame_cache_lifecycle() {
        let (_project_dir, project) = write_project(
            "frame-cache-source-fingerprint",
            "[build.vars]\nREV = { source_fingerprint = { roots = [\"Root\"], files = [\"support.txt\"] } }\n",
            "value Root { count: int = 1 }\nfn main() { print(build_var(\"REV\")) }\n",
        );
        let support = project.join("support.txt");
        std::fs::write(&support, "decoder-one\n").unwrap();
        let cache = tempfile::tempdir().unwrap();
        let stats = cache.path().join("events.jsonl");
        let run = |name: &str| {
            let output = cache.path().join("outputs").join(name);
            run_frame_cache_worker("frame", &project, cache.path(), &stats, 1, &output);
            main_lua_in(&output)
        };

        let first_lua = run("first");
        assert_eq!(
            first_lua,
            run("hit"),
            "the second fresh process reuses the recorded artifact"
        );
        let first_revision = emitted_source_revision(&first_lua);

        std::fs::write(
            project.join("main.rock"),
            "value Root { count: int = 2 }\nfn main() { print(build_var(\"REV\")) }\n",
        )
        .unwrap();
        let source_changed_lua = run("source-changed");
        let source_changed_revision = emitted_source_revision(&source_changed_lua);
        assert_ne!(
            first_revision, source_changed_revision,
            "a changed typed default updates the deferred value"
        );
        assert_ne!(
            first_lua, source_changed_lua,
            "a changed typed default updates the deferred value and artifact"
        );

        std::fs::write(&support, "decoder-two\n").unwrap();
        let support_changed_lua = run("support-changed");
        assert_ne!(
            source_changed_revision,
            emitted_source_revision(&support_changed_lua),
            "changed explicit support bytes update the deferred value",
        );
        assert_ne!(
            source_changed_lua, support_changed_lua,
            "changed explicit support bytes update the artifact"
        );

        assert_eq!(
            event_names(&stats),
            [
                "miss", "compile", "store", "hit", "miss", "compile", "store", "miss", "compile",
                "store"
            ],
            "a fresh-process hit is stable before source and explicit support changes each force a miss",
        );
    }

    /// Compile `project` in a fresh test process through the production entry
    /// point, with `command` as the recorded build command.
    fn run_frame_cache_worker(
        command: &str,
        project: &Path,
        cache: &Path,
        stats: &Path,
        frame_count: usize,
        output: &Path,
    ) {
        let status = worker("frame_cache_wrapper_worker", project)
            .env("ROCK_BUILD_COMMAND", command)
            .env("ROCK_FRAME_COMPILE_CACHE", "1")
            .env("ROCK_FRAME_COMPILE_CACHE_DIR", cache.join("cache"))
            .env("ROCK_FRAME_COMPILE_CACHE_STATS", stats)
            .env("FRAME_CACHE_WRAPPER_TEST", "1")
            .env("FRAME_CACHE_FRAME_COUNT", frame_count.to_string())
            .env("FRAME_CACHE_PROJECT", project)
            .env("FRAME_CACHE_OUTPUT", output)
            .env_remove("SKULL_ALLOCATION_REPORT")
            .env_remove("SKULL_ALLOCATION_ACTIVATION")
            .status()
            .unwrap();
        assert!(
            status.success(),
            "production frame compiler worker failed at {frame_count} frames"
        );
    }

    #[test]
    fn frame_cache_wrapper_worker() {
        if std::env::var_os("FRAME_CACHE_WRAPPER_TEST").is_none() {
            return;
        }
        let project = PathBuf::from(std::env::var_os("FRAME_CACHE_PROJECT").unwrap())
            .canonicalize()
            .unwrap();
        let output = PathBuf::from(std::env::var_os("FRAME_CACHE_OUTPUT").unwrap());
        let frame_count = std::env::var("FRAME_CACHE_FRAME_COUNT")
            .unwrap()
            .parse::<usize>()
            .unwrap();
        let oracle = Oracle::of(&project);
        let compiled = compile_game_for_frame(
            Backend::Skull,
            &project.to_string_lossy(),
            &output.to_string_lossy(),
            frame_count,
        )
        .unwrap();
        oracle.assert_matches(&compiled, "the frame compile agrees with a fresh compile");
    }

    #[test]
    fn source_edit_misses_the_frame_compile_cache() {
        let (_project_dir, project) = project();
        let cache = tempfile::tempdir().unwrap();
        let first_lookup = Lookup::current(&project, Backend::Skull).unwrap();
        let (_first_output, first) = compile_cached(cache.path(), &first_lookup, &project);
        std::fs::write(project.join("main.rock"), "fn main() { print(2) }\n").unwrap();
        let changed_lookup = Lookup::current(&project, Backend::Skull).unwrap();
        let (_second_output, second) = compile_cached(cache.path(), &changed_lookup, &project);
        let oracle = Oracle::of(&project);
        oracle.assert_matches(
            &second,
            "the changed source emits the production compiler's current Lua",
        );
        assert_ne!(
            std::fs::read(&first.lua_file).unwrap(),
            oracle.lua,
            "a stale artifact from the prior source cannot pass as the changed compile"
        );
    }

    #[test]
    fn build_var_override_misses_the_frame_compile_cache() {
        let (_project_dir, project) = write_project(
            "frame-cache-var-fixture",
            "[build.vars]\nMODE = { value = \"default\" }\n",
            "fn main() { print(build_var(\"MODE\")) }\n",
        );
        let cache = tempfile::tempdir().unwrap();
        let one_output = cache.path().join("output-one/main.lua");
        let two_output = cache.path().join("output-two/main.lua");
        run_build_var_cache_worker(&project, cache.path(), "one", &one_output);
        run_build_var_cache_worker(&project, cache.path(), "two", &two_output);
        let one = std::fs::read(one_output).unwrap();
        let two = std::fs::read(two_output).unwrap();
        assert_ne!(
            one, two,
            "the production compiler emits the selected --var value"
        );
    }

    fn run_build_var_cache_worker(project: &Path, cache: &Path, value: &str, output: &Path) {
        let status = worker("build_var_cache_worker", project)
            .env("FRAME_CACHE_TEST_VAR", value)
            .env("FRAME_CACHE_TEST_PROJECT", project)
            .env("FRAME_CACHE_TEST_ROOT", cache)
            .env("FRAME_CACHE_TEST_OUTPUT", output)
            .status()
            .unwrap();
        assert!(
            status.success(),
            "production cache worker for --var MODE={value} failed"
        );
    }

    #[test]
    fn build_var_cache_worker() {
        let Ok(value) = std::env::var("FRAME_CACHE_TEST_VAR") else {
            return;
        };
        let project = PathBuf::from(std::env::var_os("FRAME_CACHE_TEST_PROJECT").unwrap())
            .canonicalize()
            .unwrap();
        let root = PathBuf::from(std::env::var_os("FRAME_CACHE_TEST_ROOT").unwrap());
        let output = PathBuf::from(std::env::var_os("FRAME_CACHE_TEST_OUTPUT").unwrap());
        crate::comptime::set_invocation_selection(None, None, None, &[format!("MODE={value}")])
            .unwrap();
        let lookup = Lookup::current(&project, Backend::Skull).unwrap();
        let compiled = prepare_cached_artifact(&root, &lookup, &output, 1, |dir| {
            compile_game(Backend::Skull, &project.to_string_lossy(), dir)
        })
        .unwrap();
        Oracle::of(&project).assert_matches(
            &compiled,
            &format!("cached output agrees with a fresh production compile for --var MODE={value}"),
        );
    }

    #[test]
    fn backend_change_misses_the_frame_compile_cache() {
        let (_project_dir, project) = project();
        let cache = tempfile::tempdir().unwrap();
        let skull = Lookup::current(&project, Backend::Skull).unwrap();
        let (_output, compiled) = compile_cached(cache.path(), &skull, &project);
        Oracle::of(&project).assert_matches(&compiled, "the skull compile");

        let changed_backend = Lookup::current(&project, Backend::Bone).unwrap();
        assert_ne!(
            skull.key, changed_backend.key,
            "the production compile identity includes its backend"
        );
        let changed_output = tempfile::tempdir().unwrap();
        let result = prepare_cached_artifact(
            cache.path(),
            &changed_backend,
            &changed_output.path().join("main.lua"),
            1,
            |_| {
                Err(anyhow!(
                    "changed backend compile path reached after cache miss"
                ))
            },
        );
        assert!(
            result.is_err(),
            "a backend change reaches its own compile path"
        );
    }

    #[test]
    fn tampered_manifest_hash_is_rejected_and_recompiled() {
        let (_project_dir, project) = project();
        let cache = tempfile::tempdir().unwrap();
        let lookup = Lookup::current(&project, Backend::Skull).unwrap();
        let entry = cache.path().join(&lookup.key);
        let oracle = Oracle::of(&project);
        let assert_recompiled = |why: &str| {
            let (_output, compiled) = compile_cached(cache.path(), &lookup, &project);
            oracle.assert_matches(&compiled, why);
        };
        assert_recompiled("the first compile");

        let manifest_path = entry.join("manifest.json");
        let mut manifest: Value =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        manifest["artifact_fingerprint"] = json!("tampered-fingerprint");
        std::fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert_recompiled("a changed manifest is rejected before its fingerprint is reused");

        std::fs::write(
            entry.join(rock_host::project::MAIN_LUA_FILENAME),
            b"tampered main.lua\n",
        )
        .unwrap();
        assert_recompiled("a cached main.lua hash mismatch cannot reach the run output");
    }

    #[test]
    fn cache_miss_wipes_a_stale_incremental_overlay() {
        let (_project_dir, project) = project();
        let cache = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let overlay = output.path().join("incremental.lua");
        std::fs::write(&overlay, b"-- stale overlay\n").unwrap();
        let lookup = Lookup::current(&project, Backend::Skull).unwrap();
        prepare_cached_artifact(
            cache.path(),
            &lookup,
            &output.path().join("main.lua"),
            1,
            |dir| compile_game(Backend::Skull, &project.to_string_lossy(), dir),
        )
        .unwrap();
        assert!(
            !overlay.exists(),
            "a fresh main.lua never runs beside an older overlay"
        );
    }

    #[test]
    fn entry_is_keyed_on_the_build_vars_the_compile_stamped() {
        let (_project_dir, project) = write_project(
            "frame-cache-stamp-fixture",
            "[build.vars]\nMODE = { cmd = \"cat mode.txt\" }\n",
            "fn main() { print(build_var(\"MODE\")) }\n",
        );
        let mode = project.join("mode.txt");
        std::fs::write(&mode, "a").unwrap();
        let cache = tempfile::tempdir().unwrap();
        let lookup = Lookup::current(&project, Backend::Skull).unwrap();
        let output = tempfile::tempdir().unwrap();
        prepare_cached_artifact(
            cache.path(),
            &lookup,
            &output.path().join("main.lua"),
            1,
            |dir| {
                // A non-monotonic var: B during the compile, A before and after.
                std::fs::write(&mode, "b").unwrap();
                let artifact = compile_game(Backend::Skull, &project.to_string_lossy(), dir);
                std::fs::write(&mode, "a").unwrap();
                artifact
            },
        )
        .unwrap();
        assert!(
            !cache.path().join(&lookup.key).exists(),
            "Lua built with MODE=b is never stored under the MODE=a key"
        );
    }

    #[test]
    fn lookup_failure_propagates_the_build_var_error() {
        let (dir, project) = write_project(
            "frame-cache-lookup-fixture",
            "[build.vars]\nBROKEN = { cmd = \"exit 3\" }\n",
            "fn main() { print(build_var(\"BROKEN\")) }\n",
        );
        std::fs::create_dir(project.join("tmp")).unwrap();
        let status = worker("lookup_failure_worker", &project)
            .env("FRAME_CACHE_LOOKUP_FAILURE_PROJECT", &project)
            .env("ROCK_FRAME_COMPILE_CACHE_DIR", dir.path().join("cache"))
            .env_remove("ROCK_FRAME_COMPILE_CACHE")
            .env_remove("SKULL_ALLOCATION_REPORT")
            .env_remove("SKULL_ALLOCATION_ACTIVATION")
            .status()
            .unwrap();
        assert!(status.success(), "lookup failure worker failed");
    }

    #[test]
    fn lookup_failure_worker() {
        let Some(project) = std::env::var_os("FRAME_CACHE_LOOKUP_FAILURE_PROJECT") else {
            return;
        };
        let project = PathBuf::from(project);
        let Err(error) =
            compile_game_for_frame(Backend::Skull, &project.to_string_lossy(), "tmp", 1)
        else {
            panic!("a failing build var fails the frame compile");
        };
        assert!(format!("{error:#}").contains("BROKEN"), "{error:#}");
    }

    /// Compile a session's first program through the production session
    /// compiler and publish it, as `rock stone` does on a miss.
    fn stone_session_compiles_and_stores(project: &Path, cache: &Path, main_lua: &Path) -> String {
        let SessionStart::Miss(pending) = session_start(
            Some(cache),
            Backend::Skull,
            &project.to_string_lossy(),
            main_lua,
        )
        .unwrap() else {
            panic!("an empty cache misses");
        };
        use crate::game_runtime::GameCompiler as _;
        let mut compiler = crate::game_runtime::SkullGameCompiler::new(
            &project.to_string_lossy(),
            main_lua.to_path_buf(),
            main_lua.with_file_name("incremental.lua"),
        );
        compiler
            .compile(&std::collections::HashSet::new(), false)
            .expect("the session's first compile");
        let fingerprint = compiler.artifact_fingerprint().unwrap();
        let embedded = compiler
            .comptime_watch_files_with_hashes()
            .into_iter()
            .map(|(path, _)| path)
            .collect::<Vec<_>>();
        store_session_artifact(
            cache,
            &pending,
            main_lua,
            &fingerprint,
            compiler.comptime_inputs(),
            &embedded,
        )
        .unwrap();
        fingerprint
    }

    #[test]
    fn one_key_serves_a_stone_session_and_a_frame_run() {
        let (_project_dir, project) = project();
        let cache = tempfile::tempdir().unwrap();
        let session_output = tempfile::tempdir().unwrap();
        let session_main = session_output.path().join("main.lua");
        let fingerprint = stone_session_compiles_and_stores(&project, cache.path(), &session_main);

        // `rock frame` resolves the same lookup and must find that entry.
        let lookup = Lookup::current(&project, Backend::Skull).unwrap();
        let frame_output = tempfile::tempdir().unwrap();
        let compiled = prepare_cached_artifact(
            cache.path(),
            &lookup,
            &frame_output.path().join("main.lua"),
            1,
            |_| {
                Err(anyhow!(
                    "a frame run must not compile what a stone session stored"
                ))
            },
        )
        .expect("the frame run reads the session's entry");
        assert_eq!(compiled.artifact_fingerprint, fingerprint);
        assert_eq!(
            std::fs::read(&compiled.lua_file).unwrap(),
            std::fs::read(&session_main).unwrap(),
            "both commands run the same emitted program",
        );
    }

    #[test]
    fn a_session_keeps_a_volatile_build_var_and_misses_a_changed_one() {
        let (_project_dir, project) = write_project(
            "session-cache-var-fixture",
            "[build.vars]\nMODE = { cmd = \"cat mode.txt\" }\nCLOCK = { cmd = \"cat clock.txt\", volatile = true }\n",
            "fn main() { print(build_var(\"MODE\")); print(build_var(\"CLOCK\")) }\n",
        );
        std::fs::write(project.join("mode.txt"), "a\n").unwrap();
        std::fs::write(project.join("clock.txt"), "1\n").unwrap();
        let cache = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        stone_session_compiles_and_stores(&project, cache.path(), &output.path().join("main.lua"));
        let start = || {
            let output = tempfile::tempdir().unwrap();
            session_start(
                Some(cache.path()),
                Backend::Skull,
                &project.to_string_lossy(),
                &output.path().join("main.lua"),
            )
            .unwrap()
        };

        std::fs::write(project.join("clock.txt"), "2\n").unwrap();
        assert!(
            matches!(start(), SessionStart::Hit(_)),
            "a volatile build var is not part of the key; a clock would make every session miss",
        );

        std::fs::write(project.join("mode.txt"), "b\n").unwrap();
        assert!(
            matches!(start(), SessionStart::Miss(_)),
            "a non-volatile build var is identity",
        );
    }
}
