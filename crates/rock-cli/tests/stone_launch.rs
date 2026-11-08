use std::path::Path;

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
    use std::process::{Child, Stdio};
    struct Watch(Child);
    impl Drop for Watch {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
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
        let start = bone_core::prelude::Instant::now();
        while !condition() {
            assert!(
                watch.0.try_wait().unwrap().is_none(),
                "watch exited: {}",
                read(&log)
            );
            assert!(
                start.elapsed() < test_utils::HANG_BACKSTOP,
                "watch stalled: {}",
                read(&log)
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
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
fn concurrent_game_runs_keep_their_own_output() {
    use std::process::{Child, Command, Stdio};
    struct Runs(Vec<Child>);
    impl Drop for Runs {
        fn drop(&mut self) {
            for child in &mut self.0 {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("tmp")).unwrap();
    let shared = root.path().join("tmp/main.lua");
    std::fs::write(&shared, "unrelated output").unwrap();
    let mut runs = Runs(Vec::new());
    for name in ["first", "second"] {
        let project = root.path().join(name);
        std::fs::create_dir(&project).unwrap();
        std::fs::write(
            project.join("stick.toml"),
            format!("[package]\nname = \"{name}\"\n"),
        )
        .unwrap();
        std::fs::write(
            project.join("main_stone.rock"),
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
        )
        .unwrap();
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
        "[package]\nname = \"bounded-probe\"\n[dependencies]\nstone = { package = \"stone\" }\n[build.vars]\nGIT_SHA = \"probe\"\nBUILD_UNIX = \"0\"\nBUILD_DATE = \"test\"\nCHANNEL = { builtin = \"channel\" }\nPLATFORM = { builtin = \"platform\" }\n",
    )
    .unwrap();
    std::fs::write(
        game.join("main.rock"),
        r#"
@init
fn probe_init() {
    if os_getenv("BOUNDED_FAIL") == "1" { error("bounded runtime sentinel") }
}
fn main() {
    stone_run_game(GameConfig { title: "bounded-probe", width: 64, height: 64,
        vsync: 0, clear_color: color(0.2, 0.3, 0.4, 1.0) })
}
"#,
    )
    .unwrap();
    let screenshot = fixture.path().join("frame.png");
    let run = |command: &str, fail: bool| {
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
            .env("ROCK_FRAME_TIMEOUT_SECS", "120")
            .env("BOUNDED_FAIL", if fail { "1" } else { "0" })
            .env_remove("STONE_HOT_RELOAD")
            .env_remove("FRAME_COUNT");
        if command == "perf" {
            cmd.args(["--warmup-frames", "0", "--format", "json"]);
        }
        let result = cmd.output().unwrap();
        let diagnostic = format!(
            "{}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
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
        } else if command == "perf" {
            let summary: serde_json::Value = serde_json::from_slice(&result.stdout)
                .unwrap_or_else(|error| panic!("{error}: {diagnostic}"));
            assert_eq!(summary["measurement"]["completed_frames"], 3, "{summary}");
            assert_eq!(summary["scenario"]["complete"], true, "{summary}");
            assert!(output.join("perf-summary.json").is_file());
        }
    };
    run("frame", false);
    assert_eq!(
        &std::fs::read(&screenshot).unwrap()[..8],
        b"\x89PNG\r\n\x1a\n"
    );
    run("perf", false);
    run("frame", true);
    run("perf", true);
}

#[cfg(unix)]
#[test]
fn overlapping_frames_execute_their_own_compiled_program() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Child, Command, Stdio};
    struct Runs(Vec<Child>, std::path::PathBuf);
    impl Drop for Runs {
        fn drop(&mut self) {
            let _ = std::fs::write(&self.1, "");
            for child in &mut self.0 {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
    let root = tempfile::tempdir().unwrap();
    let wrapper = root.path().join("held-luajit");
    std::fs::write(
        &wrapper,
        r#"#!/bin/sh
for argument in "$@"; do
    case "$argument" in *.lua) printf '%s' "$argument" > "$LAUNCH_PATH";; esac
done
: > "$LAUNCH_READY"
for attempt in $(seq 1 1200); do
    if test -f "$LAUNCH_RELEASE"; then exec "$REAL_LUAJIT" "$@"; fi
    sleep 0.05
done
exit 97
"#,
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    for cache in ["0", "1"] {
        let output = root.path().join(format!("output-{cache}"));
        std::fs::create_dir(&output).unwrap();
        std::fs::write(output.join("main.lua"), "unrelated output").unwrap();
        let release = root.path().join(format!("release-{cache}"));
        let mut runs = Runs(Vec::new(), release.clone());
        for name in ["first", "second"] {
            let project = root.path().join(format!("{name}-{cache}"));
            std::fs::create_dir(&project).unwrap();
            std::fs::write(
                project.join("stick.toml"),
                format!("[package]\nname = \"{name}\"\n"),
            )
            .unwrap();
            std::fs::write(
                project.join("main.rock"),
                format!("fn main() {{ print(\"PROGRAM_{name}\") }}\n"),
            )
            .unwrap();
            if cache == "1" {
                let warm = Command::new(env!("CARGO_BIN_EXE_rock"))
                    .arg("frame")
                    .arg(&project)
                    .arg("--output")
                    .arg(&output)
                    .args(["--frames", "1", "--headless"])
                    .current_dir(root.path())
                    .env("ROCK_SKIP_ENGINE_BUILD", "1")
                    .env("ROCK_FRAME_COMPILE_CACHE", cache)
                    .env("ROCK_FRAME_COMPILE_CACHE_DIR", root.path().join("cache"))
                    .output()
                    .unwrap();
                assert!(
                    warm.status.success(),
                    "{}",
                    String::from_utf8_lossy(&warm.stderr)
                );
            }
            let log =
                std::fs::File::create(root.path().join(format!("{name}-{cache}.log"))).unwrap();
            runs.0.push(
                Command::new(env!("CARGO_BIN_EXE_rock"))
                    .arg("frame")
                    .arg(&project)
                    .arg("--output")
                    .arg(&output)
                    .args(["--frames", "1", "--headless"])
                    .current_dir(root.path())
                    .env("ROCK_SKIP_ENGINE_BUILD", "1")
                    .env("ROCK_FRAME_COMPILE_CACHE", cache)
                    .env("ROCK_FRAME_COMPILE_CACHE_DIR", root.path().join("cache"))
                    .env("REAL_LUAJIT", rock_engine_host::runtime::luajit_path())
                    .env("ROCK_LUAJIT", &wrapper)
                    .env(
                        "LAUNCH_PATH",
                        root.path().join(format!("{name}-{cache}.path")),
                    )
                    .env(
                        "LAUNCH_READY",
                        root.path().join(format!("{name}-{cache}.ready")),
                    )
                    .env("LAUNCH_RELEASE", &release)
                    .stdout(Stdio::from(log.try_clone().unwrap()))
                    .stderr(Stdio::from(log))
                    .spawn()
                    .unwrap(),
            );
        }
        let start = rock_core::clock::Instant::now();
        while !["first", "second"]
            .iter()
            .all(|name| root.path().join(format!("{name}-{cache}.ready")).exists())
        {
            for (index, child) in runs.0.iter_mut().enumerate() {
                assert!(
                    child.try_wait().unwrap().is_none(),
                    "{}",
                    std::fs::read_to_string(
                        root.path()
                            .join(format!("{}-{cache}.log", ["first", "second"][index]))
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
            let log =
                std::fs::read_to_string(root.path().join(format!("{name}-{cache}.log"))).unwrap();
            assert!(log.contains(&format!("PROGRAM_{name}")), "{log}");
            if cache == "1" {
                assert!(log.contains("frame compile cache hit"), "{log}");
            }
            let lua =
                std::fs::read_to_string(root.path().join(format!("{name}-{cache}.path"))).unwrap();
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
}
