use std::path::{Path, PathBuf};
use std::process::Command;

fn root() -> PathBuf {
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

fn toml_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"")
}

#[test]
fn missing_stone_root_fails_at_the_asset_resolution_boundary() {
    let repo = root();
    std::fs::create_dir_all(repo.join("tmp")).unwrap();
    let scratch = tempfile::Builder::new()
        .prefix("stone-root-resolution-")
        .tempdir_in(repo.join("tmp"))
        .unwrap();
    let project = scratch.path().join("game");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("stick.toml"),
        format!(
            "[package]\nname = \"stone-root-resolution\"\ntype = \"bin\"\n\n[dependencies]\nstone = {{ path = \"{}\" }}\n\n[build.vars]\nGIT_SHA = \"test\"\n",
            toml_path(&repo.join("stone")),
        ),
    )
    .unwrap();
    std::fs::write(
        project.join("main.rock"),
        "fn main() { print(asset_path(\"@engine/compat/assets.rock\")) }\n",
    )
    .unwrap();

    let lua = project.join("main.lua");
    let compile = Command::new(rock_bin())
        .current_dir(&repo)
        .args(["compile", project.to_str().unwrap(), "--output", lua.to_str().unwrap()])
        .output()
        .expect("compile Stone-root fixture");
    assert!(
        compile.status.success(),
        "fixture compile failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&compile.stdout),
        String::from_utf8_lossy(&compile.stderr)
    );

    let emitted = std::fs::read_to_string(&lua).unwrap();
    assert!(
        emitted.contains("@engine/compat/assets.rock"),
        "fixture must reach the real asset_path consumer"
    );
    let without_stone_root = emitted
        .lines()
        .filter(|line| !line.starts_with("__ROCK_STONE_DIR = "))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(&lua, without_stone_root).unwrap();

    let mut run = rock_host::project::build_luajit_command(&lua, &[]);
    run.current_dir(&project)
        .env_remove("STONE_SOURCE_DIR")
        .env_remove("__ROCK_STONE_DIR")
        .env_remove("GAME_SOURCE_DIR")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let output = run.output().expect("run stripped Stone-root fixture");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "a missing Stone root must fail closed; current relative fallback ran: stdout={stdout:?} stderr={stderr:?}"
    );
    assert!(
        stderr.contains("Stone source directory") || stdout.contains("Stone source directory"),
        "missing Stone root error must name the boundary:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}
