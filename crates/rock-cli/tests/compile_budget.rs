//! `ROCK_COMPILE_STATS=<file>` appends one compile budget record per compile:
//! a tree whose every level sums to its parent, so a phase cannot hide.
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

pub fn twice(n: int) -> int {
    return bump(bump(n))
}
"#;

fn project() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("stick.toml"),
        "[package]\nname = \"budget\"\n\n[language]\nmodules = \"v1\"\n",
    )
    .unwrap();
    std::fs::write(root.path().join("main.rock"), MAIN).unwrap();
    std::fs::write(root.path().join("util.rock"), UTIL).unwrap();
    root
}

fn compile(root: &Path, stats: &Path, output: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rock"))
        .current_dir(root)
        .args(["compile", ".", "--output"])
        .arg(output)
        .env("ROCK_DIR", root)
        .env("ROCK_INVOCATION_LOG", "off")
        .env("ROCK_COMPILE_STATS", stats)
        .env_remove("ROCK_STDLIB_DIR")
        .output()
        .expect("start rock")
}

fn ms(node: &serde_json::Value) -> f64 {
    node["wall_ms"]
        .as_f64()
        .unwrap_or_else(|| panic!("node without wall_ms: {node}"))
}

/// Every node with children sums to them, recursively; returns the names seen.
fn check_sums(node: &serde_json::Value, names: &mut Vec<String>) {
    names.push(node["name"].as_str().expect("node name").to_string());
    let Some(children) = node.get("children").and_then(|c| c.as_array()) else {
        return;
    };
    let sum: f64 = children.iter().map(ms).sum();
    assert!(
        (sum - ms(node)).abs() < 1e-6,
        "{} is {} ms but its children sum to {} ms:\n{}",
        node["name"],
        ms(node),
        sum,
        serde_json::to_string_pretty(node).unwrap()
    );
    for child in children {
        assert!(ms(child) >= 0.0, "negative phase {child}");
        check_sums(child, names);
    }
}

fn child<'a>(node: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    node["children"]
        .as_array()
        .unwrap_or_else(|| panic!("{} has no children", node["name"]))
        .iter()
        .find(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("{} has no child {name}:\n{node}", node["name"]))
}

#[test]
fn a_compile_appends_a_budget_whose_levels_sum_to_the_wall() {
    let root = project();
    let stats = root.path().join("stats.jsonl");
    let output = root.path().join("out.lua");
    let out = compile(root.path(), &stats, &output);
    assert!(
        out.status.success(),
        "exit {:?}\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let text = std::fs::read_to_string(&stats).expect("stats file written");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 1, "one record per compile:\n{text}");
    let record: serde_json::Value = serde_json::from_str(lines[0]).expect("JSON record");
    assert_eq!(record["schema"], 2);
    assert_eq!(record["status"], "ok");
    assert_eq!(record["command"], "compile");
    assert!(record["threads"].as_u64().unwrap() >= 1);
    // The written file adds the host preamble and main wrapper around the emit.
    let emitted = record["totals"]["emitted_lua_bytes"].as_u64().unwrap();
    let written = std::fs::metadata(&output).unwrap().len();
    assert!(
        emitted > 0 && emitted <= written,
        "emitted {emitted} vs written {written}"
    );
    assert_eq!(record["totals"]["source_files"], 2);

    let budget = &record["budget"];
    assert_eq!(budget["name"], "compile");
    let mut names = Vec::new();
    check_sums(budget, &mut names);
    let wall = record["wall_ms"].as_f64().unwrap();
    assert!(
        (ms(budget) - wall).abs() < 1e-6,
        "budget {} vs wall {wall}",
        ms(budget)
    );
    for phase in [
        "discovery",
        "parse+stdlib",
        "semantic",
        "prepare",
        "codegen",
        "write",
    ] {
        child(budget, phase);
    }
    let semantic = child(budget, "semantic");
    child(semantic, "infer functions");

    let codegen = child(budget, "codegen");
    let setup = child(codegen, "setup");
    child(setup, "graph");
    child(setup, "concrete plans");
    let lanes = child(setup, "artifact+shader+ABI lanes");
    // Lane branches overlap, so they are reported beside the lane, not summed.
    assert!(
        lanes["branches"]
            .as_array()
            .map_or(false, |b| !b.is_empty()),
        "{lanes}"
    );

    let fan_out = child(codegen, "fan-out");
    assert!(fan_out["threads"].as_u64().unwrap() >= 1);
    let cpu = codegen["unit_cpu_ms"]
        .as_f64()
        .expect("codegen unit_cpu_ms");
    let passes = codegen["unit_cpu"].as_array().expect("per-pass unit time");
    let cpu_sum: f64 = passes.iter().map(|p| p["cpu_ms"].as_f64().unwrap()).sum();
    assert!(
        (cpu - cpu_sum).abs() < 1e-6,
        "cpu {cpu} vs breakdown {cpu_sum}"
    );
    assert!(passes.iter().any(|p| p["name"] == "lower hir->mir"));

    let assembly = child(codegen, "assembly");
    child(assembly, "initializers");
    child(assembly, "fingerprint");
    assert!(
        names.iter().any(|n| n == "other"),
        "each level carries its remainder"
    );
}

#[test]
fn without_the_variable_nothing_is_written() {
    let root = project();
    let output = root.path().join("out.lua");
    let out = Command::new(env!("CARGO_BIN_EXE_rock"))
        .current_dir(root.path())
        .args(["compile", ".", "--output"])
        .arg(&output)
        .env("ROCK_DIR", root.path())
        .env("ROCK_INVOCATION_LOG", "off")
        .env_remove("ROCK_COMPILE_STATS")
        .env_remove("ROCK_STDLIB_DIR")
        .output()
        .expect("start rock");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!root.path().join("stats.jsonl").exists());
}
