//! The public CLI runs outside the monorepo with only a LuaJIT installation.
use std::io::ErrorKind;
use std::path::Path;
use std::process::{Command, Output};

fn invoke(root: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rock"))
        .current_dir(root)
        .args(args)
        .env("ROCK_DIR", root)
        .env("ROCK_INVOCATION_LOG", "off")
        .env("ROCK_TEST_VERBOSE", "1")
        .env_remove("ROCK_STDLIB_DIR")
        .env_remove("LUA_PATH")
        .env_remove("LUA_CPATH")
        .env_remove("ROCK_SKULL_LINE_DIRECTIVES")
        .output()
        .expect("start public plain CLI")
}

fn text(out: &Output) -> String {
    test_utils::strip_ansi_sgr(&format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    ))
}

#[test]
fn plain_lsp_serve_answers_initialize_over_stdio() {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::process::Stdio;
    use std::time::Duration;
    let root = tempfile::tempdir().unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_rock"))
        .current_dir(root.path())
        .args(["lsp", "serve"])
        .env("ROCK_DIR", root.path())
        .env("ROCK_INVOCATION_LOG", "off")
        .env_remove("ROCK_STDLIB_DIR")
        .env_remove("LUA_PATH")
        .env_remove("LUA_CPATH")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn public rock lsp serve");
    struct KillOnDrop(std::process::Child);
    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut child = KillOnDrop(child);
    let mut stdin = child.0.stdin.take().unwrap();
    let stdout = BufReader::new(child.0.stdout.take().unwrap());
    let body = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"processId\":null,\"capabilities\":{}}}";
    write!(stdin, "Content-Length: {}\r\n\r\n{body}", body.len()).unwrap();
    stdin.flush().unwrap();
    // Read the one framed response on a thread so a silent server fails as a
    // timeout instead of hanging the suite.
    let (done, response) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut stdout = stdout;
        let mut length = None;
        let mut line = String::new();
        let body = (|| {
            loop {
                line.clear();
                if stdout.read_line(&mut line).ok()? == 0 {
                    return None;
                }
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.strip_prefix("Content-Length:") {
                    length = value.trim().parse::<usize>().ok();
                }
            }
            let mut body = vec![0u8; length?];
            stdout.read_exact(&mut body).ok()?;
            String::from_utf8(body).ok()
        })();
        let _ = done.send(body);
    });
    let body = response
        .recv_timeout(Duration::from_secs(30))
        .expect("server answered initialize")
        .expect("framed response");
    assert!(body.contains("\"id\":1"), "{body}");
    assert!(body.contains("\"result\""), "{body}");
    assert!(body.contains("capabilities"), "{body}");
}

#[test]
fn plain_fix_rejects_unregistered_recipe() {
    let root = tempfile::tempdir().unwrap();
    let out = invoke(root.path(), &["fix", "--across-yield", "missing.rock"]);
    assert!(!out.status.success());
    assert!(
        text(&out).contains("unexpected argument '--across-yield'"),
        "{}",
        text(&out)
    );
    let help = invoke(root.path(), &["--help"]);
    assert!(help.status.success());
    assert!(!text(&help).contains("--across-yield"), "{}", text(&help));
    assert!(!root.path().join("tmp").exists());
}

#[test]
fn plain_named_action_layer_lints_report_no_policy() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("main.rock"), "fn main() {}\n").unwrap();
    for lint in [
        "action_layer_action_callers",
        "action_layer_raw_input",
        "action_layer_raw_input_classification",
    ] {
        let out = invoke(root.path(), &["lint", "main.rock", "--lints", lint]);
        assert!(
            text(&out).contains("no action-layer policy configured"),
            "{lint}: {}",
            text(&out)
        );
    }
}

