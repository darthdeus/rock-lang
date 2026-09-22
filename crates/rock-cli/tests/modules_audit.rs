//! `rock modules audit` (modules plan M5, spec 15.1), driven through the real
//! `rock` binary against the real chess graph and small temp-dir packages.
//!
//! Exit status is part of the contract: 0 is a completed report (and, under
//! `--closed`, a closed graph); 3 is a `--closed` failure because a legacy
//! package is reachable; 4 is an analysis failure (the graph or the semantic
//! pass did not complete), which must never read as a clean result. 1 and 2
//! stay the CLI's generic error and clap's usage error.

use std::path::{Path, PathBuf};
use std::process::Command;

fn rock_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_rock"))
}

fn repo_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p
}

fn scratch(tag: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("rock_modules_audit_{tag}_"))
        .tempdir_in(std::env::temp_dir().canonicalize().unwrap())
        .expect("create isolated module fixture")
}

fn write(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, body).unwrap();
}

struct Run {
    stdout: String,
    stderr: String,
    code: Option<i32>,
}

fn audit(project: &Path, extra: &[&str]) -> Run {
    let project = project.to_string_lossy().into_owned();
    let mut args = vec!["modules", "audit", project.as_str()];
    args.extend_from_slice(extra);
    let out = Command::new(rock_bin()).args(&args).output().expect("spawn rock");
    Run {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        code: out.status.code(),
    }
}

fn json(run: &Run) -> serde_json::Value {
    serde_json::from_str(&run.stdout).unwrap_or_else(|e| {
        panic!("--format json must print one JSON document ({e})\nstdout:\n{}\nstderr:\n{}", run.stdout, run.stderr)
    })
}

fn names(list: &serde_json::Value) -> Vec<String> {
    list.as_array()
        .expect("array")
        .iter()
        .map(|v| v["package"].as_str().expect("package name").to_string())
        .collect()
}

fn manifest(name: &str, v1: bool, deps: &str) -> String {
    format!(
        "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\ntype = \"lib\"\n{}\n{deps}\n[build]\nchannels = [\"dev\"]\ndefault_channel = \"dev\"\n",
        if v1 { "\n[language]\nmodules = \"v1\"\n" } else { "" },
    )
}

#[test]
fn chess_core_is_closed() {
    let run = audit(&repo_root().join("libs/chess-core"), &["--closed"]);
    assert_eq!(run.code, Some(0), "chess-core is v1 with no dependencies\nstdout:\n{}\nstderr:\n{}", run.stdout, run.stderr);
    assert!(run.stdout.contains("--closed: pass"), "stdout:\n{}", run.stdout);
}

