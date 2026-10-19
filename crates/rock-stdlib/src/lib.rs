//! The Rock stdlib source files: their names, load order, ids and where they
//! come from at runtime (they are not embedded). A native process reads them
//! from a directory; a host without a filesystem installs them from memory
//! with [`install_stdlib`] before the first compile.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// A stdlib source file: its short name, full contents, and the path editors
/// and diagnostics name it by.
pub struct StdlibFile {
    pub name: &'static str,
    pub contents: &'static str,
    /// The file on disk for a stdlib read from a directory; `<dir>/<name>`
    /// under the host's display directory for one installed from memory.
    pub display_path: &'static str,
}

static FILES: OnceLock<Vec<StdlibFile>> = OnceLock::new();

/// Why [`install_stdlib`] refused a file set.
#[derive(Debug, PartialEq, Eq)]
pub enum StdlibInstallError {
    /// The stdlib was already installed or read from disk; it cannot change
    /// within a process.
    AlreadyLoaded,
    UnknownFile(String),
    DuplicateFile(String),
    MissingFile(&'static str),
}

impl std::fmt::Display for StdlibInstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyLoaded => write!(f, "the stdlib is already loaded in this process"),
            Self::UnknownFile(name) => write!(f, "`{name}` is not a stdlib file"),
            Self::DuplicateFile(name) => write!(f, "stdlib file `{name}` was supplied twice"),
            Self::MissingFile(name) => write!(f, "stdlib file `{name}` was not supplied"),
        }
    }
}

impl std::error::Error for StdlibInstallError {}

/// Install the stdlib from memory, for a host with no filesystem. `files` are
/// `(name, contents)` pairs naming every file in [`STDLIB_NAMES`] exactly
/// once, in any order; each is displayed as `<display_dir>/<name>` instead of
/// a path on the machine that built the compiler. Must run before anything
/// reads [`stdlib_files`].
pub fn install_stdlib(
    display_dir: &str,
    files: impl IntoIterator<Item = (String, String)>,
) -> Result<(), StdlibInstallError> {
    if FILES.get().is_some() {
        return Err(StdlibInstallError::AlreadyLoaded);
    }
    let files = files_from_memory(display_dir, files)?;
    FILES.set(files).map_err(|_| StdlibInstallError::AlreadyLoaded)
}

fn files_from_memory(
    display_dir: &str,
    files: impl IntoIterator<Item = (String, String)>,
) -> Result<Vec<StdlibFile>, StdlibInstallError> {
    let mut slots: Vec<Option<String>> = STDLIB_NAMES.iter().map(|_| None).collect();
    for (name, contents) in files {
        let Some(index) = STDLIB_NAMES.iter().position(|n| *n == name) else {
            return Err(StdlibInstallError::UnknownFile(name));
        };
        if slots[index].replace(contents).is_some() {
            return Err(StdlibInstallError::DuplicateFile(name));
        }
    }
    let display_dir = display_dir.trim_end_matches('/');
    STDLIB_NAMES
        .iter()
        .zip(slots)
        .map(|(&name, contents)| {
            let contents = contents.ok_or(StdlibInstallError::MissingFile(name))?;
            Ok(StdlibFile {
                name,
                contents: Box::leak(contents.into_boxed_str()),
                display_path: Box::leak(format!("{display_dir}/{name}").into_boxed_str()),
            })
        })
        .collect()
}

/// The stdlib files, in load order. `unified.rock` is the core stdlib;
/// `strings.rock` (portable string algorithms) and `json.rock` build on it.
/// `js.rock` holds JS-backend bodies for stdlib fns defined in the others, and
/// is LAST so adding one shifts no `__SITE__` line in a lua-emitting file.
pub const STDLIB_NAMES: &[&str] = &[
    "unified.rock",
    "logging.rock",
    "vec.rock",
    "mat.rock",
    "arena.rock",
    "scene.rock",
    "strings.rock",
    "json.rock",
    "shader.rock",
    "js.rock",
];

/// The `stdlib/` of the source checkout this crate was built from.
const BUILD_CHECKOUT_STDLIB: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../stdlib");

/// Directory the stdlib `.rock` files are read from. A fresh process (or LSP
/// restart) picks up stdlib edits. Order: `ROCK_STDLIB_DIR`; else
/// `<root>/stdlib` when the running exe is `<root>/bin/<exe>` (the release
/// packaged installation layout); else the build checkout's `stdlib/`.
pub fn stdlib_dir() -> PathBuf {
    resolve_stdlib_dir(
        std::env::var_os("ROCK_STDLIB_DIR").map(PathBuf::from),
        std::env::current_exe().ok().as_deref(),
        Path::new(BUILD_CHECKOUT_STDLIB),
        |p| p.is_dir(),
    )
}

fn resolve_stdlib_dir(
    override_dir: Option<PathBuf>,
    exe: Option<&Path>,
    build_checkout_stdlib: &Path,
    is_dir: impl Fn(&Path) -> bool,
) -> PathBuf {
    if let Some(dir) = override_dir {
        return dir;
    }
    let release_stdlib = exe
        .and_then(Path::parent)
        .filter(|bin| bin.file_name().is_some_and(|n| n == "bin"))
        .and_then(Path::parent)
        .map(|root| root.join("stdlib"))
        .filter(|dir| is_dir(dir));
    release_stdlib.unwrap_or_else(|| build_checkout_stdlib.to_path_buf())
}

