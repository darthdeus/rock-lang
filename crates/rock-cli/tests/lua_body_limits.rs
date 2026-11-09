//! Function bodies LuaJIT refuses to load fail at compile time, at a Rock
//! location, rather than at the point the emitted chunk is first run.

use std::path::Path;

fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
}

fn rock_test(src: &str) -> (bool, String) {
    let dir = tempfile::tempdir().expect("fixture dir");
    let file = dir.path().join("big.rock");
    std::fs::write(&file, src).expect("write fixture");
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_rock"))
        .arg("test")
        .arg(&file)
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

fn assign_chain(arms: usize) -> String {
    let mut src = "fn f(x: int) -> int {\n    let r = 0\n    if x == 0 { r = 0 }\n".to_string();
    for i in 1..arms {
        src.push_str(&format!("    else if x == {i} {{ r = {i} }}\n"));
    }
    src.push_str("    return r\n}\n@test\nfn t() { assert(f(3) == 3) }\n");
    src
}

fn assert_located_limit(text: &str, phrase: &str) {
    assert!(text.contains("too large for LuaJIT"), "{text}");
    assert!(text.contains(phrase), "{text}");
    assert!(text.contains("big.rock:"), "no location: {text}");
    assert!(!text.contains("execution failed"), "{text}");
}

#[test]
fn a_chain_under_the_limits_runs() {
    let (ok, text) = rock_test(&assign_chain(2000));
    assert!(ok, "{text}");
}

#[test]
fn a_chain_past_the_jump_range_is_a_located_error() {
    let (ok, text) = rock_test(&assign_chain(6000));
    assert!(!ok, "{text}");
    assert_located_limit(&text, "control structure too long");
}

#[test]
fn a_chain_past_the_label_stack_is_a_located_error() {
    let (ok, text) = rock_test(&assign_chain(20000));
    assert!(!ok, "{text}");
    assert_located_limit(&text, "LJ_MAX_VSTACK");
}
