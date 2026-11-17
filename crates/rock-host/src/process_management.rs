use log::{info, warn};
use std::sync::{Mutex, MutexGuard, PoisonError};
use sysinfo::{Pid, ProcessesToUpdate, System};

/// The running executable image, for self-spawn and compiler fingerprints.
/// Linux keeps this image available even after a build replaces its pathname.
/// Resource location and launching a newly built replacement use disk paths.
pub fn current_executable_image() -> std::io::Result<std::path::PathBuf> {
    #[cfg(target_os = "linux")]
    let executable = std::path::PathBuf::from("/proc/self/exe");
    #[cfg(not(target_os = "linux"))]
    let executable = std::env::current_exe()?;
    Ok(executable)
}

/// File a budgeting parent (the rock-cli test harness) names for this
/// process to report its current phase in, so each phase gets its own cap.
pub const SPAWN_PHASE_FILE_ENV: &str = "ROCK_SPAWN_PHASE_FILE";

pub fn report_spawn_phase(phase: &str) {
    let Some(path) = std::env::var_os(SPAWN_PHASE_FILE_ENV) else {
        return;
    };
    if let Err(error) = fs_err::write(&path, phase) {
        warn!(target: "CLI", "cannot record spawn phase {phase} at {}: {error}", std::path::Path::new(&path).display());
    }
}

/// Collect a child pipe drained by `thread`. A panicked reader or a failed
/// read is a harness failure: returning empty or truncated output instead
/// would let the caller judge the program on bytes it never fully saw.
pub fn join_drained_pipe(
    thread: std::thread::JoinHandle<std::io::Result<Vec<u8>>>,
    pipe: &str,
) -> Result<Vec<u8>, String> {
    match thread.join() {
        Ok(Ok(buf)) => Ok(buf),
        Ok(Err(e)) => Err(format!("failed to read the child's {pipe}: {e}")),
        Err(_) => Err(format!("the thread draining the child's {pipe} panicked")),
    }
}

/// Child processes to kill on exit.
static ACTIVE_CHILDREN: Mutex<Vec<u32>> = Mutex::new(Vec::new());

fn active_children() -> MutexGuard<'static, Vec<u32>> {
    ACTIVE_CHILDREN
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

pub fn register_child_process(pid: u32) {
    active_children().push(pid);
}

pub fn unregister_child_process(pid: u32) {
    active_children().retain(|&x| x != pid);
}

pub fn cleanup_all_children() {
    for &child in active_children().iter() {
        if kill_pid(child) {
            info!("Successfully killed child process with PID: {}", child);
        } else {
            warn!("Failed to kill child process with PID: {}", child);
        }
    }
}

/// Signal `pid` if it is still running; refreshes only that process.
fn kill_pid(pid: u32) -> bool {
    let pid = Pid::from_u32(pid);
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    system.process(pid).is_some_and(|process| process.kill())
}

#[cfg(all(test, unix))]
mod tests {
    // Kills only its own child: cleanup_all_children would also kill
    // children registered by tests running concurrently in this binary.
    #[test]
    fn kill_pid_kills_a_running_child() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        assert!(super::kill_pid(child.id()));
        assert!(!child.wait().unwrap().success());
    }
}
