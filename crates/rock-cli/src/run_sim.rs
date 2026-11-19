//! Run Stone's @sim coroutines in fresh processes and read their __SIM__ stderr events.

use crate::sim_observability::{FrameTiming, SimRunRecorder};
use crate::skull_backend::{IncrementalProject, collect_sim_descriptors};
use anyhow::{Context, Result, anyhow};
use colored::Colorize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;
use tempfile::TempDir;

#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;

const DEFAULT_FRAME_BUDGET: usize = 600;

fn sim_env_value<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
    env.iter().rfind(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

fn frame_budget_for(sim_name: &str, env_pairs: &[(String, String)]) -> Result<usize> {
    let Some(raw) = sim_env_value(env_pairs, "FRAME_COUNT") else {
        return Ok(DEFAULT_FRAME_BUDGET);
    };
    raw.parse::<usize>().ok().filter(|n| *n > 0).ok_or_else(|| {
        anyhow!("sim `{sim_name}` declares @sim_env(\"FRAME_COUNT\", \"{raw}\"), which is not a positive integer")
    })
}

fn timeout_for(sim_name: &str, env_pairs: &[(String, String)]) -> Result<Duration> {
    let Some(raw) = sim_env_value(env_pairs, "ROCK_SIM_TIMEOUT_SECS") else {
        return Ok(Duration::from_secs(sim_timeout_secs()));
    };
    raw.parse::<u64>().ok().filter(|n| *n > 0).map(Duration::from_secs).ok_or_else(|| {
        anyhow!("sim `{sim_name}` declares @sim_env(\"ROCK_SIM_TIMEOUT_SECS\", \"{raw}\"), which is not a positive integer")
    })
}

const DEFAULT_TIMEOUT_SECS: u64 = 300;

const DEFAULT_MEMORY_CAP_MB: u64 = 8192;

#[derive(Clone, Debug, PartialEq, serde::Deserialize)]
struct AllocationCensus {
    build_sha: String,
    rendering_enabled: bool,
    warmup_frames_requested: usize,
    warmup_frames: usize,
    frame_count: usize,
    measured_frames: usize,
    initialization_gross_kb: f64,
    outside_frame_gross_kb: f64,
    warmup_gross_kb: f64,
    measured_gross_kb: f64,
    total_gross_kb: f64,
    measured_allocator_calls: u64,
    measured_allocation_calls: u64,
    measured_reallocation_calls: u64,
    stop_sim_reached: bool,
    measurement_complete: bool,
}

fn sim_timeout_secs() -> u64 {
    match std::env::var("ROCK_SIM_TIMEOUT_SECS") {
        Ok(v) => match v.parse::<u64>() {
            Ok(n) if n > 0 => n,
            _ => {
                log::warn!(target: "SIM",
                    "ROCK_SIM_TIMEOUT_SECS=`{v}` is not a positive integer; \
                     using {DEFAULT_TIMEOUT_SECS}s");
                DEFAULT_TIMEOUT_SECS
            }
        },
        Err(_) => DEFAULT_TIMEOUT_SECS,
    }
}

fn sim_memory_cap_mb() -> Option<u64> {
    match std::env::var("ROCK_SIM_MEMORY_MB") {
        Ok(v) => match v.parse::<u64>() {
            Ok(0) => None,
            Ok(n) => Some(n),
            Err(_) => {
                log::warn!(target: "SIM",
                    "ROCK_SIM_MEMORY_MB=`{v}` is not a nonnegative integer; \
                     using {DEFAULT_MEMORY_CAP_MB} MB");
                Some(DEFAULT_MEMORY_CAP_MB)
            }
        },
        Err(_) => Some(DEFAULT_MEMORY_CAP_MB),
    }
}

/// A child still inside exec can share the launcher's address space.
#[cfg(target_os = "linux")]
fn process_memory_mb(pid: u32) -> Option<u64> {
    // Checked before the read: once the child's mm differs it never reverts.
    if shares_runner_address_space(pid) {
        return None;
    }
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let mut kb = 0u64;
    for line in status.lines() {
        if let Some(rest) = line
            .strip_prefix("VmRSS:")
            .or_else(|| line.strip_prefix("VmSwap:"))
        {
            kb += rest
                .trim()
                .trim_end_matches("kB")
                .trim()
                .parse::<u64>()
                .ok()?;
        }
    }
    Some(kb / 1024)
}

#[cfg(target_os = "linux")]
fn shares_runner_address_space(pid: u32) -> bool {
    const KCMP_VM: libc::c_int = 1;
    // SAFETY: scalar-only syscall.
    let order = unsafe {
        libc::syscall(
            libc::SYS_kcmp,
            libc::getpid(),
            pid as libc::pid_t,
            KCMP_VM,
            0usize,
            0usize,
        )
    };
    if order < 0 {
        // Without kcmp the cap still measures; only the execve window is unguarded.
        static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            log::warn!(target: "SIM",
                "kcmp failed ({}); the sim memory cap may read a child still inside execve as the runner",
                std::io::Error::last_os_error());
        }
        return false;
    }
    order == 0
}

#[cfg(target_os = "linux")]
fn process_memory_breakdown(pid: u32) -> String {
    let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
        return "breakdown unavailable".to_string();
    };
    status
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            if key == "Name" {
                return Some(format!("process {}", value.trim()));
            }
            if !matches!(
                key,
                "RssAnon" | "RssFile" | "RssShmem" | "VmSwap" | "VmSize"
            ) {
                return None;
            }
            let kb = value
                .trim()
                .trim_end_matches("kB")
                .trim()
                .parse::<u64>()
                .ok()?;
            Some(format!("{key} {} MB", kb / 1024))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(not(target_os = "linux"))]
fn process_memory_breakdown(_pid: u32) -> String {
    "breakdown unavailable".to_string()
}

// Only Linux exposes this cheaply (/proc); elsewhere the cap is not enforced
// and the wall-clock watchdog remains the only bound.
#[cfg(not(target_os = "linux"))]
fn process_memory_mb(_pid: u32) -> Option<u64> {
    None
}

/// Exact wire names win. Otherwise a unique trailing sequence of scope segments selects the sim.
pub(crate) fn resolve_sim_specs(wire_names: &[String], specs: &[String]) -> Result<Vec<String>> {
    let mut problems = Vec::new();
    let mut resolved = Vec::with_capacity(specs.len());
    for spec in specs {
        match resolve_sim_name(wire_names, spec) {
            Ok(wire) => resolved.push(wire),
            Err(problem) => problems.push(problem),
        }
    }
    if problems.is_empty() {
        return Ok(resolved);
    }
    Err(anyhow!(
        "{} requested sim name{} not runnable (list sims with `rock sim --list <project>`):\n  {}",
        problems.len(),
        if problems.len() == 1 { " is" } else { "s are" },
        problems.join("\n  ")
    ))
}

fn resolve_sim_name(wire_names: &[String], name: &str) -> std::result::Result<String, String> {
    if name.contains(char::is_whitespace) {
        return Err(format!(
            "sim name {name:?} contains whitespace; pass each sim name as its own argument"
        ));
    }
    if name.contains(',') {
        return Err(format!(
            "sim name `{name}` contains a comma; pass each sim name as its own argument"
        ));
    }
    if wire_names.iter().any(|wire| wire == name) {
        return Ok(name.to_string());
    }
    let suffix = format!("::{name}");
    let candidates: Vec<&str> = wire_names
        .iter()
        .filter(|wire| wire.ends_with(&suffix))
        .map(String::as_str)
        .collect();
    match candidates.as_slice() {
        [] => Err(format!("unknown sim: {name}")),
        [only] => Ok((*only).to_string()),
        many => Err(format!(
            "sim name `{name}` is ambiguous; name one of: {}",
            many.join(", ")
        )),
    }
}

fn sim_output_dir() -> Result<TempDir> {
    fs_err::create_dir_all("tmp").context("creating tmp/ for sim output")?;
    let mut dir = tempfile::Builder::new()
        .prefix("sim-")
        .tempdir_in("tmp")
        .context("creating sim output directory")?;
    if std::env::var_os("ROCK_SIM_KEEP_BUILD").is_some() {
        dir.disable_cleanup(true);
        ::log::info!(target: "SIM", "retaining sim build at {}", dir.path().display());
    }
    Ok(dir)
}

fn compile_sims(project_path: PathBuf, output_path: PathBuf) -> Result<CompiledSims> {
    compile_sims_with_allocation(project_path, output_path, None, &[])
}

const SIM_COMPILE_RECEIPT_DIR_ENV: &str = "ROCK_SIM_COMPILE_RECEIPT_DIR";
const SIM_COMPILE_RECEIPT_PREPARE_ENV: &str = "ROCK_SIM_COMPILE_RECEIPT_PREPARE";
const SIM_COMPILE_RECEIPT_VERSION: u64 = 4;
const SIM_COMPILE_SELECTION_ENV: &str = "ROCK_SIM_COMPILE_SELECTION";

fn normalized_sim_selection(specs: &[String]) -> Vec<String> {
    let mut names = specs.to_vec();
    names.sort();
    names.dedup();
    names
}

fn child_sim_selection() -> Result<Vec<String>> {
    match std::env::var(SIM_COMPILE_SELECTION_ENV) {
        Ok(value) => serde_json::from_str::<Vec<String>>(&value)
            .map(|names| normalized_sim_selection(&names))
            .context("parse sim compile selection"),
        Err(std::env::VarError::NotPresent) => Ok(Vec::new()),
        Err(error) => Err(error).context("read sim compile selection"),
    }
}

mod events;
mod worker_admission;

use events::{SimEvent, parse_sim_marker};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct SimReceiptInput {
    kind: String,
    path: String,
    sha256: String,
}

struct PreparedSims {
    registry: SimRegistry,
    stone: Option<crate::skull_backend::IncrementalProject>,
}

#[derive(Debug)]
struct SimRegistry {
    wire_names: Vec<String>,
    env_by_wire: HashMap<String, Vec<(String, String)>>,
    source_files: HashMap<String, String>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct RegisteredSim {
    wire_name: String,
    env: Vec<(String, String)>,
    #[serde(default)]
    source_file: String,
}

impl SimRegistry {
    fn select(&self, project: &str, names: &[String]) -> Result<Vec<String>> {
        let selected = if names.is_empty() {
            self.wire_names.clone()
        } else {
            resolve_sim_specs(&self.wire_names, names)?
        };
        if selected.is_empty() {
            return Err(anyhow!("no @sim coroutines found in project `{project}`"));
        }
        Ok(selected)
    }
}

impl From<Vec<RegisteredSim>> for SimRegistry {
    fn from(entries: Vec<RegisteredSim>) -> Self {
        let mut wire_names = Vec::with_capacity(entries.len());
        let mut env_by_wire = HashMap::with_capacity(entries.len());
        let mut source_files = HashMap::with_capacity(entries.len());
        for entry in entries {
            env_by_wire.insert(entry.wire_name.clone(), entry.env);
            source_files.insert(entry.wire_name.clone(), entry.source_file);
            wire_names.push(entry.wire_name);
        }
        Self {
            wire_names,
            env_by_wire,
            source_files,
        }
    }
}

struct CompiledSims {
    registry: Vec<RegisteredSim>,
    stone: crate::skull_backend::IncrementalProject,
    selection_is_full: bool,
}

/// Resolve the receipt's workspace-relative sim sources against this
/// invocation's source root for baseline lookup. Timings keep the relative
/// keys (stable across machines); the runner needs on-disk paths.
fn resolve_sim_sources(
    project_path: &Path,
    relative: &HashMap<String, String>,
) -> Result<HashMap<String, PathBuf>> {
    let root = source_root_for_sim_project(project_path)?;
    Ok(relative
        .iter()
        .map(|(wire, rel)| (wire.clone(), root.join(rel)))
        .collect())
}

fn sha256_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn sim_receipt_project_dir(root: &Path, project: &Path) -> PathBuf {
    root.join(sha256_bytes(project.to_string_lossy().as_bytes()))
}

fn canonical_string(path: &Path) -> Result<String> {
    Ok(path
        .canonicalize()
        .with_context(|| format!("canonicalize {}", path.display()))?
        .to_string_lossy()
        .into_owned())
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

fn receipt_source_inputs(
    project: &Path,
    sources: &rock_core::SourceFiles,
) -> Result<Vec<SimReceiptInput>> {
    let mut inputs = Vec::new();
    let mut manifests = std::collections::BTreeSet::new();
    manifests.extend(nearest_manifest(project));
    for source in &sources.files {
        let source = source.borrow();
        let path = PathBuf::from(source.file.to_id_path());
        inputs.push(SimReceiptInput {
            kind: "source".into(),
            path: canonical_string(&path)?,
            sha256: sha256_bytes(source.contents.as_bytes()),
        });
        manifests.extend(nearest_manifest(&path));
    }
    for path in manifests {
        inputs.push(SimReceiptInput {
            kind: "manifest".into(),
            path: canonical_string(&path)?,
            sha256: hash_file(&path)?,
        });
    }
    Ok(inputs)
}

fn hash_file(path: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    std::io::copy(&mut fs_err::File::open(path)?, &mut hasher)
        .with_context(|| format!("hash {}", path.display()))?;
    Ok(format!("{:x}", hasher.finalize()))
}

fn receipt_inputs_from_stone(
    project: &Path,
    stone: &crate::skull_backend::IncrementalProject,
) -> Result<Vec<SimReceiptInput>> {
    let mut inputs = receipt_source_inputs(project, stone.sources())?;
    for (path, _) in stone.comptime_watch_files_with_hashes() {
        inputs.push(SimReceiptInput {
            kind: "embedded".into(),
            path: canonical_string(&path)?,
            sha256: hash_file(&path)?,
        });
    }
    inputs.sort_by(|a, b| (&a.kind, &a.path).cmp(&(&b.kind, &b.path)));
    Ok(inputs)
}

fn sim_receipt_context(project: &Path) -> Result<serde_json::Value> {
    const ENV_KEYS: &[&str] = &[
        "ROCK_ASSET_OVERLAYS",
        "ROCK_CONTEXT_ROOT",
        "ROCK_SOURCE_REVISION",
        "ROCK_STDLIB_DIR",
        "ROCK_SKULL_LINE_DIRECTIVES",
    ];
    let env = ENV_KEYS
        .iter()
        .map(|key| {
            (
                (*key).to_string(),
                std::env::var_os(key).map(|value| value.to_string_lossy().into_owned()),
            )
        })
        .collect::<BTreeMap<_, _>>();
    Ok(serde_json::json!({
        "project": canonical_string(project)?,
        "environment": env,
    }))
}

fn receipt_rows<T: serde::de::DeserializeOwned>(
    value: &serde_json::Value,
    field: &str,
) -> Result<Vec<T>> {
    let rows = value
        .as_array()
        .with_context(|| format!("sim compile receipt {field} is not an array"))?;
    rows.iter()
        .enumerate()
        .map(|(index, row)| {
            anyhow::ensure!(
                row.is_object(),
                "sim compile receipt {field}[{index}] is not an object"
            );
            serde_path_to_error::deserialize(row)
                .with_context(|| format!("invalid sim compile receipt {field}[{index}]"))
        })
        .collect()
}

fn parse_registry(value: &serde_json::Value) -> Result<SimRegistry> {
    let entries = receipt_rows::<RegisteredSim>(value, "registry")?;
    for entry in &entries {
        anyhow::ensure!(
            !entry.source_file.is_empty(),
            "sim compile receipt source file for `{}` is missing or invalid",
            entry.wire_name
        );
    }
    Ok(entries.into())
}

fn publish_sim_receipt(
    root: &Path,
    receipt_dir: &Path,
    project: &Path,
    output: &Path,
    registry: &[RegisteredSim],
    inputs: &[SimReceiptInput],
    selection: &[String],
) -> Result<()> {
    std::fs::create_dir_all(root)
        .with_context(|| format!("create sim compile receipt root {}", root.display()))?;
    let staged = tempfile::Builder::new()
        .prefix(".sim-receipt-")
        .tempdir_in(root)
        .with_context(|| format!("stage sim compile receipt under {}", root.display()))?;
    let artifact = rock_host::project::MAIN_LUA_FILENAME;
    let staged_main = staged.path().join(artifact);
    fs_err::copy(output.join(artifact), &staged_main)?;
    let manifest = serde_json::json!({
        "schema_version": SIM_COMPILE_RECEIPT_VERSION,
        "context": sim_receipt_context(project)?,
        "selection": selection,
        "main_sha256": hash_file(&staged_main)?,
        "inputs": inputs,
        "registry": registry,
    });
    let mut manifest_bytes =
        serde_json::to_vec_pretty(&manifest).context("serialize sim compile receipt")?;
    manifest_bytes.push(b'\n');
    std::fs::write(staged.path().join("receipt.json"), manifest_bytes)
        .context("write staged sim compile receipt manifest")?;
    let staged_path = staged.keep();
    std::fs::rename(&staged_path, receipt_dir).with_context(|| {
        format!(
            "publish sim compile receipt {} -> {}",
            staged_path.display(),
            receipt_dir.display()
        )
    })?;
    Ok(())
}

fn load_sim_receipt(receipt_dir: &Path, project: &Path, output: &Path) -> Result<SimRegistry> {
    let manifest_path = receipt_dir.join("receipt.json");
    let manifest_bytes = std::fs::read(&manifest_path)
        .with_context(|| format!("read sim compile receipt {}", manifest_path.display()))?;
    let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes)
        .with_context(|| format!("parse sim compile receipt {}", manifest_path.display()))?;
    let version = manifest
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| anyhow!("sim compile receipt has no numeric schema version"))?;
    if version != SIM_COMPILE_RECEIPT_VERSION {
        return Err(anyhow!(
            "sim compile receipt schema version {version} is not supported (expected {SIM_COMPILE_RECEIPT_VERSION})"
        ));
    }
    let expected_context = manifest
        .get("context")
        .ok_or_else(|| anyhow!("sim compile receipt has no context"))?;
    let current_context = sim_receipt_context(project)?;
    let mut current_inputs = receipt_source_inputs(
        project,
        &rock_host::compilation_units::load_sources_for_project(project)?,
    )?;
    if expected_context != &current_context {
        return Err(anyhow!(
            "sim compile receipt context changed; prepare a new receipt for this test lane"
        ));
    }

