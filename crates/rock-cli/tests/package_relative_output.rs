//! Generated Lua names source files relative to their package, so one package
//! compiled from two checkouts gives the same bytes.
use std::path::Path;
use std::process::Command;

fn write(root: &Path, path: &str, text: &str) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A legacy `app` (file-keyed definitions) using a v1 `lib`, under `base`.
fn compile_under(base: &Path) -> String {
    write(
        base,
        "lib/stick.toml",
        "[package]\nname = \"lib\"\nversion = \"0.1.0\"\ntype = \"lib\"\n\n\
         [language]\nmodules = \"v1\"\n",
    );
    write(
        base,
        "lib/lib.rock",
        "pub fn lib_site() -> string { __SITE__ }\n\
         pub fn lib_apply(f: (int) -> int, x: int) -> int { f(x) }\n\
         pub fn lib_offset(x: int) -> int { lib_apply((v) => v + x, 1) }\n",
    );
    write(
        base,
        "app/stick.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\ntype = \"bin\"\n\n\
         [dependencies]\nlib = { path = \"../lib\" }\n",
    );
    write(
        base,
        "app/util.rock",
        "global COUNT: int = 0\n\
         fn app_site() -> string { return __SITE__ }\n\
         fn apply(f: (int) -> int, x: int) -> int { COUNT = COUNT + 1\n return f(x) }\n",
    );
    write(
        base,
        "app/main.rock",
        "fn main() {\n\
         \x20   print(app_site())\n\
         \x20   print(lib_site())\n\
         \x20   print(\"{apply((v) => v + 1, 2)} {lib_offset(3)} {COUNT}\")\n\
         }\n",
    );
    let out = base.join("out.lua");
    let status = Command::new(env!("CARGO_BIN_EXE_rock"))
        .current_dir(base.join("app"))
        .args(["compile", ".", "--output"])
        .arg(&out)
        .env("ROCK_DIR", base)
        .env("ROCK_INVOCATION_LOG", "off")
        .env_remove("ROCK_STDLIB_DIR")
        .env_remove("ROCK_SKULL_LINE_DIRECTIVES")
        .status()
        .expect("start rock");
    assert!(status.success(), "compile under {}", base.display());
    std::fs::read_to_string(out).unwrap()
}

#[test]
fn a_package_compiled_from_two_directories_gives_identical_lua() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let nested = second.path().join("a").join("deeper").join("checkout");
    let a = compile_under(first.path());
    let b = compile_under(&nested);
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
