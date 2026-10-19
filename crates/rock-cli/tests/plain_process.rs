//! The public CLI runs outside the monorepo with only a LuaJIT installation.
use std::process::{Command, Output};

fn invoke(root: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rock-plain"))
        .current_dir(root).args(args)
        .env("ROCK_DIR", root).env("ROCK_INVOCATION_LOG", "off").env("ROCK_TEST_VERBOSE", "1")
        .env_remove("ROCK_STDLIB_DIR").env_remove("LUA_PATH").env_remove("LUA_CPATH")
        .output().expect("start public plain CLI")
}

fn text(out: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
}

#[test]
fn plain_fix_rejects_unregistered_recipe() {
    let root = tempfile::tempdir().unwrap();
    let out = invoke(root.path(), &["fix", "--across-yield", "missing.rock"]);
    assert!(!out.status.success());
    assert!(text(&out).contains("unexpected argument '--across-yield'"), "{}", text(&out));
    let help = invoke(root.path(), &["--help"]);
    assert!(help.status.success());
    assert!(!text(&help).contains("--across-yield"), "{}", text(&help));
    assert!(!root.path().join("tmp").exists());
}

#[test]
fn plain_named_action_layer_lints_report_no_policy() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("main.rock"), "fn main() {}\n").unwrap();
    for lint in ["action_layer_action_callers", "action_layer_raw_input", "action_layer_raw_input_classification"] {
        let out = invoke(root.path(), &["lint", "main.rock", "--lints", lint]);
        assert!(text(&out).contains("no action-layer policy configured"), "{lint}: {}", text(&out));
    }
}

#[test]
fn plain_compiler_process_without_private_runtime() {
    let root = tempfile::tempdir().unwrap();
    let source = "fn main() { print(6 * 7) }\n\n@test(output = \"42\")\nfn answer() { print(6 * 7) }\n";
    std::fs::write(root.path().join("main.rock"), source).unwrap();
    for command in ["check", "test", "run"] {
        let out = invoke(root.path(), &[command, "main.rock"]);
        assert!(out.status.success(), "{command}: {}", text(&out));
        if command == "run" { assert_eq!(String::from_utf8_lossy(&out.stdout), "42\n"); }
        if command == "test" { assert!(text(&out).contains("1/1 @tests passed"), "{}", text(&out)); }
    }
    std::fs::write(root.path().join("stick.toml"), "[package]\nname = \"plain-process\"\n").unwrap();
    let directory = invoke(root.path(), &["run", "."]);
    assert!(directory.status.success(), "directory launcher: {}", text(&directory));
    assert_eq!(String::from_utf8_lossy(&directory.stdout), "42\n");
    std::fs::write(root.path().join("broken.rock"), "fn broken( {\n").unwrap();
    let rejected = invoke(root.path(), &["check", "broken.rock"]);
    assert!(!rejected.status.success(), "check accepted an invalid program");
    std::fs::remove_file(root.path().join("broken.rock")).unwrap();
    std::fs::write(root.path().join("fails.rock"), "@test inline(lua) fn fails() = \"error('intentional test failure')\"\n").unwrap();
    let failed = invoke(root.path(), &["test", "fails.rock"]);
    assert!(!failed.status.success(), "test accepted a runtime error");
    assert!(text(&failed).contains("intentional test failure"), "{}", text(&failed));
    assert!(!root.path().join("tmp").exists(), "plain test created <cwd>/tmp after failure");
    std::fs::remove_file(root.path().join("fails.rock")).unwrap();
    std::fs::write(root.path().join("main.rock"), "fn   main(  ){print(6*7)}\n").unwrap();
    let unformatted = invoke(root.path(), &["fmt", "main.rock", "--check"]);
    assert!(!unformatted.status.success(), "fmt must detect unformatted input");
    let formatted = invoke(root.path(), &["fmt", "main.rock"]);
    assert!(formatted.status.success(), "{}", text(&formatted));
    let checked = invoke(root.path(), &["fmt", "main.rock", "--check"]);
    assert!(checked.status.success(), "{}", text(&checked));
    let unavailable = invoke(root.path(), &["unregistered-runtime", "main.rock"]);
    assert!(!unavailable.status.success());
    assert!(text(&unavailable).contains("unavailable"), "{}", text(&unavailable));
    assert!(!root.path().join("prebuilt").exists());
}