#[test]
fn plain_compiler_process_without_private_runtime() {
    let root = tempfile::tempdir().unwrap();
    let source =
        "fn main() { print(6 * 7) }\n\n@test(output = \"42\")\nfn answer() { print(6 * 7) }\n";
    std::fs::write(root.path().join("main.rock"), source).unwrap();
    for command in ["check", "test", "run"] {
        let out = invoke(root.path(), &[command, "main.rock"]);
        assert!(out.status.success(), "{command}: {}", text(&out));
        if command == "run" {
            assert_eq!(String::from_utf8_lossy(&out.stdout), "42\n");
        }
        if command == "test" {
            assert!(text(&out).contains("1/1 @tests passed"), "{}", text(&out));
        }
    }
    std::fs::write(
        root.path().join("stick.toml"),
        "[package]\nname = \"plain-process\"\n",
    )
    .unwrap();
    let directory = invoke(root.path(), &["run", "."]);
    assert!(
        directory.status.success(),
        "directory launcher: {}",
        text(&directory)
    );
    assert_eq!(String::from_utf8_lossy(&directory.stdout), "42\n");
    std::fs::write(root.path().join("broken.rock"), "fn broken( {\n").unwrap();
    let rejected = invoke(root.path(), &["check", "broken.rock"]);
    assert!(
        !rejected.status.success(),
        "check accepted an invalid program"
    );
    std::fs::remove_file(root.path().join("broken.rock")).unwrap();
    std::fs::write(
        root.path().join("fails.rock"),
        "@test inline(lua) fn fails() = \"error('intentional test failure')\"\n",
    )
    .unwrap();
    let failed = invoke(root.path(), &["test", "fails.rock"]);
    assert!(!failed.status.success(), "test accepted a runtime error");
    assert!(
        text(&failed).contains("intentional test failure"),
        "{}",
        text(&failed)
    );
    assert!(
        !root.path().join("tmp").exists(),
        "plain test created <cwd>/tmp after failure"
    );
    std::fs::remove_file(root.path().join("fails.rock")).unwrap();
    std::fs::write(root.path().join("main.rock"), "fn   main(  ){print(6*7)}\n").unwrap();
    let unformatted = invoke(root.path(), &["fmt", "main.rock", "--check"]);
    assert!(
        !unformatted.status.success(),
        "fmt must detect unformatted input"
    );
    let formatted = invoke(root.path(), &["fmt", "main.rock"]);
    assert!(formatted.status.success(), "{}", text(&formatted));
    let checked = invoke(root.path(), &["fmt", "main.rock", "--check"]);
    assert!(checked.status.success(), "{}", text(&checked));
    assert!(!root.path().join("prebuilt").exists());
}

#[test]
fn library_resolves_game_services_without_private_registration() {
    use rock_cli::registry::{ProgramKind, RuntimeServices};
    use rock_cli::runtime_process::RuntimeProcess;
    assert!(std::ptr::eq(
        rock_cli::CliCommand::all(),
        rock_cli::plain::COMMANDS
    ));
    let services = RuntimeServices::registered();
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("stone_ffi.rock"), "// engine marker\n").unwrap();
    assert!((services.is_game_project)(root.path()).unwrap());
    assert!(
        (services.configured_runtime_dir)(root.path())
            .unwrap()
            .is_none()
    );
    assert!(
        (services.program_preamble)(".", ProgramKind::Project)
            .unwrap()
            .is_empty()
    );
    let preamble =
        (services.program_preamble)(root.path().to_str().unwrap(), ProgramKind::Game).unwrap();
    assert!(preamble.contains("__ROCK_GAME_SOURCE_DIR"));
    assert!(preamble.contains(root.path().to_str().unwrap()));
    let mut command = Command::new("unused-runtime");
    command.arg("sentinel").env("PLAIN_WITNESS", "preserved");
    let command = rock_cli::runtime_process::configure(command, RuntimeProcess::LuaTest);
    assert_eq!(
        command.get_program(),
        rock_engine_host::runtime::luajit_path().as_os_str()
    );
    assert_eq!(command.get_args().collect::<Vec<_>>(), ["sentinel"]);
    let env: std::collections::HashMap<_, _> = command.get_envs().collect();
    assert_eq!(
        env[std::ffi::OsStr::new("PLAIN_WITNESS")],
        Some(std::ffi::OsStr::new("preserved"))
    );
    assert_eq!(
        env[std::ffi::OsStr::new("STONE_HEADLESS")],
        Some(std::ffi::OsStr::new("1"))
    );
}

#[test]
fn retired_backend_is_rejected_before_source_discovery() {
    let root = tempfile::tempdir().unwrap();
    assert!(rock_cli::BackendSpec::named("c").is_none());
    for command in ["compile", "test", "run"] {
        let out = invoke(root.path(), &[command, "missing.rock", "--backend", "c"]);
        assert!(!out.status.success());
        assert!(text(&out).contains("invalid value"), "{}", text(&out));
    }
    assert!(!root.path().join("tmp").exists());
}

