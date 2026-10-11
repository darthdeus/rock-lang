//! Per-spawn phase budgets for heavy child processes.
//!
//! A heavy test enters one [`SpawnBudget`]; every child it spawns then gets
//! fresh per-phase caps derived from that budget instead of sharing one
//! absolute deadline. A slow-but-progressing spawn under load passes: the
//! slot wait survives while the compile queue moves, compiles get a generous
//! cap, an engine build gets the hang backstop, and execution gets the full
//! budget from when it starts. A hang still
//! fails, named with the phase it died in, within its phase cap.

use std::cell::Cell;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

thread_local! {
    static BUDGET: Cell<Option<Duration>> = const { Cell::new(None) };
}

/// How often [`BudgetedChild::wait`] checks the child and its reported phase.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Fresh cap for draining output pipes after the child exits. A descendant
/// can hold a pipe after the launcher is gone; this bounds that wait without
/// eating the phase budgets.
const DRAIN_CAP: Duration = Duration::from_secs(30);

/// Overall slot-wait multiple of the budget. With a moving queue a waiter can
/// legitimately outlast one budget; the stall cap is the hang bound, this one
/// only caps pathological starvation while others cycle through the slots.
const SLOT_OVERALL_MULTIPLE: u32 = 5;

/// Compile and cache-wait multiple of the budget: one generous compile, not
/// one shared deadline slice. Timeouts report the elapsed time so slow
/// compiles under load stay visible instead of silently passing.
const COMPILE_MULTIPLE: u32 = 2;

/// Per-spawn phase budgets for one heavy test.
///
/// The budget is a duration, not a deadline: each spawned child gets fresh
/// caps (spawn and execution: the budget; slot stall: the budget without
/// queue movement, overall: five budgets; compile and cache-wait: two
/// budgets). Sequential spawns never inherit each other's spent time, so a
/// test with several slow-but-progressing children passes. Nested budgets
/// take the tighter one; dropping the guard restores the outer budget, even
/// across an assertion unwind, so a fixture failure never leaks a zero budget
/// into its siblings.
pub struct SpawnBudget {
    previous: Option<Duration>,
    // A guard restores the thread on which it was entered.
    _thread: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl SpawnBudget {
    pub fn new(limit: Duration) -> Self {
        Self::enter(limit)
    }

    /// The per-phase caps are the hang detector; nextest's kill is only a
    /// backstop well beyond them (`.config/nextest.toml`).
    pub fn for_test(seconds: u64) -> Self {
        Self::new(Duration::from_secs(seconds))
    }

    pub fn budget() -> Duration {
        BUDGET.with(|budget| budget.get().expect("heavy test must enter a SpawnBudget before spawning"))
    }

    pub fn enter(limit: Duration) -> Self {
        let previous = BUDGET.with(|current| current.replace(Some(
            current.get().map_or(limit, |outer| outer.min(limit))
        )));
        Self { previous, _thread: std::marker::PhantomData }
    }
}

impl Drop for SpawnBudget {
    fn drop(&mut self) {
        BUDGET.with(|budget| budget.set(self.previous));
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
        let budget = SpawnBudget::budget();
        let label = format!("{:?} {:?}", self.get_program(), self.get_args().collect::<Vec<_>>());
        if budget.is_zero() {
            return Err(io::Error::new(io::ErrorKind::TimedOut,
                format!("spawn {label} refused: spawn budget is zero before spawn; phase=spawn")));
        }
        // Preserve shorter explicit execution limits. One second inside the
        // execution budget, so the child's own watchdog reports first instead
        // of racing the outer phase cap.
        let cap = budget.as_secs().saturating_sub(1);
        if cap > 0 {
            for key in ["ROCK_FRAME_TIMEOUT_SECS", "ROCK_SIM_TIMEOUT_SECS"] {
                let explicit = self.get_envs().find(|(name, _)| *name == key);
                let requested = explicit
                    .and_then(|(_, value)| value).and_then(|value| value.to_str())
                    .and_then(|value| value.parse::<u64>().ok());
                if let Some(seconds) = requested {
                    self.env(key, seconds.min(cap).to_string());
                } else if key == "ROCK_FRAME_TIMEOUT_SECS" && explicit.is_none()
                    && std::env::var_os(key).is_none()
                {
                    // The frame watchdog's 5 s default is an interactive hang
                    // check; under a budget the budget is the hang detector.
                    self.env(key, cap.to_string());
                }
            }
        }
        let phase = tempfile::NamedTempFile::new()?;
        std::fs::write(phase.path(), "spawn")?;
        self.env("ROCK_SPAWN_PHASE_FILE", phase.path());
        let slot_dir = effective_slot_dir(self);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            self.process_group(0);
        }
        let mut child = self.spawn()?;
        let (stdout, stdout_log) = budgeted_pipe(child.stdout.take(), "out")?;
        let (stderr, stderr_log) = budgeted_pipe(child.stderr.take(), "err")?;
        Ok(BudgetedChild { child, budget, spawned_at: Instant::now(), slot_dir, label, phase,
            stdout, stderr, stdout_log, stderr_log, reaped: false })
    }
}

