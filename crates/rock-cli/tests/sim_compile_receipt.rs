use std::process::Command;

fn list_sims(
    project: &str,
    receipt_dir: &std::path::Path,
    timing_log: &std::path::Path,
    prepare: bool,
) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rock"));
    command
        .args(["sim", "--list", project])
        .env("ROCK_SIM_COMPILE_RECEIPT_DIR", receipt_dir)
        .env("ROCK_SIM_COMPILE_TIMING", timing_log);
    if prepare {
        command.env("ROCK_SIM_COMPILE_RECEIPT_PREPARE", "1");
    } else {
        command.env_remove("ROCK_SIM_COMPILE_RECEIPT_PREPARE");
    }
    command.output().expect("spawn rock sim --list")
}

#[test]
fn prepared_sim_receipt_reuses_one_project_compile() {
    let dir = tempfile::tempdir().expect("tempdir");
    let receipt_dir = dir.path().join("receipts");
    let timing_log = dir.path().join("compile.jsonl");

    let prepared = list_sims("testbed", &receipt_dir, &timing_log, true);
    assert!(
        prepared.status.success(),
        "receipt preparation failed:\n{}",
        String::from_utf8_lossy(&prepared.stderr)
    );
    let reused_a = list_sims("testbed", &receipt_dir, &timing_log, false);
    assert!(
        reused_a.status.success(),
        "first receipt reuse failed:\n{}",
        String::from_utf8_lossy(&reused_a.stderr)
    );
    let reused_b = list_sims("testbed", &receipt_dir, &timing_log, false);
    assert!(
        reused_b.status.success(),
        "second receipt reuse failed:\n{}",
        String::from_utf8_lossy(&reused_b.stderr)
    );
    assert_eq!(prepared.stdout, reused_a.stdout);
    assert_eq!(prepared.stdout, reused_b.stdout);

    let records = std::fs::read_to_string(&timing_log).expect("read compile timing log");
    let emit_count = records
        .lines()
        .filter(|line| line.contains("\"phase\":\"skull_compile_emit\""))
        .count();
    assert_eq!(
        emit_count, 1,
        "one prepared project must compile exactly once; timing log:\n{records}"
    );
    let reuse_count = records
        .lines()
        .filter(|line| line.contains("\"phase\":\"receipt_materialize\""))
        .count();
    assert_eq!(reuse_count, 2, "both consumers must materialize the receipt");

    let project_receipt = std::fs::read_dir(&receipt_dir)
        .expect("read receipt root")
        .next()
        .expect("one project receipt")
        .expect("read project receipt")
        .path();
    let main_path = project_receipt.join("main.lua");
    let mut main = std::fs::read(&main_path).expect("read receipt main.lua");
    main.extend_from_slice(b"\n-- tampered\n");
    std::fs::write(&main_path, main).expect("tamper receipt main.lua");
    let rejected = list_sims("testbed", &receipt_dir, &timing_log, false);
    let stderr = String::from_utf8_lossy(&rejected.stderr);
    assert!(!rejected.status.success(), "a modified receipt artifact must be rejected");
    assert!(
        stderr.contains("main.lua hash does not match"),
        "wrong rejection for a modified receipt artifact:\n{stderr}"
    );
}

#[test]
fn prepared_sim_receipt_rejects_changed_project_sources() {
    let dir = tempfile::tempdir().expect("tempdir");
    let project = dir.path().join("project");
    let receipt_dir = dir.path().join("receipts");
    let timing_log = dir.path().join("compile.jsonl");
    std::fs::create_dir(&project).expect("create project");
    let workspace = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("workspace root")
        .to_path_buf();
    std::fs::write(
        project.join("stick.toml"),
        format!(
            "[package]\nname = \"receipt-fixture\"\nversion = \"0.1.0\"\ntype = \"bin\"\n\n[dependencies]\nstone = {{ path = {:?} }}\n\n[build.vars]\nGIT_SHA = \"receipt\"\nBUILD_UNIX = \"0\"\nBUILD_DATE = \"test\"\nCHANNEL = {{ builtin = \"channel\" }}\nPLATFORM = {{ builtin = \"platform\" }}\n",
            workspace.join("stone")
        ),
    )
    .expect("write fixture manifest");
    let source = "fn main() {}\n\n@sim\nco fn receipt_probe() -> Coroutine<Wait> {\n    stop_sim()\n}\n";
    std::fs::write(project.join("main.rock"), source).expect("write fixture source");
    let project_arg = project.to_string_lossy();

    let prepared = list_sims(&project_arg, &receipt_dir, &timing_log, true);
    assert!(
        prepared.status.success(),
        "receipt preparation failed:\n{}",
        String::from_utf8_lossy(&prepared.stderr)
    );
    std::fs::write(project.join("main.rock"), format!("{source}\n// changed\n"))
        .expect("change fixture source");
    let rejected = list_sims(&project_arg, &receipt_dir, &timing_log, false);
    let stderr = String::from_utf8_lossy(&rejected.stderr);
    assert!(!rejected.status.success(), "a source-stale receipt must be rejected");
    assert!(
        stderr.contains("receipt inputs changed"),
        "wrong rejection for a source-stale receipt:\n{stderr}"
    );
}
