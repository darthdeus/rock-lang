//! `rock test` records each `@test`'s wall time: a slowest-tests block in the
//! output and, with `ROCK_TEST_TIMINGS=<path>`, one JSON line per test, so
//! gate lanes can rank their slowest package tests.

use std::process::Command;

const PACKAGE: &[(&str, &str)] = &[
    ("stick.toml", "[package]\nname = \"timed-tests\"\ntype = \"lib\"\n"),
    ("tests/timed.rock", r#"
@test inline(lua) fn sleeps() = "os.execute('sleep 0.3')"

@test
fn quick() { assert(1 == 1, "holds") }

@test
fn red() { assert(1 == 2, "this test is supposed to fail") }

@test(returns = 1)
fn value_green() -> int { return 1 }

@test(returns = 1)
fn value_red() -> int { return 2 }
"#),
];

#[test]
fn rock_test_records_wall_time_per_test() {
    let dir = tempfile::tempdir().expect("tempdir");
    for (path, text) in PACKAGE {
        let path = dir.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).expect("package dir");
        std::fs::write(path, text).expect("package file");
    }
    let timings = dir.path().join("timings.jsonl");
    let out = Command::new(env!("CARGO_BIN_EXE_rock"))
        .arg("test")
        .arg(dir.path())
        .env("ROCK_TEST_TIMINGS", &timings)
        .env("NO_COLOR", "1")
        .output()
        .expect("spawn rock");
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("3/5 @tests passed (2 failed)"), "{text}");

    let rows = std::fs::read_to_string(&timings).unwrap_or_else(|e| panic!("{e}: {text}"));
    let rows: Vec<serde_json::Value> = rows.lines()
        .map(|line| serde_json::from_str(line).expect("JSON line"))
        .collect();
    let row = |name: &str| rows.iter()
        .find(|row| row["name"].as_str().is_some_and(|n| n.ends_with(name)))
        .unwrap_or_else(|| panic!("no row for {name}: {rows:?}"));
    assert_eq!(rows.len(), 5, "{rows:?}");
    for (name, status) in [("sleeps", "pass"), ("quick", "pass"), ("red", "fail"),
                           ("value_green", "pass"), ("value_red", "fail")] {
        assert_eq!(row(name)["status"], status, "{name}: {rows:?}");
        assert!(row(name)["ms"].as_f64().is_some_and(|ms| ms >= 0.0), "{name}: {rows:?}");
    }
    // A sleep costs no CPU, so this holds only for a wall clock.
    assert!(row("sleeps")["ms"].as_f64().unwrap() >= 250.0, "{rows:?}");

    let block = text.split("slowest @tests").nth(1).unwrap_or_else(|| panic!("{text}"));
    let first = block.lines().nth(1).unwrap_or_default();
    assert!(first.contains(" ms") && first.ends_with("sleeps"), "{text}");
}