/// A draining pipe plus the temp file it streams to. A child killed by the
/// test runner (rather than by its phase cap) takes its in-memory capture
/// with it; the streamed file survives for post-mortem attribution.
fn budgeted_pipe<T: Read + Send + 'static>(
    pipe: Option<T>,
    stream: &str,
) -> io::Result<(Option<Drain>, Option<tempfile::NamedTempFile>)> {
    let Some(pipe) = pipe else { return Ok((None, None)); };
    let log = tempfile::Builder::new().prefix(&format!("rock-spawn-{stream}-")).suffix(".log").tempfile()?;
    let drain = Drain::start(pipe, log.reopen()?);
    Ok((Some(drain), Some(log)))
}

/// The compile slot directory the child will wait on: its explicit override,
/// else the inherited environment, else the scheduler's home default. This
/// mirrors the scheduler's directory resolution (see `compile_slots.rs`) so
/// the parent watches the same queue the child joins. It must not change
/// what the child runs: no new environment is set from it.
fn effective_slot_dir(command: &Command) -> Option<PathBuf> {
    let mut explicit = None;
    for (key, value) in command.get_envs() {
        if key == "ROCK_COMPILE_SLOT_DIR" {
            explicit = Some(value.map(PathBuf::from));
        }
    }
    match explicit {
        Some(Some(dir)) if !dir.as_os_str().is_empty() => Some(dir),
        // Removed or empty: the child falls back to its home default too.
        Some(_) => default_slot_dir(),
        None => std::env::var_os("ROCK_COMPILE_SLOT_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(default_slot_dir),
    }
}

fn default_slot_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(|home| PathBuf::from(home).join("tmp/rock-compile-slots"))
}

struct Drain {
    bytes: Arc<Mutex<Vec<u8>>>,
    reader: JoinHandle<io::Result<()>>,
}

impl Drain {
    fn start(mut pipe: impl Read + Send + 'static, log: std::fs::File) -> Self {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let output = bytes.clone();
        let reader = std::thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            let mut log = Some(log);
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
                // Memory first, then disk: an empty snapshot always means an
                // empty file, so the keep decision never needs the file.
                if let Some(file) = log.as_mut() {
                    if let Err(error) = file.write_all(&chunk[..count]) {
                        eprintln!("spawn pipe log failed: {error}");
                        log = None;
                    }
                }
            }
        });
        Self { bytes, reader }
    }

    fn snapshot(&self) -> Vec<u8> { self.bytes.lock().unwrap().clone() }
}

/// One budgeted wait bucket. The child reports fine-grained phases; buckets
/// group them so a sub-step transition never restarts a cap.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PhaseBucket {
    Spawn,
    SlotWait,
    CacheWait,
    Compile,
    EngineBuild,
    Execution,
    OutputDrain,
    Unknown,
}

fn bucket_for(phase: &str) -> PhaseBucket {
    match phase {
        "spawn" => PhaseBucket::Spawn,
        "slot-wait" => PhaseBucket::SlotWait,
        "cache-wait" => PhaseBucket::CacheWait,
        "compile-prepare" | "compile" => PhaseBucket::Compile,
        "engine-build" => PhaseBucket::EngineBuild,
        "runtime-prepare" | "execution" => PhaseBucket::Execution,
        _ => PhaseBucket::Unknown,
    }
}

impl PhaseBucket {
    fn name(&self) -> &'static str {
        match self {
            PhaseBucket::Spawn => "spawn",
            PhaseBucket::SlotWait => "slot-wait",
            PhaseBucket::CacheWait => "cache-wait",
            PhaseBucket::Compile => "compile",
            PhaseBucket::EngineBuild => "engine-build",
            PhaseBucket::Execution => "execution",
            PhaseBucket::OutputDrain => "output-drain",
            PhaseBucket::Unknown => "unknown phase",
        }
    }

    fn cap(&self, budget: Duration) -> Duration {
        match self {
            PhaseBucket::Spawn | PhaseBucket::Execution | PhaseBucket::Unknown => budget,
            PhaseBucket::SlotWait => budget.saturating_mul(SLOT_OVERALL_MULTIPLE),
            PhaseBucket::CacheWait | PhaseBucket::Compile => budget.saturating_mul(COMPILE_MULTIPLE),
            // A cold cargo build of the engine scales with host load, not
            // with the test; only a hang should stop it. `rock test` builds
            // libstone after compiling and runs its tests in this bucket, so
            // it never gets less than the compile cap.
            PhaseBucket::EngineBuild => test_utils::HANG_BACKSTOP.max(budget.saturating_mul(COMPILE_MULTIPLE)),
            PhaseBucket::OutputDrain => DRAIN_CAP,
        }
    }
}

struct Breach {
    bucket: PhaseBucket,
    stall: bool,
    elapsed: Duration,
    cap: Duration,
}

impl Breach {
    fn describe(&self) -> String {
        if self.stall {
            format!("{} with a stalled queue ({:?} without queue movement of {:?} stall cap)",
                self.bucket.name(), self.elapsed, self.cap)
        } else {
            format!("{} ({:?} elapsed of {:?} cap)", self.bucket.name(), self.elapsed, self.cap)
        }
    }
}

