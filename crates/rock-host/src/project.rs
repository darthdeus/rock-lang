use anyhow::Result;
use std::path::{Path, PathBuf};

/// Check if a directory looks like the Rock workspace root.
///
/// Recognizes two layouts:
/// - Dev workspace: has Cargo.toml + crates/
/// - Release layout: has bin/rock + lib/
fn is_rock_root(path: &Path) -> bool {
    (path.join("Cargo.toml").exists() && path.join("crates").is_dir()) || is_release_layout_in(path)
}

/// Find the Rock workspace root by looking for known layout markers.
/// This identifies the top-level Rock development workspace (which contains target/,
/// crates/, tmp/, etc.) or a release installation (which contains bin/, lib/).
///
/// Walks up from the given path. Returns None if no workspace root is found.
pub fn find_workspace_root(path: &Path) -> Option<PathBuf> {
    if let Some(root) = context_root_from_env() {
        return Some(root);
    }
    if let Some(root) = release_root_from_exe() {
        return Some(root);
    }

    let initial_path = fs_err::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());

    let mut current = if initial_path.is_file() {
        initial_path.parent()?.to_path_buf()
    } else {
        initial_path
    };

    loop {
        if is_rock_root(&current) {
            return Some(current);
        }
        current = current.parent()?.to_path_buf();
    }
}

pub(crate) fn context_root_from_env() -> Option<PathBuf> {
    let root = PathBuf::from(std::env::var_os("ROCK_CONTEXT_ROOT")?);
    if root.is_dir() && root.join("sdk").is_dir() && root.join(".context/context.json").is_file() {
        Some(root)
    } else {
        None
    }
}

/// If the current executable is `<root>/bin/rock` inside a release layout,
/// return that root. The binary knows where its own install directory is,
/// which is more authoritative than walking up CWD — a user running a
/// release zip from inside another Rock workspace should still use the
/// release zip's lib/, not the outer dev workspace.
fn release_root_from_exe() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let bin_dir = exe.parent()?;
    let root = bin_dir.parent()?;
    if is_rock_root(root) && !root.join("Cargo.toml").exists() {
        Some(root.to_path_buf())
    } else {
        None
    }
}

/// Find project directory by looking for stick.toml or other project markers
/// This walks up the directory tree from the given path until it finds a project root
pub fn find_project_root(path: &Path) -> Option<PathBuf> {
    // Convert to absolute path first to handle relative paths correctly
    let initial_path = fs_err::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());

    // Start from the directory containing the path (or the path itself if it's a directory)
    let mut current_path = if initial_path.is_file() || initial_path.extension().is_some() {
        // If it's a file or has an extension, start from parent
        initial_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| initial_path.clone())
    } else {
        initial_path
    };

    // Project markers in order of preference
    // Only look for stick.toml - don't treat Cargo.toml or .git as Rock project roots
    let markers = ["stick.toml"];

    loop {
        // Check for each marker in order
        for marker in &markers {
            let marker_path = current_path.join(marker);
            if marker_path.exists() {
                return Some(current_path);
            }
        }

        current_path = current_path.parent()?.to_path_buf();
    }
}

/// Returns true when the rock_dir looks like an extracted release zip
/// (bin/rock + lib/) rather than a dev workspace (Cargo.toml + crates/).
///
/// In release layout the native libraries are already staged under lib/, so
/// commands like `rock run` must not shell out to cargo or copy from
/// target/debug/ — there is no target dir.
pub fn is_release_layout() -> bool {
    is_release_layout_in(Path::new(&crate::rock_dir()))
}

/// Requires the `rock` executable itself: system roots such as `/` also hold
/// bin/ and lib/.
pub fn is_release_layout_in(root: &Path) -> bool {
    root.join("bin")
        .join(format!("rock{}", std::env::consts::EXE_SUFFIX))
        .is_file()
        && root.join("lib").is_dir()
        && !root.join("Cargo.toml").exists()
}

/// Find LuaJIT through an explicit override, beside this executable, or PATH.
/// A bare name on failure lets process spawning report the OS lookup error.
pub fn luajit_path() -> PathBuf {
    locate_luajit(
        std::env::var_os("ROCK_LUAJIT"),
        std::env::current_exe().ok().as_deref(),
        std::env::var_os("PATH").as_deref(),
    )
}

