use serde_json::Value;
use std::process::Command;

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("tests")).unwrap();
    std::fs::write(dir.path().join("stick.toml"),
        "[package]\nname = \"compile-stats-fixture\"\ntype = \"lib\"\n").unwrap();
    std::fs::write(dir.path().join("main.rock"), "fn helper() -> int { 1 }\n").unwrap();
    std::fs::write(dir.path().join("tests/check.rock"),
        "@test\nfn check() { assert(helper() == 1) }\n").unwrap();
    dir
}

fn command() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rock"));
    cmd.env("ROCK_NUM_THREADS", "4").env("ROCK_INVOCATION_LOG", "off")
        .env_remove("ROCK_TEST_FILTER").env_remove("ROCK_COMPILE_STATS")
        .env_remove("ROCK_SIM_COMPILE_RECEIPT_PREPARE")
        // Fixture compiles must not queue behind the machine's real compiles.
        .env("ROCK_COMPILE_SLOTS", "0");
    cmd
}

#[test]
fn package_compile_appends_one_phase_record() {
    let dir = fixture();
    let stats = dir.path().join("records/stats.jsonl");
    let out = command().arg("test").arg(dir.path())
        .env("ROCK_COMPILE_STATS", &stats).env("ROCK_JOB_ID", "stats-fixture").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = std::fs::read_to_string(stats).expect("compile must write a stats record");
    let records: Vec<Value> = text.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record["schema"], 1);
    assert_eq!(record["kind"], "test");
    assert_eq!(record["job_id"], "stats-fixture");
    assert_eq!(record["status"], "ok");
    assert!(record["peak_rss_bytes"].as_u64().unwrap() > 0);
    let phases = record["phases"].as_array().unwrap();
    let names: Vec<_> = phases.iter().map(|phase| phase["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["compile slot", "discovery", "parse+stdlib", "semantic", "codegen", "prepare", "write"]);
    assert_eq!(record["compile_slot"], serde_json::json!({"slot": null, "waited_ms": 0.0}));
    for phase in phases {
        assert!(phase["peak_rss_bytes"].as_u64().unwrap() > 0);
        assert!(phase["wall_ms"].as_f64().unwrap() >= 0.0);
    }
    assert!(record["totals"]["chunks"].as_u64().unwrap() > 0);
    assert!(record["totals"]["emitted_lua_bytes"].as_u64().unwrap() > 0);
}

#[test]
fn disabled_stats_and_build_output_are_identical() {
    let dir = fixture();
    let a = dir.path().join("off.lua");
    let b = dir.path().join("on.lua");
    let stats = dir.path().join("stats.jsonl");
    let off = command().arg("compile").arg(dir.path()).arg("--output").arg(&a).output().unwrap();
    assert!(off.status.success(), "{}", String::from_utf8_lossy(&off.stderr));
    assert!(!stats.exists());
    let on = command().arg("compile").arg(dir.path()).arg("--output").arg(&b)
        .env("ROCK_COMPILE_STATS", &stats).output().unwrap();
    assert!(on.status.success(), "{}", String::from_utf8_lossy(&on.stderr));
    assert_eq!(std::fs::read(a).unwrap(), std::fs::read(b).unwrap());
    let records: Vec<Value> = std::fs::read_to_string(stats).unwrap().lines()
        .map(|line| serde_json::from_str(line).unwrap()).collect();
    assert_eq!(records.len(), 1, "nested emit helper must not produce a second record");
    assert_eq!(records[0]["kind"], "build");
    assert_eq!(records[0]["phases"].as_array().unwrap().last().unwrap()["name"], "write");
    assert_eq!(records[0]["totals"]["source_files"], 1);
    assert_eq!(records[0]["totals"]["source_bytes"], "fn helper() -> int { 1 }\n".len());
}

#[test]
fn compile_slot_wait_is_its_own_phase() {
    use std::time::{Duration, Instant};
    use wait_timeout::ChildExt;

    struct ReapChild(std::process::Child);
    impl Drop for ReapChild {
        fn drop(&mut self) {
            match self.0.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => {},
                Err(error) => log::warn!("checking compile child during cleanup: {error}"),
            }
            if let Err(error) = self.0.kill() {
                log::warn!("killing compile child during cleanup: {error}");
            }
            if let Err(error) = self.0.wait() {
                log::warn!("reaping compile child during cleanup: {error}");
            }
        }
    }

    let dir = fixture();
    let slots = dir.path().join("slots");
    std::fs::create_dir(&slots).unwrap();
    let held = std::fs::OpenOptions::new().create(true).truncate(false).write(true)
        .open(slots.join("batch-0")).unwrap();
    held.lock().unwrap();
    let stats = dir.path().join("stats.jsonl");
    let log = dir.path().join("stderr.log");
    let mut child = ReapChild(command().arg("compile").arg(dir.path()).arg("--output").arg(dir.path().join("out.lua"))
        .env("ROCK_COMPILE_STATS", &stats).env("ROCK_COMPILE_SLOTS", "1")
        .env("ROCK_COMPILE_CLASS", "batch").env("ROCK_COMPILE_SLOT_DIR", &slots)
        .env("STONE_LOG", "info").env("ROCK_LOG_COLOR", "never")
        .stdout(std::process::Stdio::null()).stderr(std::fs::File::create(&log).unwrap()).spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let stderr = std::fs::read_to_string(&log).unwrap();
        if stderr.contains("[COMPILE] waiting for a compile slot") { break }
        assert!(child.0.try_wait().unwrap().is_none(), "compile exited before waiting: {stderr}");
        assert!(Instant::now() < deadline, "compile never reported slot contention: {stderr}");
        std::thread::sleep(Duration::from_millis(10));
    }
    // The lower bound starts after contention is observed, excluding child startup.
    let observed = Instant::now();
    std::thread::sleep(Duration::from_millis(300));
    let held_for = observed.elapsed();
    drop(held);
    let status = child.0.wait_timeout(Duration::from_secs(30)).unwrap()
        .expect("compile did not finish after releasing its slot");
    assert!(status.success(), "{}", std::fs::read_to_string(&log).unwrap());
    let record: Value = serde_json::from_str(std::fs::read_to_string(stats).unwrap().trim()).unwrap();
    // The test's locked `batch-0` filled the one-compile pool; the compile took the first ticket.
    assert_eq!(record["compile_slot"]["slot"], "held-000000000001");
    let waited = record["compile_slot"]["waited_ms"].as_f64().unwrap();
    assert!(waited >= held_for.as_secs_f64() * 1000.0, "held after observing contention: {held_for:?}; {record}");
    let first = &record["phases"][0];
    assert_eq!(first["name"], "compile slot");
    assert!(first["wall_ms"].as_f64().unwrap() >= waited, "{record}");
}

