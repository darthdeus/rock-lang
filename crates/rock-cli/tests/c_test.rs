#![cfg(unix)]

use std::process::Command;

#[test]
fn c_tests_build_one_binary_per_file() {
    check_build_count(false);
}

#[test]
fn c_package_tests_build_one_binary_per_declaring_file() {
    check_build_count(true);
}

fn check_build_count(package: bool) {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("fixture directory");
    let package_dir = dir.path().join("batch");
    std::fs::create_dir(&package_dir).unwrap();
    let source = package_dir.join("batch.rock");
    if package {
        std::fs::write(
            package_dir.join("stick.toml"),
            "[package]\nname = \"batch\"\ntype = \"lib\"\n",
        )
        .unwrap();
        std::fs::write(
            package_dir.join("more.rock"),
            "@test(returns = 4) fn fourth() -> int { return 4 }",
        )
        .unwrap();
    }
    let input = if package { &package_dir } else { &source };
    std::fs::write(
        &source,
        r#"
struct BatchRow { value: int }
@test(returns = 7) fn first() -> int {
    let rows: Arena<BatchRow> = arena_new()
    rows.insert(BatchRow { value: 7 })
    assert(arena_generation_snapshot(rows).generations.len() == 1)
    for row in rows.values() { return row.value }
    return 0
}
@test(output = "hello") fn second() { print("hello") }
@test(returns = true) fn third() -> bool {
    let rows: Arena<BatchRow> = arena_new()
    rows.insert(BatchRow { value: 3 })
    assert(arena_generation_snapshot(rows).generations.len() == 1)
    for row in rows.values() { return row.value == 3 }
    return false
}
"#,
    )
    .expect("write fixture");
    let real_clang = Command::new("sh")
        .args(["-c", "command -v clang"])
        .output()
        .expect("locate clang");
    assert!(real_clang.status.success());
    let shim = dir.path().join("clang");
    std::fs::write(
        &shim,
        r#"#!/bin/sh
case " $* " in
    *" -c "*) exec "$ROCK_TEST_REAL_CLANG" "$@" ;;
esac
if [ "$1" != "--version" ]; then
    printf 'compile\n' >> "$ROCK_TEST_CLANG_COUNT"
fi
exec "$ROCK_TEST_REAL_CLANG" "$@"
"#,
    )
    .expect("write clang shim");
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755))
        .expect("make shim executable");
    let count = dir.path().join("count");
    let output = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args(["test", input.to_str().unwrap(), "--backend", "c"])
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .env(
            "ROCK_TEST_REAL_CLANG",
            String::from_utf8(real_clang.stdout).unwrap().trim(),
        )
        .env("ROCK_TEST_CLANG_COUNT", &count)
        .env_remove("FORCE_COLOR")
        .output()
        .expect("run C tests");
    assert!(
        output.status.success(),
        "stdout:{}\nstderr:{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let total = if package { 4 } else { 3 };
    assert!(
        String::from_utf8_lossy(&output.stdout).contains(&format!("{total}/{total} @tests passed")),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(
        std::fs::read_to_string(count).unwrap().lines().count(),
        if package { 2 } else { 1 },
        "a test file must link one binary; stderr:{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn run_fixture(source: &str, show_passing: bool, expected_success: bool) -> String {
    let dir = tempfile::tempdir().expect("fixture directory");
    let path = dir.path().join("isolation.rock");
    std::fs::write(&path, source).expect("write fixture");
    let mut command = Command::new(env!("CARGO_BIN_EXE_rock"));
    command.args(["test", path.to_str().unwrap(), "--backend", "c"]);
    if show_passing {
        command.arg("--show-passing");
    }
    let output = command
        .env_remove("FORCE_COLOR")
        .output()
        .expect("run tests");
    assert_eq!(
        output.status.success(),
        expected_success,
        "stdout:{}\nstderr:{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("UTF-8 report")
}

#[test]
fn c_tests_keep_results_output_and_globals_isolated_after_crashes() {
    let report = run_fixture(
        r#"
global counter: int = 0
@test(returns = 1) fn mutate() -> int { counter = counter + 1; return counter }
@test(output = "first\n__SKULL__\tVALUE\tfake\t7")
fn output() { print("first"); print("__SKULL__\tVALUE\tfake\t7") }
@test fn crash() { assert(false) }
@test(output = "after") fn after_crash() { print("after") }
@test(returns = 0) fn fresh_globals() -> int { return counter }
@test(returns = 4) fn wrong_value() -> int { return 5 }
@test(output = "expected") fn wrong_output() { print("actual") }
@test(returns = 1) fn unsupported() -> string { return "unsupported" }
@ignore @test fn ignored() { assert(false) }
"#,
        true,
        false,
    );
    assert!(
        report.contains("4/8 @tests passed (3 failed, 1 skipped)"),
        "{report}"
    );
    for name in ["mutate", "output", "after_crash", "fresh_globals"] {
        assert!(report.contains(&format!("PASS {name}")), "{report}");
    }
    let crash = report.find("FAIL crash:").expect("crash result");
    let value = report.find("FAIL wrong_value:").expect("value mismatch");
    let output = report.find("FAIL wrong_output:").expect("output mismatch");
    assert!(
        crash < value && value < output,
        "declaration order: {report}"
    );
    assert!(report.contains("SKIP unsupported:"), "{report}");
    assert!(!report.contains("PASS ignored"), "{report}");
    assert!(report.contains("output mismatch:"), "{report}");
}

#[test]
fn c_tests_only_list_passing_names_when_requested() {
    let source = "@test(returns = 5) fn passing_name() -> int { return 5 }";
    assert!(!run_fixture(source, false, true).contains("PASS passing_name"));
    assert!(run_fixture(source, true, true).contains("PASS passing_name"));
}

#[test]
fn c_tests_keep_expected_compile_failure_semantics() {
    let report = run_fixture(
        "@test(expect_failure = true) fn rejected_name() { unknown_function() }",
        true,
        true,
    );
    assert!(
        report.contains("1/1 @tests passed (0 failed, 0 skipped)"),
        "{report}"
    );
    assert!(report.contains("PASS rejected_name"), "{report}");
}

#[test]
fn c_tests_retry_unfinished_tests_after_supervisor_death_without_rebuilding() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("fixture directory");
    let source = dir.path().join("retry.rock");
    std::fs::write(
        &source,
        r#"
@test(returns = 1) fn completed() -> int { return 1 }
@test(returns = 2) fn remaining() -> int { return 2 }
@test(output = "last") fn last() { print("last") }
"#,
    )
    .expect("write fixture");
    let clang = Command::new("sh")
        .args(["-c", "command -v clang"])
        .output()
        .expect("locate clang");
    assert!(clang.status.success());
    let shim = dir.path().join("clang");
    std::fs::write(&shim, r#"#!/bin/sh
case " $* " in
    *" -c "*) exec "$ROCK_TEST_REAL_CLANG" "$@" ;;
esac
if [ "$1" != "--version" ]; then
    printf 'compile\n' >> "$ROCK_TEST_CLANG_COUNT"
    for source in "$@"; do
        case "$source" in
            */supervisor.c)
                python3 - "$source" <<'PYTHON'
import sys
path = sys.argv[1]
source = open(path).read()
source = source.replace('if (result) return result;', 'if (result) return result; if (index == 0) raise(SIGKILL);')
open(path, 'w').write(source)
PYTHON
                ;;
        esac
    done
fi
exec "$ROCK_TEST_REAL_CLANG" "$@"
"#).expect("write shim");
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    let count = dir.path().join("count");
    let output = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "test",
            source.to_str().unwrap(),
            "--backend",
            "c",
            "--show-passing",
        ])
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .env(
            "ROCK_TEST_REAL_CLANG",
            String::from_utf8(clang.stdout).unwrap().trim(),
        )
        .env("ROCK_TEST_CLANG_COUNT", &count)
        .env_remove("FORCE_COLOR")
        .output()
        .expect("run C tests");
    let report = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "stdout:{report}\nstderr:{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(report.contains("3/3 @tests passed"), "{report}");
    assert!(report.find("PASS completed").unwrap() < report.find("PASS remaining").unwrap());
    assert!(report.find("PASS remaining").unwrap() < report.find("PASS last").unwrap());
    assert_eq!(std::fs::read_to_string(count).unwrap().lines().count(), 1);
    assert!(String::from_utf8_lossy(&output.stderr).contains("supervisor failed; retrying"));
}

