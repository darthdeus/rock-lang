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
use std::sync::atomic::{AtomicI32, Ordering};
#[cfg(unix)]
use std::sync::Once;

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
                libc::signal(signal, forward_and_die as extern "C" fn(libc::c_int) as libc::sighandler_t);
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
/// Preserve it before that thread can exec and replace the original leader.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
pub(crate) struct ParentDeathSignal {
    parent: libc::pid_t,
    signal: libc::c_int,
}

#[cfg(target_os = "linux")]
impl ParentDeathSignal {
    pub(crate) fn capture() -> io::Result<Self> {
        let mut signal = 0;
        if unsafe { libc::prctl(libc::PR_GET_PDEATHSIG, &mut signal) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { parent: unsafe { libc::getppid() }, signal })
    }

    pub(crate) fn arm(self) -> io::Result<()> {
        if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, self.signal) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // Arming after the original parent exits would miss its death.
        if self.signal != 0 && unsafe { libc::getppid() } != self.parent {
            unsafe { libc::raise(self.signal); }
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

/// Forget a bound child that has been waited for.
pub fn release_bound_child(pid: u32) {
    #[cfg(unix)]
    for slot in BOUND.iter() {
        let _ = slot.compare_exchange(pid as i32, 0, Ordering::SeqCst, Ordering::SeqCst);
    }
    rock_host::process_management::unregister_child_process(pid);
}
