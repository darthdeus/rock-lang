use crate::*;
use std::process::Child;
use std::time::Duration;

/// Compile a game once for `rock frame` / `rock perf` style commands on the
/// chosen `backend`: [`prepare_game_compile`], then [`compile_game_into`]
/// `output`. On success clears tmp/error.txt and returns the `<output>/main.lua`
/// path luajit should be exec'd on. On failure writes tmp/error.txt and
/// propagates the error (caller exits).
pub struct CompiledGame {
    pub lua_file: std::path::PathBuf,
    pub artifact_fingerprint: String,
}

pub fn compile_game_for_run(
    backend: Backend,
    path: &str,
    output: &str,
) -> Result<CompiledGame> {
    prepare_game_compile(path)?;
    let artifact = compile_game_into(backend, path, std::path::Path::new(output))?;
    clear_error_file();
    Ok(CompiledGame {
        lua_file: PathBuf::from(output).join(rock_host::project::MAIN_LUA_FILENAME),
        artifact_fingerprint: artifact.artifact_fingerprint,
    })
}

/// What a from-scratch compile published beside its `main.lua`.
pub(crate) struct GameArtifact {
    pub artifact_fingerprint: String,
    /// Comptime file inputs (`embed_string`) baked into the program.
    pub embedded_files: Vec<PathBuf>,
    /// The `[build.vars]` selection the compile stamped, when the backend reports it.
    pub comptime_inputs: Option<bone_semantic::inference::ComptimeInputs>,
}

/// Everything a run compile needs before the compiler: workspace root,
/// renderer feature, sound bank and engine libs.
pub(crate) fn prepare_game_compile(path: &str) -> Result<()> {
    rock_host::project::ensure_workspace_root()?;
    crate::skull_backend::ensure_renderer_feature(path)?;
    crate::package::prepare_sound_bank(path)?;
    rock_host::engine_build::build_engine_libs(path)?;
    Ok(())
}

/// One from-scratch compile of `path` into `<output_dir>/main.lua` via the
/// backend's `GameCompiler` (skull: `emit_full`). On failure writes
/// tmp/error.txt and propagates the error; success leaves it to the caller.
pub(crate) fn compile_game_into(
    backend: Backend,
    path: &str,
    output_dir: &std::path::Path,
) -> Result<GameArtifact> {
    use anyhow::Context as _;
    use std::collections::HashSet;

    let compiled = (|| -> Result<GameArtifact> {
        std::fs::create_dir_all(output_dir).with_context(|| format!("create {}", output_dir.display()))?;
        let output_path = output_dir.join(rock_host::project::MAIN_LUA_FILENAME);
        let mut compiler = crate::game_runtime::game_compiler_for(backend, path, &output_path);
        // One-shot from-scratch compile: always the init path, so the outcome
        // can never request a shape restart (there is no live process).
        compiler.compile(&HashSet::new(), true)?;
        Ok(GameArtifact {
            artifact_fingerprint: compiler.artifact_fingerprint()?,
            embedded_files: compiler.comptime_watch_files(),
            comptime_inputs: compiler.comptime_inputs().cloned(),
        })
    })();
    if let Err(error) = &compiled {
        record_compilation_error(error);
    }
    compiled
}

/// Total RAM in MiB; `None` when the platform gives no reading.
pub(crate) fn total_system_memory_mib() -> Option<u64> {
    let mut system = sysinfo::System::new();
    system.refresh_memory_specifics(sysinfo::MemoryRefreshKind::nothing().with_ram());
    Some(system.total_memory() / (1024 * 1024)).filter(|&mib| mib > 0)
}

/// Trivial path -> SourceSpec mapping for callers that don't need
/// stick.toml walk-up: directory becomes `Directory`, single file
/// becomes `Files`. `do_run_timeout` keeps its own variant because
/// it walks up to find the enclosing stick package.
pub fn path_to_source_spec(path: &str) -> crate::unified_compile::SourceSpec {
    let path_buf = std::path::PathBuf::from(path);
    if path_buf.is_dir() {
        crate::unified_compile::SourceSpec::Directory(path_buf)
    } else {
        crate::unified_compile::SourceSpec::Files(vec![path_buf])
    }
}

/// Path of the compile-error file `rock stone` writes for in-game
/// display. Stone's runner reads this and surfaces the message inside
/// the game window so the user doesn't have to flip to the terminal.
pub const ERROR_FILE_PATH: &str = "tmp/error.txt";

