use crate::frame_runtime::*;
use crate::*;
use std::process::{Command, Stdio};

/// Run the game for N frames with profiling enabled and display performance summary
fn apply_perf_env(cmd: &mut Command) {
    let perf_no_sections = std::env::var("PERF_NO_SECTIONS").unwrap_or_else(|_| "1".to_string());
    cmd.env("PERF_MODE", "1")
        .env("ROCK_FRAME_WATCHDOG", "1")
        .env("ROCK_PERF_STARTUP_WATCHDOG", "1")
        .env("STONE_HEADLESS", "1")
        .env("PERF_NO_SECTIONS", perf_no_sections);
}

pub fn do_perf(
    backend: crate::Backend,
    path: &str,
    output: String,
    frames: usize,
    seconds: Option<f64>,
    warmup_frames: Option<usize>,
    gc_window: usize,
    gc_window_start: usize,
    gc_window_mode: &str,
    format: &str,
    app_args: &[String],
) -> Result<()> {
    use crate::common::ProcessManager;

    rock_host::process_management::report_spawn_phase("compile");
    crate::package::check_project_assets(Path::new(path))?;
    // Perf runs of one tree (tools/perf-check runs several processes) share
    // one compile through the frame compile cache.
    let compiled =
        crate::frame_compile_cache::compile_game_for_frame(backend, path, &output, frames)?;
    let lua_file = &compiled.lua_file;
    rock_host::process_management::report_spawn_phase("runtime-prepare");

    let measured_frames = frames;
    anyhow::ensure!(
        measured_frames > 0 && measured_frames <= 65536,
        "perf measured frames must be between 1 and 65536"
    );
    if let Some(seconds) = seconds {
        anyhow::ensure!(
            seconds.is_finite() && seconds > 0.0,
            "--seconds must be finite and positive"
        );
        Duration::try_from_secs_f64(seconds).map_err(|error| {
            anyhow::anyhow!("--seconds is outside the supported duration range: {error}")
        })?;
    }
    // FRAME_COUNT remains an independent emergency process cap. --frames
    // describes measured work and must never include boot or readiness.

    let mut cmd = crate::headless_scope::headless_command(
        rock_engine_host::runtime::build_luajit_command(lua_file, app_args),
    );
    apply_perf_env(&mut cmd);
    cmd.env("STONE_BUILD_ID", &compiled.artifact_fingerprint);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    // Pin the RNG so the measured scene is byte-identical run-to-run: the
    // prelude otherwise seeds `random()` from os.time() (whole-second
    // granularity), which drifts RNG-driven content
    // across runs and made graphics.draw_calls a non-gateable metric. Matches
    // simulation defaults; an explicit env still overrides.
    if std::env::var_os("ROCK_TEST_SEED").is_none() {
        cmd.env("ROCK_TEST_SEED", "42");
    }

    cmd.env("PERF_DEFAULT_MEASURE_FRAMES", measured_frames.to_string());
    if let Some(secs) = seconds {
        cmd.env("PERF_SECONDS", secs.to_string());
    }
    if let Some(warmup) = warmup_frames {
        cmd.env("PERF_WARMUP_FRAMES", warmup.to_string());
    }
    if gc_window > 0 {
        cmd.env("PERF_GC_WINDOW", gc_window.to_string());
        cmd.env("PERF_GC_WINDOW_START", gc_window_start.to_string());
        cmd.env("PERF_GC_WINDOW_MODE", gc_window_mode);
    }
    cmd.env("PERF_FORMAT", format);
    if std::env::var_os("PERF_OUT").is_none() {
        let name = if format == "json" {
            "perf-summary.json"
        } else {
            "perf-summary.txt"
        };
        cmd.env("PERF_OUT", std::path::Path::new(&output).join(name));
    }

    match seconds {
        Some(secs) => info!(
            "Profiling {} measured frames; launch wall cap {}s",
            measured_frames, secs
        ),
        None => info!(
            "Profiling {} measured frames; no launch wall cap",
            measured_frames
        ),
    }

    // Asset loading is startup, not part of the budget for executing frames
    // and dumping the summary. The override bounds each phase independently.
    let timeout_override = frame_watchdog().override_secs;
    let startup_timeout = timeout_override
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(120));
    let timeout_duration = if let Some(secs) = timeout_override {
        Duration::from_secs(secs)
    } else {
        let wall_budget = seconds.unwrap_or(120.0).max(15.0) + 15.0;
        Duration::from_secs_f64(wall_budget.max(30.0))
    };

    rock_host::process_management::report_spawn_phase("execution");
    let launch_start = bone_core::prelude::Instant::now();
    let launched = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs_f64();
    cmd.env("PERF_LAUNCH_UNIX_S", launched.to_string());
    let mut child = cmd.spawn()?;
    #[cfg(unix)]
    let (process_result, launch_timed_out) = {
        let (cancel, cancelled) = std::sync::mpsc::channel::<()>();
        let pid = child.id() as libc::pid_t;
        let deadline = seconds.map(|seconds| {
            std::thread::spawn(move || -> std::io::Result<bool> {
                if cancelled.recv_timeout(
                    Duration::from_secs_f64(seconds).saturating_sub(launch_start.elapsed()),
                ) != Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                {
                    return Ok(false);
                }
                // The cooperative frame boundary owns normal exit and evidence;
                // this backstop also bounds a blocked init or native frame call.
                if unsafe { libc::kill(pid, libc::SIGKILL) } == 0 {
                    return Ok(true);
                }
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ESRCH) {
                    Ok(false)
                } else {
                    Err(error)
                }
            })
        });
        let result = ProcessManager::wait_with_progress_budget(
            &mut child,
            startup_timeout,
            timeout_duration,
            parse_frame_progress,
        );
        drop(cancel);
        let expired = match deadline {
            Some(thread) => thread
                .join()
                .map_err(|_| anyhow::anyhow!("perf launch watchdog panicked"))??,
            None => false,
        };
        (result?, expired)
    };
    #[cfg(not(unix))]
    let (process_result, launch_timed_out) = {
        let result = if let Some(seconds) = seconds {
            let result = ProcessManager::wait_with_timeout(
                &mut child,
                Duration::from_secs_f64(seconds).saturating_sub(launch_start.elapsed()),
            )?;
            crate::common::ProcessResult {
                output: result.output,
                timed_out: result.timed_out,
                timeout_phase: result.timeout_phase,
                progress: None,
                running_test: result.running_test,
                last_completed_test: result.last_completed_test,
                progress_stopped: result.progress_stopped,
            }
        } else {
            ProcessManager::wait_with_progress_budget(
                &mut child,
                startup_timeout,
                timeout_duration,
                parse_frame_progress,
            )?
        };
        let launch_timed_out = seconds.is_some() && result.timed_out;
        (result, launch_timed_out)
    };

    let killed_by_cap = process_result.timed_out || launch_timed_out;
    if killed_by_cap {
        let termination = if launch_timed_out {
            "seconds_cap"
        } else if process_result.timeout_phase == Some(crate::common::TimeoutPhase::Startup) {
            "startup_timeout"
        } else {
            "frame_timeout"
        };
        // A blocked runtime has not published its scenario identity or counters.
        let diagnostic = serde_json::json!({
            "scenario": {"name": serde_json::Value::Null,
                "ready": false, "complete": false},
            "measurement": {"termination": termination, "requested_frames": measured_frames,
                "completed_frames": null, "seconds_cap": seconds},
            "error": "runtime killed by watchdog before it could emit interval counters",
        });
        println!("{}", diagnostic);
        std::fs::write(
            std::path::Path::new(&output).join("perf-runtime.stdout.log"),
            &process_result.output.stdout,
        )?;
        std::fs::write(
            std::path::Path::new(&output).join("perf-incomplete.json"),
            diagnostic.to_string(),
        )?;
    }

    // Extract and display the perf summary from stdout
    if !process_result.output.stdout.is_empty() && !killed_by_cap {
        let stdout_str = String::from_utf8_lossy(&process_result.output.stdout);
        const DELIMITER: &str = "--- PERF_SUMMARY ---";
        if let Some(idx) = stdout_str.find(DELIMITER) {
            let summary = stdout_str[idx + DELIMITER.len()..].trim();
            // Result output, NOT logging: the perf summary is the command's
            // answer, written to stdout for piping/consumption.
            println!("{}", summary);
        } else {
            // No summary found, print raw output
            warn!("No perf summary found in output");
            if !stdout_str.trim().is_empty() {
                // Result output, NOT logging: raw game stdout is the
                // command's output when no structured summary is present.
                println!("{}", stdout_str.trim());
            }
        }
    }

    if !process_result.output.stderr.is_empty() {
        let stderr_str = String::from_utf8_lossy(&process_result.output.stderr);
        let child_failed = killed_by_cap || !process_result.output.status.success();
        for line in perf_stderr_lines(&stderr_str, child_failed) {
            // Result output, NOT logging: forwarding the spawned game
            // process's own stderr lines verbatim, not rock-cli diagnostics.
            eprintln!("{}", line);
        }
    }

    anyhow::ensure!(
        !launch_timed_out,
        "perf seconds cap expired while runtime was blocked; diagnostic artifact retained"
    );

    // A perf run that timed out or the child failed did NOT produce a valid
    // summary -- error out (nonzero exit) instead of the old warn-and-Ok(0),
    // which let a corrupt/partial benchmark JSON sail past the gate recipes
    // (justfile render-bench). Same classifier as `rock frame`.
    let timeout_secs = if process_result.timeout_phase == Some(crate::common::TimeoutPhase::Startup)
    {
        startup_timeout.as_secs()
    } else {
        timeout_duration.as_secs()
    };
    frame_exit_result_with_progress(
        process_result.timed_out,
        &process_result.output.status,
        timeout_secs,
        Some(measured_frames),
        process_result.progress.as_ref(),
        process_result.progress_stopped,
        process_result.timeout_phase,
    )?;

    Ok(())
}