#[test]
fn library_uses_plain_services_without_installation() {
    use rock_cli::registry::{RuntimeServices, ProgramKind, RuntimeProcess};
    assert!(std::ptr::eq(rock_cli::CliCommand::all(), rock_cli::plain::COMMANDS));
    let services = RuntimeServices::registered();
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("runtime_ffi.rock"), "// private marker\n").unwrap();
    assert!(!(services.is_game_project)(root.path()).unwrap());
    assert!((services.configured_runtime_dir)(root.path()).unwrap().is_none());
    assert!((services.program_preamble)(".", ProgramKind::Project).unwrap().is_empty());
    assert!((services.program_preamble)(".", ProgramKind::Game).unwrap().is_empty());
    assert!((services.build_engine_libs)(".").unwrap_err().to_string().contains("unavailable"));
    let mut command = Command::new("unused-runtime");
    command.arg("sentinel").env("PLAIN_WITNESS", "preserved");
    let command = (services.runtime_command)(command, RuntimeProcess::LuaTest);
    assert_eq!(command.get_program(), "unused-runtime");
    assert_eq!(command.get_args().collect::<Vec<_>>(), ["sentinel"]);
    assert_eq!(command.get_envs().collect::<Vec<_>>(), [(std::ffi::OsStr::new("PLAIN_WITNESS"), Some(std::ffi::OsStr::new("preserved")))]);
}

#[test]
fn private_backends_are_omitted_before_source_discovery() {
    let root = tempfile::tempdir().unwrap();
    for backend in ["c", "js"] {
        let spec = rock_cli::BackendSpec::named(backend).unwrap();
        assert!(spec.omitted.is_some());
        assert!(spec.operations.is_empty(), "plain CLI retained a private handler");
        for command in ["compile", "test", "run"] {
            let out = invoke(root.path(), &[command, "missing.rock", "--backend", backend]);
            assert!(!out.status.success());
            assert!(text(&out).contains(&format!("the `{backend}` backend is not in this build")), "{}", text(&out));
        }
    }
    let help = invoke(root.path(), &["--help"]);
    assert!(help.status.success());
    for backend in ["c", "js"] {
        assert!(text(&help).contains(&format!("{backend} (not in this build)")), "{}", text(&help));
    }
    assert!(rock_cli::CliCommand::named("mono-manifest").is_none());
    assert!(!root.path().join("tmp").exists(), "omitted backend reached source discovery");
}

#[test]
fn plain_parser_rejects_options_whose_private_rules_are_absent() {
    let root = tempfile::tempdir().unwrap();
    let help = invoke(root.path(), &["--help"]);
    assert!(help.status.success());
    let help = text(&help);
    for option in ["--require-test", "--show-passing", "--all-rejections", "--rejections-out"] {
        let out = invoke(root.path(), &["test", "missing.rock", option, "sentinel"]);
        assert!(!out.status.success());
        assert!(text(&out).contains("unexpected argument"), "{option}: {}", text(&out));
        assert!(!help.contains(option), "private option remained in public help: {option}");
    }
}

#[test]
fn plain_reports_identity_storage_in_private_package_names() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("stick.toml"), "[package]\nname = \"example-runtime\"\n").unwrap();
    std::fs::write(root.path().join("main.rock"),
        "struct Actor { hp: int }\nstruct Holder { actor: Actor }\nfn main() {}\n").unwrap();
    for backend in ["bone", "skull"] {
        let out = invoke(root.path(), &["check", ".", "--backend", backend]);
        assert!(!out.status.success(), "plain compiler exempted a private package: {}", text(&out));
        assert!(text(&out).contains("cannot store identity struct `Actor`"), "{backend}: {}", text(&out));
    }
    std::fs::remove_file(root.path().join("stick.toml")).unwrap();
    let unpackaged = invoke(root.path(), &["check", "main.rock"]);
    assert!(unpackaged.status.success(), "unpackaged program lost its exemption: {}", text(&unpackaged));
}