/// Write a compilation error to `tmp/error.txt` (read by stone's
/// in-game error overlay). Failures to write are logged to stderr;
/// they're never load-bearing -- the message is also on stderr.
pub fn write_error_file(error_message: &str) {
    let cleaned = strip_ansi_and_unicode(error_message);
    if let Err(e) = fs_err::write(ERROR_FILE_PATH, &cleaned) {
        log::warn!(target: "CLI", "failed to write error to {}: {}", ERROR_FILE_PATH, e);
    }
}

/// Remove `tmp/error.txt` after a successful compile.
pub fn clear_error_file() {
    if fs_err::metadata(ERROR_FILE_PATH).is_ok() {
        if let Err(e) = fs_err::remove_file(ERROR_FILE_PATH) {
            log::warn!(target: "CLI", "failed to remove error file {}: {}", ERROR_FILE_PATH, e);
        }
    }
}

/// Record a compilation error into `tmp/error.txt` for stone's
/// in-game overlay. Does **not** eprintln -- callers that propagate
/// the error via `?` let main print it once, callers that need it
/// surfaced sooner (e.g. the watch loop) handle the eprintln
/// themselves.
pub fn record_compilation_error(error: &anyhow::Error) {
    write_error_file(&format!("Compilation failed: {}", error));
}

/// Process management utilities for child processes
/// Keep a test child on the same Rock tree the CLI resolved, even when the
/// package fixture is under a temporary working directory.
pub fn apply_test_rock_dir(command: &mut std::process::Command) {
    command.env("ROCK_DIR", rock_host::rock_dir());
}

/// A fresh directory for one test process's scratch files (`ROCK_TEST_TMP`),
/// under the worktree `tmp/` the tests used before, so it stays disk-backed.
/// Keep it alive until the child and its descendants have exited.
pub fn test_scratch_dir(command: &mut std::process::Command) -> Result<tempfile::TempDir> {
    let root = match std::env::var_os("ROCK_DIR") {
        Some(dir) => std::path::PathBuf::from(dir),
        None => std::env::current_dir()?,
    };
    let tmp = root.join("tmp");
    fs_err::create_dir_all(&tmp)?;
    let scratch = tempfile::Builder::new().prefix("rock-test-").tempdir_in(tmp)?;
    command.env("ROCK_TEST_TMP", scratch.path());
    Ok(scratch)
}

pub struct ProcessManager;

impl ProcessManager {
    /// Wait for a child process with timeout, returning output and timeout status.
    ///
    /// Pipe outputs (stdout/stderr) are drained by background threads throughout
    /// the child's lifetime. Without this, a child that writes more than the OS
    /// pipe buffer (~64KB on macOS/Linux) blocks on write(), the parent's
    /// try_wait() never sees an exit, and the process appears to hang until the
    /// timeout fires. Observed: `rock frame mag --dump-state` writing 1.2MB to a
    /// piped stdout took 15s (the full timeout) instead of ~1s.
    pub fn wait_with_timeout(child: &mut Child, timeout: Duration) -> Result<ProcessResult> {
        Self::wait_with_policy(child, WaitPolicy::Total(timeout), false, false)
    }

    /// Wait for a bounded frame run. The startup allowance applies until the
    /// first completed-frame marker; after that only inactivity is bounded, so
    /// a slow run that keeps making progress may outlive its frame count.
    pub fn wait_with_frame_watchdog(
        child: &mut Child,
        startup: Duration,
        inactivity: Duration,
        forward_stdout: bool,
        forward_stderr: bool,
    ) -> Result<ProcessResult> {
        Self::wait_with_policy(
            child,
            WaitPolicy::Frame { startup, inactivity },
            forward_stdout,
            forward_stderr,
        )
    }

    /// Start the run budget at the first completed frame, bounding startup
    /// separately. Perf needs only that marker, not logs on measured frames.
    pub fn wait_with_startup_budget(
        child: &mut Child,
        startup: Duration,
        budget: Duration,
    ) -> Result<ProcessResult> {
        Self::wait_with_policy(child, WaitPolicy::FrameBudget { startup, budget }, false, false)
    }

