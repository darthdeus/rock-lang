//! Stone runtime preparation for the public compiler.

use crate::registry::{ProgramHost, ProgramKind, RuntimeFailure, TestUnit};
use crate::skull_backend::{GameProgram, write_full_main};
use anyhow::Result;
use rock_engine_host::project::configured_stone_dir;
use std::path::{Path, PathBuf};

pub(crate) fn prepare_unit(path: &str) -> anyhow::Result<()> {
    if let Some(project) = rock_host::project::find_project_root(std::path::Path::new(path)) {
        let project = project
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("non-UTF-8 test project path"))?;
        crate::package::prepare_sound_bank(project)?;
    }
    Ok(())
}

pub(crate) fn program_host(unit: TestUnit<'_>) -> std::result::Result<ProgramHost, RuntimeFailure> {
    let path = match &unit {
        TestUnit::File(file) => file
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| std::path::Path::new(".")),
        TestUnit::Package { dir, .. } => std::path::Path::new(dir),
    };
    let stone_dir = configured_stone_dir(path).map_err(|error| RuntimeFailure {
        reason: "Stone root resolution failed",
        rendered: error.to_string(),
    })?;
    let TestUnit::Package { dir, facts } = unit else {
        // Single files only receive their configured asset root, as before.
        return Ok(ProgramHost {
            preamble: stone_dir
                .as_deref()
                .map(stone_root_preamble)
                .unwrap_or_default(),
            setup: "",
        });
    };
    let is_game =
        rock_engine_host::project::is_game_project(path).map_err(|error| RuntimeFailure {
            reason: "test host resolution failed",
            rendered: error.to_string(),
        })?;
    let needs_native_build = is_game
        || stone_dir.is_some()
        || facts.ffi_libraries.iter().any(|library| library == "stone");
    if needs_native_build {
        rock_engine_host::engine_build::build_libstone().map_err(|error| RuntimeFailure {
            reason: "engine library staging failed",
            rendered: format!(
                "`rock test {dir}` could not stage libstone for the test process: {error:#}"
            ),
        })?;
    }
    for lib in
        rock_engine_host::engine_build::game_engine_libs(dir).map_err(|error| RuntimeFailure {
            reason: "native library resolution failed",
            rendered: error.to_string(),
        })?
    {
        if facts
            .ffi_libraries
            .iter()
            .any(|library| library == &lib.dst_stem)
        {
            rock_engine_host::engine_build::build_optional_engine_lib(&lib).map_err(|error| {
                RuntimeFailure {
                    reason: "engine library staging failed",
                    rendered: format!(
                        "`rock test {dir}` could not stage {} for the test process: {error:#}",
                        lib.dst_stem,
                    ),
                }
            })?;
        }
    }
    let mut preamble = if is_game {
        program_preamble(dir, ProgramKind::Game).map_err(|error| RuntimeFailure {
            reason: "game preamble resolution failed",
            rendered: error.to_string(),
        })?
    } else {
        stone_dir
            .as_deref()
            .map(stone_root_preamble)
            .unwrap_or_default()
    };
    if !is_game {
        preamble.push_str(
            &rock_engine_host::native_libraries::native_preamble(dir).map_err(|error| {
                RuntimeFailure {
                    reason: "native library preamble failed",
                    rendered: error.to_string(),
                }
            })?,
        );
    }
    // Full Stone dependencies start the CPU host after definitions, before tests.
    let setup = if stone_dir.is_some() {
        "\nsetup_stone()\n"
    } else {
        ""
    };
    Ok(ProgramHost { preamble, setup })
}

fn stone_root_preamble(stone_dir: &std::path::Path) -> String {
    let escaped = stone_dir
        .display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    format!("__ROCK_STONE_DIR = \"{escaped}\"\n")
}

/// Resolve ordinary project and game-only builder preambles in one place.
pub fn program_preamble(dir: &str, kind: ProgramKind) -> Result<String> {
    // Preserve ordinary compile's empty non-game preamble. Game-only builders
    // historically inject the globals even for plain fixtures, as do their keys.
    if matches!(kind, ProgramKind::Project)
        && !rock_engine_host::project::is_game_project(std::path::Path::new(dir)).unwrap_or(false)
    {
        return rock_engine_host::native_libraries::native_preamble(dir);
    }
    let abs = PathBuf::from(dir)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(dir));
    let escaped = abs
        .display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    let config = rock_host::stick::StickConfig::find_from_dir(&abs)?.map(|(_, config)| config);
    let overlay_defaults = rock_engine_host::release::configured_asset_overlays_from(
        &abs,
        config.as_ref(),
        std::env::var_os(rock_engine_host::release::ASSET_OVERLAYS_ENV).as_deref(),
    )?;
    let mut full = rock_engine_host::native_libraries::native_preamble(dir)?;
    full.push_str(&format!("__ROCK_GAME_SOURCE_DIR = \"{escaped}\"\n"));
    if let Some(stone_dir) = configured_stone_dir(&abs)? {
        full.push_str(&stone_root_preamble(&stone_dir));
    }
    full.push_str("__ROCK_ASSET_OVERLAY_DEFAULTS = {");
    for (index, path) in overlay_defaults.iter().enumerate() {
        if index > 0 {
            full.push_str(", ");
        }
        let path = path
            .display()
            .to_string()
            .replace('\\', "\\\\")
            .replace('"', "\\\"");
        full.push('"');
        full.push_str(&path);
        full.push('"');
    }
    full.push_str("}\n");
    Ok(full)
}

