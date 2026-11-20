//! Runtime discovery and process placement shared by public CLI commands.
use crate::Result;
use std::process::Command;

#[derive(Clone, Debug)]
pub enum RuntimeProcess {
    LuaRun { headless: bool },
    LuaStdin,
    LuaTest,
    LuaBytecode,
    Native,
}

/// Call once before setting stdio: Lua retargeting and scope placement rebuild the command.
pub fn configure(command: Command, purpose: RuntimeProcess) -> Command {
    use RuntimeProcess::*;
    let lua = matches!(purpose, LuaRun { .. } | LuaStdin | LuaTest | LuaBytecode);
    let trace = matches!(purpose, LuaRun { .. } | LuaTest);
    let mut command = rock_engine_host::runtime::configure_command(
        command,
        lua,
        trace,
        matches!(purpose, LuaRun { .. }),
    );
    if matches!(purpose, LuaTest | LuaRun { headless: true }) {
        command.env("STONE_HEADLESS", "1");
    }
    let scoped = matches!(purpose, LuaRun { headless: true })
        || matches!(purpose, LuaRun { .. }) && crate::headless_scope::inherits_headless();
    if scoped {
        crate::headless_scope::headless_command(command)
    } else {
        command
    }
}

/// Bind an interactive game's lifetime to its launcher, including SIGKILL.
pub(crate) fn spawn_game(
    command: &mut Command,
    scratch: &std::path::Path,
) -> std::io::Result<crate::headless_scope::BoundChild> {
    // The macOS guard must be able to end the whole game process group.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(command, 0);
    let child = crate::headless_scope::spawn_bound_child(command, scratch)?;
    crate::child_lifetime::guard_against_parent_kill(&child)?;
    crate::child_lifetime::track_game(child.id());
    Ok(child)
}

pub(crate) fn exit_after_game_interrupt(
    game: Option<&mut crate::headless_scope::BoundChild>,
) -> Result<()> {
    if crate::child_lifetime::game_interrupted() {
        if let Some(game) = game {
            game.wait()?;
        }
        crate::invocation_log::exit(130);
    }
    Ok(())
}

/// Portable scratch belongs to the OS/user-selected temporary directory,
/// independently of project and installation roots.
pub(crate) fn prepare_test(command: &mut std::process::Command) -> Result<tempfile::TempDir> {
    command.env("ROCK_DIR", rock_host::rock_dir());
    let scratch = tempfile::Builder::new().prefix("rock-test-").tempdir()?;
    command.env("ROCK_TEST_TMP", scratch.path());
    Ok(scratch)
}

#[cfg(test)]
mod process_tests {
    #[test]
    fn native_commands_use_the_authoritative_library_path() {
        let command = super::configure(
            std::process::Command::new("unused-native-program"),
            super::RuntimeProcess::Native,
        );
        let actual = command
            .get_envs()
            .find(|(name, _)| {
                *name == std::ffi::OsStr::new(rock_engine_host::runtime::lib_path_var())
            })
            .and_then(|(_, value)| value);
        assert_eq!(
            actual,
            Some(std::ffi::OsStr::new(
                &rock_engine_host::runtime::lib_path_with_inherited()
            ))
        );
    }