    let expected_inputs = receipt_rows::<SimReceiptInput>(
        manifest
            .get("inputs")
            .ok_or_else(|| anyhow!("sim compile receipt has no inputs"))?,
        "inputs",
    )?;
    for expected in expected_inputs
        .iter()
        .filter(|input| input.kind == "embedded")
    {
        current_inputs.push(SimReceiptInput {
            kind: expected.kind.clone(),
            path: expected.path.clone(),
            sha256: hash_file(Path::new(&expected.path))?,
        });
    }
    current_inputs.sort_by(|a, b| (&a.kind, &a.path).cmp(&(&b.kind, &b.path)));
    if expected_inputs != current_inputs {
        return Err(anyhow!(
            "sim compile receipt inputs changed; prepare a new receipt for this test lane"
        ));
    }

    let main_path = receipt_dir.join(rock_host::project::MAIN_LUA_FILENAME);
    let expected_main_hash = manifest
        .get("main_sha256")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow!("sim compile receipt has no main artifact hash"))?;
    let materialized = output.join(rock_host::project::MAIN_LUA_FILENAME);
    fs_err::copy(main_path, &materialized)?;
    if hash_file(&materialized)? != expected_main_hash {
        return Err(anyhow!(
            "sim compile receipt main.lua hash does not match its manifest"
        ));
    }
    let incremental = output.join("incremental.lua");
    if incremental.exists() {
        std::fs::remove_file(&incremental)
            .with_context(|| format!("remove stale sim overlay {}", incremental.display()))?;
    }
    parse_registry(
        manifest
            .get("registry")
            .ok_or_else(|| anyhow!("sim compile receipt has no registry"))?,
    )
}

fn prepare_sims(project_path: PathBuf, output_path: PathBuf) -> Result<PreparedSims> {
    crate::package::check_project_assets(&project_path)?;
    let selection = child_sim_selection()?;
    let Some(root) = std::env::var_os(SIM_COMPILE_RECEIPT_DIR_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
    else {
        let compiled = compile_sims_with_allocation(project_path, output_path, None, &selection)?;
        return Ok(PreparedSims {
            registry: compiled.registry.into(),
            stone: Some(compiled.stone),
        });
    };
    let project_path = project_path.canonicalize().with_context(|| {
        format!(
            "canonicalize sim receipt project {}",
            project_path.display()
        )
    })?;
    let receipt_dir = sim_receipt_project_dir(&root, &project_path);
    if receipt_dir.is_dir() {
        return consume_sim_receipt(&receipt_dir, &project_path, &output_path);
    }
    if std::env::var_os(SIM_COMPILE_RECEIPT_PREPARE_ENV).as_deref()
        != Some(std::ffi::OsStr::new("1"))
    {
        return Err(anyhow!(
            "no sim compile receipt for `{}` under {}; prepare this test lane first",
            project_path.display(),
            root.display()
        ));
    }
    let compiled =
        compile_sims_with_allocation(project_path.clone(), output_path.clone(), None, &selection)?;
    let inputs = receipt_inputs_from_stone(&project_path, &compiled.stone)?;
    let receipt_selection = if compiled.selection_is_full {
        Vec::new()
    } else {
        selection
    };
    drop(compiled.stone);
    publish_sim_receipt(
        &root,
        &receipt_dir,
        &project_path,
        &output_path,
        &compiled.registry,
        &inputs,
        &receipt_selection,
    )?;
    Ok(PreparedSims {
        registry: compiled.registry.into(),
        stone: None,
    })
}

fn consume_sim_receipt(
    receipt_dir: &Path,
    project_path: &Path,
    output_path: &Path,
) -> Result<PreparedSims> {
    crate::package::prepare_sound_bank(&project_path.to_string_lossy())?;
    let registry = load_sim_receipt(receipt_dir, project_path, output_path)?;
    Ok(PreparedSims {
        registry,
        stone: None,
    })
}

fn prepare_sims_out_of_process(
    project_path: PathBuf,
    output_path: PathBuf,
    rock_exe: &Path,
    selection: &[String],
) -> Result<PreparedSims> {
    crate::package::check_project_assets(&project_path)?;
    rock_host::process_management::report_spawn_phase("compile-prepare");
    if std::env::var_os(SIM_COMPILE_RECEIPT_DIR_ENV).is_some_and(|value| !value.is_empty()) {
        return prepare_sims(project_path, output_path);
    }
    sweep_stale_sim_receipt_roots(Path::new("tmp"));
    let project_path = project_path
        .canonicalize()
        .with_context(|| format!("canonicalize sim project {}", project_path.display()))?;
    fs_err::create_dir_all("tmp").context("creating tmp/ for the sim compile receipt")?;
    let receipt_prefix = format!("{SIM_RECEIPT_PREFIX}{}-", std::process::id());
    let root = tempfile::Builder::new()
        .prefix(&receipt_prefix)
        .tempdir_in("tmp")
        .context("creating sim compile receipt directory")?;
    compile_receipt_in_child(&project_path, rock_exe, root.path(), selection)?;
    consume_sim_receipt(
        &sim_receipt_project_dir(root.path(), &project_path),
        &project_path,
        &output_path,
    )
}

const SIM_RECEIPT_PREFIX: &str = "sim-receipt-";

const STALE_RECEIPT_GRACE: Duration = Duration::from_secs(6 * 60 * 60);

fn receipt_runner_pid(name: &str) -> Option<u32> {
    name.strip_prefix(SIM_RECEIPT_PREFIX)?
        .split('-')
        .next()?
        .parse()
        .ok()
}

fn sweep_stale_sim_receipt_roots(tmp: &Path) {
    let Ok(read) = std::fs::read_dir(tmp) else {
        return;
    };
    for entry in read.filter_map(|entry| entry.ok()) {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(SIM_RECEIPT_PREFIX) || !entry.path().is_dir() {
            continue;
        }
        let pid = receipt_runner_pid(&name);
        if pid.is_some_and(crate::child_lifetime::pid_alive) {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .is_some_and(|mtime| mtime.elapsed().unwrap_or(Duration::ZERO) > STALE_RECEIPT_GRACE);
        if pid.is_some() || old {
            if let Err(error) = std::fs::remove_dir_all(entry.path()) {
                log::info!(target: "SIM", "sim receipt sweep: cannot remove {}: {error}", entry.path().display());
            }
        }
    }
}

fn compile_receipt_in_child(
    project_path: &Path,
    rock_exe: &Path,
    receipt_root: &Path,
    selection: &[String],
) -> Result<()> {
    rock_host::process_management::report_spawn_phase("compile");
    let mut cmd = std::process::Command::new(rock_exe);
    cmd.args(compile_child_invocation_args())
        .arg("sim")
        .arg("--list")
        .arg("--backend")
        .arg("skull")
        .arg(project_path)
        .env(SIM_COMPILE_RECEIPT_DIR_ENV, receipt_root)
        .env(SIM_COMPILE_RECEIPT_PREPARE_ENV, "1")
        .env(
            SIM_COMPILE_SELECTION_ENV,
            serde_json::to_string(&normalized_sim_selection(selection))?,
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    let mut child = crate::child_lifetime::spawn_bound_child(&mut cmd).with_context(|| {
        format!(
            "spawning the sim compile `{} sim --list`",
            rock_exe.display()
        )
    })?;
    let status = child.wait().with_context(|| {
        format!(
            "waiting for the sim compile `{} sim --list`",
            rock_exe.display()
        )
    })?;
    if !status.success() {
        return Err(anyhow!(
            "cannot compile project {}: the sim compile process exited with {status}",
            project_path.display()
        ));
    }
    Ok(())
}

fn compile_child_invocation_args() -> Vec<String> {
    let mut args = crate::comptime::invocation_selection_args();
    match crate::lint_pass::policy() {
        crate::lint_pass::LintReport::Full => {}
        crate::lint_pass::LintReport::Summary => args.push("--lint-summary".to_string()),
        crate::lint_pass::LintReport::Off => args.push("--no-lints".to_string()),
    }
    args
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

fn source_root_for_sim_project(project_dir: &std::path::Path) -> Result<std::path::PathBuf> {
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
                text.trim()
                    .strip_prefix("gitdir: ")
                    .map(|dir| current.join(dir))
            })
        };
        if git_dir.is_some_and(|dir| dir.join("HEAD").is_file()) {
            return Ok(current.to_path_buf());
        }
        let Some(parent) = current.parent() else {
            break;
        };
        current = parent;
    }
    if let Some(root) = rock_engine_host::project::source_workspace_root(&project_dir) {
        if let Ok(root) = root.canonicalize() {
            if project_dir.starts_with(&root) {
                return Ok(root);
            }
        }
    }
    Ok(project_dir)
}

fn skull_compile_game_and_collect_sims_with_allocation(
    dir: &str,
    main_path: &std::path::Path,
    allocation_manifest: Option<&std::path::Path>,
    selection: &[String],
) -> Result<CompiledSims> {
    let incremental_path = main_path.with_file_name("incremental.lua");
    let _activation = allocation_manifest.map(|_| skull::allocation_report::activation_scope(true));
    let mut selection_is_full = true;
    let stone = IncrementalProject::init_for_sim_selection(
        dir,
        main_path,
        &incremental_path,
        allocation_manifest,
        |ws| {
            let all_names = skull::mir_emit::collect_sim_fns(ws)
                .into_iter()
                .map(|(_, _, wire)| wire)
                .collect::<Vec<_>>();
            let selected = crate::run_sim::resolve_sim_specs(&all_names, selection)?;
            let selected = selected
                .into_iter()
                .flat_map(|spec| spec.split(',').map(str::to_owned).collect::<Vec<_>>())
                .collect::<std::collections::BTreeSet<_>>();
            selection_is_full = selection.is_empty() || selected.len() == all_names.len();
            Ok((!selection_is_full).then(|| selected.into_iter().collect()))
        },
    )
    .map_err(|e| anyhow::anyhow!("cannot compile project {dir}: {e}"))?;

    let project_dir = std::path::Path::new(dir)
        .canonicalize()
        .with_context(|| format!("canonicalize sim project {dir}"))?;
    let workspace_root = source_root_for_sim_project(&project_dir)?;
    let mut sims = collect_sim_descriptors(stone.workspace());
    crate::sim_environment::apply(stone.workspace(), &mut sims);
    if !selection_is_full {
        let selected = stone.registered_sim_names();
        sims.retain(|sim| selected.contains(&sim.wire_name));
    }
    let mut registry = Vec::with_capacity(sims.len());
    let mut relative_by_source = HashMap::new();
    for sim in sims {
        let wire_name = sim.wire_name;
        let source_file = match relative_by_source.entry(sim.source_file) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let relative = relative_sim_source_path(&workspace_root, &project_dir, entry.key())
                    .with_context(|| format!("record source for @sim `{wire_name}`"))?;
                anyhow::ensure!(
                    !relative.is_empty(),
                    "compiled @sim `{wire_name}` has no recorded source file"
                );
                entry.insert(relative)
            }
        };
        registry.push(RegisteredSim {
            wire_name,
            source_file: source_file.clone(),
            env: sim.env_pairs,
        });
    }
    Ok(CompiledSims {
        registry,
        stone,
        selection_is_full,
    })
}

#[cfg(test)]
mod sim_source_path_tests {
    use super::{relative_sim_source_path, source_root_for_sim_project};

    #[test]
    fn sim_source_paths_are_workspace_relative_across_projects_and_dependencies() {
        let temp = tempfile::tempdir().expect("tempdir");
        let workspace = temp.path().join("workspace");
        let project = workspace.join("game");
        let source = project.join("tests/sims/example.rock");
        std::fs::create_dir_all(source.parent().unwrap()).expect("create project source dir");
        std::fs::write(&source, "@sim\nco fn example() {}\n").expect("write project source");

        let workspace = workspace.canonicalize().expect("canonical workspace");
        let project = project.canonicalize().expect("canonical project");
        assert_eq!(
            relative_sim_source_path(&workspace, &project, source.to_str().unwrap()).unwrap(),
            "game/tests/sims/example.rock"
        );

        let dependency = temp.path().join("dependency/tests/sims/example.rock");
        std::fs::create_dir_all(dependency.parent().unwrap())
            .expect("create dependency source dir");
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
        let project = source_root.join("game");
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
            relative_sim_source_path(&project_root, &project_root, source.to_str().unwrap())
                .unwrap(),
            "tests/sims/probe.rock"
        );

