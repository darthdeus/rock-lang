//! `rock sim <project> <name>` — runner for Rock-native `@sim`
//! coroutines.
//!
//! Orchestrates: compile project + spawn the game with
//! `ROCK_RUN_SIM=<name>` set, demux `__SIM__`-prefixed JSON lines from
//! stderr, pretty-print pass/fail, return non-zero on any expect_fail.
//!
//! Side-channel format: stderr lines with the literal prefix
//! `__SIM__ {json}\n`, where each JSON object has `op: string` and
//! op-specific fields. See `stone/engine/sim.rock`.

use crate::sim_observability::SimRunRecorder;
use crate::skull_backend::{IncrementalProject, collect_sim_descriptors};
use anyhow::{Context, Result, anyhow};
use colored::Colorize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;
use tempfile::TempDir;

#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;

/// Frame budget for a sim that declares none. Sims terminate naturally via the
/// runner's `sim_done` check; this is the upper bound for a runaway / hung
/// body. A sim overrides it with `@sim_env("FRAME_COUNT", "<n>")`.
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

/// The wall-clock budget one entry runs on: its own
/// `@sim_env("ROCK_SIM_TIMEOUT_SECS", ...)`, or the process-wide
/// `sim_timeout_secs()`. Rejects a value that is not a positive integer,
/// like the frame budget -- a silently replaced bound guards nothing.
fn timeout_for(sim_name: &str, env_pairs: &[(String, String)]) -> Result<Duration> {
    let Some((_, raw)) = env_pairs
        .iter()
        .rev()
        .find(|(k, _)| k == "ROCK_SIM_TIMEOUT_SECS")
    else {
        return Ok(Duration::from_secs(sim_timeout_secs()));
    };
    raw.parse::<u64>().ok().filter(|n| *n > 0).map(Duration::from_secs).ok_or_else(|| {
        anyhow!("sim `{sim_name}` declares @sim_env(\"ROCK_SIM_TIMEOUT_SECS\", \"{raw}\"), which is not a positive integer")
    })
}

/// Wall-clock timeout. An INDEPENDENT bound, not a backstop for the frame
/// budget: it is the only thing that catches a child which never advances a
/// frame at all -- an infinite loop inside one tick, a blocked FFI call, a
/// driver stall -- where `FRAME_COUNT` can never be reached and so can never
/// fire. Neither bound subsumes the other.
///
/// It bounds ONE entry: boot up to its `entry_start`, then the sim from that
/// start.
///
/// Deliberately generous, because it is a wedge detector and not a deadline.
/// Per-child wall time rises sharply under `--jobs` (render-heavy game sims have
/// been measured taking 15 s for a single frame with four children live), and a
/// value tuned for serial runs kills healthy children. Override with
/// `ROCK_SIM_TIMEOUT_SECS` for a genuinely long-running sim, or per entry
/// with `@sim_env("ROCK_SIM_TIMEOUT_SECS", ...)` (see `timeout_for`).
const DEFAULT_TIMEOUT_SECS: u64 = 300;

/// Resident-plus-swap ceiling for one sim child, in MB. A sim that loops
/// while allocating grows until the machine's OOM killer picks a victim, and
/// that victim's cgroup can take unrelated processes down with it; the
/// wall-clock watchdog is far too slow to stop it (one looping sim reached
/// ~25 GB inside the 300 s timeout, 2026-09-18). A normal game sim child peaks
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
    fn from_value(value: &serde_json::Value) -> Result<Self> {
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
                .ok_or_else(|| {
                    anyhow!("allocation census field `{name}` is not a nonnegative integer")
                })
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
                .ok_or_else(|| {
                    anyhow!("allocation census field `measured_allocator_calls` is not an integer")
                })?,
            measured_allocation_calls: value
                .get("measured_allocation_calls")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| {
                    anyhow!("allocation census field `measured_allocation_calls` is not an integer")
                })?,
            measured_reallocation_calls: value
                .get("measured_reallocation_calls")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| {
                    anyhow!(
                        "allocation census field `measured_reallocation_calls` is not an integer"
                    )
                })?,
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
/// looping sim that set the cap held 20 of its 25 GB in swap. None while the
/// process still runs on this process's address space: `spawn` can return
/// while the child is inside execve, before its new mm replaces the runner's,
/// and /proc then reports the runner's memory as the child's.
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

/// The `/proc` memory fields of a live process, in MB, so a cap kill says
/// whether the growth was heap, mapped files or shared memory.
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
/// `tmp` entry that `rock_engine_host::runtime::lib_path_value` puts on the library
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
    started: rock_core::clock::Instant,
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
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
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
        write_result?;
        if unlock_result != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
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
    let started = rock_core::clock::Instant::now();
    let result = action();
    record_sim_compile_phase(operation, phase, project, output, started, result.is_ok());
    result
}

