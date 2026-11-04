//! The stdlib window metric keeps its sorted window, insertion-order ring and
//! summary fields equal to a model rebuilt from the raw window after every
//! sample. No test covered eviction order before this one.
use std::path::{Path, PathBuf};
use std::process::Command;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .canonicalize()
        .unwrap()
}

#[test]
fn metric_add_preserves_the_window_across_eviction() {
    let dir = tempfile::tempdir().expect("fixture directory");
    let file = dir.path().join("metric_add_window.rock");
    std::fs::write(&file, include_str!("fixtures/metric_add_window.rock"))
        .expect("write property test");

    let out = Command::new(env!("CARGO_BIN_EXE_rock"))
        .arg("test")
        .arg(&file)
        .current_dir(root())
        .env("ROCK_TEST_VERBOSE", "1")
        .env_remove("ROCK_TEST_FILTER")
        .env_remove("ROCK_STDLIB_DIR")
        .output()
        .expect("run metric window property test");
    let text = test_utils::strip_ansi_sgr(&format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    ));
    assert!(out.status.success(), "property test failed:\n{text}");
    assert!(
        text.contains("2/2 @tests passed"),
        "expected both window tests to pass:\n{text}"
    );
}