#[test]
fn append_failure_logs_and_preserves_compile_success() {
    let dir = fixture();
    let out = command().arg("test").arg(dir.path())
        .env("ROCK_COMPILE_STATS", dir.path()).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("cannot append"), "write failure must be logged: {text}");
}

#[test]
fn empty_stats_path_disables_recording() {
    let dir = fixture();
    let out = command().arg("test").arg(dir.path()).env("ROCK_COMPILE_STATS", "").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(!text.contains("COMPILE_STATS"), "an empty path must not try to record: {text}");
}

#[test]
fn concurrent_compiles_append_complete_records() {
    let dir = fixture();
    let stats = dir.path().join("stats.jsonl");
    let mut children = Vec::new();
    for _ in 0..4 {
        children.push(command().arg("test").arg(dir.path())
            .env("ROCK_COMPILE_STATS", &stats)
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped()).spawn().unwrap());
    }
    for child in children {
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }
    let records: Vec<Value> = std::fs::read_to_string(stats).unwrap().lines()
        .map(|line| serde_json::from_str(line).unwrap()).collect();
    assert_eq!(records.len(), 4);
    let pids: std::collections::HashSet<_> = records.iter().map(|record| record["pid"].as_u64().unwrap()).collect();
    assert_eq!(pids.len(), 4);
}

