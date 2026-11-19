#[allow(dead_code)]
mod support;

use std::path::{Path, PathBuf};
use std::process::Command;
use support::{repo_root, text, write_files};

const STONE_GAME: &str = "[language]\nmodules = \"v1\"\n[dependencies]\nstone = { package = \"stone\" }\nstone-core = { package = \"stone-core\" }\n[build.vars]\nGIT_SHA = \"probe\"\nBUILD_UNIX = \"0\"\nBUILD_DATE = \"test\"\nCHANNEL = { builtin = \"channel\" }\nPLATFORM = { builtin = \"platform\" }\n";

fn write_project(dir: &Path, package: &str, manifest: &str, files: &[(&str, &str)]) -> PathBuf {
    write_files(
        dir,
        &[(
            "stick.toml",
            &format!("[package]\nname = \"{package}\"\n{manifest}"),
        )],
    );
    write_files(dir, files);
    dir.to_path_buf()
}

fn rock_in(cwd: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rock"));
    command.current_dir(cwd);
    command
}

fn skip_engine_build(cwd: &Path) -> Command {
    let mut command = rock_in(cwd);
    command.env("ROCK_SKIP_ENGINE_BUILD", "1");
    command
}

fn package_test_command(project: &Path) -> Command {
    let mut command = rock_in(project);
    command
        .arg("test")
        .arg(project)
        .env("ROCK_DIR", repo_root())
        .env("ROCK_TEST_SHARDS", "1")
        .env("STONE_HEADLESS", "1")
        .env_remove("ROCK_PREBUILT_GAME")
        .env_remove("ROCK_ENGINE_BUILD_RECEIPT");
    command
}

fn native_library_name() -> String {
    format!(
        "{}probe.{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_EXTENSION
    )
}

fn native_source(value: &str) -> String {
    format!("#[unsafe(no_mangle)] pub extern \"C\" fn native_probe() -> i32 {{ {value} }}\n")
}