/// The current holder record of every slot file. Any change (an acquisition
/// rewrites its record) proves the queue is moving. Releases alone are
/// silent, but a released slot is taken within one retry interval, so a live
/// queue always shows turnover.
fn snapshot_slot_queue(dir: Option<&Path>) -> Option<Vec<(String, Vec<u8>)>> {
    let dir = dir?;
    let mut holders = Vec::new();
    for item in std::fs::read_dir(dir).ok()? {
        let item = item.ok()?;
        holders.push((
            item.file_name().to_string_lossy().into_owned(),
            std::fs::read(item.path()).unwrap_or_default(),
        ));
    }
    holders.sort();
    Some(holders)
}

/// The child's latest reported phase, if it has completed a write. The child
/// writes with a plain truncating write, so an empty read is a torn write,
/// not a phase.
fn read_spawn_phase(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let trimmed = content.trim();
    if trimmed.is_empty() { None } else { Some(trimmed.to_string()) }
}

struct WaitState {
    current: PhaseBucket,
    /// One timer per bucket, started at first observation and never reset:
    /// flapping between phases must not extend any cap.
    started: [Option<Instant>; 8],
    slot_progress_at: Instant,
    queue: Option<Vec<(String, Vec<u8>)>>,
    slot_dir: Option<PathBuf>,
}

impl WaitState {
    fn new(spawned_at: Instant, slot_dir: Option<PathBuf>) -> Self {
        let mut started = [None; 8];
        started[PhaseBucket::Spawn as usize] = Some(spawned_at);
        Self { current: PhaseBucket::Spawn, started, slot_progress_at: Instant::now(), queue: None, slot_dir }
    }

    fn advance(&mut self, phase: Option<&str>, budget: Duration) -> Result<(), Breach> {
        let now = Instant::now();
        if let Some(name) = phase {
            let bucket = bucket_for(name);
            if bucket != self.current {
                self.current = bucket;
                let started = &mut self.started[bucket as usize];
                if started.is_none() {
                    *started = Some(now);
                }
                // Any transition proves the child alive; a flap back into the
                // slot queue must not inherit a stale stall clock.
                self.slot_progress_at = now;
            }
        }
        let start = self.started[self.current as usize].expect("current bucket timer");
        let cap = self.current.cap(budget);
        let elapsed = now.saturating_duration_since(start);
        if elapsed >= cap {
            return Err(Breach { bucket: self.current, stall: false, elapsed, cap });
        }
        if self.current == PhaseBucket::SlotWait {
            let snapshot = snapshot_slot_queue(self.slot_dir.as_deref());
            if snapshot != self.queue {
                self.queue = snapshot;
                self.slot_progress_at = now;
            }
            let idle = now.saturating_duration_since(self.slot_progress_at);
            if idle >= budget {
                return Err(Breach { bucket: self.current, stall: true, elapsed: idle, cap: budget });
            }
        }
        Ok(())
    }
}

pub struct BudgetedChild {
    child: Child,
    budget: Duration,
    spawned_at: Instant,
    slot_dir: Option<PathBuf>,
    label: String,
    phase: tempfile::NamedTempFile,
    stdout: Option<Drain>,
    stderr: Option<Drain>,
    stdout_log: Option<tempfile::NamedTempFile>,
    stderr_log: Option<tempfile::NamedTempFile>,
    reaped: bool,
}

/// The kept-log report shared by the timeout error and the stderr line.
fn kept_section(out: &Option<PathBuf>, err: &Option<PathBuf>) -> String {
    let show = |kept: &Option<PathBuf>| kept.as_ref()
        .map(|path| path.display().to_string()).unwrap_or_else(|| "(none)".to_string());
    format!("child output kept at:\n  out: {}\n  err: {}", show(out), show(err))
}

/// (stdout log, stderr log) named by a timeout error, if it kept any.
pub fn kept_log_paths(error: &str) -> (Option<PathBuf>, Option<PathBuf>) {
    let mut lines = error.lines();
    if !lines.any(|line| line == "child output kept at:") {
        return (None, None);
    }
    let path = |line: Option<&str>, prefix: &str| {
        line.and_then(|line| line.strip_prefix(prefix))
            .filter(|path| *path != "(none)")
            .map(PathBuf::from)
    };
    (path(lines.next(), "  out: "), path(lines.next(), "  err: "))
}

/// Remove the logs a timeout error kept. Tests that PROVE a timeout call
/// this, so green runs leave nothing behind; unexpected timeouts keep their
/// files for post-mortem.
pub fn discard_kept_logs(error: &io::Error) {
    let (out, err) = kept_log_paths(&error.to_string());
    for path in out.into_iter().chain(err) {
        let _ = std::fs::remove_file(path);
    }
}

/// Persist a streamed log that captured output; empty and unpiped streams
/// carry no diagnosis and their temp files delete themselves.
fn keep_stream_log(drain: Option<&Drain>, log: Option<tempfile::NamedTempFile>) -> Option<PathBuf> {
    let drain = drain?;
    let log = log?;
    if drain.snapshot().is_empty() {
        return None;
    }
    match log.into_temp_path().keep() {
        Ok(path) => Some(path),
        Err(error) => {
            eprintln!("spawn log keep failed: {error}");
            None
        }
    }
}