fn sim_spawn_barrier() {
    let Some(gate) = std::env::var_os("ROCK_SIM_SPAWN_BARRIER") else {
        return;
    };
    let gate = PathBuf::from(gate);
    let deadline = rock_core::clock::Instant::now() + Duration::from_secs(120);
    while !gate.exists() {
        if rock_core::clock::Instant::now() > deadline {
            log::warn!(target: "SIM",
                "ROCK_SIM_SPAWN_BARRIER `{}` never appeared; proceeding", gate.display());
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Parent-side child lifecycle trace, enabled by `ROCK_SIM_LIFECYCLE_LOG`.
///
/// `start`/`end` bracket an entry and `proc_start`/`proc_end` its process; with
/// one entry per child both pairs are written at spawn and at exit.
fn record_sim_lifecycle(event: &str, sim_name: &str) {
    let Some(path) = std::env::var_os("ROCK_SIM_LIFECYCLE_LOG") else {
        return;
    };
    static LOCK: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
    let Ok(_guard) = LOCK.get_or_init(|| Mutex::new(())).lock() else {
        return;
    };
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
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
    crate::skull_backend::IncrementalProject,
)> {
    let (names, env, sources, session, _) =
        compile_sims_with_allocation(backend, project_path, output_path, None, &[])?;
    Ok((names, env, sources, session))
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

#[path = "sim_compile_cache.rs"]
mod sim_compile_cache;
mod worker_admission;

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
    stone: Option<crate::skull_backend::IncrementalProject>,
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
    stone: &crate::skull_backend::IncrementalProject,
) -> Result<Vec<SimReceiptInput>> {
    let sources = stone
        .sources()
        .files
        .iter()
        .map(|source| {
            let source = source.borrow();
            (source.file.to_id_path(), source.contents.clone())
        })
        .collect();
    let comptime_files = stone.comptime_watch_files_with_hashes();
    let mut inputs = receipt_source_inputs_from_pairs(project, sources)?;
    for (path, _) in comptime_files {
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
        Ok(Self {
            context: sim_receipt_context(project)?,
            inputs: current_receipt_source_inputs(project)?,
        })
    }
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
) -> Result<(
    Vec<String>,
    HashMap<String, Vec<(String, String)>>,
    HashMap<String, String>,
)> {
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
            .ok_or_else(|| {
                anyhow!("sim compile receipt registry env for `{name}` is not an array")
            })?;
        let mut env = Vec::with_capacity(env_rows.len());
        for pair in env_rows {
            let pair = pair
                .as_array()
                .filter(|pair| pair.len() == 2)
                .ok_or_else(|| {
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
            .ok_or_else(|| {
                anyhow!("sim compile receipt source file for `{name}` is missing or invalid")
            })?;
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
    std::fs::write(
        staged.path().join(rock_host::project::MAIN_LUA_FILENAME),
        &main_bytes,
    )
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

fn load_sim_receipt(
    receipt_dir: &Path,
    project: &Path,
    output: &Path,
    current: Option<&CurrentReceiptState>,
) -> Result<(
    Vec<String>,
    HashMap<String, Vec<(String, String)>>,
    HashMap<String, String>,
)> {
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
    for expected in expected_inputs
        .iter()
        .filter(|input| input.kind == "embedded")
    {
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
    // Copy, then hash the copy that will run: game's chunk is about 160 MB, and
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
        &mut std::fs::File::open(&materialized).with_context(|| {
            format!("open materialized sim artifact {}", materialized.display())
        })?,
        &mut hasher,
    )
    .with_context(|| format!("hash materialized sim artifact {}", materialized.display()))?;
    if format!("{:x}", hasher.finalize()) != expected_main_hash {
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

fn prepare_sims(
    backend: crate::Backend,
    project_path: PathBuf,
    output_path: PathBuf,
) -> Result<PreparedSims> {
    crate::package::check_project_assets(&project_path)?;
    let selection = child_sim_selection()?;
    let Some(root) = std::env::var_os(SIM_COMPILE_RECEIPT_DIR_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
    else {
        let (wire_names, env_by_wire, source_files, stone, _) =
            compile_sims_with_allocation(backend, project_path, output_path, None, &selection)?;
        return Ok(PreparedSims {
            wire_names,
            env_by_wire,
            source_files,
            stone: Some(stone),
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
    let (wire_names, env_by_wire, source_files, stone, selection_is_full) =
        compile_sims_with_allocation(
            backend,
            project_path.clone(),
            output_path.clone(),
            None,
            &selection,
        )?;
    let inputs = receipt_inputs_from_stone(&project_path, &stone)?;
    let cache_identity = sim_compile_cache::identity(
        &project_path,
        backend,
        stone.comptime_inputs(),
        &rock_host::process_management::current_executable_image()?,
    )?;
    let receipt_selection = if selection_is_full {
        Vec::new()
    } else {
        selection
    };
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
    Ok(PreparedSims {
        wire_names,
        env_by_wire,
        source_files,
        stone: None,
    })
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
    Ok(PreparedSims {
        wire_names,
        env_by_wire,
        source_files,
        stone: None,
    })
}

/// `prepare_sims` for a `rock sim` run: the compile happens in a child `rock`
/// that publishes a receipt, so the runner never holds the compiler's memory
/// while sims run. Compiling game with every sim peaks at about 4.4 GB, which an
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
    crate::package::check_project_assets(&project_path)?;
    rock_host::process_management::report_spawn_phase("compile-prepare");
    if std::env::var_os(SIM_COMPILE_RECEIPT_DIR_ENV).is_some_and(|value| !value.is_empty()) {
        return prepare_sims(backend, project_path, output_path);
    }
    // Above the cache branch, so a cache-hit run still sweeps: the stale
    // roots it removes are from cache-bypassed runs, not from the cache.
    sweep_stale_sim_receipt_roots(Path::new("tmp"));
    let project_path = project_path
        .canonicalize()
        .with_context(|| format!("canonicalize sim project {}", project_path.display()))?;
    if let Some(cache) = sim_compile_cache::root(rock_exe)? {
        return sim_compile_cache::prepare(
            backend,
            &project_path,
            &output_path,
            &cache,
            rock_exe,
            selection,
        );
    }
    fs_err::create_dir_all("tmp").context("creating tmp/ for the sim compile receipt")?;
    // Pid-keyed so the next run can tell a concurrent live compile (pid
    // alive: keep) from a killed runner's leftover (pid dead: sweep).
    let receipt_prefix = format!("{SIM_RECEIPT_PREFIX}{}-", std::process::id());
    let root = tempfile::Builder::new()
        .prefix(&receipt_prefix)
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

const SIM_RECEIPT_PREFIX: &str = "sim-receipt-";

/// Roots without a parseable runner pid predate pid-keying; only age can
/// judge them, and no live compile runs this long.
const STALE_RECEIPT_GRACE: Duration = Duration::from_secs(6 * 60 * 60);

fn receipt_runner_pid(name: &str) -> Option<u32> {
    name.strip_prefix(SIM_RECEIPT_PREFIX)?
        .split('-')
        .next()?
        .parse()
        .ok()
}

/// Remove `sim-receipt-*` roots whose runner is gone. A SIGKILLed runner
/// skips its receipt tempdir's drop (~160 MB per game run); the next run
/// reclaims it here. Best-effort: every decision errs toward keeping, and a
/// removal failure is a diagnostic, never a run failure.
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
    rock_host::process_management::report_spawn_phase("compile");
    // Bound like a worker: a SIGKILLed runner must not leave the compile
    // running unparented (and publishing into a root nobody consumes).
    let mut cmd = std::process::Command::new(rock_exe);
    cmd.args(compile_child_invocation_args())
        .arg("sim")
        .arg("--list")
        .arg("--backend")
        .arg(backend.name())
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
    crate::child_lifetime::release_bound_child(child.id());
    if !status.success() {
        return Err(anyhow!(
            "cannot compile project {}: the sim compile process exited with {status}",
            project_path.display()
        ));
    }
    Ok(())
}

/// This invocation's flags that change what the sim compile builds or reports:
/// the `[build.vars]` selection and the build lint policy. The
/// compile child must see them, or it compiles something the run did not ask for.
fn compile_child_invocation_args() -> Vec<String> {
    let mut args = crate::comptime::invocation_selection_args();
    match crate::lint_pass::policy() {
        crate::lint_pass::LintReport::Full => {}
        crate::lint_pass::LintReport::Summary => args.push("--lint-summary".to_string()),
        crate::lint_pass::LintReport::Off => args.push("--no-lints".to_string()),
    }
    args
}

fn collect_skull_sims(
    ws: &bone_semantic::inference::Workspace,
) -> Vec<(String, String, Vec<(String, String)>)> {
    let mut sims = collect_sim_descriptors(ws);
    crate::sim_environment::apply(ws, &mut sims);
    sims.into_iter()
        .map(|sim| (sim.wire_name, sim.source_file, sim.env_pairs))
        .collect()
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

/// Build the skull game program for `dir`, write the game-shaped `main.lua`
/// (preamble + entry + the `@sim` registry) to `main_path`, and collect the
/// project's `@sim` wire names, source paths and env pairs from the SAME
/// workspace build. The skull counterpart of rock's `compile_sims` -- one
/// type-check feeds both the emitted registry and the runner's wire/env list,
/// so they cannot drift.
#[cfg(test)]
pub(crate) fn skull_compile_game_and_collect_sims(
    dir: &str,
    main_path: &std::path::Path,
) -> Result<(
    Vec<String>,
    HashMap<String, Vec<(String, String)>>,
    HashMap<String, String>,
    IncrementalProject,
)> {
    let (names, env, sources, session, _) =
        skull_compile_game_and_collect_sims_with_allocation(dir, main_path, None, &[])?;
    Ok((names, env, sources, session))
}

fn skull_compile_game_and_collect_sims_with_allocation(
    dir: &str,
    main_path: &std::path::Path,
    allocation_manifest: Option<&std::path::Path>,
    selection: &[String],
) -> Result<(
    Vec<String>,
    HashMap<String, Vec<(String, String)>>,
    HashMap<String, String>,
    IncrementalProject,
    bool,
)> {
    // Session-based build (same machinery as `rock stone`, tests included so the
    // `@sim` bodies are in the program). The live IncrementalProject is returned so the
    // runner can serve mid-sim `reload_edit` ops through the REAL incremental
    // path.
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
            let selected = (crate::registry::RuntimeServices::registered().resolve_sim_specs)(
                &all_names, selection,
            )?;
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
    let mut sims = collect_skull_sims(stone.workspace());
    if !selection_is_full {
        let selected = stone.registered_sim_names();
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
    Ok((
        wire_names,
        env_by_wire,
        source_files,
        stone,
        selection_is_full,
    ))
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
    backend: crate::Backend,
    project_path: PathBuf,
    output_path: PathBuf,
    allocation_manifest: Option<&Path>,
    selection: &[String],
) -> Result<(
    Vec<String>,
    HashMap<String, Vec<(String, String)>>,
    HashMap<String, String>,
    crate::skull_backend::IncrementalProject,
    bool,
)> {
    rock_host::process_management::report_spawn_phase("compile");
    // Only skull serves sims now (bone has no codegen, rock is gone): build the
    // bone workspace once (a live Session, tests included), emit the game-shaped
    // main.lua with its `@sim` registry, and collect the wire/env list from the
    // same build. Source paths are captured from the same HIR declarations, and
    // the returned IncrementalProject serves mid-sim `reload_edit` ops.
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
                    skull_compile_game_and_collect_sims_with_allocation(
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
    stone: crate::skull_backend::IncrementalProject,
    output: String,
    _dir: TempDir,
}

fn reload_lane_session(
    backend: crate::Backend,
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
        // A receipt contains emitted code, but no editable compiler Session.
        None => {
            compile_sims(
                backend,
                project_path.to_path_buf(),
                dir.path().to_path_buf(),
            )?
            .3
        }
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

/// Whether any selected entry opted into the hot-reload lane.
fn any_reload_lane_selected(
    specs: &[String],
    env_by_wire: &HashMap<String, Vec<(String, String)>>,
) -> bool {
    specs
        .iter()
        .any(|spec| entry_is_reload_lane(spec, env_by_wire))
}

/// Compile the project and print each `@sim` wire name on stdout, one
/// per line, sorted by source span. Lets scripts enumerate the suite without
/// hardcoding names.
pub fn do_list_sims(backend: crate::Backend, project: &str) -> Result<()> {
    let project_path = rock_engine_host::project::resolve_project_dir(project)?;

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

/// Compile/discover and report the run plan without building engines
/// or spawning children. Explicit names retain their occurrence semantics.
pub fn do_plan_sims(
    backend: crate::Backend,
    project: &str,
    names: &[String],
    jobs: Option<usize>,
    jobs_force: bool,
) -> Result<()> {
    let project_path = rock_engine_host::project::resolve_project_dir(project)?;
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
    // Explicit requests narrow to their entries (see `resolve_jobs`); a sweep
    // takes --jobs literally and an over-budget sweep still needs --jobs-force.
    let lanes = if names.is_empty() {
        None
    } else {
        Some(specs.len())
    };
    let plan = serde_json::json!({
        "jobs": resolve_jobs(jobs, lanes, false, jobs_force)?,
        "max_jobs": resolve_pool_jobs(jobs, lanes, false, jobs_force)?,
        "entries": specs,
        "manual_excluded": manual_excluded,
    });
    println!("{}", serde_json::to_string_pretty(&plan)?);
    Ok(())
}

pub fn do_run_sim_opts(
    backend: crate::Backend,
    project: &str,
    sim_name: &str,
    quiet: bool,
    update_snapshots: bool,
) -> Result<()> {
    do_run_sim_opts_with_request(backend, project, sim_name, quiet, update_snapshots, Some(1))
}

fn do_run_sim_opts_with_request(
    backend: crate::Backend,
    project: &str,
    sim_name: &str,
    quiet: bool,
    update_snapshots: bool,
    requested_jobs: Option<usize>,
) -> Result<()> {
    let recorder = SimRunRecorder::begin(project, sim_name, quiet, requested_jobs);
    let result = do_run_sim_opts_recorded(
        backend,
        project,
        sim_name,
        quiet,
        update_snapshots,
        requested_jobs,
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
    recorder: &SimRunRecorder,
) -> Result<()> {
    let project_path = recorder.phase("resolve_project", || {
        rock_engine_host::project::resolve_project_dir(project)
    })?;
    // Thread the RESOLVED path everywhere the raw arg used to flow (engine libs,
    // sim-source lookup for image baselines) so a bare name behaves exactly
    // like its expanded path.
    let project = project_path.to_string_lossy().into_owned();
    recorder.phase("build_engine", || {
        rock_engine_host::engine_build::build_engine_libs(&project)
    })?;

    let out_dir = sim_output_dir()?;
    let output = out_dir.path().to_string_lossy().into_owned();
    let output_path = out_dir.path().to_path_buf();

    // Compile once and collect `@sim_env("K", "V")` env pairs by wire name.
    // The runner applies them via `Command::env` before luajit spawns --
    // the in-process runtime `set_env()` runs after `@init`, so it can't
    // influence env-var-driven game config like `MATCH3_PRESET`.
    let mut prepared = recorder.phase("compile_and_discover", || {
        prepare_sims_out_of_process(
            backend,
            project_path.clone(),
            output_path.clone(),
            &rock_host::process_management::current_executable_image()?,
            &[sim_name.to_string()],
        )
    })?;
    let sim_name = resolve_sim_specs(&prepared.wire_names, &[sim_name.to_string()])?.remove(0);
    let sim_name = sim_name.as_str();
    // Receipt consumers do not retain the compiler session. A reload entry is
    // the one exception: it edits that live session, so preserve the existing
    // fresh-compile behavior for that explicitly selected lane.
    if prepared.stone.is_none() && entry_is_reload_lane(sim_name, &prepared.env_by_wire) {
        let (wire_names, env_by_wire, source_files, stone) = recorder
            .phase("compile_reload_session", || {
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
        update_snapshots,
        &prepared.source_files,
    );
    emit_bytecode_image(Path::new(&output));
    sim_spawn_barrier();

    let sim_sources = resolve_sim_sources(&project_path, &prepared.source_files)?;
    let (display_name, summary) = run_named_entry(
        sim_name,
        quiet,
        false,
        update_snapshots,
        &output,
        &prepared.env_by_wire,
        &sim_sources,
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
        return Err(anyhow!(
            "sim `{sim_name}` reached stop_sim() without running a check"
        ));
    }
    Ok(())
}

/// Run one named entry into a fresh tempdir, returning its name and summary.
/// Shared by the single-run path (`do_run_sim_opts`) and the multi-run path
/// (`do_run_named_sims`) so the tempdir / cleanup handling can't drift
/// between them. The tempdir is exposed to the child as `OUT`.
struct SimArtifacts {
    out_dir: TempDir,
}

impl SimArtifacts {
    fn new() -> Result<Self> {
        Ok(Self {
            out_dir: TempDir::new().context("creating sim tempdir")?,
        })
    }

    fn path(&self) -> &Path {
        self.out_dir.path()
    }

    /// The sim's `cleanup_path` removals, then `$OUT` itself.
    fn finish(self, summary: &mut SimSummary) {
        for path in std::mem::take(&mut summary.cleanup_paths) {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn run_named_entry(
    name: &str,
    quiet: bool,
    buffered: bool,
    update_snapshots: bool,
    output: &str,
    env_by_wire: &HashMap<String, Vec<(String, String)>>,
    source_files: &HashMap<String, PathBuf>,
    stone: Option<&mut crate::skull_backend::IncrementalProject>,
    census_warmup_frames: Option<usize>,
    recorder: Option<&SimRunRecorder>,
    occurrence_id: Option<u64>,
) -> Result<(String, SimSummary)> {
    let artifacts = SimArtifacts::new()?;
    let out_path = artifacts.path().to_path_buf();

    if name.is_empty() {
        return Err(anyhow!("sim got an empty sim name"));
    }
    let env_pairs = env_by_wire.get(name).cloned().unwrap_or_default();
    // Only a sim that opted into the reload lane may touch the session.
    let lane = if is_reload_lane(&env_pairs) {
        stone
    } else {
        None
    };
    let part = WorkerPart {
        name: name.to_string(),
        out_dir: out_path,
        frame_budget: frame_budget_for(name, &env_pairs)?,
        timeout: timeout_for(name, &env_pairs)?,
        source_file: source_files.get(name).cloned(),
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
    artifacts.finish(&mut summary);
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
    match env
        .iter()
        .rev()
        .find(|(key, _)| key == "SIM_MANUAL")
        .map(|(_, value)| value.as_str())
    {
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
) -> Result<()> {
    // One name: identical to the classic single-run path (full detail). There
    // is nothing to overlap, so `--jobs` does not apply.
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
        );
    }

    let recorder = SimRunRecorder::begin(project, &names.join(" "), quiet, jobs);
    let result = do_run_named_sims_recorded(
        backend,
        project,
        names,
        quiet,
        update_snapshots,
        jobs,
        jobs_force,
        full_asset_jobs,
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
    recorder: &SimRunRecorder,
) -> Result<()> {
    let project_path = recorder.phase("resolve_project", || {
        rock_engine_host::project::resolve_project_dir(project)
    })?;
    let project = project_path.to_string_lossy().into_owned();
    recorder.phase("build_engine", || {
        rock_engine_host::engine_build::build_engine_libs(&project)
    })?;

    let out_dir = sim_output_dir()?;
    let output = out_dir.path().to_string_lossy().into_owned();
    let prepared = recorder.phase("compile_and_discover", || {
        prepare_sims_out_of_process(
            backend,
            project_path.clone(),
            PathBuf::from(&output),
            &rock_host::process_management::current_executable_image()?,
            names,
        )
    })?;
    let wire_names = prepared.wire_names;
    let env_by_wire = prepared.env_by_wire;
    let source_files = prepared.source_files;
    let sim_sources = resolve_sim_sources(&project_path, &source_files)?;
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

    let effective_jobs = resolve_pool_jobs(jobs, Some(names.len()), update_snapshots, jobs_force)?;
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
    let mut reload_lane = reload_lane_session(
        backend,
        &project_path,
        any_reload_lane_selected(names, &env_by_wire),
        Path::new(&output),
        prepared.stone,
        quiet,
    )?;
    // Buffered, not silenced: the per-expect stream is suppressed (it would
    // interleave), but each record carries its diagnostics and sited failures.
    let results = run_fresh_entries(FreshRun {
        output: &output,
        env_by_wire: &env_by_wire,
        specs: names,
        source_files: &sim_sources,
        quiet,
        streaming: false,
        update_snapshots,
        jobs: effective_jobs,
        full_asset_jobs,
        reload_lane: reload_lane.as_mut(),
        recorder,
        entry_ids: &entry_ids,
    })?;
    let (passed, excluded, failed) = tally_entries(&results);
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
    // Result output, NOT logging: the multi-run verdict, consumed by you/CI.
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

/// Count `results`: (passed, excluded, failed names in `results` order).
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

/// Inputs for [`run_fresh_entries`]. Named fields, so a swapped boolean cannot
/// silently change what a run prints or records.
struct FreshRun<'a> {
    output: &'a str,
    env_by_wire: &'a HashMap<String, Vec<(String, String)>>,
    /// Resolved sim names; each runs in its own child with its own `$OUT`.
    specs: &'a [String],
    source_files: &'a HashMap<String, PathBuf>,
    quiet: bool,
    /// Stream each child's per-expect lines live; only sound with one job.
    streaming: bool,
    update_snapshots: bool,
    jobs: usize,
    full_asset_jobs: usize,
    reload_lane: Option<&'a mut ReloadLaneSession>,
    recorder: &'a SimRunRecorder,
    entry_ids: &'a [u64],
}

/// Run every spec in its own fresh child process and return the summaries in
/// `specs` order.
///
/// Reload-lane entries share the lane's one compile session, so they run one
/// at a time beside the pool (`beside_reload_lane`). The pool starts the
/// longest recorded entries first, so the suite's tail starts while the pool is
/// still full, and holds at most `full_asset_jobs` full-asset children at
/// once. Each record prints whole under one lock, so concurrent records never
/// interleave.
fn run_fresh_entries(run: FreshRun<'_>) -> Result<Vec<(String, SimSummary)>> {
    let FreshRun {
        output,
        env_by_wire,
        specs,
        source_files,
        quiet,
        streaming,
        update_snapshots,
        jobs,
        full_asset_jobs,
        reload_lane,
        recorder,
        entry_ids,
    } = run;
    let (reload_idx, ordinary_idx): (Vec<usize>, Vec<usize>) =
        (0..specs.len()).partition(|&i| entry_is_reload_lane(&specs[i], env_by_wire));

    let estimates: Vec<_> = ordinary_idx
        .iter()
        .map(|&i| recorder.recorded_duration_ms(&specs[i]))
        .collect();
    // A serial run gains nothing from reordering, so it keeps registry order.
    let dispatch: Vec<usize> = if jobs > 1 {
        if !quiet {
            let history = estimates.iter().filter(|ms| ms.is_some()).count();
            println!(
                "[sim] entries start longest-first ({history} history-timed, {} untimed keep \
                 registry order of {})",
                estimates.len() - history,
                estimates.len()
            );
        }
        longest_first(&estimates)
            .into_iter()
            .map(|k| ordinary_idx[k])
            .collect()
    } else {
        ordinary_idx.clone()
    };
    let default_stub = asset_stub_value();
    let full_assets: Vec<bool> = dispatch
        .iter()
        .map(|&i| {
            entry_full_assets(
                env_by_wire.get(&specs[i]).map_or(&[][..], |v| v),
                default_stub,
            )
        })
        .collect();
    if !quiet && full_asset_jobs < jobs && full_assets.iter().any(|f| *f) {
        let full = full_assets.iter().filter(|f| **f).count();
        println!(
            "[sim] at most {full_asset_jobs} of {jobs} children hold full-size assets at once ({full} full-asset entries)"
        );
    }

    // Only the reload lane owns the Session and its isolated output copy.
    let run_at = |i: usize,
                  lane: Option<(&mut crate::skull_backend::IncrementalProject, &str)>|
     -> Result<(String, SimSummary)> {
        let (stone, entry_output) = match lane {
            Some((stone, out)) => (Some(stone), out),
            None => (None, output),
        };
        let (display_name, summary) = run_named_entry(
            &specs[i],
            quiet,
            !streaming,
            update_snapshots,
            entry_output,
            env_by_wire,
            source_files,
            stone,
            None,
            Some(recorder),
            Some(entry_ids[i]),
        )?;
        // A streaming run is serial, so its records already come in spec order.
        if streaming && !quiet {
            print_record(&display_name, &summary, true);
        }
        Ok((display_name, summary))
    };

    let (pool, lane) = beside_reload_lane(
        jobs,
        !reload_idx.is_empty(),
        |width| {
            run_bounded_classed(width, full_asset_jobs, &full_assets, |k| {
                run_at(dispatch[k], None)
            })
        },
        || run_reload_lane_serially(reload_lane, &reload_idx, |i, lane| run_at(i, Some(lane))),
    );
    let mut results: HashMap<usize, Result<(String, SimSummary)>> = HashMap::new();
    for (k, r) in pool {
        results.insert(dispatch[k], r);
    }
    results.extend(lane);
    // Results walk `specs` order, so records and the failed list match a
    // serial run's regardless of completion order.
    let ordered: Vec<_> = (0..specs.len())
        .map(|i| {
            results
                .remove(&i)
                .unwrap_or_else(|| Err(anyhow!("sim `{}` was never scheduled", specs[i])))
        })
        .collect();
    if !streaming && !quiet {
        for (display_name, summary) in ordered.iter().flatten() {
            print_record(display_name, summary, false);
        }
    }
    ordered.into_iter().collect()
}

/// One entry's record with the child's retained stderr/stdout. A buffered
/// record never streamed its per-expect lines, so it carries its own sited
/// failures (`streamed = false`).
fn print_record(display_name: &str, summary: &SimSummary, streamed: bool) {
    if !summary.diagnostics.is_empty() {
        eprint!("{}", summary.diagnostics);
    }
    summary.print(display_name, streamed);
}

/// Run every `@sim` in `project` (the no-name form `rock sim <project>`).
/// Compiles once, then spawns each sim as its own luajit child with a
/// fresh tempdir, prints each sim's pass/fail line, and a final suite
/// tally. Returns Err (non-zero exit) if any sim failed. Each sim gets its
/// own `OUT`.
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
            scope.spawn(|| {
                loop {
                    let _slot = width.enter();
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if i >= n {
                        break;
                    }
                    let r = task(i);
                    collected.lock().unwrap().push((i, r));
                }
            });
        }
    });
    collected
        .into_inner()
        .expect("no lane panicked while holding the results lock")
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

/// Pool dispatch order: longest recorded duration first, so the lanes that
/// would set the suite's tail start while the pool is still full. The sort is
/// stable, so equal and untimed lanes keep their planned order, after every
/// timed one.
fn longest_first(estimates: &[Option<u64>]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..estimates.len()).collect();
    order.sort_by_key(|&w| std::cmp::Reverse(estimates[w].unwrap_or(0)));
    order
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
    // The test gate holds the pool until lane dispatch, so a drain barrier deadlocks there.
    let test_barrier = std::env::var_os("ROCK_SIM_RELOAD_LANE_TEST_BARRIER").map(PathBuf::from);
    std::thread::scope(|scope| {
        let pool = scope.spawn(|| {
            if let Some(dir) = &test_barrier {
                while !dir.join("lane_dispatched").exists() {
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
            let result = pool(&width);
            if test_barrier.is_some() {
                record_sim_lifecycle("pool_finished", "");
            }
            result
        });
        let lane = dispatch_reload_lane(lane, test_barrier.as_deref());
        width.widen_to_jobs();
        let pool = pool
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        (pool, lane)
    })
}

fn dispatch_reload_lane<B>(lane: impl FnOnce() -> B, test_barrier: Option<&Path>) -> B {
    if let Some(dir) = test_barrier {
        record_sim_lifecycle("reload_lane_dispatched", "");
        std::fs::write(dir.join("lane_dispatched"), b"").expect("write reload lane test barrier");
    }
    lane()
}

/// Each reload entry at `indices`, one at a time through the lane's session.
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
            scope.spawn(|| {
                loop {
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
                }
            });
        }
    });
    collected
        .into_inner()
        .expect("no lane panicked while holding the results lock")
}

pub fn do_run_all_sims(
    backend: crate::Backend,
    project: &str,
    quiet: bool,
    update_snapshots: bool,
    jobs: Option<usize>,
    jobs_force: bool,
    full_asset_jobs: Option<usize>,
) -> Result<()> {
    let recorder = SimRunRecorder::begin(project, "<all>", quiet, jobs);
    let result = do_run_all_sims_recorded(
        backend,
        project,
        quiet,
        update_snapshots,
        jobs,
        jobs_force,
        full_asset_jobs,
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
    recorder: &SimRunRecorder,
) -> Result<()> {
    let project_path = recorder.phase("resolve_project", || {
        rock_engine_host::project::resolve_project_dir(project)
    })?;
    let project = project_path.to_string_lossy().into_owned();
    recorder.phase("build_engine", || {
        rock_engine_host::engine_build::build_engine_libs(&project)
    })?;

    let out_dir = sim_output_dir()?;
    let output = out_dir.path().to_string_lossy().into_owned();
    let prepared = recorder.phase("compile_and_discover", || {
        prepare_sims_out_of_process(
            backend,
            project_path.clone(),
            PathBuf::from(&output),
            &rock_host::process_management::current_executable_image()?,
            &[],
        )
    })?;
    let wire_names = prepared.wire_names;
    let env_by_wire = prepared.env_by_wire;
    let source_files = prepared.source_files;
    let sim_sources = resolve_sim_sources(&project_path, &source_files)?;
    emit_bytecode_image(Path::new(&output));
    sim_spawn_barrier();
    if wire_names.is_empty() {
        return Err(anyhow!("no @sim coroutines found in project `{project}`"));
    }

    let entry_ids = recorder.plan_entries(&wire_names);
    let effective_jobs = resolve_pool_jobs(jobs, None, update_snapshots, jobs_force)?;
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
            }
            .print(&name, false);
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
    // Concurrent children cannot stream: their progress lines would
    // interleave. Serial runs keep the live stream.
    let results = run_fresh_entries(FreshRun {
        output: &output,
        env_by_wire: &env_by_wire,
        specs: &wire_names,
        source_files: &sim_sources,
        quiet,
        streaming: effective_jobs == 1 && !quiet,
        update_snapshots,
        jobs: effective_jobs,
        full_asset_jobs,
        reload_lane: reload_lane.as_mut(),
        recorder,
        entry_ids: &entry_ids,
    })?;
    let (passed, excluded, failed) = tally_entries(&results);
    let excluded = excluded + manual_excluded;

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
    println!("[sim] {project} suite: {passed}/{executed} sims passed{excl_suffix}{suffix}");

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
    let mut messages: Vec<String> = summary
        .errors
        .iter()
        .map(|m| {
            if m.site.is_empty() {
                m.message.clone()
            } else {
                format!("{}: {}", m.site, m.message)
            }
        })
        .collect();
    messages.extend(summary.expects_fail.iter().map(|m| {
        if m.site.is_empty() {
            m.message.clone()
        } else {
            format!("{}: {}", m.site, m.message)
        }
    }));
    messages
}

fn allocation_census_record(
    name: &str,
    summary: &SimSummary,
) -> (serde_json::Value, Option<(f64, f64)>) {
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
    let project_path = rock_engine_host::project::resolve_project_dir(project)?;
    let project = project_path.to_string_lossy().into_owned();
    rock_engine_host::engine_build::build_engine_libs(&project)?;

    let out_dir = sim_output_dir()?;
    let output = out_dir.path().to_string_lossy().into_owned();
    let (wire_names, env_by_wire, source_files, stone) =
        compile_sims(backend, project_path.clone(), PathBuf::from(&output))?;
    let sim_sources = resolve_sim_sources(&project_path, &source_files)?;
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
    let (reload_idx, ordinary_idx): (Vec<usize>, Vec<usize>) =
        (0..selected.len()).partition(|&i| entry_is_reload_lane(&selected[i], &env_by_wire));
    let jobs = resolve_pool_jobs(jobs, None, false, jobs_force)?;
    // Reload entries own the Session and an isolated copy of the boot chunk.
    let mut reload_lane = reload_lane_session(
        backend,
        &project_path,
        !reload_idx.is_empty(),
        Path::new(&output),
        Some(stone),
        false,
    )?;
    let run_at = |i: usize, lane: Option<(&mut crate::skull_backend::IncrementalProject, &str)>| {
        let (stone, entry_output) = match lane {
            Some((stone, out)) => (Some(stone), out),
            None => (None, output.as_str()),
        };
        run_named_entry(
            &selected[i],
            true,
            true,
            false,
            entry_output,
            &env_by_wire,
            &sim_sources,
            stone,
            Some(DEFAULT_SIM_ALLOCATION_WARMUP_FRAMES),
            None,
            None,
        )
    };

    let (pool, lane) = beside_reload_lane(
        jobs,
        !reload_idx.is_empty(),
        |width| {
            run_bounded(width, ordinary_idx.len(), |slot| {
                run_at(ordinary_idx[slot], None)
            })
        },
        || {
            run_reload_lane_serially(reload_lane.as_mut(), &reload_idx, |i, lane| {
                run_at(i, Some(lane))
            })
        },
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

    let project_path = rock_engine_host::project::resolve_project_dir(project)?;
    let project = project_path.to_string_lossy().into_owned();
    rock_engine_host::engine_build::build_engine_libs(&project)?;

    let out_dir = sim_output_dir()?;
    let output = out_dir.path().to_string_lossy().into_owned();
    let (wire_names, env_by_wire, source_files, stone, _) = compile_sims_with_allocation(
        backend,
        project_path.clone(),
        PathBuf::from(&output),
        Some(&manifest_path),
        &[],
    )?;
    let sim_sources = resolve_sim_sources(&project_path, &source_files)?;
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

    let selected = if names.is_empty() {
        wire_names.clone()
    } else {
        resolve_sim_specs(&wire_names, names)?
    };
    if selected.is_empty() {
        return Err(anyhow!("no @sim coroutines found in project `{project}`"));
    }
    for name in &selected {
        if entry_is_reload_lane(name, &env_by_wire) {
            return Err(anyhow!(
                "allocation-site diagnostics do not support reload sim `{name}` because its emitted site IDs can change after the initial manifest"
            ));
        }
    }

    let jobs = resolve_pool_jobs(jobs, None, false, jobs_force)?;
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
            name,
            true,
            true,
            false,
            &output,
            &instrumented_env,
            &sim_sources,
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

/// A mid-sim `reload_edit` request parsed off the `__SIM__` side channel
///.
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
/// published only after `IncrementalProject` writes the overlay, and can become
/// terminal only after the production watcher reports what it consumed.
#[derive(Default, Debug)]
struct ReloadLedger {
    requests: BTreeMap<u64, ReloadRequestState>,
}

impl ReloadLedger {
    fn book(&mut self, request_id: u64) -> Result<()> {
        if self
            .requests
            .insert(request_id, ReloadRequestState::Booked)
            .is_some()
        {
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
            None => Err(anyhow!(
                "reload request {request_id} published before it was booked"
            )),
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
            None => Err(anyhow!(
                "reload request {request_id} rejected before it was booked"
            )),
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
            None => Err(anyhow!(
                "reload request {request_id} completed before it was booked"
            )),
        }
    }

    fn summary(&self) -> ReloadLedgerSummary {
        let mut summary = ReloadLedgerSummary {
            requests: self.requests.len(),
            ..Default::default()
        };
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
                "reload ledger conservation violated: requests={} classified={}",
                summary.requests,
                classified
            ));
        }
        Ok(summary)
    }
}

/// Apply one `reload_edit`: in-memory find/replace on the file's CURRENT
/// in-session text, recompiled through the real incremental path
/// (`IncrementalProject::apply_edit`), which writes the overlay the running game
/// hot-reloads. BodyOnly edits only -- any other tier is a sim-author error
/// (an interface edit would rewrite main.lua under the running child).
/// `pristine` records each file's pre-edit text at first touch so
/// `revert_reload_edits` can restore the session after the child exits.
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

/// Whether a named entry belongs to the exclusive reload lane.
fn entry_is_reload_lane(spec: &str, env_by_wire: &HashMap<String, Vec<(String, String)>>) -> bool {
    env_by_wire.get(spec).is_some_and(|e| is_reload_lane(e))
}

/// CPU bound on sim concurrency. Memory bounds are applied by `sim_job_budget`.
fn sim_cpu_count() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// Only the default lane count takes the environment cap; explicit jobs keep
/// the physical CPU/RAM/GPU bounds independently of this preference. Invalid
/// or zero caps leave the CPU default in charge.
fn default_sim_jobs(cores: usize, env_cap: Option<&str>) -> usize {
    env_cap
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|jobs| *jobs > 0)
        .unwrap_or(cores)
        .clamp(1, cores.max(1))
}

/// Resolve the cap on concurrently FULL-ASSET children (`entry_full_assets`).
/// Such a child holds every image at the file's own size on the GPU (2-4 GB
/// on the measured host, against 0.2-0.4 GB for any other child, rendering
/// or not), so on a card with a fixed budget the number of them alive at
/// once is what overflows, not the job count. `None` takes
/// `DEFAULT_FULL_ASSET_JOBS`; anything requested is clamped into `1..=jobs`.
fn resolve_full_asset_jobs(requested: Option<usize>, jobs: usize) -> usize {
    requested
        .unwrap_or(DEFAULT_FULL_ASSET_JOBS)
        .clamp(1, jobs.max(1))
}

/// How many full-asset lanes (`entry_full_assets`) may be live at once unless
/// `--full-asset-jobs` says otherwise.
///
/// Measured 2026-09-13 on the game suite at `-j 14` with `footprint -p` per
/// worker (`IOAccelerator (graphics)`): a capped lane holds 0.2-0.4 GB of GPU
/// memory whether or not it renders, and a full-asset lane 2.0-4.3 GB. With
/// fourteen lanes the capped floor is ~5.6 GB, so two full-asset lanes peak
/// near 14 GB and three near 18.5 GB. On the Linux workstation a capped
/// worker lane grows to ~2 GiB, so there the job budget (`SIM_LANE_VRAM_MIB`)
/// is what bounds GPU memory and this cap only spreads the heaviest lanes.
/// `SIM_DRAW` is not the variable (the earlier `--render-jobs` cap
/// serialised 49 lanes for 100 s of wall and changed no verdict).
const DEFAULT_FULL_ASSET_JOBS: usize = 2;

/// Resolve the requested job count. `--update-snapshots` pins to serial: it
/// rewrites image baseline files, and the runner does not yet prove every
/// destination path is unique before spawning (several `@sim`s can share one
/// source file, so its `.snap` directory is shared too).
///
/// `lanes` is the number of entries for an explicit (named) request: a
/// request above it asks for lanes that will never run, so it is narrowed
/// before the budget check rather than rejected for them. Sweeps (no names)
/// pass `None` and take the request literally: an over-budget sweep still
/// requires `--jobs-force`.
pub(crate) fn resolve_jobs(
    requested: Option<usize>,
    lanes: Option<usize>,
    update_snapshots: bool,
    force: bool,
) -> Result<usize> {
    let requested = narrow_to_lanes(requested, lanes);
    let budget = sim_job_budget();
    let env_cap = std::env::var("ROCK_SIM_JOBS").ok();
    let jobs = resolve_jobs_with(
        requested,
        update_snapshots,
        sim_cpu_count(),
        budget,
        force,
        env_cap.as_deref(),
    )?;
    if let Some(asked) = requested.filter(|_| !update_snapshots) {
        static WARNED: std::sync::Once = std::sync::Once::new();
        if force {
            FORCED_SIM_JOBS.fetch_max(jobs, std::sync::atomic::Ordering::Relaxed);
        }
        if force && jobs > budget.lanes() {
            WARNED.call_once(|| {
                log::warn!(target: "SIM",
                "--jobs {jobs} exceeds the default budget of {} lanes", budget.lanes())
            });
        } else if !force && jobs < asked.min(sim_cpu_count()) {
            WARNED.call_once(|| log::warn!(target: "SIM",
                "--jobs {asked} exceeds the free-GPU budget of {} lanes; initial capacity {jobs}, expandable as VRAM frees", budget.gpu));
        }
    }
    Ok(jobs)
}

/// GPU load limits physical spawns, not the lifetime width of the task pool.
/// Keep enough threads to recover capacity when another suite finishes.
fn resolve_pool_jobs(
    requested: Option<usize>,
    lanes: Option<usize>,
    update_snapshots: bool,
    force: bool,
) -> Result<usize> {
    resolve_jobs(requested, lanes, update_snapshots, force)?;
    let budget = JobBudget {
        ram: sim_job_budget().ram,
        gpu: sim_cpu_count(),
    };
    let env_cap = std::env::var("ROCK_SIM_JOBS").ok();
    resolve_jobs_with(
        narrow_to_lanes(requested, lanes),
        update_snapshots,
        sim_cpu_count(),
        budget,
        force,
        env_cap.as_deref(),
    )
}

/// Explicit forced pool width; physical admission honors the same override.
/// Zero when not forced.
static FORCED_SIM_JOBS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn narrow_to_lanes(requested: Option<usize>, lanes: Option<usize>) -> Option<usize> {
    requested.map(|jobs| lanes.map_or(jobs, |lanes| jobs.min(lanes.max(1))))
}

/// The default lane budgets, each at least one lane and at most the cores.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct JobBudget {
    /// From total physical RAM: a fixed property of the host.
    ram: usize,
    /// From GPU memory free at launch: a load reading.
    gpu: usize,
}

impl JobBudget {
    fn lanes(&self) -> usize {
        self.ram.min(self.gpu)
    }
}

/// An explicit request above the RAM budget fails without `--jobs-force`.
/// One above only the free-GPU budget runs at that budget instead
/// (GPU_JOBS_CLAMP): free VRAM measures what else
/// is running, and load costs lanes, not the run.
fn resolve_jobs_with(
    requested: Option<usize>,
    update_snapshots: bool,
    cores: usize,
    budget: JobBudget,
    force: bool,
    env_cap: Option<&str>,
) -> Result<usize> {
    if update_snapshots {
        if requested.is_some_and(|j| j > 1) {
            log::warn!(target: "SIM",
                "--update-snapshots rewrites image baselines; running serially");
        }
        return Ok(1);
    }
    let jobs = match requested {
        None => budget.lanes().min(default_sim_jobs(cores, env_cap)),
        Some(jobs) if force => jobs,
        Some(jobs) if jobs > budget.ram => {
            anyhow::bail!(
                "--jobs {jobs} exceeds the RAM budget of {} lanes; use --jobs-force to override",
                budget.ram
            );
        }
        Some(jobs) => jobs.min(budget.gpu),
    };
    Ok(jobs.clamp(1, cores.max(1)))
}

/// GPU memory one sim child is planned at. Measured 2026-09-22 on the Linux
/// workstation (RTX 4090, game suite at `-j 6`, per-process `nvidia-smi`):
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
/// and beat 14 and 7 lanes at full-asset cap 2 in the same-SHA registry sweep.
/// Retune only with paired wall-time, memory and swap evidence. Keep the ratio
/// exact rather than rounding MiB.
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

/// Initial planning budgets, logged once. Physical GPU admission refreshes its
/// own reading; only the CPU/RAM ceiling persists for the execution pool.
fn sim_job_budget() -> JobBudget {
    static BUDGET: std::sync::OnceLock<JobBudget> = std::sync::OnceLock::new();
    *BUDGET.get_or_init(|| {
        let cores = sim_cpu_count();
        let total_ram = total_system_memory_mib();
        let probe = free_gpu_memory_mib();
        let free_vram = probe.as_ref().ok().copied().flatten();
        let mut budget = memory_budgets(cores, total_ram, free_vram);
        if let Err(reason) = probe {
            log::warn!(target: "SIM", "GPU memory probe failed: {reason}; limiting to one lane unless --jobs-force is set");
            budget.gpu = 1;
        }
        let env_cap = std::env::var("ROCK_SIM_JOBS").ok();
        let default_jobs = budget.lanes().min(default_sim_jobs(cores, env_cap.as_deref()));
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
        // Hosts without NVIDIA tooling use the CPU/RAM budget.
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
    let started = rock_core::clock::Instant::now();
    let command = rock_host::project::build_luajit_bytecode_command(&lua_file, &bytecode);
    let mut command = (crate::registry::RuntimeServices::registered().runtime_command)(
        command,
        crate::registry::RuntimeProcess::LuaBytecode,
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
/// Reload sims drive `IncrementalProject::apply_edit` against the one live session and
/// the one `incremental.lua`, so they cannot share the runner with any other
/// child. Opting in is `@sim_env("STONE_HOT_RELOAD", "1")` -- the same switch
/// that makes the child's watcher poll the overlay at all, so a sim that could
/// observe a reload is exactly a sim that declares this.
pub fn is_reload_lane(env_pairs: &[(String, String)]) -> bool {
    env_pairs
        .iter()
        .any(|(k, v)| k == "STONE_HOT_RELOAD" && v != "0")
}

fn writes_live(buffered: bool) -> bool {
    !buffered
}

/// Fixed values the runner publishes to EVERY sim child, layered BEFORE
/// `preset_env` so a declaration still wins.
///
/// `STONE_IMAGE_MAX_SIDE` holds every loaded image's GL storage to this many
/// texels on the longer side while its LOGICAL size stays the file's own, so
/// frame rects, sheet indices and UVs are unchanged and only sampling
/// resolution drops. A rendering game worker holds 3.2 GB of RGBA8 texture
/// pixels without it, which is what stops a second suite fitting in this
/// machine's memory. A
/// sim whose assertion genuinely needs the file's own texels declares
/// `@sim_env("STONE_IMAGE_MAX_SIDE", "0")`. No production boot publishes it: `rock
/// run`, `rock frame`, `rock perf` and `rock stone` build their own child
/// environments and never read this table.
///
/// 256 is the value the author asked for, and it is reachable because the one
/// thing that refused it is now declared rather than resampled. game's rendered
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
    ("SIM_DRAW", "none"),
    ("STONE_HEADLESS", "1"),
    ("STONE_IMAGE_MAX_SIDE", "256"),
    ("XDG_DATA_HOME", "$OUT/data"),
];

/// Whether an entry RENDERS, from the effective `SIM_DRAW` value.
///
/// Read last-wins off the resolved pairs, never from the source annotations:
/// `@sim_render` is lowered to `SIM_DRAW=on_demand` before an explicit
/// `@sim_env` is appended (`sim_environment::apply`), so a sim
/// carrying both is whatever it says LAST. Classifying on the annotations would
/// disagree with the value the child actually boots with.
///
/// A sim declaring nothing does not render: draw systems are skipped by default
/// under the harness. `commands` (draw phase on, nothing submitted to the GPU)
/// is NOT rendering here: its process needs no image pixels and holds no
/// rendered frame, which is what this oracle decides (the asset stub).
fn entry_renders(env: &[(String, String)]) -> bool {
    env.iter()
        .rfind(|(k, _)| k == "SIM_DRAW")
        .is_some_and(|(_, v)| matches!(v.as_str(), "on_demand" | "every_frame"))
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
/// `SIM_DRAW` is deliberately not consulted: a `commands` process with
/// full assets holds them all the same (measured 2.0 GB against 2.4-4.3 GB
/// for its rendering siblings).
fn entry_full_assets(env: &[(String, String)], default_stub: &str) -> bool {
    let last = |key: &str| env.iter().rfind(|(k, _)| k == key).map(|(_, v)| v.as_str());
    let stub = last("STONE_ASSET_STUB").unwrap_or(default_stub);
    let cap_default = RUNNER_BASELINE_ENV
        .iter()
        .find(|(k, _)| *k == "STONE_IMAGE_MAX_SIDE")
        .map(|(_, v)| *v)
        .expect("the runner baseline declares the image cap");
    let cap = last("STONE_IMAGE_MAX_SIDE").unwrap_or(cap_default);
    stub == "0" && cap == "0"
}

/// The sim environment a child boots with, last value wins, sorted by key:
/// the runner defaults under the declared pairs. `$OUT` stays unexpanded so a
/// per-entry temp path does not make every run's workload distinct.
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

/// `STONE_ASSET_STUB` for a sim child: headers-only assets, whether or not
/// anything in the process renders.
///
/// A rendering sim gets ONE WHITE TEXEL per image and asserts on layout,
/// silhouettes and gameplay all the same; decoding game's sheets for it costs
/// about 6 s of CPU and 456 MB of driver allocation per rendering process, and
/// the probe over all 132 non-manual rendering sims found only 22 that read
/// texels at all. Those 22 and the manual texel readers declare
/// `@sim_env("STONE_ASSET_STUB", "0")`, which wins because `preset_env` is
/// layered after this.
fn asset_stub_value() -> &'static str {
    unstub_override("STONE_ASSET_STUB")
}

/// A stub knob's value, honouring an inherited request to load for real.
///
/// The parent environment can only ever turn stubbing OFF. Letting it turn
/// stubbing ON would let a stray shell variable strip the pixels out of a
/// image snapshot sim, which fails as a wrong image rather than as a missing one;
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

/// One sim and the fresh child that runs it, with its own `out_dir`.
#[derive(Clone)]
struct WorkerPart {
    name: String,
    out_dir: PathBuf,
    frame_budget: usize,
    timeout: Duration,
    env: Vec<(String, String)>,
    occurrence_id: u64,
    /// On-disk source from the compiled wire -> source mapping, for image
    /// baseline lookup. `None` only when the wire missed the registry.
    source_file: Option<PathBuf>,
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
    fn from_value(
        value: &serde_json::Value,
        frame_count: Option<usize>,
        line: &str,
    ) -> Result<Self> {
        let number = |name: &str| -> Result<u64> {
            value
                .get(name)
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| {
                    anyhow!(
                        "entry frame timing field `{name}` is not a nonnegative integer: {line}"
                    )
                })
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

/// The child's stderr, carved by its `entry_start`/`entry_end` envelope.
#[derive(Default)]
struct EntryStream {
    started: bool,
    /// Saw the `entry_end`. False for an entry its child died inside.
    terminated: bool,
    /// Number of game-loop frames the entry completed, when the child
    /// reported it on its validated `entry_end`.
    frame_count: Option<usize>,
    sim: Vec<String>,
    /// Every raw line, including the boot and teardown noise outside the
    /// envelope. This is the debugging stream.
    raw_all: Vec<String>,
}

/// Split the child's stderr by its envelope, validating the protocol strictly
/// (`Envelope::observe`). The one tolerated gap is a started entry with no
/// `entry_end`: that is what a crashed or timed-out child leaves behind, and
/// it is recorded as `terminated == false` rather than accepted as a clean
/// close.
fn split_child_stream(lines: &[String], name: &str) -> Result<EntryStream> {
    let mut out = EntryStream::default();
    let mut envelope = Envelope::new(name);
    for line in lines {
        if let Some((prefix, _)) = line
            .split_once("__SIM__")
            .filter(|(prefix, _)| !prefix.is_empty())
        {
            out.raw_all.push(prefix.to_string());
        }
        match envelope.observe(line)? {
            EnvelopeStep::Raw => out.raw_all.push(line.clone()),
            EnvelopeStep::Started => out.started = true,
            EnvelopeStep::Ended(frame_count, _) => {
                out.terminated = true;
                out.frame_count = frame_count;
            }
            EnvelopeStep::Op => out.sim.push(line.clone()),
        }
    }
    Ok(out)
}

/// The ONE interpretation of the child's envelope, driven a line at a time.
/// `split_child_stream` uses it after the child exits, and the live stderr
/// reader uses it to decide when the wall-clock deadline anchor may reset, so
/// a line the split would reject can never buy a wedged child more time.
struct Envelope {
    name: String,
    started: bool,
    ended: bool,
}

/// What one observed line meant to the envelope.
#[derive(Clone, Copy)]
enum EnvelopeStep {
    /// The accepted `entry_start` -- the only step the deadline anchor may
    /// reset on.
    Started,
    Ended(Option<usize>, Option<EntryFrameTiming>),
    /// A structured operation inside the open entry.
    Op,
    /// A non-`__SIM__` line.
    Raw,
}

/// The live half of the wedged-child guard: the deadline anchor resets
/// exactly on the accepted `entry_start`. The first violation poisons the
/// guard, so no later line can extend the deadline; the violation itself is
/// reported after the exit by `split_child_stream`.
struct DeadlineGuard {
    envelope: Option<Envelope>,
}

impl DeadlineGuard {
    fn new(name: &str) -> Self {
        Self {
            envelope: Some(Envelope::new(name)),
        }
    }

    /// True exactly when `line` must reset the deadline anchor.
    #[cfg(test)]
    fn observe_resets(&mut self, line: &str) -> bool {
        matches!(self.observe_boundary(line), Some(EnvelopeStep::Started))
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

impl Envelope {
    fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            started: false,
            ended: false,
        }
    }

    /// Strict: a second start, an end with no open entry, a name other than
    /// this child's sim, or a structured operation outside the envelope is a
    /// protocol error rather than something to guess at.
    fn observe(&mut self, line: &str) -> Result<EnvelopeStep> {
        let Some(event) = parse_sim_marker(line)? else {
            return Ok(EnvelopeStep::Raw);
        };
        let rest = &event;
        let op = extract_str_field(rest, "op").unwrap_or_default();
        match op.as_str() {
            "entry_start" => {
                if self.started {
                    return Err(anyhow!("sim child started its entry twice: {line}"));
                }
                envelope_name_matches(rest, &self.name, line)?;
                self.started = true;
                Ok(EnvelopeStep::Started)
            }
            "entry_end" => {
                if !self.started || self.ended {
                    return Err(anyhow!(
                        "sim child ended an entry that was not open: {line}"
                    ));
                }
                envelope_name_matches(rest, &self.name, line)?;
                let frame_count = envelope_frame_count(rest, line)?;
                let frame_timing = envelope_frame_timing(rest, frame_count, line)?;
                self.ended = true;
                Ok(EnvelopeStep::Ended(frame_count, frame_timing))
            }
            _ => {
                if !self.started || self.ended {
                    return Err(anyhow!(
                        "sim child emitted a `{op}` operation outside its entry envelope: {line}"
                    ));
                }
                Ok(EnvelopeStep::Op)
            }
        }
    }
}

fn envelope_frame_count(rest: &serde_json::Value, line: &str) -> Result<Option<usize>> {
    let Some(raw) = extract_raw_field(rest, "frame_count") else {
        return Ok(None);
    };
    raw.parse::<usize>()
        .map(Some)
        .map_err(|_| anyhow!("sim child sent an invalid entry frame_count: {line}"))
}

fn envelope_frame_timing(
    rest: &serde_json::Value,
    frame_count: Option<usize>,
    line: &str,
) -> Result<Option<EntryFrameTiming>> {
    let Some(value) = rest.get("frame_timing") else {
        return Ok(None);
    };
    EntryFrameTiming::from_value(value, frame_count, line).map(Some)
}

fn envelope_name_matches(rest: &serde_json::Value, expected: &str, line: &str) -> Result<()> {
    match extract_str_field(rest, "name") {
        Some(name) if name == expected => Ok(()),
        Some(name) => Err(anyhow!(
            "sim child envelope names `{name}` where the runner asked for `{expected}`: {line}"
        )),
        None => Err(anyhow!(
            "sim child sent an entry envelope with no name: {line}"
        )),
    }
}

/// Admission namespace used by the physical worker spawn.
pub fn sim_worker_slot_directory() -> Result<PathBuf> {
    let root = match std::env::var_os("ROCK_SIM_WORKER_SLOT_DIR") {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        Some(_) => anyhow::bail!("ROCK_SIM_WORKER_SLOT_DIR must not be empty"),
        None => {
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
    };
    Ok(root)
}

fn record_worker_wait(label: &str) {
    let Some(path) = std::env::var_os("ROCK_SIM_WORKER_WAIT_LOG") else {
        return;
    };
    let bytes = format!("wait\t{label}\n");
    if let Err(error) = append_sim_trace(Path::new(&path), bytes.as_bytes()) {
        log::warn!(target: "SIM", "cannot write worker wait trace: {error}");
    }
}

/// Share CPU/RAM slots and fresh GPU admission across all physical spawns.
fn acquire_worker_slot(
    label: &str,
) -> Result<(std::fs::File, u64, u64, BTreeMap<&'static str, u64>)> {
    let forced = FORCED_SIM_JOBS.load(std::sync::atomic::Ordering::Relaxed);
    let budget = sim_job_budget().ram.max(forced);
    let slots = match std::env::var("ROCK_SIM_WORKER_SLOTS") {
        Ok(raw) => raw
            .parse::<usize>()
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| anyhow!("ROCK_SIM_WORKER_SLOTS={raw:?} must be a positive integer"))?
            .min(budget),
        Err(std::env::VarError::NotPresent) => budget,
        Err(error) => return Err(anyhow!("ROCK_SIM_WORKER_SLOTS: {error}")),
    };
    let root = sim_worker_slot_directory()?;
    let started = rock_core::clock::Instant::now();
    let mut checks = Duration::ZERO;
    let mut waits: BTreeMap<&'static str, Duration> = BTreeMap::new();
    let mut waiting = false;
    loop {
        let checking = rock_core::clock::Instant::now();
        let result =
            worker_admission::try_acquire(&root, slots, forced > 0, worker_admission::gpu_memory)?;
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
            record_worker_wait(label);
            log::info!(target: "SIM", "waiting for host sim capacity (at most {slots} workers, {reason}): {label}");
            waiting = true;
        }
        let blocked = rock_core::clock::Instant::now();
        std::thread::sleep(Duration::from_millis(100));
        *waits.entry(reason).or_default() += blocked.elapsed();
    }
}

struct AdmittedWorker {
    child: crate::headless_scope::BoundChild,
    _slot: std::fs::File,
    _phase: worker_admission::WorkerPhase,
}

impl std::ops::Deref for AdmittedWorker {
    type Target = Child;
    fn deref(&self) -> &Child {
        &self.child
    }
}

impl std::ops::DerefMut for AdmittedWorker {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.child
    }
}

/// Inputs for one sim child: its part plus the run's reporting and session
/// options. Named fields, not positionals, so a swapped boolean cannot
/// silently change what a run records.
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

/// Spawn ONE fresh luajit process for one sim part and parse its side
/// channel into the part's summary.
fn run_child(config: ChildConfig<'_>) -> Result<SimSummary> {
    let ChildConfig {
        part,
        quiet,
        buffered,
        update_snapshots,
        output,
        census_warmup_frames,
        stone,
        recorder,
    } = config;

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

    // Expand `$OUT` in env values to the entry's tempdir absolute path.
    // Lets sims write `@sim_env("GAME_HISTORY_DIR", "$OUT/history")`
    // and have it resolve before luajit spawns. Runtime `set_env()`
    // runs after `@init` so it can't influence env-var-driven config.
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
        // Apply per-entry defaults, including private user data. Explicit
        // `@sim_env` values override these defaults below.
        .envs(
            RUNNER_BASELINE_ENV
                .iter()
                .map(|(k, v)| (*k, v.replace("$OUT", &out_str))),
        )
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
    // `[sim]` verdict lines aren't buried under [STONE]/[game]/[ENV] INFO
    // noise. The `__SIM__` side channel is emitted directly, not through the
    // logger, so results are unaffected; warn/error still pass through on a
    // real failure. Respect an explicit ROCK_LOG from the env or a sim's
    // `@sim_env` so a debugging run can still turn logs back up.
    let rock_log_preset = preset_env.iter().any(|(k, _)| k == "ROCK_LOG");
    if quiet && !rock_log_preset && std::env::var_os("ROCK_LOG").is_none() {
        cmd.env("ROCK_LOG", "warn");
    }

    let label = part.name.clone();
    let mut worker_phase = worker_admission::WorkerPhase::waiting();
    let (mut worker_slot, admission_ms, checks_ms, waits) = acquire_worker_slot(&label)?;
    worker_phase.admitted();
    let worker_attempt = recorder
        .map(|recorder| {
            recorder.worker_spawn(part.occurrence_id, &part.name, &workload_env(&part.env))
        })
        .unwrap_or(0);
    if let Some(recorder) = recorder {
        recorder.worker_admission(worker_attempt, &label, admission_ms, checks_ms, &waits);
    }
    // Bound: on Linux the child dies with a SIGKILLed runner instead of
    // orphaning (parent-death signal); everywhere else TERM/INT/HUP are
    // forwarded. Released once reaped below, and in `Drop` for early exits.
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
    worker_admission::publish_pid(&sim_worker_slot_directory()?, &mut worker_slot, child.id())?;
    let mut child = AdmittedWorker {
        child,
        _slot: worker_slot,
        _phase: worker_phase,
    };
    let process_id = child.id();
    if let Some(recorder) = recorder {
        recorder.worker_started(worker_attempt, process_id);
    }
    record_sim_lifecycle("proc_start", &label);
    record_sim_lifecycle("start", &part.name);
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
    // What the wall-clock deadline is measured from: the spawn until the entry
    // opens, then its `entry_start`, so boot does not spend the entry's budget.
    let deadline_anchor = Arc::new(Mutex::new(rock_core::clock::Instant::now()));
    // Mid-sim reload_edit ops flow stderr-thread -> wait-loop, which owns the
    // (single-threaded) IncrementalProject and services them between try_wait polls.
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
        let (occurrence_id, name) = (part.occurrence_id, part.name.clone());
        let mut deadline_guard = DeadlineGuard::new(&part.name);
        // The open entry's own lines, for its live checkpoint verdict: the
        // same `from_stderr` parse the post-run summary uses, so the two
        // verdicts cannot drift apart in what they count.
        let mut checkpoint_lines: Vec<String> = Vec::new();
        // Buffered runs must not write anything from this thread: several
        // children are live, so a direct write here interleaves into the middle
        // of another sim's locked record. The lines are retained and handed
        // back for the caller to emit inside that lock.
        let quiet = quiet || buffered;
        std::thread::spawn(move || -> Option<String> {
            let reader = BufReader::new(stderr_handle);
            let mut parse_error = None;
            for line in reader.lines() {
                let line = match line {
                    Ok(line) => line,
                    Err(e) => return Some(format!("error reading game stderr: {e}")),
                };
                let marker = match parse_sim_marker(&line) {
                    Ok(marker) => marker,
                    Err(error) => {
                        parse_error.get_or_insert_with(|| error.to_string());
                        None
                    }
                };
                let boundary = deadline_guard.observe_boundary(&line);
                if let Some(EnvelopeStep::Started) = boundary {
                    *anchor.lock().unwrap() = rock_core::clock::Instant::now();
                }
                if let Some(recorder) = recorder.as_ref() {
                    match boundary {
                        Some(EnvelopeStep::Started) => {
                            recorder.worker_first_entry(worker_attempt, occurrence_id, &name);
                            if let Some(context) = marker
                                .as_ref()
                                .and_then(|event| event.get("runtime").cloned())
                            {
                                recorder.entry_runtime_context(
                                    worker_attempt,
                                    occurrence_id,
                                    context,
                                );
                            }
                            recorder.entry_start(worker_attempt, occurrence_id, &name);
                        }
                        Some(EnvelopeStep::Ended(frame_count, frame_timing)) => {
                            recorder.entry_end(
                                worker_attempt,
                                occurrence_id,
                                &name,
                                frame_count,
                                frame_timing.map(EntryFrameTiming::value),
                            );
                            let checkpoint = SimSummary::from_stderr(&checkpoint_lines.join("\n"));
                            recorder.entry_checkpoint(
                                worker_attempt,
                                occurrence_id,
                                &name,
                                verdict_word(&checkpoint),
                            );
                        }
                        Some(EnvelopeStep::Op) => checkpoint_lines.push(line.clone()),
                        _ => {}
                    }
                }
                if let Some(event) = marker {
                    let rest = &event;
                    if writes_live(buffered) {
                        if let Some((prefix, _)) = line
                            .split_once("__SIM__")
                            .filter(|(prefix, _)| !prefix.is_empty())
                        {
                            eprintln!("{prefix}");
                        }
                    }
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
            parse_error
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
                reload_errors.push(SiteMessage {
                    message: format!("reload completion: {e}"),
                    site: String::new(),
                });
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
                if since_anchor > part.timeout {
                    runner_killed = true;
                    let _ = child.kill();
                    let s = child.wait()?;
                    break (s, true);
                }
                if let (Some(cap), Some(used)) = (memory_cap_mb, process_memory_mb(child.id())) {
                    if used > cap {
                        let breakdown = process_memory_breakdown(child.id());
                        runner_killed = true;
                        let _ = child.kill();
                        let s = child.wait()?;
                        memory_error = Some(SiteMessage {
                            message: format!(
                                "sim child exceeded its memory cap of {cap} MB ({used} MB resident \
                                 plus swap: {breakdown}) and was killed; override with ROCK_SIM_MEMORY_MB"
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
                                let mut apply =
                                    || service_reload_edit(st, output, &mut pristine, &req);
                                if let Some(recorder) = recorder {
                                    recorder.phase(&format!("reload_apply:{}", part.name), apply)
                                } else {
                                    apply()
                                }
                            }
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
                                let e = anyhow!(
                                    "reload_edit returned non-incremental result {other:?}"
                                );
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
                            if let Err(e) =
                                consume_reload_completion(&mut reload_ledger, completion)
                            {
                                reload_errors.push(SiteMessage {
                                    message: format!("reload completion: {e}"),
                                    site: String::new(),
                                });
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
    // Every loop exit reaped the child; release its bound slot now so a
    // later TERM cannot be forwarded to a reused pid.
    crate::child_lifetime::release_bound_child(process_id);
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
            reload_errors.push(SiteMessage {
                message: format!("reload completion: {e}"),
                site: String::new(),
            });
        } else if failed {
            reload_errors.push(SiteMessage {
                message:
                    "reload watcher reported failed publication; last-good artifact remains active"
                        .into(),
                site: String::new(),
            });
        }
    }
    if let Err(e) = reload_ledger.validate() {
        reload_errors.push(SiteMessage {
            message: format!("reload ledger: {e}"),
            site: String::new(),
        });
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
    record_sim_lifecycle("end", &part.name);
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

    let stream = split_child_stream(&collected, &part.name)?;
    let worker_end = describe_worker_end(
        &status,
        timed_out,
        part.timeout,
        runner_killed,
        memory_error.as_ref(),
        &collected,
    );

    let mut summary = SimSummary::from_stderr(&stream.sim.join("\n"));
    summary.frame_count = stream.frame_count;
    summary.process_id = Some(process_id);
    if recorder.is_some() {
        summary.worker_attempt = Some(worker_attempt);
    }
    if !stream.started {
        summary.errors.push(SiteMessage {
            message: format!(
                "sim `{}` never started: its process ended before the entry opened ({worker_end})",
                part.name,
            ),
            site: String::new(),
        });
    }
    // A started entry with no `entry_end` is only explicable as one its child
    // died inside. Anywhere else the envelope was lost, and a record built
    // from a half-seen stream must not read as a clean result.
    if stream.started && !stream.terminated && status.success() && !timed_out {
        summary.errors.push(SiteMessage {
            message: format!(
                "sim `{}` never emitted its entry_end although its process exited cleanly",
                part.name
            ),
            site: String::new(),
        });
    }
    summary.errors.extend(std::mem::take(&mut reload_errors));
    summary.errors.extend(memory_error.take());
    if let Some(note) = crate::headless_scope::outside_kill_note(&status, runner_killed) {
        summary.errors.push(SiteMessage {
            message: note,
            site: String::new(),
        });
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
        summary.errors.push(SiteMessage {
            message: if timed_out {
                format!(
                    "sim `{}` exceeded its {}s wall-clock entry budget; \
                     its process ended with {status}; runner_killed={runner_killed}",
                    part.name,
                    part.timeout.as_secs()
                )
            } else {
                format!("sim process ended with {status}; runner_killed={runner_killed}")
            },
            site: String::new(),
        });
    }

    match snapshot_source_for(part) {
        Ok(source) => {
            process_image_snapshots(&mut summary, &snap_dir_for(&source), update_snapshots)
        }
        Err(reason) if !summary.image_snapshots.is_empty() => {
            summary.errors.push(SiteMessage {
                message: reason,
                site: String::new(),
            });
        }
        Err(_) => {}
    }

    if buffered {
        // Same CONTENT a streaming run writes live, just held until the
        // caller prints the record: the child's own stderr, plus its stdout
        // when the sim failed. Without this a concurrent run silently loses
        // every game-side diagnostic that a serial run prints.
        let mut diag = String::new();
        for line in &stream.raw_all {
            diag.push_str(line);
            diag.push('\n');
        }
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
            diag.push_str(&format!("--- game stdout for `{}` ---\n", part.name));
            diag.push_str(&stdout_str);
        }
        summary.diagnostics = diag;
    }

    // Restore the session + remove the overlay even when the sim failed --
    // a revert failure is itself a loud error (it would poison every later
    // sim in the suite).
    if let Err(e) = stone
        .map(|st| {
            if pristine.is_empty() {
                return revert_reload_edits(st, output, &mut pristine);
            }
            let mut restore = || revert_reload_edits(st, output, &mut pristine);
            if let Some(recorder) = recorder {
                recorder.phase(&format!("reload_restore:{}", part.name), restore)
            } else {
                restore()
            }
        })
        .unwrap_or(Ok(()))
    {
        summary.errors.push(SiteMessage {
            message: format!("reload_edit revert failed: {e}"),
            site: String::new(),
        });
    }

    if !quiet && writes_live(buffered) {
        if (summary.has_failures() || summary.timed_out || summary.process_failure_code.is_some())
            && !stdout_str.trim().is_empty()
        {
            log::error!(target: "SIM", "--- game stdout for `{}` ---", part.name);
            // Result output, NOT logging: verbatim dump of the child
            // game's captured stdout on failure — structured child
            // output, not a run_sim log record.
            eprint!("{stdout_str}");
        }
    }

    Ok(summary)
}

/// Run `entry_specs` (each its own child and `$OUT`) and return a
/// record per entry. Every entry's `$OUT` stays live until its child has
/// exited and its deferred artifacts have been processed.
/// How a worker process ended, for the entries it never started: its exit
/// status (code or signal), whether the runner killed it and why, and the last
/// lines of game stderr outside the `__SIM__` channel.
fn describe_worker_end(
    status: &ExitStatus,
    timed_out: bool,
    timeout: Duration,
    runner_killed: bool,
    memory_error: Option<&SiteMessage>,
    stderr: &[String],
) -> String {
    const TAIL: usize = 8;
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
    let game: Vec<&str> = stderr
        .iter()
        .map(|line| {
            line.split_once("__SIM__")
                .map_or(line.as_str(), |(prefix, _)| prefix)
        })
        .filter(|line| !line.is_empty())
        .collect();
    let tail = &game[game.len().saturating_sub(TAIL)..];
    if tail.is_empty() {
        out.push_str("; no game stderr");
    } else {
        let tail: Vec<String> = tail
            .iter()
            .map(|line| line.chars().take(240).collect())
            .collect();
        out.push_str(&format!("; last game stderr: {}", tail.join(" | ")));
    }
    out
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
/// progress display, not a parser. Heavy fields (image_snapshot path)
/// are summarized rather than dumped.
fn print_progress(rest: &serde_json::Value) {
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
            match (
                extract_str_field(rest, "actual"),
                extract_str_field(rest, "expected"),
            ) {
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
        "image_snapshot" => {
            let n = extract_str_field(rest, "name").unwrap_or_default();
            format!("{} {}", "image_snapshot".cyan(), n.yellow())
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

/// The on-disk source for a part's image baselines, from the compiled
/// wire -> source mapping the part carries -- never a source-text scan, so
/// same-named sims in different files resolve in their own files.
fn snapshot_source_for(part: &WorkerPart) -> Result<PathBuf, String> {
    part.source_file.clone().ok_or_else(|| {
        format!(
            "sim `{}` has no compiled source file; image_snapshot cannot resolve a baseline dir",
            part.name
        )
    })
}

/// `foo/bar/baz.rock` -> `foo/bar/baz.snap`, beside the image baselines.
fn snap_dir_for(source: &Path) -> PathBuf {
    let stem = source.file_stem().unwrap_or_default();
    let parent = source.parent().unwrap_or_else(|| Path::new("."));
    let mut name = stem.to_os_string();
    name.push(".snap");
    parent.join(name)
}

/// Compare collected image snapshots with their baselines and record each
/// result as an expect.
fn process_image_snapshots(summary: &mut SimSummary, snap_dir: &Path, update: bool) {
    // Post-run image_snapshot resolutions are NOT printed by the live stream
    // (the stream only announces the op, not its diff result), so we
    // pass an empty site -- that's the signal `SimSummary::print` uses
    // to know "synthesized after run; please print." The original site
    // is folded into the message so the user still sees where it came
    // from. Fixed 2026-05-11 after the print() guard introduced in
    // 2c78c5cef accidentally suppressed image_snapshot failures.
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

/// A failure or error message paired with the workspace-relative site
/// (`game/tests/sims/foo.rock:42`) it originated at, captured via
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
    process_id: Option<u32>,
    worker_attempt: Option<u64>,
    allocation_census: Option<AllocationCensus>,
    image_snapshots: Vec<ImageSnapshotOp>,
    cleanup_paths: Vec<String>,
    timed_out: bool,
    /// The wall-clock budget the timed-out entry ran under, when the runner
    /// killed it: its declared budget or the process-wide default.
    timeout_budget: Option<Duration>,
    process_failure_code: Option<i32>,
    /// Child diagnostics retained by a buffered (concurrent) run, emitted by
    /// the caller inside the record lock. Empty when the run wrote them live.
    diagnostics: String,
    /// Parent-side conservation evidence for the real watcher/publication
    /// completion path. None means this entry issued no reload request.
    reload_ledger: Option<ReloadLedgerSummary>,
    /// Why this entry was excluded from execution: `sim_exclude` at runtime, or
    /// a manual sim the suite skips.
    excluded: Option<String>,
    /// Why this entry runs no check (`sim_no_checks`): another test reads its
    /// log or artifacts. Without it, an entry with no check fails.
    no_checks: Option<String>,
}

impl SimSummary {}

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
            let event = match parse_sim_marker(line) {
                Ok(Some(event)) => event,
                Ok(None) => continue,
                Err(error) => {
                    s.errors.push(SiteMessage {
                        message: error.to_string(),
                        site: String::new(),
                    });
                    continue;
                }
            };
            let rest = &event;
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
                "allocation_census" => match AllocationCensus::from_value(rest) {
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
                "cleanup_path" => {
                    if let Some(path) = extract_str_field(rest, "path") {
                        s.cleanup_paths.push(path);
                    }
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
        self.expects_pass.len() + self.expects_fail.len() + self.image_snapshots.len()
    }

    /// `streamed` = the live per-op stream already printed sited
    /// failures/errors (the normal single-run path). When false (concise
    /// multi-run), nothing was streamed, so sited entries must be reprinted
    /// here -- with their `[site]` -- or a failing sim would show only a
    /// count and no clue what broke.
    fn print(&self, sim_name: &str, streamed: bool) {
        // Per-fail lines: when streamed, only re-print failures that DIDN'T
        // come through the live stream (empty-site, post-run synthesis:
        // image diff, runner errors) -- reprinting sited
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
            self.expects_pass.len(),
            total
        );
    }
}

/// Foreign stderr writers can leave a prefix before a complete marker. All
/// consumers validate the entire payload before interpreting any operation.
fn parse_sim_marker(line: &str) -> Result<Option<serde_json::Value>> {
    let Some((_, payload)) = line.split_once("__SIM__") else {
        return Ok(None);
    };
    let raw = payload
        .strip_prefix(' ')
        .ok_or_else(|| anyhow!("invalid sim marker: missing payload separator: {line}"))?;
    let event: serde_json::Value = serde_json::from_str(raw)
        .map_err(|error| anyhow!("invalid sim marker ({error}): {line}"))?;
    if !event.is_object()
        || event
            .get("op")
            .and_then(serde_json::Value::as_str)
            .is_none_or(str::is_empty)
    {
        return Err(anyhow!(
            "invalid sim marker: expected an object with a nonempty string op: {line}"
        ));
    }
    Ok(Some(event))
}

fn extract_str_field(json: &serde_json::Value, key: &str) -> Option<String> {
    json.get(key)?.as_str().map(str::to_owned)
}

fn extract_raw_field(json: &serde_json::Value, key: &str) -> Option<String> {
    json.get(key).map(serde_json::Value::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    extern "C" fn block_in_shared_address_space(_: *mut libc::c_void) -> libc::c_int {
        // Raw syscall only: this child runs on the test's heap and TLS.
        loop {
            unsafe {
                libc::syscall(
                    libc::SYS_ppoll,
                    std::ptr::null_mut::<libc::pollfd>(),
                    0usize,
                    std::ptr::null::<libc::timespec>(),
                    std::ptr::null::<libc::sigset_t>(),
                );
            }
        }
    }

    /// `spawn` can return while the child is still inside execve on the
    /// runner's address space; measured then, a fat runner reads as a fat
    /// child and the cap kills a healthy sim worker before its first entry.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_fat_runner_does_not_trip_the_cap_of_a_small_child() {
        let ballast = vec![1u8; 512 << 20];
        let mut stack = vec![0u8; 256 << 10];
        let stack_top = (stack.as_mut_ptr() as usize + stack.len()) & !15;
        let pid = unsafe {
            libc::clone(
                block_in_shared_address_space,
                stack_top as *mut libc::c_void,
                libc::CLONE_VM | libc::SIGCHLD,
                std::ptr::null_mut(),
            )
        };
        assert!(pid > 0, "clone: {}", std::io::Error::last_os_error());
        let measured = process_memory_mb(pid as u32);
        unsafe {
            libc::kill(pid, libc::SIGKILL);
            libc::waitpid(pid, std::ptr::null_mut(), 0);
        }
        std::hint::black_box(&ballast);
        assert_eq!(
            measured, None,
            "a child on the runner's address space was measured as its own"
        );

        // Killed below; it must outlive the wait for its own exec.
        let mut small = std::process::Command::new("sleep")
            .arg("3600")
            .spawn()
            .expect("spawn sleep");
        let deadline = rock_core::clock::Instant::now() + test_utils::HANG_BACKSTOP;
        let measured = loop {
            if let Some(mb) = process_memory_mb(small.id()) {
                break Some(mb);
            }
            if rock_core::clock::Instant::now() > deadline {
                break None;
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        let _ = small.kill();
        let _ = small.wait();
        let mb = measured.expect("an exec'd child is measured");
        assert!(mb < 256, "sleep measured at {mb} MB");
    }

    #[test]
    fn sim_receipt_prepare_releases_session() {
        const HELPER: &str = "ROCK_TEST_RECEIPT_RELEASE_HELPER";
        if let Some(root) = std::env::var_os(HELPER) {
            let root = PathBuf::from(root);
            let prepared = prepare_sims(
                crate::Backend::Skull,
                root.join("project"),
                root.join("output"),
            )
            .expect("prepare receipt");
            assert!(
                prepared.stone.is_none(),
                "receipt preparation retained its compiler session"
            );
            return;
        }
        let root = tempfile::tempdir().expect("receipt fixture");
        std::fs::create_dir(root.path().join("project")).unwrap();
        std::fs::create_dir(root.path().join("output")).unwrap();
        std::fs::write(
            root.path().join("project/main.rock"),
            "@nomangle fn answer() -> int { 41 }\n",
        )
        .unwrap();
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "run_sim::tests::sim_receipt_prepare_releases_session",
                "--nocapture",
            ])
            .env(HELPER, root.path())
            .env(SIM_COMPILE_RECEIPT_DIR_ENV, root.path().join("receipts"))
            .env(SIM_COMPILE_RECEIPT_PREPARE_ENV, "1")
            .output()
            .expect("receipt test child");
        assert!(
            child.status.success(),
            "{}{}",
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr)
        );
    }

    #[test]
    fn sim_receipt_registry_requires_source_files() {
        let wire_names = vec!["probe".to_string()];
        let env_by_wire = HashMap::new();
        let missing = registry_json(&wire_names, &env_by_wire, &HashMap::new()).unwrap_err();
        assert!(
            missing
                .to_string()
                .contains("compiled @sim `probe` has no recorded source file")
        );

        let malformed = serde_json::json!([{"wire_name":"probe", "env":[]}]);
        assert!(
            parse_registry(&malformed)
                .unwrap_err()
                .to_string()
                .contains("source file for `probe` is missing or invalid")
        );
    }

    #[test]
    fn receipt_sweep_reclaims_dead_pid_roots_and_keeps_live_ones() {
        let tmp = tempfile::tempdir().unwrap();
        // `u32::MAX` converts to no unix pid: dead on every platform the
        // liveness probe runs on.
        let dead = tmp.path().join(format!("sim-receipt-{}-stale", u32::MAX));
        let live = tmp
            .path()
            .join(format!("sim-receipt-{}-running", std::process::id()));
        let legacy = tmp.path().join("sim-receipt-nopid");
        let other = tmp.path().join("sim-output-keep");
        for dir in [&dead, &live, &legacy, &other] {
            std::fs::create_dir_all(dir).unwrap();
        }
        // A published receipt does not spare a dead runner's root: the
        // tempdir is one-run-owned, never shared.
        std::fs::write(dead.join("receipt.json"), "{}").unwrap();
        sweep_stale_sim_receipt_roots(tmp.path());
        assert!(live.is_dir(), "a concurrent live compile keeps its root");
        assert!(
            legacy.is_dir(),
            "an unparseable fresh root keeps the safe direction"
        );
        assert!(other.is_dir(), "outside the prefix nothing is touched");
        #[cfg(unix)]
        assert!(!dead.exists(), "a dead runner's root is reclaimed");
        #[cfg(not(unix))]
        assert!(
            dead.is_dir(),
            "without a liveness probe the safe direction keeps"
        );
    }

    #[test]
    fn reload_lane_reuses_the_compiled_session_and_isolates_its_chunk() {
        let project = tempfile::tempdir().expect("project");
        let shared = tempfile::tempdir().expect("shared output");
        let source = project.path().join("main.rock");
        std::fs::write(&source, "@nomangle fn answer() -> int { 41 }\n").unwrap();
        let main = shared.path().join(rock_host::project::MAIN_LUA_FILENAME);
        let stone = crate::skull_backend::IncrementalProject::init_with_tests(
            project.path().to_str().unwrap(),
            &main,
            &shared.path().join("incremental.lua"),
        )
        .expect("compile once");
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
        )
        .expect("reuse without reading source")
        .expect("selected lane");
        assert_eq!(lane.stone.artifact_fingerprint(), fingerprint);
        let lane_main = Path::new(&lane.output).join(rock_host::project::MAIN_LUA_FILENAME);
        assert_eq!(std::fs::read(&lane_main).unwrap(), original);
        std::fs::write(&lane_main, "fixture rewrite").unwrap();
        assert_eq!(std::fs::read(&main).unwrap(), original);
    }

    /// Assets are stubbed for EVERY sim child, rendering included: a stubbed
    /// image is one white texel, which is what a sim's layout and gameplay
    /// assertions sample. Only a declared or inherited "0" turns it off.
    #[test]
    fn every_sim_child_stubs_its_assets_unless_asked_not_to() {
        // The inherited escape hatch is one-way: it can ask for real assets,
        // never for stubs. A stray "1" must not strip pixels out of an
        // image snapshot.
        assert_eq!(unstub_from(None), "1", "unset stays stubbed");
        assert_eq!(unstub_from(Some("0")), "0", "an inherited 0 unstubs");
        assert_eq!(
            unstub_from(Some("1")),
            "1",
            "an inherited 1 is the default anyway"
        );
        assert_eq!(
            unstub_from(Some("yes")),
            "1",
            "anything else is not an unstub request"
        );

        // No ambient request, so the runner's own default, which every entry now
        // takes. A fixture that needs the file's own texels declares
        // `@sim_env("STONE_ASSET_STUB", "0")` and `preset_env` carries it.
        if std::env::var("STONE_ASSET_STUB").is_err() {
            assert_eq!(asset_stub_value(), "1", "the runner default is stubbed");
        }
    }

    /// `entry_renders` reads the EFFECTIVE value, last-wins. `@sim_render` is
    /// lowered to `SIM_DRAW=on_demand` before an explicit `@sim_env` is appended,
    /// so a sim carrying both is whatever it says last -- and that is the value
    /// its child actually boots with. Nothing schedules on it now, but the
    /// census and the capability docs classify sims by it.
    #[test]
    fn rendering_is_the_effective_last_wins_value() {
        let pair = |k: &str, v: &str| (k.to_string(), v.to_string());
        assert!(!entry_renders(&[]), "declaring nothing does not render");
        assert!(
            entry_renders(&[pair("SIM_DRAW", "on_demand")]),
            "@sim_render sugar"
        );
        assert!(!entry_renders(&[pair("SIM_DRAW", "none")]), "explicit skip");
        assert!(
            !entry_renders(&[pair("SIM_DRAW", "commands")]),
            "submit needs no GPU result"
        );
        assert!(
            entry_renders(&[pair("SIM_DRAW", "every_frame")]),
            "continuous GPU rendering"
        );
        // `@sim_render` then an explicit override, in lowering order.
        assert!(
            !entry_renders(&[pair("SIM_DRAW", "on_demand"), pair("SIM_DRAW", "none")]),
            "a later explicit @sim_env overrides the @sim_render sugar"
        );
        assert!(
            entry_renders(&[pair("SIM_DRAW", "none"), pair("SIM_DRAW", "on_demand")]),
            "... and the other way round"
        );
        assert!(
            !entry_renders(&[pair("GAME_FORCE_VFX", "1")]),
            "an unrelated key is not it"
        );
    }

    /// A MISSING image golden used to bootstrap itself from the current frame
    /// and count as a passing expect, so a sim whose goldens are untracked
    /// (`game/tests/sims/vfx_capture.snap/`, gitignored as display-specific)
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
        assert!(
            !baseline.exists(),
            "and it must NOT have been written behind our back"
        );

        // Both sides: with --update-snapshots, writing it is the point.
        let r = handle_image_snapshot(&img, &baseline, true).expect("no hard error");
        assert!(
            matches!(r, SnapResult::Wrote),
            "update writes the baseline; got {r:?}"
        );
        assert!(baseline.exists(), "the baseline is created under update");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The lane discriminant decides whether a child may touch the one live
    /// `IncrementalProject` session and the one `incremental.lua`. Getting it wrong in
    /// the permissive direction lets an ordinary child mutate build state
    /// shared with every other child once the suite runs in parallel.
    #[test]
    fn reload_lane_is_opt_in_via_stone_hot_reload() {
        let pair = |k: &str, v: &str| vec![(k.to_string(), v.to_string())];

        assert!(is_reload_lane(&pair("STONE_HOT_RELOAD", "1")));
        // Only the opt-in key routes to the lane; a busy sim env does not.
        assert!(!is_reload_lane(&pair("SIM_DRAW", "on_demand")));
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

    #[test]
    fn the_workload_env_applies_defaults_last_wins_and_keeps_out_literal() {
        let pairs = |items: &[(&str, &str)]| -> Vec<(String, String)> {
            items
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        assert_eq!(
            workload_env(&[]),
            workload_env(&pairs(&[("SIM_DRAW", "none")])),
            "omitting a runner default and declaring it are the same workload"
        );
        assert_ne!(
            workload_env(&[]),
            workload_env(&pairs(&[("SIM_DRAW", "on_demand")]))
        );
        let last = workload_env(&pairs(&[("SIM_DRAW", "none"), ("SIM_DRAW", "on_demand")]));
        assert!(last.contains(&("SIM_DRAW".into(), "on_demand".into())));
        let out = workload_env(&pairs(&[("GAME_HISTORY_DIR", "$OUT/history")]));
        assert!(out.contains(&("GAME_HISTORY_DIR".into(), "$OUT/history".into())));
    }

    /// Only the pair marks a lane: real assets with the cap declined. The
    /// runner's stub default and the baseline cap stand in for what a fixture
    /// does not say, last declaration wins, and the render flag is not read.
    #[test]
    fn entry_full_assets_needs_real_assets_and_no_image_cap() {
        let pair = |k: &str, v: &str| (k.to_string(), v.to_string());
        assert!(
            !entry_full_assets(&[], "1"),
            "declaring nothing is stubbed and capped"
        );
        assert!(
            !entry_full_assets(&[pair("STONE_ASSET_STUB", "0")], "1"),
            "real assets alone stay capped"
        );
        assert!(
            !entry_full_assets(&[pair("STONE_IMAGE_MAX_SIDE", "0")], "1"),
            "an uncapped stub is one texel"
        );
        assert!(entry_full_assets(
            &[
                pair("STONE_ASSET_STUB", "0"),
                pair("STONE_IMAGE_MAX_SIDE", "0")
            ],
            "1"
        ));
        assert!(
            entry_full_assets(
                &[
                    pair("STONE_IMAGE_MAX_SIDE", "0"),
                    pair("SIM_DRAW", "commands")
                ],
                "0"
            ),
            "an inherited STONE_ASSET_STUB=0 counts, and submit is not consulted"
        );
        assert!(
            !entry_full_assets(
                &[
                    pair("STONE_ASSET_STUB", "0"),
                    pair("STONE_IMAGE_MAX_SIDE", "0"),
                    pair("STONE_IMAGE_MAX_SIDE", "512")
                ],
                "1"
            ),
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
        assert!(
            results
                .iter()
                .all(|(i, r)| r.as_ref().map(|v| v == i).unwrap_or(false))
        );
        assert!(
            peak.load(Ordering::SeqCst) <= 2,
            "peak live rendering {}",
            peak.load(Ordering::SeqCst)
        );
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
        pool: impl FnOnce(
            &PoolWidth,
            &(dyn Fn(usize) -> Result<usize> + Sync),
        ) -> Vec<(usize, Result<usize>)>
        + Send,
    ) {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        // Ordered by events, not sleeps: tasks beside the lane hold until it
        // ends, the lane ends once the narrowed pool is full, and after it
        // three tasks must be live together. A missing widening leaves that
        // barrier at two until the hang backstop, which every wait shares.
        let deadline = rock_core::clock::Instant::now() + test_utils::HANG_BACKSTOP;
        let wait_for = |done: &dyn Fn() -> bool| {
            while !done() && rock_core::clock::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        };
        let live = AtomicUsize::new(0);
        let peak_beside_lane = AtomicUsize::new(0);
        let peak_after_lane = AtomicUsize::new(0);
        let lane_done = AtomicBool::new(false);
        let task = |i: usize| {
            let now = live.fetch_add(1, Ordering::SeqCst) + 1;
            if lane_done.load(Ordering::SeqCst) {
                peak_after_lane.fetch_max(now, Ordering::SeqCst);
                wait_for(&|| peak_after_lane.load(Ordering::SeqCst) >= 3);
            } else {
                peak_beside_lane.fetch_max(now, Ordering::SeqCst);
                wait_for(&|| lane_done.load(Ordering::SeqCst));
            }
            live.fetch_sub(1, Ordering::SeqCst);
            Ok::<usize, anyhow::Error>(i)
        };
        let (pool, ()) = beside_reload_lane(
            3,
            true,
            |width| pool(width, &task),
            || {
                wait_for(&|| live.load(Ordering::SeqCst) >= 2);
                // A pool that ignored the lane's slot would start a third
                // task in this window.
                std::thread::sleep(std::time::Duration::from_millis(40));
                lane_done.store(true, Ordering::SeqCst);
            },
        );
        assert_eq!(pool.len(), 60, "every task exactly once");
        assert_eq!(
            peak_beside_lane.load(Ordering::SeqCst),
            2,
            "the lane holds one of three jobs while it runs"
        );
        assert_eq!(
            peak_after_lane.load(Ordering::SeqCst),
            3,
            "the pool widens to every job once the lane is done"
        );
    }

    #[test]
    fn lanes_dispatch_longest_first_with_untimed_lanes_last_in_plan_order() {
        let estimates = [
            Some(1_000),
            None,
            Some(30_000),
            Some(1_000),
            None,
            Some(8_000),
        ];
        assert_eq!(longest_first(&estimates), vec![2, 5, 0, 3, 1, 4]);
    }

    #[test]
    fn the_classed_pool_with_no_cap_is_the_plain_pool() {
        let renders = vec![true; 5];
        let results = run_bounded_classed(&PoolWidth::fixed(4), 4, &renders, |i| {
            Ok::<usize, anyhow::Error>(i * 2)
        });
        let mut got: Vec<(usize, usize)> =
            results.into_iter().map(|(i, r)| (i, r.unwrap())).collect();
        got.sort_unstable();
        assert_eq!(got, (0..5).map(|i| (i, i * 2)).collect::<Vec<_>>());
    }

    #[test]
    fn sim_environment_cap_reaches_default_lane_resolution() {
        const CHILD: &str = "ROCK_SIM_CAP_PROBE";
        if std::env::var_os(CHILD).is_some() {
            assert_eq!(resolve_jobs(None, None, false, false).unwrap(), 1);
            assert_eq!(resolve_pool_jobs(None, None, false, false).unwrap(), 1);
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "run_sim::tests::sim_environment_cap_reaches_default_lane_resolution",
                "--exact",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("ROCK_SIM_JOBS", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn sim_environment_caps_defaults_but_explicit_flags_win() {
        let budget = JobBudget { ram: 10, gpu: 8 };
        assert_eq!(
            resolve_jobs_with(None, false, 32, budget, false, Some("2")).unwrap(),
            2
        );
        assert_eq!(
            resolve_jobs_with(Some(6), false, 32, budget, false, Some("2")).unwrap(),
            6
        );
        assert_eq!(
            resolve_jobs_with(Some(12), false, 32, budget, true, Some("2")).unwrap(),
            12
        );
        assert!(resolve_jobs_with(Some(12), false, 32, budget, false, Some("2")).is_err());
        assert_eq!(
            resolve_jobs_with(Some(6), true, 32, budget, true, Some("2")).unwrap(),
            1
        );
        assert_eq!(
            resolve_jobs_with(None, false, 32, budget, false, Some("99")).unwrap(),
            budget.lanes()
        );
        for cap in [
            None,
            Some("0"),
            Some("invalid"),
            Some("-1"),
            Some("184467440737095516160"),
        ] {
            assert_eq!(
                resolve_jobs_with(None, false, 32, budget, false, cap).unwrap(),
                budget.lanes()
            );
        }
    }

    /// The job count is a safety bound, not a preference: it must never exceed
    /// what the machine can run, and `--update-snapshots` must pin to serial
    /// because it writes baseline files whose destinations are not proven
    /// unique before spawning.
    #[test]
    fn resolve_jobs_is_bounded_and_serial_for_snapshot_updates() {
        let (cores, budget) = (32, JobBudget { ram: 4, gpu: 4 });

        assert_eq!(
            resolve_jobs_with(None, false, cores, budget, false, None).unwrap(),
            4,
            "no request takes the memory budget"
        );
        assert_eq!(
            resolve_jobs_with(Some(1), false, cores, budget, false, None).unwrap(),
            1
        );
        let error = resolve_jobs_with(Some(10), false, cores, budget, false, None).unwrap_err();
        assert!(error.to_string().contains("RAM budget of 4"));
        assert!(error.to_string().contains("--jobs-force"));
        assert_eq!(
            resolve_jobs_with(Some(4), false, cores, budget, false, None).unwrap(),
            4
        );
        assert_eq!(
            resolve_jobs_with(Some(8), true, cores, budget, false, None).unwrap(),
            1
        );
        // A forced request may exceed the budget, never the cores.
        assert_eq!(
            resolve_jobs_with(Some(10), false, cores, budget, true, None).unwrap(),
            10
        );
        assert_eq!(
            resolve_jobs_with(Some(usize::MAX), false, cores, budget, true, None).unwrap(),
            cores
        );
        // 0 is meaningless; clamp up rather than deadlock on an empty pool.
        assert_eq!(
            resolve_jobs_with(Some(0), false, cores, budget, false, None).unwrap(),
            1
        );
        // --update-snapshots wins over ANY explicit request.
        assert_eq!(
            resolve_jobs_with(None, true, cores, budget, true, None).unwrap(),
            1
        );
        assert_eq!(
            resolve_jobs_with(Some(8), true, cores, budget, true, None).unwrap(),
            1
        );
    }

    #[test]
    fn a_request_above_only_the_free_gpu_budget_runs_at_that_budget() {
        let (cores, budget) = (32, JobBudget { ram: 10, gpu: 2 });
        assert_eq!(
            resolve_jobs_with(Some(8), false, cores, budget, false, None).unwrap(),
            2
        );
        assert_eq!(
            resolve_jobs_with(Some(10), false, cores, budget, false, None).unwrap(),
            2
        );
        let error = resolve_jobs_with(Some(11), false, cores, budget, false, None).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("--jobs 11 exceeds the RAM budget of 10"),
            "{error}"
        );
        // Force still takes the request as asked.
        assert_eq!(
            resolve_jobs_with(Some(8), false, cores, budget, true, None).unwrap(),
            8
        );
        assert_eq!(
            resolve_jobs_with(None, false, cores, budget, false, None).unwrap(),
            2
        );
    }

    #[test]
    fn a_request_above_the_known_lanes_is_narrowed_before_the_budget() {
        let (cores, budget) = (32, JobBudget { ram: 1, gpu: 1 });
        // One name under a one-lane budget runs in its one lane.
        assert_eq!(
            resolve_jobs_with(
                narrow_to_lanes(Some(4), Some(1)),
                false,
                cores,
                budget,
                false,
                None
            )
            .unwrap(),
            1
        );
        // Two names still need two lanes, which the RAM budget refuses.
        let error = resolve_jobs_with(
            narrow_to_lanes(Some(4), Some(2)),
            false,
            cores,
            budget,
            false,
            None,
        )
        .unwrap_err();
        assert!(error.to_string().contains("--jobs 2 exceeds"), "{error}");
        // Unknown lanes leave the request alone; no request stays no request.
        assert_eq!(narrow_to_lanes(Some(4), None), Some(4));
        assert_eq!(narrow_to_lanes(None, Some(2)), None);
        assert_eq!(narrow_to_lanes(Some(0), Some(2)), Some(0));
    }

    #[test]
    fn the_default_job_count_leaves_a_fixed_gpu_memory_reserve() {
        // Four lanes' worth above the reserve: four lanes.
        assert_eq!(
            vram_budgeted_jobs(32, Some(SIM_VRAM_RESERVE_MIB + 4 * SIM_LANE_VRAM_MIB + 1)),
            4
        );
        // A small machine with a large card stays bounded by its cores.
        assert_eq!(
            vram_budgeted_jobs(2, Some(SIM_VRAM_RESERVE_MIB + 64 * SIM_LANE_VRAM_MIB)),
            2
        );
        // Nothing above the reserve still runs one lane rather than none.
        assert_eq!(vram_budgeted_jobs(32, Some(SIM_VRAM_RESERVE_MIB)), 1);
        assert_eq!(vram_budgeted_jobs(32, Some(SIM_LANE_VRAM_MIB / 4)), 1);
        // Without a reading (no NVIDIA tooling, e.g. macOS) the cores decide.
        assert_eq!(vram_budgeted_jobs(14, None), 14);
    }

    #[test]
    fn default_jobs_fit_ram_and_gpu_budgets() {
        let studio = memory_budgets(14, Some(36 * 1024), None);
        assert_eq!(studio.lanes(), 10);
        assert_eq!(
            resolve_jobs_with(None, false, 14, studio, false, None).unwrap(),
            10
        );
        assert_eq!(
            resolve_jobs_with(Some(14), false, 14, studio, true, None).unwrap(),
            14
        );
        assert_eq!(
            resolve_jobs_with(Some(14), true, 14, studio, true, None).unwrap(),
            1
        );
        assert_eq!(memory_budgets(32, Some(62 * 1024), Some(17851)).lanes(), 5);
        assert_eq!(memory_budgets(32, Some(62 * 1024), None).lanes(), 17);
        assert_eq!(memory_budgets(2, Some(36 * 1024), None).lanes(), 2);
        assert_eq!(memory_budgets(14, Some(0), None).lanes(), 1);
        assert_eq!(memory_budgets(0, Some(36 * 1024), None).lanes(), 1);
        assert_eq!(memory_budgets(14, None, None).lanes(), 14);
    }

    #[test]
    fn nvidia_smi_free_memory_reads_the_first_gpu() {
        assert_eq!(parse_nvidia_smi_free_mib("19115\n"), Some(19115));
        assert_eq!(parse_nvidia_smi_free_mib(" 19115\n4000\n"), Some(19115));
        assert_eq!(parse_nvidia_smi_free_mib(""), None);
        assert_eq!(parse_nvidia_smi_free_mib("[N/A]\n"), None);
    }

    #[test]
    fn a_hot_reload_sim_is_reload_lane() {
        let mut env: HashMap<String, Vec<(String, String)>> = HashMap::new();
        env.insert("plain".into(), vec![("ROCK_TEST_SEED".into(), "42".into())]);
        env.insert(
            "reloader".into(),
            vec![("STONE_HOT_RELOAD".into(), "1".into())],
        );

        assert!(!entry_is_reload_lane("plain", &env));
        assert!(entry_is_reload_lane("reloader", &env));
        // A name with no recorded env is ordinary, not a panic.
        assert!(!entry_is_reload_lane("unknown", &env));
    }

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn a_declared_frame_count_is_the_entrys_budget() {
        assert_eq!(
            frame_budget_for("s", &env(&[])).expect("default"),
            DEFAULT_FRAME_BUDGET
        );
        assert_eq!(
            frame_budget_for("s", &env(&[("FRAME_COUNT", "90")])).expect("declared"),
            90
        );
        // The two watchdog sims in sample declare this; a runner that quietly
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
    fn a_declared_timeout_is_the_entrys_wall_budget() {
        assert_eq!(
            timeout_for("s", &env(&[])).expect("default"),
            Duration::from_secs(sim_timeout_secs())
        );
        assert_eq!(
            timeout_for("s", &env(&[("ROCK_SIM_TIMEOUT_SECS", "1200")])).expect("declared"),
            Duration::from_secs(1200)
        );
        // Other keys are not budgets, and the last declaration wins.
        assert_eq!(
            timeout_for("s", &env(&[("FRAME_COUNT", "90")])).expect("default"),
            Duration::from_secs(sim_timeout_secs())
        );
        assert_eq!(
            timeout_for(
                "s",
                &env(&[
                    ("ROCK_SIM_TIMEOUT_SECS", "90"),
                    ("ROCK_SIM_TIMEOUT_SECS", "1200")
                ])
            )
            .expect("declared"),
            Duration::from_secs(1200)
        );
    }

    #[test]
    fn an_unusable_timeout_is_rejected_not_defaulted() {
        for bad in ["", "0", "-1", "abc", "1.5"] {
            assert!(
                timeout_for("s", &env(&[("ROCK_SIM_TIMEOUT_SECS", bad)])).is_err(),
                "`ROCK_SIM_TIMEOUT_SECS={bad}` must reject rather than silently become the default"
            );
        }
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
            frame_budget_for("s", &env(&[("FRAME_COUNT", "90"), ("FRAME_COUNT", "bad")])).is_err(),
            "the effective last declaration is the one that must be validated"
        );
    }

    #[test]
    fn buffered_runs_never_write_live() {
        assert!(writes_live(false));
        assert!(!writes_live(true));
    }

    fn lines(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|s| s.to_string()).collect()
    }

    /// The deadline anchor resets ONLY on `EnvelopeStep::Started`, so the
    /// envelope's classification IS the guard's admission: a start the split
    /// would reject -- malformed, for another sim, or a second one -- must not
    /// read as Started.
    #[test]
    fn only_an_accepted_entry_start_reads_as_started() {
        let started =
            |t: &mut Envelope, line: &str| matches!(t.observe(line), Ok(EnvelopeStep::Started));

        let mut t = Envelope::new("a");
        assert!(
            started(&mut t, &start("a")),
            "the child's own start is Started"
        );
        assert!(
            t.observe(r#"__SIM__ {"op":"entry_start"}"#).is_err(),
            "malformed: no name"
        );
        assert!(
            t.observe(&start("a")).is_err(),
            "a second start while the entry is open"
        );
        assert!(matches!(
            t.observe(&end("a")),
            Ok(EnvelopeStep::Ended(None, None))
        ));
        assert!(
            t.observe(&start("a")).is_err(),
            "a closed entry cannot restart"
        );

        assert!(
            Envelope::new("a").observe(&start("b")).is_err(),
            "another sim's name"
        );
    }

    /// The wedged-child evasion, deterministically: a sim stuck inside one
    /// tick can flood `entry_start`-shaped lines, and the deadline may reset on
    /// none of them. After any violation the guard is poisoned, so even a line
    /// that would otherwise be the valid start extends nothing.
    #[test]
    fn the_deadline_guard_resets_only_on_the_accepted_start() {
        let mut g = DeadlineGuard::new("a");
        assert!(g.observe_resets(&start("a")), "the child's start resets");
        assert!(
            !g.observe_resets(&expect_ok("inside a")),
            "an ordinary op does not"
        );
        assert!(
            !g.observe_resets("[   0.1s INFO ] raw logging"),
            "raw output does not"
        );
        assert!(!g.observe_resets(&start("a")), "a second start does not");

        let mut g = DeadlineGuard::new("a");
        assert!(
            !g.observe_resets(&expect_ok("before the start")),
            "an op before the start poisons"
        );
        assert!(
            !g.observe_resets(&start("a")),
            "a poisoned guard must not resume resetting on a later well-formed start"
        );
    }

    #[test]
    fn entry_frame_timing_is_validated_and_retained() {
        let mut envelope = Envelope::new("a");
        assert!(envelope.observe(&start("a")).is_ok());
        let valid = end_with_frame_timing(
            "a",
            3,
            r#"{"frames":3,"draw_frames":2,"submitted_frames":1,"logic_us":10,"render_cpu_us":20,"present_us":30}"#,
        );
        assert!(matches!(
            envelope.observe(&valid),
            Ok(EnvelopeStep::Ended(
                Some(3),
                Some(EntryFrameTiming {
                    frames: 3,
                    draw_frames: 2,
                    submitted_frames: 1,
                    logic_us: 10,
                    render_cpu_us: 20,
                    present_us: 30,
                }),
            ))
        ));

        for timing in [
            r#"{"frames":3,"draw_frames":4,"submitted_frames":1,"logic_us":10,"render_cpu_us":20,"present_us":30}"#,
            r#"{"frames":2,"draw_frames":2,"submitted_frames":1,"logic_us":10,"render_cpu_us":20,"present_us":30}"#,
            r#"{"frames":3,"draw_frames":2,"submitted_frames":1,"logic_us":-1,"render_cpu_us":20,"present_us":30}"#,
        ] {
            let mut envelope = Envelope::new("a");
            assert!(envelope.observe(&start("a")).is_ok());
            assert!(
                envelope
                    .observe(&end_with_frame_timing("a", 3, timing))
                    .is_err(),
                "invalid frame timing must reject: {timing}"
            );
        }
    }

    fn start(name: &str) -> String {
        format!(r#"__SIM__ {{"op":"entry_start","name":"{name}"}}"#)
    }

    fn end(name: &str) -> String {
        format!(r#"__SIM__ {{"op":"entry_end","name":"{name}"}}"#)
    }

    fn end_with_frame_count(name: &str, frame_count: &str) -> String {
        format!(r#"__SIM__ {{"op":"entry_end","name":"{name}","frame_count":{frame_count}}}"#)
    }

    fn end_with_frame_timing(name: &str, frame_count: usize, timing: &str) -> String {
        format!(
            r#"__SIM__ {{"op":"entry_end","name":"{name}","frame_count":{frame_count},"frame_timing":{timing}}}"#
        )
    }

    fn expect_ok(msg: &str) -> String {
        format!(r#"__SIM__ {{"op":"expect_ok","message":"{msg}","site":"f.rock:1"}}"#)
    }

    /// Operations inside the envelope are the sim's; boot and teardown
    /// logging outside it is not part of the entry but stays in the debugging
    /// stream.
    #[test]
    fn the_envelope_partitions_operations_and_logging() {
        let stream = vec![
            "[   0.1s INFO ] booting".to_string(),
            start("a"),
            "[   0.2s INFO ] inside a".to_string(),
            expect_ok("from a"),
            end_with_frame_count("a", "7"),
            "[   0.4s INFO ] teardown".to_string(),
        ];

        let out = split_child_stream(&stream, "a").expect("well-formed stream");
        assert_eq!(out.sim, vec![expect_ok("from a")]);
        assert!(out.started && out.terminated);
        assert_eq!(out.frame_count, Some(7));
        assert!(out.raw_all.iter().any(|l| l.contains("booting")));
        assert!(out.raw_all.iter().any(|l| l.contains("teardown")));
    }

    /// A child killed mid-entry leaves its entry open. That is the ONE
    /// tolerated gap, and it is recorded rather than read as a clean close.
    #[test]
    fn a_killed_child_leaves_its_entry_unterminated() {
        let out = split_child_stream(&[start("a"), expect_ok("partial")], "a")
            .expect("a truncated stream is not malformed");
        assert!(out.started && !out.terminated);
        assert_eq!(out.sim, vec![expect_ok("partial")]);
    }

    #[test]
    fn malformed_entry_frame_counts_are_rejected() {
        for raw in ["-1", "1.5", "null", "\"seven\""] {
            assert!(
                split_child_stream(&[start("a"), end_with_frame_count("a", raw)], "a").is_err(),
                "invalid frame_count {raw} was accepted"
            );
        }
    }

    /// Every other shape is a protocol violation, and none may be quietly
    /// absorbed into the record.
    #[test]
    fn malformed_envelopes_are_rejected_not_guessed_at() {
        let cases: Vec<(&str, Vec<String>)> = vec![
            ("a second start", vec![start("a"), start("a")]),
            (
                "a restart after the end",
                vec![start("a"), end("a"), start("a")],
            ),
            ("an end with no entry open", vec![end("a")]),
            ("an end naming another sim", vec![start("a"), end("b")]),
            ("a start naming another sim", vec![start("wrong")]),
            (
                "a start with no name",
                lines(&[r#"__SIM__ {"op":"entry_start"}"#]),
            ),
            ("an operation before the entry", vec![expect_ok("orphan")]),
            (
                "an operation after the entry",
                vec![start("a"), end("a"), expect_ok("orphan")],
            ),
        ];
        for (what, stream) in cases {
            assert!(
                split_child_stream(&stream, "a").is_err(),
                "{what} was accepted"
            );
        }
    }

    #[test]
    fn extract_str_field_basic() {
        let json = serde_json::from_str::<serde_json::Value>(
            r#"{"op":"expect_fail","message":"1 segment"}"#,
        )
        .unwrap();
        assert_eq!(extract_str_field(&json, "op"), Some("expect_fail".into()));
        assert_eq!(
            extract_str_field(&json, "message"),
            Some("1 segment".into())
        );
        assert_eq!(extract_str_field(&json, "missing"), None);
    }

    #[test]
    fn extract_str_field_handles_escapes() {
        let json =
            serde_json::from_str::<serde_json::Value>(r#"{"message":"hello \"world\""}"#).unwrap();
        assert_eq!(
            extract_str_field(&json, "message"),
            Some(r#"hello "world""#.into())
        );
    }

    #[test]
    fn extract_raw_field_object_args() {
        let line = serde_json::from_str::<serde_json::Value>(
            r#"{"op":"custom","name":"x","args":{"a":1,"b":[2,3]}}"#,
        )
        .unwrap();
        assert_eq!(
            extract_raw_field(&line, "args"),
            Some(r#"{"a":1,"b":[2,3]}"#.to_string())
        );
    }

    #[test]
    fn extract_image_snapshot_tolerance_and_save_status() {
        let line = serde_json::from_str::<serde_json::Value>(
            r#"{"op":"image_snapshot","name":"x","tol":0.001,"ok":true,"path":"/tmp/x"}"#,
        )
        .unwrap();
        assert_eq!(extract_raw_field(&line, "tol"), Some("0.001".into()));
        assert_eq!(extract_raw_field(&line, "ok"), Some("true".into()));
    }

    #[test]
    fn extract_raw_field_handles_quotes_inside_strings() {
        // The "args" field contains a string with an escaped `}`.
        // Naive depth tracking would stop early at the string's `}`.
        let line =
            serde_json::from_str::<serde_json::Value>(r#"{"args":{"msg":"abc}def"},"after":1}"#)
                .unwrap();
        assert_eq!(
            extract_raw_field(&line, "args"),
            Some(r#"{"msg":"abc}def"}"#.to_string())
        );
    }

    #[test]
    fn requested_sim_names_resolve_to_registered_wire_names() {
        let wires: Vec<String> = [
            "game::boots",
            "game::ui::hover",
            "game::hover",
            "legacy_sim",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let resolve = |specs: &[&str]| {
            resolve_sim_specs(
                &wires,
                &specs.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            )
        };
        assert_eq!(resolve(&["boots"]).unwrap(), ["game::boots"]);
        assert_eq!(
            resolve(&["legacy_sim", "game::hover"]).unwrap(),
            ["legacy_sim", "game::hover"]
        );
        assert_eq!(resolve(&["ui::hover"]).unwrap(), ["game::ui::hover"]);
        let error = resolve(&["boots,ui::hover"]).unwrap_err().to_string();
        assert!(error.contains("contains a comma"), "{error}");
        let error = resolve(&["oots"]).unwrap_err().to_string();
        assert!(
            error.contains("unknown sim: oots"),
            "only whole segments match: {error}"
        );
        let error = resolve(&["hover"]).unwrap_err().to_string();
        assert!(
            error.contains("`hover` is ambiguous")
                && error.contains("game::ui::hover")
                && error.contains("game::hover"),
            "{error}"
        );
    }

    // A name the registry lacks used to pass through to the worker's entry
    // plan, where a name holding a line break (one argv element of
    // newline-joined names) split the plan line and killed the whole worker
    // with `malformed entry plan line: <a valid sim>`.
    #[test]
    fn requested_sim_names_that_are_not_sims_are_all_refused_before_boot() {
        let wires: Vec<String> = ["game::boots", "game::hover"]
            .into_iter()
            .map(String::from)
            .collect();
        let specs: Vec<String> = [
            "boots",
            "helper_probe",
            "boots\nhover",
            "missing_part",
            "other_helper",
        ]
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
            assert!(
                error.contains(expected),
                "missing `{expected}` in:\n{error}"
            );
        }
        assert!(
            !error.contains("unknown sim: boots"),
            "a valid sim must not be named unknown:\n{error}"
        );
    }

    #[test]
    fn entry_passed_requires_stop_and_no_failures() {
        // Shared pass-predicate for both multi-run paths. Default (no
        // stop_sim observed) is a fail even with zero expects.
        let mut s = SimSummary::default();
        assert!(!entry_passed(&s));
        s.saw_stop = true;
        s.expects_pass.push(SiteMessage {
            message: "ok".into(),
            site: "f:0".into(),
        });
        assert!(entry_passed(&s));
        s.expects_fail.push(SiteMessage {
            message: "x".into(),
            site: "f:1".into(),
        });
        assert!(!entry_passed(&s));
    }

    #[test]
    fn a_stopped_sim_whose_checks_never_ran_fails() {
        // A fixture that returns before its checks (`STATE ?? return`) still
        // reaches stop_sim() and fails nothing; its coverage is gone.
        let mut empty = SimSummary {
            saw_stop: true,
            ..SimSummary::default()
        };
        assert!(!entry_passed(&empty));
        assert_eq!(verdict_word(&empty), "FAIL");
        assert!(sim_outcome(&empty, "s").is_err());

        let mut declared = SimSummary {
            saw_stop: true,
            ..SimSummary::default()
        };
        declared.no_checks = Some("a seed; its probe checks".into());
        assert!(
            entry_passed(&declared),
            "a declared check-free entry passes"
        );

        empty.image_snapshots.push(ImageSnapshotOp {
            name: "n".into(),
            path: "frame.png".into(),
            tol: 0.001,
            save_ok: true,
            site: String::new(),
        });
        assert!(entry_passed(&empty), "an image snapshot is a check");
    }

    #[test]
    fn an_aborted_sim_prints_fail_not_pass() {
        // The imgui docking abort: zero expects ran, none failed, and the sim
        // never reached stop_sim(). The verdict word must follow the real
        // outcome, not the expect tally.
        let mut aborted = SimSummary::default();
        assert!(
            !aborted.has_failures(),
            "an abort fails no expect -- that is the trap"
        );
        assert_eq!(verdict_word(&aborted), "FAIL");

        aborted.saw_stop = true;
        aborted.expects_pass.push(SiteMessage {
            message: "ok".into(),
            site: "f:0".into(),
        });
        assert_eq!(verdict_word(&aborted), "PASS");

        let mut timed_out = SimSummary {
            saw_stop: true,
            ..SimSummary::default()
        };
        timed_out.timed_out = true;
        assert_eq!(verdict_word(&timed_out), "FAIL");

        let mut died = SimSummary {
            saw_stop: true,
            ..SimSummary::default()
        };
        died.process_failure_code = Some(1);
        assert_eq!(verdict_word(&died), "FAIL");
    }

    #[test]
    fn a_timeout_names_the_budget_that_fired() {
        // The single-run message must state the entry's declared budget, not
        // the process-wide default it overrode.
        let mut summary = SimSummary {
            saw_stop: true,
            ..SimSummary::default()
        };
        summary.timed_out = true;
        summary.timeout_budget = Some(Duration::from_secs(2));
        let error = sim_outcome(&summary, "declared_timeout_slow")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("sim `declared_timeout_slow` timed out after 2s"),
            "the timeout must name the sim and its declared budget: {error}"
        );
    }

    #[test]
    fn manual_gallery_selection_is_explicit_and_last_wins() {
        assert!(manual_sim(&[("SIM_MANUAL".into(), "1".into())]).unwrap());
        assert!(!manual_sim(&[]).unwrap());
        assert!(
            !manual_sim(&[
                ("SIM_MANUAL".into(), "1".into()),
                ("SIM_MANUAL".into(), "0".into())
            ])
            .unwrap()
        );
        assert!(manual_sim(&[("SIM_MANUAL".into(), "yes".into())]).is_err());
    }

    #[test]
    fn entry_excluded_is_not_passed_and_verdict_word_is_excluded() {
        let s = SimSummary {
            excluded: Some("opt-in fixture".to_string()),
            saw_stop: true,
            ..Default::default()
        };
        assert!(!entry_passed(&s));
        assert!(entry_excluded(&s));
        assert_eq!(verdict_word(&s), "EXCLUDED");
    }

    #[test]
    fn sim_summary_from_stderr_parses_exclude_op() {
        let stderr = "__SIM__ {\"op\":\"exclude\",\"reason\":\"opt-in fixture\"}\n";
        let s = SimSummary::from_stderr(stderr);
        assert_eq!(s.excluded.as_deref(), Some("opt-in fixture"));
        assert!(s.saw_stop);
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
    fn foreign_stderr_prefixes_preserve_envelopes_deadlines_and_verdicts() {
        let mut guard = DeadlineGuard::new("a");
        let stream: Vec<String> = [
            start("a"),
            expect_ok("checked"),
            r#"__SIM__ {"op": "stop"}"#.to_string(),
            end("a"),
        ]
        .into_iter()
        .map(|line| format!("foreign stderr: {line}"))
        .collect();
        assert!(guard.observe_resets(&stream[0]));
        for line in &stream[1..] {
            assert!(!guard.observe_resets(line));
        }
        let entry = split_child_stream(&stream, "a").unwrap();
        assert!(entry.started && entry.terminated);
        assert_eq!(entry.raw_all, vec!["foreign stderr: "; 4]);
        assert_eq!(
            verdict_word(&SimSummary::from_stderr(&entry.sim.join("\n"))),
            "PASS"
        );
        for line in [
            "noise __SIM__",
            "noise __SIM__ broken JSON",
            r#"noise __SIM__ {"op":"stop"}junk"#,
        ] {
            let error = split_child_stream(&[start("a"), line.to_string()], "a")
                .err()
                .unwrap();
            assert!(error.to_string().contains("invalid sim marker"));
        }
    }

    #[test]
    fn summary_stop_survives_foreign_unterminated_stderr() {
        let production = std::fs::read_to_string(
            rock_stdlib::package_dir("stone")
                .unwrap()
                .join("engine/sim.rock"),
        )
        .unwrap();
        let emitter = production
            .split_once("inline(lua) fn _sim_emit_impl(")
            .unwrap()
            .1
            .split_once("\"\"\"")
            .unwrap()
            .1
            .split_once("\"\"\"")
            .unwrap()
            .0;
        let script = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            script.path(),
            format!(
                r#"
local function emit(json_line, site)
{emitter}
end
emit('{{"op":"expect_ok","message":"checked"}}', 'test.rock:1')
io.stderr:write('audio stream error: A backend-specific error has occurred: ')
io.stderr:flush()
emit('{{"op":"stop"}}', 'test.rock:2')
io.stderr:write('`alsa::poll()` returned POLLERR\n')
"#
            ),
        )
        .unwrap();
        let child = std::process::Command::new(rock_host::project::luajit_path())
            .arg(script.path())
            .env("STONE_HEADLESS", "1")
            .output()
            .unwrap();
        assert!(
            child.status.success(),
            "{}",
            String::from_utf8_lossy(&child.stderr)
        );
        let summary = SimSummary::from_stderr(&String::from_utf8(child.stderr).unwrap());
        assert_eq!(
            verdict_word(&summary),
            "PASS",
            "no stop_sim() observed: {}",
            !summary.saw_stop
        );
    }

    // Each emission once ran two ffi.cdef parses, about 8 ctypes, so a long sim
    // overflowed LuaJIT's ctype table near 8,000 markers.
    #[test]
    fn many_sim_markers_do_not_exhaust_ffi_ctypes() {
        let production = std::fs::read_to_string(
            rock_stdlib::package_dir("stone")
                .unwrap()
                .join("engine/sim.rock"),
        )
        .unwrap();
        let emitter = production
            .split_once("inline(lua) fn _sim_emit_impl(")
            .unwrap()
            .1
            .split_once("\"\"\"")
            .unwrap()
            .1
            .split_once("\"\"\"")
            .unwrap()
            .0;
        let script = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            script.path(),
            format!(
                r#"
local function emit(json_line, site)
{emitter}
end
for i = 1, 20000 do emit('{{"op":"expect_ok","message":"checked"}}', 'test.rock:1') end
emit('{{"op":"stop"}}', 'test.rock:2')
"#
            ),
        )
        .unwrap();
        let child = std::process::Command::new(rock_host::project::luajit_path())
            .arg(script.path())
            .env("STONE_HEADLESS", "1")
            .output()
            .unwrap();
        let stderr = String::from_utf8(child.stderr).unwrap();
        assert!(
            child.status.success(),
            "{}",
            stderr.lines().rev().take(5).collect::<Vec<_>>().join("\n")
        );
        let summary = SimSummary::from_stderr(&stderr);
        assert_eq!(summary.expects_pass.len(), 20000);
        assert_eq!(verdict_word(&summary), "PASS");
    }

    #[test]
    fn malformed_sim_markers_fail_the_summary() {
        for marker in [
            r#"{"op":"stop""#,
            r#"{"op":"stop"} trailing"#,
            "{}",
            "[]",
            r#"{"op":false}"#,
        ] {
            let summary = SimSummary::from_stderr(&format!(
                "__SIM__ {{\"op\":\"expect_ok\"}}\nnoise __SIM__ {marker}\n__SIM__ {{\"op\":\"stop\"}}"
            ));
            assert_eq!(verdict_word(&summary), "FAIL", "accepted {marker}");
            assert!(
                summary
                    .errors
                    .iter()
                    .any(|error| error.message.contains("invalid sim marker"))
            );
        }
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
        let missing = SimSummary {
            saw_stop: true,
            ..Default::default()
        };
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
        assert!(
            allocation_census_record("incomplete", &incomplete)
                .1
                .is_none()
        );
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
        let stderr = r#"__SIM__ {"op":"expect_fail","message":"x","site":"/work/repo/game/tests/sims/bandage.rock:29"}"#;
        let s = SimSummary::from_stderr(stderr);
        assert_eq!(s.expects_fail[0].site, "bandage:29");
    }

    #[test]
    fn resolve_sim_sources_keeps_same_named_sims_in_their_own_files() {
        // Two modules declare the same trailing segment; the receipt's
        // per-wire entries disambiguate where a decl-segment scan errored.
        let root = tempfile::tempdir().unwrap();
        let proj = root.path().join("proj");
        let sims = proj.join("tests").join("sims");
        std::fs::create_dir_all(&sims).unwrap();
        std::fs::create_dir_all(root.path().join(".git")).unwrap();
        std::fs::write(
            root.path().join(".git").join("HEAD"),
            "ref: refs/heads/master\n",
        )
        .unwrap();
        std::fs::write(
            sims.join("a.rock"),
            "@sim\nco fn only() -> Coroutine<Wait> {\n}\n",
        )
        .unwrap();
        std::fs::write(
            sims.join("b.rock"),
            "mod b;\n@sim\nco fn only() -> Coroutine<Wait> {\n}\n",
        )
        .unwrap();
        let relative: HashMap<String, String> = [
            (
                "game::only".to_string(),
                "proj/tests/sims/a.rock".to_string(),
            ),
            (
                "game::b::only".to_string(),
                "proj/tests/sims/b.rock".to_string(),
            ),
        ]
        .into_iter()
        .collect();
        let resolved = resolve_sim_sources(&proj, &relative).unwrap();
        let canonical = |name: &str| sims.join(name).canonicalize().unwrap();
        assert_eq!(resolved["game::only"], canonical("a.rock"));
        assert_eq!(resolved["game::b::only"], canonical("b.rock"));
    }

    #[test]
    fn image_snapshot_source_for_reports_a_wire_missing_from_the_registry() {
        let part = WorkerPart {
            name: "game::missing".to_string(),
            out_dir: PathBuf::from("/tmp/out"),
            frame_budget: 60,
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            env: Vec::new(),
            occurrence_id: 0,
            source_file: None,
        };
        let error = snapshot_source_for(&part).unwrap_err();
        assert!(error.contains("game::missing"), "{error}");
    }

    #[test]
    fn prefix_with_site_keeps_origin_in_message() {
        // Post-run image_snapshot failures clear `site` so
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
            ReloadLedgerSummary {
                requests: 2,
                applied: 0,
                pending: 0,
                rejected: 1,
                failed: 1
            }
        );
    }

    #[test]
    fn reload_ledger_rejects_drop_duplicate_and_early_completion() {
        let mut dropped = ReloadLedger::default();
        dropped.book(1).unwrap();
        assert_eq!(
            dropped.summary().pending,
            1,
            "a dropped request cannot disappear from accounting"
        );

        let mut duplicate = ReloadLedger::default();
        duplicate.book(1).unwrap();
        assert!(duplicate.complete(1, ReloadTerminal::Applied).is_err());
        duplicate.publish(1).unwrap();
        assert!(duplicate.publish(1).is_err());
        duplicate.complete(1, ReloadTerminal::Applied).unwrap();
        assert!(duplicate.complete(1, ReloadTerminal::Applied).is_err());
    }
}
