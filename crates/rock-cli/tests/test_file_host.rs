use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent().unwrap().parent().unwrap().to_path_buf()
}

fn toml_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"")
}

fn run_file_host(kind: &str, transitive: bool) {
    let dir = tempfile::tempdir().expect("Stone test host fixture");
    let project = dir.path().join("project");
    std::fs::create_dir_all(project.join("tests")).unwrap();
    let stone = toml_path(&root().join("stone"));
    let dependency = if transitive {
        let bridge = dir.path().join("bridge");
        std::fs::create_dir(&bridge).unwrap();
        std::fs::write(bridge.join("stick.toml"), format!(
            "[package]\nname = \"file-host-bridge\"\ntype = \"lib\"\n\
             [language]\nmodules = \"v1\"\nglobals = \"none\"\n\
             [dependencies]\nstone = {{ path = \"{stone}\" }}\n\
             [build.vars]\nGIT_SHA = \"test\"\n"
        )).unwrap();
        std::fs::write(bridge.join("exports.rock"),
            "pub use stone::{file_read, file_write, file_append, stone_window, stone_imgui_initialized};\n"
        ).unwrap();
        format!("bridge = {{ path = \"{}\" }}", toml_path(&bridge))
    } else {
        format!("stone = {{ path = \"{stone}\" }}")
    };
    std::fs::write(project.join("stick.toml"), format!(
        "[package]\nname = \"test-file-host\"\ntype = \"{kind}\"\n\
         [language]\nmodules = \"v1\"\nglobals = \"none\"\n\
         [dependencies]\n{dependency}\n\
         [build.vars]\nGIT_SHA = \"test\"\n"
    )).unwrap();
    std::fs::write(project.join("main.rock"), "fn main() {}\n").unwrap();
    let host = if transitive { "bridge" } else { "stone" };
    let repository_file = toml_path(&root().join("stone/stick.toml"));
    let output_file = dir.path().join("written.txt");
    let written = toml_path(&output_file);
    let missing = toml_path(&dir.path().join("missing.txt"));
    std::fs::write(project.join("tests/files.rock"), format!(r#"
use {host}::{{file_read, file_write, file_append, stone_window, stone_imgui_initialized}};

@test
fn repository_file_read() {{
    let data = file_read("{repository_file}").unwrap()
    assert(data.contains("name = \"stone\""), "read the real repository file")
    assert(file_read("{missing}") == null, "a missing file returns null")
}}

@test
fn native_file_roundtrip() {{
    assert(file_write("{written}", "native\nfile"), "write through Stone")
    assert(file_append("{written}", "\thost"), "append through Stone")
    assert_eq(file_read("{written}").unwrap(), "native\nfile\thost")
}}

@test
fn cpu_host_has_no_window() {{
    assert(stone_window == null, "unit tests do not create a window")
    assert(!stone_imgui_initialized(), "unit tests do not initialize ImGui")
}}
"#)).unwrap();
    let output = crate::command::rock_bin()
        .arg("test").arg(project.join("tests"))
        .current_dir(root())
        .env("ROCK_TEST_VERBOSE", "1")
        .output().expect("run Stone file-host tests");
    let text = format!("{}{}", String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr));
    assert!(text.contains("3/3 @tests passed") && output.status.success(),
        "Stone file IO must work without a game boot:\n{text}");
    assert_eq!(std::fs::read(output_file).unwrap(), b"native\nfile\thost",
        "the host must write real bytes to disk");
}

#[test]
fn stone_file_io_has_a_headless_library_test_host() {
    run_file_host("lib", false);
}

#[test]
fn stone_file_io_has_a_headless_binary_test_host() {
    run_file_host("bin", false);
}

#[test]
fn transitive_stone_dependency_selects_the_file_host() {
    run_file_host("lib", true);
}

#[cfg(unix)]
#[test]
fn native_stone_package_tests_stage_a_fresh_library_without_the_full_host() {
    let root = root();
    std::fs::create_dir_all(root.join("tmp")).unwrap();
    let dir = tempfile::tempdir_in(root.join("tmp")).expect("isolated native test root");
    let runtime = dir.path().join("runtime");
    std::fs::create_dir(&runtime).unwrap();
    // Share source and build inputs, but give staging its own empty tmp directory.
    for entry in std::fs::read_dir(&root).unwrap() {
        let entry = entry.unwrap();
        if matches!(entry.file_name().to_str(), Some("tmp" | "target" | ".git")) {
            continue;
        }
        std::os::unix::fs::symlink(entry.path(), runtime.join(entry.file_name())).unwrap();
    }
    let extension = if cfg!(target_os = "macos") { "dylib" } else { "so" };
    let staged = runtime.join("tmp").join(format!("libstone.{extension}"));
    assert!(!staged.exists(), "fixture must start without staged libstone");

    let provider = dir.path().join("provider");
    std::fs::create_dir_all(provider.join("tests")).unwrap();
    std::fs::write(provider.join("stick.toml"), format!(
        "[package]\nname = \"native-provider-fixture\"\ntype = \"lib\"\n\
         [language]\nmodules = \"v1\"\n\
         [dependencies]\ncore = {{ path = \"{}\" }}\n",
        toml_path(&root.join("stone-core"))
    )).unwrap();
    std::fs::write(provider.join("lib.rock"),
        std::fs::read_to_string(root.join("stone-native/lib.rock")).unwrap()
            .replace("use stone_core::*;", "use core::*;")
    ).unwrap();
    let check = r#"
@test
fn native_flow_loads_without_a_game_host() {
    stone_native_load()
    assert_eq(stone_flow_poll(0), -1)
}
"#;
    std::fs::write(provider.join("tests/native.rock"),
        check.replace("native_flow_loads_without_a_game_host", "native_flow_provider")
    ).unwrap();

    let bridge = dir.path().join("bridge");
    let project = dir.path().join("consumer");
    for (package, dependency) in [(&bridge, &provider), (&project, &bridge)] {
        std::fs::create_dir_all(package.join("tests")).unwrap();
        std::fs::write(package.join("stick.toml"), format!(
            "[package]\nname = \"native-{}-fixture\"\ntype = \"lib\"\n\
             [language]\nmodules = \"v1\"\n\
             [dependencies]\nnative = {{ path = \"{}\" }}\n",
            package.file_name().unwrap().to_str().unwrap(), toml_path(dependency)
        )).unwrap();
        std::fs::write(package.join("lib.rock"),
            "pub use native::{stone_native_load, stone_flow_poll};\n").unwrap();
        std::fs::write(package.join("tests/native.rock"),
            check.replace("native_flow_loads_without_a_game_host", &format!(
                "native_flow_{}", package.file_name().unwrap().to_str().unwrap()
            ))
        ).unwrap();
    }

    for package in [&provider, &bridge, &project] {
        let output = crate::command::rock_bin()
            .arg("test").arg(package.join("tests"))
            .current_dir(&root)
            .env("ROCK_DIR", &runtime)
            .env("CARGO_TARGET_DIR", runtime.join("target"))
            .env("ROCK_STONE_LIB", &staged)
            .env("ROCK_TEST_VERBOSE", "1")
            .env_remove("ROCK_ENGINE_BUILD_RECEIPT")
            .output().expect("run isolated native package tests");
        let text = format!("{}{}", String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr));
        assert!(output.status.success() && text.contains("1/1 @tests passed"),
            "native package tests must stage current libstone without setup_stone:\n{text}");
        assert!(staged.is_file(), "the package runner must stage libstone");
        // A subsequent run must replace an existing stale provider too.
        std::fs::write(&staged, b"stale native library").unwrap();
    }
}
