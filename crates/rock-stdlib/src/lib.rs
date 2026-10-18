//! The Rock stdlib source files: their names, load order, ids and the
//! directory they are read from at runtime (they are not embedded).

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// A stdlib source file: its short name and full contents (read from disk).
pub struct StdlibFile {
    pub name: &'static str,
    pub contents: &'static str,
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

/// All stdlib files, read from disk on first call and cached for the process
/// lifetime. Contents are leaked to `&'static str`: the stdlib is resident for
/// the whole process, so a one-time leak of a bounded, always-live buffer is
/// not an unbounded leak. Panics with the resolved path if a file can't be
/// read (the stdlib is not optional; a clear failure beats a thousand
/// downstream "callee not found"s).
pub fn stdlib_files() -> &'static [StdlibFile] {
    static FILES: OnceLock<Vec<StdlibFile>> = OnceLock::new();
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

    #[test]
    fn build_checkout_stdlib_holds_every_file() {
        for name in STDLIB_NAMES {
            let path = Path::new(BUILD_CHECKOUT_STDLIB).join(name);
            assert!(path.is_file(), "missing {}", path.display());
        }
    }
}
