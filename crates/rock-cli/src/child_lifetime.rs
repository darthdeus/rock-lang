//! Children whose lifetime is bound to this launcher process.
//!
//! `rock run` hands the terminal to a `luajit` child and waits. A plain
//! `kill` of the launcher used to leave that child playing on with its
//! window and its agent-daemon session. Two mechanisms close that:
//! SIGTERM/SIGINT/SIGHUP to the launcher are forwarded to every bound child
//! before the launcher dies of the same signal, and on Linux the child asks
//! the kernel for SIGTERM when its parent thread dies by any route,
//! SIGKILL included.

use std::io;
use std::process::{Child, Command};
#[cfg(unix)]
use std::sync::Once;
#[cfg(unix)]
use std::sync::atomic::{AtomicI32, Ordering};

#[cfg(unix)]
const SLOTS: usize = 256;
// Read from a signal handler, so a lock-free table rather than a Mutex.
// Sized for sim workers: one slot per live worker child up to the jobs
// ceiling (hard-capped at the core count), plus the compile child.
#[cfg(unix)]
static BOUND: [AtomicI32; SLOTS] = [const { AtomicI32::new(0) }; SLOTS];
#[cfg(unix)]
static INSTALL: Once = Once::new();

#[cfg(unix)]
extern "C" fn forward_and_die(signal: libc::c_int) {
    for slot in BOUND.iter() {
        let pid = slot.load(Ordering::Relaxed);
        if pid > 0 {
            // Only async-signal-safe calls here.
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
        }
    }
    unsafe {
        libc::signal(signal, libc::SIG_DFL);
        libc::raise(signal);
    }
}

#[cfg(unix)]
fn install_forwarding() {
    INSTALL.call_once(|| {
        for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            unsafe {
                libc::signal(
                    signal,
                    forward_and_die as extern "C" fn(libc::c_int) as libc::sighandler_t,
                );
            }
        }
    });
}

#[cfg(unix)]
fn bind_slot(pid: u32) -> Result<(), io::Error> {
    for slot in BOUND.iter() {
        if slot
            .compare_exchange(0, pid as i32, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            return Ok(());
        }
    }
    Err(io::Error::other(format!(
        "more than {SLOTS} bound children alive at once"
    )))
}

/// Linux keeps this binding on the calling thread; a new thread starts unbound.
/// The process leader stays alive for the whole run, so the bound child's own
/// binding on it is enough.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
pub(crate) struct ParentDeathSignal {
    parent: libc::pid_t,
    signal: libc::c_int,
}

#[cfg(target_os = "linux")]
impl ParentDeathSignal {
    pub(crate) fn arm(self) -> io::Result<()> {
        if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, self.signal) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // Arming after the original parent exits would miss its death.
        if self.signal != 0 && unsafe { libc::getppid() } != self.parent {
            unsafe {
                libc::raise(self.signal);
            }
        }
        Ok(())
    }
}

/// Spawn `cmd` so that it does not outlive this process.
pub fn spawn_bound_child(cmd: &mut Command) -> io::Result<Child> {
    #[cfg(unix)]
    install_forwarding();
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        let binding = ParentDeathSignal {
            parent: unsafe { libc::getpid() },
            signal: libc::SIGTERM,
        };
        // Only async-signal-safe scalar syscalls run between fork and exec.
        unsafe {
            cmd.pre_exec(move || binding.arm());
        }
    }
    let child = cmd.spawn()?;
    #[cfg(unix)]
    bind_slot(child.id())?;
    rock_host::process_management::register_child_process(child.id());
    Ok(child)
}

/// Extend a long-lived bound child's binding to a launcher killed outright.
/// Linux already has this from the parent-death signal. macOS has none, so a
/// detached watcher kills the child's process group once this process is gone;
/// on Windows a kill-on-close job ends the child with this process.
pub(crate) fn guard_against_parent_kill(child: &Child) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    spawn_parent_watcher(std::process::id(), child.id());
    #[cfg(windows)]
    assign_kill_on_close_job(child)?;
    #[cfg(not(any(target_os = "macos", windows)))]
    let _ = child;
    Ok(())
}

