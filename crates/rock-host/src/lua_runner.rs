//! In-memory luajit eval: pipe Rock-compiled Lua into luajit's stdin, capture
//! combined stdout/stderr. Used by the Rust test suite and the `@test`-runner —
//! callers that compile a snippet and want its output as a String. NOT for
//! `rock run`/`rock stone` (those exec the compiled `.lua` file with inherited
//! stdio so interactive apps reach the tty).

use crate::process_management::{register_child_process, unregister_child_process};
use rock_core::CompilerError;
use std::time::Duration;
use wait_timeout::ChildExt;

/// What the luajit child did: its streams, and whether it EXITED CLEANLY.
///
/// `success` is the child's real exit status, kept as data. It used to be
/// encoded in-band -- the returned String got `"STDERR: "` prepended -- so every
/// caller had to sniff for that substring to learn whether the program died, and
/// `rock run` (which just printed the string) could not tell at all and exited 0
/// on a crash. An in-band marker also conflated two different things: a run that
/// FAILED, and a run that merely wrote to stderr.
///
/// `Err` from the runner stays reserved for HARNESS failures -- luajit missing,
/// a broken pipe, a timeout -- which is a different category from "the program
/// ran and exited non-zero".
#[derive(Debug, Clone)]
pub struct LuaRun {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
}

impl LuaRun {
    /// stdout followed by stderr -- for callers that only want to show a human
    /// what happened. Never use this to DECIDE anything; read `success`.
    pub fn combined(&self) -> String {
        if self.stderr.is_empty() {
            self.stdout.clone()
        } else {
            format!("{}STDERR: {}", self.stdout, self.stderr)
        }
    }
}

pub fn run_lua_code_with_timeout(
    code: &str,
    timeout: Option<Duration>,
) -> Result<LuaRun, CompilerError> {
    let luajit_path = crate::project::luajit_path();
    let luajit_path_str = luajit_path.to_string_lossy().into_owned();

    if !luajit_path.exists() {
        return Err(CompilerError::new_no_location(format!(
            "LuaJIT not found at: {}. Make sure prebuilt gamelibs are installed or ROCK_DIR is set.",
            luajit_path_str
        )));
    }

    let mut cmd = std::process::Command::new(&luajit_path_str);
    // `-` = "run stdin as a script". WITHOUT it luajit treats piped stdin as an
    // INTERACTIVE session: it prints a fatal error and still exits 0. That is the
    // root of the whole "`rock run` exits 0 while printing a LuaJIT crash" trap --
    // the status was unusable, so failure got encoded as a `"STDERR: "` marker in
    // the output string instead, and `rock run` printed it and returned Ok.
    cmd.arg("-")
        .env("LUA_PATH", crate::project::lua_path_value())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = cmd.spawn().expect("failed to execute process");
    let pid = child.id();
    register_child_process(pid);

    {
        use std::io::Write;
        let stdin = child.stdin.as_mut().expect("Failed to open stdin");
        if let Err(e) = stdin.write_all(code.as_bytes()) {
            let _ = child.kill();
            let _ = child.wait();
            unregister_child_process(pid);
            return Err(CompilerError::new_no_location(format!(
                "Failed to write test code to luajit process: {}.",
                e
            )));
        }
    }

    // Drain both pipes on threads BEFORE waiting. Waiting first deadlocks as
    // soon as the child fills a pipe buffer, which is the ~50k-line-script hang
    // recorded in `stone_test_decorator_cleanup.rs`.
    let mut stdout_pipe = child.stdout.take().expect("stdout piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr piped");
    let stdout_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::Read::read_to_end(&mut stdout_pipe, &mut buf);
        buf
    });
    let stderr_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::Read::read_to_end(&mut stderr_pipe, &mut buf);
        buf
    });

    // The status MUST come from whichever call reaped the child. `wait_timeout`
    // reaps it itself, so a following `wait`/`wait_with_output` cannot report the
    // real status -- it fabricates `exit(0)`. That is why this used to treat any
    // stderr output as failure: the stderr check was compensating for an exit
    // status that was always 0 here, which in turn is why `rock run` exited 0 on
    // a program that died.
    let status = if let Some(timeout_duration) = timeout {
        match child
            .wait_timeout(timeout_duration)
            .expect("Failed to wait for process")
        {
            Some(status) => status,
            None => {
                child.kill().expect("Failed to kill timed out process");
                child.wait().expect("Failed to wait for killed process");
                unregister_child_process(pid);
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                return Err(CompilerError::new_no_location(format!(
                    "Luajit process timed out after {} seconds.",
                    timeout_duration.as_secs_f32()
                )));
            }
        }
    } else {
        child.wait().expect("Failed to wait for process")
    };

    unregister_child_process(pid);

    let stdout = stdout_thread.join().unwrap_or_default();
    let stderr = stderr_thread.join().unwrap_or_default();

    Ok(LuaRun {
        stdout: String::from_utf8_lossy(&stdout).to_string(),
        stderr: String::from_utf8_lossy(&stderr).to_string(),
        success: status.success(),
    })
}

pub fn run_lua_code(code: &str) -> Result<LuaRun, CompilerError> {
    run_lua_code_with_timeout(code, Some(Duration::from_millis(1800)))
}