/// `root/game` declares the cdylib crate at `root/native` as its native library.
fn native_game(root: &Path, package: &str, main: &str) -> (PathBuf, PathBuf) {
    let game = write_project(
        &root.join("game"),
        package,
        "[native]\nmanifest = \"../native/Cargo.toml\"\nlibrary = \"probe\"\n",
        &[("main.rock", main)],
    );
    let native = root.join("native");
    write_files(
        &native,
        &[(
            "Cargo.toml",
            "[package]\nname = \"source-name\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[lib]\npath = \"lib.rs\"\ncrate-type = [\"cdylib\"]\n[workspace]\n",
        )],
    );
    (game, native)
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// Kills the children on drop, first creating the release file they may wait on.
#[cfg(unix)]
struct Runs(Vec<std::process::Child>, Option<PathBuf>);

#[cfg(unix)]
impl Drop for Runs {
    fn drop(&mut self) {
        if let Some(release) = &self.1 {
            let _ = std::fs::write(release, "");
        }
        for child in &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(unix)]
fn wait_until(
    children: &mut [std::process::Child],
    condition: &dyn Fn() -> bool,
    diagnostic: impl Fn() -> String,
) {
    let start = bone_core::prelude::Instant::now();
    while !condition() {
        for child in children.iter_mut() {
            assert!(
                child.try_wait().unwrap().is_none(),
                "exited early: {}",
                diagnostic()
            );
        }
        assert!(
            start.elapsed() < test_utils::HANG_BACKSTOP,
            "stalled: {}",
            diagnostic()
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[test]
fn package_tests_observe_native_errors_and_scoped_mixed_records() {
    for dependency in ["stone", "stone-native"] {
        let fixture = tempfile::tempdir().unwrap();
        let source = r#"
inline(lua) fn native_error() = "ffi.cdef('void stone_gpu_scope_begin(const char* name);'); stone.stone_gpu_scope_begin(nil)"
inline(lua) fn continued() = "local f = assert(io.open('continued', 'w')); f:write('yes'); f:close()"
inline(lua) fn background_error() -> string = """
    ffi.cdef[[
        int stone_file_transaction_begin(void);
        bool stone_file_transaction_write(int transaction, const char* path, const char* data, size_t len, bool append);
        int stone_file_transaction_commit(int transaction);
        int stone_file_transaction_poll(int job, bool wait);
    ]]
    local blocker = assert(io.open("blocked-output", "w")); blocker:write("x"); blocker:close()
    local transaction = stone.stone_file_transaction_begin()
    assert(transaction > 0)
    assert(stone.stone_file_transaction_write(transaction, "blocked-output/out", "x", 1, false))
    local job = stone.stone_file_transaction_commit(transaction)
    assert(job > 0)
    assert(stone.stone_file_transaction_poll(job, true) < 0)
    for _, record in ipairs(_G.__rock_logged_errors.records()) do
        if record:find("[FILE] file transaction write failed for blocked-output/out: ", 1, true) == 1 then
            return record
        end
    end
    error("worker did not report its failed write")
"""
inline(lua) fn consume_background_error(record: string) = "assert(_G.__rock_logged_errors.take(record))"
inline(lua) fn background_scope_completed() = "local f = assert(io.open('background-continued', 'w')); f:write('yes'); f:close()"
@test
fn native_failure() {
    native_error()
    continued()
}
@test
fn scoped_mixed_control() {
    assert_error_logs(["[PROBE] expected", "[STONE] stone_gpu_scope_begin: null name"], () => {
        log_error("PROBE", "expected")
        native_error()
    })
}
@test
fn later_clean_control() { assert(true) }
@test
fn extra_native_failure() {
    assert_error_logs(["[PROBE] expected"], () => {
        log_error("PROBE", "expected")
        native_error()
    })
}
@test
fn scoped_background_control() {
    let worker_record = ""
    assert_error_logs(["[PROBE] expected"], () => {
        log_error("PROBE", "expected")
        worker_record = background_error()
    })
    consume_background_error(worker_record)
}
@test
fn unverified_background_failure() {
    assert_error_logs(["[PROBE] expected"], () => {
        log_error("PROBE", "expected")
        let _ = background_error()
    })
    background_scope_completed()
}
@test
fn native_overflow_failure() {
    for _ in 0..257 { native_error() }
}
"#;
        write_project(
            fixture.path(),
            "native-errors",
            &STONE_GAME.replace(
                "stone = { package = \"stone\" }",
                &format!("{dependency} = {{ package = \"{dependency}\" }}"),
            ),
            &[(
                "main.rock",
                &format!("use {}::*;\n{source}", dependency.replace('-', "_")),
            )],
        );
        let output = package_test_command(fixture.path())
            .env_remove("ROCK_TEST_VERBOSE")
            .output()
            .unwrap();
        let text = text(&output);
        assert!(!output.status.success(), "native errors passed: {text}");
        for expected in [
            "native_errors::native_failure: error:",
            "native_errors::extra_native_failure: error:",
            "native_errors::native_overflow_failure: error:",
            "native_errors::unverified_background_failure: error:",
            "3/7 @tests passed (4 failed)",
            "1 additional error record(s) dropped",
        ] {
            assert!(text.contains(expected), "missing {expected:?}: {text}");
        }
        assert_eq!(read(&fixture.path().join("continued")), "yes");
        assert_eq!(
            read(&fixture.path().join("background-continued")),
            "yes",
            "background errors fail the run after the synchronous scope completes"
        );
    }
}

#[test]
fn package_tests_reject_errors_outside_test_bodies() {
    for dependency in [None, Some("stone-native")] {
        let fixture = tempfile::tempdir().unwrap();
        let (manifest, import, native_error) = match dependency {
            Some(dependency) => (
                format!("[dependencies]\n{dependency} = {{ package = \"{dependency}\" }}\n"),
                "use stone_native::*;",
                r#"inline(lua) fn initializer_native_error() = "ffi.cdef('void stone_gpu_scope_begin(const char* name);'); stone.stone_gpu_scope_begin(nil)""#,
            ),
            None => (String::new(), "", "fn initializer_native_error() {}"),
        };
        let source = format!(
            r#"{import}
inline(lua) fn probe_phase() -> string = "return os.getenv(\"PROBE_PHASE\") or \"\""
fn initialize() -> bool {{
    if probe_phase() == "init" {{
        log_error("PROBE", "initialization failure")
        initializer_native_error()
    }}
    return true
}}
const READY: bool = initialize()
inline(lua) fn late_error() = """
    local guard = newproxy(true)
    getmetatable(guard).__gc = function()
        __rock_log(0, "PROBE", "finalizer failure")
    end
"""
@test
fn checked_body() {{
    assert(READY)
    if probe_phase() == "gc" {{ late_error() }}
}}
{native_error}"#
        );
        write_project(
            fixture.path(),
            "outside-errors",
            &format!("[language]\nmodules = \"v1\"\n{manifest}"),
            &[("main.rock", &source)],
        );
        for phase in ["init", "gc", "clean", "load-only"] {
            if phase == "load-only" {
                write_files(
                    fixture.path(),
                    &[("main.rock", &source.replace("@test\n", ""))],
                );
            }
            let output = package_test_command(fixture.path())
                .env("ROCK_TEST_GC_COLLECT_MB", "0.000001")
                .env(
                    "PROBE_PHASE",
                    if phase == "load-only" { "init" } else { phase },
                )
                .output()
                .unwrap();
            let text = text(&output);
            assert_eq!(
                output.status.success(),
                phase == "clean",
                "{dependency:?}/{phase}: {text}"
            );
            if phase != "clean" {
                let record = if phase != "gc" {
                    "initialization failure"
                } else {
                    "finalizer failure"
                };
                assert!(text.contains(&format!("[PROBE] {record}")), "{text}");
                assert!(
                    text.contains("logged error(s) outside test bodies"),
                    "{text}"
                );
                if phase == "init" && dependency.is_some() {
                    assert!(
                        text.contains("[STONE] stone_gpu_scope_begin: null name"),
                        "{text}"
                    );
                }
            }
        }
    }
}

#[test]
fn an_external_project_rebuilds_its_native_library_with_a_prebuilt_toolchain() {
    let fixture = tempfile::tempdir().unwrap();
    let filename = native_library_name();
    let (game, native) = native_game(
        fixture.path(),
        "external",
        &format!(
            r#"
inline(lua) fn probe() -> int = """
    local ffi = require("ffi")
    ffi.cdef("int native_probe(void);")
    return ffi.load(__native_library_dir .. "/{filename}").native_probe()
"""
fn main() {{ print(probe()) }}
"#
        ),
    );
    let installed = fixture.path().join("sdk");
    std::fs::create_dir(&installed).unwrap();
    let run = || {
        rock_in(fixture.path())
            .arg("run")
            .arg(&game)
            .env("ROCK_DIR", &installed)
            .env("ROCK_PREBUILT_GAME", "1")
            .env("STONE_HEADLESS", "1")
            .env("CARGO_TARGET_DIR", fixture.path().join("cargo-output"))
            .env_remove("ROCK_ENGINE_BUILD_RECEIPT")
            .output()
            .unwrap()
    };
    for value in ["17", "29"] {
        std::fs::write(native.join("lib.rs"), native_source(value)).unwrap();
        let output = run();
        assert!(output.status.success(), "{}", text(&output));
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.lines().any(|line| line.trim() == value), "{stdout}");
    }
    let staged = game.join("tmp/native").join(filename);
    let good = std::fs::read(&staged).unwrap();
    std::fs::write(native.join("lib.rs"), "this does not compile").unwrap();
    assert!(!run().status.success());
    assert_eq!(std::fs::read(&staged).unwrap(), good);
    assert!(
        installed.read_dir().unwrap().next().is_none(),
        "project build modified the toolchain"
    );
}

#[cfg(unix)]
#[test]
fn stone_restarts_after_native_edits_and_keeps_running_after_a_failed_build() {
    let fixture = tempfile::tempdir().unwrap();
    let filename = native_library_name();
    let (game, native) = native_game(
        fixture.path(),
        "watch-probe",
        &format!(
            r#"
inline(lua) fn run_probe() = """
    local ffi = require("ffi")
    ffi.cdef("int native_probe(void); int getpid(void); int usleep(unsigned int);")
    local lib = ffi.load(__native_library_dir .. "/{filename}")
    while true do
        local file = assert(io.open(os.getenv("NATIVE_PROBE_HEARTBEAT"), "a"))
        file:write(tonumber(ffi.C.getpid()), ":", tonumber(lib.native_probe()), "\n")
        file:close()
        ffi.C.usleep(50000)
    end
"""
fn main() {{ run_probe() }}
"#
        ),
    );
    std::fs::write(native.join("lib.rs"), native_source("17")).unwrap();
    let sdk = fixture.path().join("sdk");
    write_files(&sdk, &[("bin/rock", "installed toolchain marker")]);
    std::fs::create_dir(sdk.join("lib")).unwrap();
    let heartbeat = fixture.path().join("heartbeat");
    let log = fixture.path().join("watch.log");
    let mut watch = Runs(
        vec![
            support::log_output(
                rock_in(fixture.path())
                    .arg("stone")
                    .arg(&game)
                    .arg("--quiet")
                    .env("ROCK_DIR", &sdk)
                    .env("ROCK_PREBUILT_GAME", "1")
                    .env("STONE_HEADLESS", "1")
                    .env("NATIVE_PROBE_HEARTBEAT", &heartbeat)
                    .env("CARGO_TARGET_DIR", fixture.path().join("cargo-output"))
                    .env_remove("ROCK_ENGINE_BUILD_RECEIPT"),
                &log,
            )
            .spawn()
            .unwrap(),
        ],
        None,
    );
    let mut wait =
        |condition: &dyn Fn() -> bool| wait_until(&mut watch.0, condition, || read(&log));
    wait(&|| read(&heartbeat).contains(":17\n") && read(&log).contains("Watching for changes"));
    let before = read(&heartbeat).lines().last().unwrap().to_owned();
    std::fs::write(native.join("lib.rs"), native_source("29")).unwrap();
    wait(&|| read(&heartbeat).contains(":29\n"));
    let after = read(&heartbeat).lines().last().unwrap().to_owned();
    assert_ne!(
        before.split(':').next(),
        after.split(':').next(),
        "native rebuild must restart the process"
    );
    std::fs::write(native.join("lib.rs"), "this does not compile").unwrap();
    wait(&|| read(&log).contains("Project native rebuild failed"));
    let count = read(&heartbeat).lines().count();
    wait(&|| read(&heartbeat).lines().count() > count);
    assert_eq!(
        read(&heartbeat).lines().last().unwrap(),
        after,
        "failed build replaced the live process"
    );
    write_files(
        &fixture.path().join("support"),
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"support\"\nversion = \"0.1.0\"\n[lib]\npath = \"lib.rs\"\n",
            ),
            ("lib.rs", "invalid newly added dependency"),
        ],
    );
    std::fs::write(native.join("lib.rs"), native_source("support::value()")).unwrap();
    let manifest = native.join("Cargo.toml");
    std::fs::write(
        &manifest,
        read(&manifest) + "\n[dependencies]\nsupport = { path = \"../support\" }\n",
    )
    .unwrap();
    wait(&|| read(&log).contains("invalid newly added dependency"));
    std::fs::write(
        fixture.path().join("support/lib.rs"),
        "pub fn value() -> i32 { 41 }\n",
    )
    .unwrap();
    wait(&|| read(&heartbeat).contains(":41\n"));
    assert_eq!(read(&sdk.join("bin/rock")), "installed toolchain marker");
    assert!(sdk.join("lib").read_dir().unwrap().next().is_none());
}

#[cfg(unix)]
#[test]
fn a_stone_launch_preserves_process_and_state_on_the_first_body_edit() {
    let fixture = tempfile::tempdir().unwrap();
    let source = r#"
use stone::*;
use stone_core::*;

global ticks: int = 0
fn version() -> int { 17 }
inline(lua) fn record(ticks: int, version: int) = """
    local ffi = require("ffi")
    ffi.cdef("int getpid(void);")
    local file = assert(io.open(os.getenv("WATCH_HEARTBEAT"), "a"))
    file:write(tonumber(ffi.C.getpid()), ":", ticks, ":", version, "\n")
    file:close()
"""
@update
fn advance() {
    ticks += 1
    if ticks % 60 == 0 { record(ticks, version()) }
}
fn main() {
    stone_run_game(GameConfig { title: "watch-probe", width: 64, height: 64,
        vsync: 0 })
}
"#;
    let game = write_project(
        &fixture.path().join("game"),
        "watch-probe",
        STONE_GAME,
        &[("main.rock", source)],
    );
    let heartbeat = fixture.path().join("heartbeat");
    let log = fixture.path().join("watch.log");
    let mut watch = Runs(
        vec![
            support::log_output(
                skip_engine_build(fixture.path())
                    .arg("stone")
                    .arg(&game)
                    .arg("--quiet")
                    .env("ROCK_DIR", repo_root())
                    .env("WATCH_HEARTBEAT", &heartbeat)
                    .env("STONE_HEADLESS", "1")
                    .env_remove("FRAME_COUNT")
                    .env_remove("STONE_HOT_RELOAD"),
                &log,
            )
            .spawn()
            .unwrap(),
        ],
        None,
    );
    let mut wait =
        |condition: &dyn Fn() -> bool| wait_until(&mut watch.0, condition, || read(&log));
    wait(&|| read(&heartbeat).contains(":17\n") && read(&log).contains("Watching for changes"));
    let before = read(&heartbeat).lines().last().unwrap().to_owned();
    write_files(
        &game,
        &[(
            "main.rock",
            &source.replace("-> int { 17 }", "-> int { 29 }"),
        )],
    );
    wait(&|| read(&heartbeat).contains(":29\n"));
    let after = read(&heartbeat)
        .lines()
        .find(|line| line.ends_with(":29"))
        .unwrap()
        .to_owned();
    let fields = |line: &str| {
        line.split(':')
            .map(|n| n.parse::<u64>().unwrap())
            .collect::<Vec<_>>()
    };
    let (before, after) = (fields(&before), fields(&after));
    assert_eq!(
        before[0],
        after[0],
        "body edit restarted the process: {}",
        read(&log)
    );
    assert!(
        after[1] > before[1],
        "body edit reset live state: {}",
        read(&heartbeat)
    );
    assert!(
        read(&log).contains("body edit -> incremental.lua"),
        "{}",
        read(&log)
    );
}

#[cfg(unix)]
#[test]
fn a_stone_launch_builds_the_engine_library_it_stages() {
    let fixture = tempfile::tempdir().unwrap();
    let bin = fixture.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    test_utils::write_executable(
        &bin.join("cargo"),
        "#!/bin/sh\necho \"$*\" >> \"$CARGO_LOG\"\ncase \"$*\" in *stone-rs*) exit 1;; esac\n",
    );
    let game = write_project(
        &fixture.path().join("game"),
        "probe",
        "",
        &[("main.rock", "fn main() {}\n")],
    );
    let log = fixture.path().join("cargo.log");
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = rock_in(fixture.path())
        .arg("stone")
        .arg(&game)
        .env("PATH", path)
        .env("CARGO_LOG", &log)
        .env("CARGO_TARGET_DIR", fixture.path().join("empty-target"))
        .env("ROCK_DIR", repo_root())
        .env("STONE_HEADLESS", "1")
        .env_remove("ROCK_PREBUILT_GAME")
        .env_remove("ROCK_ENGINE_BUILD_RECEIPT")
        .output()
        .unwrap();
    let builds = read(&log);
    assert!(
        builds.lines().any(|line| line.contains("-p stone-rs")),
        "rock stone staged libstone without building it: {builds}\n{}",
        text(&output)
    );
    assert!(
        !output.status.success(),
        "the stone-rs build failure was lost"
    );
}

#[cfg(unix)]
fn wait_for_ready(root: &Path, runs: &mut Runs, names: [&str; 2]) {
    wait_until(
        &mut runs.0,
        &|| {
            names
                .iter()
                .all(|name| root.join(format!("{name}.ready")).exists())
        },
        || {
            names
                .map(|name| read(&root.join(format!("{name}.log"))))
                .join("\n")
        },
    );
}

#[cfg(unix)]
#[test]
fn concurrent_game_runs_keep_their_own_output() {
    let root = tempfile::tempdir().unwrap();
    let shared = root.path().join("tmp/main.lua");
    write_files(root.path(), &[("tmp/main.lua", "unrelated output")]);
    let names = ["first", "second"];
    let mut runs = Runs(Vec::new(), None);
    for name in names {
        let source = format!(
            r#"
inline(lua) fn wait_for_release() = """
    local ffi = require("ffi")
    ffi.cdef("int usleep(unsigned int);")
    local ready = assert(io.open("{name}.ready", "w"))
    ready:write("ready")
    ready:close()
    while true do
        local release = io.open("release", "r")
        if release then release:close(); return end
        ffi.C.usleep(10000)
    end
"""
fn main() {{ wait_for_release() }}
"#
        );
        let project = write_project(
            &root.path().join(name),
            name,
            "",
            &[("main_stone.rock", &source)],
        );
        runs.0.push(
            support::log_output(
                skip_engine_build(root.path())
                    .arg("run")
                    .arg(project)
                    .env("STONE_HEADLESS", "1"),
                &root.path().join(format!("{name}.log")),
            )
            .spawn()
            .unwrap(),
        );
    }
    wait_for_ready(root.path(), &mut runs, names);
    assert_eq!(
        read(&shared),
        "unrelated output",
        "game run overwrote shared output"
    );
    for (child, (name, other)) in runs
        .0
        .iter()
        .zip([("first", "second"), ("second", "first")])
    {
        let generated = root
            .path()
            .join("tmp/stone")
            .join(format!("{name}-{}", child.id()))
            .join("main.lua");
        let contents = std::fs::read_to_string(generated).unwrap();
        assert!(contents.contains(&format!("{name}.ready")));
        assert!(!contents.contains(&format!("{other}.ready")));
    }
    std::fs::write(root.path().join("release"), "").unwrap();
    for child in &mut runs.0 {
        assert!(child.wait().unwrap().success());
    }
}

#[test]
fn bounded_commands_run_an_external_stone_project() {
    let fixture = tempfile::tempdir().unwrap();
    let game = write_project(
        &fixture.path().join("game"),
        "bounded-probe",
        STONE_GAME,
        &[(
            "main.rock",
            r#"
use stone::*;
use stone_core::*;

@init
fn probe_init() {
    if os_getenv("BOUNDED_FAIL") == "throw" { error("bounded runtime sentinel") }
    // Production code logs and continues; the bounded run still fails.
    if os_getenv("BOUNDED_FAIL") == "log" { log_error("PROBE", "bounded runtime sentinel") }
}
fn main() {
    stone_run_game(GameConfig { title: "bounded-probe", width: 64, height: 64,
        vsync: 0, clear_color: color(0.2, 0.3, 0.4, 1.0) })
}
"#,
        )],
    );
    let screenshot = fixture.path().join("frame.png");
    let run = |command: &str, failure: Option<&str>| {
        let fail = failure.is_some();
        let output = fixture.path().join(format!("{command}-{fail}"));
        let mut cmd = rock_in(fixture.path());
        cmd.arg(command)
            .arg(&game)
            .args(["--frames", "3", "--output"])
            .arg(&output)
            .env("ROCK_DIR", repo_root())
            .env("STONE_SCREENSHOT_PATH", &screenshot)
            .env(
                "ROCK_FRAME_TIMEOUT_SECS",
                test_utils::HANG_BACKSTOP.as_secs().to_string(),
            )
            .env("BOUNDED_FAIL", failure.unwrap_or("none"))
            .env_remove("STONE_HOT_RELOAD")
            .env_remove("FRAME_COUNT");
        if command == "perf" {
            cmd.args(["--warmup-frames", "0", "--format", "json"]);
        }
        let result = cmd.output().unwrap();
        let diagnostic = text(&result);
        assert_eq!(result.status.success(), !fail, "{command}: {diagnostic}");
        assert!(
            std::fs::read_dir(&output).unwrap().all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("frame-run-")
            }),
            "{command}: invocation output leaked after fail={fail}"
        );
        if fail {
            assert!(
                diagnostic.contains("bounded runtime sentinel"),
                "{diagnostic}"
            );
            // Reported at the end: the run continued past the logged error.
            assert_eq!(
                diagnostic.contains("error record(s) logged during the bounded run"),
                failure == Some("log"),
                "{diagnostic}"
            );
        } else if command == "perf" {
            let summary: serde_json::Value = serde_json::from_slice(&result.stdout)
                .unwrap_or_else(|error| panic!("{error}: {diagnostic}"));
            assert_eq!(summary["measurement"]["completed_frames"], 3, "{summary}");
            assert_eq!(summary["scenario"]["complete"], true, "{summary}");
            assert!(output.join("perf-summary.json").is_file());
        }
    };
    run("frame", None);
    assert_eq!(
        &std::fs::read(&screenshot).unwrap()[..8],
        b"\x89PNG\r\n\x1a\n"
    );
    run("perf", None);
    run("frame", Some("log"));
    run("perf", Some("throw"));
}

