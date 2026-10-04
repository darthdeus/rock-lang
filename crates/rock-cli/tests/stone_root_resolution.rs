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

#[test]
fn package_tests_receive_the_injected_stone_root() {
    let repo = root();
    std::fs::create_dir_all(repo.join("tmp")).unwrap();
    let scratch = tempfile::Builder::new()
        .prefix("stone-root-test-package-")
        .tempdir_in(repo.join("tmp"))
        .unwrap();
    let project = scratch.path().join("game");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("stick.toml"),
        format!(
            "[package]\nname = \"stone-root-test-package\"\ntype = \"bin\"\n\n[dependencies]\nstone = {{ path = \"{}\" }}\n\n[build.vars]\nGIT_SHA = \"test\"\n",
            toml_path(&repo.join("stone")),
        ),
    )
    .unwrap();
    std::fs::write(project.join("main.rock"), "fn main() {}\n").unwrap();

    let result = Command::new(rock_bin())
        .current_dir(&repo)
        .args(["test", project.to_str().unwrap(), "--test-threads", "1"])
        .output()
        .expect("run package Stone-root fixture tests");
    assert!(
        result.status.success(),
        "package tests must receive an injected Stone root\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn transitive_stone_dependency_injects_root_for_library_tests() {
    let repo = root();
    let library = repo.join("libs/idle-reference");
    let result = Command::new(rock_bin())
        .current_dir(&repo)
        .args(["test", library.to_str().unwrap(), "--test-threads", "1"])
        .output()
        .expect("run library tests with transitive Stone dependency");
    let stdout = String::from_utf8_lossy(&result.stdout);
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        result.status.success(),
        "library tests with a transitive Stone dependency must receive its source root\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("78/78 @tests passed"),
        "the library's tests must execute, not merely emit successfully:\n{stdout}"
    );
}

#[test]
fn projects_without_stone_do_not_receive_an_invented_root() {
    let repo = root();
    std::fs::create_dir_all(repo.join("tmp")).unwrap();
    let scratch = tempfile::Builder::new()
        .prefix("stone-root-no-dependency-")
        .tempdir_in(repo.join("tmp"))
        .unwrap();
    let project = scratch.path().join("game");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("stick.toml"),
        "[package]\nname = \"stone-root-no-dependency\"\ntype = \"bin\"\n\n[build.vars]\nGIT_SHA = \"test\"\n",
    )
    .unwrap();
    std::fs::write(project.join("main.rock"), "fn main() {}\n").unwrap();
    std::fs::write(project.join("stone_ffi.rock"), "// legacy game marker\n").unwrap();

    let lua = project.join("main.lua");
    let compile = Command::new(rock_bin())
        .current_dir(&repo)
        .args(["compile", project.to_str().unwrap(), "--output", lua.to_str().unwrap()])
        .output()
        .expect("compile project without Stone");
    assert!(
        compile.status.success(),
        "a compiler fixture without Stone must still compile\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&compile.stdout),
        String::from_utf8_lossy(&compile.stderr)
    );
    let emitted = std::fs::read_to_string(lua).unwrap();
    assert!(
        !emitted.contains("__ROCK_STONE_DIR = "),
        "the preamble must not invent a Stone root for a project without a Stone dependency"
    );
}

fn check_test_asset_root(transitive: bool) {
    let repo = root();
    let scratch = tempfile::tempdir().unwrap();
    let project = scratch.path().join("project");
    std::fs::create_dir_all(project.join("tests")).unwrap();
    let stone = toml_path(&repo.join("stone"));
    let (dependency, host) = if transitive {
        let bridge = scratch.path().join("bridge");
        std::fs::create_dir(&bridge).unwrap();
        std::fs::write(bridge.join("stick.toml"), format!(
            "[package]\nname = \"stone-root-bridge\"\ntype = \"lib\"\n\
             [language]\nmodules = \"v1\"\n\
             [dependencies]\nstone = {{ path = \"{stone}\" }}\n"
        )).unwrap();
        std::fs::write(bridge.join("exports.rock"),
            "pub use stone::project_asset_path;\n"
        ).unwrap();
        (format!("bridge = {{ path = \"{}\" }}", toml_path(&bridge)), "bridge")
    } else {
        (format!("stone = {{ path = \"{stone}\" }}"), "stone")
    };
    std::fs::write(project.join("stick.toml"), format!(
        "[package]\nname = \"stone-root-test\"\ntype = \"lib\"\n\
         [language]\nmodules = \"v1\"\n\
         [dependencies]\n{dependency}\n\
         [build.vars]\nGIT_SHA = \"test\"\n"
    )).unwrap();
    let expected = toml_path(&repo.join("stone/engine/compat/assets.rock"));
    std::fs::write(project.join("tests/assets.rock"), format!(
        "use {host}::project_asset_path;\n\
         @test fn engine_asset_root() {{\n\
             assert_eq(project_asset_path(\"@engine/compat/assets.rock\"), \"{expected}\")\n\
         }}\n"
    )).unwrap();
    for backend in ["skull", "c"] {
        let result = Command::new(rock_bin())
            .current_dir(&repo)
            .arg("test").arg(project.join("tests"))
            .args(["--backend", backend])
            .env_remove("STONE_SOURCE_DIR")
            .env_remove("__ROCK_STONE_DIR")
            .env_remove("GAME_SOURCE_DIR")
            .env("ROCK_TEST_VERBOSE", "1")
            .output().expect("run Stone asset-root test");
        let text = format!("{}{}", String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr));
        assert!(result.status.success() && text.contains("1/1 @tests passed"),
            "{backend} must resolve the dependency's Stone asset root without ambient configuration:\n{text}");
    }
}

#[test]
fn direct_stone_asset_root_is_supplied_to_both_test_backends() {
    check_test_asset_root(false);
}

#[test]
fn transitive_stone_asset_root_is_supplied_to_both_test_backends() {
    check_test_asset_root(true);
}
