//! rock-host — neutral CLI/runtime substrate shared by all backends.
//!
//! Houses the infra the command surface and every backend need but that has
//! no business living in the legacy `rock` compiler crate: project discovery,
//! the libstone/luajit launch recipe, process management.
//! Leaf crate (no `rock-semantic`/`rock-codegen`/`rock` dep) so bone/skull can
//! depend on it without the legacy pipeline. See `docs/cli-unification-plan.md`.

pub mod compilation_units;
pub mod engine_build;
pub mod js_runner;
pub mod lua_runner;
pub mod preprocessor;
pub mod process_management;
pub mod project;
pub mod release;
pub mod source_files;
pub mod stdlib_loader;
pub mod stick;

#[cfg(target_os = "linux")]
const ROCK_DIR_FALLBACK: &str = "/home/developer/projects/rock";
#[cfg(target_os = "macos")]
const ROCK_DIR_FALLBACK: &str = "/Users/developer/projects/rock";
#[cfg(target_os = "windows")]
const ROCK_DIR_FALLBACK: &str = "c:\\dev\\rock";

/// Resolve the Rock root directory at runtime from environment, exe path, or CWD.
pub fn resolve_rock_dir_with(
    env_override: Option<String>,
    exe_path: Option<&std::path::Path>,
    cwd: Option<&std::path::Path>,
) -> String {
    if let Some(dir) = env_override {
        return dir;
    }

    if let Some(exe) = exe_path {
        // Release layout: <root>/bin/rock where <root> has lib/ + stone/
        if let Some(bin_dir) = exe.parent() {
            if let Some(root) = bin_dir.parent() {
                let has_lib = root.join("lib").is_dir();
                let has_stone = root.join("stone").is_dir();
                if has_lib && has_stone {
                    return root.to_string_lossy().into_owned();
                }
            }
        }

        // Dev layout: <root>/target/{debug,release}/rock where <root> has Cargo.toml + prebuilt/
        if let Some(profile_dir) = exe.parent() {
            if let Some(target_dir) = profile_dir.parent() {
                if target_dir.file_name().and_then(|n| n.to_str()) == Some("target") {
                    if let Some(root) = target_dir.parent() {
                        if root.join("Cargo.toml").exists() && root.join("prebuilt").is_dir() {
                            return root.to_string_lossy().into_owned();
                        }
                    }
                }
            }
        }
    }

    if let Some(cwd) = cwd {
        if let Some(ws) = project::find_workspace_root(cwd) {
            return ws.to_string_lossy().into_owned();
        }
    }

    ROCK_DIR_FALLBACK.to_string()
}

/// Resolve the Rock root directory at runtime. `ROCK_DIR` env wins; else derive
/// from the running exe's location (release install or dev target/); else workspace
/// root above cwd; else a per-OS fallback.
pub fn rock_dir() -> String {
    use std::sync::OnceLock;
    static CACHED: OnceLock<String> = OnceLock::new();
    CACHED
        .get_or_init(|| {
            let env = std::env::var("ROCK_DIR").ok();
            let exe = std::env::current_exe().ok();
            let cwd = std::env::current_dir().ok();
            resolve_rock_dir_with(env, exe.as_deref(), cwd.as_deref())
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn resolve_rock_dir_from_target_debug_exe() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().canonicalize().unwrap();
        fs_err::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
        fs_err::create_dir_all(root.join("prebuilt")).unwrap();
        let exe = root.join("target/debug/rock");
        fs_err::create_dir_all(exe.parent().unwrap()).unwrap();
        fs_err::write(&exe, "").unwrap();

        let unrelated = TempDir::new().unwrap();
        let result = resolve_rock_dir_with(None, Some(&exe), Some(unrelated.path()));
        assert_eq!(result, root.to_string_lossy());
    }
}

