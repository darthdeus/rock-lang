//! Unresolved bare names in modules-v1 bodies: `rock check` reports every one
//! in a single pass, and `rock fix --imports` repairs the importable ones even
//! when a name with no candidate comes first in the same body.
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

/// A v1 package whose `main.rock` misses a struct and three helpers that
/// `util` exports and uses two names nothing defines. `nope_x` comes before
/// `helper_a` in one body, and `nope_y` before `helper_b` in another.
fn package() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("stick.toml"),
        "[package]\nname = \"names\"\nversion = \"0.1.0\"\ntype = \"bin\"\n\n\
         [language]\nmodules = \"v1\"\n",
    )
    .unwrap();
    std::fs::create_dir(root.path().join("util")).unwrap();
    std::fs::write(
        root.path().join("util/util.rock"),
        "mod util;\n\npub struct Pt { pub x: int }\npub fn helper_a() -> int { 1 }\n\
         pub fn helper_b() -> int { 2 }\npub fn helper_c() -> int { 3 }\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("main.rock"),
        "fn one(p: Pt) -> int {\n    nope_x + helper_a() + p.x\n}\n\n\
         fn two() -> int {\n    let y: int = nope_y\n    helper_b() + y + helper_c()\n}\n\n\
         fn main() {\n    print(\"{one(Pt { x: 1 })} {two()}\")\n}\n",
    )
    .unwrap();
    root
}

/// The `did not resolve` names a check reports, in report order, and the
/// number of diagnostics of any kind.
fn unresolved(out: &str) -> (Vec<String>, usize) {
    let errors: Vec<&str> = out.lines().filter(|l| l.contains("Error:")).collect();
    let names = errors
        .iter()
        .filter_map(|l| {
            let rest = l.split("Error: `").nth(1)?;
            let (name, tail) = rest.split_once('`')?;
            tail.starts_with(" did not resolve to any definition")
                .then(|| name.to_string())
        })
        .collect();
    (names, errors.len())
}

#[test]
fn check_reports_every_unresolved_name_without_cascades() {
    let root = package();
    let out = invoke(root.path(), &["check", "."]);
    let output = text(&out);
    assert!(!out.status.success(), "{output}");
    let (names, total) = unresolved(&output);
    assert_eq!(
        names,
        [
            "Pt", "nope_x", "helper_a", "nope_y", "helper_b", "helper_c", "Pt"
        ],
        "{output}"
    );
    assert_eq!(total, names.len(), "no follow-on diagnostics: {output}");
}

#[test]
fn fix_imports_repairs_every_importable_name_together() {
    let root = package();
    let planned = invoke(root.path(), &["fix", "--imports", "."]);
    let output = text(&planned);
    for name in ["Pt", "helper_a", "helper_b", "helper_c"] {
        assert!(
            output.contains(&format!("`{name}` -> use util::{name};")),
            "{name}: {output}"
        );
    }
    for name in ["nope_x", "nope_y"] {
        assert!(output.contains(&format!("`{name}`")), "{name}: {output}");
    }

    let applied = invoke(root.path(), &["fix", "--imports", ".", "--apply"]);
    let applied_text = text(&applied);
    let main = std::fs::read_to_string(root.path().join("main.rock")).unwrap();
    for name in ["Pt", "helper_a", "helper_b", "helper_c"] {
        assert!(
            main.contains(&format!("use util::{name};")),
            "{name}: {applied_text}\n{main}"
        );
    }

    let out = invoke(root.path(), &["check", "."]);
    let output = text(&out);
    let (names, total) = unresolved(&output);
    assert_eq!(names, ["nope_x", "nope_y"], "{output}");
    assert_eq!(total, 2, "{output}");
}
