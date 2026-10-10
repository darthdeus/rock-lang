//! In-memory luajit eval: pipe Rock-compiled Lua into luajit's stdin, capture
//! combined stdout/stderr. Used by the Rust test suite and the `@test`-runner —
//! callers that compile a snippet and want its output as a String. NOT for
//! `rock run`/`rock stone` (those exec the compiled `.lua` file with inherited
//! stdio so interactive apps reach the tty).

use crate::process_management::{register_child_process, unregister_child_process};
use rock_core::CompilerError;
use std::time::Duration;
use wait_timeout::ChildExt;

/// What the luajit child did: its streams, and whether it EXITED CLEANLY.
///
/// `success` is the child's real exit status, kept as data. It used to be
/// encoded in-band -- the returned String got `"STDERR: "` prepended -- so every
/// caller had to sniff for that substring to learn whether the program died, and
/// `rock run` (which just printed the string) could not tell at all and exited 0
/// on a crash. An in-band marker also conflated two different things: a run that
/// FAILED, and a run that merely wrote to stderr.
///
/// `Err` from the runner stays reserved for HARNESS failures -- luajit missing,
/// a broken pipe, a timeout -- which is a different category from "the program
/// ran and exited non-zero".
#[derive(Debug, Clone)]
pub struct LuaRun {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
}

impl LuaRun {
    /// stdout followed by stderr -- for callers that only want to show a human
    /// what happened. Never use this to DECIDE anything; read `success`.
    pub fn combined(&self) -> String {
        if self.stderr.is_empty() {
            self.stdout.clone()
        } else {
            format!("{}STDERR: {}", self.stdout, self.stderr)
        }
    }
}

pub fn run_lua_code_with_timeout(
    code: &str,
    timeout: Option<Duration>,
) -> Result<LuaRun, CompilerError> {
    run_lua_code_with_timeout_and_env(code, timeout, &[])
}

pub fn run_lua_code_with_timeout_and_env(
    code: &str,
    timeout: Option<Duration>,
    env: &[(&str, &str)],
) -> Result<LuaRun, CompilerError> {
    let luajit_path = crate::project::luajit_path();
    run_lua_code_with_timeout_at(&luajit_path, code, timeout, env)
}