    /// Bound program loading and test progress by the child's CPU time. Valid
    /// START and TIME markers reset the CPU budget; a generous wall backstop
    /// still catches a child blocked without consuming CPU.
    pub fn wait_with_test_watchdog(child: &mut Child, limit: Duration, watch: TestWatch) -> Result<ProcessResult> {
        Self::wait_with_policy_and_tests(child, WaitPolicy::Test(limit), false, false, Some(watch))
    }

    fn wait_with_policy(
        child: &mut Child,
        policy: WaitPolicy,
        forward_stdout: bool,
        forward_stderr: bool,
    ) -> Result<ProcessResult> {
        Self::wait_with_policy_and_tests(child, policy, forward_stdout, forward_stderr, None)
    }

    fn wait_with_policy_and_tests(
        child: &mut Child,
        policy: WaitPolicy,
        forward_stdout: bool,
        forward_stderr: bool,
        test_watch: Option<TestWatch>,
    ) -> Result<ProcessResult> {
        use std::io::{self, BufRead, BufReader, Read, Write};
        use std::sync::mpsc;
        use std::thread;

        let observe_progress = !matches!(policy, WaitPolicy::Total(_));
        let (progress_tx, progress_rx) =
            if observe_progress {
                let (tx, rx) = mpsc::channel::<ProgressEvent>();
                (Some(tx), Some(rx))
            } else {
                (None, None)
            };
        let test_token = test_watch.as_ref().map(|watch| watch.token.clone());
        let child_pid = child.id();

        // Spawn background threads to drain pipes continuously. This prevents
        // the child from blocking on a full pipe buffer while the parent polls.
        // Read errors are surfaced to parent stderr -- silently truncating
        // would hand callers a partial buffer that looks complete.
        let stdout_thread = child.stdout.take().map(|mut h| {
            let progress_tx = progress_tx.clone();
            let test_token = test_token.clone();
            thread::spawn(move || {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                let mut line = Vec::with_capacity(128);
                let mut line_overflow = false;
                loop {
                    match h.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => {
                            buf.extend_from_slice(&chunk[..n]);
                            if let (Some(tx), Some(token)) = (&progress_tx, test_token.as_deref()) {
                                for &byte in &chunk[..n] {
                                    if !line_overflow {
                                        line.push(byte);
                                        if line.len() > 4096 {
                                            line.clear();
                                            line_overflow = true;
                                        }
                                    }
                                    if byte == b'\n' {
                                        if !line_overflow {
                                            if let Some(ordinal) = parse_test_time_progress(&line, token) {
                                                let _ = tx.send(ProgressEvent {
                                                    at: std::time::Instant::now(),
                                                    progress: ProcessProgress::TestTime(ordinal),
                                                    child_cpu: child_process_cpu_time(child_pid),
                                                });
                                            }
                                        }
                                        line.clear();
                                        line_overflow = false;
                                    }
                                }
                            }
                            if forward_stdout {
                                let mut stdout = io::stdout().lock();
                                let _ = stdout.write_all(&chunk[..n]);
                                let _ = stdout.flush();
                            }
                        }
                        Err(e) => {
                            log::error!(target: "CLI", "ProcessManager: error reading child stdout: {}", e);
                            break;
                        }
                    }
                }
                buf
            })
        });
        let stderr_thread = child.stderr.take().map(|h| {
            let progress_tx = progress_tx.clone();
            thread::spawn(move || {
                let mut reader = BufReader::new(h);
                let mut buf = Vec::new();
                let mut line = Vec::new();
                loop {
                    line.clear();
                    match reader.read_until(b'\n', &mut line) {
                        Ok(0) => break,
                        Ok(_) => {
                            buf.extend_from_slice(&line);
                            if forward_stderr {
                                let mut stderr = io::stderr().lock();
                                let _ = stderr.write_all(&line);
                                let _ = stderr.flush();
                            }
                            if let Some(tx) = &progress_tx {
                                let progress = match policy {
                                    WaitPolicy::Test(_) => test_token.as_deref()
                                        .and_then(|token| parse_test_start(&line, token))
                                        .map(ProcessProgress::TestStart),
                                    _ => parse_frame_progress(&line).map(ProcessProgress::Frame),
                                };
                                if let Some(progress) = progress {
                                    let child_cpu = if matches!(&progress, ProcessProgress::TestStart(_)) {
                                        child_process_cpu_time(child_pid)
                                    } else {
                                        None
                                    };
                                    let _ = tx.send(ProgressEvent {
                                        at: std::time::Instant::now(),
                                        progress,
                                        child_cpu,
                                    });
                                }
                            }
                        }
                        Err(e) => {
                            log::error!(target: "CLI", "ProcessManager: error reading child stderr: {}", e);
                            break;
                        }
                    }
                }
                buf
            })
        });
        drop(progress_tx);

