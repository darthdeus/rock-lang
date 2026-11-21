//! Bundled LuaJIT, native libraries and GPU launch environment.
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use rock_host::project::is_release_layout_in;

pub fn luajit_path() -> PathBuf {
    if let Some(path) = std::env::var_os("ROCK_LUAJIT").filter(|path| !path.is_empty()) {
        return PathBuf::from(path);
    }

    let binary = if cfg!(windows) {
        "bin/luajit.exe"
    } else {
        "bin/luajit"
    };
    let release_path = Path::new(&rock_host::rock_dir()).join(binary);
    if release_path.exists() {
        return release_path;
    }

    let bundled = gamelibs::host_dir().join(binary);
    if bundled.is_file() {
        bundled
    } else {
        rock_host::project::luajit_path()
    }
}

const LIB_PATH_SEPARATOR: char = if cfg!(windows) { ';' } else { ':' };

fn lib_path_value_in(root: &Path) -> String {
    // Release layouts must not load stale development libraries from tmp/.
    if is_release_layout_in(root) {
        let lib = root.join("lib");
        return if cfg!(windows) {
            // DLLs sit next to the executables in bin/.
            format!("{};{}", lib.display(), root.join("bin").display())
        } else {
            lib.to_string_lossy().into_owned()
        };
    }

    // The global initializer loads cimgui before stone_init can use rpath.
    let native = gamelibs::host_dir().join(if cfg!(windows) { "bin" } else { "lib" });
    let sep = LIB_PATH_SEPARATOR;
    format!(
        "tmp{sep}{}{sep}{}",
        root.join("tmp").display(),
        native.display()
    )
}

/// Runtime libraries precede the inherited search path, which is still needed
/// for transitive system dependencies. Bundled LuaJIT does not inherit NixOS
/// nix-ld defaults, so Linux graphics loader paths are added where available.
fn lib_path_with_values(inherited: Option<&OsStr>, nix_ld: Option<&OsStr>) -> String {
    let mut value = lib_path_value_in(Path::new(&rock_host::rock_dir()));
    let mut append = |path: &OsStr| {
        if !path.is_empty() {
            value.push(LIB_PATH_SEPARATOR);
            value.push_str(&path.to_string_lossy());
        }
    };

    #[cfg(target_os = "linux")]
    for path in linux_vulkan_library_paths() {
        append(path.as_os_str());
    }
    for path in [inherited, nix_ld].into_iter().flatten() {
        append(path);
    }
    value
}

pub fn lib_path_with_inherited() -> String {
    let inherited = std::env::var_os(lib_path_var());
    let nix_ld = if cfg!(target_os = "linux") {
        std::env::var_os("NIX_LD_LIBRARY_PATH")
    } else {
        None
    };
    lib_path_with_values(inherited.as_deref(), nix_ld.as_deref())
}

#[cfg(target_os = "linux")]
pub fn linux_vulkan_library_paths() -> Vec<PathBuf> {
    linux_vulkan_library_paths_from(Path::new("/nix/store"), Path::new("/run/opengl-driver/lib"))
}

#[cfg(target_os = "linux")]
fn linux_vulkan_library_paths_from(nix_store: &Path, opengl_driver: &Path) -> Vec<PathBuf> {
    let mut paths = std::fs::read_dir(nix_store)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains("-vulkan-loader-"))
        })
        .map(|path| path.join("lib"))
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    paths.sort();

    if opengl_driver.is_dir() {
        paths.push(opengl_driver.to_path_buf());
    }
    paths
}

pub fn lib_path_var() -> &'static str {
    if cfg!(target_os = "macos") {
        "DYLD_LIBRARY_PATH"
    } else if cfg!(target_os = "windows") {
        "PATH"
    } else {
        "LD_LIBRARY_PATH"
    }
}

pub fn lua_path_value() -> String {
    lua_path_for(&luajit_path())
}

fn lua_path_for(luajit: &Path) -> String {
    rock_host::project::lua_path_for(luajit, format!("{}/?.lua;", rock_host::rock_dir()))
}

pub fn build_luajit_command(lua_file: &Path, app_args: &[String]) -> Command {
    let mut command = luajit_command(true);
    command.arg(lua_file).args(app_args);
    configure_environment(command, true)
}

/// Arguments, cwd and unrelated environment overrides survive; the runtime owns
/// LUA_PATH and the native library search path.
pub fn configure_command(command: Command, lua: bool, trace: bool) -> Command {
    let command = if lua {
        let mut configured = luajit_command(trace);
        configured.args(command.get_args());
        if let Some(dir) = command.get_current_dir() {
            configured.current_dir(dir);
        }
        for (key, value) in command.get_envs() {
            match value {
                Some(value) => configured.env(key, value),
                None => configured.env_remove(key),
            };
        }
        configured
    } else {
        command
    };
    configure_environment(command, lua)
}

fn luajit_command(trace: bool) -> Command {
    let mut command = Command::new(luajit_path());
    if trace && std::env::var("STONE_JV").as_deref() == Ok("1") {
        command.arg("-jv");
    }
    rock_host::compiler_thp::configure_runtime_command(&mut command);
    command
}

