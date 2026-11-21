//! In-memory evaluation captures output; interactive execution needs inherited stdio.

use crate::process_management::{register_child_process, unregister_child_process};
use rock_core::CompilerError;
use std::io::{self, Read, Write};
use std::time::Duration;
use wait_timeout::ChildExt;

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
    let mut command = crate::project::luajit_command();
    command.envs(env.iter().copied());
    run_lua_code_with_command(command, code, timeout)
}

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

    // Drain both pipes before waiting or a full pipe can deadlock the child.
    let mut stdin = child.stdin.take().expect("stdin piped");
    let stdout_thread = drain_pipe(child.stdout.take().expect("stdout piped"));
    let stderr_thread = drain_pipe(child.stderr.take().expect("stderr piped"));

    // A premature exit can break stdin; preserve the child diagnostic first.
    let write_error = stdin.write_all(code.as_bytes()).err();
    drop(stdin);

    // `wait_timeout` reaps the child itself: take the status from it, since a later
    // `wait` would fabricate exit(0). On a wait error the child stays registered so
    // process cleanup can still kill it.
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

    // A child failure takes precedence over the resulting stdin error.
    if let Some(error) = write_error.filter(|_| status.success()) {
        return Err(CompilerError::new_no_location(format!(
            "Failed to write test code to luajit process: {error}."
        )));
    }

    Ok(LuaRun {
        stdout: String::from_utf8_lossy(&stdout).to_string(),
        stderr: String::from_utf8_lossy(&stderr).to_string(),
        success: status.success(),
    })
}

fn drain_pipe(
    mut pipe: impl Read + Send + 'static,
) -> std::thread::JoinHandle<io::Result<Vec<u8>>> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        pipe.read_to_end(&mut bytes)?;
        Ok(bytes)
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