/// Sound assets must be ready before the compiler evaluates build-time inputs.
pub(crate) fn prepare_run(dir: &str) -> Result<()> {
    if rock_engine_host::project::is_game_project(Path::new(dir)).unwrap_or(false) {
        crate::package::prepare_sound_bank(dir)?;
    } else {
        for lib in rock_engine_host::engine_build::game_engine_libs(dir)? {
            rock_engine_host::engine_build::build_optional_engine_lib(&lib)?;
        }
    }
    Ok(())
}

/// Select the runtime recipe after the public compiler has emitted definitions.
pub fn launch(dir: &str, lua: &GameProgram, emits_main: bool) -> Result<()> {
    if !rock_engine_host::project::is_game_project(Path::new(dir)).unwrap_or(false) {
        return crate::compile_cmd::run_lua_program(
            &program_preamble(dir, ProgramKind::Project)?,
            lua,
            emits_main,
        );
    }
    if !std::env::var("ROCK_SKIP_ENGINE_BUILD").is_ok_and(|v| v == "1") {
        if let Err(e) = rock_engine_host::engine_build::build_engine_libs(dir) {
            log::error!(target: "SKULL", "engine build failed: {}", e);
            crate::invocation_log::exit(1);
        }
    }
    // A fresh baseline must not reload an overlay from an earlier session.
    let lua_file = rock_engine_host::project::stone_session_main_lua_path(dir, std::process::id());
    if let Err(e) = write_full_main(
        &program_preamble(dir, ProgramKind::Game)?,
        lua,
        emits_main,
        &lua_file,
        &lua_file.with_file_name("incremental.lua"),
    ) {
        log::error!(target: "SKULL", "cannot write {}: {}", lua_file.display(), e);
        crate::invocation_log::exit(1);
    }
    // No timeout: games run an infinite frame loop.
    exec_luajit_file(&lua_file, None, &[])
}

/// Exec luajit on a compiled `.lua` file with stdio inherited from the
/// caller, preserving keyboard input for interactive games.
fn exec_luajit_file(
    lua_file: &Path,
    timeout: Option<std::time::Duration>,
    app_args: &[String],
) -> Result<()> {
    let mut cmd = rock_host::project::build_luajit_command(lua_file, app_args);
    cmd = crate::runtime_process::configure(
        cmd,
        crate::runtime_process::RuntimeProcess::LuaRun { headless: false },
    );
    cmd.stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());

    // The CLI goes silent here: the next output is whatever the game
    // prints once Stone's engine init runs. luajit loading +
    // JIT-warming the generated main.lua and ffi.load-ing native libs
    // can take seconds with no signal, making a slow/failed handoff
    // look like a hang. Mark the boundary so it's attributable.
    let spawn_t = bone_core::prelude::Instant::now();
    log::info!(target: "RUN", "launching luajit -> {}", lua_file.display());

    let mut child = crate::child_lifetime::spawn_bound_child(&mut cmd)
        .map_err(|e| anyhow::anyhow!("failed to exec luajit: {}", e))?;
    log::info!(target: "RUN",
        "luajit pid {} spawned in {:.0}ms; waiting for game",
        child.id(),
        spawn_t.elapsed().as_secs_f64() * 1000.0
    );
    let pid = child.id();

    let status = if let Some(t) = timeout {
        use wait_timeout::ChildExt;
        match child
            .wait_timeout(t)
            .map_err(|e| anyhow::anyhow!("wait_timeout failed: {}", e))?
        {
            Some(s) => s,
            None => {
                let _ = child.kill();
                let _ = child.wait();
                crate::child_lifetime::release_bound_child(pid);
                anyhow::bail!(
                    "Luajit process timed out after {} seconds.",
                    t.as_secs_f32()
                );
            }
        }
    } else {
        child
            .wait()
            .map_err(|e| anyhow::anyhow!("wait failed: {}", e))?
    };

    crate::child_lifetime::release_bound_child(pid);
    if !status.success() {
        crate::invocation_log::exit(status.code().unwrap_or(1));
    }
    Ok(())
}