#[test]
fn rejected_compile_records_error_without_changing_exit() {
    let dir = fixture();
    std::fs::write(dir.path().join("main.rock"), "fn broken( {\n").unwrap();
    let stats = dir.path().join("stats.jsonl");
    let out = command().arg("compile").arg(dir.path()).arg("--output").arg(dir.path().join("out.lua"))
        .env("ROCK_COMPILE_STATS", &stats).output().unwrap();
    assert!(!out.status.success());
    let records: Vec<Value> = std::fs::read_to_string(stats).unwrap().lines()
        .map(|line| serde_json::from_str(line).unwrap()).collect();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["status"], "error");
    assert!(records[0]["peak_rss_bytes"].as_u64().unwrap() > 0);
}

#[test]
fn sim_compile_child_is_labeled_separately() {
    let dir = fixture();
    let stats = dir.path().join("stats.jsonl");
    // A test compile exercises the same context marker as the real child;
    // the cold MAG gate additionally witnesses the actual sim subprocess.
    let out = command().arg("test").arg(dir.path()).env("ROCK_COMPILE_STATS", &stats)
        .env("ROCK_SIM_COMPILE_RECEIPT_PREPARE", "1").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let record: Value = serde_json::from_str(std::fs::read_to_string(stats).unwrap().trim()).unwrap();
    assert_eq!(record["kind"], "sim compile child");
}

#[test]
fn live_initial_and_body_reload_record_separate_compiles() {
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "compile_stats::live_reload_child", "--nocapture"])
        .env("ROCK_COMPILE_STATS_LIVE_CHILD", "1").env_remove("ROCK_COMPILE_STATS")
        .output().unwrap();
    assert!(out.status.success(), "{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
}

#[test]
fn live_reload_child() {
    if std::env::var_os("ROCK_COMPILE_STATS_LIVE_CHILD").is_none() { return }
    let dir = fixture();
    let stats = dir.path().join("stats.jsonl");
    // This helper runs alone in a fresh child; no concurrent env readers.
    unsafe { std::env::set_var("ROCK_COMPILE_STATS", &stats); }
    let source = dir.path().join("main.rock");
    let main = dir.path().join("main.lua");
    let incremental = dir.path().join("incremental.lua");
    let mut stone = rock_cli::skull_backend::SkullStone::init(
        dir.path().to_str().unwrap(), &main, &incremental).unwrap();
    stone.enable_route_profile();
    let reload = stone.apply_edit(&source, "fn helper() -> int { 2 }\n", &main, &incremental).unwrap();
    assert!(matches!(reload, rock_cli::skull_backend::SkullReload::Incremental { .. }));
    assert!(stone.take_last_route_profile().is_some());
    let records: Vec<Value> = std::fs::read_to_string(stats).unwrap().lines()
        .map(|line| serde_json::from_str(line).unwrap()).collect();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["status"], "ok");
    assert_eq!(records[1]["status"], "ok");
    let names: Vec<_> = records[1]["phases"].as_array().unwrap().iter()
        .map(|phase| phase["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["live-project", "codegen", "write"]);
    assert!(records[1]["totals"]["source_files"].as_u64().unwrap() > 0);
    assert!(records[1]["totals"]["source_bytes"].is_null());
}

#[test]
fn relative_test_and_sim_paths_keep_source_categories() {
    let dir = fixture();
    std::fs::create_dir(dir.path().join("tests/sims")).unwrap();
    let stats = dir.path().join("stats.jsonl");
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent().unwrap().parent().unwrap();
    for path in ["tests/check.rock", "tests/sims/check.rock"] {
        std::fs::write(dir.path().join(path), "@test\nfn check() { assert(true) }\n").unwrap();
        let out = command().current_dir(dir.path()).env("ROCK_DIR", root)
            .args(["compile-single", path, "--output"])
            .arg(dir.path().join("out.lua")).env("ROCK_COMPILE_STATS", &stats)
            .output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }
    let records: Vec<Value> = std::fs::read_to_string(stats).unwrap().lines()
        .map(|line| serde_json::from_str(line).unwrap()).collect();
    assert_eq!(records.len(), 2);
    for (record, category) in records.iter().zip(["tests", "sims"]) {
        assert_eq!(record["totals"]["source_files"], 1);
        assert_eq!(record["totals"][category]["files"], 1);
        assert_eq!(record["totals"]["production"]["files"], 0);
    }
}