/// A bounded frame run gets a startup allowance, then must report completed
/// frames often enough to show it is alive. The old total budget grew with the
/// requested frame count and killed legitimate slow runs. The explicit env
/// override remains the escape hatch for unusually slow startup or frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FrameWatchdogConfig {
    startup: Duration,
    inactivity: Duration,
    override_secs: Option<u64>,
}

fn frame_watchdog_config(override_secs: Option<u64>) -> FrameWatchdogConfig {
    let duration = override_secs.map(Duration::from_secs);
    FrameWatchdogConfig {
        startup: duration.unwrap_or(Duration::from_secs(5)),
        inactivity: duration.unwrap_or(Duration::from_secs(5)),
        override_secs,
    }
}

fn frame_watchdog() -> FrameWatchdogConfig {
    frame_watchdog_config(
        std::env::var("ROCK_FRAME_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok()),
    )
}

/// Run the game for a specified number of frames and take a screenshot
///
/// This is useful for debugging and testing - compiles the game and runs it
/// for N frames, taking a screenshot at the end.
pub fn do_one_frame(
    backend: crate::Backend,
    path: &str,
    output: String,
    frames: usize,
    _headless: bool,
    app_args: &[String],
) -> Result<()> {
    use crate::common::ProcessManager;

    rock_host::process_management::report_spawn_phase("compile");
    let compiled =
        crate::frame_compile_cache::compile_game_for_frame(backend, path, &output, frames)?;
    let lua_file = &compiled.lua_file;
    rock_host::process_management::report_spawn_phase("runtime-prepare");

    if frames == 1 {
        info!("Running stone for one frame...");
    } else {
        info!("Running stone for {} frames...", frames);
    }

    // Run the compiled Lua with luajit (stone handles window creation)
    let mut cmd = crate::headless_scope::headless_command(
        rock_engine_host::runtime::build_luajit_command(lua_file, app_args),
    );
    cmd.env("FRAME_COUNT", frames.to_string())
        .env("TAKE_SCREENSHOT", "1") // Always take screenshot when running frames
        .env("STONE_HEADLESS", "1") // No visible window for frame runs
        .env("ROCK_FRAME_WATCHDOG", "1")
        .env("STONE_BUILD_ID", &compiled.artifact_fingerprint);
    apply_headless_audio_env(&mut cmd);
    // ProcessManager drains both pipes in background threads, so the
    // watchdog can observe frame markers without risking a full pipe.
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    // Use spawn instead of output to implement the progress watchdog.
    rock_host::process_management::report_spawn_phase("execution");
    let mut child = cmd.spawn()?;

    let watchdog = frame_watchdog();
    let process_result = ProcessManager::wait_with_progress_watchdog(
        &mut child,
        watchdog.startup,
        watchdog.inactivity,
        parse_frame_progress,
        true,
        true,
    )?;

    // A watchdog timeout means the run did NOT complete -- treat it as a failure
    // (nonzero exit), not a warning-and-exit-0. Exiting 0 on timeout is precisely
    // how a load-starved run once masqueraded as a clean-but-empty log
    let timeout_secs = if process_result.timeout_phase == Some(crate::common::TimeoutPhase::Startup)
    {
        watchdog.startup.as_secs()
    } else {
        watchdog.inactivity.as_secs()
    };
    frame_exit_result_with_progress(
        process_result.timed_out,
        &process_result.output.status,
        timeout_secs,
        Some(frames),
        process_result.progress.as_ref(),
        process_result.progress_stopped,
        process_result.timeout_phase,
    )?;
    if frames == 1 {
        info!("One frame executed successfully");
    } else {
        info!("{} frames executed successfully", frames);
    }

    Ok(())
}

/// Classify a finished `rock frame` run: Ok only on a clean zero-exit. A
/// watchdog timeout or a nonzero child exit is an error, so the CLI exits
/// nonzero (test-games / callers actually fail). Split out to be unit-testable
/// without spawning a real hang.
#[cfg(test)]
fn frame_exit_result(
    timed_out: bool,
    status: &std::process::ExitStatus,
    timeout_secs: u64,
) -> Result<()> {
    frame_exit_result_with_progress(
        timed_out,
        status,
        timeout_secs,
        None,
        None,
        false,
        Some(crate::common::TimeoutPhase::TotalBudget),
    )
}

fn frame_exit_result_with_progress(
    timed_out: bool,
    status: &std::process::ExitStatus,
    timeout_secs: u64,
    requested: Option<usize>,
    progress: Option<&crate::frame_runtime::FrameProgress>,
    progress_stopped: bool,
    timeout_phase: Option<crate::common::TimeoutPhase>,
) -> Result<()> {
    if timed_out {
        let phase =
            timeout_phase.ok_or_else(|| anyhow::anyhow!("Stone timeout is missing its phase"))?;
        let phase = match phase {
            crate::common::TimeoutPhase::ProgressInactivity => "frame inactivity".to_owned(),
            crate::common::TimeoutPhase::ProgressBudget => "frame budget".to_owned(),
            phase => phase.to_string(),
        };
        let (renderer, completed, requested) = match progress {
            Some(progress) => (
                progress.renderer.as_str(),
                progress.completed,
                progress.requested.or(requested),
            ),
            None => ("unknown", 0, requested),
        };
        let requested = requested
            .map(|frames| frames.to_string())
            .unwrap_or_else(|| "-".into());
        anyhow::bail!(
            "Stone process timed out after {timeout_secs}s -- phase={phase}, renderer={renderer}, \
             completed/requested={completed}/{requested} (last reported), progress_stopped={progress_stopped}; \
             no clean exit (a hang, an infinite loop, or a run starved past the watchdog). \
             For a legitimately slow run, raise ROCK_FRAME_TIMEOUT_SECS."
        );
    }
    if !status.success() {
        // Propagate the child's failure (e.g. the runner's GL-error exit 3).
        anyhow::bail!("Stone exited with code: {:?}", status.code());
    }
    Ok(())
}

// The game's stderr lines a perf run forwards. A failed run forwards all of
// it: a runtime error's message line ("luajit: ...") matches no filter, and
// keeping only its traceback hid why the run failed. A successful run keeps
// profile lines, errors and structured logger records that need action.
fn perf_stderr_lines(stderr: &str, child_failed: bool) -> Vec<&str> {
    if child_failed {
        return stderr.lines().collect();
    }
    stderr
        .lines()
        .filter(|line| {
            line.contains("[profile]")
                || line.contains("Error")
                || line.contains("error")
                || crate::log_record::is_error_or_warning_log_record(line)
        })
        .collect()
}
/// `rock frame` on the skull backend (do_one_frame is backend-parameterized).
pub fn handle_frame(cx: &CommandContext) -> Result<()> {
    let file = get_game_file(cx)?;
    let output = cx.args.output.clone().unwrap_or_else(|| "tmp".into());
    do_one_frame(
        cx.args.backend,
        &file,
        output,
        cx.args.frames,
        cx.args.headless,
        &cx.args.app_args,
    )
}

/// `rock perf` on the skull backend.
pub fn handle_perf(cx: &CommandContext) -> Result<()> {
    let file = get_game_file(cx)?;
    let output = cx.args.output.clone().unwrap_or_else(|| "tmp".into());
    do_perf(
        cx.args.backend,
        &file,
        output,
        cx.args.frames,
        cx.args.seconds,
        (cx.args.warmup != usize::MAX).then_some(cx.args.warmup),
        cx.args.gc_window,
        cx.args.gc_window_start,
        &cx.args.gc_window_mode,
        &cx.args.perf_format,
        &cx.args.app_args,
    )
}

fn get_game_file(cx: &CommandContext) -> Result<String> {
    let Some(arg) = cx.args.files.first() else {
        // No project arg: default to `game/`, with a directory
        // suggestion (then exit) if it's absent -- unchanged behavior.
        let path = "game".to_string();
        if !std::path::Path::new(&path).exists() {
            suggest_available_directories(&path);
        }
        return Ok(path);
    };
    // Explicit project arg: bare-name resolution, same as `rock sim`
    // (a literal path wins; unknown rejects
    // listing the searched parents).
    Ok(rock_engine_host::project::resolve_project_dir(arg)?
        .to_string_lossy()
        .into_owned())
}

fn suggest_available_directories(missing_dir: &str) {
    use fs_err as fs;

    log::error!(target: "FRAME", "directory '{}' not found", missing_dir);

    // Try to find directories that look like game projects
    let mut found_dirs = Vec::new();

    if let Ok(entries) = fs::read_dir(".") {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let name = path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();

                // Skip common non-game directories
                if matches!(
                    name.as_str(),
                    "target"
                        | "rock"
                        | "stone"
                        | "src"
                        | "docs"
                        | "examples"
                        | "libs"
                        | ".git"
                        | ".github"
                        | "node_modules"
                ) {
                    continue;
                }

                // Check if it looks like a game directory (has .rock files)
                if let Ok(sub_entries) = fs::read_dir(&path) {
                    let has_rock_files = sub_entries
                        .flatten()
                        .any(|e| e.path().extension().map_or(false, |ext| ext == "rock"));

                    if has_rock_files {
                        found_dirs.push(name);
                    }
                }
            }
        }
    }

    // Same level as the "not found" error above: these are the
    // actionable recovery hints for that fatal error and emit
    // immediately before exit(1) — at info they'd be suppressed
    // under ROCK_LOG=warn|error, leaving a bare error with no
    // guidance (the original unconditional eprintln! always showed
    // them).
    if !found_dirs.is_empty() {
        found_dirs.sort();
        log::error!(target: "FRAME", "available game directories:");
        for dir in found_dirs {
            log::error!(target: "FRAME", "  rock frame {}", dir);
        }
    } else {
        log::error!(target: "FRAME", "no game directories found with .rock files");
    }

    log::error!(target: "FRAME", "usage: rock frame [GAME_DIR] [OUTPUT_DIR]");
    log::error!(target: "FRAME", "example: rock frame game --output tmp");
    std::process::exit(1);
}

