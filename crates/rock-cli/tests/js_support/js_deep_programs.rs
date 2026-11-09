//! Programs deep enough to exhaust V8's parser when emitted naively: they run
//! when the emitter can flatten them and otherwise fail with a located limit
//! error instead of reading as a backend bug.

use std::path::Path;

fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
}

fn rock_test_js(src: &str) -> (bool, String) {
    let dir = tempfile::tempdir().expect("fixture dir");
    let file = dir.path().join("deep.rock");
    std::fs::write(&file, src).expect("write fixture");
    let out = crate::command::rock_bin()
        .arg("test")
        .arg(&file)
        .args(["--backend", "js"])
        .env("ROCK_TEST_VERBOSE", "1")
        .current_dir(workspace_root())
        .output()
        .expect("spawn rock");
    let text = test_utils::strip_ansi_sgr(&format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    ));
    (out.status.success(), text)
}

fn else_if_chain(arms: usize) -> String {
    let mut src = "fn f(x: int) -> int {\n    let r = 0\n    if x == 0 { r = 0 }\n".to_string();
    for i in 1..arms {
        src.push_str(&format!("    else if x == {i} {{ r = {i} }}\n"));
    }
    src.push_str("    return r\n}\n@test\nfn t() { assert(f(3) == 3) }\n");
    src
}

#[test]
fn a_long_operator_chain_runs() {
    if !crate::command::require_node() {
        return;
    }
    let sum = vec!["1"; 2000].join(" + ");
    let src =
        format!("fn f() -> int {{ return {sum} }}\n@test\nfn t() {{ assert(f() == 2000) }}\n");
    let (ok, text) = rock_test_js(&src);
    assert!(ok, "{text}");
}

#[test]
fn a_long_else_if_chain_runs() {
    if !crate::command::require_node() {
        return;
    }
    let (ok, text) = rock_test_js(&else_if_chain(2000));
    assert!(ok, "{text}");
}

#[test]
fn nesting_past_the_parser_limit_is_a_located_error() {
    let n = 2000;
    let mut src = "fn f(x: int) -> int {\n".to_string();
    src.push_str(&"    if x == 0 {\n".repeat(n));
    src.push_str("    return 1\n");
    src.push_str(&"    }\n".repeat(n));
    src.push_str("    return 0\n}\n@test\nfn t() { assert(f(0) == 1) }\n");
    let (ok, text) = rock_test_js(&src);
    assert!(!ok, "{text}");
    assert!(text.contains("nests control flow too deeply"), "{text}");
    assert!(text.contains("deep.rock:"), "no location: {text}");
    assert!(!text.contains("js backend bug"), "{text}");
}
