//! Bundled LuaJIT, native libraries and GPU launch environment.
use std::path::{Path, PathBuf};

use rock_host::project::is_release_layout_in;

/// Get the path to the LuaJIT binary.
///
/// In release layout: `<rock_dir>/bin/luajit`
/// In dev layout: the host cache's `gamelibs-<platform>/bin/luajit`
pub fn luajit_path() -> PathBuf {
    if let Some(path) = std::env::var_os("ROCK_LUAJIT").filter(|path| !path.is_empty()) {
        return PathBuf::from(path);
    }

    let root = PathBuf::from(rock_host::rock_dir());

    // Release layout: bin/luajit next to bin/rock
    let release_path = if cfg!(target_os = "windows") {
        root.join("bin").join("luajit.exe")
    } else {
        root.join("bin").join("luajit")
    };
    if release_path.exists() {
        return release_path;
    }

    let bundled = gamelibs::host_dir().join(if cfg!(target_os = "windows") {
        "bin/luajit.exe"
    } else {
        "bin/luajit"
    });
    if bundled.is_file() {
        bundled
    } else {
        rock_host::project::luajit_path()
    }
}

/// Get the library search path value for the current platform.
///
/// In release layout: `<rock_dir>/lib` (Windows adds `<rock_dir>\bin`)
/// In dev layout: `tmp`, the workspace's `tmp`, and the cached native lib dir.
pub fn lib_path_value() -> String {
    lib_path_value_in(Path::new(&rock_host::rock_dir()))
}

fn lib_path_value_in(root: &Path) -> String {
    // Release layout stages nothing into tmp/, so a worktree's tmp/ can only
    // hold stale dev-layout builds; searching it shadowed the SDK's libstone.
    if is_release_layout_in(root) {
        let lib_str = root.join("lib").to_string_lossy().into_owned();
        if cfg!(target_os = "windows") {
            // Windows also needs bin/ on PATH for DLLs next to executables
            let bin_str = root.join("bin").to_string_lossy().into_owned();
            return format!("{};{}", lib_str, bin_str);
        } else {
            return lib_str;
        }
    }

    // Dev layout: cached native libraries plus tmp/ rooted at rock_dir().
    // The prebuilt lib dir MUST be on the search path: stone's Rock global
    // initializer does ffi.load("libcimgui_complete.{dylib,so,dll}") before
    // stone_init has had a chance to dlopen anything via rpath.
    let dev_tmp = root.join("tmp");
    let dev_tmp_str = dev_tmp.to_string_lossy();
    let prefix = gamelibs::host_dir();
    if cfg!(target_os = "windows") {
        format!("tmp;{};{}", dev_tmp_str, prefix.join("bin").display())
    } else {
        format!("tmp:{}:{}", dev_tmp_str, prefix.join("lib").display())
    }
}

/// `lib_path_value()` with platform graphics libraries and the caller's
/// inherited search paths appended.
/// Linux also gets the system Vulkan loader and driver directories when they
/// exist; the bundled LuaJIT does not receive NixOS's nix-ld defaults itself.
///
/// Rock's own dirs (SDK `lib/`, or `tmp` and cached gamelibs) come FIRST so the bundled
/// luajit/cimgui libs always win; the inherited value follows so a
/// user-provided `LD_LIBRARY_PATH`/`DYLD_LIBRARY_PATH`/`PATH` still
/// resolves transitive system deps the prebuilt FHS libs need but the
/// gamelibs dir doesn't ship. On NixOS those deps live in nix-store dirs the
/// user puts on the path (e.g. via `programs.nix-ld.libraries` ->
/// `$NIX_LD_LIBRARY_PATH`); clobbering instead of appending drops them.
#[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
fn lib_path_with_values(
    inherited: Option<&std::ffi::OsStr>,
    nix_ld: Option<&std::ffi::OsStr>,
) -> String {
    let mut value = lib_path_value();
    let sep = if cfg!(target_os = "windows") {
        ";"
    } else {
        ":"
    };

    #[cfg(target_os = "linux")]
    for path in linux_vulkan_library_paths() {
        value.push_str(sep);
        value.push_str(&path.to_string_lossy());
    }

    if let Some(inherited) = inherited {
        if !inherited.is_empty() {
            value.push_str(sep);
            value.push_str(&inherited.to_string_lossy());
        }
    }

    #[cfg(target_os = "linux")]
    if let Some(nix_ld) = nix_ld {
        if !nix_ld.is_empty() {
            value.push_str(sep);
            value.push_str(&nix_ld.to_string_lossy());
        }
    }
    value
}