fn configure_environment(mut command: Command, lua: bool) -> Command {
    // Retargeting creates a new Command, so restore its runtime pre-exec policy.
    if lua {
        command.env("LUA_PATH", lua_path_for(Path::new(command.get_program())));
    }
    command.env(lib_path_var(), lib_path_with_inherited());
    command
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs_err as fs;
    use tempfile::TempDir;
    fn lib_path_entries(value: &str) -> Vec<&str> {
        value.split(if cfg!(windows) { ';' } else { ':' }).collect()
    }
    #[test]
    fn release_lib_path_never_searches_a_worktree_tmp() {
        let temp = TempDir::new().unwrap();
        let canonical = fs::canonicalize(temp.path()).unwrap();
        let root = canonical.as_path();
        for dir in ["lib", "bin", "tmp"] {
            fs::create_dir(root.join(dir)).unwrap();
        }
        fs::write(
            root.join("bin")
                .join(format!("rock{}", std::env::consts::EXE_SUFFIX)),
            "",
        )
        .unwrap();
        assert_eq!(
            rock_host::project::find_workspace_root(root).as_deref(),
            Some(root)
        );
        assert!(is_release_layout_in(root));

        let value = lib_path_value_in(root);
        let entries = lib_path_entries(&value);
        assert_eq!(entries[0], root.join("lib").to_string_lossy(), "{value}");
        assert!(
            !entries
                .iter()
                .any(|entry| Path::new(entry).ends_with("tmp")),
            "{value}"
        );
    }
    #[test]
    fn dev_lib_path_searches_staged_tmp_first() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
        fs::create_dir(root.join("tmp")).unwrap();
        assert!(!is_release_layout_in(root));

        let value = lib_path_value_in(root);
        let entries = lib_path_entries(&value);
        assert_eq!(entries[0], "tmp", "{value}");
        assert_eq!(entries[1], root.join("tmp").to_string_lossy(), "{value}");
    }
    #[test]
    fn stone_jv_env_inserts_jv_before_script() {
        use std::ffi::OsStr;
        let lua = std::path::Path::new("tmp/main.lua");

        let _guard = EnvGuard::set(&[("STONE_JV", Some("1"))]);
        let cmd = build_luajit_command(lua, &[]);
        let args: Vec<_> = cmd.get_args().map(OsStr::to_os_string).collect();
        let jv = args.iter().position(|a| a == "-jv");
        let script = args.iter().position(|a| a == OsStr::new("tmp/main.lua"));
        assert!(jv.is_some(), "STONE_JV=1 should insert -jv; args={args:?}");
        assert!(script.is_some(), "script arg present; args={args:?}");
        // luajit only honors -j* flags BEFORE the script name.
        assert!(
            jv.unwrap() < script.unwrap(),
            "-jv must precede the script; args={args:?}"
        );

        unsafe { std::env::set_var("STONE_JV", "0") };
        let cmd0 = build_luajit_command(lua, &[]);
        assert!(
            !cmd0.get_args().any(|a| a == "-jv"),
            "STONE_JV=0 must not insert -jv"
        );
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_vulkan_library_paths_find_loader_and_driver_dirs() {
        let temp = TempDir::new().unwrap();
        let store = temp.path().join("store");
        let driver = temp.path().join("opengl-driver/lib");
        fs::create_dir_all(store.join("aaa-vulkan-loader-1/lib")).unwrap();
        fs::create_dir_all(store.join("zzz-vulkan-loader-2/lib")).unwrap();
        fs::create_dir_all(store.join("not-vulkan-1/lib")).unwrap();
        fs::create_dir_all(&driver).unwrap();

        let paths = linux_vulkan_library_paths_from(&store, &driver);

        assert_eq!(
            paths,
            vec![
                store.join("aaa-vulkan-loader-1/lib"),
                store.join("zzz-vulkan-loader-2/lib"),
                driver,
            ]
        );
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn authoritative_library_path_includes_nix_ld_after_the_inherited_path() {
        let value = lib_path_with_values(
            Some(std::ffi::OsStr::new("/inherited")),
            Some(std::ffi::OsStr::new("/nix-ld")),
        );
        assert!(
            value.ends_with(":/inherited:/nix-ld"),
            "unexpected library path: {value}"
        );
    }
    struct EnvGuard {
        saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl EnvGuard {
        /// SAFETY: the caller upholds the same nextest-process-isolation
        /// assumption as the stone_jv test; the guard restores on every exit.
        fn set(vars: &[(&'static str, Option<&str>)]) -> Self {
            let mut saved = Vec::with_capacity(vars.len());
            for (key, value) in vars {
                saved.push((*key, std::env::var_os(key)));
                unsafe {
                    match value {
                        Some(v) => std::env::set_var(key, v),
                        None => std::env::remove_var(key),
                    }
                }
            }
            Self { saved }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (key, prev) in self.saved.drain(..) {
                unsafe {
                    match prev {
                        Some(v) => std::env::set_var(key, v),
                        None => std::env::remove_var(key),
                    }
                }
            }
        }
    }
}
