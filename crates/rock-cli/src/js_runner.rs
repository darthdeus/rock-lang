//! In-memory node eval: pipe an emitted JS program into node's stdin, capture
//! stdout/stderr separately. The node sibling of `lua_runner` -- used by the
//! `--backend js` test executor. node is resolved from PATH; its absence is a
//! hard `CompilerError` (the caller asked for the JS backend explicitly).

use rock_core::CompilerError;
use rock_host::process_management::{register_child_process, unregister_child_process};
use std::time::Duration;
use wait_timeout::ChildExt;

/// One node run's outcome. Unlike `lua_runner`, stderr stays separate: the JS
/// executor's protocol lives on stdout and its error reports quote stderr
/// verbatim. There is no exit status: the harness catches every test, so the
/// stdout markers alone decide the outcome.
pub struct NodeRun {
    pub stdout: String,
    pub stderr: String,
}

pub fn run_node_code_with_timeout(
    code: &str,
    timeout: Option<Duration>,
) -> Result<NodeRun, CompilerError> {
    let mut cmd = std::process::Command::new("node");
    rock_host::compiler_thp::configure_runtime_command(&mut cmd);
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| {
        CompilerError::new_no_location(if e.kind() == std::io::ErrorKind::NotFound {
            "node not found on PATH (required for --backend js)".to_string()
        } else {
            format!("failed to spawn node: {e}")
        })
    })?;
    let pid = child.id();
    register_child_process(pid);

    {
        use std::io::Write;
        let stdin = child.stdin.as_mut().expect("node stdin is piped");
        if let Err(e) = stdin.write_all(code.as_bytes()) {
            let _ = child.kill();
            let _ = child.wait();
            unregister_child_process(pid);
            return Err(CompilerError::new_no_location(format!(
                "failed to write program to node process: {e}"
            )));
        }
    }
    // Close stdin so node sees EOF and executes the piped program.
    drop(child.stdin.take());

    let output = if let Some(timeout_duration) = timeout {
        match child
            .wait_timeout(timeout_duration)
            .expect("failed to wait for node process")
        {
            Some(_status) => child
                .wait_with_output()
                .expect("failed to read node output"),
            None => {
                child.kill().expect("failed to kill timed-out node process");
                child
                    .wait()
                    .expect("failed to wait for killed node process");
                unregister_child_process(pid);
                return Err(CompilerError::new_no_location(format!(
                    "node process timed out after {} seconds",
                    timeout_duration.as_secs_f32()
                )));
            }
        }
    } else {
        child
            .wait_with_output()
            .expect("failed to read node output")
    };

    unregister_child_process(pid);

    Ok(NodeRun {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// `rock run --backend js`: pipe the program to node's stdin but INHERIT
/// stdout/stderr (the program streams directly to the terminal) and wait with NO
/// timeout, returning node's exit code. The captured-output timeout variant
/// (`run_node_code_with_timeout`) stays test-only; a real program can run
/// arbitrarily long and must not be truncated at 15s.
pub fn run_node_code_streaming(code: &str) -> Result<i32, CompilerError> {
    let mut cmd = std::process::Command::new("node");
    rock_host::compiler_thp::configure_runtime_command(&mut cmd);
    let mut child = cmd
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .map_err(|e| {
            CompilerError::new_no_location(if e.kind() == std::io::ErrorKind::NotFound {
                "node not found on PATH (required for --backend js)".to_string()
            } else {
                format!("failed to spawn node: {e}")
            })
        })?;
    let pid = child.id();
    register_child_process(pid);

    {
        use std::io::Write;
        let stdin = child.stdin.as_mut().expect("node stdin is piped");
        if let Err(e) = stdin.write_all(code.as_bytes()) {
            let _ = child.kill();
            let _ = child.wait();
            unregister_child_process(pid);
            return Err(CompilerError::new_no_location(format!(
                "failed to write program to node process: {e}"
            )));
        }
    }
    drop(child.stdin.take());

    let status = child.wait().map_err(|e| {
        CompilerError::new_no_location(format!("failed to wait for node process: {e}"))
    })?;
    unregister_child_process(pid);
    Ok(status.code().unwrap_or(1))
}
