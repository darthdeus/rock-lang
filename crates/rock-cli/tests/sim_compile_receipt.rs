use std::process::Command;
use crate::stone_harness::{HeavyCommandExt, SpawnBudget};
#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};
#[cfg(target_os = "linux")]
use crate::kill_harness::{children_of, cmdline, RunnerGuard};

fn list_sims(
    rock: &std::path::Path,
    project: &str,
    receipt_dir: &std::path::Path,
    timing_log: &std::path::Path,
    prepare: bool,
) -> std::process::Output {
    let mut command = Command::new(rock);
    command
        .args(["sim", "--list", project])
        .env("ROCK_SIM_COMPILE_RECEIPT_DIR", receipt_dir)
        .env("ROCK_SIM_COMPILE_TIMING", timing_log);
    if prepare {
        command.env("ROCK_SIM_COMPILE_RECEIPT_PREPARE", "1");
    } else {
        command.env_remove("ROCK_SIM_COMPILE_RECEIPT_PREPARE");
    }
    command.output_with_deadline().expect("spawn rock sim --list")
}

#[test]
fn prepared_sim_receipt_reuses_one_project_compile() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let dir = tempfile::tempdir().expect("tempdir");
    let rock = pinned_rock(dir.path());
    let receipt_dir = dir.path().join("receipts");
    let timing_log = dir.path().join("compile.jsonl");

    let prepared = list_sims(&rock, "testbed", &receipt_dir, &timing_log, true);
    assert!(
        prepared.status.success(),
        "receipt preparation failed:\n{}",
        String::from_utf8_lossy(&prepared.stderr)
    );
    let reused_a = list_sims(&rock, "testbed", &receipt_dir, &timing_log, false);
    assert!(
        reused_a.status.success(),
        "first receipt reuse failed:\n{}",
        String::from_utf8_lossy(&reused_a.stderr)
    );
    let reused_b = list_sims(&rock, "testbed", &receipt_dir, &timing_log, false);
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
    let manifest: serde_json::Value = serde_json::from_slice(
        &std::fs::read(project_receipt.join("receipt.json")).expect("read receipt manifest"),
    )
    .expect("parse receipt manifest");
    assert_eq!(manifest["schema_version"], 4, "the cache identity changes the receipt schema");
    let registry = manifest["registry"].as_array().expect("receipt registry");
    assert!(!registry.is_empty(), "testbed receipt should include simulations");
    for entry in registry {
        let source_file = entry["source_file"].as_str().expect("receipt source file");
        assert!(
            source_file.starts_with("games/testbed/"),
            "receipt source paths should be workspace-relative, got {source_file}"
        );
    }
    let main_path = project_receipt.join("main.lua");
    let mut main = std::fs::read(&main_path).expect("read receipt main.lua");
    main.extend_from_slice(b"\n-- tampered\n");
    std::fs::write(&main_path, main).expect("tamper receipt main.lua");
    let rejected = list_sims(&rock, "testbed", &receipt_dir, &timing_log, false);
    let stderr = String::from_utf8_lossy(&rejected.stderr);
    assert!(!rejected.status.success(), "a modified receipt artifact must be rejected");
    assert!(
        stderr.contains("main.lua hash does not match"),
        "wrong rejection for a modified receipt artifact:\n{stderr}"
    );
}

#[test]
fn prepared_sim_receipt_rejects_changed_project_sources() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let dir = tempfile::tempdir().expect("tempdir");
    let rock = pinned_rock(dir.path());
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

    let prepared = list_sims(&rock, &project_arg, &receipt_dir, &timing_log, true);
    assert!(
        prepared.status.success(),
        "receipt preparation failed:\n{}",
        String::from_utf8_lossy(&prepared.stderr)
    );
    std::fs::write(project.join("main.rock"), format!("{source}\n// changed\n"))
        .expect("change fixture source");
    let rejected = list_sims(&rock, &project_arg, &receipt_dir, &timing_log, false);
    let stderr = String::from_utf8_lossy(&rejected.stderr);
    assert!(!rejected.status.success(), "a source-stale receipt must be rejected");
    assert!(
        stderr.contains("receipt inputs changed"),
        "wrong rejection for a source-stale receipt:\n{stderr}"
    );
}

#[test]
fn sim_run_compiles_in_a_child_process_and_consumes_its_receipt() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let dir = tempfile::tempdir().expect("tempdir");
    let rock = pinned_rock(dir.path());
    let timing_log = dir.path().join("compile.jsonl");
    let workspace = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("workspace root")
        .to_path_buf();
    let output = Command::new(&rock)
        .args(["sim", "testbed", "testbed::co_join2_tuple"])
        .current_dir(&workspace)
        .env_remove("ROCK_SIM_COMPILE_RECEIPT_DIR")
        .env_remove("ROCK_SIM_COMPILE_RECEIPT_PREPARE")
        .env_remove("ROCK_SIM_COMPILE_CACHE")
        .env("ROCK_SIM_COMPILE_CACHE_DIR", dir.path().join("cache"))
        .env("ROCK_SIM_COMPILE_TIMING", &timing_log)
        .env("ROCK_SIM_LOG_DIR", dir.path().join("history"))
        .env("ROCK_SIM_TIMEOUT_SECS", "180")
        .output_with_deadline()
        .expect("run testbed sim");
    assert!(
        output.status.success(),
        "testbed sim failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let records = std::fs::read_to_string(&timing_log).expect("read compile timing log");
    let phase_pids = |phase: &str| {
        records
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("timing record"))
            .filter(|record| record["phase"] == phase)
            .map(|record| record["pid"].as_u64().expect("timing pid"))
            .collect::<Vec<_>>()
    };
    let compiled = phase_pids("skull_compile_emit");
    let materialized = phase_pids("receipt_materialize");
    assert_eq!(compiled.len(), 1, "one run compiles once; timing log:\n{records}");
    assert_eq!(materialized.len(), 1, "the runner loads the compile's receipt; timing log:\n{records}");
    assert_ne!(
        compiled[0], materialized[0],
        "the compiler must run in its own process so the runner never holds its memory"
    );
}

