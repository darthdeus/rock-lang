//! Content-addressed full-compilation artifacts for `rock frame`.
//! Runtime frame counts and output paths select execution, not emitted Lua.

use crate::common::{GameArtifact, compile_game_into};
use crate::{Backend, CompiledGame};
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
    requested_renderer: Option<rock_host::stick::RendererBackend>,
}

impl Lookup {
    fn current(project: &Path, backend: Backend, requested_renderer: Option<rock_host::stick::RendererBackend>) -> Result<Self> {
        let comptime = crate::comptime::inputs_for_project_dir(project)?;
        Self::with_comptime(project, backend, requested_renderer, &comptime)
    }

    /// The key of what a finished compile actually read: the build vars it
    /// stamped and the sources as they are now. A resolution between lookup and
    /// compile can differ (a `cmd` var), so only an equal key may be stored.
    fn after_compile(&self, artifact: &GameArtifact) -> Result<Self> {
        let comptime = artifact.comptime_inputs.as_ref()
            .ok_or_else(|| anyhow!("the compiler did not report the build vars it stamped"))?;
        Self::with_comptime(&self.project, self.backend, self.requested_renderer, comptime)
    }

    fn with_comptime(
        project: &Path,
        backend: Backend,
        requested_renderer: Option<rock_host::stick::RendererBackend>,
        comptime: &bone_semantic::inference::ComptimeInputs,
    ) -> Result<Self> {
        let project = project.canonicalize().with_context(|| format!("canonicalize {}", project.display()))?;
        let identity = crate::skull_backend::compile_cache_identity(&project, backend, comptime)?;
        let inputs = frame_source_inputs(&project)?;
        let configured_renderer = renderer_name(crate::skull_backend::skull_renderer_backend(
            &project.to_string_lossy(),
        )?);
        let context = json!({
            "project": project.to_string_lossy(),
            "renderer": configured_renderer,
            "requested_renderer": requested_renderer.map(renderer_name),
        });
        let key = cache_key(&identity, &context, &inputs);
        Ok(Self { key, identity, context, inputs, project, backend, requested_renderer })
    }
}

fn renderer_name(renderer: rock_host::stick::RendererBackend) -> &'static str {
    match renderer {
        rock_host::stick::RendererBackend::Gl => "gl",
        rock_host::stick::RendererBackend::Wgpu => "wgpu",
    }
}

fn cache_key(identity: &Value, context: &Value, inputs: &Value) -> String {
    let canonical = json!([CACHE_SCHEMA, identity, context, inputs]);
    crate::skull_backend::cache_sha256_bytes(canonical.to_string().as_bytes())
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
            .with_context(|| format!("canonicalize frame compile source {}", source.file.to_id_path()))?;
        if let Some(path) = nearest_manifest(&source_path) {
            manifests.insert(path);
        }
        inputs.push(json!({
            "kind": "source",
            "path": source_path.to_string_lossy(),
            "sha256": crate::skull_backend::cache_sha256_bytes(source.contents.as_bytes()),
        }));
    }
    for path in manifests {
        let canonical = path.canonicalize().with_context(|| format!("canonicalize {}", path.display()))?;
        let bytes = std::fs::read(&canonical).with_context(|| format!("read {}", canonical.display()))?;
        inputs.push(json!({
            "kind": "manifest",
            "path": canonical.to_string_lossy(),
            "sha256": crate::skull_backend::cache_sha256_bytes(&bytes),
        }));
    }
    inputs.sort_by(|left, right| {
        (left["kind"].as_str(), left["path"].as_str())
            .cmp(&(right["kind"].as_str(), right["path"].as_str()))
    });
    Ok(Value::Array(inputs))
}

fn cache_root() -> Result<Option<PathBuf>> {
    // A hit skips SkullStone::init, which owns these requested report files.
    if std::env::var_os("SKULL_ALLOCATION_REPORT").is_some()
        || std::env::var_os("SKULL_ALLOCATION_ACTIVATION").is_some()
    {
        return Ok(None);
    }
    if std::env::var_os(CACHE_ENV).is_some_and(|value| value == "0")
        || crate::skull_backend::compile_cache_compiler_sha256().is_none()
    {
        return Ok(None);
    }
    if let Some(dir) = std::env::var_os(CACHE_DIR_ENV).filter(|value| !value.is_empty()) {
        return Ok(Some(PathBuf::from(dir)));
    }
    let home = std::env::var_os("HOME").filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("no HOME for the frame compile cache; set {CACHE_DIR_ENV} or {CACHE_ENV}=0"))?;
    Ok(Some(PathBuf::from(home).join("tmp").join("rock-frame-compile-cache").join("cache")))
}

fn budget_bytes() -> Result<u64> {
    let mb = match std::env::var(CACHE_MAX_MB_ENV) {
        Ok(value) => value.parse::<u64>()
            .with_context(|| format!("{CACHE_MAX_MB_ENV}={value:?} is not a whole number of MB"))?,
        Err(std::env::VarError::NotPresent) => DEFAULT_MAX_MB,
        Err(error) => return Err(anyhow!("{CACHE_MAX_MB_ENV}: {error}")),
    };
    mb.checked_mul(1024 * 1024).ok_or_else(|| anyhow!("{CACHE_MAX_MB_ENV} is too large"))
}

