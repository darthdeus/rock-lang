//! In-memory luajit eval: pipe Rock-compiled Lua into luajit's stdin, capture
//! combined stdout/stderr. Used by the Rust test suite and the `@test`-runner —
//! callers that compile a snippet and want its output as a String. NOT for
//! interactive program execution (those exec the compiled `.lua` file with inherited
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
    run_lua_code_with_timeout_and_env(code, timeout, &[])
}

pub fn run_lua_code_with_timeout_and_env(
    code: &str,
    timeout: Option<Duration>,
    env: &[(&str, &str)],
) -> Result<LuaRun, CompilerError> {
    let mut command = crate::project::build_luajit_stdin_command();
    command.envs(env.iter().copied());
    run_lua_code_with_command(command, code, timeout)
}

/// Evaluate stdin using a host-configured command. Configure before calling;
/// the runner owns stdin selection, stdio, child lifetime and error reporting.
pub fn run_lua_code_with_command(
    mut cmd: std::process::Command,
    code: &str,
    timeout: Option<Duration>,
) -> Result<LuaRun, CompilerError> {
    crate::compiler_thp::configure_runtime_command(&mut cmd);
    cmd.arg("-")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = cmd.spawn().map_err(|error| CompilerError::new_no_location(format!(
        "Could not run LuaJIT ({}): {error}. Set ROCK_LUAJIT or install luajit beside rock or on PATH.", cmd.get_program().to_string_lossy()
    )))?;
    let pid = child.id();
    register_child_process(pid);

    // Drain both pipes on threads BEFORE waiting. Waiting first deadlocks as
    // soon as the child fills a pipe buffer, which is the ~50k-line-script hang
    // that can otherwise block a child before its pipes are drained.
    let mut stdin = child.stdin.take().expect("stdin piped");
    let mut stdout_pipe = child.stdout.take().expect("stdout piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr piped");
    // The read result is PROPAGATED, not dropped. Discarding it turned a failed
    // drain into empty output, which reads downstream as "the program printed
    // nothing" -- a harness failure wearing a successful program's clothes.
    let stdout_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut stdout_pipe, &mut buf).map(|_| buf)
    });
    let stderr_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut stderr_pipe, &mut buf).map(|_| buf)
    });

    // LuaJIT can report a load error and exit before the whole script has been
    // written. Keep that write error until the child status and stderr are
    // available so it cannot mask the load diagnostic.
    let write_error = {
        use std::io::Write;
        let result = stdin.write_all(code.as_bytes());
        drop(stdin);
        result.err()
    };

    // The status MUST come from whichever call reaped the child. `wait_timeout`
    // reaps it itself, so a following `wait`/`wait_with_output` cannot report the
    // real status -- it fabricates `exit(0)`. That is why this used to treat any
    // stderr output as failure: the stderr check was compensating for an exit
    // status that was always 0 here, which in turn is why `rock run` exited 0 on
    // a program that died.
    // On a wait error the child stays registered, so process cleanup can still
    // kill it; the reader threads are left to end with it.
    let process_error = |action: &str, error: std::io::Error| {
        CompilerError::new_no_location(format!("Failed to {action} LuaJIT process {pid}: {error}"))
    };
    let status = if let Some(timeout_duration) = timeout {
        match child
            .wait_timeout(timeout_duration)
            .map_err(|error| process_error("wait for", error))?
        {
            Some(status) => status,
            None => {
                child
                    .kill()
                    .map_err(|error| process_error("kill timed out", error))?;
                child
                    .wait()
                    .map_err(|error| process_error("wait for killed", error))?;
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
        child
            .wait()
            .map_err(|error| process_error("wait for", error))?
    };

    unregister_child_process(pid);

    let drained = |thread: std::thread::JoinHandle<std::io::Result<Vec<u8>>>, pipe: &str| {
        crate::process_management::join_drained_pipe(thread, pipe)
            .map_err(|e| CompilerError::new_no_location(format!("luajit run: {e}")))
    };
    let stdout = drained(stdout_thread, "stdout")?;
    let stderr = drained(stderr_thread, "stderr")?;

    if let Some(e) = write_error {
        // A child load/runtime failure is the useful result even when it
        // closed stdin before the host finished writing. A successful child
        // has no load/runtime failure that can replace the write failure.
        if status.success() {
            return Err(CompilerError::new_no_location(format!(
                "Failed to write test code to luajit process: {}.",
                e
            )));
        }
    }

    Ok(LuaRun {
        stdout: String::from_utf8_lossy(&stdout).to_string(),
        stderr: String::from_utf8_lossy(&stderr).to_string(),
        success: status.success(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_error_wins_over_stdin_broken_pipe() {
        let mut code = "local = 1\n".to_string();
        code.push_str(&"-- valid filler\n".repeat(1_000_000));

        let run = run_lua_code_with_timeout(&code, Some(test_utils::HANG_BACKSTOP))
            .expect("LuaJIT load errors are child results, not runner failures");

        assert!(
            !run.success,
            "invalid Lua unexpectedly loaded: {}",
            run.combined()
        );
        assert!(
            run.stderr.contains("expected near") || run.stderr.contains("unexpected symbol"),
            "LuaJIT's parse error was not reported: {}",
            run.combined()
        );
        assert!(
            !run.combined().contains("Broken pipe"),
            "the stdin write error masked LuaJIT's parse error: {}",
            run.combined()
        );
    }

    #[cfg(unix)]
    #[test]
    fn broken_pipe_without_a_child_error_is_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let child_path = dir.path().join("close_stdin");
        test_utils::write_executable(&child_path, "#!/bin/sh\nexec </dev/null\nsleep 1\n");

        let mut code = "ignored by child\n".to_string();
        code.push_str(&"-- valid filler\n".repeat(1_000_000));

        let error = run_lua_code_with_command(
            std::process::Command::new(&child_path),
            &code,
            Some(test_utils::HANG_BACKSTOP),
        )
        .expect_err("a child with no diagnostic must not swallow the broken pipe");

        assert!(
            error.message.contains("Broken pipe"),
            "the genuine write failure was not reported: {}",
            error.message
        );
    }
}