        // A source archive has no Git metadata but keeps workspace-relative sim labels.
        std::fs::write(temp.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::create_dir(temp.path().join("crates")).unwrap();
        let archive_root = source_root_for_sim_project(&project).unwrap();
        assert_eq!(archive_root, temp.path().canonicalize().unwrap());
        assert_eq!(
            relative_sim_source_path(&archive_root, &project_root, source.to_str().unwrap())
                .unwrap(),
            "standalone-project/tests/sims/probe.rock"
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
        for gitdir in [
            "../git-metadata".to_string(),
            metadata.to_string_lossy().into_owned(),
        ] {
            std::fs::write(source_root.join(".git"), format!("gitdir: {gitdir}\n"))
                .expect("write worktree git file");
            assert_eq!(
                source_root_for_sim_project(&project).unwrap(),
                source_root.canonicalize().unwrap()
            );
        }
    }
}

fn compile_sims_with_allocation(
    project_path: PathBuf,
    output_path: PathBuf,
    allocation_manifest: Option<&Path>,
    selection: &[String],
) -> Result<CompiledSims> {
    rock_host::process_management::report_spawn_phase("compile");
    crate::package::prepare_sound_bank(&project_path.to_string_lossy())?;
    let main_path = output_path.join(rock_host::project::MAIN_LUA_FILENAME);
    skull_compile_game_and_collect_sims_with_allocation(
        &project_path.to_string_lossy(),
        &main_path,
        allocation_manifest,
        selection,
    )
}

pub(crate) struct ReloadLaneSession {
    stone: crate::skull_backend::IncrementalProject,
    output: String,
    _dir: TempDir,
}

fn reload_lane_session(
    project_path: &Path,
    lane_is_selected: bool,
    shared_output: &Path,
    prepared_stone: Option<crate::skull_backend::IncrementalProject>,
    quiet: bool,
) -> Result<Option<ReloadLaneSession>> {
    if !lane_is_selected {
        return Ok(None);
    }
    let dir = sim_output_dir()?;
    let output = dir.path().to_string_lossy().into_owned();
    let stone = match prepared_stone {
        Some(stone) => {
            fs_err::copy(
                shared_output.join(rock_host::project::MAIN_LUA_FILENAME),
                dir.path().join(rock_host::project::MAIN_LUA_FILENAME),
            )?;
            stone
        }
        None => compile_sims(project_path.to_path_buf(), dir.path().to_path_buf())?.stone,
    };
    if !quiet {
        println!("[sim] hot-reload lane: isolated chunk with the shared callee shape");
    }
    Ok(Some(ReloadLaneSession {
        stone,
        output,
        _dir: dir,
    }))
}

fn any_reload_lane_selected(
    specs: &[String],
    env_by_wire: &HashMap<String, Vec<(String, String)>>,
) -> bool {
    specs
        .iter()
        .any(|spec| entry_is_reload_lane(spec, env_by_wire))
}

pub(crate) fn do_list_sims(project: &str) -> Result<()> {
    let project_path = rock_engine_host::project::resolve_project_dir(project)?;

    let out_dir = sim_output_dir()?;
    let prepared = prepare_sims(project_path, out_dir.path().to_path_buf())?;
    for name in prepared.registry.wire_names {
        println!("{}", name);
    }
    Ok(())
}

fn prepare_run(
    project: &str,
    names: &[String],
    recorder: &SimRunRecorder,
) -> Result<(PathBuf, TempDir, PreparedSims)> {
    let project_path = recorder.phase("resolve_project", || {
        rock_engine_host::project::resolve_project_dir(project)
    })?;
    recorder.phase("build_engine", || {
        rock_engine_host::engine_build::build_engine_libs(&project_path.to_string_lossy())
    })?;
    let out_dir = sim_output_dir()?;
    let prepared = recorder.phase("compile_and_discover", || {
        prepare_sims_out_of_process(
            project_path.clone(),
            out_dir.path().to_path_buf(),
            &rock_host::process_management::current_executable_image()?,
            names,
        )
    })?;
    Ok((project_path, out_dir, prepared))
}

fn run_single_sim(
    project_path: &Path,
    output_path: &Path,
    mut prepared: PreparedSims,
    specs: &[String],
    quiet: bool,
    update_snapshots: bool,
    requested_jobs: Option<usize>,
    recorder: &SimRunRecorder,
) -> Result<()> {
    let sim_name = &specs[0];
    let output = output_path.to_string_lossy();
    if prepared.stone.is_none() && entry_is_reload_lane(sim_name, &prepared.registry.env_by_wire) {
        let compiled = recorder.phase("compile_reload_session", || {
            compile_sims(project_path.to_path_buf(), output_path.to_path_buf())
        })?;
        prepared = PreparedSims {
            registry: compiled.registry.into(),
            stone: Some(compiled.stone),
        };
    }
    let entry_ids = recorder.plan_entries(specs);
    recorder.configure(
        project_path,
        &prepared.registry.wire_names,
        requested_jobs,
        1,
        None,
        1,
        update_snapshots,
        &prepared.registry.source_files,
    );
    emit_bytecode_image(Path::new(output.as_ref()));

    let sim_sources = resolve_sim_sources(project_path, &prepared.registry.source_files)?;
    let (display_name, summary) = run_named_entry(
        sim_name,
        quiet,
        false,
        update_snapshots,
        &output,
        prepared
            .registry
            .env_by_wire
            .get(sim_name)
            .cloned()
            .unwrap_or_default(),
        sim_sources.get(sim_name).cloned(),
        prepared.stone.as_mut(),
        None,
        Some(recorder),
        entry_ids.first().copied(),
    )?;
    let sim_name = display_name.as_str();

    if !quiet {
        summary.print(sim_name, true);
    }

    sim_outcome(&summary, sim_name)
}

fn sim_outcome(summary: &SimSummary, sim_name: &str) -> Result<()> {
    if summary.excluded.is_some() {
        return Ok(());
    }

    if summary.timed_out {
        return Err(anyhow!(
            "sim `{sim_name}` timed out after {}s (wall clock, not its frame budget); \
             override with ROCK_SIM_TIMEOUT_SECS",
            summary
                .timeout_budget
                .map(|d| d.as_secs())
                .unwrap_or_else(sim_timeout_secs)
        ));
    }
    if let Some(code) = summary.process_failure_code {
        return Err(anyhow!(
            "sim `{sim_name}` process exited with non-zero status: {code:?}"
        ));
    }
    if summary.has_failures() {
        let fails = summary.expects_fail.len();
        let errs = summary.errors.len();
        return Err(anyhow!(
            "sim `{sim_name}` had {fails} expect failure(s), {errs} error(s)"
        ));
    }
    // A coroutine can abort without reporting a failed check.
    if !summary.saw_stop {
        return Err(anyhow!(
            "sim `{sim_name}` aborted before stop_sim() -- incomplete run (likely a swallowed runtime error)"
        ));
    }
    // A fixture that returned before its checks still reaches stop_sim().
    if !summary.checks_accounted() {
        return Err(anyhow!(
            "sim `{sim_name}` reached stop_sim() without running a check"
        ));
    }
    Ok(())
}

fn run_named_entry(
    name: &str,
    quiet: bool,
    buffered: bool,
    update_snapshots: bool,
    output: &str,
    env_pairs: Vec<(String, String)>,
    source_file: Option<PathBuf>,
    stone: Option<&mut crate::skull_backend::IncrementalProject>,
    census_warmup_frames: Option<usize>,
    recorder: Option<&SimRunRecorder>,
    occurrence_id: Option<u64>,
) -> Result<(String, SimSummary)> {
    let out_dir = TempDir::new().context("creating sim tempdir")?;

    if name.is_empty() {
        return Err(anyhow!("sim got an empty sim name"));
    }
    let lane = if is_reload_lane(&env_pairs) {
        stone
    } else {
        None
    };
    let part = WorkerPart {
        name: name.to_string(),
        out_dir: out_dir.path().to_path_buf(),
        frame_budget: frame_budget_for(name, &env_pairs)?,
        timeout: timeout_for(name, &env_pairs)?,
        source_file,
        env: env_pairs,
        occurrence_id: occurrence_id.unwrap_or(0),
    };
    let mut summary = run_child(ChildConfig {
        part: &part,
        quiet,
        buffered,
        update_snapshots,
        output,
        census_warmup_frames,
        stone: lane,
        recorder,
    })?;

    if let (Some(recorder), Some(occurrence_id)) = (recorder, occurrence_id) {
        recorder.artifact_check_start(occurrence_id);
    }
    for path in std::mem::take(&mut summary.cleanup_paths) {
        let _ = std::fs::remove_file(path);
    }
    if let (Some(recorder), Some(occurrence_id)) = (recorder, occurrence_id) {
        recorder.artifact_check_end(occurrence_id, !summary.has_failures());
        recorder.entry_result(
            occurrence_id,
            summary.worker_attempt.unwrap_or(0),
            verdict_word(&summary),
            summary
                .frame_count
                .or_else(|| summary.allocation_census.as_ref().map(|c| c.frame_count)),
            summary.expects_pass.len(),
            summary.timed_out,
            summary.process_failure_code,
        );
    }
    Ok((name.to_string(), summary))
}

fn manual_sim(env: &[(String, String)]) -> Result<bool> {
    match sim_env_value(env, "SIM_MANUAL") {
        None | Some("0") => Ok(false),
        Some("1") => Ok(true),
        Some(value) => Err(anyhow!("SIM_MANUAL must be 0 or 1, got `{value}`")),
    }
}

fn record_excluded_entry(recorder: Option<&SimRunRecorder>, occurrence_id: Option<u64>) {
    if let (Some(recorder), Some(id)) = (recorder, occurrence_id) {
        recorder.entry_result(id, 0, "EXCLUDED", None, 0, false, None);
    }
}

fn entry_excluded(summary: &SimSummary) -> bool {
    summary.excluded.is_some()
}

fn entry_passed(summary: &SimSummary) -> bool {
    !entry_excluded(summary) && sim_outcome(summary, "").is_ok()
}

fn verdict_word(summary: &SimSummary) -> &'static str {
    if entry_excluded(summary) {
        "EXCLUDED"
    } else if entry_passed(summary) {
        "PASS"
    } else {
        "FAIL"
    }
}

pub(crate) fn do_run_sims(
    project: &str,
    names: &[String],
    quiet: bool,
    update_snapshots: bool,
    jobs: Option<usize>,
    full_asset_jobs: Option<usize>,
) -> Result<()> {
    if names.len() == 1 && jobs.is_some_and(|j| j > 1) {
        resolve_jobs(jobs, Some(1), update_snapshots)?;
    }
    let selection = match names {
        [] => std::borrow::Cow::Borrowed("<all>"),
        [name] => std::borrow::Cow::Borrowed(name.as_str()),
        _ => std::borrow::Cow::Owned(names.join(" ")),
    };
    let recorder = SimRunRecorder::begin(project, &selection, quiet, jobs);
    let result = do_run_sims_recorded(
        project,
        names,
        quiet,
        update_snapshots,
        jobs,
        full_asset_jobs,
        &recorder,
    );
    let error = result.as_ref().err().map(ToString::to_string);
    recorder.finish(result.is_ok(), error.as_deref());
    result
}

fn do_run_sims_recorded(
    project: &str,
    names: &[String],
    quiet: bool,
    update_snapshots: bool,
    jobs: Option<usize>,
    full_asset_jobs: Option<usize>,
    recorder: &SimRunRecorder,
) -> Result<()> {
    let (project_path, out_dir, prepared) = prepare_run(project, names, recorder)?;
    let output_path = out_dir.path();
    let suite = names.is_empty();
    let specs = if suite {
        Ok(prepared.registry.wire_names.clone())
    } else {
        resolve_sim_specs(&prepared.registry.wire_names, names)
    };
    if names.len() == 1 {
        return run_single_sim(
            &project_path,
            output_path,
            prepared,
            &specs?,
            quiet,
            update_snapshots,
            jobs,
            recorder,
        );
    }

    let project = project_path.to_string_lossy();
    let output = output_path.to_string_lossy();
    let SimRegistry {
        wire_names,
        env_by_wire,
        source_files,
    } = prepared.registry;
    let sim_sources = resolve_sim_sources(&project_path, &source_files)?;
    let specs = specs?;
    emit_bytecode_image(Path::new(output.as_ref()));
    if specs.is_empty() {
        return Err(anyhow!("no @sim coroutines found in project `{project}`"));
    }
    let entry_ids = recorder.plan_entries(&specs);
    if !suite && !quiet {
        if let Some(events) = recorder.events_path() {
            println!(
                "[sim] {} sims: records print when all are done; live entry_start/entry_end in {}",
                specs.len(),
                events.display()
            );
        }
    }

    let lanes = (!suite).then_some(specs.len());
    let effective_jobs = resolve_pool_jobs(jobs, lanes, update_snapshots)?;
    let requested_full_asset_jobs = full_asset_jobs;
    let full_asset_jobs = resolve_full_asset_jobs(full_asset_jobs, effective_jobs);
    recorder.configure(
        &project_path,
        &wire_names,
        jobs,
        effective_jobs,
        requested_full_asset_jobs,
        full_asset_jobs,
        update_snapshots,
        &source_files,
    );
    let selected = specs.len();
    let mut routine_specs = Vec::new();
    let mut routine_ids = Vec::new();
    for (name, id) in specs.into_iter().zip(entry_ids) {
        if suite && manual_sim(env_by_wire.get(&name).map_or(&[][..], |v| v))? {
            record_excluded_entry(Some(recorder), Some(id));
            SimSummary {
                excluded: Some("manual; run by name".to_string()),
                saw_stop: true,
                ..SimSummary::default()
            }
            .print(&name, false);
        } else {
            routine_specs.push(name);
            routine_ids.push(id);
        }
    }
    let manual_excluded = selected - routine_specs.len();
    let mut reload_lane = reload_lane_session(
        &project_path,
        any_reload_lane_selected(&routine_specs, &env_by_wire),
        Path::new(output.as_ref()),
        prepared.stone,
        quiet,
    )?;
    let results = SimRun {
        output: &output,
        env_by_wire: &env_by_wire,
        specs: &routine_specs,
        source_files: &sim_sources,
        jobs: effective_jobs,
        reload_lane: reload_lane.as_mut(),
    }
    .execute(SimMode::Standard {
        quiet,
        streaming: suite && effective_jobs == 1 && !quiet,
        update_snapshots,
        full_asset_jobs,
        recorder,
        entry_ids: &routine_ids,
    })?
    .into_iter()
    .collect::<Result<Vec<_>>>()?;
    let (passed, excluded, failed) = tally_entries(&results);
    let excluded = excluded + manual_excluded;

    let executed = selected - excluded;
    let excl_suffix = if excluded > 0 {
        format!(" (selected {selected}, {excluded} excluded)")
    } else {
        String::new()
    };
    let suffix = if failed.is_empty() {
        String::new()
    } else {
        format!(" -- FAILED: {}", failed.join(", "))
    };
    let (scope, what) = if suite {
        (format!(" {project} suite:"), "sims")
    } else {
        (String::new(), "named sims")
    };
    println!("[sim]{scope} {passed}/{executed} sims passed{excl_suffix}{suffix}");
    if !failed.is_empty() {
        return Err(anyhow!(
            "{} of {} {what} failed in `{project}`",
            failed.len(),
            executed
        ));
    }
    Ok(())
}

fn tally_entries(results: &[(String, SimSummary)]) -> (usize, usize, Vec<String>) {
    let mut passed = 0usize;
    let mut excluded = 0usize;
    let mut failed = Vec::new();
    for (name, summary) in results {
        if entry_excluded(summary) {
            excluded += 1;
        } else if entry_passed(summary) {
            passed += 1;
        } else {
            failed.push(name.clone());
        }
    }
    (passed, excluded, failed)
}