/// All stdlib files: the ones [`install_stdlib`] supplied, else read from
/// [`stdlib_dir`] on first call; cached for the process lifetime. Contents are leaked to `&'static str`: the stdlib is resident for
/// the whole process, so a one-time leak of a bounded, always-live buffer is
/// not an unbounded leak. Panics with the resolved path if a file can't be
/// read (the stdlib is not optional; a clear failure beats a thousand
/// downstream "callee not found"s).
pub fn stdlib_files() -> &'static [StdlibFile] {
    FILES.get_or_init(|| {
        let dir = stdlib_dir();
        STDLIB_NAMES
            .iter()
            .map(|&name| {
                let path = dir.join(name);
                let contents = std::fs::read_to_string(&path).unwrap_or_else(|e| {
                    panic!(
                        "rock: cannot read stdlib `{}`: {e}\n  (resolved stdlib dir: {}; \
                         override with ROCK_STDLIB_DIR)",
                        path.display(),
                        dir.display()
                    )
                });
                StdlibFile {
                    name,
                    contents: Box::leak(contents.into_boxed_str()),
                    display_path: Box::leak(path.to_string_lossy().into_owned().into_boxed_str()),
                }
            })
            .collect()
    })
}

/// Contents of stdlib file `name` (one of [`STDLIB_NAMES`]). Panics on an
/// unknown name: callers name a stdlib file they know exists.
pub fn stdlib_source(name: &str) -> &'static str {
    stdlib_files()
        .iter()
        .find(|f| f.name == name)
        .unwrap_or_else(|| panic!("rock: `{name}` is not a stdlib file (see STDLIB_NAMES)"))
        .contents
}

/// The source-file id the loaders give stdlib file `name`. Bone reads the
/// compiler-owned `std` package and the std-only primitives from this
/// spelling (`bone_core::modules::is_stdlib_module_file`), so every host that
/// feeds bone the stdlib -- the CLI and the editor alike -- keys it this way.
pub fn stdlib_file_id(name: &str) -> String {
    format!("<stdlib>/{name}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHECKOUT: &str = "/build/rock/stdlib";

    fn resolve(override_dir: Option<&str>, exe: Option<&str>, dirs: &[&str]) -> PathBuf {
        resolve_stdlib_dir(
            override_dir.map(PathBuf::from),
            exe.map(Path::new),
            Path::new(CHECKOUT),
            |p| dirs.iter().any(|d| Path::new(d) == p),
        )
    }

    #[test]
    fn release_layout_stdlib_wins_over_build_checkout() {
        assert_eq!(
            resolve(None, Some("/opt/rock/bin/rock"), &["/opt/rock/stdlib"]),
            PathBuf::from("/opt/rock/stdlib")
        );
    }

    #[test]
    fn override_wins_over_release_layout_and_build_checkout() {
        assert_eq!(
            resolve(Some("/controlled/stdlib"), Some("/opt/rock/bin/rock"), &["/opt/rock/stdlib"]),
            PathBuf::from("/controlled/stdlib")
        );
    }

    #[test]
    fn exe_outside_a_release_layout_uses_build_checkout() {
        // A dev exe under target/ and a bin/ without a sibling stdlib/ both
        // fall through to the checkout the crate was built from.
        assert_eq!(
            resolve(None, Some("/work/rock/target/debug/rock"), &["/work/rock/target/stdlib"]),
            PathBuf::from(CHECKOUT)
        );
        assert_eq!(resolve(None, Some("/usr/bin/rock"), &[]), PathBuf::from(CHECKOUT));
        assert_eq!(resolve(None, None, &[]), PathBuf::from(CHECKOUT));
    }

    fn memory_files(names: &[&str]) -> Vec<(String, String)> {
        names.iter().map(|n| (n.to_string(), format!("// {n}"))).collect()
    }

    #[test]
    fn memory_stdlib_keeps_load_order_and_neutral_display_paths() {
        let mut names = STDLIB_NAMES.to_vec();
        names.reverse();
        let files = files_from_memory("/stdlib/", memory_files(&names)).unwrap();
        let got: Vec<_> = files.iter().map(|f| (f.name, f.display_path.to_string())).collect();
        let want: Vec<_> = STDLIB_NAMES.iter().map(|n| (*n, format!("/stdlib/{n}"))).collect();
        assert_eq!(got, want);
        assert_eq!(files[0].contents, "// unified.rock");
    }

    #[test]
    fn memory_stdlib_rejects_unknown_duplicate_and_missing_files() {
        let mut files = memory_files(STDLIB_NAMES);
        files.push(("extra.rock".into(), String::new()));
        assert_eq!(
            files_from_memory("/stdlib", files).err(),
            Some(StdlibInstallError::UnknownFile("extra.rock".into()))
        );
        let mut files = memory_files(STDLIB_NAMES);
        files.push(("json.rock".into(), String::new()));
        assert_eq!(
            files_from_memory("/stdlib", files).err(),
            Some(StdlibInstallError::DuplicateFile("json.rock".into()))
        );
        assert_eq!(
            files_from_memory("/stdlib", memory_files(&STDLIB_NAMES[1..])).err(),
            Some(StdlibInstallError::MissingFile("unified.rock"))
        );
    }

    #[test]
    fn build_checkout_stdlib_holds_every_file() {
        for name in STDLIB_NAMES {
            let path = Path::new(BUILD_CHECKOUT_STDLIB).join(name);
            assert!(path.is_file(), "missing {}", path.display());
        }
    }
}
