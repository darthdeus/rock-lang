//! JS coroutine emission (S4a Slice B): the `tests/js_coroutines.rock` corpus
//! (echo two-yield, resume-with-value, result/status/kill/last_error) must
//! emit through the trampoline path and run GREEN under node -- no skips.

fn node_available() -> bool {
    std::process::Command::new("node")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn js_coroutines_run_green_under_node() {
    if !node_available() {
        eprintln!("SKIP js_coroutines_run_green_under_node: `node` not on PATH");
        return;
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root");
    let paths = vec![root.join("tests/js_coroutines.rock").to_string_lossy().into_owned()];
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
    assert_eq!(summary.tests_passed, 7);
}
