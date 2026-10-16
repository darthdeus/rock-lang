
#[test]
fn help_exposes_the_game_bank_inspector() {
    let output = crate::command::rock_bin()
        .arg("--help")
        .output()
        .expect("failed to spawn rock");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("non-utf8 help output");
    assert!(
        stdout.contains("sound-inspect"),
        "rock --help did not expose sound-inspect:\n{stdout}",
    );
}

#[test]
fn sound_inspect_requires_a_declared_sound_project() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("workspace root");
    let output = crate::command::rock_bin()
        .current_dir(root)
        .args(["sound-inspect", "games/chess"])
        .output()
        .expect("failed to spawn rock");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("has no [sound].project declaration"),
        "unexpected sound-inspect rejection:\n{stderr}",
    );
}
