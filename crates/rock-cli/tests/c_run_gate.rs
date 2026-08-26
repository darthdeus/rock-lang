use std::process::Command;

fn run(path: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_rock"))
        .args(["run", path.to_str().expect("utf8 path"), "--backend", "c"])
        .output()
        .expect("spawn rock")
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

    let unsupported = dir.path().join("unsupported.rock");
    std::fs::write(&unsupported, "fn main() { print(1) }\n").expect("write unsupported program");
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
