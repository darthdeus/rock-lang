//! `rock sim <project> <name>` — runner for Rock-native `@sim`
//! coroutines. See `docs/parked/sim-rock.md` for design history.
//!
//! Orchestrates: compile project + spawn the game with
//! `ROCK_RUN_SIM=<name>` set, demux `__SIM__`-prefixed JSON lines from
//! stderr, pretty-print pass/fail, return non-zero on any expect_fail.
//!
//! Side-channel format: stderr lines with the literal prefix
//! `__SIM__ {json}\n`, where each JSON object has `op: string` and
//! op-specific fields. See `stone/engine/sim.rock`.

use anyhow::{anyhow, Context, Result};
use std::collections::{BTreeMap, HashMap};
use colored::Colorize;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;
use tempfile::TempDir;

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
    let Some((_, raw)) = env_pairs.iter().find(|(k, _)| k == "FRAME_COUNT") else {
        return Ok(DEFAULT_FRAME_BUDGET);
    };
    raw.parse::<usize>().ok().filter(|n| *n > 0).ok_or_else(|| {
        anyhow!("sim `{sim_name}` declares @sim_env(\"FRAME_COUNT\", \"{raw}\"), which is not a positive integer")
    })
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

/// Split a `--sim` value on `,` into chain parts. Empty parts dropped,
/// whitespace trimmed. Single source of truth for the multi-start
/// chain syntax — used by `do_run_sim_opts` and `rock stone`'s sim
/// resolver.
pub fn parse_sim_chain(name: &str) -> Vec<&str> {
    name.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).collect()
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
/// The engine cdylibs stay in `tmp/` itself: they are staged before any spawn
/// and children resolve them through the relative `tmp` entry that
/// `rock_host::project::lib_path_value` puts on the library search path.
fn sim_output_dir() -> Result<TempDir> {
    fs_err::create_dir_all("tmp").context("creating tmp/ for sim output")?;
    tempfile::Builder::new()
        .prefix("sim-")
        .tempdir_in("tmp")
        .context("creating sim output directory")
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
/// once, returning the registry-ordered wire names alongside each sim's
/// `@sim_env` pairs. Single source of suite discovery for the list,
/// single-run, and run-all paths so they can't drift in which sims they
/// see. Does not validate the project dir or build engine libs — callers
/// own that (listing needs neither).
fn compile_sims(
    backend: crate::Backend,
    project_path: PathBuf,
    output_path: PathBuf,
) -> Result<(Vec<String>, HashMap<String, Vec<(String, String)>>, crate::skull_backend::SkullStone)> {
    // Only skull serves sims now (bone has no codegen, rock is gone): build the
    // bone workspace once (a live Session, tests included), emit the game-shaped
    // main.lua with its `@sim` registry, and collect the wire/env list from the
    // same build. The returned SkullStone serves mid-sim `reload_edit` ops.
    match backend {
        crate::Backend::Skull => {
            let main_path = output_path.join(rock_host::project::MAIN_LUA_FILENAME);
            crate::skull_backend::skull_compile_game_and_collect_sims(
                &project_path.to_string_lossy(),
                &main_path,
            )
        }
        other => panic!(
            "compile_sims reached non-skull backend {:?}; only skull serves sims",
            other.name()
        ),
    }
}

/// Compile the project and print each `@sim` wire name on stdout, one
/// per line, sorted by source span. Lets `just anvil-sim` enumerate
/// the suite without hardcoding names.
pub fn do_list_sims(backend: crate::Backend, project: &str) -> Result<()> {
    rock_host::project::ensure_workspace_root()?;

    let project_path = rock_host::project::resolve_project_dir(project)?;

    let out_dir = sim_output_dir()?;
    let (wire_names, _, _) = compile_sims(backend, project_path, out_dir.path().to_path_buf())?;
    for name in wire_names {
        // Result output, NOT logging: `--list` exists to print the sim
        // names to stdout — this is the command's answer, consumed by
        // callers/CI, not a diagnostic.
        println!("{}", name);
    }
    Ok(())
}

pub fn do_run_sim_opts(backend: crate::Backend, project: &str, sim_name: &str, quiet: bool, update_snapshots: bool) -> Result<()> {
    rock_host::project::ensure_workspace_root()?;

    let project_path = rock_host::project::resolve_project_dir(project)?;
    // Thread the RESOLVED path everywhere the raw arg used to flow (engine libs,
    // sim-source lookup for snapshot baselines) so a bare name behaves exactly
    // like its expanded path.
    let project = project_path.to_string_lossy().into_owned();
    rock_host::engine_build::build_engine_libs(&project)?;

    let out_dir = sim_output_dir()?;
    let output = out_dir.path().to_string_lossy().into_owned();
    let output_path = out_dir.path().to_path_buf();

    // Compile once and collect `@sim_env("K", "V")` env pairs by wire name.
    // The runner applies them via `Command::env` before luajit spawns --
    // the in-process runtime `set_env()` runs after `@init`, so it can't
    // influence env-var-driven game config like `ANVIL_VIDEO`.
    let (_, env_by_wire, mut stone) = compile_sims(backend, project_path, output_path)?;
    sim_spawn_barrier();

    let (display_name, summary) = run_named_entry(
        &project,
        sim_name,
        quiet,
        false,
        update_snapshots,
        &output,
        &env_by_wire,
        Some(&mut stone),
    )?;
    let sim_name = display_name.as_str();

    if !quiet {
        summary.print(sim_name, true);
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
        // Caller decides exit code. The standalone `rock sim` CLI maps
        // this to a non-zero process exit in `dispatcher.rs`; the
        // `rock stone --sim` watcher path logs and keeps running so the
        // user can fix and recompile.
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

    let mut combined = SimSummary::default();
    let mut stone = stone;
    for part in &parts {
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
        )?;
        combined.merge(part_summary);
    }

    // Video assertions after all parts run (they need ffprobe/ffmpeg
    // subprocesses, and a multi-start chain may assert in either part).
    artifacts.finish(&mut combined);
    Ok((display_name, combined))
}

/// Whether an entry's summary is a clean pass (no fail, no timeout, no
/// process error, reached `stop_sim()`).
fn entry_passed(summary: &SimSummary) -> bool {
    !summary.timed_out
        && summary.process_failure_code.is_none()
        && summary.saw_stop
        && !summary.has_failures()
}

/// The word printed in a sim's verdict line. Derived from [`entry_passed`] so the
/// human-facing report and the process exit status can never disagree -- this
/// used to be computed from `has_failures()` alone, which printed `PASS` for a
/// sim that aborted mid-body (an abort fails no expect).
fn verdict_word(summary: &SimSummary) -> &'static str {
    if entry_passed(summary) {
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
    mode: SimExecMode,
) -> Result<()> {
    // One name: identical to the classic single-run path (full detail). There
    // is nothing to overlap, so `--jobs` does not apply -- nor `--workers`,
    // since a one-entry queue takes no checkpoint either way.
    if names.len() == 1 {
        return do_run_sim_opts(backend, project, &names[0], quiet, update_snapshots);
    }

    rock_host::project::ensure_workspace_root()?;
    let project_path = rock_host::project::resolve_project_dir(project)?;
    let project = project_path.to_string_lossy().into_owned();
    rock_host::engine_build::build_engine_libs(&project)?;

    let out_dir = sim_output_dir()?;
    let output = out_dir.path().to_string_lossy().into_owned();
    let (_, env_by_wire, mut stone) =
        compile_sims(backend, project_path, PathBuf::from(&output))?;
    sim_spawn_barrier();

    if mode == SimExecMode::Workers {
        let results = run_specs_worker_mode(
            &project,
            &output,
            &env_by_wire,
            names,
            quiet,
            update_snapshots,
            jobs,
            &mut stone,
        )?;
        let mut passed = 0usize;
        let mut failed: Vec<String> = Vec::new();
        for (name, summary) in &results {
            if entry_passed(summary) {
                passed += 1;
            } else {
                failed.push(name.clone());
            }
        }
        let suffix = if failed.is_empty() {
            String::new()
        } else {
            format!(" -- FAILED: {}", failed.join(", "))
        };
        println!("[sim] {passed}/{} sims passed{suffix}", names.len());
        if !failed.is_empty() {
            return Err(anyhow!(
                "{} of {} named sims failed in `{project}`",
                failed.len(),
                names.len()
            ));
        }
        return Ok(());
    }

    let jobs = resolve_jobs(jobs, update_snapshots);

    // Same two lanes as the suite path. The unit of parallelism is the ENTRY,
    // not the part: a comma-chain's parts share one `$OUT` and run in order,
    // so they stay together on one worker.
    let (reload_idx, ordinary_idx): (Vec<usize>, Vec<usize>) =
        (0..names.len()).partition(|&i| entry_is_reload_lane(&names[i], &env_by_wire));

    let report = Mutex::new(());
    let run_at = |i: usize,
                  stone: Option<&mut crate::skull_backend::SkullStone>|
     -> Result<(String, SimSummary)> {
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
            &output,
            &env_by_wire,
            stone,
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

    let mut results: HashMap<usize, Result<(String, SimSummary)>> = HashMap::new();
    if jobs > 1 && ordinary_idx.len() > 1 {
        let next = std::sync::atomic::AtomicUsize::new(0);
        let collected: Mutex<Vec<(usize, Result<(String, SimSummary)>)>> =
            Mutex::new(Vec::new());
        let workers = jobs.min(ordinary_idx.len());
        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| loop {
                    let slot = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let Some(&i) = ordinary_idx.get(slot) else { break };
                    let r = run_at(i, None);
                    collected.lock().unwrap().push((i, r));
                });
            }
        });
        results.extend(collected.into_inner().unwrap());
    } else {
        for &i in &ordinary_idx {
            results.insert(i, run_at(i, None));
        }
    }
    for &i in &reload_idx {
        results.insert(i, run_at(i, Some(&mut stone)));
    }

    // Accounting walks the order the user gave, so the failed list matches a
    // serial run's regardless of completion order.
    let mut passed = 0usize;
    let mut failed: Vec<String> = Vec::new();
    for i in 0..names.len() {
        let (display_name, summary) = match results.remove(&i) {
            Some(Ok(v)) => v,
            Some(Err(e)) => return Err(e),
            None => return Err(anyhow!("sim `{}` was never scheduled", names[i])),
        };
        if entry_passed(&summary) {
            passed += 1;
        } else {
            failed.push(display_name);
        }
    }

    // Result output, NOT logging: the multi-run verdict, consumed by you/CI.
    let suffix = if failed.is_empty() {
        String::new()
    } else {
        format!(" -- FAILED: {}", failed.join(", "))
    };
    println!("[sim] {passed}/{} sims passed{suffix}", names.len());

    if !failed.is_empty() {
        return Err(anyhow!(
            "{} of {} named sims failed in `{project}`",
            failed.len(),
            names.len()
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
fn run_bounded<T, F>(jobs: usize, n: usize, task: F) -> Vec<(usize, Result<T>)>
where
    F: Fn(usize) -> Result<T> + Sync,
    T: Send,
{
    if jobs <= 1 || n <= 1 {
        return (0..n).map(|i| (i, task(i))).collect();
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    let collected: Mutex<Vec<(usize, Result<T>)>> = Mutex::new(Vec::new());
    let lanes = jobs.min(n);
    std::thread::scope(|scope| {
        for _ in 0..lanes {
            scope.spawn(|| loop {
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

pub fn do_run_all_sims(
    backend: crate::Backend,
    project: &str,
    quiet: bool,
    update_snapshots: bool,
    jobs: Option<usize>,
    mode: SimExecMode,
) -> Result<()> {
    rock_host::project::ensure_workspace_root()?;

    let project_path = rock_host::project::resolve_project_dir(project)?;
    let project = project_path.to_string_lossy().into_owned();
    rock_host::engine_build::build_engine_libs(&project)?;

    let out_dir = sim_output_dir()?;
    let output = out_dir.path().to_string_lossy().into_owned();
    let (wire_names, env_by_wire, mut stone) =
        compile_sims(backend, project_path, PathBuf::from(&output))?;
    sim_spawn_barrier();
    if wire_names.is_empty() {
        return Err(anyhow!("no @sim coroutines found in project `{project}`"));
    }

    if mode == SimExecMode::Workers {
        let results = run_specs_worker_mode(
            &project,
            &output,
            &env_by_wire,
            &wire_names,
            quiet,
            update_snapshots,
            jobs,
            &mut stone,
        )?;
        let mut passed = 0usize;
        let mut failed: Vec<String> = Vec::new();
        for (name, summary) in &results {
            if entry_passed(summary) {
                passed += 1;
            } else {
                failed.push(name.clone());
            }
        }
        let suffix = if failed.is_empty() {
            String::new()
        } else {
            format!(" -- FAILED: {}", failed.join(", "))
        };
        println!(
            "[sim] {project} suite: {passed}/{} sims passed{suffix}",
            wire_names.len()
        );
        if !failed.is_empty() {
            return Err(anyhow!(
                "{} of {} sims failed in `{project}`",
                failed.len(),
                wire_names.len()
            ));
        }
        return Ok(());
    }

    let jobs = resolve_jobs(jobs, update_snapshots);

    // Two lanes. Reload sims mutate the one session and the one overlay, so
    // they run alone with nothing else in flight; everything else is
    // independent once the compiled output is invocation-owned.
    let (reload_idx, ordinary_idx): (Vec<usize>, Vec<usize>) = (0..wire_names.len())
        .partition(|&i| is_reload_lane(env_by_wire.get(&wire_names[i]).map_or(&[][..], |v| v)));

    // Concurrent children cannot stream: their progress lines would interleave
    // into nonsense. Buffer instead and print each sim's complete record in
    // registry order below, which is what the concise multi-run path already
    // does. Serial runs keep today's live stream.
    let streaming = jobs == 1 && !quiet;
    let mut summaries: HashMap<usize, Result<SimSummary>> = HashMap::new();

    // A record is finished, printed, and accounted in ONE place, so a sim's
    // lines can never interleave with another's: everything after the child
    // exits happens under `report` for concurrent runs. Video assertions shell
    // out to ffprobe/ffmpeg but ride the same pool, so they stay bounded by
    // `--jobs` rather than fanning out on their own.
    let report = Mutex::new(());
    let run_at = |i: usize, stone: Option<&mut crate::skull_backend::SkullStone>| -> Result<SimSummary> {
        // Fresh tempdir per sim -- distinct sims don't share OUT the way
        // a comma-chain's parts do.
        let artifacts = SimArtifacts::new()?;
        let env_pairs = env_by_wire.get(&wire_names[i]).cloned().unwrap_or_default();
        let mut summary = run_one_sim(
            &project,
            &wire_names[i],
            quiet,
            !streaming,
            update_snapshots,
            artifacts.path(),
            &output,
            &env_pairs,
            stone,
        )?;

        let _lock = report.lock().unwrap();
        artifacts.finish(&mut summary);
        if !quiet {
            // Result output: the child's own retained stderr/stdout, emitted
            // here so it lands with its record instead of interleaving.
            if !summary.diagnostics.is_empty() {
                eprint!("{}", summary.diagnostics);
            }
            // Concurrent runs never streamed the per-expect lines, so the
            // record must carry its own sited failures (`streamed = false`).
            summary.print(&wire_names[i], streaming);
        }
        Ok(summary)
    };

    for (slot, r) in run_bounded(jobs, ordinary_idx.len(), |slot| run_at(ordinary_idx[slot], None))
    {
        summaries.insert(ordinary_idx[slot], r);
    }

    // Reload lane, strictly after the pool has drained.
    for &i in &reload_idx {
        summaries.insert(i, run_at(i, Some(&mut stone)));
    }

    // Records printed in completion order; the ACCOUNTING is registry order, so
    // the failed list and tally are identical to a serial run's.
    let mut passed = 0usize;
    let mut failed: Vec<String> = Vec::new();
    for (i, name) in wire_names.iter().enumerate() {
        let summary = match summaries.remove(&i) {
            Some(Ok(s)) => s,
            Some(Err(e)) => return Err(e),
            None => return Err(anyhow!("sim `{name}` was never scheduled")),
        };
        if entry_passed(&summary) {
            passed += 1;
        } else {
            failed.push(name.clone());
        }
    }

    // Result output, NOT logging: the suite verdict is this command's
    // answer, consumed by the user / CI.
    let suffix = if failed.is_empty() {
        String::new()
    } else {
        format!(" -- FAILED: {}", failed.join(", "))
    };
    println!(
        "[sim] {project} suite: {passed}/{} sims passed{suffix}",
        wire_names.len()
    );

    if !failed.is_empty() {
        return Err(anyhow!(
            "{} of {} sims failed in `{project}`",
            failed.len(),
            wire_names.len()
        ));
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
///   pool; the hot-reload lane runs strictly AFTER the pool drains, because it
///   mutates the one session and the one overlay;
/// - reporting is deferred to spec order. Completion order is not
///   deterministic, and a record has to be emitted whole.
///
/// Returns one summary per spec, in spec order.
fn run_specs_worker_mode(
    project: &str,
    output: &str,
    env_by_wire: &HashMap<String, Vec<(String, String)>>,
    specs: &[String],
    quiet: bool,
    update_snapshots: bool,
    jobs: Option<usize>,
    stone: &mut crate::skull_backend::SkullStone,
) -> Result<Vec<(String, SimSummary)>> {
    if cfg!(windows) {
        return Err(anyhow!(
            "--workers is not available on Windows: the per-entry environment reaches \
             only the CRT view, so a native reader using the Win32 environment block \
             would see the previous entry's $OUT"
        ));
    }
    if update_snapshots {
        return Err(anyhow!(
            "--workers cannot rewrite snapshot baselines: the runner does not yet prove \
             every destination path is unique. Run --update-snapshots isolated."
        ));
    }

    let jobs = resolve_jobs(jobs, update_snapshots);
    let (groups, isolated) = group_worker_specs(specs, env_by_wire);
    // One slot per spec POSITION: the same sim can be named twice, and each
    // occurrence is its own entry with its own result.
    let mut slots: Vec<Option<SimSummary>> = specs.iter().map(|_| None).collect();

    // The lane plan. A group splits into contiguous blocks so the entries a
    // worker serves stay in their requested order; splitting round-robin would
    // reshuffle every predecessor for no gain.
    let mut lanes: Vec<Vec<usize>> = Vec::new();
    for group in &groups {
        let blocks = jobs.min(group.len()).max(1);
        for b in 0..blocks {
            let lo = group.len() * b / blocks;
            let hi = group.len() * (b + 1) / blocks;
            if lo < hi {
                lanes.push(group[lo..hi].to_vec());
            }
        }
    }
    // An isolated entry needs the compile SESSION only when it opted into the
    // hot-reload lane; everything else isolated is an ordinary fresh process
    // and can overlap. `worker_ineligibility` has other reasons (a `$OUT`-
    // interpolating env, disagreeing chain parts, `SIM_WORKER_ISOLATED`) and
    // none of them touch the session.
    let (exclusive, poolable): (Vec<_>, Vec<_>) = isolated.into_iter().partition(|(i, _)| {
        parse_sim_chain(&specs[*i])
            .iter()
            .any(|p| is_reload_lane(env_by_wire.get(*p).map_or(&[][..], |v| v)))
    });

    if !quiet {
        for lane in &lanes {
            if lane.len() > 1 {
                println!("[sim] worker: {} entries share one booted process", lane.len());
            }
        }
        for (i, reason) in poolable.iter().chain(exclusive.iter()) {
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

    let mut collected: Vec<(usize, SimSummary)> = Vec::new();
    for (_, r) in run_bounded(jobs, work.len(), |w| match work[w] {
        Lane::Worker(lane) => {
            let lane_specs: Vec<String> = lane.iter().map(|&i| specs[i].clone()).collect();
            // `run_specs_recovering` is positional, so its results line up with
            // `lane_specs` and therefore with `lane`'s indices.
            let results = run_specs_recovering(
                project,
                output,
                env_by_wire,
                &lane_specs,
                update_snapshots,
                false,
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
            let (_, summary) =
                run_named_entry(project, &specs[i], quiet, true, update_snapshots, output, env_by_wire, None)?;
            Ok(vec![(i, summary)])
        }
    }) {
        collected.extend(r?);
    }
    for (i, summary) in collected {
        slots[i] = Some(summary);
    }

    // The hot-reload lane, strictly after the pool has drained: it mutates the
    // one `SkullStone` session and the one `incremental.lua` overlay.
    for &(i, _) in &exclusive {
        let (_, summary) = run_named_entry(
            project,
            &specs[i],
            quiet,
            true,
            update_snapshots,
            output,
            env_by_wire,
            Some(&mut *stone),
        )?;
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
struct ReloadEditReq {
    file: String,
    find: String,
    replace: String,
    site: String,
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
) -> Result<()> {
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
        crate::skull_backend::SkullReload::Incremental { bytes, reinferred, .. } => {
            log::info!(target: "SIM",
                "reload_edit `{}`: overlay {bytes}B, {reinferred} bodies", req.file);
            Ok(())
        }
        other => Err(anyhow!(
            "must stay a body-only edit; `{}` routed to {other:?} \
             (interface/structural edits are not injectable mid-sim)",
            req.file
        )),
    }
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
/// sequence and the session must not be live while another child exists.
fn entry_is_reload_lane(
    chain_spec: &str,
    env_by_wire: &HashMap<String, Vec<(String, String)>>,
) -> bool {
    parse_sim_chain(chain_spec)
        .iter()
        .any(|part| env_by_wire.get(*part).is_some_and(|e| is_reload_lane(e)))
}

/// Conservative default for concurrent sim children.
///
/// Deliberately well under the point where a machine starts failing: sim
/// children each create a GL context, and oversubscribing the driver produces
/// GL errors rather than slowdown. The suite only has to beat serial, so this
/// is not tuned for the best observed number and should only be raised from a
/// measurement on more than one machine (`docs/sim-speed-plan.md`).
const DEFAULT_SIM_JOBS: usize = 4;

/// Resolve the requested job count. `--update-snapshots` pins to serial: it
/// rewrites baseline files, and the runner does not yet prove every
/// destination path is unique before spawning (several `@sim`s can share one
/// source file, so its `.snap` directory is shared too).
fn resolve_jobs(requested: Option<usize>, update_snapshots: bool) -> usize {
    if update_snapshots {
        if requested.is_some_and(|j| j > 1) {
            log::warn!(target: "SIM",
                "--update-snapshots rewrites snapshot baselines; running serially");
        }
        return 1;
    }
    let available = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    requested.unwrap_or(DEFAULT_SIM_JOBS).clamp(1, available.max(1))
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

fn writes_live(buffered: bool) -> bool {
    !buffered
}

/// `@sim_env` keys the runner owns per entry, so they are NOT part of the boot
/// signature two entries must agree on to share a process.
///
/// `FRAME_COUNT` is the entry's runaway bound, carried in the entry plan and
/// republished by `sim_start_entry` at every boundary. `SIM_DT` and
/// `SIM_SKIP_RENDER` are read ONCE during boot (`runner_load_sim_entries`), so
/// they are still boot signature until the worker applies them per entry.
///
/// `ROCK_MASK_WALL_CLOCK` is owned outright: the runner publishes its own value
/// after `preset_env`, so a declared one never reaches the child and cannot
/// stand for a difference between two entries. Splitting a group on an inert
/// value would isolate sims for nothing.
const RUNNER_OWNED_ENV_KEYS: &[&str] = &["FRAME_COUNT", "ROCK_MASK_WALL_CLOCK"];

/// The part of a sim's `@sim_env` that is consumed during global init, engine
/// init and `@init` -- what a shared process cannot vary between entries.
///
/// Canonical (stable-sorted by key) so declaration order does not split a
/// group: two sims agreeing on the same pairs share a signature however they
/// spelled them. The sort is stable, so a duplicated key keeps its last-wins
/// application order and a genuine effective difference still refuses.
fn boot_signature(env_pairs: &[(String, String)]) -> Vec<(String, String)> {
    let mut sig: Vec<(String, String)> = env_pairs
        .iter()
        .filter(|(k, _)| !RUNNER_OWNED_ENV_KEYS.contains(&k.as_str()))
        .cloned()
        .collect();
    sig.sort_by(|a, b| a.0.cmp(&b.0));
    sig
}

/// How `rock sim` maps entries to processes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SimExecMode {
    /// One fresh process per entry: the permanent reference oracle, and (until
    /// B3/B4 land) the default.
    Isolated,
    /// Boot-checkpoint workers: eligible entries grouped by boot signature
    /// share one booted process with the restore boundary between them.
    Workers,
}

/// Resolve the two CLI flags. clap already rejects the combination.
pub fn exec_mode(isolated: bool, workers: bool) -> SimExecMode {
    debug_assert!(!(isolated && workers));
    if workers {
        SimExecMode::Workers
    } else {
        SimExecMode::Isolated
    }
}

/// Why an entry cannot join a shared worker process. The reason is REPORTED --
/// a silently isolated entry would read as a worker result.
fn worker_ineligibility(spec: &str, env_by_wire: &HashMap<String, Vec<(String, String)>>) -> Option<String> {
    let parts = parse_sim_chain(spec);
    // A spec with no parts has nothing to agree on; the isolated path rejects
    // it with the runner's one "empty sim name" error.
    if parts.is_empty() {
        return Some("empty sim name".to_string());
    }
    for part in &parts {
        let env = env_by_wire.get(*part).map_or(&[][..], |v| v);
        if is_reload_lane(env) {
            return Some("hot-reload lane: mutates the one session and overlay".to_string());
        }
        // The explicit opt-out, for sims that operate process-global machinery
        // a shared worker cannot fence -- the checkpoint-machinery fixtures
        // declare it because they replace the very image the boundary restores.
        if env.iter().any(|(k, v)| k == "SIM_WORKER_ISOLATED" && v != "0") {
            return Some("declares @sim_env(SIM_WORKER_ISOLATED)".to_string());
        }
        if let Some((k, v)) = env.iter().find(|(_, v)| v.contains("$OUT")) {
            return Some(format!(
                "@sim_env(\"{k}\", \"{v}\") interpolates $OUT, which resolves before the process starts"
            ));
        }
    }
    // A chain's parts run as consecutive entries of one queue, so they must
    // agree with each other exactly as distinct entries must.
    let first_sig = boot_signature(env_by_wire.get(parts[0]).map_or(&[][..], |v| v));
    for part in &parts[1..] {
        if boot_signature(env_by_wire.get(*part).map_or(&[][..], |v| v)) != first_sig {
            return Some(format!(
                "chain parts `{}` and `{part}` disagree on their @sim_env boot signature",
                parts[0]
            ));
        }
    }
    None
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
    let mut groups: Vec<(Vec<(String, String)>, Vec<usize>)> = Vec::new();
    let mut isolated: Vec<(usize, String)> = Vec::new();
    for (i, spec) in specs.iter().enumerate() {
        if let Some(reason) = worker_ineligibility(spec, env_by_wire) {
            isolated.push((i, reason));
            continue;
        }
        let head = parse_sim_chain(spec)
            .first()
            .copied()
            .expect("an empty chain is classified ineligible above");
        let sig = boot_signature(env_by_wire.get(head).map_or(&[][..], |v| v));
        match groups.iter_mut().find(|(s, _)| *s == sig) {
            Some((_, members)) => members.push(i),
            None => groups.push((sig, vec![i])),
        }
    }
    (groups.into_iter().map(|(_, members)| members).collect(), isolated)
}

/// One queued sim body. A worker child runs these in order in ONE process;
/// the parts of a comma-chain share an `out_dir`, distinct entries do not.
#[derive(Clone)]
struct WorkerPart {
    name: String,
    out_dir: PathBuf,
    frame_budget: usize,
    env: Vec<(String, String)>,
}

/// One part's `__SIM__` and raw stderr lines, carved out of the worker's single
/// stream by the child's `entry_start`/`entry_end` envelope.
#[derive(Default)]
struct EntryStream {
    started: bool,
    /// Saw a matching `entry_end`. False for the entry a crashed or killed
    /// worker died inside.
    terminated: bool,
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
            EnvelopeStep::Ended(seq) => out[seq].terminated = true,
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
enum EnvelopeStep {
    /// A valid `entry_start` for the expected next entry -- the only step the
    /// deadline anchor may reset on.
    Started(usize),
    Ended(usize),
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
    fn observe_resets(&mut self, line: &str) -> bool {
        match self.envelope.as_mut().map(|t| t.observe(line)) {
            Some(Ok(EnvelopeStep::Started(_))) => true,
            Some(Err(_)) => {
                self.envelope = None;
                false
            }
            _ => false,
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
                self.open = None;
                Ok(EnvelopeStep::Ended(live))
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

fn envelope_name_matches(rest: &str, expected: &str, line: &str) -> Result<()> {
    match extract_str_field(rest, "name") {
        Some(name) if name == expected => Ok(()),
        Some(name) => Err(anyhow!(
            "worker entry envelope names `{name}` where the queue has `{expected}`: {line}"
        )),
        None => Err(anyhow!("worker sent an entry envelope with no name: {line}")),
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
    stone: Option<&mut crate::skull_backend::SkullStone>,
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
        if let Some((k, v)) = first.env.iter().find(|(_, v)| v.contains("$OUT")) {
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
    let mut plan_file = tempfile::Builder::new()
        .prefix("entry-plan-")
        .suffix(".tsv")
        .tempfile_in(output)
        .context("creating the worker entry plan")?;
    for p in parts {
        writeln!(
            plan_file,
            "{}\t{}\t{}",
            p.name,
            p.out_dir.to_string_lossy(),
            p.frame_budget
        )?;
    }
    plan_file.flush()?;

    // Expand `$OUT` in env values to the entry's tempdir absolute path.
    // Lets sims write `@sim_env("ANVIL_SESSION", "$OUT/resume.json")`
    // and have it resolve before luajit spawns. Runtime `set_env()`
    // runs after `@init` so it can't influence env-var-driven config.
    let out_str = first.out_dir.to_string_lossy().to_string();
    let preset_env: Vec<(String, String)> = first
        .env
        .iter()
        .map(|(k, v)| (k.clone(), v.replace("$OUT", &out_str)))
        .collect();

    let lua_file = std::path::PathBuf::from(output).join(rock_host::project::MAIN_LUA_FILENAME);
    let mut cmd = rock_host::project::build_luajit_command(&lua_file, &[]);
    // `ROCK_RUN_SIM` / `OUT` / `FRAME_COUNT` carry the FIRST entry: they are read
    // during boot (`sim_mode_active()` gates audio, the screenshot helper reads
    // the budget) before the queue starts, and the child re-publishes them
    // through setenv at each entry boundary.
    cmd.env("ROCK_SIM_ENTRY_PLAN", plan_file.path())
        .env("ROCK_RUN_SIM", &first.name)
        .env("FRAME_COUNT", first.frame_budget.to_string())
        .env("STONE_HEADLESS", "1")
        .env("OUT", &first.out_dir)
        // Default deterministic seed for sims. An explicit
        // `@sim_env("ROCK_TEST_SEED", ...)` in a sim still overrides
        // because `preset_env` is layered after this baseline.
        .env("ROCK_TEST_SEED", "42")
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
        .env("FPS_LOG", "0")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

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
    let mut child = cmd.spawn()?;
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

    let stderr_thread = {
        let lines = Arc::clone(&stderr_lines);
        let anchor = Arc::clone(&deadline_anchor);
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
                if deadline_guard.observe_resets(&line) {
                    *anchor.lock().unwrap() = std::time::Instant::now();
                }
                if let Some(rest) = line.strip_prefix("__SIM__ ") {
                    let op = extract_str_field(rest, "op");
                    if op.as_deref() == Some("reload_edit") {
                        // Send failure = the wait loop already reaped the child;
                        // the edit can't apply, which the sim's own wait_for
                        // timeout (or the post-exit drain) surfaces.
                        let _ = reload_tx.send(ReloadEditReq {
                            file: extract_str_field(rest, "file").unwrap_or_default(),
                            find: extract_str_field(rest, "find").unwrap_or_default(),
                            replace: extract_str_field(rest, "replace").unwrap_or_default(),
                            site: extract_str_field(rest, "site").unwrap_or_default(),
                        });
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
    let mut pristine: HashMap<String, String> = HashMap::new();
    let mut reload_errors: Vec<SiteMessage> = Vec::new();
    let mut stone = stone;
    let (status, timed_out) = loop {
        match child.try_wait()? {
            Some(s) => break (s, false),
            None => {
                let since_anchor = deadline_anchor.lock().unwrap().elapsed();
                if since_anchor > timeout {
                    let _ = child.kill();
                    let s = child.wait()?;
                    break (s, true);
                }
                // Service mid-sim reload_edit requests while waiting; the 50ms
                // recv timeout doubles as the child poll interval. Disconnected
                // = the stderr stream closed before the child was reaped --
                // keep the poll cadence with a plain sleep.
                match reload_rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(req) => {
                        // An ordinary (non-reload) sim has no session to edit.
                        // Servicing it here would mutate build state shared with
                        // every other child, so refuse loudly rather than
                        // silently dropping the request or reaching for the
                        // session from a parallel lane.
                        let serviced = match stone.as_deref_mut() {
                            Some(st) => service_reload_edit(st, output, &mut pristine, &req),
                            None => Err(anyhow!(
                                "reload_edit is only available to sims that opt into the \
                                 reload lane with `@sim_env(\"STONE_HOT_RELOAD\", \"1\")`"
                            )),
                        };
                        if let Err(e) = serviced {
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
    // A request still queued here arrived after the child exited -- the sim
    // emitted reload_edit and stopped without waiting for the effect. That's
    // a sim-author bug; dropping it silently would hide the mis-ordering.
    while let Ok(req) = reload_rx.try_recv() {
        reload_errors.push(SiteMessage {
            message: format!(
                "reload_edit for `{}` arrived after the child exited (emit it before a wait_for on its effect)",
                req.file
            ),
            site: compress_site(&req.site),
        });
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
            summary.timed_out = timed_out;
            if !status.success() {
                summary.process_failure_code = Some(status.code().unwrap_or(-1));
            }
        }

        let snap_dir = find_sim_source(project, &part.name)
            .as_deref()
            .map(snap_dir_for);
        if let Some(dir) = snap_dir.as_ref() {
            process_snapshots(&mut summary, dir, update_snapshots);
        } else if !summary.snapshots.is_empty() || !summary.image_snapshots.is_empty() {
            summary.errors.push(SiteMessage {
                message: format!(
                    "could not locate .rock source for sim `{}` -- snapshot ops cannot resolve baseline dir",
                    part.name
                ),
                site: String::new(),
            });
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
        .map(|st| revert_reload_edits(st, output, &mut pristine))
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
) -> Result<SimSummary> {
    let part = WorkerPart {
        name: sim_part.to_string(),
        out_dir: out_path.to_path_buf(),
        frame_budget: frame_budget_for(sim_part, env_pairs)?,
        env: env_pairs.to_vec(),
    };
    let mut summaries = run_worker(
        project,
        std::slice::from_ref(&part),
        quiet,
        buffered,
        update_snapshots,
        output,
        // Both callers build their `$OUT` from a non-recording `SimArtifacts`,
        // so no record from here is ever compared.
        false,
        stone,
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
            });
        }
        artifacts.push(entry_artifacts);
    }

    // Not quiet, but buffered: nothing is written live (several entries share
    // one stream), and every diagnostic is retained into its record instead.
    let summaries = match grouping {
        Grouping::Shared => run_worker(project, &parts, false, true, false, output, true, None)?,
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
fn run_specs_recovering(
    project: &str,
    output: &str,
    env_by_wire: &HashMap<String, Vec<(String, String)>>,
    entry_specs: &[String],
    update_snapshots: bool,
    record_manifest: bool,
) -> Result<Vec<(String, SimSummary, bool)>> {
    let mut plans = Vec::with_capacity(entry_specs.len());
    for spec in entry_specs {
        let artifacts = SimArtifacts::with_manifest(record_manifest)?;
        let chain = parse_sim_chain(spec);
        if chain.is_empty() {
            return Err(anyhow!("sim got an empty sim name"));
        }
        let parts = chain
            .iter()
            .map(|name| {
                let env = env_by_wire.get(*name).cloned().unwrap_or_default();
                Ok(WorkerPart {
                    name: name.to_string(),
                    out_dir: artifacts.path().to_path_buf(),
                    frame_budget: frame_budget_for(name, &env)?,
                    env,
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
    while base < plans.len() {
        let parts: Vec<WorkerPart> =
            plans[base..].iter().flat_map(|p| p.parts.iter().cloned()).collect();
        let summaries =
            run_worker(project, &parts, false, true, update_snapshots, output, record_manifest, None)?;

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
            if let Some(reason) = summary.worker_ineligible.take() {
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
            artifacts.finish(&mut summary);
            done.push((plan.spec.clone(), summary, provenance));
        }
        base += finals;
    }
    Ok(done)
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
        let results = run_specs_recovering(project, output, env_by_wire, &specs, false, true)?;
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
pub fn run_multi_order_gate(backend: crate::Backend, project: &str) -> Result<()> {
    use crate::sim_record::{compare, COMPARED_ARMS};

    rock_host::project::ensure_workspace_root()?;
    let project_path = rock_host::project::resolve_project_dir(project)?;
    let project = project_path.to_string_lossy().into_owned();
    rock_host::engine_build::build_engine_libs(&project)?;

    let out_dir = sim_output_dir()?;
    let output = out_dir.path().to_string_lossy().into_owned();
    let (wire_names, env_by_wire, _stone) =
        compile_sims(backend, project_path, PathBuf::from(&output))?;
    // Hot-reload sims are out of the corpus, and not because they are awkward:
    // they mutate the ONE `incremental.lua` overlay, which the suite runner
    // serializes behind an exclusive lane holding the compile session. This gate
    // runs the corpus four times over, so each pass would hand them a different
    // starting overlay and the divergence measured would be the harness's, not
    // the worker's. They also never reach a worker (`worker_ineligibility`
    // classifies the reload lane), so excluding them removes nothing this gate
    // could have decided.
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
    if !empty.is_empty() {
        // Not a fudge factor to relax: `expects_fail` and `errors` populate only
        // from sims that FAIL, so a corpus where everything passes standalone
        // cannot certify them and this gate must say so rather than report a
        // green it did not measure. mag carries known-red sims and populates
        // both; testbed does not, which is why testbed validates the mechanism
        // and mag is the gate.
        return Err(anyhow!(
            "these arms compared an empty population, so their agreement is evidence of \
             nothing: {}",
            empty.join(", ")
        ));
    }
    println!(
        "[multi-order] {} comparisons equivalent to the isolated reference",
        n * arms.len()
    );
    Ok(())
}

pub fn run_worker_suite(
    backend: crate::Backend,
    project: &str,
    entry_specs: &[String],
) -> Result<Vec<(crate::sim_record::SimRecord, bool)>> {
    rock_host::project::ensure_workspace_root()?;
    let project_path = rock_host::project::resolve_project_dir(project)?;
    let project = project_path.to_string_lossy().into_owned();
    rock_host::engine_build::build_engine_libs(&project)?;

    let out_dir = sim_output_dir()?;
    let output = out_dir.path().to_string_lossy().into_owned();
    let (_, env_by_wire, _stone) = compile_sims(backend, project_path, PathBuf::from(&output))?;

    let results =
        run_specs_recovering(&project, &output, &env_by_wire, entry_specs, false, true)?;
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
/// the process. Compare them with [`crate::sim_record::compare`].
pub fn run_differential(
    backend: crate::Backend,
    project: &str,
    plan: &DifferentialPlan,
) -> Result<(crate::sim_record::SimRecord, crate::sim_record::SimRecord)> {
    rock_host::project::ensure_workspace_root()?;
    let project_path = rock_host::project::resolve_project_dir(project)?;
    let project = project_path.to_string_lossy().into_owned();
    rock_host::engine_build::build_engine_libs(&project)?;

    let out_dir = sim_output_dir()?;
    let output = out_dir.path().to_string_lossy().into_owned();
    let (_, env_by_wire, _stone) = compile_sims(backend, project_path, PathBuf::from(&output))?;

    let isolated = run_entries(
        &project,
        &output,
        &env_by_wire,
        std::slice::from_ref(&plan.target),
        Grouping::PerPart,
    )?
    .remove(0);

    let mut worker_specs = plan.predecessors.clone();
    worker_specs.push(plan.target.clone());
    let mut worker = run_entries(
        &project,
        &output,
        &env_by_wire,
        &worker_specs,
        Grouping::Shared,
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
/// scanning `<project>/tests/sims/*.rock`.
fn find_sim_source(project: &str, sim_name: &str) -> Option<PathBuf> {
    let sims_dir = PathBuf::from(project).join("tests").join("sims");
    let entries = std::fs::read_dir(&sims_dir).ok()?;
    for ent in entries.flatten() {
        let path = ent.path();
        if path.extension().and_then(|e| e.to_str()) != Some("rock") {
            continue;
        }
        let Ok(src) = std::fs::read_to_string(&path) else {
            continue;
        };
        if src_declares_sim(&src, sim_name) {
            return Some(path);
        }
    }
    None
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
        let stop_note = if !self.saw_stop {
            " (no stop_sim() observed -- sim aborted early)"
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

    /// The job count is a safety bound, not a preference: it must never exceed
    /// what the machine can run, and `--update-snapshots` must pin to serial
    /// because it writes baseline files whose destinations are not proven
    /// unique before spawning.
    #[test]
    fn resolve_jobs_is_bounded_and_serial_for_snapshot_updates() {
        let cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);

        assert_eq!(resolve_jobs(None, false), DEFAULT_SIM_JOBS.min(cpus));
        assert_eq!(resolve_jobs(Some(1), false), 1);
        // Never above what the machine has, however large the request.
        assert_eq!(resolve_jobs(Some(usize::MAX), false), cpus);
        // 0 is meaningless; clamp up rather than deadlock on an empty pool.
        assert_eq!(resolve_jobs(Some(0), false), 1);
        // --update-snapshots wins over ANY explicit request.
        assert_eq!(resolve_jobs(None, true), 1);
        assert_eq!(resolve_jobs(Some(8), true), 1);
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
    fn the_boot_signature_excludes_only_runner_owned_keys() {
        let a = env(&[("FRAME_COUNT", "90"), ("STONE_FX_OFF", "1")]);
        let b = env(&[("FRAME_COUNT", "40"), ("STONE_FX_OFF", "1")]);
        assert_eq!(boot_signature(&a), boot_signature(&b), "the budget is per entry");

        let c = env(&[("FRAME_COUNT", "90"), ("STONE_FX_OFF", "0")]);
        assert_ne!(boot_signature(&a), boot_signature(&c), "a boot-consumed value is not");
        // SIM_DT and SIM_SKIP_RENDER are read once during boot, so they stay in.
        let d = env(&[("SIM_DT", "0.1")]);
        assert_eq!(boot_signature(&d), d);
    }

    #[test]
    fn the_boot_signature_is_declaration_order_independent() {
        let a = env(&[("STONE_FX_OFF", "1"), ("SIM_DT", "0.1")]);
        let b = env(&[("SIM_DT", "0.1"), ("STONE_FX_OFF", "1")]);
        assert_eq!(boot_signature(&a), boot_signature(&b), "same pairs, either spelling");

        // The sort is stable, so a duplicated key keeps last-wins order and a
        // genuine effective difference still refuses.
        let dup_a = env(&[("K", "1"), ("K", "2")]);
        let dup_b = env(&[("K", "2"), ("K", "1")]);
        assert_ne!(boot_signature(&dup_a), boot_signature(&dup_b));
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
            ("fx_off", &[("STONE_FX_OFF", "1")]),
            ("reload", &[("STONE_HOT_RELOAD", "1")]),
            ("outref", &[("SESSION", "$OUT/resume.json")]),
            ("machinery", &[("SIM_WORKER_ISOLATED", "1")]),
        ]);
        let (groups, isolated) = group_worker_specs(
            &specs(&["plain_a", "plain_b", "fx_off", "reload", "outref", "machinery"]),
            &wires,
        );

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
            ("head", &[("STONE_FX_OFF", "1")]),
            ("tail_same", &[("STONE_FX_OFF", "1"), ("FRAME_COUNT", "30")]),
            ("tail_other", &[("STONE_FX_OFF", "0")]),
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

        assert!(matches!(t.observe(&end(0, "a")), Ok(EnvelopeStep::Ended(0))));
        assert!(t.observe(&start(0, "a")).is_err(), "a closed entry cannot restart");
        assert!(started(&mut t, &start(1, "b")), "the valid NEXT start is Started");
        assert!(matches!(t.observe(&end(1, "b")), Ok(EnvelopeStep::Ended(1))));
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

    fn start(seq: usize, name: &str) -> String {
        format!(r#"__SIM__ {{"op":"entry_start","name":"{name}","seq":{seq}}}"#)
    }

    fn end(seq: usize, name: &str) -> String {
        format!(r#"__SIM__ {{"op":"entry_end","name":"{name}","seq":{seq}}}"#)
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
            end(0, "a"),
            "[   0.3s INFO ] between entries".to_string(),
            start(1, "b"),
            expect_ok("from b"),
            end(1, "b"),
            "[   0.4s INFO ] teardown".to_string(),
        ];

        let out = split_worker_stream(&stream, &parts).expect("well-formed stream");
        assert_eq!(out[0].sim, vec![expect_ok("from a")]);
        assert_eq!(out[1].sim, vec![expect_ok("from b")]);
        assert!(out[0].started && out[0].terminated);
        assert!(out[1].started && out[1].terminated);

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
    fn entry_passed_requires_stop_and_no_failures() {
        // Shared pass-predicate for both multi-run paths. Default (no
        // stop_sim observed) is a fail even with zero expects.
        let mut s = SimSummary::default();
        assert!(!entry_passed(&s));
        s.saw_stop = true;
        assert!(entry_passed(&s));
        s.expects_fail.push(SiteMessage { message: "x".into(), site: "f:1".into() });
        assert!(!entry_passed(&s));
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
        assert_eq!(verdict_word(&aborted), "PASS");

        let mut timed_out = SimSummary { saw_stop: true, ..SimSummary::default() };
        timed_out.timed_out = true;
        assert_eq!(verdict_word(&timed_out), "FAIL");

        let mut died = SimSummary { saw_stop: true, ..SimSummary::default() };
        died.process_failure_code = Some(1);
        assert_eq!(verdict_word(&died), "FAIL");
    }

    #[test]
    fn summary_counts_expects() {
        let stderr = r#"
__SIM__ {"op":"start_game","name":"anvil","site":"foo.rock:1"}
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
}