#[test]
fn sim_run_compile_child_keeps_the_invocations_selection() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let dir = tempfile::tempdir().expect("tempdir");
    let rock = pinned_rock(dir.path());
    let workspace = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("workspace root")
        .to_path_buf();
    for (flag, value, rejection) in [
        ("--var", "UNDECLARED_SIM_VAR=1", "--var UNDECLARED_SIM_VAR is not declared"),
        ("--channel", "undeclared-channel", "channel \"undeclared-channel\" is not declared"),
    ] {
        let output = Command::new(&rock)
            .args(["sim", "testbed", "testbed::co_join2_tuple", flag, value])
            .current_dir(&workspace)
            .env_remove("ROCK_SIM_COMPILE_RECEIPT_DIR")
            .env_remove("ROCK_SIM_COMPILE_RECEIPT_PREPARE")
            .env("ROCK_SIM_LOG_DIR", dir.path().join("history"))
            .env("ROCK_SIM_TIMEOUT_SECS", "180")
            .output_with_deadline()
            .expect("run testbed sim");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "`{flag} {value}` must reject the run:\n{stderr}");
        assert!(stderr.contains(rejection), "wrong rejection for `{flag} {value}`:\n{stderr}");
    }
}

fn workspace_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

/// A one-sim stone package whose build vars come from files the test edits:
/// `PROBE` is an ordinary command-backed var and `CLOCK` a volatile one.
/// A library sim run (the `rock_cli_heavy` harness) keys its compile as the
/// `rock sim --list` child records it, so a repeat run loads the cached
/// receipt. The test process is no `rock sim` invocation and has no
/// `ROCK_BUILD_COMMAND`; a build var that reads it, as the telemetry key
/// loader does, must still resolve to the child's value.
#[test]
fn library_sim_runs_key_their_compile_as_the_sim_child_does() {
    const HELPER: &str = "ROCK_TEST_LIBRARY_SIM_PROJECT";
    if let Some(project) = std::env::var_os(HELPER) {
        let rock = std::path::Path::new(env!("CARGO_BIN_EXE_rock"));
        for run in 0..2 {
            let records = rock_cli::run_sim::run_worker_suite(
                rock_cli::Backend::Skull,
                project.to_str().unwrap(),
                rock,
                &["cache_probe".to_string()],
            )
            .unwrap_or_else(|error| panic!("library sim run {run}: {error:#}"));
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].0.verdict(), "PASS", "{:?}", records[0].0);
        }
        return;
    }
    let _spawn_budget = SpawnBudget::for_test(120);
    let dir = tempfile::tempdir().expect("tempdir");
    let project = dir.path().join("project");
    write_cache_fixture(&project);
    let manifest = project.join("stick.toml");
    let mut stick = std::fs::read_to_string(&manifest).unwrap();
    stick.push_str("COMMAND = { cmd = \"echo ${ROCK_BUILD_COMMAND:-none}\" }\n");
    std::fs::write(&manifest, stick).unwrap();
    let timing_log = dir.path().join("compile.jsonl");
    let output = Command::new(std::env::current_exe().expect("test executable"))
        .args(["sim_compile_receipt::library_sim_runs_key_their_compile_as_the_sim_child_does", "--exact", "--nocapture"])
        .current_dir(workspace_root())
        .env(HELPER, &project)
        .env("ROCK_SIM_COMPILE_CACHE_DIR", dir.path().join("cache"))
        .env("ROCK_SIM_COMPILE_TIMING", &timing_log)
        .env_remove("ROCK_SIM_COMPILE_CACHE")
        .env_remove("ROCK_SIM_COMPILE_RECEIPT_DIR")
        .env_remove("ROCK_SIM_COMPILE_RECEIPT_PREPARE")
        .env_remove("ROCK_BUILD_COMMAND")
        .output_with_deadline()
        .expect("library sim helper");
    assert!(output.status.success(), "{}{}",
        String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    assert!(String::from_utf8_lossy(&output.stdout).contains("test result: ok. 1 passed;"),
        "the helper must run its test:\n{}", String::from_utf8_lossy(&output.stdout));
    let timing = std::fs::read_to_string(&timing_log).expect("compile timing log");
    let count = |phase: &str| timing.lines().filter(|line| line.contains(&format!("\"phase\":\"{phase}\""))).count();
    assert_eq!(count("cache_lookup"), 2, "{timing}");
    assert_eq!(count("skull_compile_emit"), 1, "the second library run must hit the cache:\n{timing}");
}

