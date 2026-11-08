use crate::{Backend, Result};
use std::path::PathBuf;

pub struct CompiledGame {
    pub lua_file: std::path::PathBuf,
    pub artifact_fingerprint: String,
}

pub fn compile_game_for_run(backend: Backend, path: &str, output: &str) -> Result<CompiledGame> {
    prepare_game_compile(path)?;
    let artifact = crate::game_compile::compile_game(backend, path, std::path::Path::new(output))?;
    Ok(CompiledGame {
        lua_file: PathBuf::from(output).join(rock_host::project::MAIN_LUA_FILENAME),
        artifact_fingerprint: artifact.artifact_fingerprint,
    })
}

// Keep this directory alive until the child exits. The requested output directory
// may also be targeted by a compiler, watcher or another bounded invocation.
pub fn compile_frame_invocation(
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
    rock_host::project::write_atomic_streamed(
        PathBuf::from(output).join(rock_host::project::MAIN_LUA_FILENAME),
        |file| {
            std::io::copy(&mut std::fs::File::open(&compiled.lua_file)?, file)?;
            Ok(())
        },
    )?;
    Ok((invocation, compiled))
}

/// Everything a run compile needs before the compiler: project resolution,
/// renderer feature, sound bank and engine libs.
pub fn prepare_game_compile(path: &str) -> Result<()> {
    let project = (crate::registry::RuntimeServices::registered().resolve_project_dir)(path)?;
    let path = project.to_string_lossy();
    let path = path.as_ref();
    (crate::registry::RuntimeServices::registered().prepare_sound_bank)(path)?;
    (crate::registry::RuntimeServices::registered().build_engine_libs)(path)?;
    Ok(())
}

/// Disable device audio for bounded headless frame execution unless explicitly selected.
pub fn apply_headless_audio_env(cmd: &mut std::process::Command) {
    if std::env::var_os("STONE_AUDIO_STUB").is_none() {
        cmd.env("STONE_AUDIO_STUB", "1");
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameProgress {
    pub renderer: String,
    pub completed: usize,
    pub requested: Option<usize>,
}

pub fn parse_frame_progress(line: &[u8]) -> Option<FrameProgress> {
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
    fn frame_progress_parser_rejects_incomplete_markers() {
        assert_eq!(
            parse_frame_progress(b"ROCK_FRAME_PROGRESS renderer=gl completed=17\n"),
            None
        );
        assert_eq!(parse_frame_progress(b"ordinary game output\n"), None);
    }
}
