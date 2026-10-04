use crate::*;
use std::process::Child;
use std::time::Duration;

/// Compile a game once for `rock frame` / `rock perf` style commands on the
/// chosen `backend`: ensure workspace root, build engine libs, then a full
/// from-scratch compile via the backend's `GameCompiler` (skull: `emit_full`).
/// On success clears tmp/error.txt and returns the `output/main.lua` path luajit
/// should be exec'd on. On failure writes tmp/error.txt and propagates the error
/// (caller exits).
///
/// `output` is the output directory (typically "tmp"); the returned
/// path is `<output>/main.lua`.
pub struct CompiledGame {
    pub lua_file: std::path::PathBuf,
    pub artifact_fingerprint: String,
}

pub fn compile_game_for_run(
    backend: Backend,
    path: &str,
    output: &str,
) -> Result<CompiledGame> {
    use std::collections::HashSet;

    rock_host::project::ensure_workspace_root()?;
    crate::skull_backend::ensure_renderer_feature(path)?;
    crate::package::prepare_sound_bank(path)?;
    rock_host::engine_build::build_engine_libs(path)?;

    let output_path = PathBuf::from(output).join(rock_host::project::MAIN_LUA_FILENAME);
    let mut compiler = crate::game_runtime::game_compiler_for(backend, path, &output_path);
    match compiler.compile(&HashSet::new(), true) {
        // One-shot from-scratch compile: always the init path, so the outcome
        // can never request a shape restart (there is no live process).
        Ok(_) => {
            let artifact_fingerprint = compiler.artifact_fingerprint()?;
            clear_error_file();
            Ok(CompiledGame { lua_file: output_path, artifact_fingerprint })
        }
        Err(e) => {
            record_compilation_error(&e);
            Err(e)
        }
    }
}

/// Trivial path -> SourceSpec mapping for callers that don't need
/// stick.toml walk-up: directory becomes `Directory`, single file
/// becomes `Files`. `do_run_timeout` keeps its own variant because
/// it walks up to find the enclosing stick package.
pub fn path_to_source_spec(path: &str) -> crate::unified_compile::SourceSpec {
    let path_buf = std::path::PathBuf::from(path);
    if path_buf.is_dir() {
        crate::unified_compile::SourceSpec::Directory(path_buf)
    } else {
        crate::unified_compile::SourceSpec::Files(vec![path_buf])
    }
}

/// Path of the compile-error file `rock stone` writes for in-game
/// display. Stone's runner reads this and surfaces the message inside
/// the game window so the user doesn't have to flip to the terminal.
pub const ERROR_FILE_PATH: &str = "tmp/error.txt";

/// Write a compilation error to `tmp/error.txt` (read by stone's
/// in-game error overlay). Failures to write are logged to stderr;
/// they're never load-bearing -- the message is also on stderr.
pub fn write_error_file(error_message: &str) {
    let cleaned = strip_ansi_and_unicode(error_message);
    if let Err(e) = fs_err::write(ERROR_FILE_PATH, &cleaned) {
        log::warn!(target: "CLI", "failed to write error to {}: {}", ERROR_FILE_PATH, e);
    }
}

/// Remove `tmp/error.txt` after a successful compile.
pub fn clear_error_file() {
    if fs_err::metadata(ERROR_FILE_PATH).is_ok() {
        if let Err(e) = fs_err::remove_file(ERROR_FILE_PATH) {
            log::warn!(target: "CLI", "failed to remove error file {}: {}", ERROR_FILE_PATH, e);
        }
    }
}

/// Record a compilation error into `tmp/error.txt` for stone's
/// in-game overlay. Does **not** eprintln -- callers that propagate
/// the error via `?` let main print it once, callers that need it
/// surfaced sooner (e.g. the watch loop) handle the eprintln
/// themselves.
pub fn record_compilation_error(error: &anyhow::Error) {
    write_error_file(&format!("Compilation failed: {}", error));
}

