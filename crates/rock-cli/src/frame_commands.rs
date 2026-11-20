use crate::frame_runtime::*;
use crate::*;
use std::process::{Command, Stdio};

fn apply_perf_env(cmd: &mut Command) {
    let perf_no_sections = std::env::var("PERF_NO_SECTIONS").unwrap_or_else(|_| "1".to_string());
    cmd.env("PERF_MODE", "1")
        .env("ROCK_FRAME_WATCHDOG", "1")
        .env("ROCK_PERF_STARTUP_WATCHDOG", "1")
        .env("STONE_HEADLESS", "1")
        .env("PERF_NO_SECTIONS", perf_no_sections);
}

pub(crate) fn do_perf(
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

    crate::package::check_project_assets(Path::new(path))?;
    let (_invocation, compiled) = compile_frame_invocation(path, &output)?;
    let lua_file = &compiled.lua_file;

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
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

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
    let name = if format == "json" {
        "perf-summary.json"
    } else {
        "perf-summary.txt"
    };
    cmd.env("PERF_OUT", std::path::Path::new(&output).join(name));

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

    let timeout_override = frame_timeout_override();
    let startup_timeout = timeout_override.unwrap_or(Duration::from_secs(120));
    let timeout_duration = timeout_override.unwrap_or_else(|| {
        let wall_budget = seconds.unwrap_or(120.0).max(15.0) + 15.0;
        Duration::from_secs_f64(wall_budget.max(30.0))
    });

    let launch_start = bone_core::prelude::Instant::now();
    let launched = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs_f64();
    cmd.env("PERF_LAUNCH_UNIX_S", launched.to_string());
    let mut child = crate::headless_scope::spawn_bound_child(&mut cmd, lua_file.parent().unwrap())?;
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
            result.with_progress(None)
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

    if !process_result.output.stdout.is_empty() && !killed_by_cap {
        let stdout_str = String::from_utf8_lossy(&process_result.output.stdout);
        const DELIMITER: &str = "--- PERF_SUMMARY ---";
        if let Some(idx) = stdout_str.find(DELIMITER) {
            let summary = stdout_str[idx + DELIMITER.len()..].trim();
            println!("{}", summary);
        } else {
            // No summary found, print raw output
            warn!("No perf summary found in output");
            if !stdout_str.trim().is_empty() {
                println!("{}", stdout_str.trim());
            }
        }
    }

    if !process_result.output.stderr.is_empty() {
        let stderr_str = String::from_utf8_lossy(&process_result.output.stderr);
        let child_failed = killed_by_cap || !process_result.output.status.success();
        for line in perf_stderr_lines(&stderr_str, child_failed) {
            eprintln!("{}", line);
        }
    }

    anyhow::ensure!(
        !launch_timed_out,
        "perf seconds cap expired while runtime was blocked; diagnostic artifact retained"
    );

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

fn frame_timeout_override() -> Option<Duration> {
    std::env::var("ROCK_FRAME_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
}

pub(crate) fn do_one_frame(
    path: &str,
    output: String,
    frames: usize,
    app_args: &[String],
) -> Result<()> {
    use crate::common::ProcessManager;

    let (_invocation, compiled) = compile_frame_invocation(path, &output)?;
    let lua_file = &compiled.lua_file;

    if frames == 1 {
        info!("Running stone for one frame...");
    } else {
        info!("Running stone for {} frames...", frames);
    }

    let mut cmd = crate::headless_scope::headless_command(
        rock_engine_host::runtime::build_luajit_command(lua_file, app_args),
    );
    cmd.env("FRAME_COUNT", frames.to_string())
        .env("TAKE_SCREENSHOT", "1") // Always take screenshot when running frames
        .env("STONE_HEADLESS", "1") // No visible window for frame runs
        .env("ROCK_FRAME_WATCHDOG", "1");
    apply_headless_audio_env(&mut cmd);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = crate::headless_scope::spawn_bound_child(&mut cmd, lua_file.parent().unwrap())?;

    let watchdog = frame_timeout_override().unwrap_or(Duration::from_secs(5));
    let process_result = ProcessManager::wait_with_progress_watchdog(
        &mut child,
        watchdog,
        watchdog,
        parse_frame_progress,
        true,
        true,
    )?;

    frame_exit_result_with_progress(
        process_result.timed_out,
        &process_result.output.status,
        watchdog.as_secs(),
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
pub(crate) fn handle_frame(args: &Args) -> Result<()> {
    let file = get_game_file(args)?;
    let output = args.output.clone().unwrap_or_else(|| "tmp".into());
    do_one_frame(&file, output, args.frames.unwrap_or(1), &args.app_args)
}

pub(crate) fn handle_perf(args: &Args) -> Result<()> {
    let file = get_game_file(args)?;
    let output = args.output.clone().unwrap_or_else(|| "tmp".into());
    do_perf(
        &file,
        output,
        args.frames.unwrap_or(1200),
        args.seconds,
        args.warmup_frames,
        args.gc_window,
        args.gc_window_start,
        &args.gc_window_mode,
        &args.perf_format,
        &args.app_args,
    )
}

fn get_game_file(args: &Args) -> Result<String> {
    let Some(arg) = args.files.first() else {
        let path = "game".to_string();
        if !std::path::Path::new(&path).exists() {
            suggest_available_directories(&path);
        }
        return Ok(path);
    };
    Ok(rock_engine_host::project::resolve_project_dir(arg)?
        .to_string_lossy()
        .into_owned())
}

fn suggest_available_directories(missing_dir: &str) {
    use fs_err as fs;

    log::error!(target: "FRAME", "directory '{}' not found", missing_dir);

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

pub(crate) fn initialize_process(args: &Args) {
    if args.jv {
        unsafe { std::env::set_var("STONE_JV", "1") };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_failed_perf_run_forwards_the_whole_runtime_error() {
        let stderr = "[   0.1s INFO ] [ASSETS] release asset assets does not exist, skipping\n\
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
    fn successful_perf_runs_preserve_the_exact_log_filter() {
        let lines = [
            ("plain output", false),
            ("[profile] samples", true),
            ("Error: runtime failure", true),
            ("an error continuation", true),
            ("[   0.001s ERROR] failure", true),
            ("[   0.001s WARN ] caution", true),
            ("[   0.001s INFO ] status", false),
            ("[   0.001s DEBUG] status", false),
            ("[   0.001s TRACE] status", false),
            ("\x1b[1;31m[   0.001s ERROR] failure\x1b[0m", true),
            ("\x1b[33m[   0.001s WARN ] caution\x1b[0m", true),
            ("\x1b[33m[   0.001s ERROR] wrong color\x1b[0m", false),
            ("[   0.001s WARN ] continuation without reset", true),
            ("\x1b[0m", false),
            ("[1.23s SCORE] result", false),
            ("[   0.001s ERROR]no separator", false),
            ("[    0.001s WARN ] excess padding", false),
            ("[  0.001s WARN ] insufficient padding", false),
            ("[   0.01s WARN ] short fraction", false),
            ("[   0.0010s WARN ] long fraction", false),
            ("[   0.001s WARN] short tag", false),
            ("[   0.001s WARN ]", false),
            ("[   0.001s WARN ] ", true),
            ("[1234.001s WARN ] wide time", true),
            ("[   0.001s WARN ]x", false),
            ("[   0.001s WARN ]~time", true),
        ];
        let stderr = lines
            .iter()
            .map(|(line, _)| *line)
            .collect::<Vec<_>>()
            .join("\n");
        let expected: Vec<_> = lines
            .iter()
            .filter_map(|(line, keep)| keep.then_some(*line))
            .collect();
        assert_eq!(perf_stderr_lines(&stderr, false), expected);
        assert_eq!(
            perf_stderr_lines(&stderr, true),
            stderr.lines().collect::<Vec<_>>()
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
        let result = |timed_out, status| {
            frame_exit_result_with_progress(
                timed_out,
                &std::process::ExitStatus::from_raw(status),
                3,
                None,
                None,
                false,
                Some(crate::common::TimeoutPhase::TotalBudget),
            )
        };
        assert!(result(false, 0).is_ok(), "clean zero-exit is Ok");
        assert!(
            result(true, 0).is_err(),
            "a timeout must be an error even if the killed status looks like success"
        );
        assert!(
            result(false, 3 << 8).is_err(),
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
}
