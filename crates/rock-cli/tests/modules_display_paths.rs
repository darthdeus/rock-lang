//! `rock modules audit` and `rock modules migrate` print package roots and
//! files relative to the working directory, so their reports are the same on
//! every machine. The temporary root is reached through a symlink on some
//! hosts (`/var` -> `/private/var`), which a plain prefix strip misses.
use std::path::Path;
use std::process::{Command, Output};

fn invoke(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rock"))
        .current_dir(root)
        .args(args)
        .env("ROCK_DIR", root)
        .env("ROCK_INVOCATION_LOG", "off")
        .env_remove("ROCK_STDLIB_DIR")
        .env_remove("LUA_PATH")
        .env_remove("LUA_CPATH")
        .output()
        .expect("start rock")
}

fn text(out: &Output) -> String {
    test_utils::strip_ansi_sgr(&format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    ))
}

/// A legacy `app` depending on a v1 `lib`, laid out as `app/` and `lib/`.
fn workspace() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    let write = |path: &str, text: &str| {
        let path = root.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    };
    write(
        "lib/stick.toml",
        "[package]\nname = \"lib\"\nversion = \"0.1.0\"\ntype = \"lib\"\n\n\
         [language]\nmodules = \"v1\"\n",
    );
    write("lib/lib.rock", "pub fn answer() -> int { 42 }\n");
    write(
        "app/stick.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\ntype = \"bin\"\n\n\
         [dependencies]\nlib = { path = \"../lib\" }\n",
    );
    write(
        "app/main.rock",
        "fn main() {\n    print(\"{answer()}\")\n}\n",
    );
    root
}

#[test]
fn module_reports_print_paths_relative_to_the_working_directory() {
    let root = workspace();
    let given = root.path().to_string_lossy().into_owned();
    let canonical = root.path().canonicalize().unwrap();
    let canonical = canonical.to_string_lossy().into_owned();
    for (args, relative) in [
        (
            &["modules", "audit", "app"][..],
            "#0 app (legacy, selected root) app\n",
        ),
        (
            &["modules", "audit", "app", "--format", "json"],
            "\"file\": \"app/main.rock\"",
        ),
        (
            &["modules", "migrate", "app", "--check"],
            "    app/main.rock: lib (1)\n",
        ),
    ] {
        let out = text(&invoke(root.path(), args));
        assert!(out.contains(relative), "{args:?}: {out}");
        assert!(
            !out.contains(&given) && !out.contains(&canonical),
            "{args:?} printed an absolute path: {out}"
        );
    }
}