    #[cfg(unix)]
    #[test]
    fn interrupt_waits_for_game_shutdown() {
        use bone_core::prelude::{Duration, Instant};
        use std::process::Command;

        const SCRATCH: &str = "ROCK_INTERRUPT_TEST_SCRATCH";
        if let Some(root) = std::env::var_os(SCRATCH) {
            let root = std::path::PathBuf::from(root);
            let mut command = Command::new("sh");
            command.current_dir(&root).args([
                "-c",
                r#"trap 'touch draining; read -r release; touch finished; exit 0' TERM
touch ready
while :; do sleep 0.02; done"#,
            ]);
            let mut game = super::spawn_game(&mut command, &root).unwrap();
            std::fs::write(root.join("bound"), "").unwrap();
            game.wait().unwrap();
            super::exit_after_game_interrupt(None).unwrap();
            panic!("launcher should have exited with the interrupt status");
        }

        let scratch = tempfile::tempdir().unwrap();
        let mut launcher = Command::new(std::env::current_exe().unwrap());
        launcher
            .args([
                "--exact",
                "runtime_process::process_tests::interrupt_waits_for_game_shutdown",
            ])
            .env(SCRATCH, scratch.path())
            .stdin(std::process::Stdio::piped());
        let mut launcher = crate::child_lifetime::spawn_bound_child(&mut launcher).unwrap();
        let wait_for = |name| {
            let marker = scratch.path().join(name);
            let deadline = Instant::now() + test_utils::HANG_BACKSTOP;
            while !marker.exists() {
                assert!(Instant::now() < deadline, "timed out waiting for {name}");
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        wait_for("bound");
        wait_for("ready");
        assert_eq!(unsafe { libc::kill(launcher.id() as i32, libc::SIGINT) }, 0);
        wait_for("draining");
        assert!(
            launcher.try_wait().unwrap().is_none(),
            "launcher exited during shutdown"
        );
        use std::io::Write;
        launcher
            .stdin
            .take()
            .unwrap()
            .write_all(b"finish\n")
            .unwrap();
        wait_for("finished");
        assert_eq!(launcher.wait().unwrap().code(), Some(130));
    }
}

#[cfg(test)]
mod runtime_service_tests {
    use super::{RuntimeProcess, configure};
    #[test]
    fn lua_process_purposes_preserve_launch_and_capture_boundaries() {
        const CHILD: &str = "ROCK_LUA_PURPOSE_WITNESS";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir().unwrap();
            let library = root.path().join("librenderdoc.so");
            std::fs::write(&library, "").unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "runtime_process::runtime_service_tests::lua_process_purposes_preserve_launch_and_capture_boundaries"])
                .env(CHILD, "1").env("STONE_RENDERDOC", "capture")
                .env("STONE_RENDERDOC_LIB", &library).env("STONE_JV", "1")
                .env("STONE_HEADLESS", "0").env("ROCK_HEADLESS_SCOPE", "0")
                .output().unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        for (purpose, trace, capture, headless) in [
            (RuntimeProcess::LuaRun { headless: false }, true, true, None),
            (RuntimeProcess::LuaTest, true, false, Some("1")),
            (RuntimeProcess::LuaStdin, false, false, None),
            (RuntimeProcess::LuaBytecode, false, false, None),
        ] {
            let mut input = std::process::Command::new("plain-luajit");
            input
                .args(["script.lua", "app-arg"])
                .current_dir("/")
                .env("ROCK_CALLER_ENV", "preserved")
                .env_remove("ROCK_REMOVED_ENV");
            let command = configure(input, purpose);
            assert_eq!(
                command.get_program(),
                rock_engine_host::runtime::luajit_path().as_os_str()
            );
            let expected = if trace {
                vec!["-jv", "script.lua", "app-arg"]
            } else {
                vec!["script.lua", "app-arg"]
            };
            assert_eq!(command.get_args().collect::<Vec<_>>(), expected);
            assert_eq!(command.get_current_dir(), Some(std::path::Path::new("/")));
            let env = |name: &str| {
                command
                    .get_envs()
                    .find(|(key, _)| *key == name)
                    .and_then(|(_, value)| value)
            };
            assert_eq!(
                env("ROCK_CALLER_ENV"),
                Some(std::ffi::OsStr::new("preserved"))
            );
            assert!(
                command
                    .get_envs()
                    .any(|(key, value)| key == "ROCK_REMOVED_ENV" && value.is_none())
            );
            assert_eq!(env("STONE_HEADLESS"), headless.map(std::ffi::OsStr::new));
            #[cfg(target_os = "linux")]
            assert_eq!(env("LD_PRELOAD").is_some(), capture);
            #[cfg(not(target_os = "linux"))]
            let _ = capture;
        }
    }
}
