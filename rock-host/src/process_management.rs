use log::{info, warn};
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use std::sync::Arc;
use sysinfo::{Pid, System};

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
