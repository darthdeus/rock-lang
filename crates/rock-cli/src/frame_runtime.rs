use crate::Result;
use anyhow::Context as _;
use std::path::PathBuf;

// Keep this directory alive until the child exits. The requested output directory
// may also be targeted by a compiler, watcher or another bounded invocation.
pub(crate) fn compile_frame_invocation(
    path: &str,
    output: &str,
) -> Result<(tempfile::TempDir, PathBuf)> {
    std::fs::create_dir_all(output)?;
    let invocation = tempfile::Builder::new()
        .prefix("frame-run-")
        .tempdir_in(output)?;
    let project = std::path::Path::new(path)
        .canonicalize()
        .map_err(|error| anyhow::anyhow!("canonicalize game {path}: {error}"))?;
    let project = project.to_string_lossy();
    let lua_file = invocation
        .path()
        .join(rock_host::project::MAIN_LUA_FILENAME);
    let engine_project = rock_host::timed("resolve project", || {
        rock_engine_host::project::resolve_project_dir(&project)
    })?;
    let engine_project = engine_project.to_string_lossy();
    rock_host::timed("sound bank", || {
        crate::package::prepare_sound_bank(&engine_project)
    })?;
    beside(
        || {
            rock_host::timed("engine libraries", || {
                rock_engine_host::engine_build::build_engine_libs(&engine_project)
            })
        },
        || {
            let compiler = rock_host::timed("compile", || {
                crate::skull_backend::IncrementalProject::init(
                    &project,
                    &lua_file,
                    &lua_file.with_file_name("incremental.lua"),
                )
            })?;
            crate::allocator::drop_and_release_in_background(compiler);
            Ok(())
        },
    )?;
    // Profiling tools inspect this diagnostic copy after the run. Execution uses
    // only the private file, so another invocation can safely replace this copy.
    rock_host::timed("diagnostic copy", || {
        diagnostic_copy(
            &lua_file,
            &PathBuf::from(output).join(rock_host::project::MAIN_LUA_FILENAME),
        )
    })?;
    Ok((invocation, lua_file))
}

/// Install `source` as `destination` and wipe the overlay beside it, as a full
/// compile does.
fn diagnostic_copy(source: &std::path::Path, destination: &std::path::Path) -> Result<()> {
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    rock_host::project::write_atomic_streamed(destination, |file| {
        std::io::copy(&mut std::fs::File::open(source)?, file)?;
        Ok(())
    })
    .with_context(|| format!("copy frame artifact to {}", destination.display()))?;
    crate::skull_backend::wipe_overlay(&destination.with_file_name("incremental.lua"))
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
                    .recv_timeout(test_utils::HANG_BACKSTOP)
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
}
