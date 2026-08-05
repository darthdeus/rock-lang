//! JS execution harness (docs/js-backend-plan.md S3): emit an intrinsic's Rock
//! fallback body to JS, run it under `node`, and read back the number. This is
//! the second EXECUTING backend the intrinsics arc needed (`node`); S4 uses it
//! for the full mapping-vs-fallback differential.
//!
//! Gated on `node` being present -- absent, the test skips WITH A LOG (never a
//! silent pass). node is at `/run/current-system/sw/bin/node` on the dev box.


use skull_codegen_js::{fill_template, JS_INTRINSICS};
use crate::common::{emit_scalar, stdlib_ws};

fn node_available() -> bool {
    crate::common::node()
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Run a JS program that prints exactly one number; parse it as f64.
fn node_eval_number(program: &str) -> f64 {
    let out = crate::common::node()
        .arg("-e")
        .arg(program)
        .output()
        .expect("spawn node");
    assert!(
        out.status.success(),
        "node failed:\n--- program ---\n{program}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    s.trim()
        .parse::<f64>()
        .unwrap_or_else(|_| panic!("node did not print a number: {s:?}\nprogram:\n{program}"))
}

/// A JS numeric literal for a f64 arg (`-2` / `0.5`), valid in `node -e`.
fn arg_lit(a: f64) -> String {
    a.to_string()
}

/// Emit `name`'s fallback body and evaluate `name_fb(args)` under node.
fn eval_fallback(
    ws: &bone_semantic::inference::Workspace,
    name: &str,
    arity: usize,
    args: &[f64],
) -> f64 {
    let js = emit_scalar(ws, name, arity);
    let call = args.iter().map(|a| arg_lit(*a)).collect::<Vec<_>>().join(", ");
    node_eval_number(&format!("{js}\nconsole.log({name}_fb({call}));"))
}

/// Evaluate `name`'s JS TABLE MAPPING on `args` under node (`fill_template` +
/// eval). The native side of the differential -- no emitter involved.
fn eval_mapping(name: &str, arity: usize, args: &[f64]) -> f64 {
    let tmpl = JS_INTRINSICS
        .iter()
        .find(|(n, a, _)| *n == name && *a == arity)
        .map(|(_, _, t)| *t)
        .unwrap_or_else(|| panic!("no JS mapping for `{name}`/{arity}"));
    let lits: Vec<String> = args.iter().map(|a| arg_lit(*a)).collect();
    let refs: Vec<&str> = lits.iter().map(|s| s.as_str()).collect();
    node_eval_number(&format!("console.log({});", fill_template(tmpl, &refs)))
}


/// (name, arity, args, reference) from `tests/intrinsics/fallback_reference.rock`
/// -- the values the luajit fallback body produces. min/max are generic (no
/// emittable monomorphic body), so they are mapping-only and excluded here.
const CASES: &[(&str, usize, &[f64], f64)] = &[
    ("clamp", 3, &[-2.0, 0.0, 5.0], 0.0),
    ("clamp", 3, &[9.0, 0.0, 5.0], 5.0),
    ("clamp", 3, &[3.0, 0.0, 5.0], 3.0),
    ("lerp", 3, &[0.0, 20.0, 0.5], 10.0),
    ("smoothstep", 3, &[0.0, 1.0, -1.0], 0.0),
    ("smoothstep", 3, &[0.0, 1.0, 2.0], 1.0),
    ("smoothstep", 3, &[0.0, 1.0, 0.5], 0.5),
    ("round", 1, &[2.5], 3.0),
    ("round", 1, &[-2.5], -2.0),
    ("sign", 1, &[7.0], 1.0),
    ("sign", 1, &[0.0], 0.0),
    ("sign", 1, &[-4.0], -1.0),
    ("fract", 1, &[1.25], 0.25),
    ("fract", 1, &[-0.25], 0.75),
];

/// Mapping-only cases: min/max are generic (`fn max<T>`), so no monomorphic
/// MirFunc and no emittable fallback -- their differential is the JS mapping vs
/// the luajit reference (the fallback side runs on luajit as that reference).
const MAPPING_ONLY_CASES: &[(&str, usize, &[f64], f64)] = &[
    ("min", 2, &[3.0, 5.0], 3.0),
    ("max", 2, &[3.0, 5.0], 5.0),
];

// The emitted JS fallback bodies EXECUTE under node and reproduce the luajit
// reference values -- proving the S2 emitter produces runnable, correct JS.
#[test]
fn fallback_bodies_execute_under_node() {
    if !node_available() {
        eprintln!("SKIP fallback_bodies_execute_under_node: `node` not on PATH");
        return;
    }
    let ws = stdlib_ws();
    for &(name, arity, args, reference) in CASES {
        let got = eval_fallback(&ws, name, arity, args);
        assert!(
            (got - reference).abs() < 1e-9,
            "{name}({args:?}) fallback = {got}, reference = {reference}"
        );
    }
}

// The native-vs-fallback numeric differential the intrinsics arc was built for
// and slice 5 walled as "not achievable in-arc" (one body per fn on luajit).
// With node as the second executing backend, each intrinsic runs BOTH ways --
// its JS table mapping and its Rock fallback lowered to JS -- and they agree
// with each other AND the luajit reference. min/max are mapping-only (generic,
// no emittable fallback); their fallback side is the luajit reference.
#[test]
fn mapping_vs_fallback_differential() {
    if !node_available() {
        eprintln!("SKIP mapping_vs_fallback_differential: `node` not on PATH");
        return;
    }
    let ws = stdlib_ws();
    for &(name, arity, args, reference) in CASES {
        let mapping = eval_mapping(name, arity, args);
        let fallback = eval_fallback(&ws, name, arity, args);
        assert!(
            (mapping - reference).abs() < 1e-9,
            "{name}({args:?}) mapping = {mapping}, reference = {reference}"
        );
        assert!(
            (mapping - fallback).abs() < 1e-9,
            "{name}({args:?}) mapping = {mapping} disagrees with fallback = {fallback}"
        );
    }
    for &(name, arity, args, reference) in MAPPING_ONLY_CASES {
        let mapping = eval_mapping(name, arity, args);
        assert!(
            (mapping - reference).abs() < 1e-9,
            "{name}({args:?}) mapping = {mapping}, reference = {reference}"
        );
    }
}
