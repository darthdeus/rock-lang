//! Places every headless luajit child -- `rock sim` workers, `rock frame`,
//! `rock perf`, replay profiling and `rock run --agent-headless` -- in a
//! transient systemd user scope under `rock-headless.slice`. Windowed runs
//! (`rock stone`, plain `rock run`, replay windows) stay outside it.
//!
//! The per-worker sim cap (`ROCK_SIM_MEMORY_MB`) stops one runaway sim, but not
//! many healthy runs: on 2026-09-22 ~47 workers from parallel agents, none
//! above 6 GB, filled RAM and swap together and the global OOM killer took
//! down the desktop session. The slice gives all concurrent headless runs one
//! kernel-enforced budget (its limits live in the author's home-manager
//! config), so reaching it kills a run inside the slice instead of an
//! unrelated process. `OOMPolicy=continue` keeps such a kill from stopping
//! anything else in the run's scope.
//!
//! `systemd-run --scope` execs the command in place, so the spawned PID is the
//! luajit child itself: stdio pipes, `child.kill()`, the parent-death signal
//! and the `/proc` memory poll behave exactly as for a direct spawn.

use std::process::{Command, ExitStatus};
use std::sync::OnceLock;

pub(crate) const HEADLESS_SLICE: &str = "rock-headless.slice";

/// Whether headless children are placed in the slice, decided once per process.
#[derive(Debug)]
enum Placement {
    Scoped,
    Unscoped,
}

fn placement() -> &'static Placement {
    static PLACEMENT: OnceLock<Placement> = OnceLock::new();
    PLACEMENT.get_or_init(decide_placement)
}

fn decide_placement() -> Placement {
    if std::env::var("ROCK_HEADLESS_SCOPE").as_deref() == Ok("0") {
        log::info!(target: "RUN", "ROCK_HEADLESS_SCOPE=0: headless runs stay outside {HEADLESS_SLICE}");
        return Placement::Unscoped;
    }
    if !cfg!(target_os = "linux") {
        return Placement::Unscoped;
    }
    // One real placement is the only reliable availability check: the binary
    // can exist while the user manager or its D-Bus socket does not.
    match scope_args(Command::new("systemd-run")).arg("true").output() {
        Ok(out) if out.status.success() => Placement::Scoped,
        Ok(out) => {
            log::warn!(target: "RUN",
                "headless runs stay outside {HEADLESS_SLICE}, without the shared memory budget: \
                 systemd-run failed ({}): {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim());
            Placement::Unscoped
        }
        Err(e) => {
            log::warn!(target: "RUN",
                "headless runs stay outside {HEADLESS_SLICE}, without the shared memory budget: \
                 cannot run systemd-run: {e}");
            Placement::Unscoped
        }
    }
}

fn scope_args(mut cmd: Command) -> Command {
    cmd.args(["--user", "--scope", "--quiet", "--collect"])
        .arg(format!("--slice={HEADLESS_SLICE}"))
        .args(["-p", "OOMPolicy=continue", "--"]);
    cmd
}

/// `inner` re-issued through `systemd-run` when headless children are scoped,
/// otherwise `inner` itself. Call before configuring stdio, which a `Command`
/// cannot report back for copying.
pub(crate) fn headless_command(inner: Command) -> Command {
    match placement() {
        Placement::Scoped => wrap(inner),
        Placement::Unscoped => inner,
    }
}

/// Whether this process hands `STONE_HEADLESS=1` down to the children it
/// spawns without setting it on the `Command` itself.
pub(crate) fn inherits_headless() -> bool {
    std::env::var("STONE_HEADLESS").as_deref() == Ok("1")
}

fn wrap(inner: Command) -> Command {
    let mut cmd = scope_args(Command::new("systemd-run"));
    cmd.arg(inner.get_program()).args(inner.get_args());
    for (key, value) in inner.get_envs() {
        match value {
            Some(value) => cmd.env(key, value),
            None => cmd.env_remove(key),
        };
    }
    if let Some(dir) = inner.get_current_dir() {
        cmd.current_dir(dir);
    }
    cmd
}

/// Names the likely cause when a scoped sim worker dies of SIGKILL the runner
/// did not send: the slice budget, shared with every other headless run on the
/// machine, was reached.
pub(crate) fn outside_kill_note(status: &ExitStatus, runner_killed: bool) -> Option<String> {
    if runner_killed || !matches!(placement(), Placement::Scoped) {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if status.signal() == Some(libc::SIGKILL) {
            return Some(format!(
                "sim worker was killed by SIGKILL from outside the runner; the likely cause \
                 is the kernel OOM killer enforcing {HEADLESS_SLICE}'s MemoryMax, which all \
                 concurrent headless runs share (`journalctl -k | grep -i oom` names the victim)"
            ));
        }
    }
    #[cfg(not(unix))]
    let _ = status;
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn wrap_runs_the_inner_program_after_the_scope_options() {
        let mut inner = Command::new("/opt/luajit");
        inner.arg("main.lua").env("OUT", "/tmp/x").env_remove("STONE_LOG").current_dir("/work");
        let cmd = wrap(inner);

        assert_eq!(cmd.get_program(), OsStr::new("systemd-run"));
        let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        let sep = args.iter().position(|a| a == "--").expect("-- separator");
        assert!(args[..sep].contains(&format!("--slice={HEADLESS_SLICE}")), "{args:?}");
        assert!(args[..sep].iter().any(|a| a == "OOMPolicy=continue"), "{args:?}");
        assert_eq!(&args[sep + 1..], ["/opt/luajit", "main.lua"]);

        let envs: Vec<_> = cmd.get_envs().collect();
        assert!(envs.contains(&(OsStr::new("OUT"), Some(OsStr::new("/tmp/x")))));
        assert!(envs.contains(&(OsStr::new("STONE_LOG"), None)));
        assert_eq!(cmd.get_current_dir(), Some(std::path::Path::new("/work")));
    }
}
