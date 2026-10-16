use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent().unwrap().parent().unwrap().canonicalize().unwrap()
}

// A package test that reads ROCK_DIR (as mag-world's content tests do) must see
// the same value under a bare `rock test` as under a gate that exports it.
fn run_rock_dir_test(rock_dir: Option<&Path>, expected: &Path) -> String {
    let dir = tempfile::tempdir().expect("ROCK_DIR fixture");
    let project = dir.path().join("project");
    std::fs::create_dir_all(project.join("tests")).unwrap();
    std::fs::write(project.join("stick.toml"),
        "[package]\nname = \"test-rock-dir\"\ntype = \"lib\"\n\
         [language]\nmodules = \"v1\"\nglobals = \"none\"\n\
         [build.vars]\nGIT_SHA = \"test\"\n").unwrap();
    std::fs::write(project.join("lib.rock"), "pub fn placeholder() {}\n").unwrap();
    std::fs::write(project.join("tests/rock_dir.rock"), format!(r#"
@test
fn rock_dir_is_the_project_root() {{
    assert_eq(env_var("ROCK_DIR").unwrap(), "{}")
}}
"#, expected.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\""))).unwrap();
    let mut command = crate::command::rock_bin();
    command.arg("test").arg(project.join("tests"))
        .current_dir(root())
        .env("ROCK_TEST_VERBOSE", "1");
    match rock_dir {
        Some(value) => command.env("ROCK_DIR", value),
        None => command.env_remove("ROCK_DIR"),
    };
    let output = command.output().expect("run ROCK_DIR package test");
    let text = format!("{}{}", String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr));
    assert!(text.contains("1/1 @tests passed") && output.status.success(),
        "ROCK_DIR package test failed:\n{text}");
    text
}

#[test]
fn rock_test_defaults_rock_dir_to_the_working_directory() {
    run_rock_dir_test(None, &root());
}

#[test]
fn rock_test_keeps_an_exported_rock_dir() {
    let exported = tempfile::tempdir().expect("exported ROCK_DIR");
    // The runner's own root lookup honors ROCK_DIR, so the exported directory
    // has to be a usable toolchain root: the repository itself, via a symlink.
    let link = exported.path().join("rock");
    #[cfg(unix)]
    std::os::unix::fs::symlink(root(), &link).unwrap();
    #[cfg(unix)]
    run_rock_dir_test(Some(&link), &link);
}