fn write_cache_fixture(project: &std::path::Path) {
    std::fs::create_dir_all(project).expect("create fixture project");
    std::fs::write(
        project.join("stick.toml"),
        format!(
            "[package]\nname = \"cache-fixture\"\nversion = \"0.1.0\"\ntype = \"bin\"\n\n[dependencies]\nstone = {{ path = {:?} }}\n\n[build.vars]\nGIT_SHA = \"cache\"\nBUILD_UNIX = \"0\"\nBUILD_DATE = \"test\"\nCHANNEL = {{ builtin = \"channel\" }}\nPLATFORM = {{ builtin = \"platform\" }}\nPROBE = {{ cmd = \"cat probe.txt\" }}\nCLOCK = {{ cmd = \"cat clock.txt\", volatile = true }}\n",
            workspace_root().join("stone")
        ),
    )
    .expect("write fixture manifest");
    std::fs::write(project.join("probe.txt"), "declared\n").expect("write probe var");
    std::fs::write(project.join("clock.txt"), "1\n").expect("write clock var");
    std::fs::write(
        project.join("main.rock"),
        concat!(
            "fn main() {\n",
            "    stone_run_game(GameConfig {\n",
            "        title: \"cache-fixture\",\n",
            "        width: 320,\n",
            "        height: 180,\n",
            "        vsync: 0,\n",
            "        clear_color: color(0.0, 0.0, 0.0, 1.0),\n",
            "        esc_quits: true,\n",
            "        cap_height_fonts: true,\n",
            "    })\n",
            "}\n\n",
            "@sim\n",
            "co fn cache_probe() -> Coroutine<Wait> {\n",
            "    start_game \"cache-fixture\"\n",
            "    sim_expect(build_var(\"PROBE\") != \"\", \"PROBE resolved\")\n",
            "    stop_sim()\n",
            "}\n\n",
            "@sim\n",
            "co fn other_probe() -> Coroutine<Wait> {\n",
            "    start_game \"cache-fixture\"\n",
            "    sim_expect(build_var(\"PROBE\") != \"\", \"PROBE resolved\")\n",
            "    stop_sim()\n",
            "}\n\n",
            "@sim\n",
            "co fn forwarded_probe() -> Coroutine<Wait> {\n",
            "    start_game \"cache-fixture\"\n",
            "    sim_expect(build_var(\"PROBE\") == \"forwarded\", \"the compile sees the runner's --var\")\n",
            "    stop_sim()\n",
            "}\n",
        ),
    )
    .expect("write fixture source");
}

struct CacheRun<'a> {
    rock: &'a std::path::Path,
    project: &'a std::path::Path,
    cache: &'a std::path::Path,
    scratch: &'a std::path::Path,
}

fn copy_rock_executable(source: &std::path::Path, destination: &std::path::Path) {
    crate::stone_harness::copy_executable(source, destination);
}

fn pinned_rock(tempdir: &std::path::Path) -> std::path::PathBuf {
    pin_rock_executable(std::path::Path::new(env!("CARGO_BIN_EXE_rock")), tempdir)
}

fn pin_rock_executable(source: &std::path::Path, tempdir: &std::path::Path) -> std::path::PathBuf {
    let pinned = tempdir.join("rock-pinned");
    copy_rock_executable(source, &pinned);
    pinned
}

impl CacheRun<'_> {
    /// Run the fixture's sim once and report whether that run compiled.
    fn compiled(&self, label: &str, sim: &str, args: &[&str], env: &[(&str, &str)]) -> bool {
        let timing_log = self.scratch.join(format!("{label}.jsonl"));
        let mut command = Command::new(self.rock);
        command
            .args(args)
            .arg("sim")
            .arg(self.project)
            .arg(sim)
            .current_dir(workspace_root())
            .env("ROCK_DIR", workspace_root())
            .env("ROCK_SIM_COMPILE_CACHE_DIR", self.cache)
            .env_remove("ROCK_SIM_COMPILE_CACHE")
            .env_remove("ROCK_SIM_COMPILE_RECEIPT_DIR")
            .env_remove("ROCK_SIM_COMPILE_RECEIPT_PREPARE")
            .env("ROCK_SIM_COMPILE_TIMING", &timing_log)
            .env("ROCK_SIM_LOG_DIR", self.scratch.join("history"))
            .env("ROCK_SIM_TIMEOUT_SECS", "180");
        for (key, value) in env {
            command.env(key, value);
        }
        let output = command.output_with_deadline().expect("run fixture sim");
        assert!(
            output.status.success(),
            "{label}: fixture sim failed ({}):\n{}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let records = std::fs::read_to_string(&timing_log).expect("read compile timing log");
        let count = |phase: &str| {
            records
                .lines()
                .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("timing record"))
                .filter(|record| record["phase"] == phase)
                .count()
        };
        assert_eq!(count("receipt_materialize"), 1, "{label}: one receipt load; log:\n{records}");
        match count("skull_compile_emit") {
            0 => false,
            1 => true,
            n => panic!("{label}: {n} compiles in one run; log:\n{records}"),
        }
    }
}

