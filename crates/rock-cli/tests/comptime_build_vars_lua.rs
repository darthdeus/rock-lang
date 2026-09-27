use std::process::Command;

fn rock_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_rock"))
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
    let dir = std::env::temp_dir().join(format!("rock_comptime_gate_lua_{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("stick.toml"), STICK).unwrap();
    std::fs::write(dir.join("main.rock"), MAIN).unwrap();
    std::fs::write(dir.join("data.txt"), "embedded-data").unwrap();
    dir
}

fn run(dir: &std::path::Path, extra: &[&str]) -> (String, String, bool) {
    let out = Command::new(rock_bin())
        .arg("run")
        .arg(dir)
        .args(extra)
        .output()
        .expect("spawn rock");
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

#[test]
fn rock_test_runs_a_build_vars_package() {
    let dir = project("pkg_test");
    let out = Command::new(rock_bin())
        .args(["test", dir.to_str().unwrap()])
        .output()
        .expect("spawn rock");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "rock test failed:\nstdout:{stdout}\nstderr:{stderr}");
    assert!(
        stdout.contains("1/1 @tests passed"),
        "package test must RUN (denominator 1), not emit-fail:\n{stdout}\n{stderr}"
    );
    assert!(stdout.contains("0 emit-fail"), "no emit failures:\n{stdout}");
}

#[test]
fn single_file_inside_a_project_resolves_on_luajit() {
    let dir = project("single_file");
    let main = dir.join("main.rock");
    let (stdout, stderr, ok) = run(&main, &[]);
    assert!(ok, "single-file lua run failed:\nstdout:{stdout}\nstderr:{stderr}");
    assert_eq!(stdout, EXPECTED_DEV);
}

#[test]
fn iter_codegen_measures_a_build_vars_project() {
    let dir = project("iter_codegen");
    let out = Command::new(rock_bin())
        .args(["iter-codegen", dir.to_str().unwrap(), "--iters", "1"])
        .output()
        .expect("spawn rock");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "iter-codegen failed:\nstdout:{stdout}\nstderr:{stderr}");
    let all = format!("{stdout}{stderr}");
    assert!(!all.contains("not declared"), "no undeclared-var rejection:\n{all}");
    assert!(all.contains("ms/iter"), "a measurement was actually taken:\n{all}");
}
