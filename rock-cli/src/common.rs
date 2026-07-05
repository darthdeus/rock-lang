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
pub fn compile_game_for_run(
    backend: Backend,
    path: &str,
    output: &str,
) -> Result<std::path::PathBuf> {
    use std::collections::HashSet;

    rock_host::project::ensure_workspace_root()?;
    rock_host::engine_build::build_engine_libs(path)?;

    let output_path = PathBuf::from(output).join(rock_host::project::MAIN_LUA_FILENAME);
    let mut compiler = crate::game_runtime::game_compiler_for(backend, path, &output_path);
    match compiler.compile(&HashSet::new(), true) {
        Ok(()) => {
            clear_error_file();
            Ok(output_path)
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
        use std::io::Read;
        use std::thread;

        // Spawn background threads to drain pipes continuously. This prevents
        // the child from blocking on a full pipe buffer while the parent polls.
        // Read errors are surfaced to parent stderr -- silently truncating
        // would hand callers a partial buffer that looks complete.
        let stdout_thread = child.stdout.take().map(|mut h| {
            thread::spawn(move || {
                let mut buf = Vec::new();
                if let Err(e) = h.read_to_end(&mut buf) {
                    log::error!(target: "CLI", "ProcessManager: error reading child stdout: {}", e);
                }
                buf
            })
        });
        let stderr_thread = child.stderr.take().map(|mut h| {
            thread::spawn(move || {
                let mut buf = Vec::new();
                if let Err(e) = h.read_to_end(&mut buf) {
                    log::error!(target: "CLI", "ProcessManager: error reading child stderr: {}", e);
                }
                buf
            })
        });

        let start_time = std::time::Instant::now();
        let timed_out;
        let status;

        loop {
            match child.try_wait()? {
                Some(s) => {
                    status = s;
                    timed_out = false;
                    break;
                }
                None => {
                    if start_time.elapsed() > timeout {
                        child.kill()?;
                        status = child.wait()?;
                        timed_out = true;
                        break;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
            }
        }

        let stdout = stdout_thread.map(|t| t.join().unwrap_or_default()).unwrap_or_default();
        let stderr = stderr_thread.map(|t| t.join().unwrap_or_default()).unwrap_or_default();

        Ok(ProcessResult {
            output: std::process::Output { status, stdout, stderr },
            timed_out,
        })
    }
}

/// Result of waiting for a child process with timeout
#[derive(Debug)]
pub struct ProcessResult {
    pub output: std::process::Output,
    pub timed_out: bool,
}
