use std::path::{Path, PathBuf};
use std::process::Command;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent().unwrap().parent().unwrap().to_owned()
}

#[test]
fn emitted_harness_has_a_threshold_gated_full_collect() {
    let source = include_str!("../src/skull_backend.rs");
    let harness = source.split("fn build_test_harness(").nth(1).unwrap()
        .split("/// Render single-file errors").next().unwrap();
    assert!(harness.contains("ROCK_TEST_GC_COLLECT_MB"));
    assert!(harness.contains("collectgarbage(\"count\") > __skull_gc_limit_kb"));
    assert!(harness.contains("collectgarbage(\"collect\")"));
    assert!(harness.find("collectgarbage(\"collect\")").unwrap()
        > harness.find("__SKULL__\\tTIME").unwrap());
}

fn run_with_limit(limit: Option<&str>, count_kb: usize, expected: usize) -> std::process::Output {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("stick.toml"),
        "[package]\nname = \"gc-harness\"\ntype = \"lib\"\n").unwrap();
    std::fs::write(dir.path().join("tests.rock"), format!(r#"
@test inline(lua) fn install_probe() = """
_G.__gc_collects = 0
local real_gc = collectgarbage
collectgarbage = function(op, ...)
  if op == "count" then return {count_kb} end
  if op == "collect" then _G.__gc_collects = _G.__gc_collects + 1 end
  return real_gc(op, ...)
end
"""
@test(output = "{expected}") inline(lua) fn observe_collect() = "print(_G.__gc_collects)"
"#)).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rock"));
    cmd.arg("test").arg(dir.path()).current_dir(root())
        .env("ROCK_TEST_SHARDS", "1").env("ROCK_TEST_VERBOSE", "1")
        .env_remove("ROCK_TEST_FILTER").env_remove("ROCK_TEST_GC_COLLECT_MB");
    if let Some(limit) = limit { cmd.env("ROCK_TEST_GC_COLLECT_MB", limit); }
    cmd.output().unwrap()
}

#[test]
fn emitted_harness_honors_the_collect_override_and_strict_threshold() {
    for (limit, count, expected) in [
        (Some("0"), usize::MAX, 0),
        (Some("1"), 1024, 0),
        (Some("1"), 1025, 1),
        (Some("1024"), 768 * 1024, 0),
        (None, 2048 * 1024, 1),
    ] {
        let output = run_with_limit(limit, count, expected);
        let text = format!("{}{}", String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr));
        assert!(output.status.success(), "limit={limit:?}, count={count}: {text}");
        assert!(text.contains("2/2 @tests passed (0 failed)"), "{text}");
    }
}

#[test]
fn emitted_harness_rejects_invalid_collect_limits() {
    for limit in ["-1", "bad", "nan", "inf"] {
        let output = run_with_limit(Some(limit), 0, 0);
        let text = format!("{}{}", String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr));
        assert!(!output.status.success(), "limit={limit}: {text}");
        assert!(text.contains("ROCK_TEST_GC_COLLECT_MB must be a finite non-negative number"), "{text}");
    }
}