#[cfg(target_os = "macos")]
fn spawn_parent_watcher(parent_pid: u32, child_pid: u32) {
    let script = format!(
        r#"#!/bin/bash
# Monitor parent PID {parent_pid} and kill child PID {child_pid} when parent dies
while kill -0 {parent_pid} 2>/dev/null; do
    kill -0 {child_pid} 2>/dev/null || exit 0
    sleep 0.1
done
kill -TERM {child_pid} 2>/dev/null
sleep 0.1
kill -KILL {child_pid} 2>/dev/null
kill -KILL -{child_pid} 2>/dev/null
"#
    );
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(script)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    // Double fork so the watcher is orphaned and outlives this process.
    unsafe {
        let pid = libc::fork();
        if pid == 0 {
            libc::setsid();
            if libc::fork() == 0 {
                if let Err(error) = cmd.spawn() {
                    log::error!(target: "STONE",
                        "failed to spawn the macOS parent watcher (parent {parent_pid}, child {child_pid}): {error}"
                    );
                }
            }
            // `_exit`, not `exit`: a fork of a multithreaded process must not
            // run the parent's exit handlers or flush its inherited stdio.
            libc::_exit(0);
        } else if pid > 0 {
            let mut status = 0;
            libc::waitpid(pid, &mut status, 0);
        }
    }
}

/// The job handle is leaked on purpose: Windows closes it when this process
/// exits by any route, and closing it kills every process in the job.
#[cfg(windows)]
fn assign_kill_on_close_job(child: &Child) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use winapi::um::{jobapi2, winnt};
    let failed =
        |call: &str| io::Error::other(format!("{call} failed: {}", io::Error::last_os_error()));
    unsafe {
        let job = jobapi2::CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
        if job.is_null() {
            return Err(failed("CreateJobObjectW"));
        }
        let mut info: winnt::JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = winnt::JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if jobapi2::SetInformationJobObject(
            job,
            winnt::JobObjectExtendedLimitInformation,
            &mut info as *mut _ as *mut _,
            std::mem::size_of::<winnt::JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) == 0
        {
            return Err(failed("SetInformationJobObject"));
        }
        let handle = child.as_raw_handle() as winapi::shared::ntdef::HANDLE;
        if jobapi2::AssignProcessToJobObject(job, handle) == 0 {
            return Err(failed("AssignProcessToJobObject"));
        }
    }
    Ok(())
}

/// Forget a bound child that has been waited for.
pub fn release_bound_child(pid: u32) {
    #[cfg(unix)]
    for slot in BOUND.iter() {
        let _ = slot.compare_exchange(pid as i32, 0, Ordering::SeqCst, Ordering::SeqCst);
    }
    rock_host::process_management::unregister_child_process(pid);
}

#[cfg(unix)]
pub(crate) fn pid_alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // `kill(pid, 0)` performs no signal; EPERM still means the process
    // exists (another user's), so only a clean error means dead.
    unsafe {
        libc::kill(pid, 0) == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

#[cfg(not(unix))]
pub(crate) fn pid_alive(_pid: u32) -> bool {
    // Without a liveness probe, keep PID-keyed artifacts.
    true
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    #[test]
    fn the_parent_watcher_exits_with_its_child() {
        let mut child = std::process::Command::new("sleep")
            .arg("0.2")
            .spawn()
            .unwrap();
        super::guard_against_parent_kill(&child).unwrap();
        let pid = child.id();
        child.wait().unwrap();
        let watcher_alive = || {
            std::process::Command::new("pgrep")
                .args(["-f", &format!("kill -TERM {pid} 2>")])
                .output()
                .unwrap()
                .status
                .success()
        };
        let deadline = bone_core::prelude::Instant::now() + std::time::Duration::from_secs(5);
        while watcher_alive() {
            assert!(
                bone_core::prelude::Instant::now() < deadline,
                "the watcher outlived its child and would later signal a reused PID"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}