        let start_time = std::time::Instant::now();
        let mut last_progress_at = start_time;
        let mut first_progress_at = None;
        let mut latest_progress = None;
        let mut tests = test_watch.map(|watch| TestSequence {
            count: watch.tests,
            next_start: 0,
            next_time: 0,
            running: None,
            last_completed: None,
        });
        let mut last_test_cpu = child_process_cpu_time(child_pid);
        let mut saw_progress = false;
        let timed_out;
        let timeout_phase;
        let status;

        loop {
            if let Some(rx) = &progress_rx {
                for event in rx.try_iter() {
                    let accepted = match &event.progress {
                        ProcessProgress::TestStart(ordinal) => {
                            tests.as_mut().is_some_and(|tests| tests.start(*ordinal))
                        }
                        ProcessProgress::TestTime(ordinal) => {
                            tests.as_mut().is_some_and(|tests| tests.complete(*ordinal))
                        }
                        ProcessProgress::Frame(_) => true,
                    };
                    if !accepted {
                        continue;
                    }
                    saw_progress = true;
                    last_progress_at = if matches!(policy, WaitPolicy::Test(_)) {
                        last_progress_at.max(event.at)
                    } else {
                        std::time::Instant::now()
                    };
                    first_progress_at.get_or_insert(last_progress_at);
                    if let ProcessProgress::Frame(frame) = event.progress {
                        latest_progress = Some(frame);
                    }
                    if matches!(policy, WaitPolicy::Test(_)) {
                        if let Some(sample) = event.child_cpu {
                            last_test_cpu = Some(last_test_cpu.map_or(sample, |previous| previous.max(sample)));
                        }
                    }
                }
            }

            match child.try_wait()? {
                Some(s) => {
                    status = s;
                    timed_out = false;
                    timeout_phase = None;
                    break;
                }
                None => {
                    let (elapsed, limit, phase) = match policy {
                        WaitPolicy::Total(timeout) =>
                            (start_time.elapsed(), timeout, TimeoutPhase::TotalBudget),
                        WaitPolicy::Frame { startup, inactivity } => {
                            if saw_progress {
                                (last_progress_at.elapsed(), inactivity, TimeoutPhase::FrameInactivity)
                            } else {
                                (start_time.elapsed(), startup, TimeoutPhase::Startup)
                            }
                        }
                        WaitPolicy::Test(cpu_limit) => {
                            let wall_limit = test_watchdog_wall_backstop(cpu_limit);
                            let wall_elapsed = last_progress_at.elapsed();
                            let cpu_elapsed = child_process_cpu_time(child_pid)
                                .zip(last_test_cpu)
                                .map(|(now, baseline)| now.saturating_sub(baseline));
                            if cpu_elapsed.is_some_and(|elapsed| elapsed >= cpu_limit) {
                                child.kill()?;
                                status = child.wait()?;
                                timed_out = true;
                                timeout_phase = Some(if saw_progress {
                                    TimeoutPhase::TestCpu
                                } else {
                                    TimeoutPhase::StartupCpu
                                });
                                break;
                            }
                            if wall_elapsed >= wall_limit {
                                child.kill()?;
                                status = child.wait()?;
                                timed_out = true;
                                timeout_phase = Some(if saw_progress {
                                    TimeoutPhase::TestWall
                                } else {
                                    TimeoutPhase::StartupWall
                                });
                                break;
                            }
                            (wall_elapsed, wall_limit, if saw_progress {
                                TimeoutPhase::TestWall
                            } else {
                                TimeoutPhase::StartupWall
                            })
                        }
                        WaitPolicy::FrameBudget { startup, budget } => {
                            match first_progress_at {
                                Some(first) => (first.elapsed(), budget, TimeoutPhase::FrameBudget),
                                None => (start_time.elapsed(), startup, TimeoutPhase::Startup),
                            }
                        }
                    };
                    if !matches!(policy, WaitPolicy::Test(_)) && elapsed >= limit {
                        child.kill()?;
                        status = child.wait()?;
                        timed_out = true;
                        timeout_phase = Some(phase);
                        break;
                    }
                    thread::sleep(Duration::from_millis(50));
                }
            }
        }

