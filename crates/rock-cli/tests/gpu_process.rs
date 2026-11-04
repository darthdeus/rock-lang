#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

fn fixture(root: &Path, code: i32, partial: bool) -> std::path::PathBuf {
    std::fs::write(
        root.join("probe.rock"),
        "@test(returns = 3)\nfn gpu_probe() -> int { return 3 }\n",
    )
    .unwrap();
    let runner = root.join("runner");
    let available = if partial {
        "if [ \"$2\" = 300es ]; then\n  input=$(cat)\n  if [ \"$input\" != '[]' ]; then printf '__SKULL__\\tVALUE\\tgpu_probe\\t3\\n'; fi\n  exit 0\nfi\n"
    } else {
        ""
    };
    std::fs::write(&runner, format!("#!/bin/sh\n{available}cat >/dev/null\necho 'no GL context: display or GL driver unavailable' >&2\nexit {code}\n")).unwrap();
    std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o755)).unwrap();
    runner
}

fn invoke(root: &Path, runner: &Path, shader: bool, skip: bool) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rock"));
    command
        .current_dir(root)
        .env("ROCK_DIR", root)
        .env("ROCK_GLSL_RUNNER", runner)
        .env("ROCK_INVOCATION_LOG", "off")
        .env_remove("ROCK_TEST_FILTER")
        .env_remove("ROCK_BACKEND");
    if shader {
        command.args(["shader-validate", "."]);
    } else {
        command.args(["test", "probe.rock", "--backend", "glsl"]);
    }
    if skip {
        command.arg("--skip-gpu");
    }
    command.output().unwrap()
}

fn text(output: &Output) -> String {
    test_utils::strip_ansi_sgr(&format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    ))
}

#[test]
fn gpu_commands_require_every_profile_by_default() {
    for shader in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let runner = fixture(root.path(), 2, true);
        let output = invoke(root.path(), &runner, shader, false);
        let message = text(&output);
        assert!(
            !output.status.success(),
            "missing profile accepted: {message}"
        );
        assert!(message.contains("330core"), "{message}");
        assert!(
            message.contains("display or GL driver unavailable"),
            "{message}"
        );
        assert!(message.contains("--skip-gpu"), "{message}");
    }
}

#[test]
fn skip_gpu_explicitly_reports_unavailable_profiles() {
    for shader in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let runner = fixture(root.path(), 2, false);
        let output = invoke(root.path(), &runner, shader, true);
        assert!(output.status.success(), "{}", text(&output));
        assert!(text(&output).contains("SKIPPED"), "{}", text(&output));
    }
}

#[test]
fn skip_gpu_keeps_runner_errors_hard() {
    for shader in [false, true] {
        let root = tempfile::tempdir().unwrap();
        for code in [3, 99] {
            let runner = fixture(root.path(), code, false);
            let output = invoke(root.path(), &runner, shader, true);
            assert!(!output.status.success());
            assert!(
                text(&output).contains(&format!("exited Some({code})")),
                "{}",
                text(&output)
            );
        }
        let output = invoke(
            root.path(),
            &root.path().join("missing-runner"),
            shader,
            true,
        );
        assert!(!output.status.success());
        assert!(
            text(&output).contains("missing stone-glsl-runner"),
            "{}",
            text(&output)
        );
    }
}

#[test]
fn skip_gpu_is_limited_to_gpu_commands() {
    let root = tempfile::tempdir().unwrap();
    for args in [vec!["check", "--skip-gpu"], vec!["test", "--skip-gpu"]] {
        let output = Command::new(env!("CARGO_BIN_EXE_rock"))
            .current_dir(root.path())
            .args(args)
            .env_remove("ROCK_BACKEND")
            .env("ROCK_INVOCATION_LOG", "off")
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(
            text(&output).contains("supported only by"),
            "{}",
            text(&output)
        );
    }
}

#[test]
fn skip_gpu_still_executes_available_profiles() {
    let root = tempfile::tempdir().unwrap();
    let runner = fixture(root.path(), 2, true);
    let output = invoke(root.path(), &runner, false, true);
    assert!(output.status.success(), "{}", text(&output));
    assert!(
        text(&output).contains("1/1 value tests passed"),
        "{}",
        text(&output)
    );
    assert!(text(&output).contains("SKIPPED"), "{}", text(&output));
}