pub fn lib_path_with_inherited() -> String {
    let inherited = std::env::var_os(lib_path_var());
    let nix_ld = std::env::var_os("NIX_LD_LIBRARY_PATH");
    lib_path_with_values(inherited.as_deref(), nix_ld.as_deref())
}

#[cfg(target_os = "linux")]
pub fn linux_vulkan_library_paths() -> Vec<PathBuf> {
    linux_vulkan_library_paths_from(Path::new("/nix/store"), Path::new("/run/opengl-driver/lib"))
}

#[cfg(target_os = "linux")]
fn linux_vulkan_library_paths_from(nix_store: &Path, opengl_driver: &Path) -> Vec<PathBuf> {
    let mut loader_paths = std::fs::read_dir(nix_store)
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
    loader_paths.sort();

    let mut paths = loader_paths;
    if opengl_driver.is_dir() {
        paths.push(opengl_driver.to_path_buf());
    }
    paths
}

/// Get the environment variable name for the library search path.
pub fn lib_path_var() -> &'static str {
    if cfg!(target_os = "macos") {
        "DYLD_LIBRARY_PATH"
    } else if cfg!(target_os = "windows") {
        "PATH"
    } else {
        "LD_LIBRARY_PATH"
    }
}

/// Get the LUA_PATH value for requiring Lua modules.
///
/// Every layout includes its Rock root and the matching LuaJIT toolkit.
pub fn lua_path_value() -> String {
    let root = rock_host::rock_dir();
    let p = format!("{root}/?.lua;");

    rock_host::project::lua_path_for(&luajit_path(), p)
}

/// Env switch enabling a RenderDoc GPU capture: the value is the capture
/// file path template stone hands to `SetCaptureFilePathTemplate`
/// (RenderDoc appends `_frame<N>.rdc`).
pub const RENDERDOC_ENV: &str = "STONE_RENDERDOC";
/// Optional swap count after which stone auto-triggers one capture.
/// Without it, captures are manual via `stone_renderdoc_trigger_capture`.
pub const RENDERDOC_FRAME_ENV: &str = "STONE_RENDERDOC_FRAME";
/// Optional explicit `librenderdoc.so` path for the preload gate.
pub const RENDERDOC_LIB_ENV: &str = "STONE_RENDERDOC_LIB";

/// `Some(template)` when a RenderDoc capture was requested, else `None`.
/// Empty counts as unset, matching the `ROCK_LUAJIT` precedent.
pub fn renderdoc_capture_requested() -> Option<String> {
    std::env::var(RENDERDOC_ENV).ok().filter(|v| !v.is_empty())
}

/// Resolve the `librenderdoc.so` path to preload: `STONE_RENDERDOC_LIB`
/// (verbatim, even when missing -- the child then fails loudly naming the
/// user's path), else the `../lib/librenderdoc.so` sibling of `renderdoccmd`
/// on PATH (the nix case), else the bare name for loader search. Never fails.
pub fn resolve_renderdoc_lib() -> std::ffi::OsString {
    resolve_renderdoc_lib_with(
        std::env::var_os(RENDERDOC_LIB_ENV).filter(|v| !v.is_empty()),
        std::env::var_os("PATH"),
    )
}

pub(crate) fn resolve_renderdoc_lib_with(
    lib_override: Option<std::ffi::OsString>,
    path_var: Option<std::ffi::OsString>,
) -> std::ffi::OsString {
    if let Some(lib) = lib_override {
        return lib;
    }
    if let Some(exe) = find_on_path_in(path_var.as_deref(), "renderdoccmd")
        && let Some(sibling) = renderdoc_lib_sibling_of(&exe)
    {
        return sibling.into_os_string();
    }
    std::ffi::OsString::from("librenderdoc.so")
}