        let stdout = stdout_thread.map(|t| t.join().unwrap_or_default()).unwrap_or_default();
        let stderr = stderr_thread.map(|t| t.join().unwrap_or_default()).unwrap_or_default();

        // Reader joins ensure the final markers have arrived. After a timeout
        // retain the test state observed when the budget expired.
        if !timed_out || !matches!(policy, WaitPolicy::Test(_)) {
            if let Some(rx) = &progress_rx {
                for event in rx.try_iter() {
                    match event.progress {
                        ProcessProgress::Frame(frame) => latest_progress = Some(frame),
                        ProcessProgress::TestStart(ordinal) => {
                            if let Some(tests) = tests.as_mut() {
                                tests.start(ordinal);
                            }
                        }
                        ProcessProgress::TestTime(ordinal) => {
                            if let Some(tests) = tests.as_mut() {
                                tests.complete(ordinal);
                            }
                        }
                    }
                }
            }
        }

        let (running_test, last_completed_test) = tests
            .map(|tests| (tests.running, tests.last_completed))
            .unwrap_or((None, None));
        Ok(ProcessResult {
            output: std::process::Output { status, stdout, stderr },
            timed_out,
            timeout_phase,
            frame_progress: latest_progress,
            running_test,
            last_completed_test,
            progress_stopped: matches!(policy, WaitPolicy::Frame { .. }) && saw_progress && timed_out,
        })
    }
}

pub(crate) fn test_watchdog_wall_backstop(cpu_limit: Duration) -> Duration {
    cpu_limit.saturating_mul(8).max(Duration::from_secs(5))
}

#[derive(Debug, Clone, Copy)]
enum WaitPolicy {
    Total(Duration),
    Frame { startup: Duration, inactivity: Duration },
    FrameBudget { startup: Duration, budget: Duration },
    Test(Duration),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutPhase {
    TotalBudget,
    Startup,
    FrameInactivity,
    FrameBudget,
    StartupCpu,
    StartupWall,
    TestCpu,
    TestWall,
}

impl std::fmt::Display for TimeoutPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::TotalBudget => "total budget",
            Self::Startup => "startup",
            Self::FrameInactivity => "frame inactivity",
            Self::FrameBudget => "frame budget",
            Self::StartupCpu => "test process startup CPU budget",
            Self::StartupWall => "test process startup wall backstop",
            Self::TestCpu => "test child CPU-time budget",
            Self::TestWall => "test wall-clock backstop",
        })
    }
}

struct ProgressEvent {
    at: std::time::Instant,
    progress: ProcessProgress,
    child_cpu: Option<Duration>,
}

enum ProcessProgress {
    Frame(FrameProgress),
    TestStart(usize),
    TestTime(usize),
}

/// A test harness's start and completion events. The token is generated per
/// run and distinct from accidental test output, so marker-shaped output
/// without this run's token is not an event.
#[derive(Debug, Clone)]
pub struct TestWatch {
    pub token: String,
    pub tests: usize,
}

/// Starts and TIME completions are accepted once each, in declaration order.
/// They arrive on separate pipes, so either side may be observed first.
struct TestSequence {
    count: usize,
    next_start: usize,
    next_time: usize,
    running: Option<usize>,
    last_completed: Option<usize>,
}

impl TestSequence {
    fn start(&mut self, ordinal: usize) -> bool {
        if ordinal != self.next_start || ordinal >= self.count {
            return false;
        }
        self.next_start += 1;
        if ordinal < self.next_time {
            return false;
        }
        self.running = Some(ordinal);
        true
    }

    fn complete(&mut self, ordinal: usize) -> bool {
        if ordinal != self.next_time || ordinal >= self.count {
            return false;
        }
        self.next_time += 1;
        self.last_completed = Some(ordinal);
        if self.running == Some(ordinal) {
            self.running = None;
        }
        true
    }
}

/// `ROCK_TEST_START<TAB>token<TAB>ordinal`, ordinal counting from 0.
fn parse_test_start(line: &[u8], token: &str) -> Option<usize> {
    let line = std::str::from_utf8(line).ok()?;
    let fields = line.strip_prefix("ROCK_TEST_START\t")?.strip_suffix('\n')?;
    let (got, ordinal) = fields.split_once('\t')?;
    if got != token {
        return None;
    }
    ordinal.parse().ok()
}