fn run_lua_code_with_timeout_at(
    luajit_path: &std::path::Path,
    code: &str,
    timeout: Option<Duration>,
    env: &[(&str, &str)],
) -> Result<LuaRun, CompilerError> {
    let luajit_path_str = luajit_path.to_string_lossy().into_owned();

    if !luajit_path.exists() {
        return Err(CompilerError::new_no_location(format!(
            "LuaJIT not found at: {}. Make sure prebuilt gamelibs are installed or ROCK_DIR is set.",
            luajit_path_str
        )));
    }

    let mut cmd = std::process::Command::new(luajit_path);
    crate::compiler_thp::configure_runtime_command(&mut cmd);
    cmd.envs(env.iter().copied());
    // `-` = "run stdin as a script". WITHOUT it luajit treats piped stdin as an
    // INTERACTIVE session: it prints a fatal error and still exits 0. That is the
    // root of the whole "`rock run` exits 0 while printing a LuaJIT crash" trap --
    // the status was unusable, so failure got encoded as a `"STDERR: "` marker in
    // the output string instead, and `rock run` printed it and returned Ok.
    cmd.arg("-")
        .env("LUA_PATH", crate::project::lua_path_value())
        // The same library search path `build_luajit_command` gives a game or
        // sim process. Inert on its own -- nothing here dlopens anything until
        // a program asks -- but without it a `@test` that loads the engine
        // cdylib for a headless capability (the metrics-only font the
        // `rock test <game>` process uses to measure text) cannot find it, and
        // neither can the prebuilt libs it links against.
        .env(
            crate::project::lib_path_var(),
            crate::project::lib_path_with_inherited(),
        )
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = cmd.spawn().expect("failed to execute process");
    let pid = child.id();
    register_child_process(pid);

    // Drain both pipes on threads BEFORE waiting. Waiting first deadlocks as
    // soon as the child fills a pipe buffer, which is the ~50k-line-script hang
    // recorded in `stone_test_decorator_cleanup.rs`.
    let mut stdin = child.stdin.take().expect("stdin piped");
    let mut stdout_pipe = child.stdout.take().expect("stdout piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr piped");
    // The read result is PROPAGATED, not dropped. Discarding it turned a failed
    // drain into empty output, which reads downstream as "the program printed
    // nothing" -- a harness failure wearing a successful program's clothes.
    let stdout_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut stdout_pipe, &mut buf).map(|_| buf)
    });
    let stderr_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut stderr_pipe, &mut buf).map(|_| buf)
    });

    // LuaJIT can report a load error and exit before the whole script has been
    // written. Keep that write error until the child status and stderr are
    // available so it cannot mask the load diagnostic.
    let write_error = {
        use std::io::Write;
        let result = stdin.write_all(code.as_bytes());
        drop(stdin);
        result.err()
    };

    // The status MUST come from whichever call reaped the child. `wait_timeout`
    // reaps it itself, so a following `wait`/`wait_with_output` cannot report the
    // real status -- it fabricates `exit(0)`. That is why this used to treat any
    // stderr output as failure: the stderr check was compensating for an exit
    // status that was always 0 here, which in turn is why `rock run` exited 0 on
    // a program that died.
    let status = if let Some(timeout_duration) = timeout {
        match child
            .wait_timeout(timeout_duration)
            .expect("Failed to wait for process")
        {
            Some(status) => status,
            None => {
                child.kill().expect("Failed to kill timed out process");
                child.wait().expect("Failed to wait for killed process");
                unregister_child_process(pid);
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                return Err(CompilerError::new_no_location(format!(
                    "Luajit process timed out after {} seconds.",
                    timeout_duration.as_secs_f32()
                )));
            }
        }
    } else {
        child.wait().expect("Failed to wait for process")
    };

    unregister_child_process(pid);

    // A panicked reader thread or a failed read is a HARNESS failure and is
    // reported as one. `unwrap_or_default()` here would hand back empty output
    // and let the caller judge the program on it.
    let drained = |joined: std::thread::Result<std::io::Result<Vec<u8>>>, which: &str| {
        match joined {
            Ok(Ok(buf)) => Ok(buf),
            Ok(Err(e)) => Err(CompilerError::new_no_location(format!(
                "failed to read the luajit process's {which}: {e}"
            ))),
            Err(_) => Err(CompilerError::new_no_location(format!(
                "the thread draining the luajit process's {which} panicked"
            ))),
        }
    };
    let stdout = drained(stdout_thread.join(), "stdout")?;
    let stderr = drained(stderr_thread.join(), "stderr")?;

    if let Some(e) = write_error {
        // A child load/runtime failure is the useful result even when it
        // closed stdin before the host finished writing. A successful child
        // has no load/runtime failure that can replace the write failure.
        if status.success() {
            return Err(CompilerError::new_no_location(format!(
                "Failed to write test code to luajit process: {}.",
                e
            )));
        }
    }

    Ok(LuaRun {
        stdout: String::from_utf8_lossy(&stdout).to_string(),
        stderr: String::from_utf8_lossy(&stderr).to_string(),
        success: status.success(),
    })
}

