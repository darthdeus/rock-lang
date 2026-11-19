//! Generated Lua names source files relative to their package, so one package
//! compiled from two checkouts gives the same bytes.
use crate::support::{rock, write_files};
use std::path::Path;

/// An `app` using a `lib`, with explicit or default modules v1, under `base`.
fn compile_under(base: &Path, v1: bool) -> String {
    let language = if v1 {
        "\n[language]\nmodules = \"v1\"\n"
    } else {
        ""
    };
    write_files(
        base,
        &[
            (
                "lib/stick.toml",
                &format!(
                    "[package]\nname = \"lib\"\nversion = \"0.1.0\"\ntype = \"lib\"\n{language}"
                ),
            ),
            (
                "lib/lib.rock",
                "pub fn lib_site() -> string { __SITE__ }\n\
                 pub fn lib_apply(f: (int) -> int, x: int) -> int { f(x) }\n\
                 pub fn lib_offset(x: int) -> int { lib_apply((v) => v + x, 1) }\n",
            ),
            (
                "app/stick.toml",
                &format!(
                    "[package]\nname = \"app\"\nversion = \"0.1.0\"\ntype = \"bin\"\n{language}\n\
                     [dependencies]\nlib = {{ path = \"../lib\" }}\n"
                ),
            ),
            (
                "app/util.rock",
                "global COUNT: int = 0\n\
                 fn app_site() -> string { return __SITE__ }\n\
                 fn apply(f: (int) -> int, x: int) -> int { COUNT = COUNT + 1\n return f(x) }\n",
            ),
            (
                "app/main.rock",
                "use lib::{lib_offset, lib_site};\n\
                 fn main() {\n\
                 \x20   print(app_site())\n\
                 \x20   print(lib_site())\n\
                 \x20   print(\"{apply((v) => v + 1, 2)} {lib_offset(3)} {COUNT}\")\n\
                 }\n",
            ),
        ],
    );
    let out = base.join("out.lua");
    let status = rock(base)
        .current_dir(base.join("app"))
        .args(["compile", ".", "--output"])
        .arg(&out)
        .status()
        .expect("start rock");
    assert!(status.success(), "compile under {}", base.display());
    std::fs::read_to_string(out).unwrap()
}

#[test]
fn a_package_compiled_from_two_directories_gives_identical_lua() {
    for v1 in [true, false] {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let nested = second.path().join("a").join("deeper").join("checkout");
        let a = compile_under(first.path(), v1);
        let b = compile_under(&nested, v1);
        for (lua, root) in [(&a, first.path()), (&b, nested.as_path())] {
            let canonical = root.canonicalize().unwrap();
            for spelling in [root, canonical.as_path()] {
                let spelling = spelling.to_string_lossy();
                assert!(
                    !lua.contains(spelling.as_ref()),
                    "the output names its build directory {spelling}"
                );
            }
        }
        assert!(
            a.contains("\"app/util.rock:2\""),
            "site spelled package-relative"
        );
        assert!(
            a.contains("\"lib/lib.rock:1\""),
            "site spelled package-relative"
        );
        assert_eq!(a, b);
    }
}