/// `__SKULL__ TIME` completion records carry the token and ordinal at the tail.
fn parse_test_time_progress(line: &[u8], token: &str) -> Option<usize> {
    let line = std::str::from_utf8(line).ok()?;
    let fields = line.strip_suffix('\n')?.split('\t').collect::<Vec<_>>();
    if fields.len() != 7
        || fields[0] != "__SKULL__"
        || fields[1] != "TIME"
        || fields[2].is_empty()
        || fields[3].parse::<f64>().is_err()
        || !matches!(fields[4], "pass" | "fail" | "value")
        || fields[5] != token
    {
        return None;
    }
    fields[6].parse().ok()
}

fn parse_frame_progress(line: &[u8]) -> Option<FrameProgress> {
    let line = std::str::from_utf8(line).ok()?;
    let fields = line.split_once("ROCK_FRAME_PROGRESS ")?.1.split_whitespace();
    let mut renderer = None;
    let mut completed = None;
    let mut requested = None;
    for field in fields {
        let (name, value) = field.split_once('=')?;
        match name {
            "renderer" => renderer = Some(value.to_owned()),
            "completed" => completed = value.parse().ok(),
            "requested" => requested = match value {
                "-" | "-1" => Some(None),
                _ => Some(Some(value.parse().ok()?)),
            },
            _ => {}
        }
    }
    Some(FrameProgress {
        renderer: renderer?,
        completed: completed?,
        requested: requested?,
    })
}

#[cfg(target_os = "linux")]
fn child_process_cpu_time(pid: u32) -> Option<Duration> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields = stat.rsplit_once(')')?.1.split_whitespace().collect::<Vec<_>>();
    let user_ticks = fields.get(11)?.parse::<u64>().ok()?;
    let system_ticks = fields.get(12)?.parse::<u64>().ok()?;
    let ticks = user_ticks.checked_add(system_ticks)?;
    let ticks_per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if ticks_per_second <= 0 {
        return None;
    }
    let nanos = u128::from(ticks) * 1_000_000_000 / ticks_per_second as u128;
    Some(Duration::from_nanos(u64::try_from(nanos).ok()?))
}

