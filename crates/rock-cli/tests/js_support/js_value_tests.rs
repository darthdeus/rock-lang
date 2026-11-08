//! `@test(returns = <literal>)` under the JS executor: the node harness emits
//! VALUE marker lines and the shared driver compares host-side, so a value
//! mismatch FAILs and matching int/float/bool values pass -- the same
//! `test_protocol::compare_value` the luajit path compares through.

fn run_src(src: &str) -> rock_cli::js_test::JsSuiteSummary {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("val.rock");
    std::fs::write(&file, src).expect("write fixture");
    rock_cli::js_test::js_test_run(&[file.to_string_lossy().into_owned()], false)
        .expect("js test run")
}

#[test]
fn js_value_mismatch_fails() {
    if !crate::command::require_node() {
        return;
    }
    let summary = run_src("@test(returns = 43)\nfn r() -> int { return 42 }\n");
    assert_eq!(summary.unit_failures, 0);
    assert_eq!(summary.tests_passed, 0);
    assert_eq!(
        summary.tests_failed, 1,
        "mismatching value test must FAIL under node"
    );
}

#[test]
fn js_value_tests_pass_int_float_bool() {
    if !crate::command::require_node() {
        return;
    }
    let summary = run_src(
        "@test(returns = 42)\nfn a() -> int { return 42 }\n\
         @test(returns = -1.5)\nfn b() -> float { return -1.5 }\n\
         @test(returns = true)\nfn c() -> bool { return true }\n",
    );
    assert_eq!(summary.unit_failures, 0);
    assert_eq!(summary.tests_failed, 0);
    assert_eq!(summary.tests_skipped, 0);
    assert_eq!(summary.tests_passed, 3);
}
