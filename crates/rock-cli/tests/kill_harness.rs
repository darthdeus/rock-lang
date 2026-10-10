//! Kill a launcher process mid-run and assert on what it left behind.
//!
//! Linux-only: finding children and proving a pid is gone uses `/proc`, and
//! binding a child to its launcher's death needs the parent-death signal.

use std::process::{Child, Command};
use std::time::{Duration, Instant};

/// Whether `pid` is gone (a zombie counts: it has died and waits for a
/// reaper that is not this test).
fn process_gone(pid: i32) -> bool {
    if unsafe { libc::kill(pid, 0) } != 0 {
        return true;
    }
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat.rsplit(')').next().map(|s| s.trim().starts_with('Z')).unwrap_or(true),
        Err(_) => true,
    }
}

pub fn wait_gone(pid: i32, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if process_gone(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    process_gone(pid)
}

/// Direct children of `pid`, across all its tasks. Empty when the process is
/// gone (or unreadable), never an error.
pub fn children_of(pid: u32) -> Vec<u32> {
    let mut out = Vec::new();
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else { return out };
    for task in tasks.filter_map(|entry| entry.ok()) {
        let text = std::fs::read_to_string(task.path().join("children")).unwrap_or_default();
        out.extend(text.split_whitespace().filter_map(|word| word.parse::<u32>().ok()));
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// NUL-split `/proc/<pid>/cmdline`. Empty when the process is gone.
pub fn cmdline(pid: u32) -> Vec<String> {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .unwrap_or_default()
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect()
}

/// A child process in its own process group, reaped on drop: the group is
/// SIGKILLed and the launcher waited for, so a test failure (or a launcher
/// that orphans its worker) never leaks processes onto the machine. Drop
/// runs during assertion unwinds, before the failure propagates.
pub struct RunnerGuard {
    child: Child,
    pgid: u32,
}

impl RunnerGuard {
    /// The command must send stdio to files, never pipes: an orphaned child
    /// would hold test pipes open past the launcher's death.
    pub fn spawn(mut command: Command) -> Self {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
        let child = command.spawn().expect("spawn runner");
        let pgid = child.id();
        Self { child, pgid }
    }

    pub fn id(&self) -> u32 {
        self.child.id()
    }

    /// SIGKILL only the launcher (never the group: a bound child's death
    /// must come from its binding to the launcher, not from this test).
    pub fn kill_runner(&mut self) {
        unsafe {
            assert_eq!(libc::kill(self.id() as i32, libc::SIGKILL), 0, "kill runner");
        }
        assert!(wait_gone(self.id() as i32, Duration::from_secs(10)), "runner must die of SIGKILL");
        let _ = self.child.wait();
    }
}

impl Drop for RunnerGuard {
    fn drop(&mut self) {
        unsafe {
            libc::killpg(self.pgid as i32, libc::SIGKILL);
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) | Err(_) => return,
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            }
        }
    }
}
