use std::process::Command;

fn rock_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_rock"))
}

const GATE: &str = r#"
inline(lua) fn plus_one(x: int) -> int = """return x + 1"""
inline(js) fn plus_one(x: int) -> int = """return x + 1"""

inline(lua) fn triple(x: int) -> int = """return x * 3"""
inline(js) fn triple(x: int) -> int = """return x + x + x"""

fn main() {
    print("{plus_one(41)},{triple(4)}")
}
"#;

const VALUE_GATE: &str = r#"
inline(lua) fn triple(x: int) -> int = """return x * 3"""
inline(js) fn triple(x: int) -> int = """return x + x + x"""

fn apply(f: fn(int) -> int, x: int) -> int { return f(x) }

fn main() {
    let g = triple
    print("{apply(g, 4)},{apply(triple, 5)}")
}
"#;

fn run(src: &str, name: &str) -> (String, bool) {
    let dir = tempfile::tempdir().expect("fixture dir");
    let path = dir.path().join(name);
    std::fs::write(&path, src).expect("write fixture");
    let out = Command::new(rock_bin())
        .args(["run", path.to_str().unwrap()])
        .output()
        .expect("spawn rock");
    (String::from_utf8_lossy(&out.stdout).into_owned(), out.status.success())
}

#[test]
fn same_name_backend_pair_runs_on_luajit() {
    let (out, ok) = run(GATE, "pair.rock");
    assert!(ok, "luajit run failed:\n{out}");
    assert_eq!(out, "42,12\n", "luajit output drifted");
}

#[test]
fn sibling_pair_as_fn_value_binds_on_luajit() {
    let (out, ok) = run(VALUE_GATE, "value.rock");
    assert!(ok, "luajit run failed:\n{out}");
    assert_eq!(out, "12,15\n", "luajit fn-value output drifted");
}
