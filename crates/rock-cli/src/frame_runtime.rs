use crate::{Backend, Result};
use std::path::PathBuf;

pub(crate) struct CompiledGame {
    pub lua_file: std::path::PathBuf,
    pub artifact_fingerprint: String,
}

pub(crate) fn compile_game_for_run(
    backend: Backend,
    path: &str,
    output: &str,
) -> Result<CompiledGame> {
    let artifact = with_game_prepared(path, || {
        crate::game_compile::compile_game(backend, path, std::path::Path::new(output))
    })?;
    Ok(CompiledGame {
        lua_file: PathBuf::from(output).join(rock_host::project::MAIN_LUA_FILENAME),
        artifact_fingerprint: artifact.artifact_fingerprint,
    })
}

// Keep this directory alive until the child exits. The requested output directory
// may also be targeted by a compiler, watcher or another bounded invocation.
pub(crate) fn compile_frame_invocation(
    backend: Backend,
    path: &str,
    output: &str,
    frames: usize,
) -> Result<(tempfile::TempDir, CompiledGame)> {
    std::fs::create_dir_all(output)?;
    let invocation = tempfile::Builder::new()
        .prefix("frame-run-")
        .tempdir_in(output)?;
    let compiled = crate::frame_compile_cache::compile_game_for_frame(
        backend,
        path,
        &invocation.path().to_string_lossy(),
        frames,
    )?;
    // Profiling tools inspect this diagnostic copy after the run. Execution uses
    // only the private file, so another invocation can safely replace this copy.
    rock_host::timed("diagnostic copy", || {
        crate::frame_compile_cache::materialize(
            &compiled.lua_file,
            &PathBuf::from(output).join(rock_host::project::MAIN_LUA_FILENAME),
        )
    })?;
    Ok((invocation, compiled))
}

/// Runs `compile` with what a run compile needs: project resolution and the
/// sound bank first, then the engine libraries beside the compile. The compile
/// does not read the libraries, and they are built before the program starts.
pub(crate) fn with_game_prepared<T>(path: &str, compile: impl FnOnce() -> Result<T>) -> Result<T> {
    let services = crate::registry::RuntimeServices::registered();
    let project = rock_host::timed("resolve project", || (services.resolve_project_dir)(path))?;
    let project = project.to_string_lossy();
    rock_host::timed("sound bank", || (services.prepare_sound_bank)(&project))?;
    beside(
        || {
            rock_host::timed("engine libraries", || {
                (services.build_engine_libs)(&project)
            })
        },
        compile,
    )
}

/// Runs `prepare` beside `compile` and returns the compile's result once
/// both are done. `prepare`'s error wins, as when it ran first.
pub(crate) fn beside<T>(
    prepare: impl FnOnce() -> Result<()> + Send,
    compile: impl FnOnce() -> Result<T>,
) -> Result<T> {
    std::thread::scope(|scope| {
        let prepared = scope.spawn(prepare);
        let compiled = compile();
        match prepared.join() {
            Ok(prepared) => prepared?,
            Err(panic) => std::panic::resume_unwind(panic),
        }
        compiled
    })
}

/// Disable device audio for bounded headless frame execution unless explicitly selected.
pub(crate) fn apply_headless_audio_env(cmd: &mut std::process::Command) {
    if std::env::var_os("STONE_AUDIO_STUB").is_none() {
        cmd.env("STONE_AUDIO_STUB", "1");
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FrameProgress {
    pub renderer: String,
    pub completed: usize,
    pub requested: Option<usize>,
}

pub(crate) fn parse_frame_progress(line: &[u8]) -> Option<FrameProgress> {
    let line = std::str::from_utf8(line).ok()?;
    let fields = line
        .split_once("ROCK_FRAME_PROGRESS ")?
        .1
        .split_whitespace();
    let mut renderer = None;
    let mut completed = None;
    let mut requested = None;
    for field in fields {
        let (name, value) = field.split_once('=')?;
        match name {
            "renderer" => renderer = Some(value.to_owned()),
            "completed" => completed = value.parse().ok(),
            "requested" => {
                requested = match value {
                    "-" | "-1" => Some(None),
                    _ => Some(Some(value.parse().ok()?)),
                }
            }
            _ => {}
        }
    }
    Some(FrameProgress {
        renderer: renderer?,
        completed: completed?,
        requested: requested?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn frame_progress_parser_keeps_renderer_and_denominator() {
        assert_eq!(
            parse_frame_progress(
                b"[WATCHDOG] ROCK_FRAME_PROGRESS renderer=gl completed=17 requested=600\n"
            ),
            Some(FrameProgress {
                renderer: "gl".into(),
                completed: 17,
                requested: Some(600),
            })
        );
        assert_eq!(
            parse_frame_progress(b"ROCK_FRAME_PROGRESS renderer=gl completed=4 requested=-1\n"),
            Some(FrameProgress {
                renderer: "gl".into(),
                completed: 4,
                requested: None,
            })
        );
    }

    #[test]
    fn preparation_runs_beside_the_compile() {
        let (started, compile_started) = std::sync::mpsc::channel();
        let compiled = beside(
            move || {
                compile_started
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .map_err(|_| anyhow::anyhow!("the compile waited for the preparation"))
            },
            || {
                started.send(()).unwrap();
                Ok(7)
            },
        );
        assert_eq!(compiled.unwrap(), 7);
    }

    #[test]
    fn a_failed_preparation_wins_over_a_failed_compile() {
        let error = beside(
            || anyhow::bail!("native build failed"),
            || -> Result<()> { anyhow::bail!("type error") },
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "native build failed");
    }

    #[test]
    fn frame_progress_parser_rejects_incomplete_markers() {
        assert_eq!(
            parse_frame_progress(b"ROCK_FRAME_PROGRESS renderer=gl completed=17\n"),
            None
        );
        assert_eq!(parse_frame_progress(b"ordinary game output\n"), None);
    }
}
