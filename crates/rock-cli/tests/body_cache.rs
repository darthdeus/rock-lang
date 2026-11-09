//! `rock compile` serves unchanged bodies from the persistent cache and
//! reports what it did in the compile budget.
use std::path::Path;
use std::process::{Command, Output};

const MAIN: &str = r#"
mod main;

use super::util::bump;

fn main() {
    print("{bump(41)}")
}
"#;

const UTIL: &str = r#"
mod util;

pub fn bump(n: int) -> int {
    let step = 1
    return n + step
}
"#;

fn project() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("stick.toml"),
        "[package]\nname = \"cached\"\n\n[language]\nmodules = \"v1\"\n",
    )
    .unwrap();
    std::fs::write(root.path().join("main.rock"), MAIN).unwrap();
    std::fs::write(root.path().join("util.rock"), UTIL).unwrap();
    root
}

fn compile(root: &Path, cache: &str, stats: &Path, output: &Path) -> Output {
    let out = Command::new(env!("CARGO_BIN_EXE_rock"))
        .current_dir(root)
        .args(["compile", ".", "--output"])
        .arg(output)
        .env("ROCK_DIR", root)
        .env("ROCK_INVOCATION_LOG", "off")
        .env("ROCK_COMPILE_STATS", stats)
        .env("ROCK_BODY_CACHE", cache)
        .env_remove("ROCK_STDLIB_DIR")
        .output()
        .expect("start rock");
    assert!(
        out.status.success(),
        "exit {:?}\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn last_record(stats: &Path) -> serde_json::Value {
    let text = std::fs::read_to_string(stats).expect("stats file written");
    serde_json::from_str(text.lines().last().expect("a record")).expect("JSON record")
}

fn codegen(record: &serde_json::Value) -> &serde_json::Value {
    record["budget"]["children"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "codegen")
        .expect("codegen node")
}

#[test]
fn a_warm_cache_reproduces_the_cold_output_and_reports_its_hits() {
    let root = project();
    let cache = root.path().join("cache");
    let stats = root.path().join("stats.jsonl");
    let cold_out = root.path().join("cold.lua");
    let warm_out = root.path().join("warm.lua");
    compile(root.path(), cache.to_str().unwrap(), &stats, &cold_out);
    let cold = last_record(&stats);
    let cold_cache = &codegen(&cold)["body_cache"];
    assert_eq!(cold_cache["enabled"], true, "{cold_cache}");
    assert_eq!(cold_cache["hits"], 0, "{cold_cache}");
    assert!(cold_cache["misses"].as_u64().unwrap() > 0, "{cold_cache}");
    assert!(cache.join(".").exists(), "the cache directory was created");

    compile(root.path(), cache.to_str().unwrap(), &stats, &warm_out);
    let warm = last_record(&stats);
    let warm_cache = &codegen(&warm)["body_cache"];
    assert_eq!(warm_cache["misses"], 0, "{warm_cache}");
    assert_eq!(warm_cache["hits"], cold_cache["misses"], "{warm_cache}");
    assert_eq!(warm["totals"]["body_cache_misses"], 0);
    assert_eq!(
        std::fs::read(&cold_out).unwrap(),
        std::fs::read(&warm_out).unwrap(),
        "a warm cache must reproduce the cold output byte for byte"
    );

    // The switch's off position leaves no cache node in the budget.
    compile(root.path(), "0", &stats, &warm_out);
    let off = last_record(&stats);
    assert!(codegen(&off).get("body_cache").is_none());
    assert_eq!(
        std::fs::read(&cold_out).unwrap(),
        std::fs::read(&warm_out).unwrap()
    );
}
