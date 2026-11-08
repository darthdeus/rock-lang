use std::path::Path;
use std::process::{Command, Output};

fn run(root: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rock"));
    command.current_dir(root).env("ROCK_DIR", root).args(args);
    for key in [
        "ROCK_ENGINE_BUILD_RECEIPT",
        "ROCK_PREBUILT_GAME",
        "ROCK_TOOLCHAIN",
        "ROCK_STONE_LIB",
    ] {
        command.env_remove(key);
    }
    command.output().expect("run public CLI")
}

fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn failure(output: Output, expected: &str) {
    assert!(!output.status.success());
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(text.contains(expected), "expected {expected:?}: {text}");
}

#[test]
fn native_commands_discover_and_stage_a_declared_library() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path();
    std::fs::write(project.join("stick.toml"),
        "[package]\nname = \"probe\"\n[native]\nmanifest = \"Cargo.toml\"\nlibrary = \"staged_probe\"\nshipped = false\n").unwrap();
    std::fs::write(project.join("Cargo.toml"),
        "[package]\nname = \"native-probe\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[lib]\nname = \"source_probe\"\npath = \"lib.rs\"\ncrate-type = [\"cdylib\"]\n[workspace]\n").unwrap();
    std::fs::write(
        project.join("lib.rs"),
        "#[unsafe(no_mangle)] pub extern \"C\" fn probe() -> i32 { 42 }\n",
    )
    .unwrap();
    let output = success(run(project, &["engine-libs", "."]));
    let libraries: serde_json::Value = serde_json::from_str(&output).unwrap();
    assert_eq!(libraries.as_array().unwrap().len(), 1);
    assert_eq!(libraries[0]["need"], "probe");
    assert_eq!(libraries[0]["crate"], "native-probe");
    assert_eq!(libraries[0]["src_stem"], "source_probe");
    assert_eq!(libraries[0]["dst_stem"], "staged_probe");
    assert_eq!(libraries[0]["shipped"], false);
    success(run(project, &["native-build", "."]));
    let staged = project.join("tmp/native").join(format!(
        "{}staged_probe.{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_EXTENSION
    ));
    assert!(staged.is_file(), "{}", staged.display());
}

#[test]
fn native_commands_validate_arguments_and_receipt_layout() {
    let root = tempfile::tempdir().unwrap();
    failure(
        run(root.path(), &["native-build"]),
        "usage: rock native-build",
    );
    failure(run(root.path(), &["engine-build-receipt"]), "--output");
    assert_eq!(success(run(root.path(), &["engine-libs"])).trim(), "[]");
    std::fs::create_dir(root.path().join("bin")).unwrap();
    std::fs::create_dir(root.path().join("lib")).unwrap();
    std::fs::write(
        root.path()
            .join("bin")
            .join(format!("rock{}", std::env::consts::EXE_SUFFIX)),
        b"fixture",
    )
    .unwrap();
    failure(
        run(
            root.path(),
            &["engine-build-receipt", "--output", "receipt.json"],
        ),
        "only meaningful in a source workspace",
    );
    assert!(!root.path().join("receipt.json").exists());
}

#[test]
fn sdk_check_validates_the_selected_sdk_and_project() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("stick.toml"),
        "[package]\nname = \"probe\"\n",
    )
    .unwrap();
    failure(run(root.path(), &["sdk-check"]), "usage: rock sdk-check");
    failure(
        run(root.path(), &["sdk-check", ".", "."]),
        "usage: rock sdk-check",
    );
    failure(
        run(root.path(), &["sdk-check", "."]),
        "SDK native library is missing",
    );
    std::fs::create_dir(root.path().join("lib")).unwrap();
    std::fs::write(
        root.path().join("lib").join(format!(
            "{}stone.{}",
            std::env::consts::DLL_PREFIX,
            std::env::consts::DLL_EXTENSION
        )),
        b"fixture",
    )
    .unwrap();
    assert!(
        success(run(root.path(), &["sdk-check", "."])).contains("SDK manifest compatibility: OK")
    );
    assert!(
        !root.path().join("tmp").exists(),
        "preflight must not build or stage libraries"
    );
}