pub fn arguments(command: clap::Command) -> clap::Command {
    use clap::{Arg, ArgAction};
    registry::extend_arguments(command, vec![
        registry::ArgumentGroup { before: "perf-format", arguments: vec![
            Arg::new("frames").short('n').long("frames").default_value("1")
                .value_parser(clap::value_parser!(usize))
                .help("Frame command process frames; perf command measured frames (default 1200)"),
            Arg::new("seconds").long("seconds")
                .value_parser(clap::value_parser!(f64))
                .help("Perf wall-clock safety cap from runtime launch, including readiness and warmup. Incomplete intervals fail."),
            Arg::new("warmup-frames").long("warmup-frames")
                .value_parser(clap::value_parser!(usize))
                .help("Perf warmup frames after scenario readiness (default: scenario declaration). Zero disables warmup."),
            Arg::new("gc-window").long("gc-window").default_value("0")
                .value_parser(clap::value_parser!(usize))
                .help("`rock perf` only: record N frames of gross Lua allocator request traffic (opening at --gc-window-start), byte flow, and request populations. This is request traffic, not retained heap growth. 0 = disabled."),
            Arg::new("gc-window-start").long("gc-window-start").default_value("100")
                .value_parser(clap::value_parser!(usize))
                .help("`rock perf` only: frame the --gc-window opens on (default 100). Window compilation is counted separately; warmup does not guarantee a settled JIT; the allocation window must fit within the declared measured interval."),
            Arg::new("gc-window-mode").long("gc-window-mode").default_value("stopped")
                .value_parser(["stopped", "running"])
                .help("`rock perf --gc-window` only: `stopped` pauses GC after a collection; gross requests still include JIT compilation; `running` preserves the ordinary collector schedule. The mode is recorded in profiler output."),
        ] },
        registry::ArgumentGroup { before: "test-threads", arguments: vec![
            Arg::new("headless").long("headless").action(ArgAction::SetTrue)
                .help("Run frame command in headless mode (no display required)"),
        ] },
        registry::ArgumentGroup { before: "app_args", arguments: vec![
            Arg::new("jv").long("jv").action(ArgAction::SetTrue)
                .help("Run the game's luajit with -jv (verbose JIT trace log to stderr). Same as STONE_JV=1; works for stone/frame/run/perf/sim."),
        ] },
    ])
}

