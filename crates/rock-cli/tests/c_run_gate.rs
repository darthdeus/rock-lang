use std::process::Command;

fn run(path: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_rock"))
        .args(["run", path.to_str().expect("utf8 path"), "--backend", "c"])
        .output()
        .expect("spawn rock")
}

#[test]
fn c_compile_keeps_artifacts_and_invalidates_stale_defaults() {
    assert!(
        Command::new("clang")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success()),
        "C backend gate requires clang"
    );
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("foo.rock"), "fn main() { print(\"hi\") }\n")
        .expect("write program");
    let compile = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args(["compile", "foo.rock", "--backend", "c"])
        .current_dir(dir.path())
        .output()
        .expect("spawn rock compile");
    assert!(
        compile.status.success(),
        "C compile failed:\nstdout:{}\nstderr:{}",
        String::from_utf8_lossy(&compile.stdout),
        String::from_utf8_lossy(&compile.stderr)
    );

    let c_path = dir.path().join("tmp/foo.c");
    let binary = dir.path().join("tmp/foo");
    let source = std::fs::read_to_string(&c_path).expect("generated C source");
    assert!(source.contains("rock_print_string"), "{source}");
    assert!(binary.is_file(), "native binary was not written");
    let run = Command::new(&binary).output().expect("run native binary");
    assert!(run.status.success(), "native binary failed: {run:?}");
    assert_eq!(run.stdout, b"hi\n");

    let explicit_compile = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "compile",
            "foo.rock",
            "--backend",
            "c",
            "--output",
            "build/custom",
        ])
        .current_dir(dir.path())
        .output()
        .expect("spawn explicit rock compile");
    assert!(explicit_compile.status.success(), "explicit C compile failed");
    assert!(dir.path().join("build/custom.c").is_file());
    assert!(dir.path().join("build/custom").is_file());

    std::fs::write(
        dir.path().join("foo.rock"),
        "extern(lua) fn unsupported_runtime()\nfn main() { unsupported_runtime() }\n",
    )
    .expect("write unsupported program");
    let failed_compile = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args(["compile", "foo.rock", "--backend", "c"])
        .current_dir(dir.path())
        .output()
        .expect("spawn failing rock compile");
    assert!(
        !failed_compile.status.success(),
        "unsupported compile passed"
    );
    assert!(!c_path.exists(), "failed compile left stale C source");
    assert!(!binary.exists(), "failed compile left stale binary");
}

#[test]
fn c_run_executes_main_and_propagates_native_failure() {
    assert!(
        Command::new("clang")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success()),
        "C backend gate requires clang"
    );
    let dir = tempfile::tempdir().expect("tempdir");
    let good = dir.path().join("good.rock");
    std::fs::write(
        &good,
        r#"
        fn sum_to(n: int) -> int {
            let total = 0
            let i = 0
            while i < n {
                total += i
                i += 1
            }
            return total
        }

        fn main() {
            if sum_to(5) != 10 { let _ = 1 ./ 0 }
        }
        "#,
    )
    .expect("write good program");
    let good_run = run(&good);
    assert!(
        good_run.status.success(),
        "valid C program failed:\nstdout:{}\nstderr:{}",
        String::from_utf8_lossy(&good_run.stdout),
        String::from_utf8_lossy(&good_run.stderr)
    );

    let bad = dir.path().join("bad.rock");
    std::fs::write(&bad, "fn main() { let _ = 1 ./ 0 }\n").expect("write bad program");
    let bad_run = run(&bad);
    assert!(
        !bad_run.status.success(),
        "native failure was reported as success"
    );

    let printing = dir.path().join("printing.rock");
    std::fs::write(&printing, "fn main() {\n    print(\"hi\")\n}\n")
        .expect("write printing program");
    let printing_run = run(&printing);
    assert!(
        printing_run.status.success(),
        "C print program failed:\nstdout:{}\nstderr:{}",
        String::from_utf8_lossy(&printing_run.stdout),
        String::from_utf8_lossy(&printing_run.stderr)
    );
    assert_eq!(printing_run.stdout, b"hi\n");

    let unsupported = dir.path().join("unsupported.rock");
    std::fs::write(
        &unsupported,
        "extern(lua) fn unsupported_runtime()\nfn main() { unsupported_runtime() }\n",
    )
    .expect("write unsupported program");
    let unsupported_run = run(&unsupported);
    let unsupported_output = format!(
        "{}{}",
        String::from_utf8_lossy(&unsupported_run.stdout),
        String::from_utf8_lossy(&unsupported_run.stderr)
    );
    assert!(
        !unsupported_run.status.success(),
        "unsupported runtime boundary was reported as success"
    );
    assert!(
        unsupported_output.contains("C backend"),
        "unsupported boundary lost its C diagnostic:\n{unsupported_output}"
    );
}