#[cfg(feature = "backend-js")]
#[test]
fn js_backend_is_served_by_the_plain_cli() {
    let root = tempfile::tempdir().unwrap();
    let spec = rock_cli::BackendSpec::named("js").unwrap();
    assert!(spec.omitted.is_none());
    for command in ["test", "run"] {
        let cmd = rock_cli::CliCommand::named(command).unwrap();
        assert!(
            rock_cli::Backend::Js.serves(cmd),
            "js must serve `{command}`"
        );
        let out = invoke(root.path(), &[command, "missing.rock", "--backend", "js"]);
        assert!(!out.status.success());
        assert!(
            !text(&out).contains("not included in this build"),
            "{}",
            text(&out)
        );
    }
    let out = invoke(root.path(), &["compile", "missing.rock", "--backend", "js"]);
    assert!(!out.status.success());
    assert!(
        text(&out).contains("the `js` backend cannot serve `compile`"),
        "{}",
        text(&out)
    );
    assert!(
        text(&out).contains("Supported commands: test, run"),
        "{}",
        text(&out)
    );
    let help = invoke(root.path(), &["--help"]);
    assert!(help.status.success());
    assert!(
        !text(&help).contains("js (not in this build)"),
        "{}",
        text(&help)
    );
}

#[test]
fn plain_parser_rejects_options_whose_private_rules_are_absent() {
    let root = tempfile::tempdir().unwrap();
    let help = invoke(root.path(), &["--help"]);
    assert!(help.status.success());
    let help = text(&help);
    for option in [
        "--require-test",
        "--show-passing",
        "--all-rejections",
        "--rejections-out",
    ] {
        let out = invoke(root.path(), &["test", "missing.rock", option, "sentinel"]);
        assert!(!out.status.success());
        assert!(
            text(&out).contains("unexpected argument"),
            "{option}: {}",
            text(&out)
        );
        assert!(
            !help.contains(option),
            "removed option remained in public help: {option}"
        );
    }
}

// A package's identity-storage migration state is its own manifest's
// declaration: the package name exempts nothing, and the declaration reaches
// both checking backends and the LSP of the plain compiler.
#[test]
fn plain_identity_storage_follows_the_manifest_declaration() {
    let root = tempfile::tempdir().unwrap();
    let manifest = |language: &str| {
        std::fs::write(
            root.path().join("stick.toml"),
            format!("[package]\nname = \"stone\"\n{language}"),
        )
        .unwrap();
    };
    let write_source = |header: &str| {
        std::fs::write(root.path().join("main.rock"),
            format!("{header}struct Actor {{ hp: int }}\nstruct Holder {{ actor: Actor }}\nfn main() {{}}\n")).unwrap();
    };
    let commands: [&[&str]; 3] = [
        &["check", ".", "--backend", "bone"],
        &["check", ".", "--backend", "skull"],
        &["lsp", "diagnostics", "."],
    ];
    let reports = |what: &str| {
        for args in commands {
            let out = invoke(root.path(), args);
            let output = text(&out);
            assert!(
                output.contains("cannot store identity struct"),
                "{what} {args:?}: {output}"
            );
        }
    };
    let accepts = |what: &str| {
        for args in commands {
            let out = invoke(root.path(), args);
            let output = text(&out);
            assert!(out.status.success(), "{what} {args:?}: {output}");
            assert!(
                !output.contains("cannot store identity struct"),
                "{what} {args:?}: {output}"
            );
        }
    };

    write_source("");
    manifest("");
    reports("a package name alone exempts nothing");
    manifest("[language]\nidentity_storage = \"exempt\"\n");
    accepts("a package declared exempt");
    manifest("[language]\nidentity_storage = \"enforced\"\n");
    reports("a package declared enforced");

    write_source("mod nav::graph;\n");
    manifest("[language]\nmodules = \"v1\"\nidentity_storage = { exempt_modules = [\"nav\"] }\n");
    accepts("a module under a declared exempt prefix");
    write_source("mod navigation;\n");
    reports("a module that only shares the prefix's spelling");

    std::fs::remove_file(root.path().join("stick.toml")).unwrap();
    write_source("");
    let unpackaged = invoke(root.path(), &["check", "main.rock"]);
    assert!(
        unpackaged.status.success(),
        "unpackaged program lost its exemption: {}",
        text(&unpackaged)
    );
}

