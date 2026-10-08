use std::cell::Cell;
use std::io::{self, Read};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

thread_local! {
    static DEADLINE: Cell<Option<Instant>> = const { Cell::new(None) };
}

pub struct SpawnBudget {
    previous: Option<Instant>,
    // A guard restores the thread on which it was entered.
    _thread: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl SpawnBudget {
    pub fn new(limit: Duration) -> Self {
        Self::enter(Instant::now() + limit)
    }

    pub fn for_test(seconds: u64) -> Self {
        // Reserve teardown time before nextest's full-profile kill deadline.
        Self::new(Duration::from_secs(seconds.saturating_sub(5)))
    }

    pub fn deadline() -> Instant {
        DEADLINE.with(|deadline| deadline.get().expect("heavy test must enter a SpawnBudget before spawning"))
    }

    pub fn enter(deadline: Instant) -> Self {
        let previous = DEADLINE.with(|current| current.replace(Some(
            current.get().map_or(deadline, |outer| outer.min(deadline))
        )));
        Self { previous, _thread: std::marker::PhantomData }
    }
}

impl Drop for SpawnBudget {
    fn drop(&mut self) {
        DEADLINE.with(|deadline| deadline.set(self.previous));
    }
}

pub trait HeavyCommandExt {
    fn output_with_deadline(&mut self) -> io::Result<Output>;
    fn spawn_with_deadline(&mut self) -> io::Result<BudgetedChild>;
}

impl HeavyCommandExt for Command {
    fn output_with_deadline(&mut self) -> io::Result<Output> {
        self.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        self.spawn_with_deadline()?.wait_with_output()
    }

    fn spawn_with_deadline(&mut self) -> io::Result<BudgetedChild> {
        let deadline = SpawnBudget::deadline();
        let label = format!("{:?} {:?}", self.get_program(), self.get_args().collect::<Vec<_>>());
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(io::ErrorKind::TimedOut,
                format!("spawn {label}: test wall budget exhausted before spawn; phase=spawn")));
        }
        // Preserve shorter explicit execution limits; leave defaults alone.
        let cap = remaining.as_secs().saturating_sub(1);
        if cap > 0 {
            for key in ["ROCK_FRAME_TIMEOUT_SECS", "ROCK_SIM_TIMEOUT_SECS"] {
                let requested = self.get_envs().find(|(name, _)| *name == key)
                    .and_then(|(_, value)| value).and_then(|value| value.to_str())
                    .and_then(|value| value.parse::<u64>().ok());
                if let Some(seconds) = requested {
                    self.env(key, seconds.min(cap).to_string());
                }
            }
        }
        let phase = tempfile::NamedTempFile::new()?;
        std::fs::write(phase.path(), "spawn")?;
        self.env("ROCK_SPAWN_PHASE_FILE", phase.path());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            self.process_group(0);
        }
        let mut child = self.spawn()?;
        let stdout = child.stdout.take().map(Drain::start);
        let stderr = child.stderr.take().map(Drain::start);
        Ok(BudgetedChild { child, deadline, label, phase, stdout, stderr, reaped: false })
    }
}

struct Drain {
    bytes: Arc<Mutex<Vec<u8>>>,
    reader: JoinHandle<io::Result<()>>,
}

impl Drain {
    fn start(mut pipe: impl Read + Send + 'static) -> Self {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let output = bytes.clone();
        let reader = std::thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            loop {
                let count = match pipe.read(&mut chunk) {
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => {
                        eprintln!("spawn pipe read failed: {error}");
                        return Err(error);
                    }
                    result => result?,
                };
                if count == 0 { return Ok(()); }
                output.lock().unwrap().extend_from_slice(&chunk[..count]);
            }
        });
        Self { bytes, reader }
    }

    fn snapshot(&self) -> Vec<u8> { self.bytes.lock().unwrap().clone() }
}

pub struct BudgetedChild {
    child: Child,
    deadline: Instant,
    label: String,
    phase: tempfile::NamedTempFile,
    stdout: Option<Drain>,
    stderr: Option<Drain>,
    reaped: bool,
}

