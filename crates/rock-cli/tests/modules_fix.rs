//! `rock fix --imports` (modules plan 7.4, spec 15.3), driven through the real
//! `rock` binary on temp-dir packages.
//!
//! Exit status follows `rock modules audit` and `migrate`: 0 nothing left to
//! do; 3 work to do under `--check`, or names `--apply` could not fix; 4 an
//! analysis failed.

use std::path::{Path, PathBuf};
use std::process::Command;

fn rock_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_rock"))
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rock_modules_fix_{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

fn write(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, body).unwrap();
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

struct Run {
    stdout: String,
    stderr: String,
    code: Option<i32>,
}

fn rock(cwd: &Path, args: &[&str]) -> Run {
    let out = Command::new(rock_bin()).current_dir(cwd).args(args).output().expect("spawn rock");
    Run {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        code: out.status.code(),
    }
}

fn fix(cwd: &Path, target: &str, extra: &[&str]) -> Run {
    let mut args = vec!["fix", "--imports", target];
    args.extend_from_slice(extra);
    rock(cwd, &args)
}

fn json(run: &Run) -> serde_json::Value {
    serde_json::from_str(&run.stdout).unwrap_or_else(|e| {
        panic!("--format json must print one JSON document ({e})\nstdout:\n{}\nstderr:\n{}", run.stdout, run.stderr)
    })
}

fn shown(run: &Run) -> String {
    format!("exit {:?}\nstdout:\n{}\nstderr:\n{}", run.code, run.stdout, run.stderr)
}

fn v1_manifest(name: &str, kind: &str, deps: &str) -> String {
    format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\ntype = \"{kind}\"\n\n[language]\nmodules = \"v1\"\n\n{deps}")
}

/// `(name, path)` of each entry in a JSON list of fixes.
fn fixes(v: &serde_json::Value) -> Vec<(String, String)> {
    v["fixes"]
        .as_array()
        .unwrap_or_else(|| panic!("no fixes list: {v}"))
        .iter()
        .map(|f| (f["name"].as_str().unwrap().to_string(), f["use"].as_str().unwrap().to_string()))
        .collect()
}

/// A v1 library `geo` with a root function and type and two child modules.
fn geo(dir: &Path, solids_extra: &str) {
    write(&dir.join("geo/stick.toml"), &v1_manifest("geo", "lib", ""));
    write(&dir.join("geo/lib.rock"), "pub fn area(n: int) -> int {\n    n * n\n}\n\npub struct Pt {\n    pub x: int,\n}\n\nfn secret() -> int {\n    9\n}\n");
    write(&dir.join("geo/shapes.rock"), "pub mod shapes;\n\npub fn depth() -> int {\n    3\n}\n");
    write(&dir.join("geo/solids.rock"), &format!("pub mod solids;\n\npub fn volume() -> int {{\n    5\n}}\n{solids_extra}"));
}

fn app(dir: &Path, deps: &str, main: &str) {
    write(&dir.join("app/stick.toml"), &v1_manifest("app", "bin", &format!("[dependencies]\n{deps}")));
    write(&dir.join("app/main.rock"), main);
}