/// Process management utilities for child processes
pub struct ProcessManager;

impl ProcessManager {
    /// Wait for a child process with timeout, returning output and timeout status.
    ///
    /// Pipe outputs (stdout/stderr) are drained by background threads throughout
    /// the child's lifetime. Without this, a child that writes more than the OS
    /// pipe buffer (~64KB on macOS/Linux) blocks on write(), the parent's
    /// try_wait() never sees an exit, and the process appears to hang until the
    /// timeout fires. Observed: `rock frame mag --dump-state` writing 1.2MB to a
    /// piped stdout took 15s (the full timeout) instead of ~1s.
    pub fn wait_with_timeout(child: &mut Child, timeout: Duration) -> Result<ProcessResult> {
        Self::wait_with_policy(child, WaitPolicy::Total(timeout), false, false)
    }

    /// Wait for a bounded frame run. The startup allowance applies until the
    /// first completed-frame marker; after that only inactivity is bounded, so
    /// a slow run that keeps making progress may outlive its frame count.
    pub fn wait_with_frame_watchdog(
        child: &mut Child,
        startup: Duration,
        inactivity: Duration,
        forward_stdout: bool,
        forward_stderr: bool,
    ) -> Result<ProcessResult> {
        Self::wait_with_policy(
            child,
            WaitPolicy::Frame { startup, inactivity },
            forward_stdout,
            forward_stderr,
        )
    }

    /// Start the run budget at the first completed frame, bounding startup
    /// separately. Perf needs only that marker, not logs on measured frames.
    pub fn wait_with_startup_budget(
        child: &mut Child,
        startup: Duration,
        budget: Duration,
    ) -> Result<ProcessResult> {
        Self::wait_with_policy(child, WaitPolicy::FrameBudget { startup, budget }, false, false)
    }

    /// Bound program loading and each test file separately; only a file
    /// boundary resets the budget, never output or individual test results.
    pub fn wait_with_test_file_budget(child: &mut Child, limit: Duration) -> Result<ProcessResult> {
        Self::wait_with_policy(child, WaitPolicy::TestFile(limit), false, false)
    }

    fn wait_with_policy(
        child: &mut Child,
        policy: WaitPolicy,
        forward_stdout: bool,
        forward_stderr: bool,
    ) -> Result<ProcessResult> {
        use std::io::{self, BufRead, BufReader, Read, Write};
        use std::sync::mpsc;
        use std::thread;

        let observe_progress = !matches!(policy, WaitPolicy::Total(_));
        let (progress_tx, progress_rx) =
            if observe_progress {
                let (tx, rx) = mpsc::channel::<(std::time::Instant, ProcessProgress)>();
                (Some(tx), Some(rx))
            } else {
                (None, None)
            };

        // Spawn background threads to drain pipes continuously. This prevents
        // the child from blocking on a full pipe buffer while the parent polls.
        // Read errors are surfaced to parent stderr -- silently truncating
        // would hand callers a partial buffer that looks complete.
        let stdout_thread = child.stdout.take().map(|mut h| {
            thread::spawn(move || {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                loop {
                    match h.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => {
                            buf.extend_from_slice(&chunk[..n]);
                            if forward_stdout {
                                let mut stdout = io::stdout().lock();
                                let _ = stdout.write_all(&chunk[..n]);
                                let _ = stdout.flush();
                            }
                        }
                        Err(e) => {
                            log::error!(target: "CLI", "ProcessManager: error reading child stdout: {}", e);
                            break;
                        }
                    }
                }
                buf
            })
        });
        let stderr_thread = child.stderr.take().map(|h| {
            let progress_tx = progress_tx.clone();
            thread::spawn(move || {
                let mut reader = BufReader::new(h);
                let mut buf = Vec::new();
                let mut line = Vec::new();
                loop {
                    line.clear();
                    match reader.read_until(b'\n', &mut line) {
                        Ok(0) => break,
                        Ok(_) => {
                            buf.extend_from_slice(&line);
                            if forward_stderr {
                                let mut stderr = io::stderr().lock();
                                let _ = stderr.write_all(&line);
                                let _ = stderr.flush();
                            }
                            if let Some(tx) = &progress_tx {
                                let progress = match policy {
                                    WaitPolicy::TestFile(_) => parse_test_file_progress(&line)
                                        .map(ProcessProgress::TestFile),
                                    _ => parse_frame_progress(&line).map(ProcessProgress::Frame),
                                };
                                if let Some(progress) = progress {
                                    let _ = tx.send((std::time::Instant::now(), progress));
                                }
                            }
                        }
                        Err(e) => {
                            log::error!(target: "CLI", "ProcessManager: error reading child stderr: {}", e);
                            break;
                        }
                    }
                }
                buf
            })
        });
        drop(progress_tx);