fn embedded_inputs(paths: &[PathBuf]) -> Result<Value> {
    let mut inputs = Vec::with_capacity(paths.len());
    for path in paths {
        let canonical = path.canonicalize().with_context(|| format!("canonicalize embedded input {}", path.display()))?;
        inputs.push(json!({
            "path": canonical.to_string_lossy(),
            "sha256": crate::skull_backend::cache_sha256_file(&canonical)?,
        }));
    }
    inputs.sort_by(|left, right| left["path"].as_str().cmp(&right["path"].as_str()));
    Ok(Value::Array(inputs))
}

fn validate_embedded_inputs(value: &Value) -> Result<()> {
    let rows = value.as_array().ok_or_else(|| anyhow!("frame compile manifest embedded_inputs is not an array"))?;
    for row in rows {
        let path = row.get("path").and_then(Value::as_str)
            .ok_or_else(|| anyhow!("frame compile manifest embedded input has no path"))?;
        let expected = row.get("sha256").and_then(Value::as_str)
            .ok_or_else(|| anyhow!("frame compile manifest embedded input has no sha256"))?;
        let path = Path::new(path);
        let canonical = path.canonicalize().with_context(|| format!("resolve embedded input {}", path.display()))?;
        if canonical != path {
            return Err(anyhow!("frame compile manifest embedded input path changed: {}", path.display()));
        }
        let actual = crate::skull_backend::cache_sha256_file(&canonical)?;
        if actual != expected {
            return Err(anyhow!("embedded input hash changed: {}", canonical.display()));
        }
    }
    Ok(())
}

fn manifest_key(manifest: &Value) -> Result<String> {
    let schema = manifest.get("schema_version").and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("frame compile manifest has no schema_version"))?;
    if schema != CACHE_SCHEMA {
        return Err(anyhow!("unsupported frame compile manifest schema {schema}"));
    }
    let identity = manifest.get("compile_identity")
        .ok_or_else(|| anyhow!("frame compile manifest has no compile_identity"))?;
    let context = manifest.get("context")
        .ok_or_else(|| anyhow!("frame compile manifest has no context"))?;
    let inputs = manifest.get("inputs")
        .ok_or_else(|| anyhow!("frame compile manifest has no inputs"))?;
    Ok(cache_key(identity, context, inputs))
}

