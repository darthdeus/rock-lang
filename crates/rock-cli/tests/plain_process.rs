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