        let start_time = std::time::Instant::now();
        let mut last_progress_at = start_time;
        let mut first_progress_at = None;
        let mut latest_progress = None;
        let mut test_file = None;
        let mut saw_progress = false;
        let timed_out;
        let timeout_phase;
        let status;

        loop {
            if let Some(rx) = &progress_rx {
                for (at, progress) in rx.try_iter() {
                    saw_progress = true;
                    last_progress_at = if matches!(policy, WaitPolicy::TestFile(_)) {
                        at
                    } else {
                        std::time::Instant::now()
                    };
                    first_progress_at.get_or_insert(last_progress_at);
                    match progress {
                        ProcessProgress::Frame(frame) => latest_progress = Some(frame),
                        ProcessProgress::TestFile(file) => test_file = Some(file),
                    }
                }
            }

            match child.try_wait()? {
                Some(s) => {
                    status = s;
                    timed_out = false;
                    timeout_phase = None;
                    break;
                }
                None => {
                    let (elapsed, limit, phase) = match policy {
                        WaitPolicy::Total(timeout) =>
                            (start_time.elapsed(), timeout, TimeoutPhase::TotalBudget),
                        WaitPolicy::Frame { startup, inactivity } => {
                            if saw_progress {
                                (last_progress_at.elapsed(), inactivity, TimeoutPhase::FrameInactivity)
                            } else {
                                (start_time.elapsed(), startup, TimeoutPhase::Startup)
                            }
                        }
                        WaitPolicy::TestFile(limit) => {
                            if saw_progress {
                                (last_progress_at.elapsed(), limit, TimeoutPhase::TestFile)
                            } else {
                                (start_time.elapsed(), limit, TimeoutPhase::Startup)
                            }
                        }
                        WaitPolicy::FrameBudget { startup, budget } => {
                            match first_progress_at {
                                Some(first) => (first.elapsed(), budget, TimeoutPhase::FrameBudget),
                                None => (start_time.elapsed(), startup, TimeoutPhase::Startup),
                            }
                        }
                    };
                    if elapsed >= limit {
                        child.kill()?;
                        status = child.wait()?;
                        timed_out = true;
                        timeout_phase = Some(phase);
                        break;
                    }
                    thread::sleep(Duration::from_millis(50));
                }
            }
        }

        let stdout = stdout_thread.map(|t| t.join().unwrap_or_default()).unwrap_or_default();
        let stderr = stderr_thread.map(|t| t.join().unwrap_or_default()).unwrap_or_default();

        // Reader joins ensure the final markers have arrived. After a timeout
        // retain the boundary that was active when the budget expired.
        if !timed_out || !matches!(policy, WaitPolicy::TestFile(_)) {
            if let Some(rx) = &progress_rx {
                for (_, progress) in rx.try_iter() {
                    match progress {
                        ProcessProgress::Frame(frame) => latest_progress = Some(frame),
                        ProcessProgress::TestFile(file) => test_file = Some(file),
                    }
                }
            }
        }

        Ok(ProcessResult {
            output: std::process::Output { status, stdout, stderr },
            timed_out,
            timeout_phase,
            frame_progress: latest_progress,
            test_file,
            progress_stopped: matches!(policy, WaitPolicy::Frame { .. }) && saw_progress && timed_out,
        })
    }
}

