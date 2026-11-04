use crate::*;
use std::process::Child;
use std::time::Duration;

/// Total RAM in MiB; `None` when the platform gives no reading.
pub fn total_system_memory_mib() -> Option<u64> {
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

pub struct ProcessManager;

impl ProcessManager {
    /// Wait for a child process with timeout, returning output and timeout status.
    ///
    /// Pipe outputs (stdout/stderr) are drained by background threads throughout
    /// the child's lifetime. Without this, a child that writes more than the OS
    /// pipe buffer (~64KB on macOS/Linux) blocks on write(), the parent's
    /// try_wait() never sees an exit, and the process appears to hang until timeout.
    pub fn wait_with_timeout(child: &mut Child, timeout: Duration) -> Result<ProcessResult> {
        Self::wait_with_policy(child, WaitPolicy::Total(timeout), false, false)
    }

    /// Bound startup and inactivity using a caller-supplied stderr decoder.
    pub fn wait_with_progress_watchdog<P: Send + 'static>(
        child: &mut Child,
        startup: Duration,
        inactivity: Duration,
        decode: fn(&[u8]) -> Option<P>,
        forward_stdout: bool,
        forward_stderr: bool,
    ) -> Result<ProcessResult<P>> {
        Self::wait_with_policy_and_tests(
            child,
            WaitPolicy::Progress {
                startup,
                inactivity,
            },
            forward_stdout,
            forward_stderr,
            None,
            Some(decode),
        )
    }

    /// Start a total run budget at the first decoded progress event.
    pub fn wait_with_progress_budget<P: Send + 'static>(
        child: &mut Child,
        startup: Duration,
        budget: Duration,
        decode: fn(&[u8]) -> Option<P>,
    ) -> Result<ProcessResult<P>> {
        Self::wait_with_policy_and_tests(
            child,
            WaitPolicy::ProgressBudget { startup, budget },
            false,
            false,
            None,
            Some(decode),
        )
    }

    /// Bound program loading and test progress by the child's CPU time. Valid
    /// START and TIME markers reset the CPU budget; a generous wall backstop
    /// still catches a child blocked without consuming CPU.
    pub fn wait_with_test_watchdog(
        child: &mut Child,
        limit: Duration,
        watch: TestWatch,
    ) -> Result<ProcessResult> {
        Self::wait_with_policy_and_tests(
            child,
            WaitPolicy::Test {
                cpu_limit: limit,
                wall_limit: test_watchdog_wall_backstop(limit),
            },
            false,
            false,
            Some(watch),
            None,
        )
    }

    fn wait_with_policy(
        child: &mut Child,
        policy: WaitPolicy,
        forward_stdout: bool,
        forward_stderr: bool,
    ) -> Result<ProcessResult> {
        Self::wait_with_policy_and_tests(child, policy, forward_stdout, forward_stderr, None, None)
    }

    pub(crate) fn wait_with_policy_and_tests<P: Send + 'static>(
        child: &mut Child,
        policy: WaitPolicy,
        forward_stdout: bool,
        forward_stderr: bool,
        test_watch: Option<TestWatch>,
        decode: Option<fn(&[u8]) -> Option<P>>,
    ) -> Result<ProcessResult<P>> {
        use std::io::{self, BufRead, BufReader, Read, Write};
        use std::sync::mpsc;
        use std::thread;

        let observe_progress = !matches!(policy, WaitPolicy::Total(_));
        let (progress_tx, progress_rx) = if observe_progress {
            let (tx, rx) = mpsc::channel::<ProgressEvent<P>>();
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        let test_token = test_watch.as_ref().map(|watch| watch.token.clone());
        let child_pid = child.id();

        // Spawn background threads to drain pipes continuously. This prevents
        // the child from blocking on a full pipe buffer while the parent polls.
        // A read error ends the thread with that error, and the join below
        // fails the wait: truncated output must never be judged as complete.
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
                                            if let Some(ordinal) =
                                                parse_test_time_progress(&line, token)
                                            {
                                                let _ = tx.send(ProgressEvent {
                                                    at: bone_core::prelude::Instant::now(),
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
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                        Err(e) => return Err(e),
                    }
                }
                Ok(buf)
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
                                    WaitPolicy::Test { .. } => test_token
                                        .as_deref()
                                        .and_then(|token| parse_test_start(&line, token))
                                        .map(ProcessProgress::TestStart),
                                    _ => decode
                                        .and_then(|decode| decode(&line))
                                        .map(ProcessProgress::Observed),
                                };
                                if let Some(progress) = progress {
                                    let child_cpu =
                                        if matches!(&progress, ProcessProgress::TestStart(_)) {
                                            child_process_cpu_time(child_pid)
                                        } else {
                                            None
                                        };
                                    let _ = tx.send(ProgressEvent {
                                        at: bone_core::prelude::Instant::now(),
                                        progress,
                                        child_cpu,
                                    });
                                }
                            }
                        }
                        Err(e) => return Err(e),
                    }
                }
                Ok(buf)
            })
        });
        drop(progress_tx);

        let start_time = bone_core::prelude::Instant::now();
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
                        ProcessProgress::Observed(_) => true,
                    };
                    if !accepted {
                        continue;
                    }
                    saw_progress = true;
                    last_progress_at = if matches!(policy, WaitPolicy::Test { .. }) {
                        last_progress_at.max(event.at)
                    } else {
                        bone_core::prelude::Instant::now()
                    };
                    first_progress_at.get_or_insert(last_progress_at);
                    if let ProcessProgress::Observed(progress) = event.progress {
                        latest_progress = Some(progress);
                    }
                    if matches!(policy, WaitPolicy::Test { .. }) {
                        if let Some(sample) = event.child_cpu {
                            last_test_cpu =
                                Some(last_test_cpu.map_or(sample, |previous| previous.max(sample)));
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
                        WaitPolicy::Total(timeout) => {
                            (start_time.elapsed(), timeout, TimeoutPhase::TotalBudget)
                        }
                        WaitPolicy::Progress {
                            startup,
                            inactivity,
                        } => {
                            if saw_progress {
                                (
                                    last_progress_at.elapsed(),
                                    inactivity,
                                    TimeoutPhase::ProgressInactivity,
                                )
                            } else {
                                (start_time.elapsed(), startup, TimeoutPhase::Startup)
                            }
                        }
                        WaitPolicy::Test {
                            cpu_limit,
                            wall_limit,
                        } => {
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
                            (
                                wall_elapsed,
                                wall_limit,
                                if saw_progress {
                                    TimeoutPhase::TestWall
                                } else {
                                    TimeoutPhase::StartupWall
                                },
                            )
                        }
                        WaitPolicy::ProgressBudget { startup, budget } => match first_progress_at {
                            Some(first) => (first.elapsed(), budget, TimeoutPhase::ProgressBudget),
                            None => (start_time.elapsed(), startup, TimeoutPhase::Startup),
                        },
                    };
                    if !matches!(policy, WaitPolicy::Test { .. }) && elapsed >= limit {
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

        // A pipe the caller did not capture has no output; a captured one must
        // drain completely.
        let drained = |thread: Option<thread::JoinHandle<io::Result<Vec<u8>>>>,
                       pipe: &str|
         -> Result<Vec<u8>> {
            thread
                .map(|t| {
                    rock_host::process_management::join_drained_pipe(t, pipe)
                        .map_err(anyhow::Error::msg)
                })
                .transpose()
                .map(Option::unwrap_or_default)
        };
        let stdout = drained(stdout_thread, "stdout")?;
        let stderr = drained(stderr_thread, "stderr")?;

        // Reader joins ensure the final markers have arrived. After a timeout
        // retain the test state observed when the budget expired.
        if !timed_out || !matches!(policy, WaitPolicy::Test { .. }) {
            if let Some(rx) = &progress_rx {
                for event in rx.try_iter() {
                    match event.progress {
                        ProcessProgress::Observed(progress) => latest_progress = Some(progress),
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
            output: std::process::Output {
                status,
                stdout,
                stderr,
            },
            timed_out,
            timeout_phase,
            progress: latest_progress,
            running_test,
            last_completed_test,
            progress_stopped: matches!(policy, WaitPolicy::Progress { .. })
                && saw_progress
                && timed_out,
        })
    }
}

/// The wall backstop catches a child blocked without using CPU. A starved but
/// busy child must reach its CPU budget first, so the floor leaves room for a
/// loaded host even under a short CPU budget.
pub(crate) fn test_watchdog_wall_backstop(cpu_limit: Duration) -> Duration {
    cpu_limit.saturating_mul(8).max(Duration::from_secs(60))
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum WaitPolicy {
    Total(Duration),
    Progress {
        startup: Duration,
        inactivity: Duration,
    },
    ProgressBudget {
        startup: Duration,
        budget: Duration,
    },
    Test {
        cpu_limit: Duration,
        wall_limit: Duration,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutPhase {
    TotalBudget,
    Startup,
    ProgressInactivity,
    ProgressBudget,
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
            Self::ProgressInactivity => "progress inactivity",
            Self::ProgressBudget => "progress budget",
            Self::StartupCpu => "test process startup CPU budget",
            Self::StartupWall => "test process startup wall backstop",
            Self::TestCpu => "test child CPU-time budget",
            Self::TestWall => "test wall-clock backstop",
        })
    }
}

struct ProgressEvent<P> {
    at: bone_core::prelude::Instant,
    progress: ProcessProgress<P>,
    child_cpu: Option<Duration>,
}

enum ProcessProgress<P> {
    Observed(P),
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

#[cfg(target_os = "linux")]
fn child_process_cpu_time(pid: u32) -> Option<Duration> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields = stat
        .rsplit_once(')')?
        .1
        .split_whitespace()
        .collect::<Vec<_>>();
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
        fn proc_pidinfo(
            pid: c_int,
            flavor: c_int,
            arg: u64,
            buffer: *mut c_void,
            buffersize: c_int,
        ) -> c_int;
    }

    #[link(name = "System")]
    unsafe extern "C" {
        fn mach_timebase_info(info: *mut MachTimebaseInfo) -> c_int;
    }

    const PROC_PIDTASKINFO: c_int = 4;
    let mut info = std::mem::MaybeUninit::<ProcTaskInfo>::zeroed();
    let size = std::mem::size_of::<ProcTaskInfo>() as c_int;
    let read = unsafe {
        proc_pidinfo(
            pid as c_int,
            PROC_PIDTASKINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if read != size {
        return None;
    }
    let info = unsafe { info.assume_init() };
    let ticks = info.total_user.checked_add(info.total_system)?;
    let mut timebase = MachTimebaseInfo { numer: 0, denom: 0 };
    if unsafe { mach_timebase_info(&mut timebase) } != 0
        || timebase.numer == 0
        || timebase.denom == 0
    {
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
pub struct ProcessResult<P = ()> {
    pub output: std::process::Output,
    pub timed_out: bool,
    pub timeout_phase: Option<TimeoutPhase>,
    pub progress: Option<P>,
    /// Index of the test whose START has no matching TIME completion.
    pub running_test: Option<usize>,
    /// Index of the last `TIME` completion a [`TestWatch`] run observed.
    pub last_completed_test: Option<usize>,
    pub progress_stopped: bool,
}

#[cfg(test)]
mod tests {
    use super::{TestSequence, parse_test_start, parse_test_time_progress};

    #[test]
    fn thread_count_request_rejects_a_value_that_is_not_a_count() {
        use super::parse_thread_count_request as parse;
        assert_eq!(parse(None, None), Ok(None));
        assert_eq!(parse(Some("2"), Some("3")), Ok(Some(2)));
        assert_eq!(parse(None, Some("3")), Ok(Some(3)));
        assert_eq!(parse(Some("0"), Some("3")), Ok(None));
        let error = parse(Some("eight"), Some("3")).unwrap_err();
        assert!(
            error.contains("'eight'") && error.contains("ROCK_NUM_THREADS"),
            "got: {error}"
        );
        assert!(
            parse(None, Some("-1"))
                .unwrap_err()
                .contains("RAYON_NUM_THREADS")
        );
    }

    #[test]
    fn panicked_pipe_reader_fails_the_wait_instead_of_returning_empty_output() {
        fn decode_panics(_: &[u8]) -> Option<()> {
            panic!("decoder panic inside the stderr reader")
        }
        let mut child = std::process::Command::new("sh")
            .args(["-c", "echo progress >&2"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn sh");
        let result = super::ProcessManager::wait_with_progress_watchdog(
            &mut child,
            std::time::Duration::from_secs(30),
            std::time::Duration::from_secs(30),
            decode_panics,
            false,
            false,
        );
        let error = result.expect_err("a panicked reader must fail the wait");
        assert!(error.to_string().contains("stderr"), "got: {error}");
    }

    #[test]
    fn test_start_requires_the_run_token() {
        assert_eq!(
            parse_test_start(b"ROCK_TEST_START\tabc\t3\n", "abc"),
            Some(3)
        );
        assert_eq!(
            parse_test_start(b"ROCK_TEST_START\tforged\t3\n", "abc"),
            None
        );
        assert_eq!(parse_test_start(b"ROCK_TEST_START\tabc\t3", "abc"), None);
        assert_eq!(parse_test_start(b"ROCK_TEST_START\tabc\tx\n", "abc"), None);
        assert_eq!(
            parse_test_start(b"ROCK_TEST_START\t\"a.rock\"\t\"t\"\n", "abc"),
            None
        );
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
        assert!(
            tests.start(1),
            "the next start may be observed before the previous TIME line"
        );
        assert!(!tests.complete(1), "completion cannot skip ahead");
        assert!(tests.complete(0));
        assert_eq!(
            tests.running,
            Some(1),
            "TIME for the previous test keeps the current test active"
        );
        assert!(!tests.complete(0), "repeated completion");
        assert!(tests.complete(1));
        assert!(!tests.start(0), "stale start");
        assert!(tests.start(2));
        assert!(!tests.start(3), "beyond the declared tests");
        assert_eq!(tests.running, Some(2));
        assert_eq!(tests.last_completed, Some(1));
    }
}

/// Thread count requested by `ROCK_NUM_THREADS`, else `RAYON_NUM_THREADS`.
/// It sizes both the compiler's global rayon pool and the C test pool.
/// `0` asks for the default, as in rayon. A value that is not a count is an
/// error: ignoring it would silently run with a different pool size.
pub fn thread_count_request() -> std::result::Result<Option<usize>, String> {
    parse_thread_count_request(
        std::env::var("ROCK_NUM_THREADS").ok().as_deref(),
        std::env::var("RAYON_NUM_THREADS").ok().as_deref(),
    )
}

fn parse_thread_count_request(
    rock: Option<&str>,
    rayon: Option<&str>,
) -> std::result::Result<Option<usize>, String> {
    let Some((var, value)) = rock
        .map(|v| ("ROCK_NUM_THREADS", v))
        .or(rayon.map(|v| ("RAYON_NUM_THREADS", v)))
    else {
        return Ok(None);
    };
    match value.parse::<usize>() {
        Ok(0) => Ok(None),
        Ok(n) => Ok(Some(n)),
        Err(_) => Err(format!(
            "invalid value '{value}' for '{var}': expected a thread count"
        )),
    }
}

/// [`thread_count_request`] for pool construction. `main_with` rejects an
/// invalid value at startup, so reaching the panic means a pool was built
/// outside the CLI process lifecycle with a bad environment.
pub fn requested_thread_count() -> Option<usize> {
    thread_count_request().unwrap_or_else(|error| panic!("{error}"))
}
