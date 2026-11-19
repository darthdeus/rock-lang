//! The Rock stdlib source files: their names, load order, ids and where they
//! come from at runtime. A native process reads them from a directory when one
//! resolves ([`stdlib_origin`]) and otherwise serves the copy compiled into
//! the binary; without a filesystem only the embedded copy exists.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub struct StdlibFile {
    pub name: &'static str,
    pub contents: &'static str,
    pub display_path: &'static str,
}

static FILES: OnceLock<&'static [StdlibFile]> = OnceLock::new();

macro_rules! define_stdlib_files {
    ($($name:literal),+ $(,)?) => {
        /// The stdlib files, in load order. `unified.rock` is the core stdlib;
        /// `strings.rock` (portable string algorithms) and `json.rock` build on it.
        /// `js.rock` holds JS-backend bodies for stdlib fns defined in the others, and
        /// is LAST so adding one shifts no `__SITE__` line in a lua-emitting file.
        pub const STDLIB_NAMES: &[&str] = &[$($name),+];

        /// The stdlib compiled into the binary: what [`stdlib_files`] serves when no
        /// rung resolves on disk. `display_path` names no real file; on wasm it is
        /// `/stdlib/<name>` so the playground keeps opening `file:///stdlib/<name>`,
        /// elsewhere diagnostics show the `<embedded-stdlib>/` prefix.
        static EMBEDDED_FILES: &[StdlibFile] = &[
            $(StdlibFile {
                name: $name,
                contents: include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../stdlib/", $name)),
                #[cfg(target_arch = "wasm32")]
                display_path: concat!("/stdlib/", $name),
                #[cfg(not(target_arch = "wasm32"))]
                display_path: concat!("<embedded-stdlib>/", $name),
            }),+
        ];
    };
}

define_stdlib_files!(
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
);

#[cfg(not(target_arch = "wasm32"))]
const BUILD_CHECKOUT_STDLIB: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../stdlib");

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
    #[cfg(target_arch = "wasm32")]
    {
        StdlibSource::Embedded
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

#[cfg(not(target_arch = "wasm32"))]
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

pub fn stdlib_files() -> &'static [StdlibFile] {
    FILES.get_or_init(|| match stdlib_origin() {
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
            .collect::<Vec<_>>()
            .leak(),
        StdlibSource::Embedded => EMBEDDED_FILES,
    })
}

pub use rock_core::{is_stdlib_module_file, stdlib_file_id, stdlib_file_name};

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
        assert_eq!(
            resolve(Some("/controlled/stdlib"), Some("/opt/rock/bin/rock"), &[]),
            StdlibSource::Dir(PathBuf::from("/controlled/stdlib"))
        );
    }

    #[test]
    fn exe_outside_a_release_layout_uses_build_checkout() {
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
    fn embedded_stdlib_matches_the_checkout() {
        for file in EMBEDDED_FILES {
            let name = file.name;
            let on_disk = std::fs::read_to_string(Path::new(BUILD_CHECKOUT_STDLIB).join(name))
                .expect("checkout stdlib readable");
            assert_eq!(file.contents, on_disk, "{name} drifted from the checkout");
            assert_eq!(file.display_path, format!("<embedded-stdlib>/{name}"));
        }
    }
}
