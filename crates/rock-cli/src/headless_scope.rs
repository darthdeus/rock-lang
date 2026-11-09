//! Places every headless luajit child -- `rock sim` workers, `rock frame`,
//! `rock perf`, replay profiling and `STONE_HEADLESS=1 rock run` -- in a
//! transient systemd user scope under `rock-headless.slice`. Windowed runs
//! (`rock stone`, plain `rock run`, replay windows) stay outside it.
//!
//! All concurrent headless runs share a kernel-enforced memory budget when
//! the user configures this slice. OOMPolicy=continue keeps one killed process
//! from stopping unrelated work in its scope.
//!
//! `systemd-run --scope` execs the command in place, so the spawned PID is the
//! luajit child itself: stdio pipes, `child.kill()`, the parent-death signal
//! and the `/proc` memory poll behave exactly as for a direct spawn.

use std::process::{Child, Command, ExitStatus};
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
    // A transient manager outage must not turn off containment for this process.
    Placement::Scoped
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
pub fn inherits_headless() -> bool {
    std::env::var("STONE_HEADLESS").as_deref() == Ok("1")
}

fn wrap(inner: Command) -> Command {
    wrap_in(inner, Command::new("systemd-run"))
}

fn wrap_in(inner: Command, launcher: Command) -> Command {
    let mut cmd = scope_args(launcher);
    // The receipt precedes exec, so its absence after launcher exit proves no inner work ran.
    cmd.args([
        "sh",
        "-c",
        r#"if [ -n "${ROCK_SCOPE_START_RECEIPT:-}" ]; then
: > "$ROCK_SCOPE_START_RECEIPT" || exit 125
unset ROCK_SCOPE_START_RECEIPT
fi
exec "$@""#,
        "rock-scoped-exec",
    ])
    .arg(inner.get_program())
    .args(inner.get_args());
    cmd.env_remove("ROCK_SCOPE_START_RECEIPT");
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

/// A bound process is always reaped before its admission lease can be released.
pub struct BoundChild(Option<Child>);

impl std::ops::Deref for BoundChild {
    type Target = Child;
    fn deref(&self) -> &Child {
        self.0.as_ref().unwrap()
    }
}

impl std::ops::DerefMut for BoundChild {
    fn deref_mut(&mut self) -> &mut Child {
        self.0.as_mut().unwrap()
    }
}

impl Drop for BoundChild {
    fn drop(&mut self) {
        let Some(child) = self.0.as_mut() else { return };
        let needs_reap = match child.try_wait() {
            Ok(Some(_)) => false,
            Ok(None) => true,
            Err(error) => {
                log::warn!(target: "RUN", "child cleanup status: {error}");
                true
            }
        };
        if needs_reap {
            if let Err(error) = child.kill() {
                log::warn!(target: "RUN", "child cleanup kill: {error}");
            }
            if let Err(error) = child.wait() {
                log::warn!(target: "RUN", "child cleanup reap: {error}");
            }
        }
        crate::child_lifetime::release_bound_child(child.id());
    }
}

pub fn spawn_bound_child(
    cmd: &mut Command,
    scratch: &std::path::Path,
) -> std::io::Result<BoundChild> {
    if std::path::Path::new(cmd.get_program()).file_name()
        != Some(std::ffi::OsStr::new("systemd-run"))
    {
        return crate::child_lifetime::spawn_bound_child(cmd).map(|child| BoundChild(Some(child)));
    }
    spawn_scoped(cmd, scratch, std::time::Duration::from_secs(10))
}

fn spawn_scoped(
    cmd: &mut Command,
    scratch: &std::path::Path,
    budget: std::time::Duration,
) -> std::io::Result<BoundChild> {
    use bone_core::prelude::{Duration, Instant};
    use std::io::Read;
    let directory = tempfile::Builder::new()
        .prefix("scope-launch-")
        .tempdir_in(scratch)?;
    let receipt = directory.path().canonicalize()?.join("started");
    cmd.env("ROCK_SCOPE_START_RECEIPT", &receipt);
    let deadline = Instant::now() + budget;
    let mut attempts = 0;
    loop {
        attempts += 1;
        let mut child = BoundChild(Some(crate::child_lifetime::spawn_bound_child(cmd)?));
        loop {
            if receipt.try_exists()? {
                if attempts > 1 {
                    log::warn!(target: "RUN", "headless scope recovered after {attempts} launch attempts; inner command started once");
                }
                return Ok(child);
            }
            if let Some(status) = child.try_wait()? {
                // The receipt may have been written between the first check and process exit.
                if receipt.try_exists()? {
                    return Ok(child);
                }
                if status.code().is_none() || status.success() {
                    return Err(std::io::Error::other(format!(
                        "headless scope ended without a launch receipt ({status}); command will not be retried"
                    )));
                }
                break;
            }
            if Instant::now() >= deadline {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "headless scope launch timed out before execution was confirmed; command will not be retried",
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let mut diagnostic = String::new();
        if let Some(stderr) = child.stderr.take() {
            stderr.take(65536).read_to_string(&mut diagnostic)?;
        }
        drop(child);
        if Instant::now() >= deadline {
            return Err(std::io::Error::other(format!(
                "headless scope did not start the command after {attempts} launch attempts: {}",
                diagnostic.trim()
            )));
        }
        std::thread::sleep(
            Duration::from_millis(250).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

/// Names the likely cause when a scoped sim worker dies of SIGKILL the runner
/// did not send: the slice budget, shared with every other headless run on the
/// machine, was reached.
pub fn outside_kill_note(status: &ExitStatus, runner_killed: bool) -> Option<String> {
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
        inner
            .arg("main.lua")
            .env("OUT", "/tmp/x")
            .env_remove("ROCK_LOG")
            .current_dir("/work");
        let cmd = wrap(inner);

        assert_eq!(cmd.get_program(), OsStr::new("systemd-run"));
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let sep = args.iter().position(|a| a == "--").expect("-- separator");
        assert!(
            args[..sep].contains(&format!("--slice={HEADLESS_SLICE}")),
            "{args:?}"
        );
        assert!(
            args[..sep].iter().any(|a| a == "OOMPolicy=continue"),
            "{args:?}"
        );
        assert_eq!(&args[args.len() - 2..], ["/opt/luajit", "main.lua"]);
        assert_eq!(&args[sep + 1..sep + 3], ["sh", "-c"]);

        let envs: Vec<_> = cmd.get_envs().collect();
        assert!(envs.contains(&(OsStr::new("OUT"), Some(OsStr::new("/tmp/x")))));
        assert!(envs.contains(&(OsStr::new("ROCK_LOG"), None)));
        assert_eq!(cmd.get_current_dir(), Some(std::path::Path::new("/work")));
    }
    #[cfg(unix)]
    #[test]
    fn retries_only_before_the_inner_command_can_start() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Stdio;
        let dir = tempfile::tempdir().unwrap();
        let launcher = dir.path().join("systemd-run");
        let attempts = dir.path().join("attempts");
        let executions = dir.path().join("executions");
        std::fs::write(
            &launcher,
            r#"#!/bin/sh
if [ ! -e "$ATTEMPTS" ]; then
  echo failed > "$ATTEMPTS"
  echo 'Failed to connect to bus: Connection refused' >&2
  exit 1
fi
while [ "$1" != -- ]; do shift; done
shift
exec "$@"
"#,
        )
        .unwrap();
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut inner = Command::new("sh");
        inner.args(["-c", "echo executed >> \"$EXECUTIONS\"; echo child-output; echo 'Failed to connect to bus: Connection refused' >&2; exit 7"])
            .env("ATTEMPTS", &attempts).env("EXECUTIONS", &executions);
        let mut cmd = wrap_in(inner, Command::new(&launcher));
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = spawn_bound_child(&mut cmd, dir.path()).unwrap();
        let pid = child.id();
        let result = child.0.take().unwrap().wait_with_output().unwrap();
        crate::child_lifetime::release_bound_child(pid);
        assert_eq!(
            result.status.code(),
            Some(7),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(std::fs::read_to_string(executions).unwrap(), "executed\n");
        assert_eq!(result.stdout, b"child-output\n");
    }

    #[cfg(unix)]
    #[test]
    fn an_unavailable_manager_returns_a_launch_error_without_running_the_command() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Stdio;
        let dir = tempfile::tempdir().unwrap();
        let launcher = dir.path().join("systemd-run");
        let executed = dir.path().join("executed");
        std::fs::write(
            &launcher,
            "#!/bin/sh\necho 'manager unavailable' >&2\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut inner = Command::new("touch");
        inner.arg(&executed);
        let mut cmd = wrap_in(inner, Command::new(&launcher));
        cmd.stderr(Stdio::piped());
        let result = spawn_scoped(&mut cmd, dir.path(), std::time::Duration::from_secs(1));
        assert!(
            result.is_err(),
            "a scope-launch failure cannot be a running worker"
        );
        assert!(!executed.exists());
        assert!(!std::fs::read_dir(dir.path()).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("scope-launch-")
        }));
    }

    #[cfg(unix)]
    #[test]
    fn a_signalled_launcher_is_not_retried() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Stdio;
        let dir = tempfile::tempdir().unwrap();
        let launcher = dir.path().join("systemd-run");
        let attempts = dir.path().join("attempts");
        std::fs::write(
            &launcher,
            "#!/bin/sh\necho attempt >> \"$ATTEMPTS\"\nkill -TERM $$\n",
        )
        .unwrap();
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut inner = Command::new("true");
        inner.env("ATTEMPTS", &attempts);
        let mut cmd = wrap_in(inner, Command::new(&launcher));
        cmd.stderr(Stdio::piped());
        assert!(spawn_bound_child(&mut cmd, dir.path()).is_err());
        assert_eq!(std::fs::read_to_string(attempts).unwrap(), "attempt\n");
    }
}
