//! `ROCK_COMPILE_STATS=<file>` appends one compile budget record per compile:
//! a tree whose every level sums to its parent, so a phase cannot hide.
use crate::support::{PACKAGE_MAIN, read_jsonl, rock, success};
use std::path::Path;

fn project() -> tempfile::TempDir {
    crate::support::two_module_package("budget")
}

/// `rock compile . --output <output>` with one of the two report variables set.
fn compile_reporting(root: &Path, variable: &str, report: &Path, output: &Path) {
    let other = if variable == "ROCK_COMPILE_STATS" {
        "ROCK_BODY_COSTS"
    } else {
        "ROCK_COMPILE_STATS"
    };
    success(
        rock(root)
            .args(["compile", ".", "--output"])
            .arg(output)
            .env(variable, report)
            .env_remove(other)
            .output()
            .expect("start rock"),
    );
}

fn compile(root: &Path, stats: &Path, output: &Path) {
    compile_reporting(root, "ROCK_COMPILE_STATS", stats, output);
}

fn body_costs(root: &Path, costs: &Path, output: &Path) -> Vec<serde_json::Value> {
    compile_reporting(root, "ROCK_BODY_COSTS", costs, output);
    read_jsonl(costs)
}

fn record_ending<'a>(records: &'a [serde_json::Value], name: &str) -> &'a serde_json::Value {
    records
        .iter()
        .find(|r| r["name"].as_str().unwrap_or("").ends_with(name))
        .unwrap_or_else(|| panic!("no record for {name}: {records:?}"))
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
    compile(root.path(), &stats, &output);
    let records = read_jsonl(&stats);
    assert_eq!(records.len(), 1, "one record per compile: {records:?}");
    let record = &records[0];
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
    let lanes = child(setup, "concrete plans+lanes");
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

/// Body costs leave the output unchanged, and a compile without
/// `ROCK_COMPILE_STATS` writes no budget.
#[test]
fn body_costs_name_each_body_by_its_source_definition() {
    let root = project();
    let attributed = root.path().join("attributed.lua");
    let records = body_costs(root.path(), &root.path().join("costs.jsonl"), &attributed);
    let stats = root.path().join("stats.jsonl");
    assert!(!stats.exists());
    let plain = root.path().join("plain.lua");
    compile(root.path(), &stats, &plain);
    assert_eq!(
        std::fs::read(&plain).unwrap(),
        std::fs::read(&attributed).unwrap()
    );
    let main = record_ending(&records, "main::main");
    assert!(main["file"].as_str().unwrap().ends_with("main.rock"));
    assert_eq!(main["kind"], "function");
    assert_eq!(main["symbol"], "main");
    assert_eq!(main["line"], 6);
    assert_eq!(main["emitted"], true);
    assert!(main["lua_bytes"].as_u64().unwrap() > 0);
    assert!(main["lua_lines"].as_u64().unwrap() > 0);
    assert!(main["mir_lowered"].as_u64().unwrap() > 0);
    assert!(main["lower_ns"].as_u64().is_some());
    assert!(main["thread_cpu_ns"].as_u64().is_some());
    assert_eq!(main["record"], "body");
    assert!(
        records.iter().any(|r| r["record"] == "module"
            && r["pass"] == "infer functions"
            && r["file"].as_str().unwrap().ends_with("util.rock")
            && r["elapsed_ns"].as_u64().is_some()),
        "no inference time for util.rock"
    );
}

const WIDE: &str = r#"
mod main;

value Pose { x: float, y: float, z: float, yaw: float, pitch: float, roll: float }

fn spin(p: Pose, turns: int) -> Pose {
    let out = p
    for i in 0..turns {
        out = Pose { ..out, yaw: out.yaw + out.x + float(i), pitch: out.pitch + out.y, roll: out.roll + out.z }
    }
    return out
}

fn main() {
    let a = spin(Pose { x: 1.0, y: 2.0, z: 3.0, yaw: 0.0, pitch: 0.0, roll: 0.0 }, 3)
    let b = spin(a, 4)
    print("{b.yaw + b.pitch + b.roll}")
}
"#;

/// A body whose signature flattens a value records, per declared parameter
/// and then the result, its lane count and the value type it flattens, so the
/// report can rank types by the width they add to calls.
#[test]
fn body_costs_record_the_value_types_a_signature_flattens() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("stick.toml"),
        "[package]\nname = \"wide\"\n\n[language]\nmodules = \"v1\"\n",
    )
    .unwrap();
    std::fs::write(root.path().join("main.rock"), WIDE).unwrap();
    let records = body_costs(
        root.path(),
        &root.path().join("costs.jsonl"),
        &root.path().join("out.lua"),
    );
    let spin = record_ending(&records, "main::spin");
    let wire = spin["wire"].as_array().expect("a wire list");
    assert_eq!(wire.len(), 3, "two parameters and a result: {wire:?}");
    assert_eq!(wire[0]["type"], "wide::main::Pose");
    assert_eq!(wire[0]["lanes"], 6);
    assert_eq!(wire[0]["nobox"], false);
    assert_eq!(wire[0]["result"], false);
    assert!(
        wire[0]["declared"]
            .as_str()
            .unwrap()
            .ends_with("main.rock:4")
    );
    assert_eq!(wire[1]["type"], serde_json::Value::Null);
    assert_eq!(wire[1]["lanes"], 1);
    assert_eq!(wire[2]["type"], "wide::main::Pose");
    assert_eq!(wire[2]["result"], true);
    let main = record_ending(&records, "main::main");
    assert!(main.get("wire").is_none(), "nothing flattened: {main}");
}

/// A project's `cmd` build vars run beside discovery, parsing and checking:
/// the compile reads their values only after the program checks, so a slow
/// command must not hold up the front end.
#[test]
fn slow_build_var_commands_overlap_the_front_end() {
    let root = project();
    std::fs::write(
        root.path().join("stick.toml"),
        "[package]\nname = \"budget\"\n\n[language]\nmodules = \"v1\"\n\n\
         [build]\nchannels = [\"dev\"]\n\n\
         [build.vars]\nSLOW = { cmd = \"sleep 1; echo late-value\" }\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("main.rock"),
        PACKAGE_MAIN.replace("{bump(41)}", "{bump(41)} {build_var(\"SLOW\")}"),
    )
    .unwrap();
    let stats = root.path().join("stats.jsonl");
    let output = root.path().join("out.lua");
    compile(root.path(), &stats, &output);
    assert!(
        std::fs::read_to_string(&output)
            .unwrap()
            .contains("late-value"),
        "the command's value is stamped"
    );
    let budget = &read_jsonl(&stats)[0]["budget"];
    let before_discovery = ms(child(budget, "build vars"));
    assert!(
        before_discovery < 500.0,
        "build vars held the front end for {before_discovery} ms"
    );
    assert!(ms(budget) >= 1000.0, "the compile waited for the command");
}
