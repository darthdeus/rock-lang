//! Run helper modes on the initial thread so the incoming binding is real.
#[cfg(target_os = "linux")]
mod linux {
    use std::path::Path;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    static REGISTRATIONS: rock_cli::registry::Registrations = rock_cli::registry::Registrations {
        early_entries: &[rock_cli::registry::EarlyEntry {
            name: "command",
            run: command_thread,
        }],
        ..rock_cli::plain::REGISTRATIONS
    };

    fn command_thread(args: Vec<String>) -> i32 {
        assert_eq!(std::thread::current().name(), Some("rock"));
        rock_host::compiler_thp::enter_compiler_process().expect("THP re-exec");
        assert_eq!(std::env::var("MIMALLOC_ALLOW_THP").as_deref(), Ok("0"));
        assert!(std::env::var_os("MIMALLOC_ROCK_COMPILER_THP_DEFAULT").is_some());
        let mut signal = 0;
        assert_eq!(unsafe { libc::prctl(libc::PR_GET_PDEATHSIG, &mut signal) }, 0);
        let ready = Path::new(&args[0]);
        let staging = ready.with_extension("staging");
        std::fs::write(&staging, format!("{} {signal}", std::process::id())).unwrap();
        std::fs::rename(staging, ready).unwrap();
        loop {
            std::thread::park();
        }
    }

    fn launcher(ready: &str) {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args(["command", ready])
            .env_remove("MIMALLOC_ALLOW_THP")
            .env_remove("mimalloc_allow_thp")
            .env_remove("MIMALLOC_ROCK_COMPILER_THP_DEFAULT");
        let mut child = rock_cli::child_lifetime::spawn_bound_child(&mut command).unwrap();
        std::fs::write(Path::new(ready).with_extension("pid"), child.id().to_string()).unwrap();
        let status = child.wait().unwrap();
        rock_cli::child_lifetime::release_bound_child(child.id());
        panic!("bound child exited before launcher death: {status}");
    }

    struct Processes {
        launcher: Child,
        child: Option<i32>,
    }

    impl Drop for Processes {
        fn drop(&mut self) {
            let _ = self.launcher.kill();
            let _ = self.launcher.wait();
            if let Some(pid) = self.child {
                unsafe { libc::kill(pid, libc::SIGKILL); }
                let mut status = 0;
                unsafe { libc::waitpid(pid, &mut status, 0); }
            }
        }
    }

    fn wait_until(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + test_utils::HANG_BACKSTOP;
        while !condition() {
            assert!(Instant::now() < deadline, "child-lifetime handshake timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn bound_command_reexec_keeps_parent_death_signal() {
        // Adopt the orphan ourselves so cleanup never depends on the host's init.
        assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) }, 0);
        let scratch = tempfile::tempdir().unwrap();
        let ready = scratch.path().join("ready");
        let launcher = Command::new(std::env::current_exe().unwrap())
            .arg("launcher").arg(&ready).stdin(Stdio::null()).spawn().unwrap();
        let mut processes = Processes { launcher, child: None };
        wait_until(|| {
            if let Ok(pid) = std::fs::read_to_string(ready.with_extension("pid")) {
                processes.child = Some(pid.parse().unwrap());
            }
            assert!(processes.launcher.try_wait().unwrap().is_none(), "launcher exited before readiness");
            processes.child.is_some() && ready.exists()
        });
        let record = std::fs::read_to_string(&ready).unwrap();
        let values: Vec<i32> = record.split_whitespace().map(|s| s.parse().unwrap()).collect();
        assert_eq!(values.len(), 2);
        let pid = values[0];
        assert_eq!(processes.child, Some(pid), "re-exec must preserve the bound child's PID");
        assert_eq!(values[1], libc::SIGTERM, "command thread lost PDEATHSIG across THP re-exec");
        assert!(Path::new(&format!("/proc/{pid}")).exists(), "child must be alive before launcher death");
        processes.launcher.kill().unwrap();
        processes.launcher.wait().unwrap();
        let mut status = 0;
        wait_until(|| {
            let result = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
            assert!(result >= 0, "wait for adopted child: {}", std::io::Error::last_os_error());
            result == pid
        });
        processes.child = None;
        assert!(libc::WIFSIGNALED(status), "child did not die from a signal: {status}");
        assert_eq!(libc::WTERMSIG(status), libc::SIGTERM);
        println!("test bound_command_reexec_keeps_parent_death_signal ... ok");
    }

    pub fn run() {
        let args: Vec<String> = std::env::args().collect();
        match args.get(1).map(String::as_str) {
            Some("launcher") => launcher(&args[2]),
            Some("command") => rock_cli::main_with(&REGISTRATIONS),
            _ => bound_command_reexec_keeps_parent_death_signal(),
        }
    }
}

fn main() {
    #[cfg(target_os = "linux")]
    linux::run();
}