impl BudgetedChild {
    pub fn kill(&mut self) -> io::Result<()> { self.stop() }

    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        use wait_timeout::ChildExt;
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        match self.child.wait_timeout(remaining)? {
            Some(status) => {
                self.reaped = true;
                Ok(status)
            }
            None => {
                let phase = match std::fs::read_to_string(self.phase.path()) {
                    Ok(phase) if !phase.trim().is_empty() => phase,
                    Ok(_) => "unknown (phase write incomplete)".to_string(),
                    Err(error) => format!("unknown (cannot read phase: {error})"),
                };
                self.stop()?;
                self.child.wait()?;
                self.reaped = true;
                Err(self.timeout(&phase))
            }
        }
    }

    fn timeout(&self, phase: &str) -> io::Error {
        let stdout = self.stdout.as_ref().map(Drain::snapshot).unwrap_or_default();
        let stderr = self.stderr.as_ref().map(Drain::snapshot).unwrap_or_default();
        io::Error::new(io::ErrorKind::TimedOut, format!(
            "spawn {} exceeded the remaining test wall budget; phase={} (slot wait + compile + execution)\nstdout:\n{}\nstderr:\n{}",
            self.label, phase.trim(), String::from_utf8_lossy(&stdout), String::from_utf8_lossy(&stderr)
        ))
    }

    pub fn wait_with_output(mut self) -> io::Result<Output> {
        let status = self.wait()?;
        // A descendant can hold a pipe after the launcher has exited.
        while [self.stdout.as_ref(), self.stderr.as_ref()].into_iter().flatten()
            .any(|drain| !drain.reader.is_finished())
        {
            if Instant::now() >= self.deadline {
                self.stop()?;
                return Err(self.timeout("output-drain"));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let finish = |drain: Option<Drain>| -> io::Result<Vec<u8>> {
            match drain {
                None => Ok(Vec::new()),
                Some(drain) => {
                    let bytes = drain.bytes.clone();
                    drain.reader.join().map_err(|_| io::Error::other("spawn pipe reader panicked"))??;
                    let output = bytes.lock().unwrap().clone();
                    Ok(output)
                }
            }
        };
        let stdout = finish(self.stdout.take())?;
        let stderr = finish(self.stderr.take())?;
        Ok(Output { status, stdout, stderr })
    }

    fn stop(&mut self) -> io::Result<()> {
        // Game runners may put their children in another process group. Capture
        // descendants before ending the launcher, while parent links still exist.
        let mut system = sysinfo::System::new();
        system.refresh_processes_specifics(sysinfo::ProcessesToUpdate::All, true, sysinfo::ProcessRefreshKind::nothing());
        let root = sysinfo::Pid::from_u32(self.child.id());
        let mut descendants = vec![root];
        let mut cursor = 0;
        while cursor < descendants.len() {
            let parent = descendants[cursor];
            for (pid, process) in system.processes() {
                if process.parent() == Some(parent) && !descendants.contains(pid) {
                    descendants.push(*pid);
                }
            }
            cursor += 1;
        }
        for pid in descendants.into_iter().rev().filter(|pid| *pid != root) {
            if let Some(process) = system.process(pid) {
                if !process.kill() {
                    eprintln!("spawn {}: could not kill descendant {pid}", self.label);
                }
            }
        }
        #[cfg(unix)]
        {
            let result = unsafe { libc::kill(-(self.child.id() as i32), libc::SIGKILL) };
            if result != 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) { return Err(error); }
            }
        }
        if !self.reaped {
            self.child.kill()?;
        }
        Ok(())
    }
}

