//! `rock sim <project> <name>` — runner for Rock-native `@sim`
//! coroutines. See `docs/sim-layering.md` for the current sim architecture.
//!
//! Orchestrates: compile project + spawn the game with
//! `ROCK_RUN_SIM=<name>` set, demux `__SIM__`-prefixed JSON lines from
//! stderr, pretty-print pass/fail, return non-zero on any expect_fail.
//!
//! Side-channel format: stderr lines with the literal prefix
//! `__SIM__ {json}\n`, where each JSON object has `op: string` and
//! op-specific fields. See `stone/engine/sim.rock`.

use anyhow::{anyhow, Context, Result};
use crate::sim_observability::SimRunRecorder;
use std::collections::{BTreeMap, HashMap};
use colored::Colorize;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;
use tempfile::TempDir;
use sha2::{Digest, Sha256};

#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;

/// Frame budget for a sim that declares none. Sims terminate naturally via the
/// runner's `sim_done` check; this is the upper bound for a runaway / hung
/// body. A sim overrides it with `@sim_env("FRAME_COUNT", "<n>")`, which is a
/// PER-ENTRY value: `sim_start_entry` republishes it at every entry boundary
/// and the entry plan carries it, so entries with different budgets can share
/// one worker process.
const DEFAULT_FRAME_BUDGET: usize = 600;

/// The budget one entry runs on: its own `@sim_env("FRAME_COUNT", ...)`, or the
/// default. Rejects a value that is not a positive integer rather than falling
/// back -- a sim asking for a bound the runner silently replaces would be
/// guarded by a number nobody wrote.
fn frame_budget_for(sim_name: &str, env_pairs: &[(String, String)]) -> Result<usize> {
    let Some((_, raw)) = env_pairs.iter().rev().find(|(k, _)| k == "FRAME_COUNT") else {
        return Ok(DEFAULT_FRAME_BUDGET);
    };
    raw.parse::<usize>().ok().filter(|n| *n > 0).ok_or_else(|| {
        anyhow!("sim `{sim_name}` declares @sim_env(\"FRAME_COUNT\", \"{raw}\"), which is not a positive integer")
    })
}

fn sim_required_backend(env_pairs: &[(String, String)]) -> Option<&str> {
    for (k, v) in env_pairs {
        if k == "SIM_BACKEND"
            || k == "STONE_RENDERER"
            || k == "SIM_REQUIRES"
            || k == "SIM_REQUIRED_RENDERER"
        {
            return Some(v.as_str());
        }
    }
    None
}

fn active_sim_renderer(project_dir: &Path) -> &'static str {
    match rock_host::stick::StickConfig::find_from_dir(project_dir) {
        Ok(Some((_, cfg))) => match cfg.renderer_backend() {
            Ok(rock_host::stick::RendererBackend::Wgpu) => "wgpu",
            _ => "gl",
        },
        _ => "gl",
    }
}
/// Wall-clock timeout. An INDEPENDENT bound, not a backstop for the frame
/// budget: it is the only thing that catches a child which never advances a
/// frame at all -- an infinite loop inside one tick, a blocked FFI call, a
/// driver stall -- where `FRAME_COUNT` can never be reached and so can never
/// fire. Neither bound subsumes the other.
///
/// It bounds ONE entry: boot up to the first `entry_start`, then each entry
/// from its own start. A worker's queued entries never accumulate against it.
///
/// Deliberately generous, because it is a wedge detector and not a deadline.
/// Per-child wall time rises sharply under `--jobs` (render-heavy mag sims have
/// been measured taking 15 s for a single frame with four children live), and a
/// value tuned for serial runs kills healthy children. Override with
/// `ROCK_SIM_TIMEOUT_SECS` for a genuinely long-running sim.
const DEFAULT_TIMEOUT_SECS: u64 = 300;

/// Resident-plus-swap ceiling for one sim child, in MB. A sim that loops
/// while allocating grows until the machine's OOM killer picks a victim, and
/// that victim's cgroup can take unrelated processes down with it; the
/// wall-clock watchdog is far too slow to stop it (one looping sim reached
/// ~25 GB inside the 300 s timeout, 2026-09-18). A normal MAG sim child peaks
/// under 3 GB. `ROCK_SIM_MEMORY_MB` overrides; `0` disables the cap.
const DEFAULT_MEMORY_CAP_MB: u64 = 8192;

#[derive(Clone, Debug, PartialEq)]
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

impl AllocationCensus {
    fn from_json(json: &str) -> Result<Self> {
        let value: serde_json::Value = serde_json::from_str(json)
            .context("allocation census side-channel payload is not JSON")?;
        let required_str = |name: &str| {
            value
                .get(name)
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| anyhow!("allocation census field `{name}` is not a string"))
        };
        let required_bool = |name: &str| {
            value
                .get(name)
                .and_then(serde_json::Value::as_bool)
                .ok_or_else(|| anyhow!("allocation census field `{name}` is not a bool"))
        };
        let required_usize = |name: &str| {
            value
                .get(name)
                .and_then(serde_json::Value::as_u64)
                .and_then(|n| usize::try_from(n).ok())
                .ok_or_else(|| anyhow!("allocation census field `{name}` is not a nonnegative integer"))
        };
        let required_float = |name: &str| {
            value
                .get(name)
                .and_then(serde_json::Value::as_f64)
                .ok_or_else(|| anyhow!("allocation census field `{name}` is not a number"))
        };

        Ok(Self {
            build_sha: required_str("build_sha")?,
            rendering_enabled: required_bool("rendering_enabled")?,
            warmup_frames_requested: required_usize("warmup_frames_requested")?,
            warmup_frames: required_usize("warmup_frames")?,
            frame_count: required_usize("frame_count")?,
            measured_frames: required_usize("measured_frames")?,
            initialization_gross_kb: required_float("initialization_gross_kb")?,
            outside_frame_gross_kb: required_float("outside_frame_gross_kb")?,
            warmup_gross_kb: required_float("warmup_gross_kb")?,
            measured_gross_kb: required_float("measured_gross_kb")?,
            total_gross_kb: required_float("total_gross_kb")?,
            measured_allocator_calls: value
                .get("measured_allocator_calls")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| anyhow!("allocation census field `measured_allocator_calls` is not an integer"))?,
            measured_allocation_calls: value
                .get("measured_allocation_calls")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| anyhow!("allocation census field `measured_allocation_calls` is not an integer"))?,
            measured_reallocation_calls: value
                .get("measured_reallocation_calls")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| anyhow!("allocation census field `measured_reallocation_calls` is not an integer"))?,
            stop_sim_reached: required_bool("stop_sim_reached")?,
            measurement_complete: required_bool("measurement_complete")?,
        })
    }
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

/// Resident plus swapped memory of a live process, in MB. Swap counts: the
/// looping sim that set the cap held 20 of its 25 GB in swap.
#[cfg(target_os = "linux")]
fn process_memory_mb(pid: u32) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let mut kb = 0u64;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:").or_else(|| line.strip_prefix("VmSwap:")) {
            kb += rest.trim().trim_end_matches("kB").trim().parse::<u64>().ok()?;
        }
    }
    Some(kb / 1024)
}

// Only Linux exposes this cheaply (/proc); elsewhere the cap is not enforced
// and the wall-clock watchdog remains the only bound.
#[cfg(not(target_os = "linux"))]
fn process_memory_mb(_pid: u32) -> Option<u64> {
    None
}

/// Split a `--sim` value on `,` into chain parts. Empty parts dropped,
/// whitespace trimmed. Single source of truth for the multi-start
/// chain syntax — used by `do_run_sim_opts` and `rock stone`'s sim
/// resolver.
pub fn parse_sim_chain(name: &str) -> Vec<&str> {
    name.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).collect()
}

/// Map each part of each requested spec to its registered wire name. A v1
/// package registers sims under canonical DefKeys (`game::module::name`); a
/// trailing `::`-segment path such as the declared name selects the one sim it
/// ends (spec sections 15 and 16). An exact wire name always wins.
///
/// Every name must be a registered sim, checked here before anything boots: a
/// helper coroutine named in a batch would otherwise kill its whole worker
/// with `unknown sim`, and a name holding whitespace (one argv element of
/// newline-joined names) splits the worker's entry-plan line. Every problem is
/// reported at once, one `unknown sim: <name>` line per name, so a batch is
/// fixed in one pass.
pub(crate) fn resolve_sim_specs(wire_names: &[String], specs: &[String]) -> Result<Vec<String>> {
    let mut problems = Vec::new();
    let mut resolved = Vec::with_capacity(specs.len());
    for spec in specs {
        let mut parts = Vec::new();
        for part in parse_sim_chain(spec) {
            match resolve_sim_name(wire_names, part) {
                Ok(wire) => parts.push(wire),
                Err(problem) => problems.push(problem),
            }
        }
        resolved.push(parts.join(","));
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
        return Err(format!("sim name {name:?} contains whitespace; pass each sim name as its own argument"));
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
        many => Err(format!("sim name `{name}` is ambiguous; name one of: {}", many.join(", "))),
    }
}

/// Test-only barrier between compiling the sim build and spawning the first
/// child. `ROCK_SIM_SPAWN_BARRIER=<path>` makes an invocation park here until
/// `<path>` exists, so a test can deterministically interleave a second
/// invocation's compile against this one's spawn. Bounded so a mistaken set
/// cannot wedge a run forever. Env-gated and once per invocation, never in a
/// per-frame or per-sim path.
/// A sim invocation's private directory for `main.lua` + its `incremental.lua`
/// overlay.
///
/// This MUST NOT be a fixed workspace path. Two `rock sim` invocations (or a
/// `rock stone` watcher alongside one) each compile their own `@sim` registry,
/// and a shared path lets the second compile replace the chunk the first is
/// about to run -- the first then reports `unknown sim: <name>` for names its
/// own registry contains. Pinned by
/// `tests/sim_concurrent_invocations.rs`.
///
/// Lives under `tmp/` rather than the system temp dir so a failing run's build
/// is still findable next to `tmp/libstone.so` for `luajit main.lua` debugging.
/// In a source workspace the engine cdylibs stay in `tmp/` itself: they are
/// staged before any spawn and children resolve them through the relative
/// `tmp` entry that `rock_host::project::lib_path_value` puts on the library
/// search path. An SDK resolves them from its own `lib/` instead.
fn sim_output_dir() -> Result<TempDir> {
    fs_err::create_dir_all("tmp").context("creating tmp/ for sim output")?;
    let mut dir = tempfile::Builder::new()
        .prefix("sim-")
        .tempdir_in("tmp")
        .context("creating sim output directory")?;
    // A JIT abort census names `main.lua:<line>`, which is unreadable once the
    // build that produced it is gone. Retaining it is opt-in because every
    // ordinary sim run would otherwise leave a 20MB directory behind.
    if std::env::var_os("ROCK_SIM_KEEP_BUILD").is_some() {
        dir.disable_cleanup(true);
        ::log::info!(target: "SIM", "retaining sim build at {}", dir.path().display());
    }
    Ok(dir)
}

/// Optional parent-side preparation timing for the serial worker/differential
/// lane. The value is a JSONL path; each phase is one self-contained record so
/// an interrupted nextest process still leaves the completed phases behind.
/// This is deliberately separate from the child lifecycle log: it measures
/// work owned by `rock`, not the fresh game process, and is silent by default.
fn record_sim_compile_phase(
    operation: &str,
    phase: &str,
    project: &Path,
    output: Option<&Path>,
    started: std::time::Instant,
    success: bool,
) {
    let Some(path) = std::env::var_os("ROCK_SIM_COMPILE_TIMING")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
    else {
        return;
    };
    let record = serde_json::json!({
        "schema_version": 1,
        "event": "sim_compile_timing",
        "operation": operation,
        "phase": phase,
        "project": project.to_string_lossy(),
        "output": output.map(|path| path.to_string_lossy().into_owned()),
        "duration_ms": started.elapsed().as_secs_f64() * 1000.0,
        "success": success,
        "pid": std::process::id(),
    });
    let mut bytes = match serde_json::to_vec(&record) {
        Ok(bytes) => bytes,
        Err(error) => {
            log::warn!(target: "SIM", "cannot serialize compile timing: {error}");
            return;
        }
    };
    bytes.push(b'\n');
    if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        if let Err(error) = std::fs::create_dir_all(parent) {
            log::warn!(target: "SIM", "cannot create compile timing directory {}: {error}", parent.display());
            return;
        }
    }
    let result = append_sim_trace(&path, &bytes);
    if let Err(error) = result {
        log::warn!(target: "SIM", "cannot write compile timing {}: {error}", path.display());
    }
}

fn append_sim_trace(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;

        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let write_result = std::io::Write::write_all(&mut file, bytes);
        let unlock_result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
        if let Err(error) = write_result {
            return Err(error);
        }
        if unlock_result != 0 {
            return Err(std::io::Error::last_os_error());
        }
        return Ok(());
    }
    #[cfg(not(unix))]
    std::io::Write::write_all(&mut file, bytes)
}

fn timed_sim_compile_phase<T>(
    operation: &str,
    phase: &str,
    project: &Path,
    output: Option<&Path>,
    action: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let started = std::time::Instant::now();
    let result = action();
    record_sim_compile_phase(
        operation,
        phase,
        project,
        output,
        started,
        result.is_ok(),
    );
    result
}

fn timed_sim_output_dir(operation: &str, project: &Path) -> Result<TempDir> {
    let started = std::time::Instant::now();
    let result = sim_output_dir();
    let output = result.as_ref().ok().map(|dir| dir.path().to_path_buf());
    record_sim_compile_phase(
        operation,
        "output_dir",
        project,
        output.as_deref(),
        started,
        result.is_ok(),
    );
    result
}

fn sim_spawn_barrier() {
    let Some(gate) = std::env::var_os("ROCK_SIM_SPAWN_BARRIER") else {
        return;
    };
    let gate = PathBuf::from(gate);
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    while !gate.exists() {
        if std::time::Instant::now() > deadline {
            log::warn!(target: "SIM",
                "ROCK_SIM_SPAWN_BARRIER `{}` never appeared; proceeding", gate.display());
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Parent-side child lifecycle trace, enabled by `ROCK_SIM_LIFECYCLE_LOG`.
///
/// Two granularities, because they answer different questions. `start`/`end`
/// bracket an ENTRY and are written for every part a child was handed, at spawn
/// and at exit -- which is what the exclusive-lane property is about ("was any
/// other entry in flight"). `proc_start`/`proc_end` bracket the PROCESS, keyed
/// by its comma-joined label. A worker is handed a whole queue at once, so its
/// entries all read live from spawn to exit; only the process events can say how
/// many booted workers were alive at the same moment, which is the B5 property.
fn record_sim_lifecycle(event: &str, sim_name: &str) {
    let Some(path) = std::env::var_os("ROCK_SIM_LIFECYCLE_LOG") else {
        return;
    };
    static LOCK: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
    let Ok(_guard) = LOCK.get_or_init(|| Mutex::new(())).lock() else {
        return;
    };
    let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let _ = writeln!(file, "{event}\t{sim_name}");
}

/// Compile `project` into `output_path` and walk the `@sim` registry
/// once, returning registry-ordered wire names, source paths and each sim's
/// `@sim_env` pairs. Single source of suite discovery for the list, single-run,
/// and run-all paths so they can't drift in which sims they see. Does not
/// validate the project dir or build engine libs -- callers own that (listing
/// needs neither).
fn compile_sims(
    backend: crate::Backend,
    project_path: PathBuf,
    output_path: PathBuf,
) -> Result<(
    Vec<String>,
    HashMap<String, Vec<(String, String)>>,
    HashMap<String, String>,
    crate::skull_backend::SkullStone,
)> {
    compile_sims_with_allocation(backend, project_path, output_path, None, &[])
}

const SIM_COMPILE_RECEIPT_DIR_ENV: &str = "ROCK_SIM_COMPILE_RECEIPT_DIR";
const SIM_COMPILE_RECEIPT_PREPARE_ENV: &str = "ROCK_SIM_COMPILE_RECEIPT_PREPARE";
const SIM_COMPILE_RECEIPT_VERSION: u64 = 4;
const SIM_COMPILE_SELECTION_ENV: &str = "ROCK_SIM_COMPILE_SELECTION";

fn normalized_sim_selection(specs: &[String]) -> Vec<String> {
    let mut names = specs.iter().flat_map(|spec| parse_sim_chain(spec).into_iter().map(str::to_owned))
        .collect::<Vec<_>>();
    names.sort();
    names.dedup();
    names
}

fn child_sim_selection() -> Result<Vec<String>> {
    match std::env::var(SIM_COMPILE_SELECTION_ENV) {
        Ok(value) => serde_json::from_str::<Vec<String>>(&value)
            .map(|names| normalized_sim_selection(&names)).context("parse sim compile selection"),
        Err(std::env::VarError::NotPresent) => Ok(Vec::new()),
        Err(error) => Err(error).context("read sim compile selection"),
    }
}

#[path = "sim_compile_cache.rs"]
mod sim_compile_cache;

#[derive(Clone, Debug, PartialEq, Eq)]
struct SimReceiptInput {
    kind: String,
    path: String,
    sha256: String,
}

struct PreparedSims {
    wire_names: Vec<String>,
    env_by_wire: HashMap<String, Vec<(String, String)>>,
    source_files: HashMap<String, String>,
    stone: Option<crate::skull_backend::SkullStone>,
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

fn receipt_source_inputs_from_pairs(
    project: &Path,
    sources: Vec<(String, String)>,
) -> Result<Vec<SimReceiptInput>> {
    let mut inputs = Vec::new();
    let mut manifests = std::collections::BTreeSet::new();
    if let Some(path) = nearest_manifest(project) {
        manifests.insert(path);
    }
    for (path, contents) in sources {
        let path = PathBuf::from(path);
        let canonical = canonical_string(&path)?;
        inputs.push(SimReceiptInput {
            kind: "source".to_string(),
            path: canonical,
            sha256: sha256_bytes(contents.as_bytes()),
        });
        if let Some(path) = nearest_manifest(&path) {
            manifests.insert(path);
        }
    }
    for path in manifests {
        let canonical = canonical_string(&path)?;
        let bytes = std::fs::read(&path)
            .with_context(|| format!("read sim receipt manifest input {}", path.display()))?;
        inputs.push(SimReceiptInput {
            kind: "manifest".to_string(),
            path: canonical,
            sha256: sha256_bytes(&bytes),
        });
    }
    inputs.sort_by(|a, b| (&a.kind, &a.path).cmp(&(&b.kind, &b.path)));
    Ok(inputs)
}

fn receipt_inputs_from_stone(
    project: &Path,
    stone: &crate::skull_backend::SkullStone,
) -> Result<Vec<SimReceiptInput>> {
    let (sources, comptime_files) = stone.sim_receipt_inputs();
    let mut inputs = receipt_source_inputs_from_pairs(project, sources)?;
    for path in comptime_files {
        let canonical = canonical_string(&path)?;
        let bytes = std::fs::read(&path)
            .with_context(|| format!("read sim receipt embedded input {}", path.display()))?;
        inputs.push(SimReceiptInput {
            kind: "embedded".to_string(),
            path: canonical,
            sha256: sha256_bytes(&bytes),
        });
    }
    inputs.sort_by(|a, b| (&a.kind, &a.path).cmp(&(&b.kind, &b.path)));
    Ok(inputs)
}

fn current_receipt_source_inputs(project: &Path) -> Result<Vec<SimReceiptInput>> {
    let spec = crate::unified_compile::SourceSpec::Directory(project.to_path_buf());
    let sources = crate::unified_compile::discover_sources(&spec)?;
    let pairs = sources
        .files
        .iter()
        .map(|source| {
            let source = source.borrow();
            (source.file.to_id_path(), source.contents.clone())
        })
        .collect();
    receipt_source_inputs_from_pairs(project, pairs)
}

/// The receipt context and discovered inputs of a project as it is now: what
/// a receipt must match before a run may load it.
#[derive(Clone)]
struct CurrentReceiptState {
    context: serde_json::Value,
    inputs: Vec<SimReceiptInput>,
}

impl CurrentReceiptState {
    fn of(project: &Path) -> Result<Self> {
        Ok(Self { context: sim_receipt_context(project)?, inputs: current_receipt_source_inputs(project)? })
    }
}

fn sim_receipt_context(project: &Path) -> Result<serde_json::Value> {
    const ENV_KEYS: &[&str] = &[
        "ROCK_ASSET_OVERLAYS",
        "ROCK_CONTEXT_ROOT",
        "ROCK_SOURCE_REVISION",
        "ROCK_STDLIB_DIR",
        "ROCK_SKULL_CALLEE_BINDING",
        "ROCK_SKULL_INDEX_SHAPE",
        "ROCK_SKULL_LOOP_SHAPE",
        "ROCK_SKULL_NIL_KILL",
        "ROCK_SKULL_STATE_CELL",
        "ROCK_SKULL_STATE_RESERVE",
        "ROCK_SKULL_TAIL_CALLS",
        "ROCK_SKULL_VALUE_CTOR",
        "ROCK_SKULL_WRAPPER_RET",
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
        "renderer": active_sim_renderer(project),
        "callee_binding": match skull::callee_binding_mode() {
            skull::CalleeBindingMode::Global => "global",
            skull::CalleeBindingMode::Upvalue => "upvalue",
        },
        "environment": env,
    }))
}

fn receipt_inputs_json(inputs: &[SimReceiptInput]) -> serde_json::Value {
    serde_json::Value::Array(
        inputs
            .iter()
            .map(|input| {
                serde_json::json!({
                    "kind": input.kind,
                    "path": input.path,
                    "sha256": input.sha256,
                })
            })
            .collect(),
    )
}

fn parse_receipt_inputs(value: &serde_json::Value) -> Result<Vec<SimReceiptInput>> {
    let rows = value
        .as_array()
        .ok_or_else(|| anyhow!("sim compile receipt `inputs` is not an array"))?;
    rows.iter()
        .map(|row| {
            let field = |name| {
                row.get(name)
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
                    .ok_or_else(|| anyhow!("sim compile receipt input `{name}` is not a string"))
            };
            Ok(SimReceiptInput {
                kind: field("kind")?,
                path: field("path")?,
                sha256: field("sha256")?,
            })
        })
        .collect()
}

fn registry_json(
    wire_names: &[String],
    env_by_wire: &HashMap<String, Vec<(String, String)>>,
    source_files: &HashMap<String, String>,
) -> Result<serde_json::Value> {
    let rows = wire_names
        .iter()
        .map(|name| {
            let source_file = source_files
                .get(name)
                .filter(|source_file| !source_file.is_empty())
                .ok_or_else(|| anyhow!("compiled @sim `{name}` has no recorded source file"))?;
            let env = env_by_wire.get(name).map_or(&[][..], Vec::as_slice);
            Ok(serde_json::json!({
                "wire_name": name,
                "env": env.iter().map(|(key, value)| serde_json::json!([key, value])).collect::<Vec<_>>(),
                "source_file": source_file,
            }))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(serde_json::Value::Array(rows))
}

fn parse_registry(
    value: &serde_json::Value,
) -> Result<(Vec<String>, HashMap<String, Vec<(String, String)>>, HashMap<String, String>)> {
    let rows = value
        .as_array()
        .ok_or_else(|| anyhow!("sim compile receipt `registry` is not an array"))?;
    let mut names = Vec::with_capacity(rows.len());
    let mut env_by_wire = HashMap::new();
    let mut source_files = HashMap::new();
    for row in rows {
        let name = row
            .get("wire_name")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow!("sim compile receipt registry wire name is not a string"))?
            .to_owned();
        let env_rows = row
            .get("env")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| anyhow!("sim compile receipt registry env for `{name}` is not an array"))?;
        let mut env = Vec::with_capacity(env_rows.len());
        for pair in env_rows {
            let pair = pair.as_array().filter(|pair| pair.len() == 2).ok_or_else(|| {
                anyhow!("sim compile receipt registry env pair for `{name}` is malformed")
            })?;
            let key = pair[0].as_str().ok_or_else(|| {
                anyhow!("sim compile receipt registry env key for `{name}` is not a string")
            })?;
            let value = pair[1].as_str().ok_or_else(|| {
                anyhow!("sim compile receipt registry env value for `{name}` is not a string")
            })?;
            env.push((key.to_owned(), value.to_owned()));
        }
        names.push(name.clone());
        env_by_wire.insert(name.clone(), env);
        let source_file = row
            .get("source_file")
            .and_then(serde_json::Value::as_str)
            .filter(|source_file| !source_file.is_empty())
            .ok_or_else(|| anyhow!("sim compile receipt source file for `{name}` is missing or invalid"))?;
        source_files.insert(name, source_file.to_owned());
    }
    Ok((names, env_by_wire, source_files))
}

fn publish_sim_receipt(
    root: &Path,
    receipt_dir: &Path,
    project: &Path,
    output: &Path,
    wire_names: &[String],
    env_by_wire: &HashMap<String, Vec<(String, String)>>,
    source_files: &HashMap<String, String>,
    inputs: &[SimReceiptInput],
    cache_identity: &serde_json::Value,
    selection: &[String],
) -> Result<()> {
    std::fs::create_dir_all(root)
        .with_context(|| format!("create sim compile receipt root {}", root.display()))?;
    let staged = tempfile::Builder::new()
        .prefix(".sim-receipt-")
        .tempdir_in(root)
        .with_context(|| format!("stage sim compile receipt under {}", root.display()))?;
    let main_source = output.join(rock_host::project::MAIN_LUA_FILENAME);
    let main_bytes = std::fs::read(&main_source)
        .with_context(|| format!("read compiled sim artifact {}", main_source.display()))?;
    std::fs::write(staged.path().join(rock_host::project::MAIN_LUA_FILENAME), &main_bytes)
        .context("write staged sim compile artifact")?;
    let manifest = serde_json::json!({
        "schema_version": SIM_COMPILE_RECEIPT_VERSION,
        "context": sim_receipt_context(project)?,
        "cache_identity": cache_identity,
        "selection": selection,
        "main_sha256": sha256_bytes(&main_bytes),
        "inputs": receipt_inputs_json(inputs),
        "registry": registry_json(wire_names, env_by_wire, source_files)?,
    });
    let mut manifest_bytes = serde_json::to_vec_pretty(&manifest)
        .context("serialize sim compile receipt")?;
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

fn load_sim_receipt(
    receipt_dir: &Path,
    project: &Path,
    output: &Path,
    current: Option<&CurrentReceiptState>,
) -> Result<(Vec<String>, HashMap<String, Vec<(String, String)>>, HashMap<String, String>)> {
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
    let computed;
    let current = match current {
        Some(current) => current,
        None => {
            computed = CurrentReceiptState::of(project)?;
            &computed
        }
    };
    if expected_context != &current.context {
        return Err(anyhow!(
            "sim compile receipt context changed; prepare a new receipt for this test lane"
        ));
    }

    let expected_inputs = parse_receipt_inputs(
        manifest
            .get("inputs")
            .ok_or_else(|| anyhow!("sim compile receipt has no inputs"))?,
    )?;
    let mut current_inputs = current.inputs.clone();
    for expected in expected_inputs.iter().filter(|input| input.kind == "embedded") {
        let bytes = std::fs::read(&expected.path).with_context(|| {
            format!("read sim compile receipt embedded input {}", expected.path)
        })?;
        current_inputs.push(SimReceiptInput {
            kind: expected.kind.clone(),
            path: expected.path.clone(),
            sha256: sha256_bytes(&bytes),
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
    // Copy, then hash the copy that will run: mag's chunk is about 160 MB, and
    // reading it into the runner would keep that much resident beside its sims.
    let materialized = output.join(rock_host::project::MAIN_LUA_FILENAME);
    std::fs::copy(&main_path, &materialized).with_context(|| {
        format!(
            "materialize sim compile receipt {} into {}",
            main_path.display(),
            output.display()
        )
    })?;
    let mut hasher = Sha256::new();
    std::io::copy(
        &mut std::fs::File::open(&materialized)
            .with_context(|| format!("open materialized sim artifact {}", materialized.display()))?,
        &mut hasher,
    )
    .with_context(|| format!("hash materialized sim artifact {}", materialized.display()))?;
    if format!("{:x}", hasher.finalize()) != expected_main_hash {
        return Err(anyhow!("sim compile receipt main.lua hash does not match its manifest"));
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

fn prepare_sims(
    backend: crate::Backend,
    project_path: PathBuf,
    output_path: PathBuf,
) -> Result<PreparedSims> {
    let selection = child_sim_selection()?;
    let Some(root) = std::env::var_os(SIM_COMPILE_RECEIPT_DIR_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
    else {
        let (wire_names, env_by_wire, source_files, stone) =
            compile_sims_with_allocation(backend, project_path, output_path, None, &selection)?;
        return Ok(PreparedSims { wire_names, env_by_wire, source_files, stone: Some(stone) });
    };
    let project_path = project_path.canonicalize().with_context(|| {
        format!("canonicalize sim receipt project {}", project_path.display())
    })?;
    let receipt_dir = sim_receipt_project_dir(&root, &project_path);
    if receipt_dir.is_dir() {
        return consume_sim_receipt(&receipt_dir, &project_path, &output_path, None);
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
    let (wire_names, env_by_wire, source_files, stone) =
        compile_sims_with_allocation(backend, project_path.clone(), output_path.clone(), None, &selection)?;
    let inputs = receipt_inputs_from_stone(&project_path, &stone)?;
    let cache_identity =
        sim_compile_cache::identity(&project_path, backend, stone.comptime_inputs(), &crate::skull_backend::current_rock_exe()?)?;
    let receipt_selection = if stone.sim_selection_is_full() { Vec::new() } else { selection };
    drop(stone);
    timed_sim_compile_phase(
        "compile_sims",
        "receipt_publish",
        &project_path,
        Some(&output_path),
        || {
            publish_sim_receipt(
                &root,
                &receipt_dir,
                &project_path,
                &output_path,
                &wire_names,
                &env_by_wire,
                &source_files,
                &inputs,
                &cache_identity,
                &receipt_selection,
            )
        },
    )?;
    Ok(PreparedSims { wire_names, env_by_wire, source_files, stone: None })
}

fn consume_sim_receipt(
    receipt_dir: &Path,
    project_path: &Path,
    output_path: &Path,
    current: Option<&CurrentReceiptState>,
) -> Result<PreparedSims> {
    timed_sim_compile_phase(
        "compile_sims",
        "sound_bank",
        project_path,
        Some(output_path),
        || crate::package::prepare_sound_bank(&project_path.to_string_lossy()).map(|_| ()),
    )?;
    let (wire_names, env_by_wire, source_files) = timed_sim_compile_phase(
        "compile_sims",
        "receipt_materialize",
        project_path,
        Some(output_path),
        || load_sim_receipt(receipt_dir, project_path, output_path, current),
    )?;
    Ok(PreparedSims { wire_names, env_by_wire, source_files, stone: None })
}

/// `prepare_sims` for a `rock sim` run: the compile happens in a child `rock`
/// that publishes a receipt, so the runner never holds the compiler's memory
/// while sims run. Compiling mag with every sim peaks at about 4.4 GB, which an
/// in-process compile kept resident beside every sim child; a child's exit
/// returns all of it, whatever the allocator does with freed pages. A reload
/// entry still compiles its live session in-process, as it does under any
/// receipt.
///
/// The receipt goes through the cross-run compile cache (`sim_compile_cache`)
/// unless `ROCK_SIM_COMPILE_CACHE=0`, which compiles into a one-run receipt.
/// An explicit receipt lane (`ROCK_SIM_COMPILE_RECEIPT_DIR`) keeps its own
/// rules. The compile child is `rock_exe`: the CLI passes itself, a test
/// harness the `rock` binary it was built beside.
fn prepare_sims_out_of_process(
    backend: crate::Backend,
    project_path: PathBuf,
    output_path: PathBuf,
    rock_exe: &Path,
    selection: &[String],
) -> Result<PreparedSims> {
    crate::compile_slots::report_spawn_phase("compile-prepare");
    if std::env::var_os(SIM_COMPILE_RECEIPT_DIR_ENV).is_some_and(|value| !value.is_empty()) {
        return prepare_sims(backend, project_path, output_path);
    }
    let project_path = project_path.canonicalize().with_context(|| {
        format!("canonicalize sim project {}", project_path.display())
    })?;
    if let Some(cache) = sim_compile_cache::root(rock_exe)? {
        return sim_compile_cache::prepare(backend, &project_path, &output_path, &cache, rock_exe, selection);
    }
    fs_err::create_dir_all("tmp").context("creating tmp/ for the sim compile receipt")?;
    let root = tempfile::Builder::new()
        .prefix("sim-receipt-")
        .tempdir_in("tmp")
        .context("creating sim compile receipt directory")?;
    compile_receipt_in_child(backend, &project_path, rock_exe, root.path(), selection)?;
    consume_sim_receipt(
        &sim_receipt_project_dir(root.path(), &project_path),
        &project_path,
        &output_path,
        None,
    )
}

/// Compile `project` in a child `rock_exe sim --list` that publishes its receipt
/// under `receipt_root`, with this invocation's flags that change what the
/// compile builds or reports.
fn compile_receipt_in_child(
    backend: crate::Backend,
    project_path: &Path,
    rock_exe: &Path,
    receipt_root: &Path,
    selection: &[String],
) -> Result<()> {
    crate::compile_slots::report_spawn_phase("compile");
    let status = std::process::Command::new(rock_exe)
        .args(compile_child_invocation_args())
        .arg("sim")
        .arg("--list")
        .arg("--backend")
        .arg(backend.name())
        .arg(project_path)
        .env(SIM_COMPILE_RECEIPT_DIR_ENV, receipt_root)
        .env(SIM_COMPILE_RECEIPT_PREPARE_ENV, "1")
        .env(SIM_COMPILE_SELECTION_ENV, serde_json::to_string(&normalized_sim_selection(selection))?)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status()
        .with_context(|| format!("spawning the sim compile `{} sim --list`", rock_exe.display()))?;
    if !status.success() {
        return Err(anyhow!(
            "cannot compile project {}: the sim compile process exited with {status}",
            project_path.display()
        ));
    }
    Ok(())
}

/// This invocation's flags that change what the sim compile builds or reports:
/// the `[build.vars]` selection, the renderer and the build lint policy. The
/// compile child must see them, or it compiles something the run did not ask for.
fn compile_child_invocation_args() -> Vec<String> {
    let mut args = crate::comptime::invocation_selection_args();
    if let Some(renderer) = rock_host::stick::renderer_override() {
        let name = match renderer {
            rock_host::stick::RendererBackend::Gl => "gl",
            rock_host::stick::RendererBackend::Wgpu => "wgpu",
        };
        args.extend(["--renderer".to_string(), name.to_string()]);
    }
    match crate::lint_pass::policy() {
        crate::lint_pass::LintReport::Full => {}
        crate::lint_pass::LintReport::Summary => args.push("--lint-summary".to_string()),
        crate::lint_pass::LintReport::Off => args.push("--no-lints".to_string()),
    }
    args
}

fn compile_sims_with_allocation(
    backend: crate::Backend,
    project_path: PathBuf,
    output_path: PathBuf,
    allocation_manifest: Option<&Path>,
    selection: &[String],
) -> Result<(
    Vec<String>,
    HashMap<String, Vec<(String, String)>>,
    HashMap<String, String>,
    crate::skull_backend::SkullStone,
)> {
    crate::compile_slots::report_spawn_phase("compile");
    // Only skull serves sims now (bone has no codegen, rock is gone): build the
    // bone workspace once (a live Session, tests included), emit the game-shaped
    // main.lua with its `@sim` registry, and collect the wire/env list from the
    // same build. Source paths are captured from the same HIR declarations, and
    // the returned SkullStone serves mid-sim `reload_edit` ops.
    timed_sim_compile_phase(
        "compile_sims",
        "sound_bank",
        &project_path,
        Some(&output_path),
        || crate::package::prepare_sound_bank(&project_path.to_string_lossy()).map(|_| ()),
    )?;
    match backend {
        crate::Backend::Skull => {
            let main_path = output_path.join(rock_host::project::MAIN_LUA_FILENAME);
            timed_sim_compile_phase(
                "compile_sims",
                "skull_compile_emit",
                &project_path,
                Some(&output_path),
                || {
                    crate::skull_backend::skull_compile_game_and_collect_sims_with_allocation(
                        &project_path.to_string_lossy(),
                        &main_path,
                        allocation_manifest,
                        selection,
                    )
                },
            )
        }
        other => panic!(
            "compile_sims reached non-skull backend {:?}; only skull serves sims",
            other.name()
        ),
    }
}

/// The reload lane owns the live compile session and an isolated copy of the
/// emitted chunk, so its edits cannot change what ordinary workers boot.
pub struct ReloadLaneSession {
    stone: crate::skull_backend::SkullStone,
    output: String,
    _dir: TempDir,
}

fn reload_lane_session(
    backend: crate::Backend,
    project_path: &Path,
    lane_is_selected: bool,
    shared_output: &Path,
    prepared_stone: Option<crate::skull_backend::SkullStone>,
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
        // A receipt contains emitted code, but no editable compiler Session.
        None => compile_sims(backend, project_path.to_path_buf(), dir.path().to_path_buf())?.3,
    };
    if !quiet {
        println!("[sim] hot-reload lane: isolated chunk with the shared callee shape");
    }
    Ok(Some(ReloadLaneSession { stone, output, _dir: dir }))
}

/// Whether any selected entry opted into the hot-reload lane.
fn any_reload_lane_selected(
    specs: &[String],
    env_by_wire: &HashMap<String, Vec<(String, String)>>,
) -> bool {
    specs.iter().any(|spec| entry_is_reload_lane(spec, env_by_wire))
}

/// Compile the project and print each `@sim` wire name on stdout, one
/// per line, sorted by source span. Lets scripts enumerate the suite without
/// hardcoding names.
pub fn do_list_sims(backend: crate::Backend, project: &str) -> Result<()> {
    rock_host::project::ensure_workspace_root()?;

    let project_path = rock_host::project::resolve_project_dir(project)?;

    let out_dir = sim_output_dir()?;
    let prepared = prepare_sims(backend, project_path, out_dir.path().to_path_buf())?;
    for name in prepared.wire_names {
        // Result output, NOT logging: `--list` exists to print the sim
        // names to stdout — this is the command's answer, consumed by
        // callers/CI, not a diagnostic.
        println!("{}", name);
    }
    Ok(())
}

/// Compile/discover and report the ordinary worker plan without building engines
/// or spawning children. Explicit names retain their occurrence semantics.
pub fn do_plan_sims(
    backend: crate::Backend,
    project: &str,
    names: &[String],
    jobs: Option<usize>,
    jobs_force: bool,
) -> Result<()> {
    rock_host::project::ensure_workspace_root()?;
    let project_path = rock_host::project::resolve_project_dir(project)?;
    let out_dir = sim_output_dir()?;
    let prepared = prepare_sims(backend, project_path.clone(), out_dir.path().to_path_buf())?;
    let mut manual_excluded = 0;
    let specs = if names.is_empty() {
        let mut routine = Vec::new();
        for name in &prepared.wire_names {
            if manual_sim(prepared.env_by_wire.get(name).map_or(&[][..], |v| v))? {
                manual_excluded += 1;
            } else {
                routine.push(name.clone());
            }
        }
        routine
    } else {
        resolve_sim_specs(&prepared.wire_names, names)?
    };
    let mut plan = worker_lane_plan(
        &specs, &prepared.env_by_wire, active_sim_renderer(&project_path),
        resolve_jobs(jobs, None, false, jobs_force)?,
    );
    plan["manual_excluded"] = serde_json::json!(manual_excluded);
    println!("{}", serde_json::to_string_pretty(&plan)?);
    Ok(())
}

pub fn do_run_sim_opts(backend: crate::Backend, project: &str, sim_name: &str, quiet: bool, update_snapshots: bool) -> Result<()> {
    do_run_sim_opts_with_request(backend, project, sim_name, quiet, update_snapshots, Some(1), "single")
}

fn do_run_sim_opts_with_request(
    backend: crate::Backend,
    project: &str,
    sim_name: &str,
    quiet: bool,
    update_snapshots: bool,
    requested_jobs: Option<usize>,
    requested_mode: &str,
) -> Result<()> {
    rock_host::project::ensure_workspace_root()?;
    let recorder = SimRunRecorder::begin(
        project,
        sim_name,
        quiet,
        requested_jobs,
        requested_mode,
    );
    let result = do_run_sim_opts_recorded(
        backend,
        project,
        sim_name,
        quiet,
        update_snapshots,
        requested_jobs,
        requested_mode,
        &recorder,
    );
    let error = result.as_ref().err().map(ToString::to_string);
    recorder.finish(result.is_ok(), error.as_deref());
    result
}

fn do_run_sim_opts_recorded(
    backend: crate::Backend,
    project: &str,
    sim_name: &str,
    quiet: bool,
    update_snapshots: bool,
    requested_jobs: Option<usize>,
    requested_mode: &str,
    recorder: &SimRunRecorder,
) -> Result<()> {

    let project_path = recorder.phase("resolve_project", || {
        rock_host::project::resolve_project_dir(project)
    })?;
    // Thread the RESOLVED path everywhere the raw arg used to flow (engine libs,
    // sim-source lookup for snapshot baselines) so a bare name behaves exactly
    // like its expanded path.
    let project = project_path.to_string_lossy().into_owned();
    recorder.phase("build_engine", || {
        rock_host::engine_build::build_engine_libs(&project)
    })?;

    let out_dir = sim_output_dir()?;
    let output = out_dir.path().to_string_lossy().into_owned();
    let output_path = out_dir.path().to_path_buf();

    // Compile once and collect `@sim_env("K", "V")` env pairs by wire name.
    // The runner applies them via `Command::env` before luajit spawns --
    // the in-process runtime `set_env()` runs after `@init`, so it can't
    // influence env-var-driven game config like `MATCH3_PRESET`.
    let mut prepared = recorder.phase("compile_and_discover", || {
        prepare_sims_out_of_process(backend, project_path.clone(), output_path.clone(), &crate::skull_backend::current_rock_exe()?, &[sim_name.to_string()])
    })?;
    let sim_name = resolve_sim_specs(&prepared.wire_names, &[sim_name.to_string()])?.remove(0);
    let sim_name = sim_name.as_str();
    // Receipt consumers do not retain the compiler session. A reload entry is
    // the one exception: it edits that live session, so preserve the existing
    // fresh-compile behavior for that explicitly selected lane.
    if prepared.stone.is_none()
        && entry_is_reload_lane(sim_name, &prepared.env_by_wire)
    {
        let (wire_names, env_by_wire, source_files, stone) = recorder.phase("compile_reload_session", || {
            compile_sims(backend, project_path.clone(), output_path)
        })?;
        prepared = PreparedSims {
            wire_names,
            env_by_wire,
            source_files,
            stone: Some(stone),
        };
    }
    let entry_ids = recorder.plan_entries(&[sim_name.to_string()]);
    recorder.configure(
        &project_path,
        &prepared.wire_names,
        requested_jobs,
        1,
        None,
        1,
        requested_mode,
        "single",
        update_snapshots,
        &prepared.source_files,
    );
    emit_bytecode_image(Path::new(&output));
    sim_spawn_barrier();

    let (display_name, summary) = run_named_entry(
        &project,
        sim_name,
        quiet,
        false,
        update_snapshots,
        &output,
        &prepared.env_by_wire,
        prepared.stone.as_mut(),
        None,
        Some(&recorder),
        entry_ids.first().copied(),
    )?;
    let sim_name = display_name.as_str();

    if !quiet {
        summary.print(sim_name, true);
    }

    sim_outcome(&summary, sim_name)
}

/// The single-run result: the same predicate as [`entry_passed`], with the
/// reason as the error.
fn sim_outcome(summary: &SimSummary, sim_name: &str) -> Result<()> {
    if summary.excluded.is_some() {
        return Ok(());
    }

    if summary.timed_out {
        return Err(anyhow!(
            "sim `{sim_name}` timed out after {}s (wall clock, not its frame budget); \
             override with ROCK_SIM_TIMEOUT_SECS",
            sim_timeout_secs()
        ));
    }
    if let Some(code) = summary.process_failure_code {
        return Err(anyhow!(
            "sim `{sim_name}` process exited with non-zero status: {code:?}"
        ));
    }
    if summary.has_failures() {
        // Caller decides exit code; `rock sim` maps this to a non-zero
        // process exit in `dispatcher.rs`.
        let fails = summary.expects_fail.len();
        let errs = summary.errors.len();
        return Err(anyhow!(
            "sim `{sim_name}` had {fails} expect failure(s), {errs} error(s)"
        ));
    }
    // A sim that never reached stop_sim() ran incompletely -- a coroutine
    // abort / swallowed runtime error mid-body. That is a failure even
    // when no expect explicitly failed; otherwise an aborted sim exits 0
    // and silently "passes" (every real sim ends in stop_sim()).
    if !summary.saw_stop {
        return Err(anyhow!(
            "sim `{sim_name}` aborted before stop_sim() -- incomplete run (likely a swallowed runtime error)"
        ));
    }
    // A fixture that returned before its checks still reaches stop_sim().
    if !summary.checks_accounted() {
        return Err(anyhow!("sim `{sim_name}` reached stop_sim() without running a check"));
    }
    Ok(())
}

/// Run one named entry (possibly a comma-chain) into a fresh tempdir,
/// returning its display name and merged summary. Shared by the single-run
/// path (`do_run_sim_opts`) and the multi-run path (`do_run_named_sims`) so
/// the tempdir / chain / video / cleanup handling can't drift between them.
///
/// The tempdir is exposed to the child as `OUT`; comma-chained parts (`a,b`)
/// share it so a fixture saved by part 1 is readable by part 2. Distinct
/// named entries each get their own via separate calls.
struct SimArtifacts {
    out_dir: TempDir,
    /// Whether to hash `$OUT` into an artifact manifest when the entry
    /// finishes. OFF for ordinary suite runs: the walk reads every produced
    /// byte, which for a game writing videos is exactly the cost this whole
    /// plan exists to remove, and it would happen under the reporting lock.
    /// Only the differential oracle needs the manifest.
    record_manifest: bool,
}

impl SimArtifacts {
    fn new() -> Result<Self> {
        Self::with_manifest(false)
    }

    fn recording() -> Result<Self> {
        Self::with_manifest(true)
    }

    fn with_manifest(record_manifest: bool) -> Result<Self> {
        Ok(Self {
            out_dir: TempDir::new().context("creating sim tempdir")?,
            record_manifest,
        })
    }

    fn path(&self) -> &Path {
        self.out_dir.path()
    }

    fn finish(self, summary: &mut SimSummary) {
        self.finish_with(summary, process_video_assertions);
    }

    /// Deferred artifact work, all of it while `$OUT` is still live: run
    /// `process` (the video assertions, which shell out to ffprobe/ffmpeg
    /// against files under `$OUT`), record the manifest of what the entry
    /// produced, and only then honor the sim's `cleanup_path` removals. The
    /// manifest is taken BEFORE cleanup so it describes what the sim wrote,
    /// not what survived teardown.
    fn finish_with(self, summary: &mut SimSummary, process: impl FnOnce(&mut SimSummary)) {
        process(summary);
        if self.record_manifest {
            // Fail closed: a manifest that quietly dropped a file would let two
            // runs agree about evidence neither of them collected.
            match crate::sim_record::manifest_of_dir(self.out_dir.path()) {
                Ok(manifest) => summary.artifacts = manifest,
                Err(e) => summary.errors.push(SiteMessage {
                    message: format!("artifact manifest for `{}` failed: {e:#}", self.out_dir.path().display()),
                    site: String::new(),
                }),
            }
        }
        for path in std::mem::take(&mut summary.cleanup_paths) {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn run_named_entry(
    project: &str,
    chain_spec: &str,
    quiet: bool,
    buffered: bool,
    update_snapshots: bool,
    output: &str,
    env_by_wire: &HashMap<String, Vec<(String, String)>>,
    stone: Option<&mut crate::skull_backend::SkullStone>,
    census_warmup_frames: Option<usize>,
    recorder: Option<&SimRunRecorder>,
    occurrence_id: Option<u64>,
) -> Result<(String, SimSummary)> {
    let artifacts = SimArtifacts::new()?;
    let out_path = artifacts.path().to_path_buf();

    // Comma-separated names = chained multi-start. Each part spawns a fresh
    // luajit child; expects/snapshots/video-assertions aggregate into one
    // pass/fail. Single-name (no commas) is the common case.
    let parts = parse_sim_chain(chain_spec);
    if parts.is_empty() {
        return Err(anyhow!("sim got an empty sim name"));
    }
    let display_name = if parts.len() == 1 {
        parts[0].to_string()
    } else {
        chain_spec.to_string()
    };

    let active_renderer = active_sim_renderer(Path::new(project));
    for part in &parts {
        let env_pairs = env_by_wire.get(*part).cloned().unwrap_or_default();
        if let Some(req) = sim_required_backend(&env_pairs) {
            if req != active_renderer {
                let reason = format!("requires {req} renderer (active is {active_renderer})");
                let mut s = SimSummary::default();
                s.excluded = Some(reason);
                s.saw_stop = true;
                record_excluded_entry(recorder, occurrence_id);
                return Ok((display_name, s));
            }
        }
    }

    let mut combined = SimSummary::default();
    let mut stone = stone;
    for (part_index, part) in parts.iter().enumerate() {
        let env_pairs = env_by_wire.get(*part).cloned().unwrap_or_default();
        // Parts of a chain run in order and share `$OUT`; only the parts that
        // opted into the reload lane may touch the session.
        let lane = if is_reload_lane(&env_pairs) {
            stone.as_deref_mut()
        } else {
            None
        };
        let part_summary = run_one_sim(
            project,
            part,
            quiet,
            buffered,
            update_snapshots,
            &out_path,
            output,
            &env_pairs,
            lane,
            census_warmup_frames,
            recorder,
            occurrence_id,
            part_index,
        )?;
        combined.merge(part_summary);
    }

    // Video assertions after all parts run (they need ffprobe/ffmpeg
    // subprocesses, and a multi-start chain may assert in either part).
    if let (Some(recorder), Some(occurrence_id)) = (recorder, occurrence_id) {
        recorder.artifact_check_start(occurrence_id);
    }
    artifacts.finish(&mut combined);
    if let (Some(recorder), Some(occurrence_id)) = (recorder, occurrence_id) {
        recorder.artifact_check_end(occurrence_id, !combined.has_failures());
        recorder.entry_result(
            occurrence_id,
            combined.worker_attempt.unwrap_or(0),
            verdict_word(&combined),
            combined
                .frame_count
                .or_else(|| combined.allocation_census.as_ref().map(|c| c.frame_count)),
            combined.expects_pass.len(),
            combined.timed_out,
            combined.process_failure_code,
        );
    }
    Ok((display_name, combined))
}

fn manual_sim(env: &[(String, String)]) -> Result<bool> {
    match env.iter().rev().find(|(key, _)| key == "SIM_MANUAL").map(|(_, value)| value.as_str()) {
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

/// Whether an entry's summary is a clean pass (no fail, no timeout, no
/// process error, reached `stop_sim()`, ran at least one check).
fn entry_passed(summary: &SimSummary) -> bool {
    !entry_excluded(summary)
        && !summary.timed_out
        && summary.process_failure_code.is_none()
        && summary.saw_stop
        && !summary.has_failures()
        && summary.checks_accounted()
}

/// The word printed in a sim's verdict line. Derived from [`entry_passed`] so the
/// human-facing report and the process exit status can never disagree -- this
/// used to be computed from `has_failures()` alone, which printed `PASS` for a
/// sim that aborted mid-body (an abort fails no expect).
fn verdict_word(summary: &SimSummary) -> &'static str {
    if entry_excluded(summary) {
        "EXCLUDED"
    } else if entry_passed(summary) {
        "PASS"
    } else {
        "FAIL"
    }
}

/// Run a specific list of named sims (the `rock sim <project> a b c` form).
/// A single name delegates to `do_run_sim_opts` and streams full per-expect
/// detail. Multiple names run concise: no live log, just each sim's verdict
/// (with its failing assertions) plus a final tally. Compiles once; each
/// named entry gets its own tempdir. Returns Err (non-zero exit) if any
/// named entry failed.
pub fn do_run_named_sims(
    backend: crate::Backend,
    project: &str,
    names: &[String],
    quiet: bool,
    update_snapshots: bool,
    jobs: Option<usize>,
    jobs_force: bool,
    full_asset_jobs: Option<usize>,
    mode: SimExecMode,
) -> Result<()> {
    // One name: identical to the classic single-run path (full detail). There
    // is nothing to overlap, so `--jobs` does not apply -- nor `--workers`,
    // since a one-entry queue takes no checkpoint either way.
    if names.len() == 1 {
        if jobs.is_some_and(|j| j > 1) {
            resolve_jobs(jobs, Some(1), update_snapshots, jobs_force)?;
        }
        return do_run_sim_opts_with_request(
            backend,
            project,
            &names[0],
            quiet,
            update_snapshots,
            jobs,
            sim_exec_mode_name(mode),
        );
    }

    rock_host::project::ensure_workspace_root()?;
    let recorder = SimRunRecorder::begin(
        project,
        &names.join(" "),
        quiet,
        jobs,
        sim_exec_mode_name(mode),
    );
    let result = do_run_named_sims_recorded(
        backend,
        project,
        names,
        quiet,
        update_snapshots,
        jobs,
        jobs_force,
        full_asset_jobs,
        mode,
        &recorder,
    );
    let error = result.as_ref().err().map(ToString::to_string);
    recorder.finish(result.is_ok(), error.as_deref());
    result
}

fn do_run_named_sims_recorded(
    backend: crate::Backend,
    project: &str,
    names: &[String],
    quiet: bool,
    update_snapshots: bool,
    jobs: Option<usize>,
    jobs_force: bool,
    full_asset_jobs: Option<usize>,
    mode: SimExecMode,
    recorder: &SimRunRecorder,
) -> Result<()> {

    let project_path = recorder.phase("resolve_project", || {
        rock_host::project::resolve_project_dir(project)
    })?;
    let project = project_path.to_string_lossy().into_owned();
    recorder.phase("build_engine", || {
        rock_host::engine_build::build_engine_libs(&project)
    })?;

    let out_dir = sim_output_dir()?;
    let output = out_dir.path().to_string_lossy().into_owned();
    let prepared = recorder.phase("compile_and_discover", || {
        prepare_sims_out_of_process(backend, project_path.clone(), PathBuf::from(&output), &crate::skull_backend::current_rock_exe()?, names)
    })?;
    let wire_names = prepared.wire_names;
    let env_by_wire = prepared.env_by_wire;
    let source_files = prepared.source_files;
    let names = &resolve_sim_specs(&wire_names, names)?;
    emit_bytecode_image(Path::new(&output));
    let entry_ids = recorder.plan_entries(names);
    sim_spawn_barrier();
    // Records print whole once every sim is done, so a run of several says
    // where to follow it live.
    if names.len() > 1 && !quiet {
        if let Some(events) = recorder.events_path() {
            println!(
                "[sim] {} sims: records print when all are done; live entry_start/entry_end in {}",
                names.len(),
                events.display()
            );
        }
    }

    let requested_mode = sim_exec_mode_name(mode);
    let mode = resolve_exec_mode(mode, update_snapshots, quiet)?;
    let effective_jobs = resolve_jobs(jobs, Some(names.len()), update_snapshots, jobs_force)?;
    let requested_full_asset_jobs = full_asset_jobs;
    let full_asset_jobs = resolve_full_asset_jobs(full_asset_jobs, effective_jobs);
    recorder.configure(
        &project_path,
        &wire_names,
        jobs,
        effective_jobs,
        requested_full_asset_jobs,
        full_asset_jobs,
        requested_mode,
        sim_exec_mode_name(mode),
        update_snapshots,
        &source_files,
    );
    let mut reload_lane = reload_lane_session(
        backend,
        &project_path,
        any_reload_lane_selected(names, &env_by_wire),
        Path::new(&output),
        prepared.stone,
        quiet,
    )?;
    if mode != SimExecMode::Isolated {
        let results = run_specs_worker_mode(
            &project,
            &output,
            &env_by_wire,
            names,
            quiet,
            update_snapshots,
            jobs,
            jobs_force,
            full_asset_jobs,
            reload_lane.as_mut(),
            Some(recorder),
            Some(&entry_ids),
        )?;
        let mut passed = 0usize;
        let mut excluded = 0usize;
        let mut failed: Vec<String> = Vec::new();
        for (name, summary) in &results {
            if entry_excluded(summary) {
                excluded += 1;
            } else if entry_passed(summary) {
                passed += 1;
            } else {
                failed.push(name.clone());
            }
        }
        let executed = names.len() - excluded;
        let excl_suffix = if excluded > 0 {
            format!(" (selected {}, {excluded} excluded)", names.len())
        } else {
            String::new()
        };
        let suffix = if failed.is_empty() {
            String::new()
        } else {
            format!(" -- FAILED: {}", failed.join(", "))
        };
        println!("[sim] {passed}/{executed} sims passed{excl_suffix}{suffix}");
        if !failed.is_empty() {
            return Err(anyhow!(
                "{} of {} named sims failed in `{project}`",
                failed.len(),
                executed
            ));
        }
        return Ok(());
    }

    let jobs = resolve_jobs(jobs, Some(names.len()), update_snapshots, jobs_force)?;

    // Same two lanes as the suite path. The unit of parallelism is the ENTRY,
    // not the part: a comma-chain's parts share one `$OUT` and run in order,
    // so they stay together on one worker.
    let (reload_idx, ordinary_idx): (Vec<usize>, Vec<usize>) =
        (0..names.len()).partition(|&i| entry_is_reload_lane(&names[i], &env_by_wire));

    let report = Mutex::new(());
    // Only the reload lane owns the Session and its isolated output copy.
    let run_at = |i: usize,
                  lane: Option<(&mut crate::skull_backend::SkullStone, &str)>|
     -> Result<(String, SimSummary)> {
        let (stone, entry_output) = match lane {
            Some((stone, out)) => (Some(stone), out),
            None => (None, output.as_str()),
        };
        // Buffered, not silenced: the per-expect stream is suppressed (it
        // would interleave), but the child's diagnostics are retained and
        // emitted with the record below, and `print` lists each sim's failing
        // assertions so a failure isn't opaque.
        let (display_name, summary) = run_named_entry(
            &project,
            &names[i],
            quiet,
            true,
            update_snapshots,
            entry_output,
            &env_by_wire,
            stone,
            None,
            Some(recorder),
            Some(entry_ids[i]),
        )?;
        let _lock = report.lock().unwrap();
        if !quiet {
            if !summary.diagnostics.is_empty() {
                eprint!("{}", summary.diagnostics);
            }
            summary.print(&display_name, false);
        }
        Ok((display_name, summary))
    };

    let (pool, lane) = beside_reload_lane(
        jobs,
        !reload_idx.is_empty(),
        |width| run_bounded(width, ordinary_idx.len(), |slot| run_at(ordinary_idx[slot], None)),
        || run_reload_lane_serially(reload_lane.as_mut(), &reload_idx, |i, lane| run_at(i, Some(lane))),
    );
    let mut results: HashMap<usize, Result<(String, SimSummary)>> = HashMap::new();
    for (slot, r) in pool {
        results.insert(ordinary_idx[slot], r);
    }
    results.extend(lane);

    // Accounting walks the order the user gave, so the failed list matches a
    // serial run's regardless of completion order.
    let mut passed = 0usize;
    let mut excluded = 0usize;
    let mut failed: Vec<String> = Vec::new();
    for i in 0..names.len() {
        let (display_name, summary) = match results.remove(&i) {
            Some(Ok(v)) => v,
            Some(Err(e)) => return Err(e),
            None => return Err(anyhow!("sim `{}` was never scheduled", names[i])),
        };
        if entry_excluded(&summary) {
            excluded += 1;
        } else if entry_passed(&summary) {
            passed += 1;
        } else {
            failed.push(display_name);
        }
    }

    // Result output, NOT logging: the multi-run verdict, consumed by you/CI.
    let executed = names.len() - excluded;
    let excl_suffix = if excluded > 0 {
        format!(" (selected {}, {excluded} excluded)", names.len())
    } else {
        String::new()
    };
    let suffix = if failed.is_empty() {
        String::new()
    } else {
        format!(" -- FAILED: {}", failed.join(", "))
    };
    println!("[sim] {passed}/{executed} sims passed{excl_suffix}{suffix}");

    if !failed.is_empty() {
        return Err(anyhow!(
            "{} of {} named sims failed in `{project}`",
            failed.len(),
            executed
        ));
    }
    Ok(())
}

/// Run every `@sim` in `project` (the no-name form `rock sim <project>`).
/// Compiles once, then spawns each sim as its own luajit child with a
/// fresh tempdir, prints each sim's pass/fail line, and a final suite
/// tally. Returns Err (non-zero exit) if any sim failed. Distinct sims
/// don't chain, so each gets its own `OUT`; use the comma form for
/// multi-start chains.
/// The one bounded scheduler. `task` is claimed by index off a shared cursor,
/// so a task runs exactly once on at most `jobs` OS threads at a time, and the
/// serial path is the same call with `jobs == 1` rather than a second code path
/// that could drift from it. Results come back keyed by index; the CALLER
/// decides the reporting order, because completion order is not deterministic.
///
/// Both fan-outs go through here: Phase A's fresh-process children
/// ([`do_run_all_sims`]) and Phase B's boot-checkpoint worker lanes
/// ([`run_specs_worker_mode`]).
fn run_bounded<T, F>(width: &PoolWidth, n: usize, task: F) -> Vec<(usize, Result<T>)>
where
    F: Fn(usize) -> Result<T> + Sync,
    T: Send,
{
    if width.jobs <= 1 || n <= 1 {
        return (0..n).map(|i| (i, task(i))).collect();
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    let collected: Mutex<Vec<(usize, Result<T>)>> = Mutex::new(Vec::new());
    let lanes = width.jobs.min(n);
    std::thread::scope(|scope| {
        for _ in 0..lanes {
            scope.spawn(|| loop {
                let _slot = width.enter();
                let i = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if i >= n {
                    break;
                }
                let r = task(i);
                collected.lock().unwrap().push((i, r));
            });
        }
    });
    collected.into_inner().expect("no lane panicked while holding the results lock")
}

/// How many of a pool's `jobs` threads may be inside a task at once. A pool
/// spawns every thread up front; a thread takes a slot before it claims work,
/// so a narrower width holds threads back without claiming anything for them.
struct PoolWidth {
    jobs: usize,
    /// (width, live)
    slots: Mutex<(usize, usize)>,
    changed: std::sync::Condvar,
}

impl PoolWidth {
    fn fixed(jobs: usize) -> Self {
        Self::starting_at(jobs, jobs)
    }

    fn starting_at(jobs: usize, width: usize) -> Self {
        let jobs = jobs.max(1);
        Self { jobs, slots: Mutex::new((width.clamp(1, jobs), 0)), changed: std::sync::Condvar::new() }
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

/// Pool dispatch order: longest recorded duration first, so the lanes that
/// would set the suite's tail start while the pool is still full. The sort is
/// stable, so equal and untimed lanes keep their planned order, after every
/// timed one.
fn longest_first(estimates: &[Option<u64>]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..estimates.len()).collect();
    order.sort_by_key(|&w| std::cmp::Reverse(estimates[w].unwrap_or(0)));
    order
}

/// Where one chain part's scheduling estimate came from, for the dispatch log
/// line: a lane with no estimate anywhere keeps plan order, and the log says
/// how many did, so the seeded fallback is never silent.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum EstimateSource {
    History,
    Seeded,
}

/// One part's scheduling estimate: this machine's own most recent finished
/// duration first, else the checked-in seeded median, else nothing. History
/// always wins because it is fresher than any checked-in prior; a missing or
/// stale seeded entry (the table never saw the sim, or it failed to load)
/// falls back to today's behavior with the `None` the caller counts.
fn entry_schedule_estimate(history_ms: Option<u64>, sim_name: &str) -> (Option<u64>, Option<EstimateSource>) {
    if let Some(ms) = history_ms {
        return (Some(ms), Some(EstimateSource::History));
    }
    if let Some(ms) = crate::sim_observability::seeded_median_ms(sim_name) {
        return (Some(ms), Some(EstimateSource::Seeded));
    }
    (None, None)
}

fn spec_schedule_estimate(spec: &str, recorder: Option<&SimRunRecorder>) -> (Option<u64>, Option<EstimateSource>) {
    let parts: Vec<_> = parse_sim_chain(spec).iter().map(|part| {
        entry_schedule_estimate(recorder.and_then(|recorder| recorder.recorded_duration_ms(part)), part)
    }).collect();
    if parts.iter().any(|(ms, _)| ms.is_none()) {
        return (None, None);
    }
    sum_estimates(parts.into_iter())
}

/// Sum scheduling estimates across a lane's entries (or a chain's parts):
/// the total is present when any estimate is, and the source is history when
/// any contributor is history, else seeded when any contributor is seeded.
fn sum_estimates(
    estimates: impl Iterator<Item = (Option<u64>, Option<EstimateSource>)>,
) -> (Option<u64>, Option<EstimateSource>) {
    let mut total: Option<u64> = None;
    let mut source: Option<EstimateSource> = None;
    for (ms, part_source) in estimates {
        if let Some(ms) = ms {
            total = Some(total.unwrap_or(0).saturating_add(ms));
        }
        if source != Some(EstimateSource::History) {
            source = part_source.or(source);
        }
    }
    (total, source)
}

/// Run the ordinary `pool` and the hot-reload `lane` at the same time.
///
/// The lane boots a chunk of its own from a session of its own
/// (`reload_lane_session`), and no reload fixture writes a file an ordinary
/// entry reads, so an ordinary child cannot observe it. The lane's entries stay
/// serial among themselves because they share that one session. While the lane
/// has work it holds one of the `jobs`; when it finishes, the pool widens to
/// all of them.
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
        let pool = pool.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        (pool, lane)
    })
}

/// Each reload entry at `indices`, one at a time through the lane's session.
fn run_reload_lane_serially<T>(
    lane: Option<&mut ReloadLaneSession>,
    indices: &[usize],
    mut run: impl FnMut(usize, (&mut crate::skull_backend::SkullStone, &str)) -> Result<T>,
) -> Vec<(usize, Result<T>)> {
    let Some(lane) = lane else {
        return indices
            .iter()
            .map(|&i| (i, Err(anyhow!("reload entry {i} was selected without a lane session"))))
            .collect();
    };
    indices
        .iter()
        .map(|&i| (i, run(i, (&mut lane.stone, lane.output.as_str()))))
        .collect()
}

/// `run_bounded` with a second cap: at most `capped_jobs` of the `jobs`
/// threads may be inside a task whose `capped[i]` is true at any moment.
///
/// Capped tasks are handed out in index order and the others fill every slot
/// the cap leaves free, so a full pool stays full whenever there is uncapped
/// work; a thread only waits when the cap is reached AND nothing else
/// remains. With `capped_jobs >= jobs` the schedule is the plain pool's, task
/// order included, so the uncapped run is not a second policy.
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
    let collected: Mutex<Vec<(usize, Result<T>)>> = Mutex::new(Vec::new());
    let lanes = jobs.min(n);
    std::thread::scope(|scope| {
        for _ in 0..lanes {
            scope.spawn(|| loop {
                let _slot = width.enter();
                let picked = {
                    let mut q = queue.lock().unwrap();
                    loop {
                        if q.live_capped < capped_jobs {
                            if let Some(i) = q.capped.pop_front() {
                                q.live_capped += 1;
                                break Some((i, true));
                            }
                        }
                        if let Some(i) = q.other.pop_front() {
                            break Some((i, false));
                        }
                        if q.capped.is_empty() {
                            break None;
                        }
                        // Only capped work is left and the cap is reached:
                        // wait for a capped slot, never spin.
                        q = freed.wait(q).unwrap();
                    }
                };
                let Some((i, is_capped)) = picked else { break };
                let r = task(i);
                collected.lock().unwrap().push((i, r));
                if is_capped {
                    queue.lock().unwrap().live_capped -= 1;
                    freed.notify_all();
                }
            });
        }
    });
    collected.into_inner().expect("no lane panicked while holding the results lock")
}

pub fn do_run_all_sims(
    backend: crate::Backend,
    project: &str,
    quiet: bool,
    update_snapshots: bool,
    jobs: Option<usize>,
    jobs_force: bool,
    full_asset_jobs: Option<usize>,
    mode: SimExecMode,
) -> Result<()> {
    rock_host::project::ensure_workspace_root()?;

    let recorder = SimRunRecorder::begin(
        project,
        "<all>",
        quiet,
        jobs,
        sim_exec_mode_name(mode),
    );
    let result = do_run_all_sims_recorded(
        backend,
        project,
        quiet,
        update_snapshots,
        jobs,
        jobs_force,
        full_asset_jobs,
        mode,
        &recorder,
    );
    let error = result.as_ref().err().map(ToString::to_string);
    recorder.finish(result.is_ok(), error.as_deref());
    result
}

fn do_run_all_sims_recorded(
    backend: crate::Backend,
    project: &str,
    quiet: bool,
    update_snapshots: bool,
    jobs: Option<usize>,
    jobs_force: bool,
    full_asset_jobs: Option<usize>,
    mode: SimExecMode,
    recorder: &SimRunRecorder,
) -> Result<()> {

    let project_path = recorder.phase("resolve_project", || {
        rock_host::project::resolve_project_dir(project)
    })?;
    let project = project_path.to_string_lossy().into_owned();
    recorder.phase("build_engine", || {
        rock_host::engine_build::build_engine_libs(&project)
    })?;

    let out_dir = sim_output_dir()?;
    let output = out_dir.path().to_string_lossy().into_owned();
    let prepared = recorder.phase("compile_and_discover", || {
        prepare_sims_out_of_process(backend, project_path.clone(), PathBuf::from(&output), &crate::skull_backend::current_rock_exe()?, &[])
    })?;
    let wire_names = prepared.wire_names;
    let env_by_wire = prepared.env_by_wire;
    let source_files = prepared.source_files;
    emit_bytecode_image(Path::new(&output));
    sim_spawn_barrier();
    if wire_names.is_empty() {
        return Err(anyhow!("no @sim coroutines found in project `{project}`"));
    }

    let entry_ids = recorder.plan_entries(&wire_names);
    let requested_mode = sim_exec_mode_name(mode);
    let mode = resolve_exec_mode(mode, update_snapshots, quiet)?;
    let effective_jobs = resolve_jobs(jobs, None, update_snapshots, jobs_force)?;
    let requested_full_asset_jobs = full_asset_jobs;
    let full_asset_jobs = resolve_full_asset_jobs(full_asset_jobs, effective_jobs);
    recorder.configure(
        &project_path,
        &wire_names,
        jobs,
        effective_jobs,
        requested_full_asset_jobs,
        full_asset_jobs,
        requested_mode,
        sim_exec_mode_name(mode),
        update_snapshots,
        &source_files,
    );
    let selected = wire_names.len();
    let mut routine_names = Vec::new();
    let mut routine_ids = Vec::new();
    for (name, id) in wire_names.into_iter().zip(entry_ids) {
        if manual_sim(env_by_wire.get(&name).map_or(&[][..], |v| v))? {
            record_excluded_entry(Some(recorder), Some(id));
            SimSummary {
                excluded: Some("manual; run by name".to_string()),
                saw_stop: true,
                ..SimSummary::default()
            }.print(&name, false);
        } else {
            routine_names.push(name);
            routine_ids.push(id);
        }
    }
    let wire_names = routine_names;
    let entry_ids = routine_ids;
    let manual_excluded = selected - wire_names.len();
    let mut reload_lane = reload_lane_session(
        backend,
        &project_path,
        any_reload_lane_selected(&wire_names, &env_by_wire),
        Path::new(&output),
        prepared.stone,
        quiet,
    )?;
    if mode != SimExecMode::Isolated {
        let results = run_specs_worker_mode(
            &project,
            &output,
            &env_by_wire,
            &wire_names,
            quiet,
            update_snapshots,
            jobs,
            jobs_force,
            full_asset_jobs,
            reload_lane.as_mut(),
            Some(recorder),
            Some(&entry_ids),
        )?;
        let mut passed = 0usize;
        let mut excluded = manual_excluded;
        let mut failed: Vec<String> = Vec::new();
        for (name, summary) in &results {
            if entry_excluded(summary) {
                excluded += 1;
            } else if entry_passed(summary) {
                passed += 1;
            } else {
                failed.push(name.clone());
            }
        }
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
        println!(
            "[sim] {project} suite: {passed}/{executed} sims passed{excl_suffix}{suffix}"
        );
        if !failed.is_empty() {
            return Err(anyhow!(
                "{} of {} sims failed in `{project}`",
                failed.len(),
                executed
            ));
        }
        return Ok(());
    }

    let jobs = resolve_jobs(jobs, None, update_snapshots, jobs_force)?;
    let active_renderer = active_sim_renderer(&project_path);
    let mut summaries: HashMap<usize, Result<SimSummary>> = HashMap::new();
    let mut active_indices: Vec<usize> = Vec::new();

    for i in 0..wire_names.len() {
        let env_pairs = env_by_wire.get(&wire_names[i]).map_or(&[][..], |v| v);
        if let Some(req) = sim_required_backend(env_pairs) {
            if req != active_renderer {
                let reason = format!("requires {req} renderer (active is {active_renderer})");
                let mut s = SimSummary::default();
                s.excluded = Some(reason);
                s.saw_stop = true;
                record_excluded_entry(Some(recorder), Some(entry_ids[i]));
                if !quiet {
                    s.print(&wire_names[i], false);
                }
                summaries.insert(i, Ok(s));
                continue;
            }
        }
        active_indices.push(i);
    }

    // Two lanes: reload sims share the lane's one session, so they run one at a
    // time beside the pool (`beside_reload_lane`); everything else is
    // independent once the compiled output is invocation-owned.
    let (reload_idx, ordinary_idx): (Vec<usize>, Vec<usize>) = active_indices
        .into_iter()
        .partition(|&i| entry_is_reload_lane(&wire_names[i], &env_by_wire));

    // Concurrent children cannot stream: their progress lines would interleave
    // into nonsense. Buffer instead and print each sim's complete record in
    // registry order below, which is what the concise multi-run path already
    // does. Serial runs keep today's live stream.
    let streaming = jobs == 1 && !quiet;

    // A record is finished, printed, and accounted in ONE place, so a sim's
    // lines can never interleave with another's: everything after the child
    // exits happens under `report` for concurrent runs. Video assertions shell
    // out to ffprobe/ffmpeg but ride the same pool, so they stay bounded by
    // `--jobs` rather than fanning out on their own.
    let report = Mutex::new(());
    // Reload entries own the Session and an isolated copy of the boot chunk.
    let run_at = |i: usize, lane: Option<(&mut crate::skull_backend::SkullStone, &str)>| -> Result<SimSummary> {
        let (stone, entry_output) = match lane {
            Some((stone, out)) => (Some(stone), out),
            None => (None, output.as_str()),
        };
        let (display_name, summary) = run_named_entry(
            &project,
            &wire_names[i],
            quiet,
            !streaming,
            update_snapshots,
            entry_output,
            &env_by_wire,
            stone,
            None,
            Some(recorder),
            Some(entry_ids[i]),
        )?;

        let _lock = report.lock().unwrap();
        if !quiet {
            // Result output: the child's own retained stderr/stdout, emitted
            // here so it lands with its record instead of interleaving.
            if !summary.diagnostics.is_empty() {
                eprint!("{}", summary.diagnostics);
            }
            // Concurrent runs never streamed the per-expect lines, so the
            // record must carry its own sited failures (`streamed = false`).
            summary.print(&display_name, streaming);
        }
        Ok(summary)
    };

    let (pool, lane) = beside_reload_lane(
        jobs,
        !reload_idx.is_empty(),
        |width| run_bounded(width, ordinary_idx.len(), |slot| run_at(ordinary_idx[slot], None)),
        || run_reload_lane_serially(reload_lane.as_mut(), &reload_idx, |i, lane| run_at(i, Some(lane))),
    );
    for (slot, r) in pool {
        summaries.insert(ordinary_idx[slot], r);
    }
    summaries.extend(lane);

    // Records printed in completion order; the ACCOUNTING is registry order, so
    // the failed list and tally are identical to a serial run's.
    let mut passed = 0usize;
    let mut excluded = manual_excluded;
    let mut failed: Vec<String> = Vec::new();
    for (i, name) in wire_names.iter().enumerate() {
        let summary = match summaries.remove(&i) {
            Some(Ok(s)) => s,
            Some(Err(e)) => return Err(e),
            None => return Err(anyhow!("sim `{name}` was never scheduled")),
        };
        if entry_excluded(&summary) {
            excluded += 1;
        } else if entry_passed(&summary) {
            passed += 1;
        } else {
            failed.push(name.clone());
        }
    }

    // Result output, NOT logging: the suite verdict is this command's
    // answer, consumed by the user / CI.
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
    println!(
        "[sim] {project} suite: {passed}/{executed} sims passed{excl_suffix}{suffix}"
    );

    if !failed.is_empty() {
        return Err(anyhow!(
            "{} of {} sims failed in `{project}`",
            failed.len(),
            executed
        ));
    }
    Ok(())
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
        || !summary.allocation_census.as_ref().unwrap().measurement_complete
    {
        "no-verdict"
    } else {
        "PASS"
    }
}

fn census_failure_messages(summary: &SimSummary) -> Vec<String> {
    let mut messages: Vec<String> = summary
        .errors
        .iter()
        .map(|m| if m.site.is_empty() {
            m.message.clone()
        } else {
            format!("{}: {}", m.site, m.message)
        })
        .collect();
    messages.extend(summary.expects_fail.iter().map(|m| if m.site.is_empty() {
        m.message.clone()
    } else {
        format!("{}: {}", m.site, m.message)
    }));
    messages
}

fn allocation_census_record(name: &str, summary: &SimSummary) -> (serde_json::Value, Option<(f64, f64)>) {
    let census = summary.allocation_census.as_ref();
    let measured_frames = census.map_or(0, |c| c.measured_frames);
    let measured_frames_f = measured_frames as f64;
    let measured_gross_kb = census.map_or(0.0, |c| c.measured_gross_kb);
    let measured_allocator_calls = census.map_or(0, |c| c.measured_allocator_calls);
    let measured_allocation_calls = census.map_or(0, |c| c.measured_allocation_calls);
    let measured_reallocation_calls = census.map_or(0, |c| c.measured_reallocation_calls);
    let gross_kb_per_frame = if measured_frames > 0 {
        measured_gross_kb / measured_frames_f
    } else {
        0.0
    };
    let allocator_calls_per_frame = if measured_frames > 0 {
        measured_allocator_calls as f64 / measured_frames_f
    } else {
        0.0
    };
    let allocation_calls_per_frame = if measured_frames > 0 {
        measured_allocation_calls as f64 / measured_frames_f
    } else {
        0.0
    };
    let reallocation_calls_per_frame = if measured_frames > 0 {
        measured_reallocation_calls as f64 / measured_frames_f
    } else {
        0.0
    };
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

/// Run each selected sim in its own process with the allocator census enabled.
/// This is deliberately separate from ordinary `rock sim`: the sensor is a
/// discovery instrument, not a default test-suite cost or a regression gate.
pub fn do_run_allocation_census(
    backend: crate::Backend,
    project: &str,
    names: &[String],
    output_path: &str,
    jobs: Option<usize>,
    jobs_force: bool,
) -> Result<()> {
    let path = PathBuf::from(output_path);
    if path.exists() {
        std::fs::remove_file(&path)
            .with_context(|| format!("removing stale census artifact `{}`", path.display()))?;
    }
    rock_host::project::ensure_workspace_root()?;
    let project_path = rock_host::project::resolve_project_dir(project)?;
    let project = project_path.to_string_lossy().into_owned();
    rock_host::engine_build::build_engine_libs(&project)?;

    let out_dir = sim_output_dir()?;
    let output = out_dir.path().to_string_lossy().into_owned();
    let (wire_names, env_by_wire, _source_files, stone) =
        compile_sims(backend, project_path.clone(), PathBuf::from(&output))?;
    emit_bytecode_image(Path::new(&output));
    sim_spawn_barrier();

    let selected = if names.is_empty() {
        wire_names.clone()
    } else {
        resolve_sim_specs(&wire_names, names)?
    };
    if selected.is_empty() {
        return Err(anyhow!("no @sim coroutines found in project `{project}`"));
    }
    for name in &selected {
        if parse_sim_chain(name).len() != 1 {
            return Err(anyhow!(
                "allocation census accepts one sim per entry; comma-chain `{name}` would hide a per-sim row"
            ));
        }
    }

    let (reload_idx, ordinary_idx): (Vec<usize>, Vec<usize>) =
        (0..selected.len()).partition(|&i| entry_is_reload_lane(&selected[i], &env_by_wire));
    let jobs = resolve_jobs(jobs, None, false, jobs_force)?;
    // Reload entries own the Session and an isolated copy of the boot chunk.
    let mut reload_lane = reload_lane_session(
        backend,
        &project_path,
        !reload_idx.is_empty(),
        Path::new(&output),
        Some(stone),
        false,
    )?;
    let run_at = |i: usize, lane: Option<(&mut crate::skull_backend::SkullStone, &str)>| {
        let (stone, entry_output) = match lane {
            Some((stone, out)) => (Some(stone), out),
            None => (None, output.as_str()),
        };
        run_named_entry(
            &project,
            &selected[i],
            true,
            true,
            false,
            entry_output,
            &env_by_wire,
            stone,
            Some(DEFAULT_SIM_ALLOCATION_WARMUP_FRAMES),
            None,
            None,
        )
    };

    let (pool, lane) = beside_reload_lane(
        jobs,
        !reload_idx.is_empty(),
        |width| run_bounded(width, ordinary_idx.len(), |slot| run_at(ordinary_idx[slot], None)),
        || run_reload_lane_serially(reload_lane.as_mut(), &reload_idx, |i, lane| run_at(i, Some(lane))),
    );
    let mut results: HashMap<usize, Result<(String, SimSummary)>> = HashMap::new();
    for (slot, result) in pool {
        results.insert(ordinary_idx[slot], result);
    }
    results.extend(lane);

    let mut records = Vec::with_capacity(selected.len());
    let mut ranked = Vec::new();
    let mut failed = Vec::new();
    for (i, name) in selected.iter().enumerate() {
        match results.remove(&i) {
            Some(Ok((display_name, summary))) => {
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
            Some(Err(error)) => {
                failed.push(format!("{name} (process failure)"));
                records.push(serde_json::json!({
                    "sim_name": name,
                    "process_id": serde_json::Value::Null,
                    "build_sha": serde_json::Value::Null,
                    "status": "process failure",
                    "sim_verdict": "FAIL",
                    "failure_messages": [error.to_string()],
                    "stop_sim_reached": false,
                    "rendering_enabled": serde_json::Value::Null,
                    "rendering_skipped": serde_json::Value::Null,
                    "measurement_complete": false,
                    "warmup_frames_requested": DEFAULT_SIM_ALLOCATION_WARMUP_FRAMES,
                    "warmup_frames": 0,
                    "frame_count": 0,
                    "measured_frames": 0,
                    "initialization_gross_kb": 0.0,
                    "outside_frame_gross_kb": 0.0,
                    "warmup_gross_kb": 0.0,
                    "measured_gross_kb": 0.0,
                    "total_gross_kb": 0.0,
                    "gross_kb_per_frame": 0.0,
                    "measured_allocator_calls": 0,
                    "measured_allocation_calls": 0,
                    "measured_reallocation_calls": 0,
                    "allocator_calls_per_frame": 0.0,
                    "allocation_calls_per_frame": 0.0,
                    "reallocation_calls_per_frame": 0.0,
                }));
            }
            None => {
                return Err(anyhow!("sim `{name}` was never scheduled"));
            }
        }
    }
    if records.len() != selected.len() {
        return Err(anyhow!(
            "allocation census emitted {} records for {} examined sims",
            records.len(),
            selected.len()
        ));
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
            Some(serde_json::json!({"sim_name": name, "status": record["status"], "reason": reason}))
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
        .map(|ch| if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' { ch } else { '_' })
        .collect::<String>();
    format!("{index:04}-{safe}")
}

fn allocation_activation_population(text: &str) -> Result<(usize, usize)> {
    let mut lines = text.lines();
    if lines.next() != Some("rock.allocation-activation.v2") {
        return Err(anyhow!("sim allocation-site activation is not frame-aware v2"));
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
        return Err(anyhow!("sim allocation-site activation declared {sites} sites but wrote {rows} rows"));
    }
    Ok((frames, sites))
}

pub fn do_run_allocation_sites(
    backend: crate::Backend,
    project: &str,
    names: &[String],
    output_dir: &str,
    jobs: Option<usize>,
    jobs_force: bool,
) -> Result<()> {
    let artifact_dir = PathBuf::from(output_dir);
    if artifact_dir.exists() {
        if !artifact_dir.is_dir() {
            return Err(anyhow!("allocation-site output `{}` is not a directory", artifact_dir.display()));
        }
        if fs_err::read_dir(&artifact_dir)?.next().is_some() {
            return Err(anyhow!("allocation-site output directory `{}` is not empty", artifact_dir.display()));
        }
    } else {
        fs_err::create_dir_all(&artifact_dir)?;
    }
    let artifact_dir = fs_err::canonicalize(&artifact_dir)?;
    let manifest_path = artifact_dir.join("manifest.txt");

    rock_host::project::ensure_workspace_root()?;
    let project_path = rock_host::project::resolve_project_dir(project)?;
    let project = project_path.to_string_lossy().into_owned();
    rock_host::engine_build::build_engine_libs(&project)?;

    let out_dir = sim_output_dir()?;
    let output = out_dir.path().to_string_lossy().into_owned();
    let (wire_names, env_by_wire, _source_files, stone) = compile_sims_with_allocation(
        backend,
        project_path,
        PathBuf::from(&output),
        Some(&manifest_path),
        &[],
    )?;
    let artifact_fingerprint = stone.artifact_fingerprint().to_string();
    drop(stone);
    let generated_lua_path = artifact_dir.join("main.lua");
    fs_err::copy(
        Path::new(&output).join(rock_host::project::MAIN_LUA_FILENAME),
        &generated_lua_path,
    )?;
    let manifest = fs_err::read_to_string(&manifest_path)?;
    let generated_lua = fs_err::read_to_string(&generated_lua_path)?;
    sim_spawn_barrier();

    let selected = if names.is_empty() { wire_names.clone() } else { resolve_sim_specs(&wire_names, names)? };
    if selected.is_empty() {
        return Err(anyhow!("no @sim coroutines found in project `{project}`"));
    }
    for name in &selected {
        if parse_sim_chain(name).len() != 1 {
            return Err(anyhow!("allocation-site diagnostics accept one sim per entry; comma-chain `{name}` would merge frame populations"));
        }
        if entry_is_reload_lane(name, &env_by_wire) {
            return Err(anyhow!("allocation-site diagnostics do not support reload sim `{name}` because its emitted site IDs can change after the initial manifest"));
        }
    }

    let jobs = resolve_jobs(jobs, None, false, jobs_force)?;
    let run_at = |index: usize| -> Result<(String, SimSummary)> {
        let name = &selected[index];
        let stem = allocation_site_artifact_stem(index, name);
        let activation_path = artifact_dir.join(format!("{stem}.activation.tsv"));
        let mut instrumented_env = env_by_wire.clone();
        instrumented_env.entry(name.clone()).or_default().push((
            "SKULL_ALLOCATION_ACTIVATION".to_string(),
            activation_path.to_string_lossy().into_owned(),
        ));
        instrumented_env.entry(name.clone()).or_default().extend([
            ("STONE_TEXT_LAYOUT_SIM_PROBES".to_string(), "0".to_string()),
            ("STONE_IMGUI_SIM_PROBES".to_string(), "0".to_string()),
        ]);
        run_named_entry(
            &project,
            name,
            true,
            true,
            false,
            &output,
            &instrumented_env,
            None,
            None,
            None,
            None,
        )
    };
    let mut results = HashMap::new();
    for (index, result) in run_bounded(&PoolWidth::fixed(jobs), selected.len(), run_at) {
        results.insert(index, result);
    }

    let mut records = Vec::with_capacity(selected.len());
    let mut failed = Vec::new();
    for (index, name) in selected.iter().enumerate() {
        let stem = allocation_site_artifact_stem(index, name);
        let activation_name = format!("{stem}.activation.tsv");
        let joined_name = format!("{stem}.joined.tsv");
        let activation_path = artifact_dir.join(&activation_name);
        let joined_path = artifact_dir.join(&joined_name);
        match results.remove(&index) {
            Some(Ok((display_name, summary))) => {
                let status = verdict_word(&summary);
                if status != "PASS" {
                    failed.push(display_name.clone());
                    // The run was buffered; its child's stderr (a LuaJIT load
                    // error included) exists only here.
                    if !summary.diagnostics.is_empty() {
                        eprint!("{}", summary.diagnostics);
                    }
                    summary.print(&display_name, false);
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
                    format!("reading allocation-site activation for `{display_name}` from `{}`", activation_path.display())
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
                let env = env_by_wire.get(name).map_or(&[][..], Vec::as_slice);
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
            Some(Err(error)) => {
                failed.push(name.clone());
                records.push(serde_json::json!({
                    "sim_name": name,
                    "status": "process failure",
                    "failure": error.to_string(),
                    "completed_frames": 0,
                    "active_sites": 0,
                }));
            }
            None => return Err(anyhow!("sim `{name}` was never scheduled")),
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
    fs_err::write(artifact_dir.join("index.json"), serde_json::to_vec_pretty(&index)?)?;
    println!("[sim] allocation sites: {} examined, artifact={}", selected.len(), artifact_dir.display());
    if !failed.is_empty() {
        return Err(anyhow!("allocation-site diagnostics recorded {} failed sims: {}", failed.len(), failed.join(", ")));
    }
    Ok(())
}

/// Worker-mode execution for a list of entry specs (`--workers`): eligible
/// specs grouped by boot signature run in shared boot-checkpoint workers with
/// recovery; the ineligible remainder runs isolated, each with its REPORTED
/// reason.
///
/// B5 is the composition of Phase B's worker with Phase A's bounded scheduler,
/// and it is a LANE PLAN handed to [`run_bounded`], not a second scheduler or a
/// second worker lifecycle:
///
/// - a boot-signature group splits into at most `jobs` CONTIGUOUS lanes, so a
///   process still serves exactly one canonical signature and the entries in a
///   lane keep their requested ORDER. Splitting does change predecessors: each
///   lane's head now starts a fresh worker where serially it would have
///   followed someone, so `--jobs` trades boundary crossings for wall time.
///   That is why `--multi-order` does not split (see [`run_worker_arm`]);
/// - a lane is a whole `run_specs_recovering` queue, so recovery, the
///   checkpoint-identity guard and the per-entry deadline are unchanged, and a
///   worker death in one lane cannot reach another;
/// - the unit of parallelism is the ENTRY (a whole comma-chain), never a chain
///   part: a lane holds spec POSITIONS and `run_specs_recovering` expands each
///   into its parts in order, sharing one `$OUT`;
/// - an isolated entry that does not need the compile session rides the same
///   pool; the hot-reload lane runs BESIDE the pool from its own session and
///   chunk (`beside_reload_lane`), its entries serial among themselves;
/// - reporting is deferred to spec order. Completion order is not
///   deterministic, and a record has to be emitted whole.
///
/// Returns one summary per spec, in spec order.
/// Why worker mode cannot run here, if it cannot.
///
/// One place, so the DEFAULT and an explicit `--workers` cannot disagree about
/// what is possible -- they differ only in what they do about it.
fn worker_mode_unavailable(update_snapshots: bool) -> Option<String> {
    if cfg!(windows) {
        return Some(
            "worker mode is not available on Windows: the per-entry environment reaches \
             only the CRT view, so a native reader using the Win32 environment block \
             would see the previous entry's $OUT"
                .to_string(),
        );
    }
    if update_snapshots {
        return Some(
            "worker mode cannot rewrite snapshot baselines: the runner does not yet \
             prove every destination path is unique"
                .to_string(),
        );
    }
    None
}

/// Resolve the mode actually used. A DEFAULT degrades to the oracle and SAYS SO
/// -- an unreported downgrade reads as a worker result, which is the same trap
/// the isolated-entry reasons exist to avoid. An explicit `--workers` errors
/// instead, because it was asked for on purpose.
fn resolve_exec_mode(mode: SimExecMode, update_snapshots: bool, quiet: bool) -> Result<SimExecMode> {
    let Some(reason) = worker_mode_unavailable(update_snapshots) else {
        return Ok(mode);
    };
    match mode {
        SimExecMode::Workers => Err(anyhow!("--workers: {reason}")),
        SimExecMode::Default => {
            if !quiet {
                println!("[sim] running isolated: {reason}");
            }
            Ok(SimExecMode::Isolated)
        }
        SimExecMode::Isolated => Ok(SimExecMode::Isolated),
    }
}

fn run_specs_worker_mode(
    project: &str,
    output: &str,
    env_by_wire: &HashMap<String, Vec<(String, String)>>,
    specs: &[String],
    quiet: bool,
    update_snapshots: bool,
    jobs: Option<usize>,
    jobs_force: bool,
    full_asset_jobs: usize,
    reload_lane: Option<&mut ReloadLaneSession>,
    recorder: Option<&SimRunRecorder>,
    entry_ids: Option<&[u64]>,
) -> Result<Vec<(String, SimSummary)>> {

    let jobs = resolve_jobs(jobs, None, update_snapshots, jobs_force)?;
    let active_renderer = active_sim_renderer(Path::new(project));
    let mut slots: Vec<Option<SimSummary>> = specs.iter().map(|_| None).collect();
    let mut active_specs: Vec<String> = Vec::new();
    let mut active_indices: Vec<usize> = Vec::new();

    for (i, spec) in specs.iter().enumerate() {
        let excluded_reason = entry_backend_exclusion(spec, env_by_wire, active_renderer);
        if let Some(reason) = excluded_reason {
            let mut s = SimSummary::default();
            s.excluded = Some(reason);
            s.saw_stop = true;
            record_excluded_entry(recorder, entry_ids.and_then(|ids| ids.get(i)).copied());
            slots[i] = Some(s);
        } else {
            active_specs.push(spec.clone());
            active_indices.push(i);
        }
    }

    let (groups, isolated) = group_worker_specs(&active_specs, env_by_wire);

    let entry_estimates: Vec<_> = specs.iter().map(|spec| spec_schedule_estimate(spec, recorder)).collect();
    let active_costs: Vec<_> = active_indices.iter().map(|&i| entry_estimates[i].0).collect();
    let lanes: Vec<Vec<usize>> = allocate_worker_lanes(&groups, jobs, &active_costs)
        .into_iter()
        .map(|lane| lane.into_iter().map(|idx| active_indices[idx]).collect())
        .collect();

    // An isolated entry needs the compile SESSION only when it opted into the
    // hot-reload lane; everything else isolated is an ordinary fresh process
    // and can overlap. `worker_ineligibility` has other reasons (a `$OUT`-
    // interpolating env, disagreeing chain parts, `SIM_WORKER_ISOLATED`) and
    // none of them touch the session.
    let (exclusive, poolable): (Vec<_>, Vec<_>) = isolated
        .into_iter()
        .map(|(idx, r)| (active_indices[idx], r))
        .partition(|(i, _)| entry_is_reload_lane(&specs[*i], env_by_wire));

    // One entry's scheduling estimate and where it came from: this machine's
    // history first, else the checked-in seeded median, else nothing. A lane
    // sums its entries; see `sum_estimates` for the source rule.
    let estimate_ms = |i: usize| entry_estimates[i];
    let recorded_ms = |i: usize| -> Option<u64> { estimate_ms(i).0 };
    let lane_estimates_sources: Vec<(Option<u64>, Option<EstimateSource>)> = lanes
        .iter()
        .map(|lane| sum_estimates(lane.iter().map(|&i| estimate_ms(i))))
        .chain(poolable.iter().map(|(i, _)| estimate_ms(*i)))
        .collect();
    let lane_estimates: Vec<Option<u64>> =
        lane_estimates_sources.iter().map(|(ms, _)| *ms).collect();

    if let Some(recorder) = recorder {
        for (lane_id, lane) in lanes.iter().enumerate() {
            let entries: Vec<(u64, String)> = lane
                .iter()
                .map(|&i| {
                    (
                        entry_ids.and_then(|ids| ids.get(i)).copied().unwrap_or(0),
                        specs[i].clone(),
                    )
                })
                .collect();
            let signature = parse_sim_chain(&specs[lane[0]])
                .first()
                .and_then(|name| env_by_wire.get(*name))
                .map_or_else(Vec::new, |env| boot_signature(env));
            recorder.lane_planned(lane_id, "shared", &entries, &signature, None, lane_estimates[lane_id]);
        }
        for (offset, (i, reason)) in poolable.iter().chain(exclusive.iter()).enumerate() {
            let entries = vec![(
                entry_ids.and_then(|ids| ids.get(*i)).copied().unwrap_or(0),
                specs[*i].clone(),
            )];
            let signature = parse_sim_chain(&specs[*i])
                .first()
                .and_then(|name| env_by_wire.get(*name))
                .map_or_else(Vec::new, |env| boot_signature(env));
            recorder.lane_planned(
                offset + lanes.len(),
                "isolated",
                &entries,
                &signature,
                Some(reason),
                recorded_ms(*i),
            );
        }
    }

    if !quiet {
        // EVERY lane is a boot, so every lane reports one. A singleton lane
        // printed nothing, which made a sim that silently got its own process
        // indistinguishable in the log from one sharing a 46-way worker -- the
        // same thing the isolation reasons below exist to prevent, on the path
        // they do not cover. Measured on mag: 18 lines printed against 43 boots.
        //
        // A group larger than the job count splits into contiguous lanes, and
        // that split CHANGES PREDECESSORS, so it decides which entries are
        // boundary-crossing evidence. Say so rather than leaving it to be
        // derived from `--jobs`.
        for lane in &lanes {
            match lane.len() {
                1 => println!("[sim] worker: 1 entry in its own booted process"),
                n => println!("[sim] worker: {n} entries share one booted process"),
            }
        }
        for (i, reason) in poolable.iter() {
            println!("[sim] {}: isolated ({reason})", specs[*i]);
        }
        for (i, reason) in exclusive.iter() {
            println!("[sim] {}: isolated ({reason})", specs[*i]);
        }
    }

    // One pool over both kinds of work: a recovering worker queue, and an
    // isolated entry that needs no session.
    #[derive(Clone, Copy)]
    enum Lane<'a> {
        Worker(&'a [usize]),
        Isolated(usize),
    }
    let work: Vec<Lane> = lanes
        .iter()
        .map(|l| Lane::Worker(l.as_slice()))
        .chain(poolable.iter().map(|(i, _)| Lane::Isolated(*i)))
        .collect();
    // A serial run gains nothing from reordering, so it keeps the plan's order.
    let work: Vec<Lane> = if jobs > 1 {
        if !quiet {
            // Every fallback is counted here: history-timed and seeded lanes
            // dispatch longest-first, untimed lanes keep plan order after them.
            let history = lane_estimates_sources
                .iter()
                .filter(|(_, source)| *source == Some(EstimateSource::History))
                .count();
            let seeded = lane_estimates_sources
                .iter()
                .filter(|(_, source)| *source == Some(EstimateSource::Seeded))
                .count();
            let untimed = work.len().saturating_sub(history + seeded);
            println!(
                "[sim] worker: lanes start longest-first \
                ({history} history-timed, {seeded} seeded, {untimed} untimed keep plan order of {} lanes)",
                work.len()
            );
        }
        longest_first(&lane_estimates).into_iter().map(|w| work[w]).collect()
    } else {
        work
    };

    let default_stub = asset_stub_value();
    let spec_full_assets = |i: usize| {
        parse_sim_chain(&specs[i]).iter().any(|p| {
            entry_full_assets(env_by_wire.get(*p).map_or(&[][..], |v| v), default_stub)
        })
    };
    let work_full_assets: Vec<bool> = work
        .iter()
        .map(|l| match l {
            Lane::Worker(lane) => lane.iter().any(|&i| spec_full_assets(i)),
            Lane::Isolated(i) => spec_full_assets(*i),
        })
        .collect();
    if !quiet && full_asset_jobs < jobs {
        let full = work_full_assets.iter().filter(|r| **r).count();
        println!(
            "[sim] worker: at most {full_asset_jobs} of {jobs} lanes hold full-size assets at once ({full} full-asset lanes of {})",
            work.len()
        );
    }

    let pool = |width: &PoolWidth| run_bounded_classed(width, full_asset_jobs, &work_full_assets, |w| match work[w] {
        Lane::Worker(lane) => {
            let lane_specs: Vec<String> = lane.iter().map(|&i| specs[i].clone()).collect();
            // `run_specs_recovering` is positional, so its results line up with
            // `lane_specs` and therefore with `lane`'s indices.
            let lane_entry_ids = entry_ids.map(|ids| {
                lane.iter().map(|&i| ids[i]).collect::<Vec<_>>()
            });
            let results = run_specs_recovering(
                project,
                output,
                env_by_wire,
                &lane_specs,
                update_snapshots,
                false,
                recorder,
                lane_entry_ids.as_deref(),
                None,
            )?;
            if results.len() != lane.len() {
                return Err(anyhow!(
                    "worker lane returned {} results for {} specs",
                    results.len(),
                    lane.len()
                ));
            }
            Ok(lane
                .iter()
                .zip(results)
                .map(|(&i, (_, summary, _crossed))| (i, summary))
                .collect::<Vec<_>>())
        }
        Lane::Isolated(i) => {
            let (_, summary) = run_named_entry(
                project,
                &specs[i],
                quiet,
                true,
                update_snapshots,
                output,
                env_by_wire,
                None,
                None,
                recorder,
                entry_ids.and_then(|ids| ids.get(i)).copied(),
            )?;
            Ok(vec![(i, summary)])
        }
    });

    // Reload workers run serially through the lane's session. Only entries
    // with compatible boot signatures share a child process.
    //
    // Each reload sim restores the source it edited before it stops, so an
    // entry cannot hand reloaded code to its successor; `revert_reload_edits`
    // still runs once when the child exits, as the backstop it always was.
    // A reload sim that asserts what BOOT did cannot follow another entry, and
    // several do -- a chained successor restores the boot image instead of
    // booting. Those declare `SIM_WORKER_ISOLATED` and run alone inside the
    // lane; the rest chain. Same opt-out the pool honours, read here because
    // `worker_ineligibility` reports the reload lane first.
    let run_lane = || -> Result<Vec<(usize, SimSummary)>> {
        let mut done = Vec::new();
        if exclusive.is_empty() {
            return Ok(done);
        }
        let lane = reload_lane
            .ok_or_else(|| anyhow!("reload sims were selected without a lane session"))?;
        let (lane_stone, lane_output) = (&mut lane.stone, lane.output.as_str());
        let reload_indices: Vec<usize> = exclusive.iter().map(|(i, _)| *i).collect();
        let (lane_alone, lane_chains) = group_reload_worker_specs(&reload_indices, specs, env_by_wire);
        for i in lane_alone {
            let (_, summary) = run_named_entry(
                project,
                &specs[i],
                quiet,
                true,
                update_snapshots,
                lane_output,
                env_by_wire,
                Some(&mut *lane_stone),
                None,
                recorder,
                entry_ids.and_then(|ids| ids.get(i)).copied(),
            )?;
            done.push((i, summary));
        }
        for lane_chained in lane_chains {
            let lane_specs: Vec<String> =
                lane_chained.iter().map(|i| specs[*i].clone()).collect();
            let lane_entry_ids: Option<Vec<u64>> = entry_ids
                .map(|ids| lane_chained.iter().map(|i| ids[*i]).collect());
            let results = run_specs_recovering(
                project,
                lane_output,
                env_by_wire,
                &lane_specs,
                update_snapshots,
                false,
                recorder,
                lane_entry_ids.as_deref(),
                Some(&mut *lane_stone),
            )?;
            if results.len() != lane_chained.len() {
                return Err(anyhow!(
                    "reload lane returned {} results for {} entries",
                    results.len(),
                    lane_chained.len()
                ));
            }
            for (i, (_, summary, _crossed)) in lane_chained.iter().zip(results) {
                done.push((*i, summary));
            }
        }
        Ok(done)
    };

    let (pool, lane) = beside_reload_lane(jobs, !exclusive.is_empty(), pool, run_lane);
    for (_, r) in pool {
        for (i, summary) in r? {
            slots[i] = Some(summary);
        }
    }
    for (i, summary) in lane? {
        slots[i] = Some(summary);
    }

    // Records are emitted in SPEC order, whole. Completion order under a pool is
    // not deterministic, and a record's diagnostics have to land with it.
    if !quiet {
        for (spec, slot) in specs.iter().zip(slots.iter()) {
            if let Some(summary) = slot {
                if !summary.diagnostics.is_empty() {
                    eprint!("{}", summary.diagnostics);
                }
                summary.print(spec, false);
            }
        }
    }

    specs
        .iter()
        .zip(slots)
        .map(|(spec, slot)| {
            let summary = slot.ok_or_else(|| anyhow!("sim `{spec}` was never scheduled"))?;
            Ok((spec.clone(), summary))
        })
        .collect()
}

/// A mid-sim `reload_edit` request parsed off the `__SIM__` side channel
/// (`docs/sim-reload-harness-plan.md`).
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

/// The one parent-side accounting authority for the request -> publication ->
/// watcher-consumption path. A request is booked before compilation, becomes
/// published only after `SkullStone` writes the overlay, and can become
/// terminal only after the production watcher reports what it consumed.
#[derive(Default, Debug)]
struct ReloadLedger {
    requests: BTreeMap<u64, ReloadRequestState>,
}

impl ReloadLedger {
    fn book(&mut self, request_id: u64) -> Result<()> {
        if self.requests.insert(request_id, ReloadRequestState::Booked).is_some() {
            return Err(anyhow!("reload request {request_id} was booked twice"));
        }
        Ok(())
    }

    fn publish(&mut self, request_id: u64) -> Result<()> {
        match self.requests.get_mut(&request_id) {
            Some(state @ ReloadRequestState::Booked) => {
                *state = ReloadRequestState::Published;
                Ok(())
            }
            Some(state) => Err(anyhow!(
                "reload request {request_id} published from terminal/non-booked state {state:?}"
            )),
            None => Err(anyhow!("reload request {request_id} published before it was booked")),
        }
    }

    fn reject(&mut self, request_id: u64) -> Result<()> {
        match self.requests.get_mut(&request_id) {
            Some(state @ ReloadRequestState::Booked) => {
                *state = ReloadRequestState::Rejected;
                Ok(())
            }
            Some(state) => Err(anyhow!(
                "reload request {request_id} rejected from state {state:?}"
            )),
            None => Err(anyhow!("reload request {request_id} rejected before it was booked")),
        }
    }

    fn complete(&mut self, request_id: u64, terminal: ReloadTerminal) -> Result<()> {
        match self.requests.get_mut(&request_id) {
            Some(state @ ReloadRequestState::Published) => {
                *state = match terminal {
                    ReloadTerminal::Applied => ReloadRequestState::Applied,
                    ReloadTerminal::Failed => ReloadRequestState::Failed,
                };
                Ok(())
            }
            Some(state) => Err(anyhow!(
                "reload request {request_id} completed before publication or twice (state {state:?})"
            )),
            None => Err(anyhow!("reload request {request_id} completed before it was booked")),
        }
    }

    fn summary(&self) -> ReloadLedgerSummary {
        let mut summary = ReloadLedgerSummary { requests: self.requests.len(), ..Default::default() };
        for state in self.requests.values() {
            match state {
                ReloadRequestState::Booked | ReloadRequestState::Published => summary.pending += 1,
                ReloadRequestState::Applied => summary.applied += 1,
                ReloadRequestState::Rejected => summary.rejected += 1,
                ReloadRequestState::Failed => summary.failed += 1,
            }
        }
        summary
    }

    fn validate(&self) -> Result<ReloadLedgerSummary> {
        let summary = self.summary();
        let classified = summary.applied + summary.pending + summary.rejected + summary.failed;
        if classified != summary.requests {
            return Err(anyhow!(
                "reload ledger conservation violated: requests={} classified={}"
                , summary.requests, classified
            ));
        }
        Ok(summary)
    }
}

/// Apply one `reload_edit`: in-memory find/replace on the file's CURRENT
/// in-session text, recompiled through the real incremental path
/// (`SkullStone::apply_edit`), which writes the overlay the running game
/// hot-reloads. BodyOnly edits only -- any other tier is a sim-author error
/// (an interface edit would rewrite main.lua under the running child).
/// `pristine` records each file's pre-edit text at first touch so
/// `revert_reload_edits` can restore the session after the child exits.
fn service_reload_edit(
    stone: &mut crate::skull_backend::SkullStone,
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
    pristine.entry(req.file.clone()).or_insert_with(|| current.clone());
    let new_text = current.replacen(&req.find, &req.replace, 1);
    let main_path = Path::new(output).join(rock_host::project::MAIN_LUA_FILENAME);
    let incremental_path = main_path.with_file_name("incremental.lua");
    match stone.apply_edit(path, &new_text, &main_path, &incremental_path)? {
        incremental @ crate::skull_backend::SkullReload::Incremental { bytes, reinferred, .. } => {
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

fn book_reload_request(ledger: &mut ReloadLedger, req: &ReloadEditReq) -> Result<u64> {
    let request_id = req
        .request_id
        .ok_or_else(|| anyhow!("reload_edit omitted request_id"))?;
    ledger.book(request_id)?;
    Ok(request_id)
}

fn consume_reload_completion(
    ledger: &mut ReloadLedger,
    completion: ReloadCompletion,
) -> Result<u64> {
    let request_id = completion
        .request_id
        .ok_or_else(|| anyhow!("reload_terminal omitted request_id"))?;
    ledger.complete(request_id, completion.terminal)?;
    Ok(request_id)
}

fn terminate_reload_child(child: &mut Child) -> Result<ExitStatus> {
    if let Err(kill_error) = child.kill() {
        if child.try_wait()?.is_none() {
            return Err(kill_error).context("stopping sim after reload rejection");
        }
    }
    child.wait().context("reaping sim after reload rejection")
}

/// Restore every reload_edit-touched file to its pre-sim text and remove the
/// overlay. Both matter for the NEXT child in a suite run: `emit_overlay`
/// accumulates, so an unreverted session would leak this sim's edits into a
/// later reload sim's overlay -- and the in-game watcher fires on the
/// overlay's first appearance (baseline modtime 0), so a leftover file would
/// be dofile'd at that sim's startup.
fn revert_reload_edits(
    stone: &mut crate::skull_backend::SkullStone,
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

/// Whether a named entry belongs to the exclusive reload lane.
///
/// An entry is a comma-chain of parts sharing one `$OUT`; if ANY part opts into
/// the reload lane the whole entry is exclusive, because the parts run in one
/// sequence. This selects the session-owned chunk; sharing a worker also
/// requires compatible boot signatures (`group_reload_worker_specs`).
fn entry_is_reload_lane(
    chain_spec: &str,
    env_by_wire: &HashMap<String, Vec<(String, String)>>,
) -> bool {
    parse_sim_chain(chain_spec)
        .iter()
        .any(|part| env_by_wire.get(*part).is_some_and(|e| is_reload_lane(e)))
}

/// CPU bound on sim concurrency. Memory bounds are applied by `sim_job_budget`.
fn default_sim_jobs() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// Resolve the cap on concurrently FULL-ASSET children (`entry_full_assets`).
/// Such a child holds every image at the file's own size on the GPU (2-4 GB
/// on the measured host, against 0.2-0.4 GB for any other child, rendering
/// or not), so on a card with a fixed budget the number of them alive at
/// once is what overflows, not the job count. `None` takes
/// `DEFAULT_FULL_ASSET_JOBS`; anything requested is clamped into `1..=jobs`.
fn resolve_full_asset_jobs(requested: Option<usize>, jobs: usize) -> usize {
    requested.unwrap_or(DEFAULT_FULL_ASSET_JOBS).clamp(1, jobs.max(1))
}

/// How many full-asset lanes (`entry_full_assets`) may be live at once unless
/// `--full-asset-jobs` says otherwise.
///
/// Measured 2026-09-13 on the mag suite at `-j 14` with `footprint -p` per
/// worker (`IOAccelerator (graphics)`): a capped lane holds 0.2-0.4 GB of GPU
/// memory whether or not it renders, and a full-asset lane 2.0-4.3 GB. With
/// fourteen lanes the capped floor is ~5.6 GB, so two full-asset lanes peak
/// near 14 GB and three near 18.5 GB. On the Linux workstation a capped
/// worker lane grows to ~2 GiB, so there the job budget (`SIM_LANE_VRAM_MIB`)
/// is what bounds GPU memory and this cap only spreads the heaviest lanes.
/// `SIM_SKIP_RENDER` is not the variable (the earlier `--render-jobs` cap
/// serialised 49 lanes for 100 s of wall and changed no verdict).
const DEFAULT_FULL_ASSET_JOBS: usize = 2;

/// Resolve the requested job count. `--update-snapshots` pins to serial: it
/// rewrites baseline files, and the runner does not yet prove every
/// destination path is unique before spawning (several `@sim`s can share one
/// source file, so its `.snap` directory is shared too).
///
/// `lanes` is the number of entries when the caller knows it: a request
/// above it asks for lanes that will never run, so it is narrowed before the
/// budget check rather than rejected for them.
pub(crate) fn resolve_jobs(requested: Option<usize>, lanes: Option<usize>, update_snapshots: bool, force: bool) -> Result<usize> {
    let requested = narrow_to_lanes(requested, lanes);
    let budget = sim_job_budget();
    let jobs = resolve_jobs_with(requested, update_snapshots, default_sim_jobs(), budget, force)?;
    if force && requested.is_some() && !update_snapshots && jobs > budget {
        static WARNED: std::sync::Once = std::sync::Once::new();
        WARNED.call_once(|| log::warn!(target: "SIM",
            "--jobs {jobs} exceeds the default memory budget of {budget} lanes"));
        FORCED_SIM_JOBS.fetch_max(jobs, std::sync::atomic::Ordering::Relaxed);
    }
    Ok(jobs)
}

/// Lanes this process was forced above the memory budget; worker admission
/// (`acquire_worker_slot`) honors the same override. Zero when not forced.
static FORCED_SIM_JOBS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn narrow_to_lanes(requested: Option<usize>, lanes: Option<usize>) -> Option<usize> {
    requested.map(|jobs| lanes.map_or(jobs, |lanes| jobs.min(lanes.max(1))))
}

fn resolve_jobs_with(requested: Option<usize>, update_snapshots: bool, cores: usize, budget: usize, force: bool) -> Result<usize> {
    if update_snapshots {
        if requested.is_some_and(|j| j > 1) {
            log::warn!(target: "SIM",
                "--update-snapshots rewrites snapshot baselines; running serially");
        }
        return Ok(1);
    }
    if let Some(jobs) = requested.filter(|j| !force && *j > budget) {
        anyhow::bail!("--jobs {jobs} exceeds the default memory budget of {budget} lanes; use --jobs-force to override");
    }
    Ok(requested.unwrap_or(budget).clamp(1, cores.max(1)))
}

/// GPU memory one sim child is planned at. Measured 2026-09-22 on the Linux
/// workstation (RTX 4090, mag suite at `-j 6`, per-process `nvidia-smi`):
/// long-lived workers peak at 2.0 GiB median and 3.8 GiB max as they run
/// more sims, a one-shot child at ~0.3 GiB, and six lanes peaked at 12.8 GiB
/// together.
const SIM_LANE_VRAM_MIB: u64 = 2048;

/// GPU memory a suite leaves free for the author's live session and anything
/// started after it (author, 2026-09-22).
const SIM_VRAM_RESERVE_MIB: u64 = 6 * 1024;

/// Lanes affordable from the GPU memory free at launch less the reserve,
/// bounded by the cores. A second suite launched meanwhile sees less free
/// memory and takes fewer lanes. No reading leaves the cores in charge.
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

/// Total-RAM planning bound: the Studio's 36 GiB / 10 lanes avoided swap
/// and beat 14 and 7 lanes at full-asset cap 2 in the same-SHA registry sweep
/// (`docs/gate-speed-plan.md`). Keep the ratio exact rather than rounding MiB.
fn memory_budgeted_jobs(cores: usize, total_ram_mib: Option<u64>, free_vram_mib: Option<u64>) -> usize {
    let gpu_jobs = vram_budgeted_jobs(cores, free_vram_mib);
    let ram_jobs = total_ram_mib.map(|ram| {
        usize::try_from(ram.saturating_mul(10) / (36 * 1024))
            .unwrap_or(cores).clamp(1, cores.max(1))
    }).unwrap_or(cores.max(1));
    gpu_jobs.min(ram_jobs)
}

fn total_system_memory_mib() -> Option<u64> {
    let mib = crate::common::total_system_memory_mib();
    if mib.is_none() {
        log::warn!(target: "SIM", "no total RAM reading; default jobs use CPU/GPU bounds only");
    }
    mib
}

/// The default job count for this process, read once so every resolution in
/// one invocation agrees and the decision is logged once.
fn sim_job_budget() -> usize {
    static BUDGET: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *BUDGET.get_or_init(|| {
        let cores = default_sim_jobs();
        let total_ram = total_system_memory_mib();
        let free_vram = match free_gpu_memory_mib() {
            Ok(free) => Some(free),
            Err(reason) => {
                // Without NVIDIA tooling, the CPU and total-RAM bounds still apply.
                log::info!(target: "SIM", "no GPU memory reading: {reason}");
                None
            }
        };
        let jobs = memory_budgeted_jobs(cores, total_ram, free_vram);
        log::info!(target: "SIM",
            "default jobs {jobs}: {cores} cores, total RAM {total_ram:?} MiB at 3.6 GiB a lane, free GPU {free_vram:?} MiB less {SIM_VRAM_RESERVE_MIB} MiB reserve at {SIM_LANE_VRAM_MIB} MiB a lane");
        jobs
    })
}

fn free_gpu_memory_mib() -> std::result::Result<u64, String> {
    let output = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=memory.free", "--format=csv,noheader,nounits"])
        .output()
        .map_err(|error| format!("nvidia-smi: {error}"))?;
    if !output.status.success() {
        return Err(format!("nvidia-smi exited with {}", output.status));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_nvidia_smi_free_mib(&stdout).ok_or_else(|| format!("nvidia-smi printed no free memory: {:?}", stdout.trim()))
}

/// The first GPU's free memory: the one GL contexts land on by default.
fn parse_nvidia_smi_free_mib(stdout: &str) -> Option<u64> {
    stdout.lines().next()?.trim().parse().ok()
}

/// Dump the compiled `main.lua` in `output` to a LuaJIT bytecode image beside
/// it, once per invocation, so every worker skips the source parse.
///
/// Parsing the 48MB `main.lua` costs a child ~300ms of its ~1.1s boot, and a
/// full run boots ~150 children from the same file; the dump costs the parent
/// one parse (~0.4s). Best-effort by design: a failed dump is logged and the
/// workers run the source file, which is exactly what they ran before. A
/// partial image is removed so `worker_chunk_path` cannot pick it up.
fn emit_bytecode_image(output: &Path) {
    let lua_file = output.join(rock_host::project::MAIN_LUA_FILENAME);
    let bytecode = output.join(rock_host::project::MAIN_BYTECODE_FILENAME);
    let started = std::time::Instant::now();
    let result = rock_host::project::build_luajit_bytecode_command(&lua_file, &bytecode).output();
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

/// The chunk a child boots from: the bytecode image when one exists, except for
/// the reload lane, which must keep the source file. `check_hot_reload`
/// derives the overlay path by replacing `main.lua` in the running script's
/// own path (`stone/engine/hot_reload.rock`), and `read_chunk_source_dir`
/// reads line 1 of the chunk as text to decide whether it is this game's;
/// neither question has an answer against a bytecode image.
fn worker_chunk_path(output: &Path, reload: bool) -> PathBuf {
    let main = output.join(rock_host::project::MAIN_LUA_FILENAME);
    if reload {
        return main;
    }
    let bytecode = output.join(rock_host::project::MAIN_BYTECODE_FILENAME);
    if bytecode.is_file() { bytecode } else { main }
}

/// Whether a sim belongs to the exclusive reload lane.
///
/// Reload sims drive `SkullStone::apply_edit` against the one live session and
/// the one `incremental.lua`, so they cannot share the runner with any other
/// child. Opting in is `@sim_env("STONE_HOT_RELOAD", "1")` -- the same switch
/// that makes the child's watcher poll the overlay at all, so a sim that could
/// observe a reload is exactly a sim that declares this.
pub fn is_reload_lane(env_pairs: &[(String, String)]) -> bool {
    env_pairs
        .iter()
        .any(|(k, v)| k == "STONE_HOT_RELOAD" && v != "0")
}

/// The explicit per-fixture opt-out of sharing a process, read on its own so
/// the reload lane can honour it too.
///
/// `worker_ineligibility` reports the reload lane FIRST, which is the right
/// reason to print, but a reload sim can also be boot-scoped -- several assert
/// what happened at boot, and a chained successor never boots. Those declare
/// this and run alone inside the lane.
pub fn declares_worker_isolated(env_pairs: &[(String, String)]) -> bool {
    env_pairs
        .iter()
        .any(|(k, v)| k == "SIM_WORKER_ISOLATED" && v != "0")
}

fn writes_live(buffered: bool) -> bool {
    !buffered
}

/// `@sim_env` keys the runner owns per entry, so they are NOT part of the boot
/// signature two entries must agree on to share a process.
///
/// `FRAME_COUNT` is the entry's runaway bound, carried in the entry plan and
/// republished by `sim_start_entry` at every boundary. `SIM_DT` travels the
/// same way, with an explicit empty value for an entry that declares nothing
/// so the engine default is re-applied rather than a predecessor's value.
/// `STONE_FX_OFF` travels like `SIM_DT`: every consumer is a per-frame gate
/// (`fx_disabled`), so an entry that ablates the world draw or the screen UI
/// shares a boot with one that does not. `SIM_SKIP_RENDER` is ALSO carried
/// per entry (`runner_owned_plan_pairs`) and re-applied at every boundary, and
/// it stays boot signature conservatively: the asset stub no longer turns on it
/// (every process is stubbed), but nothing has shown a rendering and a
/// non-rendering entry to be interchangeable in one boot, and the render-lane
/// throttle schedules whole lanes by it.
///
/// `STONE_WINDOW` travels like `STONE_FX_OFF`, with an empty value standing for
/// the size the game asks for: a worker boots at its first entry's window and
/// `sim_start_entry` sizes the window for each entry, re-deriving what boot
/// derives from it (`sim_apply_window`).
///
/// `ROCK_MASK_WALL_CLOCK` and `ROCK_DETERMINISTIC_IDS` are owned outright: the
/// runner publishes its own value for both after `preset_env`, so a declared one
/// never reaches the child and cannot stand for a difference between two
/// entries. Splitting a group on an inert value would isolate sims for nothing.
const RUNNER_OWNED_ENV_KEYS: &[&str] =
    &["FRAME_COUNT", "ROCK_DETERMINISTIC_IDS", "ROCK_MASK_WALL_CLOCK", "SIM_DT", "SIM_MANUAL", "STONE_FX_OFF", "STONE_WINDOW"];

/// Fixed values the runner publishes to EVERY sim child, layered BEFORE
/// `preset_env` so a declaration still wins.
///
/// One source, read by the spawn that applies them and by `boot_signature`
/// which discounts them: a sim that restates one of these gets a byte-identical
/// child environment to a sim that declares nothing, so the pair cannot stand
/// for a difference between two entries. Splitting a group on it isolates sims
/// for nothing -- the `ROCK_MASK_WALL_CLOCK` argument above, for a key whose
/// DECLARED VALUE happens to equal the baseline.
///
/// Matched as (key, value) and not by key, which is the whole point: a
/// different value is a real boot difference. Putting the key in
/// `RUNNER_OWNED_ENV_KEYS` instead would silently run a seed-7 sim in a
/// seed-42 process.
///
/// Only fixed literals belong here. `STONE_ASSET_STUB` and `STONE_AUDIO_STUB`
/// are computed from the group's own composition, so discounting them would be
/// circular -- the value depends on the grouping this decides.
///
/// `STONE_IMAGE_MAX_SIDE` holds every loaded image's GL storage to this many
/// texels on the longer side while its LOGICAL size stays the file's own, so
/// frame rects, sheet indices and UVs are unchanged and only sampling
/// resolution drops. A rendering mag worker holds 3.2 GB of RGBA8 texture
/// pixels without it, which is what stops a second suite fitting in this
/// machine's memory (`docs/sim-audits-2026-09-11/headless-vram-report.md`). A
/// sim whose assertion genuinely needs the file's own texels declares
/// `@sim_env("STONE_IMAGE_MAX_SIDE", "0")`, which is a real boot difference and
/// isolates it from the capped group. No production boot publishes it: `rock
/// run`, `rock frame`, `rock perf` and `rock stone` build their own child
/// environments and never read this table.
///
/// 256 is the value the author asked for, and it is reachable because the one
/// thing that refused it is now declared rather than resampled. mag's rendered
/// sims used to pass at 2048 and 1024 and fail at 512 and 256 with a Metal GPU
/// hang on the first rendered frame rather than an assertion. The cause was a
/// single asset family: the relight caster bakes are a bounding-volume
/// hierarchy packed into RGBA8, they are 768 texels wide, and any cap below
/// that resampled an index structure into garbage. They now load with
/// `exact_texels` and decline every cap, so the width of a bake no longer sets
/// the floor for this constant. See the report section dated 2026-09-12 in the
/// file above.
const RUNNER_BASELINE_ENV: &[(&str, &str)] = &[
    ("ROCK_TEST_SEED", "42"),
    ("SIM_SKIP_RENDER", "1"),
    ("STONE_HEADLESS", "1"),
    ("STONE_IMAGE_MAX_SIDE", "256"),
];

/// The opt-in a fixture declares to move one of its own `@sim_env` keys off the
/// boot signature and onto the per-entry channel the worker already uses for
/// `OUT`, `ROCK_RUN_SIM` and `FRAME_COUNT`: a comma-separated list of key names,
/// e.g. `@sim_env("SIM_WORKER_PER_ENTRY_ENV", "MAG_TELEMETRY_ROOT")`.
///
/// It exists because `$OUT` is per entry while a spawn environment is not, and
/// the ONLY thing that decides whether republishing a value at the entry
/// boundary is enough is whether the GAME reads that variable live or captures
/// it into a global during boot. The runner cannot see which, so it must be
/// told, and the default has to be the safe one: an undeclared `$OUT` key still
/// isolates. A blanket "all `$OUT` keys are per-entry now" would still be wrong,
/// because whether a game reads a variable live is a property of that game at
/// that revision, not of the runner. MAG's bindings/layout-history group WAS the
/// standing example of the unsafe half -- read into globals at boot, so a
/// per-entry setenv was accepted and ignored and the second entry read and wrote
/// the FIRST entry's file. Those three accessors now read the environment live
/// (`mag/actions.rock`, `mag/spellbook.rock`), witnessed by
/// `mag/tests/sims/bindings_path_per_entry.rock`, and the fifteen fixtures
/// declare the keys here. The rule that made that a fixture-side declaration
/// rather than a runner-side list is unchanged.
///
/// Reading the variable live is NECESSARY but not SUFFICIENT, and a fixture
/// declaring a key here is asserting the whole condition. The READER has to run
/// per entry as well, and whatever it configures has to be reset per entry.
/// `MAG_SCENE` is the recorded counter-example: read live, re-applied correctly
/// at every entry, and still unshareable because two pieces of what its scene
/// sets up survive the boundary (`docs/sim-boot-cost-analysis.md`, queue item 5).
///
/// Symmetric to `SIM_WORKER_ISOLATED`, which is the opt-OUT declared the same
/// way; both are properties of the fixture, stated at the fixture.
const PER_ENTRY_ENV_DECL: &str = "SIM_WORKER_PER_ENTRY_ENV";

/// What the boot signature carries in place of a per-entry key's value: the
/// PRESENCE is kept and the value dropped.
///
/// Keeping the presence is what makes the per-entry channel dense within a
/// lane, and that is the whole clearing story. `OUT` and `FRAME_COUNT` are safe
/// because every entry sets them; a sparsely published key would let an entry
/// that declares nothing inherit its predecessor's value. Because an entry that
/// does not carry the key has a DIFFERENT signature, it lands in a different
/// lane, so within one worker either every entry publishes the key or none
/// does. Nothing has to be unset at the boundary because nothing is ever stale.
const PER_ENTRY_ENV_MARK: &str = "<per-entry>";

/// A sim's `@sim_env` reduced to its effective pairs: last declaration wins,
/// in first-declaration order.
fn effective_env(env_pairs: &[(String, String)]) -> Vec<(String, String)> {
    let mut effective: Vec<(String, String)> = Vec::with_capacity(env_pairs.len());
    for (key, value) in env_pairs {
        if let Some(previous) = effective.iter().position(|(k, _)| k == key) {
            effective.remove(previous);
        }
        effective.push((key.clone(), value.clone()));
    }
    effective
}

/// The keys this entry declares as per-entry, in declaration order.
///
/// Empty names are dropped rather than rejected so a trailing comma is not a
/// hard error, but an unrecognized name is NOT an error either -- naming a key
/// the fixture does not declare simply has no effect, because the signature
/// records presence and there is nothing present to record.
fn per_entry_env_keys(env_pairs: &[(String, String)]) -> Vec<String> {
    effective_env(env_pairs)
        .into_iter()
        .find(|(k, _)| k == PER_ENTRY_ENV_DECL)
        .map(|(_, v)| {
            v.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The declared per-entry key set in canonical form: sorted, so the same set
/// shares a signature however the fixture ordered its declaration. Parsed the
/// same way `per_entry_env_keys` reads it (comma-separated, trimmed, empty
/// names dropped), then sorted and de-duplicated, so spelling variants that
/// publish the identical key set compare equal. A different SET still reads
/// differently and refuses to share.
fn canonical_per_entry_decl(value: &str) -> String {
    let mut keys: Vec<&str> = value.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
    keys.sort_unstable();
    keys.dedup();
    keys.join(",")
}

/// The part of a sim's `@sim_env` that is consumed during global init, engine
/// init and `@init` -- what a shared process cannot vary between entries.
///
/// Canonical (last-wins, then stable-sorted by key) so redundant declarations
/// do not split a group: two sims agreeing on the same effective pairs share a
/// signature however they spelled them. A different final value still refuses.
///
/// A key the fixture declared per-entry keeps its place with its value replaced
/// by `PER_ENTRY_ENV_MARK`: the value is republished at every entry boundary so
/// it is not boot state, but whether the key is published AT ALL still has to
/// split the group -- see `PER_ENTRY_ENV_MARK`.
fn boot_signature(env_pairs: &[(String, String)]) -> Vec<(String, String)> {
    let effective = effective_env(env_pairs);
    let per_entry = per_entry_env_keys(env_pairs);
    let mut sig: Vec<(String, String)> = effective
        .into_iter()
        .filter(|(k, _)| !RUNNER_OWNED_ENV_KEYS.contains(&k.as_str()))
        .filter(|(k, v)| {
            !RUNNER_BASELINE_ENV.iter().any(|(bk, bv)| *bk == k.as_str() && *bv == v.as_str())
        })
        .map(|(k, v)| {
            if per_entry.iter().any(|p| *p == k) {
                (k, PER_ENTRY_ENV_MARK.to_string())
            } else if k == PER_ENTRY_ENV_DECL {
                (k, canonical_per_entry_decl(&v))
            } else {
                (k, v)
            }
        })
        .collect();
    sig.sort_by(|a, b| a.0.cmp(&b.0));
    sig
}

/// The runner-owned pairs carried in the entry plan for EVERY entry, so that
/// `sim_start_entry` re-applies each at the boundary and an entry inherits
/// nothing from its predecessor.
///
/// `SIM_DT`, `STONE_FX_OFF` and `STONE_WINDOW` travel with an explicit empty
/// value standing for the engine default: an entry that declares nothing must
/// not run at the fixed dt, with the ablation, or in the window its predecessor
/// declared.
/// `SIM_SKIP_RENDER` travels as the value the entry
/// boots with (`entry_renders`, the one oracle): it is still boot signature,
/// so two entries in one worker agree on it, but the global is
/// `realization=runner_rebind` and the boundary restore never touches it, so
/// a fixture that exits with the flag flipped would otherwise hand that value
/// to the next entry.
fn runner_owned_plan_pairs(env: &[(String, String)]) -> Vec<(String, String)> {
    let sim_dt = effective_env(env)
        .into_iter()
        .find(|(k, _)| k == "SIM_DT")
        .map(|(_, v)| v)
        .unwrap_or_default();
    // The raw effective value, not `entry_renders`: `submit` is a third
    // value (draw phase on, GPU submission off) that the engine must see
    // as itself, while the runner classifies it as non-rendering for the
    // asset stub.
    let skip_render = effective_env(env)
        .into_iter()
        .find(|(k, _)| k == "SIM_SKIP_RENDER")
        .map(|(_, v)| v)
        .unwrap_or_else(|| "1".to_string());
    let fx_off = effective_env(env)
        .into_iter()
        .find(|(k, _)| k == "STONE_FX_OFF")
        .map(|(_, v)| v)
        .unwrap_or_default();
    let window = effective_env(env)
        .into_iter()
        .find(|(k, _)| k == "STONE_WINDOW")
        .map(|(_, v)| v)
        .unwrap_or_default();
    vec![
        ("SIM_DT".to_string(), sim_dt),
        ("SIM_SKIP_RENDER".to_string(), skip_render),
        ("STONE_FX_OFF".to_string(), fx_off),
        ("STONE_WINDOW".to_string(), window),
    ]
}

/// The pairs this part publishes through setenv when its entry starts, with
/// `$OUT` resolved against the part's OWN tempdir.
///
/// Effective values only, so a redeclared key publishes what the signature
/// recorded rather than the first spelling.
fn per_entry_env_pairs(part: &WorkerPart) -> Vec<(String, String)> {
    let keys = per_entry_env_keys(&part.env);
    if keys.is_empty() {
        return Vec::new();
    }
    let out = part.out_dir.to_string_lossy().to_string();
    effective_env(&part.env)
        .into_iter()
        .filter(|(k, _)| keys.iter().any(|p| p == k))
        .map(|(k, v)| (k, v.replace("$OUT", &out)))
        .collect()
}

/// One entry-plan line, without its newline: `<name>\t<out>\t<budget>` and a
/// flat key, value tail (see `sim_load_entries` in stone/engine/sim.rock).
fn entry_plan_line(p: &WorkerPart) -> Result<String> {
    let mut fields = vec![p.name.clone(), p.out_dir.to_string_lossy().into_owned(), p.frame_budget.to_string()];
    for (k, v) in per_entry_env_pairs(p).into_iter().chain(runner_owned_plan_pairs(&p.env)) {
        fields.push(k);
        fields.push(v);
    }
    // The worker splits on these unescaped, so one inside a field would shift
    // or split the line and kill every entry in the worker.
    if let Some(bad) = fields.iter().find(|f| f.contains(['\t', '\n', '\r'])) {
        return Err(anyhow!(
            "entry plan field {bad:?} of sim `{}` contains a tab or line break the worker cannot parse",
            p.name
        ));
    }
    Ok(fields.join("\t"))
}

/// How `rock sim` maps entries to processes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SimExecMode {
    /// One fresh process per entry: the permanent reference oracle, and what
    /// `--isolated` selects. Never inferred -- it is always the caller's word.
    Isolated,
    /// Boot-checkpoint workers for everything eligible, RENDERED SIMS INCLUDED.
    /// The explicit force/diagnostic spelling (`--workers`): it is how a
    /// rendered sim gets measured in a worker at all, which the differential
    /// and the multi-order gate both need.
    Workers,
    /// The DEFAULT: workers for every eligible entry, RENDERED INCLUDED,
    /// falling back to `Isolated` where worker mode cannot run at all.
    ///
    /// It differs from `Workers` only in what happens when the mode is
    /// unavailable -- a default degrades with a reported reason, an explicit
    /// request errors, because someone who typed `--workers` is measuring
    /// something and a silent downgrade would answer the wrong question.
    ///
    /// Rendered sims run in workers too. The evidence is `--multi-order` on the
    /// mag corpus, which compares 840 boundary-crossing entries across
    /// registry, reverse and a seeded shuffle and finds zero divergences.
    Default,
}

/// Resolve the two CLI flags. clap already rejects the combination.
pub fn exec_mode(isolated: bool, workers: bool) -> SimExecMode {
    debug_assert!(!(isolated && workers));
    match (isolated, workers) {
        (true, _) => SimExecMode::Isolated,
        (_, true) => SimExecMode::Workers,
        _ => SimExecMode::Default,
    }
}

fn sim_exec_mode_name(mode: SimExecMode) -> &'static str {
    match mode {
        SimExecMode::Isolated => "isolated",
        SimExecMode::Workers => "workers",
        SimExecMode::Default => "default",
    }
}

/// Whether an entry RENDERS, from the effective `SIM_SKIP_RENDER` value.
///
/// Read last-wins off the resolved pairs, never from the source annotations:
/// `@sim_render` is lowered to `SIM_SKIP_RENDER=0` before an explicit
/// `@sim_env` is appended (`skull_backend::collect_skull_sims`), so a sim
/// carrying both is whatever it says LAST. Classifying on the annotations would
/// disagree with the value the child actually boots with.
///
/// A sim declaring nothing does not render: draw systems are skipped by default
/// under the harness. `submit` (draw phase on, nothing submitted to the GPU)
/// is NOT rendering here: its process needs no image pixels and holds no
/// rendered frame, which is what this oracle decides (the asset stub).
fn entry_renders(env: &[(String, String)]) -> bool {
    env.iter()
        .filter(|(k, _)| k == "SIM_SKIP_RENDER")
        .next_back()
        .is_some_and(|(_, v)| v == "0")
}

/// Whether this entry's process holds every image at the file's own size:
/// real assets (`STONE_ASSET_STUB=0`) with the storage cap declined
/// (`STONE_IMAGE_MAX_SIDE=0`). Either alone is cheap -- a stub image is one
/// texel whatever the cap says, and a capped real image is at most 256 texels
/// a side -- so only the pair marks the lanes the full-asset cap counts.
///
/// Read last-wins off the resolved pairs, like `entry_renders`, over the same
/// defaults the child boots with: `default_stub` is the runner's asset-stub
/// value for this invocation and the cap default is `RUNNER_BASELINE_ENV`'s.
/// `SIM_SKIP_RENDER` is deliberately not consulted: a `submit` process with
/// full assets holds them all the same (measured 2.0 GB against 2.4-4.3 GB
/// for its rendering siblings).
fn entry_full_assets(env: &[(String, String)], default_stub: &str) -> bool {
    let last = |key: &str| env.iter().filter(|(k, _)| k == key).next_back().map(|(_, v)| v.as_str());
    let stub = last("STONE_ASSET_STUB").unwrap_or(default_stub);
    let cap_default = RUNNER_BASELINE_ENV
        .iter()
        .find(|(k, _)| *k == "STONE_IMAGE_MAX_SIDE")
        .map(|(_, v)| *v)
        .expect("the runner baseline declares the image cap");
    let cap = last("STONE_IMAGE_MAX_SIDE").unwrap_or(cap_default);
    stub == "0" && cap == "0"
}

/// `STONE_ASSET_STUB` for a sim child: headers-only assets, whether or not
/// anything in the process renders.
///
/// A rendering sim gets ONE WHITE TEXEL per image and asserts on layout,
/// silhouettes and gameplay all the same; decoding mag's sheets for it costs
/// about 6 s of CPU and 456 MB of driver allocation per rendering process, and
/// the probe over all 132 non-manual rendering sims found only 22 that read
/// texels at all (`docs/sim-audits-2026-09-11/headless-vram-report.md`, the
/// 2026-09-12 section). Those 22 and the manual texel readers declare
/// `@sim_env("STONE_ASSET_STUB", "0")`, which wins because `preset_env` is
/// layered after this.
///
/// It takes no entry at all, which is the point: the parts of a comma-separated
/// chain share one process and therefore one image cache, so a per-entry answer
/// was never available to give. A fixture's declaration is a boot-signature
/// difference, so an unstubbed entry cannot silently share a worker with a
/// stubbed one.
fn asset_stub_value() -> &'static str {
    unstub_override("STONE_ASSET_STUB")
}

/// A stub knob's value, honouring an inherited request to load for real.
///
/// The parent environment can only ever turn stubbing OFF. Letting it turn
/// stubbing ON would let a stray shell variable strip the pixels out of a
/// snapshot sim, which fails as a wrong image rather than as a missing one;
/// the other direction just makes a run slower and more real, which is exactly
/// what someone debugging a stubbed sim wants -- and it makes the whole suite
/// A/B-able without rebuilding.
fn unstub_override(var: &str) -> &'static str {
    unstub_from(std::env::var(var).ok().as_deref())
}

/// The decision itself, separated from the lookup so it is testable without
/// mutating the process environment out from under a parallel test.
fn unstub_from(inherited: Option<&str>) -> &'static str {
    match inherited {
        Some("0") => "0",
        _ => "1",
    }
}

/// Why an entry cannot join a shared worker process. The reason is REPORTED --
/// a silently isolated entry would read as a worker result.
fn worker_ineligibility(
    spec: &str,
    env_by_wire: &HashMap<String, Vec<(String, String)>>,
) -> Option<String> {
    let parts = parse_sim_chain(spec);
    // A spec with no parts has nothing to agree on; the isolated path rejects
    // it with the runner's one "empty sim name" error.
    if parts.is_empty() {
        return Some("empty sim name".to_string());
    }
    for part in &parts {
        let env = env_by_wire.get(*part).map_or(&[][..], |v| v);
        if is_reload_lane(env) {
            return Some("hot-reload lane: drives its own compile session and overlay".to_string());
        }
        // The explicit opt-out, for sims that operate process-global machinery
        // a shared worker cannot fence -- the checkpoint-machinery fixtures
        // declare it because they replace the very image the boundary restores.
        if env.iter().any(|(k, v)| k == "SIM_WORKER_ISOLATED" && v != "0") {
            return Some("declares @sim_env(SIM_WORKER_ISOLATED)".to_string());
        }
        // `$OUT` is per entry and a spawn environment is not, so an
        // interpolating key is boot state a shared worker cannot vary -- UNLESS
        // the fixture declared it per-entry, in which case the worker
        // republishes it at the boundary and the value is no longer fixed at
        // spawn. Undeclared stays isolated: see `PER_ENTRY_ENV_DECL` for why
        // the runner cannot decide this for itself.
        let per_entry = per_entry_env_keys(env);
        if let Some((k, v)) =
            env.iter().find(|(k, v)| v.contains("$OUT") && !per_entry.iter().any(|p| p == k))
        {
            return Some(format!(
                "@sim_env(\"{k}\", \"{v}\") interpolates $OUT, which resolves before the process starts"
            ));
        }
    }
    entry_boot_signature(spec, env_by_wire).err()
}

fn entry_boot_signature(
    spec: &str,
    env_by_wire: &HashMap<String, Vec<(String, String)>>,
) -> std::result::Result<Vec<(String, String)>, String> {
    let parts = parse_sim_chain(spec);
    let Some(head) = parts.first() else {
        return Err("empty sim name".to_string());
    };
    let first_sig = boot_signature(env_by_wire.get(*head).map_or(&[][..], |v| v));
    for part in &parts[1..] {
        if boot_signature(env_by_wire.get(*part).map_or(&[][..], |v| v)) != first_sig {
            return Err(format!(
                "chain parts `{head}` and `{part}` disagree on their @sim_env boot signature"
            ));
        }
    }
    Ok(first_sig)
}

fn group_specs_by_boot_signature(
    indices: impl IntoIterator<Item = usize>,
    specs: &[String],
    env_by_wire: &HashMap<String, Vec<(String, String)>>,
) -> Vec<Vec<usize>> {
    let mut groups: Vec<(Vec<(String, String)>, Vec<usize>)> = Vec::new();
    for i in indices {
        let sig = entry_boot_signature(&specs[i], env_by_wire)
            .expect("only boot-compatible entries are grouped");
        match groups.iter_mut().find(|(s, _)| *s == sig) {
            Some((_, members)) => members.push(i),
            None => groups.push((sig, vec![i])),
        }
    }
    groups.into_iter().map(|(_, members)| members).collect()
}

/// Partition entry specs for worker mode: groups of specs sharing a boot
/// signature (in first-appearance order, each group keeping registry order)
/// plus the isolated remainder, each with its reason.
///
/// Everything is carried as an INDEX into `specs`, never as the spec spelling:
/// a suite may name the same sim twice, and those are two entries that each
/// need scheduling and each own a result.
#[allow(clippy::type_complexity)]
fn group_worker_specs(
    specs: &[String],
    env_by_wire: &HashMap<String, Vec<(String, String)>>,
) -> (Vec<Vec<usize>>, Vec<(usize, String)>) {
    let mut eligible = Vec::new();
    let mut isolated: Vec<(usize, String)> = Vec::new();
    for (i, spec) in specs.iter().enumerate() {
        if let Some(reason) = worker_ineligibility(spec, env_by_wire) {
            isolated.push((i, reason));
        } else {
            eligible.push(i);
        }
    }
    (group_specs_by_boot_signature(eligible, specs, env_by_wire), isolated)
}

/// Reload workers share the lane's session, but each boot signature needs its
/// own child. Incompatible comma-chain parts use the fresh-process entry path.
fn group_reload_worker_specs(
    indices: &[usize],
    specs: &[String],
    env_by_wire: &HashMap<String, Vec<(String, String)>>,
) -> (Vec<usize>, Vec<Vec<usize>>) {
    let (alone, chained): (Vec<_>, Vec<_>) = indices.iter().copied().partition(|&i| {
        parse_sim_chain(&specs[i])
            .iter()
            .any(|p| declares_worker_isolated(env_by_wire.get(*p).map_or(&[][..], |v| v)))
            || entry_boot_signature(&specs[i], env_by_wire).is_err()
    });
    (alone, group_specs_by_boot_signature(chained, specs, env_by_wire))
}

fn entry_backend_exclusion(
    spec: &str,
    env_by_wire: &HashMap<String, Vec<(String, String)>>,
    renderer: &str,
) -> Option<String> {
    for part in parse_sim_chain(spec) {
        if let Some(required) = sim_required_backend(env_by_wire.get(part).map_or(&[][..], |v| v)) {
            if required != renderer {
                return Some(format!("requires {required} renderer (active is {renderer})"));
            }
        }
    }
    None
}

fn worker_lane_plan(
    specs: &[String],
    env_by_wire: &HashMap<String, Vec<(String, String)>>,
    renderer: &str,
    jobs: usize,
) -> serde_json::Value {
    let active_specs: Vec<String> = specs.iter()
        .filter(|spec| entry_backend_exclusion(spec, env_by_wire, renderer).is_none())
        .cloned().collect();
    let (groups, isolated) = group_worker_specs(&active_specs, env_by_wire);
    let reload_entries = isolated.iter()
        .filter(|(i, _)| entry_is_reload_lane(&active_specs[*i], env_by_wire)).count();
    // Fresh-machine dispatch view: seeded medians only, no local history, so
    // `--plan` shows the lane order a fresh checkout gets. Local history
    // still supersedes seeds at run time; lanes with no seed keep plan order.
    let estimates: Vec<_> = active_specs.iter().map(|spec| spec_schedule_estimate(spec, None)).collect();
    let costs: Vec<_> = estimates.iter().map(|estimate| estimate.0).collect();
    let lanes = allocate_worker_lanes(&groups, jobs, &costs);
    let lane_estimates: Vec<Option<u64>> = lanes.iter()
        .map(|lane| sum_estimates(lane.iter().map(|&idx| estimates[idx])).0)
        .collect();
    let (_, seed_label) = crate::sim_observability::seeded_table_summary();
    serde_json::json!({
        "selected_entries": specs.len(),
        "backend_excluded": specs.len() - active_specs.len(),
        "jobs": jobs,
        "batch_quantum": SIM_BATCH_QUANTUM,
        "block_target_ms": SIM_BLOCK_TARGET_MS,
        "block_boot_tax_ms": SIM_BLOCK_BOOT_TAX_MS,
        "count_fallback_groups": groups.iter().filter(|group| group.iter().any(|&i| costs[i].is_none())).count(),
        "signature_entry_counts": groups.iter().map(Vec::len).collect::<Vec<_>>(),
        "ordinary_entries": groups.iter().map(Vec::len).sum::<usize>(),
        "ordinary_lanes": lanes.len(),
        "isolated_entries": isolated.len() - reload_entries,
        "reload_entries": reload_entries,
        "seed_source": seed_label,
        "seeded_lanes": lane_estimates.iter().filter(|estimate| estimate.is_some()).count(),
        "lane_estimates_ms": lane_estimates,
        "lane_dispatch_order": longest_first(&lane_estimates),
    })
}

/// Count fallback when a signature has incomplete duration estimates.
/// Measured at 56 on the full MAG registry (`docs/gate-speed-plan.md` step 0).
const SIM_BATCH_QUANTUM: usize = 56;

fn worker_lane_count(group_len: usize, jobs: usize) -> usize {
    if group_len == 0 {
        return 0;
    }
    let jobs = jobs.max(1);
    let quantum = SIM_BATCH_QUANTUM;
    let full_lanes = group_len / quantum;
    let partial_lane = usize::from(group_len % quantum != 0);
    (full_lanes + partial_lane).min(jobs).max(1)
}

// Small signatures do not amortize another boot. The warm ordinary MAG boots
// in the gate-time investigation span 3-10s; charge 5s for EVERY extra boot,
// even though the pool overlaps them. Measurement must justify these priors.
const SIM_BLOCK_TINY_ENTRIES: usize = 20;
const SIM_BLOCK_TARGET_MS: u64 = 20_000;
const SIM_BLOCK_BOOT_TAX_MS: u64 = 5_000;

fn allocate_worker_lanes(groups: &[Vec<usize>], jobs: usize, estimates: &[Option<u64>]) -> Vec<Vec<usize>> {
    let mut lanes = Vec::new();
    for group in groups {
        if group.is_empty() {
            continue;
        }
        let costs: Option<Vec<u64>> = group.iter()
            .map(|&i| estimates.get(i).copied().flatten()).collect();
        if let Some(costs) = costs {
            lanes.extend(duration_worker_blocks(group, &costs, jobs.max(1)));
        } else {
            // A partial sum can hide an expensive untimed entry. Keep the
            // established count policy for the whole signature, deterministically.
            log::info!(target: "SIM", "worker block count fallback: incomplete timings for {} entries; quantum {}", group.len(), SIM_BATCH_QUANTUM);
            let blocks = worker_lane_count(group.len(), jobs);
            for block in 0..blocks {
                let lo = group.len() * block / blocks;
                let hi = group.len() * (block + 1) / blocks;
                lanes.push(group[lo..hi].to_vec());
            }
        }
    }
    lanes
}

fn duration_worker_blocks(group: &[usize], costs: &[u64], jobs: usize) -> Vec<Vec<usize>> {
    let total: u128 = costs.iter().map(|&cost| cost as u128).sum();
    let useful = total.div_ceil(SIM_BLOCK_TARGET_MS as u128)
        .min(jobs as u128).min(group.len() as u128) as usize;
    let mut best = vec![group.to_vec()];
    let mut best_cost = total;
    if group.len() <= SIM_BLOCK_TINY_ENTRIES {
        return best;
    }
    for blocks in 2..=useful {
        let mut lo = costs.iter().copied().max().unwrap_or(0) as u128;
        let mut hi = total;
        // Find the smallest feasible contiguous block capacity. Greedy packing
        // minimizes block count at a fixed capacity, so the predicate is monotone.
        while lo < hi {
            let cap = lo + (hi - lo) / 2;
            if contiguous_cost_ranges(costs, cap).len() <= blocks {
                hi = cap;
            } else {
                lo = cap + 1;
            }
        }
        let ranges = contiguous_cost_ranges(costs, lo);
        let score = lo + (ranges.len() - 1) as u128 * SIM_BLOCK_BOOT_TAX_MS as u128;
        if score < best_cost {
            best_cost = score;
            best = ranges.into_iter().map(|range| group[range].to_vec()).collect();
        }
    }
    best
}

fn contiguous_cost_ranges(costs: &[u64], cap: u128) -> Vec<std::ops::Range<usize>> {
    let mut ranges = Vec::new();
    let (mut start, mut sum) = (0, 0u128);
    for (i, &cost) in costs.iter().enumerate() {
        if i > start && sum + cost as u128 > cap {
            ranges.push(start..i);
            start = i;
            sum = 0;
        }
        sum += cost as u128;
    }
    if start < costs.len() {
        ranges.push(start..costs.len());
    }
    ranges
}

/// One queued sim body. A worker child runs these in order in ONE process;
/// the parts of a comma-chain share an `out_dir`, distinct entries do not.
#[derive(Clone)]
struct WorkerPart {
    name: String,
    out_dir: PathBuf,
    frame_budget: usize,
    env: Vec<(String, String)>,
    occurrence_id: u64,
    part_index: usize,
}

/// CPU timing accumulated by the child for one closed entry. The values are
/// optional in the envelope so retained journals from before this probe remain
/// readable. Microseconds keep the side channel integer-only and avoid
/// formatting a floating point value in the Rock runner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct EntryFrameTiming {
    frames: usize,
    draw_frames: usize,
    submitted_frames: usize,
    logic_us: u64,
    render_cpu_us: u64,
    present_us: u64,
}

impl EntryFrameTiming {
    fn from_value(value: &serde_json::Value, frame_count: Option<usize>, line: &str) -> Result<Self> {
        let number = |name: &str| -> Result<u64> {
            value
                .get(name)
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| anyhow!("entry frame timing field `{name}` is not a nonnegative integer: {line}"))
        };
        let frames = usize::try_from(number("frames")?)
            .map_err(|_| anyhow!("entry frame timing frames do not fit usize: {line}"))?;
        let draw_frames = usize::try_from(number("draw_frames")?)
            .map_err(|_| anyhow!("entry frame timing draw_frames do not fit usize: {line}"))?;
        let submitted_frames = usize::try_from(number("submitted_frames")?)
            .map_err(|_| anyhow!("entry frame timing submitted_frames do not fit usize: {line}"))?;
        if let Some(frame_count) = frame_count {
            if frames != frame_count {
                return Err(anyhow!(
                    "entry frame timing frames={frames} disagrees with frame_count={frame_count}: {line}"
                ));
            }
        }
        if draw_frames > frames {
            return Err(anyhow!(
                "entry frame timing draw_frames={draw_frames} exceeds frames={frames}: {line}"
            ));
        }
        if submitted_frames > draw_frames {
            return Err(anyhow!(
                "entry frame timing submitted_frames={submitted_frames} exceeds draw_frames={draw_frames}: {line}"
            ));
        }
        Ok(Self {
            frames,
            draw_frames,
            submitted_frames,
            logic_us: number("logic_us")?,
            render_cpu_us: number("render_cpu_us")?,
            present_us: number("present_us")?,
        })
    }

    fn value(self) -> serde_json::Value {
        serde_json::json!({
            "frames": self.frames,
            "draw_frames": self.draw_frames,
            "submitted_frames": self.submitted_frames,
            "logic_us": self.logic_us,
            "render_cpu_us": self.render_cpu_us,
            "present_us": self.present_us,
        })
    }
}

/// Native work done at a successful shared-worker entry boundary. A missing
/// value means this journal predates the probe or the entry did not close
/// cleanly. `boot_restore_us` is null when native reset made the worker
/// ineligible before restore could run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct EntryBoundaryTiming {
    native_reset_us: u64,
    teardown_us: u64,
    boot_restore_us: Option<u64>,
}

impl EntryBoundaryTiming {
    fn from_value(value: &serde_json::Value, line: &str) -> Result<Self> {
        let native_reset_us = value
            .get("native_reset_us")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| anyhow!("entry boundary timing native_reset_us is not a nonnegative integer: {line}"))?;
        let teardown_us = value
            .get("teardown_us")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| anyhow!("entry boundary timing teardown_us is not a nonnegative integer: {line}"))?;
        let restore = value
            .get("boot_restore_us")
            .ok_or_else(|| anyhow!("entry boundary timing has no boot_restore_us: {line}"))?;
        let boot_restore_us = if restore.is_null() {
            None
        } else {
            Some(restore.as_u64().ok_or_else(|| {
                anyhow!("entry boundary timing boot_restore_us is not a nonnegative integer or null: {line}")
            })?)
        };
        Ok(Self { native_reset_us, teardown_us, boot_restore_us })
    }

    fn value(self) -> serde_json::Value {
        serde_json::json!({
            "native_reset_us": self.native_reset_us,
            "teardown_us": self.teardown_us,
            "boot_restore_us": self.boot_restore_us,
        })
    }
}

/// One part's `__SIM__` and raw stderr lines, carved out of the worker's single
/// stream by the child's `entry_start`/`entry_end` envelope.
#[derive(Default)]
struct EntryStream {
    started: bool,
    /// Saw a matching `entry_end`. False for the entry a crashed or killed
    /// worker died inside.
    terminated: bool,
    /// Number of game-loop frames completed by this entry, when the child
    /// reported it on its validated envelope boundary.
    frame_count: Option<usize>,
    sim: Vec<String>,
    /// Every raw line attributed to this entry, including the boot and
    /// teardown noise that falls outside the envelope. This is the debugging
    /// stream, kept as it was before the envelope existed.
    raw_all: Vec<String>,
    /// Raw lines emitted strictly BETWEEN this entry's start and end. Boot and
    /// teardown are excluded, so the two sides of a differential see the same
    /// window and this can be compared.
    raw_in_entry: Vec<String>,
}

/// Split a worker's stderr by the per-entry envelope, validating the protocol
/// strictly.
///
/// Entries must open and close in queue order, exactly once each: a start with
/// the wrong sequence, a start while another entry is open, an end that does
/// not match the open entry's name and sequence, or a structured `__SIM__`
/// operation outside any envelope are all rejected. Attributing a stray
/// operation to a neighbour would file one sim's failures under another's name,
/// which is the one mistake this whole layer exists to prevent.
///
/// The single tolerated gap is a final entry with no `entry_end`: that is what
/// a crashed or timed-out worker leaves behind, and it is recorded as
/// `terminated == false` rather than accepted as a clean close. Raw (non-
/// `__SIM__`) lines outside the envelope are ordinary boot/teardown logging and
/// are attributed to the most recently started entry for debugging only.
fn split_worker_stream(lines: &[String], parts: &[WorkerPart]) -> Result<Vec<EntryStream>> {
    let mut out: Vec<EntryStream> = (0..parts.len()).map(|_| EntryStream::default()).collect();
    let mut tracker = EnvelopeTracker::for_parts(parts);
    // Where raw lines land while no entry is open: the entry that just closed,
    // or entry 0 before anything has started.
    let mut attribution = 0usize;

    for line in lines {
        match tracker.observe(line)? {
            EnvelopeStep::Raw { in_entry } => {
                out[attribution].raw_all.push(line.clone());
                if in_entry {
                    out[attribution].raw_in_entry.push(line.clone());
                }
            }
            EnvelopeStep::Started(seq) => {
                attribution = seq;
                out[seq].started = true;
            }
            EnvelopeStep::Ended(seq, frame_count, _, _) => {
                out[seq].terminated = true;
                out[seq].frame_count = frame_count;
            }
            EnvelopeStep::Op(live) => out[live].sim.push(line.clone()),
        }
    }

    // An entry left open is only explicable as the one the process died in, so
    // it has to be the last one that started.
    if let Some(live) = tracker.open {
        if live + 1 != tracker.next_expected
            || tracker.next_expected != out.iter().filter(|s| s.started).count()
        {
            return Err(anyhow!(
                "worker left entry {live} (`{}`) open but started later entries too",
                parts[live].name
            ));
        }
    }
    Ok(out)
}

/// The ONE interpretation of the per-entry envelope, driven a line at a time.
/// `split_worker_stream` uses it after the child exits to demux the stream,
/// and the live stderr reader uses it to decide when the wall-clock deadline
/// anchor may reset -- so a line the demux would reject can never buy a wedged
/// child more time. An unvalidated `entry_start` reset was exactly that hole:
/// a sim flooding bogus starts from inside one tick evaded the only bound
/// that can kill it.
struct EnvelopeTracker {
    names: Vec<String>,
    open: Option<usize>,
    next_expected: usize,
}

/// What one observed line meant to the envelope.
#[derive(Clone, Copy)]
enum EnvelopeStep {
    /// A valid `entry_start` for the expected next entry -- the only step the
    /// deadline anchor may reset on.
    Started(usize),
    Ended(
        usize,
        Option<usize>,
        Option<EntryFrameTiming>,
        Option<EntryBoundaryTiming>,
    ),
    /// A structured operation inside the open entry.
    Op(usize),
    /// A non-`__SIM__` line; `in_entry` says whether an envelope was open.
    Raw { in_entry: bool },
}

/// The live half of the wedged-child guard: each stderr line runs through the
/// strict envelope, and the deadline anchor may reset exactly when this says
/// so -- an accepted `entry_start`, nothing else. The first violation poisons
/// the guard, so no later line can extend the deadline either; the violation
/// itself is reported after the exit by `split_worker_stream` running the
/// same interpretation. Without the gate, a sim wedged inside one tick --
/// where the frame budget can never be evaluated -- could flood bogus starts
/// and evade the only bound able to kill it.
struct DeadlineGuard {
    envelope: Option<EnvelopeTracker>,
}

impl DeadlineGuard {
    fn for_parts(parts: &[WorkerPart]) -> Self {
        Self { envelope: Some(EnvelopeTracker::for_parts(parts)) }
    }

    /// True exactly when `line` must reset the deadline anchor.
    #[cfg(test)]
    fn observe_resets(&mut self, line: &str) -> bool {
        matches!(self.observe_boundary(line), Some(EnvelopeStep::Started(_)))
    }

    fn observe_boundary(&mut self, line: &str) -> Option<EnvelopeStep> {
        match self.envelope.as_mut().map(|t| t.observe(line)) {
            Some(Ok(step)) => Some(step),
            Some(Err(_)) => {
                self.envelope = None;
                None
            }
            None => None,
        }
    }
}

impl EnvelopeTracker {
    fn for_parts(parts: &[WorkerPart]) -> Self {
        Self {
            names: parts.iter().map(|p| p.name.clone()).collect(),
            open: None,
            next_expected: 0,
        }
    }

    fn observe(&mut self, line: &str) -> Result<EnvelopeStep> {
        let Some(rest) = line.strip_prefix("__SIM__ ") else {
            return Ok(EnvelopeStep::Raw { in_entry: self.open.is_some() });
        };
        let n = self.names.len();
        let op = extract_str_field(rest, "op").unwrap_or_default();
        match op.as_str() {
            "entry_start" => {
                let seq = envelope_seq(rest, line)?;
                // Bounds FIRST. After the last entry closes `next_expected`
                // equals the queue length, so a further start would satisfy the
                // ordering check below and then index off the end.
                if seq >= n {
                    return Err(anyhow!(
                        "worker started entry {seq} but the queue has only {n} entries: {line}"
                    ));
                }
                if let Some(live) = self.open {
                    return Err(anyhow!(
                        "worker started entry {seq} while entry {live} (`{}`) was still open: {line}",
                        self.names[live]
                    ));
                }
                if seq != self.next_expected {
                    return Err(anyhow!(
                        "worker started entry {seq} out of order (expected {}): {line}",
                        self.next_expected
                    ));
                }
                envelope_name_matches(rest, &self.names[seq], line)?;
                self.open = Some(seq);
                self.next_expected = seq + 1;
                Ok(EnvelopeStep::Started(seq))
            }
            "entry_end" => {
                let seq = envelope_seq(rest, line)?;
                let Some(live) = self.open else {
                    return Err(anyhow!("worker ended entry {seq} with no entry open: {line}"));
                };
                if seq != live {
                    return Err(anyhow!(
                        "worker ended entry {seq} while entry {live} (`{}`) was open: {line}",
                        self.names[live]
                    ));
                }
                envelope_name_matches(rest, &self.names[live], line)?;
                let frame_count = envelope_frame_count(rest, line)?;
                let frame_timing = envelope_frame_timing(rest, frame_count, line)?;
                let boundary_timing = envelope_boundary_timing(rest, line)?;
                self.open = None;
                Ok(EnvelopeStep::Ended(
                    live,
                    frame_count,
                    frame_timing,
                    boundary_timing,
                ))
            }
            _ => {
                let Some(live) = self.open else {
                    return Err(anyhow!(
                        "worker emitted a `{op}` operation outside any entry envelope: {line}"
                    ));
                };
                Ok(EnvelopeStep::Op(live))
            }
        }
    }
}

fn envelope_seq(rest: &str, line: &str) -> Result<usize> {
    extract_raw_field(rest, "seq")
        .and_then(|s| s.parse::<usize>().ok())
        .ok_or_else(|| anyhow!("worker sent an entry envelope with no usable seq: {line}"))
}

fn envelope_frame_count(rest: &str, line: &str) -> Result<Option<usize>> {
    let Some(raw) = extract_raw_field(rest, "frame_count") else {
        return Ok(None);
    };
    raw.parse::<usize>()
        .map(Some)
        .map_err(|_| anyhow!("worker sent an invalid entry frame_count: {line}"))
}

fn envelope_frame_timing(
    rest: &str,
    frame_count: Option<usize>,
    line: &str,
) -> Result<Option<EntryFrameTiming>> {
    let Some(raw) = extract_raw_field(rest, "frame_timing") else {
        return Ok(None);
    };
    let value: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|error| anyhow!("worker sent invalid entry frame_timing ({error}): {line}"))?;
    EntryFrameTiming::from_value(&value, frame_count, line).map(Some)
}

fn envelope_boundary_timing(rest: &str, line: &str) -> Result<Option<EntryBoundaryTiming>> {
    let Some(raw) = extract_raw_field(rest, "boundary_timing") else {
        return Ok(None);
    };
    let value: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|error| anyhow!("worker sent invalid entry boundary_timing ({error}): {line}"))?;
    if value.is_null() {
        return Ok(None);
    }
    EntryBoundaryTiming::from_value(&value, line).map(Some)
}

fn envelope_name_matches(rest: &str, expected: &str, line: &str) -> Result<()> {
    match extract_str_field(rest, "name") {
        Some(name) if name == expected => Ok(()),
        Some(name) => Err(anyhow!(
            "worker entry envelope names `{name}` where the queue has `{expected}`: {line}"
        )),
        None => Err(anyhow!("worker sent an entry envelope with no name: {line}")),
    }
}

/// Admission namespace used by the physical worker spawn.
pub fn sim_worker_slot_directory() -> Result<PathBuf> {
    let root = match std::env::var_os("ROCK_SIM_WORKER_SLOT_DIR") {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        Some(_) => anyhow::bail!("ROCK_SIM_WORKER_SLOT_DIR must not be empty"),
        None => {
            #[cfg(unix)]
            { PathBuf::from(format!("/tmp/rock-sim-worker-slots-{}", unsafe { libc::geteuid() })) }
            #[cfg(not(unix))]
            { std::env::temp_dir().join("rock-sim-worker-slots") }
        }
    };
    Ok(root)
}

fn record_worker_wait(label: &str) {
    let Some(path) = std::env::var_os("ROCK_SIM_WORKER_WAIT_LOG") else { return };
    let bytes = format!("wait\t{label}\n");
    if let Err(error) = append_sim_trace(Path::new(&path), bytes.as_bytes()) {
        log::warn!(target: "SIM", "cannot write worker wait trace: {error}");
    }
}

/// All callers meet at the physical spawn, including library gates and their
/// test-binary/CLI subprocesses. Kernel locks share this budget across them;
/// a harness mutex or a per-invocation -j bound cannot do that.
/// The bound is the existing CPU/RAM/GPU budget, raised to the lane count an
/// invocation forced with `--jobs-force`: a forced lane must be admitted, or
/// the planner the flag overrides would serialize it here instead.
/// `ROCK_SIM_WORKER_SLOTS` lowers that bound. The default
/// slot directory is shared by the OS user even under a fixture HOME;
/// `ROCK_SIM_WORKER_SLOT_DIR`
/// gives resource-isolated fixtures their own namespace.
fn acquire_worker_slot(label: &str) -> Result<std::fs::File> {
    let budget = sim_job_budget().max(FORCED_SIM_JOBS.load(std::sync::atomic::Ordering::Relaxed));
    let slots = match std::env::var("ROCK_SIM_WORKER_SLOTS") {
        Ok(raw) => raw.parse::<usize>().ok().filter(|n| *n > 0)
            .ok_or_else(|| anyhow!("ROCK_SIM_WORKER_SLOTS={raw:?} must be a positive integer"))?
            .min(budget),
        Err(std::env::VarError::NotPresent) => budget,
        Err(error) => return Err(anyhow!("ROCK_SIM_WORKER_SLOTS: {error}")),
    };
    let root = sim_worker_slot_directory()?;
    fs_err::create_dir_all(&root).with_context(|| format!("create worker slots {}", root.display()))?;
    let mut files = (0..slots).map(|slot| {
        let path = root.join(format!("worker-{slot}.lock"));
        // Retain inodes: unlinking a slot can strand waiters on the old lock.
        std::fs::File::options().read(true).write(true).create(true).truncate(false)
            .open(&path).with_context(|| format!("open worker slot {}", path.display()))
    }).collect::<Result<Vec<_>>>()?;
    let mut waiting = false;
    loop {
        for (index, file) in files.iter().enumerate() {
            match file.try_lock() {
                Ok(()) => {
                    return Ok(files.swap_remove(index));
                }
                Err(std::fs::TryLockError::WouldBlock) => {}
                Err(std::fs::TryLockError::Error(error)) => return Err(error).context("lock sim worker slot"),
            }
        }
        if !waiting {
            record_worker_wait(label);
            log::info!(target: "SIM", "waiting for one of {slots} host sim worker slots");
            waiting = true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

struct AdmittedWorker {
    child: Child,
    _slot: std::fs::File,
}

impl std::ops::Deref for AdmittedWorker {
    type Target = Child;
    fn deref(&self) -> &Child { &self.child }
}

impl std::ops::DerefMut for AdmittedWorker {
    fn deref_mut(&mut self) -> &mut Child { &mut self.child }
}

impl Drop for AdmittedWorker {
    fn drop(&mut self) {
        match self.child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => {}
            Err(error) => log::warn!(target: "SIM", "sim worker cleanup status: {error}"),
        }
        if let Err(error) = self.child.kill() {
            log::warn!(target: "SIM", "sim worker cleanup kill: {error}");
        }
        if let Err(error) = self.child.wait() {
            log::warn!(target: "SIM", "sim worker cleanup reap: {error}");
        }
    }
}

/// Spawn ONE luajit process that runs `parts` in order and demux its side
/// channel into one summary per part.
///
/// A single-part call is the ordinary fresh-process sim: the queue mechanism is
/// the same one, so there is no second lifecycle to keep in step. More than one
/// part means the entries share a booted process, with the child checkpointing
/// at boot and restoring that image at every entry boundary
/// (`sim_worker_restore_boot`); the native half B3 owes is what the remaining
/// differential sentinels pin.
fn run_worker(
    project: &str,
    parts: &[WorkerPart],
    quiet: bool,
    buffered: bool,
    update_snapshots: bool,
    output: &str,
    recording: bool,
    census_warmup_frames: Option<usize>,
    stone: Option<&mut crate::skull_backend::SkullStone>,
    recorder: Option<&SimRunRecorder>,
) -> Result<Vec<SimSummary>> {
    let Some(first) = parts.first() else {
        return Err(anyhow!("worker got an empty entry queue"));
    };

    // Only runner-owned values (`$OUT`, sim name, frame budget) are per entry.
    // Everything else in `@sim_env` is consumed during global init, engine init
    // and `@init`, so a shared process can only serve entries that agree on it.
    if parts.len() > 1 {
        // `os_setenv` reaches the C runtime environment. On Windows that is
        // `_putenv_s`, which Microsoft documents as operating on the
        // CRT-accessible structures -- a native reader going through the Win32
        // block directly would still see the previous entry's `$OUT`. Until
        // both views are updated AND tested, Windows keeps the fresh-process
        // path rather than running a queue that is quietly wrong.
        if cfg!(windows) {
            return Err(anyhow!(
                "worker entry queues are not available on Windows: the per-entry \
                 environment reaches only the CRT view, so a native reader using the \
                 Win32 environment block would see the previous entry's $OUT"
            ));
        }
        for p in &parts[1..] {
            if boot_signature(&p.env) != boot_signature(&first.env) {
                return Err(anyhow!(
                    "sims `{}` and `{}` have different @sim_env boot signatures and \
                     cannot share one worker process",
                    first.name,
                    p.name
                ));
            }
        }
        // The same rule `worker_ineligibility` applies at planning time, restated
        // where the process is actually spawned so a grouping bug is loud rather
        // than a queue that is quietly wrong. A key the fixture declared
        // per-entry is exempt because the child republishes it at each boundary.
        let per_entry = per_entry_env_keys(&first.env);
        if let Some((k, v)) =
            first.env.iter().find(|(k, v)| v.contains("$OUT") && !per_entry.iter().any(|p| p == k))
        {
            return Err(anyhow!(
                "@sim_env(\"{k}\", \"{v}\") is resolved before the process starts, so it \
                 cannot follow a per-entry $OUT; sims using it stay on the isolated path"
            ));
        }
    }

    // Overlay handling belongs ONLY to the reload lane. The overlay is one file
    // shared by every child of this invocation, so wiping it here would be a
    // cross-child mutation the moment ordinary sims run concurrently. Ordinary
    // children never write it, and the reload lane is exclusive and reverts its
    // own edits, so nothing stale can survive into an ordinary child.
    //
    // With STONE_HOT_RELOAD=1 the in-game watcher fires on the overlay's FIRST
    // appearance (baseline modtime 0), so a leftover would be dofile'd at
    // startup -- that is what this wipe prevents, for the lane that can create
    // one.
    if stone.is_some() {
        crate::skull_backend::wipe_overlay(
            &Path::new(output)
                .join(rock_host::project::MAIN_LUA_FILENAME)
                .with_file_name("incremental.lua"),
        )?;
    }

    // The entry queue the child reads. Lives in the invocation-owned output
    // directory and stays alive for the child's whole run.
    //
    // `<name>\t<out>\t<budget>` and then a flat key, value, key, value tail of
    // the pairs this entry publishes through setenv when it starts. Key and
    // value take a TSV field each rather than sharing one as `K=V`, so a path
    // containing `=` needs no escaping and the child needs no parser.
    let mut plan_file = tempfile::Builder::new()
        .prefix("entry-plan-")
        .suffix(".tsv")
        .tempfile_in(output)
        .context("creating the worker entry plan")?;
    for p in parts {
        writeln!(plan_file, "{}", entry_plan_line(p)?)?;
    }
    plan_file.flush()?;

    // Expand `$OUT` in env values to the entry's tempdir absolute path.
    // Lets sims write `@sim_env("GAME_SESSION", "$OUT/resume.json")`
    // and have it resolve before luajit spawns. Runtime `set_env()`
    // runs after `@init` so it can't influence env-var-driven config.
    let out_str = first.out_dir.to_string_lossy().to_string();
    let preset_env: Vec<(String, String)> = first
        .env
        .iter()
        .map(|(k, v)| (k.clone(), v.replace("$OUT", &out_str)))
        .collect();

    let reload = parts.iter().any(|p| is_reload_lane(&p.env));
    let lua_file = worker_chunk_path(Path::new(output), reload);
    let mut cmd =
        crate::headless_scope::headless_command(rock_host::project::build_luajit_command(&lua_file, &[]));
    // `ROCK_RUN_SIM` / `OUT` / `FRAME_COUNT` carry the FIRST entry: they are read
    // during boot (`sim_mode_active()` gates audio, the screenshot helper reads
    // the budget) before the queue starts, and the child re-publishes them
    // through setenv at each entry boundary.
    cmd.env("ROCK_SIM_ENTRY_PLAN", plan_file.path())
        .env("ROCK_RUN_SIM", &first.name)
        .env("FRAME_COUNT", first.frame_budget.to_string())
        .env("OUT", &first.out_dir)
        // `STONE_HEADLESS` and the deterministic sim seed. Applied from the same
        // table `boot_signature` discounts, so the baseline a sim can restate
        // for free is stated once. An explicit `@sim_env` still overrides,
        // because `preset_env` is layered after this.
        .envs(RUNNER_BASELINE_ENV.iter().copied())
        // Headers-only assets for EVERY entry, rendering included: a stubbed
        // image is one white texel, and a sim asserts layout and gameplay
        // rather than texel colour. The sims that do read texels declare
        // `@sim_env("STONE_ASSET_STUB", "0")`, which wins because `preset_env`
        // is layered after this.
        .env("STONE_ASSET_STUB", asset_stub_value())
        // Audio is stubbed for EVERY entry, not just non-rendering ones: a
        // rendered snapshot wants pixels and no sound, so `entry_renders` says
        // nothing about it. The handful of sims that assert on voices, bus
        // pulls or sound reload set `@sim_env("STONE_AUDIO_STUB", "0")`, which
        // wins because `preset_env` is layered after this.
        .env("STONE_AUDIO_STUB", unstub_override("STONE_AUDIO_STUB"))
        .envs(preset_env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        // Renders every `wall_clock_ms(..)` value as a fixed token, but ONLY
        // for a run whose records get compared: two runs of the same sim cannot
        // agree on a duration, and the records carrying one also carry facts
        // that must stay compared. An ordinary run keeps the real numbers --
        // masking them would turn the editor's timing logs into noise and would
        // change what a sim's own artifacts contain. Same flag as
        // `SimArtifacts::record_manifest`, threaded from the same source.
        //
        // Published AFTER `preset_env`, and an explicit "0" rather than an
        // absence, because this variable is the HARNESS's on both sides. A
        // fixture's `@sim_env` must not be able to unmask a compared run, and an
        // inherited value must not be able to mask an ordinary one -- and
        // `wall_clock_ms` latches it into a `static` on first read, so a
        // per-entry override could not be honoured coherently anyway.
        .env("ROCK_MASK_WALL_CLOCK", if recording { "1" } else { "0" })
        // Makes `stone-rs`'s telemetry identifiers reproducible, on the same
        // terms and for the same reason: the session id and the consent
        // generation are `Uuid::new_v4()`, so two runs of one sim wrote
        // different artifact BYTES and the isolated reference was not
        // reproducible against itself. Published from the same `recording`
        // bool, after `preset_env` and as an explicit "0", and listed in
        // `RUNNER_OWNED_ENV_KEYS` beside the mask -- a fixture must not be able
        // to unmask a compared run, and an inherited value must not be able to
        // mask an ordinary one. Telemetry is a shipping feature; an ordinary
        // run keeps its real random ids.
        //
        // A separate key rather than a second meaning for the mask: that one
        // REPLACES a rendered duration with `<ms>` in log text, this one DERIVES
        // a value that must stay a parseable UUID. See `DETERMINISTIC_IDS_ENV`
        // in `stone-rs/src/telemetry.rs`.
        .env("ROCK_DETERMINISTIC_IDS", if recording { "1" } else { "0" })
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

    // Concise (quiet) runs: quiet the child's own logger to warn+ so the
    // `[sim]` verdict lines aren't buried under [STONE]/[MAG]/[ENV] INFO
    // noise. The `__SIM__` side channel is emitted directly, not through the
    // logger, so results are unaffected; warn/error still pass through on a
    // real failure. Respect an explicit STONE_LOG from the env or a sim's
    // `@sim_env` so a debugging run can still turn logs back up.
    let stone_log_preset = preset_env.iter().any(|(k, _)| k == "STONE_LOG");
    if quiet && !stone_log_preset && std::env::var_os("STONE_LOG").is_none() {
        cmd.env("STONE_LOG", "warn");
    }

    let label = parts.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(",");
    let worker_slot = acquire_worker_slot(&label)?;
    let worker_attempt = recorder
        .map(|recorder| {
            let mut entries = Vec::new();
            for part in parts {
                if entries.last().map(|(id, _)| *id) != Some(part.occurrence_id) {
                    entries.push((part.occurrence_id, part.name.clone()));
                }
            }
            recorder.worker_spawn(
                &entries,
                &boot_signature(&first.env),
                if parts.len() > 1 { "shared" } else { "isolated" },
            )
        })
        .unwrap_or(0);
    crate::compile_slots::report_spawn_phase("execution");
    let child = match cmd.spawn() {
        Ok(child) => child,
        Err(error) => {
            if let Some(recorder) = recorder {
                recorder.worker_end(worker_attempt, "spawn_failed", None, None, false);
            }
            return Err(error.into());
        }
    };
    let mut child = AdmittedWorker { child, _slot: worker_slot };
    let process_id = child.id();
    if let Some(recorder) = recorder {
        recorder.worker_started(worker_attempt, process_id);
    }
    record_sim_lifecycle("proc_start", &label);
    for p in parts {
        record_sim_lifecycle("start", &p.name);
    }
    let stderr_handle = child.stderr.take().expect("piped");
    let stdout_handle = child.stdout.take().expect("piped");

    // Streaming reader: print `__SIM__` ops as colored progress lines as
    // they arrive, capture all lines into a shared buffer for the
    // post-run `from_stderr` parse. Non-`__SIM__` lines (game logs)
    // pass through to the parent stderr verbatim. The old DSL printed
    // progress live; without streaming, batched output appears only
    // after the child exits which is useless feedback for a 30s sim.
    let stderr_lines: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let stdout_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    // What the wall-clock deadline is measured from: the spawn until the first
    // entry opens, then each `entry_start`. The bound is a wedge detector for
    // ONE entry, so a queue of healthy entries must not accumulate against it.
    let deadline_anchor = Arc::new(Mutex::new(std::time::Instant::now()));
    // Mid-sim reload_edit ops flow stderr-thread -> wait-loop, which owns the
    // (single-threaded) SkullStone and services them between try_wait polls.
    let (reload_tx, reload_rx) = mpsc::channel::<ReloadEditReq>();
    // The production watcher sends one terminal event after it has consumed
    // the published overlay. The parent only accounts for it; it never
    // fabricates publication or completion for a test.
    let (reload_completion_tx, reload_completion_rx) = mpsc::channel::<ReloadCompletion>();

    let stderr_thread = {
        let lines = Arc::clone(&stderr_lines);
        let anchor = Arc::clone(&deadline_anchor);
        let reload_completion_tx = reload_completion_tx;
        let recorder = recorder.cloned();
        let entry_meta = parts
            .iter()
            .map(|part| (part.occurrence_id, part.name.clone(), part.part_index))
            .collect::<Vec<_>>();
        let mut first_entry_seen = false;
        let mut deadline_guard = DeadlineGuard::for_parts(parts);
        // Buffered runs must not write anything from this thread: several
        // children are live, so a direct write here interleaves into the middle
        // of another sim's locked record. The lines are retained and handed
        // back for the caller to emit inside that lock.
        let quiet = quiet || buffered;
        let buffered = buffered;
        std::thread::spawn(move || -> Option<String> {
            let reader = BufReader::new(stderr_handle);
            for line in reader.lines() {
                let line = match line {
                    Ok(line) => line,
                    Err(e) => return Some(format!("error reading game stderr: {e}")),
                };
                let boundary = deadline_guard.observe_boundary(&line);
                if matches!(boundary, Some(EnvelopeStep::Started(_))) {
                    *anchor.lock().unwrap() = std::time::Instant::now();
                }
                if let Some(recorder) = recorder.as_ref() {
                    match boundary {
                        Some(EnvelopeStep::Started(seq)) => {
                            if let Some((occurrence_id, name, part)) = entry_meta.get(seq) {
                                if !first_entry_seen {
                                    first_entry_seen = true;
                                    recorder.worker_first_entry(
                                        worker_attempt,
                                        *occurrence_id,
                                        name,
                                        *part,
                                    );
                                }
                                if let Some(context) = line.strip_prefix("__SIM__ ")
                                    .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
                                    .and_then(|event| event.get("runtime").cloned())
                                {
                                    recorder.entry_runtime_context(worker_attempt, *occurrence_id, *part, context);
                                }
                                recorder.entry_start(
                                    worker_attempt,
                                    *occurrence_id,
                                    name,
                                    *part,
                                );
                            }
                        }
                        Some(EnvelopeStep::Ended(
                            seq,
                            frame_count,
                            frame_timing,
                            boundary_timing,
                        )) => {
                            if let Some((occurrence_id, name, part)) = entry_meta.get(seq) {
                                recorder.entry_end(
                                    worker_attempt,
                                    *occurrence_id,
                                    name,
                                    *part,
                                    frame_count,
                                    frame_timing.map(EntryFrameTiming::value),
                                    boundary_timing.map(EntryBoundaryTiming::value),
                                );
                            }
                        }
                        _ => {}
                    }
                }
                if let Some(rest) = line.strip_prefix("__SIM__ ") {
                    let op = extract_str_field(rest, "op");
                    if op.as_deref() == Some("reload_edit") {
                        // Send failure = the wait loop already reaped the child;
                        // the edit can't apply, which the sim's own wait_for
                        // timeout (or the post-exit drain) surfaces.
                        let _ = reload_tx.send(ReloadEditReq {
                            request_id: extract_raw_field(rest, "request_id")
                                .and_then(|id| id.parse::<u64>().ok()),
                            file: extract_str_field(rest, "file").unwrap_or_default(),
                            find: extract_str_field(rest, "find").unwrap_or_default(),
                            replace: extract_str_field(rest, "replace").unwrap_or_default(),
                            site: extract_str_field(rest, "site").unwrap_or_default(),
                        });
                    }
                    if op.as_deref() == Some("reload_terminal") {
                        let terminal = match extract_str_field(rest, "status").as_deref() {
                            Some("applied") => Some(ReloadTerminal::Applied),
                            Some("failed") => Some(ReloadTerminal::Failed),
                            _ => None,
                        };
                        if let Some(terminal) = terminal {
                            let _ = reload_completion_tx.send(ReloadCompletion {
                                request_id: extract_raw_field(rest, "request_id")
                                    .and_then(|id| id.parse::<u64>().ok()),
                                terminal,
                            });
                        }
                    }
                    if !quiet {
                        print_progress(rest);
                    }
                } else if writes_live(buffered) {
                    // Result output, NOT logging: verbatim passthrough of
                    // the child game's own stderr (non-`__SIM__` lines) —
                    // the game's output stream, not run_sim's diagnostics.
                    eprintln!("{line}");
                }
                lines.lock().unwrap().push(line);
            }
            None
        })
    };

    let stdout_thread = {
        let buf = Arc::clone(&stdout_buf);
        let label = label.clone();
        std::thread::spawn(move || -> Option<String> {
            let mut h = stdout_handle;
            let mut local = Vec::new();
            // Surface read failures: a silent `let _ = ...` would hand
            // an empty buffer back to the post-run "game stdout for ..."
            // dump on failure, hiding any partial output we did get
            // (silent-errors audit, 2026-05-02).
            let error = h
                .read_to_end(&mut local)
                .err()
                .map(|e| format!("rock sim `{label}`: error reading game stdout: {e}"));
            *buf.lock().unwrap() = local;
            error
        })
    };

    let timeout = Duration::from_secs(sim_timeout_secs());
    let memory_cap_mb = sim_memory_cap_mb();
    let mut memory_error: Option<SiteMessage> = None;
    let mut runner_killed = false;
    let mut pristine: HashMap<String, String> = HashMap::new();
    let mut reload_errors: Vec<SiteMessage> = Vec::new();
    let mut reload_ledger = ReloadLedger::default();
    let mut stone = stone;
    let (status, timed_out) = 'child: loop {
        let mut completion_protocol_error = false;
        while let Ok(completion) = reload_completion_rx.try_recv() {
            let failed = completion.terminal == ReloadTerminal::Failed;
            if let Err(e) = consume_reload_completion(&mut reload_ledger, completion) {
                reload_errors.push(SiteMessage { message: format!("reload completion: {e}"), site: String::new() });
                completion_protocol_error = true;
            } else if failed {
                reload_errors.push(SiteMessage {
                    message: "reload watcher reported failed publication; last-good artifact remains active".into(),
                    site: String::new(),
                });
            }
        }
        if completion_protocol_error {
            runner_killed = true;
            break (terminate_reload_child(&mut child)?, false);
        }
        match child.try_wait()? {
            Some(s) => break (s, false),
            None => {
                let since_anchor = deadline_anchor.lock().unwrap().elapsed();
                if since_anchor > timeout {
                    runner_killed = true;
                    let _ = child.kill();
                    let s = child.wait()?;
                    break (s, true);
                }
                if let (Some(cap), Some(used)) = (memory_cap_mb, process_memory_mb(child.id())) {
                    if used > cap {
                        runner_killed = true;
                        let _ = child.kill();
                        let s = child.wait()?;
                        memory_error = Some(SiteMessage {
                            message: format!(
                                "sim child exceeded its memory cap of {cap} MB ({used} MB resident \
                                 plus swap) and was killed; override with ROCK_SIM_MEMORY_MB"
                            ),
                            site: String::new(),
                        });
                        break (s, false);
                    }
                }
                // Service mid-sim reload_edit requests while waiting; the 50ms
                // recv timeout doubles as the child poll interval. Disconnected
                // = the stderr stream closed before the child was reaped --
                // keep the poll cadence with a plain sleep.
                match reload_rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(req) => {
                        let request_id = match book_reload_request(&mut reload_ledger, &req) {
                            Ok(id) => id,
                            Err(e) => {
                                reload_errors.push(SiteMessage {
                                    message: format!("reload request: {e}"),
                                    site: compress_site(&req.site),
                                });
                                break 'child (terminate_reload_child(&mut child)?, false);
                            }
                        };
                        // An ordinary (non-reload) sim has no session to edit.
                        // Servicing it here would mutate build state shared with
                        // every other child, so refuse loudly rather than
                        // silently dropping the request or reaching for the
                        // session from a parallel lane.
                        let serviced = match stone.as_deref_mut() {
                            Some(st) => {
                                let mut apply = || service_reload_edit(st, output, &mut pristine, &req);
                                if let Some(recorder) = recorder {
                                    let names = parts.iter().map(|part| part.name.as_str()).collect::<Vec<_>>().join(",");
                                    recorder.phase(&format!("reload_apply:{names}"), apply)
                                } else {
                                    apply()
                                }
                            },
                            None => Err(anyhow!(
                                "reload_edit is only available to sims that opt into the \
                                 reload lane with `@sim_env(\"STONE_HOT_RELOAD\", \"1\")`"
                            )),
                        };
                        match serviced {
                            Ok(crate::skull_backend::SkullReload::Incremental { .. }) => {
                                if let Err(e) = reload_ledger.publish(request_id) {
                                    let _ = reload_ledger.reject(request_id);
                                    reload_errors.push(SiteMessage {
                                        message: format!("reload publication: {e}"),
                                        site: compress_site(&req.site),
                                    });
                                    break 'child (terminate_reload_child(&mut child)?, false);
                                }
                            }
                            Ok(other) => {
                                let e = anyhow!("reload_edit returned non-incremental result {other:?}");
                                let _ = reload_ledger.reject(request_id);
                                let site = compress_site(&req.site);
                                reload_errors.push(SiteMessage {
                                    message: format!("reload_edit: {e}"),
                                    site: site.clone(),
                                });
                                break 'child (terminate_reload_child(&mut child)?, false);
                            }
                            Err(e) => {
                                let _ = reload_ledger.reject(request_id);
                                let site = compress_site(&req.site);
                                // Stream now for a single run; buffered callers emit
                                // the retained error under their record lock.
                                if writes_live(buffered) {
                                    eprintln!(
                                        "{}{} reload_edit: {e}",
                                        format!("[{}] ", site).bright_black(),
                                        "error:".red().bold(),
                                    );
                                }
                                reload_errors.push(SiteMessage {
                                    message: format!("reload_edit: {e}"),
                                    site,
                                });
                                break 'child (terminate_reload_child(&mut child)?, false);
                            }
                        }
                        while let Ok(completion) = reload_completion_rx.try_recv() {
                            let failed = completion.terminal == ReloadTerminal::Failed;
                            if let Err(e) = consume_reload_completion(&mut reload_ledger, completion) {
                                reload_errors.push(SiteMessage { message: format!("reload completion: {e}"), site: String::new() });
                            } else if failed {
                                reload_errors.push(SiteMessage {
                                    message: "reload watcher reported failed publication; last-good artifact remains active".into(),
                                    site: String::new(),
                                });
                            }
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
            }
        }
    };
    if let Some(recorder) = recorder {
        #[cfg(unix)]
        let signal = status.signal();
        #[cfg(not(unix))]
        let signal = None;
        recorder.worker_end(
            worker_attempt,
            if timed_out {
                "timeout"
            } else if status.success() {
                "success"
            } else {
                "failure"
            },
            status.code(),
            signal,
            timed_out,
        );
    }
    // A request still queued here arrived after the child exited -- the sim
    // emitted reload_edit and stopped without waiting for the effect. That's
    // a sim-author bug; dropping it silently would hide the mis-ordering.
    while let Ok(req) = reload_rx.try_recv() {
        match book_reload_request(&mut reload_ledger, &req) {
            Ok(request_id) => {
                let _ = reload_ledger.reject(request_id);
                reload_errors.push(SiteMessage {
                    message: format!(
                        "reload_edit for `{}` arrived after the child exited (emit it before a wait_for on its effect)",
                        req.file
                    ),
                    site: compress_site(&req.site),
                });
            }
            Err(e) => reload_errors.push(SiteMessage {
                message: format!("reload request after child exit: {e}"),
                site: compress_site(&req.site),
            }),
        }
    }

    // Surface reader-thread panics: a silent JoinError here meant a panic
    // in the BufReader/Read loop would discard all sim output (the
    // __SIM__ side channel + raw game stderr) with no diagnostic, leaving
    // the user with a "0/0 expects passed" PASS line and no clue why
    // (silent-errors audit, 2026-05-02).
    let mut reader_diagnostics = Vec::new();
    match stderr_thread.join() {
        Ok(Some(message)) => reader_diagnostics.push(format!("rock sim `{label}`: {message}")),
        Ok(None) => {}
        Err(panic) => reader_diagnostics.push(format!(
            "rock sim `{label}`: stderr reader thread panicked; sim output may be incomplete: {panic:?}"
        )),
    }
    match stdout_thread.join() {
        Ok(Some(message)) => reader_diagnostics.push(message),
        Ok(None) => {}
        Err(panic) => reader_diagnostics.push(format!(
            "rock sim `{label}`: stdout reader thread panicked; game stdout may be incomplete: {panic:?}"
        )),
    }
    while let Ok(completion) = reload_completion_rx.try_recv() {
        let failed = completion.terminal == ReloadTerminal::Failed;
        if let Err(e) = consume_reload_completion(&mut reload_ledger, completion) {
            reload_errors.push(SiteMessage { message: format!("reload completion: {e}"), site: String::new() });
        } else if failed {
            reload_errors.push(SiteMessage {
                message: "reload watcher reported failed publication; last-good artifact remains active".into(),
                site: String::new(),
            });
        }
    }
    if let Err(e) = reload_ledger.validate() {
        reload_errors.push(SiteMessage { message: format!("reload ledger: {e}"), site: String::new() });
    }
    let reload_summary = reload_ledger.summary();
    if reload_summary.requests > 0 && reload_summary.pending > 0 {
        reload_errors.push(SiteMessage {
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
    for p in parts {
        record_sim_lifecycle("end", &p.name);
    }
    record_sim_lifecycle("proc_end", &label);
    if writes_live(buffered) {
        for message in &reader_diagnostics {
            log::warn!(target: "SIM", "{message}");
        }
    }

    let collected = stderr_lines.lock().unwrap().clone();
    let stdout_buf_locked = stdout_buf.lock().unwrap();
    let stdout_str = String::from_utf8_lossy(&stdout_buf_locked).to_string();
    drop(stdout_buf_locked);

    let streams = split_worker_stream(&collected, parts)?;
    // Process-level outcomes belong to the entry that was live when they
    // happened: the LAST one that started. For a single-entry worker that is
    // entry 0, exactly as before.
    let blamed = streams.iter().rposition(|s| s.started).unwrap_or(0);

    let mut summaries = Vec::with_capacity(parts.len());
    for (i, (part, stream)) in parts.iter().zip(streams.iter()).enumerate() {
        let mut summary = SimSummary::from_stderr(&stream.sim.join("\n"));
        summary.frame_count = stream.frame_count;
        summary.frame_count_parts = 1;
        summary.process_id = Some(process_id);
        if recorder.is_some() {
            summary.worker_attempt = Some(worker_attempt);
        }
        if !stream.started {
            summary.never_started = true;
            summary.errors.push(SiteMessage {
                message: format!(
                    "sim `{}` never started: the worker process ended during an earlier entry",
                    part.name
                ),
                site: String::new(),
            });
        }
        summary.entry_diagnostics =
            crate::sim_record::normalize_diagnostics(&stream.raw_in_entry, &part.out_dir);
        // A started entry with no `entry_end` is only explicable as the one a
        // crashed or killed worker died inside. Anywhere else it means the
        // envelope was lost, and a record built from a half-seen stream must
        // not read as a clean result.
        if stream.started && !stream.terminated && status.success() && !timed_out {
            summary.errors.push(SiteMessage {
                message: format!(
                    "sim `{}` never emitted its entry_end although the worker exited cleanly",
                    part.name
                ),
                site: String::new(),
            });
        }
        if i == blamed {
            summary.errors.extend(std::mem::take(&mut reload_errors));
            summary.errors.extend(memory_error.take());
            if let Some(note) = crate::headless_scope::outside_kill_note(&status, runner_killed) {
                summary.errors.push(SiteMessage { message: note, site: String::new() });
            }
            if reload_summary.requests > 0 {
                summary.reload_ledger = Some(reload_summary);
            }
            summary.timed_out = timed_out;
            if !status.success() {
                summary.process_failure_code = Some(status.code().unwrap_or(-1));
                summary.errors.push(SiteMessage {
                    message: format!("sim worker process ended with {status}; runner_killed={runner_killed}"),
                    site: String::new(),
                });
            }
        }

        match find_sim_source(project, &part.name) {
            Ok(source) => process_snapshots(&mut summary, &snap_dir_for(&source), update_snapshots),
            Err(reason) if !summary.snapshots.is_empty() || !summary.image_snapshots.is_empty() => {
                summary.errors.push(SiteMessage {
                    message: format!(
                        "could not locate .rock source for sim `{}` ({reason}) -- snapshot ops cannot resolve baseline dir",
                        part.name
                    ),
                    site: String::new(),
                });
            }
            Err(_) => {}
        }

        if buffered {
            // Same CONTENT a streaming run writes live, just held until the
            // caller owns the record lock: the child's own stderr, plus its
            // stdout when the sim failed. Without this a concurrent run
            // silently loses every game-side diagnostic that a serial run
            // prints.
            let mut diag = String::new();
            for line in &stream.raw_all {
                diag.push_str(line);
                diag.push('\n');
            }
            if i == blamed {
                for message in &reader_diagnostics {
                    diag.push_str("[SIM] warning: ");
                    diag.push_str(message);
                    diag.push('\n');
                }
                if !quiet
                    && (summary.has_failures()
                        || summary.timed_out
                        || summary.process_failure_code.is_some())
                    && !stdout_str.trim().is_empty()
                {
                    // Process-wide stream; it cannot be split per entry, so it
                    // rides with the entry the process died in.
                    diag.push_str(&format!("--- game stdout for `{}` ---\n", part.name));
                    diag.push_str(&stdout_str);
                }
            }
            summary.diagnostics = diag;
        }
        summaries.push(summary);
    }

    // Restore the session + remove the overlay even when the sim failed --
    // a revert failure is itself a loud error (it would poison every later
    // sim in the suite).
    if let Err(e) = stone
        .as_deref_mut()
        .map(|st| {
            if pristine.is_empty() {
                return revert_reload_edits(st, output, &mut pristine);
            }
            let mut restore = || revert_reload_edits(st, output, &mut pristine);
            if let Some(recorder) = recorder {
                let names = parts.iter().map(|part| part.name.as_str()).collect::<Vec<_>>().join(",");
                recorder.phase(&format!("reload_restore:{names}"), restore)
            } else {
                restore()
            }
        })
        .unwrap_or(Ok(()))
    {
        summaries[blamed].errors.push(SiteMessage {
            message: format!("reload_edit revert failed: {e}"),
            site: String::new(),
        });
    }

    if !quiet && writes_live(buffered) {
        let blamed_summary = &summaries[blamed];
        if (blamed_summary.has_failures()
            || blamed_summary.timed_out
            || blamed_summary.process_failure_code.is_some())
            && !stdout_str.trim().is_empty()
        {
            log::error!(target: "SIM", "--- game stdout for `{}` ---", parts[blamed].name);
            // Result output, NOT logging: verbatim dump of the child
            // game's captured stdout on failure — structured child
            // output, not a run_sim log record.
            eprint!("{stdout_str}");
        }
    }

    Ok(summaries)
}

/// Spawn one luajit process for `sim_part` and demux its side channel.
/// `out_path` is the shared `OUT` env value across all parts in a
/// multi-start chain.
fn run_one_sim(
    project: &str,
    sim_part: &str,
    quiet: bool,
    buffered: bool,
    update_snapshots: bool,
    out_path: &Path,
    output: &str,
    env_pairs: &[(String, String)],
    stone: Option<&mut crate::skull_backend::SkullStone>,
    census_warmup_frames: Option<usize>,
    recorder: Option<&SimRunRecorder>,
    occurrence_id: Option<u64>,
    part_index: usize,
) -> Result<SimSummary> {
    let part = WorkerPart {
        name: sim_part.to_string(),
        out_dir: out_path.to_path_buf(),
        frame_budget: frame_budget_for(sim_part, env_pairs)?,
        env: env_pairs.to_vec(),
        occurrence_id: occurrence_id.unwrap_or(0),
        part_index,
    };
    let mut summaries = run_worker(
        project,
        std::slice::from_ref(&part),
        quiet,
        buffered,
        update_snapshots,
        output,
        false,
        census_warmup_frames,
        stone,
        recorder,
    )?;
    Ok(summaries.remove(0))
}

/// Build the structured record for one finished entry.
fn record_of(name: &str, seq: usize, summary: SimSummary) -> crate::sim_record::SimRecord {
    crate::sim_record::SimRecord {
        name: name.to_string(),
        seq,
        expects_pass: summary.expects_pass,
        expects_fail: summary.expects_fail,
        errors: summary.errors,
        saw_stop: summary.saw_stop,
        timed_out: summary.timed_out,
        exit_code: summary.process_failure_code,
        diagnostics: summary.diagnostics,
        entry_diagnostics: summary.entry_diagnostics,
        artifacts: summary.artifacts,
    }
}

/// How many processes an entry queue is spread across.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Grouping {
    /// The fresh-process contract: one child per part, which is what the suite
    /// runner does today and therefore what the oracle compares against.
    PerPart,
    /// One booted child for the whole queue -- the Phase B seam under test.
    Shared,
}

/// Run `entry_specs` (each a comma-chain sharing one `$OUT`) and return a
/// record per entry. Every entry's `$OUT` stays live until its child has
/// exited and its deferred artifacts have been processed.
fn run_entries(
    project: &str,
    output: &str,
    env_by_wire: &HashMap<String, Vec<(String, String)>>,
    entry_specs: &[String],
    grouping: Grouping,
) -> Result<Vec<crate::sim_record::SimRecord>> {
    let mut artifacts = Vec::with_capacity(entry_specs.len());
    let mut part_counts = Vec::with_capacity(entry_specs.len());
    let mut parts: Vec<WorkerPart> = Vec::new();
    for spec in entry_specs {
        let entry_artifacts = SimArtifacts::recording()?;
        let chain = parse_sim_chain(spec);
        if chain.is_empty() {
            return Err(anyhow!("sim got an empty sim name"));
        }
        part_counts.push(chain.len());
        for name in chain {
            let env = env_by_wire.get(name).cloned().unwrap_or_default();
            parts.push(WorkerPart {
                name: name.to_string(),
                out_dir: entry_artifacts.path().to_path_buf(),
                frame_budget: frame_budget_for(name, &env)?,
                env,
                occurrence_id: 0,
                part_index: parts.len(),
            });
        }
        artifacts.push(entry_artifacts);
    }

    // Not quiet, but buffered: nothing is written live (several entries share
    // one stream), and every diagnostic is retained into its record instead.
    let summaries = match grouping {
        Grouping::Shared => run_worker(project, &parts, false, true, false, output, true, None, None, None)?,
        Grouping::PerPart => {
            let mut out = Vec::with_capacity(parts.len());
            for part in &parts {
                out.extend(run_worker(
                    project,
                    std::slice::from_ref(part),
                    false,
                    true,
                    false,
                    output,
                    true,
                    None,
                    None,
                    None,
                )?);
            }
            out
        }
    };

    let mut records = Vec::with_capacity(entry_specs.len());
    let mut summaries = summaries.into_iter();
    for ((seq, spec), entry_artifacts) in entry_specs.iter().enumerate().zip(artifacts) {
        let mut merged = SimSummary::default();
        for summary in summaries.by_ref().take(part_counts[seq]) {
            merged.merge(summary);
        }
        entry_artifacts.finish(&mut merged);
        records.push(record_of(spec, seq, merged));
    }
    Ok(records)
}

/// One queued entry spec (a name or a comma-chain) with its `$OUT` and parts,
/// built once so recovery rounds rerun the SAME plan against the same
/// directories.
struct SpecPlan {
    spec: String,
    artifacts: Option<SimArtifacts>,
    parts: Vec<WorkerPart>,
}

/// Run `entry_specs` in ONE boot-checkpoint worker, with failure containment:
/// when the worker dies (crash, kill, timeout), the entry it died inside keeps
/// its failure record and every UNSTARTED entry reruns in a fresh worker of
/// the same boot signature. Containment, not a state reset claim -- the
/// in-child boundary owns the reset.
///
/// The caller has already grouped `entry_specs` by boot signature;
/// `run_worker` still refuses a mixed queue, so a grouping bug is loud.
/// `stone` is the live compile session, and it is `Some` only for the exclusive
/// reload lane: those entries drive `apply_edit` mid-sim, so the worker that
/// serves them has to hold the session the edits are published through. Every
/// other lane passes `None` and cannot reload.
fn run_specs_recovering(
    project: &str,
    output: &str,
    env_by_wire: &HashMap<String, Vec<(String, String)>>,
    entry_specs: &[String],
    update_snapshots: bool,
    record_manifest: bool,
    recorder: Option<&SimRunRecorder>,
    entry_ids: Option<&[u64]>,
    mut stone: Option<&mut crate::skull_backend::SkullStone>,
) -> Result<Vec<(String, SimSummary, bool)>> {
    let mut plans = Vec::with_capacity(entry_specs.len());
    for (entry_index, spec) in entry_specs.iter().enumerate() {
        let artifacts = SimArtifacts::with_manifest(record_manifest)?;
        let chain = parse_sim_chain(spec);
        if chain.is_empty() {
            return Err(anyhow!("sim got an empty sim name"));
        }
        let occurrence_id = entry_ids
            .and_then(|ids| ids.get(entry_index))
            .copied()
            .unwrap_or(0);
        let parts = chain
            .iter()
            .enumerate()
            .map(|(part_index, name)| {
                let env = env_by_wire.get(*name).cloned().unwrap_or_default();
                Ok(WorkerPart {
                    name: name.to_string(),
                    out_dir: artifacts.path().to_path_buf(),
                    frame_budget: frame_budget_for(name, &env)?,
                    env,
                    occurrence_id: occurrence_id,
                    part_index,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        plans.push(SpecPlan { spec: spec.clone(), artifacts: Some(artifacts), parts });
    }

    // The third field is PROVENANCE: did this spec actually follow another one
    // inside the same process? Position in the queue cannot answer that, because
    // recovery reruns unstarted specs in a fresh worker where they start over at
    // offset 0. Only the round that produced the record knows.
    let mut done: Vec<(String, SimSummary, bool)> = Vec::new();
    let mut base = 0usize;
    let mut recovery_round = 0usize;
    while base < plans.len() {
        let parts: Vec<WorkerPart> =
            plans[base..].iter().flat_map(|p| p.parts.iter().cloned()).collect();
        if base > 0 {
            println!(
                "[sim] worker: rebooting for {} unstarted entr{}",
                plans.len() - base,
                if plans.len() - base == 1 { "y" } else { "ies" }
            );
        }
        let summaries =
            run_worker(
                project,
                &parts,
                false,
                true,
                update_snapshots,
                output,
                record_manifest,
                None,
                stone.as_deref_mut(),
                recorder,
            )?;

        // Reassemble per spec and decide how far this round got. Specs whose
        // parts all started are final; the first spec with an unstarted part is
        // where the process died IF any of its parts started (a half-run chain
        // keeps its failure record). A fully-unstarted first spec means the
        // worker died during boot: its record carries the process failure, so
        // it is final too -- which is also the progress guarantee, since every
        // round finalizes at least one spec.
        let mut merged: Vec<(SimSummary, bool)> = Vec::new();
        let mut it = summaries.into_iter();
        for plan in &plans[base..] {
            let part_summaries: Vec<SimSummary> = it.by_ref().take(plan.parts.len()).collect();
            let any_unstarted = part_summaries.iter().any(|s| s.never_started);
            let any_started = part_summaries.iter().any(|s| !s.never_started);
            let mut m = SimSummary::default();
            for s in part_summaries {
                m.merge(s);
            }
            merged.push((m, any_unstarted && !any_started));
            if any_unstarted {
                break;
            }
        }

        let finals = match merged.iter().position(|(m, _)| m.never_started) {
            None => merged.len(),
            // Died inside a started spec (or during boot for spec 0): that
            // spec's record is final. A fully-unstarted later spec reruns.
            Some(i) if merged[i].1 && i > 0 => i,
            Some(i) => i + 1,
        };
        debug_assert!(finals >= 1, "a recovery round must finalize at least one spec");

        if finals < plans.len() - base {
            let interrupted = merged
                .iter()
                .position(|(summary, _)| summary.never_started)
                .unwrap_or_else(|| merged.len().saturating_sub(1));
            let responsible_offset = if merged[interrupted].1 && interrupted > 0 {
                interrupted - 1
            } else {
                interrupted
            };
            let responsible = &merged[responsible_offset].0;
            let responsible_plan = &plans[base + responsible_offset];
            let responsible_occurrence_id = responsible_plan
                .parts
                .first()
                .map(|part| part.occurrence_id);
            let successors = plans[base + finals..]
                .iter()
                .map(|plan| {
                    (
                        plan.parts.first().map(|part| part.occurrence_id).unwrap_or(0),
                        plan.spec.clone(),
                    )
                })
                .collect::<Vec<_>>();
            recovery_round += 1;
            if let Some(recorder) = recorder {
                recorder.recovery(
                    recovery_round,
                    &recovery_cause(responsible),
                    responsible.worker_attempt,
                    responsible_occurrence_id,
                    &successors,
                );
            }
        }

        for (offset, (mut summary, _)) in merged.into_iter().take(finals).enumerate() {
            let plan = &mut plans[base + offset];
            // An entry the native reset boundary could not return ENDS its
            // worker, but its own record stands: it ran to completion inside a
            // closed envelope, and it followed a predecessor, so it is real
            // boundary evidence. Rerunning it would execute it twice and repeat
            // whatever external side effects it had. Only its unstarted
            // successors need a fresh worker, which the recovery loop already
            // does for them. The reason is REPORTED so the entry is not a
            // silently slower one.
            let provenance = offset > 0;
            if let Some(reason) = summary.worker_ineligible.as_deref() {
                eprintln!(
                    "{} `{}` ended its worker: {}",
                    "[sim]".yellow(),
                    plan.spec,
                    reason
                );
            }
            let artifacts = plan
                .artifacts
                .take()
                .expect("a spec is finalized exactly once");
            let occurrence_id = plan.parts.first().map(|part| part.occurrence_id).unwrap_or(0);
            if let Some(recorder) = recorder {
                recorder.artifact_check_start(occurrence_id);
            }
            artifacts.finish(&mut summary);
            if let Some(recorder) = recorder {
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
            done.push((plan.spec.clone(), summary, provenance));
        }
        base += finals;
    }
    Ok(done)
}

fn recovery_cause(summary: &SimSummary) -> String {
    if let Some(reason) = summary.worker_ineligible.as_deref() {
        return reason.to_string();
    }
    if summary.timed_out {
        return "worker timed out".to_string();
    }
    if let Some(code) = summary.process_failure_code {
        return format!("worker process failed (exit_code={code})");
    }
    summary
        .errors
        .first()
        .map(|error| error.message.clone())
        .unwrap_or_else(|| "worker ended before successor entries".to_string())
}

/// Test-facing surface for the recovering worker queue: compile, run
/// `entry_specs` in one worker (with recovery), and return a structured record
/// per spec, artifact manifest included.
///
/// The flag beside each record is PROVENANCE: true when that spec actually
/// followed another one inside the same process, and so is the only kind of
/// record that can be evidence about a restore boundary. Recovery makes this
/// unanswerable from queue position, since a rerun spec starts its fresh worker
/// with no predecessor.
/// The shuffled arm's permutation. Deterministic by construction: an order that
/// varies per run makes a divergence unreproducible and a green run unrepeatable,
/// which is the opposite of what a gate is for. One fixed seed, Fisher-Yates.
fn fixed_shuffle(n: usize) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..n).collect();
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    for i in (1..n).rev() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        idx.swap(i, (state % (i as u64 + 1)) as usize);
    }
    idx
}

/// Run `ordered` the way `--workers` GROUPS it -- eligible specs grouped by
/// boot signature into shared recovering workers, the remainder isolated -- and
/// return a record per spec, aligned to `ordered`.
///
/// Deliberately one worker per signature, where `--workers` splits a group into
/// `--jobs` lanes (B5): a lane split can only reduce how many entries actually
/// follow a predecessor in one process, and that population is this gate's
/// denominator. Nothing about what a worker does at a boundary differs between
/// the two.
///
/// Results are carried by INDEX, never by spec spelling: a corpus may name the
/// same sim twice and those are two entries with two results.
fn run_worker_arm(
    project: &str,
    output: &str,
    env_by_wire: &HashMap<String, Vec<(String, String)>>,
    ordered: &[String],
) -> Result<Vec<(crate::sim_record::SimRecord, bool)>> {
    let (groups, isolated) = group_worker_specs(ordered, env_by_wire);
    let mut out: Vec<Option<(crate::sim_record::SimRecord, bool)>> = vec![None; ordered.len()];

    for members in groups {
        let specs: Vec<String> = members.iter().map(|&i| ordered[i].clone()).collect();
        let results = run_specs_recovering(project, output, env_by_wire, &specs, false, true, None, None, None)?;
        if results.len() != members.len() {
            return Err(anyhow!(
                "worker group returned {} results for {} specs",
                results.len(),
                members.len()
            ));
        }
        for (k, (spec, summary, crossed)) in results.into_iter().enumerate() {
            out[members[k]] = Some((record_of(&spec, members[k], summary), crossed));
        }
    }
    // Ineligible entries run in a fresh process, so they cross no boundary --
    // they are still COMPARED (a difference here is a real one), they just
    // cannot be evidence that a restore put the world back.
    for (i, reason) in isolated {
        println!("[multi-order]   isolated: {} -- {reason}", ordered[i]);
        let mut records = run_entries(
            project,
            output,
            env_by_wire,
            std::slice::from_ref(&ordered[i]),
            Grouping::PerPart,
        )?;
        out[i] = Some((records.remove(0), false));
    }

    out.into_iter()
        .enumerate()
        .map(|(i, r)| r.ok_or_else(|| anyhow!("sim `{}` was never scheduled", ordered[i])))
        .collect()
}

/// The Phase B4 corpus gate (`docs/sim-speed-plan.md`): every sim's worker
/// result must equal its own fresh-process reference, under three different
/// predecessor assignments.
///
/// Three orders and not one because the class this gate exists to catch is
/// leakage FROM a predecessor: in registry order a given sim always follows the
/// same neighbour, so a leak between two sims that happen to agree would never
/// be exercised. Reverse and a fixed shuffle give each sim different
/// predecessors without giving up reproducibility.
///
/// The reference is per SIM, taken once in one fresh process each, which is the
/// permanent `--isolated` oracle -- not a second worker arm, and not the
/// previous arm's result.
/// Arms no corpus in this tree can populate, so an empty one is the documented
/// permanent state rather than a hole the gate should refuse over.
///
/// `errors` counts runner/sim `error` ops, and every op that emits one also
/// FAILS its sim -- a fixture built to populate it would be a permanently red
/// entry in `rock sim testbed`.
///
/// `expects_fail` is the same shape for the same reason: the only thing that
/// populates it is a sim whose expect FAILS, so a corpus for it is a
/// deliberately red sim kept alive to feed a gate. A healthy project has none,
/// and mag is 299/299 -- degrading the normal suite to populate one arm trades
/// a real signal for a bookkeeping one. What this arm can be wrong about is
/// covered where a failing expect is a fixture rather than a regression: the
/// targeted `sim_worker_differential` gates drive failing entries directly and
/// compare them across the boundary.
///
/// Both arms are still guarded twice over: `Comparison::population` panics on
/// an arm that is ABSENT rather than reading as "nothing to see", so an arm
/// that stops being censused at all is still a hard failure, and `sim_record`'s
/// synthetic per-arm tests fail when an arm is censused but no longer compared.
/// Encoded here so that "permanently empty" is a stated exception with a
/// reason, not a gate everyone learns to ignore.
const ARMS_WITHOUT_A_CORPUS: &[&str] = &["errors", "expects_fail"];

/// Whether an empty comparison arm should stop the gate, given which arms came
/// back empty and whether this run measured a named SUBSET.
///
/// Over the whole corpus an empty arm is a hole: the gate would be reporting a
/// green over something it never compared. Over a subset it is expected -- the
/// caller picked two sims to re-measure one class, and whether that pair
/// happens to fail an expect is unrelated to why it was picked. A subset's
/// evidence requirement is the boundary-crossing population instead, which is
/// checked separately and is what a subset can actually be wrong about.
fn population_refusal(empty: &[&str], subset: bool) -> Option<String> {
    if subset {
        return None;
    }
    let unexpected: Vec<&str> =
        empty.iter().copied().filter(|a| !ARMS_WITHOUT_A_CORPUS.contains(a)).collect();
    if unexpected.is_empty() {
        return None;
    }
    Some(format!(
        "these arms compared an empty population, so their agreement is evidence of nothing: {}",
        unexpected.join(", ")
    ))
}

/// The corpus one `--multi-order` invocation measures: the whole registry, or
/// the named subset of it in REGISTRY ORDER, which is what the "registry" arm
/// means and what makes the reverse arm its reverse.
///
/// Every refusal here is loud, because each one would otherwise silently shrink
/// the corpus and a gate that measured less than it was asked to would still
/// print a green:
///
/// - an unknown name is a typo, and dropping it would run the gate over the
///   sims that happened to match;
/// - a REPEATED name has one reference record and two worker records, so the
///   second comparison would be scored against a reference that is not its own;
/// - a `a,b` comma-chain is a different unit -- chain parts share one `$OUT`
///   and are one entry -- so it cannot name a corpus member.
fn select_gate_corpus(registry: &[String], selected: &[String]) -> Result<Vec<String>> {
    if selected.is_empty() {
        return Ok(registry.to_vec());
    }
    let mut wanted: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    let (mut unknown, mut repeated, mut chained) = (Vec::new(), Vec::new(), Vec::new());
    for name in selected {
        if name.contains(',') {
            chained.push(name.as_str());
        } else if !registry.iter().any(|r| r == name) {
            unknown.push(name.as_str());
        } else if !wanted.insert(name.as_str()) {
            repeated.push(name.as_str());
        }
    }
    if !chained.is_empty() {
        return Err(anyhow!(
            "`--multi-order` takes sim NAMES, not comma-chains: {}. A chain's parts share one \
             $OUT and run as a single entry, so a chain cannot be a member of the compared \
             corpus.",
            chained.join(", ")
        ));
    }
    if !unknown.is_empty() {
        return Err(anyhow!(
            "no sim named {} (try `rock sim --list <project>`); refusing to run this gate over \
             the names that did match",
            unknown.join(", ")
        ));
    }
    if !repeated.is_empty() {
        return Err(anyhow!(
            "{} named more than once. Each sim has ONE reference record, so a repeat would be \
             compared against a reference that is not its own.",
            repeated.join(", ")
        ));
    }
    Ok(registry.iter().filter(|r| wanted.contains(r.as_str())).cloned().collect())
}

/// `selected` empty means the whole corpus. A non-empty `selected` runs the SAME
/// four arms over just those sims, in registry order, so a divergence already
/// isolated to a predecessor/target pair can be re-measured in seconds instead
/// of re-running 262 sims. It narrows the CORPUS and nothing else: provenance,
/// the boundary-crossing denominator, the empty-arm refusal and the scheduling
/// are the whole-corpus ones, so a subset run is the same gate over less.
///
/// A subset cannot DISCOVER a divergence class the way the full corpus can --
/// it fixes which predecessors exist -- so it confirms a class already isolated
/// rather than replacing the arc checkpoint.
pub fn run_multi_order_gate(
    backend: crate::Backend,
    project: &str,
    rock_exe: &Path,
    selected: &[String],
) -> Result<()> {
    use crate::sim_record::{compare, COMPARED_ARMS};

    rock_host::project::ensure_workspace_root()?;
    let project_path = rock_host::project::resolve_project_dir(project)?;
    let project = project_path.to_string_lossy().into_owned();
    rock_host::engine_build::build_engine_libs(&project)?;

    let out_dir = sim_output_dir()?;
    let output = out_dir.path().to_string_lossy().into_owned();
    let prepared = prepare_sims_out_of_process(backend, project_path, PathBuf::from(&output), rock_exe, &[])?;
    let wire_names = prepared.wire_names;
    let env_by_wire = prepared.env_by_wire;
    // Hot-reload sims are out of the corpus, and not because they are awkward:
    // they mutate the ONE `incremental.lua` overlay, which the suite runner
    // serializes behind an exclusive lane holding the compile session. This gate
    // runs the corpus four times over, so each pass would hand them a different
    // starting overlay and the divergence measured would be the harness's, not
    // the worker's. They also never reach a worker (`worker_ineligibility`
    // classifies the reload lane), so excluding them removes nothing this gate
    // could have decided.
    //
    // Selection happens BEFORE that partition so an explicitly named reload sim
    // is reported as excluded rather than silently missing from a corpus the
    // caller wrote out by hand.
    let selected = resolve_sim_specs(&wire_names, selected)?;
    let wire_names = select_gate_corpus(&wire_names, &selected)?;
    let (wire_names, excluded): (Vec<String>, Vec<String>) = wire_names
        .into_iter()
        .partition(|name| !is_reload_lane(env_by_wire.get(name).map_or(&[][..], |v| v)));
    let n = wire_names.len();
    if n == 0 {
        return Err(anyhow!("`{project}` registered no sims, so this gate would measure nothing"));
    }
    if !excluded.is_empty() {
        println!(
            "[multi-order] excluded {} hot-reload sim(s) -- they share one overlay and never \
             reach a worker: {}",
            excluded.len(),
            excluded.join(", ")
        );
    }

    println!("[multi-order] {n} sims; reference arm (one fresh process per sim)");
    let reference = run_entries(&project, &output, &env_by_wire, &wire_names, Grouping::PerPart)?;
    let reference_at: HashMap<&str, &crate::sim_record::SimRecord> =
        wire_names.iter().map(String::as_str).zip(reference.iter()).collect();

    let arms: Vec<(&str, Vec<usize>)> = vec![
        ("registry", (0..n).collect()),
        ("reverse", (0..n).rev().collect()),
        ("shuffle", fixed_shuffle(n)),
    ];

    let mut totals: BTreeMap<&'static str, usize> =
        COMPARED_ARMS.iter().map(|a| (*a, 0usize)).collect();
    let mut diverged: Vec<String> = Vec::new();
    let (mut crossed_n, mut fresh_n) = (0usize, 0usize);

    for (label, order) in &arms {
        println!("[multi-order] {label} arm");
        let ordered: Vec<String> = order.iter().map(|&i| wire_names[i].clone()).collect();
        let records = run_worker_arm(&project, &output, &env_by_wire, &ordered)?;
        for (pos, (record, crossed)) in records.iter().enumerate() {
            let name = &ordered[pos];
            let Some(expected) = reference_at.get(name.as_str()) else {
                return Err(anyhow!("`{name}` has no reference record"));
            };
            let c = compare(expected, record);
            // EVERY comparison decides divergence -- a difference is a
            // difference wherever it appears. Only a boundary-crossing entry
            // feeds the DENOMINATOR: an entry that ran first in its process, or
            // ran isolated, never had a predecessor's world to inherit, so its
            // agreement with a fresh process is agreement between two fresh
            // processes and cannot certify that a restore works.
            if *crossed {
                crossed_n += 1;
                for a in COMPARED_ARMS {
                    *totals.get_mut(a).expect("totals is keyed by COMPARED_ARMS") +=
                        c.population(a);
                }
            } else {
                fresh_n += 1;
            }
            if !c.is_equivalent() {
                diverged.push(format!("[{label}] {name}\n{}", c.report()));
            }
        }
    }
    println!(
        "[multi-order] {crossed_n} comparisons followed a predecessor in one process and are \
         the evidence; {fresh_n} ran first in their process or isolated and are compared but \
         not counted"
    );
    if crossed_n == 0 {
        return Err(anyhow!(
            "no entry followed a predecessor in one process, so nothing here is evidence \
             about a restore boundary"
        ));
    }

    // A comparison that found nothing is only evidence when each arm had
    // something to compare. `verdict`, `saw_stop`, `timed_out` and `exit_code`
    // are populated by every record by construction, so they cannot report an
    // empty corpus; the rest can, and a zero there means the gate ran over sims
    // that never produced that kind of observation.
    println!("[multi-order] compared populations across {} arms:", arms.len());
    let mut empty: Vec<&str> = Vec::new();
    for a in COMPARED_ARMS {
        let total = totals[a];
        println!("[multi-order]   {a}: {total}");
        if total == 0 {
            empty.push(a);
        }
    }

    if !diverged.is_empty() {
        for d in &diverged {
            println!("{d}");
        }
        return Err(anyhow!(
            "{} of {} comparisons diverged from the isolated reference",
            diverged.len(),
            n * arms.len()
        ));
    }
    // An empty arm is REPORTED in both forms, and what it means differs.
    //
    // Over the whole corpus it is a hole in the gate: `expects_fail` populates
    // only from sims that FAIL, so a corpus where everything passes standalone
    // cannot certify that arm, and the gate must say so rather than report a
    // green it did not measure. mag carries known-red sims and populates it;
    // testbed does not, which is why testbed validates the mechanism and mag is
    // the gate. Not a fudge factor to relax.
    //
    // Over a NAMED SUBSET it means nothing at all. The caller chose two sims to
    // re-measure one class; whether that pair happens to fail an expect or
    // write an artifact is unrelated to what they were selected for, and
    // failing on it would make an equivalent pair exit nonzero and be unusable
    // as an iteration gate. The subset's evidence requirement is the
    // boundary-crossing population, which is enforced above and is the thing a
    // subset can actually be wrong about.
    if let Some(reason) = population_refusal(&empty, !selected.is_empty()) {
        return Err(anyhow!(reason));
    }
    println!(
        "[multi-order] {} comparisons equivalent to the isolated reference",
        n * arms.len()
    );
    Ok(())
}

/// `rock_exe` compiles the project in a child, so a test harness running many
/// suites at once never holds a compiler's memory itself.
pub fn run_worker_suite(
    backend: crate::Backend,
    project: &str,
    rock_exe: &Path,
    entry_specs: &[String],
) -> Result<Vec<(crate::sim_record::SimRecord, bool)>> {
    rock_host::project::ensure_workspace_root()?;
    let project_arg = Path::new(project);
    let project_path = timed_sim_compile_phase(
        "run_worker_suite",
        "resolve_project",
        project_arg,
        None,
        || rock_host::project::resolve_project_dir(project),
    )?;
    let project = project_path.to_string_lossy().into_owned();
    timed_sim_compile_phase(
        "run_worker_suite",
        "engine_build",
        &project_path,
        None,
        || rock_host::engine_build::build_engine_libs(&project),
    )?;

    let out_dir = timed_sim_output_dir("run_worker_suite", &project_path)?;
    let output = out_dir.path().to_string_lossy().into_owned();
    let prepared = prepare_sims_out_of_process(backend, project_path.clone(), PathBuf::from(&output), rock_exe, &[])?;
    let env_by_wire = prepared.env_by_wire;

    let results = timed_sim_compile_phase(
        "run_worker_suite",
        "child_execution",
        &project_path,
        Some(Path::new(&output)),
        || run_specs_recovering(&project, &output, &env_by_wire, entry_specs, false, true, None, None, None),
    )?;
    Ok(results
        .into_iter()
        .enumerate()
        .map(|(seq, (spec, summary, crossed))| (record_of(&spec, seq, summary), crossed))
        .collect())
}

/// What to compare: `target` run alone in a fresh process against `target` run
/// after `predecessors` in one process.
pub struct DifferentialPlan {
    /// Entries the worker runs before the target. Each gets its own `$OUT`.
    pub predecessors: Vec<String>,
    /// The entry under comparison. A comma-chain runs its parts in order
    /// sharing one `$OUT`, exactly as the suite schedules it.
    pub target: String,
}

/// Run both sides of the Phase B differential and return
/// `(isolated, worker)` records for the target entry.
///
/// Both sides go through the same spawn, demux, artifact and record code, so
/// the two schemas cannot drift; the ONLY difference is how many entries share
/// the process. Compare them with [`crate::sim_record::compare`]. `rock_exe`
/// compiles the project, as in [`run_worker_suite`].
pub fn run_differential(
    backend: crate::Backend,
    project: &str,
    rock_exe: &Path,
    plan: &DifferentialPlan,
) -> Result<(crate::sim_record::SimRecord, crate::sim_record::SimRecord)> {
    rock_host::project::ensure_workspace_root()?;
    let project_arg = Path::new(project);
    let project_path = timed_sim_compile_phase(
        "run_differential",
        "resolve_project",
        project_arg,
        None,
        || rock_host::project::resolve_project_dir(project),
    )?;
    let project = project_path.to_string_lossy().into_owned();
    timed_sim_compile_phase(
        "run_differential",
        "engine_build",
        &project_path,
        None,
        || rock_host::engine_build::build_engine_libs(&project),
    )?;

    let out_dir = timed_sim_output_dir("run_differential", &project_path)?;
    let output = out_dir.path().to_string_lossy().into_owned();
    let prepared = prepare_sims_out_of_process(backend, project_path.clone(), PathBuf::from(&output), rock_exe, &[])?;
    let env_by_wire = prepared.env_by_wire;

    let isolated = timed_sim_compile_phase(
        "run_differential",
        "child_execution_isolated",
        &project_path,
        Some(Path::new(&output)),
        || {
            run_entries(
                &project,
                &output,
                &env_by_wire,
                std::slice::from_ref(&plan.target),
                Grouping::PerPart,
            )
        },
    )?
    .remove(0);

    let mut worker_specs = plan.predecessors.clone();
    worker_specs.push(plan.target.clone());
    let mut worker = timed_sim_compile_phase(
        "run_differential",
        "child_execution_worker",
        &project_path,
        Some(Path::new(&output)),
        || run_entries(&project, &output, &env_by_wire, &worker_specs, Grouping::Shared),
    )?;
    let worker = worker.remove(worker.len() - 1);

    Ok((isolated, worker))
}

/// Compress a `@call_site` path to `<stem>:<line>` for display — the
/// directory is noise and every sim file ends in `.rock`. Drops the
/// workspace prefix implicitly by taking only the basename.
fn compress_site(path: &str) -> String {
    let (file_part, line_part) = match path.rsplit_once(':') {
        Some((f, l)) => (f, Some(l)),
        None => (path, None),
    };
    let basename = file_part.rsplit('/').next().unwrap_or(file_part);
    let stem = basename.strip_suffix(".rock").unwrap_or(basename);
    match line_part {
        Some(l) => format!("{stem}:{l}"),
        None => stem.to_string(),
    }
}

/// Render one `__SIM__` op as a colored progress line, mirroring the
/// `[file:line] op args` shape of the legacy `.sim` runner so muscle
/// memory carries over. Unknown ops are silently dropped — this is
/// progress display, not a parser. Heavy fields (snapshot value,
/// image_snapshot path) are summarized rather than dumped.
fn print_progress(rest: &str) {
    let op = extract_str_field(rest, "op").unwrap_or_default();
    let site_raw = extract_str_field(rest, "site").unwrap_or_default();
    let site = compress_site(&site_raw);
    let prefix = if site.is_empty() {
        String::new()
    } else {
        format!("[{}] ", site).bright_black().to_string()
    };
    let body = match op.as_str() {
        "start_game" => {
            let n = extract_str_field(rest, "name").unwrap_or_default();
            format!("{} {}", "start".cyan(), n.yellow())
        }
        "set_env" => {
            let k = extract_str_field(rest, "key").unwrap_or_default();
            let v = extract_str_field(rest, "value").unwrap_or_default();
            format!("{} {}={}", "env".cyan(), k.yellow(), v)
        }
        "key" => {
            let n = extract_str_field(rest, "name").unwrap_or_default();
            format!("{} {}", "key".cyan(), n.yellow())
        }
        "click" => {
            let x = extract_raw_field(rest, "x").unwrap_or_default();
            let y = extract_raw_field(rest, "y").unwrap_or_default();
            format!("{} {} {}", "click".cyan(), x, y)
        }
        "custom" => {
            let n = extract_str_field(rest, "name").unwrap_or_default();
            let a = extract_raw_field(rest, "args").unwrap_or_default();
            format!("{} {} {}", "custom".cyan(), n.bright_cyan(), a)
        }
        "expect_ok" => {
            let m = extract_str_field(rest, "message").unwrap_or_default();
            format!("{} {}  {}", "expect".cyan(), m, "OK".green().bold())
        }
        "expect_fail" => {
            let m = extract_str_field(rest, "message").unwrap_or_default();
            let head = format!("{} {}  {}", "expect".cyan(), m, "FAIL".red().bold());
            match (extract_str_field(rest, "actual"), extract_str_field(rest, "expected")) {
                (Some(a), Some(e)) => format!(
                    "{head}\n        {} {}\n        {} {}",
                    "actual:  ".red().bold(),
                    a.red(),
                    "expected:".green().bold(),
                    e.green(),
                ),
                _ => head,
            }
        }
        "snapshot" => {
            let n = extract_str_field(rest, "name").unwrap_or_default();
            format!("{} {}", "snapshot".cyan(), n.yellow())
        }
        "image_snapshot" => {
            let n = extract_str_field(rest, "name").unwrap_or_default();
            format!("{} {}", "image_snapshot".cyan(), n.yellow())
        }
        "expect_video" => {
            let p = extract_str_field(rest, "path").unwrap_or_default();
            let f = extract_str_field(rest, "field").unwrap_or_default();
            let cmp = extract_str_field(rest, "cmp").unwrap_or_default();
            let v = extract_raw_field(rest, "value").unwrap_or_default();
            format!("{} {} {} {} {}", "expect_video".cyan(), p, f, cmp, v)
        }
        "expect_videos_match" => {
            let a = extract_str_field(rest, "a").unwrap_or_default();
            let b = extract_str_field(rest, "b").unwrap_or_default();
            format!("{} {} {}", "expect_videos_match".cyan(), a, b)
        }
        "cleanup_path" => {
            let p = extract_str_field(rest, "path").unwrap_or_default();
            format!("{} {}", "cleanup_path".cyan(), p)
        }
        "reload_edit" => {
            let f = extract_str_field(rest, "file").unwrap_or_default();
            format!("{} {}", "reload_edit".cyan(), f.yellow())
        }
        "log" => {
            let m = extract_str_field(rest, "message").unwrap_or_default();
            format!("{} {}", "log".cyan(), m)
        }
        "error" => {
            let m = extract_str_field(rest, "message").unwrap_or_default();
            format!("{} {}", "error:".red().bold(), m)
        }
        "stop" => "stop".cyan().to_string(),
        _ => return,
    };
    // Carve-out (logging-architecture.md decision 2): the live
    // `[file:line] op args` progress/expect stream is the sim's
    // result surface (PASS/FAIL/expect lines consumed by you/CI),
    // not a log record — stays a direct, colored stderr write.
    eprintln!("{prefix}{body}");
}

/// Whether `src` declares an `@sim` coroutine named `sim_name`.
/// Matches the current decl shape `co fn <sim_name>(`.
fn src_declares_sim(src: &str, sim_name: &str) -> bool {
    let needle = format!("co fn {sim_name}(");
    src.contains(&needle)
}

/// Locate the .rock file containing the `@sim` decl for `sim_name` by
/// scanning `<project>/tests/sims/*.rock`. A v1 wire name is a DefKey, and the
/// source declares only its last segment; two modules may declare that segment
/// in different files, which this scan cannot tell apart, so that is an error.
fn find_sim_source(project: &str, sim_name: &str) -> Result<PathBuf, String> {
    let declared = sim_name.rsplit("::").next().unwrap_or(sim_name);
    let sims_dir = PathBuf::from(project).join("tests").join("sims");
    let entries = std::fs::read_dir(&sims_dir)
        .map_err(|error| format!("cannot read {}: {error}", sims_dir.display()))?;
    let mut found = Vec::new();
    for ent in entries.flatten() {
        let path = ent.path();
        if path.extension().and_then(|e| e.to_str()) != Some("rock") {
            continue;
        }
        let Ok(src) = std::fs::read_to_string(&path) else {
            continue;
        };
        if src_declares_sim(&src, declared) {
            found.push(path);
        }
    }
    found.sort();
    match found.len() {
        0 => Err(format!("no file in {} declares `co fn {declared}(`", sims_dir.display())),
        1 => Ok(found.remove(0)),
        _ => Err(format!(
            "`co fn {declared}(` is declared in {}",
            found.iter().map(|path| path.display().to_string()).collect::<Vec<_>>().join(", ")
        )),
    }
}

/// `foo/bar/baz.rock` -> `foo/bar/baz.snap`. Carries over the .sim
/// runner's baseline layout (`<sim_source>.snap/<name>.{json,png}`)
/// across the migration.
fn snap_dir_for(source: &Path) -> PathBuf {
    let stem = source.file_stem().unwrap_or_default();
    let parent = source.parent().unwrap_or_else(|| Path::new("."));
    let mut name = stem.to_os_string();
    name.push(".snap");
    parent.join(name)
}

/// Walk collected snapshot ops, write or compare against baselines,
/// record results back into the summary as expect_ok/expect_fail.
fn process_snapshots(summary: &mut SimSummary, snap_dir: &Path, update: bool) {
    // Post-run snapshot resolutions are NOT printed by the live stream
    // (the stream only announces the op, not its diff result), so we
    // pass an empty site -- that's the signal `SimSummary::print` uses
    // to know "synthesized after run; please print." The original site
    // is folded into the message so the user still sees where it came
    // from. Fixed 2026-05-11 after the print() guard introduced in
    // 2c78c5cef accidentally suppressed every snapshot/image_snapshot
    // diff failure.
    for snap in std::mem::take(&mut summary.snapshots) {
        let baseline = snap_dir.join(format!("{}.json", snap.name));
        let site = snap.site.clone();
        match handle_json_snapshot(&snap, &baseline, update) {
            Ok(SnapResult::Pass) => summary.expects_pass.push(SiteMessage {
                message: prefix_with_site(&site, &format!("snapshot `{}` matches baseline", snap.name)),
                site: String::new(),
            }),
            Ok(SnapResult::Wrote) => summary.expects_pass.push(SiteMessage {
                message: prefix_with_site(&site, &format!("snapshot `{}` baseline written", snap.name)),
                site: String::new(),
            }),
            Ok(SnapResult::Fail(msg)) => summary.expects_fail.push(SiteMessage {
                message: prefix_with_site(&site, &msg),
                site: String::new(),
            }),
            Err(e) => summary.errors.push(SiteMessage {
                message: prefix_with_site(&site, &format!("snapshot `{}`: {}", snap.name, e)),
                site: String::new(),
            }),
        }
    }
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
                message: prefix_with_site(&site, &format!("image_snapshot `{}` within tolerance", img.name)),
                site: String::new(),
            }),
            Ok(SnapResult::Wrote) => summary.expects_pass.push(SiteMessage {
                message: prefix_with_site(&site, &format!("image_snapshot `{}` baseline written", img.name)),
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
        // Best-effort cleanup of the temp file.
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

fn process_video_assertions(summary: &mut SimSummary) {
    for v in std::mem::take(&mut summary.video_assertions) {
        match eval_video_assertion(&v) {
            Ok(true) => summary.expects_pass.push(SiteMessage {
                message: format!("expect_video {} {} {} {}", v.path, v.field, v.cmp, v.value_json),
                site: String::new(),
            }),
            Ok(false) => summary.expects_fail.push(SiteMessage {
                message: format!("expect_video {} {} {} {}", v.path, v.field, v.cmp, v.value_json),
                site: String::new(),
            }),
            Err(e) => summary.errors.push(SiteMessage {
                message: format!("expect_video `{}` failed: {e}", v.path),
                site: String::new(),
            }),
        }
    }
    for m in std::mem::take(&mut summary.video_match_assertions) {
        match crate::image_video::ffmpeg_ssim(&m.a, &m.b) {
            Ok(score) if score >= m.min_sim => summary.expects_pass.push(SiteMessage {
                message: format!(
                    "expect_videos_match {} {} similarity>={:.4} (got {:.4})",
                    m.a, m.b, m.min_sim, score
                ),
                site: String::new(),
            }),
            Ok(score) => summary.expects_fail.push(SiteMessage {
                message: format!(
                    "expect_videos_match {} {} similarity>={:.4} (got {:.4})",
                    m.a, m.b, m.min_sim, score
                ),
                site: String::new(),
            }),
            Err(e) => summary.errors.push(SiteMessage {
                message: format!("expect_videos_match `{}` vs `{}`: {e}", m.a, m.b),
                site: String::new(),
            }),
        }
    }
}

fn eval_video_assertion(v: &VideoAssertion) -> Result<bool> {
    let meta = crate::image_video::ffprobe_meta(&v.path)?;
    let actual = meta
        .get(&v.field)
        .ok_or_else(|| anyhow!("ffprobe yielded no `{}` for `{}`", v.field, v.path))?;
    let expected: serde_json::Value = serde_json::from_str(&v.value_json)
        .map_err(|e| anyhow!("expect_video value `{}` is not valid JSON: {e}", v.value_json))?;

    // String field: codec. Numeric field: duration, width, height,
    // nb_frames. cmp is one of `==`, `!=`, `>`, `>=`, `<`, `<=`, `~=`.
    if let (Some(av), Some(ev)) = (actual.as_str(), expected.as_str()) {
        return Ok(match v.cmp.as_str() {
            "==" | "~=" => av == ev,
            "!=" => av != ev,
            _ => false,
        });
    }
    let an = actual.as_f64().ok_or_else(|| {
        anyhow!("ffprobe field `{}` for `{}` is not numeric", v.field, v.path)
    })?;
    let en = expected
        .as_f64()
        .ok_or_else(|| anyhow!("expect_video value for `{}` is not numeric", v.field))?;
    Ok(match v.cmp.as_str() {
        "==" => (an - en).abs() < f64::EPSILON,
        "!=" => (an - en).abs() >= f64::EPSILON,
        ">" => an > en,
        ">=" => an >= en,
        "<" => an < en,
        "<=" => an <= en,
        // 1% relative + 1e-3 absolute floor; mirrors `.sim` runner.
        "~=" => (an - en).abs() < (0.01 * en.abs()).max(1e-3),
        _ => false,
    })
}

fn handle_json_snapshot(snap: &SnapshotOp, baseline: &Path, update: bool) -> Result<SnapResult> {
    let actual: serde_json::Value = serde_json::from_str(&snap.value_json)
        .map_err(|e| anyhow!("emitted value is not valid JSON: {e}; raw={}", snap.value_json))?;
    let actual_pretty = serde_json::to_string_pretty(&actual)? + "\n";

    if update {
        if let Some(parent) = baseline.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(baseline, &actual_pretty)?;
        return Ok(SnapResult::Wrote);
    }
    // An assertion that cannot be evaluated is not satisfied -- the same rule
    // `handle_image_snapshot` already applies. Blessing a missing baseline from
    // the current run and counting it as a passing expect proves nothing, and
    // under `--jobs` two sims sharing a snapshot name could each bless a
    // different value and both pass, with scheduling picking the winner.
    if !baseline.exists() {
        return Ok(SnapResult::Fail(format!(
            "snapshot `{}` has no baseline at `{}`; re-run with --update-snapshots to \
             create it (a missing baseline is not a pass)",
            snap.name,
            baseline.display()
        )));
    }

    let baseline_str = std::fs::read_to_string(baseline)?;
    let expected: serde_json::Value = serde_json::from_str(&baseline_str)
        .map_err(|e| anyhow!("baseline `{}` is not valid JSON: {e}", baseline.display()))?;
    if expected == actual {
        Ok(SnapResult::Pass)
    } else {
        // Write `.actual` next to baseline so the user can diff it.
        let actual_path = baseline.with_extension("json.actual");
        std::fs::write(&actual_path, &actual_pretty)?;
        Ok(SnapResult::Fail(format!(
            "snapshot `{}` mismatch (wrote {} for diff)",
            snap.name,
            actual_path.display()
        )))
    }
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
    // An assertion that cannot be evaluated is not satisfied. Bootstrapping a
    // missing golden from the current frame and counting it as a passing expect
    // meant a sim whose goldens are untracked -- `vfx_capture.snap/` is
    // gitignored as display-specific -- self-passed on any machine that had
    // never run it, proving nothing about the pixels it exists to check.
    if !baseline.exists() {
        return Ok(SnapResult::Fail(format!(
            "image_snapshot `{}` has no baseline at `{}`; re-run with --update-snapshots to \
             create it (a missing golden is not a pass)",
            img.name,
            baseline.display()
        )));
    }

    let diff = crate::image_video::image_diff(baseline, produced)
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

/// A failure or error message paired with the workspace-relative site
/// (`mag/tests/sims/foo.rock:42`) it originated at, captured via
/// `@call_site` in the sim runtime helpers. Empty `site` for messages
/// the runner synthesizes itself (e.g. process failure / timeout).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SiteMessage {
    pub message: String,
    pub site: String,
}

#[derive(Default)]
struct SimSummary {
    /// Passing expects, in emission order, with the site each came from.
    /// A bare count could not tell two runs apart when one passed a
    /// DIFFERENT assertion than the other.
    expects_pass: Vec<SiteMessage>,
    expects_fail: Vec<SiteMessage>,
    errors: Vec<SiteMessage>,
    saw_stop: bool,
    /// Number of game-loop frames completed by this entry. The count comes
    /// from the child-side envelope boundary, so it includes the final
    /// presented frame and frozen watcher frames.
    frame_count: Option<usize>,
    /// Number of worker parts represented by this summary. This distinguishes
    /// an initial empty merge from a part that ran but could not report a
    /// count, so a comma-chain never reports a misleading partial total.
    frame_count_parts: usize,
    process_id: Option<u32>,
    worker_attempt: Option<u64>,
    allocation_census: Option<AllocationCensus>,
    snapshots: Vec<SnapshotOp>,
    image_snapshots: Vec<ImageSnapshotOp>,
    video_assertions: Vec<VideoAssertion>,
    video_match_assertions: Vec<VideoMatchAssertion>,
    cleanup_paths: Vec<String>,
    timed_out: bool,
    process_failure_code: Option<i32>,
    /// Child diagnostics retained by a buffered (concurrent) run, emitted by
    /// the caller inside the record lock. Empty when the run wrote them live.
    diagnostics: String,
    /// Normalized child stderr from inside this entry's envelope. Comparable
    /// across runs, unlike `diagnostics`, which carries boot/teardown noise.
    entry_diagnostics: String,
    /// What the entry left under its `$OUT`, keyed by relative path. Captured
    /// by `SimArtifacts::finish_with` while the directory is still live.
    artifacts: std::collections::BTreeMap<String, crate::sim_record::ArtifactEntry>,
    /// The worker process died before this entry's envelope ever opened. The
    /// recovery loop reruns such entries in a fresh worker; every other caller
    /// keeps the error record this flag rides beside.
    never_started: bool,
    /// Why this entry could not run as anything but the first entry of a
    /// process: the native reset boundary found a resource it could neither
    /// destroy nor return (`stone/engine/sim_native.rock`). The recovery loop
    /// reruns the entry ALONE and reports the reason, so the record that
    /// survives is the fresh-process one.
    worker_ineligible: Option<String>,
    /// Parent-side conservation evidence for the real watcher/publication
    /// completion path. None means this entry issued no reload request.
    reload_ledger: Option<ReloadLedgerSummary>,
    /// Why this entry was excluded from execution by an admission check
    /// (e.g. requires a different renderer backend like WGPU on an active GL suite).
    excluded: Option<String>,
    /// Why this entry runs no check (`sim_no_checks`): another test reads its
    /// log or artifacts. Without it, an entry with no check fails.
    no_checks: Option<String>,
}

impl SimSummary {
    fn merge(&mut self, other: SimSummary) {
        self.expects_pass.extend(other.expects_pass);
        self.expects_fail.extend(other.expects_fail);
        self.errors.extend(other.errors);
        // saw_stop reflects the *last* part: if the chain's tail
        // didn't reach stop_sim, surface it. Earlier parts in a
        // multi-start chain are expected to stop and respawn.
        self.saw_stop = other.saw_stop;
        if other.frame_count_parts > 0 {
            if self.frame_count_parts == 0 {
                self.frame_count = other.frame_count;
            } else if let (Some(current), Some(other)) = (self.frame_count, other.frame_count) {
                self.frame_count = Some(current.saturating_add(other));
            } else {
                self.frame_count = None;
            }
            self.frame_count_parts += other.frame_count_parts;
        }
        self.process_id = other.process_id.or(self.process_id);
        self.worker_attempt = other.worker_attempt.or(self.worker_attempt);
        if other.allocation_census.is_some() {
            self.allocation_census = other.allocation_census;
        }
        self.snapshots.extend(other.snapshots);
        self.image_snapshots.extend(other.image_snapshots);
        self.video_assertions.extend(other.video_assertions);
        self.video_match_assertions.extend(other.video_match_assertions);
        self.cleanup_paths.extend(other.cleanup_paths);
        self.diagnostics.push_str(&other.diagnostics);
        self.entry_diagnostics.push_str(&other.entry_diagnostics);
        self.artifacts.extend(other.artifacts);
        self.timed_out = self.timed_out || other.timed_out;
        if other.process_failure_code.is_some() {
            self.process_failure_code = other.process_failure_code;
        }
        self.never_started = self.never_started || other.never_started;
        if other.worker_ineligible.is_some() {
            self.worker_ineligible = other.worker_ineligible;
        }
        if other.excluded.is_some() {
            self.excluded = other.excluded;
        }
        if other.no_checks.is_some() {
            self.no_checks = other.no_checks;
        }
        if let Some(other) = other.reload_ledger {
            let current = self.reload_ledger.get_or_insert_default();
            current.requests += other.requests;
            current.applied += other.applied;
            current.pending += other.pending;
            current.rejected += other.rejected;
            current.failed += other.failed;
        }
    }
}

struct VideoAssertion {
    path: String,
    field: String,
    cmp: String,
    /// Numeric for duration/width/height/nb_frames; serialized JSON
    /// number literal. For codec the side channel sends a JSON-quoted
    /// string and `cmp` is always "==".
    value_json: String,
}

struct VideoMatchAssertion {
    a: String,
    b: String,
    min_sim: f64,
}

struct SnapshotOp {
    name: String,
    value_json: String,
    site: String,
}

struct ImageSnapshotOp {
    name: String,
    path: String,
    tol: f64,
    save_ok: bool,
    site: String,
}

impl SimSummary {
    fn from_stderr(stderr: &str) -> Self {
        let mut s = Self::default();
        for line in stderr.lines() {
            let Some(rest) = line.strip_prefix("__SIM__ ") else {
                continue;
            };
            // The protocol is JSON-shaped but we only need a few fields.
            // A minimal parser pulls "op" + string-typed peers without
            // pulling in serde -- the schema is intentionally narrow.
            let op = extract_str_field(rest, "op").unwrap_or_default();
            let site_raw = extract_str_field(rest, "site").unwrap_or_default();
            let site = compress_site(&site_raw);
            match op.as_str() {
                "expect_ok" => {
                    let message = extract_str_field(rest, "message").unwrap_or_default();
                    s.expects_pass.push(SiteMessage { message, site });
                }
                "expect_fail" => {
                    let message = extract_str_field(rest, "message").unwrap_or_default();
                    s.expects_fail.push(SiteMessage { message, site });
                }
                "error" => {
                    let message = extract_str_field(rest, "message").unwrap_or_default();
                    s.errors.push(SiteMessage { message, site });
                }
                "stop" => s.saw_stop = true,
                "exclude" => {
                    let reason = extract_str_field(rest, "reason")
                        .unwrap_or_else(|| "unspecified".to_string());
                    s.excluded = Some(reason);
                    s.saw_stop = true;
                }
                "no_checks" => {
                    s.no_checks = Some(extract_str_field(rest, "reason").unwrap_or_default());
                }
                "allocation_census" => match AllocationCensus::from_json(rest) {
                    Ok(census) if s.allocation_census.is_none() => {
                        s.allocation_census = Some(census)
                    }
                    Ok(_) => s.errors.push(SiteMessage {
                        message: "duplicate allocation census result".to_string(),
                        site,
                    }),
                    Err(e) => s.errors.push(SiteMessage {
                        message: format!("invalid allocation census result: {e}"),
                        site,
                    }),
                },
                "worker_ineligible" => {
                    s.worker_ineligible =
                        Some(extract_str_field(rest, "message").unwrap_or_default());
                }
                "snapshot" => {
                    let name = extract_str_field(rest, "name").unwrap_or_default();
                    let value_json = extract_raw_field(rest, "value").unwrap_or_else(|| "null".into());
                    s.snapshots.push(SnapshotOp { name, value_json, site: site.clone() });
                }
                "image_snapshot" => {
                    let name = extract_str_field(rest, "name").unwrap_or_default();
                    let path = extract_str_field(rest, "path").unwrap_or_default();
                    let tol = extract_raw_field(rest, "tol")
                        .and_then(|t| t.parse::<f64>().ok())
                        .unwrap_or(0.0);
                    let save_ok = extract_raw_field(rest, "ok").as_deref() == Some("true");
                    s.image_snapshots.push(ImageSnapshotOp {
                        name,
                        path,
                        tol,
                        save_ok,
                        site: site.clone(),
                    });
                }
                "expect_video" => {
                    let path = extract_str_field(rest, "path").unwrap_or_default();
                    let field = extract_str_field(rest, "field").unwrap_or_default();
                    let cmp = extract_str_field(rest, "cmp").unwrap_or_else(|| "==".into());
                    // Codec is a string field (sent quoted); numeric
                    // fields come through as raw numbers. Either way
                    // we capture the raw JSON source for later compare.
                    let value_json = extract_raw_field(rest, "value").unwrap_or_else(|| "null".into());
                    s.video_assertions.push(VideoAssertion {
                        path,
                        field,
                        cmp,
                        value_json,
                    });
                }
                "cleanup_path" => {
                    if let Some(path) = extract_str_field(rest, "path") {
                        s.cleanup_paths.push(path);
                    }
                }
                "expect_videos_match" => {
                    let a = extract_str_field(rest, "a").unwrap_or_default();
                    let b = extract_str_field(rest, "b").unwrap_or_default();
                    let min_sim = extract_raw_field(rest, "min")
                        .and_then(|t| t.parse::<f64>().ok())
                        .unwrap_or(0.0);
                    s.video_match_assertions.push(VideoMatchAssertion { a, b, min_sim });
                }
                _ => {} // set_env / start_game / etc. are informational
            }
        }
        s
    }

    fn has_failures(&self) -> bool {
        !self.expects_fail.is_empty() || !self.errors.is_empty()
    }

    /// Whether the entry ran a check, or declared why it runs none.
    fn checks_accounted(&self) -> bool {
        self.checks_ran() > 0 || self.no_checks.is_some()
    }

    /// Every check the entry reported, passing or failing.
    fn checks_ran(&self) -> usize {
        self.expects_pass.len()
            + self.expects_fail.len()
            + self.snapshots.len()
            + self.image_snapshots.len()
            + self.video_assertions.len()
            + self.video_match_assertions.len()
    }

    /// `streamed` = the live per-op stream already printed sited
    /// failures/errors (the normal single-run path). When false (concise
    /// multi-run), nothing was streamed, so sited entries must be reprinted
    /// here -- with their `[site]` -- or a failing sim would show only a
    /// count and no clue what broke.
    fn print(&self, sim_name: &str, streamed: bool) {
        // Per-fail lines: when streamed, only re-print failures that DIDN'T
        // come through the live stream (empty-site, post-run synthesis:
        // snapshot diff, video assertion, runner errors) -- reprinting sited
        // ones would just duplicate the stream. When not streamed, print all.
        // Carve-out (logging-architecture.md decision 2): the
        // `[sim] error:` / `[sim] FAIL:` / final PASS|FAIL summary
        // lines are the command's result, consumed by you/CI — they
        // stay direct stderr writes, not log records.
        for e in &self.errors {
            if e.site.is_empty() {
                eprintln!("[sim] error: {}", e.message);
            } else if !streamed {
                eprintln!("[sim] error [{}]: {}", e.site, e.message);
            }
        }
        for f in &self.expects_fail {
            if f.site.is_empty() {
                eprintln!("[sim] FAIL: {}", f.message);
            } else if !streamed {
                eprintln!("[sim] FAIL [{}]: {}", f.site, f.message);
            }
        }
        if let Some(ledger) = self.reload_ledger {
            eprintln!(
                "[sim] reload ledger: requests={} applied={} pending={} rejected={} failed={}",
                ledger.requests, ledger.applied, ledger.pending, ledger.rejected, ledger.failed
            );
        }
        let total = self.expects_pass.len() + self.expects_fail.len();
        // The PRINTED verdict must come from the same predicate as the real
        // outcome. Computing it from `has_failures()` alone printed `PASS` for a
        // sim that had aborted mid-body -- a C++ assert in imgui produced
        // `PASS (0/0 expects passed)` next to the crash -- because an abort
        // fails no expect. The exit status was already correct (`entry_passed`
        // requires `saw_stop`), so only the human-facing line lied, which is
        // worse than either being wrong alone: a log reader and a shell `&&`
        // disagreed.
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
            self.expects_pass.len(), total
        );
    }
}

/// Pull a string field out of a JSON object literal without a full
/// JSON parser. Looks for `"<key>":"<value>"` and returns `<value>`
/// with `\"` and `\\` unescaped. Returns `None` if not found or on
/// any escape we don't handle -- callers treat that as "field absent."
///
/// Deliberately minimal: the side-channel schema is fixed and emitted
/// by `sim_emit` in `stone/engine/sim.rock`, which only writes plain
/// `"<key>":"<value>"` shapes. Rich JSON would warrant pulling in a
/// real parser; today's needs don't.
fn extract_str_field(json: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":\"");
    let start = json.find(&needle)? + needle.len();
    let rest = &json[start..];
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                _ => return None,
            },
            other => out.push(other),
        }
    }
    None
}

/// Pull the raw bytes after `"<key>":` from a JSON object literal,
/// stopping at the matching `}`/`]`/`,` boundary. Used for fields
/// whose value is not a string (objects, arrays, numbers, bools).
/// Bracket-balanced; respects nesting and string literals.
fn extract_raw_field(json: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":");
    let start = json.find(&needle)? + needle.len();
    let bytes = json.as_bytes();
    let mut i = start;
    let mut depth_brace = 0i32;
    let mut depth_brack = 0i32;
    let mut in_str = false;
    let mut esc = false;
    while i < bytes.len() {
        let c = bytes[i];
        if in_str {
            if esc {
                esc = false;
            } else if c == b'\\' {
                esc = true;
            } else if c == b'"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'"' => in_str = true,
            b'{' => depth_brace += 1,
            b'}' => {
                if depth_brace == 0 {
                    break;
                }
                depth_brace -= 1;
            }
            b'[' => depth_brack += 1,
            b']' => {
                if depth_brack == 0 {
                    break;
                }
                depth_brack -= 1;
            }
            b',' if depth_brace == 0 && depth_brack == 0 => break,
            _ => {}
        }
        i += 1;
    }
    Some(json[start..i].trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sim_receipt_prepare_releases_session() {
        const HELPER: &str = "ROCK_TEST_RECEIPT_RELEASE_HELPER";
        if let Some(root) = std::env::var_os(HELPER) {
            let root = PathBuf::from(root);
            let prepared = prepare_sims(crate::Backend::Skull, root.join("project"), root.join("output"))
                .expect("prepare receipt");
            assert!(prepared.stone.is_none(), "receipt preparation retained its compiler session");
            return;
        }
        let root = tempfile::tempdir().expect("receipt fixture");
        std::fs::create_dir(root.path().join("project")).unwrap();
        std::fs::create_dir(root.path().join("output")).unwrap();
        std::fs::write(root.path().join("project/main.rock"), "@nomangle fn answer() -> int { 41 }\n").unwrap();
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "run_sim::tests::sim_receipt_prepare_releases_session", "--nocapture"])
            .env(HELPER, root.path())
            .env(SIM_COMPILE_RECEIPT_DIR_ENV, root.path().join("receipts"))
            .env(SIM_COMPILE_RECEIPT_PREPARE_ENV, "1")
            .output().expect("receipt test child");
        assert!(child.status.success(), "{}{}", String::from_utf8_lossy(&child.stdout), String::from_utf8_lossy(&child.stderr));
    }

    #[test]
    fn sim_receipt_registry_requires_source_files() {
        let wire_names = vec!["probe".to_string()];
        let env_by_wire = HashMap::new();
        let missing = registry_json(&wire_names, &env_by_wire, &HashMap::new()).unwrap_err();
        assert!(missing.to_string().contains("compiled @sim `probe` has no recorded source file"));

        let malformed = serde_json::json!([{"wire_name":"probe", "env":[]}]);
        assert!(parse_registry(&malformed)
            .unwrap_err()
            .to_string()
            .contains("source file for `probe` is missing or invalid"));
    }

    fn corpus() -> Vec<String> {
        ["a_first", "b_second", "c_third"].iter().map(|s| s.to_string()).collect()
    }

    fn pick(names: &[&str]) -> Result<Vec<String>> {
        let selected: Vec<String> = names.iter().map(|s| s.to_string()).collect();
        select_gate_corpus(&corpus(), &selected)
    }

    fn sig(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        boot_signature(
            &pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<Vec<_>>(),
        )
    }

    #[test]
    fn reload_lane_reuses_the_compiled_session_and_isolates_its_chunk() {
        let project = tempfile::tempdir().expect("project");
        let shared = tempfile::tempdir().expect("shared output");
        let source = project.path().join("main.rock");
        std::fs::write(&source, "@nomangle fn answer() -> int { 41 }\n").unwrap();
        let main = shared.path().join(rock_host::project::MAIN_LUA_FILENAME);
        let stone = crate::skull_backend::SkullStone::init_with_tests(
            project.path().to_str().unwrap(),
            &main,
            &shared.path().join("incremental.lua"),
        ).expect("compile once");
        let original = std::fs::read(&main).unwrap();
        let fingerprint = stone.artifact_fingerprint().to_string();
        std::fs::remove_file(&source).unwrap();
        let lane = reload_lane_session(
            crate::Backend::Skull,
            project.path(),
            true,
            shared.path(),
            Some(stone),
            true,
        ).expect("reuse without reading source").expect("selected lane");
        assert_eq!(lane.stone.artifact_fingerprint(), fingerprint);
        let lane_main = Path::new(&lane.output).join(rock_host::project::MAIN_LUA_FILENAME);
        assert_eq!(std::fs::read(&lane_main).unwrap(), original);
        std::fs::write(&lane_main, "fixture rewrite").unwrap();
        assert_eq!(std::fs::read(&main).unwrap(), original);
    }

    /// A declaration that RESTATES the runner's own baseline produces a
    /// byte-identical child environment, so it cannot stand for a difference
    /// between two entries -- the same argument `RUNNER_OWNED_ENV_KEYS` already
    /// makes for `ROCK_MASK_WALL_CLOCK`. mag declares `ROCK_TEST_SEED=42` in 182
    /// sims and nothing in 73, and splitting those cost 6 of its 43 boots.
    #[test]
    fn a_declaration_at_the_runner_baseline_does_not_split_a_group() {
        for (key, value) in RUNNER_BASELINE_ENV {
            assert_eq!(
                sig(&[(key, value)]),
                sig(&[]),
                "`{key}={value}` restates the runner's baseline, so it is not a boot difference"
            );
        }
    }

    /// The other side, and the reason this is a (key, value) match rather than
    /// an entry in `RUNNER_OWNED_ENV_KEYS`: a DIFFERENT value is a real boot
    /// difference and must still refuse to share a process. Dropping the key
    /// outright would silently run a seed-7 sim in a seed-42 process.
    #[test]
    fn a_declaration_away_from_the_baseline_still_splits_a_group() {
        assert_ne!(
            sig(&[("ROCK_TEST_SEED", "7")]),
            sig(&[]),
            "a seed the runner did not publish is a genuine boot difference"
        );
        assert_ne!(
            sig(&[("ROCK_TEST_SEED", "7")]),
            sig(&[("ROCK_TEST_SEED", "42")]),
            "two different declared seeds are two different boot worlds"
        );
    }

    /// The canonicalization must not swallow a neighbour: a signature keeps
    /// every pair that is not a baseline restatement.
    #[test]
    fn canonicalizing_the_baseline_leaves_other_pairs_alone() {
        assert_eq!(
            sig(&[("ROCK_TEST_SEED", "42"), ("SIM_SKIP_RENDER", "0")]),
            sig(&[("SIM_SKIP_RENDER", "0")]),
            "only the inert pair comes out"
        );
        assert_eq!(sig(&[("SIM_SKIP_RENDER", "0")]).len(), 1);
    }

    /// Assets are stubbed for EVERY sim child, rendering included: a stubbed
    /// image is one white texel, which is what a sim's layout and gameplay
    /// assertions sample. Only a declared or inherited "0" turns it off, and it
    /// is decided per PROCESS because a chain shares one image cache. That a
    /// rendering entry no longer opts its process in is structural -- the
    /// decision cannot see an entry at all.
    #[test]
    fn every_sim_child_stubs_its_assets_unless_asked_not_to() {
        // The inherited escape hatch is one-way: it can ask for real assets,
        // never for stubs. A stray "1" must not strip pixels out of a snapshot.
        assert_eq!(unstub_from(None), "1", "unset stays stubbed");
        assert_eq!(unstub_from(Some("0")), "0", "an inherited 0 unstubs");
        assert_eq!(unstub_from(Some("1")), "1", "an inherited 1 is the default anyway");
        assert_eq!(unstub_from(Some("yes")), "1", "anything else is not an unstub request");

        // No ambient request, so the runner's own default, which every entry now
        // takes. A fixture that needs the file's own texels declares
        // `@sim_env("STONE_ASSET_STUB", "0")` and `preset_env` carries it.
        if std::env::var("STONE_ASSET_STUB").is_err() {
            assert_eq!(asset_stub_value(), "1", "the runner default is stubbed");
        }
    }

    /// `entry_renders` reads the EFFECTIVE value, last-wins. `@sim_render` is
    /// lowered to `SIM_SKIP_RENDER=0` before an explicit `@sim_env` is appended,
    /// so a sim carrying both is whatever it says last -- and that is the value
    /// its child actually boots with. Nothing schedules on it now, but the
    /// census and the capability docs classify sims by it.
    #[test]
    fn rendering_is_the_effective_last_wins_value() {
        let pair = |k: &str, v: &str| (k.to_string(), v.to_string());
        assert!(!entry_renders(&[]), "declaring nothing does not render");
        assert!(entry_renders(&[pair("SIM_SKIP_RENDER", "0")]), "@sim_render sugar");
        assert!(!entry_renders(&[pair("SIM_SKIP_RENDER", "1")]), "explicit skip");
        assert!(!entry_renders(&[pair("SIM_SKIP_RENDER", "submit")]), "submit needs no GPU result");
        // `@sim_render` then an explicit override, in lowering order.
        assert!(
            !entry_renders(&[pair("SIM_SKIP_RENDER", "0"), pair("SIM_SKIP_RENDER", "1")]),
            "a later explicit @sim_env overrides the @sim_render sugar"
        );
        assert!(
            entry_renders(&[pair("SIM_SKIP_RENDER", "1"), pair("SIM_SKIP_RENDER", "0")]),
            "... and the other way round"
        );
        assert!(!entry_renders(&[pair("MAG_FORCE_VFX", "1")]), "an unrelated key is not it");
    }

    /// The default DEGRADES where worker mode cannot run and an explicit
    /// `--workers` REJECTS, which is the only difference between them. Both
    /// arms are here because a resolver that always degraded, or always
    /// rejected, would satisfy a test that checked one side.
    #[test]
    fn the_default_degrades_where_workers_rejects() {
        let degraded = resolve_exec_mode(SimExecMode::Default, true, true)
            .expect("the default falls back rather than failing the run");
        assert_eq!(
            degraded,
            SimExecMode::Isolated,
            "and it falls back to the ORACLE, not to some third behaviour"
        );

        let err = resolve_exec_mode(SimExecMode::Workers, true, true)
            .expect_err("--workers was asked for on purpose and must not be downgraded");
        assert!(
            err.to_string().contains("--workers"),
            "the error names the flag that was refused: {err}"
        );
    }

    /// The control for the pair above: with no constraint in play the default
    /// stays in worker mode. Without this, a resolver that degraded
    /// unconditionally would pass the degrade arm and silently cost the 4.3x.
    #[test]
    fn the_default_stays_in_worker_mode_when_nothing_blocks_it() {
        assert_eq!(
            resolve_exec_mode(SimExecMode::Default, false, true).expect("no constraint"),
            SimExecMode::Default,
        );
        assert_eq!(
            resolve_exec_mode(SimExecMode::Workers, false, true).expect("no constraint"),
            SimExecMode::Workers,
        );
    }

    /// `--isolated` is unreachable by either path: it is already the oracle, so
    /// there is nothing to degrade to and nothing to refuse.
    #[test]
    fn isolated_is_unaffected_by_the_availability_check() {
        assert_eq!(
            resolve_exec_mode(SimExecMode::Isolated, true, true).expect("isolated always runs"),
            SimExecMode::Isolated,
        );
    }

    /// The default admits a RENDERED entry as readily as a render-skipped one.
    /// Both populations are present in the input, so a rule that admitted only
    /// one kind would be visible here rather than reading as a green.
    #[test]
    fn the_default_admits_rendered_and_render_skipped_alike() {
        let mut wires: HashMap<String, Vec<(String, String)>> = HashMap::new();
        wires.insert("logic".to_string(), vec![]);
        wires.insert(
            "pixel".to_string(),
            vec![("SIM_SKIP_RENDER".to_string(), "0".to_string())],
        );
        let specs = vec!["logic".to_string(), "pixel".to_string()];

        let (groups, isolated) = group_worker_specs(&specs, &wires);
        assert_eq!(isolated.len(), 0, "neither entry is isolated: {isolated:?}");
        assert_eq!(
            groups.iter().map(Vec::len).sum::<usize>(),
            2,
            "both entries are admitted, the rendered one included: {groups:?}"
        );
    }

    /// A RENDERED chain is admitted whole. It used to be isolated for rendering;
    /// what still isolates a chain is its parts DISAGREEING, which is a
    /// different rule with its own test -- and why both parts render here, since
    /// a rendered part beside a render-skipped one disagrees by construction and
    /// would be isolated for that reason instead.
    #[test]
    fn a_rendered_chain_is_admitted() {
        let mut wires: HashMap<String, Vec<(String, String)>> = HashMap::new();
        wires.insert(
            "head".to_string(),
            vec![("SIM_SKIP_RENDER".to_string(), "0".to_string())],
        );
        wires.insert(
            "tail".to_string(),
            vec![("SIM_SKIP_RENDER".to_string(), "0".to_string())],
        );
        let (groups, isolated) =
            group_worker_specs(&["head,tail".to_string()], &wires);
        assert_eq!(isolated.len(), 0, "the chain is admitted: {isolated:?}");
        assert_eq!(groups.len(), 1, "as one queue: {groups:?}");
    }

    /// A hole in the full-corpus gate stops it: the run would otherwise print a
    /// green over an arm it never compared.
    ///
    /// The fixture arm is `verdict` because it is non-exempt PERMANENTLY: a run
    /// that compared no verdicts compared nothing, so it can never join
    /// `ARMS_WITHOUT_A_CORPUS`. An arm that is merely unpopulated today would
    /// disarm this test the moment it was exempted, which is exactly what
    /// happened when `expects_fail` stood here.
    #[test]
    fn the_full_corpus_refuses_an_arm_it_could_not_populate() {
        let reason = population_refusal(&["verdict"], false).expect("must refuse");
        assert!(reason.contains("verdict"), "names the arm: {reason}");
    }

    /// The arms no corpus in this tree can populate are stated permanent
    /// exceptions rather than a gate everyone learns to ignore: an `error` op
    /// also fails its sim, and only a deliberately red sim populates
    /// `expects_fail`.
    #[test]
    fn a_permanently_unpopulatable_arm_is_not_a_hole() {
        assert!(population_refusal(&["errors"], false).is_none());
        assert!(population_refusal(&["expects_fail"], false).is_none());
        assert!(population_refusal(&["errors", "expects_fail"], false).is_none());
        // ... and neither launders a real hole reported beside it.
        let reason =
            population_refusal(&["errors", "expects_fail", "verdict"], false).expect("must refuse");
        assert!(reason.contains("verdict"), "names the real hole: {reason}");
        assert!(!reason.contains("errors"), "does not name the exception: {reason}");
        assert!(!reason.contains("expects_fail"), "does not name the exception: {reason}");
    }

    /// A subset is chosen to re-measure ONE class. Whether that pair happens to
    /// fail an expect or write an artifact is unrelated to why it was picked,
    /// and refusing over it would make an equivalent pair exit nonzero -- which
    /// is exactly the iteration gate this form exists to be.
    #[test]
    fn a_named_subset_is_not_refused_for_an_empty_population() {
        assert!(population_refusal(&["expects_fail", "artifacts", "errors"], true).is_none());
    }

    /// No names is the whole registry, which is the arc-checkpoint form.
    #[test]
    fn an_empty_selection_is_the_whole_corpus() {
        assert_eq!(pick(&[]).expect("whole corpus"), corpus());
    }

    /// REGISTRY order, not the order the caller typed -- that is what the
    /// "registry" arm means, and what makes the "reverse" arm its reverse. A
    /// subset that honoured argv order would silently rename its own arms.
    #[test]
    fn a_selection_keeps_registry_order_whatever_order_it_was_typed_in() {
        assert_eq!(pick(&["c_third", "a_first"]).expect("subset"), vec!["a_first", "c_third"]);
    }

    /// Each refusal below would otherwise shrink the corpus silently, and a
    /// gate that measured less than it was asked to would still print a green.
    #[test]
    fn an_unknown_name_refuses_instead_of_running_the_ones_that_matched() {
        let err = pick(&["a_first", "typo_here"]).expect_err("unknown name must refuse");
        assert!(err.to_string().contains("typo_here"), "names the offender: {err}");
    }

    #[test]
    fn a_repeated_name_refuses_because_it_has_only_one_reference_record() {
        let err = pick(&["a_first", "a_first"]).expect_err("repeat must refuse");
        assert!(err.to_string().contains("a_first"), "names the offender: {err}");
    }

    /// Asserts the chain-SPECIFIC reason, not merely that something refused.
    /// A chain name is also absent from the registry, so with this arm deleted
    /// it falls through to the unknown-name refusal, which names the offender
    /// too -- an "it refused" assertion passes either way and cannot tell the
    /// two apart. Mutation-proven only in this form.
    #[test]
    fn a_comma_chain_is_refused_for_being_a_chain_not_for_being_unknown() {
        let err = pick(&["a_first,b_second"]).expect_err("chain must refuse");
        let msg = err.to_string();
        assert!(msg.contains("a_first,b_second"), "names the offender: {err}");
        assert!(
            msg.contains("comma-chains") && msg.contains("$OUT"),
            "must refuse it for being a chain, not for being an unknown name: {err}"
        );
    }

    /// A MISSING image golden used to bootstrap itself from the current frame
    /// and count as a passing expect, so a sim whose goldens are untracked
    /// (`mag/tests/sims/vfx_capture.snap/`, gitignored as display-specific)
    /// self-passed on any machine that had never run it -- the assertion was
    /// never evaluated. Only `--update-snapshots` may write a baseline.
    #[test]
    fn a_missing_image_golden_is_not_a_pass() {
        let dir = std::env::temp_dir().join(format!(
            "rock-golden-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("tmp dir");
        let produced = dir.join("produced.png");
        // A 1x1 PNG; the bytes never get compared on the paths under test.
        std::fs::write(&produced, [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]).expect("write");
        let baseline = dir.join("golden.png");
        let img = ImageSnapshotOp {
            name: "golden".into(),
            path: produced.to_string_lossy().into_owned(),
            tol: 0.001,
            save_ok: true,
            site: String::new(),
        };

        let r = handle_image_snapshot(&img, &baseline, false).expect("no hard error");
        assert!(
            matches!(r, SnapResult::Fail(_)),
            "a missing golden must not pass; got {r:?}"
        );
        assert!(!baseline.exists(), "and it must NOT have been written behind our back");

        // Both sides: with --update-snapshots, writing it is the point.
        let r = handle_image_snapshot(&img, &baseline, true).expect("no hard error");
        assert!(matches!(r, SnapResult::Wrote), "update writes the baseline; got {r:?}");
        assert!(baseline.exists(), "the baseline is created under update");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The lane discriminant decides whether a child may touch the one live
    /// `SkullStone` session and the one `incremental.lua`. Getting it wrong in
    /// the permissive direction lets an ordinary child mutate build state
    /// shared with every other child once the suite runs in parallel.
    #[test]
    fn reload_lane_is_opt_in_via_stone_hot_reload() {
        let pair = |k: &str, v: &str| vec![(k.to_string(), v.to_string())];

        assert!(is_reload_lane(&pair("STONE_HOT_RELOAD", "1")));
        // Only the opt-in key routes to the lane; a busy sim env does not.
        assert!(!is_reload_lane(&pair("SIM_SKIP_RENDER", "0")));
        assert!(!is_reload_lane(&[]));
        // An explicit disable is not an opt-in: the child's watcher does not
        // poll, so it can never observe a reload.
        assert!(!is_reload_lane(&pair("STONE_HOT_RELOAD", "0")));
        // The key still routes when it is not the only pair.
        let mut mixed = pair("ROCK_TEST_SEED", "42");
        mixed.extend(pair("STONE_HOT_RELOAD", "1"));
        assert!(is_reload_lane(&mixed));
    }

    /// The reload lane's exclusion from the bytecode image is a correctness
    /// requirement, not a tuning choice: `check_hot_reload` derives the overlay
    /// path from the running script's own path by replacing `main.lua` with
    /// `incremental.lua` (`stone/engine/hot_reload.rock:87`), and
    /// `read_chunk_source_dir` (`:43`) reads line 1 of a chunk AS TEXT to decide
    /// whether it belongs to this game. Both questions are unanswerable against
    /// a bytecode image, so a reload child booted from one would silently stop
    /// watching -- a reload sim would pass every assertion and observe nothing.
    #[test]
    fn only_a_non_reload_worker_boots_from_the_bytecode_image() {
        let dir = std::env::temp_dir().join(format!("rock-chunk-path-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let main = dir.join(rock_host::project::MAIN_LUA_FILENAME);
        let bytecode = dir.join(rock_host::project::MAIN_BYTECODE_FILENAME);
        std::fs::write(&main, "-- source").unwrap();

        // No image yet: both lanes run the source file. The fallback is what
        // makes the dump best-effort rather than a new failure mode.
        assert_eq!(worker_chunk_path(&dir, false), main);
        assert_eq!(worker_chunk_path(&dir, true), main);

        std::fs::write(&bytecode, "-- image").unwrap();
        assert_eq!(worker_chunk_path(&dir, false), bytecode);
        assert_eq!(
            worker_chunk_path(&dir, true),
            main,
            "a reload child must keep the source path it derives the overlay from"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_full_asset_jobs_is_bounded_by_jobs_and_defaults_to_two() {
        assert_eq!(resolve_full_asset_jobs(None, 12), DEFAULT_FULL_ASSET_JOBS);
        assert_eq!(resolve_full_asset_jobs(None, 1), 1);
        assert_eq!(resolve_full_asset_jobs(Some(4), 12), 4);
        assert_eq!(resolve_full_asset_jobs(Some(40), 12), 12);
        assert_eq!(resolve_full_asset_jobs(Some(0), 12), 1);
        assert_eq!(resolve_full_asset_jobs(Some(3), 1), 1);
    }

    /// Only the pair marks a lane: real assets with the cap declined. The
    /// runner's stub default and the baseline cap stand in for what a fixture
    /// does not say, last declaration wins, and the render flag is not read.
    #[test]
    fn entry_full_assets_needs_real_assets_and_no_image_cap() {
        let pair = |k: &str, v: &str| (k.to_string(), v.to_string());
        assert!(!entry_full_assets(&[], "1"), "declaring nothing is stubbed and capped");
        assert!(!entry_full_assets(&[pair("STONE_ASSET_STUB", "0")], "1"), "real assets alone stay capped");
        assert!(!entry_full_assets(&[pair("STONE_IMAGE_MAX_SIDE", "0")], "1"), "an uncapped stub is one texel");
        assert!(entry_full_assets(&[pair("STONE_ASSET_STUB", "0"), pair("STONE_IMAGE_MAX_SIDE", "0")], "1"));
        assert!(
            entry_full_assets(&[pair("STONE_IMAGE_MAX_SIDE", "0"), pair("SIM_SKIP_RENDER", "submit")], "0"),
            "an inherited STONE_ASSET_STUB=0 counts, and submit is not consulted"
        );
        assert!(
            !entry_full_assets(&[pair("STONE_ASSET_STUB", "0"), pair("STONE_IMAGE_MAX_SIDE", "0"), pair("STONE_IMAGE_MAX_SIDE", "512")], "1"),
            "last declaration wins"
        );
    }

    #[test]
    fn the_classed_pool_caps_live_capped_tasks_and_runs_everything_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let renders: Vec<bool> = (0..40).map(|i| i % 3 == 0).collect();
        let live = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let results = run_bounded_classed(&PoolWidth::fixed(8), 2, &renders, |i| {
            if renders[i] {
                let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(3));
                live.fetch_sub(1, Ordering::SeqCst);
            } else {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            Ok::<usize, anyhow::Error>(i)
        });
        let mut seen: Vec<usize> = results.iter().map(|(i, _)| *i).collect();
        seen.sort_unstable();
        assert_eq!(seen, (0..40).collect::<Vec<_>>(), "every task exactly once");
        assert!(results.iter().all(|(i, r)| r.as_ref().map(|v| v == i).unwrap_or(false)));
        assert!(peak.load(Ordering::SeqCst) <= 2, "peak live rendering {}", peak.load(Ordering::SeqCst));
        assert!(peak.load(Ordering::SeqCst) >= 1);
    }

    /// Both pools the lane runs beside: the plain one and the worker-mode
    /// pool with a capped class.
    #[test]
    fn the_pool_takes_the_reload_lanes_slot_back_when_the_lane_finishes() {
        let capped: Vec<bool> = (0..60).map(|i| i % 4 == 0).collect();
        reload_lane_slot_returns(|width, task| run_bounded(width, 60, task));
        reload_lane_slot_returns(|width, task| run_bounded_classed(width, 1, &capped, task));
    }

    fn reload_lane_slot_returns(
        pool: impl FnOnce(&PoolWidth, &(dyn Fn(usize) -> Result<usize> + Sync)) -> Vec<(usize, Result<usize>)> + Send,
    ) {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        let live = AtomicUsize::new(0);
        let peak_beside_lane = AtomicUsize::new(0);
        let peak_after_lane = AtomicUsize::new(0);
        let lane_done = AtomicBool::new(false);
        let task = |i: usize| {
            let now = live.fetch_add(1, Ordering::SeqCst) + 1;
            if lane_done.load(Ordering::SeqCst) {
                peak_after_lane.fetch_max(now, Ordering::SeqCst);
            } else {
                peak_beside_lane.fetch_max(now, Ordering::SeqCst);
            }
            std::thread::sleep(std::time::Duration::from_millis(4));
            live.fetch_sub(1, Ordering::SeqCst);
            Ok::<usize, anyhow::Error>(i)
        };
        let (pool, ()) = beside_reload_lane(
            3,
            true,
            |width| pool(width, &task),
            || {
                std::thread::sleep(std::time::Duration::from_millis(40));
                lane_done.store(true, Ordering::SeqCst);
            },
        );
        assert_eq!(pool.len(), 60, "every task exactly once");
        assert!(
            peak_beside_lane.load(Ordering::SeqCst) <= 2,
            "the lane holds one of three jobs while it runs, peak {}",
            peak_beside_lane.load(Ordering::SeqCst)
        );
        assert_eq!(
            peak_after_lane.load(Ordering::SeqCst),
            3,
            "the pool widens to every job once the lane is done"
        );
    }

    #[test]
    fn lanes_dispatch_longest_first_with_untimed_lanes_last_in_plan_order() {
        let estimates = [Some(1_000), None, Some(30_000), Some(1_000), None, Some(8_000)];
        assert_eq!(longest_first(&estimates), vec![2, 5, 0, 3, 1, 4]);
    }

    #[test]
    fn fresh_run_dispatches_seeded_longest_lane_first() {
        // A fresh machine has no local history for anything; the checked-in
        // seeded medians still order the seeded-longest lane first. Anchored
        // on the table's own extremes so a regen keeps this honest.
        let mut table = crate::sim_observability::seeded_table_snapshot();
        assert!(!table.is_empty(), "the checked-in seeded table must ship medians");
        table.sort_by_key(|(_, ms)| *ms);
        let (fast_name, fast_ms) = table.first().expect("seeded table").clone();
        let (slow_name, slow_ms) = table.last().expect("seeded table").clone();
        assert!(slow_ms > fast_ms, "a degenerate seeded table cannot order lanes");

        assert_eq!(
            entry_schedule_estimate(None, &slow_name),
            (Some(slow_ms), Some(EstimateSource::Seeded))
        );
        assert_eq!(
            entry_schedule_estimate(None, &fast_name),
            (Some(fast_ms), Some(EstimateSource::Seeded))
        );
        // This machine's own measurement always wins over the checked-in prior.
        assert_eq!(
            entry_schedule_estimate(Some(5), &slow_name),
            (Some(5), Some(EstimateSource::History))
        );
        // A sim the table never saw keeps today's behavior: no estimate, plan order.
        assert_eq!(entry_schedule_estimate(None, "no_such_sim_in_the_table"), (None, None));

        let estimates = [
            entry_schedule_estimate(None, &fast_name).0,
            entry_schedule_estimate(None, &slow_name).0,
        ];
        assert_eq!(longest_first(&estimates), vec![1, 0], "the seeded-longest lane dispatches first");
    }

    #[test]
    fn the_classed_pool_with_no_cap_is_the_plain_pool() {
        let renders = vec![true; 5];
        let results = run_bounded_classed(&PoolWidth::fixed(4), 4, &renders, |i| Ok::<usize, anyhow::Error>(i * 2));
        let mut got: Vec<(usize, usize)> = results.into_iter().map(|(i, r)| (i, r.unwrap())).collect();
        got.sort_unstable();
        assert_eq!(got, (0..5).map(|i| (i, i * 2)).collect::<Vec<_>>());
    }

    /// The job count is a safety bound, not a preference: it must never exceed
    /// what the machine can run, and `--update-snapshots` must pin to serial
    /// because it writes baseline files whose destinations are not proven
    /// unique before spawning.
    #[test]
    fn resolve_jobs_is_bounded_and_serial_for_snapshot_updates() {
        let (cores, budget) = (32, 4);

        assert_eq!(resolve_jobs_with(None, false, cores, budget, false).unwrap(), budget, "no request takes the memory budget");
        assert_eq!(resolve_jobs_with(Some(1), false, cores, budget, false).unwrap(), 1);
        let error = resolve_jobs_with(Some(10), false, cores, budget, false).unwrap_err();
        assert!(error.to_string().contains("memory budget of 4"));
        assert!(error.to_string().contains("--jobs-force"));
        assert_eq!(resolve_jobs_with(Some(4), false, cores, budget, false).unwrap(), 4);
        assert_eq!(resolve_jobs_with(Some(8), true, cores, budget, false).unwrap(), 1);
        // A forced request may exceed the budget, never the cores.
        assert_eq!(resolve_jobs_with(Some(10), false, cores, budget, true).unwrap(), 10);
        assert_eq!(resolve_jobs_with(Some(usize::MAX), false, cores, budget, true).unwrap(), cores);
        // 0 is meaningless; clamp up rather than deadlock on an empty pool.
        assert_eq!(resolve_jobs_with(Some(0), false, cores, budget, false).unwrap(), 1);
        // --update-snapshots wins over ANY explicit request.
        assert_eq!(resolve_jobs_with(None, true, cores, budget, true).unwrap(), 1);
        assert_eq!(resolve_jobs_with(Some(8), true, cores, budget, true).unwrap(), 1);
    }

    #[test]
    fn a_request_above_the_known_lanes_is_narrowed_before_the_budget() {
        let (cores, budget) = (32, 1);
        // One name under a one-lane budget runs in its one lane.
        assert_eq!(resolve_jobs_with(narrow_to_lanes(Some(4), Some(1)), false, cores, budget, false).unwrap(), 1);
        // Two names still need two lanes, which the budget refuses.
        let error = resolve_jobs_with(narrow_to_lanes(Some(4), Some(2)), false, cores, budget, false).unwrap_err();
        assert!(error.to_string().contains("--jobs 2 exceeds"), "{error}");
        // Unknown lanes leave the request alone; no request stays no request.
        assert_eq!(narrow_to_lanes(Some(4), None), Some(4));
        assert_eq!(narrow_to_lanes(None, Some(2)), None);
        assert_eq!(narrow_to_lanes(Some(0), Some(2)), Some(0));
    }

    #[test]
    fn the_default_job_count_leaves_a_fixed_gpu_memory_reserve() {
        // Four lanes' worth above the reserve: four lanes.
        assert_eq!(vram_budgeted_jobs(32, Some(SIM_VRAM_RESERVE_MIB + 4 * SIM_LANE_VRAM_MIB + 1)), 4);
        // A small machine with a large card stays bounded by its cores.
        assert_eq!(vram_budgeted_jobs(2, Some(SIM_VRAM_RESERVE_MIB + 64 * SIM_LANE_VRAM_MIB)), 2);
        // Nothing above the reserve still runs one lane rather than none.
        assert_eq!(vram_budgeted_jobs(32, Some(SIM_VRAM_RESERVE_MIB)), 1);
        assert_eq!(vram_budgeted_jobs(32, Some(SIM_LANE_VRAM_MIB / 4)), 1);
        // Without a reading (no NVIDIA tooling, e.g. macOS) the cores decide.
        assert_eq!(vram_budgeted_jobs(14, None), 14);
    }

    #[test]
    fn default_jobs_fit_ram_and_gpu_budgets() {
        let studio = memory_budgeted_jobs(14, Some(36 * 1024), None);
        assert_eq!(studio, 10);
        assert_eq!(resolve_jobs_with(None, false, 14, studio, false).unwrap(), 10);
        assert_eq!(resolve_jobs_with(Some(14), false, 14, studio, true).unwrap(), 14);
        assert_eq!(resolve_jobs_with(Some(14), true, 14, studio, true).unwrap(), 1);
        assert_eq!(memory_budgeted_jobs(32, Some(62 * 1024), Some(17851)), 5);
        assert_eq!(memory_budgeted_jobs(32, Some(62 * 1024), None), 17);
        assert_eq!(memory_budgeted_jobs(2, Some(36 * 1024), None), 2);
        assert_eq!(memory_budgeted_jobs(14, Some(0), None), 1);
        assert_eq!(memory_budgeted_jobs(0, Some(36 * 1024), None), 1);
        assert_eq!(memory_budgeted_jobs(14, None, None), 14);
    }

    #[test]
    fn nvidia_smi_free_memory_reads_the_first_gpu() {
        assert_eq!(parse_nvidia_smi_free_mib("19115\n"), Some(19115));
        assert_eq!(parse_nvidia_smi_free_mib(" 19115\n4000\n"), Some(19115));
        assert_eq!(parse_nvidia_smi_free_mib(""), None);
        assert_eq!(parse_nvidia_smi_free_mib("[N/A]\n"), None);
    }

    #[test]
    fn duration_estimate_totals_stay_within_the_recorded_integer_domain() {
        let costs = [u64::MAX / 2 + 1, u64::MAX / 2 + 1, 1234];
        let wide: u128 = costs.iter().map(|&ms| ms as u128).sum();
        let representable = u64::try_from(wide).unwrap_or(u64::MAX);
        assert_eq!(sum_estimates(costs.into_iter().map(|ms| (Some(ms), None))).0,
            Some(representable));
    }

    #[test]
    fn duration_blocks_reduce_a_slow_tail_without_changing_predecessor_order() {
        let group: Vec<usize> = (0..100).collect();
        let costs: Vec<u64> = group.iter().map(|i| if [53, 54, 55, 56].contains(i) { 20_000 } else { 100 }).collect();
        let lanes = allocate_worker_lanes(&[group.clone()], 4, &costs.iter().copied().map(Some).collect::<Vec<_>>());
        let peak = lanes.iter().map(|lane| lane.iter().map(|&i| costs[i]).sum::<u64>()).max().unwrap();
        // A feasible independent partition isolates the four adjacent slow entries.
        let oracle = [&group[..54], &group[54..55], &group[55..56], &group[56..]];
        let feasible_peak = oracle.iter().map(|lane| lane.iter().map(|&i| costs[i]).sum::<u64>()).max().unwrap();
        assert!(peak <= feasible_peak, "idle workers leave a {peak}ms tail; feasible peak {feasible_peak}ms");
        assert_eq!(lanes.concat(), group);
    }

    #[test]
    fn duration_blocks_match_an_independent_partition_oracle() {
        fn optimal_peak(costs: &[u64], blocks: usize) -> u128 {
            let n = costs.len();
            let mut dp = vec![vec![u128::MAX; n + 1]; blocks + 1];
            dp[0][0] = 0;
            for b in 1..=blocks {
                for end in b..=n {
                    let mut tail = 0;
                    for cut in (b - 1..end).rev() {
                        tail += costs[cut] as u128;
                        dp[b][end] = dp[b][end].min(dp[b - 1][cut].max(tail));
                    }
                }
            }
            dp[blocks][n]
        }
        for seed in 0..12 {
            let group: Vec<_> = (0..31).collect();
            let costs: Vec<_> = (0..31).map(|i| 1_000 + ((i * 7919 + seed * 104729) % 10_000) as u64).collect();
            let estimates: Vec<_> = costs.iter().copied().map(Some).collect();
            for jobs in [1, 2, 3, 4] {
                let lanes = allocate_worker_lanes(&[group.clone()], jobs, &estimates);
                let peak = lanes.iter().map(|lane| lane.iter().map(|&i| costs[i] as u128).sum::<u128>()).max().unwrap();
                let score = peak + (lanes.len() - 1) as u128 * SIM_BLOCK_BOOT_TAX_MS as u128;
                let oracle = (1..=jobs).map(|blocks| optimal_peak(&costs, blocks)
                    + (blocks - 1) as u128 * SIM_BLOCK_BOOT_TAX_MS as u128).min().unwrap();
                assert_eq!(score, oracle, "seed {seed}, jobs {jobs}");
                assert_eq!(lanes.concat(), group);
                assert!(lanes.len() <= jobs);
            }
        }
    }

    #[test]
    fn duration_blocks_keep_tiny_and_boot_dominated_groups_whole() {
        let tiny: Vec<_> = (0..SIM_BLOCK_TINY_ENTRIES).collect();
        assert_eq!(allocate_worker_lanes(&[tiny.clone()], 4, &vec![Some(60_000); tiny.len()]), vec![tiny]);
        let group: Vec<_> = (0..80).collect();
        for costs in [vec![Some(0); group.len()], vec![Some(1); group.len()]] {
            assert_eq!(allocate_worker_lanes(&[group.clone()], 4, &costs), vec![group.clone()]);
        }
        let mut huge = vec![Some(1); group.len()];
        huge[35] = Some(u64::MAX);
        assert_eq!(allocate_worker_lanes(&[group.clone()], 4, &huge), vec![group.clone()], "no split can shorten the indivisible entry");
        assert_eq!(allocate_worker_lanes(&[group.clone()], 0, &huge), vec![group]);
        assert!(allocate_worker_lanes(&[vec![]], 0, &[]).is_empty());
    }

    #[test]
    fn duration_blocks_bound_useful_workers_by_the_time_target() {
        let group: Vec<_> = (0..21).collect();
        let costs = vec![Some(SIM_BLOCK_TARGET_MS / (group.len() as u64 + 1)); group.len()];
        // A feasible split beats the boot tax, but all work fits in one target.
        assert_eq!(allocate_worker_lanes(&[group.clone()], 4, &costs), vec![group]);
    }

    #[test]
    fn duration_blocks_use_deterministic_count_fallback_for_incomplete_costs() {
        let groups = vec![(0..90).collect(), (90..180).collect()];
        let missing = allocate_worker_lanes(&groups, 4, &[]);
        let mut partial = vec![Some(30_000); 180];
        partial[0] = None;
        partial[179] = None;
        let actual = allocate_worker_lanes(&groups, 4, &partial);
        assert_eq!(actual, missing);
        assert_eq!(actual, allocate_worker_lanes(&groups, 4, &partial));
        assert_eq!(actual.concat(), groups.concat());
        assert!(actual.iter().all(|lane| groups.iter().any(|group| group.windows(lane.len()).any(|window| window == lane))));
    }

    #[test]
    fn duration_estimates_require_every_chain_part_and_count_repeated_names() {
        let table = crate::sim_observability::seeded_table_snapshot();
        let (name, ms) = table.into_iter().find(|(_, ms)| *ms > 0).expect("seeded timings");
        assert_eq!(spec_schedule_estimate(&format!("{name},{name}"), None).0, ms.checked_mul(2));
        assert_eq!(spec_schedule_estimate(&format!("{name},no_such_sim_in_the_table"), None), (None, None));
        assert_eq!(spec_schedule_estimate("no_such_sim_in_the_table", None), (None, None));
        let specs = vec![name; 100];
        let plan = worker_lane_plan(&specs, &HashMap::new(), "gl", 4);
        assert_eq!(plan["ordinary_entries"], specs.len());
        assert_eq!(plan["count_fallback_groups"], 0);
        assert!(plan["ordinary_lanes"].as_u64().unwrap() <= 4);
    }

    #[test]
    fn worker_lane_allocation_keeps_small_groups_intact() {
        let groups = vec![vec![0, 1], vec![2, 3]];
        assert_eq!(allocate_worker_lanes(&groups, 4, &[]), groups);
    }

    #[test]
    fn worker_lane_allocation_splits_a_group_into_contiguous_quantum_blocks() {
        let fits: Vec<usize> = (0..SIM_BATCH_QUANTUM).collect();
        assert_eq!(allocate_worker_lanes(&[fits.clone()], 4, &[]), vec![fits]);

        let group: Vec<usize> = (0..2 * SIM_BATCH_QUANTUM + 1).collect();
        let lanes = allocate_worker_lanes(&[group.clone()], 4, &[]);
        assert_eq!(lanes.len(), 3);
        assert_eq!(lanes.concat(), group);
        assert_eq!(worker_lane_count(0, 4), 0);
        assert_eq!(worker_lane_count(3, 0), 1);
    }

    #[test]
    fn reload_worker_chains_partition_boot_signatures_and_preserve_isolation() {
        let specs = ["ordinary", "reload", "submit", "budget", "alone", "reload"]
            .map(str::to_string);
        let wires = HashMap::from([
            ("reload".into(), env(&[("STONE_HOT_RELOAD", "1")])),
            ("submit".into(), env(&[("STONE_HOT_RELOAD", "1"), ("SIM_SKIP_RENDER", "submit")])),
            ("budget".into(), env(&[("STONE_HOT_RELOAD", "1"), ("FRAME_COUNT", "900")])),
            ("alone".into(), env(&[("STONE_HOT_RELOAD", "1"), ("SIM_WORKER_ISOLATED", "1")])),
        ]);
        let (ordinary, isolated) = group_worker_specs(&specs, &wires);
        let reload_indices: Vec<usize> = isolated.iter()
            .filter(|(i, _)| entry_is_reload_lane(&specs[*i], &wires))
            .map(|(i, _)| *i).collect();
        let (alone, chains) = group_reload_worker_specs(&reload_indices, &specs, &wires);
        let mut planned = ordinary.concat();
        planned.extend(&alone);
        planned.extend(chains.concat());
        planned.sort_unstable();
        assert_eq!(planned, (0..specs.len()).collect::<Vec<_>>(), "every occurrence has one runner");
        assert!(ordinary.iter().flatten().all(|&i| !entry_is_reload_lane(&specs[i], &wires)));
        assert_eq!(alone, vec![4], "the boot-scoped reload entry runs alone");
        for chain in &chains {
            let signature = boot_signature(&wires[&specs[chain[0]]]);
            assert!(chain.iter().all(|&i| boot_signature(&wires[&specs[i]]) == signature),
                "one reload worker must have one boot signature: {chain:?}");
        }
        assert_eq!(chains, vec![vec![1, 3, 5], vec![2]],
            "budgets share a boot; render modes split it, in first-appearance order");
    }

    #[test]
    fn reload_worker_chains_keep_incompatible_and_boot_scoped_parts_alone() {
        let specs = ["reload", "reload,submit", "reload,alone", "reload,ordinary", "reload,budget"]
            .map(str::to_string);
        let wires = HashMap::from([
            ("reload".into(), env(&[("STONE_HOT_RELOAD", "1")])),
            ("submit".into(), env(&[("STONE_HOT_RELOAD", "1"), ("SIM_SKIP_RENDER", "submit")])),
            ("alone".into(), env(&[("STONE_HOT_RELOAD", "1"), ("SIM_WORKER_ISOLATED", "1")])),
            ("budget".into(), env(&[("STONE_HOT_RELOAD", "1"), ("FRAME_COUNT", "900")])),
        ]);
        let indices: Vec<usize> = (0..specs.len()).collect();
        let (alone, chains) = group_reload_worker_specs(&indices, &specs, &wires);
        assert_eq!(alone, vec![1, 2, 3],
            "incompatible chain parts and any isolated part need fresh processes");
        assert_eq!(chains, vec![vec![0, 4]],
            "compatible comma-chain parts still share the reload worker");
    }

    #[test]
    fn worker_lane_plan_counts_only_active_workers_and_keeps_occurrences() {
        let mut specs = vec!["plain".to_string(); 2 * SIM_BATCH_QUANTUM + 1];
        specs.extend(["full", "isolated", "reload", "other_backend"].map(str::to_string));
        let env = HashMap::from([
            ("full".into(), vec![("STONE_ASSET_STUB".into(), "0".into()), ("STONE_IMAGE_MAX_SIDE".into(), "0".into())]),
            ("isolated".into(), vec![("SIM_WORKER_ISOLATED".into(), "1".into())]),
            ("reload".into(), vec![("STONE_HOT_RELOAD".into(), "1".into())]),
            ("other_backend".into(), vec![("SIM_REQUIRED_RENDERER".into(), "wgpu".into())]),
        ]);
        let plan = worker_lane_plan(&specs, &env, "gl", 14);
        assert_eq!(plan["selected_entries"], 2 * SIM_BATCH_QUANTUM + 5);
        assert_eq!(plan["backend_excluded"], 1);
        assert_eq!(plan["ordinary_entries"], 2 * SIM_BATCH_QUANTUM + 2);
        assert_eq!(plan["signature_entry_counts"], serde_json::json!([2 * SIM_BATCH_QUANTUM + 1, 1]));
        assert_eq!(plan["ordinary_lanes"], 4);
        assert_eq!(plan["isolated_entries"], 1);
        assert_eq!(plan["reload_entries"], 1);
        assert_eq!(plan["batch_quantum"], SIM_BATCH_QUANTUM);
    }

    #[test]
    fn plan_shows_seeded_lane_dispatch_order() {
        // Two boot groups on a fresh machine: the seeded-slower lane plans
        // first. Anchored on the checked-in table's own extremes so a regen
        // keeps this honest.
        let mut table = crate::sim_observability::seeded_table_snapshot();
        assert!(!table.is_empty(), "the checked-in seeded table must ship medians");
        table.sort_by_key(|(_, ms)| *ms);
        let (fast_name, fast_ms) = table.first().expect("seeded table").clone();
        let (slow_name, slow_ms) = table.last().expect("seeded table").clone();
        assert!(slow_ms > fast_ms, "a degenerate seeded table cannot order lanes");

        let mut env_by_wire: HashMap<String, Vec<(String, String)>> = HashMap::new();
        env_by_wire.insert(slow_name.clone(), env(&[("MAG_SCENE", "arena")]));
        env_by_wire.insert(fast_name.clone(), env(&[("MAG_SCENE", "chapel")]));
        let plan = worker_lane_plan(&[slow_name, fast_name], &env_by_wire, "gl", 14);
        assert_eq!(plan["signature_entry_counts"], serde_json::json!([1, 1]));
        assert_eq!(plan["ordinary_lanes"], 2);
        assert_eq!(
            plan["lane_dispatch_order"],
            serde_json::json!([0, 1]),
            "the seeded-slower lane (specs[0]) dispatches first on a fresh machine"
        );
        assert_eq!(plan["seeded_lanes"], 2);
    }

    #[test]
    fn worker_lane_count_is_ceil_of_entries_over_the_quantum_capped_by_jobs() {
        let q = SIM_BATCH_QUANTUM;
        for (entries, lanes) in [
            (0, 0), (1, 1), (14, 1), (15, 1), (q, 1), (q + 1, 2),
            (2 * q, 2), (2 * q + 1, 3), (196, 4), (1660, 14),
        ] {
            assert_eq!(worker_lane_count(entries, 14), lanes, "N={entries}");
        }
        assert_eq!(worker_lane_count(10 * q, 4), 4);
        assert_eq!(worker_lane_count(usize::MAX, 14), 14);
    }

    #[test]
    fn worker_lane_partition_covers_every_entry_once_in_group_order() {
        let groups = vec![(0..15).collect(), (15..1675).collect(), vec![], vec![1675]];
        for jobs in [1, 4, 14, 64] {
            let lanes = allocate_worker_lanes(&groups, jobs, &[]);
            assert_eq!(lanes.concat(), (0..1676).collect::<Vec<_>>());
            assert!(lanes.iter().all(|lane| !lane.is_empty()));
            for lane in lanes {
                assert!(groups.iter().any(|group| group.windows(lane.len()).any(|w| w == lane)));
            }
        }
    }

    /// A comma-chain is scheduled as one entry, so ONE reload part makes the
    /// whole entry exclusive. Missing that would let the session go live while
    /// other children exist.
    #[test]
    fn a_chain_is_reload_lane_if_any_part_is() {
        let mut env: HashMap<String, Vec<(String, String)>> = HashMap::new();
        env.insert("plain".into(), vec![("ROCK_TEST_SEED".into(), "42".into())]);
        env.insert(
            "reloader".into(),
            vec![("STONE_HOT_RELOAD".into(), "1".into())],
        );

        assert!(!entry_is_reload_lane("plain", &env));
        assert!(entry_is_reload_lane("reloader", &env));
        // Either position in the chain routes the entry to the lane.
        assert!(entry_is_reload_lane("plain,reloader", &env));
        assert!(entry_is_reload_lane("reloader,plain", &env));
        assert!(!entry_is_reload_lane("plain,plain", &env));
        // A name with no recorded env is ordinary, not a panic.
        assert!(!entry_is_reload_lane("unknown", &env));
    }

    /// A missing JSON baseline used to bless itself from the current run and
    /// count as a passing expect -- the same hole `handle_image_snapshot` had.
    /// It is worse under `--jobs`: two sims sharing a snapshot name can each
    /// write a different value and both pass, with the winner decided by
    /// scheduling. Only `--update-snapshots` may create a baseline.
    #[test]
    fn a_missing_json_baseline_is_not_a_pass() {
        let dir = std::env::temp_dir().join(format!(
            "rock-json-golden-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("tmp dir");
        let baseline = dir.join("golden.json");
        let snap = SnapshotOp {
            name: "golden".into(),
            value_json: r#"{"a":1}"#.into(),
            site: String::new(),
        };

        let r = handle_json_snapshot(&snap, &baseline, false).expect("no hard error");
        assert!(
            matches!(r, SnapResult::Fail(_)),
            "a missing JSON baseline must not pass; got {r:?}"
        );
        assert!(
            !baseline.exists(),
            "and it must NOT have been written behind our back"
        );

        // Both sides: with --update-snapshots, writing it is the point.
        let r = handle_json_snapshot(&snap, &baseline, true).expect("no hard error");
        assert!(matches!(r, SnapResult::Wrote), "update writes it; got {r:?}");
        assert!(baseline.exists(), "the baseline is created under update");

        // And an existing baseline still compares rather than being rewritten.
        let r = handle_json_snapshot(&snap, &baseline, false).expect("no hard error");
        assert!(matches!(r, SnapResult::Pass), "matching value passes; got {r:?}");

        std::fs::remove_dir_all(&dir).ok();
    }

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn a_declared_frame_count_is_the_entrys_budget() {
        assert_eq!(frame_budget_for("s", &env(&[])).expect("default"), DEFAULT_FRAME_BUDGET);
        assert_eq!(frame_budget_for("s", &env(&[("FRAME_COUNT", "90")])).expect("declared"), 90);
        // The two watchdog sims in testbed declare this; a runner that quietly
        // capped them at the default would guard them with a bound nobody wrote.
        assert_eq!(
            frame_budget_for("s", &env(&[("FRAME_COUNT", "100000")])).expect("declared"),
            100000
        );
        // Other keys are not budgets.
        assert_eq!(
            frame_budget_for("s", &env(&[("STONE_FX_OFF", "1")])).expect("default"),
            DEFAULT_FRAME_BUDGET
        );
    }

    #[test]
    fn an_unusable_frame_count_is_rejected_not_defaulted() {
        for bad in ["abc", "", "0", "-5", "12.5"] {
            assert!(
                frame_budget_for("s", &env(&[("FRAME_COUNT", bad)])).is_err(),
                "`FRAME_COUNT={bad}` must reject rather than silently become the default"
            );
        }
    }

    #[test]
    fn a_frame_count_uses_the_last_declaration() {
        assert_eq!(
            frame_budget_for("s", &env(&[("FRAME_COUNT", "80"), ("FRAME_COUNT", "90")]))
                .expect("last declaration is valid"),
            90
        );
        assert_eq!(
            frame_budget_for("s", &env(&[("FRAME_COUNT", "bad"), ("FRAME_COUNT", "90")]))
                .expect("an overridden invalid declaration is inert"),
            90
        );
        assert!(
            frame_budget_for("s", &env(&[("FRAME_COUNT", "90"), ("FRAME_COUNT", "bad")]))
                .is_err(),
            "the effective last declaration is the one that must be validated"
        );
    }

    #[test]
    fn the_boot_signature_uses_effective_defaults_and_last_wins() {
        assert_eq!(
            sig(&[("SIM_SKIP_RENDER", "1")]),
            sig(&[]),
            "explicit skip-render default matches the runner-published default"
        );
        assert_eq!(
            sig(&[("SIM_SKIP_RENDER", "0"), ("SIM_SKIP_RENDER", "1")]),
            sig(&[]),
            "an overridden render value does not split on its discarded value"
        );
        assert_ne!(
            sig(&[("SIM_SKIP_RENDER", "1"), ("SIM_SKIP_RENDER", "0")]),
            sig(&[]),
            "the effective explicit render opt-in remains a boot difference"
        );
        assert_eq!(
            sig(&[("OTHER", "old"), ("OTHER", "new")]),
            sig(&[("OTHER", "new")]),
            "ordinary environment declarations use last-wins semantics too"
        );
    }

    #[test]
    fn the_boot_signature_excludes_only_runner_owned_keys() {
        let a = env(&[("FRAME_COUNT", "90"), ("MAG_SCENE", "arena")]);
        let b = env(&[("FRAME_COUNT", "40"), ("MAG_SCENE", "arena")]);
        assert_eq!(boot_signature(&a), boot_signature(&b), "the budget is per entry");

        let c = env(&[("FRAME_COUNT", "90"), ("MAG_SCENE", "chapel")]);
        assert_ne!(boot_signature(&a), boot_signature(&c), "a boot-consumed value is not");
        // STONE_WINDOW is applied per entry by `sim_start_entry`.
        let w = env(&[("STONE_WINDOW", "640x360")]);
        assert_eq!(boot_signature(&w), env(&[]), "the window is per entry");
        // STONE_FX_OFF is a per-frame gate re-applied at every boundary, so
        // an ablating entry and a plain one share a boot.
        let g = env(&[("STONE_FX_OFF", "draw")]);
        assert_eq!(boot_signature(&g), env(&[]), "ablation is per entry");
        // SIM_DT is carried in the entry plan and re-applied by
        // `sim_start_entry`, so two entries that disagree on it share a boot.
        let d = env(&[("SIM_DT", "0.1")]);
        let e = env(&[("SIM_DT", "0")]);
        assert_eq!(boot_signature(&d), boot_signature(&e), "the fixed dt is per entry");
        assert_eq!(boot_signature(&d), env(&[]), "and an undeclared one shares too");
        // SIM_SKIP_RENDER decides the process's asset stub, so it stays in
        // even though it is also carried per entry.
        let f = env(&[("SIM_SKIP_RENDER", "0")]);
        assert_eq!(boot_signature(&f), f);
    }

    #[test]
    fn the_plan_carries_the_runner_owned_pairs_for_every_entry() {
        // An entry that declares nothing still carries both keys: the empty
        // dt is the engine default, and the skip flag is the baseline.
        assert_eq!(
            runner_owned_plan_pairs(&env(&[])),
            env(&[("SIM_DT", ""), ("SIM_SKIP_RENDER", "1"), ("STONE_FX_OFF", ""), ("STONE_WINDOW", "")])
        );
        assert_eq!(
            runner_owned_plan_pairs(&env(&[
                ("SIM_DT", "0.5"),
                ("SIM_SKIP_RENDER", "0"),
                ("STONE_FX_OFF", "draw,ui"),
                ("STONE_WINDOW", "640x360")
            ])),
            env(&[
                ("SIM_DT", "0.5"),
                ("SIM_SKIP_RENDER", "0"),
                ("STONE_FX_OFF", "draw,ui"),
                ("STONE_WINDOW", "640x360")
            ])
        );
        // Last-wins, the same reading `entry_renders` and the child's env use.
        assert_eq!(
            runner_owned_plan_pairs(&env(&[("SIM_SKIP_RENDER", "0"), ("SIM_SKIP_RENDER", "1")])),
            env(&[("SIM_DT", ""), ("SIM_SKIP_RENDER", "1"), ("STONE_FX_OFF", ""), ("STONE_WINDOW", "")])
        );
        // The raw value travels: `submit` must reach the engine as itself.
        assert_eq!(
            runner_owned_plan_pairs(&env(&[("SIM_SKIP_RENDER", "submit")])),
            env(&[("SIM_DT", ""), ("SIM_SKIP_RENDER", "submit"), ("STONE_FX_OFF", ""), ("STONE_WINDOW", "")])
        );
    }

    #[test]
    fn the_boot_signature_is_declaration_order_independent() {
        let a = env(&[("MAG_SCENE", "arena"), ("SIM_DT", "0.1")]);
        let b = env(&[("SIM_DT", "0.1"), ("MAG_SCENE", "arena")]);
        assert_eq!(boot_signature(&a), boot_signature(&b), "same pairs, either spelling");

        // The sort is stable, so a duplicated key keeps last-wins order and a
        // genuine effective difference still refuses.
        let dup_a = env(&[("K", "1"), ("K", "2")]);
        let dup_b = env(&[("K", "2"), ("K", "1")]);
        assert_ne!(boot_signature(&dup_a), boot_signature(&dup_b));
    }

    #[test]
    fn the_boot_signature_ignores_per_entry_declaration_order() {
        // keyboard_spell_drag boots alone while its compact/summary siblings
        // share a boot: same per-entry key set, declared in a different order.
        let drag = env(&[
            (
                "SIM_WORKER_PER_ENTRY_ENV",
                "MAG_BINDINGS_PATH_OVERRIDE,MAG_BINDINGS_HISTORY_PATH_OVERRIDE,MAG_SETTINGS_PATH_OVERRIDE",
            ),
            ("MAG_BINDINGS_PATH_OVERRIDE", "$OUT/keyboard-drag-bindings.json"),
            ("MAG_BINDINGS_HISTORY_PATH_OVERRIDE", "$OUT/keyboard-drag-history.jsonl"),
            ("MAG_SETTINGS_PATH_OVERRIDE", "$OUT/keyboard-drag-settings.json"),
        ]);
        let compact = env(&[
            (
                "SIM_WORKER_PER_ENTRY_ENV",
                "MAG_SETTINGS_PATH_OVERRIDE,MAG_BINDINGS_PATH_OVERRIDE,MAG_BINDINGS_HISTORY_PATH_OVERRIDE",
            ),
            ("MAG_SETTINGS_PATH_OVERRIDE", "$OUT/keyboard-compact-settings.json"),
            ("MAG_BINDINGS_PATH_OVERRIDE", "$OUT/keyboard-compact-bindings.json"),
            ("MAG_BINDINGS_HISTORY_PATH_OVERRIDE", "$OUT/keyboard-compact-history.jsonl"),
        ]);
        assert_eq!(
            boot_signature(&drag),
            boot_signature(&compact),
            "the same per-entry key set shares a boot however it is spelled"
        );

        let subset = env(&[
            (
                "SIM_WORKER_PER_ENTRY_ENV",
                "MAG_BINDINGS_PATH_OVERRIDE,MAG_SETTINGS_PATH_OVERRIDE",
            ),
            ("MAG_BINDINGS_PATH_OVERRIDE", "$OUT/other-bindings.json"),
            ("MAG_SETTINGS_PATH_OVERRIDE", "$OUT/other-settings.json"),
        ]);
        assert_ne!(
            boot_signature(&drag),
            boot_signature(&subset),
            "a different per-entry key set still refuses to share"
        );
    }

    fn wires(entries: &[(&str, &[(&str, &str)])]) -> HashMap<String, Vec<(String, String)>> {
        entries
            .iter()
            .map(|(name, pairs)| (name.to_string(), env(pairs)))
            .collect()
    }

    fn specs(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn worker_grouping_shares_a_signature_and_isolates_with_reasons() {
        let wires = wires(&[
            ("plain_a", &[]),
            ("plain_b", &[("FRAME_COUNT", "40")]),
            ("scene", &[("MAG_SCENE", "arena")]),
            ("reload", &[("STONE_HOT_RELOAD", "1")]),
            ("outref", &[("SESSION", "$OUT/resume.json")]),
            ("machinery", &[("SIM_WORKER_ISOLATED", "1")]),
        ]);
        let (groups, isolated) = group_worker_specs(
            &specs(&["plain_a", "plain_b", "scene", "reload", "outref", "machinery"]),
            &wires);

        // FRAME_COUNT is runner-owned, so plain_a and plain_b share a group;
        // fx_off's boot-consumed value is its own.
        assert_eq!(groups, vec![vec![0, 1], vec![2]]);

        assert_eq!(isolated.len(), 3);
        assert_eq!(isolated[0].0, 3);
        assert!(isolated[0].1.contains("hot-reload"), "{}", isolated[0].1);
        assert_eq!(isolated[1].0, 4);
        assert!(isolated[1].1.contains("$OUT"), "{}", isolated[1].1);
        assert!(
            !isolated[1].1.contains("hot-reload"),
            "the $OUT refusal must carry its OWN reason: {}",
            isolated[1].1
        );
        assert_eq!(isolated[2].0, 5);
        assert!(isolated[2].1.contains("SIM_WORKER_ISOLATED"), "{}", isolated[2].1);
    }

    /// The declared opt-in admits a `$OUT` key, and ONLY the declared one: an
    /// entry naming one variable per-entry still isolates on a second
    /// undeclared `$OUT` variable, which is what keeps the conservative
    /// direction the default.
    #[test]
    fn a_declared_per_entry_key_is_admitted_and_an_undeclared_one_is_not() {
        let wires = wires(&[
            ("declared", &[("SIM_WORKER_PER_ENTRY_ENV", "ROOT"), ("ROOT", "$OUT/telemetry")]),
            (
                "partly",
                &[
                    ("SIM_WORKER_PER_ENTRY_ENV", "ROOT"),
                    ("ROOT", "$OUT/telemetry"),
                    ("BINDINGS", "$OUT/bindings.json"),
                ],
            ),
            ("undeclared", &[("ROOT", "$OUT/telemetry")]),
        ]);
        let (groups, isolated) =
            group_worker_specs(&specs(&["declared", "partly", "undeclared"]), &wires);

        assert_eq!(groups, vec![vec![0]], "the fully declared entry is eligible");
        assert_eq!(isolated.len(), 2);
        assert_eq!(isolated[0].0, 1);
        assert!(
            isolated[0].1.contains("BINDINGS"),
            "the refusal must name the UNDECLARED variable, not the declared one: {}",
            isolated[0].1
        );
        assert_eq!(isolated[1].0, 2);
        assert!(isolated[1].1.contains("ROOT"), "{}", isolated[1].1);
    }

    /// Two entries whose only difference is the VALUE of a declared per-entry
    /// key share a worker; an entry that does not carry the key at all does
    /// not. That second half is the clearing boundary: publication is dense
    /// within a lane, so no entry can inherit a predecessor's value.
    #[test]
    fn a_per_entry_key_splits_on_presence_and_not_on_value() {
        let decl = ("SIM_WORKER_PER_ENTRY_ENV", "ROOT");
        let a = env(&[decl, ("ROOT", "$OUT/telemetry")]);
        let b = env(&[decl, ("ROOT", "$OUT/menu-telemetry")]);
        assert_eq!(
            boot_signature(&a),
            boot_signature(&b),
            "the value is republished per entry, so it is not boot state"
        );

        let absent = env(&[decl]);
        assert_ne!(
            boot_signature(&a),
            boot_signature(&absent),
            "an entry that publishes nothing for the key must not share a worker with one \
             that does, or it would inherit the predecessor's value"
        );

        let undeclared = env(&[("ROOT", "$OUT/telemetry")]);
        assert_ne!(
            boot_signature(&a),
            boot_signature(&undeclared),
            "without the declaration the key is ordinary boot state"
        );
    }

    /// Each part resolves `$OUT` against its OWN directory -- the whole point,
    /// since a shared spawn environment can only carry the first entry's.
    #[test]
    fn per_entry_pairs_resolve_out_against_the_parts_own_directory() {
        let part = |name: &str, out: &str| WorkerPart {
            name: name.to_string(),
            out_dir: PathBuf::from(out),
            frame_budget: 0,
            env: env(&[
                ("SIM_WORKER_PER_ENTRY_ENV", "ROOT"),
                ("ROOT", "$OUT/telemetry"),
                ("OTHER", "$OUT/ignored"),
            ]),
            occurrence_id: 0,
            part_index: 0,
        };
        assert_eq!(
            per_entry_env_pairs(&part("a", "/tmp/one")),
            vec![("ROOT".to_string(), "/tmp/one/telemetry".to_string())],
            "only the declared key is published, resolved against this part's $OUT"
        );
        assert_eq!(
            per_entry_env_pairs(&part("b", "/tmp/two")),
            vec![("ROOT".to_string(), "/tmp/two/telemetry".to_string())]
        );

        let plain = WorkerPart {
            name: "plain".to_string(),
            out_dir: PathBuf::from("/tmp/three"),
            frame_budget: 0,
            env: env(&[("ROOT", "/fixed")]),
            occurrence_id: 0,
            part_index: 0,
        };
        assert!(per_entry_env_pairs(&plain).is_empty(), "no declaration publishes nothing");
    }

    /// Last-wins applies to the declaration itself and to the value it names,
    /// so the pair published matches the pair the signature recorded.
    #[test]
    fn per_entry_keys_and_values_use_effective_declarations() {
        let e = env(&[
            ("SIM_WORKER_PER_ENTRY_ENV", "STALE"),
            ("SIM_WORKER_PER_ENTRY_ENV", " ROOT , OTHER ,"),
            ("ROOT", "$OUT/first"),
            ("ROOT", "$OUT/second"),
        ]);
        assert_eq!(per_entry_env_keys(&e), vec!["ROOT".to_string(), "OTHER".to_string()]);

        let part = WorkerPart {
            name: "p".to_string(),
            out_dir: PathBuf::from("/tmp/out"),
            frame_budget: 0,
            env: e,
            occurrence_id: 0,
            part_index: 0,
        };
        assert_eq!(
            per_entry_env_pairs(&part),
            vec![("ROOT".to_string(), "/tmp/out/second".to_string())],
            "the effective value is published; a named key that is absent publishes nothing"
        );
    }

    /// A suite may name the same sim twice. Both occurrences are entries that
    /// must be scheduled and must each own a result, so admission carries spec
    /// POSITIONS -- keying by spelling collapses them into one.
    #[test]
    fn a_duplicated_spec_is_two_scheduled_entries() {
        let wires = wires(&[("plain", &[]), ("reload", &[("STONE_HOT_RELOAD", "1")])]);

        let (groups, isolated) =
            group_worker_specs(&specs(&["plain", "plain", "reload", "reload"]), &wires);
        assert_eq!(groups, vec![vec![0, 1]]);
        assert_eq!(isolated.iter().map(|(i, _)| *i).collect::<Vec<_>>(), vec![2, 3]);

        // Every position is scheduled exactly once, whatever it was named.
        let mut seen: Vec<usize> =
            groups.concat().into_iter().chain(isolated.iter().map(|(i, _)| *i)).collect();
        seen.sort_unstable();
        assert_eq!(seen, vec![0, 1, 2, 3]);
    }

    #[test]
    fn a_chain_groups_by_its_parts_and_disagreeing_parts_isolate() {
        let wires = wires(&[
            ("head", &[("MAG_SCENE", "arena")]),
            ("tail_same", &[("MAG_SCENE", "arena"), ("FRAME_COUNT", "30")]),
            ("tail_other", &[("MAG_SCENE", "chapel")]),
            ("plain", &[]),
        ]);

        let (groups, isolated) =
            group_worker_specs(&specs(&["head,tail_same", "plain"]), &wires);
        assert_eq!(groups, vec![vec![0], vec![1]]);
        assert!(isolated.is_empty());

        let (groups, isolated) = group_worker_specs(&specs(&["head,tail_other"]), &wires);
        assert!(groups.is_empty());
        assert_eq!(isolated.len(), 1);
        assert!(
            isolated[0].1.contains("disagree"),
            "a mixed chain must say why it stays isolated: {}",
            isolated[0].1
        );
    }

    /// An empty spec is a user error with an existing message, not a crash:
    /// admission must classify it rather than index its first part.
    #[test]
    fn an_empty_spec_is_classified_not_indexed() {
        let wires = wires(&[("plain", &[])]);

        assert!(worker_ineligibility("", &wires).is_some());
        assert!(worker_ineligibility(" , ", &wires).is_some());

        let (groups, isolated) = group_worker_specs(&specs(&["plain", ""]), &wires);
        assert_eq!(groups, vec![vec![0]]);
        assert_eq!(isolated.len(), 1);
        assert_eq!(isolated[0].0, 1);
        assert!(isolated[0].1.contains("empty sim name"), "{}", isolated[0].1);
    }

    #[test]
    fn buffered_runs_never_write_live() {
        assert!(writes_live(false));
        assert!(!writes_live(true));
    }

    #[test]
    fn deferred_artifacts_run_before_their_out_dir_is_dropped() {
        let artifacts = SimArtifacts::new().expect("sim artifacts");
        let out_path = artifacts.path().to_path_buf();
        let marker = out_path.join("marker");
        std::fs::write(&marker, "live").expect("write marker");
        let mut summary = SimSummary::default();

        artifacts.finish_with(&mut summary, |_| {
            assert!(marker.exists(), "$OUT was dropped before deferred processing");
        });

        assert!(!out_path.exists(), "$OUT must be removed after processing");
    }

    fn worker_parts(names: &[&str]) -> Vec<WorkerPart> {
        names
            .iter()
            .map(|n| WorkerPart {
                name: (*n).to_string(),
                out_dir: PathBuf::from("/tmp/out"),
                frame_budget: 600,
                env: Vec::new(),
                occurrence_id: 0,
                part_index: 0,
            })
            .collect()
    }

    fn lines(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|s| s.to_string()).collect()
    }

    /// The deadline anchor resets ONLY on `EnvelopeStep::Started`, so the
    /// tracker's classification IS the guard's admission: a start the demux
    /// would reject -- malformed, out of order, duplicate, while another
    /// entry is open, or past the queue -- must not read as Started, and
    /// after any violation the live reader stops consulting the tracker
    /// entirely. A valid next start, and only that, is Started.
    #[test]
    fn only_an_accepted_entry_start_reads_as_started() {
        let parts = worker_parts(&["a", "b"]);
        let started = |t: &mut EnvelopeTracker, line: &str| {
            matches!(t.observe(line), Ok(EnvelopeStep::Started(_)))
        };

        let mut t = EnvelopeTracker::for_parts(&parts);
        assert!(started(&mut t, &start(0, "a")), "the expected first start is Started");

        // Inside the open entry, every start shape refuses: the wedged-sim
        // evasion is exactly a start emitted while its own entry is open.
        assert!(t.observe(r#"__SIM__ {"op":"entry_start"}"#).is_err(), "malformed: no seq");
        // A fresh tracker still refuses the malformed shape with nothing open.
        assert!(
            EnvelopeTracker::for_parts(&parts)
                .observe(r#"__SIM__ {"op":"entry_start"}"#)
                .is_err(),
            "malformed: no seq, no entry open"
        );
        assert!(t.observe(&start(0, "a")).is_err(), "duplicate of the open entry");
        assert!(t.observe(&start(1, "b")).is_err(), "the next entry cannot start while one is open");

        assert!(matches!(
            t.observe(&end(0, "a")),
            Ok(EnvelopeStep::Ended(0, None, None, None))
        ));
        assert!(t.observe(&start(0, "a")).is_err(), "a closed entry cannot restart");
        assert!(started(&mut t, &start(1, "b")), "the valid NEXT start is Started");
        assert!(matches!(
            t.observe(&end(1, "b")),
            Ok(EnvelopeStep::Ended(1, None, None, None))
        ));
        assert!(t.observe(&start(2, "c")).is_err(), "past the end of the queue");

        let mut t = EnvelopeTracker::for_parts(&parts);
        assert!(t.observe(&start(1, "b")).is_err(), "out of order: entry 1 before entry 0");
        let mut t = EnvelopeTracker::for_parts(&parts);
        assert!(t.observe(&start(0, "b")).is_err(), "wrong name for the seq");
    }

    /// The wedged-child evasion, deterministically: a sim stuck inside one
    /// tick can flood `entry_start`-shaped lines, and the deadline may reset
    /// on NONE of them -- only on a start the strict envelope accepts. After
    /// any violation the guard is poisoned, so even a line that would
    /// otherwise be the valid next start extends nothing.
    #[test]
    fn the_deadline_guard_resets_only_on_accepted_starts() {
        let parts = worker_parts(&["a", "b"]);

        let mut g = DeadlineGuard::for_parts(&parts);
        assert!(g.observe_resets(&start(0, "a")), "the expected first start resets");
        assert!(!g.observe_resets(&expect_ok("inside a")), "an ordinary op does not");
        assert!(!g.observe_resets("[   0.1s INFO ] raw logging"), "raw output does not");
        // Every start shape a wedged sim can emit while its own entry is open.
        assert!(!g.observe_resets(r#"__SIM__ {"op":"entry_start"}"#), "malformed: no seq");
        assert!(!g.observe_resets(&start(0, "a")), "duplicate of the open entry");
        assert!(!g.observe_resets(&start(1, "b")), "next entry while one is open");
        // Poisoned by the violations above: the sequence end(0) -> start(1)
        // WOULD be valid to a fresh envelope, and must extend nothing now.
        assert!(!g.observe_resets(&end(0, "a")));
        assert!(
            !g.observe_resets(&start(1, "b")),
            "a poisoned guard must not resume resetting on later well-formed lines"
        );

        // The healthy path keeps resetting across the whole queue.
        let mut g = DeadlineGuard::for_parts(&parts);
        assert!(g.observe_resets(&start(0, "a")));
        assert!(!g.observe_resets(&end(0, "a")), "an entry's end is not a reset");
        assert!(g.observe_resets(&start(1, "b")), "the valid next start resets");
    }

    #[test]
    fn entry_frame_timing_is_validated_and_retained() {
        let parts = worker_parts(&["a"]);
        let mut tracker = EnvelopeTracker::for_parts(&parts);
        assert!(tracker.observe(&start(0, "a")).is_ok());
        let valid = end_with_frame_timing_and_boundary(
            0,
            "a",
            3,
            r#"{"frames":3,"draw_frames":2,"submitted_frames":1,"logic_us":10,"render_cpu_us":20,"present_us":30}"#,
            r#"{"native_reset_us":40,"teardown_us":5,"boot_restore_us":50}"#,
        );
        assert!(matches!(
            tracker.observe(&valid),
            Ok(EnvelopeStep::Ended(
                0,
                Some(3),
                Some(EntryFrameTiming {
                    frames: 3,
                    draw_frames: 2,
                    submitted_frames: 1,
                    logic_us: 10,
                    render_cpu_us: 20,
                    present_us: 30,
                }),
                Some(EntryBoundaryTiming {
                    native_reset_us: 40,
                    teardown_us: 5,
                    boot_restore_us: Some(50),
                })
            ))
        ));

        for timing in [
            r#"{"frames":3,"draw_frames":4,"submitted_frames":1,"logic_us":10,"render_cpu_us":20,"present_us":30}"#,
            r#"{"frames":2,"draw_frames":2,"submitted_frames":1,"logic_us":10,"render_cpu_us":20,"present_us":30}"#,
            r#"{"frames":3,"draw_frames":2,"submitted_frames":1,"logic_us":-1,"render_cpu_us":20,"present_us":30}"#,
        ] {
            let mut tracker = EnvelopeTracker::for_parts(&parts);
            assert!(tracker.observe(&start(0, "a")).is_ok());
            assert!(
                tracker
                    .observe(&end_with_frame_timing(0, "a", 3, timing))
                    .is_err(),
                "invalid frame timing must reject: {timing}"
            );
        }

        for boundary in [
            r#"{"native_reset_us":40,"boot_restore_us":50}"#,
            r#"{"native_reset_us":-1,"teardown_us":5,"boot_restore_us":50}"#,
            r#"{"native_reset_us":40,"teardown_us":5,"boot_restore_us":-1}"#,
        ] {
            let mut tracker = EnvelopeTracker::for_parts(&parts);
            assert!(tracker.observe(&start(0, "a")).is_ok());
            assert!(
                tracker
                    .observe(&end_with_frame_timing_and_boundary(
                        0,
                        "a",
                        3,
                        r#"{"frames":3,"draw_frames":2,"submitted_frames":1,"logic_us":10,"render_cpu_us":20,"present_us":30}"#,
                        boundary,
                    ))
                    .is_err(),
                "invalid boundary timing must reject: {boundary}"
            );
        }
    }

    fn start(seq: usize, name: &str) -> String {
        format!(r#"__SIM__ {{"op":"entry_start","name":"{name}","seq":{seq}}}"#)
    }

    fn end(seq: usize, name: &str) -> String {
        format!(r#"__SIM__ {{"op":"entry_end","name":"{name}","seq":{seq}}}"#)
    }

    fn end_with_frame_count(seq: usize, name: &str, frame_count: usize) -> String {
        format!(r#"__SIM__ {{"op":"entry_end","name":"{name}","seq":{seq},"frame_count":{frame_count}}}"#)
    }

    fn end_with_raw_frame_count(seq: usize, name: &str, frame_count: &str) -> String {
        format!(r#"__SIM__ {{"op":"entry_end","name":"{name}","seq":{seq},"frame_count":{frame_count}}}"#)
    }

    fn end_with_frame_timing(seq: usize, name: &str, frame_count: usize, timing: &str) -> String {
        format!(
            r#"__SIM__ {{"op":"entry_end","name":"{name}","seq":{seq},"frame_count":{frame_count},"frame_timing":{timing}}}"#
        )
    }

    fn end_with_frame_timing_and_boundary(
        seq: usize,
        name: &str,
        frame_count: usize,
        frame_timing: &str,
        boundary_timing: &str,
    ) -> String {
        format!(
            r#"__SIM__ {{"op":"entry_end","name":"{name}","seq":{seq},"frame_count":{frame_count},"frame_timing":{frame_timing},"boundary_timing":{boundary_timing}}}"#
        )
    }

    fn expect_ok(msg: &str) -> String {
        format!(r#"__SIM__ {{"op":"expect_ok","message":"{msg}","site":"f.rock:1"}}"#)
    }

    /// The whole point of the envelope: one sim's operations may never be filed
    /// under another's name, and boot/teardown logging is not part of an entry.
    #[test]
    fn the_envelope_partitions_operations_and_in_entry_logging() {
        let parts = worker_parts(&["a", "b"]);
        let stream = vec![
            "[   0.1s INFO ] booting".to_string(),
            start(0, "a"),
            "[   0.2s INFO ] inside a".to_string(),
            expect_ok("from a"),
            end_with_frame_count(0, "a", 7),
            "[   0.3s INFO ] between entries".to_string(),
            start(1, "b"),
            expect_ok("from b"),
            end_with_frame_count(1, "b", 11),
            "[   0.4s INFO ] teardown".to_string(),
        ];

        let out = split_worker_stream(&stream, &parts).expect("well-formed stream");
        assert_eq!(out[0].sim, vec![expect_ok("from a")]);
        assert_eq!(out[1].sim, vec![expect_ok("from b")]);
        assert!(out[0].started && out[0].terminated);
        assert!(out[1].started && out[1].terminated);
        assert_eq!(out[0].frame_count, Some(7));
        assert_eq!(out[1].frame_count, Some(11));

        // In-entry logging excludes boot, teardown and the gap between entries,
        // so the two sides of a differential look at the same window.
        assert_eq!(out[0].raw_in_entry, vec!["[   0.2s INFO ] inside a".to_string()]);
        assert!(out[1].raw_in_entry.is_empty());
        // The debugging stream still keeps everything.
        assert!(out[0].raw_all.iter().any(|l| l.contains("booting")));
        assert!(out[1].raw_all.iter().any(|l| l.contains("teardown")));
    }

    /// A worker killed mid-entry leaves its last entry open. That is the ONE
    /// tolerated gap, and it is recorded rather than read as a clean close.
    #[test]
    fn a_killed_worker_leaves_its_last_entry_unterminated() {
        let parts = worker_parts(&["a", "b"]);
        let stream = vec![start(0, "a"), end(0, "a"), start(1, "b"), expect_ok("partial")];

        let out = split_worker_stream(&stream, &parts).expect("a truncated stream is not malformed");
        assert!(out[0].terminated);
        assert!(out[1].started && !out[1].terminated);
        assert_eq!(out[1].sim, vec![expect_ok("partial")]);
    }

    #[test]
    fn malformed_entry_frame_counts_are_rejected() {
        let parts = worker_parts(&["a"]);
        for raw in ["-1", "1.5", "null", "\"seven\""] {
            let stream = vec![start(0, "a"), end_with_raw_frame_count(0, "a", raw)];
            assert!(
                split_worker_stream(&stream, &parts).is_err(),
                "invalid frame_count {raw} was accepted"
            );
        }
    }

    /// Every other shape is a protocol violation. Attributing a stray operation
    /// to a neighbour is precisely the failure this layer exists to prevent, so
    /// none of these may be quietly absorbed.
    #[test]
    fn malformed_envelopes_are_rejected_not_guessed_at() {
        let parts = worker_parts(&["a", "b"]);
        let cases: Vec<(&str, Vec<String>)> = vec![
            ("a second start while one is open", vec![start(0, "a"), start(1, "b")]),
            ("a repeated start", vec![start(0, "a"), end(0, "a"), start(0, "a")]),
            ("an out-of-order start", vec![start(1, "b")]),
            ("an end with no entry open", vec![end(0, "a")]),
            (
                "an end whose seq is not the open entry",
                vec![start(0, "a"), end(1, "b")],
            ),
            (
                "an end whose name is not the open entry",
                vec![start(0, "a"), end(0, "b")],
            ),
            (
                "a start whose name is not the queued sim",
                vec![start(0, "wrong")],
            ),
            (
                "a start with no seq",
                lines(&[r#"__SIM__ {"op":"entry_start","name":"a"}"#]),
            ),
            (
                "a start with no name",
                lines(&[r#"__SIM__ {"op":"entry_start","seq":0}"#]),
            ),
            ("an operation before any entry", vec![expect_ok("orphan")]),
            (
                "an operation between entries",
                vec![start(0, "a"), end(0, "a"), expect_ok("orphan")],
            ),
            (
                "a start past the end of the queue",
                vec![
                    start(0, "a"),
                    end(0, "a"),
                    start(1, "b"),
                    end(1, "b"),
                    start(2, "c"),
                ],
            ),
            (
                "an operation after the last entry",
                vec![
                    start(0, "a"),
                    end(0, "a"),
                    start(1, "b"),
                    end(1, "b"),
                    expect_ok("orphan"),
                ],
            ),
        ];

        for (what, stream) in cases {
            assert!(
                split_worker_stream(&stream, &parts).is_err(),
                "{what} was accepted; a stray op would land on the wrong sim"
            );
        }

        // Same guard against a ONE-entry queue, where `next_expected` reaches
        // the queue length after a single clean entry.
        let one = worker_parts(&["a"]);
        assert!(
            split_worker_stream(&[start(0, "a"), end(0, "a"), start(1, "b")], &one).is_err(),
            "a start past the end of a one-entry queue was accepted"
        );
    }

    /// Hashing `$OUT` reads every byte a sim produced -- for a game writing
    /// video that is exactly the cost this plan exists to remove, and in a
    /// parallel suite it would happen under the reporting lock. Only the
    /// differential oracle asks for a manifest.
    #[test]
    fn ordinary_runs_do_not_hash_their_out_directory() {
        for (recording, expected) in [(false, 0), (true, 1)] {
            let artifacts = SimArtifacts::with_manifest(recording).expect("sim artifacts");
            std::fs::write(artifacts.path().join("produced.bin"), "bytes").expect("write");
            let mut summary = SimSummary::default();
            artifacts.finish_with(&mut summary, |_| {});
            assert_eq!(
                summary.artifacts.len(),
                expected,
                "record_manifest={recording} produced the wrong manifest"
            );
            assert!(summary.errors.is_empty(), "{:?}", summary.errors.len());
        }
    }

    #[test]
    fn extract_str_field_basic() {
        let json = r#"{"op":"expect_fail","message":"1 segment"}"#;
        assert_eq!(extract_str_field(json, "op"), Some("expect_fail".into()));
        assert_eq!(
            extract_str_field(json, "message"),
            Some("1 segment".into())
        );
        assert_eq!(extract_str_field(json, "missing"), None);
    }

    #[test]
    fn extract_str_field_handles_escapes() {
        let json = r#"{"message":"hello \"world\""}"#;
        assert_eq!(
            extract_str_field(json, "message"),
            Some(r#"hello "world""#.into())
        );
    }

    #[test]
    fn extract_raw_field_object_value() {
        let line = r#"{"op":"snapshot","name":"x","value":{"a":1,"b":[2,3]}}"#;
        assert_eq!(
            extract_raw_field(line, "value"),
            Some(r#"{"a":1,"b":[2,3]}"#.to_string())
        );
    }

    #[test]
    fn extract_raw_field_number_and_bool() {
        let line = r#"{"tol":0.001,"ok":true,"path":"/tmp/x"}"#;
        assert_eq!(extract_raw_field(line, "tol"), Some("0.001".into()));
        assert_eq!(extract_raw_field(line, "ok"), Some("true".into()));
    }

    #[test]
    fn extract_raw_field_handles_quotes_inside_strings() {
        // The "value" field contains a string with an escaped `}`.
        // Naive depth tracking would stop early at the string's `}`.
        let line = r#"{"value":{"msg":"abc}def"},"after":1}"#;
        assert_eq!(
            extract_raw_field(line, "value"),
            Some(r#"{"msg":"abc}def"}"#.to_string())
        );
    }

    #[test]
    fn requested_sim_names_resolve_to_registered_wire_names() {
        let wires: Vec<String> = ["game::boots", "game::ui::hover", "game::hover", "legacy_sim"]
            .into_iter()
            .map(String::from)
            .collect();
        let resolve = |specs: &[&str]| {
            resolve_sim_specs(&wires, &specs.iter().map(|s| s.to_string()).collect::<Vec<_>>())
        };
        assert_eq!(resolve(&["boots"]).unwrap(), ["game::boots"]);
        assert_eq!(resolve(&["legacy_sim", "game::hover"]).unwrap(), ["legacy_sim", "game::hover"]);
        assert_eq!(resolve(&["ui::hover"]).unwrap(), ["game::ui::hover"]);
        assert_eq!(resolve(&["boots, ui::hover"]).unwrap(), ["game::boots,game::ui::hover"]);
        let error = resolve(&["oots"]).unwrap_err().to_string();
        assert!(error.contains("unknown sim: oots"), "only whole segments match: {error}");
        let error = resolve(&["hover"]).unwrap_err().to_string();
        assert!(
            error.contains("`hover` is ambiguous") && error.contains("game::ui::hover") && error.contains("game::hover"),
            "{error}"
        );
    }

    // A name the registry lacks used to pass through to the worker's entry
    // plan, where a name holding a line break (one argv element of
    // newline-joined names) split the plan line and killed the whole worker
    // with `malformed entry plan line: <a valid sim>`.
    #[test]
    fn requested_sim_names_that_are_not_sims_are_all_refused_before_boot() {
        let wires: Vec<String> = ["game::boots", "game::hover"].into_iter().map(String::from).collect();
        let specs: Vec<String> =
            ["boots", "helper_probe", "boots\nhover", "hover,missing_part", "other_helper"]
                .into_iter()
                .map(String::from)
                .collect();
        let error = resolve_sim_specs(&wires, &specs).unwrap_err().to_string();
        for expected in [
            "unknown sim: helper_probe",
            "unknown sim: missing_part",
            "unknown sim: other_helper",
            "\"boots\\nhover\" contains whitespace",
        ] {
            assert!(error.contains(expected), "missing `{expected}` in:\n{error}");
        }
        assert!(!error.contains("unknown sim: boots"), "a valid sim must not be named unknown:\n{error}");
    }

    #[test]
    fn an_entry_plan_field_that_would_split_the_line_is_refused() {
        let part = |name: &str, env: Vec<(String, String)>| WorkerPart {
            name: name.to_string(),
            out_dir: PathBuf::from("/tmp/out"),
            frame_budget: 60,
            env,
            occurrence_id: 0,
            part_index: 0,
        };
        let line = entry_plan_line(&part("game::boots", Vec::new())).unwrap();
        let fields: Vec<&str> = line.split('\t').collect();
        assert_eq!(&fields[..3], ["game::boots", "/tmp/out", "60"], "{line:?}");
        assert!(!line.contains('\n'), "{line:?}");
        for bad in ["a\nb", "a\tb", "a\rb"] {
            let error = entry_plan_line(&part(bad, Vec::new())).unwrap_err().to_string();
            assert!(error.contains("entry plan"), "{error}");
        }
        let env = vec![
            (PER_ENTRY_ENV_DECL.to_string(), "KEY".to_string()),
            ("KEY".to_string(), "line\nbreak".to_string()),
        ];
        assert!(entry_plan_line(&part("game::boots", env)).is_err(), "a value must not split the line");
    }

    #[test]
    fn entry_passed_requires_stop_and_no_failures() {
        // Shared pass-predicate for both multi-run paths. Default (no
        // stop_sim observed) is a fail even with zero expects.
        let mut s = SimSummary::default();
        assert!(!entry_passed(&s));
        s.saw_stop = true;
        s.expects_pass.push(SiteMessage { message: "ok".into(), site: "f:0".into() });
        assert!(entry_passed(&s));
        s.expects_fail.push(SiteMessage { message: "x".into(), site: "f:1".into() });
        assert!(!entry_passed(&s));
    }

    #[test]
    fn a_stopped_sim_whose_checks_never_ran_fails() {
        // A fixture that returns before its checks (`STATE ?? return`) still
        // reaches stop_sim() and fails nothing; its coverage is gone.
        let mut empty = SimSummary { saw_stop: true, ..SimSummary::default() };
        assert!(!entry_passed(&empty));
        assert_eq!(verdict_word(&empty), "FAIL");
        assert!(sim_outcome(&empty, "s").is_err());

        let mut declared = SimSummary { saw_stop: true, ..SimSummary::default() };
        declared.no_checks = Some("a seed; its probe checks".into());
        assert!(entry_passed(&declared), "a declared check-free entry passes");

        empty.snapshots.push(SnapshotOp { name: "n".into(), value_json: "{}".into(), site: String::new() });
        assert!(entry_passed(&empty), "a snapshot is a check");
    }

    #[test]
    fn an_aborted_sim_prints_fail_not_pass() {
        // The imgui docking abort: zero expects ran, none failed, and the sim
        // never reached stop_sim(). The verdict word must follow the real
        // outcome, not the expect tally.
        let mut aborted = SimSummary::default();
        assert!(!aborted.has_failures(), "an abort fails no expect -- that is the trap");
        assert_eq!(verdict_word(&aborted), "FAIL");

        aborted.saw_stop = true;
        aborted.expects_pass.push(SiteMessage { message: "ok".into(), site: "f:0".into() });
        assert_eq!(verdict_word(&aborted), "PASS");

        let mut timed_out = SimSummary { saw_stop: true, ..SimSummary::default() };
        timed_out.timed_out = true;
        assert_eq!(verdict_word(&timed_out), "FAIL");

        let mut died = SimSummary { saw_stop: true, ..SimSummary::default() };
        died.process_failure_code = Some(1);
        assert_eq!(verdict_word(&died), "FAIL");
    }

    #[test]
    fn manual_gallery_selection_is_explicit_and_last_wins() {
        assert!(manual_sim(&[("SIM_MANUAL".into(), "1".into())]).unwrap());
        assert!(!manual_sim(&[]).unwrap());
        assert!(!manual_sim(&[("SIM_MANUAL".into(), "1".into()),
            ("SIM_MANUAL".into(), "0".into())]).unwrap());
        assert!(manual_sim(&[("SIM_MANUAL".into(), "yes".into())]).is_err());
    }

    #[test]
    fn excluded_named_entry_finalizes_history_without_spawning() {
        let root = tempfile::tempdir().unwrap();
        let recorder = SimRunRecorder::begin_at(root.path(), "testbed", "gpu_only", true, None, "single");
        let ids = recorder.plan_entries(&specs(&["gpu_only"]));
        let env = wires(&[("gpu_only", &[("SIM_BACKEND", "wgpu")])]);
        let (_, summary) = run_named_entry("testbed", "gpu_only", true, false, false,
            "", &env, None, None, Some(&recorder), Some(ids[0])).unwrap();
        assert!(entry_excluded(&summary));
        recorder.finish(true, None);
        let records: Vec<serde_json::Value> = std::fs::read_to_string(recorder.events_path().unwrap())
            .unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect();
        assert_eq!(records.last().unwrap()["complete"], true);
        let results: Vec<_> = records.iter().filter(|e| e["event"] == "entry_result").collect();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["verdict"], "EXCLUDED");
        assert!(!records.iter().any(|e| e["event"] == "worker_spawn"));
    }

    #[test]
    fn entry_excluded_is_not_passed_and_verdict_word_is_excluded() {
        let mut s = SimSummary::default();
        s.excluded = Some("requires wgpu".to_string());
        s.saw_stop = true;
        assert!(!entry_passed(&s));
        assert!(entry_excluded(&s));
        assert_eq!(verdict_word(&s), "EXCLUDED");
    }

    #[test]
    fn sim_summary_from_stderr_parses_exclude_op() {
        let stderr = "__SIM__ {\"op\":\"exclude\",\"reason\":\"requires wgpu\"}\n";
        let s = SimSummary::from_stderr(stderr);
        assert_eq!(s.excluded.as_deref(), Some("requires wgpu"));
        assert!(s.saw_stop);
    }

    #[test]
    fn sim_required_backend_parses_env_pairs() {
        assert_eq!(sim_required_backend(&[("SIM_BACKEND".into(), "wgpu".into())]), Some("wgpu"));
        assert_eq!(sim_required_backend(&[("STONE_RENDERER".into(), "wgpu".into())]), Some("wgpu"));
        assert_eq!(sim_required_backend(&[("SIM_REQUIRES".into(), "wgpu".into())]), Some("wgpu"));
        assert_eq!(sim_required_backend(&[("FRAME_COUNT".into(), "10".into())]), None);
    }

    #[test]
    fn summary_counts_expects() {
        let stderr = r#"
__SIM__ {"op":"start_game","name":"match3","site":"foo.rock:1"}
__SIM__ {"op":"expect_ok","message":"a","site":"foo.rock:2"}
__SIM__ {"op":"expect_ok","message":"b","site":"foo.rock:3"}
__SIM__ {"op":"expect_fail","message":"c","site":"foo.rock:4"}
__SIM__ {"op":"stop","site":"foo.rock:5"}
"#;
        let s = SimSummary::from_stderr(stderr);
        assert_eq!(s.expects_pass.len(), 2);
        assert_eq!(s.expects_fail.len(), 1);
        assert_eq!(s.expects_fail[0].message, "c");
        assert_eq!(s.expects_fail[0].site, "foo:4");
        assert!(s.saw_stop);
        assert!(s.has_failures());
    }

    #[test]
    fn summary_flags_missing_stop() {
        let stderr = r#"__SIM__ {"op":"expect_ok","message":"a","site":"foo.rock:1"}"#;
        let s = SimSummary::from_stderr(stderr);
        assert_eq!(s.expects_pass.len(), 1);
        assert!(!s.saw_stop);
        assert!(!s.has_failures());
    }

    #[test]
    fn summary_parses_allocation_census_and_preserves_frame_window() {
        let stderr = "__SIM__ {\"op\":\"allocation_census\",\"build_sha\":\"abc123\",\"rendering_enabled\":false,\"warmup_frames_requested\":2,\"warmup_frames\":2,\"frame_count\":6,\"measured_frames\":4,\"initialization_gross_kb\":3.5,\"outside_frame_gross_kb\":1.25,\"warmup_gross_kb\":8.0,\"measured_gross_kb\":24.0,\"total_gross_kb\":36.75,\"measured_allocator_calls\":40,\"measured_allocation_calls\":32,\"measured_reallocation_calls\":8,\"stop_sim_reached\":true,\"measurement_complete\":true}\n__SIM__ {\"op\":\"stop\"}";
        let summary = SimSummary::from_stderr(stderr);
        let census = summary.allocation_census.as_ref().expect("census result");
        assert_eq!(census.build_sha, "abc123");
        assert!(!census.rendering_enabled);
        assert_eq!(census.frame_count, 6);
        assert_eq!(census.measured_frames, 4);
        assert_eq!(census.measured_allocation_calls, 32);

        let (record, rank_values) = allocation_census_record("fixture", &summary);
        assert_eq!(record["status"], "PASS");
        assert_eq!(record["rendering_skipped"], true);
        assert_eq!(record["measured_frames"], 4);
        assert_eq!(record["gross_kb_per_frame"], 6.0);
        assert_eq!(record["allocation_calls_per_frame"], 8.0);
        assert_eq!(rank_values, Some((6.0, 8.0)));
    }

    #[test]
    fn census_missing_or_incomplete_result_is_not_a_zero_success() {
        let mut missing = SimSummary::default();
        missing.saw_stop = true;
        let (record, rank_values) = allocation_census_record("missing", &missing);
        assert_eq!(census_status(&missing), "no-verdict");
        assert_eq!(record["status"], "no-verdict");
        assert_eq!(record["measured_frames"], 0);
        assert!(rank_values.is_none());

        let incomplete = AllocationCensus {
            build_sha: "abc123".into(),
            rendering_enabled: false,
            warmup_frames_requested: 2,
            warmup_frames: 2,
            frame_count: 2,
            measured_frames: 0,
            initialization_gross_kb: 0.0,
            outside_frame_gross_kb: 0.0,
            warmup_gross_kb: 1.0,
            measured_gross_kb: 0.0,
            total_gross_kb: 1.0,
            measured_allocator_calls: 0,
            measured_allocation_calls: 0,
            measured_reallocation_calls: 0,
            stop_sim_reached: false,
            measurement_complete: false,
        };
        let incomplete = SimSummary {
            saw_stop: true,
            allocation_census: Some(incomplete),
            ..SimSummary::default()
        };
        assert_eq!(census_status(&incomplete), "no-verdict");
        assert!(allocation_census_record("incomplete", &incomplete).1.is_none());
    }

    #[test]
    fn census_rankings_use_the_requested_per_frame_metric() {
        let rows = vec![
            ("gross".to_string(), "PASS", 20.0, 1.0, 4),
            ("calls".to_string(), "PASS", 10.0, 3.0, 4),
        ];
        assert_eq!(census_rankings(&rows, true)[0]["sim_name"], "gross");
        assert_eq!(census_rankings(&rows, false)[0]["sim_name"], "calls");
    }

    #[test]
    fn summary_compresses_site_to_stem_and_line() {
        let stderr = r#"__SIM__ {"op":"expect_fail","message":"x","site":"/work/repo/mag/tests/sims/bandage.rock:29"}"#;
        let s = SimSummary::from_stderr(stderr);
        assert_eq!(s.expects_fail[0].site, "bandage:29");
    }

    #[test]
    fn src_declares_sim_matches_co_fn_decl() {
        // Real shape: `co fn smoke() -> Coroutine<Wait> { ... }`.
        // Regression: prior needle `co {name}(` missed every sim.
        let src = "@sim_render\n@sim\nco fn smoke() -> Coroutine<Wait> {\n}\n";
        assert!(src_declares_sim(src, "smoke"));
        assert!(!src_declares_sim(src, "other"));
    }

    #[test]
    fn find_sim_source_reads_the_declared_segment_of_a_v1_wire_name() {
        let project = tempfile::tempdir().unwrap();
        let sims = project.path().join("tests").join("sims");
        std::fs::create_dir_all(&sims).unwrap();
        std::fs::write(sims.join("a.rock"), "@sim\nco fn only() -> Coroutine<Wait> {\n}\nco fn twice() -> Coroutine<Wait> {\n}\n").unwrap();
        std::fs::write(sims.join("b.rock"), "mod b;\n@sim\nco fn twice() -> Coroutine<Wait> {\n}\n").unwrap();
        let project = project.path().to_str().unwrap();
        assert_eq!(find_sim_source(project, "game::only").unwrap(), sims.join("a.rock"));
        assert_eq!(find_sim_source(project, "only").unwrap(), sims.join("a.rock"));
        let error = find_sim_source(project, "game::b::twice").unwrap_err();
        assert!(error.contains("a.rock") && error.contains("b.rock"), "{error}");
        assert!(find_sim_source(project, "game::missing").is_err());
    }

    #[test]
    fn prefix_with_site_keeps_origin_in_message() {
        // Post-run snapshot/image_snapshot failures clear `site` so
        // `print()`'s "already-streamed" guard prints the line, but the
        // origin still has to reach the user -- fold it into the
        // message. Regression for the bug that hid every `diff > tol`
        // line behind the streamed image_snapshot op announcement.
        assert_eq!(
            prefix_with_site("smoke:10", "diff 0.006 > tol 0.001"),
            "at smoke:10: diff 0.006 > tol 0.001"
        );
        assert_eq!(prefix_with_site("", "msg"), "msg");
    }

    #[test]
    fn src_declares_sim_does_not_match_substring_or_call_site() {
        // `co fn smoke_alt(` must not match `smoke`. Plain calls to the
        // sim name (e.g. `smoke()` from another fn) must not match either.
        let src = "co fn smoke_alt() -> Coroutine<Wait> {\n  smoke()\n}\n";
        assert!(!src_declares_sim(src, "smoke"));
        assert!(src_declares_sim(src, "smoke_alt"));
    }

    #[test]
    fn reload_completion_is_terminal_after_publication() {
        let mut ledger = ReloadLedger::default();
        ledger.book(7).expect("request books once");
        ledger.publish(7).expect("artifact publishes once");
        ledger
            .complete(7, ReloadTerminal::Applied)
            .expect("watcher completion follows publication");
        assert_eq!(ledger.summary().requests, 1);
        assert_eq!(ledger.summary().applied, 1);
        assert_eq!(ledger.summary().pending, 0);
        assert_eq!(ledger.summary().rejected, 0);
        assert_eq!(ledger.summary().failed, 0);
    }

    #[test]
    fn reload_ledger_distinguishes_booked_and_published_pending() {
        let mut ledger = ReloadLedger::default();
        ledger.book(1).unwrap();
        ledger.book(2).unwrap();
        ledger.publish(2).unwrap();
        assert_eq!(ledger.summary().requests, 2);
        assert_eq!(ledger.summary().pending, 2);
        assert_eq!(ledger.validate().unwrap().pending, 2);
    }

    #[test]
    fn reload_ledger_accounts_rejection_and_failed_publication() {
        let mut ledger = ReloadLedger::default();
        ledger.book(1).unwrap();
        ledger.reject(1).unwrap();
        ledger.book(2).unwrap();
        ledger.publish(2).unwrap();
        ledger.complete(2, ReloadTerminal::Failed).unwrap();
        assert_eq!(
            ledger.validate().unwrap(),
            ReloadLedgerSummary { requests: 2, applied: 0, pending: 0, rejected: 1, failed: 1 }
        );
    }

    #[test]
    fn reload_ledger_rejects_drop_duplicate_and_early_completion() {
        let mut dropped = ReloadLedger::default();
        dropped.book(1).unwrap();
        assert_eq!(dropped.summary().pending, 1, "a dropped request cannot disappear from accounting");

        let mut duplicate = ReloadLedger::default();
        duplicate.book(1).unwrap();
        assert!(duplicate.complete(1, ReloadTerminal::Applied).is_err());
        duplicate.publish(1).unwrap();
        assert!(duplicate.publish(1).is_err());
        duplicate.complete(1, ReloadTerminal::Applied).unwrap();
        assert!(duplicate.complete(1, ReloadTerminal::Applied).is_err());
    }
}