#[test]
fn plain_watch_resolves_project_outside_workspace() {
    use std::process::{Child, Stdio};
    use std::time::{Duration, Instant};
    struct StopOnDrop<'a>(&'a mut Child);
    impl Drop for StopOnDrop<'_> {
        fn drop(&mut self) {
            if self.0.try_wait().expect("inspect watcher").is_none() {
                self.0.kill().expect("stop watcher");
            }
            self.0.wait().expect("reap watcher");
        }
    }
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("stick.toml"), "[package]\nname = \"plain-watch\"\n").unwrap();
    std::fs::write(root.path().join("main.rock"), "fn main() { print(6 * 7) }\n").unwrap();
    let output = root.path().join("watched.lua");
    let log = root.path().join("watch.log");
    for fail in [false, true] {
        if output.exists() { std::fs::remove_file(&output).unwrap(); }
        let stream = std::fs::File::create(&log).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_rock-plain"))
            .current_dir(root.path()).args(["watch", ".", "--output"]).arg(&output)
            .env("ROCK_DIR", root.path()).env("ROCK_INVOCATION_LOG", "off").env("ROCK_LOG", "info")
            .env_remove("ROCK_STDLIB_DIR").env_remove("ROCK_BACKEND")
            .stdout(Stdio::from(stream.try_clone().unwrap())).stderr(Stdio::from(stream))
            .spawn().unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let stop = StopOnDrop(&mut child);
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                let logs = std::fs::read_to_string(&log).unwrap();
                if output.is_file() && logs.contains("watching . for changes") { break; }
                assert!(stop.0.try_wait().unwrap().is_none(), "watch exited before compiling: {logs}");
                assert!(Instant::now() < deadline, "watch did not compile: {logs}");
                std::thread::sleep(Duration::from_millis(20));
            }
            assert!(!std::fs::read(&output).unwrap().is_empty());
            if fail { panic!("intentional watcher cleanup check"); }
        }));
        if fail {
            assert_eq!(result.unwrap_err().downcast_ref::<&str>(), Some(&"intentional watcher cleanup check"));
        } else {
            result.unwrap();
        }
        assert!(child.try_wait().unwrap().is_some(), "watcher survived cleanup");
    }
    let path = root.path().to_path_buf();
    root.close().unwrap();
    assert!(!path.exists(), "watch scratch survived cleanup");
}