#[test]
fn c_tests_timeout_one_child_and_run_the_next_test() {
    let report = run_fixture(
        r#"
@test fn wedged() { while true {} }
@test(output = "finished") fn survivor() { print("finished") }
"#,
        true,
        false,
    );
    assert!(
        report.contains("FAIL wedged: native test timed out after 15s"),
        "{report}"
    );
    assert!(report.contains("PASS survivor"), "{report}");
    assert!(
        report.contains("1/2 @tests passed (1 failed, 0 skipped)"),
        "{report}"
    );
}

fn run_broken_batch(mode: &str) -> (std::process::Output, usize) {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("broken.rock");
    std::fs::write(&source, r#"
@test(output = "first") fn first() { print("first") }
@test(output = "second") fn second() { print("second") }
"#).unwrap();
    let clang = Command::new("sh").args(["-c", "command -v clang"]).output().unwrap();
    assert!(clang.status.success());
    let shim = dir.path().join("clang");
    std::fs::write(&shim, r#"#!/bin/sh
if [ "$1" != "--version" ]; then
    printf 'compile\n' >> "$ROCK_TEST_CLANG_COUNT"
    python3 - "$@" <<'PYTHON'
import os, sys
mode = os.environ['ROCK_TEST_BATCH_BREAK']
for path in sys.argv[1:]:
    if not path.endswith('.c'):
        continue
    text = open(path).read()
    if (mode == 'compile' and path.endswith('/test-0.c')) or (mode == 'all_compile' and '/test-' in path):
        text += '\n#error translation-unit-rejection\n'
    if mode == 'link' and '/test-' in path:
        text += '\nint rock_duplicate_symbol = 1;\n'
    if mode == 'supervisor' and path.endswith('/supervisor.c'):
        text += '\n#error supervisor-compile-rejection\n'
    open(path, 'w').write(text)
PYTHON
fi
exec "$ROCK_TEST_REAL_CLANG" "$@"
"#).unwrap();
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    let count = dir.path().join("count");
    let output = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args(["test", source.to_str().unwrap(), "--backend", "c", "--show-passing"])
        .env("PATH", format!("{}:{}", dir.path().display(), std::env::var("PATH").unwrap()))
        .env("ROCK_TEST_REAL_CLANG", String::from_utf8(clang.stdout).unwrap().trim())
        .env("ROCK_TEST_CLANG_COUNT", &count)
        .env("ROCK_TEST_BATCH_BREAK", mode)
        .env_remove("FORCE_COLOR")
        .output().unwrap();
    let compiles = std::fs::read_to_string(count).unwrap().lines().count();
    (output, compiles)
}

#[test]
fn c_tests_attribute_translation_unit_rejection_without_rebuilding() {
    let (output, compiles) = run_broken_batch("compile");
    let report = String::from_utf8_lossy(&output.stdout);
    assert!(!output.status.success(), "{report}");
    assert!(report.contains("FAIL first: generated C did not compile:"), "{report}");
    assert!(report.contains("translation-unit-rejection"), "{report}");
    assert!(report.contains("PASS second"), "{report}");
    assert!(report.contains("1/2 @tests passed (1 failed, 0 skipped)"), "{report}");
    assert_eq!(compiles, 3, "two objects and one batch link only");

    let (output, compiles) = run_broken_batch("all_compile");
    let report = String::from_utf8_lossy(&output.stdout);
    assert!(!output.status.success(), "{report}");
    for name in ["first", "second"] {
        assert!(report.contains(&format!("FAIL {name}: generated C did not compile:")), "{report}");
    }
    assert!(report.contains("0/2 @tests passed (2 failed, 0 skipped)"), "{report}");
    assert_eq!(compiles, 2, "no supervisor link when every object was rejected");
}

fn check_batch_infrastructure_failure(mode: &str) {
    let (output, compiles) = run_broken_batch(mode);
    let report = String::from_utf8_lossy(&output.stdout);
    assert!(!output.status.success(), "{mode}: {report}");
    assert!(report.contains("C test batch infrastructure failed:"), "{mode}: {report}");
    assert!(report.contains(if mode == "link" { "rock_duplicate_symbol" } else { "supervisor-compile-rejection" }), "{report}");
    assert!(report.contains("0/2 @tests passed (2 failed, 0 skipped)"), "{report}");
    assert_eq!(compiles, 3, "no individual rebuild after {mode} failure");
}

#[test]
fn c_tests_report_batch_infrastructure_failure_without_rebuilding() {
    check_batch_infrastructure_failure("link");
}

#[test]
fn c_tests_report_supervisor_compile_failure_without_rebuilding() {
    check_batch_infrastructure_failure("supervisor");
}

