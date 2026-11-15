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
    std::fs::create_dir_all(output_dir)
        .with_context(|| format!("create {}", output_dir.display()))?;
    let output_path = output_dir.join(rock_host::project::MAIN_LUA_FILENAME);
    compile_with(super::game_runtime::game_compiler_for(
        backend,
        path,
        &output_path,
    ))
}

fn compile_with(
    mut compiler: Box<dyn super::game_runtime::GameCompiler + Send>,
) -> Result<GameArtifact> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game_runtime::{CompileOutcome, GameCompiler};
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::time::Duration;

    /// A compiler whose teardown blocks until the test lets it finish.
    struct BlockingDrop(mpsc::Receiver<()>);

    impl GameCompiler for BlockingDrop {
        fn compile(&mut self, _: &HashSet<PathBuf>, _: bool) -> Result<CompileOutcome> {
            Ok(CompileOutcome::default())
        }

        fn artifact_fingerprint(&self) -> Result<String> {
            Ok("fixture".into())
        }
    }

    impl Drop for BlockingDrop {
        fn drop(&mut self) {
            let _ = self.0.recv();
        }
    }

    #[test]
    fn a_compile_returns_without_waiting_for_its_compiler_to_be_dropped() {
        let (finish_drop, drop_waits) = mpsc::channel();
        let (returned, compile_returns) = mpsc::channel();
        std::thread::spawn(move || {
            let artifact = compile_with(Box::new(BlockingDrop(drop_waits)));
            let _ = returned.send(artifact.is_ok());
        });
        let outcome = compile_returns.recv_timeout(Duration::from_secs(10));
        let _ = finish_drop.send(());
        assert_eq!(
            outcome,
            Ok(true),
            "the compile waited for its compiler's teardown"
        );
    }
}
