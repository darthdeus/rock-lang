use log::{info, warn};
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use std::sync::Arc;
use sysinfo::{Pid, System};

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
    let Some(path) = std::env::var_os(SPAWN_PHASE_FILE_ENV) else { return };
    if let Err(error) = fs_err::write(&path, phase) {
        warn!(target: "CLI", "cannot record spawn phase {phase} at {}: {error}", std::path::Path::new(&path).display());
    }
}

// Global list to track active child processes for cleanup on exit
static ACTIVE_CHILDREN: Lazy<Arc<Mutex<Vec<u32>>>> = Lazy::new(|| Arc::new(Mutex::new(Vec::new())));

pub fn register_child_process(pid: u32) {
    ACTIVE_CHILDREN.lock().push(pid);
}

pub fn unregister_child_process(pid: u32) {
    ACTIVE_CHILDREN.lock().retain(|&x| x != pid);
}

pub fn cleanup_all_children() {
    for child in ACTIVE_CHILDREN.lock().iter() {
        if let Ok(ok) = kill_pid_crossplatform(*child as i32) {
            if ok {
                info!("Successfully killed child process with PID: {}", child);
            } else {
                warn!("Failed to kill child process with PID: {}", child);
            }
        } else {
            warn!(
                "Error occurred while trying to kill child process with PID: {}",
                child
            );
        }
    }
}

fn kill_pid_crossplatform(pid: i32) -> Result<bool, Box<dyn std::error::Error>> {
    // Build a System object that knows about current processes
    let mut sys = System::new_all();
    sys.refresh_all();

    // Convert our i32 to sysinfo's Pid type and see if it's running
    let target = Pid::from(pid as usize);
    if let Some(process) = sys.process(target) {
        // `kill()` returns `true` if the kill-signal was sent successfully
        let sent = process.kill();
        return Ok(sent);
    }
    Ok(false)
}
