#![cfg(unix)]

use crate::support::{assert_mentions, rock, text};
use std::path::Path;
use std::process::Output;

fn fixture(root: &Path, code: i32) -> std::path::PathBuf {
    std::fs::write(
        root.join("probe.rock"),
        "@test(returns = 3)\nfn gpu_probe() -> int { return 3 }\n",
    )
    .unwrap();
    let runner = root.join("runner");
    let script = if code == 0 {
        "#!/bin/sh\ninput=$(cat)\nif [ \"$input\" != '[]' ]; then printf '__SKULL__\\tVALUE\\tgpu_probe\\t3\\n'; fi\n".to_owned()
    } else {
        format!(
            "#!/bin/sh\ncat >/dev/null\necho 'no GL context: display or GL driver unavailable' >&2\nexit {code}\n"
        )
    };
    test_utils::write_executable(&runner, &script);
    runner
}

fn invoke(root: &Path, runner: &Path, shader: bool, skip: bool) -> Output {
    let mut command = rock(root);
    command.env("ROCK_GLSL_RUNNER", runner);
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

#[test]
fn an_unavailable_context_fails_gpu_commands_unless_skipped() {
    for shader in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let runner = fixture(root.path(), 2);
        let required = invoke(root.path(), &runner, shader, false);
        assert!(!required.status.success(), "{}", text(&required));
        assert_mentions(&required, "330core");
        assert_mentions(&required, "display or GL driver unavailable");
        assert_mentions(&required, "--skip-gpu");
        let skipped = invoke(root.path(), &runner, shader, true);
        assert!(skipped.status.success(), "{}", text(&skipped));
        assert_mentions(&skipped, "SKIPPED");
    }
}

#[test]
fn skip_gpu_keeps_runner_errors_hard() {
    for shader in [false, true] {
        let root = tempfile::tempdir().unwrap();
        for code in [3, 99] {
            let runner = fixture(root.path(), code);
            let output = invoke(root.path(), &runner, shader, true);
            assert!(!output.status.success());
            assert_mentions(&output, &format!("exited Some({code})"));
        }
        let output = invoke(
            root.path(),
            &root.path().join("missing-runner"),
            shader,
            true,
        );
        assert!(!output.status.success());
        assert_mentions(&output, "missing stone-glsl-runner");
    }
}

#[test]
fn skip_gpu_is_limited_to_gpu_commands() {
    let root = tempfile::tempdir().unwrap();
    for args in [vec!["check", "--skip-gpu"], vec!["test", "--skip-gpu"]] {
        let output = crate::support::invoke(root.path(), &args);
        assert!(!output.status.success());
        assert_mentions(&output, "supported only by");
    }
}

#[test]
fn skip_gpu_still_executes_available_context() {
    let root = tempfile::tempdir().unwrap();
    let runner = fixture(root.path(), 0);
    let output = invoke(root.path(), &runner, false, true);
    assert!(output.status.success(), "{}", text(&output));
    assert_mentions(&output, "1/1 value tests passed");
    assert!(!text(&output).contains("SKIPPED"), "{}", text(&output));
}
