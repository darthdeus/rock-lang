//! rock-host — neutral CLI/runtime substrate shared by all backends.
//!
//! Houses the infra the command surface and every backend need but that has
//! no business living in the legacy `rock` compiler crate: project discovery,
//! the libstone/luajit launch recipe, the cimgui prelude, process management.
//! Leaf crate (no `rock-semantic`/`rock-codegen`/`rock` dep) so bone/skull can
//! depend on it without the legacy pipeline. See `docs/cli-unification-plan.md`.

pub mod cimgui_embed;
pub mod compilation_units;
pub mod engine_build;
pub mod lua_runner;
pub mod preprocessor;
pub mod process_management;
pub mod project;
pub mod source_files;
pub mod stdlib_loader;
pub mod stick;

#[cfg(target_os = "linux")]
const ROCK_DIR_FALLBACK: &str = "/home/developer/projects/rock";
#[cfg(target_os = "macos")]
const ROCK_DIR_FALLBACK: &str = "/Users/developer/projects/rock";
#[cfg(target_os = "windows")]
const ROCK_DIR_FALLBACK: &str = "c:\\dev\\rock";

/// Resolve the Rock root directory at runtime. `ROCK_DIR` env wins; else derive
/// from the running exe's grandparent if it looks like a rock checkout (has
/// `lib/` + `stone/`); else the workspace root above cwd; else a per-OS fallback.
pub fn rock_dir() -> String {
    use std::sync::OnceLock;
    static CACHED: OnceLock<String> = OnceLock::new();
    CACHED
        .get_or_init(|| {
            if let Ok(dir) = std::env::var("ROCK_DIR") {
                return dir;
            }

            if let Ok(exe) = std::env::current_exe() {
                if let Some(bin_dir) = exe.parent() {
                    if let Some(root) = bin_dir.parent() {
                        let has_lib = root.join("lib").is_dir();
                        let has_stone = root.join("stone").is_dir();
                        if has_lib && has_stone {
                            return root.to_string_lossy().into_owned();
                        }
                    }
                }
            }

            if let Ok(cwd) = std::env::current_dir() {
                if let Some(ws) = project::find_workspace_root(&cwd) {
                    return ws.to_string_lossy().into_owned();
                }
            }

            ROCK_DIR_FALLBACK.to_string()
        })
        .clone()
}