#[test]
fn plain_watch_resolves_project_outside_workspace() {
    use bone_core::prelude::Instant;
    use std::process::{Child, Stdio};
    use std::time::Duration;
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
    std::fs::write(
        root.path().join("stick.toml"),
        "[package]\nname = \"plain-watch\"\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("main.rock"),
        "fn main() { print(6 * 7) }\n",
    )
    .unwrap();
    let output = root.path().join("watched.lua");
    let log = root.path().join("watch.log");
    for fail in [false, true] {
        if output.exists() {
            std::fs::remove_file(&output).unwrap();
        }
        let stream = std::fs::File::create(&log).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_rock"))
            .current_dir(root.path())
            .args(["watch", ".", "--output"])
            .arg(&output)
            .env("ROCK_DIR", root.path())
            .env("ROCK_INVOCATION_LOG", "off")
            .env("ROCK_LOG", "info")
            .env_remove("ROCK_STDLIB_DIR")
            .env_remove("ROCK_BACKEND")
            .stdout(Stdio::from(stream.try_clone().unwrap()))
            .stderr(Stdio::from(stream))
            .spawn()
            .unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let stop = StopOnDrop(&mut child);
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                let logs = std::fs::read_to_string(&log).unwrap();
                if output.is_file() && logs.contains("watching . for changes") {
                    break;
                }
                assert!(
                    stop.0.try_wait().unwrap().is_none(),
                    "watch exited before compiling: {logs}"
                );
                assert!(Instant::now() < deadline, "watch did not compile: {logs}");
                std::thread::sleep(Duration::from_millis(20));
            }
            assert!(!std::fs::read(&output).unwrap().is_empty());
            if fail {
                panic!("intentional watcher cleanup check");
            }
        }));
        if fail {
            assert_eq!(
                result.unwrap_err().downcast_ref::<&str>(),
                Some(&"intentional watcher cleanup check")
            );
        } else {
            result.unwrap();
        }
        assert!(
            child.try_wait().unwrap().is_some(),
            "watcher survived cleanup"
        );
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
        let source = format!(
            r#"@test inline(lua) fn scratch_probe() = """
local scratch = assert(os.getenv("ROCK_TEST_TMP"))
local probe = assert(io.open(scratch .. "/writable", "w"))
probe:write("scratch is writable")
probe:close()
local log = assert(io.open(os.getenv("SCRATCH_LOG"), "w"))
log:write(scratch)
log:close()
{}
"""
"#,
            if fail {
                "error('intentional scratch failure')"
            } else {
                ""
            }
        );
        std::fs::write(root.path().join("scratch.rock"), source).unwrap();
        let out = Command::new(env!("CARGO_BIN_EXE_rock"))
            .current_dir(root.path())
            .args(["test", "scratch.rock"])
            .env("ROCK_DIR", root.path())
            .env("SCRATCH_LOG", &log)
            .env("ROCK_INVOCATION_LOG", "off")
            .env("ROCK_TEST_VERBOSE", "1")
            .env_remove("ROCK_STDLIB_DIR")
            .output()
            .unwrap();
        assert_eq!(out.status.success(), !fail, "{}", text(&out));
        if fail {
            assert!(
                text(&out).contains("intentional scratch failure"),
                "{}",
                text(&out)
            );
        }
        let scratch = std::fs::read_to_string(&log).unwrap();
        assert!(
            !std::path::Path::new(&scratch).exists(),
            "scratch survived child exit: {scratch}"
        );
        assert!(
            !root.path().join("tmp").exists(),
            "plain test created <cwd>/tmp"
        );
    }
}

#[test]
fn plain_parser_rejects_private_runtime_options() {
    let root = tempfile::tempdir().unwrap();
    let help = invoke(root.path(), &["--help"]);
    assert!(help.status.success());
    let help = text(&help);
    for (option, value) in [
        ("--warmup-seconds", Some("0")),
        ("--workers", None),
        ("--isolated", None),
        ("--multi-order", None),
        ("--renderer", Some("gl")),
        ("--dump-state", None),
        ("--port", Some("8000")),
        ("--single-file", None),
        ("--itch", None),
        ("--agent-headless", None),
        ("--agent-access", None),
        ("--level", Some("catalog-entry")),
        ("--daemon-sock", Some("daemon.sock")),
    ] {
        let mut argv = vec!["check", "missing.rock", option];
        argv.extend(value);
        let out = invoke(root.path(), &argv);
        assert_eq!(out.status.code(), Some(2), "{option}: {}", text(&out));
        assert!(
            text(&out).contains(&format!("unexpected argument '{option}'")),
            "{}",
            text(&out)
        );
        assert!(
            !help.contains(option),
            "removed option remained in public help: {option}"
        );
    }
    let matches = rock_cli::Args::command()
        .try_get_matches_from([
            "rock",
            "test",
            "main.rock",
            "--watch",
            "--iters",
            "2",
            "--warmup",
            "0",
            "--channel",
            "release",
            "--target-os",
            "web",
            "--format",
            "json",
        ])
        .unwrap();
    let args = rock_cli::Args::from_matches(matches);
    assert!(args.watch);
    assert_eq!(args.iters, 2);
    assert_eq!(args.warmup, 0);
    assert_eq!(args.target_os.as_deref(), Some("web"));
    assert_eq!(args.perf_format, "json");
    assert!(!root.path().join("tmp").exists());
}

#[test]
fn plain_compile_has_no_cross_process_slot_policy() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("main.rock"), "fn main() {}\n").unwrap();
    std::fs::write(
        root.path().join("stick.toml"),
        r#"[package]
name = "slot-probe"
type = "bin"
"#,
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_rock"))
        .current_dir(root.path())
        .args(["compile", ".", "out.lua"])
        .env("HOME", root.path())
        .env("ROCK_DIR", root.path())
        .env("ROCK_INVOCATION_LOG", "off")
        .env_remove("ROCK_COMPILE_ADMISSION")
        .env_remove("ROCK_COMPILE_SLOTS")
        .env_remove("ROCK_COMPILE_SLOT_DIR")
        .env_remove("ROCK_COMPILE_CLASS")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        !root.path().join("tmp").exists(),
        "plain compile wrote a private slot policy under HOME/tmp"
    );
}

#[test]
fn plain_scan_assigns_no_archive_semantics() {
    let root = tempfile::tempdir().unwrap();
    let projects = [
        root.path().join("games/live"),
        root.path().join("games/archived/past"),
    ];
    for (index, project) in projects.iter().enumerate() {
        std::fs::create_dir_all(project).unwrap();
        std::fs::write(
            project.join("stick.toml"),
            format!("[package]\nname = \"project{index}\"\ntype = \"bin\"\n"),
        )
        .unwrap();
    }
    let mut expected = projects.to_vec();
    expected.sort();
    assert_eq!(
        rock_cli::lint_union::bin_projects_under(root.path()),
        expected
    );
    assert_eq!(
        rock_cli::lint_union::bin_projects_under(&projects[1]),
        vec![projects[1].clone()]
    );
}

fn nested_programs(depth: usize) -> [(String, String); 2] {
    let sum = vec!["1"; depth].join(" + ");
    let ifs = format!(
        "{}print(1)\n{}",
        "if true {\n".repeat(depth),
        "}\n".repeat(depth)
    );
    [
        (
            format!("sum{depth}"),
            format!("fn main() {{\n    let x = {sum}\n    print(x)\n}}\n"),
        ),
        (format!("ifs{depth}"), format!("fn main() {{\n{ifs}}}\n")),
    ]
}

#[test]
fn plain_compiles_deeply_nested_generated_code() {
    let root = tempfile::tempdir().unwrap();
    for depth in [200, 2000] {
        for (name, source) in nested_programs(depth) {
            let file = format!("{name}.rock");
            std::fs::write(root.path().join(&file), source).unwrap();
            let out = invoke(root.path(), &["compile-single", &file]);
            assert!(out.status.success(), "{name}: {}", text(&out));
            assert!(
                String::from_utf8_lossy(&out.stdout).contains("print"),
                "{name}: no Lua emitted"
            );
        }
    }
}

#[test]
fn plain_rejects_nesting_beyond_the_limit_with_a_diagnostic() {
    let root = tempfile::tempdir().unwrap();
    // The ifs go deep enough that the parser's own descent needs the guard.
    let [sum, _] = nested_programs(20_000);
    let [_, ifs] = nested_programs(100_000);
    for (name, source) in [sum, ifs] {
        let file = format!("{name}.rock");
        std::fs::write(root.path().join(&file), source).unwrap();
        let out = invoke(root.path(), &["compile-single", &file]);
        assert!(!out.status.success(), "{name} compiled");
        assert!(
            text(&out).contains("nested too deeply"),
            "{name}: {}",
            text(&out)
        );
    }
}

#[test]
fn plain_writes_an_invocation_log_only_where_rock_invocation_log_names() {
    let root = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("main.rock"), "fn main() {}\n").unwrap();
    let run = |log: Option<&std::path::Path>| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rock"));
        command
            .current_dir(root.path())
            .args(["fmt", "--check", "main.rock"])
            .env("ROCK_DIR", root.path())
            .env("XDG_STATE_HOME", state.path())
            .env("HOME", state.path())
            .env_remove("ROCK_INVOCATION_LOG")
            .env_remove("ROCK_STDLIB_DIR");
        if let Some(log) = log {
            command.env("ROCK_INVOCATION_LOG", log);
        }
        command.output().expect("start public plain CLI")
    };
    let out = run(None);
    let mut written = Vec::new();
    let mut pending = vec![state.path().to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path)
            } else {
                written.push(path)
            }
        }
    }
    assert!(
        written.is_empty(),
        "plain build logged without opting in: {written:?}\n{}",
        text(&out)
    );

    let log = state.path().join("chosen.jsonl");
    let out = run(Some(&log));
    let record = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        record.contains("\"fmt\""),
        "explicit ROCK_INVOCATION_LOG not written: <{record}>\n{}",
        text(&out)
    );
}

/// Publishes the built binary at `exe` without leaving its inode open for
/// writing, so executing `exe` cannot fail with "Text file busy".
///
/// A plain copy holds the destination open for writing until it finishes, and a
/// test thread that forks in that window passes the write fd to a child; the
/// child keeps the inode write-referenced until it execs (O_CLOEXEC closes the
/// fd only at exec), and neither closing our fd nor renaming the file clears
/// that. A hard link never opens the inode for writing, so it removes the race
/// wherever the built binary and the relocation dir share a filesystem; the copy
/// fallback stages under a temporary name so `exe` never holds a partial file.
fn relocate(from: &str, exe: &Path) {
    if std::fs::hard_link(from, exe).is_ok() {
        return;
    }
    let staging = exe.with_extension("staging");
    std::fs::copy(from, &staging).expect("copy the built binary");
    std::fs::rename(&staging, exe).expect("publish the relocated binary");
}

/// Starts the relocated binary, retrying while the copy fallback's write fd is
/// still held by a sibling test's forked child (see `relocate`). A retry spans
/// one fork-to-exec hop; the deadline keeps a genuine failure reportable.
fn start_relocated(exe: &Path, root: &Path, args: &[&str]) -> Output {
    use bone_core::prelude::{Duration, Instant};
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let started = Command::new(exe)
            .current_dir(root)
            .args(args)
            .env("ROCK_DIR", root)
            .env("ROCK_INVOCATION_LOG", "off")
            .env_remove("ROCK_STDLIB_DIR")
            .env_remove("LUA_PATH")
            .env_remove("LUA_CPATH")
            .output();
        match started {
            Err(error)
                if error.kind() == ErrorKind::ExecutableFileBusy && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(5));
            }
            other => return other.expect("start relocated rock"),
        }
    }
}

#[test]
fn plain_rock_runs_relocated_outside_any_checkout() {
    // An installed binary has no checkout and no stdlib beside it: relocate the
    // built binary into a bare temp dir (no bin/ layout, no stdlib sibling)
    // and compile + run a small program from another temp dir.
    let home = tempfile::tempdir().unwrap();
    let exe = home
        .path()
        .join(if cfg!(windows) { "rock.exe" } else { "rock" });
    relocate(env!("CARGO_BIN_EXE_rock"), &exe);
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("main.rock"),
        "fn main() { print(6 * 7) }\n",
    )
    .unwrap();
    let run = |args: &[&str]| start_relocated(&exe, root.path(), args);
    let compiled = run(&["compile-single", "main.rock"]);
    assert!(compiled.status.success(), "{}", text(&compiled));
    assert!(
        String::from_utf8_lossy(&compiled.stdout).contains("print"),
        "no Lua emitted"
    );
    let ran = run(&["run", "main.rock"]);
    assert!(ran.status.success(), "{}", text(&ran));
    assert_eq!(String::from_utf8_lossy(&ran.stdout), "42\n");
}

#[test]
fn plain_rejects_a_thread_count_that_is_not_a_count() {
    let root = tempfile::tempdir().unwrap();
    for var in ["ROCK_NUM_THREADS", "RAYON_NUM_THREADS"] {
        let out = Command::new(env!("CARGO_BIN_EXE_rock"))
            .current_dir(root.path())
            .args(["fmt", "--check", "missing.rock"])
            .env("ROCK_DIR", root.path())
            .env("ROCK_INVOCATION_LOG", "off")
            .env_remove("ROCK_NUM_THREADS")
            .env_remove("RAYON_NUM_THREADS")
            .env(var, "eight")
            .output()
            .expect("start public plain CLI");
        assert_eq!(out.status.code(), Some(2), "{var}: {}", text(&out));
        assert!(
            text(&out).contains(&format!("invalid value 'eight' for '{var}'")),
            "{}",
            text(&out)
        );
    }
}

fn line_directive_project() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("main.rock"),
        "fn main() {\n    print(\"hi\")\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("stick.toml"),
        "[package]\nname = \"lines\"\ntype = \"bin\"\n",
    )
    .unwrap();
    root
}

#[test]
fn plain_compile_numbers_lua_with_rock_lines_under_lua_line_directives() {
    let root = line_directive_project();
    let out = invoke(
        root.path(),
        &["compile", ".", "numbered.lua", "--lua-line-directives"],
    );
    assert!(out.status.success(), "{}", text(&out));
    let numbered = std::fs::read_to_string(root.path().join("numbered.lua")).unwrap();
    assert!(
        numbered
            .lines()
            .any(|line| line.starts_with("# ") && line.contains("main.rock")),
        "{numbered}"
    );
    let out = invoke(root.path(), &["compile", ".", "plain.lua"]);
    assert!(out.status.success(), "{}", text(&out));
    let plain = std::fs::read_to_string(root.path().join("plain.lua")).unwrap();
    assert!(!plain.lines().any(|line| line.starts_with('#')), "{plain}");
}

#[test]
fn plain_lua_line_directives_reject_a_release_channel() {
    let root = line_directive_project();
    let out = invoke(
        root.path(),
        &[
            "compile",
            ".",
            "out.lua",
            "--lua-line-directives",
            "--channel",
            "steam",
        ],
    );
    assert!(!out.status.success(), "{}", text(&out));
    assert!(
        text(&out).contains("Lua line directives are a development option"),
        "{}",
        text(&out)
    );
}

#[test]
fn run_lets_a_program_outlive_the_test_runner_timeout() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("slow.rock"),
        r#"inline(lua) fn busy_wait(seconds: float) = """
    local deadline = os.clock() + seconds
    while os.clock() < deadline do end
"""
fn main() {
    busy_wait(2.5)
    print("done")
}
"#,
    )
    .unwrap();
    let out = invoke(root.path(), &["run", "slow.rock"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("done"), "{}", text(&out));
}

#[test]
fn public_frame_and_perf_options_preserve_their_units_and_defaults() {
    let parse = |argv: &[&str]| {
        rock_cli::Args::from_matches(
            rock_cli::Args::command()
                .try_get_matches_from(argv)
                .unwrap(),
        )
    };
    let frame = parse(&["rock", "frame", "game"]);
    assert_eq!(frame.frames, 1);
    let perf = parse(&["rock", "perf", "game"]);
    assert_eq!(perf.frames, 1200);
    assert_eq!(perf.warmup, usize::MAX);
    let perf = parse(&[
        "rock",
        "perf",
        "game",
        "--frames",
        "7",
        "--seconds",
        "2.5",
        "--warmup-frames",
        "0",
        "--gc-window",
        "3",
        "--gc-window-start",
        "2",
        "--gc-window-mode",
        "running",
        "--headless",
        "--jv",
    ]);
    assert_eq!(perf.frames, 7);
    assert_eq!(perf.seconds, Some(2.5));
    assert_eq!(perf.warmup, 0);
    assert_eq!((perf.gc_window, perf.gc_window_start), (3, 2));
    assert_eq!(perf.gc_window_mode, "running");
    assert!(perf.headless && perf.jv);
}

#[test]
fn plain_lint_reads_project_action_rules_for_directory_and_file() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("game");
    std::fs::create_dir(&project).unwrap();
    let manifest = project.join("stick.toml");
    let header = "[package]\nname = \"sample\"\n";
    std::fs::write(&manifest, header).unwrap();
    std::fs::write(
        project.join("main.rock"),
        "global DO_MOVE: int = 1\nfn main() {}\n",
    )
    .unwrap();
    let args = ["lint", "game", "--lints=action_layer_action_callers"];
    let missing = invoke(root.path(), &args);
    assert!(missing.status.success(), "{}", text(&missing));
    assert!(text(&missing).contains("no action-layer policy configured"));
    let rules = r#"[lint.action_layer]
production_root = "game"
production_label = "Sample"
action_prefix = "DO_"
raw_input_names = []
raw_input_classes = []
registration_functions = []
"#;
    std::fs::write(&manifest, format!("{header}{rules}")).unwrap();
    for path in ["game", "game/main.rock"] {
        let output = invoke(root.path(), &["lint", path, args[2]]);
        assert!(output.status.success(), "{}", text(&output));
        assert!(
            text(&output).contains("production Sample action `DO_MOVE` has no production caller"),
            "{}",
            text(&output)
        );
    }
    let default = invoke(root.path(), &["lint", "game"]);
    assert!(default.status.success(), "{}", text(&default));
    assert!(!text(&default).contains("has no production caller"));
    let nested = project.join("nested");
    std::fs::create_dir(&nested).unwrap();
    std::fs::write(
        nested.join("main.rock"),
        "global DO_NESTED: int = 1\nfn main() {}\n",
    )
    .unwrap();
    let nested_output = invoke(&nested, &["lint", "main.rock", args[2]]);
    assert!(nested_output.status.success(), "{}", text(&nested_output));
    assert!(
        text(&nested_output)
            .contains("production Sample action `DO_NESTED` has no production caller"),
        "{}",
        text(&nested_output)
    );
    std::fs::write(&manifest, format!("{header}{rules}unknown_rule = true\n")).unwrap();
    let invalid = invoke(root.path(), &args);
    assert!(!invalid.status.success(), "{}", text(&invalid));
    assert!(
        text(&invalid).contains("unknown field"),
        "{}",
        text(&invalid)
    );
}

#[cfg(target_os = "linux")]
#[test]
fn headless_launch_uses_the_shared_scope_and_preserves_command_inputs() {
    const CHILD: &str = "ROCK_HEADLESS_PLACEMENT_WITNESS";
    if std::env::var_os(CHILD).is_none() {
        let out = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "plain_process::headless_launch_uses_the_shared_scope_and_preserves_command_inputs",
            ])
            .env(CHILD, "1")
            .env("STONE_HEADLESS", "1")
            .env("ROCK_HEADLESS_SCOPE", "1")
            .env_remove("STONE_JV")
            .env_remove("STONE_RENDERDOC")
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", text(&out));
        return;
    }
    for headless in [false, true] {
        let mut command = Command::new("unused-lua");
        command
            .arg("sentinel.lua")
            .current_dir("/")
            .env("SCOPE_INPUT", "kept");
        let command = rock_cli::runtime_process::configure(
            command,
            rock_cli::runtime_process::RuntimeProcess::LuaRun { headless },
        );
        assert_eq!(command.get_program(), "systemd-run");
        let args: Vec<_> = command.get_args().collect();
        assert!(args.contains(&std::ffi::OsStr::new("--slice=rock-headless.slice")));
        assert_eq!(args.last().unwrap(), &"sentinel.lua");
        assert_eq!(command.get_current_dir(), Some(Path::new("/")));
        assert!(command.get_envs().any(
            |(key, value)| key == "SCOPE_INPUT" && value == Some(std::ffi::OsStr::new("kept"))
        ));
    }
}

#[test]
fn explicit_runner_policy_controls_admission_and_log_context() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("main.rock"), "fn main() {}\n").unwrap();
    std::fs::write(
        root.path().join("stick.toml"),
        "[package]\nname = \"runner-policy\"\n",
    )
    .unwrap();
    let slots = root.path().join("slots");
    let log = root.path().join("invocations.jsonl");
    let policy = serde_json::json!({"dir": slots, "pool_mib": 1024, "full_compile_mib": 512,
        "preferred_slot": "priority-0", "try_preferred_slot": true});
    let run = |policy: &str| {
        Command::new(env!("CARGO_BIN_EXE_rock"))
            .current_dir(root.path())
            .args(["compile", ".", "out.lua"])
            .env("ROCK_DIR", root.path())
            .env("ROCK_COMPILE_ADMISSION", policy)
            .env("ROCK_LOG", "debug")
            .env("ROCK_LOG_COLOR", "never")
            .env("ROCK_INVOCATION_LOG", &log)
            .env("ROCK_INVOCATION_CONTEXT_KEYS", "TASK_TAG")
            .env("TASK_TAG", "fixture")
            .env("UNSELECTED_TAG", "omit")
            .output()
            .unwrap()
    };
    let output = run(&policy.to_string());
    assert!(output.status.success(), "{}", text(&output));
    assert!(
        text(&output).contains("took compile slot priority-0"),
        "{}",
        text(&output)
    );
    let record: serde_json::Value = serde_json::from_str(
        std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(record["env"]["TASK_TAG"], "fixture");
    assert!(record["env"].get("UNSELECTED_TAG").is_none());
    for invalid in [
        "{bad".to_owned(),
        {
            let mut bad = policy.clone();
            bad["full_compile_mib"] = 0.into();
            bad.to_string()
        },
        {
            let mut bad = policy.clone();
            bad["preferred_slot"] = "../outside".into();
            bad.to_string()
        },
    ] {
        let output = run(&invalid);
        assert!(!output.status.success(), "{}", text(&output));
        assert!(
            text(&output).contains("ROCK_COMPILE_ADMISSION"),
            "{}",
            text(&output)
        );
    }
    assert!(run("null").status.success());
}