#[cfg(unix)]
#[test]
fn overlapping_frames_execute_their_own_compiled_program() {
    let root = tempfile::tempdir().unwrap();
    let wrapper = root.path().join("held-luajit");
    test_utils::write_executable(
        &wrapper,
        r#"#!/bin/sh
for argument in "$@"; do
    case "$argument" in *.lua) printf '%s' "$argument" > "$LAUNCH_PATH";; esac
done
: > "$LAUNCH_READY"
for attempt in $(seq 1 12000); do
    if test -f "$LAUNCH_RELEASE"; then exec "$REAL_LUAJIT" "$@"; fi
    sleep 0.05
done
exit 97
"#,
    );
    let output = root.path().join("output");
    write_files(&output, &[("main.lua", "unrelated output")]);
    let release = root.path().join("release");
    let names = ["first", "second"];
    let mut runs = Runs(Vec::new(), Some(release.clone()));
    for name in names {
        let project = write_project(
            &root.path().join(name),
            name,
            "",
            &[(
                "main.rock",
                &format!("fn main() {{ print(\"PROGRAM_{name}\") }}\n"),
            )],
        );
        write_files(&output, &[("incremental.lua", "stale overlay")]);
        runs.0.push(
            support::log_output(
                skip_engine_build(root.path())
                    .arg("frame")
                    .arg(&project)
                    .arg("--output")
                    .arg(&output)
                    .args(["--frames", "1"])
                    .env("REAL_LUAJIT", rock_engine_host::runtime::luajit_path())
                    .env("ROCK_LUAJIT", &wrapper)
                    .env("LAUNCH_PATH", root.path().join(format!("{name}.path")))
                    .env("LAUNCH_READY", root.path().join(format!("{name}.ready")))
                    .env("LAUNCH_RELEASE", &release)
                    // The held run idles in startup until the other run is ready.
                    .env(
                        "ROCK_FRAME_TIMEOUT_SECS",
                        test_utils::HANG_BACKSTOP.as_secs().to_string(),
                    ),
                &root.path().join(format!("{name}.log")),
            )
            .spawn()
            .unwrap(),
        );
    }
    wait_for_ready(root.path(), &mut runs, names);
    std::fs::write(&release, "").unwrap();
    for (child, name) in runs.0.iter_mut().zip(names) {
        assert!(child.wait().unwrap().success());
        let log = read(&root.path().join(format!("{name}.log")));
        assert!(log.contains(&format!("PROGRAM_{name}")), "{log}");
        let lua = read(&root.path().join(format!("{name}.path")));
        assert!(
            !Path::new(&lua).exists(),
            "invocation output was not reclaimed"
        );
    }
    let diagnostic = Command::new(rock_engine_host::runtime::luajit_path())
        .arg(output.join("main.lua"))
        .output()
        .unwrap();
    assert!(diagnostic.status.success());
    let stdout = String::from_utf8_lossy(&diagnostic.stdout);
    assert!(stdout.contains("PROGRAM_first") || stdout.contains("PROGRAM_second"));
    assert_eq!(std::fs::read_dir(&output).unwrap().count(), 1);
}