#[cfg(target_os = "macos")]
fn child_process_cpu_time(pid: u32) -> Option<Duration> {
    use std::ffi::c_void;
    use std::os::raw::c_int;

    // proc_pidinfo writes the complete ABI struct, including unused trailing fields.
    #[allow(dead_code)]
    #[repr(C)]
    struct ProcTaskInfo {
        virtual_size: u64,
        resident_size: u64,
        total_user: u64,
        total_system: u64,
        threads_user: u64,
        threads_system: u64,
        policy: i32,
        faults: i32,
        pageins: i32,
        cow_faults: i32,
        messages_sent: i32,
        messages_received: i32,
        syscalls_mach: i32,
        syscalls_unix: i32,
        context_switches: i32,
        thread_count: i32,
        running_thread_count: i32,
        priority: i32,
    }

    #[repr(C)]
    struct MachTimebaseInfo {
        numer: u32,
        denom: u32,
    }

    #[link(name = "proc")]
    unsafe extern "C" {
        fn proc_pidinfo(pid: c_int, flavor: c_int, arg: u64, buffer: *mut c_void, buffersize: c_int) -> c_int;
    }

    #[link(name = "System")]
    unsafe extern "C" {
        fn mach_timebase_info(info: *mut MachTimebaseInfo) -> c_int;
    }

    const PROC_PIDTASKINFO: c_int = 4;
    let mut info = std::mem::MaybeUninit::<ProcTaskInfo>::zeroed();
    let size = std::mem::size_of::<ProcTaskInfo>() as c_int;
    let read = unsafe { proc_pidinfo(pid as c_int, PROC_PIDTASKINFO, 0, info.as_mut_ptr().cast(), size) };
    if read != size {
        return None;
    }
    let info = unsafe { info.assume_init() };
    let ticks = info.total_user.checked_add(info.total_system)?;
    let mut timebase = MachTimebaseInfo { numer: 0, denom: 0 };
    if unsafe { mach_timebase_info(&mut timebase) } != 0 || timebase.numer == 0 || timebase.denom == 0 {
        return None;
    }
    let nanos = u128::from(ticks) * u128::from(timebase.numer) / u128::from(timebase.denom);
    Some(Duration::from_nanos(u64::try_from(nanos).ok()?))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn child_process_cpu_time(_pid: u32) -> Option<Duration> {
    // Platforms without a live child-CPU query use the wall-clock backstop.
    None
}

/// Result of waiting for a child process with timeout
#[derive(Debug)]
pub struct ProcessResult {
    pub output: std::process::Output,
    pub timed_out: bool,
    pub timeout_phase: Option<TimeoutPhase>,
    pub frame_progress: Option<FrameProgress>,
    /// Index of the test whose START has no matching TIME completion.
    pub running_test: Option<usize>,
    /// Index of the last `TIME` completion a [`TestWatch`] run observed.
    pub last_completed_test: Option<usize>,
    pub progress_stopped: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameProgress {
    pub renderer: String,
    pub completed: usize,
    pub requested: Option<usize>,
}

#[cfg(test)]
mod tests {
    use super::{
        parse_frame_progress, parse_test_start, parse_test_time_progress, FrameProgress, TestSequence,
    };

    #[test]
    fn test_start_requires_the_run_token() {
        assert_eq!(parse_test_start(b"ROCK_TEST_START\tabc\t3\n", "abc"), Some(3));
        assert_eq!(parse_test_start(b"ROCK_TEST_START\tforged\t3\n", "abc"), None);
        assert_eq!(parse_test_start(b"ROCK_TEST_START\tabc\t3", "abc"), None);
        assert_eq!(parse_test_start(b"ROCK_TEST_START\tabc\tx\n", "abc"), None);
        assert_eq!(parse_test_start(b"ROCK_TEST_START\t\"a.rock\"\t\"t\"\n", "abc"), None);
        assert_eq!(parse_test_start(b"ordinary output\n", "abc"), None);
    }

    #[test]
    fn test_time_requires_the_run_token_and_protocol_fields() {
        assert_eq!(
            parse_test_time_progress(b"__SKULL__\tTIME\tt_a\t12.500\tpass\tabc\t3\n", "abc"),
            Some(3),
        );
        assert_eq!(
            parse_test_time_progress(b"__SKULL__\tTIME\tt_a\t12.500\tpass\tforged\t3\n", "abc"),
            None,
        );
        assert_eq!(
            parse_test_time_progress(b"__SKULL__\tTIME\tt_a\t12.500\tpass\tabc\tx\n", "abc"),
            None,
        );
        assert_eq!(parse_test_time_progress(b"ordinary output\n", "abc"), None);
    }

    #[test]
    fn test_sequence_accepts_each_start_and_time_once_in_order() {
        let mut tests = TestSequence {
            count: 3,
            next_start: 0,
            next_time: 0,
            running: None,
            last_completed: None,
        };
        assert!(!tests.start(1), "skipped ahead");
        assert!(tests.start(0));
        assert!(!tests.start(0), "repeated");
        assert!(tests.start(1), "the next start may be observed before the previous TIME line");
        assert!(!tests.complete(1), "completion cannot skip ahead");
        assert!(tests.complete(0));
        assert_eq!(tests.running, Some(1), "TIME for the previous test keeps the current test active");
        assert!(!tests.complete(0), "repeated completion");
        assert!(tests.complete(1));
        assert!(!tests.start(0), "stale start");
        assert!(tests.start(2));
        assert!(!tests.start(3), "beyond the declared tests");
        assert_eq!(tests.running, Some(2));
        assert_eq!(tests.last_completed, Some(1));
    }


    #[test]
    fn frame_progress_parser_keeps_renderer_and_denominator() {
        assert_eq!(
            parse_frame_progress(b"[WATCHDOG] ROCK_FRAME_PROGRESS renderer=wgpu completed=17 requested=600\n"),
            Some(FrameProgress {
                renderer: "wgpu".into(),
                completed: 17,
                requested: Some(600),
            })
        );
        assert_eq!(
            parse_frame_progress(b"ROCK_FRAME_PROGRESS renderer=gl completed=4 requested=-1\n"),
            Some(FrameProgress {
                renderer: "gl".into(),
                completed: 4,
                requested: None,
            })
        );
    }

    #[test]
    fn frame_progress_parser_rejects_incomplete_markers() {
        assert_eq!(
            parse_frame_progress(b"ROCK_FRAME_PROGRESS renderer=wgpu completed=17\n"),
            None
        );
        assert_eq!(parse_frame_progress(b"ordinary game output\n"), None);
    }
}
