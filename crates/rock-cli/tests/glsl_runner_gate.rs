//! The GLSL gate must be loud when `stone-glsl-runner` is missing or broken.
//! The runner is a workspace artifact -- always buildable -- so its absence is
//! a broken gate (hard error), never a skip. Only a genuine no-display/driver
//! environment (runner exit 2) may skip-with-log.

fn workspace_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}

fn corpus() -> Vec<String> {
    vec![workspace_root()
        .join("tests/intrinsics/value_reference.rock")
        .to_string_lossy()
        .into_owned()]
}

/// A tiny single-test corpus for the paths that reach compilation.
fn tiny_corpus(dir: &std::path::Path) -> Vec<String> {
    let file = dir.join("gate_smoke.rock");
    fs_err::write(&file, "@test(returns = 3)\nfn gate_probe_smoke() -> int {\n    return 3\n}\n")
        .expect("write tiny corpus");
    vec![file.to_string_lossy().into_owned()]
}

#[cfg(unix)]
fn fake_runner(dir: &std::path::Path, exit_code: i32) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("fake-stone-glsl-runner");
    fs_err::write(&path, format!("#!/bin/sh\ncat > /dev/null\nexit {exit_code}\n"))
        .expect("write fake runner");
    let mut perms = fs_err::metadata(&path).expect("stat").permissions();
    perms.set_mode(0o755);
    fs_err::set_permissions(&path, perms).expect("chmod");
    path
}

#[test]
fn missing_runner_is_a_hard_error() {
    let _guard = crate::glsl_runner_env();
    unsafe { std::env::set_var("ROCK_GLSL_RUNNER", "/nonexistent/stone-glsl-runner") };
    let result = rock_cli::glsl_test::glsl_test_run(&corpus(), false);
    unsafe { std::env::remove_var("ROCK_GLSL_RUNNER") };
    let Err(err) = result else {
        panic!("a missing runner must be a hard error, not a silent skip");
    };
    let msg = format!("{err:#}");
    assert!(msg.contains("stone-glsl-runner"), "error must name the runner: {msg}");
}

/// Exit 3 (malformed invocation) or any non-0/2 exit on the probe is a harness
/// bug, not a missing display -- must be loud.
#[cfg(unix)]
#[test]
fn probe_anomaly_is_a_hard_error() {
    let _guard = crate::glsl_runner_env();
    let dir = tempfile::tempdir().expect("tempdir");
    let runner = fake_runner(dir.path(), 3);
    unsafe { std::env::set_var("ROCK_GLSL_RUNNER", &runner) };
    let result = rock_cli::glsl_test::glsl_test_run(&corpus(), false);
    unsafe { std::env::remove_var("ROCK_GLSL_RUNNER") };
    let Err(err) = result else {
        panic!("a probe exiting 3 must be a hard error, not a skip");
    };
    let msg = format!("{err:#}");
    assert!(msg.contains("3"), "error must carry the exit code: {msg}");
}

/// Exit 2 = no GL context (headless CI): the one sanctioned skip. Stays
/// Ok-with-gl_available=false so the ratchet's skip branch keeps working.
#[cfg(unix)]
#[test]
fn no_display_still_skips_with_log() {
    let _guard = crate::glsl_runner_env();
    let dir = tempfile::tempdir().expect("tempdir");
    let runner = fake_runner(dir.path(), 2);
    unsafe { std::env::set_var("ROCK_GLSL_RUNNER", &runner) };
    let result = rock_cli::glsl_test::glsl_test_run(&tiny_corpus(dir.path()), false);
    unsafe { std::env::remove_var("ROCK_GLSL_RUNNER") };
    let summary = result.expect("exit 2 is an environment fact, not an error");
    assert!(
        summary.per_profile.iter().all(|p| !p.gl_available),
        "no profile should report a GL context"
    );
    assert_eq!(summary.per_profile.len(), 2);
    assert_eq!(summary.unit_failures, 0, "the tiny corpus must build");
    assert_eq!(summary.outside_value_class, 0);
}
