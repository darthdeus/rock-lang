use std::process::Command;

#[test]
fn loose_directory_remains_a_valid_parse_corpus() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rock"), "fn a() { }\n").unwrap();
    std::fs::write(dir.path().join("b.rock"), "fn b() { }\n").unwrap();

    let result = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args(["iter-parse", dir.path().to_str().unwrap(), "--iters", "1"])
        .output()
        .expect("spawn rock");

    assert!(result.status.success(), "loose corpus rejected: {}", String::from_utf8_lossy(&result.stderr));
}

#[test]
fn malformed_project_manifest_cannot_fall_back_to_a_flat_parse_corpus() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.rock"), "fn main() { }\n").unwrap();
    std::fs::write(dir.path().join("stick.toml"), "[package\n").unwrap();

    let result = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args(["iter-parse", dir.path().to_str().unwrap(), "--iters", "1"])
        .output()
        .expect("spawn rock");

    assert!(!result.status.success(), "malformed stick.toml was benchmarked as a flat corpus");
}

#[cfg(unix)]
#[test]
fn unreadable_source_cannot_disappear_from_the_parse_corpus() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.rock"), "fn main() { }\n").unwrap();
    symlink(dir.path().join("missing"), dir.path().join("missing.rock")).unwrap();

    let result = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args(["iter-parse", dir.path().to_str().unwrap(), "--iters", "1"])
        .output()
        .expect("spawn rock");

    assert!(!result.status.success(), "unreadable .rock source disappeared from the corpus");
}