impl Drop for BudgetedChild {
    fn drop(&mut self) {
        if !self.reaped {
            if let Err(error) = self.stop().and_then(|_| self.child.wait().map(|_| ())) {
                eprintln!("spawn {}: cleanup failed: {error}", self.label);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn child(action: &str) -> Command {
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args(["stone_harness::spawn::tests::spawn_helper", "--exact", "--nocapture"])
            .env("HEAVY_SPAWN_HELPER", action);
        cmd
    }

    #[test]
    fn spawn_helper() {
        match std::env::var("HEAVY_SPAWN_HELPER").as_deref() {
            Ok("sleep") => std::thread::sleep(Duration::from_secs(2)),
            Ok("short") => std::thread::sleep(Duration::from_millis(150)),
            Ok("descendant") => {
                let mut command = Command::new("sleep");
                command.arg("2");
                #[cfg(unix)]
                {
                    use std::os::unix::process::CommandExt;
                    command.process_group(0);
                }
                let mut child = command.spawn().unwrap();
                std::fs::write(std::env::var_os("HEAVY_DESCENDANT_PID").unwrap(), child.id().to_string()).unwrap();
                child.wait().unwrap();
            }
            Ok("flood") => {
                use std::io::Write;
                let data = vec![b'x'; 256 * 1024];
                std::io::stdout().write_all(&data).unwrap();
                std::io::stderr().write_all(&data).unwrap();
            }
            _ => {}
        }
    }

    #[test]
    fn whole_spawn_deadline_stops_a_slow_child_and_names_the_command() {
        let _budget = SpawnBudget::new(Duration::from_millis(250));
        let start = Instant::now();
        let error = child("sleep").output_with_deadline().expect_err("slow spawn must time out");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("spawn_helper"), "{error}");
        assert!(error.to_string().contains("phase="), "{error}");
        assert!(start.elapsed() < Duration::from_secs(1), "deadline did not stop the child");
    }

    #[test]
    fn spawn_drains_both_pipes_while_waiting() {
        let _budget = SpawnBudget::new(Duration::from_secs(5));
        let out = child("flood").output_with_deadline().unwrap();
        assert!(out.status.success());
        assert!(out.stdout.len() >= 256 * 1024);
        assert!(out.stderr.len() >= 256 * 1024);
    }

    #[test]
    fn later_spawns_cannot_restart_an_expired_test_budget() {
        let _budget = SpawnBudget::new(Duration::ZERO);
        let error = child("flood").output_with_deadline().expect_err("expired test must not spawn");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("before spawn"), "{error}");
    }

    #[test]
    fn sequential_spawns_share_the_remaining_test_budget() {
        let _budget = SpawnBudget::new(Duration::from_millis(250));
        assert!(child("short").output_with_deadline().unwrap().status.success());
        let error = child("short").output_with_deadline().expect_err("second child cannot restart the wall budget");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn spawn_budget_is_restored_after_an_assertion_unwinds() {
        let _outer = SpawnBudget::new(Duration::from_secs(5));
        assert!(std::panic::catch_unwind(|| {
            let _inner = SpawnBudget::new(Duration::ZERO);
            panic!("intentional fixture failure");
        }).is_err());
        assert!(child("flood").output_with_deadline().unwrap().status.success());
    }

    #[test]
    fn live_child_is_cleaned_up_before_fixture_failure_is_rethrown() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let failed = std::panic::catch_unwind(|| {
            let _budget = SpawnBudget::new(Duration::from_secs(5));
            let _child = child("descendant").env("HEAVY_DESCENDANT_PID", &pid_file)
                .spawn_with_deadline().unwrap();
            let deadline = Instant::now() + Duration::from_secs(1);
            while !pid_file.exists() {
                assert!(Instant::now() < deadline, "helper never spawned its child");
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("intentional live fixture failure");
        });
        let pid = std::fs::read_to_string(pid_file).unwrap();
        let gone = wait_gone(&pid, Duration::from_secs(1));
        if !gone {
            assert!(Command::new("kill").args(["-KILL", pid.trim()]).status().unwrap().success());
        }
        assert!(failed.is_err());
        assert!(gone, "descendant {pid} survived fixture failure");
    }

    fn wait_gone(pid: &str, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        loop {
            let out = Command::new("ps").args(["-p", pid.trim(), "-o", "stat="]).output().unwrap();
            let state = String::from_utf8_lossy(&out.stdout);
            if !out.status.success() || state.trim().starts_with('Z') { return true; }
            if Instant::now() >= deadline { return false; }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn deadline_stops_descendants_that_hold_the_output_pipes() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let _budget = SpawnBudget::new(Duration::from_millis(500));
        let result = child("descendant").env("HEAVY_DESCENDANT_PID", &pid_file).output_with_deadline();
        let pid = std::fs::read_to_string(pid_file).expect("helper reached descendant spawn");
        let gone = wait_gone(&pid, Duration::from_secs(1));
        if !gone {
            assert!(Command::new("kill").args(["-KILL", pid.trim()]).status().unwrap().success());
        }
        assert!(gone, "descendant {pid} survived deadline cleanup");
        assert_eq!(result.expect_err("descendant spawn must time out").kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn whole_spawn_deadline_covers_compile_slot_wait() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let slots = dir.path().join("slots");
        std::fs::create_dir(&project).unwrap();
        std::fs::create_dir(&slots).unwrap();
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap();
        std::fs::write(project.join("stick.toml"), format!(
            "[package]\nname = \"deadline-probe\"\nversion = \"0.1.0\"\ntype = \"bin\"\n[dependencies]\nstone = {{ path = {:?} }}\n[build.vars]\nGIT_SHA = \"deadline\"\n", root.join("stone")
        )).unwrap();
        std::fs::write(project.join("main.rock"), "fn main() {}\n@sim\nco fn deadline_probe() -> Coroutine<Wait> { stop_sim() }\n").unwrap();
        let mut slot = std::fs::File::options().create(true).truncate(false).read(true).write(true)
            .open(slots.join("batch-0")).unwrap();
        slot.lock().unwrap();
        writeln!(slot, "{} deadline test barrier", std::process::id()).unwrap();
        let _budget = SpawnBudget::new(Duration::from_secs(10));
        let mut pending = Command::new(env!("CARGO_BIN_EXE_rock"))
            .args(["sim", "--list"]).arg(&project)
            .env("ROCK_COMPILE_SLOT_DIR", &slots).env("ROCK_COMPILE_SLOTS", "1")
            .env("ROCK_COMPILE_CLASS", "batch").env("ROCK_SIM_COMPILE_CACHE", "0")
            .env_remove("ROCK_SIM_COMPILE_RECEIPT_DIR").env_remove("ROCK_SIM_COMPILE_RECEIPT_PREPARE")
            .stdout(Stdio::piped()).stderr(Stdio::piped())
            .current_dir(root).spawn_with_deadline().unwrap();
        let ready_deadline = Instant::now() + Duration::from_secs(10);
        while std::fs::read_to_string(pending.phase.path()).unwrap() != "slot-wait" {
            assert!(Instant::now() < ready_deadline, "compiler never reached the held slot");
            std::thread::sleep(Duration::from_millis(10));
        }
        pending.deadline = Instant::now() + Duration::from_millis(250);
        let started = Instant::now();
        let output = pending.wait_with_output();
        slot.unlock().unwrap();
        assert!(started.elapsed() < Duration::from_secs(2), "slot wait escaped its deadline");
        let error = output.expect_err("slot wait must consume the spawn budget");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("phase=slot-wait"), "{error}");
        drop(_budget);
        let _control_budget = SpawnBudget::new(Duration::from_secs(30));
        let control = Command::new(env!("CARGO_BIN_EXE_rock"))
            .args(["sim", "--list"]).arg(&project)
            .env("ROCK_COMPILE_SLOT_DIR", &slots).env("ROCK_COMPILE_SLOTS", "1")
            .env("ROCK_COMPILE_CLASS", "batch").env("ROCK_SIM_COMPILE_CACHE", "0")
            .env_remove("ROCK_SIM_COMPILE_RECEIPT_DIR").env_remove("ROCK_SIM_COMPILE_RECEIPT_PREPARE")
            .current_dir(root).output_with_deadline().unwrap();
        assert!(control.status.success(), "fixture must compile: {}", String::from_utf8_lossy(&control.stderr));
    }
}