/// Stands in for LuaJIT: records its PID where `$GAME_PID` names and parks.
#[cfg(any(target_os = "linux", target_os = "macos"))]
const PARKED_LUAJIT: &str = "#!/bin/sh\necho $$ > \"$GAME_PID.staging\"\nmv \"$GAME_PID.staging\" \"$GAME_PID\"\nexec sleep 600\n";

/// Spawns `rock` with the parked LuaJIT, waits for the game, kills `rock`
/// and asserts the game goes with it.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn assert_game_dies_with_rock(root: &Path, command: &mut Command, label: &str) {
    struct Game(Option<i32>);
    impl Drop for Game {
        fn drop(&mut self) {
            if let Some(pid) = self.0 {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        }
    }
    fn running(pid: i32) -> bool {
        #[cfg(target_os = "linux")]
        {
            // An orphan that died but is not yet reaped is a zombie, not a game.
            std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
                stat.rsplit_once(") ")
                    .is_some_and(|(_, rest)| !rest.starts_with('Z'))
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            unsafe { libc::kill(pid, 0) == 0 }
        }
    }
    let wrapper = root.join("parked-luajit");
    test_utils::write_executable(&wrapper, PARKED_LUAJIT);
    let pid_path = root.join(format!("{label}.pid"));
    let mut rock = Runs(
        vec![
            command
                .env("ROCK_LUAJIT", &wrapper)
                .env("GAME_PID", &pid_path)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        ],
        None,
    );
    wait_until(&mut rock.0, &|| pid_path.exists(), || {
        format!("`rock {label}` never launched the game")
    });
    let mut game = Game(Some(read(&pid_path).trim().parse().unwrap()));
    let pid = game.0.unwrap();
    rock.0[0].kill().unwrap();
    rock.0[0].wait().unwrap();
    let killed = bone_core::prelude::Instant::now();
    while running(pid) && killed.elapsed() < test_utils::HANG_BACKSTOP {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(!running(pid), "`rock {label}` left its game running");
    game.0 = None;
}

#[cfg(target_os = "linux")]
#[test]
fn bounded_runs_do_not_outlive_a_killed_rock() {
    let root = tempfile::tempdir().unwrap();
    let project = write_project(
        &root.path().join("game"),
        "parked",
        "",
        &[("main.rock", "fn main() {}\n")],
    );
    for command in ["frame", "perf"] {
        assert_game_dies_with_rock(
            root.path(),
            skip_engine_build(root.path())
                .arg(command)
                .arg(&project)
                .arg("--output")
                .arg(root.path().join(command))
                .env("ROCK_FRAME_TIMEOUT_SECS", "600"),
            command,
        );
    }
}

#[cfg(target_os = "macos")]
#[test]
fn game_run_does_not_outlive_a_killed_rock() {
    let root = tempfile::tempdir().unwrap();
    let project = game_run_fixture(root.path());
    assert_game_dies_with_rock(
        root.path(),
        skip_engine_build(root.path())
            .arg("run")
            .arg(project)
            .env("ROCK_HEADLESS_SCOPE", "0")
            .env("STONE_HEADLESS", "0"),
        "run",
    );
}

#[test]
fn sim_commands_run_and_record_an_external_stone_project() {
    let fixture = tempfile::tempdir().unwrap();
    // Parallel builds can replace the CLI, but receipts need a stable compiler image.
    let rock = fixture
        .path()
        .join(format!("rock{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(env!("CARGO_BIN_EXE_rock"), &rock).unwrap();
    let game = write_project(
        &fixture.path().join("game"),
        "sim-probe",
        &format!("{STONE_GAME}\n[sim]\nenvironment_prefixes = [\"PROBE_\"]\n"),
        &[
            (
                "main.rock",
                r#"use stone::*;
inline(lua) fn sim_exit_finalizer() = """
    local guard = newproxy(true)
    getmetatable(guard).__gc = function() __rock_log(0, "PROBE", "finalizer failure") end
"""
fn main() {
    if os_getenv("SIM_PROBE_EARLY") == "1" {
        log_error("PROBE", "startup failure")
    }
    stone_run_game(GameConfig { title: "sim-probe", width: 64, height: 64, vsync: 0 })
    if os_getenv("SIM_PROBE_TAIL") == "1" { log_error("PROBE", "tail failure") }
    if os_getenv("SIM_PROBE_FINALIZER") == "1" { sim_exit_finalizer() }
}
"#,
            ),
            (
                "tests/sims/probe.rock",
                r#"
use stone::*;
use stone_core::*;

@shutdown fn probe_shutdown() {
    if os_getenv("SIM_PROBE_LATE") == "1" {
        log_error("PROBE", "shutdown failure")
        probe_native_error()
    }
}

// The engine logs a null scope name at error level from Rust.
inline(lua) fn probe_native_error() = "stone.stone_gpu_scope_begin(nil)"

@sim
co fn portable_sim() -> Coroutine<Wait> {
    assert_error_logs(["[PROBE] intentional"], () => { log_error("PROBE", "intentional") })
    // A declared record may arrive after the declaration, frames later.
    sim_expect_error_log("[PROBE] asynchronous")
    await frame(1)
    log_error("PROBE", "asynchronous")
    // Native records reach the same list as Rock records.
    sim_expect_error_log("[STONE] stone_gpu_scope_begin: null name")
    probe_native_error()
    // Production code logs and continues; the sim still fails.
    if os_getenv("SIM_PROBE_FAIL") == "1" {
        log_error("PROBE", "recovered failure")
        sim_expect_error_log("[PROBE] never logged")
        probe_native_error()
    }
    sim_expect(true, "portable sim sentinel")
    stop_sim()
}
"#,
            ),
        ],
    );
    let sim = |args: &[&str], log_dir: &str| {
        let mut command = Command::new(&rock);
        command
            .current_dir(fixture.path())
            .arg("sim")
            .arg(&game)
            .args(args)
            .env("ROCK_DIR", repo_root())
            .env("ROCK_SIM_LOG_DIR", fixture.path().join(log_dir))
            .env_remove("STONE_HOT_RELOAD");
        command
    };
    let list = sim(&["--list"], "list").output().unwrap();
    assert!(list.status.success(), "{}", text(&list));
    assert!(
        String::from_utf8_lossy(&list.stdout).contains("portable_sim"),
        "{}",
        text(&list)
    );
    let run_args = ["portable_sim", "--jobs", "1"];
    for (phase, variable) in [
        ("startup", "SIM_PROBE_EARLY"),
        ("shutdown", "SIM_PROBE_LATE"),
        ("tail", "SIM_PROBE_TAIL"),
        ("finalizer", "SIM_PROBE_FINALIZER"),
    ] {
        let late = sim(&run_args, phase).env(variable, "1").output().unwrap();
        let late_text = text(&late);
        assert!(!late.status.success(), "{phase} errors passed: {late_text}");
        assert!(
            late_text.contains(&format!("{phase} failure")),
            "{late_text}"
        );
    }
    for fail in [false, true] {
        let history = if fail { "failed" } else { "passed" };
        let run = sim(&run_args, history)
            .env("SIM_PROBE_FAIL", if fail { "1" } else { "0" })
            .env("PROBE_VARIANT", "declared-workload")
            .env("UNRELATED_VALUE", "excluded")
            .env(
                "ROCK_SIM_TIMEOUT_SECS",
                test_utils::HANG_BACKSTOP.as_secs().to_string(),
            )
            .output()
            .unwrap();
        let text = text(&run);
        assert_eq!(run.status.success(), !fail, "{text}");
        assert!(text.contains("portable sim sentinel"), "{text}");
        for (message, expected) in [
            ("logged error: [PROBE] recovered failure", fail),
            (
                "expected error log was not logged: [PROBE] never logged",
                fail,
            ),
            ("logged error: [PROBE] asynchronous", false),
            (
                "logged error: [STONE] stone_gpu_scope_begin: null name",
                fail,
            ),
        ] {
            assert_eq!(text.contains(message), expected, "{message}: {text}");
        }
        let run_dir = std::fs::read_dir(fixture.path().join(history))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let events = support::read_jsonl(&run_dir.join("events.jsonl"));
        let ambient = &events[0]["timing_host"]["ambient"];
        assert_eq!(ambient["PROBE_VARIANT"], "declared-workload");
        assert!(ambient.get("UNRELATED_VALUE").is_none());
        let end = events.last().unwrap();
        assert_eq!(end["event"], "run_end", "{events:?}");
        assert_eq!(
            end["status"],
            if fail { "FAIL" } else { "PASS" },
            "{events:?}"
        );
    }
}

#[cfg(unix)]
fn game_run_fixture(root: &Path) -> PathBuf {
    write_project(
        &root.join("game"),
        "launch-probe",
        "",
        &[("main_stone.rock", "fn main() {}\n")],
    )
}

/// `rock run` of a minimal game through the launcher script `launcher` names.
#[cfg(unix)]
fn game_run(root: &Path, launcher: &str, script: &str) -> Command {
    let project = game_run_fixture(root);
    let launcher = root.join(launcher);
    test_utils::write_executable(&launcher, script);
    let mut command = skip_engine_build(root);
    command
        .arg("run")
        .arg(project)
        .env("ROCK_LUAJIT", launcher)
        .env("ROCK_HEADLESS_SCOPE", "0");
    command
}

#[cfg(unix)]
#[test]
fn game_run_forwards_app_arguments() {
    let root = tempfile::tempdir().unwrap();
    let arguments = root.path().join("arguments");
    let output = game_run(
        root.path(),
        "luajit-probe",
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$ARGUMENTS\"\n",
    )
    .args(["--", "first", "two words", "--flag"])
    .env("STONE_HEADLESS", "0")
    .env("STONE_JV", "0")
    .env("ARGUMENTS", &arguments)
    .output()
    .unwrap();
    assert!(output.status.success(), "{}", text(&output));
    let arguments = read(&arguments);
    let arguments: Vec<_> = arguments.lines().collect();
    assert_eq!(&arguments[1..], ["first", "two words", "--flag"]);
}

#[cfg(unix)]
#[test]
fn game_run_retries_a_scope_failure_before_execution() {
    let root = tempfile::tempdir().unwrap();
    let executions = root.path().join("executions");
    // Named `systemd-run` so the launch takes the scoped, retrying path.
    let output = game_run(
        root.path(),
        "systemd-run",
        r#"#!/bin/sh
if [ ! -e "$ATTEMPTS" ]; then
    echo failed > "$ATTEMPTS"
    exit 1
fi
: > "$ROCK_SCOPE_START_RECEIPT" || exit 125
echo executed >> "$EXECUTIONS"
"#,
    )
    .env("STONE_HEADLESS", "1")
    .env("ATTEMPTS", root.path().join("attempts"))
    .env("EXECUTIONS", &executions)
    .output()
    .unwrap();
    assert!(output.status.success(), "{}", text(&output));
    assert_eq!(read(&executions), "executed\n");
}