struct SimRun<'a> {
    output: &'a str,
    env_by_wire: &'a HashMap<String, Vec<(String, String)>>,
    specs: &'a [String],
    source_files: &'a HashMap<String, PathBuf>,
    jobs: usize,
    reload_lane: Option<&'a mut ReloadLaneSession>,
}

#[derive(Clone, Copy)]
enum SimMode<'a> {
    Standard {
        quiet: bool,
        streaming: bool,
        update_snapshots: bool,
        full_asset_jobs: usize,
        recorder: &'a SimRunRecorder,
        entry_ids: &'a [u64],
    },
    Census,
    Sites(&'a Path),
}

type SimResult = Result<(String, SimSummary)>;

impl SimRun<'_> {
    fn execute(self, mode: SimMode<'_>) -> Result<Vec<SimResult>> {
        let Self {
            output,
            env_by_wire,
            specs,
            source_files,
            jobs,
            reload_lane,
        } = self;
        let (reload_idx, mut dispatch): (Vec<usize>, Vec<usize>) =
            (0..specs.len()).partition(|&i| entry_is_reload_lane(&specs[i], env_by_wire));

        let mut asset_cap = jobs;
        let mut full_assets = Vec::new();
        if let SimMode::Standard {
            quiet,
            full_asset_jobs,
            recorder,
            ..
        } = mode
        {
            if jobs > 1 {
                let estimates: Vec<_> = dispatch
                    .iter()
                    .map(|&i| recorder.recorded_duration_ms(&specs[i]))
                    .collect();
                if !quiet {
                    let history = estimates.iter().filter(|ms| ms.is_some()).count();
                    println!(
                        "[sim] entries start longest-first ({history} history-timed, {} untimed keep \
                         registry order of {})",
                        estimates.len() - history,
                        estimates.len()
                    );
                }
                dispatch = longest_first(&estimates)
                    .into_iter()
                    .map(|k| dispatch[k])
                    .collect();
            }
            let default_stub = asset_stub_value();
            asset_cap = full_asset_jobs;
            full_assets = dispatch
                .iter()
                .map(|&i| {
                    entry_full_assets(
                        env_by_wire.get(&specs[i]).map_or(&[][..], Vec::as_slice),
                        default_stub,
                    )
                })
                .collect();
            let full = full_assets.iter().filter(|f| **f).count();
            if !quiet && asset_cap < jobs && full > 0 {
                println!(
                    "[sim] at most {asset_cap} of {jobs} children hold full-size assets at once ({full} full-asset entries)"
                );
            }
        }

        let run_at = |i: usize,
                      lane: Option<(&mut crate::skull_backend::IncrementalProject, &str)>|
         -> SimResult {
            let name = &specs[i];
            let (stone, entry_output) = match lane {
                Some((stone, out)) => (Some(stone), out),
                None => (None, output),
            };
            let mut env = env_by_wire.get(name).cloned().unwrap_or_default();
            let (quiet, streaming, update_snapshots, recorder, occurrence_id) = match mode {
                SimMode::Standard {
                    quiet,
                    streaming,
                    update_snapshots,
                    recorder,
                    entry_ids,
                    ..
                } => (
                    quiet,
                    streaming,
                    update_snapshots,
                    Some(recorder),
                    Some(entry_ids[i]),
                ),
                SimMode::Census => (true, false, false, None, None),
                SimMode::Sites(artifact_dir) => {
                    let stem = allocation_site_artifact_stem(i, name);
                    let activation = artifact_dir.join(format!("{stem}.activation.tsv"));
                    env.extend([
                        (
                            "SKULL_ALLOCATION_ACTIVATION".into(),
                            activation.to_string_lossy().into_owned(),
                        ),
                        ("STONE_TEXT_LAYOUT_SIM_PROBES".into(), "0".into()),
                        ("STONE_IMGUI_SIM_PROBES".into(), "0".into()),
                    ]);
                    (true, false, false, None, None)
                }
            };
            let (display_name, summary) = run_named_entry(
                name,
                quiet,
                !streaming,
                update_snapshots,
                entry_output,
                env,
                source_files.get(name).cloned(),
                stone,
                matches!(mode, SimMode::Census).then_some(DEFAULT_SIM_ALLOCATION_WARMUP_FRAMES),
                recorder,
                occurrence_id,
            )?;
            if streaming && !quiet {
                print_record(&display_name, &summary, true);
            }
            Ok((display_name, summary))
        };

        let (pool, lane) = beside_reload_lane(
            jobs,
            !reload_idx.is_empty(),
            |width| {
                let task = |k| run_at(dispatch[k], None);
                if asset_cap < jobs {
                    run_bounded_classed(width, asset_cap, &full_assets, task)
                } else {
                    run_bounded(width, dispatch.len(), task)
                }
            },
            || run_reload_lane_serially(reload_lane, &reload_idx, |i, lane| run_at(i, Some(lane))),
        );
        let mut results: HashMap<_, _> = pool
            .into_iter()
            .map(|(k, result)| (dispatch[k], result))
            .collect();
        results.extend(lane);
        let ordered: Vec<_> = specs
            .iter()
            .enumerate()
            .map(|(i, name)| {
                results
                    .remove(&i)
                    .ok_or_else(|| anyhow!("sim `{name}` was never scheduled"))
            })
            .collect::<Result<_>>()?;
        if matches!(
            mode,
            SimMode::Standard {
                quiet: false,
                streaming: false,
                ..
            }
        ) {
            for (display_name, summary) in ordered.iter().flatten() {
                print_record(display_name, summary, false);
            }
        }
        Ok(ordered)
    }
}

fn print_record(display_name: &str, summary: &SimSummary, streamed: bool) {
    if !summary.diagnostics.is_empty() {
        eprint!("{}", summary.diagnostics);
    }
    summary.print(display_name, streamed);
}

fn run_bounded<T, F>(width: &PoolWidth, n: usize, task: F) -> Vec<(usize, Result<T>)>
where
    F: Fn(usize) -> Result<T> + Sync,
    T: Send,
{
    if width.jobs <= 1 || n <= 1 {
        return (0..n).map(|i| (i, task(i))).collect();
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    run_pool(width, n, || {
        let i = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        (i < n).then(|| (i, task(i)))
    })
}

fn run_pool<T: Send>(
    width: &PoolWidth,
    n: usize,
    run_next: impl Fn() -> Option<T> + Sync,
) -> Vec<T> {
    let collected = Mutex::new(Vec::with_capacity(n));
    std::thread::scope(|scope| {
        for _ in 0..width.jobs.min(n) {
            scope.spawn(|| {
                loop {
                    let _slot = width.enter();
                    let Some(result) = run_next() else { break };
                    collected.lock().unwrap().push(result);
                }
            });
        }
    });
    collected
        .into_inner()
        .expect("no lane panicked while holding the results lock")
}

struct PoolWidth {
    jobs: usize,
    slots: Mutex<(usize, usize)>,
    changed: std::sync::Condvar,
}

impl PoolWidth {
    fn fixed(jobs: usize) -> Self {
        Self::starting_at(jobs, jobs)
    }

    fn starting_at(jobs: usize, width: usize) -> Self {
        let jobs = jobs.max(1);
        Self {
            jobs,
            slots: Mutex::new((width.clamp(1, jobs), 0)),
            changed: std::sync::Condvar::new(),
        }
    }

    fn widen_to_jobs(&self) {
        self.slots.lock().unwrap().0 = self.jobs;
        self.changed.notify_all();
    }

    fn enter(&self) -> PoolSlot<'_> {
        self.acquire();
        PoolSlot(self)
    }

    fn acquire(&self) {
        let mut slots = self.slots.lock().unwrap();
        while slots.1 >= slots.0 {
            slots = self.changed.wait(slots).unwrap();
        }
        slots.1 += 1;
    }

    fn release(&self) {
        // A poisoned count accompanies a task panic, which the joining scope
        // re-raises. Wake other waiters even during teardown.
        if let Ok(mut slots) = self.slots.lock() {
            slots.1 -= 1;
        }
        self.changed.notify_all();
    }
}

struct PoolSlot<'a>(&'a PoolWidth);

impl Drop for PoolSlot<'_> {
    fn drop(&mut self) {
        self.0.release();
    }
}

fn longest_first(estimates: &[Option<u64>]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..estimates.len()).collect();
    order.sort_by_key(|&w| std::cmp::Reverse(estimates[w].unwrap_or(0)));
    order
}

fn beside_reload_lane<A, B>(
    jobs: usize,
    lane_has_work: bool,
    pool: impl FnOnce(&PoolWidth) -> A + Send,
    lane: impl FnOnce() -> B,
) -> (A, B)
where
    A: Send,
{
    if !lane_has_work {
        return (pool(&PoolWidth::fixed(jobs)), lane());
    }
    let width = PoolWidth::starting_at(jobs, jobs.saturating_sub(1));
    std::thread::scope(|scope| {
        let pool = scope.spawn(|| pool(&width));
        let lane = lane();
        width.widen_to_jobs();
        let pool = pool
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        (pool, lane)
    })
}

fn run_reload_lane_serially<T>(
    lane: Option<&mut ReloadLaneSession>,
    indices: &[usize],
    mut run: impl FnMut(usize, (&mut crate::skull_backend::IncrementalProject, &str)) -> Result<T>,
) -> Vec<(usize, Result<T>)> {
    let Some(lane) = lane else {
        return indices
            .iter()
            .map(|&i| {
                (
                    i,
                    Err(anyhow!(
                        "reload entry {i} was selected without a lane session"
                    )),
                )
            })
            .collect();
    };
    indices
        .iter()
        .map(|&i| (i, run(i, (&mut lane.stone, lane.output.as_str()))))
        .collect()
}

fn run_bounded_classed<T, F>(
    width: &PoolWidth,
    capped_jobs: usize,
    capped: &[bool],
    task: F,
) -> Vec<(usize, Result<T>)>
where
    F: Fn(usize) -> Result<T> + Sync,
    T: Send,
{
    let n = capped.len();
    let jobs = width.jobs;
    if jobs <= 1 || n <= 1 || capped_jobs >= jobs {
        return run_bounded(width, n, task);
    }
    struct Queue {
        capped: std::collections::VecDeque<usize>,
        other: std::collections::VecDeque<usize>,
        live_capped: usize,
    }
    let queue = Mutex::new(Queue {
        capped: (0..n).filter(|&i| capped[i]).collect(),
        other: (0..n).filter(|&i| !capped[i]).collect(),
        live_capped: 0,
    });
    let freed = std::sync::Condvar::new();
    run_pool(width, n, || {
        let (i, is_capped) = {
            let mut q = queue.lock().unwrap();
            loop {
                if q.live_capped < capped_jobs {
                    if let Some(i) = q.capped.pop_front() {
                        q.live_capped += 1;
                        break (i, true);
                    }
                }
                if let Some(i) = q.other.pop_front() {
                    break (i, false);
                }
                if q.capped.is_empty() {
                    return None;
                }
                q = freed.wait(q).unwrap();
            }
        };
        let result = task(i);
        if is_capped {
            queue.lock().unwrap().live_capped -= 1;
            freed.notify_all();
        }
        Some((i, result))
    })
}

const DEFAULT_SIM_ALLOCATION_WARMUP_FRAMES: usize = 2;

fn census_status(summary: &SimSummary) -> &'static str {
    if summary.timed_out {
        "timeout"
    } else if summary.process_failure_code.is_some() {
        "process failure"
    } else if !summary.saw_stop {
        "no-verdict"
    } else if summary.has_failures() {
        "FAIL"
    } else if summary.allocation_census.is_none()
        || summary.allocation_census.as_ref().unwrap().measured_frames == 0
        || !summary
            .allocation_census
            .as_ref()
            .unwrap()
            .measurement_complete
    {
        "no-verdict"
    } else {
        "PASS"
    }
}

fn census_failure_messages(summary: &SimSummary) -> Vec<String> {
    summary
        .errors
        .iter()
        .chain(&summary.expects_fail)
        .map(|m| {
            if m.site.is_empty() {
                m.message.clone()
            } else {
                format!("{}: {}", m.site, m.message)
            }
        })
        .collect()
}

fn allocation_census_record(
    name: &str,
    summary: &SimSummary,
) -> (serde_json::Value, Option<(f64, f64)>) {
    let census = summary.allocation_census.as_ref();
    let measured_frames = census.map_or(0, |c| c.measured_frames);
    let per_frame = |total: f64| {
        if measured_frames > 0 {
            total / measured_frames as f64
        } else {
            0.0
        }
    };
    let measured_gross_kb = census.map_or(0.0, |c| c.measured_gross_kb);
    let measured_allocator_calls = census.map_or(0, |c| c.measured_allocator_calls);
    let measured_allocation_calls = census.map_or(0, |c| c.measured_allocation_calls);
    let measured_reallocation_calls = census.map_or(0, |c| c.measured_reallocation_calls);
    let gross_kb_per_frame = per_frame(measured_gross_kb);
    let allocator_calls_per_frame = per_frame(measured_allocator_calls as f64);
    let allocation_calls_per_frame = per_frame(measured_allocation_calls as f64);
    let reallocation_calls_per_frame = per_frame(measured_reallocation_calls as f64);
    let record = serde_json::json!({
        "sim_name": name,
        "process_id": summary.process_id,
        "build_sha": census.map(|c| c.build_sha.clone()),
        "status": census_status(summary),
        "sim_verdict": if entry_passed(summary) { "PASS" } else { "FAIL" },
        "failure_messages": census_failure_messages(summary),
        "stop_sim_reached": summary.saw_stop,
        "rendering_enabled": census.map(|c| c.rendering_enabled),
        "rendering_skipped": census.map(|c| !c.rendering_enabled),
        "measurement_complete": census.map(|c| c.measurement_complete),
        "warmup_frames_requested": census.map_or(0, |c| c.warmup_frames_requested),
        "warmup_frames": census.map_or(0, |c| c.warmup_frames),
        "frame_count": census.map_or(0, |c| c.frame_count),
        "measured_frames": measured_frames,
        "initialization_gross_kb": census.map_or(0.0, |c| c.initialization_gross_kb),
        "outside_frame_gross_kb": census.map_or(0.0, |c| c.outside_frame_gross_kb),
        "warmup_gross_kb": census.map_or(0.0, |c| c.warmup_gross_kb),
        "measured_gross_kb": measured_gross_kb,
        "total_gross_kb": census.map_or(0.0, |c| c.total_gross_kb),
        "gross_kb_per_frame": gross_kb_per_frame,
        "measured_allocator_calls": measured_allocator_calls,
        "measured_allocation_calls": measured_allocation_calls,
        "measured_reallocation_calls": measured_reallocation_calls,
        "allocator_calls_per_frame": allocator_calls_per_frame,
        "allocation_calls_per_frame": allocation_calls_per_frame,
        "reallocation_calls_per_frame": reallocation_calls_per_frame,
    });
    let rank_values = census
        .filter(|c| c.measurement_complete && c.measured_frames > 0)
        .map(|_| (gross_kb_per_frame, allocation_calls_per_frame));
    (record, rank_values)
}

fn compile_sim_diagnostic(
    project: &str,
    allocation_manifest: Option<&Path>,
) -> Result<(PathBuf, TempDir, CompiledSims)> {
    let project_path = rock_engine_host::project::resolve_project_dir(project)?;
    rock_engine_host::engine_build::build_engine_libs(&project_path.to_string_lossy())?;
    let out_dir = sim_output_dir()?;
    let compiled = compile_sims_with_allocation(
        project_path.clone(),
        out_dir.path().to_path_buf(),
        allocation_manifest,
        &[],
    )?;
    Ok((project_path, out_dir, compiled))
}