fn cache_entries(cache: &std::path::Path) -> Vec<String> {
    let mut names = std::fs::read_dir(cache)
        .expect("read cache dir")
        .map(|entry| entry.expect("cache entry"))
        .filter(|entry| entry.file_type().expect("cache entry type").is_dir())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    names.sort();
    names
}

#[test]
fn sim_compile_cache_run_survives_original_executable_removal() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let dir = tempfile::tempdir().expect("tempdir");
    let source = std::path::PathBuf::from(env!("CARGO_BIN_EXE_rock"));
    let original = dir.path().join("rock-original");
    copy_rock_executable(&source, &original);
    let rock = pin_rock_executable(&original, dir.path());
    let project = dir.path().join("project");
    let cache = dir.path().join("cache");
    write_cache_fixture(&project);
    let run = CacheRun { rock: &rock, project: &project, cache: &cache, scratch: dir.path() };

    assert!(run.compiled("before-removal", "cache_probe", &[], &[]), "cold run compiles");
    std::fs::remove_file(&original).expect("remove the replaceable original path");
    assert!(
        !run.compiled("after-removal", "cache_probe", &[], &[]),
        "the cached fixture still runs after the original executable path disappears"
    );
}

#[test]
fn sim_compile_cache_reuses_identical_inputs_and_recompiles_when_any_key_input_changes() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let dir = tempfile::tempdir().expect("tempdir");
    let rock = pinned_rock(dir.path());
    let project = dir.path().join("project");
    let cache = dir.path().join("cache");
    write_cache_fixture(&project);
    let run = CacheRun { rock: &rock, project: &project, cache: &cache, scratch: dir.path() };

    assert!(run.compiled("cold", "cache_probe", &[], &[]), "an empty cache compiles");
    assert!(!run.compiled("warm", "cache_probe", &[], &[]), "identical inputs reuse the cached compile");
    assert!(
        run.compiled("other-sim", "other_probe", &[], &[]),
        "a different narrow selection compiles its own receipt"
    );

    std::fs::write(project.join("clock.txt"), "2\n").expect("change volatile var");
    assert!(
        !run.compiled("volatile", "cache_probe", &[], &[]),
        "a volatile build var does not change the artifact's identity"
    );

    let source = std::fs::read_to_string(project.join("main.rock")).expect("read source");
    std::fs::write(project.join("main.rock"), format!("{source}\n// edited\n")).expect("edit source");
    assert!(run.compiled("source", "cache_probe", &[], &[]), "a source edit recompiles");
    assert!(!run.compiled("source-warm", "cache_probe", &[], &[]), "the edited source is cached");

    std::fs::write(project.join("probe.txt"), "forwarded\n").expect("change build var");
    assert!(run.compiled("build-var", "cache_probe", &[], &[]), "a changed build var value recompiles");
    assert!(
        run.compiled("var-override", "cache_probe", &["--var", "PROBE=overridden"], &[]),
        "a --var override recompiles"
    );

    assert!(
        run.compiled("environment", "cache_probe", &[], &[("ROCK_CACHE_KEY_PROBE", "1")]),
        "an unlisted ROCK_ variable is keyed"
    );

    let stdlib = dir.path().join("stdlib");
    std::fs::create_dir(&stdlib).expect("create stdlib copy");
    for entry in std::fs::read_dir(workspace_root().join("stdlib")).expect("read stdlib") {
        let path = entry.expect("stdlib entry").path();
        if path.extension().is_some_and(|ext| ext == "rock") {
            std::fs::copy(&path, stdlib.join(path.file_name().unwrap())).expect("copy stdlib file");
        }
    }
    let stdlib_env = [("ROCK_STDLIB_DIR", stdlib.to_str().expect("utf-8 stdlib path"))];
    assert!(run.compiled("stdlib-dir", "cache_probe", &[], &stdlib_env), "a stdlib dir change recompiles");
    assert!(!run.compiled("stdlib-warm", "cache_probe", &[], &stdlib_env), "the stdlib copy is cached");
    let unified = stdlib.join("unified.rock");
    let text = std::fs::read_to_string(&unified).expect("read stdlib copy");
    std::fs::write(&unified, format!("{text}\n// edited\n")).expect("edit stdlib copy");
    assert!(run.compiled("stdlib-edit", "cache_probe", &[], &stdlib_env), "a stdlib edit recompiles");

    let compiler = dir.path().join("rock-copy");
    copy_rock_executable(&rock, &compiler);
    assert_eq!(
        std::fs::read(&rock).expect("read pinned rock"),
        std::fs::read(&compiler).expect("read byte-identical rock copy"),
        "compiler identity copy starts with identical executable bytes"
    );
    let changed = CacheRun { rock: &compiler, ..run };
    assert!(
        !changed.compiled("compiler-copy", "cache_probe", &[], &[]),
        "a byte-identical compiler at another path reuses the compile"
    );
    #[cfg(target_os = "macos")]
    {
        // Rewriting signed bytes can make macOS kill the executable before startup.
        let before = std::fs::read(&compiler).expect("read rock copy");
        let signed = Command::new("codesign")
            .args(["--force", "--sign", "-", "--identifier", "rock-sim-cache-fixture"])
            .arg(&compiler)
            .output_with_deadline()
            .expect("change rock copy's signed identity");
        assert!(
            signed.status.success(),
            "codesign failed ({}):\n{}",
            signed.status,
            String::from_utf8_lossy(&signed.stderr)
        );
        assert!(
            std::fs::read(&compiler).expect("read signed rock copy") != before,
            "changing the signed identity must change the compiler bytes"
        );
    }
    #[cfg(not(target_os = "macos"))]
    {
        let mut bytes = std::fs::read(&compiler).expect("read rock copy");
        bytes.push(0);
        crate::stone_harness::overwrite_executable(&compiler, &bytes);
    }
    assert_ne!(
        std::fs::read(&rock).expect("read pinned rock"),
        std::fs::read(&compiler).expect("read changed rock copy"),
        "changed compiler copy has distinct executable bytes"
    );
    assert!(changed.compiled("compiler", "cache_probe", &[], &[]), "a changed compiler recompiles");

    let entries = cache_entries(&cache);
    assert!(
        entries.iter().all(|name| !name.starts_with('.')),
        "no staging or eviction directories remain: {entries:?}"
    );
}

