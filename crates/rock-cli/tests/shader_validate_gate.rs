//! `rock shader-validate` gate contract (docs/shaders.md § Testing):
//! a missing runner is a hard error (never a green skip), a real project's
//! registered pairs get per-profile compile verdicts on real GL contexts, and
//! runner FAIL lines land in the summary (the fail path is live, not
//! decorative). Mirrors `glsl_runner_gate.rs`; the one sanctioned skip is a
//! genuine no-display environment (runner exit 2).

/// A tempdir project registering one pipeline pair and one fullscreen entry.
/// Single `.rock` file: directory discovery needs no stick.toml for that.
fn mini_project(dir: &std::path::Path) -> String {
    let file = dir.join("shaders.rock");
    fs_err::write(
        &file,
        r#"
value QuadVert { aPos: Vec2, aTexCoord: Vec2 }
value UiVertU  { uProjection: Mat4, uView: Mat4, uTransform: Mat4 }
value UiVary   { position: Vec4, TexCoord: Vec2 }

@shader(vertex)
fn gate_vert(@attr a: QuadVert, @uniform u: UiVertU) -> UiVary {
    let clip = u.uProjection * u.uView * u.uTransform * Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 }
    return UiVary { position: clip, TexCoord: a.aTexCoord }
}

@shader(fragment)
fn gate_frag(vary: UiVary) -> Vec4 {
    return Vec4 { x: 1.0, y: 0.0, z: 1.0, w: 1.0 }
}

@shader(fragment)
fn gate_fs(tc: Vec2) -> Vec4 {
    return Vec4 { x: tc.x, y: tc.y, z: 0.0, w: 1.0 }
}

fn use_pair() -> string { return rock_shader(gate_vert, gate_frag) }
fn use_fs() -> string { return rock_shader(gate_fs) }
"#,
    )
    .expect("write mini project");
    dir.to_string_lossy().into_owned()
}

#[cfg(unix)]
fn fake_runner(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("fake-stone-glsl-runner");
    fs_err::write(&path, format!("#!/bin/sh\ncat > /dev/null\n{body}\n")).expect("write fake runner");
    let mut perms = fs_err::metadata(&path).expect("stat").permissions();
    perms.set_mode(0o755);
    fs_err::set_permissions(&path, perms).expect("chmod");
    path
}

#[test]
fn missing_runner_is_a_hard_error() {
    let _guard = crate::glsl_runner_env();
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = mini_project(tmp.path());
    unsafe { std::env::set_var("ROCK_GLSL_RUNNER", "/nonexistent/stone-glsl-runner") };
    let result = rock_cli::glsl_test::shader_validate_run(&[project], false);
    unsafe { std::env::remove_var("ROCK_GLSL_RUNNER") };
    let Err(err) = result else {
        panic!("a missing runner must be a hard error, not a silent skip");
    };
    let msg = format!("{err:#}");
    assert!(msg.contains("stone-glsl-runner"), "error must name the runner: {msg}");
}

/// The full path against the REAL runner: both pairs collected, and every
/// GL-available profile reports exactly 2 compiled shaders. A no-display
/// environment leaves `gl_available` false -- the sanctioned skip -- but pair
/// collection must still have happened.
#[test]
fn validates_a_minimal_project() {
    let _guard = crate::glsl_runner_env();
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = mini_project(tmp.path());
    let summary =
        rock_cli::glsl_test::shader_validate_run(&[project], false).expect("shader_validate_run");
    assert_eq!(summary.projects.len(), 1);
    assert_eq!(summary.projects[0].1, 2, "one pipeline pair + one fullscreen entry");
    for p in &summary.per_profile {
        if !p.gl_available {
            continue;
        }
        assert_eq!(
            p.failed,
            Vec::<String>::new(),
            "[{}] emitted engine-shape shaders must compile",
            p.profile
        );
        assert_eq!(p.passed, 2, "[{}] both pairs must get verdicts", p.profile);
    }
}

/// Runner FAIL lines must land in the summary as failures -- pins the fail
/// plumbing with a fake runner that "compiles" nothing. (The real driver
/// rejection path is exercised end-to-end by the runner's own compile_stage,
/// shared with the value-test path.)
#[cfg(unix)]
#[test]
fn runner_fail_lines_become_failures() {
    let _guard = crate::glsl_runner_env();
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = mini_project(tmp.path());
    let fake = fake_runner(
        tmp.path(),
        r#"printf '__SKULL__\tFAIL\tgate a: nope\n__SKULL__\tFAIL\tgate b: nope\n'"#,
    );
    unsafe { std::env::set_var("ROCK_GLSL_RUNNER", &fake) };
    let result = rock_cli::glsl_test::shader_validate_run(&[project], false);
    unsafe { std::env::remove_var("ROCK_GLSL_RUNNER") };
    let summary = result.expect("fake runner run");
    for p in &summary.per_profile {
        assert!(p.gl_available, "[{}] fake runner probe must pass", p.profile);
        assert_eq!(p.passed, 0, "[{}] nothing may count as compiled", p.profile);
        assert_eq!(p.failed.len(), 2, "[{}] both FAIL lines must surface", p.profile);
        assert!(
            p.failed.iter().all(|f| f.contains("nope")),
            "[{}] FAIL detail must carry the driver log: {:?}",
            p.profile,
            p.failed
        );
    }
}
