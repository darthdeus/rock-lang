//! THP-off is a compiler default, never an ambient runtime policy.
//!
//! On Linux the default is the kernel's per-process switch, set once the
//! command is known to be a compiler. mimalloc's own `allow_thp=0` does the
//! same prctl during allocator start; asking for it through the environment
//! meant restarting the whole binary.

use std::io;
use std::process::Command;

#[cfg(target_os = "linux")]
const PR_SET_THP_DISABLE: std::ffi::c_int = 41;
#[cfg(target_os = "linux")]
const PR_GET_THP_DISABLE: std::ffi::c_int = 42;
#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn prctl(option: std::ffi::c_int, ...) -> std::ffi::c_int;
}

/// The kernel THP-disable state this process had before the compiler default;
/// negative until `enter_compiler_process` applied it.
#[cfg(target_os = "linux")]
static INHERITED: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

/// Apply the compiler's THP-off default unless the caller chose through
/// `MIMALLOC_ALLOW_THP`, which mimalloc itself honors.
pub fn enter_compiler_process() -> io::Result<()> {
    if std::env::var_os("MIMALLOC_ALLOW_THP").is_some()
        || std::env::var_os("mimalloc_allow_thp").is_some()
    {
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    {
        use std::sync::atomic::Ordering;
        // SAFETY: scalar-only query; preserve an externally disabled parent too.
        let state = unsafe { prctl(PR_GET_THP_DISABLE, 0usize, 0usize, 0usize, 0usize) };
        if state < 0 {
            return Err(io::Error::last_os_error());
        }
        INHERITED.store(state, Ordering::Relaxed);
        // SAFETY: scalar-only call.
        if state == 0 && unsafe { prctl(PR_SET_THP_DISABLE, 1usize, 0usize, 0usize, 0usize) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Lua, C, JS and GLSL execution share this runtime spawn boundary.
pub fn configure_runtime_command(command: &mut Command) {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        let state = INHERITED.load(std::sync::atomic::Ordering::Relaxed);
        if state == 0 {
            // SAFETY: the forked child uses only scalar prctl and error construction.
            // PR_SET_THP_DISABLE survives exec.
            unsafe {
                command.pre_exec(|| {
                    if prctl(PR_SET_THP_DISABLE, 0usize, 0usize, 0usize, 0usize) != 0 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = command;
}
