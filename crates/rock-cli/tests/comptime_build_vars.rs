//! End-to-end gate for tier-1 comptime inputs (docs/comptime-plan.md):
//! `rock run` on a project whose stick.toml declares [build.vars] inlines the
//! resolved values through `build_var`/`embed_string`, `--channel` selects the
//! channel (undeclared channels fail the invocation), `--var` overrides, and
//! the JS backend produces byte-identical output from the same project.

use std::process::Command;

fn rock_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_rock"))
}

fn node_available() -> bool {
    Command::new("node").arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

const STICK: &str = r#"
[package]
name = "comptime-gate"

[build]
channels = ["dev", "steam"]
default_channel = "dev"

[build.vars]
GREETING = "hello-from-vars"
CHANNEL = { builtin = "channel" }
SHA = { cmd = "echo abc123" }
"#;

const MAIN: &str = r#"
fn main() {
    print(build_var("GREETING"))
    print(build_var("CHANNEL"))
    print(build_var("SHA"))
    print(embed_string("data.txt"))
}

@test(output = "hello-from-vars")
fn build_var_reaches_package_tests() {
    print(build_var("GREETING"))
}
"#;

fn project(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("rock_comptime_gate_{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("stick.toml"), STICK).unwrap();
    std::fs::write(dir.join("main.rock"), MAIN).unwrap();
    std::fs::write(dir.join("data.txt"), "embedded-data").unwrap();
    dir
}

fn run(dir: &std::path::Path, extra: &[&str]) -> (String, String, bool) {
    let mut cmd = Command::new(rock_bin());
    cmd.arg("run").arg(dir);
    cmd.args(extra);
    let out = cmd.output().expect("spawn rock");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

const EXPECTED_DEV: &str = "hello-from-vars\ndev\nabc123\nembedded-data\n";

#[test]
fn run_resolves_declared_vars_and_embeds() {
    let dir = project("dev");
    let (stdout, stderr, ok) = run(&dir, &[]);
    assert!(ok, "rock run failed:\nstdout:{stdout}\nstderr:{stderr}");
    assert_eq!(stdout, EXPECTED_DEV);
}

#[test]
fn channel_flag_selects_the_channel() {
    let dir = project("chan");
    let (stdout, stderr, ok) = run(&dir, &["--channel", "steam"]);
    assert!(ok, "rock run --channel steam failed:\nstdout:{stdout}\nstderr:{stderr}");
    assert_eq!(stdout, "hello-from-vars\nsteam\nabc123\nembedded-data\n");
}

#[test]
fn undeclared_channel_fails_the_invocation() {
    let dir = project("badchan");
    let (stdout, stderr, ok) = run(&dir, &["--channel", "stema"]);
    assert!(!ok, "undeclared channel must fail:\nstdout:{stdout}");
    assert!(stderr.contains("stema"), "error names the channel:\n{stderr}");
}

#[test]
fn var_flag_overrides_the_source() {
    let dir = project("override");
    let (stdout, stderr, ok) = run(&dir, &["--var", "GREETING=overridden"]);
    assert!(ok, "rock run --var failed:\nstdout:{stdout}\nstderr:{stderr}");
    assert!(stdout.starts_with("overridden\n"), "override wins:\n{stdout}");
}

fn run_cmd(args: &[&str]) -> (String, String, bool) {
    let out = Command::new(rock_bin()).args(args).output().expect("spawn rock");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

// `rock test <package>` shipped broken for every [build.vars] project (the
// package path never stamped comptime inputs and reported the deceptive
// `0/0 @tests passed (0 failed)` shape). Assert the DENOMINATOR, never the
// "0 failed" (CLAUDE.md).
#[test]
fn rock_test_runs_a_build_vars_package() {
    let dir = project("pkg_test");
    let (stdout, stderr, ok) = run_cmd(&["test", dir.to_str().unwrap()]);
    assert!(ok, "rock test failed:\nstdout:{stdout}\nstderr:{stderr}");
    assert!(
        stdout.contains("1/1 @tests passed"),
        "package test must RUN (denominator 1), not emit-fail:\n{stdout}\n{stderr}"
    );
    assert!(stdout.contains("0 emit-fail"), "no emit failures:\n{stdout}");
}

#[test]
fn rock_test_runs_a_build_vars_package_on_js() {
    if !node_available() {
        eprintln!("SKIP: node not found");
        return;
    }
    let dir = project("pkg_test_js");
    let (stdout, stderr, ok) =
        run_cmd(&["test", dir.to_str().unwrap(), "--backend", "js"]);
    assert!(ok, "rock test --backend js failed:\nstdout:{stdout}\nstderr:{stderr}");
    assert!(
        stdout.contains("1/1 @tests passed"),
        "js package test must RUN (denominator 1):\n{stdout}\n{stderr}"
    );
}

// A single FILE inside a [build.vars] project resolves from its enclosing
// project on BOTH backends. The Lua path shipped without the walk-up the JS
// path had, so the same source rejected on luajit and ran on JS.
#[test]
fn single_file_inside_a_project_resolves_on_both_backends() {
    let dir = project("single_file");
    let main = dir.join("main.rock");
    let (stdout, stderr, ok) = run_cmd(&["run", main.to_str().unwrap()]);
    assert!(ok, "single-file lua run failed:\nstdout:{stdout}\nstderr:{stderr}");
    assert_eq!(stdout, EXPECTED_DEV);

    if node_available() {
        let (js_out, js_err, js_ok) =
            run_cmd(&["run", main.to_str().unwrap(), "--backend", "js"]);
        assert!(js_ok, "single-file js run failed:\nstdout:{js_out}\nstderr:{js_err}");
        assert_eq!(js_out, stdout, "backends must agree on a single file");
    }
}

// The profiling tools build their own workspace; an unstamped one rejects
// every [build.vars] project and (worse) iter-codegen once exited 0 while
// measuring nothing.
#[test]
fn iter_codegen_measures_a_build_vars_project() {
    let dir = project("iter_codegen");
    let (stdout, stderr, ok) =
        run_cmd(&["iter-codegen", dir.to_str().unwrap(), "--iters", "1"]);
    assert!(ok, "iter-codegen failed:\nstdout:{stdout}\nstderr:{stderr}");
    let all = format!("{stdout}{stderr}");
    assert!(!all.contains("not declared"), "no undeclared-var rejection:\n{all}");
    assert!(all.contains("ms/iter"), "a measurement was actually taken:\n{all}");
}

#[test]
fn js_backend_matches_luajit() {
    if !node_available() {
        eprintln!("SKIP: node not found");
        return;
    }
    let dir = project("js");
    let (lua_out, lua_err, lua_ok) = run(&dir, &[]);
    assert!(lua_ok, "luajit run failed:\nstdout:{lua_out}\nstderr:{lua_err}");
    let (js_out, js_err, js_ok) = run(&dir, &["--backend", "js"]);
    assert!(js_ok, "js run failed:\nstdout:{js_out}\nstderr:{js_err}");
    assert_eq!(js_out, lua_out, "backends must agree byte-for-byte");
    assert_eq!(lua_out, EXPECTED_DEV);
}
