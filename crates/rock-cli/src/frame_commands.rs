use crate::common::{ProcessManager, ProcessResult, ProgressWatchdog, TimeoutPhase};
use crate::frame_runtime::*;
use crate::*;
use bone_core::prelude::Instant;
use std::process::{Command, Stdio};

fn apply_perf_env(cmd: &mut Command) {
    let perf_no_sections = std::env::var("PERF_NO_SECTIONS").unwrap_or_else(|_| "1".to_string());
    cmd.env("PERF_MODE", "1")
        .env("ROCK_FRAME_WATCHDOG", "1")
        .env("ROCK_PERF_STARTUP_WATCHDOG", "1")
        .env("STONE_HEADLESS", "1")
        .env("PERF_NO_SECTIONS", perf_no_sections);
}

pub(crate) fn handle_perf(args: &Args) -> Result<()> {
    let path = get_game_file(args)?;
    let output = args.output.as_deref().unwrap_or("tmp");
    crate::package::check_project_assets(Path::new(&path))?;
    let (_invocation, lua_file) = compile_frame_invocation(&path, output)?;
    let measured_frames = args.frames.unwrap_or(1200);
    let seconds = args.seconds;
    let format = &args.perf_format;
    anyhow::ensure!(
        measured_frames > 0 && measured_frames <= 65536,
        "perf measured frames must be between 1 and 65536"
    );
    let seconds_cap = seconds
        .map(|seconds| {
            anyhow::ensure!(
                seconds.is_finite() && seconds > 0.0,
                "--seconds must be finite and positive"
            );
            Duration::try_from_secs_f64(seconds).map_err(|error| {
                anyhow::anyhow!("--seconds is outside the supported duration range: {error}")
            })
        })
        .transpose()?;
    // FRAME_COUNT remains an independent emergency process cap. --frames
    // describes measured work and must never include boot or readiness.

    let mut cmd = frame_command(&lua_file, &args.app_args);
    apply_perf_env(&mut cmd);

    if std::env::var_os("ROCK_TEST_SEED").is_none() {
        cmd.env("ROCK_TEST_SEED", "42");
    }

    cmd.env("PERF_DEFAULT_MEASURE_FRAMES", measured_frames.to_string());
    if let Some(secs) = seconds {
        cmd.env("PERF_SECONDS", secs.to_string());
    }
    if let Some(warmup) = args.warmup_frames {
        cmd.env("PERF_WARMUP_FRAMES", warmup.to_string());
    }
    if args.gc_window > 0 {
        cmd.env("PERF_GC_WINDOW", args.gc_window.to_string());
        cmd.env("PERF_GC_WINDOW_START", args.gc_window_start.to_string());
        cmd.env("PERF_GC_WINDOW_MODE", &args.gc_window_mode);
    }
    cmd.env("PERF_FORMAT", format);
    let name = if format == "json" {
        "perf-summary.json"
    } else {
        "perf-summary.txt"
    };
    cmd.env("PERF_OUT", std::path::Path::new(output).join(name));

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
        let margin = Duration::from_secs(15);
        seconds_cap
            .unwrap_or(startup_timeout)
            .max(margin)
            .saturating_add(margin)
    });

    let launch_start = Instant::now();
    let launched = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs_f64();
    cmd.env("PERF_LAUNCH_UNIX_S", launched.to_string());
    // With an explicit cap the non-Unix launcher historically uses only that budget.
    let phase_limits = cfg!(unix) || seconds_cap.is_none();
    let process_result = wait_for_frames(
        cmd,
        &lua_file,
        ProgressWatchdog {
            startup: if phase_limits {
                startup_timeout
            } else {
                Duration::MAX
            },
            running: if phase_limits {
                timeout_duration
            } else {
                Duration::MAX
            },
            inactivity: false,
            launch_cap: seconds_cap.map(|cap| (launch_start, cap)),
        },
    )?;
    let launch_timed_out = process_result.timeout_phase == Some(TimeoutPhase::TotalBudget);
    if process_result.timed_out {
        let termination = match process_result.timeout_phase {
            Some(TimeoutPhase::TotalBudget) => "seconds_cap",
            Some(TimeoutPhase::Startup) => "startup_timeout",
            _ => "frame_timeout",
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
            std::path::Path::new(output).join("perf-runtime.stdout.log"),
            &process_result.output.stdout,
        )?;
        std::fs::write(
            std::path::Path::new(output).join("perf-incomplete.json"),
            diagnostic.to_string(),
        )?;
    }

    if !process_result.output.stdout.is_empty() && !process_result.timed_out {
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
        let child_failed = process_result.timed_out || !process_result.output.status.success();
        for line in perf_stderr_lines(&stderr_str, child_failed) {
            eprintln!("{}", line);
        }
    }

    anyhow::ensure!(
        !launch_timed_out,
        "perf seconds cap expired while runtime was blocked; diagnostic artifact retained"
    );

    frame_exit_result(
        &process_result,
        startup_timeout,
        timeout_duration,
        measured_frames,
    )
}

