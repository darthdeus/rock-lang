//! `@test(backends = "...")` limits a test to the listed backends. Every other
//! backend neither compiles nor runs it, and the JS summary counts it apart
//! from real skips, so the CI skip cap measures only JS gaps.

use std::path::Path;

/// Each backend-limited test pins the output only its own backend produces, so
/// running it anywhere else fails.
const FIXTURE: &str = r#"
@nomangle inline(lua) fn scope_backend_name() -> string = """return "lua" """
@nomangle inline(js) fn scope_backend_name() -> string = """return "js";"""

@test(output = "lua", backends = "lua")
fn lua_scoped() { print(scope_backend_name()) }

@test(output = "js", backends = "js")
fn js_scoped() { print(scope_backend_name()) }

@test(backends = "lua, js")
fn listed_both() { assert(1 + 1 == 2) }

@test
fn unscoped() { assert(2 + 2 == 4) }
"#;

fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
}

/// Writes `src` to a temp `.rock` and runs `rock test <file> [args]`.
fn rock_test(src: &str, args: &[&str]) -> (bool, String) {
    let dir = tempfile::tempdir().expect("fixture dir");
    let file = dir.path().join("scoped.rock");
    std::fs::write(&file, src).expect("write fixture");
    let out = crate::command::rock_bin()
        .arg("test")
        .arg(&file)
        .args(args)
        .current_dir(workspace_root())
        .output()
        .expect("spawn rock");
    let text = test_utils::strip_ansi_sgr(&format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    ));
    (out.status.success(), text)
}

/// The skip count CI reads, extracted with the same sed expression ci.yml runs.
fn ci_skip_count(text: &str) -> String {
    let ci = std::fs::read_to_string(workspace_root().join(".github/workflows/ci.yml"))
        .expect("read ci.yml");
    let line = ci
        .lines()
        .find(|l| l.contains("sed -n") && l.contains("js backend"))
        .expect("ci.yml extracts the JS skip count with sed");
    let start = line.find("sed -n '").expect("quoted sed expression") + "sed -n '".len();
    let len = line[start..].find('\'').expect("closing quote");
    let expr = &line[start..start + len];
    let mut child = std::process::Command::new("sed")
        .args(["-n", expr])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn sed");
    use std::io::Write;
    child
        .stdin
        .take()
        .expect("sed stdin")
        .write_all(text.as_bytes())
        .expect("feed sed");
    let out = child.wait_with_output().expect("sed output");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn js_runner_counts_other_backend_tests_apart_from_skips() {
    let (ok, text) = rock_test(FIXTURE, &["--backend", "js"]);
    assert!(ok, "JS run must pass:\n{text}");
    assert!(
        text.contains("js backend: 3/3 @tests passed (0 failed, 0 skipped, 1 not for js)"),
        "the lua-scoped test must neither run nor count as a skip:\n{text}"
    );
    assert_eq!(
        ci_skip_count(&text),
        "0",
        "CI must read the real skip count"
    );
}

#[test]
fn ci_skip_count_reads_the_skip_number_not_the_backend_count() {
    let line = "js backend: 10/12 @tests passed (0 failed, 2 skipped, 7 not for js) \
                across 3 files (1.00s)\n";
    assert_eq!(ci_skip_count(line), "2");
}

#[test]
fn lua_runner_runs_only_tests_scoped_to_lua() {
    let (ok, text) = rock_test(FIXTURE, &[]);
    assert!(ok, "Lua run must pass:\n{text}");
    assert!(
        text.contains("3/3 @tests passed"),
        "the js-scoped test must not run on Lua:\n{text}"
    );
}

#[test]
fn unknown_backend_name_is_a_compile_error() {
    let (ok, text) = rock_test("@test(backends = \"luajit\")\nfn a() { }\n", &[]);
    assert!(!ok, "an unknown backend must fail the run:\n{text}");
    assert!(
        text.contains("unknown backend `luajit`"),
        "the diagnostic must name the bad backend:\n{text}"
    );
}