fn census_rankings(
    ranked: &[(String, &'static str, f64, f64, usize)],
    by_gross: bool,
) -> Vec<serde_json::Value> {
    let mut rows = ranked.to_vec();
    rows.sort_by(|a, b| {
        let left = if by_gross { a.2 } else { a.3 };
        let right = if by_gross { b.2 } else { b.3 };
        right
            .partial_cmp(&left)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    rows.into_iter()
        .enumerate()
        .map(|(rank, (name, status, gross, calls, frames))| {
            serde_json::json!({
                "rank": rank + 1,
                "sim_name": name,
                "status": status,
                "gross_kb_per_frame": gross,
                "allocation_calls_per_frame": calls,
                "measured_frames": frames,
            })
        })
        .collect()
}

pub(crate) fn do_run_allocation_census(
    project: &str,
    names: &[String],
    output_path: &str,
    jobs: Option<usize>,
) -> Result<()> {
    let path = PathBuf::from(output_path);
    if path.exists() {
        std::fs::remove_file(&path)
            .with_context(|| format!("removing stale census artifact `{}`", path.display()))?;
    }
    let (project_path, out_dir, compiled) = compile_sim_diagnostic(project, None)?;
    let project = project_path.to_string_lossy().into_owned();
    let output = out_dir.path().to_string_lossy().into_owned();
    let registry = SimRegistry::from(compiled.registry);
    let sim_sources = resolve_sim_sources(&project_path, &registry.source_files)?;
    let stone = compiled.stone;
    emit_bytecode_image(Path::new(&output));

    let selected = registry.select(&project, names)?;
    let jobs = resolve_pool_jobs(jobs, None, false)?;
    let mut reload_lane = reload_lane_session(
        &project_path,
        any_reload_lane_selected(&selected, &registry.env_by_wire),
        Path::new(&output),
        Some(stone),
        false,
    )?;
    let results = SimRun {
        output: &output,
        env_by_wire: &registry.env_by_wire,
        specs: &selected,
        source_files: &sim_sources,
        jobs,
        reload_lane: reload_lane.as_mut(),
    }
    .execute(SimMode::Census)?;

    let mut records = Vec::with_capacity(selected.len());
    let mut ranked = Vec::new();
    let mut failed = Vec::new();
    for (name, result) in selected.iter().zip(results) {
        match result {
            Ok((display_name, summary)) => {
                let (record, rank_values) = allocation_census_record(&display_name, &summary);
                let status = census_status(&summary);
                if status != "PASS" {
                    failed.push(format!("{display_name} ({status})"));
                }
                if let Some((gross, calls)) = rank_values {
                    ranked.push((
                        display_name,
                        status,
                        gross,
                        calls,
                        record["measured_frames"].as_u64().unwrap_or(0) as usize,
                    ));
                }
                records.push(record);
            }
            Err(error) => {
                failed.push(format!("{name} (process failure)"));
                let (mut record, _) = allocation_census_record(name, &SimSummary::default());
                record["status"] = "process failure".into();
                record["failure_messages"] = serde_json::json!([error.to_string()]);
                record["measurement_complete"] = false.into();
                record["warmup_frames_requested"] = DEFAULT_SIM_ALLOCATION_WARMUP_FRAMES.into();
                records.push(record);
            }
        }
    }

    let ranked_names: std::collections::HashSet<&str> = ranked
        .iter()
        .map(|(name, _, _, _, _)| name.as_str())
        .collect();
    let excluded: Vec<serde_json::Value> = records
        .iter()
        .filter_map(|record| {
            let name = record["sim_name"].as_str()?;
            if ranked_names.contains(name) {
                return None;
            }
            let reason = if record["status"] == "no-verdict" {
                "no complete measured window"
            } else if record["status"] == "timeout" {
                "timeout"
            } else if record["status"] == "process failure" {
                "process failure"
            } else {
                "zero measured frames or missing census"
            };
            Some(
                serde_json::json!({"sim_name": name, "status": record["status"], "reason": reason}),
            )
        })
        .collect();
    let artifact = serde_json::json!({
        "schema": "rock.sim-allocation-census.v1",
        "project": project,
        "examined": selected.len(),
        "sensor": {
            "accounting": "gross_lua_allocator_requests",
            "jit": true,
            "trace_observer": false,
            "warmup_frames_requested": DEFAULT_SIM_ALLOCATION_WARMUP_FRAMES,
            "final_frame": "after_deferred_stop_sim",
            "text_layout_sim_probes": false,
            "imgui_sim_probes": false,
        },
        "records": records,
        "rankings": {
            "eligible_records": ranked.len(),
            "by_gross_kb_per_frame": census_rankings(&ranked, true),
            "by_allocation_calls_per_frame": census_rankings(&ranked, false),
            "excluded": excluded,
        },
        "interpretation": "Discovery evidence only. Sims differ in workload, duration, rendering mode, trace warmup, and accelerated game time; these values are not interchangeable gameplay benchmarks and are not regression thresholds.",
    });
    if let Some(parent) = path.parent() {
        fs_err::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(&artifact).context("serialize allocation census")?;
    fs_err::write(&path, bytes)
        .with_context(|| format!("writing allocation census `{}`", path.display()))?;
    if !path.is_file() {
        return Err(anyhow!(
            "allocation census write did not recreate `{}`",
            path.display()
        ));
    }
    println!(
        "[sim] allocation census: {} examined, {} complete ranked records, artifact={}",
        selected.len(),
        ranked.len(),
        path.display()
    );
    if !failed.is_empty() {
        return Err(anyhow!(
            "allocation census recorded {} failed/incomplete sims: {}",
            failed.len(),
            failed.join(", ")
        ));
    }
    Ok(())
}

fn allocation_site_artifact_stem(index: usize, name: &str) -> String {
    let safe = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    format!("{index:04}-{safe}")
}

fn allocation_activation_population(text: &str) -> Result<(usize, usize)> {
    let mut lines = text.lines();
    if lines.next() != Some("rock.allocation-activation.v2") {
        return Err(anyhow!(
            "sim allocation-site activation is not frame-aware v2"
        ));
    }
    let frames = lines
        .next()
        .and_then(|line| line.strip_prefix("frames\t"))
        .and_then(|value| value.parse::<usize>().ok())
        .ok_or_else(|| anyhow!("sim allocation-site activation has no frame denominator"))?;
    let sites = lines
        .next()
        .and_then(|line| line.strip_prefix("sites\t"))
        .and_then(|value| value.parse::<usize>().ok())
        .ok_or_else(|| anyhow!("sim allocation-site activation has no site population"))?;
    let rows = lines.count();
    if rows != sites {
        return Err(anyhow!(
            "sim allocation-site activation declared {sites} sites but wrote {rows} rows"
        ));
    }
    Ok((frames, sites))
}

pub(crate) fn do_run_allocation_sites(
    project: &str,
    names: &[String],
    output_dir: &str,
    jobs: Option<usize>,
) -> Result<()> {
    let artifact_dir = PathBuf::from(output_dir);
    if artifact_dir.exists() {
        if !artifact_dir.is_dir() {
            return Err(anyhow!(
                "allocation-site output `{}` is not a directory",
                artifact_dir.display()
            ));
        }
        if fs_err::read_dir(&artifact_dir)?.next().is_some() {
            return Err(anyhow!(
                "allocation-site output directory `{}` is not empty",
                artifact_dir.display()
            ));
        }
    } else {
        fs_err::create_dir_all(&artifact_dir)?;
    }
    let artifact_dir = fs_err::canonicalize(&artifact_dir)?;
    let manifest_path = artifact_dir.join("manifest.txt");

    let (project_path, out_dir, compiled) = compile_sim_diagnostic(project, Some(&manifest_path))?;
    let project = project_path.to_string_lossy().into_owned();
    let output = out_dir.path().to_string_lossy().into_owned();
    let registry = SimRegistry::from(compiled.registry);
    let sim_sources = resolve_sim_sources(&project_path, &registry.source_files)?;
    let artifact_fingerprint = compiled.stone.artifact_fingerprint().to_string();
    drop(compiled.stone);
    let generated_lua_path = artifact_dir.join("main.lua");
    fs_err::copy(
        Path::new(&output).join(rock_host::project::MAIN_LUA_FILENAME),
        &generated_lua_path,
    )?;
    let manifest = fs_err::read_to_string(&manifest_path)?;
    let generated_lua = fs_err::read_to_string(&generated_lua_path)?;

    let selected = registry.select(&project, names)?;
    for name in &selected {
        if entry_is_reload_lane(name, &registry.env_by_wire) {
            return Err(anyhow!(
                "allocation-site diagnostics do not support reload sim `{name}` because its emitted site IDs can change after the initial manifest"
            ));
        }
    }

    let results = SimRun {
        output: &output,
        env_by_wire: &registry.env_by_wire,
        specs: &selected,
        source_files: &sim_sources,
        jobs: resolve_pool_jobs(jobs, None, false)?,
        reload_lane: None,
    }
    .execute(SimMode::Sites(&artifact_dir))?;

    let mut records = Vec::with_capacity(selected.len());
    let mut failed = Vec::new();
    for (index, (name, result)) in selected.iter().zip(results).enumerate() {
        let stem = allocation_site_artifact_stem(index, name);
        let activation_name = format!("{stem}.activation.tsv");
        let joined_name = format!("{stem}.joined.tsv");
        let activation_path = artifact_dir.join(&activation_name);
        let joined_path = artifact_dir.join(&joined_name);
        match result {
            Ok((display_name, summary)) => {
                let status = verdict_word(&summary);
                if status != "PASS" {
                    failed.push(display_name.clone());
                    print_record(&display_name, &summary, false);
                    if !activation_path.exists() {
                        records.push(serde_json::json!({
                            "sim_name": display_name,
                            "status": status,
                            "process_id": summary.process_id,
                            "failure": "the sim wrote no allocation-site activation; its diagnostics are above",
                            "completed_frames": 0,
                            "active_sites": 0,
                        }));
                        continue;
                    }
                }
                let activation = fs_err::read_to_string(&activation_path).with_context(|| {
                    format!(
                        "reading allocation-site activation for `{display_name}` from `{}`",
                        activation_path.display()
                    )
                })?;
                let (frames, active_sites) = allocation_activation_population(&activation)?;
                let joined = crate::allocation_join::render_joined_report_with_jit(
                    &manifest,
                    &activation,
                    Some("all"),
                    Some(&generated_lua),
                    None,
                )?;
                fs_err::write(&joined_path, joined)?;
                let env = registry
                    .env_by_wire
                    .get(name)
                    .map_or(&[][..], Vec::as_slice);
                records.push(serde_json::json!({
                    "sim_name": display_name,
                    "status": status,
                    "process_id": summary.process_id,
                    "rendering_enabled": entry_renders(env),
                    "completed_frames": frames,
                    "active_sites": active_sites,
                    "activation": activation_name,
                    "joined": joined_name,
                }));
            }
            Err(error) => {
                failed.push(name.clone());
                records.push(serde_json::json!({
                    "sim_name": name,
                    "status": "process failure",
                    "failure": error.to_string(),
                    "completed_frames": 0,
                    "active_sites": 0,
                }));
            }
        }
    }
    let index = serde_json::json!({
        "schema": "rock.sim-allocation-sites.v1",
        "project": project,
        "artifact_fingerprint": artifact_fingerprint,
        "examined": selected.len(),
        "manifest": "manifest.txt",
        "generated_lua": "main.lua",
        "sensor": {
            "jit": true,
            "trace_observer": false,
            "frame_envelope": "update_and_draw",
            "timing": false,
            "allocator_sampling": false,
            "text_layout_sim_probes": false,
            "imgui_sim_probes": false,
        },
        "records": records,
        "interpretation": "Execution counts identify persistent and amplified compiler-known allocation-capable sites. They do not prove LuaJIT materialized heap memory; validate candidates with an ordinary JIT-on witness.",
    });
    fs_err::write(
        artifact_dir.join("index.json"),
        serde_json::to_vec_pretty(&index)?,
    )?;
    println!(
        "[sim] allocation sites: {} examined, artifact={}",
        selected.len(),
        artifact_dir.display()
    );
    if !failed.is_empty() {
        return Err(anyhow!(
            "allocation-site diagnostics recorded {} failed sims: {}",
            failed.len(),
            failed.join(", ")
        ));
    }
    Ok(())
}

#[derive(Debug)]
struct ReloadEditReq {
    request_id: Option<u64>,
    file: String,
    find: String,
    replace: String,
    site: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReloadTerminal {
    Applied,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ReloadCompletion {
    request_id: Option<u64>,
    terminal: ReloadTerminal,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ReloadLedgerSummary {
    requests: usize,
    applied: usize,
    pending: usize,
    rejected: usize,
    failed: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReloadRequestState {
    Booked,
    Published,
    Applied,
    Rejected,
    Failed,
}

fn reload_ledger_summary(requests: &BTreeMap<u64, ReloadRequestState>) -> ReloadLedgerSummary {
    let mut summary = ReloadLedgerSummary {
        requests: requests.len(),
        ..Default::default()
    };
    for state in requests.values() {
        match state {
            ReloadRequestState::Booked | ReloadRequestState::Published => summary.pending += 1,
            ReloadRequestState::Applied => summary.applied += 1,
            ReloadRequestState::Rejected => summary.rejected += 1,
            ReloadRequestState::Failed => summary.failed += 1,
        }
    }
    summary
}

fn service_reload_edit(
    stone: &mut crate::skull_backend::IncrementalProject,
    output: &str,
    pristine: &mut HashMap<String, String>,
    req: &ReloadEditReq,
) -> Result<crate::skull_backend::SkullReload> {
    let path = Path::new(&req.file);
    let current = stone
        .source_text(path)
        .ok_or_else(|| anyhow!("`{}` is not a source of this build", req.file))?;
    let hits = current.matches(&req.find).count();
    if hits != 1 {
        return Err(anyhow!(
            "find matched {hits} times in `{}` (need exactly 1): {:?}",
            req.file,
            req.find
        ));
    }
    pristine
        .entry(req.file.clone())
        .or_insert_with(|| current.clone());
    let new_text = current.replacen(&req.find, &req.replace, 1);
    let main_path = Path::new(output).join(rock_host::project::MAIN_LUA_FILENAME);
    let incremental_path = main_path.with_file_name("incremental.lua");
    match stone.apply_edit(path, &new_text, &main_path, &incremental_path)? {
        incremental @ crate::skull_backend::SkullReload::Incremental {
            bytes, reinferred, ..
        } => {
            log::info!(target: "SIM",
                "reload_edit `{}`: overlay {bytes}B, {reinferred} bodies", req.file);
            Ok(incremental)
        }
        other => Err(anyhow!(
            "must stay a body-only edit; `{}` routed to {other:?} \
             (interface/structural edits are not injectable mid-sim)",
            req.file
        )),
    }
}

fn book_reload_request(
    ledger: &mut BTreeMap<u64, ReloadRequestState>,
    req: &ReloadEditReq,
) -> Result<u64> {
    let request_id = req
        .request_id
        .ok_or_else(|| anyhow!("reload_edit omitted request_id"))?;
    if ledger
        .insert(request_id, ReloadRequestState::Booked)
        .is_some()
    {
        return Err(anyhow!("reload request {request_id} was booked twice"));
    }
    Ok(request_id)
}

fn drain_reload_completions(
    completions: &mpsc::Receiver<ReloadCompletion>,
    ledger: &mut BTreeMap<u64, ReloadRequestState>,
    errors: &mut Vec<SiteMessage>,
) -> bool {
    let mut protocol_error = false;
    while let Ok(completion) = completions.try_recv() {
        let failed = completion.terminal == ReloadTerminal::Failed;
        if let Err(e) = consume_reload_completion(ledger, completion) {
            errors.push(SiteMessage {
                message: format!("reload completion: {e}"),
                site: String::new(),
            });
            protocol_error = true;
        } else if failed {
            errors.push(SiteMessage {
                message:
                    "reload watcher reported failed publication; last-good artifact remains active"
                        .into(),
                site: String::new(),
            });
        }
    }
    protocol_error
}

fn consume_reload_completion(
    ledger: &mut BTreeMap<u64, ReloadRequestState>,
    completion: ReloadCompletion,
) -> Result<u64> {
    let request_id = completion
        .request_id
        .ok_or_else(|| anyhow!("reload_terminal omitted request_id"))?;
    match ledger.get_mut(&request_id) {
        Some(state @ ReloadRequestState::Published) => {
            *state = match completion.terminal {
                ReloadTerminal::Applied => ReloadRequestState::Applied,
                ReloadTerminal::Failed => ReloadRequestState::Failed,
            };
        }
        Some(state) => {
            return Err(anyhow!(
                "reload request {request_id} completed before publication or twice (state {state:?})"
            ));
        }
        None => {
            return Err(anyhow!(
                "reload request {request_id} completed before it was booked"
            ));
        }
    }
    Ok(request_id)
}

fn terminate_reload_child(child: &mut crate::headless_scope::BoundChild) -> Result<ExitStatus> {
    if let Err(kill_error) = child.kill() {
        if child.try_wait()?.is_none() {
            return Err(kill_error).context("stopping sim after reload rejection");
        }
    }
    child.wait().context("reaping sim after reload rejection")
}

fn revert_reload_edits(
    stone: &mut crate::skull_backend::IncrementalProject,
    output: &str,
    pristine: &mut HashMap<String, String>,
) -> Result<()> {
    let main_path = Path::new(output).join(rock_host::project::MAIN_LUA_FILENAME);
    let incremental_path = main_path.with_file_name("incremental.lua");
    for (file, text) in pristine.drain() {
        stone.apply_edit(Path::new(&file), &text, &main_path, &incremental_path)?;
    }
    crate::skull_backend::wipe_overlay(&incremental_path)
}

fn entry_is_reload_lane(spec: &str, env_by_wire: &HashMap<String, Vec<(String, String)>>) -> bool {
    env_by_wire.get(spec).is_some_and(|e| is_reload_lane(e))
}

fn sim_cpu_count() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

fn resolve_full_asset_jobs(requested: Option<usize>, jobs: usize) -> usize {
    requested
        .unwrap_or(DEFAULT_FULL_ASSET_JOBS)
        .clamp(1, jobs.max(1))
}

const DEFAULT_FULL_ASSET_JOBS: usize = 2;

pub(crate) fn resolve_jobs(
    requested: Option<usize>,
    lanes: Option<usize>,
    update_snapshots: bool,
) -> Result<usize> {
    let requested = narrow_to_lanes(requested, lanes);
    let budget = sim_job_budget();
    let jobs = resolve_jobs_with(requested, update_snapshots, sim_cpu_count(), budget)?;
    if let Some(asked) = requested.filter(|_| !update_snapshots) {
        static WARNED: std::sync::Once = std::sync::Once::new();
        if jobs < asked.min(sim_cpu_count()) {
            WARNED.call_once(|| log::warn!(target: "SIM",
                "--jobs {asked} exceeds the free-GPU budget of {} lanes; initial capacity {jobs}, expandable as VRAM frees", budget.gpu));
        }
    }
    Ok(jobs)
}

fn resolve_pool_jobs(
    requested: Option<usize>,
    lanes: Option<usize>,
    update_snapshots: bool,
) -> Result<usize> {
    resolve_jobs(requested, lanes, update_snapshots)?;
    let budget = JobBudget {
        ram: sim_job_budget().ram,
        gpu: sim_cpu_count(),
    };
    resolve_jobs_with(
        narrow_to_lanes(requested, lanes),
        update_snapshots,
        sim_cpu_count(),
        budget,
    )
}

fn narrow_to_lanes(requested: Option<usize>, lanes: Option<usize>) -> Option<usize> {
    requested.map(|jobs| lanes.map_or(jobs, |lanes| jobs.min(lanes.max(1))))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct JobBudget {
    ram: usize,
    gpu: usize,
}

impl JobBudget {
    fn lanes(&self) -> usize {
        self.ram.min(self.gpu)
    }
}

fn resolve_jobs_with(
    requested: Option<usize>,
    update_snapshots: bool,
    cores: usize,
    budget: JobBudget,
) -> Result<usize> {
    if update_snapshots {
        if requested.is_some_and(|j| j > 1) {
            log::warn!(target: "SIM",
                "--update-snapshots rewrites image baselines; running serially");
        }
        return Ok(1);
    }
    let jobs = match requested {
        None => budget.lanes(),
        Some(jobs) if jobs > budget.ram => {
            anyhow::bail!(
                "--jobs {jobs} exceeds the RAM budget of {} lanes",
                budget.ram
            );
        }
        Some(jobs) => jobs.min(budget.gpu),
    };
    Ok(jobs.clamp(1, cores.max(1)))
}

const SIM_LANE_VRAM_MIB: u64 = 2048;

const SIM_VRAM_RESERVE_MIB: u64 = 6 * 1024;

fn vram_budgeted_jobs(cores: usize, free_vram_mib: Option<u64>) -> usize {
    let cores = cores.max(1);
    match free_vram_mib {
        Some(free) => {
            let lanes = free.saturating_sub(SIM_VRAM_RESERVE_MIB) / SIM_LANE_VRAM_MIB;
            usize::try_from(lanes).unwrap_or(cores).clamp(1, cores)
        }
        None => cores,
    }
}

fn memory_budgets(
    cores: usize,
    total_ram_mib: Option<u64>,
    free_vram_mib: Option<u64>,
) -> JobBudget {
    let gpu = vram_budgeted_jobs(cores, free_vram_mib);
    let ram = total_ram_mib
        .map(|ram| {
            usize::try_from(ram.saturating_mul(10) / (36 * 1024))
                .unwrap_or(cores)
                .clamp(1, cores.max(1))
        })
        .unwrap_or(cores.max(1));
    JobBudget { ram, gpu }
}

fn total_system_memory_mib() -> Option<u64> {
    let mib = crate::common::total_system_memory_mib();
    if mib.is_none() {
        log::warn!(target: "SIM", "no total RAM reading; default jobs use CPU/GPU bounds only");
    }
    mib
}

fn sim_job_budget() -> JobBudget {
    static BUDGET: std::sync::OnceLock<JobBudget> = std::sync::OnceLock::new();
    *BUDGET.get_or_init(|| {
        let cores = sim_cpu_count();
        let total_ram = total_system_memory_mib();
        let probe = free_gpu_memory_mib();
        let free_vram = probe.as_ref().ok().copied().flatten();
        let mut budget = memory_budgets(cores, total_ram, free_vram);
        if let Err(reason) = probe {
            log::warn!(target: "SIM", "GPU memory probe failed: {reason}; limiting to one lane");
            budget.gpu = 1;
        }
        let default_jobs = budget.lanes();
        log::info!(target: "SIM",
            "default jobs {default_jobs}: {cores} cores, total RAM {total_ram:?} MiB at 3.6 GiB a lane ({} lanes), free GPU {free_vram:?} MiB less {SIM_VRAM_RESERVE_MIB} MiB reserve at {SIM_LANE_VRAM_MIB} MiB a lane ({} lanes)",
            budget.ram, budget.gpu);
        budget
    })
}

fn free_gpu_memory_mib() -> std::result::Result<Option<u64>, String> {
    let output = match std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=memory.free", "--format=csv,noheader,nounits"])
        .output()
    {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("nvidia-smi: {error}")),
    };
    if !output.status.success() {
        return Err(format!("nvidia-smi exited with {}", output.status));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_nvidia_smi_free_mib(&stdout)
        .map(Some)
        .ok_or_else(|| format!("nvidia-smi printed no free memory: {:?}", stdout.trim()))
}

fn parse_nvidia_smi_free_mib(stdout: &str) -> Option<u64> {
    stdout.lines().next()?.trim().parse().ok()
}

fn emit_bytecode_image(output: &Path) {
    let lua_file = output.join(rock_host::project::MAIN_LUA_FILENAME);
    let bytecode = output.join(rock_host::project::MAIN_BYTECODE_FILENAME);
    let started = rock_core::clock::Instant::now();
    let command = rock_host::project::build_luajit_bytecode_command(&lua_file, &bytecode);
    let mut command = crate::runtime_process::configure(
        command,
        crate::runtime_process::RuntimeProcess::LuaBytecode,
    );
    let result = command.output();
    let failure = match result {
        Ok(out) if out.status.success() => None,
        Ok(out) => Some(String::from_utf8_lossy(&out.stderr).trim().to_string()),
        Err(e) => Some(e.to_string()),
    };
    match failure {
        None => log::debug!(target: "SIM",
            "bytecode image `{}` written in {}ms", bytecode.display(),
            started.elapsed().as_millis()),
        Some(why) => {
            log::warn!(target: "SIM",
                "bytecode image not written, workers boot from source `{}`: {}",
                lua_file.display(), why);
            let _ = std::fs::remove_file(&bytecode);
        }
    }
}

fn worker_chunk_path(output: &Path, reload: bool) -> PathBuf {
    let main = output.join(rock_host::project::MAIN_LUA_FILENAME);
    if reload {
        return main;
    }
    let bytecode = output.join(rock_host::project::MAIN_BYTECODE_FILENAME);
    if bytecode.is_file() { bytecode } else { main }
}

pub(crate) fn is_reload_lane(env_pairs: &[(String, String)]) -> bool {
    env_pairs
        .iter()
        .any(|(k, v)| k == "STONE_HOT_RELOAD" && v != "0")
}

const RUNNER_BASELINE_ENV: &[(&str, &str)] = &[
    ("ROCK_TEST_SEED", "42"),
    ("SIM_DRAW", "none"),
    ("STONE_HEADLESS", "1"),
    ("STONE_IMAGE_MAX_SIDE", "256"),
    ("XDG_DATA_HOME", "$OUT/data"),
];

/// The last SIM_DRAW wins. Command-only runs do not submit to the GPU.
fn entry_renders(env: &[(String, String)]) -> bool {
    sim_env_value(env, "SIM_DRAW").is_some_and(|v| matches!(v, "on_demand" | "every_frame"))
}

fn entry_full_assets(env: &[(String, String)], default_stub: &str) -> bool {
    let stub = sim_env_value(env, "STONE_ASSET_STUB").unwrap_or(default_stub);
    let cap_default = RUNNER_BASELINE_ENV
        .iter()
        .find(|(k, _)| *k == "STONE_IMAGE_MAX_SIDE")
        .map(|(_, v)| *v)
        .expect("the runner baseline declares the image cap");
    let cap = sim_env_value(env, "STONE_IMAGE_MAX_SIDE").unwrap_or(cap_default);
    stub == "0" && cap == "0"
}

fn workload_env(env: &[(String, String)]) -> Vec<(String, String)> {
    let mut effective: std::collections::BTreeMap<String, String> = RUNNER_BASELINE_ENV
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    effective.insert("STONE_ASSET_STUB".into(), asset_stub_value().into());
    effective.insert(
        "STONE_AUDIO_STUB".into(),
        unstub_override("STONE_AUDIO_STUB").into(),
    );
    effective.extend(env.iter().cloned());
    effective.into_iter().collect()
}

fn asset_stub_value() -> &'static str {
    unstub_override("STONE_ASSET_STUB")
}

fn unstub_override(var: &str) -> &'static str {
    unstub_from(std::env::var(var).ok().as_deref())
}

fn unstub_from(inherited: Option<&str>) -> &'static str {
    match inherited {
        Some("0") => "0",
        _ => "1",
    }
}

#[derive(Clone)]
struct WorkerPart {
    name: String,
    out_dir: PathBuf,
    frame_budget: usize,
    timeout: Duration,
    env: Vec<(String, String)>,
    occurrence_id: u64,
    source_file: Option<PathBuf>,
}

struct EntryStream {
    name: String,
    buffered: bool,
    started: bool,
    terminated: bool,
    summary: SimSummary,
    protocol_error: Option<anyhow::Error>,
    marker_error: Option<String>,
    raw_tail: VecDeque<String>,
}

enum EntryBoundary {
    Started,
    Ended(Option<usize>, Option<FrameTiming>),
}

struct EntryLine<'a> {
    diagnostic: Option<&'a str>,
    boundary: Option<EntryBoundary>,
}

impl EntryStream {
    fn new(name: String, buffered: bool) -> Self {
        Self {
            name,
            buffered,
            started: false,
            terminated: false,
            summary: SimSummary::default(),
            protocol_error: None,
            marker_error: None,
            raw_tail: VecDeque::new(),
        }
    }

    fn observe<'line>(
        &mut self,
        line: &'line str,
        report: impl FnOnce(&EntryLine<'line>, Option<&SimEvent<'_>>),
    ) -> EntryLine<'line> {
        let (raw, payload) = line
            .split_once("__SIM__")
            .map_or((line, None), |(prefix, payload)| (prefix, Some(payload)));
        let keep_raw = payload.is_none() || !raw.is_empty();
        let mut observed = EntryLine {
            diagnostic: keep_raw.then_some(raw),
            boundary: None,
        };
        let payload_value;
        let mut event = None;
        let mut protocol_error = None;
        let mut marker_error = None;
        if let Some(payload) = payload {
            match parse_sim_marker(payload, line) {
                Ok(value) => {
                    payload_value = value;
                    let op = payload_value["op"].as_str().expect("validated marker");
                    if self.protocol_error.is_none() {
                        match self.boundary(op, &payload_value, line) {
                            Ok(boundary) => observed.boundary = boundary,
                            Err(error) => protocol_error = Some(error),
                        }
                    }
                    event = Some(SimEvent::decode(op, &payload_value));
                }
                Err(error) => {
                    if self.marker_error.is_none() {
                        marker_error = Some(error.to_string());
                    }
                    protocol_error = Some(error);
                    observed.diagnostic = Some(line);
                }
            }
        }

        // Keep the completed prefix if reporting or reading panics.
        report(&observed, event.as_ref());
        if !self.started && !raw.is_empty() {
            if self.raw_tail.len() == 8 {
                self.raw_tail.pop_front();
            }
            self.raw_tail.push_back(raw.chars().take(240).collect());
        }
        if self.buffered && keep_raw {
            self.summary.diagnostics.push_str(raw);
            self.summary.diagnostics.push('\n');
        }
        self.marker_error = self.marker_error.take().or(marker_error);
        self.protocol_error = self.protocol_error.take().or(protocol_error);
        match observed.boundary {
            Some(EntryBoundary::Started) => self.started = true,
            Some(EntryBoundary::Ended(frame_count, _)) => {
                self.terminated = true;
                self.summary.frame_count = frame_count;
            }
            None if self.protocol_error.is_none() => {
                if let Some(event) = event {
                    event.record(&mut self.summary);
                }
            }
            None => {}
        }
        observed
    }

    fn read(
        &mut self,
        reader: impl BufRead,
        mut report: impl FnMut(&EntryLine<'_>, Option<&SimEvent<'_>>),
    ) -> Option<String> {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            for line in reader.lines() {
                self.observe(&line?, &mut report);
            }
            Ok::<(), std::io::Error>(())
        })) {
            Ok(Ok(())) => self.marker_error.take(),
            Ok(Err(error)) => Some(format!("error reading game stderr: {error}")),
            Err(panic) => Some(stderr_panic_message(panic)),
        }
    }

    fn boundary(
        &self,
        op: &str,
        event: &serde_json::Value,
        line: &str,
    ) -> Result<Option<EntryBoundary>> {
        let open = self.started && !self.terminated;
        match op {
            "entry_start" if self.started => {
                anyhow::bail!("sim child started its entry twice: {line}")
            }
            "entry_end" if !open => {
                anyhow::bail!("sim child ended an entry that was not open: {line}")
            }
            "entry_start" | "entry_end" => {}
            _ if open => return Ok(None),
            _ => anyhow::bail!(
                "sim child emitted a `{op}` operation outside its entry envelope: {line}"
            ),
        }
        let name = event["name"]
            .as_str()
            .ok_or_else(|| anyhow!("sim child sent an entry envelope with no name: {line}"))?;
        anyhow::ensure!(
            name == self.name,
            "sim child envelope names `{name}` where the runner asked for `{}`: {line}",
            self.name
        );
        if op == "entry_start" {
            return Ok(Some(EntryBoundary::Started));
        }
        let frame_count = event
            .get("frame_count")
            .map(|value| {
                value
                    .as_u64()
                    .and_then(|n| usize::try_from(n).ok())
                    .ok_or_else(|| anyhow!("sim child sent an invalid entry frame_count: {line}"))
            })
            .transpose()?;
        let timing = event
            .get("frame_timing")
            .map(|value| FrameTiming::from_child_value(value, frame_count, line))
            .transpose()?;
        Ok(Some(EntryBoundary::Ended(frame_count, timing)))
    }
}

fn stderr_panic_message(panic: Box<dyn std::any::Any + Send>) -> String {
    format!("stderr reader thread panicked; sim output may be incomplete: {panic:?}")
}

fn sim_worker_slot_directory() -> PathBuf {
    #[cfg(unix)]
    {
        PathBuf::from(format!("/tmp/rock-sim-worker-slots-{}", unsafe {
            libc::geteuid()
        }))
    }
    #[cfg(not(unix))]
    {
        std::env::temp_dir().join("rock-sim-worker-slots")
    }
}

fn acquire_worker_slot(
    label: &str,
) -> Result<(std::fs::File, u64, u64, BTreeMap<&'static str, u64>)> {
    let slots = sim_job_budget().ram;
    let root = sim_worker_slot_directory();
    let started = rock_core::clock::Instant::now();
    let mut checks = Duration::ZERO;
    let mut waits: BTreeMap<&'static str, Duration> = BTreeMap::new();
    let mut waiting = false;
    loop {
        let checking = rock_core::clock::Instant::now();
        let result = worker_admission::try_acquire(&root, slots, worker_admission::gpu_memory)?;
        checks += checking.elapsed();
        let reason = match result {
            worker_admission::Admission::Granted(slot) => {
                if waiting {
                    log::info!(target: "SIM", "worker admitted after capacity wait: {label}");
                }
                return Ok((
                    slot,
                    started.elapsed().as_millis() as u64,
                    checks.as_millis() as u64,
                    waits
                        .into_iter()
                        .map(|(reason, time)| (reason, time.as_millis() as u64))
                        .collect(),
                ));
            }
            worker_admission::Admission::Denied(reason) => reason,
        };
        if !waiting {
            log::info!(target: "SIM", "waiting for host sim capacity (at most {slots} workers, {reason}): {label}");
            waiting = true;
        }
        let blocked = rock_core::clock::Instant::now();
        std::thread::sleep(Duration::from_millis(100));
        *waits.entry(reason).or_default() += blocked.elapsed();
    }
}
struct ChildConfig<'a> {
    part: &'a WorkerPart,
    quiet: bool,
    buffered: bool,
    update_snapshots: bool,
    output: &'a str,
    census_warmup_frames: Option<usize>,
    stone: Option<&'a mut crate::skull_backend::IncrementalProject>,
    recorder: Option<&'a SimRunRecorder>,
}