/// Each unresolved name with one accessible candidate -- from a declared
/// dependency's root and child module, and from the package's own module --
/// gets an explicit import, the signature's type included (the semantic pass
/// stops at an interface error, so it takes a second round). The imports go
/// in sorted order, `--apply` output runs, and a second run has nothing to
/// do.
#[test]
fn unique_candidates_get_imports_and_the_result_runs() {
    let dir = scratch("unique");
    geo(&dir, "");
    app(
        &dir,
        "geo = { path = \"../geo\" }\n",
        "fn f(p: Pt) -> int {\n    p.x\n}\n\nfn main() {\n    print(\"{area(2)} {depth()} {helper()} {f(Pt { x: 1 })}\")\n}\n",
    );
    write(&dir.join("app/util.rock"), "mod util;\n\npub(package) fn helper() -> int {\n    4\n}\n");

    let check = fix(&dir, "app", &["--format", "json"]);
    assert_eq!(check.code, Some(3), "{}", shown(&check));
    let v = json(&check);
    assert_eq!(v["verdict"], "fixes_available", "{v}");
    let mut got = fixes(&v);
    got.sort();
    assert_eq!(
        got,
        [
            ("Pt".to_string(), "geo::Pt".to_string()),
            ("area".to_string(), "geo::area".to_string()),
            ("depth".to_string(), "geo::shapes::depth".to_string()),
            ("helper".to_string(), "util::helper".to_string()),
        ]
    );
    assert!(!read(&dir.join("app/main.rock")).contains("use "), "--check writes nothing");

    let apply = fix(&dir, "app", &["--apply"]);
    assert_eq!(apply.code, Some(0), "{}", shown(&apply));
    assert!(
        read(&dir.join("app/main.rock"))
            .starts_with("use geo::Pt;\nuse geo::area;\nuse geo::shapes::depth;\nuse util::helper;\nfn f(p: Pt) -> int {\n"),
        "{}",
        read(&dir.join("app/main.rock"))
    );
    let run = rock(&dir, &["run", "app"]);
    assert!(run.stdout.contains("4 3 4 1"), "{}", shown(&run));

    let again = fix(&dir, "app", &["--format", "json"]);
    assert_eq!(again.code, Some(0), "{}", shown(&again));
    assert_eq!(json(&again)["verdict"], "clean");
}

/// Several accessible candidates are listed with their origins and get no
/// edit (spec 15.3: a choice or qualification is the author's).
#[test]
fn ambiguous_candidates_are_listed_and_nothing_is_written() {
    let dir = scratch("ambiguous");
    for lib in ["a_lib", "b_lib"] {
        write(&dir.join(format!("{lib}/stick.toml")), &v1_manifest(lib, "lib", ""));
        write(&dir.join(format!("{lib}/lib.rock")), "pub fn mix(n: int) -> int {\n    n\n}\n");
    }
    let main = "fn main() {\n    print(\"{mix(1)}\")\n}\n";
    app(&dir, "a_lib = { path = \"../a_lib\" }\nb_lib = { path = \"../b_lib\" }\n", main);

    let run = fix(&dir, "app", &["--apply", "--format", "json"]);
    assert_eq!(run.code, Some(3), "{}", shown(&run));
    let v = json(&run);
    assert_eq!(v["fixes"].as_array().map(Vec::len), Some(0), "{v}");
    let ambiguous = &v["ambiguous"][0];
    assert_eq!(ambiguous["name"], "mix", "{v}");
    let candidates: Vec<&str> = ambiguous["candidates"].as_array().unwrap().iter().map(|c| c.as_str().unwrap()).collect();
    assert_eq!(candidates, ["a_lib::mix", "b_lib::mix"]);
    assert_eq!(read(&dir.join("app/main.rock")), main);
}

/// A private definition, and one in a package the fragment's package does
/// not declare, are reported with the reason and never imported (spec 15.3:
/// no new dependencies or broader visibility).
#[test]
fn inaccessible_and_undeclared_candidates_are_reported_not_added() {
    let dir = scratch("unavailable");
    write(&dir.join("far/stick.toml"), &v1_manifest("far", "lib", ""));
    write(&dir.join("far/lib.rock"), "pub fn remote() -> int {\n    7\n}\n");
    geo(&dir, "");
    write(
        &dir.join("geo/stick.toml"),
        &v1_manifest("geo", "lib", "[dependencies]\nfar = { path = \"../far\" }\n"),
    );
    write(&dir.join("geo/uses_far.rock"), "fn near() -> int {\n    far::remote()\n}\n");
    // Separate bodies: a body reports only its first unresolved name, and a
    // name with no import to add keeps hiding the ones after it.
    let main = "fn main() {\n    print(\"{secret()}\")\n}\n\nfn other() -> int {\n    remote()\n}\n";
    app(&dir, "geo = { path = \"../geo\" }\n", main);

    let run = fix(&dir, "app", &["--apply", "--format", "json"]);
    assert_eq!(run.code, Some(3), "{}", shown(&run));
    let v = json(&run);
    assert_eq!(v["fixes"].as_array().map(Vec::len), Some(0), "{v}");
    let reasons = |name: &str| -> String {
        v["unavailable"]
            .as_array()
            .unwrap()
            .iter()
            .find(|u| u["name"] == name)
            .unwrap_or_else(|| panic!("`{name}` is reported: {v}"))["reasons"]
            .to_string()
    };
    assert!(reasons("secret").contains("geo::secret") && reasons("secret").contains("not accessible"), "{v}");
    assert!(reasons("remote").contains("far::remote") && reasons("remote").contains("not a declared dependency"), "{v}");
    assert_eq!(read(&dir.join("app/main.rock")), main);
}

