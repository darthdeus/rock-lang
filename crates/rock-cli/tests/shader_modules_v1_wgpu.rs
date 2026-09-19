//! Modules-v1 shader emission through the WGPU-enabled `rock` binary.

use std::path::{Path, PathBuf};
use std::process::Command;

fn rock_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_rock"))
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/shader-v1")
        .join(name)
}

fn staged_fixture(name: &str) -> tempfile::TempDir {
    let source = fixture(name);
    let staged = tempfile::tempdir().expect("temporary fixture directory");
    for file in ["stick.toml", "helpers.rock", "main.rock"] {
        std::fs::copy(source.join(file), staged.path().join(file)).expect("stage shader fixture");
    }
    staged
}

fn compile_fixture(name: &str) -> String {
    let staged = staged_fixture(name);
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
        .expect("spawn WGPU-enabled rock compile");
    assert!(
        compiled.status.success(),
        "v1 WGPU shader package failed to compile\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&compiled.stdout),
        String::from_utf8_lossy(&compiled.stderr)
    );
    std::fs::read_to_string(&output).expect("compile output")
}

#[test]
fn v1_explicit_wgsl_uses_identifiers_and_builtin_mappings() {
    let output = compile_fixture("explicit_wgpu");
    for invalid in [
        "struct shader_probe::",
        "fn shader_probe::",
        "var<uniform> u_shader_probe::",
        "d = dot_",
        "custom = __op_add_",
    ] {
        assert!(!output.contains(invalid), "emitted WGSL still contains `{invalid}`");
    }
    assert!(output.contains("dot("), "the v1 intrinsic must map to WGSL dot");
    assert!(
        output.contains(" + vec2<f32>"),
        "the v1 vector operator must emit a WGSL infix expression"
    );
}

#[test]
fn v1_fullscreen_wgsl_uses_identifiers_and_builtin_mappings() {
    let output = compile_fixture("fullscreen");
    assert!(!output.contains("fn shader_probe_fullscreen::"));
    assert!(!output.contains("d = dot_"));
    assert!(!output.contains("custom = __op_add_"));
    assert!(output.contains("dot("), "the v1 intrinsic must map to WGSL dot");
    assert!(
        output.contains(" + vec2<f32>"),
        "the v1 vector operator must emit a WGSL infix expression"
    );
}
