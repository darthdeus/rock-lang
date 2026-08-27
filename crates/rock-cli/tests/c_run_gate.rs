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
    std::fs::write(
        dir.path().join("foo.rock"),
        "fn echo(value: string) -> string { return value }\n\
         fn main() { let message = echo(\"hi\"); print(message) }\n",
    )
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
    assert!(source.contains("rock_string p0"), "{source}");
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
fn c_compile_and_run_load_project_dependencies() {
    assert!(
        Command::new("clang")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success()),
        "C backend gate requires clang"
    );
    let root = tempfile::tempdir().expect("tempdir");
    let library = root.path().join("numbers");
    let game = root.path().join("game");
    std::fs::create_dir_all(&library).expect("create library");
    std::fs::create_dir_all(&game).expect("create game");
    std::fs::write(
        library.join("stick.toml"),
        "[package]\nname = \"numbers\"\ntype = \"lib\"\n",
    )
    .expect("write library manifest");
    std::fs::write(
        library.join("numbers.rock"),
        "fn project_answer() -> string { return \"answer=42\" }\n",
    )
    .expect("write library source");
    std::fs::write(
        game.join("stick.toml"),
        "[package]\nname = \"game\"\ntype = \"bin\"\n\n[dependencies]\nnumbers = { path = \"../numbers\" }\n",
    )
    .expect("write game manifest");
    std::fs::write(
        game.join("main.rock"),
        "fn main() { print(project_answer()) }\n",
    )
    .expect("write game source");

    let compile = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "compile",
            game.to_str().expect("utf8 game path"),
            "--backend",
            "c",
            "--output",
            "native-game",
        ])
        .current_dir(root.path())
        .output()
        .expect("spawn rock compile");
    assert!(
        compile.status.success(),
        "C project compile failed:\nstdout:{}\nstderr:{}",
        String::from_utf8_lossy(&compile.stdout),
        String::from_utf8_lossy(&compile.stderr)
    );
    let compiled_run = Command::new(root.path().join("native-game"))
        .output()
        .expect("run compiled project");
    assert!(
        compiled_run.status.success(),
        "compiled project failed: {compiled_run:?}"
    );
    assert_eq!(compiled_run.stdout, b"answer=42\n");

    let project_run = run(&game);
    assert!(
        project_run.status.success(),
        "C project run failed:\nstdout:{}\nstderr:{}",
        String::from_utf8_lossy(&project_run.stdout),
        String::from_utf8_lossy(&project_run.stderr)
    );
    assert_eq!(project_run.stdout, b"answer=42\n");
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
    std::fs::write(
        &printing,
        "fn echo(value: string) -> string { let copy = value; return copy }\n\
         fn main() {\n    let message = echo(\"hi\")\n    print(message)\n}\n",
    )
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