#[test]
fn concurrent_sim_runs_share_one_compile_cache_entry() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let dir = tempfile::tempdir().expect("tempdir");
    let rock = pinned_rock(dir.path());
    let project = dir.path().join("project");
    let cache = dir.path().join("cache");
    write_cache_fixture(&project);
    let run = CacheRun { rock: &rock, project: &project, cache: &cache, scratch: dir.path() };
    let budget = SpawnBudget::budget();
    std::thread::scope(|scope| {
        let a = scope.spawn(|| {
            let _spawn_budget = SpawnBudget::enter(budget);
            run.compiled("concurrent-a", "cache_probe", &[], &[])
        });
        let b = scope.spawn(|| {
            let _spawn_budget = SpawnBudget::enter(budget);
            run.compiled("concurrent-b", "cache_probe", &[], &[])
        });
        a.join().expect("first run");
        b.join().expect("second run");
    });
    assert_eq!(cache_entries(&cache).len(), 1, "both runs publish one key: {:?}", cache_entries(&cache));
    assert!(!run.compiled("after", "cache_probe", &[], &[]), "the published entry serves later runs");
}

#[test]
fn concurrent_cold_sim_cache_requests_compile_exactly_once() {
    let _spawn_budget = SpawnBudget::for_test(120);
    use std::io::Write;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    let dir = tempfile::tempdir().expect("tempdir");
    let rock = pinned_rock(dir.path());
    let project = dir.path().join("project");
    let cache = dir.path().join("cache");
    let slots = dir.path().join("slots");
    write_cache_fixture(&project);
    std::fs::create_dir(&slots).unwrap();
    let mut slot = std::fs::File::options().create(true).truncate(false)
        .read(true).write(true).open(slots.join("batch-0")).unwrap();
    slot.lock().unwrap();
    writeln!(slot, "{} test barrier", std::process::id()).unwrap();
    let spawn = |label: &str| {
        Command::new(&rock)
            .arg("sim").arg(&project).arg("cache_probe")
            .current_dir(workspace_root())
            .env("ROCK_DIR", workspace_root())
            .env("ROCK_SIM_COMPILE_CACHE_DIR", &cache)
            .env_remove("ROCK_SIM_COMPILE_CACHE")
            .env_remove("ROCK_SIM_COMPILE_RECEIPT_DIR")
            .env_remove("ROCK_SIM_COMPILE_RECEIPT_PREPARE")
            .env("ROCK_SIM_COMPILE_TIMING", dir.path().join(format!("{label}.jsonl")))
            .env("ROCK_SIM_LOG_DIR", dir.path().join("history"))
            .env("ROCK_COMPILE_SLOT_DIR", &slots)
            .env("ROCK_COMPILE_SLOTS", "1")
            .env("ROCK_COMPILE_CLASS", "batch")
            .stdout(Stdio::from(std::fs::File::create(dir.path().join(format!("{label}.out"))).unwrap()))
            .stderr(Stdio::from(std::fs::File::create(dir.path().join(format!("{label}.err"))).unwrap()))
            .spawn_with_deadline().expect("spawn cold request")
    };
    let mut a = spawn("a");
    let mut b = spawn("b");
    let deadline = Instant::now() + test_utils::HANG_BACKSTOP;
    let both_missed = loop {
        if ["a", "b"].iter().all(|label| {
            std::fs::read_to_string(dir.path().join(format!("{label}.err")))
                .unwrap().contains("compile cache miss")
        }) {
            break true;
        }
        if Instant::now() >= deadline { break false; }
        std::thread::sleep(Duration::from_millis(20));
    };
    slot.unlock().unwrap();
    if !both_missed {
        a.kill().expect("stop first request");
        b.kill().expect("stop second request");
    }
    let statuses = [a.wait().unwrap(), b.wait().unwrap()];
    let logs = ["a", "b"].map(|label| {
        std::fs::read_to_string(dir.path().join(format!("{label}.err"))).unwrap()
    });
    assert!(both_missed, "both requests must reach the cold key: {logs:?}");
    assert!(statuses.iter().all(|status| status.success()), "{statuses:?}: {logs:?}");
    let records = ["a", "b"].map(|label| {
        std::fs::read_to_string(dir.path().join(format!("{label}.jsonl"))).unwrap()
    }).join("\n");
    let count = |phase: &str| records.lines().filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .filter(|record| record["phase"] == phase).count();
    assert_eq!(count("receipt_materialize"), 2, "both requests consume a receipt");
    assert_eq!(count("skull_compile_emit"), 1, "same cold key compiled twice: {records}");
    assert!(logs.iter().any(|log| log.contains("compiling the same sim program")), "{logs:?}");
}

