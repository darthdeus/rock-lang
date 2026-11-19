use anyhow::Result;
use std::path::{Path, PathBuf};

fn is_rock_root(path: &Path) -> bool {
    is_dev_workspace(path) || is_release_layout_in(path)
}

pub(crate) fn is_dev_workspace(path: &Path) -> bool {
    path.join("Cargo.toml").exists() && path.join("crates").is_dir()
}

pub fn find_workspace_root(path: &Path) -> Option<PathBuf> {
    context_root_from_env()
        .or_else(release_root_from_exe)
        .or_else(|| find_ancestor(path, is_rock_root))
}

pub fn find_source_root(path: &Path) -> Option<PathBuf> {
    context_root_from_env().or_else(|| find_ancestor(path, is_dev_workspace))
}

fn find_ancestor(path: &Path, is_root: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    let start = fs_err::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    start
        .ancestors()
        .find(|dir| is_root(dir))
        .map(Path::to_path_buf)
}

pub(crate) fn context_root_from_env() -> Option<PathBuf> {
    let root = PathBuf::from(std::env::var_os("ROCK_CONTEXT_ROOT")?);
    if root.is_dir() && root.join("sdk").is_dir() && root.join(".context/context.json").is_file() {
        Some(root)
    } else {
        None
    }
}

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

pub fn find_project_root(path: &Path) -> Option<PathBuf> {
    find_ancestor(path, |dir| dir.join("stick.toml").exists())
}

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

pub fn lua_path_value() -> String {
    lua_path_for(
        &luajit_path(),
        std::env::var("LUA_PATH").unwrap_or_else(|_| ";;".into()),
    )
}

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

pub const MAIN_LUA_FILENAME: &str = "main.lua";

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
/// `-b` must come before the script, which every other LuaJIT command puts
/// first. `LUA_PATH` is required rather than incidental: `-b` is implemented by
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

pub(crate) const DEFAULT_OUTPUT_DIR: &str = "tmp";

pub fn default_main_lua_path() -> PathBuf {
    PathBuf::from(DEFAULT_OUTPUT_DIR).join(MAIN_LUA_FILENAME)
}

pub fn write_atomic(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> std::io::Result<()> {
    use std::io::Write;
    write_atomic_streamed(path, |file| file.write_all(contents.as_ref()))
}

pub fn write_atomic_streamed(
    path: impl AsRef<Path>,
    write: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let path = path.as_ref();
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    fs_err::create_dir_all(parent)?;

    // Same directory so the rename is atomic: a hot-reload poller must never read a truncated file.
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    write(tmp.as_file_mut())?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

pub fn luajit_command() -> std::process::Command {
    let mut cmd = std::process::Command::new(luajit_path());
    crate::compiler_thp::configure_runtime_command(&mut cmd);
    cmd.env("LUA_PATH", lua_path_value());
    cmd
}

pub struct WalkedRockFile {
    pub path: String,
    pub symlink: bool,
}

pub fn find_rock_files_at_path(source_dir: impl AsRef<Path>) -> Result<Vec<String>> {
    Ok(walk_rock_files(source_dir)?
        .into_iter()
        .map(|file| file.path)
        .collect())
}

pub fn walk_rock_files(source_dir: impl AsRef<Path>) -> Result<Vec<WalkedRockFile>> {
    use anyhow::Context;
    use walkdir::WalkDir;

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

        if path.extension().is_none_or(|ext| ext != "rock") {
            continue;
        }
        let file_type = entry.file_type();
        let is_file = if file_type.is_symlink() {
            match fs_err::metadata(path) {
                Ok(metadata) => metadata.is_file(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(error) => return Err(error.into()),
            }
        } else {
            file_type.is_file()
        };
        if is_file {
            files.push(WalkedRockFile {
                path: path.to_string_lossy().to_string(),
                symlink: file_type.is_symlink(),
            });
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
    fn find_project_root_walks_up_to_the_nearest_stick_toml() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let loose = root.join("loose.rock");
        fs::write(&loose, "fn main() {}").unwrap();
        fs::create_dir(root.join(".git")).unwrap();
        assert_eq!(find_project_root(&loose), None);

        let project = root.join("my.game");
        let src = project.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(project.join("stick.toml"), "[package]\nname = \"test\"").unwrap();
        let main = src.join("main.rock");
        fs::write(&main, "fn main() {}").unwrap();
        for start in [&main, &src, &project, &src.join("unsaved.rock")] {
            assert_eq!(find_project_root(start), Some(project.clone()), "{start:?}");
        }
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
        let found_source = find_source_root(&root.join("game"));
        unsafe {
            match previous {
                Some(value) => std::env::set_var("ROCK_CONTEXT_ROOT", value),
                None => std::env::remove_var("ROCK_CONTEXT_ROOT"),
            }
        }

        assert_eq!(found, Some(root.clone()));
        assert_eq!(found_source, Some(root));
    }
}
