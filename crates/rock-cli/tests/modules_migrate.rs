//! `rock modules migrate` (modules plan M5, spec 15.2), driven through the real
//! `rock` binary on temp-dir packages and on a copy of the real chess graph.
//!
//! Exit status is part of the contract, as for `rock modules audit`: 0 nothing
//! to do (already v1) or `--apply` succeeded; 3 `--check` found work to do or
//! the plan is blocked (nothing written); 4 an analysis failed.

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
        .prefix(&format!("rock_modules_migrate_{tag}_"))
        .tempdir_in(std::env::temp_dir().canonicalize().unwrap())
        .expect("create isolated module fixture")
}

#[test]
fn scratch_invocations_do_not_replace_each_others_graph() {
    let first_fixture = scratch("concurrent_graph");
    let first = first_fixture.path();
    write(&first.join("owner.txt"), "first invocation");
    let second_fixture = scratch("concurrent_graph");
    let second = second_fixture.path();
    assert_ne!(first, second, "independent migration fixtures must not share a directory");
    assert_eq!(read(&first.join("owner.txt")), "first invocation");
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

fn migrate(cwd: &Path, package: &str, extra: &[&str]) -> Run {
    let mut args = vec!["modules", "migrate", package];
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

fn manifest(name: &str, deps: &str) -> String {
    format!(
        "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\ntype = \"lib\"\n\n{deps}\n[build]\nchannels = [\"dev\"]\ndefault_channel = \"dev\"\n"
    )
}

fn names(list: &serde_json::Value) -> Vec<String> {
    let mut v: Vec<String> = list
        .as_array()
        .expect("array")
        .iter()
        .map(|d| d["name"].as_str().expect("name").to_string())
        .collect();
    v.sort();
    v
}

/// The whole contract on one small legacy library: the plan names what a
/// consumer uses, the type closure on top of it with its reason, and the
/// ascription rewrite; `--apply` writes exactly that, the package and its
/// consumer still build and run, and a second run has nothing to do.
#[test]
fn a_legacy_library_migrates_with_its_exports_closure_and_ascriptions() {
    let fixture = scratch("leaf");
    let dir = fixture.path();
    write(&dir.join("lib/stick.toml"), &manifest("leaf-lib", ""));
    write(
        &dir.join("lib/lib.rock"),
        "// Kept: a comment before the first item.\n\
         struct Score {\n    total: int,\n}\n\n\
         struct Unused {\n    n: int,\n}\n\n\
         fn helper(n: int) -> int {\n    n * 2\n}\n\n\
         fn score_of(n: int) -> Score {\n    let doubled = helper(n) :: int\n    Score { total: doubled }\n}\n",
    );
    write(&dir.join("lib/tests/lib_test.rock"), "@test\nfn doubles() {\n    assert(score_of(3).total == 6)\n    assert(helper(1) == 2)\n}\n");
    write(&dir.join("app/stick.toml"), &manifest("leaf-app", "[dependencies]\nleaf-lib = { path = \"../lib\" }\n"));
    write(&dir.join("app/app.rock"), "fn app_total() -> int {\n    score_of(5).total\n}\n\n@test\nfn app_runs() {\n    assert(app_total() == 10)\n}\n");

    let check = migrate(&dir, "lib", &["--check", "--consumer", "app", "--format", "json"]);
    assert_eq!(check.code, Some(3), "{}", shown(&check));
    let v = json(&check);
    assert_eq!(v["verdict"], "migration_needed", "{v}");
    assert_eq!(names(&v["exports"]["pub"]), vec!["Score", "score_of"], "{v}");
    let score = v["exports"]["pub"].as_array().unwrap().iter().find(|e| e["name"] == "Score").unwrap();
    assert_eq!(score["closure"][0]["reason"], "public function", "{score}");
    assert_eq!(score["closure"][0]["exporter"], "score_of", "{score}");
    assert!(score["consumers"].as_array().unwrap().is_empty(), "the consumer never names Score: {score}");
    assert_eq!(names(&v["exports"]["private"]), vec!["Unused", "doubles", "helper"], "{v}");
    assert_eq!(v["ascriptions"]["count"], 1, "{v}");
    assert_eq!(v["imports"]["globs"].as_array().unwrap().len(), 0, "{v}");
    for ctx in v["verification"].as_array().unwrap() {
        assert!(ctx["errors"].as_array().unwrap().is_empty(), "{ctx}");
        assert!(ctx["changed_sites"].as_array().unwrap().is_empty(), "{ctx}");
    }
    // --check wrote nothing.
    assert!(!read(&dir.join("lib/stick.toml")).contains("modules"));

    let apply = migrate(&dir, "lib", &["--apply", "--consumer", "app"]);
    assert_eq!(apply.code, Some(0), "{}", shown(&apply));
    assert_eq!(
        read(&dir.join("lib/lib.rock")),
        "// Kept: a comment before the first item.\n\
         pub struct Score {\n    total: int,\n}\n\n\
         struct Unused {\n    n: int,\n}\n\n\
         fn helper(n: int) -> int {\n    n * 2\n}\n\n\
         pub fn score_of(n: int) -> Score {\n    let doubled = (helper(n) : int)\n    Score { total: doubled }\n}\n"
    );
    assert!(read(&dir.join("lib/stick.toml")).contains("[language]\nmodules = \"v1\"\n"));
    assert!(!dir.join("lib/imports.rock").exists(), "no dependency, no facade");

    let lib_tests = rock(&dir, &["test", "lib"]);
    assert!(lib_tests.stdout.contains("1/1 @tests passed"), "{}", shown(&lib_tests));
    let app_tests = rock(&dir, &["test", "app"]);
    assert!(app_tests.stdout.contains("1/1 @tests passed"), "the consumer is unchanged and still runs\n{}", shown(&app_tests));

    let again = migrate(&dir, "lib", &["--check", "--consumer", "app"]);
    assert_eq!(again.code, Some(0), "{}", shown(&again));
    assert!(again.stdout.contains("already modules-v1"), "{}", shown(&again));
}

/// A consumer that applies one of the package's decorators uses it, as it
/// uses a function it calls: the plan exports it and the consumer's
/// annotation still resolves. A decorator only the package applies, and one
/// nobody applies, stay private. This consumer is v1; the next test is
/// the legacy one.
#[test]
fn a_decorator_a_consumer_applies_is_exported() {
    let fixture = scratch("decorator_use");
    let dir = fixture.path();
    write(&dir.join("lib/stick.toml"), &manifest("deco-lib", ""));
    write(
        &dir.join("lib/lib.rock"),
        "@lifecycle(phase=\"update\", owner=\"session\")\ndeco init tick {}\n\n\
         @lifecycle(phase=\"draw\", owner=\"session\")\ndeco init own_hook {}\n\n\
         @lifecycle(phase=\"wire\", owner=\"session\")\ndeco init idle_hook {}\n\n\
         @own_hook\nfn lib_draw() {}\n",
    );
    write(
        &dir.join("app/stick.toml"),
        &manifest("deco-app", "[language]\nmodules = \"v1\"\n\n[dependencies]\ndeco-lib = { path = \"../lib\", allow_legacy = true }\n"),
    );
    write(&dir.join("app/app.rock"), "use deco_lib::tick;\n\n@tick\nfn app_tick() {}\n");

    let v = json(&migrate(dir, "lib", &["--check", "--consumer", "app", "--format", "json"]));
    assert_eq!(v["verdict"], "migration_needed", "{v}");
    assert_eq!(names(&v["exports"]["pub"]), vec!["tick"], "{v}");
    let tick = &v["exports"]["pub"][0];
    assert_eq!(tick["consumers"][0]["package"], "deco-app", "{tick}");
    assert_eq!(names(&v["exports"]["private"]), vec!["idle_hook", "lib_draw", "own_hook"], "{v}");
    for ctx in v["verification"].as_array().unwrap() {
        assert!(ctx["errors"].as_array().unwrap().is_empty(), "{ctx}");
        assert!(ctx["changed_sites"].as_array().unwrap().is_empty(), "{ctx}");
    }

    let apply = migrate(dir, "lib", &["--apply", "--consumer", "app"]);
    assert_eq!(apply.code, Some(0), "{}", shown(&apply));
    let lib = read(&dir.join("lib/lib.rock"));
    assert!(lib.starts_with("pub @lifecycle(phase=\"update\", owner=\"session\")\ndeco init tick {}\n"), "{lib}");
    assert!(lib.contains("\n@lifecycle(phase=\"draw\", owner=\"session\")\ndeco init own_hook"), "{lib}");
}

/// The Stone case: a legacy consumer applies the decorator, and after the
/// conversion reaches it through its root projection. `update` also names a
/// legacy stdlib fn, as Stone's `@update` does.
#[test]
fn a_decorator_a_legacy_consumer_applies_is_exported() {
    let fixture = scratch("decorator_legacy_consumer");
    let dir = fixture.path();
    write(&dir.join("lib/stick.toml"), &manifest("deco-lib", ""));
    write(&dir.join("lib/lib.rock"), "@lifecycle(phase=\"update\", owner=\"session\")\ndeco init update {}\n");
    write(&dir.join("app/stick.toml"), &manifest("deco-app", "[dependencies]\ndeco-lib = { path = \"../lib\" }\n"));
    write(&dir.join("app/app.rock"), "@update\nfn app_update() {}\n");

    let v = json(&migrate(dir, "lib", &["--check", "--consumer", "app", "--format", "json"]));
    assert_eq!(v["verdict"], "migration_needed", "{v}");
    assert_eq!(names(&v["exports"]["pub"]), vec!["update"], "{v}");
    for ctx in v["verification"].as_array().unwrap() {
        assert!(ctx["errors"].as_array().unwrap().is_empty(), "{ctx}");
        assert!(ctx["changed_sites"].as_array().unwrap().is_empty(), "{ctx}");
    }
    let apply = migrate(dir, "lib", &["--apply", "--consumer", "app"]);
    assert_eq!(apply.code, Some(0), "{}", shown(&apply));
}

/// Spec 8.3: in v1 an own `min(Foo, Foo)` hides the std family for the bare
/// name, where legacy merged them. The planner finds the call that would stop
/// resolving and qualifies it, and the result selects the same body.
#[test]
fn a_std_call_an_own_family_would_hide_is_qualified() {
    let fixture = scratch("hidden_std");
    let dir = fixture.path();
    write(&dir.join("lib/stick.toml"), &manifest("hide-lib", ""));
    write(
        &dir.join("lib/lib.rock"),
        "struct Foo {\n    v: int,\n}\n\nfn min(a: Foo, b: Foo) -> Foo {\n    a\n}\n\nfn smaller(a: int, b: int) -> int {\n    min(a, b)\n}\n",
    );
    write(&dir.join("lib/tests/t.rock"), "@test\nfn picks_the_smaller() {\n    assert(smaller(7, 3) == 3)\n}\n");

    let v = json(&migrate(&dir, "lib", &["--check", "--format", "json"]));
    let repairs = v["repairs"].as_array().expect("repairs");
    assert_eq!(repairs.len(), 1, "{v}");
    assert_eq!(repairs[0]["qualifier"], "std", "{v}");
    assert_eq!(repairs[0]["name"], "min", "{v}");

    let apply = migrate(&dir, "lib", &["--apply"]);
    assert_eq!(apply.code, Some(0), "{}", shown(&apply));
    assert!(read(&dir.join("lib/lib.rock")).contains("    std::min(a, b)\n"));
    let tests = rock(&dir, &["test", "lib"]);
    assert!(tests.stdout.contains("1/1 @tests passed"), "{}", shown(&tests));
}

/// A v1 dependency the package's bodies use arrives through one
/// `imports.rock` of package-visible globs (spec 8.7, 15.2); an unused
/// dependency gets no line.
#[test]
fn a_used_v1_dependency_is_imported_through_the_facade() {
    let fixture = scratch("facade");
    let dir = fixture.path();
    let v1 = "[package]\nname = \"base-lib\"\nversion = \"0.1.0\"\ntype = \"lib\"\n\n[language]\nmodules = \"v1\"\n\n[build]\nchannels = [\"dev\"]\ndefault_channel = \"dev\"\n";
    write(&dir.join("base/stick.toml"), v1);
    write(&dir.join("base/base.rock"), "pub fn base_value() -> int {\n    40\n}\n");
    write(&dir.join("idle/stick.toml"), &v1.replace("base-lib", "idle-lib"));
    write(&dir.join("idle/idle.rock"), "pub fn idle_value() -> int {\n    1\n}\n");
    write(
        &dir.join("lib/stick.toml"),
        &manifest("mid-lib", "[dependencies]\nbase-lib = { path = \"../base\" }\nidle-lib = { path = \"../idle\" }\n"),
    );
    write(&dir.join("lib/lib.rock"), "fn mid_value() -> int {\n    base_value() + 2\n}\n");
    write(&dir.join("lib/tests/t.rock"), "@test\nfn adds() {\n    assert(mid_value() == 42)\n    assert(base_value() == 40)\n}\n");

    let v = json(&migrate(&dir, "lib", &["--check", "--format", "json"]));
    let globs = v["imports"]["globs"].as_array().expect("globs");
    assert_eq!(globs.len(), 1, "{v}");
    assert_eq!(globs[0]["alias"], "base_lib", "{v}");
    assert_eq!(globs[0]["fragments"].as_array().unwrap().len(), 2, "both fragments use it: {v}");

    let apply = migrate(&dir, "lib", &["--apply"]);
    assert_eq!(apply.code, Some(0), "{}", shown(&apply));
    assert!(read(&dir.join("lib/imports.rock")).contains("pub(package) use base_lib::*;\n"));
    assert!(!read(&dir.join("lib/imports.rock")).contains("idle_lib"));
    let tests = rock(&dir, &["test", "lib"]);
    assert!(tests.stdout.contains("1/1 @tests passed"), "{}", shown(&tests));
}

/// A legacy dependency behind an `allow_legacy` edge (the author's opt-in)
/// arrives like a v1 one: the migrated bodies reach it through the view of
/// what its own files define (spec 3.2), so its alias gets a glob in
/// `imports.rock`, and the migrated package's tests run.
#[test]
fn an_allow_legacy_dependency_is_imported_through_its_view() {
    let fixture = scratch("legacy_view");
    let dir = fixture.path();
    write(&dir.join("old/stick.toml"), &manifest("old-lib", ""));
    write(&dir.join("old/old.rock"), "fn old_value() -> int {\n    40\n}\n");
    write(
        &dir.join("lib/stick.toml"),
        &manifest("mid-lib", "[dependencies]\nold-lib = { path = \"../old\", allow_legacy = true }\n"),
    );
    write(&dir.join("lib/lib.rock"), "fn mid_value() -> int {\n    old_value() + 2\n}\n");
    write(&dir.join("lib/tests/t.rock"), "@test\nfn adds() {\n    assert(mid_value() == 42)\n}\n");

    let v = json(&migrate(&dir, "lib", &["--check", "--format", "json"]));
    assert_eq!(v["blockers"].as_array().map(Vec::len), Some(0), "{v}");
    let globs = v["imports"]["globs"].as_array().expect("globs");
    assert_eq!(globs.len(), 1, "{v}");
    assert_eq!(globs[0]["alias"], "old_lib", "{v}");

    let apply = migrate(&dir, "lib", &["--apply"]);
    assert_eq!(apply.code, Some(0), "{}", shown(&apply));
    assert!(read(&dir.join("lib/imports.rock")).contains("pub(package) use old_lib::*;\n"));
    let tests = rock(&dir, &["test", "lib"]);
    assert!(tests.stdout.contains("1/1 @tests passed"), "{}", shown(&tests));
}

/// Stop and report rather than write: a legacy dependency without an
/// `allow_legacy` edge (the author's opt-in, which the migrator never adds)
/// and inline Lua naming a symbol the conversion renames. Nothing is written.
/// A definition only a consumer's tests use is not a blocker: it is exported
/// and listed as test-only.
#[test]
fn blockers_are_reported_and_nothing_is_written() {
    let fixture = scratch("blocked");
    let dir = fixture.path();
    write(&dir.join("old/stick.toml"), &manifest("old-lib", ""));
    write(&dir.join("old/old.rock"), "fn old_value() -> int {\n    1\n}\n");
    write(&dir.join("lib/stick.toml"), &manifest("b-lib", "[dependencies]\nold-lib = { path = \"../old\" }\n"));
    let lib_src = "fn b_value() -> int {\n    old_value()\n}\n\nfn b_internal() -> int {\n    2\n}\n\nfn b_reflected() -> int {\n    3\n}\n\n\
                   inline(lua) fn b_raw() -> int = \"\"\"return b_value()\"\"\"\n\n\
                   inline(lua) fn b_by_name() -> int = \"\"\"return _G[\"b_reflected\"]()\"\"\"\n";
    write(&dir.join("lib/lib.rock"), lib_src);
    write(&dir.join("app/stick.toml"), &manifest("b-app", "[dependencies]\nb-lib = { path = \"../lib\" }\n"));
    write(&dir.join("app/app.rock"), "fn app_value() -> int {\n    b_value()\n}\n");
    write(&dir.join("app/tests/app_test.rock"), "@test\nfn reaches_in() {\n    assert(b_internal() == 2)\n}\n");

    let run = migrate(&dir, "lib", &["--apply", "--consumer", "app", "--format", "json"]);
    assert_eq!(run.code, Some(3), "{}", shown(&run));
    let v = json(&run);
    assert_eq!(v["verdict"], "blocked", "{v}");
    let blockers: Vec<String> = v["blockers"].as_array().unwrap().iter().map(|b| b.as_str().unwrap().to_string()).collect();
    let has = |needle: &str| blockers.iter().any(|b| b.contains(needle));
    assert!(has("legacy dependency `old-lib`") && has("allow_legacy"), "{blockers:#?}");
    assert!(!has("`b_internal`"), "{blockers:#?}");
    assert_eq!(v["test_only_uses"][0]["name"], "b_internal", "{v}");
    assert!(has("raw ABI review [exact]") && has("`b_value`"), "{blockers:#?}");
    assert!(has("`b_reflected`") && has("through `_G`"), "{blockers:#?}");
    assert_eq!(read(&dir.join("lib/lib.rock")), lib_src, "a blocked plan writes nothing");
    assert!(!read(&dir.join("lib/stick.toml")).contains("[language]"));
}

/// The raw-ABI remedy depends on what the named definition is: `@nomangle`
/// is accepted on a fn, while a global or a type must be reached through
/// Rock (the compiler rejects `@nomangle` on a global).
#[test]
fn raw_abi_advice_matches_the_definition_kind() {
    let fixture = scratch("raw_abi_advice");
    let dir = fixture.path();
    write(&dir.join("lib/stick.toml"), &manifest("adv-lib", ""));
    write(
        &dir.join("lib/lib.rock"),
        "global COUNTER: int = 0\n\nstruct Slot {\n    n: int,\n}\n\nfn bump() -> int {\n    1\n}\n\n\
         inline(lua) fn read_counter() -> int = \"\"\"return COUNTER\"\"\"\n\n\
         inline(lua) fn slot_table() -> int = \"\"\"return _G.Slot\"\"\"\n\n\
         inline(lua) fn call_bump() -> int = \"\"\"return bump()\"\"\"\n",
    );
    let v = json(&migrate(dir, "lib", &["--check", "--format", "json"]));
    let blockers: Vec<String> = v["blockers"].as_array().unwrap().iter().map(|b| b.as_str().unwrap().to_string()).collect();
    let about = |name: &str| blockers.iter().find(|b| b.contains(&format!("`{name}`"))).unwrap_or_else(|| panic!("{name}: {blockers:#?}"));
    for name in ["COUNTER", "Slot"] {
        let b = about(name);
        assert!(!b.contains("mark the definition `@nomangle`"), "{b}");
        assert!(b.contains("reach it through Rock"), "{b}");
    }
    assert!(about("COUNTER").contains("accessor fn"), "{blockers:#?}");
    assert!(about("bump").contains("mark the fn `@nomangle`"), "{blockers:#?}");
}

/// The raw-ABI census counts only an inline Lua body's real references to a
/// renamed symbol, as LuaJIT compiles the body: a free global read or write,
/// or a resolved field of `_G`. A local, a parameter,
/// a field, a table key, an unrelated string and a
/// comment spelling the name are not references (chess-render's `fn pc`
/// against stone's Lua local `pc`).
#[test]
fn raw_abi_census_counts_only_real_references() {
    let fixture = scratch("raw_abi_exact");
    let dir = fixture.path();
    write(&dir.join("lib/stick.toml"), &manifest("r-lib", ""));
    write(&dir.join("lib/lib.rock"), "fn pc() -> int {\n    3\n}\n");
    write(&dir.join("app/stick.toml"), &manifest("r-app", "[dependencies]\nr-lib = { path = \"../lib\" }\n"));
    write(
        &dir.join("app/app.rock"),
        "fn app_value() -> int {\n    pc()\n}\n\n\
         inline(lua) fn app_trace(func: int, pc: int) -> int = \"\"\"\n\
             -- pc is the program counter\n\
             local x = { pc = 1 }\n\
             local s = \"pc\"\n\
             local function inner(pc) return pc end\n\
             return inner(x.pc) + pc\n\
         \"\"\"\n",
    );
    let run = migrate(&dir, "lib", &["--check", "--consumer", "app", "--format", "json"]);
    let v = json(&run);
    let blockers: Vec<String> = v["blockers"].as_array().unwrap().iter().map(|b| b.as_str().unwrap().to_string()).collect();
    assert!(blockers.is_empty(), "{blockers:#?}");
    assert_eq!(v["verdict"], "migration_needed", "{v}");
    assert_eq!(run.code, Some(3), "{}", shown(&run));
}

#[test]
fn raw_abi_census_uses_the_foreign_scalar_parameter_abi() {
    let fixture = scratch("raw_scalar_params");
    let dir = fixture.path();
    write(&dir.join("lib/stick.toml"), &manifest("r-lib", ""));
    write(&dir.join("lib/lib.rock"), "fn size() -> int { 3 }\n");
    write(&dir.join("app/stick.toml"), &manifest("r-app", "[dependencies]\nr-lib = { path = \"../lib\" }\n"));
    write(&dir.join("app/app.rock"), r#"
nobox value Point { x: float, y: float }
@scalar_abi inline(lua) fn width(size: Point) -> float = "return size__x + size__y"
fn app_value() -> int { size() }
"#);
    let run = migrate(dir, "lib", &["--check", "--consumer", "app", "--format", "json"]);
    let v = json(&run);
    assert_eq!(v["verdict"], "migration_needed", "{v}");
    assert!(v["blockers"].as_array().unwrap().is_empty(), "{v}");
}

#[test]
fn raw_abi_census_distinguishes_global_table_fields_from_other_strings() {
    let fixture = scratch("raw_global_fields");
    let dir = fixture.path();
    write(&dir.join("lib/stick.toml"), &manifest("r-lib", ""));
    write(&dir.join("lib/lib.rock"), "fn r() -> int { 3 }\nfn v(s: string) -> int { 4 }\n");
    write(&dir.join("app/stick.toml"), &manifest("r-app", "[dependencies]\nr-lib = { path = \"../lib\" }\n"));
    write(&dir.join("app/app.rock"), r#"
fn app_value() -> int { r() + v("witness") }
inline(lua) fn record(path: string) -> int = """
    local f = io.open(path, "r")
    local rows = _G.audit_rows
    if rows then
        for _, row in pairs(rows) do row.v = row.v + 1 end
    end
    _G.audit_rows = { v = 1 }
    if f then f:close() end
    return 1
"""
"#);
    let run = migrate(dir, "lib", &["--check", "--consumer", "app", "--format", "json"]);
    let v = json(&run);
    assert_eq!(v["verdict"], "migration_needed", "{v}");
    assert!(v["blockers"].as_array().unwrap().is_empty(), "{v}");
}

#[test]
fn raw_abi_census_reports_paused_javascript_without_blocking_lua() {
    let fixture = scratch("raw_paused_js");
    let dir = fixture.path();
    write(&dir.join("lib/stick.toml"), &manifest("r-lib", ""));
    write(&dir.join("lib/lib.rock"), "fn value() -> int { 3 }\ninline(js) fn raw() -> int = \"return value();\"\n");
    let run = migrate(dir, "lib", &["--check", "--format", "json"]);
    let v = json(&run);
    assert_eq!(v["verdict"], "migration_needed", "{v}");
    assert!(v["notes"].as_array().unwrap().iter().any(|n| n.as_str().unwrap().contains("JavaScript")), "{v}");
}

#[test]
fn raw_abi_census_keeps_real_and_conservative_environment_candidates_blocked() {
    let fixture = scratch("raw_environment_controls");
    let dir = fixture.path();
    write(&dir.join("lib/stick.toml"), &manifest("r-lib", ""));
    write(&dir.join("lib/lib.rock"), "fn pc() -> int { 3 }\n");
    write(&dir.join("app/stick.toml"), &manifest("r-app", "[dependencies]\nr-lib = { path = \"../lib\" }\n"));
    for body in [
        "return pc()",
        "return _G.pc()",
        "return _G[\"pc\"]()",
        "local env = _G; return env.pc()",
        "local env = {}; if flag then env = _G end; return env.pc()",
        "local env = {}; while flag do env = _G; flag = false end; return env.pc()",
        "local function f() return _G.pc() end; return f()",
        "local env = _G; local function f() return env.pc() end; return f()",
        "return rawget(_G, \"pc\")()",
        "local key = flag and \"pc\" or key; return _G[key]()",
        "local env = _G; return helper(env, \"pc\")",
    ] {
        write(&dir.join("app/app.rock"), &format!(
            "fn app_value() -> int {{ pc() }}\ninline(lua) fn probe(flag: bool, key: string) -> int = \"\"\"{body}\"\"\"\n"
        ));
        let run = migrate(dir, "lib", &["--apply", "--consumer", "app", "--format", "json"]);
        let v = json(&run);
        assert_eq!(v["verdict"], "blocked", "{body}: {v}");
        assert!(v["blockers"].as_array().unwrap().iter().any(|b| {
            let b = b.as_str().unwrap();
            b.contains("`pc`") || b.contains("unresolved _G flow")
        }), "{body}: {v}");
        assert!(!read(&dir.join("lib/stick.toml")).contains("modules ="));
    }
}

/// Unresolved environment flow keeps only the strings that could be an
/// environment key. A closure that captures no environment alias, a
/// `rawget(_G, "K")` with a constant key, and a dynamic `_G[...]` index do not
/// let the environment escape, so a field key on another table (`row.pc`,
/// `{ pc = 1 }`) cannot name a global there; the Stone bodies at
/// serialization.rock:187, snapshot.rock:59, state_image.rock:660 and
/// c_struct_dispatch.rock:10 have these shapes. A string value that may reach
/// a dynamic index, and every string once the environment escapes, still
/// count.
#[test]
fn raw_abi_census_ignores_field_keys_when_the_environment_does_not_escape() {
    let fixture = scratch("raw_field_keys");
    let dir = fixture.path();
    write(&dir.join("lib/stick.toml"), &manifest("r-lib", ""));
    write(&dir.join("lib/lib.rock"), "fn pc() -> int { 3 }\n");
    write(&dir.join("app/stick.toml"), &manifest("r-app", "[dependencies]\nr-lib = { path = \"../lib\" }\n"));
    let run_body = |body: &str| {
        write(&dir.join("app/app.rock"), &format!(
            "fn app_value() -> int {{ pc() }}\ninline(lua) fn probe(flag: bool, key: string) -> int = \"\"\"{body}\"\"\"\n"
        ));
        json(&migrate(dir, "lib", &["--check", "--consumer", "app", "--format", "json"]))
    };
    for body in [
        "_G.__probe_seen = true; local function f(row) return row.pc end; return f({ pc = 1 })",
        "local names = _G[\"__probe_\" .. key]; local t = { pc = 1 }; t.pc = 2; return names and t.pc or 0",
        "local reg = rawget(_G, \"__PROBE_REG\"); return reg and reg.pc or 0",
    ] {
        let v = run_body(body);
        assert_eq!(v["verdict"], "migration_needed", "{body}: {v}");
        assert!(v["blockers"].as_array().unwrap().is_empty(), "{body}: {v}");
    }
    for body in [
        "local t = { k = \"pc\" }; return _G[t.k]()",
        "local names = _G[key]; local s = \"pc\"; return names and #s or 0",
        "local env = _G; local function f(row) return env[row] end; return f(\"x\").pc",
        "local k = \"pc\"; if flag then k = key end; return rawget(_G, k)()",
        "local n = 0; for k in pairs({ pc = 1 }) do n = n + _G[k]() end; return n",
    ] {
        let v = run_body(body);
        assert_eq!(v["verdict"], "blocked", "{body}: {v}");
        assert!(v["blockers"].as_array().unwrap().iter().any(|b| b.as_str().unwrap().contains("`pc`")), "{body}: {v}");
    }
    let v = run_body("return rawget(_G, \"pc\")()");
    assert!(v["blockers"].as_array().unwrap().iter().any(|b| {
        let b = b.as_str().unwrap();
        b.contains("[exact]") && b.contains("`pc` through `_G`")
    }), "a constant rawget key is an exact environment field: {v}");
}

#[test]
fn raw_abi_census_reports_dynamic_name_coverage_without_claiming_an_exact_reference() {
    let fixture = scratch("raw_dynamic_name");
    let dir = fixture.path();
    write(&dir.join("lib/stick.toml"), &manifest("r-lib", ""));
    write(&dir.join("lib/lib.rock"), "fn pc() -> int { 3 }\ninline(lua) fn lookup(key: string) -> int = \"return _G[key]()\"\n");
    let run = migrate(dir, "lib", &["--check", "--format", "json"]);
    let v = json(&run);
    assert_eq!(v["verdict"], "migration_needed", "{v}");
    assert!(v["blockers"].as_array().unwrap().is_empty(), "{v}");
    assert!(v["notes"].as_array().unwrap().iter().any(|n| {
        n.as_str().unwrap().contains("dynamically computed names require manual review")
    }), "{v}");
}

/// A package that does not check before migration is an analysis failure
/// (exit 4), never a plan.
#[test]
fn a_package_that_does_not_check_is_an_analysis_failure() {
    let fixture = scratch("broken");
    let dir = fixture.path();
    write(&dir.join("lib/stick.toml"), &manifest("broken-lib", ""));
    write(&dir.join("lib/lib.rock"), "fn wrong() -> int {\n    \"not an int\"\n}\n");
    let run = migrate(&dir, "lib", &["--check", "--format", "json"]);
    assert_eq!(run.code, Some(4), "{}", shown(&run));
    assert_eq!(json(&run)["verdict"], "analysis_failed");
}

/// Copy what the chess graph compiles from (manifests and Rock sources only)
/// into `dest`, keeping the repository layout so manifest paths resolve.
fn copy_chess_graph(dest: &Path) {
    let root = repo_root();
    for rel in ["stone", "stone-core", "libs/chess-core", "libs/chess-game", "libs/chess-render", "libs/chess-sfx", "games/chess", "games/chesslings"] {
        copy_sources(&root.join(rel), &dest.join(rel));
    }
}

fn copy_sources(from: &Path, to: &Path) {
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_sources(&path, &target);
        } else if path.extension().is_some_and(|e| e == "rock") || entry.file_name() == "stick.toml" {
            std::fs::create_dir_all(to).unwrap();
            std::fs::copy(&path, &target).unwrap();
        }
    }
}

/// The M6 hand conversion of `libs/chess-core`, reproduced: strip its `pub`
/// modifiers and `[language]` table to recover the legacy package, migrate it
/// against its four consumers, and compare with the committed source. The
/// hand conversion also exported `ch_is_black` and `ch_parse_square`, which
/// only consumer tests and sims call; a definition a consumer's tests use is
/// exported like any other use (author decision), and reported as test-only.
///
/// The chess-game and chess-render consumers are v1, so de-migrating
/// chess-core turns its declared dependency into a legacy boundary. The copy
/// marks it `allow_legacy = true`, which is what the graph would have to say
/// for the pre-migration world to be well formed -- without it the planner
/// refuses at the consumer stage, correctly. So the round trip now also covers
/// a v1 consumer of the package being migrated.
#[test]
fn chess_core_migrates_to_the_committed_hand_conversion() {
    let fixture = scratch("chess_core");
    let dir = fixture.path();
    copy_chess_graph(&dir);
    let core = dir.join("libs/chess-core");
    let committed: Vec<(String, String)> = ["chess_board.rock", "chess_moves.rock", "stick.toml"]
        .iter()
        .map(|f| (f.to_string(), read(&core.join(f))))
        .collect();
    for (f, text) in &committed {
        let legacy = if f == "stick.toml" {
            text.replace("[language]\nmodules = \"v1\"\n\n", "")
        } else {
            text.lines()
                .map(|l| l.strip_prefix("pub ").unwrap_or(l))
                .collect::<Vec<_>>()
                .join("\n")
                + "\n"
        };
        assert_ne!(&legacy, text, "{f} must change when de-migrated");
        write(&core.join(f), &legacy);
    }
    for consumer in ["chess-game", "chess-render"] {
        let game_manifest_path = dir.join(format!("libs/{consumer}/stick.toml"));
        let game_manifest = read(&game_manifest_path);
        let allow_legacy = game_manifest.replace(
            "chess-core = { path = \"../chess-core\" }",
            "chess-core = { path = \"../chess-core\", allow_legacy = true }",
        );
        assert_ne!(allow_legacy, game_manifest, "the v1 consumer's chess-core edge must change");
        write(&game_manifest_path, &allow_legacy);
    }

    let consumers = ["--consumer", "libs/chess-game", "--consumer", "libs/chess-render", "--consumer", "games/chess", "--consumer", "games/chesslings"];

    let mut args = vec!["--check", "--format", "json"];
    args.extend_from_slice(&consumers);
    let v = json(&migrate(&dir, "libs/chess-core", &args));
    assert_eq!(v["verdict"], "migration_needed", "{v}");
    assert_eq!(v["blockers"].as_array().unwrap().len(), 0, "{v}");
    let mut test_only: Vec<&str> =
        v["test_only_uses"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    test_only.sort();
    assert_eq!(test_only, vec!["ch_is_black", "ch_parse_square"], "{v}");
    let exported: Vec<&str> = v["exports"]["pub"].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap()).collect();
    assert!(exported.contains(&"ch_is_black") && exported.contains(&"ch_parse_square"), "{v}");

    let mut args = vec!["--apply"];
    args.extend_from_slice(&consumers);
    let apply = migrate(&dir, "libs/chess-core", &args);
    assert_eq!(apply.code, Some(0), "{}", shown(&apply));
    for (f, text) in &committed {
        assert_eq!(&read(&core.join(f)), text, "{f} differs from the committed hand conversion");
    }
}