#[test]
fn sim_run_forwards_build_var_overrides_to_its_compile() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let dir = tempfile::tempdir().expect("tempdir");
    let rock = pinned_rock(dir.path());
    let project = dir.path().join("project");
    let cache = dir.path().join("cache");
    write_cache_fixture(&project);
    let run = CacheRun { rock: &rock, project: &project, cache: &cache, scratch: dir.path() };
    assert!(run.compiled("forwarded", "forwarded_probe", &["--var", "PROBE=forwarded"], &[]));
}

#[test]
fn sim_compile_receipt_narrow_selection_excludes_unselected_body() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let dir = tempfile::tempdir().expect("tempdir");
    let rock = pinned_rock(dir.path());
    let project = dir.path().join("project");
    let cache = dir.path().join("cache");
    write_cache_fixture(&project);
    let run = CacheRun { rock: &rock, project: &project, cache: &cache, scratch: dir.path() };
    assert!(run.compiled("narrow", "cache_probe", &[], &[]));
    let entries = cache_entries(&cache);
    assert_eq!(entries.len(), 1);
    let lua = std::fs::read_to_string(cache.join(&entries[0]).join("main.lua")).unwrap();
    assert!(!lua.contains("the compile sees the runner's --var"), "unselected sim body emitted");
    let manifest: serde_json::Value = serde_json::from_slice(
        &std::fs::read(cache.join(&entries[0]).join("receipt.json")).unwrap(),
    ).unwrap();
    assert_eq!(manifest["registry"].as_array().unwrap().len(), 1);
}

#[test]
fn sim_compile_receipt_cache_distinguishes_selection_and_prefers_full_entry() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let dir = tempfile::tempdir().expect("tempdir");
    let rock = pinned_rock(dir.path());
    let project = dir.path().join("project");
    let cache = dir.path().join("cache");
    write_cache_fixture(&project);
    let run = CacheRun { rock: &rock, project: &project, cache: &cache, scratch: dir.path() };
    assert!(run.compiled("first", "cache_probe", &[], &[]));
    assert!(run.compiled("second", "other_probe", &[], &[]), "another selection must compile");
    assert_eq!(cache_entries(&cache).len(), 2);
    assert!(run.compiled("pair", "cache_probe,other_probe", &[], &[]));
    assert!(!run.compiled("pair-reordered", "other_probe,cache_probe", &[], &[]),
        "order and chain grouping do not change compile roots");
    let args = ["--var", "PROBE=forwarded"];
    assert!(run.compiled("full", "cache_probe,other_probe,forwarded_probe", &args, &[]));
    assert!(!run.compiled("full-reuse", "cache_probe", &args, &[]), "narrow run reuses full entry");
}

