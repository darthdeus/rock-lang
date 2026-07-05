//! The backend-agnostic game runtime: a `GameCompiler` trait that captures the
//! ONE thing the rock and skull backends do differently in the game dev loop
//! (produce the Lua the engine hot-reloads), behind which the watch loop,
//! process lifecycle, engine/compiler rebuilds, and sims are all shared.
//!
//! Design + staging: `docs/game-runtime-plan.md`. This module currently holds
//! the trait + its two impls; `do_stone_watch` (stone.rs) calls them in place
//! of the old `match backend`. The watch loop itself folds in later (Stage 2a-ii
//! / 2b) behind an injected event source + sink.

use crate::*;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// A backend's incremental game compiler. Owns all cross-reload state and the
/// output paths; `compile` writes `tmp/main.lua` (+ optional `tmp/incremental.lua`
/// overlay) that the running engine hot-reloads. Everything around it (watch
/// loop, process, engine/compiler builds, sims) is the backend-agnostic runtime.
pub trait GameCompiler {
    /// (Re)compile the project. `changed` = game source paths the watcher saw
    /// since the last call (empty on the initial build). `force_full` forces a
    /// from-scratch rebuild (set after a compiler-binary rebuild wiped caches).
    /// Writes the engine-loadable Lua. The backend logs its own reload detail
    /// (skull: full vs overlay vs no-change) internally — the runtime does not
    /// branch on it. Err => the caller writes `tmp/error.txt` and keeps serving
    /// the last good output.
    fn compile(&mut self, changed: &HashSet<PathBuf>, force_full: bool) -> Result<()>;

    /// Fold any overlay back into `main.lua` so a *process restart* (luajit
    /// loads `main.lua` only) gets the complete current program. No-op for
    /// backends without an overlay.
    fn canonicalize_for_restart(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Skull backend: bone `Session` + `SkullEmitCache` (the `SkullStone` holder),
/// emitting a full `main.lua` baseline or an `incremental.lua` overlay per tier.
pub struct SkullGameCompiler {
    stone: Option<crate::skull_backend::SkullStone>,
    dir: String,
    output_path: PathBuf,
    incremental_path: PathBuf,
}

impl SkullGameCompiler {
    pub fn new(path: &str, output_path: PathBuf, incremental_path: PathBuf) -> Self {
        Self {
            stone: None,
            dir: path.to_string(),
            output_path,
            incremental_path,
        }
    }
}

impl GameCompiler for SkullGameCompiler {
    fn compile(&mut self, changed: &HashSet<PathBuf>, force_full: bool) -> Result<()> {
        use crate::skull_backend::{SkullReload, SkullStone};

        // First build, or a forced full rebuild (compiler changed): (re)build
        // the Session + full main.lua from scratch and wipe any overlay.
        if force_full {
            self.stone = None;
        }
        match &mut self.stone {
            None => {
                self.stone = Some(SkullStone::init(
                    &self.dir,
                    &self.output_path,
                    &self.incremental_path,
                )?);
                Ok(())
            }
            Some(st) => {
                // Feed the Session only the files the watcher saw change (it
                // re-reads those off disk; unchanged files stay cached).
                let changed_vec: Vec<PathBuf> = changed.iter().cloned().collect();
                let reload = st.recompile(&changed_vec, &self.output_path, &self.incremental_path)?;
                match reload {
                    SkullReload::Full { bytes, cook_ms, emit_ms, write_ms } => log::info!(target: "STONE",
                        "skull: full rebuild -> main.lua in {:.1}ms ({:.1} cook + {:.1} emit + {:.1} write, {} KB)",
                        cook_ms + emit_ms + write_ms, cook_ms, emit_ms, write_ms, bytes / 1024),
                    SkullReload::Incremental { bytes, reinferred, cook_ms, emit_ms, write_ms } => log::info!(target: "STONE",
                        "skull: body edit -> incremental.lua in {:.1}ms ({:.1} cook + {:.1} emit + {:.1} write, {} fn{}, {} B overlay)",
                        cook_ms + emit_ms + write_ms, cook_ms, emit_ms, write_ms, reinferred, if reinferred == 1 { "" } else { "s" }, bytes),
                    SkullReload::NoChange => {}
                }
                Ok(())
            }
        }
    }

    fn canonicalize_for_restart(&mut self) -> Result<()> {
        // A restart re-spawns luajit, which loads main.lua only -- it does NOT
        // dual-load the accumulated incremental.lua. Fold the overlay back in
        // first so the restarted process gets the complete current program.
        // Only when an overlay is actually on disk (body edits since the last
        // full build).
        if self.incremental_path.exists() {
            if let Some(st) = &mut self.stone {
                log::info!(target: "STONE", "skull: folding incremental.lua into main.lua before restart");
                st.force_full(&self.output_path, &self.incremental_path)?;
            }
        }
        Ok(())
    }
}

/// Construct the `GameCompiler` for a backend. `output_path` is `tmp/main.lua`;
/// the skull overlay is its `incremental.lua` sibling. Only skull serves the
/// game commands now (bone has no codegen, rock is gone), so anything else is a
/// dispatch bug.
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