fn find_on_path_in(path_var: Option<&std::ffi::OsStr>, name: &str) -> Option<PathBuf> {
    let path = path_var?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// `<prefix>/lib/librenderdoc.so` for a `<prefix>/bin/renderdoccmd`.
/// Canonicalizes first: on NixOS the PATH entry is a `/run` symlink whose
/// textual `../lib` does not exist.
fn renderdoc_lib_sibling_of(exe: &Path) -> Option<PathBuf> {
    let canon = fs_err::canonicalize(exe).ok()?;
    let lib = canon
        .parent()?
        .parent()?
        .join("lib")
        .join("librenderdoc.so");
    lib.is_file().then_some(lib)
}

/// Append `lib` to an ambient `LD_PRELOAD` value (append, never clobber).
/// A present-but-empty ambient value is the scrub signature (some parent
/// already loaded the lib); it contributes no entry.
// Linux production use (`apply_renderdoc_gate`) plus its unit test in `lua_runner`.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn join_preload(
    existing: Option<&std::ffi::OsStr>,
    lib: &std::ffi::OsStr,
) -> std::ffi::OsString {
    let mut joined = std::ffi::OsString::new();
    if let Some(existing) = existing.filter(|v| !v.is_empty()) {
        joined.push(existing);
        joined.push(":");
    }
    joined.push(lib);
    joined
}

/// Append the resolved lib to the child command's `LD_PRELOAD` (append,
/// never clobber). Called ONLY from `build_luajit_command` -- the final
/// game spawn.
///
/// `librenderdoc.so` empties `LD_PRELOAD` in every process it loads into,
/// so an ambient preload never survives the rock -> luajit chain (rock
/// would load the lib first and scrub the var). Setting it explicitly on
/// the final spawn's `Command` is the only shape that reaches the game.
#[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
pub fn apply_renderdoc_gate(cmd: &mut std::process::Command) {
    if renderdoc_capture_requested().is_none() {
        return;
    }
    #[cfg(target_os = "linux")]
    {
        let lib = resolve_renderdoc_lib();
        if let Some(override_path) = std::env::var_os(RENDERDOC_LIB_ENV).filter(|v| !v.is_empty())
            && !Path::new(&override_path).is_file()
        {
            eprintln!(
                "rock: {}={} does not exist; the game will fail its RenderDoc load loudly",
                RENDERDOC_LIB_ENV,
                override_path.to_string_lossy()
            );
        }
        let joined = join_preload(std::env::var_os("LD_PRELOAD").as_deref(), &lib);
        cmd.env("LD_PRELOAD", joined);
    }
    #[cfg(target_os = "macos")]
    {
        static WARN: std::sync::Once = std::sync::Once::new();
        WARN.call_once(|| {
            eprintln!(
                "rock: {RENDERDOC_ENV} is set, but RenderDoc has no macOS support; \
                 the game runs unhooked (use apitrace for GL captures)"
            );
        });
    }
    #[cfg(target_os = "windows")]
    {
        static WARN: std::sync::Once = std::sync::Once::new();
        WARN.call_once(|| {
            eprintln!(
                "rock: {RENDERDOC_ENV} is set, but the Windows preload gate is not implemented; \
                 the game runs unhooked"
            );
        });
    }
}

pub fn build_luajit_bytecode_command(
    lua_file: &Path,
    bytecode_file: &Path,
) -> std::process::Command {
    configure_command(
        rock_host::project::build_luajit_bytecode_command(lua_file, bytecode_file),
        true,
        false,
        false,
    )
}

pub fn build_luajit_command(lua_file: &Path, app_args: &[String]) -> std::process::Command {
    configure_command(
        rock_host::project::build_luajit_command(lua_file, app_args),
        true,
        true,
        true,
    )
}

/// Configure before stdio. Lua commands may change executable and prepend JIT
/// switches. Arguments, cwd and unrelated environment overrides survive; the
/// runtime owns LUA_PATH and the native library search path.
pub fn configure_command(
    command: std::process::Command,
    lua: bool,
    trace: bool,
    capture: bool,
) -> std::process::Command {
    let mut configured = if lua {
        let mut configured = std::process::Command::new(luajit_path());
        if trace && std::env::var("STONE_JV").as_deref() == Ok("1") {
            configured.arg("-jv");
        }
        configured.args(command.get_args());
        if let Some(dir) = command.get_current_dir() {
            configured.current_dir(dir);
        }
        for (key, value) in command.get_envs() {
            match value {
                Some(value) => {
                    configured.env(key, value);
                }
                None => {
                    configured.env_remove(key);
                }
            }
        }
        configured.env("LUA_PATH", lua_path_value());
        configured
    } else {
        command
    };
    // Retargeting creates a new Command, so restore its runtime pre-exec policy.
    if lua {
        rock_host::compiler_thp::configure_runtime_command(&mut configured);
    }
    configured.env(lib_path_var(), lib_path_with_inherited());
    if capture {
        apply_renderdoc_gate(&mut configured);
    }
    configured
}

