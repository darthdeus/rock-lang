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
    let manifest: serde_json::Value = serde_json::from_slice(
        &std::fs::read(project_receipt.join("receipt.json")).expect("read receipt manifest"),
    )
    .expect("parse receipt manifest");
    assert_eq!(manifest["schema_version"], 3, "the cache identity changes the receipt schema");
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

#[test]
fn sim_run_compiles_in_a_child_process_and_consumes_its_receipt() {
    let dir = tempfile::tempdir().expect("tempdir");
    let timing_log = dir.path().join("compile.jsonl");
    let workspace = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("workspace root")
        .to_path_buf();
    let output = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args(["sim", "testbed", "testbed::co_join2_tuple"])
        .current_dir(&workspace)
        .env_remove("ROCK_SIM_COMPILE_RECEIPT_DIR")
        .env_remove("ROCK_SIM_COMPILE_RECEIPT_PREPARE")
        .env_remove("ROCK_SIM_COMPILE_CACHE")
        .env("ROCK_SIM_COMPILE_CACHE_DIR", dir.path().join("cache"))
        .env("ROCK_SIM_COMPILE_TIMING", &timing_log)
        .env("ROCK_SIM_LOG_DIR", dir.path().join("history"))
        .env("ROCK_SIM_TIMEOUT_SECS", "180")
        .output()
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
    let dir = tempfile::tempdir().expect("tempdir");
    let workspace = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("workspace root")
        .to_path_buf();
    for (flag, value, rejection) in [
        ("--var", "UNDECLARED_SIM_VAR=1", "--var UNDECLARED_SIM_VAR is not declared"),
        ("--channel", "undeclared-channel", "channel \"undeclared-channel\" is not declared"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_rock"))
            .args(["sim", "testbed", "testbed::co_join2_tuple", flag, value])
            .current_dir(&workspace)
            .env_remove("ROCK_SIM_COMPILE_RECEIPT_DIR")
            .env_remove("ROCK_SIM_COMPILE_RECEIPT_PREPARE")
            .env("ROCK_SIM_LOG_DIR", dir.path().join("history"))
            .env("ROCK_SIM_TIMEOUT_SECS", "180")
            .output()
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
        let output = command.output().expect("run fixture sim");
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
        .map(|entry| entry.expect("cache entry").file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    names.sort();
    names
}

#[test]
fn sim_compile_cache_reuses_identical_inputs_and_recompiles_when_any_key_input_changes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let project = dir.path().join("project");
    let cache = dir.path().join("cache");
    write_cache_fixture(&project);
    let rock = std::path::PathBuf::from(env!("CARGO_BIN_EXE_rock"));
    let run = CacheRun { rock: &rock, project: &project, cache: &cache, scratch: dir.path() };

    assert!(run.compiled("cold", "cache_probe", &[], &[]), "an empty cache compiles");
    assert!(!run.compiled("warm", "cache_probe", &[], &[]), "identical inputs reuse the cached compile");
    assert!(
        !run.compiled("other-sim", "other_probe", &[], &[]),
        "one compile serves every sim the project defines"
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
    std::fs::copy(&rock, &compiler).expect("copy rock");
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
            .output()
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
        std::fs::write(&compiler, bytes).expect("change rock copy");
    }
    assert!(changed.compiled("compiler", "cache_probe", &[], &[]), "a changed compiler recompiles");

    let entries = cache_entries(&cache);
    assert!(
        entries.iter().all(|name| !name.starts_with('.')),
        "no staging or eviction directories remain: {entries:?}"
    );
}

#[test]
fn concurrent_sim_runs_share_one_compile_cache_entry() {
    let dir = tempfile::tempdir().expect("tempdir");
    let project = dir.path().join("project");
    let cache = dir.path().join("cache");
    write_cache_fixture(&project);
    let rock = std::path::PathBuf::from(env!("CARGO_BIN_EXE_rock"));
    let run = CacheRun { rock: &rock, project: &project, cache: &cache, scratch: dir.path() };
    std::thread::scope(|scope| {
        let a = scope.spawn(|| run.compiled("concurrent-a", "cache_probe", &[], &[]));
        let b = scope.spawn(|| run.compiled("concurrent-b", "cache_probe", &[], &[]));
        a.join().expect("first run");
        b.join().expect("second run");
    });
    assert_eq!(cache_entries(&cache).len(), 1, "both runs publish one key: {:?}", cache_entries(&cache));
    assert!(!run.compiled("after", "cache_probe", &[], &[]), "the published entry serves later runs");
}

#[test]
fn sim_run_forwards_build_var_overrides_to_its_compile() {
    let dir = tempfile::tempdir().expect("tempdir");
    let project = dir.path().join("project");
    let cache = dir.path().join("cache");
    write_cache_fixture(&project);
    let rock = std::path::PathBuf::from(env!("CARGO_BIN_EXE_rock"));
    let run = CacheRun { rock: &rock, project: &project, cache: &cache, scratch: dir.path() };
    assert!(run.compiled("forwarded", "forwarded_probe", &["--var", "PROBE=forwarded"], &[]));
}
