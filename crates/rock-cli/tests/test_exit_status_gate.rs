//! `rock test` must EXIT NONZERO when a `@test` fails.
//!
//! CI (`.github/workflows/test.yml`) keys on the exit status of `cargo run --bin
//! rock test tests/`, so an exit-0-on-failure makes the whole Rock suite
//! unable to fail CI -- only an emit failure could. The report line and the
//! status are separate signals and both are gated here: a run that prints
//! `(N failed)` and exits 0 is the bug, and a run that exits 1 with everything
//! passing would be just as wrong.


/// Writes `src` into a fresh tempdir and runs `rock test` on it, returning
/// (combined output, exit-status-success).
fn run_test(src: &str) -> (String, bool) {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("probe.rock");
    std::fs::write(&file, src).expect("write probe");
    let mut cmd = crate::command::rock_bin();
    cmd.arg("test").arg(&file);
    let out = cmd.output().expect("spawn rock");
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (text, out.status.success())
}

const FAILING_ASSERT: &str = r#"
@test
fn deliberately_failing() {
    assert(1 == 2, "this test is supposed to fail")
}
"#;

const FAILING_OUTPUT: &str = r#"
@test(output = "expected")
fn wrong_output() {
    print("actual")
}
"#;

const PASSING: &str = r#"
@test
fn genuinely_passing() {
    assert(1 == 1, "holds")
}
"#;

#[test]
fn failing_assert_exits_nonzero() {
    let (text, ok) = run_test(FAILING_ASSERT);
    assert!(text.contains("1 failed"), "expected the failure to be counted:\n{text}");
    assert!(!ok, "a failing @test must exit nonzero:\n{text}");
}

#[test]
fn failing_output_mismatch_exits_nonzero() {
    let (text, ok) = run_test(FAILING_OUTPUT);
    assert!(!ok, "an @test(output=..) mismatch must exit nonzero:\n{text}");
}

#[test]
fn passing_test_exits_zero() {
    let (text, ok) = run_test(PASSING);
    assert!(text.contains("1/1 @tests passed"), "expected a clean pass:\n{text}");
    assert!(ok, "a passing @test must exit zero:\n{text}");
}
