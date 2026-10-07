//! THP-off is a compiler default, never an ambient runtime policy.

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

const DEFAULT_MARKER: &str = "ROCK_COMPILER_THP_DEFAULT";

/// Restart before compilation so mimalloc sees the default during OS init.
/// Setting its option in main misses allocations made by Rust startup.
pub fn enter_compiler_process() -> io::Result<()> {
    if std::env::var_os("MIMALLOC_ALLOW_THP").is_some()
        || std::env::var_os("mimalloc_allow_thp").is_some()
    {
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    let inherited = {
        // SAFETY: scalar-only query; preserve an externally disabled parent too.
        let state = unsafe { prctl(PR_GET_THP_DISABLE, 0usize, 0usize, 0usize, 0usize) };
        if state < 0 {
            return Err(io::Error::last_os_error());
        }
        state
    };
    #[cfg(not(target_os = "linux"))]
    let inherited = 0;
    let mut command = Command::new(std::env::current_exe()?);
    command.args(std::env::args_os().skip(1))
        .env("MIMALLOC_ALLOW_THP", "0")
        .env(DEFAULT_MARKER, inherited.to_string());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(command.exec())
    }
    #[cfg(not(unix))]
    {
        let status = command.status()?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

/// Lua, C, JS and GLSL execution share this runtime spawn boundary.
pub fn configure_runtime_command(command: &mut Command) {
    let Some(inherited) = std::env::var_os(DEFAULT_MARKER) else { return; };
    command.env_remove(DEFAULT_MARKER);
    if std::env::var("MIMALLOC_ALLOW_THP").as_deref() != Ok("0")
        || std::env::var_os("mimalloc_allow_thp").is_some()
    {
        return;
    }
    command.env_remove("MIMALLOC_ALLOW_THP");
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        let state = inherited.to_str().and_then(|s| s.parse::<i32>().ok());
        // SAFETY: the forked child uses only scalar prctl and error construction.
        // PR_SET_THP_DISABLE survives exec, independently of mimalloc's env.
        unsafe {
            command.pre_exec(move || {
                let state = state.filter(|s| *s == 0 || *s == 1)
                    .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
                if prctl(PR_SET_THP_DISABLE, state as usize, 0usize, 0usize, 0usize) != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = inherited;
}
