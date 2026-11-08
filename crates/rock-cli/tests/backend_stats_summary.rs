//! `rock backend-stats` under `ROCK_LOG=debug`: every concrete build logs a
//! summary line, and its totals must be the folded owner totals the report's
//! concrete-facts lines print, on every repeated build in the process.
use std::process::Command;

const PROGRAM: &str = r#"
fn seed<T>() -> T? { return null }
struct Box<T> { value: T? = seed() }
fn pick<T>(a: T, b: T) -> T { return a }
fn main() {
    let a: Box<int> = Box {}
    let b: Box<string> = Box {}
    print("{a.value} {b.value} {pick(1, 2)} {pick("x", "y")}")
}
"#;

/// The two numbers around `sep` right after `key` in `line`.
fn pair(line: &str, key: &str, sep: &str) -> (u64, u64) {
    let rest = &line[line
        .find(key)
        .unwrap_or_else(|| panic!("no `{key}` in {line}"))
        + key.len()..];
    let mut parts = rest.splitn(2, sep);
    let num = |s: &str| -> u64 {
        s.trim_start()
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse()
            .unwrap_or_else(|_| panic!("no number after `{key}` in {line}"))
    };
    let a = num(parts.next().unwrap());
    let b = num(parts
        .next()
        .unwrap_or_else(|| panic!("no `{sep}` after `{key}`")));
    (a, b)
}

#[test]
fn concrete_build_summary_reports_folded_totals_on_every_build() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("main.rock"), PROGRAM).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_rock"))
        .current_dir(root.path())
        .args([
            "backend-stats",
            "main.rock",
            "--iters",
            "2",
            "--warmup",
            "1",
        ])
        .env("ROCK_DIR", root.path())
        .env("ROCK_INVOCATION_LOG", "off")
        .env("ROCK_LOG", "debug")
        .env_remove("ROCK_STDLIB_DIR")
        .output()
        .expect("start rock");
    let text = test_utils::strip_ansi_sgr(&format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    ));
    assert!(out.status.success(), "{text}");

    let facts = text
        .lines()
        .find(|l| l.contains("concrete plan facts"))
        .unwrap_or_else(|| panic!("no concrete plan facts line:\n{text}"));
    let tables = text
        .lines()
        .find(|l| l.contains("concrete tables (one timed run)"))
        .unwrap_or_else(|| panic!("no concrete tables line:\n{text}"));
    let slots = pair(facts, "source slots expr/binding ", "/");
    let type_nodes = pair(tables, "type nodes ", " examined, ");
    let foreign = pair(tables, "foreign domains ", "/");
    assert_eq!(foreign.1, 2, "two Box instantiation tables: {tables}");

    let summaries: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("concrete build:"))
        .collect();
    assert_eq!(summaries.len(), 3, "one summary per build:\n{text}");
    for summary in summaries {
        assert_eq!(pair(summary, "slots ", " expr + "), slots, "{summary}");
        assert_eq!(pair(summary, "type nodes ", "/"), type_nodes, "{summary}");
        assert_eq!(pair(summary, "foreign domains ", "/"), foreign, "{summary}");
        assert!(
            !summary.contains("(0 generic bodies)"),
            "`pick` and `seed` are generic: {summary}"
        );
    }
}
