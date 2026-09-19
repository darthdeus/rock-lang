//! A game spawned by `rock run` must not outlive the launcher: a SIGTERM to
//! the launcher (and on Linux any launcher death) ends the child too.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Re-entered by the tests below as a separate launcher process; a no-op in
/// the normal suite run.
#[test]
fn child_lifetime_helper_process() {
    if std::env::var("ROCK_CHILD_LIFETIME_HELPER").as_deref() != Ok("1") {
        return;
    }
    let mut child = rock_cli::child_lifetime::spawn_bound_child(Command::new("sleep").arg("300"))
        .expect("spawn sleep");
    println!("bound child pid {}", child.id());
    std::io::stdout().flush().unwrap();
    let _ = child.wait();
    rock_cli::child_lifetime::release_bound_child(child.id());
}

fn spawn_helper() -> (Child, i32) {
    let mut helper = Command::new(std::env::current_exe().unwrap())
        .args(["child_lifetime::child_lifetime_helper_process", "--exact", "--nocapture"])
        .env("ROCK_CHILD_LIFETIME_HELPER", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn helper launcher");
    let stdout = helper.stdout.take().unwrap();
    let mut lines = BufReader::new(stdout).lines();
    let pid = loop {
        let line = lines.next().expect("helper printed its child pid").expect("read helper stdout");
        if let Some(rest) = line.strip_prefix("bound child pid ") {
            break rest.trim().parse::<i32>().unwrap();
        }
    };
    (helper, pid)
}

fn process_gone(pid: i32) -> bool {
    if unsafe { libc::kill(pid, 0) } != 0 {
        return true;
    }
    // A zombie still answers kill(0); read its state so a reaped-later child
    // counts as ended once it has actually died.
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat.rsplit(')').next().map(|s| s.trim().starts_with('Z')).unwrap_or(true),
        // Linux exposes zombies through /proc; on platforms without /proc a
        // successful kill(0) above is the only portable evidence available.
        Err(_) => cfg!(target_os = "linux"),
    }
}

fn wait_gone(pid: i32, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if process_gone(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    process_gone(pid)
}

fn end_launcher_and_expect_child_gone(signal: libc::c_int) {
    let (mut helper, child_pid) = spawn_helper();
    assert!(!process_gone(child_pid), "the bound child should be running before the launcher dies");
    unsafe {
        libc::kill(helper.id() as i32, signal);
    }
    let _ = helper.wait();
    let gone = wait_gone(child_pid, Duration::from_secs(5));
    if !gone {
        unsafe {
            libc::kill(child_pid, libc::SIGKILL);
        }
    }
    assert!(gone, "child {child_pid} outlived a launcher ended by signal {signal}");
}

#[test]
fn sigterm_to_the_launcher_ends_the_bound_child() {
    end_launcher_and_expect_child_gone(libc::SIGTERM);
}

#[cfg(target_os = "linux")]
#[test]
fn sigkill_to_the_launcher_ends_the_bound_child() {
    end_launcher_and_expect_child_gone(libc::SIGKILL);
}
