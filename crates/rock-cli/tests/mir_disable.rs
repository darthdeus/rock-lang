use crate::support::{rock, text};

/// A safe-navigation compound store through a null base. With the branch and
/// comparison folds off, the dead store keeps a constant nil base.
const NIL_STORE_BASE: &str = r#"
struct CV2 { x: float, y: float }
fn __op_add(a: CV2, b: CV2) -> CV2 { return CV2 { x: a.x + b.x, y: a.y + b.y } }
struct Inner { vec: CV2 }
struct Outer { inner: Inner? }
fn main() {
    let n = Outer { inner: null }
    n.inner?.vec += CV2 { x: 99.0, y: 99.0 }
    if n.inner == null { print("ok") }
}
"#;

const PASSES: [&str; 13] = [
    "sroa",
    "copyprop",
    "foldcmp",
    "foldbr",
    "inline",
    "lambda",
    "forl",
    "bounds",
    "noboxflat",
    "mem2reg",
    "noboxfold",
    "latefloat",
    "callfwd",
];

#[test]
fn disabling_any_pass_keeps_a_nil_store_base_valid_lua() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.rock"), NIL_STORE_BASE).unwrap();
    for pass in PASSES.iter().copied().chain(["foldbr,foldcmp,copyprop"]) {
        let out = rock(dir.path())
            .args(["run", "main.rock"])
            .env("ROCK_MIR_DISABLE", pass)
            .output()
            .unwrap();
        let output = text(&out);
        assert!(
            out.status.success() && output.trim() == "ok",
            "ROCK_MIR_DISABLE={pass}: {output}"
        );
    }
}

#[test]
fn unknown_disabled_pass_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.rock"), NIL_STORE_BASE).unwrap();
    let out = rock(dir.path())
        .args(["run", "main.rock"])
        .env("ROCK_MIR_DISABLE", "no_such_pass")
        .output()
        .unwrap();
    assert!(!out.status.success() && text(&out).contains("unknown pass `no_such_pass`"));
}