#[test]
fn sim_compile_receipt_named_dependency_is_registered_and_runs() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let dir = tempfile::tempdir().expect("tempdir");
    let rock = pinned_rock(dir.path());
    let project = dir.path().join("project");
    let cache = dir.path().join("cache");
    write_cache_fixture(&project);
    let stone = dir.path().join("stone");
    copy_stone_sources(&workspace_root().join("stone"), &stone);
    let stone_manifest = stone.join("stick.toml");
    let text = std::fs::read_to_string(&stone_manifest).unwrap()
        .replace("../stone-core", workspace_root().join("stone-core").to_str().unwrap())
        .replace("../stone-native", workspace_root().join("stone-native").to_str().unwrap());
    std::fs::write(stone_manifest, text).unwrap();
    let manifest = project.join("stick.toml");
    let text = std::fs::read_to_string(&manifest).unwrap()
        .replace(workspace_root().join("stone").to_str().unwrap(), stone.to_str().unwrap());
    std::fs::write(manifest, text).unwrap();
    let engine_sim = stone.join("engine/sim.rock");
    let text = std::fs::read_to_string(&engine_sim).unwrap();
    std::fs::write(engine_sim, format!("{text}\npub fn receipt_dependency_body() -> (() -> Coroutine<Wait>)? {{ SIMS[\"dependency_probe\"] }}\npub fn receipt_leaf_body() -> (() -> Coroutine<Wait>)? {{ SIMS.get(\"leaf_probe\") }}\npub fn receipt_dynamic_body(name: string) -> (() -> Coroutine<Wait>)? {{ SIMS.get(name) }}\n")).unwrap();
    let path = project.join("main.rock");
    let mut source = std::fs::read_to_string(&path).unwrap();
    source.push_str(r#"
@sim
co fn dependency_probe() -> Coroutine<Wait> {
    sim_expect(true, "named dependency ran")
    let leaf = receipt_leaf_body()
    sim_expect(leaf != null, "transitive dependency is registered")
    if leaf != null { await leaf() }
}
@sim
co fn leaf_probe() -> Coroutine<Wait> {
    sim_expect(true, "transitive dependency ran")
    yield wait_frame()
}
@sim
co fn lookup_probe() -> Coroutine<Wait> {
    start_game "cache-fixture"
    let body = receipt_dependency_body()
    sim_expect(body != null, "dependency is registered")
    if body != null { await body() }
    stop_sim()
}
@sim
co fn dynamic_probe() -> Coroutine<Wait> {
    let body = receipt_dynamic_body("dependency_probe")
    if body != null { await body() }
    stop_sim()
}
"#);
    std::fs::write(path, source).unwrap();
    let run = CacheRun { rock: &rock, project: &project, cache: &cache, scratch: dir.path() };
    assert!(run.compiled("lookup", "lookup_probe", &[], &[]));
    let entries = cache_entries(&cache);
    let manifest: serde_json::Value = serde_json::from_slice(
        &std::fs::read(cache.join(&entries[0]).join("receipt.json")).unwrap(),
    ).unwrap();
    assert_eq!(manifest["registry"].as_array().unwrap().len(), 3);
    let receipt_root = dir.path().join("dynamic-receipts");
    let rejected = Command::new(&rock).args(["sim", "--list"]).arg(&project)
        .env("ROCK_SIM_COMPILE_RECEIPT_DIR", &receipt_root)
        .env("ROCK_SIM_COMPILE_RECEIPT_PREPARE", "1")
        .env("ROCK_SIM_COMPILE_SELECTION", "[\"dynamic_probe\"]")
        .output_with_deadline().unwrap();
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("cannot resolve dynamic SIMS access"));
    let full = Command::new(&rock).args(["sim", "--list"]).arg(&project)
        .env("ROCK_SIM_COMPILE_RECEIPT_DIR", &receipt_root)
        .env("ROCK_SIM_COMPILE_RECEIPT_PREPARE", "1")
        .env_remove("ROCK_SIM_COMPILE_SELECTION")
        .output_with_deadline().unwrap();
    assert!(full.status.success(), "{}", String::from_utf8_lossy(&full.stderr));
}

fn copy_stone_sources(source: &std::path::Path, target: &std::path::Path) {
    std::fs::create_dir_all(target).unwrap();
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        let destination = target.join(entry.file_name());
        if path.is_dir() {
            copy_stone_sources(&path, &destination);
        } else if path.extension().is_some_and(|ext| ext == "rock" || ext == "lua") || entry.file_name() == "stick.toml" {
            std::fs::copy(path, destination).unwrap();
        }
    }
}

#[test]
fn sim_compile_receipt_v1_entry_point_and_short_name_selection_run() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let dir = tempfile::tempdir().expect("tempdir");
    let rock = pinned_rock(dir.path());
    let project = dir.path().join("project");
    let cache = dir.path().join("cache");
    write_cache_fixture(&project);
    let manifest = project.join("stick.toml");
    let mut text = std::fs::read_to_string(&manifest).unwrap();
    text = text.replace("[build.vars]", &format!("stone-core = {{ path = {:?} }}\n\n[build.vars]", workspace_root().join("stone-core")));
    text.push_str("\n[language]\nmodules = \"v1\"\n");
    std::fs::write(project.join("imports.rock"),
        "pub(package) use stone::*;\npub(package) use stone_core::*;\n").unwrap();
    std::fs::write(manifest, text).unwrap();
    let run = CacheRun { rock: &rock, project: &project, cache: &cache, scratch: dir.path() };
    assert!(run.compiled("v1", "cache_probe", &[], &[]));
}

/// Pinning the rock executable while other threads spawn must not fail the
/// spawn with "Text file busy". A clone made during an in-process copy
/// inherits the copy's write fd, so the copier's close is not the last fput
/// and the kernel drops the write count only when that child drains; an exec
/// landing in that window fails with ETXTBSY. Every pinned copy must spawn
/// and report the same version as a direct run.
///
/// The race is Linux's, so Linux takes the full stress. macOS validates the
/// signature of every new executable file on its first exec (about a second
/// for the debug rock, serialized system-wide): there 384 copies cost 6-7
/// minutes of a test thread, so a small smoke keeps only the copy-and-spawn
/// contract.
#[test]
fn pinned_rock_copies_survive_concurrent_spawns() {
    let (threads, copies_per_thread) = if cfg!(target_os = "linux") { (32, 12) } else { (4, 2) };
    let _spawn_budget = SpawnBudget::for_test(120);
    let direct = Command::new(env!("CARGO_BIN_EXE_rock"))
        .arg("--version")
        .output_with_deadline()
        .expect("direct rock --version oracle");
    assert!(
        direct.status.success(),
        "direct rock --version failed:\n{}",
        String::from_utf8_lossy(&direct.stderr)
    );
    let budget = SpawnBudget::budget();
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                let _spawn_budget = SpawnBudget::enter(budget);
                for _ in 0..copies_per_thread {
                    let dir = tempfile::tempdir().expect("tempdir");
                    let rock = pinned_rock(dir.path());
                    let output = Command::new(&rock)
                        .arg("--version")
                        .output_with_deadline()
                        .expect("spawn pinned rock --version");
                    assert!(
                        output.status.success(),
                        "pinned rock --version failed:\n{}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                    assert_eq!(output.stdout, direct.stdout, "pinned copy reports its version");
                }
            });
        }
    });
}

