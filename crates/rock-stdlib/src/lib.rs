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
    let release_stdlib = install_sources(exe, "stdlib").filter(|dir| is_dir(dir));
    if let Some(dir) = release_stdlib {
        return StdlibSource::Dir(dir);
    }
    if is_dir(build_checkout_stdlib) {
        return StdlibSource::Dir(build_checkout_stdlib.to_path_buf());
    }
    StdlibSource::Embedded
}

/// Resolve a named package from this toolchain's sources. An explicit override
/// or existing install package directory is authoritative: a missing package
/// there must not borrow sources from a different version's checkout.
pub fn package_dir(name: &str) -> Result<PathBuf, String> {
    resolve_package(
        name,
        std::env::var_os("ROCK_PACKAGES_DIR").map(PathBuf::from),
        std::env::current_exe().ok().as_deref(),
        Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../packages")),
    )
}

fn resolve_package(
    name: &str,
    override_dir: Option<PathBuf>,
    exe: Option<&Path>,
    checkout: &Path,
) -> Result<PathBuf, String> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        return Err(format!(
            "invalid package name `{name}`; expected letters, digits, '-' or '_'"
        ));
    }
    let mut tried = Vec::new();
    let mut lookup = |root: &Path| {
        let package = root.join(name);
        tried.push(package.join("stick.toml"));
        package
    };
    let package = if let Some(root) = override_dir {
        lookup(&root)
    } else if let Some(root) = install_sources(exe, "packages").filter(|root| root.is_dir()) {
        lookup(&root)
    } else {
        // Include an absent install location in diagnostics before source lookup.
        if let Some(root) = install_sources(exe, "packages") {
            lookup(&root);
        }
        lookup(checkout)
    };
    if package.join("stick.toml").is_file() {
        return Ok(package);
    }
    Err(format!(
        "cannot resolve package `{name}`; looked for {}",
        tried
            .iter()
            .map(|p| format!("`{}`", p.display()))
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

fn install_sources(exe: Option<&Path>, directory: &str) -> Option<PathBuf> {
    exe.and_then(Path::parent)
        .filter(|bin| bin.file_name().is_some_and(|n| n == "bin"))
        .and_then(Path::parent)
        .map(|root| root.join(directory))
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

/// Serve no stdlib files for the rest of this process (`rock --no-stdlib`).
/// Every compile path reads its stdlib modules through [`stdlib_files`], so
/// this one switch removes them from check, compile, run and test alike.
/// Errors if the stdlib was already loaded.
pub fn exclude_stdlib() -> Result<(), &'static str> {
    if FILES.set(Vec::new()).is_ok() || FILES.get().is_some_and(Vec::is_empty) {
        Ok(())
    } else {
        Err("the stdlib was loaded before it could be excluded")
    }
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

/// Stdlib source identity: the loaders key every stdlib file by
/// [`stdlib_file_id`], and bone reads the compiler-owned `std` package and the
/// std-only primitives back through [`is_stdlib_module_file`]. Defined in
/// `rock-core` so the semantic crates share it without depending on the
/// embedded stdlib sources.
pub use rock_core::{is_stdlib_module_file, stdlib_file_id, stdlib_file_name};

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
        "unified.rock" => include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../stdlib/unified.rock"
        )),
        "logging.rock" => include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../stdlib/logging.rock"
        )),
        "vec.rock" => include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../stdlib/vec.rock"
        )),
        "mat.rock" => include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../stdlib/mat.rock"
        )),
        "arena.rock" => include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../stdlib/arena.rock"
        )),
        "scene.rock" => include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../stdlib/scene.rock"
        )),
        "strings.rock" => include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../stdlib/strings.rock"
        )),
        "json.rock" => include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../stdlib/json.rock"
        )),
        "shader.rock" => include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../stdlib/shader.rock"
        )),
        "js.rock" => include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../stdlib/js.rock")),
        _ => panic!("rock: `{name}` is not a stdlib file (see STDLIB_NAMES)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package_fixture(root: &Path, value: &str) -> PathBuf {
        let package = root.join("probe");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(package.join("stick.toml"), value).unwrap();
        package
    }

    #[test]
    fn packages_use_override_then_install_then_build_checkout() {
        let temp = tempfile::tempdir().unwrap();
        let exe = temp.path().join("install/bin/rock");
        let checkout = temp.path().join("checkout/packages");
        let built = package_fixture(&checkout, "build");
        assert_eq!(
            resolve_package("probe", None, Some(&exe), &checkout).unwrap(),
            built
        );
        let installed = package_fixture(&temp.path().join("install/packages"), "install");
        assert_eq!(
            resolve_package("probe", None, Some(&exe), &checkout).unwrap(),
            installed
        );
        let override_root = temp.path().join("override");
        let overridden = package_fixture(&override_root, "override");
        assert_eq!(
            resolve_package("probe", Some(override_root), Some(&exe), &checkout).unwrap(),
            overridden
        );
    }

    #[test]
    fn a_missing_package_never_borrows_from_another_version() {
        let temp = tempfile::tempdir().unwrap();
        let exe = temp.path().join("install/bin/rock");
        let checkout = temp.path().join("checkout/packages");
        package_fixture(&checkout, "build");
        let installed = temp.path().join("install/packages");
        std::fs::create_dir_all(&installed).unwrap();
        let error = resolve_package("probe", None, Some(&exe), &checkout).unwrap_err();
        assert!(
            error.contains("probe")
                && error.contains(&installed.join("probe/stick.toml").display().to_string()),
            "{error}"
        );
        let override_root = temp.path().join("missing-override");
        let error = resolve_package("probe", Some(override_root.clone()), Some(&exe), &checkout)
            .unwrap_err();
        assert!(
            error.contains(&override_root.join("probe/stick.toml").display().to_string()),
            "{error}"
        );
    }

    #[test]
    fn missing_packages_name_every_searched_manifest() {
        let temp = tempfile::tempdir().unwrap();
        let exe = temp.path().join("install/bin/rock");
        let checkout = temp.path().join("checkout/packages");
        let error = resolve_package("absent", None, Some(&exe), &checkout).unwrap_err();
        for path in [
            temp.path().join("install/packages/absent/stick.toml"),
            checkout.join("absent/stick.toml"),
        ] {
            assert!(error.contains(&path.display().to_string()), "{error}");
        }
        for name in ["", "..", "../escape", "/absolute", "a/b", "a\\b"] {
            assert!(
                resolve_package(name, None, Some(&exe), &checkout)
                    .unwrap_err()
                    .contains("invalid package name")
            );
        }
    }

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
            resolve(
                None,
                Some("/opt/rock/bin/rock"),
                &["/opt/rock/stdlib", CHECKOUT]
            ),
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
            resolve(
                None,
                Some("/work/rock/target/debug/rock"),
                &["/work/rock/target/stdlib", CHECKOUT]
            ),
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
        assert_eq!(
            resolve(None, Some("/usr/bin/rock"), &[]),
            StdlibSource::Embedded
        );
        assert_eq!(resolve(None, None, &[]), StdlibSource::Embedded);
        assert_eq!(
            resolve(None, Some("/opt/rock/bin/rock"), &[]),
            StdlibSource::Embedded
        );
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
            assert_eq!(
                embedded_source(name),
                on_disk,
                "{name} drifted from the checkout"
            );
        }
    }
}
