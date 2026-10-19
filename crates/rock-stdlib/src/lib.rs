//! The Rock stdlib source files: their names, load order, ids and where they
//! come from at runtime. A native process reads them from a directory when one
//! resolves ([`stdlib_origin`]) and otherwise serves the copy compiled into
//! the binary; without a filesystem only the embedded copy exists.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// A stdlib source file: its short name, full contents, and the path editors
/// and diagnostics name it by.
pub struct StdlibFile {
    pub name: &'static str,
    pub contents: &'static str,
    /// The file on disk for a stdlib read from a directory; `/stdlib/<name>`
    /// on wasm, `<embedded-stdlib>/<name>` for an embedded native stdlib.
    pub display_path: &'static str,
}

static FILES: OnceLock<Vec<StdlibFile>> = OnceLock::new();

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

/// Where the stdlib sources come from: a directory on disk, or the copy
/// compiled into the binary.
#[derive(Debug, PartialEq, Eq)]
pub enum StdlibSource {
    Dir(PathBuf),
    Embedded,
}

/// Where the stdlib sources come from in this process. A fresh process (or LSP
/// restart) picks up stdlib edits. Order: `ROCK_STDLIB_DIR`; else
/// `<root>/stdlib` when the running exe is `<root>/bin/<exe>` (the release
/// packaged installation layout); else the build checkout's `stdlib/` when
/// that directory exists; else the stdlib embedded in the binary.
pub fn stdlib_origin() -> StdlibSource {
    resolve_for_this_process()
}

/// Directory the stdlib `.rock` files are read from.
///
/// Kept only for the private caller until its next pin bump; new code uses
/// [`stdlib_origin`], which names the embedded rung truthfully instead of a
/// checkout path that may not exist. When the stdlib is embedded this names
/// the build checkout's `stdlib/` whether or not it exists; stdlib contents
/// always come from [`stdlib_files`], which serves the embedded copy in that
/// case.
pub fn stdlib_dir() -> PathBuf {
    match resolve_for_this_process() {
        StdlibSource::Dir(dir) => dir,
        StdlibSource::Embedded => PathBuf::from(BUILD_CHECKOUT_STDLIB),
    }
}

fn resolve_for_this_process() -> StdlibSource {
    #[cfg(target_arch = "wasm32")]
    {
        return StdlibSource::Embedded;
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        resolve_stdlib(
            std::env::var_os("ROCK_STDLIB_DIR").map(PathBuf::from),
            std::env::current_exe().ok().as_deref(),
            Path::new(BUILD_CHECKOUT_STDLIB),
            |p| p.is_dir(),
        )
    }
}

fn resolve_stdlib(
    override_dir: Option<PathBuf>,
    exe: Option<&Path>,
    build_checkout_stdlib: &Path,
    is_dir: impl Fn(&Path) -> bool,
) -> StdlibSource {
    if let Some(dir) = override_dir {
        return StdlibSource::Dir(dir);
    }
    let release_stdlib = exe
        .and_then(Path::parent)
        .filter(|bin| bin.file_name().is_some_and(|n| n == "bin"))
        .and_then(Path::parent)
        .map(|root| root.join("stdlib"))
        .filter(|dir| is_dir(dir));
    if let Some(dir) = release_stdlib {
        return StdlibSource::Dir(dir);
    }
    if is_dir(build_checkout_stdlib) {
        return StdlibSource::Dir(build_checkout_stdlib.to_path_buf());
    }
    StdlibSource::Embedded
}

