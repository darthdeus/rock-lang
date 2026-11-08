use std::path::Path;
use std::process::{Command, Output};

fn run(cwd: &Path, args: &[&str], overlay: Option<&Path>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rock"));
    command
        .current_dir(cwd)
        .args(args)
        .env("ROCK_DIR", cwd)
        .env("ROCK_INVOCATION_LOG", "off")
        .env("ROCK_NUM_THREADS", "1");
    for key in [
        "ROCK_ASSET_OVERLAYS",
        "ROCK_SKULL_LINE_DIRECTIVES",
        "ROCK_CONTEXT_ROOT",
        "ROCK_PREBUILT_GAME",
    ] {
        command.env_remove(key);
    }
    if let Some(overlay) = overlay {
        command.env("ROCK_ASSET_OVERLAYS", overlay);
    }
    command.output().expect("run public packaging command")
}

fn success(output: Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn failure(output: Output, expected: &str) {
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(expected), "expected {expected:?}: {stderr}");
}

fn project(root: &Path, extra_release: &str) {
    std::fs::create_dir_all(root.join("assets")).unwrap();
    std::fs::write(root.join("main.rock"), "fn main() {}\n").unwrap();
    std::fs::write(root.join("assets/owned.txt"), "owned asset").unwrap();
    std::fs::write(
        root.join("stick.toml"),
        format!(
            "[package]\nname = \"sample\"\n[release]\nassets = [\"assets/owned.txt\"]\n{extra_release}\n"
        ),
    )
    .unwrap();
}

#[test]
fn package_requires_an_explicit_output_directory() {
    let root = tempfile::tempdir().unwrap();
    project(root.path(), "");
    failure(
        run(root.path(), &["package"], None),
        "package requires --output <directory>",
    );
}

#[test]
fn package_uses_the_current_project_and_selected_relative_output() {
    let root = tempfile::tempdir().unwrap();
    project(root.path(), "");
    success(run(root.path(), &["package", "--output", "dist"], None));
    let output = root.path().join("dist");
    assert!(!std::fs::read(output.join("main.bc")).unwrap().is_empty());
    assert!(output.join("main.lua").is_file());
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(output.join("package-manifest.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["package"]["name"], "sample");
    assert_eq!(
        std::fs::read_to_string(output.join("assets/sample/assets/owned.txt")).unwrap(),
        "owned asset"
    );
}

#[test]
fn asset_check_and_package_validate_the_same_declared_overlay_assets() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("project");
    let overlay = root.path().join("overlay");
    project(&source, "overlay_assets = [\"assets/selected.txt\"]");
    std::fs::create_dir_all(overlay.join("assets")).unwrap();
    failure(
        run(root.path(), &["asset-check", "project"], Some(&overlay)),
        "declared overlay asset does not exist",
    );
    failure(
        run(
            root.path(),
            &["package", "project", "--output", "dist"],
            Some(&overlay),
        ),
        "declared overlay asset does not exist",
    );
    assert!(!root.path().join("dist").exists());
    std::fs::write(overlay.join("assets/selected.txt"), "selected overlay").unwrap();
    success(run(&source, &["asset-check"], Some(&overlay)));
    success(run(
        root.path(),
        &["package", "project", "--output", "dist"],
        Some(&overlay),
    ));
    let assets = root.path().join("dist/assets/sample/assets");
    assert_eq!(
        std::fs::read_to_string(assets.join("selected.txt")).unwrap(),
        "selected overlay"
    );
}