pub fn run_lua_code(code: &str) -> Result<LuaRun, CompilerError> {
    run_lua_code_with_timeout(code, Some(Duration::from_millis(1800)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Process env vars touched by this test, restored on drop -- including
    /// when an assert unwinds, so a failure cannot leak LD_PRELOAD/PATH/
    /// STONE_RENDERDOC_* into a sibling test sharing this process.
    struct EnvGuard {
        saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl EnvGuard {
        /// SAFETY: the caller upholds the same nextest-process-isolation
        /// assumption as the stone_jv test; the guard restores on every exit.
        fn set(vars: &[(&'static str, Option<&str>)]) -> Self {
            let mut saved = Vec::with_capacity(vars.len());
            for (key, value) in vars {
                saved.push((*key, std::env::var_os(key)));
                unsafe {
                    match value {
                        Some(v) => std::env::set_var(key, v),
                        None => std::env::remove_var(key),
                    }
                }
            }
            Self { saved }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (key, prev) in self.saved.drain(..) {
                unsafe {
                    match prev {
                        Some(v) => std::env::set_var(key, v),
                        None => std::env::remove_var(key),
                    }
                }
            }
        }
    }

    /// Explicitly-set env on a Command (`get_envs` is the only reader;
    /// ambient inheritance is invisible, which is exactly the pin).
    fn cmd_env(cmd: &std::process::Command, key: &str) -> Option<std::ffi::OsString> {
        cmd.get_envs()
            .find(|(k, _)| *k == std::ffi::OsStr::new(key))
            .and_then(|(_, v)| v.map(|v| v.to_os_string()))
    }

    /// The STONE_RENDERDOC preload gate reaches only the final game spawn:
    /// `build_luajit_command` gains the lib while the bytecode compile and
    /// the stdin harness spawns stay clean, and everything stays clean when
    /// the switch is unset. Append semantics and lib resolution are pinned
    /// through the pure helpers: this test mutates only STONE_RENDERDOC_*
    /// process-wide (no sibling reads them), never LD_PRELOAD/PATH, so it
    /// stays green under threaded `cargo test` as well as nextest.
    #[test]
    fn renderdoc_gate_reaches_only_the_final_spawn() {
        use std::ffi::OsStr;
        use std::path::Path;

        const SENTINEL: &str = "/ambient/sentinel.so";
        const TEMPLATE: &str = "/tmp/rd-gate-test/captures/mag-crypt";
        let lua = Path::new("tmp/main.lua");

        // Append semantics, pure: ambient preserved first, scrub-empty
        // contributes no entry (entries are `:`-separated per ld.so).
        let joined = crate::project::join_preload(
            Some(OsStr::new(SENTINEL)),
            OsStr::new("librenderdoc.so"),
        );
        let binding = joined.to_string_lossy();
        let entries: Vec<_> = binding.split(':').collect();
        assert_eq!(entries, vec![SENTINEL, "librenderdoc.so"]);
        assert_eq!(
            crate::project::join_preload(None, OsStr::new("librenderdoc.so")).to_string_lossy(),
            "librenderdoc.so"
        );
        assert_eq!(
            crate::project::join_preload(Some(OsStr::new("")), OsStr::new("librenderdoc.so"))
                .to_string_lossy(),
            "librenderdoc.so"
        );

        // Resolution, pure: override verbatim, renderdoccmd sibling, bare
        // fallback. Canonicalized: production canonicalizes the PATH hit.
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonical tempdir");
        let bin = root.join("bin");
        let lib = root.join("lib");
        std::fs::create_dir_all(&bin).expect("bin");
        std::fs::create_dir_all(&lib).expect("lib");
        std::fs::write(bin.join("renderdoccmd"), "#!/bin/sh\n").expect("fake renderdoccmd");
        let sibling = lib.join("librenderdoc.so");
        std::fs::write(&sibling, "fake").expect("fake lib");
        let resolve = crate::project::resolve_renderdoc_lib_with;
        assert_eq!(
            resolve(
                Some(sibling.as_os_str().to_os_string()),
                Some(std::ffi::OsString::from("/no/such/bin"))
            ),
            sibling.as_os_str()
        );
        assert_eq!(
            resolve(None, Some(bin.as_os_str().to_os_string())),
            sibling.as_os_str()
        );
        assert_eq!(
            resolve(None, Some(std::ffi::OsString::from("/no/such/bin"))).to_string_lossy(),
            "librenderdoc.so"
        );
        assert_eq!(resolve(None, None).to_string_lossy(), "librenderdoc.so");

        // Gate ON with an explicit lib override (a real file: keeps the
        // missing-override warning out of the test output).
        {
            let override_lib = sibling.to_string_lossy().into_owned();
            let _guard = EnvGuard::set(&[
                ("STONE_RENDERDOC", Some(TEMPLATE)),
                ("STONE_RENDERDOC_LIB", Some(&override_lib)),
            ]);
            if cfg!(target_os = "linux") {
                let game = crate::project::build_luajit_command(lua, &[]);
                let preload = cmd_env(&game, "LD_PRELOAD")
                    .expect("gate sets LD_PRELOAD on the final game spawn")
                    .to_string_lossy()
                    .into_owned();
                assert!(
                    preload.contains(&override_lib),
                    "gate must append the override lib: {preload}"
                );

                let bytecode =
                    crate::project::build_luajit_bytecode_command(lua, Path::new("tmp/main.ljbc"));
                assert!(
                    cmd_env(&bytecode, "LD_PRELOAD").is_none(),
                    "bytecode compile is not the final spawn"
                );
            }
            if cfg!(unix) {
                // Harness spawn observed through real children: an explicit
                // child preload passes through exactly, and without one no
                // gate lib appears. The sentinel travels via the child's own
                // env, never via process-global LD_PRELOAD.
                let probe = root.join("print_preload");
                std::fs::write(&probe, "#!/bin/sh\ncat >/dev/null\necho \"LD_PRELOAD=$LD_PRELOAD\"\n")
                    .expect("probe script");
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o755))
                    .expect("probe executable");
                let run = run_lua_code_with_timeout_at(
                    &probe,
                    "",
                    Some(test_utils::HANG_BACKSTOP),
                    &[("LD_PRELOAD", SENTINEL)],
                )
                .expect("probe child runs");
                assert!(run.success, "probe child failed: {}", run.combined());
                assert_eq!(
                    run.stdout.trim(),
                    format!("LD_PRELOAD={SENTINEL}"),
                    "harness spawn must pass the child preload through exactly"
                );
                let bare = run_lua_code_with_timeout_at(
                    &probe,
                    "",
                    Some(test_utils::HANG_BACKSTOP),
                    &[],
                )
                .expect("bare probe child runs");
                assert!(
                    !bare.stdout.contains("librenderdoc"),
                    "harness spawn must not gain the gate lib: {}",
                    bare.stdout
                );
            }
        }

        // Gate OFF: STONE_RENDERDOC unset leaves the final spawn clean too.
        {
            let _guard = EnvGuard::set(&[("STONE_RENDERDOC", None), ("STONE_RENDERDOC_LIB", None)]);
            if cfg!(target_os = "linux") {
                let game = crate::project::build_luajit_command(lua, &[]);
                assert!(
                    cmd_env(&game, "LD_PRELOAD").is_none(),
                    "gate off must not touch LD_PRELOAD"
                );
            }
        }
    }

    #[test]
    fn load_error_wins_over_stdin_broken_pipe() {
        let mut code = "local = 1\n".to_string();
        code.push_str(&"-- valid filler\n".repeat(1_000_000));

        let run = run_lua_code_with_timeout(&code, Some(test_utils::HANG_BACKSTOP))
            .expect("LuaJIT load errors are child results, not runner failures");

        assert!(!run.success, "invalid Lua unexpectedly loaded: {}", run.combined());
        assert!(
            run.stderr.contains("expected near") || run.stderr.contains("unexpected symbol"),
            "LuaJIT's parse error was not reported: {}",
            run.combined()
        );
        assert!(
            !run.combined().contains("Broken pipe"),
            "the stdin write error masked LuaJIT's parse error: {}",
            run.combined()
        );
    }

    #[cfg(unix)]
    #[test]
    fn broken_pipe_without_a_child_error_is_reported() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let child_path = dir.path().join("close_stdin");
        std::fs::write(&child_path, "#!/bin/sh\nexec </dev/null\nsleep 1\n")
            .expect("child script");
        let mut permissions = std::fs::metadata(&child_path)
            .expect("child metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&child_path, permissions).expect("child permissions");

        let mut code = "ignored by child\n".to_string();
        code.push_str(&"-- valid filler\n".repeat(1_000_000));

        let error = run_lua_code_with_timeout_at(&child_path, &code, Some(test_utils::HANG_BACKSTOP), &[])
            .expect_err("a child with no diagnostic must not swallow the broken pipe");

        assert!(
            error.message.contains("Broken pipe"),
            "the genuine write failure was not reported: {}",
            error.message
        );
    }
}
