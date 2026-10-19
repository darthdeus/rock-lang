use std::time::Duration;

pub fn run_lua_code(code: &str) -> std::result::Result<rock_host::lua_runner::LuaRun, rock_core::CompilerError> {
    run_lua_code_with_timeout(code, Some(Duration::from_millis(1800)))
}

pub fn run_lua_code_with_timeout(code: &str, timeout: Option<Duration>) -> std::result::Result<rock_host::lua_runner::LuaRun, rock_core::CompilerError> {
    let command = rock_host::project::build_luajit_stdin_command();
    let command = (crate::registry::RuntimeServices::registered().runtime_command)(command, crate::registry::RuntimeProcess::LuaStdin);
    rock_host::lua_runner::run_lua_code_with_command(command, code, timeout)
}

use anyhow::Result;
use crate::skull_backend::GameProgram;

/// Run an ordinary emitted program through the captured stdin evaluator.
pub fn run_lua_program(lua: &GameProgram, emits_main: bool) -> Result<()> {
    // The captured stdin evaluator requires contiguous input.
    let program = crate::skull_backend::skull_game_main("", lua, emits_main).concat();
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
                crate::ablation_report::exit_with_census(1);
            }
            Ok(())
        }
        Err(e) => {
            log::error!(target: "SKULL", "runtime error: {}", e.message);
            crate::ablation_report::exit_with_census(1);
        }
    }
}
