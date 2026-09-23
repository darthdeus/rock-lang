//! Modules-v1 shader emission through the real package loader and `rock` CLI.
//!
//! The fixture keeps DefKeys in the source so this gate reaches the same
//! production name path as a game package, rather than manufacturing HIR names
//! inside a backend unit test.

use std::path::{Path, PathBuf};
use std::process::Command;

fn rock_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_rock"))
}

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/shader-v1/explicit")
}

fn staged_fixture() -> tempfile::TempDir {
    let source = fixture();
    let staged = tempfile::tempdir().expect("temporary fixture directory");
    for name in ["stick.toml", "helpers.rock", "main.rock"] {
        std::fs::copy(source.join(name), staged.path().join(name)).expect("stage shader fixture");
    }
    staged
}

#[test]
fn v1_glsl_shader_compiles_and_validates() {
    let staged = staged_fixture();
    let output_dir = tempfile::tempdir().expect("temporary output directory");
    let output = output_dir.path().join("shader.lua");
    let package = staged.path();
    let compiled = Command::new(rock_bin())
        .args([
            "compile",
            package.to_str().expect("fixture path is UTF-8"),
            "--output",
            output.to_str().expect("output path is UTF-8"),
        ])
        .output()
        .expect("spawn rock compile");
    assert!(
        compiled.status.success(),
        "v1 shader package failed to compile\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&compiled.stdout),
        String::from_utf8_lossy(&compiled.stderr)
    );
    let lua = std::fs::read_to_string(&output).expect("compile output");
    for invalid in [
        "uniform shader_probe::",
        "vec4 shader_probe::",
        "d = dot_",
        "custom = __op_add_",
    ] {
        assert!(!lua.contains(invalid), "emitted GLSL still contains `{invalid}`");
    }
    assert!(lua.contains("dot("), "the v1 intrinsic must map to GLSL dot");
    assert!(
        lua.contains(" + vec2"),
        "the v1 vector operator must emit a GLSL infix expression"
    );

    // The runtime joins each semantic uniform group to its live GL block by
    // name, and GL binds that block by its GLSL identifier.
    let register = &lua[lua.find("__rock_shader_register(").expect("shader registration")..];
    let (physical, semantic) = register.split_once("SHADER_INTERFACE_V1").expect("semantic wire");
    let group = semantic
        .lines()
        .find_map(|line| line.strip_prefix("group "))
        .expect("the probe has one uniform group");
    let name = group.split(' ').nth(1).expect("group name");
    assert!(
        physical.contains(&format!("\"{name}\"")),
        "semantic group `{name}` has no physical block of that name:\n{}",
        &physical[physical.len().saturating_sub(600)..]
    );

    let package_text = package.to_string_lossy().into_owned();
    let summary = rock_cli::glsl_test::shader_validate_run(&[package_text], false)
        .expect("shader validation run");
    assert_eq!(summary.projects, vec![(package.to_string_lossy().into_owned(), 1)]);
    let core = summary
        .per_profile
        .iter()
        .find(|profile| profile.profile == "330core")
        .expect("330core summary");
    assert!(core.gl_available, "330core validation must run, not skip");
    assert_eq!(core.passed, 1, "330core must validate the v1 shader");
    assert!(core.failed.is_empty(), "330core failures: {:?}", core.failed);
}
