//! Stone runtime preparation for the public compiler.

use crate::skull_backend::{GameProgram, write_full_main};
use anyhow::Result;
use rock_engine_host::project::configured_stone_dir;
use std::path::{Path, PathBuf};

/// Checked native FFI libraries, independent of package names and aliases.
pub struct TestProgramFacts {
    pub ffi_libraries: Vec<String>,
}

pub enum TestUnit<'a> {
    File(&'a std::path::Path),
    Package {
        dir: &'a str,
        facts: &'a TestProgramFacts,
    },
}

/// Runtime text surrounding the emitted definitions. Native staging is done
/// by `program_host` before resolving the preamble, preserving failure order.
pub struct ProgramHost {
    pub preamble: String,
    pub setup: &'static str,
}

#[derive(Debug)]
pub struct RuntimeFailure {
    pub reason: &'static str,
    pub rendered: String,
}

/// Ordinary compile omits runtime globals for non-games. Game-only builders
/// (frame, sim and watch) always retain them, including their cache identity.
pub enum ProgramKind {
    Project,
    Game,
}

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
    // Project compilation omits game globals for non-game projects.
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

pub(crate) fn run(
    dir: &str,
    app_args: &[String],
    compile: impl FnOnce() -> Result<(GameProgram, bool)>,
) -> Result<()> {
    if !rock_engine_host::project::is_game_project(Path::new(dir)).unwrap_or(false) {
        for lib in rock_engine_host::engine_build::game_engine_libs(dir)? {
            rock_engine_host::engine_build::build_optional_engine_lib(&lib)?;
        }
        let (lua, emits_main) = compile()?;
        return crate::compile_cmd::run_lua_program(
            &program_preamble(dir, ProgramKind::Project)?,
            &lua,
            emits_main,
        );
    }
    // The compiler may read the generated sound bank as a build-time input.
    rock_host::timed("sound bank", || crate::package::prepare_sound_bank(dir))?;
    let compile = || rock_host::timed("compile", compile);
    let (lua, emits_main) = if std::env::var("ROCK_SKIP_ENGINE_BUILD").as_deref() == Ok("1") {
        compile()?
    } else {
        std::thread::scope(|scope| {
            let engine = scope.spawn(|| {
                rock_host::timed("engine libraries", || {
                    rock_engine_host::engine_build::build_engine_libs(dir)
                })
                .map_err(|error| anyhow::anyhow!("engine build failed: {error:#}"))
            });
            let compiled = compile()?;
            engine
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic))?;
            Ok::<_, anyhow::Error>(compiled)
        })?
    };
    // A fresh baseline must not reload an overlay from an earlier session.
    let lua_file = rock_engine_host::project::stone_session_main_lua_path(dir, std::process::id());
    if let Err(e) = write_full_main(
        &program_preamble(dir, ProgramKind::Game)?,
        &lua,
        emits_main,
        &lua_file,
        &lua_file.with_file_name("incremental.lua"),
    ) {
        log::error!(target: "SKULL", "cannot write {}: {}", lua_file.display(), e);
        crate::invocation_log::exit(1);
    }
    exec_luajit_file(&lua_file, app_args)
}

/// Exec luajit on a compiled `.lua` file with stdio inherited from the
/// caller, preserving keyboard input for interactive games.
fn exec_luajit_file(lua_file: &Path, app_args: &[String]) -> Result<()> {
    let mut cmd = rock_host::project::luajit_command();
    cmd.arg(lua_file).args(app_args);
    cmd = crate::runtime_process::configure(
        cmd,
        crate::runtime_process::RuntimeProcess::LuaRun { headless: false },
    );
    cmd.stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());

    // The CLI goes silent here until Stone's engine init prints. Loading
    // luajit, JIT-warming main.lua and ffi.load-ing native libs can take
    // seconds, so the launch is logged to tell a slow handoff from a hang.
    let spawn_t = bone_core::prelude::Instant::now();
    log::info!(target: "RUN", "launching luajit -> {}", lua_file.display());

    let mut child = crate::runtime_process::spawn_game(&mut cmd, lua_file.parent().unwrap())
        .map_err(|e| anyhow::anyhow!("failed to exec luajit: {}", e))?;
    log::info!(target: "RUN",
        "luajit pid {} spawned in {:.0}ms; waiting for game",
        child.id(),
        spawn_t.elapsed().as_secs_f64() * 1000.0
    );

    // No timeout: games run an infinite frame loop.
    let status = child
        .wait()
        .map_err(|e| anyhow::anyhow!("wait failed: {}", e))?;
    crate::runtime_process::exit_after_game_interrupt(None)?;
    if !status.success() {
        crate::invocation_log::exit(status.code().unwrap_or(1));
    }
    Ok(())
}
