//! rock-host — neutral CLI/runtime substrate shared by all backends.
//!
//! Houses the infra the command surface and every backend need but that has
//! no business living in the legacy `rock` compiler crate: project discovery,
//! source loading, configuration, LuaJIT execution and process management.
//! Leaf crate (no `rock-semantic`/`rock-codegen`/`rock` dep) so bone/skull can
//! depend on it without the legacy pipeline.

pub mod cargo;
pub mod compilation_units;
pub mod compiler_thp;
pub mod lua_runner;
pub mod preprocessor;
pub mod process_management;
pub mod project;
pub mod source_files;
pub mod stick;

static SOURCE_ROOT: std::sync::OnceLock<&'static std::path::Path> = std::sync::OnceLock::new();

/// Supply the binary host's source fallback before the first call to `rock_dir`.
pub fn install_source_root(root: &'static std::path::Path) {
    assert_eq!(*SOURCE_ROOT.get_or_init(|| root), root, "source root already installed");
}

/// Resolve the Rock root directory from environment, exe path, CWD, or a host fallback.
pub fn resolve_rock_dir_with(
    env_override: Option<String>,
    exe_path: Option<&std::path::Path>,
    cwd: Option<&std::path::Path>,
    source_root: Option<&std::path::Path>,
) -> String {
    if let Some(dir) = env_override {
        return dir;
    }

    if let Some(exe) = exe_path {
        // Release layout: <root>/bin/rock where <root> has lib/
        if let Some(bin_dir) = exe.parent() {
            if let Some(root) = bin_dir.parent() {
                let has_lib = root.join("lib").is_dir();
                if has_lib && bin_dir.file_name().is_some_and(|name| name == "bin") {
                    return root.to_string_lossy().into_owned();
                }
            }
        }

        // Dev layout: <root>/target/{debug,release}/rock where <root> has Cargo.toml + crates/
        if let Some(profile_dir) = exe.parent() {
            if let Some(target_dir) = profile_dir.parent() {
                if target_dir.file_name().and_then(|n| n.to_str()) == Some("target") {
                    if let Some(root) = target_dir.parent() {
                        if root.join("Cargo.toml").exists() && root.join("crates").is_dir() {
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

    source_root.or(cwd).unwrap_or_else(|| std::path::Path::new(".")).to_string_lossy().into_owned()
}

/// Resolve the Rock root directory at runtime. `ROCK_DIR` env wins; else derive
/// from the running exe's location (release install or dev target/); else workspace
/// root above cwd; else the host-supplied source root, or cwd when none is installed.
pub fn rock_dir() -> String {
    use std::sync::OnceLock;
    static CACHED: OnceLock<String> = OnceLock::new();
    CACHED
        .get_or_init(|| {
            let env = std::env::var("ROCK_DIR").ok();
            let exe = std::env::current_exe().ok();
            let cwd = std::env::current_dir().ok();
            resolve_rock_dir_with(env, exe.as_deref(), cwd.as_deref(), SOURCE_ROOT.get().copied())
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn source_fallback_follows_runtime_roots() {
        let source = TempDir::new().unwrap();
        let cwd = TempDir::new().unwrap();
        assert_eq!(resolve_rock_dir_with(None, None, Some(cwd.path()), Some(source.path())), source.path().to_string_lossy());
        assert_eq!(resolve_rock_dir_with(None, None, Some(cwd.path()), None), cwd.path().to_string_lossy());
        assert_eq!(resolve_rock_dir_with(Some("chosen-root".into()), None, Some(cwd.path()), Some(source.path())), "chosen-root");
        fs_err::write(cwd.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        fs_err::create_dir(cwd.path().join("crates")).unwrap();
        assert_eq!(resolve_rock_dir_with(None, None, Some(cwd.path()), Some(source.path())), cwd.path().to_string_lossy());
    }

    #[test]
    fn resolve_rock_dir_from_compiler_install() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().canonicalize().unwrap();
        fs_err::create_dir(root.join("bin")).unwrap();
        fs_err::create_dir(root.join("lib")).unwrap();
        let exe = root.join("bin/rock");
        fs_err::write(&exe, "").unwrap();
        assert_eq!(resolve_rock_dir_with(None, Some(&exe), None, None), root.to_string_lossy());
    }

    #[test]
    fn resolve_rock_dir_from_target_debug_exe() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().canonicalize().unwrap();
        fs_err::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
        fs_err::create_dir_all(root.join("crates")).unwrap();
        let exe = root.join("target/debug/rock");
        fs_err::create_dir_all(exe.parent().unwrap()).unwrap();
        fs_err::write(&exe, "").unwrap();

        let unrelated = TempDir::new().unwrap();
        let result = resolve_rock_dir_with(None, Some(&exe), Some(unrelated.path()), None);
        assert_eq!(result, root.to_string_lossy());
    }
}
