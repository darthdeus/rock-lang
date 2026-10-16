
fn package_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("test filter package fixture");
    let project = dir.path();
    std::fs::create_dir_all(project.join("tests")).unwrap();
    std::fs::write(
        project.join("stick.toml"),
        "[package]\nname = \"test-filter-fixture\"\ntype = \"lib\"\n",
    )
    .unwrap();
    std::fs::write(project.join("main.rock"), "fn helper() -> int { 1 }\n").unwrap();
    std::fs::write(
        project.join("tests/combat_text.rock"),
        "@test\nfn combat_text_pipeline_fixture() { assert(helper() == 1) }\n",
    )
    .unwrap();
    std::fs::write(
        project.join("tests/other.rock"),
        "@test\nfn unselected_test_must_not_run() { assert(false, \"unselected test ran\") }\n",
    )
    .unwrap();
    dir
}

#[test]
fn environment_filter_runs_matching_package_tests_and_reports_skips() {
    let fixture = package_fixture();
    let output = crate::command::rock_bin()
        .arg("test")
        .arg(fixture.path())
        .env("ROCK_TEST_FILTER", "combat_text_pipeline")
        .output()
        .expect("run filtered package tests");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        output.status.success(),
        "the nonmatching failing test must be skipped:\n{text}"
    );
    assert!(text.contains("1/1 @tests passed"), "only one test ran:\n{text}");
    assert!(
        text.contains("filter 'combat_text_pipeline': 1 selected, 1 skipped"),
        "the filter summary reports the selected and skipped tests:\n{text}"
    );
}

#[test]
fn command_line_filter_overrides_environment_filter() {
    let fixture = package_fixture();
    let output = crate::command::rock_bin()
        .arg("test")
        .arg("--filter")
        .arg("combat_text_pipeline")
        .arg(fixture.path())
        .env("ROCK_TEST_FILTER", "unselected_test_must_not_run")
        .output()
        .expect("run package tests with a command-line filter");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(output.status.success(), "--filter must take precedence:\n{text}");
    assert!(
        text.contains("filter 'combat_text_pipeline': 1 selected, 1 skipped"),
        "the command-line selection is reported:\n{text}"
    );
}

#[test]
fn command_line_filter_matches_package_test_source_path() {
    let fixture = package_fixture();
    let output = crate::command::rock_bin()
        .arg("test")
        .arg("--filter")
        .arg("tests/combat_text.rock")
        .arg(fixture.path())
        .output()
        .expect("run package tests with a source-path filter");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(output.status.success(), "the path-matched test must pass:\n{text}");
    assert!(
        text.contains("filter 'tests/combat_text.rock': 1 selected, 1 skipped"),
        "the source-path selection is reported:\n{text}"
    );
}

#[test]
fn filter_with_no_matches_exits_nonzero() {
    let fixture = package_fixture();
    let output = crate::command::rock_bin()
        .arg("test")
        .arg(fixture.path())
        .env("ROCK_TEST_FILTER", "no_such_test")
        .output()
        .expect("run package tests with a nonmatching filter");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(!output.status.success(), "a zero-match filter must fail:\n{text}");
    assert!(
        text.contains("filter 'no_such_test': 0 selected, 2 skipped"),
        "the summary shows that no tests were selected:\n{text}"
    );
    assert!(text.contains("matched no @tests"), "the error explains the empty match:\n{text}");
}

#[test]
fn c_backend_rejects_test_filter_with_a_clear_error() {
    let output = crate::command::rock_bin()
        .args(["test", "--backend", "c"])
        .env("ROCK_TEST_FILTER", "combat_text_pipeline")
        .output()
        .expect("run C backend with a test filter");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(!output.status.success(), "C backend must reject filtering:\n{text}");
    assert!(
        text.contains("does not support `--filter` or ROCK_TEST_FILTER"),
        "the rejection identifies the unsupported filter:\n{text}"
    );
}
