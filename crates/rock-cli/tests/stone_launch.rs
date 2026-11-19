use std::path::Path;

fn package_test_command(project: &Path) -> std::process::Command {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_rock"));
    command
        .arg("test")
        .arg(project)
        .current_dir(project)
        .env(
            "ROCK_DIR",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."),
        )
        .env("ROCK_TEST_SHARDS", "1")
        .env("STONE_HEADLESS", "1")
        .env_remove("ROCK_PREBUILT_GAME")
        .env_remove("ROCK_ENGINE_BUILD_RECEIPT");
    command
}

fn output_text(output: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[cfg(unix)]
fn write_game_project(
    root: &Path,
    directory: &str,
    package: &str,
    entry: &str,
    source: impl AsRef<[u8]>,
) -> std::path::PathBuf {
    let project = root.join(directory);
    std::fs::create_dir(&project).unwrap();
    std::fs::write(
        project.join("stick.toml"),
        format!("[package]\nname = \"{package}\"\n"),
    )
    .unwrap();
    std::fs::write(project.join(entry), source).unwrap();
    project
}

#[cfg(unix)]
struct Watch(std::process::Child);

#[cfg(unix)]
impl Drop for Watch {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(unix)]
fn wait_for_watch(
    child: &mut std::process::Child,
    condition: &dyn Fn() -> bool,
    diagnostic: impl Fn() -> String,
) {
    let start = bone_core::prelude::Instant::now();
    while !condition() {
        assert!(
            child.try_wait().unwrap().is_none(),
            "watch exited: {}",
            diagnostic()
        );
        assert!(
            start.elapsed() < test_utils::HANG_BACKSTOP,
            "watch stalled: {}",
            diagnostic()
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[cfg(unix)]
struct Runs(Vec<std::process::Child>, Option<std::path::PathBuf>);

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

#[test]
fn package_tests_observe_native_errors_and_scoped_mixed_records() {
    for dependency in ["stone", "stone-native"] {
        let fixture = tempfile::tempdir().unwrap();
        let manifest = "[package]\nname = \"native-errors\"\n[language]\nmodules = \"v1\"\n[dependencies]\nstone = { package = \"stone\" }\nstone-core = { package = \"stone-core\" }\n[build.vars]\nGIT_SHA = \"probe\"\nBUILD_UNIX = \"0\"\nBUILD_DATE = \"test\"\nCHANNEL = { builtin = \"channel\" }\nPLATFORM = { builtin = \"platform\" }\n";
        std::fs::write(
            fixture.path().join("stick.toml"),
            manifest.replace(
                "stone = { package = \"stone\" }",
                &format!("{dependency} = {{ package = \"{dependency}\" }}"),
            ),
        )
        .unwrap();
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
        std::fs::write(
            fixture.path().join("main.rock"),
            format!("use {}::*;\n{source}", dependency.replace('-', "_")),
        )
        .unwrap();
        let output = package_test_command(fixture.path())
            .env_remove("ROCK_TEST_VERBOSE")
            .output()
            .unwrap();
        let text = test_utils::strip_ansi_sgr(&output_text(&output));
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
        assert_eq!(
            std::fs::read_to_string(fixture.path().join("continued")).unwrap(),
            "yes"
        );
        assert_eq!(
            std::fs::read_to_string(fixture.path().join("background-continued")).unwrap(),
            "yes",
            "background errors fail the run after the synchronous scope completes"
        );
    }
}

#[test]
fn package_tests_reject_errors_outside_test_bodies() {
    for dependency in [None, Some("stone-native")] {
        let fixture = tempfile::tempdir().unwrap();
        let mut manifest =
            "[package]\nname = \"outside-errors\"\n[language]\nmodules = \"v1\"\n".to_string();
        if let Some(dependency) = dependency {
            manifest +=
                &format!("[dependencies]\n{dependency} = {{ package = \"{dependency}\" }}\n");
        }
        std::fs::write(fixture.path().join("stick.toml"), manifest).unwrap();
        let import = if dependency.is_some() {
            "use stone_native::*;"
        } else {
            ""
        };
        let source = r#"
inline(lua) fn probe_phase() -> string = "return os.getenv(\"PROBE_PHASE\") or \"\""
fn initialize() -> bool {
    if probe_phase() == "init" {
        log_error("PROBE", "initialization failure")
        initializer_native_error()
    }
    return true
}
const READY: bool = initialize()
inline(lua) fn late_error() = """
    local guard = newproxy(true)
    getmetatable(guard).__gc = function()
        __rock_log(0, "PROBE", "finalizer failure")
    end
"""
@test
fn checked_body() {
    assert(READY)
    if probe_phase() == "gc" { late_error() }
}
"#;
        std::fs::write(
            fixture.path().join("main.rock"),
            format!("{import}\n{source}{}", if dependency.is_some() {
                r#"inline(lua) fn initializer_native_error() = "ffi.cdef('void stone_gpu_scope_begin(const char* name);'); stone.stone_gpu_scope_begin(nil)""#
            } else { "fn initializer_native_error() {}" }),
        )
        .unwrap();
        for phase in ["init", "gc", "clean", "load-only"] {
            if phase == "load-only" {
                let path = fixture.path().join("main.rock");
                let source = std::fs::read_to_string(&path)
                    .unwrap()
                    .replace("@test\n", "");
                std::fs::write(path, source).unwrap();
            }
            let output = package_test_command(fixture.path())
                .env("ROCK_TEST_GC_COLLECT_MB", "0.000001")
                .env(
                    "PROBE_PHASE",
                    if phase == "load-only" { "init" } else { phase },
                )
                .output()
                .unwrap();
            let text = output_text(&output);
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
    let game = fixture.path().join("game");
    let native = fixture.path().join("native");
    let installed = fixture.path().join("sdk");
    for path in [&game, &native, &installed] {
        std::fs::create_dir_all(path).unwrap();
    }
    std::fs::write(game.join("stick.toml"),
        "[package]\nname = \"external\"\n[native]\nmanifest = \"../native/Cargo.toml\"\nlibrary = \"probe\"\n").unwrap();
    std::fs::write(native.join("Cargo.toml"),
        "[package]\nname = \"source-name\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[lib]\npath = \"lib.rs\"\ncrate-type = [\"cdylib\"]\n[workspace]\n").unwrap();
    let filename = format!(
        "{}probe.{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_EXTENSION
    );
    std::fs::write(
        game.join("main.rock"),
        format!(
            r#"
inline(lua) fn probe() -> int = """
    local ffi = require("ffi")
    ffi.cdef("int native_probe(void);")
    return ffi.load(__native_library_dir .. "/{filename}").native_probe()
"""
fn main() {{ print(probe()) }}
"#
        ),
    )
    .unwrap();
    let run = || {
        std::process::Command::new(env!("CARGO_BIN_EXE_rock"))
            .args(["run", game.to_str().unwrap()])
            .current_dir(fixture.path())
            .env("ROCK_DIR", &installed)
            .env("ROCK_PREBUILT_GAME", "1")
            .env("STONE_HEADLESS", "1")
            .env("CARGO_TARGET_DIR", fixture.path().join("cargo-output"))
            .env_remove("ROCK_ENGINE_BUILD_RECEIPT")
            .output()
            .unwrap()
    };
    let source = |value| {
        format!("#[unsafe(no_mangle)] pub extern \"C\" fn native_probe() -> i32 {{ {value} }}\n")
    };
    for value in [17, 29] {
        std::fs::write(native.join("lib.rs"), source(value)).unwrap();
        let output = run();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .any(|line| line.trim() == value.to_string()),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
    let staged = game.join("tmp/native").join(filename);
    let good = std::fs::read(&staged).unwrap();
    std::fs::write(native.join("lib.rs"), "this does not compile").unwrap();
    assert!(!run().status.success());
    assert_eq!(std::fs::read(&staged).unwrap(), good);
    assert!(
        Path::new(&installed).read_dir().unwrap().next().is_none(),
        "project build modified the toolchain"
    );
}

#[cfg(unix)]
#[test]
fn stone_restarts_after_native_edits_and_keeps_running_after_a_failed_build() {
    use std::process::Stdio;
    let fixture = tempfile::tempdir().unwrap();
    let game = fixture.path().join("game");
    let native = fixture.path().join("native");
    let sdk = fixture.path().join("sdk");
    for path in [&game, &native, &sdk.join("bin"), &sdk.join("lib")] {
        std::fs::create_dir_all(path).unwrap();
    }
    std::fs::write(sdk.join("bin/rock"), "installed toolchain marker").unwrap();
    std::fs::write(game.join("stick.toml"),
        "[package]\nname = \"watch-probe\"\n[native]\nmanifest = \"../native/Cargo.toml\"\nlibrary = \"probe\"\n").unwrap();
    std::fs::write(native.join("Cargo.toml"),
        "[package]\nname = \"source-name\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[lib]\npath = \"lib.rs\"\ncrate-type = [\"cdylib\"]\n[workspace]\n").unwrap();
    let source = |value: &str| {
        format!("#[unsafe(no_mangle)] pub extern \"C\" fn native_probe() -> i32 {{ {value} }}\n")
    };
    std::fs::write(native.join("lib.rs"), source("17")).unwrap();
    let filename = format!(
        "{}probe.{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_EXTENSION
    );
    std::fs::write(
        game.join("main.rock"),
        format!(
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
    )
    .unwrap();
    let heartbeat = fixture.path().join("heartbeat");
    let log = fixture.path().join("watch.log");
    let output = std::fs::File::create(&log).unwrap();
    let mut watch = Watch(
        std::process::Command::new(env!("CARGO_BIN_EXE_rock"))
            .args(["stone", game.to_str().unwrap(), "--quiet"])
            .current_dir(fixture.path())
            .env("ROCK_DIR", &sdk)
            .env("ROCK_PREBUILT_GAME", "1")
            .env("STONE_HEADLESS", "1")
            .env("NATIVE_PROBE_HEARTBEAT", &heartbeat)
            .env("CARGO_TARGET_DIR", fixture.path().join("cargo-output"))
            .env_remove("ROCK_ENGINE_BUILD_RECEIPT")
            .stdout(Stdio::from(output.try_clone().unwrap()))
            .stderr(Stdio::from(output))
            .spawn()
            .unwrap(),
    );
    let read = |path: &Path| std::fs::read_to_string(path).unwrap_or_default();
    let mut wait = |condition: &dyn Fn() -> bool| {
        wait_for_watch(&mut watch.0, condition, || read(&log));
    };
    wait(&|| read(&heartbeat).contains(":17\n") && read(&log).contains("Watching for changes"));
    let before = read(&heartbeat).lines().last().unwrap().to_owned();
    std::fs::write(native.join("lib.rs"), source("29")).unwrap();
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
    let support = fixture.path().join("support");
    std::fs::create_dir_all(&support).unwrap();
    std::fs::write(
        support.join("Cargo.toml"),
        "[package]\nname = \"support\"\nversion = \"0.1.0\"\n[lib]\npath = \"lib.rs\"\n",
    )
    .unwrap();
    std::fs::write(support.join("lib.rs"), "invalid newly added dependency").unwrap();
    std::fs::write(native.join("lib.rs"), source("support::value()")).unwrap();
    let manifest = native.join("Cargo.toml");
    let original = std::fs::read_to_string(&manifest).unwrap();
    std::fs::write(
        &manifest,
        format!("{original}\n[dependencies]\nsupport = {{ path = \"../support\" }}\n"),
    )
    .unwrap();
    wait(&|| read(&log).contains("invalid newly added dependency"));
    std::fs::write(support.join("lib.rs"), "pub fn value() -> i32 { 41 }\n").unwrap();
    wait(&|| read(&heartbeat).contains(":41\n"));
    assert_eq!(
        std::fs::read_to_string(sdk.join("bin/rock")).unwrap(),
        "installed toolchain marker"
    );
    assert!(sdk.join("lib").read_dir().unwrap().next().is_none());
}

#[cfg(unix)]
#[test]
fn a_stone_launch_preserves_process_and_state_on_the_first_body_edit() {
    use std::process::{Command, Stdio};
    let fixture = tempfile::tempdir().unwrap();
    let game = fixture.path().join("game");
    std::fs::create_dir(&game).unwrap();
    std::fs::write(game.join("stick.toml"),
        "[package]\nname = \"watch-probe\"\n[language]\nmodules = \"v1\"\n[dependencies]\nstone = { package = \"stone\" }\nstone-core = { package = \"stone-core\" }\n[build.vars]\nGIT_SHA = \"probe\"\nBUILD_UNIX = \"0\"\nBUILD_DATE = \"test\"\nCHANNEL = { builtin = \"channel\" }\nPLATFORM = { builtin = \"platform\" }\n").unwrap();
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
    let source_path = game.join("main.rock");
    std::fs::write(&source_path, source).unwrap();
    let heartbeat = fixture.path().join("heartbeat");
    let command = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rock"));
        command
            .current_dir(fixture.path())
            .env(
                "ROCK_DIR",
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."),
            )
            .env("ROCK_SKIP_ENGINE_BUILD", "1")
            .env("WATCH_HEARTBEAT", &heartbeat)
            .env("STONE_HEADLESS", "1")
            .env_remove("FRAME_COUNT")
            .env_remove("STONE_HOT_RELOAD");
        command
    };
    let log = fixture.path().join("watch.log");
    let output = std::fs::File::create(&log).unwrap();
    let mut watch = Watch(
        command()
            .arg("stone")
            .arg(&game)
            .arg("--quiet")
            .stdout(Stdio::from(output.try_clone().unwrap()))
            .stderr(Stdio::from(output))
            .spawn()
            .unwrap(),
    );
    let read = |path: &Path| std::fs::read_to_string(path).unwrap_or_default();
    let mut wait = |condition: &dyn Fn() -> bool| {
        wait_for_watch(&mut watch.0, condition, || read(&log));
    };
    wait(&|| read(&heartbeat).contains(":17\n") && read(&log).contains("Watching for changes"));
    let before = read(&heartbeat).lines().last().unwrap().to_owned();
    std::fs::write(
        &source_path,
        source.replace("-> int { 17 }", "-> int { 29 }"),
    )
    .unwrap();
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
    let before = fields(&before);
    let after = fields(&after);
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
    use std::os::unix::fs::PermissionsExt;
    let fixture = tempfile::tempdir().unwrap();
    let bin = fixture.path().join("bin");
    let game = fixture.path().join("game");
    for path in [&bin, &game] {
        std::fs::create_dir(path).unwrap();
    }
    let cargo = bin.join("cargo");
    std::fs::write(
        &cargo,
        "#!/bin/sh\necho \"$*\" >> \"$CARGO_LOG\"\ncase \"$*\" in *stone-rs*) exit 1;; esac\n",
    )
    .unwrap();
    std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(game.join("stick.toml"), "[package]\nname = \"probe\"\n").unwrap();
    std::fs::write(game.join("main.rock"), "fn main() {}\n").unwrap();
    let log = fixture.path().join("cargo.log");
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_rock"))
        .arg("stone")
        .arg(&game)
        .current_dir(fixture.path())
        .env("PATH", path)
        .env("CARGO_LOG", &log)
        .env("CARGO_TARGET_DIR", fixture.path().join("empty-target"))
        .env(
            "ROCK_DIR",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."),
        )
        .env("STONE_HEADLESS", "1")
        .env_remove("ROCK_PREBUILT_GAME")
        .env_remove("ROCK_ENGINE_BUILD_RECEIPT")
        .output()
        .unwrap();
    let builds = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        builds.lines().any(|line| line.contains("-p stone-rs")),
        "rock stone staged libstone without building it: {builds}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !output.status.success(),
        "the stone-rs build failure was lost"
    );
}

#[cfg(unix)]
#[test]
fn concurrent_game_runs_keep_their_own_output() {
    use std::process::{Command, Stdio};
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("tmp")).unwrap();
    let shared = root.path().join("tmp/main.lua");
    std::fs::write(&shared, "unrelated output").unwrap();
    let mut runs = Runs(Vec::new(), None);
    for name in ["first", "second"] {
        let project = write_game_project(
            root.path(),
            name,
            name,
            "main_stone.rock",
            format!(
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
            ),
        );
        let log = std::fs::File::create(root.path().join(format!("{name}.log"))).unwrap();
        runs.0.push(
            Command::new(env!("CARGO_BIN_EXE_rock"))
                .args(["run", project.to_str().unwrap()])
                .current_dir(root.path())
                .env("ROCK_SKIP_ENGINE_BUILD", "1")
                .env("STONE_HEADLESS", "1")
                .stdout(Stdio::from(log.try_clone().unwrap()))
                .stderr(Stdio::from(log))
                .spawn()
                .unwrap(),
        );
    }
    let start = bone_core::prelude::Instant::now();
    while !["first", "second"]
        .iter()
        .all(|name| root.path().join(format!("{name}.ready")).exists())
    {
        for (index, child) in runs.0.iter_mut().enumerate() {
            let name = ["first", "second"][index];
            assert!(
                child.try_wait().unwrap().is_none(),
                "{}",
                std::fs::read_to_string(root.path().join(format!("{name}.log"))).unwrap()
            );
        }
        assert!(
            start.elapsed() < test_utils::HANG_BACKSTOP,
            "runs did not become ready"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(
        std::fs::read_to_string(&shared).unwrap() == "unrelated output",
        "game run overwrote shared output"
    );
    for (index, child) in runs.0.iter().enumerate() {
        let name = ["first", "second"][index];
        let generated = root
            .path()
            .join("tmp/stone")
            .join(format!("{name}-{}", child.id()))
            .join("main.lua");
        let contents = std::fs::read_to_string(generated).unwrap();
        assert!(contents.contains(&format!("{name}.ready")));
        assert!(!contents.contains(&format!("{}.ready", ["second", "first"][index])));
    }
    std::fs::write(root.path().join("release"), "").unwrap();
    for child in &mut runs.0 {
        assert!(child.wait().unwrap().success());
    }
}

#[test]
fn bounded_commands_run_an_external_stone_project() {
    let fixture = tempfile::tempdir().unwrap();
    let game = fixture.path().join("game");
    std::fs::create_dir(&game).unwrap();
    std::fs::write(
        game.join("stick.toml"),
        "[package]\nname = \"bounded-probe\"\n[language]\nmodules = \"v1\"\n[dependencies]\nstone = { package = \"stone\" }\nstone-core = { package = \"stone-core\" }\n[build.vars]\nGIT_SHA = \"probe\"\nBUILD_UNIX = \"0\"\nBUILD_DATE = \"test\"\nCHANNEL = { builtin = \"channel\" }\nPLATFORM = { builtin = \"platform\" }\n",
    )
    .unwrap();
    std::fs::write(
        game.join("main.rock"),
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
    )
    .unwrap();
    let screenshot = fixture.path().join("frame.png");
    let run = |command: &str, failure: Option<&str>| {
        let fail = failure.is_some();
        let output = fixture.path().join(format!("{command}-{fail}"));
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_rock"));
        cmd.args([command, game.to_str().unwrap(), "--frames", "3", "--output"])
            .arg(&output)
            .current_dir(fixture.path())
            .env(
                "ROCK_DIR",
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."),
            )
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
        let diagnostic = output_text(&result);
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
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Command, Stdio};
    let root = tempfile::tempdir().unwrap();
    let wrapper = root.path().join("held-luajit");
    std::fs::write(
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
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let output = root.path().join("output");
    std::fs::create_dir(&output).unwrap();
    std::fs::write(output.join("main.lua"), "unrelated output").unwrap();
    let release = root.path().join("release");
    let mut runs = Runs(Vec::new(), Some(release.clone()));
    for name in ["first", "second"] {
        let project = write_game_project(
            root.path(),
            name,
            name,
            "main.rock",
            format!("fn main() {{ print(\"PROGRAM_{name}\") }}\n"),
        );
        std::fs::write(output.join("incremental.lua"), "stale overlay").unwrap();
        let log = std::fs::File::create(root.path().join(format!("{name}.log"))).unwrap();
        runs.0.push(
            Command::new(env!("CARGO_BIN_EXE_rock"))
                .arg("frame")
                .arg(&project)
                .arg("--output")
                .arg(&output)
                .args(["--frames", "1"])
                .current_dir(root.path())
                .env("ROCK_SKIP_ENGINE_BUILD", "1")
                .env("REAL_LUAJIT", rock_engine_host::runtime::luajit_path())
                .env("ROCK_LUAJIT", &wrapper)
                .env("LAUNCH_PATH", root.path().join(format!("{name}.path")))
                .env("LAUNCH_READY", root.path().join(format!("{name}.ready")))
                .env("LAUNCH_RELEASE", &release)
                // The held run idles in startup until the other run is ready.
                .env(
                    "ROCK_FRAME_TIMEOUT_SECS",
                    test_utils::HANG_BACKSTOP.as_secs().to_string(),
                )
                .stdout(Stdio::from(log.try_clone().unwrap()))
                .stderr(Stdio::from(log))
                .spawn()
                .unwrap(),
        );
    }
    let start = bone_core::prelude::Instant::now();
    while !["first", "second"]
        .iter()
        .all(|name| root.path().join(format!("{name}.ready")).exists())
    {
        for (index, child) in runs.0.iter_mut().enumerate() {
            assert!(
                child.try_wait().unwrap().is_none(),
                "{}",
                std::fs::read_to_string(
                    root.path()
                        .join(format!("{}.log", ["first", "second"][index]))
                )
                .unwrap()
            );
        }
        assert!(
            start.elapsed() < test_utils::HANG_BACKSTOP,
            "launch barrier timed out"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    std::fs::write(&release, "").unwrap();
    for (index, child) in runs.0.iter_mut().enumerate() {
        let name = ["first", "second"][index];
        assert!(child.wait().unwrap().success());
        let log = std::fs::read_to_string(root.path().join(format!("{name}.log"))).unwrap();
        assert!(log.contains(&format!("PROGRAM_{name}")), "{log}");
        let lua = std::fs::read_to_string(root.path().join(format!("{name}.path"))).unwrap();
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

#[cfg(target_os = "linux")]
#[test]
fn bounded_runs_do_not_outlive_a_killed_rock() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Command, Stdio};
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
        // An orphan that died but is not yet reaped is a zombie, not a game.
        std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
            stat.rsplit_once(") ")
                .is_some_and(|(_, rest)| !rest.starts_with('Z'))
        })
    }
    let root = tempfile::tempdir().unwrap();
    let wrapper = root.path().join("parked-luajit");
    std::fs::write(
        &wrapper,
        "#!/bin/sh\necho $$ > \"$GAME_PID.staging\"\nmv \"$GAME_PID.staging\" \"$GAME_PID\"\nexec sleep 600\n",
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let project = write_game_project(root.path(), "game", "parked", "main.rock", "fn main() {}\n");
    for command in ["frame", "perf"] {
        let pid_path = root.path().join(format!("{command}.pid"));
        let mut rock = Command::new(env!("CARGO_BIN_EXE_rock"))
            .arg(command)
            .arg(&project)
            .arg("--output")
            .arg(root.path().join(command))
            .current_dir(root.path())
            .env("ROCK_SKIP_ENGINE_BUILD", "1")
            .env("ROCK_LUAJIT", &wrapper)
            .env("ROCK_FRAME_TIMEOUT_SECS", "600")
            .env("GAME_PID", &pid_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let start = bone_core::prelude::Instant::now();
        let pid = loop {
            if let Ok(pid) = std::fs::read_to_string(&pid_path) {
                break pid.trim().parse::<i32>().unwrap();
            }
            if let Some(status) = rock.try_wait().unwrap() {
                panic!("{command} exited before launching the game: {status}");
            }
            assert!(
                start.elapsed() < test_utils::HANG_BACKSTOP,
                "{command} never launched the game"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        let _game = Game(Some(pid));
        rock.kill().unwrap();
        rock.wait().unwrap();
        let killed = bone_core::prelude::Instant::now();
        while running(pid) && killed.elapsed() < test_utils::HANG_BACKSTOP {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(!running(pid), "`rock {command}` left its game running");
    }
}

#[test]
fn sim_commands_run_and_record_an_external_stone_project() {
    let fixture = tempfile::tempdir().unwrap();
    // Parallel builds can replace the CLI, but receipts need a stable compiler image.
    let rock = fixture
        .path()
        .join(format!("rock{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(env!("CARGO_BIN_EXE_rock"), &rock).unwrap();
    let game = fixture.path().join("game");
    std::fs::create_dir_all(game.join("tests/sims")).unwrap();
    std::fs::write(game.join("stick.toml"),
        "[package]\nname = \"sim-probe\"\n[language]\nmodules = \"v1\"\n[dependencies]\nstone = { package = \"stone\" }\nstone-core = { package = \"stone-core\" }\n[build.vars]\nGIT_SHA = \"probe\"\nBUILD_UNIX = \"0\"\nBUILD_DATE = \"test\"\nCHANNEL = { builtin = \"channel\" }\nPLATFORM = { builtin = \"platform\" }\n").unwrap();
    std::fs::write(
        game.join("main.rock"),
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
    )
    .unwrap();
    std::fs::write(
        game.join("tests/sims/probe.rock"),
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
    )
    .unwrap();
    let manifest = game.join("stick.toml");
    std::fs::write(
        &manifest,
        format!(
            "{}\n[sim]\nenvironment_prefixes = [\"PROBE_\"]\n",
            std::fs::read_to_string(&manifest).unwrap()
        ),
    )
    .unwrap();
    let command = || {
        let mut command = std::process::Command::new(&rock);
        command
            .current_dir(fixture.path())
            .env(
                "ROCK_DIR",
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."),
            )
            .env_remove("STONE_HOT_RELOAD");
        command
    };
    let invoke = |args: &[&str], fail: bool| {
        let output = command()
            .args(args)
            .env(
                "ROCK_SIM_LOG_DIR",
                fixture.path().join(if fail { "failed" } else { "passed" }),
            )
            .env("SIM_PROBE_FAIL", if fail { "1" } else { "0" })
            .env("PROBE_VARIANT", "declared-workload")
            .env("UNRELATED_VALUE", "excluded")
            .env(
                "ROCK_SIM_TIMEOUT_SECS",
                test_utils::HANG_BACKSTOP.as_secs().to_string(),
            )
            .output()
            .unwrap();
        let diagnostic = output_text(&output);
        (output, diagnostic)
    };
    let project = game.to_str().unwrap();
    let (list, text) = invoke(&["sim", project, "--list"], false);
    assert!(list.status.success(), "{text}");
    assert!(
        String::from_utf8_lossy(&list.stdout).contains("portable_sim"),
        "{text}"
    );
    for (phase, variable) in [
        ("startup", "SIM_PROBE_EARLY"),
        ("shutdown", "SIM_PROBE_LATE"),
        ("tail", "SIM_PROBE_TAIL"),
        ("finalizer", "SIM_PROBE_FINALIZER"),
    ] {
        let late = command()
            .args(["sim", project, "portable_sim", "--jobs", "1"])
            .env("ROCK_SIM_LOG_DIR", fixture.path().join(phase))
            .env(variable, "1")
            .output()
            .unwrap();
        let late_text = output_text(&late);
        assert!(!late.status.success(), "{phase} errors passed: {late_text}");
        assert!(
            late_text.contains(&format!("{phase} failure")),
            "{late_text}"
        );
    }
    for fail in [false, true] {
        let (run, text) = invoke(&["sim", project, "portable_sim", "--jobs", "1"], fail);
        assert_eq!(run.status.success(), !fail, "{text}");
        assert!(text.contains("portable sim sentinel"), "{text}");
        assert_eq!(
            text.contains("logged error: [PROBE] recovered failure"),
            fail,
            "{text}"
        );
        assert_eq!(
            text.contains("expected error log was not logged: [PROBE] never logged"),
            fail,
            "{text}"
        );
        assert!(
            !text.contains("logged error: [PROBE] asynchronous"),
            "{text}"
        );
        assert_eq!(
            text.contains("logged error: [STONE] stone_gpu_scope_begin: null name"),
            fail,
            "{text}"
        );
        let history = fixture.path().join(if fail { "failed" } else { "passed" });
        let run_dir = std::fs::read_dir(history)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let events = std::fs::read_to_string(run_dir.join("events.jsonl")).unwrap();
        let start: serde_json::Value =
            serde_json::from_str(events.lines().next().unwrap()).unwrap();
        assert_eq!(
            start["timing_host"]["ambient"]["PROBE_VARIANT"],
            "declared-workload"
        );
        assert!(
            start["timing_host"]["ambient"]
                .get("UNRELATED_VALUE")
                .is_none()
        );
        let end: serde_json::Value = serde_json::from_str(events.lines().last().unwrap()).unwrap();
        assert_eq!(end["event"], "run_end", "{events}");
        assert_eq!(
            end["status"],
            if fail { "FAIL" } else { "PASS" },
            "{events}"
        );
    }
}

#[cfg(unix)]
fn game_run_fixture(root: &Path) -> std::path::PathBuf {
    write_game_project(
        root,
        "game",
        "launch-probe",
        "main_stone.rock",
        "fn main() {}\n",
    )
}

#[cfg(unix)]
#[test]
fn game_run_forwards_app_arguments() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let project = game_run_fixture(root.path());
    let wrapper = root.path().join("luajit-probe");
    std::fs::write(
        &wrapper,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$ARGUMENTS\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let arguments = root.path().join("arguments");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_rock"))
        .arg("run")
        .arg(project)
        .args(["--", "first", "two words", "--flag"])
        .current_dir(root.path())
        .env("ROCK_SKIP_ENGINE_BUILD", "1")
        .env("ROCK_LUAJIT", wrapper)
        .env("ROCK_HEADLESS_SCOPE", "0")
        .env("STONE_HEADLESS", "0")
        .env("STONE_JV", "0")
        .env("ARGUMENTS", &arguments)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let arguments = std::fs::read_to_string(arguments).unwrap();
    let arguments: Vec<_> = arguments.lines().collect();
    assert_eq!(&arguments[1..], ["first", "two words", "--flag"]);
}

#[cfg(unix)]
#[test]
fn game_run_retries_a_scope_failure_before_execution() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let project = game_run_fixture(root.path());
    let launcher = root.path().join("systemd-run");
    std::fs::write(
        &launcher,
        r#"#!/bin/sh
if [ ! -e "$ATTEMPTS" ]; then
    echo failed > "$ATTEMPTS"
    exit 1
fi
: > "$ROCK_SCOPE_START_RECEIPT" || exit 125
echo executed >> "$EXECUTIONS"
"#,
    )
    .unwrap();
    std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755)).unwrap();
    let executions = root.path().join("executions");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_rock"))
        .arg("run")
        .arg(project)
        .current_dir(root.path())
        .env("ROCK_SKIP_ENGINE_BUILD", "1")
        .env("ROCK_LUAJIT", launcher)
        .env("ROCK_HEADLESS_SCOPE", "0")
        .env("STONE_HEADLESS", "1")
        .env("ATTEMPTS", root.path().join("attempts"))
        .env("EXECUTIONS", &executions)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::read_to_string(executions).unwrap(), "executed\n");
}

#[cfg(target_os = "macos")]
#[test]
fn game_run_does_not_outlive_a_killed_rock() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Command, Stdio};
    struct Processes(std::process::Child, Option<i32>);
    impl Drop for Processes {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
            if let Some(pid) = self.1 {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        }
    }
    let root = tempfile::tempdir().unwrap();
    let project = game_run_fixture(root.path());
    let wrapper = root.path().join("parked-luajit");
    std::fs::write(&wrapper, "#!/bin/sh\necho $$ > \"$GAME_PID.staging\"\nmv \"$GAME_PID.staging\" \"$GAME_PID\"\nexec sleep 600\n").unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let pid_path = root.path().join("game.pid");
    let mut processes = Processes(
        Command::new(env!("CARGO_BIN_EXE_rock"))
            .arg("run")
            .arg(project)
            .current_dir(root.path())
            .env("ROCK_SKIP_ENGINE_BUILD", "1")
            .env("ROCK_LUAJIT", wrapper)
            .env("ROCK_HEADLESS_SCOPE", "0")
            .env("STONE_HEADLESS", "0")
            .env("GAME_PID", &pid_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
        None,
    );
    let start = bone_core::prelude::Instant::now();
    let pid = loop {
        if let Ok(pid) = std::fs::read_to_string(&pid_path) {
            break pid.trim().parse::<i32>().unwrap();
        }
        assert!(
            processes.0.try_wait().unwrap().is_none(),
            "launcher exited before game startup"
        );
        assert!(
            start.elapsed() < test_utils::HANG_BACKSTOP,
            "game never started"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    processes.1 = Some(pid);
    processes.0.kill().unwrap();
    processes.0.wait().unwrap();
    let killed = bone_core::prelude::Instant::now();
    while unsafe { libc::kill(pid, 0) } == 0 {
        assert!(
            killed.elapsed() < test_utils::HANG_BACKSTOP,
            "game outlived launcher"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    processes.1 = None;
}
