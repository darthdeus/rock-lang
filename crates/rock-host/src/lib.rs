//! rock-host — neutral CLI/runtime substrate shared by all backends.

pub mod cargo;
pub mod compilation_units;
pub mod compiler_thp;
#[cfg(not(target_family = "wasm"))]
pub mod lua_runner;
pub mod process_management;
pub mod program_inputs;
pub mod project;
pub mod source_files;
pub mod stick;

pub(crate) fn resolve_rock_dir_with(
    env_override: Option<String>,
    exe_path: Option<&std::path::Path>,
    cwd: Option<&std::path::Path>,
) -> String {
    if let Some(dir) = env_override {
        return dir;
    }

    if let Some(exe) = exe_path {
        if let Some(bin_dir) = exe.parent() {
            if let Some(root) = bin_dir.parent() {
                let has_lib = root.join("lib").is_dir();
                if has_lib && bin_dir.file_name().is_some_and(|name| name == "bin") {
                    return root.to_string_lossy().into_owned();
                }
            }
        }

        if let Some(profile_dir) = exe.parent() {
            if let Some(target_dir) = profile_dir.parent() {
                if target_dir.file_name().and_then(|n| n.to_str()) == Some("target") {
                    if let Some(root) = target_dir.parent() {
                        if project::is_dev_workspace(root) {
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

    cwd.unwrap_or_else(|| std::path::Path::new("."))
        .to_string_lossy()
        .into_owned()
}

/// Order: `ROCK_DIR`, the running exe's location (release install or dev target/), the
/// workspace root above cwd, then cwd.
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

pub fn timed<T>(what: impl std::fmt::Display, f: impl FnOnce() -> T) -> T {
    let started = rock_core::clock::Instant::now();
    let value = f();
    log::debug!("{what}: {:.1} ms", started.elapsed().as_secs_f64() * 1000.0);
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn rock_dir_falls_back_to_the_workspace_then_cwd() {
        let cwd = TempDir::new().unwrap();
        assert_eq!(
            resolve_rock_dir_with(None, None, Some(cwd.path())),
            cwd.path().to_string_lossy()
        );
        assert_eq!(
            resolve_rock_dir_with(Some("chosen-root".into()), None, Some(cwd.path())),
            "chosen-root"
        );
        fs_err::write(cwd.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        fs_err::create_dir(cwd.path().join("crates")).unwrap();
        assert_eq!(
            resolve_rock_dir_with(None, None, Some(cwd.path())),
            cwd.path().canonicalize().unwrap().to_string_lossy()
        );
    }

    #[test]
    fn resolve_rock_dir_from_compiler_install() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().canonicalize().unwrap();
        fs_err::create_dir(root.join("bin")).unwrap();
        fs_err::create_dir(root.join("lib")).unwrap();
        let exe = root.join("bin/rock");
        fs_err::write(&exe, "").unwrap();
        assert_eq!(
            resolve_rock_dir_with(None, Some(&exe), None),
            root.to_string_lossy()
        );
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
        let result = resolve_rock_dir_with(None, Some(&exe), Some(unrelated.path()));
        assert_eq!(result, root.to_string_lossy());
    }
}