#[test]
fn plain_tests_use_portable_scratch_without_cwd_tmp() {
    let root = tempfile::tempdir().unwrap();
    let log = root.path().join("scratch.log");
    for fail in [false, true] {
        let source = format!(r#"@test inline(lua) fn scratch_probe() = """
local scratch = assert(os.getenv("ROCK_TEST_TMP"))
local probe = assert(io.open(scratch .. "/writable", "w"))
probe:write("scratch is writable")
probe:close()
local log = assert(io.open(os.getenv("SCRATCH_LOG"), "w"))
log:write(scratch)
log:close()
{}
"""
"#, if fail { "error('intentional scratch failure')" } else { "" });
        std::fs::write(root.path().join("scratch.rock"), source).unwrap();
        let out = Command::new(env!("CARGO_BIN_EXE_rock-plain"))
            .current_dir(root.path()).args(["test", "scratch.rock"])
            .env("ROCK_DIR", root.path()).env("SCRATCH_LOG", &log)
            .env("ROCK_INVOCATION_LOG", "off").env("ROCK_TEST_VERBOSE", "1")
            .env_remove("ROCK_STDLIB_DIR").output().unwrap();
        assert_eq!(out.status.success(), !fail, "{}", text(&out));
        if fail { assert!(text(&out).contains("intentional scratch failure"), "{}", text(&out)); }
        let scratch = std::fs::read_to_string(&log).unwrap();
        assert!(!std::path::Path::new(&scratch).exists(), "scratch survived child exit: {scratch}");
        assert!(!root.path().join("tmp").exists(), "plain test created <cwd>/tmp");
    }
}

#[test]
fn plain_parser_rejects_private_runtime_options() {
    let root = tempfile::tempdir().unwrap();
    let help = invoke(root.path(), &["--help"]);
    assert!(help.status.success());
    let help = text(&help);
    for (option, value) in [
        ("--full-asset-jobs", Some("2")), ("--frames", Some("30")),
        ("--seconds", Some("1")), ("--warmup-seconds", Some("0")),
        ("--gc-window", Some("10")), ("--gc-window-start", Some("50")),
        ("--gc-window-mode", Some("running")), ("--top", Some("10")),
        ("--sim-entry", Some("entry")), ("--allocation-census", Some("alloc.json")),
        ("--sim-log-dir", Some("history")), ("--allocation-sites", Some("sites")),
        ("--jobs", Some("2")),
        ("--workers", None), ("--isolated", None), ("--plan", None),
        ("--jobs-force", None), ("--multi-order", None), ("--headless", None),
        ("--list", None), ("--dump-state", None), ("--update-snapshots", None),
        ("--port", Some("8000")), ("--single-file", None), ("--itch", None),
        ("--jv", None), ("--agent-headless", None), ("--agent-access", None),
        ("--level", Some("catalog-entry")), ("--daemon-sock", Some("daemon.sock")),
    ] {
        let mut argv = vec!["check", "missing.rock", option];
        argv.extend(value);
        let out = invoke(root.path(), &argv);
        assert_eq!(out.status.code(), Some(2), "{option}: {}", text(&out));
        assert!(text(&out).contains(&format!("unexpected argument '{option}'")), "{}", text(&out));
        assert!(!help.contains(option), "private option remained in public help: {option}");
    }
    let matches = rock_cli::Args::command().try_get_matches_from([
        "rock", "test", "main.rock", "--watch", "--iters", "2", "--warmup", "0",
        "--channel", "release", "--target-os", "web", "--renderer", "gl", "--format", "json",
    ]).unwrap();
    let args = rock_cli::Args::from_matches(matches);
    assert!(args.watch);
    assert_eq!(args.iters, 2);
    assert_eq!(args.warmup, 0);
    assert_eq!(args.target_os.as_deref(), Some("web"));
    assert_eq!(args.renderer, Some(rock_cli::RendererBackend::Gl));
    assert_eq!(args.perf_format, "json");
    assert!(!root.path().join("tmp").exists());
}

#[test]
fn plain_compile_has_no_cross_process_slot_policy() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("main.rock"), "fn main() {}\n").unwrap();
    std::fs::write(root.path().join("stick.toml"), r#"[package]
name = "slot-probe"
type = "bin"
"#).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_rock-plain"))
        .current_dir(root.path()).args(["compile", ".", "out.lua"])
        .env("HOME", root.path()).env("ROCK_DIR", root.path())
        .env("ROCK_INVOCATION_LOG", "off")
        .env_remove("ROCK_COMPILE_SLOTS").env_remove("ROCK_COMPILE_SLOT_DIR")
        .env_remove("ROCK_COMPILE_CLASS").output().unwrap();
    assert!(out.status.success(), "{}", text(&out));
    assert!(!root.path().join("tmp").exists(), "plain compile wrote a private slot policy under HOME/tmp");
}

#[test]
fn plain_scan_assigns_no_archive_semantics() {
    let root = tempfile::tempdir().unwrap();
    let projects = [root.path().join("games/live"), root.path().join("games/archived/past")];
    for (index, project) in projects.iter().enumerate() {
        std::fs::create_dir_all(project).unwrap();
        std::fs::write(project.join("stick.toml"), format!("[package]\nname = \"project{index}\"\ntype = \"bin\"\n")).unwrap();
    }
    let mut expected = projects.to_vec();
    expected.sort();
    assert_eq!(rock_cli::lint_union::bin_projects_under(root.path()), expected);
    assert_eq!(rock_cli::lint_union::bin_projects_under(&projects[1]), vec![projects[1].clone()]);
}
