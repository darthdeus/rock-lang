//! In-memory node eval: pipe an emitted JS program into node's stdin. The node
//! sibling of `lua_runner`, used by the `--backend js` executor. node is
//! resolved from PATH; its absence is a hard `CompilerError` (the caller asked
//! for the JS backend explicitly).

use rock_core::CompilerError;
use rock_host::process_management::{register_child_process, unregister_child_process};
use std::process::{Child, Stdio};
use std::time::Duration;

/// One node run's outcome. Unlike `lua_runner`, stderr stays separate: the JS
/// executor's protocol lives on stdout and its error reports quote stderr
/// verbatim. There is no exit status: the harness catches every test, so the
/// stdout markers alone decide the outcome.
pub(crate) struct NodeRun {
    pub stdout: String,
    pub stderr: String,
}

fn error(message: String) -> CompilerError {
    CompilerError::new_no_location(message)
}

/// Spawn node with `output` stdio, feed it `code` and close stdin so node runs
/// the program. The child is registered for cleanup until the caller reaps it.
fn spawn_node(code: &str, output: fn() -> Stdio) -> Result<Child, CompilerError> {
    let mut cmd = std::process::Command::new("node");
    rock_host::compiler_thp::configure_runtime_command(&mut cmd);
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(output())
        .stderr(output())
        .spawn()
        .map_err(|e| {
            error(if e.kind() == std::io::ErrorKind::NotFound {
                "node not found on PATH (required for --backend js)".to_string()
            } else {
                format!("failed to spawn node: {e}")
            })
        })?;
    register_child_process(child.id());
    let mut stdin = child.stdin.take().expect("node stdin is piped");
    // node reads the whole program before it runs any of it, so the write
    // completes before node can fill an output pipe.
    if let Err(e) = std::io::Write::write_all(&mut stdin, code.as_bytes()) {
        let _ = child.kill();
        let _ = child.wait();
        unregister_child_process(child.id());
        return Err(error(format!(
            "failed to write program to node process: {e}"
        )));
    }
    Ok(child)
}

pub(crate) fn run_node_code_with_timeout(
    code: &str,
    timeout: Duration,
) -> Result<NodeRun, CompilerError> {
    let mut child = spawn_node(code, Stdio::piped)?;
    let pid = child.id();
    let waited = crate::common::ProcessManager::wait_with_cpu_budget(&mut child, timeout)
        .map_err(|e| error(format!("failed to wait for node process: {e}")));
    if waited.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    unregister_child_process(pid);
    let run = waited?;
    if run.timed_out {
        return Err(error(format!(
            "node process timed out after {} seconds",
            timeout.as_secs_f32()
        )));
    }
    Ok(NodeRun {
        stdout: String::from_utf8_lossy(&run.output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&run.output.stderr).into_owned(),
    })
}

/// `rock run --backend js`: the program streams to the terminal and runs with
/// no timeout, returning node's exit code.
pub(crate) fn run_node_code_streaming(code: &str) -> Result<i32, CompilerError> {
    let mut child = spawn_node(code, Stdio::inherit)?;
    let status = child.wait();
    unregister_child_process(child.id());
    let status = status.map_err(|e| error(format!("failed to wait for node process: {e}")))?;
    Ok(status.code().unwrap_or(1))
}

#[cfg(test)]
mod tests {}
