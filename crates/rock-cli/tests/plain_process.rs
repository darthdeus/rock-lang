//! The public CLI runs outside the monorepo with only a LuaJIT installation.
use crate::support::{assert_mentions, read_jsonl, rock, text, write_files};
use std::io::ErrorKind;
use std::path::Path;
use std::process::{Command, Output};

fn invoke(root: &Path, args: &[&str]) -> Output {
    rock(root)
        .args(args)
        .env("ROCK_TEST_VERBOSE", "1")
        .output()
        .expect("start public plain CLI")
}

fn write(root: &Path, name: &str, contents: impl AsRef<str>) {
    write_files(root, &[(name, contents.as_ref())]);
}

#[test]
fn plain_lsp_serve_answers_initialize_and_uses_compiler_thp_default() {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::process::Stdio;
    let root = tempfile::tempdir().unwrap();
    let child = rock(root.path())
        .args(["lsp", "serve"])
        .env_remove("MIMALLOC_ALLOW_THP")
        .env_remove("mimalloc_allow_thp")
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
        .recv_timeout(test_utils::HANG_BACKSTOP)
        .expect("server answered initialize")
        .expect("framed response");
    assert!(body.contains("\"id\":1"), "{body}");
    assert!(body.contains("\"result\""), "{body}");
    assert!(body.contains("capabilities"), "{body}");

    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string(format!("/proc/{}/status", child.0.id())).unwrap();
        let thp = status
            .lines()
            .find(|line| line.starts_with("THP_enabled:"))
            .expect("kernel reports THP_enabled");
        assert_eq!(thp.split_whitespace().nth(1), Some("0"), "{thp}");
    }
}