fn load_cached(entry: &Path, lookup: &Lookup, output_path: &Path) -> Result<Option<CompiledGame>> {
    if !entry.exists() {
        return Ok(None);
    }
    let metadata = std::fs::symlink_metadata(entry)
        .with_context(|| format!("inspect frame compile cache entry {}", entry.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(anyhow!("frame compile cache entry is not a regular directory"));
    }
    let manifest_path = entry.join("manifest.json");
    let manifest_metadata = std::fs::symlink_metadata(&manifest_path)
        .with_context(|| format!("inspect {}", manifest_path.display()))?;
    if !manifest_metadata.file_type().is_file() {
        return Err(anyhow!("frame compile manifest is not a regular file"));
    }
    let manifest: Value = serde_json::from_slice(
        &std::fs::read(&manifest_path).with_context(|| format!("read {}", manifest_path.display()))?,
    )
    .with_context(|| format!("parse {}", manifest_path.display()))?;
    let stored_manifest_sha256 = manifest.get("manifest_sha256").and_then(Value::as_str)
        .ok_or_else(|| anyhow!("frame compile manifest has no manifest_sha256"))?;
    let mut unsigned_manifest = manifest.clone();
    unsigned_manifest.as_object_mut()
        .ok_or_else(|| anyhow!("frame compile manifest is not an object"))?
        .remove("manifest_sha256");
    let actual_manifest_sha256 = crate::skull_backend::cache_sha256_bytes(
        &serde_json::to_vec(&unsigned_manifest).context("serialize frame compile manifest for verification")?,
    );
    if actual_manifest_sha256 != stored_manifest_sha256 {
        return Err(anyhow!("frame compile manifest hash does not match its contents"));
    }
    let stored_key = manifest.get("key").and_then(Value::as_str)
        .ok_or_else(|| anyhow!("frame compile manifest has no key"))?;
    if manifest_key(&manifest)? != stored_key || stored_key != lookup.key {
        return Err(anyhow!("frame compile manifest key does not match its entry"));
    }
    if manifest.get("compile_identity") != Some(&lookup.identity)
        || manifest.get("context") != Some(&lookup.context)
        || manifest.get("inputs") != Some(&lookup.inputs)
    {
        return Err(anyhow!("frame compile manifest inputs do not match the current compile"));
    }
    let main_path = entry.join(rock_host::project::MAIN_LUA_FILENAME);
    let main_metadata = std::fs::symlink_metadata(&main_path)
        .with_context(|| format!("inspect {}", main_path.display()))?;
    if !main_metadata.file_type().is_file() {
        return Err(anyhow!("cached main.lua is not a regular file"));
    }
    let expected_hash = manifest.get("main_sha256").and_then(Value::as_str)
        .ok_or_else(|| anyhow!("frame compile manifest has no main_sha256"))?;
    let actual_hash = crate::skull_backend::cache_sha256_file(&main_path)?;
    if actual_hash != expected_hash {
        return Err(anyhow!("cached main.lua hash does not match its manifest"));
    }
    let embedded = manifest.get("embedded_inputs")
        .ok_or_else(|| anyhow!("frame compile manifest has no embedded_inputs"))?;
    validate_embedded_inputs(embedded)?;
    let artifact_fingerprint = manifest.get("artifact_fingerprint").and_then(Value::as_str)
        .filter(|fingerprint| !fingerprint.is_empty())
        .ok_or_else(|| anyhow!("frame compile manifest has no artifact_fingerprint"))?
        .to_string();
    materialize(&main_path, output_path)?;
    if let Err(error) = crate::skull_backend::compile_cache_mark_used(entry, "manifest.json") {
        log::warn!(target: "FRAME", "cannot update frame compile cache use time: {error:#}");
    }
    Ok(Some(CompiledGame { lua_file: output_path.to_path_buf(), artifact_fingerprint }))
}

/// Install `source` as the run's `main.lua` and wipe the overlay beside it, as
/// a full compile does.
fn materialize(source: &Path, destination: &Path) -> Result<()> {
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    rock_host::project::write_atomic_streamed(destination, |file| {
        std::io::copy(&mut std::fs::File::open(source)?, file)?;
        Ok(())
    })
    .with_context(|| format!("materialize cached frame artifact at {}", destination.display()))?;
    crate::skull_backend::wipe_overlay(&destination.with_file_name("incremental.lua"))
}

fn write_manifest(
    artifact_dir: &Path,
    lookup: &Lookup,
    artifact: &GameArtifact,
) -> Result<()> {
    let main_path = artifact_dir.join(rock_host::project::MAIN_LUA_FILENAME);
    let embedded_inputs = embedded_inputs(&artifact.embedded_files)?;
    let manifest = json!({
        "schema_version": CACHE_SCHEMA,
        "key": lookup.key,
        "compile_identity": lookup.identity,
        "context": lookup.context,
        "inputs": lookup.inputs,
        "main_sha256": crate::skull_backend::cache_sha256_file(&main_path)?,
        "artifact_fingerprint": artifact.artifact_fingerprint,
        "embedded_inputs": embedded_inputs,
    });
    let mut manifest = manifest;
    let manifest_sha256 = crate::skull_backend::cache_sha256_bytes(
        &serde_json::to_vec(&manifest).context("serialize frame compile manifest for hashing")?,
    );
    manifest["manifest_sha256"] = json!(manifest_sha256);
    let mut bytes = serde_json::to_vec_pretty(&manifest).context("serialize frame compile manifest")?;
    bytes.push(b'\n');
    std::fs::write(artifact_dir.join("manifest.json"), bytes)
        .with_context(|| format!("write frame compile manifest in {}", artifact_dir.display()))
}

fn record_cache_event(event: &str, key: &str, frame_count: usize, context: &Value) -> Result<()> {
    let path = match std::env::var_os(CACHE_STATS_ENV).filter(|value| !value.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => {
            let Some(mut path) = std::env::var_os("ROCK_COMPILE_STATS").filter(|value| !value.is_empty()) else {
                return Ok(());
            };
            path.push(".frame-cache.jsonl");
            PathBuf::from(path)
        }
    };
    if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    file.lock().with_context(|| format!("lock {}", path.display()))?;
    let row = json!({
        "schema": 1,
        "event": event,
        "key": &key[..key.len().min(16)],
        "command": std::env::var("ROCK_BUILD_COMMAND").ok(),
        "project": context["project"],
        "frame_count": frame_count,
        "pid": std::process::id(),
    });
    let mut bytes = serde_json::to_vec(&row).context("serialize frame compile cache event")?;
    bytes.push(b'\n');
    let write_result = file.write_all(&bytes).with_context(|| format!("append {}", path.display()));
    let unlock_result = file.unlock().with_context(|| format!("unlock {}", path.display()));
    write_result.and(unlock_result)
}

fn record_cache_event_best_effort(event: &str, lookup: &Lookup, frame_count: usize) {
    if let Err(error) = record_cache_event(event, &lookup.key, frame_count, &lookup.context) {
        log::warn!(target: "FRAME", "cannot record frame compile cache event: {error:#}");
    }
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
    let budget = budget_bytes()?;
    std::fs::create_dir_all(root).with_context(|| format!("create frame compile cache {}", root.display()))?;
    let short = &lookup.key[..lookup.key.len().min(16)];
    let entry = root.join(&lookup.key);
    match load_cached(&entry, lookup, output_path) {
        Ok(Some(compiled)) => {
            log::info!(target: "FRAME", "frame compile cache hit {short}");
            record_cache_event_best_effort("hit", lookup, frame_count);
            return Ok(compiled);
        }
        Ok(None) => {}
        Err(error) => {
            log::warn!(target: "FRAME", "frame compile cache entry {short} is unusable: {error:#}");
            record_cache_event_best_effort("reject", lookup, frame_count);
        }
    }
    let _key_lock = crate::skull_backend::compile_cache_lock_key(root, &lookup.key, "FRAME", "frame program")?;
    match load_cached(&entry, lookup, output_path) {
        Ok(Some(compiled)) => {
            log::info!(target: "FRAME", "frame compile cache hit {short} after wait");
            record_cache_event_best_effort("hit", lookup, frame_count);
            return Ok(compiled);
        }
        Ok(None) => {}
        Err(error) => {
            log::warn!(target: "FRAME", "frame compile cache entry {short} is unusable: {error:#}");
            if let Err(evict_error) = crate::skull_backend::compile_cache_evict(root, &entry, EVICTING_PREFIX) {
                log::warn!(target: "FRAME", "cannot evict unusable frame compile {short}: {evict_error:#}");
            }
            record_cache_event_best_effort("reject", lookup, frame_count);
        }
    }
    log::info!(target: "FRAME", "frame compile cache miss {short}");
    record_cache_event_best_effort("miss", lookup, frame_count);

    let staging = tempfile::Builder::new().prefix(STAGING_PREFIX).tempdir_in(root)
        .with_context(|| format!("stage frame compile under {}", root.display()))?;
    let artifact_dir = staging.path().join("artifact");
    std::fs::create_dir(&artifact_dir).with_context(|| format!("create {}", artifact_dir.display()))?;
    let artifact = compile(&artifact_dir)?;
    let inputs_still_match = match lookup.after_compile(&artifact) {
        Ok(after) => after.key == lookup.key,
        Err(error) => {
            log::warn!(target: "FRAME", "cannot recheck frame compile inputs after compiling {short}: {error:#}");
            false
        }
    };
    let artifact_path = artifact_dir.join(rock_host::project::MAIN_LUA_FILENAME);
    let compiled = CompiledGame {
        lua_file: output_path.to_path_buf(),
        artifact_fingerprint: artifact.artifact_fingerprint.clone(),
    };
    materialize(&artifact_path, output_path)?;
    record_cache_event_best_effort("compile", lookup, frame_count);
    if inputs_still_match {
        match write_manifest(&artifact_dir, lookup, &artifact) {
            Ok(()) => match crate::skull_backend::compile_cache_publish_dir(&artifact_dir, &entry, "manifest.json") {
                Ok(()) => {
                    if let Err(error) = crate::skull_backend::compile_cache_evict_to_budget(
                        root, budget, &entry, "manifest.json", STAGING_PREFIX, EVICTING_PREFIX, STALE_STAGING,
                    ) {
                        log::warn!(target: "FRAME", "frame compile cache eviction failed: {error:#}");
                    }
                    record_cache_event_best_effort("store", lookup, frame_count);
                }
                Err(error) => log::warn!(target: "FRAME", "cannot publish frame compile cache entry {short}: {error:#}"),
            },
            Err(error) => log::warn!(target: "FRAME", "cannot write frame compile cache manifest {short}: {error:#}"),
        }
    } else {
        log::info!(target: "FRAME", "frame compile inputs changed while compiling {short}; artifact not cached");
        record_cache_event_best_effort("input_changed", lookup, frame_count);
    }
    Ok(compiled)
}

pub(super) fn compile_game_for_frame(
    backend: Backend,
    path: &str,
    output: &str,
    frame_count: usize,
    requested_renderer: Option<rock_host::stick::RendererBackend>,
) -> Result<CompiledGame> {
    let project = Path::new(path).canonicalize().with_context(|| format!("canonicalize game {path}"))?;
    let project_path = project.to_string_lossy();
    let Some(root) = cache_root()? else {
        return crate::common::compile_game_for_run(backend, &project_path, output);
    };
    crate::common::prepare_game_compile(&project_path)?;
    // The lookup resolves what the compile would: discovery, build vars.
    let lookup = Lookup::current(&project, backend, requested_renderer).inspect_err(crate::common::record_compilation_error)?;
    let output_path = PathBuf::from(output).join(rock_host::project::MAIN_LUA_FILENAME);
    let compiled = prepare_cached_artifact(
        &root,
        &lookup,
        &output_path,
        frame_count,
        |artifact_dir| compile_game_into(backend, &project_path, artifact_dir),
    )?;
    crate::common::clear_error_file();
    Ok(compiled)
}

#[cfg(test)]
mod tests {
    use super::*;

    static ERROR_FILE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_error_file_restored<T>(run: impl FnOnce() -> T) -> T {
        let _guard = ERROR_FILE_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        with_error_file_restored_locked(run)
    }

    fn with_error_file_restored_locked<T>(run: impl FnOnce() -> T) -> T {
        use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
        let path = PathBuf::from(crate::common::ERROR_FILE_PATH);
        let parent = path.parent().expect("tmp/error.txt has a parent");
        let parent_existed = parent.exists();
        let original = match std::fs::read(&path) {
            Ok(contents) => Some(contents),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => panic!("read tmp/error.txt before frame cache test: {error}"),
        };
        if !parent_existed {
            std::fs::create_dir_all(parent).expect("create tmp for frame cache test");
        }
        let result = catch_unwind(AssertUnwindSafe(run));
        match original {
            Some(contents) => std::fs::write(&path, contents).expect("restore tmp/error.txt after frame cache test"),
            None => match std::fs::remove_file(&path) {
                Ok(()) => {},
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
                Err(error) => panic!("remove tmp/error.txt after frame cache test: {error}"),
            },
        }
        if !parent_existed {
            match std::fs::remove_dir(parent) {
                Ok(()) => {},
                Err(error) => panic!("remove test-created tmp directory after frame cache test: {error}"),
            }
        }
        match result {
            Ok(value) => value,
            Err(panic) => resume_unwind(panic),
        }
    }

    fn project() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("stick.toml"), "[package]\nname = \"frame-cache-fixture\"\n").unwrap();
        std::fs::write(dir.path().join("main.rock"), "fn main() { print(1) }\n").unwrap();
        let root = dir.path().canonicalize().unwrap();
        (dir, root)
    }

    #[test]
    fn same_compile_input_uses_one_artifact_across_frame_counts_and_output_dirs() {
        let (_project_dir, project) = project();
        let cache = tempfile::tempdir().unwrap();
        let outputs = cache.path().join("outputs");
        let stats = cache.path().join("events.jsonl");
        let first_output = outputs.join("one");
        let second_output = outputs.join("many");
        run_frame_cache_worker(&project, cache.path(), &stats, 1, &first_output);
        run_frame_cache_worker(&project, cache.path(), &stats, 600, &second_output);

        let events = std::fs::read_to_string(stats).unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        let names = events.iter().map(|row| row["event"].as_str().unwrap()).collect::<Vec<_>>();
        assert_eq!(names, ["miss", "compile", "store", "hit"]);
    }

    fn run_frame_cache_worker(
        project: &Path,
        cache: &Path,
        stats: &Path,
        frame_count: usize,
        output: &Path,
    ) {
        let rock_dir = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2).unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "stone::frame_compile_cache::tests::frame_cache_wrapper_worker", "--nocapture"])
            .current_dir(project)
            .env("ROCK_DIR", rock_dir)
            .env("ROCK_BUILD_COMMAND", "frame")
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
        assert!(status.success(), "production frame compiler worker failed at {frame_count} frames");
    }

    #[test]
    fn frame_cache_wrapper_worker() {
        if std::env::var_os("FRAME_CACHE_WRAPPER_TEST").is_none() {
            return;
        }
        with_error_file_restored(|| {
            let project = PathBuf::from(std::env::var_os("FRAME_CACHE_PROJECT").unwrap())
                .canonicalize().unwrap();
            let output = PathBuf::from(std::env::var_os("FRAME_CACHE_OUTPUT").unwrap());
            let frame_count = std::env::var("FRAME_CACHE_FRAME_COUNT").unwrap().parse::<usize>().unwrap();
            let compiled = compile_game_for_frame(
                Backend::Skull,
                &project.to_string_lossy(),
                &output.to_string_lossy(),
                frame_count,
                None,
            ).unwrap();
            let oracle_dir = tempfile::tempdir().unwrap();
            let oracle = compile_game_into(Backend::Skull, &project.to_string_lossy(), oracle_dir.path()).unwrap();
            let expected = std::fs::read(oracle_dir.path().join(rock_host::project::MAIN_LUA_FILENAME)).unwrap();
            assert_eq!(std::fs::read(&compiled.lua_file).unwrap().as_slice(), expected.as_slice());
            assert_eq!(compiled.artifact_fingerprint.as_str(), oracle.artifact_fingerprint.as_str());
        });
    }

    #[test]
    fn source_edit_misses_the_frame_compile_cache() {
        with_error_file_restored(|| {
            let (_project_dir, project) = project();
            let cache = tempfile::tempdir().unwrap();
            let first_lookup = Lookup::current(&project, Backend::Skull, None).unwrap();
            let first_output = tempfile::tempdir().unwrap();
            let first = prepare_cached_artifact(
                cache.path(), &first_lookup, &first_output.path().join("main.lua"), 1,
                |dir| compile_game_into(Backend::Skull, &project.to_string_lossy(), dir),
            ).unwrap();
            std::fs::write(
                project.join("main.rock"),
                "fn main() { print(2) }\n",
            ).unwrap();
            let changed_lookup = Lookup::current(&project, Backend::Skull, None).unwrap();
            let second_output = tempfile::tempdir().unwrap();
            let second = prepare_cached_artifact(
                cache.path(), &changed_lookup, &second_output.path().join("main.lua"), 1,
                |dir| compile_game_into(Backend::Skull, &project.to_string_lossy(), dir),
            ).unwrap();
            let oracle_dir = tempfile::tempdir().unwrap();
            let oracle = compile_game_into(Backend::Skull, &project.to_string_lossy(), oracle_dir.path()).unwrap();
            let expected = std::fs::read(oracle_dir.path().join(rock_host::project::MAIN_LUA_FILENAME)).unwrap();
            assert_eq!(std::fs::read(&second.lua_file).unwrap().as_slice(), expected.as_slice(), "the changed source emits the production compiler's current Lua");
            assert_ne!(std::fs::read(&first.lua_file).unwrap().as_slice(), expected.as_slice(), "a stale artifact from the prior source cannot pass as the changed compile");
            assert_eq!(second.artifact_fingerprint.as_str(), oracle.artifact_fingerprint.as_str());
        });
    }

    #[test]
    fn build_var_override_misses_the_frame_compile_cache() {
        let project_dir = tempfile::tempdir().unwrap();
        let project = project_dir.path().canonicalize().unwrap();
        std::fs::write(
            project.join("stick.toml"),
            "[package]\nname = \"frame-cache-var-fixture\"\n[build.vars]\nMODE = { value = \"default\" }\n",
        ).unwrap();
        std::fs::write(project.join("main.rock"), "fn main() { print(build_var(\"MODE\")) }\n").unwrap();
        let cache = tempfile::tempdir().unwrap();
        let one_output = cache.path().join("output-one/main.lua");
        let two_output = cache.path().join("output-two/main.lua");
        run_build_var_cache_worker(&project, cache.path(), "one", &one_output);
        run_build_var_cache_worker(&project, cache.path(), "two", &two_output);
        let one = std::fs::read(one_output).unwrap();
        let two = std::fs::read(two_output).unwrap();
        assert_ne!(one, two, "the production compiler emits the selected --var value");
    }

    fn run_build_var_cache_worker(project: &Path, cache: &Path, value: &str, output: &Path) {
        let rock_dir = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2).unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "stone::frame_compile_cache::tests::build_var_cache_worker", "--nocapture"])
            .current_dir(project)
            .env("ROCK_DIR", rock_dir)
            .env("FRAME_CACHE_TEST_VAR", value)
            .env("FRAME_CACHE_TEST_PROJECT", project)
            .env("FRAME_CACHE_TEST_ROOT", cache)
            .env("FRAME_CACHE_TEST_OUTPUT", output)
            .status()
            .unwrap();
        assert!(status.success(), "production cache worker for --var MODE={value} failed");
    }

    #[test]
    fn build_var_cache_worker() {
        let Ok(value) = std::env::var("FRAME_CACHE_TEST_VAR") else { return };
        with_error_file_restored(|| {
            let project = PathBuf::from(std::env::var_os("FRAME_CACHE_TEST_PROJECT").unwrap())
                .canonicalize().unwrap();
            let root = PathBuf::from(std::env::var_os("FRAME_CACHE_TEST_ROOT").unwrap());
            let output = PathBuf::from(std::env::var_os("FRAME_CACHE_TEST_OUTPUT").unwrap());
            crate::comptime::set_invocation_selection(None, None, None, &[format!("MODE={value}")]).unwrap();
            let lookup = Lookup::current(&project, Backend::Skull, None).unwrap();
            let compiled = prepare_cached_artifact(
                &root,
                &lookup,
                &output,
                1,
                |dir| compile_game_into(Backend::Skull, &project.to_string_lossy(), dir),
            ).unwrap();
            let oracle_dir = tempfile::tempdir().unwrap();
            let oracle = compile_game_into(Backend::Skull, &project.to_string_lossy(), oracle_dir.path()).unwrap();
            let expected = std::fs::read(oracle_dir.path().join(rock_host::project::MAIN_LUA_FILENAME)).unwrap();
            assert_eq!(std::fs::read(&compiled.lua_file).unwrap().as_slice(), expected.as_slice(), "cached output agrees with a fresh production compile for --var MODE={value}");
            assert_eq!(compiled.artifact_fingerprint.as_str(), oracle.artifact_fingerprint.as_str());
        });
    }

    #[test]
    fn backend_change_misses_the_frame_compile_cache() {
        with_error_file_restored(|| {
            let (_project_dir, project) = project();
            let cache = tempfile::tempdir().unwrap();
            let skull = Lookup::current(&project, Backend::Skull, None).unwrap();
            let output = tempfile::tempdir().unwrap();
            let compiled = prepare_cached_artifact(
                cache.path(), &skull, &output.path().join("main.lua"), 1,
                |dir| compile_game_into(Backend::Skull, &project.to_string_lossy(), dir),
            ).unwrap();
            let oracle_dir = tempfile::tempdir().unwrap();
            let oracle = compile_game_into(Backend::Skull, &project.to_string_lossy(), oracle_dir.path()).unwrap();
            assert_eq!(std::fs::read(&compiled.lua_file).unwrap().as_slice(), std::fs::read(oracle_dir.path().join(rock_host::project::MAIN_LUA_FILENAME)).unwrap().as_slice());
            assert_eq!(compiled.artifact_fingerprint.as_str(), oracle.artifact_fingerprint.as_str());

            let changed_backend = Lookup::current(&project, Backend::C, None).unwrap();
            assert_ne!(skull.key, changed_backend.key, "the production compile identity includes its backend");
            let changed_output = tempfile::tempdir().unwrap();
            let result = prepare_cached_artifact(
                cache.path(), &changed_backend, &changed_output.path().join("main.lua"), 1,
                |_| Err(anyhow!("C backend compile path reached after cache miss")),
            );
            assert!(result.is_err(), "a backend change reaches its own compile path");
        });
    }

    #[test]
    fn renderer_change_misses_the_frame_compile_cache() {
        if !cfg!(feature = "renderer-wgpu") {
            return;
        }
        with_error_file_restored(|| {
            let (_project_dir, project) = project();
            let cache = tempfile::tempdir().unwrap();
            let gl_guard = rock_host::stick::install_renderer_override(Some(rock_host::stick::RendererBackend::Gl));
            let gl = Lookup::current(&project, Backend::Skull, Some(rock_host::stick::RendererBackend::Gl)).unwrap();
            let first_output = tempfile::tempdir().unwrap();
            let first = prepare_cached_artifact(
                cache.path(), &gl, &first_output.path().join("main.lua"), 1,
                |dir| compile_game_into(Backend::Skull, &project.to_string_lossy(), dir),
            ).unwrap();

            let gl_oracle_dir = tempfile::tempdir().unwrap();
            let gl_oracle = compile_game_into(Backend::Skull, &project.to_string_lossy(), gl_oracle_dir.path()).unwrap();
            let gl_expected = std::fs::read(gl_oracle_dir.path().join(rock_host::project::MAIN_LUA_FILENAME)).unwrap();
            assert_eq!(std::fs::read(&first.lua_file).unwrap().as_slice(), gl_expected.as_slice());
            assert_eq!(first.artifact_fingerprint.as_str(), gl_oracle.artifact_fingerprint.as_str());
            drop(gl_guard);

            let wgpu_guard = rock_host::stick::install_renderer_override(Some(rock_host::stick::RendererBackend::Wgpu));
            let wgpu = Lookup::current(&project, Backend::Skull, Some(rock_host::stick::RendererBackend::Wgpu)).unwrap();
            let second_output = tempfile::tempdir().unwrap();
            let second = prepare_cached_artifact(
                cache.path(), &wgpu, &second_output.path().join("main.lua"), 1,
                |dir| compile_game_into(Backend::Skull, &project.to_string_lossy(), dir),
            ).unwrap();
            let oracle_dir = tempfile::tempdir().unwrap();
            let oracle = compile_game_into(Backend::Skull, &project.to_string_lossy(), oracle_dir.path()).unwrap();
            let expected = std::fs::read(oracle_dir.path().join(rock_host::project::MAIN_LUA_FILENAME)).unwrap();
            assert_eq!(std::fs::read(&second.lua_file).unwrap().as_slice(), expected.as_slice());
            assert_eq!(second.artifact_fingerprint.as_str(), oracle.artifact_fingerprint.as_str());
            assert_ne!(gl_expected.as_slice(), expected.as_slice(), "the configured renderer changes emitted production Lua");
            drop(wgpu_guard);
        });
    }

    #[test]
    fn tampered_manifest_hash_is_rejected_and_recompiled() {
        with_error_file_restored(|| {
            let (_project_dir, project) = project();
            let cache = tempfile::tempdir().unwrap();
            let lookup = Lookup::current(&project, Backend::Skull, None).unwrap();
            let entry = cache.path().join(&lookup.key);
            let first_output = tempfile::tempdir().unwrap();
            let first = prepare_cached_artifact(
                cache.path(), &lookup, &first_output.path().join("main.lua"), 1,
                |dir| compile_game_into(Backend::Skull, &project.to_string_lossy(), dir),
            ).unwrap();
            let oracle_dir = tempfile::tempdir().unwrap();
            let oracle = compile_game_into(Backend::Skull, &project.to_string_lossy(), oracle_dir.path()).unwrap();
            let expected = std::fs::read(oracle_dir.path().join(rock_host::project::MAIN_LUA_FILENAME)).unwrap();
            assert_eq!(std::fs::read(&first.lua_file).unwrap().as_slice(), expected.as_slice());
            assert_eq!(first.artifact_fingerprint.as_str(), oracle.artifact_fingerprint.as_str());

            let manifest_path = entry.join("manifest.json");
            let mut manifest: Value = serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
            manifest["artifact_fingerprint"] = json!("tampered-fingerprint");
            std::fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
            let repaired_manifest_output = tempfile::tempdir().unwrap();
            let repaired_manifest = prepare_cached_artifact(
                cache.path(), &lookup, &repaired_manifest_output.path().join("main.lua"), 1,
                |dir| compile_game_into(Backend::Skull, &project.to_string_lossy(), dir),
            ).unwrap();
            assert_eq!(std::fs::read(&repaired_manifest.lua_file).unwrap().as_slice(), expected.as_slice(), "a changed manifest is rejected before its fingerprint is reused");
            assert_eq!(repaired_manifest.artifact_fingerprint.as_str(), oracle.artifact_fingerprint.as_str());

            std::fs::write(entry.join(rock_host::project::MAIN_LUA_FILENAME), b"tampered main.lua\n").unwrap();
            let repaired_hash_output = tempfile::tempdir().unwrap();
            let repaired_hash = prepare_cached_artifact(
                cache.path(), &lookup, &repaired_hash_output.path().join("main.lua"), 1,
                |dir| compile_game_into(Backend::Skull, &project.to_string_lossy(), dir),
            ).unwrap();
            assert_eq!(std::fs::read(&repaired_hash.lua_file).unwrap().as_slice(), expected.as_slice(), "a cached main.lua hash mismatch cannot reach the run output");
            assert_eq!(repaired_hash.artifact_fingerprint.as_str(), oracle.artifact_fingerprint.as_str());
        });
    }

    #[test]
    fn cache_miss_wipes_a_stale_incremental_overlay() {
        with_error_file_restored(|| {
            let (_project_dir, project) = project();
            let cache = tempfile::tempdir().unwrap();
            let output = tempfile::tempdir().unwrap();
            let overlay = output.path().join("incremental.lua");
            std::fs::write(&overlay, b"-- stale overlay\n").unwrap();
            let lookup = Lookup::current(&project, Backend::Skull, None).unwrap();
            prepare_cached_artifact(
                cache.path(), &lookup, &output.path().join("main.lua"), 1,
                |dir| compile_game_into(Backend::Skull, &project.to_string_lossy(), dir),
            ).unwrap();
            assert!(!overlay.exists(), "a fresh main.lua never runs beside an older overlay");
        });
    }

    #[test]
    fn entry_is_keyed_on_the_build_vars_the_compile_stamped() {
        with_error_file_restored(|| {
            let project_dir = tempfile::tempdir().unwrap();
            let project = project_dir.path().canonicalize().unwrap();
            std::fs::write(
                project.join("stick.toml"),
                "[package]\nname = \"frame-cache-stamp-fixture\"\n[build.vars]\nMODE = { cmd = \"cat mode.txt\" }\n",
            ).unwrap();
            std::fs::write(project.join("main.rock"), "fn main() { print(build_var(\"MODE\")) }\n").unwrap();
            let mode = project.join("mode.txt");
            std::fs::write(&mode, "a").unwrap();
            let cache = tempfile::tempdir().unwrap();
            let lookup = Lookup::current(&project, Backend::Skull, None).unwrap();
            let output = tempfile::tempdir().unwrap();
            prepare_cached_artifact(
                cache.path(), &lookup, &output.path().join("main.lua"), 1,
                |dir| {
                    // A non-monotonic var: B during the compile, A before and after.
                    std::fs::write(&mode, "b").unwrap();
                    let artifact = compile_game_into(Backend::Skull, &project.to_string_lossy(), dir);
                    std::fs::write(&mode, "a").unwrap();
                    artifact
                },
            ).unwrap();
            assert!(!cache.path().join(&lookup.key).exists(), "Lua built with MODE=b is never stored under the MODE=a key");
        });
    }

    #[test]
    fn lookup_failure_records_the_compile_error_file() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().canonicalize().unwrap();
        std::fs::write(
            project.join("stick.toml"),
            "[package]\nname = \"frame-cache-lookup-fixture\"\n[build.vars]\nBROKEN = { cmd = \"exit 3\" }\n",
        ).unwrap();
        std::fs::write(project.join("main.rock"), "fn main() { print(build_var(\"BROKEN\")) }\n").unwrap();
        std::fs::create_dir(project.join("tmp")).unwrap();
        let rock_dir = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2).unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "stone::frame_compile_cache::tests::lookup_failure_worker", "--nocapture"])
            .current_dir(&project)
            .env("ROCK_DIR", rock_dir)
            .env("FRAME_CACHE_LOOKUP_FAILURE_PROJECT", &project)
            .env("ROCK_FRAME_COMPILE_CACHE_DIR", dir.path().join("cache"))
            .env_remove("ROCK_FRAME_COMPILE_CACHE")
            .env_remove("SKULL_ALLOCATION_REPORT")
            .env_remove("SKULL_ALLOCATION_ACTIVATION")
            .status()
            .unwrap();
        assert!(status.success(), "lookup failure worker failed");
        let recorded = std::fs::read_to_string(project.join(crate::common::ERROR_FILE_PATH))
            .expect("a failed cache lookup writes tmp/error.txt like an uncached compile");
        assert!(recorded.contains("BROKEN"), "{recorded}");
    }

    #[test]
    fn lookup_failure_worker() {
        let Some(project) = std::env::var_os("FRAME_CACHE_LOOKUP_FAILURE_PROJECT") else { return };
        let project = PathBuf::from(project);
        let Err(error) = compile_game_for_frame(Backend::Skull, &project.to_string_lossy(), "tmp", 1, None) else {
            panic!("a failing build var fails the frame compile");
        };
        assert!(format!("{error:#}").contains("BROKEN"), "{error:#}");
    }

    #[test]
    fn error_file_guard_restores_state_when_test_unwinds() {
        let dir = tempfile::tempdir().unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "stone::frame_compile_cache::tests::error_file_guard_worker", "--nocapture"])
            .current_dir(dir.path())
            .env("FRAME_CACHE_ERROR_WORKER", "1")
            .status()
            .unwrap();
        assert!(status.success(), "isolated error-file cleanup probe failed");
    }

    #[test]
    fn error_file_guard_worker() {
        if std::env::var_os("FRAME_CACHE_ERROR_WORKER").is_none() {
            return;
        }
        let _guard = ERROR_FILE_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let path = PathBuf::from(crate::common::ERROR_FILE_PATH);
        let original = match std::fs::read(&path) {
            Ok(contents) => Some(contents),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => panic!("read tmp/error.txt before cleanup test: {error}"),
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            with_error_file_restored_locked(|| {
                std::fs::write(&path, b"intentional test failure").unwrap();
                panic!("intentional unwind to exercise test cleanup");
            })
        }));
        let _panic = result.expect_err("the cleanup probe must reach its intentional panic");
        let restored = match std::fs::read(&path) {
            Ok(contents) => Some(contents),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => panic!("read tmp/error.txt after cleanup test: {error}"),
        };
        assert_eq!(restored, original, "the error file is restored after a failed assertion path");
    }
}