/// Private callers use the same captured evaluator with the bundled environment.
pub fn run_lua_code_with_timeout(
    code: &str,
    timeout: Option<std::time::Duration>,
) -> anyhow::Result<rock_host::lua_runner::LuaRun> {
    let command = configure_command(
        rock_host::project::build_luajit_stdin_command(),
        true,
        false,
        false,
    );
    rock_host::lua_runner::run_lua_code_with_command(command, code, timeout)
        .map_err(|error| anyhow::anyhow!(error.message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs_err as fs;
    use tempfile::TempDir;
    fn lib_path_entries(value: &str) -> Vec<&str> {
        value.split(if cfg!(windows) { ';' } else { ':' }).collect()
    }
    #[cfg(unix)]
    fn run_probe(
        path: &Path,
        code: &str,
        timeout: Option<std::time::Duration>,
        env: &[(&str, &str)],
    ) -> anyhow::Result<rock_host::lua_runner::LuaRun> {
        let mut command = std::process::Command::new(path);
        command.envs(env.iter().copied());
        rock_host::lua_runner::run_lua_code_with_command(command, code, timeout)
            .map_err(|e| anyhow::anyhow!(e.message))
    }
    #[test]
    fn release_lib_path_never_searches_a_worktree_tmp() {
        let temp = TempDir::new().unwrap();
        // find_workspace_root returns a canonical path; macOS temp dirs sit under the /var symlink.
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
    /// Process env vars touched by this test, restored on drop -- including
    /// when an assert unwinds, so a failure cannot leak LD_PRELOAD/PATH/
    /// STONE_RENDERDOC_* into a sibling test sharing this process.
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

    #[test]
    fn environment_guard_restores_on_success_and_failure() {
        const KEY: &str = "ROCK_RUNTIME_ENV_GUARD_WITNESS";
        let previous = std::env::var_os(KEY);
        {
            let _guard = EnvGuard::set(&[(KEY, Some("success"))]);
        }
        assert_eq!(std::env::var_os(KEY), previous);
        let failed = std::panic::catch_unwind(|| {
            let _guard = EnvGuard::set(&[(KEY, Some("failure"))]);
            panic!("intentional cleanup witness");
        });
        assert!(failed.is_err());
        assert_eq!(std::env::var_os(KEY), previous);
    }

    /// Explicitly-set env on a Command (`get_envs` is the only reader;
    /// ambient inheritance is invisible, which is exactly the pin).
    fn cmd_env(cmd: &std::process::Command, key: &str) -> Option<std::ffi::OsString> {
        cmd.get_envs()
            .find(|(k, _)| *k == std::ffi::OsStr::new(key))
            .and_then(|(_, v)| v.map(|v| v.to_os_string()))
    }

    /// The STONE_RENDERDOC preload gate reaches only the final game spawn:
    /// `build_luajit_command` gains the lib while the bytecode compile and
    /// the stdin harness spawns stay clean, and everything stays clean when
    /// the switch is unset. Append semantics and lib resolution are pinned
    /// through the pure helpers: this test mutates only STONE_RENDERDOC_*
    /// process-wide (no sibling reads them), never LD_PRELOAD/PATH, so it
    /// stays green under threaded `cargo test` as well as nextest.
    #[test]
    fn renderdoc_gate_reaches_only_the_final_spawn() {
        use std::ffi::OsStr;
        use std::path::Path;

        const SENTINEL: &str = "/ambient/sentinel.so";
        const TEMPLATE: &str = "/tmp/rd-gate-test/captures/demo-crypt";
        let lua = Path::new("tmp/main.lua");

        // Append semantics, pure: ambient preserved first, scrub-empty
        // contributes no entry (entries are `:`-separated per ld.so).
        let joined = super::join_preload(Some(OsStr::new(SENTINEL)), OsStr::new("librenderdoc.so"));
        let binding = joined.to_string_lossy();
        let entries: Vec<_> = binding.split(':').collect();
        assert_eq!(entries, vec![SENTINEL, "librenderdoc.so"]);
        assert_eq!(
            super::join_preload(None, OsStr::new("librenderdoc.so")).to_string_lossy(),
            "librenderdoc.so"
        );
        assert_eq!(
            super::join_preload(Some(OsStr::new("")), OsStr::new("librenderdoc.so"))
                .to_string_lossy(),
            "librenderdoc.so"
        );

        // Resolution, pure: override verbatim, renderdoccmd sibling, bare
        // fallback. Canonicalized: production canonicalizes the PATH hit.
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonical tempdir");
        let bin = root.join("bin");
        let lib = root.join("lib");
        std::fs::create_dir_all(&bin).expect("bin");
        std::fs::create_dir_all(&lib).expect("lib");
        std::fs::write(bin.join("renderdoccmd"), "#!/bin/sh\n").expect("fake renderdoccmd");
        let sibling = lib.join("librenderdoc.so");
        std::fs::write(&sibling, "fake").expect("fake lib");
        let resolve = super::resolve_renderdoc_lib_with;
        assert_eq!(
            resolve(
                Some(sibling.as_os_str().to_os_string()),
                Some(std::ffi::OsString::from("/no/such/bin"))
            ),
            sibling.as_os_str()
        );
        assert_eq!(
            resolve(None, Some(bin.as_os_str().to_os_string())),
            sibling.as_os_str()
        );
        assert_eq!(
            resolve(None, Some(std::ffi::OsString::from("/no/such/bin"))).to_string_lossy(),
            "librenderdoc.so"
        );
        assert_eq!(resolve(None, None).to_string_lossy(), "librenderdoc.so");

        // Gate ON with an explicit lib override (a real file: keeps the
        // missing-override warning out of the test output).
        {
            let override_lib = sibling.to_string_lossy().into_owned();
            let _guard = EnvGuard::set(&[
                ("STONE_RENDERDOC", Some(TEMPLATE)),
                ("STONE_RENDERDOC_LIB", Some(&override_lib)),
            ]);
            if cfg!(target_os = "linux") {
                let game = super::build_luajit_command(lua, &[]);
                let preload = cmd_env(&game, "LD_PRELOAD")
                    .expect("gate sets LD_PRELOAD on the final game spawn")
                    .to_string_lossy()
                    .into_owned();
                assert!(
                    preload.contains(&override_lib),
                    "gate must append the override lib: {preload}"
                );

                let bytecode =
                    super::build_luajit_bytecode_command(lua, Path::new("tmp/main.ljbc"));
                assert!(
                    cmd_env(&bytecode, "LD_PRELOAD").is_none(),
                    "bytecode compile is not the final spawn"
                );
            }
            #[cfg(unix)]
            {
                // Harness spawn observed through real children: an explicit
                // child preload passes through exactly, and without one no
                // gate lib appears. The sentinel travels via the child's own
                // env, never via process-global LD_PRELOAD.
                let probe = root.join("print_preload");
                test_utils::write_executable(
                    &probe,
                    "#!/bin/sh\ncat >/dev/null\necho \"LD_PRELOAD=$LD_PRELOAD\"\n",
                );
                let run = run_probe(
                    &probe,
                    "",
                    Some(test_utils::HANG_BACKSTOP),
                    &[("LD_PRELOAD", SENTINEL)],
                )
                .expect("probe child runs");
                assert!(run.success, "probe child failed: {}", run.combined());
                assert_eq!(
                    run.stdout.trim(),
                    format!("LD_PRELOAD={SENTINEL}"),
                    "harness spawn must pass the child preload through exactly"
                );
                let bare = run_probe(&probe, "", Some(test_utils::HANG_BACKSTOP), &[])
                    .expect("bare probe child runs");
                assert!(
                    !bare.stdout.contains("librenderdoc"),
                    "harness spawn must not gain the gate lib: {}",
                    bare.stdout
                );
            }
        }

        // Gate OFF: STONE_RENDERDOC unset leaves the final spawn clean too.
        {
            let _guard = EnvGuard::set(&[("STONE_RENDERDOC", None), ("STONE_RENDERDOC_LIB", None)]);
            if cfg!(target_os = "linux") {
                let game = super::build_luajit_command(lua, &[]);
                assert!(
                    cmd_env(&game, "LD_PRELOAD").is_none(),
                    "gate off must not touch LD_PRELOAD"
                );
            }
        }
    }
}
