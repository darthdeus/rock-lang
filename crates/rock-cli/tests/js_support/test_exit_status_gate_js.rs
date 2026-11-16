//! A failing `@test` under the JS executor exits nonzero, whether it throws
//! under node or returns a value the host-side comparison rejects. The two
//! backends once disagreed on the exit status for the same failing input.

const FAILING: &str = r#"
@test
fn deliberately_failing() {
    assert(1 == 2, "this test is supposed to fail")
}

@test(returns = 43)
fn mismatched_value() -> int { return 42 }
"#;

#[test]
fn failing_js_tests_exit_nonzero_and_name_each_failure() {
    if !crate::command::require_node() {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("probe.rock");
    std::fs::write(&file, FAILING).expect("write probe");
    let out = crate::command::rock_bin()
        .args(["test", file.to_str().unwrap(), "--backend", "js"])
        .output()
        .expect("spawn rock");
    let text = test_utils::strip_ansi_sgr(&format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    ));
    assert!(
        !out.status.success(),
        "failing @tests must exit nonzero:\n{text}"
    );
    assert!(text.contains("0/2 @tests passed (2 failed"), "{text}");
    assert!(text.contains("FAIL deliberately_failing: error:"), "{text}");
    assert!(
        text.contains("FAIL mismatched_value: value mismatch: expected <43>"),
        "{text}"
    );
}