/// One-run `tmp/sim-receipt-*` roots under `tmp`, sorted.
#[cfg(target_os = "linux")]
fn receipt_roots(tmp: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut roots: Vec<_> = std::fs::read_dir(tmp)
        .map(|read| {
            read.filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .filter(|path| {
                    path.file_name().and_then(|name| name.to_str()).is_some_and(|name| name.starts_with("sim-receipt-"))
                })
                .collect()
        })
        .unwrap_or_default();
    roots.sort();
    roots
}

#[cfg(target_os = "linux")]
fn tree_has(root: &std::path::Path, name: &str) -> bool {
    let Ok(read) = std::fs::read_dir(root) else { return false };
    for entry in read.filter_map(|entry| entry.ok()) {
        let path = entry.path();
        if path.file_name().is_some_and(|file| file == name) {
            return true;
        }
        if path.is_dir() && tree_has(&path, name) {
            return true;
        }
    }
    false
}

/// A runner SIGKILLed mid-compile leaves no orphaned compile child, and the
/// next run sweeps its receipt root instead of leaking it.
///
/// Linux-only (see the mid-run kill test). The run works in a scratch
/// directory so its `tmp/` is isolated from concurrently running suites.
#[cfg(target_os = "linux")]
#[test]
fn killed_compile_is_reaped_and_its_receipt_swept_by_the_next_run() {
    let _spawn_budget = SpawnBudget::for_test(240);
    let dir = tempfile::tempdir().expect("scratch");
    let fixture = workspace_root().join("crates/rock-cli/test-data/sim-v1-names");
    let mut command = Command::new(env!("CARGO_BIN_EXE_rock"));
    command
        .arg("sim")
        .arg(&fixture)
        .current_dir(dir.path())
        .env("ROCK_DIR", workspace_root())
        .env("ROCK_SIM_COMPILE_CACHE", "0")
        .env("ROCK_SIM_LOG_DIR", dir.path().join("history"))
        .env("ROCK_SIM_TIMEOUT_SECS", "300")
        .stdin(std::process::Stdio::null())
        .stdout(std::fs::File::create(dir.path().join("runner.out")).expect("stdout log"))
        .stderr(std::fs::File::create(dir.path().join("runner.err")).expect("stderr log"));
    let mut runner = RunnerGuard::spawn(command);

    // Wait for the compile child: a direct child whose argv runs `sim --list`.
    // Killing on first sighting has the widest margin (the child then
    // compiles for seconds), and the no-publish guard below fails loudly if
    // the window ever collapses instead of passing vacuously.
    let deadline = Instant::now() + Duration::from_secs(240);
    let compile_pid = loop {
        let found = children_of(runner.id()).into_iter().find(|pid| {
            let argv = cmdline(*pid);
            argv.iter().zip(argv.iter().skip(1)).any(|(a, b)| a == "sim" && b == "--list")
        });
        if let Some(pid) = found {
            break pid;
        }
        assert!(Instant::now() < deadline, "runner never spawned its compile child");
        std::thread::sleep(Duration::from_millis(20));
    };
    let tmp = dir.path().join("tmp");
    let stale = receipt_roots(&tmp);
    assert_eq!(stale.len(), 1, "one live receipt root");
    assert!(!tree_has(&stale[0], "receipt.json"), "the kill lands before publish, not after");
    runner.kill_runner();
    // The bound compile child dies with the runner instead of finishing: no
    // receipt may ever be published into the stale root. Liveness alone
    // cannot prove the binding (an orphan finishes compiling within seconds
    // and exits), so the publish is the signal, watched past any compile.
    let publish_deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < publish_deadline {
        assert!(
            !tree_has(&stale[0], "receipt.json"),
            "orphaned compile child {compile_pid} published after its runner died"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(stale[0].is_dir(), "the killed run skips cleanup (setup proof)");

    // The next run succeeds and sweeps the stale root; its own root is
    // removed on its clean exit, so none remain.
    let output = Command::new(env!("CARGO_BIN_EXE_rock"))
        .arg("sim")
        .arg(&fixture)
        .arg("unique_short_name")
        .current_dir(dir.path())
        .env("ROCK_DIR", workspace_root())
        .env("ROCK_SIM_COMPILE_CACHE", "0")
        .env("ROCK_SIM_LOG_DIR", dir.path().join("history"))
        .env("ROCK_SIM_TIMEOUT_SECS", "300")
        .output_with_deadline()
        .expect("rerun after kill");
    assert!(
        output.status.success(),
        "the next run must succeed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!stale[0].exists(), "the next run sweeps the stale receipt root");
    assert!(receipt_roots(&tmp).is_empty(), "no receipt roots remain");
}
