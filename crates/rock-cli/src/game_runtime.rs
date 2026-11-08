use crate::*;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// What a session gets when it starts from a cached full-compilation artifact
/// instead of compiling one: the identity the running program reports, and the
/// `embed_string` inputs baked into it.
#[derive(Clone)]
pub struct AdoptedArtifact {
    pub artifact_fingerprint: String,
    pub embedded_inputs: Vec<(PathBuf, u64)>,
}

/// A backend's incremental game compiler. Owns all cross-reload state and the
/// output paths; `compile` writes `<out>/main.lua` (+ optional
/// `<out>/incremental.lua` overlay) that the running engine hot-reloads. The
/// caller picks `<out>`: `tmp/` for run/frame/perf, a private
/// `tmp/stone/<game>-<pid>/` per `rock stone` session
/// . Everything around it (watch loop,
/// process, engine/compiler builds, sims) is the backend-agnostic runtime.
/// What a successful (re)compile asks of the runtime beyond hot-reloading.
#[derive(Default)]
pub struct CompileOutcome {
    /// Reasons whose live state or runtime registry cannot take the compatible
    /// fast path. The watcher requests the existing clean process restart for
    /// these changes; body-only edits remain on the CodeRef overlay path.
    pub restart_reasons: Vec<String>,
}

pub trait GameCompiler {
    /// (Re)compile the project. `changed` = game source paths the watcher saw
    /// since the last call (empty on the initial build). `force_full` forces a
    /// from-scratch rebuild (set after a compiler-binary rebuild wiped caches).
    /// Writes the engine-loadable Lua, EXCEPT when the outcome requests a
    /// restart (see `CompileOutcome`). The backend logs its own reload detail
    /// (skull: full vs overlay vs no-change) internally. Err => the caller
    /// reports the error and keeps serving the last good output.
    fn compile(&mut self, changed: &HashSet<PathBuf>, force_full: bool) -> Result<CompileOutcome>;

    /// Fold any overlay back into `main.lua` so a *process restart* (luajit
    /// loads `main.lua` only) gets the complete current program. No-op for
    /// backends without an overlay.
    fn canonicalize_for_restart(&mut self) -> Result<()> {
        Ok(())
    }

    /// Comptime file inputs (`embed_string`) baked into the current program,
    /// with the hash of the bytes the lowering baked in. The watcher treats
    /// them as sources and seeds them against what was compiled; empty for
    /// backends without the concept.
    fn comptime_watch_files_with_hashes(&self) -> Vec<(PathBuf, u64)> {
        Vec::new()
    }

    /// Source roots retained by the successful loaded package graph.
    fn package_roots(&self) -> Option<&[(u32, PathBuf)]> {
        None
    }

    /// The exact compiler-owned identity of the Artifact the next process
    /// would execute. A missing value is an error at every replay/recording
    /// launch boundary; callers must not invent a development identity.
    fn artifact_fingerprint(&self) -> Result<String> {
        anyhow::bail!("compiler did not publish an Artifact fingerprint")
    }

    /// The `[build.vars]` selection the current program was compiled with.
    fn comptime_inputs(&self) -> Option<&bone_semantic::inference::ComptimeInputs> {
        None
    }

    /// The caller started the game from a cached artifact of this project rather
    /// than from this compiler's output, so no session exists yet. Until the
    /// first `compile` builds one, the adopted artifact is what the running
    /// program is: its identity is the reported fingerprint and its
    /// `embed_string` inputs are what the watcher must seed. That first compile
    /// must replace the running program (a full `main.lua` the old process
    /// never loaded), which the caller serves with a process restart.
    fn adopt_cached_artifact(&mut self, _adopted: AdoptedArtifact) -> Result<()> {
        anyhow::bail!("this backend cannot start a session from a cached artifact")
    }
}

/// Skull backend: bone `Session` + `SkullEmitCache` (the `IncrementalProject` holder),
/// emitting a full `main.lua` baseline or an `incremental.lua` overlay per tier.
pub struct SkullGameCompiler {
    stone: Option<crate::skull_backend::IncrementalProject>,
    dir: String,
    output_path: PathBuf,
    incremental_path: PathBuf,
    /// A safety-gated full publication is written by the restart path before
    /// the successor process loads the current program.
    pending_full_write: bool,
    /// The cached artifact the game is running while `stone` is still `None`.
    adopted: Option<AdoptedArtifact>,
}

impl SkullGameCompiler {
    pub fn new(path: &str, output_path: PathBuf, incremental_path: PathBuf) -> Self {
        Self {
            stone: None,
            dir: path.to_string(),
            output_path,
            incremental_path,
            pending_full_write: false,
            adopted: None,
        }
    }
}

