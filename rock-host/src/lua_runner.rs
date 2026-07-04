//! In-memory luajit eval: pipe Rock-compiled Lua into luajit's stdin, capture
//! combined stdout/stderr. Used by the Rust test suite and the `@test`-runner —
//! callers that compile a snippet and want its output as a String. NOT for
//! `rock run`/`rock stone` (those exec the compiled `.lua` file with inherited
//! stdio so interactive apps reach the tty).

use crate::process_management::{register_child_process, unregister_child_process};
use rock_core::CompilerError;
use std::time::Duration;
use wait_timeout::ChildExt;

pub fn run_lua_code_with_timeout(
    code: &str,
    timeout: Option<Duration>,
) -> Result<String, CompilerError> {
    let luajit_path = crate::project::luajit_path();
    let luajit_path_str = luajit_path.to_string_lossy().into_owned();

    if !luajit_path.exists() {
        return Err(CompilerError::new_no_location(format!(
            "LuaJIT not found at: {}. Make sure prebuilt gamelibs are installed or ROCK_DIR is set.",
            luajit_path_str
        )));
    }

    let mut cmd = std::process::Command::new(&luajit_path_str);
    cmd.env("LUA_PATH", crate::project::lua_path_value())
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

    let output = if let Some(timeout_duration) = timeout {
        match child
            .wait_timeout(timeout_duration)
            .expect("Failed to wait for process")
        {
            Some(_status) => child.wait_with_output().expect("Failed to read output"),
            None => {
                child.kill().expect("Failed to kill timed out process");
                child.wait().expect("Failed to wait for killed process");
                unregister_child_process(pid);
                return Err(CompilerError::new_no_location(format!(
                    "Luajit process timed out after {} seconds.",
                    timeout_duration.as_secs_f32()
                )));
            }
        }
    } else {
        child.wait_with_output().expect("Failed to read output")
    };

    unregister_child_process(pid);

    let mut result = String::from_utf8_lossy(&output.stdout).to_string();
    if !output.status.success() || !output.stderr.is_empty() {
        result += &format!("STDERR: {}", String::from_utf8_lossy(&output.stderr));
    }
    Ok(result)
}

pub fn run_lua_code(code: &str) -> Result<String, CompilerError> {
    run_lua_code_with_timeout(code, Some(Duration::from_millis(1800)))
}
