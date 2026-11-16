use super::game_runtime::GameCompiler;
use crate::{Backend, Result};
use std::path::Path;

/// What a from-scratch compile published beside its `main.lua`.
pub(crate) struct GameArtifact {
    pub artifact_fingerprint: String,
}

pub(crate) fn compile_game(
    backend: Backend,
    path: &str,
    output_dir: &Path,
) -> Result<GameArtifact> {
    use anyhow::Context as _;
    debug_assert_eq!(backend, Backend::Skull);
    std::fs::create_dir_all(output_dir)
        .with_context(|| format!("create {}", output_dir.display()))?;
    let output_path = output_dir.join(rock_host::project::MAIN_LUA_FILENAME);
    let incremental_path = output_path.with_file_name("incremental.lua");
    let mut compiler = GameCompiler::new(path, output_path, incremental_path);
    // One-shot from-scratch compile: always the init path, so the outcome
    // can never request a shape restart (there is no live process).
    rock_host::timed("compile", || {
        compiler.compile(&std::collections::HashSet::new(), true)
    })?;
    let artifact = GameArtifact {
        artifact_fingerprint: compiler.artifact_fingerprint()?,
    };
    crate::allocator::drop_and_release_in_background(compiler);
    Ok(artifact)
}
