use std::path::PathBuf;
use std::process::Command;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn rock_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_rock"))
}

#[test]
fn sample_game_c_compiles_with_stone_build_identity() {
    let root = repo_root();
    let output_dir = tempfile::tempdir().unwrap();
    let output = output_dir.path().join("sample_game_c.lua");
    let result = Command::new(rock_bin())
        .current_dir(&root)
        .args([
            "compile",
            "games/sample_game_c",
            "--output",
            output.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        result.status.success(),
        "sample_game_c compile failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        output.is_file(),
        "sample_game_c compile did not create {}",
        output.display()
    );
}