pub fn decode(m: &clap::ArgMatches, args: &mut Args) {
    args.frames = if args.command.cli_name() == "perf"
        && m.value_source("frames") == Some(clap::parser::ValueSource::DefaultValue)
    {
        1200
    } else {
        *m.get_one::<usize>("frames").unwrap()
    };
    args.seconds = m.get_one::<f64>("seconds").copied();
    // The shared Args has a generic integer warmup transport; perf owns its units.
    if args.command.cli_name() == "perf" {
        args.warmup = m
            .get_one::<usize>("warmup-frames")
            .copied()
            .unwrap_or(usize::MAX);
    }
    args.gc_window = *m.get_one::<usize>("gc-window").unwrap();
    args.gc_window_start = *m.get_one::<usize>("gc-window-start").unwrap();
    args.gc_window_mode = m.get_one::<String>("gc-window-mode").cloned().unwrap();
    args.headless = m.get_flag("headless");
    args.jv = m.get_flag("jv");
}

pub fn initialize_process(args: &Args) {
    // The dispatcher calls this before starting worker threads.
    if args.jv {
        unsafe { std::env::set_var("STONE_JV", "1") };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_failed_perf_run_forwards_the_whole_runtime_error() {
        let stderr = "[   0.1s INFO ] [FRAME] frame compile cache miss\n\
            luajit: PERF_GC_STEPS requires tools/luajit-gc-probe.py runtime\n\
            stack traceback:\n\
            \t[C]: in function 'error'\n";
        let failed = perf_stderr_lines(stderr, true);
        assert!(
            failed.contains(&"luajit: PERF_GC_STEPS requires tools/luajit-gc-probe.py runtime"),
            "{failed:?}"
        );
        assert!(failed.contains(&"stack traceback:"), "{failed:?}");
        let passed = perf_stderr_lines(stderr, false);
        assert_eq!(
            passed,
            ["\t[C]: in function 'error'"],
            "a successful run keeps the noise filter"
        );
    }
    #[test]
    fn perf_process_is_headless_without_screenshot_mode() {
        let mut cmd = std::process::Command::new("true");
        apply_perf_env(&mut cmd);
        let env: std::collections::HashMap<_, _> = cmd
            .get_envs()
            .filter_map(|(key, value)| Some((key.to_str()?, value?.to_str()?)))
            .collect();

        assert_eq!(env.get("PERF_MODE"), Some(&"1"));
        assert_eq!(env.get("STONE_HEADLESS"), Some(&"1"));
        assert_eq!(env.get("PERF_NO_SECTIONS"), Some(&"1"));
        assert!(!env.contains_key("TAKE_SCREENSHOT"));
    }
    #[cfg(unix)]
    #[test]
    fn timed_out_frame_run_is_an_error() {
        use std::os::unix::process::ExitStatusExt;
        let ok = std::process::ExitStatus::from_raw(0);
        let failed = std::process::ExitStatus::from_raw(3 << 8); // exit code 3
        assert!(
            frame_exit_result(false, &ok, 3).is_ok(),
            "clean zero-exit is Ok"
        );
        assert!(
            frame_exit_result(true, &ok, 3).is_err(),
            "a timeout must be an error even if the killed status looks like success"
        );
        assert!(
            frame_exit_result(false, &failed, 3).is_err(),
            "a nonzero child exit must propagate"
        );
    }
    #[cfg(unix)]
    #[test]
    fn timeout_diagnostics_name_startup_and_frame_inactivity() {
        use std::os::unix::process::ExitStatusExt;
        let status = std::process::ExitStatus::from_raw(0);
        let startup = super::frame_exit_result_with_progress(
            true,
            &status,
            120,
            Some(240),
            None,
            false,
            Some(crate::common::TimeoutPhase::Startup),
        )
        .unwrap_err()
        .to_string();
        assert!(startup.contains("phase=startup"), "{startup}");
        assert!(startup.contains("completed/requested=0/240"), "{startup}");

        let progress = crate::frame_runtime::FrameProgress {
            renderer: "gl".into(),
            completed: 1,
            requested: Some(240),
        };
        let inactivity = super::frame_exit_result_with_progress(
            true,
            &status,
            30,
            Some(240),
            Some(&progress),
            true,
            Some(crate::common::TimeoutPhase::ProgressInactivity),
        )
        .unwrap_err()
        .to_string();
        assert!(
            inactivity.contains("phase=frame inactivity"),
            "{inactivity}"
        );
        assert!(
            inactivity.contains("renderer=gl, completed/requested=1/240"),
            "{inactivity}"
        );
    }

    /// The frame watchdog bounds startup and inactivity independently of the
    /// requested frame count. A legitimate slow run can therefore outlive its
    /// old total budget, while a child that stops reporting still fails.
    #[test]
    fn frame_watchdog_preserves_progress_and_override_contracts() {
        let default = frame_watchdog_config(None);
        assert_eq!(default.startup, Duration::from_secs(5));
        assert_eq!(default.inactivity, Duration::from_secs(5));
        assert_eq!(default.override_secs, None);

        let override_config = frame_watchdog_config(Some(60));
        assert_eq!(override_config.startup, Duration::from_secs(60));
        assert_eq!(override_config.inactivity, Duration::from_secs(60));
        assert_eq!(override_config.override_secs, Some(60));
    }
}
