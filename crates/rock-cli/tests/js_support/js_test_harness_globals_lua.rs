use std::path::Path;

const FIXTURE: &str = r#"
global SHARED_COUNTER: int = 0

fn sentinel_next_id() -> int {
    SHARED_COUNTER = SHARED_COUNTER + 1
    return SHARED_COUNTER
}

global sentinel_g1: int = sentinel_next_id()
global sentinel_g2: int = sentinel_next_id()

@test(output = "1")
fn first_global() { print(sentinel_g1) }

@test(output = "2")
fn second_global() { print(sentinel_g2) }
"#;

fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
}

#[test]
fn luajit_executor_keeps_shared_global_state() {
    let dir = tempfile::tempdir().expect("fixture dir");
    let file = dir.path().join("sentinel.rock");
    std::fs::write(&file, FIXTURE).expect("write fixture");
    let output = crate::command::rock_bin()
        .args(["test", file.to_str().unwrap()])
        .current_dir(workspace_root())
        .output()
        .expect("spawn rock");
    let text = test_utils::strip_ansi_sgr(&format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    ));
    assert!(
        output.status.success(),
        "luajit must evaluate the shared counter once:\n{text}"
    );
    assert!(text.contains("2/2"), "both Lua tests must run:\n{text}");
}