/// A definition in the view of an `allow_legacy` dependency is imported like
/// a v1 one; the tool never adds the edge itself.
#[test]
fn an_allow_legacy_dependency_is_a_candidate() {
    let dir = scratch("legacy");
    write(&dir.join("x_lib/stick.toml"), "[package]\nname = \"x_lib\"\nversion = \"0.1.0\"\ntype = \"lib\"\n");
    write(&dir.join("x_lib/lib.rock"), "fn wander(n: int) -> int {\n    n + 1\n}\n");
    app(&dir, "x_lib = { path = \"../x_lib\", allow_legacy = true }\n", "fn main() {\n    print(\"w={wander(1)}\")\n}\n");

    let apply = fix(&dir, "app", &["--apply"]);
    assert_eq!(apply.code, Some(0), "{}", shown(&apply));
    assert!(read(&dir.join("app/main.rock")).starts_with("use x_lib::wander;\n"));
    let run = rock(&dir, &["run", "app"]);
    assert!(run.stdout.contains("w=2"), "{}", shown(&run));
}

/// A fragment that already globs a sibling module gets a glob of the
/// candidate's module (plan 7.4) -- unless the glob would move a resolved
/// site, as here where `geo::solids` also has a `smoothstep` that would hide
/// the std one; then the explicit import is used.
#[test]
fn a_sibling_glob_is_used_unless_it_moves_a_site() {
    let dir = scratch("glob");
    geo(&dir, "");
    let main = "use geo::shapes::*;\n\nfn main() {\n    print(\"{depth()} {volume()}\")\n}\n";
    app(&dir, "geo = { path = \"../geo\" }\n", main);
    let v = json(&fix(&dir, "app", &["--format", "json"]));
    assert_eq!(fixes(&v), [("volume".to_string(), "geo::solids::*".to_string())], "{v}");

    geo(&dir, "\npub fn smoothstep(n: int) -> int {\n    n\n}\n");
    let main = "use geo::shapes::*;\n\nfn main() {\n    print(\"{depth()} {volume()} {smoothstep(0.0, 1.0, 0.5)}\")\n}\n";
    write(&dir.join("app/main.rock"), main);
    let apply = fix(&dir, "app", &["--apply", "--format", "json"]);
    assert_eq!(apply.code, Some(0), "{}", shown(&apply));
    assert_eq!(fixes(&json(&apply)), [("volume".to_string(), "geo::solids::volume".to_string())]);
    assert_eq!(read(&dir.join("app/main.rock")), format!("use geo::shapes::*;\nuse geo::solids::volume;\n{}", &main["use geo::shapes::*;\n".len()..]));
    let run = rock(&dir, &["run", "app"]);
    assert!(run.stdout.contains("3 5 0.5"), "{}", shown(&run));
}

/// A package that does not parse is an analysis failure (exit 4).
#[test]
fn a_package_that_does_not_parse_is_an_analysis_failure() {
    let dir = scratch("broken");
    app(&dir, "", "fn main( {\n}\n");
    let run = fix(&dir, "app", &["--format", "json"]);
    assert_eq!(run.code, Some(4), "{}", shown(&run));
    assert_eq!(json(&run)["verdict"], "analysis_failed");
}
