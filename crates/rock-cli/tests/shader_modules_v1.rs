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

fn compile(package: &Path) -> String {
    let output_dir = tempfile::tempdir().expect("temporary output directory");
    let output = output_dir.path().join("shader.lua");
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
    std::fs::read_to_string(&output).expect("compile output")
}

fn validate_core(package: &Path) {
    let _guard = crate::glsl_runner_env();
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

/// A `@nomangle` type keeps its declared name as its emitted symbol on the Lua
/// side, so the shader text, the GL block wire and the semantic wire must spell
/// it the same way: the runtime joins the last two by string, and a Lua `cdef`
/// of the `@attr` struct names it by the declared name.
#[test]
fn v1_nomangle_shader_types_keep_their_declared_names() {
    let staged = staged_fixture();
    let main = staged.path().join("main.rock");
    let source = std::fs::read_to_string(&main).expect("fixture main");
    let source = source
        .replace("value ProbeVertex", "@nomangle value ProbeVertex")
        .replace("value ProbeUniform", "@nomangle value ProbeUniform");
    std::fs::write(&main, source).expect("rewrite fixture main");

    let lua = compile(staged.path());
    let register = &lua[lua.find("__rock_shader_register(").expect("shader registration")..];
    let (glsl, rest) = register.split_once("]]").expect("GLSL long bracket");
    let (physical, semantic) = rest.split_once("SHADER_INTERFACE_V1").expect("semantic wire");
    assert!(
        glsl.contains("layout(std140) uniform ProbeUniform {"),
        "GLSL block must use the declared name:\n{glsl}"
    );
    // The texture slot names come first, then the block names.
    let block_names = physical.split("_data = {").nth(2).and_then(|s| s.split('}').next()).expect("block names");
    assert_eq!(block_names, "\"ProbeUniform\"", "block wire names: {physical}");
    assert!(
        semantic.lines().any(|line| line.starts_with("group fragment ProbeUniform ")),
        "semantic group must use the declared name:\n{semantic}"
    );
    assert!(
        semantic.lines().any(|line| line.starts_with("vertex_input ProbeVertex ")),
        "semantic vertex input must use the declared name:\n{semantic}"
    );
    validate_core(staged.path());
}

#[test]
fn v1_glsl_shader_compiles_and_validates() {
    let staged = staged_fixture();
    let package = staged.path();
    let lua = compile(package);
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

    validate_core(package);
}