fn child_command(
    part: &WorkerPart,
    output: &str,
    quiet: bool,
    census_warmup_frames: Option<usize>,
) -> (std::process::Command, PathBuf) {
    let out_str = part.out_dir.to_string_lossy().to_string();
    let preset_env: Vec<(String, String)> = part
        .env
        .iter()
        .map(|(k, v)| (k.clone(), v.replace("$OUT", &out_str)))
        .collect();

    let lua_file = worker_chunk_path(Path::new(output), is_reload_lane(&part.env));
    let mut cmd = crate::headless_scope::headless_command(
        rock_engine_host::runtime::build_luajit_command(&lua_file, &[]),
    );
    cmd.env("ROCK_RUN_SIM", &part.name)
        .env("FRAME_COUNT", part.frame_budget.to_string())
        .env("OUT", &part.out_dir)
        .envs(
            RUNNER_BASELINE_ENV
                .iter()
                .map(|(k, v)| (*k, v.replace("$OUT", &out_str))),
        )
        .env("STONE_ASSET_STUB", asset_stub_value())
        .env("STONE_AUDIO_STUB", unstub_override("STONE_AUDIO_STUB"))
        .envs(preset_env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .env("FPS_LOG", "0")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    match census_warmup_frames {
        Some(warmup) => {
            cmd.env("STONE_SIM_ALLOCATION_CENSUS", "1")
                .env("STONE_SIM_ALLOCATION_WARMUP_FRAMES", warmup.to_string())
                .env("STONE_TEXT_LAYOUT_SIM_PROBES", "0")
                .env("STONE_IMGUI_SIM_PROBES", "0");
        }
        None => {
            cmd.env("STONE_SIM_ALLOCATION_CENSUS", "0");
        }
    }

    let rock_log_preset = preset_env.iter().any(|(k, _)| k == "ROCK_LOG");
    if quiet && !rock_log_preset && std::env::var_os("ROCK_LOG").is_none() {
        cmd.env("ROCK_LOG", "warn");
    }
    (cmd, lua_file)
}

struct ChildIo {
    stderr_thread: std::thread::JoinHandle<(EntryStream, Option<String>)>,
    stdout_thread: std::thread::JoinHandle<Option<String>>,
    stdout_buf: Arc<Mutex<Vec<u8>>>,
    // The wall-clock deadline is measured from the spawn until the entry
    // opens, then from its `entry_start`, so boot does not spend its budget.
    deadline_anchor: Arc<Mutex<rock_core::clock::Instant>>,
}

struct ReloadChannels {
    requests: mpsc::Receiver<ReloadEditReq>,
    completions: mpsc::Receiver<ReloadCompletion>,
}

#[derive(Default)]
struct ReloadState {
    pristine: HashMap<String, String>,
    errors: Vec<SiteMessage>,
    ledger: BTreeMap<u64, ReloadRequestState>,
}

impl ReloadState {
    fn drain_completions(&mut self, channels: &ReloadChannels) -> bool {
        drain_reload_completions(&channels.completions, &mut self.ledger, &mut self.errors)
    }

    fn reject(&mut self, request_id: u64, message: String, site: String) {
        self.ledger.insert(request_id, ReloadRequestState::Rejected);
        self.errors.push(SiteMessage { message, site });
    }
}

struct Waited {
    status: ExitStatus,
    timed_out: bool,
    runner_killed: bool,
    memory_error: Option<SiteMessage>,
}

impl Waited {
    fn exited(status: ExitStatus) -> Self {
        Self {
            status,
            timed_out: false,
            runner_killed: false,
            memory_error: None,
        }
    }
}

fn spawn_readers(
    child: &mut crate::headless_scope::BoundChild,
    part: &WorkerPart,
    quiet: bool,
    buffered: bool,
    recorder: Option<&SimRunRecorder>,
    worker_attempt: u64,
) -> (ChildIo, ReloadChannels) {
    let stderr_handle = child.stderr.take().expect("piped");
    let stdout_handle = child.stdout.take().expect("piped");
    let stdout_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let deadline_anchor = Arc::new(Mutex::new(rock_core::clock::Instant::now()));
    let (reload_tx, requests) = mpsc::channel::<ReloadEditReq>();
    let (completion_tx, completions) = mpsc::channel::<ReloadCompletion>();

    let stderr_thread = {
        let anchor = Arc::clone(&deadline_anchor);
        let recorder = recorder.cloned();
        let (occurrence_id, name) = (part.occurrence_id, part.name.clone());
        // Buffered runs must not write from this thread: several children are
        // live, so lines are retained for the caller to emit inside its lock.
        let quiet = quiet || buffered;
        std::thread::spawn(move || {
            let mut stream = EntryStream::new(name.clone(), buffered);
            let diagnostic = stream.read(BufReader::new(stderr_handle), |observed, event| {
                let boundary = &observed.boundary;
                if matches!(boundary, Some(EntryBoundary::Started)) {
                    *anchor.lock().unwrap() = rock_core::clock::Instant::now();
                }
                if let Some(recorder) = recorder.as_ref() {
                    match boundary {
                        Some(EntryBoundary::Started) => {
                            recorder.worker_first_entry(worker_attempt, occurrence_id, &name);
                            recorder.entry_start(worker_attempt, occurrence_id, &name);
                        }
                        Some(EntryBoundary::Ended(frame_count, frame_timing)) => {
                            recorder.entry_end(
                                worker_attempt,
                                occurrence_id,
                                &name,
                                *frame_count,
                                *frame_timing,
                            );
                        }
                        _ => {}
                    }
                }
                if !buffered {
                    if let Some(diagnostic) = observed.diagnostic {
                        eprintln!("{diagnostic}");
                    }
                }
                if let Some(event) = event {
                    event.forward_reload(&reload_tx, &completion_tx);
                    if !quiet && let Some(line) = event.progress() {
                        eprintln!("{line}");
                    }
                }
            });
            (stream, diagnostic)
        })
    };

    let stdout_thread = {
        let buf = Arc::clone(&stdout_buf);
        let label = part.name.clone();
        std::thread::spawn(move || -> Option<String> {
            let mut h = stdout_handle;
            let mut local = Vec::new();
            let error = h
                .read_to_end(&mut local)
                .err()
                .map(|e| format!("rock sim `{label}`: error reading game stdout: {e}"));
            *buf.lock().unwrap() = local;
            error
        })
    };

    (
        ChildIo {
            stderr_thread,
            stdout_thread,
            stdout_buf,
            deadline_anchor,
        },
        ReloadChannels {
            requests,
            completions,
        },
    )
}

fn handle_reload_request(
    req: ReloadEditReq,
    config: &ChildConfig<'_>,
    stone: &mut Option<&mut crate::skull_backend::IncrementalProject>,
    reload: &mut ReloadState,
) -> bool {
    let request_id = match book_reload_request(&mut reload.ledger, &req) {
        Ok(id) => id,
        Err(e) => {
            reload.errors.push(SiteMessage {
                message: format!("reload request: {e}"),
                site: req.site,
            });
            return true;
        }
    };
    let serviced = match stone.as_deref_mut() {
        Some(st) => {
            let mut apply = || service_reload_edit(st, config.output, &mut reload.pristine, &req);
            match config.recorder {
                Some(recorder) => {
                    recorder.phase(&format!("reload_apply:{}", config.part.name), apply)
                }
                None => apply(),
            }
        }
        None => Err(anyhow!(
            "reload_edit is only available to sims that opt into the \
             reload lane with `@sim_env(\"STONE_HOT_RELOAD\", \"1\")`"
        )),
    };
    match serviced {
        Ok(crate::skull_backend::SkullReload::Incremental { .. }) => {
            reload
                .ledger
                .insert(request_id, ReloadRequestState::Published);
            false
        }
        Ok(other) => {
            reload.reject(
                request_id,
                format!("reload_edit: reload_edit returned non-incremental result {other:?}"),
                req.site,
            );
            true
        }
        Err(e) => {
            if !config.buffered {
                eprintln!(
                    "{}{} reload_edit: {e}",
                    format!("[{}] ", req.site).bright_black(),
                    "error:".red().bold(),
                );
            }
            reload.reject(request_id, format!("reload_edit: {e}"), req.site);
            true
        }
    }
}

fn wait_for_child(
    child: &mut crate::headless_scope::BoundChild,
    config: &ChildConfig<'_>,
    stone: &mut Option<&mut crate::skull_backend::IncrementalProject>,
    io: &ChildIo,
    channels: &ReloadChannels,
    reload: &mut ReloadState,
) -> Result<Waited> {
    let memory_cap_mb = sim_memory_cap_mb();
    loop {
        if reload.drain_completions(channels) {
            return Ok(Waited {
                runner_killed: true,
                ..Waited::exited(terminate_reload_child(child)?)
            });
        }
        if let Some(status) = child.try_wait()? {
            return Ok(Waited::exited(status));
        }
        if io.deadline_anchor.lock().unwrap().elapsed() > config.part.timeout {
            let _ = child.kill();
            return Ok(Waited {
                runner_killed: true,
                timed_out: true,
                ..Waited::exited(child.wait()?)
            });
        }
        if let (Some(cap), Some(used)) = (memory_cap_mb, process_memory_mb(child.id())) {
            if used > cap {
                let breakdown = process_memory_breakdown(child.id());
                let _ = child.kill();
                return Ok(Waited {
                    runner_killed: true,
                    memory_error: Some(SiteMessage {
                        message: format!(
                            "sim child exceeded its memory cap of {cap} MB ({used} MB resident \
                             plus swap: {breakdown}) and was killed; override with ROCK_SIM_MEMORY_MB"
                        ),
                        site: String::new(),
                    }),
                    ..Waited::exited(child.wait()?)
                });
            }
        }
        match channels.requests.recv_timeout(Duration::from_millis(50)) {
            Ok(req) => {
                if handle_reload_request(req, config, stone, reload) {
                    return Ok(Waited::exited(terminate_reload_child(child)?));
                }
                reload.drain_completions(channels);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

fn reject_late_reload_requests(channels: &ReloadChannels, reload: &mut ReloadState) {
    while let Ok(req) = channels.requests.try_recv() {
        match book_reload_request(&mut reload.ledger, &req) {
            Ok(request_id) => {
                let message = format!(
                    "reload_edit for `{}` arrived after the child exited (emit it before a wait_for on its effect)",
                    req.file
                );
                reload.reject(request_id, message, req.site);
            }
            Err(e) => reload.errors.push(SiteMessage {
                message: format!("reload request after child exit: {e}"),
                site: req.site,
            }),
        }
    }
}

fn record_worker_end(
    recorder: &SimRunRecorder,
    worker_attempt: u64,
    status: &ExitStatus,
    timed_out: bool,
) {
    #[cfg(unix)]
    let signal = status.signal();
    #[cfg(not(unix))]
    let signal = None;
    let outcome = if timed_out {
        "timeout"
    } else if status.success() {
        "success"
    } else {
        "failure"
    };
    recorder.worker_end(worker_attempt, outcome, status.code(), signal, timed_out);
}

fn join_readers(
    io: ChildIo,
    part: &WorkerPart,
    buffered: bool,
) -> (EntryStream, Vec<String>, String) {
    let label = &part.name;
    let mut diagnostics = Vec::new();
    let (stream, diagnostic) = match io.stderr_thread.join() {
        Ok(result) => result,
        Err(panic) => (
            EntryStream::new(part.name.clone(), buffered),
            Some(stderr_panic_message(panic)),
        ),
    };
    if let Some(message) = diagnostic {
        diagnostics.push(format!("rock sim `{label}`: {message}"));
    }
    match io.stdout_thread.join() {
        Ok(Some(message)) => diagnostics.push(message),
        Ok(None) => {}
        Err(panic) => diagnostics.push(format!(
            "rock sim `{label}`: stdout reader thread panicked; game stdout may be incomplete: {panic:?}"
        )),
    }
    let stdout = String::from_utf8_lossy(&io.stdout_buf.lock().unwrap()).to_string();
    (stream, diagnostics, stdout)
}

fn push_error(summary: &mut SimSummary, message: String) {
    summary.errors.push(SiteMessage {
        message,
        site: String::new(),
    });
}

fn record_exit_errors(
    summary: &mut SimSummary,
    part: &WorkerPart,
    waited: &Waited,
    started: bool,
    terminated: bool,
    raw_tail: VecDeque<String>,
) {
    let Waited {
        status,
        timed_out,
        runner_killed,
        memory_error,
    } = waited;
    if !started {
        let worker_end = describe_worker_end(
            status,
            *timed_out,
            part.timeout,
            *runner_killed,
            memory_error.as_ref(),
            raw_tail,
        );
        push_error(
            summary,
            format!(
                "sim `{}` never started: its process ended before the entry opened ({worker_end})",
                part.name,
            ),
        );
    }
    if started && !terminated && status.success() && !timed_out {
        push_error(
            summary,
            format!(
                "sim `{}` never emitted its entry_end although its process exited cleanly",
                part.name
            ),
        );
    }
}

fn run_child(mut config: ChildConfig<'_>) -> Result<SimSummary> {
    let mut stone = config.stone.take();
    let part = config.part;
    let (quiet, buffered, recorder) = (config.quiet, config.buffered, config.recorder);
    let output = config.output;

    // The overlay is one file shared by every child of this invocation, so
    // only the exclusive reload lane may touch it. With STONE_HOT_RELOAD=1 a
    // leftover would be dofile'd at startup.
    if stone.is_some() {
        crate::skull_backend::wipe_overlay(
            &Path::new(output)
                .join(rock_host::project::MAIN_LUA_FILENAME)
                .with_file_name("incremental.lua"),
        )?;
    }

    let (mut cmd, lua_file) = child_command(part, output, quiet, config.census_warmup_frames);

    let mut worker_phase = worker_admission::WorkerPhase::waiting();
    let (mut worker_slot, admission_ms, checks_ms, waits) = acquire_worker_slot(&part.name)?;
    worker_phase.admitted();
    let worker_attempt = recorder
        .map(|recorder| {
            recorder.worker_spawn(part.occurrence_id, &part.name, &workload_env(&part.env))
        })
        .unwrap_or(0);
    if let Some(recorder) = recorder {
        recorder.worker_admission(worker_attempt, &part.name, admission_ms, checks_ms, &waits);
    }
    let child = match crate::headless_scope::spawn_bound_child(&mut cmd, lua_file.parent().unwrap())
    {
        Ok(child) => child,
        Err(error) => {
            if let Some(recorder) = recorder {
                recorder.worker_end(worker_attempt, "spawn_failed", None, None, false);
            }
            return Err(error.into());
        }
    };
    // Until the PID is published the admission lease reserves the full estimate.
    // Keep the child bound if publishing fails, so it is reaped before the lease drops.
    worker_admission::publish_pid(&sim_worker_slot_directory(), &mut worker_slot, child.id())?;
    // Declared after the leases, so the bound child is reaped before capacity
    // and the admission phase are released, including on early return.
    let mut child = child;
    let process_id = child.id();
    if let Some(recorder) = recorder {
        recorder.worker_started(worker_attempt, process_id);
    }

    let (io, channels) = spawn_readers(&mut child, part, quiet, buffered, recorder, worker_attempt);
    let mut reload = ReloadState::default();
    let waited = wait_for_child(&mut child, &config, &mut stone, &io, &channels, &mut reload)?;
    if let Some(recorder) = recorder {
        record_worker_end(recorder, worker_attempt, &waited.status, waited.timed_out);
    }
    reject_late_reload_requests(&channels, &mut reload);

    let (stream, reader_diagnostics, stdout_str) = join_readers(io, part, buffered);
    reload.drain_completions(&channels);
    let reload_summary = reload_ledger_summary(&reload.ledger);
    if reload_summary.pending > 0 {
        reload.errors.push(SiteMessage {
            message: format!(
                "reload completion ledger has {} pending request(s): requests={} applied={} pending={} rejected={} failed={}",
                reload_summary.pending,
                reload_summary.requests,
                reload_summary.applied,
                reload_summary.pending,
                reload_summary.rejected,
                reload_summary.failed,
            ),
            site: String::new(),
        });
    }
    if !buffered {
        for message in &reader_diagnostics {
            log::warn!(target: "SIM", "{message}");
        }
    }

    let EntryStream {
        started,
        terminated,
        mut summary,
        raw_tail,
        protocol_error,
        ..
    } = stream;
    if let Some(error) = protocol_error {
        return Err(error);
    }

    summary.process_id = Some(process_id);
    if recorder.is_some() {
        summary.worker_attempt = Some(worker_attempt);
    }
    record_exit_errors(&mut summary, part, &waited, started, terminated, raw_tail);
    let Waited {
        status,
        timed_out,
        runner_killed,
        memory_error,
    } = waited;
    summary.errors.append(&mut reload.errors);
    summary.errors.extend(memory_error);
    if let Some(note) = crate::headless_scope::outside_kill_note(&status, runner_killed) {
        push_error(&mut summary, note);
    }
    if reload_summary.requests > 0 {
        summary.reload_ledger = Some(reload_summary);
    }
    summary.timed_out = timed_out;
    if timed_out {
        summary.timeout_budget = Some(part.timeout);
    }
    if !status.success() {
        summary.process_failure_code = Some(status.code().unwrap_or(-1));
        push_error(
            &mut summary,
            if timed_out {
                format!(
                    "sim `{}` exceeded its {}s wall-clock entry budget; \
                     its process ended with {status}; runner_killed={runner_killed}",
                    part.name,
                    part.timeout.as_secs()
                )
            } else {
                format!("sim process ended with {status}; runner_killed={runner_killed}")
            },
        );
    }

    match snapshot_source_for(part) {
        Ok(source) => process_image_snapshots(
            &mut summary,
            &snap_dir_for(&source),
            config.update_snapshots,
        ),
        Err(reason) if !summary.image_snapshots.is_empty() => push_error(&mut summary, reason),
        Err(_) => {}
    }

    let failed =
        summary.has_failures() || summary.timed_out || summary.process_failure_code.is_some();
    if buffered {
        let diag = &mut summary.diagnostics;
        for message in &reader_diagnostics {
            diag.push_str("[SIM] warning: ");
            diag.push_str(message);
            diag.push('\n');
        }
        if !quiet && failed && !stdout_str.trim().is_empty() {
            diag.push_str(&format!("--- game stdout for `{}` ---\n", part.name));
            diag.push_str(&stdout_str);
        }
    }

    if let Some(st) = stone {
        let result = if reload.pristine.is_empty() {
            revert_reload_edits(st, output, &mut reload.pristine)
        } else {
            let mut restore = || revert_reload_edits(st, output, &mut reload.pristine);
            match recorder {
                Some(recorder) => recorder.phase(&format!("reload_restore:{}", part.name), restore),
                None => restore(),
            }
        };
        if let Err(e) = result {
            push_error(&mut summary, format!("reload_edit revert failed: {e}"));
        }
    }

    let failed =
        summary.has_failures() || summary.timed_out || summary.process_failure_code.is_some();
    if !quiet && !buffered && failed && !stdout_str.trim().is_empty() {
        log::error!(target: "SIM", "--- game stdout for `{}` ---", part.name);
        eprint!("{stdout_str}");
    }

    Ok(summary)
}

fn describe_worker_end(
    status: &ExitStatus,
    timed_out: bool,
    timeout: Duration,
    runner_killed: bool,
    memory_error: Option<&SiteMessage>,
    mut stderr: VecDeque<String>,
) -> String {
    let mut out = format!("worker {status}");
    if timed_out {
        out.push_str(&format!(
            "; the runner killed it at the {}s entry deadline",
            timeout.as_secs()
        ));
    } else if let Some(memory) = memory_error {
        out.push_str(&format!("; {}", memory.message));
    } else if runner_killed {
        out.push_str("; the runner killed it");
    } else if let Some(note) = crate::headless_scope::outside_kill_note(status, runner_killed) {
        out.push_str(&format!("; {note}"));
    }
    if stderr.is_empty() {
        out.push_str("; no game stderr");
    } else {
        out.push_str(&format!(
            "; last game stderr: {}",
            stderr.make_contiguous().join(" | ")
        ));
    }
    out
}

fn snapshot_source_for(part: &WorkerPart) -> Result<PathBuf, String> {
    part.source_file.clone().ok_or_else(|| {
        format!(
            "sim `{}` has no compiled source file; image_snapshot cannot resolve a baseline dir",
            part.name
        )
    })
}

fn snap_dir_for(source: &Path) -> PathBuf {
    let stem = source.file_stem().unwrap_or_default();
    let parent = source.parent().unwrap_or_else(|| Path::new("."));
    let mut name = stem.to_os_string();
    name.push(".snap");
    parent.join(name)
}

fn process_image_snapshots(summary: &mut SimSummary, snap_dir: &Path, update: bool) {
    for img in std::mem::take(&mut summary.image_snapshots) {
        let site = img.site.clone();
        if !img.save_ok {
            summary.errors.push(SiteMessage {
                message: prefix_with_site(
                    &site,
                    &format!(
                        "image_snapshot `{}`: stone_save_screenshot returned false (engine refused readback)",
                        img.name
                    ),
                ),
                site: String::new(),
            });
            continue;
        }
        let baseline = snap_dir.join(format!("{}.png", img.name));
        match handle_image_snapshot(&img, &baseline, update) {
            Ok(SnapResult::Pass) => summary.expects_pass.push(SiteMessage {
                message: prefix_with_site(
                    &site,
                    &format!("image_snapshot `{}` within tolerance", img.name),
                ),
                site: String::new(),
            }),
            Ok(SnapResult::Wrote) => summary.expects_pass.push(SiteMessage {
                message: prefix_with_site(
                    &site,
                    &format!("image_snapshot `{}` baseline written", img.name),
                ),
                site: String::new(),
            }),
            Ok(SnapResult::Fail(msg)) => summary.expects_fail.push(SiteMessage {
                message: prefix_with_site(&site, &msg),
                site: String::new(),
            }),
            Err(e) => summary.errors.push(SiteMessage {
                message: prefix_with_site(&site, &format!("image_snapshot `{}`: {}", img.name, e)),
                site: String::new(),
            }),
        }
        let _ = std::fs::remove_file(&img.path);
    }
}

fn prefix_with_site(site: &str, msg: &str) -> String {
    if site.is_empty() {
        msg.to_string()
    } else {
        format!("at {site}: {msg}")
    }
}

#[derive(Debug)]
enum SnapResult {
    Pass,
    Wrote,
    Fail(String),
}

fn handle_image_snapshot(
    img: &ImageSnapshotOp,
    baseline: &Path,
    update: bool,
) -> Result<SnapResult> {
    let produced = Path::new(&img.path);
    if !produced.exists() {
        return Err(anyhow!(
            "produced PNG missing at `{}`; engine save_screenshot did not write the file",
            produced.display()
        ));
    }

    if update {
        if let Some(parent) = baseline.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(produced, baseline)?;
        return Ok(SnapResult::Wrote);
    }
    if !baseline.exists() {
        return Ok(SnapResult::Fail(format!(
            "image_snapshot `{}` has no baseline at `{}`; re-run with --update-snapshots to \
             create it (a missing golden is not a pass)",
            img.name,
            baseline.display()
        )));
    }

    let diff = crate::image_snapshot::image_diff(baseline, produced)
        .map_err(|e| anyhow!("image_diff failed: {e}"))?;
    if diff <= img.tol {
        Ok(SnapResult::Pass)
    } else {
        let actual_path = baseline.with_extension("png.actual");
        std::fs::copy(produced, &actual_path)?;
        Ok(SnapResult::Fail(format!(
            "image_snapshot `{}` diff {:.6} > tol {:.6} (wrote {} for diff)",
            img.name,
            diff,
            img.tol,
            actual_path.display()
        )))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SiteMessage {
    pub message: String,
    pub site: String,
}

#[derive(Default)]
struct SimSummary {
    expects_pass: Vec<SiteMessage>,
    expects_fail: Vec<SiteMessage>,
    errors: Vec<SiteMessage>,
    saw_stop: bool,
    frame_count: Option<usize>,
    process_id: Option<u32>,
    worker_attempt: Option<u64>,
    allocation_census: Option<AllocationCensus>,
    image_snapshots: Vec<ImageSnapshotOp>,
    cleanup_paths: Vec<String>,
    timed_out: bool,
    timeout_budget: Option<Duration>,
    process_failure_code: Option<i32>,
    diagnostics: String,
    reload_ledger: Option<ReloadLedgerSummary>,
    excluded: Option<String>,
    no_checks: Option<String>,
}

struct ImageSnapshotOp {
    name: String,
    path: String,
    tol: f64,
    save_ok: bool,
    site: String,
}

impl SimSummary {
    fn has_failures(&self) -> bool {
        !self.expects_fail.is_empty() || !self.errors.is_empty()
    }

    fn checks_accounted(&self) -> bool {
        self.checks_ran() > 0 || self.no_checks.is_some()
    }

    fn checks_ran(&self) -> usize {
        self.expects_pass.len() + self.expects_fail.len() + self.image_snapshots.len()
    }

    fn print(&self, sim_name: &str, streamed: bool) {
        for (label, messages) in [("error", &self.errors), ("FAIL", &self.expects_fail)] {
            for m in messages {
                if m.site.is_empty() {
                    eprintln!("[sim] {label}: {}", m.message);
                } else if !streamed {
                    eprintln!("[sim] {label} [{}]: {}", m.site, m.message);
                }
            }
        }
        if let Some(ledger) = self.reload_ledger {
            eprintln!(
                "[sim] reload ledger: requests={} applied={} pending={} rejected={} failed={}",
                ledger.requests, ledger.applied, ledger.pending, ledger.rejected, ledger.failed
            );
        }
        let total = self.expects_pass.len() + self.expects_fail.len();
        let status = verdict_word(self);
        if let Some(reason) = &self.excluded {
            eprintln!("[sim] {sim_name}: {status} ({reason})");
            return;
        }
        let stop_note = if !self.saw_stop {
            " (no stop_sim() observed -- sim aborted early)"
        } else if !self.checks_accounted() {
            " (no check ran)"
        } else {
            ""
        };
        eprintln!(
            "[sim] {sim_name}: {status} ({}/{} expects passed){stop_note}",
            self.expects_pass.len(),
            total
        );
    }
}

#[cfg(test)]
#[path = "run_sim_tests.rs"]
mod tests;