#[test]
fn logged_errors_fail_tests_without_interrupting_them() {
    let root = tempfile::tempdir().unwrap();
    for installed_sink in [false, true] {
        let install = if installed_sink {
            r#"inline(lua) fn install_sink() -> bool = """
                _G.__rock_log_sink = function(level, category, message)
                    if category ~= "HIDDEN" then print("[" .. category .. "] " .. message) end
                end
                return true
            """
            const sink_installed: bool = install_sink()
            "#
        } else {
            ""
        };
        write(
            root.path(),
            "logs.rock",
            format!(
                r#"{install}
            inline(lua) fn mark_continued() = """
                local f = assert(io.open("continued", "w"))
                f:write("yes")
                f:close()
            """
            @test
            fn logs_error() {{
                log_error("PROBE", "logged failure")
                mark_continued()
            }}
            @test(output = "[PROBE] warning")
            fn warning_control() {{ log_warn("PROBE", "warning") }}
            @test(output = "[PROBE] expected")
            fn expected_error_control() {{ log_once("PROBE", "expected") }}
            @test
            fn scoped_error_control() {{
                let message = "dynamic " + tostring(7)
                assert_error_logs(["[HIDDEN] " + message], () => {{
                    log_error("HIDDEN", message)
                }})
            }}
            @test
            fn scoped_extra_error() {{
                assert_error_logs(["[PROBE] expected"], () => {{
                    log_error("PROBE", "expected")
                    log_error("HIDDEN", "extra scoped error")
                }})
            }}
            @test
            fn scoped_missing_error() {{
                assert_error_logs(["[PROBE] missing"], () => {{ }})
            }}
            @test
            fn error_outside_scope() {{
                assert_error_logs(["[PROBE] expected"], () => {{ log_error("PROBE", "expected") }})
                log_error("HIDDEN", "outside scope")
            }}
            @test(output = "[PROBE] expected")
            fn unexpected_error() {{
                log_error("PROBE", "expected")
                log_error("HIDDEN", "unexpected failure")
            }}
        "#
            ),
        );
        let out = rock(root.path())
            .args(["test", "logs.rock"])
            .env("ROCK_TEST_SHARDS", "1")
            .env_remove("ROCK_TEST_VERBOSE")
            .output()
            .unwrap();
        assert!(!out.status.success(), "logged error passed: {}", text(&out));
        assert_mentions(&out, "logs_error: error:");
        assert_mentions(&out, "[PROBE] logged failure");
        assert_mentions(&out, "unexpected_error: error:");
        assert_mentions(&out, "[HIDDEN] unexpected failure");
        assert_mentions(&out, "scoped_extra_error: error:");
        assert_mentions(&out, "scoped_missing_error: error:");
        assert_mentions(&out, "error_outside_scope: error:");
        assert_mentions(&out, "3/8 @tests passed (5 failed)");
        assert_eq!(
            std::fs::read_to_string(root.path().join("continued")).unwrap(),
            "yes"
        );
        std::fs::remove_file(root.path().join("continued")).unwrap();
    }
}

#[test]
fn plain_compiler_process_without_private_runtime() {
    let root = tempfile::tempdir().unwrap();
    let source = "fn main() { print(6 * 7) }\n";
    write(root.path(), "main.rock", source);
    for command in ["check", "run"] {
        let out = invoke(root.path(), &[command, "main.rock"]);
        assert!(out.status.success(), "{command}: {}", text(&out));
        if command == "run" {
            assert_eq!(String::from_utf8_lossy(&out.stdout), "42\n");
        }
    }
    write(
        root.path(),
        "stick.toml",
        "[package]\nname = \"plain-process\"\n",
    );
    let directory = invoke(root.path(), &["run", "."]);
    assert!(
        directory.status.success(),
        "directory launcher: {}",
        text(&directory)
    );
    assert_eq!(String::from_utf8_lossy(&directory.stdout), "42\n");
    write(root.path(), "broken.rock", "fn broken( {\n");
    let rejected = invoke(root.path(), &["check", "broken.rock"]);
    assert!(
        !rejected.status.success(),
        "check accepted an invalid program"
    );
    std::fs::remove_file(root.path().join("broken.rock")).unwrap();
    write(root.path(), "main.rock", "fn   main(  ){print(6*7)}\n");
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
fn plain_main_errors_containing_interrupt_text_remain_failures() {
    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "stick.toml",
        "[package]\nname = \"main-errors\"\n",
    );
    for message in [
        "ordinary main failure",
        "operation interrupted! database write failed",
        "database operation interrupted!",
    ] {
        write(
            root.path(),
            "main.rock",
            format!("fn main() {{ error(\"{message}\") }}\n"),
        );
        let out = invoke(root.path(), &["run", "."]);
        assert!(
            !out.status.success(),
            "main error exited successfully: {message:?}: {}",
            text(&out)
        );
        assert_mentions(&out, message);
    }
}

#[test]
fn js_backend_is_served_by_the_plain_cli() {
    let root = tempfile::tempdir().unwrap();
    for command in ["test", "run"] {
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
    assert_mentions(&out, "the `js` backend cannot serve `compile`");
    assert_mentions(&out, "Supported commands: test, run");
    let help = invoke(root.path(), &["--help"]);
    assert!(help.status.success());
    assert!(
        !text(&help).contains("js (not in this build)"),
        "{}",
        text(&help)
    );
}

// A package's identity-storage migration state is its own manifest's
// declaration: the package name exempts nothing, and the declaration reaches
// both checking backends and the LSP of the plain compiler.
#[test]
fn plain_identity_storage_follows_the_manifest_declaration() {
    let root = tempfile::tempdir().unwrap();
    let manifest = |language: &str| {
        write(
            root.path(),
            "stick.toml",
            format!("[package]\nname = \"stone\"\n{language}"),
        );
    };
    let write_source = |header: &str| {
        write(
            root.path(),
            "main.rock",
            format!(
                "{header}struct Actor {{ hp: int }}\nstruct Holder {{ actor: Actor }}\nfn main() {{}}\n"
            ),
        );
    };
    let commands: [&[&str]; 3] = [
        &["check", "."],
        &["check", ".", "--backend", "skull"],
        &["lsp", "diagnostics", "."],
    ];
    let expect = |accepted: bool, what: &str| {
        for args in commands {
            let out = invoke(root.path(), args);
            let output = text(&out);
            assert_eq!(
                output.contains("cannot store identity struct"),
                !accepted,
                "{what} {args:?}: {output}"
            );
            assert!(
                !accepted || out.status.success(),
                "{what} {args:?}: {output}"
            );
        }
    };

    write_source("");
    manifest("");
    expect(false, "a package name alone exempts nothing");
    manifest("[language]\nidentity_storage = \"exempt\"\n");
    expect(true, "a package declared exempt");
    manifest("[language]\nidentity_storage = \"enforced\"\n");
    expect(false, "a package declared enforced");

    write_source("mod nav::graph;\n");
    manifest("[language]\nmodules = \"v1\"\nidentity_storage = { exempt_modules = [\"nav\"] }\n");
    expect(true, "a module under a declared exempt prefix");
    write_source("mod navigation;\n");
    expect(false, "a module that only shares the prefix's spelling");

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
        write(root.path(), "scratch.rock", source);
        let out = rock(root.path())
            .args(["test", "scratch.rock"])
            .env("SCRATCH_LOG", &log)
            .env("ROCK_TEST_VERBOSE", "1")
            .output()
            .unwrap();
        assert_eq!(out.status.success(), !fail, "{}", text(&out));
        if fail {
            assert_mentions(&out, "intentional scratch failure");
        } else {
            assert_mentions(&out, "1/1 @tests passed");
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
fn invalid_commands_and_options_report_their_exit_status_and_argument() {
    let root = tempfile::tempdir().unwrap();
    let cases: &[(&[&str], i32, &str)] = &[
        (
            &["lsp", "hole-info"],
            2,
            "unrecognized subcommand 'hole-info'",
        ),
        (&["fix", "missing.rock"], 2, "unrecognized subcommand 'fix'"),
        (
            &["run", "--timeout", "5"],
            2,
            "unexpected argument '--timeout'",
        ),
        (&["compile", "--check"], 2, "unexpected argument '--check'"),
        (
            &["run", "missing.rock", "--backend", "c"],
            2,
            "invalid value 'c'",
        ),
    ];
    for (args, code, fragment) in cases {
        let output = invoke(root.path(), args);
        assert_eq!(output.status.code(), Some(*code), "{}", text(&output));
        assert_mentions(&output, fragment);
    }
    assert!(!root.path().join("tmp").exists());
}

/// `app/` printing 42 through its path dependency `lib/`.
fn app_with_library(root: &Path) {
    write_files(
        root,
        &[
            (
                "lib/stick.toml",
                "[package]\nname = \"lib\"\ntype = \"lib\"\n[language]\nmodules = \"v1\"\n",
            ),
            ("lib/lib.rock", "pub fn answer() -> int { 42 }\n"),
            (
                "app/stick.toml",
                "[package]\nname = \"app\"\ntype = \"bin\"\n[language]\nmodules = \"v1\"\n\
                 [dependencies]\nlib = { path = \"../lib\" }\n",
            ),
            (
                "app/main.rock",
                "use lib::answer;\nfn main() { print(answer()) }\n",
            ),
        ],
    );
}

#[test]
fn run_loads_a_manifest_dependency() {
    let root = tempfile::tempdir().unwrap();
    app_with_library(root.path());
    let out = invoke(root.path(), &["run", "app"]);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "42\n");
}

#[test]
fn plain_compile_has_no_cross_process_slot_policy() {
    let root = tempfile::tempdir().unwrap();
    write(root.path(), "main.rock", "fn main() {}\n");
    write(
        root.path(),
        "stick.toml",
        r#"[package]
name = "slot-probe"
type = "bin"
"#,
    );
    let out = rock(root.path())
        .args(["compile", ".", "out.lua"])
        .env("HOME", root.path())
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
        rock_cli::lint_union::bin_projects_under(root.path()).unwrap(),
        expected
    );
    assert_eq!(
        rock_cli::lint_union::bin_projects_under(&projects[1]).unwrap(),
        vec![projects[1].clone()]
    );
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(".", root.path().join("cycle")).unwrap();
        assert_eq!(
            rock_cli::lint_union::bin_projects_under(root.path()).unwrap(),
            expected
        );
    }
}

#[test]
fn plain_writes_an_invocation_log_only_where_rock_invocation_log_names() {
    let root = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    write(root.path(), "main.rock", "fn main() {}\n");
    let run = |log: Option<&std::path::Path>| {
        let mut command = rock(root.path());
        command
            .args(["fmt", "--check", "main.rock"])
            .env("XDG_STATE_HOME", state.path())
            .env("HOME", state.path())
            .env_remove("ROCK_INVOCATION_LOG");
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
    let stone_log = state.path().join("stone.jsonl");
    let out = Command::new(env!("CARGO_BIN_EXE_rock"))
        .current_dir(root.path())
        .args(["stone", "missing-project"])
        .env("ROCK_DIR", root.path())
        .env("ROCK_INVOCATION_LOG", &stone_log)
        .env_remove("ROCK_INVOCATION_PARENT")
        .output()
        .unwrap();
    assert!(!out.status.success(), "missing project must fail");
    let records = read_jsonl(&stone_log);
    assert_eq!(records.len(), 1, "{}", text(&out));
    assert_eq!(records[0]["kind"], "stone");
    assert_ne!(records[0]["exit_code"], 0);
}

/// Publishes the built binary at `exe` without opening it for writing where a
/// hard link works, so a sibling test's forked child cannot hold a write
/// descriptor that makes exec fail with ETXTBSY (see
/// `test_utils::write_executable`). The copy fallback stages under a temporary
/// name so `exe` never holds a partial file.
fn relocate(from: &str, exe: &Path) {
    if std::fs::hard_link(from, exe).is_ok() {
        return;
    }
    let staging = exe.with_extension("staging");
    std::fs::copy(from, &staging).expect("copy the built binary");
    std::fs::rename(&staging, exe).expect("publish the relocated binary");
}

/// Starts the relocated binary, retrying while the copy fallback's write
/// descriptor is still held by a sibling test's forked child (see `relocate`).
fn start_relocated(exe: &Path, root: &Path, args: &[&str]) -> Output {
    use bone_core::prelude::{Duration, Instant};
    let deadline = Instant::now() + test_utils::HANG_BACKSTOP;
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
    write(root.path(), "main.rock", "fn main() { print(6 * 7) }\n");
    let run = |args: &[&str]| start_relocated(&exe, root.path(), args);
    let compiled = run(&["compile", "main.rock"]);
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
        let out = rock(root.path())
            .args(["fmt", "--check", "missing.rock"])
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

#[test]
fn plain_lua_line_directives_number_dev_output_and_reject_a_release_channel() {
    let root = tempfile::tempdir().unwrap();
    write_files(
        root.path(),
        &[
            ("main.rock", "fn main() {\n    print(\"hi\")\n}\n"),
            (
                "stick.toml",
                "[package]\nname = \"lines\"\ntype = \"bin\"\n",
            ),
        ],
    );
    let directives = |args: &[&str]| {
        rock(root.path())
            .args(args)
            .env("ROCK_SKULL_LINE_DIRECTIVES", "1")
            .output()
            .unwrap()
    };
    let out = directives(&["compile", ".", "numbered.lua"]);
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
    let release = directives(&["compile", ".", "out.lua", "--channel", "steam"]);
    assert!(!release.status.success(), "{}", text(&release));
    assert_mentions(&release, "Lua line directives are a development option");
}

#[test]
fn run_lets_a_program_outlive_the_test_runner_timeout() {
    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "slow.rock",
        r#"inline(lua) fn busy_wait(seconds: float) = """
    local deadline = os.clock() + seconds
    while os.clock() < deadline do end
"""
fn main() {
    busy_wait(2.5)
    print("done")
}
"#,
    );
    let out = invoke(root.path(), &["run", "slow.rock"]);
    assert!(out.status.success(), "{}", text(&out));
    assert_mentions(&out, "done");
}

#[test]
fn public_frame_and_perf_options_preserve_their_units_and_defaults() {
    let parse = |argv: &[&str]| {
        rock_cli::Args::from_matches(rock_cli::Args::cli().try_get_matches_from(argv).unwrap())
    };
    let perf = parse(&["rock", "perf", "game"]);
    assert_eq!((perf.frames, perf.warmup_frames), (None, None));
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
        "--jv",
    ]);
    assert_eq!(perf.frames, Some(7));
    assert_eq!(perf.seconds, Some(2.5));
    assert_eq!(perf.warmup_frames, Some(0));
    assert_eq!((perf.gc_window, perf.gc_window_start), (3, 2));
    assert_eq!(perf.gc_window_mode, "running");
    assert!(perf.jv);
    let test = parse(&[
        "rock",
        "test",
        "main.rock",
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
    ]);
    assert_eq!(test.iters, 2);
    assert_eq!(test.warmup, 0);
    assert_eq!(test.target_os.as_deref(), Some("web"));
    assert_eq!(test.perf_format, "json");

    let prefixed = parse(&["rock", "--format", "json", "sim", "game"]);
    assert_eq!(prefixed.perf_format, "json");
    let sim = parse(&["rock", "--jobs", "2", "sim", "game"]);
    assert_eq!(sim.jobs, Some(2));
    let pass_through = parse(&["rock", "frame", "game", "--", "--seed", "42"]);
    assert_eq!(pass_through.app_args, ["--seed", "42"]);
    let vars = parse(&[
        "rock",
        "--var",
        "A=1",
        "compile",
        "main.rock",
        "--var",
        "B=2",
    ]);
    assert_eq!(vars.build_var_overrides, ["A=1", "B=2"]);
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
    for lint in [
        "action_layer_action_callers",
        "action_layer_raw_input",
        "action_layer_raw_input_classification",
    ] {
        let option = format!("--lints={lint}");
        let missing = invoke(root.path(), &["lint", "game", &option]);
        assert!(missing.status.success(), "{lint}: {}", text(&missing));
        assert_mentions(&missing, "no action-layer policy configured");
    }
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
        assert_mentions(
            &output,
            if path == "game" {
                "production Sample action `sample::DO_MOVE` has no production caller"
            } else {
                "production Sample action `DO_MOVE` has no production caller"
            },
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
    assert_mentions(&invalid, "unknown field");
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
    write(root.path(), "main.rock", "fn main() {}\n");
    write(
        root.path(),
        "stick.toml",
        "[package]\nname = \"runner-policy\"\n",
    );
    let slots = root.path().join("slots");
    let log = root.path().join("invocations.jsonl");
    let policy = serde_json::json!({"dir": slots, "pool_mib": 1024, "full_compile_mib": 512,
        "preferred_slot": "priority-0", "try_preferred_slot": true});
    let run = |policy: &str| {
        rock(root.path())
            .args(["compile", ".", "out.lua"])
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
    assert_mentions(&output, "took compile slot priority-0");
    let env = &read_jsonl(&log)[0]["env"];
    assert_eq!(env["TASK_TAG"], "fixture");
    assert!(env.get("UNSELECTED_TAG").is_none());
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
        assert_mentions(&output, "ROCK_COMPILE_ADMISSION");
    }
    assert!(run("null").status.success());
}

/// A capture saved by an activated compile and a separate run joins offline,
/// validated against the exact generated Lua.
#[test]
fn allocation_join_reads_a_saved_activation_capture() {
    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "stick.toml",
        "[package]\nname = \"alloc-join\"\n",
    );
    write(
        root.path(),
        "main.rock",
        r#"inline(lua) fn activation_begin() = "__rock_allocation_activation_begin()"
inline(lua) fn activation_dump() = "__rock_allocation_activation_dump(1)"

fn main() {
    activation_begin()
    let items = []
    for i in 0..3 {
        items.push("item {i}")
    }
    activation_dump()
    print(items.len)
}
"#,
    );
    let activation = root.path().join("capture.tsv");
    let compiled = rock(root.path())
        .args(["compile", ".", "--output", "main.lua"])
        .env("SKULL_ALLOCATION_ACTIVATION", &activation)
        .output()
        .unwrap();
    assert!(compiled.status.success(), "compile: {}", text(&compiled));
    let ran = Command::new(rock_engine_host::runtime::luajit_path())
        .arg("main.lua")
        .current_dir(root.path())
        .env("SKULL_ALLOCATION_ACTIVATION", &activation)
        .output()
        .unwrap();
    assert!(ran.status.success(), "run: {}", text(&ran));

    let joined = invoke(
        root.path(),
        &[
            "allocation-join",
            "capture.manifest.txt",
            "capture.tsv",
            "all",
            "main.lua",
        ],
    );
    assert!(joined.status.success(), "join: {}", text(&joined));
    let report = String::from_utf8_lossy(&joined.stdout);
    assert!(report.starts_with("rock.allocation-join.v12\n"), "{report}");
    assert!(
        report
            .lines()
            .any(|row| row.starts_with("0.0000\t3\t") && row.contains("\tstring-concat\t")),
        "the pushed concat ran three times: {report}"
    );

    // A single source file writes the same manifest from `run` and `compile`.
    let single = root.path().join("single");
    std::fs::create_dir(&single).unwrap();
    std::fs::copy(root.path().join("main.rock"), single.join("main.rock")).unwrap();
    let ran = rock(&single)
        .args(["run", "main.rock"])
        .env("SKULL_ALLOCATION_ACTIVATION", single.join("run.tsv"))
        .output()
        .unwrap();
    assert!(ran.status.success(), "single-file run: {}", text(&ran));
    let joined = invoke(&single, &["allocation-join", "run.manifest.txt", "run.tsv"]);
    assert!(
        joined.status.success(),
        "single-file join: {}",
        text(&joined)
    );
    let compiled = rock(&single)
        .args(["compile", "main.rock", "--output", "main.lua"])
        .env("SKULL_ALLOCATION_ACTIVATION", single.join("compile.tsv"))
        .output()
        .unwrap();
    assert!(
        compiled.status.success(),
        "single-file compile: {}",
        text(&compiled)
    );
    assert_eq!(
        std::fs::read_to_string(single.join("compile.manifest.txt")).unwrap(),
        std::fs::read_to_string(single.join("run.manifest.txt")).unwrap()
    );
}
