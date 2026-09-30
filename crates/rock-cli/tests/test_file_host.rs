use std::path::{Path, PathBuf};
use std::process::Command;

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
    let output = Command::new(env!("CARGO_BIN_EXE_rock"))
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