#[derive(Debug, Clone, Copy)]
enum WaitPolicy {
    Total(Duration),
    Frame { startup: Duration, inactivity: Duration },
    FrameBudget { startup: Duration, budget: Duration },
    TestFile(Duration),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutPhase {
    TotalBudget,
    Startup,
    FrameInactivity,
    FrameBudget,
    TestFile,
}

impl std::fmt::Display for TimeoutPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::TotalBudget => "total budget",
            Self::Startup => "startup",
            Self::FrameInactivity => "frame inactivity",
            Self::FrameBudget => "frame budget",
            Self::TestFile => "test file",
        })
    }
}

enum ProcessProgress {
    Frame(FrameProgress),
    TestFile(String),
}

fn parse_test_file_progress(line: &[u8]) -> Option<String> {
    let line = std::str::from_utf8(line).ok()?;
    let file = line.strip_prefix("ROCK_TEST_FILE\t")?.strip_suffix('\n')?;
    serde_json::from_str(file).ok()
}

fn parse_frame_progress(line: &[u8]) -> Option<FrameProgress> {
    let line = std::str::from_utf8(line).ok()?;
    let fields = line.split_once("ROCK_FRAME_PROGRESS ")?.1.split_whitespace();
    let mut renderer = None;
    let mut completed = None;
    let mut requested = None;
    for field in fields {
        let (name, value) = field.split_once('=')?;
        match name {
            "renderer" => renderer = Some(value.to_owned()),
            "completed" => completed = value.parse().ok(),
            "requested" => requested = match value {
                "-" | "-1" => Some(None),
                _ => Some(Some(value.parse().ok()?)),
            },
            _ => {}
        }
    }
    Some(FrameProgress {
        renderer: renderer?,
        completed: completed?,
        requested: requested?,
    })
}

/// Result of waiting for a child process with timeout
#[derive(Debug)]
pub struct ProcessResult {
    pub output: std::process::Output,
    pub timed_out: bool,
    pub timeout_phase: Option<TimeoutPhase>,
    pub frame_progress: Option<FrameProgress>,
    pub test_file: Option<String>,
    pub progress_stopped: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameProgress {
    pub renderer: String,
    pub completed: usize,
    pub requested: Option<usize>,
}

#[cfg(test)]
mod tests {
    use super::{parse_frame_progress, parse_test_file_progress, FrameProgress};

    #[test]
    fn test_file_progress_preserves_escaped_paths() {
        assert_eq!(parse_test_file_progress(b"ROCK_TEST_FILE\t\"a\\tb\\nc.rock\"\n"),
            Some("a\tb\nc.rock".to_owned()));
        assert_eq!(parse_test_file_progress(b"ordinary output\n"), None);
        assert_eq!(parse_test_file_progress(b"ROCK_TEST_FILE\tinvalid\n"), None);
    }

    #[test]
    fn frame_progress_parser_keeps_renderer_and_denominator() {
        assert_eq!(
            parse_frame_progress(b"[WATCHDOG] ROCK_FRAME_PROGRESS renderer=wgpu completed=17 requested=600\n"),
            Some(FrameProgress {
                renderer: "wgpu".into(),
                completed: 17,
                requested: Some(600),
            })
        );
        assert_eq!(
            parse_frame_progress(b"ROCK_FRAME_PROGRESS renderer=gl completed=4 requested=-1\n"),
            Some(FrameProgress {
                renderer: "gl".into(),
                completed: 4,
                requested: None,
            })
        );
    }

    #[test]
    fn frame_progress_parser_rejects_incomplete_markers() {
        assert_eq!(
            parse_frame_progress(b"ROCK_FRAME_PROGRESS renderer=wgpu completed=17\n"),
            None
        );
        assert_eq!(parse_frame_progress(b"ordinary game output\n"), None);
    }
}