/// All stdlib files: the first rung of [`stdlib_origin`]'s order that resolves
/// on this machine, else the copy embedded in the binary; cached for the
/// process lifetime. Contents are leaked to `&'static str`: the stdlib is resident for
/// the whole process, so a one-time leak of a bounded, always-live buffer is
/// not an unbounded leak. Panics with the resolved path if an on-disk rung
/// names files that can't be read (the stdlib is not optional; a clear failure
/// beats a thousand downstream "callee not found"s).
pub fn stdlib_files() -> &'static [StdlibFile] {
    FILES.get_or_init(|| match resolve_for_this_process() {
        StdlibSource::Dir(dir) => STDLIB_NAMES
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
            .collect(),
        StdlibSource::Embedded => embedded_files(),
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

/// The stdlib compiled into the binary: what [`stdlib_files`] serves when no
/// rung resolves on disk. `display_path` names no real file; on wasm it is
/// `/stdlib/<name>` so the playground keeps opening `file:///stdlib/<name>`,
/// elsewhere diagnostics show the `<embedded-stdlib>/` prefix.
fn embedded_files() -> Vec<StdlibFile> {
    #[cfg(target_arch = "wasm32")]
    let dir = "/stdlib";
    #[cfg(not(target_arch = "wasm32"))]
    let dir = "<embedded-stdlib>";
    STDLIB_NAMES
        .iter()
        .map(|&name| StdlibFile {
            name,
            contents: embedded_source(name),
            display_path: Box::leak(format!("{dir}/{name}").into_boxed_str()),
        })
        .collect()
}

fn embedded_source(name: &str) -> &'static str {
    match name {
        "unified.rock" => include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../stdlib/unified.rock")),
        "logging.rock" => include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../stdlib/logging.rock")),
        "vec.rock" => include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../stdlib/vec.rock")),
        "mat.rock" => include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../stdlib/mat.rock")),
        "arena.rock" => include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../stdlib/arena.rock")),
        "scene.rock" => include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../stdlib/scene.rock")),
        "strings.rock" => include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../stdlib/strings.rock")),
        "json.rock" => include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../stdlib/json.rock")),
        "shader.rock" => include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../stdlib/shader.rock")),
        "js.rock" => include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../stdlib/js.rock")),
        _ => panic!("rock: `{name}` is not a stdlib file (see STDLIB_NAMES)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHECKOUT: &str = "/build/rock/stdlib";

    fn resolve(override_dir: Option<&str>, exe: Option<&str>, dirs: &[&str]) -> StdlibSource {
        resolve_stdlib(
            override_dir.map(PathBuf::from),
            exe.map(Path::new),
            Path::new(CHECKOUT),
            |p| dirs.iter().any(|d| Path::new(d) == p),
        )
    }

    #[test]
    fn release_layout_stdlib_wins_over_build_checkout() {
        assert_eq!(
            resolve(None, Some("/opt/rock/bin/rock"), &["/opt/rock/stdlib", CHECKOUT]),
            StdlibSource::Dir(PathBuf::from("/opt/rock/stdlib"))
        );
    }

    #[test]
    fn override_wins_over_release_layout_and_build_checkout() {
        assert_eq!(
            resolve(
                Some("/controlled/stdlib"),
                Some("/opt/rock/bin/rock"),
                &["/opt/rock/stdlib", CHECKOUT]
            ),
            StdlibSource::Dir(PathBuf::from("/controlled/stdlib"))
        );
    }

    #[test]
    fn override_names_its_dir_even_when_missing() {
        // An explicit override that points nowhere stays a loud read error,
        // never a silent fallthrough to another rung.
        assert_eq!(
            resolve(Some("/controlled/stdlib"), Some("/opt/rock/bin/rock"), &[]),
            StdlibSource::Dir(PathBuf::from("/controlled/stdlib"))
        );
    }

    #[test]
    fn exe_outside_a_release_layout_uses_build_checkout() {
        // A dev exe under target/ and a bin/ without a sibling stdlib/ both
        // fall through to the checkout the crate was built from.
        assert_eq!(
            resolve(None, Some("/work/rock/target/debug/rock"), &["/work/rock/target/stdlib", CHECKOUT]),
            StdlibSource::Dir(PathBuf::from(CHECKOUT))
        );
        assert_eq!(
            resolve(None, Some("/usr/bin/rock"), &[CHECKOUT]),
            StdlibSource::Dir(PathBuf::from(CHECKOUT))
        );
    }

    #[test]
    fn no_stdlib_on_disk_falls_back_to_embedded() {
        // No override, no release-layout sibling, no checkout: an installed
        // binary outside any checkout serves the compiled-in stdlib.
        assert_eq!(resolve(None, Some("/usr/bin/rock"), &[]), StdlibSource::Embedded);
        assert_eq!(resolve(None, None, &[]), StdlibSource::Embedded);
        assert_eq!(resolve(None, Some("/opt/rock/bin/rock"), &[]), StdlibSource::Embedded);
    }

    #[test]
    fn build_checkout_stdlib_holds_every_file() {
        for name in STDLIB_NAMES {
            let path = Path::new(BUILD_CHECKOUT_STDLIB).join(name);
            assert!(path.is_file(), "missing {}", path.display());
        }
    }

    #[test]
    fn embedded_stdlib_matches_the_checkout() {
        for name in STDLIB_NAMES {
            let on_disk = std::fs::read_to_string(Path::new(BUILD_CHECKOUT_STDLIB).join(name))
                .expect("checkout stdlib readable");
            assert_eq!(embedded_source(name), on_disk, "{name} drifted from the checkout");
        }
    }
}
