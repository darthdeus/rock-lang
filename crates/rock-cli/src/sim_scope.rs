//! Places every `rock sim` worker in a transient systemd user scope under
//! `rock-sim.slice`.
//!
//! The per-worker cap (`ROCK_SIM_MEMORY_MB`, enforced by the runner) stops one
//! runaway sim, but not many healthy ones: on 2026-09-22 ~47 workers from
//! parallel agents, none above 6 GB, filled RAM and swap together and the
//! global OOM killer took down the desktop session. The slice gives all
//! concurrent workers one kernel-enforced budget (its MemoryMax lives in the
//! author's home-manager config), so reaching it OOM-kills a sim worker inside
//! the slice instead of an unrelated process. `OOMPolicy=continue` keeps such
//! a kill from stopping anything else in the worker's scope.
//!
//! `systemd-run --scope` execs the command in place, so the spawned PID is the
//! luajit worker itself: stdio pipes, `child.kill()` and the `/proc` memory
//! poll behave exactly as for a direct spawn.

use std::process::{Command, ExitStatus};
use std::sync::OnceLock;

pub(crate) const SIM_SLICE: &str = "rock-sim.slice";

/// Whether sim workers are placed in the slice, decided once per process.
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
    if std::env::var("ROCK_SIM_SCOPE").as_deref() == Ok("0") {
        log::info!(target: "SIM", "ROCK_SIM_SCOPE=0: sim workers run outside {SIM_SLICE}");
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
            log::warn!(target: "SIM",
                "sim workers run outside {SIM_SLICE}, without the shared memory budget: \
                 systemd-run failed ({}): {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim());
            Placement::Unscoped
        }
        Err(e) => {
            log::warn!(target: "SIM",
                "sim workers run outside {SIM_SLICE}, without the shared memory budget: \
                 cannot run systemd-run: {e}");
            Placement::Unscoped
        }
    }
}

fn scope_args(mut cmd: Command) -> Command {
    cmd.args(["--user", "--scope", "--quiet", "--collect"])
        .arg(format!("--slice={SIM_SLICE}"))
        .args(["-p", "OOMPolicy=continue", "--"]);
    cmd
}

/// `inner` re-issued through `systemd-run` when workers are scoped, otherwise
/// `inner` itself. Call before configuring stdio, which a `Command` cannot
/// report back for copying.
pub(crate) fn worker_command(inner: Command) -> Command {
    match placement() {
        Placement::Scoped => wrap(inner),
        Placement::Unscoped => inner,
    }
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

/// Names the likely cause when a scoped worker dies of SIGKILL the runner did
/// not send: the slice budget, shared with every other sim running on the
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
                 is the kernel OOM killer enforcing {SIM_SLICE}'s MemoryMax, which all \
                 concurrent sims share (`journalctl -k | grep -i oom` names the victim)"
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
        assert!(args[..sep].contains(&format!("--slice={SIM_SLICE}")), "{args:?}");
        assert!(args[..sep].iter().any(|a| a == "OOMPolicy=continue"), "{args:?}");
        assert_eq!(&args[sep + 1..], ["/opt/luajit", "main.lua"]);

        let envs: Vec<_> = cmd.get_envs().collect();
        assert!(envs.contains(&(OsStr::new("OUT"), Some(OsStr::new("/tmp/x")))));
        assert!(envs.contains(&(OsStr::new("STONE_LOG"), None)));
        assert_eq!(cmd.get_current_dir(), Some(std::path::Path::new("/work")));
    }
}
