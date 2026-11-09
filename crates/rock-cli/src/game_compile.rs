use crate::{Backend, Result};
use std::path::{Path, PathBuf};

/// What a from-scratch compile published beside its `main.lua`.
pub struct GameArtifact {
    pub artifact_fingerprint: String,
    /// Comptime file inputs (`embed_string`) baked into the program.
    pub embedded_files: Vec<PathBuf>,
    /// The `[build.vars]` selection the compile stamped, when the backend reports it.
    pub comptime_inputs: Option<bone_semantic::inference::ComptimeInputs>,
}

pub(crate) fn compile_game(
    backend: Backend,
    path: &str,
    output_dir: &Path,
) -> Result<GameArtifact> {
    use anyhow::Context as _;
    use std::collections::HashSet;

    (|| -> Result<GameArtifact> {
        std::fs::create_dir_all(output_dir)
            .with_context(|| format!("create {}", output_dir.display()))?;
        let output_path = output_dir.join(rock_host::project::MAIN_LUA_FILENAME);
        let mut compiler = super::game_runtime::game_compiler_for(backend, path, &output_path);
        // One-shot from-scratch compile: always the init path, so the outcome
        // can never request a shape restart (there is no live process).
        compiler.compile(&HashSet::new(), true)?;
        Ok(GameArtifact {
            artifact_fingerprint: compiler.artifact_fingerprint()?,
            // The manifest re-hashes the bytes itself (sha256); only the paths travel here.
            embedded_files: compiler
                .comptime_watch_files_with_hashes()
                .into_iter()
                .map(|(path, _)| path)
                .collect(),
            comptime_inputs: compiler.comptime_inputs().cloned(),
        })
    })()
}
