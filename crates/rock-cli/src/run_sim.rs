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
use std::collections::HashMap;
use colored::Colorize;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;
use tempfile::TempDir;

/// Frame budget. Sims terminate naturally via the runner's `sim_done`
/// check; this is the upper bound for runaway / hung body.
const DEFAULT_FRAME_BUDGET: usize = 600;
/// Wall-clock timeout. An INDEPENDENT bound, not a backstop for the frame
/// budget: it is the only thing that catches a child which never advances a
/// frame at all -- an infinite loop inside one tick, a blocked FFI call, a
/// driver stall -- where `FRAME_COUNT` can never be reached and so can never
/// fire. Neither bound subsumes the other.
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
            "sim `{sim_name}` timed out after {}s (frame budget {DEFAULT_FRAME_BUDGET}); \
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
        summary.artifacts = crate::sim_record::manifest_of_dir(self.out_dir.path());
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
) -> Result<()> {
    // One name: identical to the classic single-run path (full detail). There
    // is nothing to overlap, so `--jobs` does not apply.
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
pub fn do_run_all_sims(
    backend: crate::Backend,
    project: &str,
    quiet: bool,
    update_snapshots: bool,
    jobs: Option<usize>,
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

    if jobs > 1 && ordinary_idx.len() > 1 {
        let next = std::sync::atomic::AtomicUsize::new(0);
        let collected: Mutex<Vec<(usize, Result<SimSummary>)>> = Mutex::new(Vec::new());
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
        summaries.extend(collected.into_inner().unwrap());
    } else {
        for &i in &ordinary_idx {
            summaries.insert(i, run_at(i, None));
        }
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
    sim: Vec<String>,
    raw: Vec<String>,
}

/// Split a worker's stderr by the per-entry envelope.
///
/// Lines outside any envelope (process boot, teardown) are attributed to the
/// most recently started entry, which is where a single-entry run has always
/// put them. An `entry_start` whose sequence is missing or out of range is a
/// protocol violation and rejected: guessing an entry would file one sim's
/// failures under another's name.
fn split_worker_stream(lines: &[String], n: usize) -> Result<Vec<EntryStream>> {
    let mut out: Vec<EntryStream> = (0..n).map(|_| EntryStream::default()).collect();
    let mut current: Option<usize> = None;
    let mut last = 0usize;
    for line in lines {
        let Some(rest) = line.strip_prefix("__SIM__ ") else {
            out[current.unwrap_or(last)].raw.push(line.clone());
            continue;
        };
        match extract_str_field(rest, "op").as_deref() {
            Some("entry_start") => {
                let seq = extract_raw_field(rest, "seq")
                    .and_then(|s| s.parse::<usize>().ok())
                    .filter(|s| *s < n)
                    .ok_or_else(|| {
                        anyhow!("worker sent an entry_start with no usable seq (n={n}): {line}")
                    })?;
                current = Some(seq);
                last = seq;
                out[seq].started = true;
            }
            Some("entry_end") => current = None,
            _ => out[current.unwrap_or(last)].sim.push(line.clone()),
        }
    }
    Ok(out)
}

/// Spawn ONE luajit process that runs `parts` in order and demux its side
/// channel into one summary per part.
///
/// A single-part call is the ordinary fresh-process sim: the queue mechanism is
/// the same one, so there is no second lifecycle to keep in step. More than one
/// part means the entries share a booted process with NO state reset between
/// them yet (`docs/sim-speed-plan.md` B1/B3) -- that is the condition the
/// differential oracle exists to measure, not a supported way to run a suite.
fn run_worker(
    project: &str,
    parts: &[WorkerPart],
    quiet: bool,
    buffered: bool,
    update_snapshots: bool,
    output: &str,
    stone: Option<&mut crate::skull_backend::SkullStone>,
) -> Result<Vec<SimSummary>> {
    let Some(first) = parts.first() else {
        return Err(anyhow!("worker got an empty entry queue"));
    };

    // Only runner-owned values (`$OUT`, sim name, frame budget) are per entry.
    // Everything else in `@sim_env` is consumed during global init, engine init
    // and `@init`, so a shared process can only serve entries that agree on it.
    if parts.len() > 1 {
        for p in &parts[1..] {
            if p.env != first.env {
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
    // Mid-sim reload_edit ops flow stderr-thread -> wait-loop, which owns the
    // (single-threaded) SkullStone and services them between try_wait polls.
    let (reload_tx, reload_rx) = mpsc::channel::<ReloadEditReq>();

    let stderr_thread = {
        let lines = Arc::clone(&stderr_lines);
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
                if let Some(rest) = line.strip_prefix("__SIM__ ") {
                    if extract_str_field(rest, "op").as_deref() == Some("reload_edit") {
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
    let start_time = std::time::Instant::now();
    let mut pristine: HashMap<String, String> = HashMap::new();
    let mut reload_errors: Vec<SiteMessage> = Vec::new();
    let mut stone = stone;
    let (status, timed_out) = loop {
        match child.try_wait()? {
            Some(s) => break (s, false),
            None => {
                if start_time.elapsed() > timeout {
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
    if writes_live(buffered) {
        for message in &reader_diagnostics {
            log::warn!(target: "SIM", "{message}");
        }
    }

    let collected = stderr_lines.lock().unwrap().clone();
    let stdout_buf_locked = stdout_buf.lock().unwrap();
    let stdout_str = String::from_utf8_lossy(&stdout_buf_locked).to_string();
    drop(stdout_buf_locked);

    let streams = split_worker_stream(&collected, parts.len())?;
    // Process-level outcomes belong to the entry that was live when they
    // happened: the LAST one that started. For a single-entry worker that is
    // entry 0, exactly as before.
    let blamed = streams.iter().rposition(|s| s.started).unwrap_or(0);

    let mut summaries = Vec::with_capacity(parts.len());
    for (i, (part, stream)) in parts.iter().zip(streams.iter()).enumerate() {
        let mut summary = SimSummary::from_stderr(&stream.sim.join("\n"));
        if !stream.started {
            summary.errors.push(SiteMessage {
                message: format!(
                    "sim `{}` never started: the worker process ended during an earlier entry",
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
            for line in &stream.raw {
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
        frame_budget: DEFAULT_FRAME_BUDGET,
        env: env_pairs.to_vec(),
    };
    let mut summaries = run_worker(
        project,
        std::slice::from_ref(&part),
        quiet,
        buffered,
        update_snapshots,
        output,
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
        let entry_artifacts = SimArtifacts::new()?;
        let chain = parse_sim_chain(spec);
        if chain.is_empty() {
            return Err(anyhow!("sim got an empty sim name"));
        }
        part_counts.push(chain.len());
        for name in chain {
            parts.push(WorkerPart {
                name: name.to_string(),
                out_dir: entry_artifacts.path().to_path_buf(),
                frame_budget: DEFAULT_FRAME_BUDGET,
                env: env_by_wire.get(name).cloned().unwrap_or_default(),
            });
        }
        artifacts.push(entry_artifacts);
    }

    // Not quiet, but buffered: nothing is written live (several entries share
    // one stream), and every diagnostic is retained into its record instead.
    let summaries = match grouping {
        Grouping::Shared => run_worker(project, &parts, false, true, false, output, None)?,
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
    /// What the entry left under its `$OUT`, keyed by relative path. Captured
    /// by `SimArtifacts::finish_with` while the directory is still live.
    artifacts: std::collections::BTreeMap<String, crate::sim_record::ArtifactEntry>,
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
        self.artifacts.extend(other.artifacts);
        self.timed_out = self.timed_out || other.timed_out;
        if other.process_failure_code.is_some() {
            self.process_failure_code = other.process_failure_code;
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