fn locate_luajit(
    override_path: Option<std::ffi::OsString>,
    exe: Option<&Path>,
    path: Option<&std::ffi::OsStr>,
) -> PathBuf {
    if let Some(path) = override_path.filter(|path| !path.is_empty()) {
        return path.into();
    }
    let name = if cfg!(windows) {
        "luajit.exe"
    } else {
        "luajit"
    };
    if let Some(candidate) = exe.and_then(Path::parent).map(|dir| dir.join(name)) {
        if candidate.is_file() {
            return candidate;
        }
    }
    if let Some(path) = path {
        for dir in std::env::split_paths(path) {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    PathBuf::from(name)
}

/// Preserve Lua's defaults and inherited module path, adding the selected
/// binary's matching JIT toolkit when installed beside it.
pub fn lua_path_value() -> String {
    lua_path_for(
        &luajit_path(),
        std::env::var("LUA_PATH").unwrap_or_else(|_| ";;".into()),
    )
}

/// Add the toolkit belonging to this binary to a host-selected module path.
pub fn lua_path_for(luajit: &Path, mut value: String) -> String {
    if let Some(prefix) = luajit.parent().and_then(Path::parent) {
        let share = prefix.join("share/luajit-2.1");
        if share.is_dir() {
            if !value.ends_with(';') {
                value.push(';');
            }
            value.push_str(&format!("{}/?.lua;", share.display()));
        }
    }
    value
}

/// Filename of the assembled Lua output inside an output directory.
pub const MAIN_LUA_FILENAME: &str = "main.lua";

/// Filename of the precompiled LuaJIT bytecode image of `main.lua`, beside it
/// in the same output directory. Optional everywhere: a consumer that cannot
/// find it runs the source file instead.
pub const MAIN_BYTECODE_FILENAME: &str = "main.ljbc";

/// Build a `Command` that dumps `lua_file` to LuaJIT bytecode at
/// `bytecode_file`.
///
/// `-g` keeps debug info, which is not optional: the dumped chunk carries the
/// SOURCE file's chunkname and line table, so a runtime error raised from the
/// bytecode image renders the same `main.lua:1234:` text a source run renders.
/// LuaJIT's `-b` default is `-s` (strip), which would renumber every traceback
/// a sim or a game can print.
///
/// `-b` consumes the rest of the argument list, so this cannot go through
/// `build_luajit_command` (which puts the script first). `LUA_PATH` is the same
/// value, and it is required rather than incidental: `-b` is implemented by
/// `jit/bcsave.lua` under `<prefix>/share/luajit-2.1/`.
pub fn build_luajit_bytecode_command(
    lua_file: &Path,
    bytecode_file: &Path,
) -> std::process::Command {
    let mut cmd = std::process::Command::new(luajit_path());
    cmd.arg("-b")
        .arg("-g")
        .arg(lua_file)
        .arg(bytecode_file)
        .env("LUA_PATH", lua_path_value());
    cmd
}

/// Default output directory for compiled programs.
pub const DEFAULT_OUTPUT_DIR: &str = "tmp";

/// The canonical default location for the compiled program:
/// `tmp/main.lua`. Used as both the compile target and the file
/// executed by `rock run`.
pub fn default_main_lua_path() -> PathBuf {
    PathBuf::from(DEFAULT_OUTPUT_DIR).join(MAIN_LUA_FILENAME)
}

/// Write `contents` to `path` atomically: write to a uniquely-named temp
/// file in the same directory, then `rename(2)` into place (atomic on
/// macOS + Linux). A concurrent reader -- a running game polling main.lua
/// for hot reload, or a parallel `rock frame` -- always sees either the
/// complete old or the complete new file, never a truncated mid-write
/// (which the hot-reload poller would `dofile` as an empty program,
/// clearing the system registries and blanking the screen).
///
/// The temp name is PID + a process-global counter, so concurrent
/// compiler instances (e.g. parallel tests) and concurrent writes within
/// one process never collide on the temp path. The single atomic-write
/// path shared by every generated-Lua writer (rock + skull backends).
pub fn write_atomic(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> std::io::Result<()> {
    write_atomic_impl(path.as_ref(), contents.as_ref(), false)
}

/// Stream a generated artifact into a same-directory temporary file before
/// atomically replacing the destination. Writer errors leave the destination intact.
pub fn write_atomic_streamed(
    path: impl AsRef<Path>,
    write: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
) -> std::io::Result<()> {
    write_atomic_with(path.as_ref(), write, false)
}

fn write_atomic_impl(path: &Path, contents: &[u8], durable: bool) -> std::io::Result<()> {
    use std::io::Write;
    write_atomic_with(path, |file| file.write_all(contents), durable)
}

fn write_atomic_with(
    path: &Path,
    write: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
    durable: bool,
) -> std::io::Result<()> {
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    fs_err::create_dir_all(parent)?;

    // Temp file in the SAME directory so `persist` is a same-filesystem
    // (atomic) rename. NamedTempFile gives an OS-level unique name -- no
    // collisions between concurrent compiler / test instances -- and is
    // removed on any early return, so a failed write leaves no `.tmp`
    // litter. Generated artifacts skip fsync; authored documents request it.
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    write(tmp.as_file_mut())?;
    if durable {
        tmp.as_file().sync_all()?;
    }
    tmp.persist(path).map_err(|e| e.error)?;
    #[cfg(unix)]
    if durable {
        fs_err::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

/// Plain LuaJIT process setup. Hosts configure their runtime service before stdio.
pub fn build_luajit_command(lua_file: &Path, app_args: &[String]) -> std::process::Command {
    let mut cmd = build_luajit_stdin_command();
    cmd.arg(lua_file).args(app_args);
    cmd
}

pub fn build_luajit_stdin_command() -> std::process::Command {
    let mut cmd = std::process::Command::new(luajit_path());
    crate::compiler_thp::configure_runtime_command(&mut cmd);
    cmd.env("LUA_PATH", lua_path_value());
    cmd
}

/// Recursively find every `.rock` file under `source_dir` (absolute paths,
/// `~`-suffixed editor backups skipped). Surfaces per-entry walk errors to
/// stderr rather than silently dropping them — a permission error on stdlib/
/// would otherwise masquerade as "module not found" (silent-errors audit).
pub fn find_rock_files_at_path(source_dir: impl AsRef<Path>) -> Result<Vec<String>> {
    use anyhow::Context;
    use walkdir::WalkDir;

    // Canonicalize the base directory once — walkdir will produce absolute paths
    // from this base, so individual files don't need per-file canonicalize.
    let canonical_base = fs_err::canonicalize(source_dir.as_ref()).with_context(|| {
        format!(
            "Failed to resolve source directory '{}'",
            source_dir.as_ref().display()
        )
    })?;

    let mut files = Vec::new();

    // Sort each directory's entries by name so discovery order is independent of
    // the filesystem's `read_dir` order (which is stable per working-tree state
    // but shifts when files are rewritten -- e.g. a `git checkout` between builds).
    // Module order flows into `ws.modules` and function-chunk emission order, so
    // an unsorted walk makes the emitted `.lua` non-reproducible across builds
    // (unfuck-backlog #58).
    for entry in WalkDir::new(&canonical_base).sort_by_file_name() {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                eprintln!(
                    "rock: skipping path under `{}` while scanning for .rock files: {e}",
                    canonical_base.display()
                );
                continue;
            }
        };
        let path = entry.path();

        if path.is_file() {
            if let Some(ext) = path.extension() {
                if ext == "rock" && !path.to_string_lossy().ends_with("~") {
                    files.push(path.to_string_lossy().to_string());
                }
            }
        }
    }

    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs_err as fs;
    use tempfile::TempDir;

    #[test]
    fn plain_luajit_locator_obeys_override_sibling_and_path_order() {
        let root = TempDir::new().unwrap();
        let bin = root.path().join("bin");
        let path_dir = root.path().join("path");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&path_dir).unwrap();
        let name = if cfg!(windows) {
            "luajit.exe"
        } else {
            "luajit"
        };
        let sibling = bin.join(name);
        let from_path = path_dir.join(name);
        fs::write(&sibling, "").unwrap();
        fs::write(&from_path, "").unwrap();
        let exe = bin.join("rock");
        let path = std::env::join_paths([&path_dir]).unwrap();
        let explicit = root.path().join("diagnostic-luajit");
        assert_eq!(
            locate_luajit(
                Some(explicit.clone().into_os_string()),
                Some(&exe),
                Some(&path)
            ),
            explicit
        );
        assert_eq!(locate_luajit(None, Some(&exe), Some(&path)), sibling);
        fs::remove_file(&sibling).unwrap();
        assert_eq!(
            locate_luajit(Some("".into()), Some(&exe), Some(&path)),
            from_path
        );
        assert_eq!(locate_luajit(None, None, None), PathBuf::from(name));
    }

    #[test]
    fn test_find_project_root_with_stick_toml() {
        let temp = TempDir::new().unwrap();
        // Canonicalize the temp path to resolve symlinks (e.g., /var -> /private/var on macOS)
        let project_dir = temp.path().canonicalize().unwrap();

        // Create stick.toml
        fs::write(project_dir.join("stick.toml"), "[package]\nname = \"test\"").unwrap();

        // Create a subdirectory with a file
        let sub_dir = project_dir.join("src");
        fs::create_dir(&sub_dir).unwrap();
        let file_path = sub_dir.join("main.rock");
        fs::write(&file_path, "fn main() {}").unwrap();

        // Should find project root from file in subdirectory
        assert_eq!(
            find_project_root(&file_path),
            Some(project_dir.to_path_buf())
        );

        // Should find project root from subdirectory
        assert_eq!(find_project_root(&sub_dir), Some(project_dir.to_path_buf()));

        // Should find project root from project directory itself
        assert_eq!(
            find_project_root(&project_dir),
            Some(project_dir.to_path_buf())
        );
    }

    #[test]
    fn test_find_project_root_with_git() {
        let temp = TempDir::new().unwrap();
        let project_dir = temp.path().canonicalize().unwrap();

        // Create .git directory (but no stick.toml)
        fs::create_dir(project_dir.join(".git")).unwrap();

        let file_path = project_dir.join("main.rock");
        fs::write(&file_path, "fn main() {}").unwrap();

        // .git alone is NOT a project marker (only stick.toml is), so should return None
        assert_eq!(find_project_root(&file_path), None);
    }

    #[test]
    fn test_find_project_root_none() {
        let temp = TempDir::new().unwrap();
        let file_path = temp.path().join("main.rock");
        fs::write(&file_path, "fn main() {}").unwrap();

        // No project markers, should return None
        assert_eq!(find_project_root(&file_path), None);
    }

    #[test]
    fn rock_luajit_env_selects_the_diagnostic_runtime() {
        const CHILD: &str = "ROCK_LUAJIT_OVERRIDE_WITNESS";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "project::tests::rock_luajit_env_selects_the_diagnostic_runtime",
                ])
                .env(CHILD, "1")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }

        let prev = std::env::var_os("ROCK_LUAJIT");
        let diagnostic = std::path::PathBuf::from("/diagnostics/tarantool-luajit");

        unsafe { std::env::set_var("ROCK_LUAJIT", &diagnostic) };
        assert_eq!(luajit_path(), diagnostic);

        unsafe {
            match prev {
                Some(v) => std::env::set_var("ROCK_LUAJIT", v),
                None => std::env::remove_var("ROCK_LUAJIT"),
            }
        }
    }

    #[test]
    fn release_root_requires_the_rock_executable() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join("bin")).unwrap();
        fs::create_dir_all(root.join("lib")).unwrap();
        assert!(!is_rock_root(root));
        assert!(!is_release_layout_in(root));
        fs::write(
            root.join("bin")
                .join(format!("rock{}", std::env::consts::EXE_SUFFIX)),
            "",
        )
        .unwrap();
        assert!(is_rock_root(root));
        assert!(is_release_layout_in(root));
    }

    #[test]
    fn context_root_env_admits_a_project_sibling_to_the_sdk() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("sdk")).unwrap();
        fs::create_dir_all(root.join("game")).unwrap();
        fs::create_dir_all(root.join(".context")).unwrap();
        fs::write(root.join(".context/context.json"), "{}\n").unwrap();

        let previous = std::env::var_os("ROCK_CONTEXT_ROOT");
        unsafe { std::env::set_var("ROCK_CONTEXT_ROOT", &root) };
        let found = find_workspace_root(&root.join("game"));
        unsafe {
            match previous {
                Some(value) => std::env::set_var("ROCK_CONTEXT_ROOT", value),
                None => std::env::remove_var("ROCK_CONTEXT_ROOT"),
            }
        }

        assert_eq!(found, Some(root));
    }
}
