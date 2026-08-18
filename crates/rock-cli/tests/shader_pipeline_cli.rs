//! Coverage for the `rock shader <file> <vert> <frag> [--profile]` two-root
//! vertex+fragment pipeline form (docs/shaders.md). Shells out
//! to the real `rock` binary to exercise the full clap parse + dispatch path.
//!
//! The single-root A1 fragment form and the fn-library form stay unchanged
//! (covered by the skull-codegen-glsl goldens); here we pin the CLI wiring.

use std::process::Command;

fn rock_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_rock"))
}

/// A vertex+fragment pipeline source, mag-ui shaped.
const PIPELINE_SRC: &str = r#"
value QuadVert { aPos: Vec2, aTexCoord: Vec2, aColor: Vec4 }
value UiVertU  { uProjection: Mat4, uView: Mat4, uTransform: Mat4 }
value UiVary   { position: Vec4, TexCoord: Vec2, Color: Vec4 }
value UiFragU  { uRatio: float }

@shader(vertex)
fn ui_vert(@attr a: QuadVert, @uniform u: UiVertU) -> UiVary {
    let clip = u.uProjection * u.uView * u.uTransform * Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 }
    return UiVary { position: clip, TexCoord: a.aTexCoord, Color: a.aColor }
}

@shader(fragment)
fn ui_frag(vary: UiVary, @uniform u: UiFragU) -> Vec4 {
    let fe = fwidth(vary.TexCoord.x)
    return Vec4 { x: vary.Color.x, y: fe, z: u.uRatio, w: vary.Color.w }
}
"#;

/// A unique temp dir for a test (nextest runs each test in its own process;
/// distinct names keep parallel tests from colliding).
fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("rock_shader_pipeline_{name}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn two_root_form_emits_pipeline() {
    let dir = scratch("two_root");
    let src = dir.join("ui.rock");
    let out = dir.join("ui.glsl");
    std::fs::write(&src, PIPELINE_SRC).unwrap();

    let status = Command::new(rock_bin())
        .args([
            "shader",
            src.to_str().unwrap(),
            "ui_vert",
            "ui_frag",
            "--output",
            out.to_str().unwrap(),
        ])
        .output()
        .expect("spawn rock");
    assert!(
        status.status.success(),
        "rock shader pipeline failed: stderr={}",
        String::from_utf8_lossy(&status.stderr)
    );
    let glsl = std::fs::read_to_string(&out).expect("output written");
    // #version must be the very first token (ES-profile requirement); the
    // GENERATED header follows it.
    assert!(glsl.starts_with("#version 330 core"), "version first:\n{glsl}");
    assert!(glsl.contains("// GENERATED"), "GENERATED header:\n{glsl}");
    assert!(glsl.contains("#ifdef VERTEX"), "vertex block:\n{glsl}");
    assert!(glsl.contains("layout (location = 0) in vec2 aPos;"), "attrs:\n{glsl}");
    assert!(glsl.contains("#ifdef PIXEL"), "pixel block:\n{glsl}");
    assert!(glsl.contains("FragColor = ui_frag();"), "frag main:\n{glsl}");
}

#[test]
fn profile_300es_selected() {
    let dir = scratch("es300");
    let src = dir.join("ui.rock");
    let out = dir.join("ui.glsl");
    std::fs::write(&src, PIPELINE_SRC).unwrap();

    let status = Command::new(rock_bin())
        .args([
            "shader",
            src.to_str().unwrap(),
            "ui_vert",
            "ui_frag",
            "--profile",
            "300es",
            "--output",
            out.to_str().unwrap(),
        ])
        .output()
        .expect("spawn rock");
    assert!(status.status.success(), "stderr={}", String::from_utf8_lossy(&status.stderr));
    let glsl = std::fs::read_to_string(&out).expect("output written");
    assert!(glsl.contains("#version 300 es"), "es profile:\n{glsl}");
    assert!(glsl.contains("precision highp float;"), "es precision:\n{glsl}");
}

#[test]
fn unknown_profile_rejects_listing_valid() {
    let dir = scratch("badprofile");
    let src = dir.join("ui.rock");
    std::fs::write(&src, PIPELINE_SRC).unwrap();

    let out = Command::new(rock_bin())
        .args(["shader", src.to_str().unwrap(), "ui_vert", "ui_frag", "--profile", "420core"])
        .output()
        .expect("spawn rock");
    assert!(!out.status.success(), "unknown profile must reject");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("330core") && stderr.contains("300es"),
        "reject should list valid profiles, got: {stderr}"
    );
}

#[test]
fn failed_emission_invalidates_the_previous_output() {
    let dir = scratch("failed_emission");
    let src = dir.join("bad.rock");
    let output = dir.join("bad.glsl");
    std::fs::write(
        &src,
        "@shader(fragment)\nfn bad(tc: Vec2) -> float { return tc.x }\n",
    )
    .unwrap();
    std::fs::write(&output, "stale output").unwrap();

    let result = Command::new(rock_bin())
        .args([
            "shader",
            src.to_str().unwrap(),
            "bad",
            "--output",
            output.to_str().unwrap(),
        ])
        .output()
        .expect("spawn rock");

    assert!(!result.status.success(), "failed emission must return nonzero");
    assert!(!output.exists(), "failed emission left stale output at {}", output.display());
}

#[test]
fn output_cannot_alias_the_source() {
    let dir = scratch("source_output_alias");
    let src = dir.join("ui.rock");
    std::fs::write(&src, PIPELINE_SRC).unwrap();

    let result = Command::new(rock_bin())
        .args([
            "shader",
            src.to_str().unwrap(),
            "ui_vert",
            "ui_frag",
            "--output",
            src.to_str().unwrap(),
        ])
        .output()
        .expect("spawn rock");

    assert!(!result.status.success(), "source/output alias must reject");
    assert_eq!(std::fs::read_to_string(&src).unwrap(), PIPELINE_SRC);

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        let alias = dir.join("alias.glsl");
        symlink(&src, &alias).unwrap();
        let result = Command::new(rock_bin())
            .args([
                "shader",
                src.to_str().unwrap(),
                "ui_vert",
                "ui_frag",
                "--output",
                alias.to_str().unwrap(),
            ])
            .output()
            .expect("spawn rock");

        assert!(!result.status.success(), "symlinked source/output alias must reject");
        assert_eq!(std::fs::read_to_string(&src).unwrap(), PIPELINE_SRC);
    }
}
