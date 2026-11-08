//! JS coroutine emission: the echo shape (two yields, resume x3),
//! resume-with-value, terminal return via result(), status/kill/last_error.
//! Runs identically on luajit; the JS trampoline emission must match -- no
//! skips.

// The corpus file, copied to a temp file so the run sees only this unit and
// runs it in-process.
const CORPUS: &str = include_str!("../../../../tests/coroutines/js_coroutines.rock");

#[test]
fn js_coroutines_run_green_under_node() {
    if !crate::command::require_node() {
        return;
    }
    let dir = tempfile::tempdir().expect("fixture dir");
    let file = dir.path().join("js_coroutines.rock");
    std::fs::write(&file, CORPUS).expect("write fixture");
    let paths = vec![file.to_string_lossy().into_owned()];
    let summary = rock_cli::js_test::js_test_run(&paths, false).expect("js test run");

    assert_eq!(summary.unit_failures, 0, "no unit should fail to build/run");
    assert_eq!(summary.tests_failed, 0, "no test should FAIL under node");
    assert_eq!(
        summary.tests_skipped,
        0,
        "coroutine tests must emit, not skip. Skip histogram:\n{}",
        summary
            .skip_reasons
            .iter()
            .map(|(r, c)| format!("  {c:>4}  {r}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert_eq!(summary.tests_passed, 8);
}