impl GameCompiler for SkullGameCompiler {
    fn compile(&mut self, changed: &HashSet<PathBuf>, force_full: bool) -> Result<CompileOutcome> {
        use crate::skull_backend::{IncrementalProject, SkullReload};

        // First build, or a forced full rebuild (compiler changed): (re)build
        // the Session + full main.lua from scratch and wipe any overlay.
        if force_full {
            self.stone = None;
        }
        match &mut self.stone {
            None => {
                let adopted = self.adopted.take();
                self.stone = Some(IncrementalProject::init(
                    &self.dir,
                    &self.output_path,
                    &self.incremental_path,
                )?);
                self.pending_full_write = false;
                // A session that only now exists was not what the game booted:
                // the init just wrote a program the running process never
                // loaded, so the caller restarts onto it.
                Ok(CompileOutcome {
                    restart_reasons: adopted
                        .is_some()
                        .then(|| {
                            "the session was built after starting from the compile cache"
                                .to_string()
                        })
                        .into_iter()
                        .collect(),
                })
            }
            Some(st) => {
                // Feed the Session only the files the watcher saw change (it
                // re-reads those off disk; unchanged files stay cached).
                let changed_vec: Vec<PathBuf> = changed.iter().cloned().collect();
                let reload =
                    st.recompile(&changed_vec, &self.output_path, &self.incremental_path)?;
                match reload {
                    SkullReload::Full {
                        bytes,
                        cook_ms,
                        emit_ms,
                        write_ms,
                    } => {
                        log::info!(target: "STONE",
                            "skull: full rebuild -> main.lua in {:.1}ms ({:.1} cook + {:.1} emit + {:.1} write, {} KB)",
                            cook_ms + emit_ms + write_ms, cook_ms, emit_ms, write_ms, bytes / 1024);
                        return Ok(CompileOutcome {
                            restart_reasons: vec![
                                "full reload requires a clean process restart".to_string(),
                            ],
                        });
                    }
                    SkullReload::Incremental {
                        bytes,
                        reinferred,
                        cook_ms,
                        emit_ms,
                        write_ms,
                    } => log::info!(target: "STONE",
                        "skull: body edit -> incremental.lua in {:.1}ms ({:.1} cook + {:.1} emit + {:.1} write, {} fn{}, {} B overlay)",
                        cook_ms + emit_ms + write_ms, cook_ms, emit_ms, write_ms, reinferred, if reinferred == 1 { "" } else { "s" }, bytes),
                    SkullReload::ShapeChanged { types } => {
                        self.pending_full_write = true;
                        return Ok(CompileOutcome {
                            restart_reasons: types,
                        });
                    }
                    SkullReload::FullRequired { reasons } => {
                        self.pending_full_write = true;
                        return Ok(CompileOutcome {
                            restart_reasons: reasons,
                        });
                    }
                    SkullReload::NoChange => {}
                }
                Ok(CompileOutcome::default())
            }
        }
    }

    fn canonicalize_for_restart(&mut self) -> Result<()> {
        // A restart re-spawns luajit, which loads main.lua only -- it does NOT
        // dual-load the accumulated incremental.lua. Fold the overlay back in
        // first so the restarted process gets the complete current program.
        // Runs when an overlay is on disk (body edits since the last full
        // build) or a safety-gated full publication was deferred.
        if self.incremental_path.exists() || self.pending_full_write {
            if let Some(st) = &mut self.stone {
                log::info!(target: "STONE", "skull: canonicalizing main.lua before restart");
                st.force_full(&self.output_path, &self.incremental_path)?;
                self.pending_full_write = false;
            }
        }
        Ok(())
    }

    fn comptime_watch_files_with_hashes(&self) -> Vec<(PathBuf, u64)> {
        match &self.stone {
            Some(st) => st.comptime_watch_files_with_hashes(),
            None => self
                .adopted
                .as_ref()
                .map(|adopted| adopted.embedded_inputs.clone())
                .unwrap_or_default(),
        }
    }

    fn package_roots(&self) -> Option<&[(u32, PathBuf)]> {
        self.stone.as_ref().map(|stone| stone.package_roots())
    }

    fn artifact_fingerprint(&self) -> Result<String> {
        self.stone
            .as_ref()
            .map(|stone| stone.artifact_fingerprint().to_string())
            .or_else(|| {
                self.adopted
                    .as_ref()
                    .map(|adopted| adopted.artifact_fingerprint.clone())
            })
            .ok_or_else(|| anyhow::anyhow!("compiler did not publish an Artifact fingerprint"))
    }

    fn comptime_inputs(&self) -> Option<&bone_semantic::inference::ComptimeInputs> {
        self.stone.as_ref().map(|stone| stone.comptime_inputs())
    }

    fn adopt_cached_artifact(&mut self, adopted: AdoptedArtifact) -> Result<()> {
        anyhow::ensure!(
            self.stone.is_none(),
            "a cached artifact is adopted before any session exists, not over one",
        );
        self.adopted = Some(adopted);
        Ok(())
    }
}

/// Construct the `GameCompiler` for a backend. `output_path` is the caller's
/// `main.lua` -- `tmp/main.lua` for run/frame/perf, a private
/// `tmp/stone/<game>-<pid>/main.lua` per `rock stone` session
/// and the skull overlay is its
/// `incremental.lua` sibling. Only skull serves the game commands now (bone has
/// no codegen, rock is gone), so anything else is a dispatch bug.
pub fn game_compiler_for(
    backend: crate::Backend,
    path: &str,
    output_path: &Path,
) -> Box<dyn GameCompiler> {
    match backend {
        crate::Backend::Skull => {
            let incremental_path = output_path.with_file_name("incremental.lua");
            Box::new(SkullGameCompiler::new(
                path,
                output_path.to_path_buf(),
                incremental_path,
            ))
        }
        other => panic!(
            "game_compiler_for reached non-skull backend {:?}; only skull serves the game commands",
            other.name()
        ),
    }
}
