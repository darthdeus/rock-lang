use crate::*;
use std::collections::HashSet;
use std::path::PathBuf;

#[derive(Default)]
pub struct CompileOutcome {
    /// Safety gates can require a clean process restart.
    pub restart_reasons: Vec<String>,
}

/// The watcher owns one incremental game compiler and its output paths.
pub struct GameCompiler {
    stone: Option<crate::skull_backend::IncrementalProject>,
    dir: String,
    output_path: PathBuf,
    incremental_path: PathBuf,
    /// A safety-gated full publication is written by the restart path before
    /// the successor process loads the current program.
    pending_full_write: bool,
}

impl GameCompiler {
    pub fn new(path: &str, output_path: PathBuf, incremental_path: PathBuf) -> Self {
        Self {
            stone: None,
            dir: path.to_string(),
            output_path,
            incremental_path,
            pending_full_write: false,
        }
    }

    pub fn compile(
        &mut self,
        changed: &HashSet<PathBuf>,
        force_full: bool,
    ) -> Result<CompileOutcome> {
        use crate::skull_backend::{IncrementalProject, SkullReload};

        if force_full {
            self.stone = None;
        }
        match &mut self.stone {
            None => {
                self.stone = Some(IncrementalProject::init(
                    &self.dir,
                    &self.output_path,
                    &self.incremental_path,
                )?);
                self.pending_full_write = false;
                Ok(CompileOutcome::default())
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

    /// Fold any overlay back into `main.lua` so a *process restart* (luajit
    /// loads `main.lua` only) gets the complete current program.
    pub fn canonicalize_for_restart(&mut self) -> Result<()> {
        if self.incremental_path.exists() || self.pending_full_write {
            if let Some(st) = &mut self.stone {
                log::info!(target: "STONE", "skull: canonicalizing main.lua before restart");
                st.force_full(&self.output_path, &self.incremental_path)?;
                self.pending_full_write = false;
            }
        }
        Ok(())
    }

    /// Comptime file inputs (`embed_string`) baked into the current program,
    /// with the hash of the bytes the lowering baked in. The watcher treats
    /// them as sources and seeds them against what was compiled.
    pub fn comptime_watch_files_with_hashes(&self) -> Vec<(PathBuf, u64)> {
        self.stone
            .as_ref()
            .map(|st| st.comptime_watch_files_with_hashes())
            .unwrap_or_default()
    }

    /// Source roots retained by the successful loaded package graph.
    pub fn package_roots(&self) -> Option<&[(u32, PathBuf)]> {
        self.stone.as_ref().map(|stone| stone.package_roots())
    }

    pub fn artifact_fingerprint(&self) -> Result<String> {
        self.stone
            .as_ref()
            .map(|stone| stone.artifact_fingerprint().to_string())
            .ok_or_else(|| anyhow::anyhow!("compiler did not publish an Artifact fingerprint"))
    }
}