impl BudgetedChild {
    pub fn kill(&mut self) -> io::Result<()> { self.stop() }

    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        let mut state = WaitState::new(self.spawned_at, self.slot_dir.clone());
        let mut reported = None;
        let mut busy_reported = false;
        loop {
            if let Some(status) = self.child.try_wait()? {
                self.reaped = true;
                return Ok(status);
            }
            let phase = read_spawn_phase(self.phase.path());
            let advanced = state.advance(phase.as_deref(), self.budget);
            // Phase lines go to the test's own stderr, so a test that nextest
            // kills still says which phase its child was in and since when.
            if reported != Some(state.current) {
                reported = Some(state.current);
                eprintln!("spawn {}: phase={} at {:.1?}", self.label,
                    phase.as_deref().unwrap_or(state.current.name()), self.spawned_at.elapsed());
            }
            if state.current == PhaseBucket::SlotWait && !busy_reported {
                if let Some(busy) = self.slot_busy_line() {
                    busy_reported = true;
                    eprintln!("spawn {}: {busy}", self.label);
                }
            }
            if let Err(breach) = advanced {
                let name = phase.as_deref().unwrap_or("unknown");
                self.stop()?;
                self.child.wait()?;
                self.reaped = true;
                let kept = self.keep_logs();
                return Err(self.timeout(name, &breach, &kept));
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// The child's "waiting for a compile slot ...; busy: <holders>" log line
    /// (`compile_slots::acquire_with`), when its stderr is piped.
    fn slot_busy_line(&self) -> Option<String> {
        let stderr = self.stderr.as_ref()?.snapshot();
        String::from_utf8_lossy(&stderr).lines().rev()
            .find(|line| line.contains("waiting for a compile slot"))
            .map(|line| line.trim().to_string())
    }

    /// Persist each streamed log that captured output and name the kept
    /// paths on the test's stderr, so a runner kill that takes this error
    /// with it still leaves attributed files behind.
    fn keep_logs(&mut self) -> (Option<PathBuf>, Option<PathBuf>) {
        let kept = (
            keep_stream_log(self.stdout.as_ref(), self.stdout_log.take()),
            keep_stream_log(self.stderr.as_ref(), self.stderr_log.take()),
        );
        if kept.0.is_some() || kept.1.is_some() {
            eprintln!("spawn {}: {}", self.label, kept_section(&kept.0, &kept.1));
        }
        kept
    }

    fn timeout(&self, phase: &str, breach: &Breach, kept: &(Option<PathBuf>, Option<PathBuf>)) -> io::Error {
        let stdout = self.stdout.as_ref().map(Drain::snapshot).unwrap_or_default();
        let stderr = self.stderr.as_ref().map(Drain::snapshot).unwrap_or_default();
        let mut message = format!(
            "spawn {} exceeded its {} budget; phase={}",
            self.label, breach.describe(), phase.trim());
        if kept.0.is_some() || kept.1.is_some() {
            message.push('\n');
            message.push_str(&kept_section(&kept.0, &kept.1));
        }
        message.push_str(&format!(
            "\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&stdout), String::from_utf8_lossy(&stderr)));
        io::Error::new(io::ErrorKind::TimedOut, message)
    }

    pub fn wait_with_output(mut self) -> io::Result<Output> {
        let status = self.wait()?;
        // A descendant can hold a pipe after the launcher has exited.
        let drain_deadline = Instant::now() + DRAIN_CAP;
        while [self.stdout.as_ref(), self.stderr.as_ref()].into_iter().flatten()
            .any(|drain| !drain.reader.is_finished())
        {
            if Instant::now() >= drain_deadline {
                self.stop()?;
                let kept = self.keep_logs();
                return Err(self.timeout("output-drain", &Breach {
                    bucket: PhaseBucket::OutputDrain, stall: false, elapsed: DRAIN_CAP, cap: DRAIN_CAP,
                }, &kept));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        // The returned Output carries every byte, so the streamed files drop
        // with their temp files: success keeps nothing behind.
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
            // A fixture failure that abandons a live child keeps whatever it
            // had streamed, so the teardown never discards the evidence.
            let _ = self.keep_logs();
        }
    }
}

/// Copy `source` to `destination` in a single-threaded `cp` child, adopting
/// the source's permission bits. Use for every file the test will execute.
///
/// Copying an executable with `std::fs::copy` while other threads spawn is
/// racy on Linux: a clone made during the copy inherits the copy's write fd,
/// so the copier's close is not the last fput and the kernel drops the
/// inode's write count only when that child drains. An exec landing in that
/// window fails with ETXTBSY ("Text file busy"). Temp-name-plus-rename does
/// not help (same inode), and O_CLOEXEC is already set (it acts at exec,
/// after the check). A `cp` child holds the write fd in its own fd table and
/// is waited for, so the write count is zero before this returns.
pub fn copy_executable(source: &Path, destination: &Path) {
    run_cp(source, destination);
    let permissions = std::fs::metadata(source)
        .expect("read copied executable permissions")
        .permissions();
    std::fs::set_permissions(destination, permissions)
        .expect("preserve copied executable permissions");
}

/// Replace the bytes of an executed file, keeping its permission bits. The
/// new content is staged under a temp name (never executed) and moved into
/// place by a `cp` child, so no write fd to the executed inode ever exists
/// in this multithreaded process; see [`copy_executable`].
pub fn overwrite_executable(destination: &Path, bytes: &[u8]) {
    let permissions = std::fs::metadata(destination)
        .expect("read executed file permissions")
        .permissions();
    let staged = destination.with_added_extension("staged");
    std::fs::write(&staged, bytes).expect("stage executable replacement");
    run_cp(&staged, destination);
    std::fs::set_permissions(destination, permissions)
        .expect("preserve executed file permissions");
}

fn run_cp(source: &Path, destination: &Path) {
    let status = Command::new("cp")
        .arg(source)
        .arg(destination)
        .status()
        .unwrap_or_else(|error| {
            panic!("spawn cp {} -> {}: {error}", source.display(), destination.display())
        });
    assert!(
        status.success(),
        "cp {} -> {} failed: {status}",
        source.display(),
        destination.display()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    use test_utils::HANG_BACKSTOP;

    /// Scheduling slack for upper bounds on how fast a cap fires. These tests
    /// prove that a cap fires and names its phase, not how promptly a loaded
    /// host runs the poll loop.
    const LOAD_MARGIN: Duration = Duration::from_secs(30);

    /// Per-stream markers the print_and_hang helper flushes before hanging.
    const OUT_MARKER: &str = "spawn-log-marker-out";
    const ERR_MARKER: &str = "spawn-log-marker-err";

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
            Ok("execute_11s") => {
                let file = std::env::var_os("ROCK_SPAWN_PHASE_FILE").expect("phase file env");
                std::fs::write(file, "execution").unwrap();
                std::thread::sleep(Duration::from_secs(11));
            }
            Ok("engine_build_12s") => {
                let file = std::env::var_os("ROCK_SPAWN_PHASE_FILE").expect("phase file env");
                std::fs::write(file, "engine-build").unwrap();
                std::thread::sleep(Duration::from_secs(12));
            }
            Ok("descendant") => {
                let mut command = Command::new("sleep");
                command.arg("60");
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
            Ok("print_and_hang") => {
                use std::io::Write;
                let file = std::env::var_os("ROCK_SPAWN_PHASE_FILE").expect("phase file env");
                std::fs::write(file, "execution").unwrap();
                println!("{OUT_MARKER}");
                eprintln!("{ERR_MARKER}");
                std::io::stdout().flush().unwrap();
                std::io::stderr().flush().unwrap();
                std::thread::sleep(Duration::from_secs(30));
            }
            Ok(action) if action.starts_with("hang_in:") => {
                let phase = action.trim_start_matches("hang_in:");
                let file = std::env::var_os("ROCK_SPAWN_PHASE_FILE").expect("phase file env");
                std::fs::write(file, phase).unwrap();
                std::thread::sleep(Duration::from_secs(30));
            }
            _ => {}
        }
    }

    #[test]
    fn spawn_cap_stops_a_child_that_never_reports_a_phase() {
        let _budget = SpawnBudget::new(Duration::from_millis(250));
        let start = Instant::now();
        let error = child("sleep").output_with_deadline().expect_err("slow spawn must time out");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("spawn_helper"), "{error}");
        assert!(error.to_string().contains("phase=spawn"), "{error}");
        assert!(start.elapsed() < Duration::from_millis(250) + LOAD_MARGIN, "spawn cap did not stop the child");
        discard_kept_logs(&error);
    }

    #[test]
    fn spawn_drains_both_pipes_while_waiting() {
        let _budget = SpawnBudget::new(HANG_BACKSTOP);
        let out = child("flood").output_with_deadline().unwrap();
        assert!(out.status.success());
        assert!(out.stdout.len() >= 256 * 1024);
        assert!(out.stderr.len() >= 256 * 1024);
    }

    /// Removes kept log files when the test leaves, pass or fail, so a
    /// failing assertion never litters the shared temp directory.
    struct ScopedLogs(Vec<PathBuf>);

    impl Drop for ScopedLogs {
        fn drop(&mut self) {
            for path in &self.0 {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    /// The drain threads stream pipe bytes to disk; they can lag the reaped
    /// child by a poll interval, so wait for the marker instead of reading once.
    fn await_log_marker(path: &Path, marker: &str) {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Ok(content) = std::fs::read_to_string(path) {
                if content.contains(marker) {
                    return;
                }
            }
            assert!(Instant::now() < deadline, "log {path:?} never streamed {marker}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn timed_out_spawn_keeps_its_streamed_output_in_named_logs() {
        let _budget = SpawnBudget::new(HANG_BACKSTOP);
        let mut pending = child("print_and_hang").stdin(Stdio::null())
            .stdout(Stdio::piped()).stderr(Stdio::piped()).spawn_with_deadline().unwrap();
        let deadline = Instant::now() + HANG_BACKSTOP;
        while read_spawn_phase(pending.phase.path()).as_deref() != Some("execution") {
            assert!(Instant::now() < deadline, "helper never reached execution");
            std::thread::sleep(Duration::from_millis(10));
        }
        pending.budget = Duration::from_millis(250);
        let error = pending.wait_with_output().expect_err("a hung child must time out");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        let (Some(out_log), Some(err_log)) = kept_log_paths(&error.to_string()) else {
            panic!("timeout names no kept logs: {error}");
        };
        assert_ne!(out_log, err_log, "streams share one log file: {error}");
        let _cleanup = ScopedLogs(vec![out_log.clone(), err_log.clone()]);
        await_log_marker(&out_log, OUT_MARKER);
        await_log_marker(&err_log, ERR_MARKER);
        let out = std::fs::read_to_string(&out_log).unwrap();
        let err = std::fs::read_to_string(&err_log).unwrap();
        assert!(out.contains(OUT_MARKER), "stdout log lacks its marker: {out:?}");
        assert!(err.contains(ERR_MARKER), "stderr log lacks its marker: {err:?}");
    }

    #[test]
    fn abandoned_spawn_keeps_what_it_had_streamed() {
        let _budget = SpawnBudget::new(HANG_BACKSTOP);
        let pending = child("print_and_hang").stdin(Stdio::null())
            .stdout(Stdio::piped()).stderr(Stdio::piped()).spawn_with_deadline().unwrap();
        let out_log = pending.stdout_log.as_ref().expect("piped stdout streams to a log")
            .path().to_path_buf();
        let err_log = pending.stderr_log.as_ref().expect("piped stderr streams to a log")
            .path().to_path_buf();
        // The markers reach disk while the child still sleeps: a runner kill
        // at any point from here leaves attributed files behind.
        await_log_marker(&out_log, OUT_MARKER);
        await_log_marker(&err_log, ERR_MARKER);
        drop(pending);
        let _cleanup = ScopedLogs(vec![out_log.clone(), err_log.clone()]);
        let out = std::fs::read_to_string(&out_log).unwrap();
        let err = std::fs::read_to_string(&err_log).unwrap();
        assert!(out.contains(OUT_MARKER), "kept stdout log lacks its marker: {out:?}");
        assert!(err.contains(ERR_MARKER), "kept stderr log lacks its marker: {err:?}");
    }

    #[test]
    fn successful_spawn_removes_its_streamed_logs() {
        let _budget = SpawnBudget::new(HANG_BACKSTOP);
        let pending = child("flood").stdin(Stdio::null())
            .stdout(Stdio::piped()).stderr(Stdio::piped()).spawn_with_deadline().unwrap();
        let out_log = pending.stdout_log.as_ref().expect("piped stdout streams to a log")
            .path().to_path_buf();
        let err_log = pending.stderr_log.as_ref().expect("piped stderr streams to a log")
            .path().to_path_buf();
        let out = pending.wait_with_output().unwrap();
        assert!(out.status.success());
        // The returned Output carries every byte; the streamed files were
        // only insurance against a kill.
        assert!(!out_log.exists(), "successful spawn left its stdout log");
        assert!(!err_log.exists(), "successful spawn left its stderr log");
    }

    #[test]
    fn reaped_direct_wait_leaves_no_logs_behind() {
        let _budget = SpawnBudget::new(HANG_BACKSTOP);
        let mut pending = child("print_and_hang").stdin(Stdio::null())
            .stdout(Stdio::piped()).stderr(Stdio::piped()).spawn_with_deadline().unwrap();
        let out_log = pending.stdout_log.as_ref().expect("piped stdout streams to a log")
            .path().to_path_buf();
        let err_log = pending.stderr_log.as_ref().expect("piped stderr streams to a log")
            .path().to_path_buf();
        // Non-empty files, so a keep would trigger: reaping must still
        // auto-delete, since direct wait() discards output by design.
        await_log_marker(&out_log, OUT_MARKER);
        pending.kill().expect("stop the hanging child");
        let status = pending.wait().expect("reap the killed child");
        assert!(!status.success());
        drop(pending);
        assert!(!out_log.exists(), "reaped spawn left its stdout log");
        assert!(!err_log.exists(), "reaped spawn left its stderr log");
    }

    #[test]
    fn later_spawns_cannot_restart_an_expired_test_budget() {
        let _budget = SpawnBudget::new(Duration::ZERO);
        let error = child("flood").output_with_deadline().expect_err("expired test must not spawn");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("before spawn"), "{error}");
    }

    #[test]
    fn sequential_spawns_each_get_a_fresh_execution_budget() {
        // Two 11 s executions under a 20 s budget: a shared deadline would kill
        // the second, but per-spawn caps grant each its own budget. Each child
        // has 20 s to start and 9 s to exit after its sleep.
        let _budget = SpawnBudget::new(Duration::from_secs(20));
        assert!(child("execute_11s").output_with_deadline().unwrap().status.success());
        assert!(child("execute_11s").output_with_deadline().unwrap().status.success());
    }

    #[test]
    fn spawn_budget_is_restored_after_an_assertion_unwinds() {
        let _outer = SpawnBudget::new(HANG_BACKSTOP);
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
            let deadline = Instant::now() + HANG_BACKSTOP;
            while !pid_file.exists() {
                assert!(Instant::now() < deadline, "helper never spawned its child");
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("intentional live fixture failure");
        });
        let pid = std::fs::read_to_string(pid_file).unwrap();
        let gone = wait_gone(&pid, HANG_BACKSTOP);
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
    fn budget_stops_descendants_that_hold_the_output_pipes() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let _budget = SpawnBudget::new(Duration::from_secs(120));
        let mut pending = child("descendant").env("HEAVY_DESCENDANT_PID", &pid_file)
            .stdout(Stdio::piped()).stderr(Stdio::piped()).spawn_with_deadline().unwrap();
        let deadline = Instant::now() + HANG_BACKSTOP;
        while !pid_file.exists() {
            assert!(Instant::now() < deadline, "helper never spawned its child");
            std::thread::sleep(Duration::from_millis(10));
        }
        // Expire the budget only once the descendant holds the pipes.
        pending.budget = Duration::from_millis(250);
        let result = pending.wait_with_output();
        let pid = std::fs::read_to_string(pid_file).expect("helper reached descendant spawn");
        let gone = wait_gone(&pid, HANG_BACKSTOP);
        if !gone {
            assert!(Command::new("kill").args(["-KILL", pid.trim()]).status().unwrap().success());
        }
        assert!(gone, "descendant {pid} survived deadline cleanup");
        let error = result.expect_err("descendant spawn must time out");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        discard_kept_logs(&error);
    }

    fn assert_hang_reports_phase_within(phase: &str, budget: Duration, cap: Duration) {
        let _budget = SpawnBudget::new(HANG_BACKSTOP);
        let mut pending = child(&format!("hang_in:{phase}")).stdin(Stdio::null())
            .stdout(Stdio::piped()).stderr(Stdio::piped()).spawn_with_deadline().unwrap();
        // The helper's own start belongs to the spawn cap, not to the phase
        // under test: apply the short budget once the child is in the phase.
        let deadline = Instant::now() + HANG_BACKSTOP;
        while read_spawn_phase(pending.phase.path()).as_deref() != Some(phase) {
            assert!(Instant::now() < deadline, "helper never reported {phase}");
            std::thread::sleep(Duration::from_millis(10));
        }
        pending.budget = budget;
        let start = Instant::now();
        let error = pending.wait_with_output().expect_err("a hung child must time out");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(error.to_string().contains(&format!("phase={phase}")), "{error}");
        assert!(start.elapsed() >= cap, "hang in {phase} died before its cap: {:?}", start.elapsed());
        assert!(start.elapsed() < cap + LOAD_MARGIN, "hang in {phase} escaped its cap: {:?}", start.elapsed());
        discard_kept_logs(&error);
    }

    #[test]
    fn execution_hang_fails_with_the_named_phase_within_its_cap() {
        assert_hang_reports_phase_within("execution", Duration::from_millis(250), Duration::from_millis(250));
    }

    #[test]
    fn compile_hang_reports_the_compile_phase_within_its_cap() {
        assert_hang_reports_phase_within("compile", Duration::from_millis(250), Duration::from_millis(500));
    }

    #[test]
    fn cache_wait_hang_reports_the_cache_wait_phase_within_its_cap() {
        assert_hang_reports_phase_within("cache-wait", Duration::from_millis(250), Duration::from_millis(500));
    }

    /// An engine build is a cargo build: a cold one under load outlasts any
    /// multiple of a test's budget while it progresses.
    #[test]
    fn an_engine_build_outlasting_the_compile_cap_is_not_a_hang() {
        let _budget = SpawnBudget::new(Duration::from_secs(5));
        let output = child("engine_build_12s").output_with_deadline()
            .expect("an engine build past twice the budget must not time out");
        assert!(output.status.success(), "{output:?}");
    }

    #[test]
    fn queue_snapshot_changes_only_when_a_holder_record_changes() {
        let dir = tempfile::tempdir().unwrap();
        assert!(snapshot_slot_queue(None).is_none());
        assert!(snapshot_slot_queue(Some(&dir.path().join("missing"))).is_none());
        std::fs::write(dir.path().join("batch-0"), "pid=1 start=1").unwrap();
        let first = snapshot_slot_queue(Some(dir.path())).unwrap();
        assert_eq!(snapshot_slot_queue(Some(dir.path())).unwrap(), first);
        std::fs::write(dir.path().join("batch-0"), "pid=2 start=2").unwrap();
        assert_ne!(snapshot_slot_queue(Some(dir.path())).unwrap(), first);
    }

    /// A minimal sim project: `rock sim --list` takes a compile slot for it.
    fn write_slot_probe_project(dir: &Path) -> PathBuf {
        let project = dir.join("project");
        std::fs::create_dir(&project).unwrap();
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap();
        std::fs::write(project.join("stick.toml"), format!(
            "[package]\nname = \"deadline-probe\"\nversion = \"0.1.0\"\ntype = \"bin\"\n[dependencies]\nstone = {{ path = {:?} }}\n[build.vars]\nGIT_SHA = \"deadline\"\n", root.join("stone")
        )).unwrap();
        std::fs::write(project.join("main.rock"), "fn main() {}\n@sim\nco fn deadline_probe() -> Coroutine<Wait> { stop_sim() }\n").unwrap();
        project
    }

    fn slot_probe_command(project: &Path, slots: &Path) -> Command {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_rock"));
        command.args(["sim", "--list"]).arg(project)
            .env("ROCK_COMPILE_SLOT_DIR", slots).env("ROCK_COMPILE_SLOTS", "1")
            .env("ROCK_COMPILE_CLASS", "batch").env("ROCK_SIM_COMPILE_CACHE", "0")
            .env_remove("ROCK_SIM_COMPILE_RECEIPT_DIR").env_remove("ROCK_SIM_COMPILE_RECEIPT_PREPARE")
            .stdout(Stdio::piped()).stderr(Stdio::piped())
            .current_dir(root);
        command
    }

    fn await_slot_wait(pending: &BudgetedChild) {
        let ready_deadline = Instant::now() + HANG_BACKSTOP;
        while read_spawn_phase(pending.phase.path()).as_deref() != Some("slot-wait") {
            assert!(Instant::now() < ready_deadline, "compiler never reached the held slot");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn hold_batch_zero(slots: &Path) -> std::fs::File {
        use std::io::Write;
        let mut slot = std::fs::File::options().create(true).truncate(false).read(true).write(true)
            .open(slots.join("batch-0")).unwrap();
        slot.lock().unwrap();
        writeln!(slot, "{} slot test barrier", std::process::id()).unwrap();
        slot
    }

    #[test]
    fn slot_wait_stall_reports_the_slot_wait_phase() {
        let dir = tempfile::tempdir().unwrap();
        let project = write_slot_probe_project(dir.path());
        let slots = dir.path().join("slots");
        std::fs::create_dir(&slots).unwrap();
        let slot = hold_batch_zero(&slots);
        let _budget = SpawnBudget::new(Duration::from_secs(10));
        let mut pending = slot_probe_command(&project, &slots).spawn_with_deadline().unwrap();
        await_slot_wait(&pending);
        // Shrink the budget the waiter sees: a queue with no turnover must
        // fail as a stall, named, instead of hanging on the held slot.
        pending.budget = Duration::from_millis(250);
        let started = Instant::now();
        let output = pending.wait_with_output();
        slot.unlock().unwrap();
        assert!(started.elapsed() < Duration::from_millis(250) + LOAD_MARGIN, "slot wait escaped its stall cap");
        let error = output.expect_err("slot wait must fail on a stalled queue");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("phase=slot-wait"), "{error}");
        assert!(error.to_string().contains("stalled"), "{error}");
        discard_kept_logs(&error);
        drop(_budget);
        let _control_budget = SpawnBudget::new(Duration::from_secs(120));
        let control = slot_probe_command(&project, &slots).output_with_deadline().unwrap();
        assert!(control.status.success(), "fixture must compile: {}", String::from_utf8_lossy(&control.stderr));
    }

    #[test]
    fn slot_wait_survives_a_moving_queue_past_the_stall_cap() {
        let dir = tempfile::tempdir().unwrap();
        let project = write_slot_probe_project(dir.path());
        let slots = dir.path().join("slots");
        std::fs::create_dir(&slots).unwrap();
        let slot = hold_batch_zero(&slots);
        // A 5 s stall cap: churn every 100 ms stays visible to the waiter even
        // when a loaded host delays both threads by seconds.
        let _budget = SpawnBudget::new(Duration::from_secs(5));
        let pending = slot_probe_command(&project, &slots).spawn_with_deadline().unwrap();
        await_slot_wait(&pending);
        let phase_path = pending.phase.path().to_path_buf();
        let waiter = std::thread::spawn(move || pending.wait_with_output());
        // Churn an unused slot file fifty times per stall cap: the queue moves,
        // so the waiter must outlive twice the stall cap it would otherwise
        // die at.
        let churn = slots.join("batch-1");
        let start = Instant::now();
        let mut tick = 0;
        while start.elapsed() < Duration::from_millis(10_500) {
            tick += 1;
            std::fs::write(&churn, format!("pid={} start=0 churn={tick}", std::process::id())).unwrap();
            std::thread::sleep(Duration::from_millis(100));
            assert!(!waiter.is_finished(), "waiter died while the queue moved");
        }
        assert_eq!(read_spawn_phase(&phase_path).as_deref(), Some("slot-wait"));
        slot.unlock().unwrap();
        // Past the slot, the 5 s budget is too tight for a compile on a loaded
        // host; this test only needs the waiter to leave the slot queue.
        match waiter.join().unwrap() {
            Ok(output) => assert!(output.status.success(), "fixture must compile: {}", String::from_utf8_lossy(&output.stderr)),
            Err(error) => {
                assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error}");
                assert!(!error.to_string().contains("phase=slot-wait"), "waiter stayed queued on a free slot: {error}");
                discard_kept_logs(&error);
            }
        }
        drop(_budget);
        let _control_budget = SpawnBudget::new(Duration::from_secs(120));
        let control = slot_probe_command(&project, &slots).output_with_deadline().unwrap();
        assert!(control.status.success(), "fixture must compile: {}", String::from_utf8_lossy(&control.stderr));
    }
}
