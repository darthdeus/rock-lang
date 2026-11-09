use std::time::Duration;

/// Run an emitted program to completion. A real program may run as long as it
/// needs; only test harnesses impose a timeout.
pub fn run_lua_code(
    code: &str,
) -> std::result::Result<rock_host::lua_runner::LuaRun, rock_core::CompilerError> {
    run_lua_code_with_timeout(code, None)
}

pub fn run_lua_code_with_timeout(
    code: &str,
    timeout: Option<Duration>,
) -> std::result::Result<rock_host::lua_runner::LuaRun, rock_core::CompilerError> {
    let command = rock_host::project::build_luajit_stdin_command();
    let command = crate::runtime_process::configure(
        command,
        crate::runtime_process::RuntimeProcess::LuaStdin,
    );
    rock_host::lua_runner::run_lua_code_with_command(command, code, timeout)
}

use crate::skull_backend::GameProgram;
use anyhow::Result;

/// Run an ordinary emitted program through the captured stdin evaluator.
pub fn run_lua_program(preamble: &str, lua: &GameProgram, emits_main: bool) -> Result<()> {
    // The captured stdin evaluator requires contiguous input.
    let program = crate::skull_backend::skull_game_main(preamble, lua, emits_main).concat();
    match run_lua_code(&program) {
        Ok(run) => {
            print!("{}", run.stdout);
            if !run.stderr.is_empty() {
                eprint!("{}", run.stderr);
            }
            // The program's own exit status is the answer. Printing a crash and
            // returning Ok is how `rock run` used to exit 0 on a dead program.
            if !run.success {
                log::error!(target: "SKULL", "program exited with a runtime error");
                crate::invocation_log::exit(1);
            }
            Ok(())
        }
        Err(e) => {
            log::error!(target: "SKULL", "runtime error: {}", e.message);
            crate::invocation_log::exit(1);
        }
    }
}