fn frame_timeout_override() -> Option<Duration> {
    std::env::var("ROCK_FRAME_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
}

pub(crate) fn handle_frame(args: &Args) -> Result<()> {
    let path = get_game_file(args)?;
    let output = args.output.as_deref().unwrap_or("tmp");
    let (_invocation, lua_file) = compile_frame_invocation(&path, output)?;
    let frames = args.frames.unwrap_or(1);

    if frames == 1 {
        info!("Running stone for one frame...");
    } else {
        info!("Running stone for {} frames...", frames);
    }

    let mut cmd = frame_command(&lua_file, &args.app_args);
    cmd.env("FRAME_COUNT", frames.to_string())
        .env("TAKE_SCREENSHOT", "1") // Always take screenshot when running frames
        .env("STONE_HEADLESS", "1") // No visible window for frame runs
        .env("ROCK_FRAME_WATCHDOG", "1");
    apply_headless_audio_env(&mut cmd);

    let watchdog = frame_timeout_override().unwrap_or(Duration::from_secs(5));
    let process_result = wait_for_frames(
        cmd,
        &lua_file,
        ProgressWatchdog {
            startup: watchdog,
            running: watchdog,
            inactivity: true,
            launch_cap: None,
        },
    )?;

    frame_exit_result(&process_result, watchdog, watchdog, frames)?;
    if frames == 1 {
        info!("One frame executed successfully");
    } else {
        info!("{} frames executed successfully", frames);
    }

    Ok(())
}

fn frame_command(lua_file: &Path, app_args: &[String]) -> Command {
    let mut command = crate::headless_scope::headless_command(
        rock_engine_host::runtime::build_luajit_command(lua_file, app_args),
    );
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    command
}

fn wait_for_frames(
    mut command: Command,
    lua_file: &Path,
    watchdog: ProgressWatchdog,
) -> Result<ProcessResult<FrameProgress>> {
    let mut child =
        crate::headless_scope::spawn_bound_child(&mut command, lua_file.parent().unwrap())?;
    ProcessManager::wait_with_progress(&mut child, watchdog, parse_frame_progress)
}

fn frame_exit_result(
    result: &ProcessResult<FrameProgress>,
    startup: Duration,
    running: Duration,
    frames: usize,
) -> Result<()> {
    if result.timed_out {
        let phase = result
            .timeout_phase
            .ok_or_else(|| anyhow::anyhow!("Stone timeout is missing its phase"))?;
        let timeout_secs = if phase == TimeoutPhase::Startup {
            startup
        } else {
            running
        }
        .as_secs();
        let phase = match phase {
            TimeoutPhase::ProgressInactivity => "frame inactivity".to_owned(),
            TimeoutPhase::ProgressBudget => "frame budget".to_owned(),
            phase => phase.to_string(),
        };
        let (renderer, completed, requested) =
            result
                .progress
                .as_ref()
                .map_or(("unknown", 0, frames), |p| {
                    (
                        p.renderer.as_str(),
                        p.completed,
                        p.requested.unwrap_or(frames),
                    )
                });
        let progress_stopped = result.progress_stopped;
        anyhow::bail!(
            "Stone process timed out after {timeout_secs}s -- phase={phase}, renderer={renderer}, \
             completed/requested={completed}/{requested} (last reported), progress_stopped={progress_stopped}; \
             no clean exit (a hang, an infinite loop, or a run starved past the watchdog). \
             For a legitimately slow run, raise ROCK_FRAME_TIMEOUT_SECS."
        );
    }
    let status = result.output.status;
    anyhow::ensure!(
        status.success(),
        "Stone exited with code: {:?}",
        status.code()
    );
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
    fn result(
        status: i32,
        phase: Option<TimeoutPhase>,
        progress: Option<FrameProgress>,
    ) -> ProcessResult<FrameProgress> {
        use std::os::unix::process::ExitStatusExt;
        ProcessResult {
            output: std::process::Output {
                status: std::process::ExitStatus::from_raw(status),
                stdout: Vec::new(),
                stderr: Vec::new(),
            },
            timed_out: phase.is_some(),
            timeout_phase: phase,
            progress_stopped: phase == Some(TimeoutPhase::ProgressInactivity),
            progress,
        }
    }

    #[cfg(unix)]
    #[test]
    fn timed_out_frame_run_is_an_error() {
        let verdict =
            |result| frame_exit_result(&result, Duration::from_secs(3), Duration::from_secs(3), 1);
        assert!(
            verdict(result(0, None, None)).is_ok(),
            "clean zero-exit is Ok"
        );
        assert!(
            verdict(result(0, Some(TimeoutPhase::TotalBudget), None)).is_err(),
            "a timeout must be an error even if the killed status looks like success"
        );
        assert!(
            verdict(result(3 << 8, None, None)).is_err(),
            "a nonzero child exit must propagate"
        );
    }

    #[cfg(unix)]
    #[test]
    fn timeout_diagnostics_name_startup_and_frame_inactivity() {
        let progress = FrameProgress {
            renderer: "gl".into(),
            completed: 1,
            requested: Some(240),
        };
        for (phase, progress, expected) in [
            (
                TimeoutPhase::Startup,
                None,
                ["phase=startup", "completed/requested=0/240"],
            ),
            (
                TimeoutPhase::ProgressInactivity,
                Some(progress),
                [
                    "phase=frame inactivity",
                    "renderer=gl, completed/requested=1/240",
                ],
            ),
        ] {
            let err = frame_exit_result(
                &result(0, Some(phase), progress),
                Duration::from_secs(120),
                Duration::from_secs(30),
                240,
            )
            .unwrap_err()
            .to_string();
            for expected in expected {
                assert!(err.contains(expected), "{err}");
            }
        }
    }
}
