//! `@test(backends = "...")` limits a test to the listed backends. Every other
//! backend neither compiles nor runs it, and the JS summary counts it apart
//! from real skips, so the CI skip cap measures only JS gaps.
#![cfg(feature = "backend-js")]

use crate::support::{repo_root, rock_test_source};

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

/// The skip count CI reads, extracted with the same sed expression ci.yml runs.
fn ci_skip_count(text: &str) -> String {
    let ci =
        std::fs::read_to_string(repo_root().join(".github/workflows/ci.yml")).expect("read ci.yml");
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
    let (ok, text) = rock_test_source(FIXTURE, &["--backend", "js"]);
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
    let (ok, text) = rock_test_source(FIXTURE, &[]);
    assert!(ok, "Lua run must pass:\n{text}");
    assert!(
        text.contains("3/3 @tests passed"),
        "the js-scoped test must not run on Lua:\n{text}"
    );
}

#[test]
fn unknown_backend_name_is_a_compile_error() {
    let (ok, text) = rock_test_source("@test(backends = \"luajit\")\nfn a() { }\n", &[]);
    assert!(!ok, "an unknown backend must fail the run:\n{text}");
    assert!(
        text.contains("unknown backend `luajit`"),
        "the diagnostic must name the bad backend:\n{text}"
    );
}

#[test]
fn js_runner_keeps_supported_tests_when_emission_rejects_other_roots() {
    let cases = [
        ("", "assert(lua_only(42) == 42)"),
        (
            r#"global initialized: int = 0
deco init register { initialized = lua_only(42) }
@register fn hook() {}"#,
            "assert(initialized == 42)",
        ),
    ];
    for (setup, body) in cases {
        let source = format!(
            r#"
inline(lua) fn lua_only(n: int) -> int = "return n"
inline(js) fn from_js(n: int) -> int = "return n;"
global seed: int = from_js(1)
{setup}
@test(returns = 1) fn supported() -> int {{ seed }}
@test fn unsupported() {{ {body} }}
"#
        );
        let (ok, text) = rock_test_source(&source, &["--backend", "js"]);
        assert!(ok, "{text}");
        assert!(
            text.contains("1/2 @tests passed (0 failed, 1 skipped"),
            "{text}"
        );
        assert!(
            text.contains("SKIP unsupported: JS subset: call to `lua_only`"),
            "{text}"
        );
    }
}

/// Both failure paths: a throw under node and a value the host comparison
/// rejects.
#[test]
fn failing_js_tests_exit_nonzero_and_name_each_failure() {
    if !crate::support::require_node() {
        return;
    }
    let (ok, text) = rock_test_source(
        r#"
@test
fn deliberately_failing() {
    assert(1 == 2, "this test is supposed to fail")
}

@test(returns = 43)
fn mismatched_value() -> int { return 42 }
"#,
        &["--backend", "js"],
    );
    assert!(!ok, "failing @tests must exit nonzero:\n{text}");
    assert!(text.contains("0/2 @tests passed (2 failed"), "{text}");
    assert!(text.contains("FAIL deliberately_failing: error:"), "{text}");
    assert!(
        text.contains("FAIL mismatched_value: value mismatch: expected <43>"),
        "{text}"
    );
}