/// chess-scoped lives under `tests/`, but package-root-relative production
/// filtering keeps its root files. The audit uses that same production
/// population rather than switching modes based on the absolute path.
#[test]
fn chess_scoped_is_closed_and_analyzed_with_its_production_population() {
    let root = repo_root().join("tests/modules/chess-scoped");
    let run = audit(&root, &["--closed", "--format", "json"]);
    assert_eq!(run.code, Some(0), "stdout:\n{}\nstderr:\n{}", run.stdout, run.stderr);
    let v = json(&run);
    assert_eq!(v["configuration"]["population"]["mode"], "production");
    assert_eq!(v["configuration"]["analysis"]["status"], "ok");
    let root_row = v["configuration"]["population"]["packages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["package"] == "chess-scoped")
        .expect("root population row")
        .clone();
    assert!(root_row["included_files"].as_u64().unwrap() > 0, "root must not be audited empty: {root_row}");
    assert_eq!(v["closed"]["verdict"], "pass");

    // The consumer's calls into chess-core are compiler-derived, not zero.
    let edges = v["cross_package_references"]["edges"].as_array().unwrap();
    let to_core = edges
        .iter()
        .find(|e| e["from"] == "chess-scoped" && e["to"] == "chess-core")
        .expect("chess-scoped -> chess-core edge");
    assert!(to_core["calls"].as_u64().unwrap() >= 3, "{to_core}");
}

#[test]
fn chesslings_is_not_closed_and_names_every_legacy_package() {
    let run = audit(&repo_root().join("games/chesslings"), &["--closed", "--format", "json"]);
    assert_eq!(run.code, Some(3), "stdout:\n{}\nstderr:\n{}", run.stdout, run.stderr);
    let v = json(&run);
    assert_eq!(v["closed"]["verdict"], "fail");
    assert_eq!(v["closed"]["root_is_legacy"], true);
    let mut offending = names(&v["closed"]["offending"]);
    offending.sort();
    assert_eq!(offending, vec!["chesslings", "stone", "stone-core"]);
    for package in ["chess-core", "chess-game", "chess-render", "chess-sfx"] {
        let reached = v["legacy_boundaries"]["reachable"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["package"] == package)
            .expect("converted library is still reached, as v1")
            .clone();
        assert_eq!(reached["legacy"], false, "{reached}");
        assert_eq!(reached["dialect"], "v1", "{reached}");
    }
    // Every offending package carries a reach path from the root.
    for row in v["closed"]["offending"].as_array().unwrap() {
        let paths = row["paths"].as_array().expect("paths");
        assert!(!paths.is_empty(), "{row}");
        assert_eq!(paths[0][0], "chesslings", "{row}");
    }
    // stone is reached three ways: directly, through chess-sfx and chess-render.
    let stone = v["closed"]["offending"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["package"] == "stone")
        .unwrap()
        .clone();
    assert_eq!(stone["path_count"], 3, "{stone}");
}

#[test]
fn a_v1_root_reaching_a_legacy_package_is_not_closed() {
    let fixture = scratch("legacy_dep");
    let dir = fixture.path();
    write(&dir.join("old/stick.toml"), &manifest("old-lib", false, ""));
    write(&dir.join("old/lib.rock"), "fn old_answer() -> int {\n    42\n}\n");
    write(&dir.join("mid/stick.toml"), &manifest("mid-lib", true, "[dependencies]\nold-lib = { path = \"../old\", allow_legacy = true }\n"));
    write(&dir.join("mid/lib.rock"), "pub fn mid_answer() -> int {\n    1\n}\n");
    write(&dir.join("app/stick.toml"), &manifest("app", true, "[dependencies]\nmid-lib = { path = \"../mid\" }\n"));
    write(&dir.join("app/lib.rock"), "pub fn app_answer() -> int {\n    mid_lib::mid_answer()\n}\n");

    let run = audit(&dir.join("app"), &["--closed"]);
    assert_eq!(run.code, Some(3), "stdout:\n{}\nstderr:\n{}", run.stdout, run.stderr);
    assert!(run.stdout.contains("--closed: FAIL"), "stdout:\n{}", run.stdout);
    assert!(
        run.stdout.contains("app -> mid-lib -> old-lib"),
        "the transitive legacy dependency is named with its reach path\nstdout:\n{}",
        run.stdout
    );

    let v = json(&audit(&dir.join("app"), &["--closed", "--format", "json"]));
    assert_eq!(v["closed"]["root_is_legacy"], false);
    assert_eq!(names(&v["closed"]["offending"]), vec!["old-lib"]);
}

#[test]
fn a_failed_analysis_is_unknown_not_clean() {
    let fixture = scratch("type_error");
    let dir = fixture.path();
    write(&dir.join("stick.toml"), &manifest("broken", true, ""));
    write(&dir.join("lib.rock"), "pub fn wrong() -> int {\n    \"not an int\"\n}\n");

    let run = audit(&dir, &["--closed"]);
    assert_eq!(run.code, Some(4), "analysis failure is its own exit status\nstdout:\n{}\nstderr:\n{}", run.stdout, run.stderr);
    assert!(run.stdout.contains("unknown (analysis failed)"), "stdout:\n{}", run.stdout);
    assert!(!run.stdout.contains("--closed: pass"), "stdout:\n{}", run.stdout);
    assert!(run.stdout.contains("lib.rock"), "the diagnostic is reported\nstdout:\n{}", run.stdout);

    let v = json(&audit(&dir, &["--closed", "--format", "json"]));
    assert_eq!(v["configuration"]["analysis"]["status"], "failed");
    assert!(!v["configuration"]["analysis"]["diagnostics"].as_array().unwrap().is_empty());
    assert_eq!(v["cross_package_references"]["status"], "unknown");
    assert_eq!(v["closed"]["verdict"], "unknown");
}

/// Counts are compiler-derived: two calls (imported and qualified), one global
/// read and one struct literal from the v1 consumer, one call from the legacy
/// consumer through the compatibility projection.
#[test]
fn cross_package_references_are_exact_counts() {
    let fixture = scratch("counts");
    let dir = fixture.path();
    write(&dir.join("lib/stick.toml"), &manifest("counted-lib", true, ""));
    write(
        &dir.join("lib/lib.rock"),
        "pub struct Box {\n    pub n: int,\n}\n\npub global LIMIT: int = 3\n\npub fn bump(n: int) -> int {\n    n\n}\n",
    );
    write(&dir.join("app/stick.toml"), &manifest("counted-app", true, "[dependencies]\ncounted-lib = { path = \"../lib\" }\n"));
    write(
        &dir.join("app/lib.rock"),
        "use counted_lib::bump;\n\npub fn twice() -> int {\n    let a = bump(1)\n    let b = counted_lib::bump(counted_lib::LIMIT)\n    return b\n}\n\npub fn boxed() -> counted_lib::Box {\n    counted_lib::Box { n: 1 }\n}\n",
    );
    write(&dir.join("old/stick.toml"), &manifest("counted-old", false, "[dependencies]\ncounted-lib = { path = \"../lib\" }\n"));
    write(&dir.join("old/lib.rock"), "fn legacy_bump() -> int {\n    bump(5)\n}\n");

    let v = json(&audit(&dir.join("app"), &["--format", "json"]));
    assert_eq!(v["cross_package_references"]["status"], "exact");
    let edges = v["cross_package_references"]["edges"].as_array().unwrap();
    let e = edges
        .iter()
        .find(|e| e["from"] == "counted-app" && e["to"] == "counted-lib")
        .unwrap_or_else(|| panic!("edge missing: {edges:?}"));
    assert_eq!(e["calls"], 2, "{e}");
    assert_eq!(e["values"], 1, "{e}");
    assert_eq!(e["constructions"], 1, "{e}");
    assert_eq!(e["enum_variants"], 0, "{e}");
    // `-> counted_lib::Box` in `boxed`'s signature.
    assert_eq!(e["types"], 1, "{e}");
    assert_eq!(e["patterns"], 0, "{e}");
    assert_eq!(e["protocol"], 0, "{e}");

    let v = json(&audit(&dir.join("old"), &["--format", "json"]));
    let edges = v["cross_package_references"]["edges"].as_array().unwrap();
    let e = edges
        .iter()
        .find(|e| e["from"] == "counted-old" && e["to"] == "counted-lib")
        .unwrap_or_else(|| panic!("edge missing: {edges:?}"));
    assert_eq!(e["calls"], 1, "{e}");
}

#[test]
fn json_carries_a_status_for_every_section() {
    let v = json(&audit(&repo_root().join("libs/chess-core"), &["--format", "json"]));
    for section in [
        "configuration",
        "package_graph",
        "legacy_boundaries",
        "cross_package_references",
        "undeclared_references",
        "census",
        "import_families",
        "legacy_exports",
        "api_closure",
        "name_collisions",
    ] {
        let status = v[section]["status"].as_str().unwrap_or_else(|| panic!("{section} has no status: {v}"));
        assert!(["exact", "unknown", "not_computed"].contains(&status), "{section}: {status}");
    }
    let not_computed = v["not_computed"].as_array().expect("not_computed list");
    assert!(!not_computed.is_empty());
    for item in not_computed {
        assert_eq!(item["status"], "not_computed", "{item}");
        assert!(item["missing"].as_str().is_some_and(|s| !s.is_empty()), "{item}");
    }
    // No dependencies: chess-core has an empty edge list, and it is exact.
    assert_eq!(v["closed"]["requested"], false);
    assert_eq!(v["package_graph"]["packages"].as_array().unwrap().len(), 1);
}

/// A graph the host refuses (here a v1 -> legacy edge without
/// `allow_legacy`) is an analysis failure with the host's reason, not a pass.
#[test]
fn an_unresolved_graph_is_unknown_not_clean() {
    let fixture = scratch("graph_error");
    let dir = fixture.path();
    write(&dir.join("old/stick.toml"), &manifest("old-lib", false, ""));
    write(&dir.join("old/lib.rock"), "fn old_answer() -> int {\n    42\n}\n");
    write(&dir.join("app/stick.toml"), &manifest("app", true, "[dependencies]\nold-lib = { path = \"../old\" }\n"));
    write(&dir.join("app/lib.rock"), "pub fn app_answer() -> int {\n    1\n}\n");

    let run = audit(&dir.join("app"), &["--closed", "--format", "json"]);
    assert_eq!(run.code, Some(4), "stdout:\n{}\nstderr:\n{}", run.stdout, run.stderr);
    let v = json(&run);
    assert_eq!(v["package_graph"]["status"], "unknown");
    assert!(v["package_graph"]["reason"].as_str().unwrap().contains("allow_legacy"), "{v}");
    assert_eq!(v["legacy_boundaries"]["status"], "unknown");
    assert_eq!(v["closed"]["verdict"], "unknown");
}

/// The sections that used to be listed as not computed, each compiler-derived:
/// the parse-tree census (ascriptions, raw backend sites), import families per
/// fragment, what a legacy package's consumers need exported, the public API
/// closure with its reasons, and latent name collisions.
#[test]
fn derived_sections_are_exact() {
    let fixture = scratch("derived");
    let dir = fixture.path();
    write(&dir.join("lib/stick.toml"), &manifest("closure-lib", true, ""));
    write(
        &dir.join("lib/lib.rock"),
        "pub struct Crate {\n    pub n: int,\n}\n\npub fn make_crate() -> Crate {\n    Crate { n: 1 }\n}\n",
    );
    write(&dir.join("old/stick.toml"), &manifest("census-old", false, "[dependencies]\nclosure-lib = { path = \"../lib\" }\n"));
    write(
        &dir.join("old/old.rock"),
        "fn old_value() -> int {\n    make_crate().n :: int\n}\n\nfn abs(s: string) -> string {\n    s\n}\n\n@nomangle inline(lua) fn old_raw() -> int = \"\"\"return 1\"\"\"\n",
    );
    write(&dir.join("app/stick.toml"), &manifest("census-app", false, "[dependencies]\ncensus-old = { path = \"../old\" }\n"));
    write(&dir.join("app/app.rock"), "fn app_value() -> int {\n    old_value() + old_value()\n}\n");

    let run = audit(&dir.join("app"), &["--format", "json"]);
    assert_eq!(run.code, Some(0), "stdout:\n{}\nstderr:\n{}", run.stdout, run.stderr);
    let v = json(&run);
    let pkg = |section: &str, name: &str| -> serde_json::Value {
        v[section]["packages"]
            .as_array()
            .unwrap_or_else(|| panic!("{section}: {v}"))
            .iter()
            .find(|p| p["package"] == name)
            .unwrap_or_else(|| panic!("{section} has no {name}: {v}"))
            .clone()
    };

    let census = pkg("census", "census-old");
    assert_eq!(census["legacy_ascriptions"], 1, "{census}");
    assert_eq!(census["inline_fns"], 1, "{census}");
    assert_eq!(census["nomangle_items"], 1, "{census}");

    let old_frag = v["import_families"]["fragments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["package"] == "census-old")
        .unwrap_or_else(|| panic!("{v}"))
        .clone();
    // The call; the `.n` read names no definition (fields are not counted).
    assert_eq!(old_frag["references"]["closure-lib"], 1, "{old_frag}");

    let exports = pkg("legacy_exports", "census-old");
    let defs = exports["definitions"].as_array().unwrap();
    assert_eq!(defs.len(), 1, "{exports}");
    assert_eq!(defs[0]["name"], "old_value");
    assert_eq!(defs[0]["references"], 2);
    assert_eq!(defs[0]["from"][0], "census-app");

    let closure = pkg("api_closure", "closure-lib");
    assert_eq!(closure["types"][0]["type"], "closure_lib::Crate", "{closure}");
    assert_eq!(closure["types"][0]["reasons"][0]["reason"], "public function", "{closure}");
    assert_eq!(closure["types"][0]["reasons"][0]["exporter"], "make_crate", "{closure}");

    let abs = v["name_collisions"]["names"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["name"] == "abs")
        .unwrap_or_else(|| panic!("{v}"))
        .clone();
    assert_eq!(abs["owners"], serde_json::json!(["census-old", "std"]), "{abs}");
}
