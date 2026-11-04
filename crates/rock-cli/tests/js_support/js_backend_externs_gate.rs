//! JS lane for per-backend same-name decls end to end. A same-name
//! `inline(lua)` + `inline(js)` pair collapses to one overload in bone, then the
//! JS backend emits and binds its variant under node.
//!
//! The Lua control lives in `js_backend_externs_lua.rs`.

use std::process::Command;

fn node_available() -> bool {
    Command::new("node")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// `plus_one` and `triple` each carry a lua + js body under one name+signature;
// the two bodies are written differently (Lua `x + 1` vs JS `x + 1`, `x * 3` two
// ways) but yield the same values, so a byte-identical result proves each backend
// bound its own variant, not that the sources coincidentally match.
const GATE: &str = r#"
inline(lua) fn plus_one(x: int) -> int = """return x + 1"""
inline(js) fn plus_one(x: int) -> int = """return x + 1"""

inline(lua) fn triple(x: int) -> int = """return x * 3"""
inline(js) fn triple(x: int) -> int = """return x + x + x"""

fn main() {
    print("{plus_one(41)},{triple(4)}")
}
"#;

const EXPECTED: &str = "42,12\n";

fn run(backend: Option<&str>, path: &std::path::Path) -> (String, bool) {
    let mut cmd = crate::command::rock_bin();
    cmd.args(["run", path.to_str().unwrap()]);
    if let Some(b) = backend {
        cmd.args(["--backend", b]);
    }
    let out = cmd.output().expect("spawn rock");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        out.status.success(),
    )
}

#[test]
fn same_name_backend_pair_runs_on_both() {
    let dir = std::env::temp_dir().join("rock_js_backend_externs");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("pair.rock");
    std::fs::write(&path, GATE).unwrap();

    if !node_available() {
        eprintln!("SKIP node half: node not found");
        return;
    }
    // node binds the inline(js) variant.
    let (js_out, js_ok) = run(Some("js"), &path);
    assert!(js_ok, "js run failed:\n{js_out}");
    assert_eq!(js_out, EXPECTED, "js output drifted");
}

// A backend-variant sibling pair taken as a fn-VALUE (stored in a local, passed
// to a higher-order fn) must bind ITS backend's variant, exactly as a direct
// call does. Before the fn-value collapse (bone) + per-target selection (skull
// `discover_globals`) landed, bone rejected the value with "ambiguous fn-value
// reference" and JS emission rejected the read with "global read not supported".
// The two bodies compute the same value via different source, so a byte-identical
// result proves each backend bound its own variant.
const VALUE_GATE: &str = r#"
inline(lua) fn triple(x: int) -> int = """return x * 3"""
inline(js) fn triple(x: int) -> int = """return x + x + x"""

fn apply(f: fn(int) -> int, x: int) -> int { return f(x) }

fn main() {
    let g = triple
    print("{apply(g, 4)},{apply(triple, 5)}")
}
"#;

const VALUE_EXPECTED: &str = "12,15\n";

#[test]
fn sibling_pair_as_fn_value_binds_each_backend() {
    let dir = std::env::temp_dir().join("rock_js_fnvalue_sibling");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("value.rock");
    std::fs::write(&path, VALUE_GATE).unwrap();

    if !node_available() {
        eprintln!("SKIP node half: node not found");
        return;
    }
    let (js_out, js_ok) = run(Some("js"), &path);
    assert!(js_ok, "js run failed:\n{js_out}");
    assert_eq!(js_out, VALUE_EXPECTED, "js fn-value output drifted");
}
