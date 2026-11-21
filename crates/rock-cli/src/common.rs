use crate::*;
use bone_core::prelude::Instant;
use std::process::Child;
use std::time::Duration;

/// Total RAM in MiB; `None` when the platform gives no reading.
pub fn total_system_memory_mib() -> Option<u64> {
    let mut system = sysinfo::System::new();
    system.refresh_memory_specifics(sysinfo::MemoryRefreshKind::nothing().with_ram());
    Some(system.total_memory() / (1024 * 1024)).filter(|&mib| mib > 0)
}

/// One in-memory source registered under `path`, which must be the file id its
/// spans carry or every snippet lookup misses.
pub(crate) fn single_source(path: &str, src: &str) -> SourceFiles {
    SourceFiles::from_single_file(source_file_from_path_and_contents(
        path.to_string(),
        path.to_string(),
        src.to_string(),
    ))
}

pub(crate) fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_string())
}

pub struct ProcessManager;

impl ProcessManager {
    /// Drain pipes while waiting so a full pipe cannot block the child.
    pub fn wait_with_timeout(child: &mut Child, timeout: Duration) -> Result<ProcessResult> {
        Self::wait_with_policy(
            child,
            WaitPolicy::Total(timeout),
            false,
            |_| None,
            |()| None,
        )
    }

    pub(crate) fn wait_with_progress<P: Send + 'static>(
        child: &mut Child,
        watchdog: ProgressWatchdog,
        decode: fn(&[u8]) -> Option<P>,
    ) -> Result<ProcessResult<P>> {
        let forward = watchdog.inactivity;
        Self::wait_with_policy(child, WaitPolicy::Progress(watchdog), forward, decode, Some)
    }

    #[cfg(feature = "backend-js")]
    pub(crate) fn wait_with_cpu_budget(
        child: &mut Child,
        budget: Duration,
    ) -> Result<ProcessResult> {
        Self::wait_with_policy(
            child,
            WaitPolicy::Cpu {
                cpu_limit: budget,
                wall_limit: Duration::MAX,
            },
            false,
            |_| None,
            |()| None,
        )
    }

    pub(crate) fn wait_with_test_watchdog(
        child: &mut Child,
        policy: WaitPolicy,
        token: String,
        tests: &mut crate::test_protocol::LuaTestSequence,
    ) -> Result<ProcessResult> {
        Self::wait_with_policy(
            child,
            policy,
            false,
            move |line| crate::test_protocol::decode_lua_test(line, &token),
            |event| tests.observe(event).then_some(()),
        )
    }

    fn wait_with_policy<P: Send + 'static, R>(
        child: &mut Child,
        policy: WaitPolicy,
        forward: bool,
        decode: impl Fn(&[u8]) -> Option<P> + Send + 'static,
        mut observe: impl FnMut(P) -> Option<R>,
    ) -> Result<ProcessResult<R>> {
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
        let sample_cpu = matches!(policy, WaitPolicy::Cpu { .. });
        let child_pid = child.id();

        // Spawn background threads to drain pipes continuously. This prevents
        // the child from blocking on a full pipe buffer while the parent polls.
        // A read error ends the thread with that error, and the join below
        // fails the wait: truncated output must never be judged as complete.
        let stdout_thread = child.stdout.take().map(|mut h| {
            thread::spawn(move || {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                loop {
                    match h.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => {
                            buf.extend_from_slice(&chunk[..n]);
                            if forward {
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
            thread::spawn(move || {
                let mut reader = BufReader::new(h);
                let mut buf = Vec::new();
                let mut line = Vec::new();
                while reader.read_until(b'\n', &mut line)? != 0 {
                    if forward {
                        let mut stderr = io::stderr().lock();
                        let _ = stderr.write_all(&line);
                        let _ = stderr.flush();
                    }
                    if let Some(tx) = &progress_tx
                        && let Some(progress) = decode(&line)
                    {
                        let _ = tx.send(ProgressEvent {
                            at: Instant::now(),
                            progress,
                            child_cpu: sample_cpu
                                .then(|| child_process_cpu_time(child_pid))
                                .flatten(),
                        });
                        if !sample_cpu {
                            buf.extend_from_slice(&line);
                        }
                    } else {
                        buf.extend_from_slice(&line);
                    }
                    line.clear();
                }
                Ok(buf)
            })
        });

        let start_time = Instant::now();
        let mut last_progress_at = start_time;
        let mut first_progress_at = None;
        let mut latest_progress = None;
        let mut last_test_cpu = sample_cpu
            .then(|| child_process_cpu_time(child_pid))
            .flatten();
        // Most children exit within a few milliseconds, so start polling
        // fast and back off to the long-running interval.
        let mut poll = Duration::from_millis(1);

        let (status, timeout_phase) = loop {
            if let Some(rx) = &progress_rx {
                for event in rx.try_iter() {
                    let Some(progress) = observe(event.progress) else {
                        continue;
                    };
                    latest_progress = Some(progress);
                    last_progress_at = if sample_cpu {
                        last_progress_at.max(event.at)
                    } else {
                        Instant::now()
                    };
                    first_progress_at.get_or_insert(last_progress_at);
                    if let Some(sample) = event.child_cpu {
                        last_test_cpu =
                            Some(last_test_cpu.map_or(sample, |previous| previous.max(sample)));
                    }
                }
            }

            if let Some(status) = child.try_wait()? {
                break (status, None);
            }
            let saw_progress = first_progress_at.is_some();
            let (remaining, phase) = match policy {
                WaitPolicy::Total(timeout) => (
                    timeout.saturating_sub(start_time.elapsed()),
                    TimeoutPhase::TotalBudget,
                ),
                WaitPolicy::Progress(watchdog) => watchdog.remaining(
                    Instant::now(),
                    start_time,
                    first_progress_at,
                    last_progress_at,
                ),
                WaitPolicy::Cpu {
                    cpu_limit,
                    wall_limit,
                } => {
                    let wall_elapsed = last_progress_at.elapsed();
                    let cpu_elapsed = child_process_cpu_time(child_pid)
                        .zip(last_test_cpu)
                        .map(|(now, baseline)| now.saturating_sub(baseline));
                    if cpu_elapsed.is_some_and(|elapsed| elapsed >= cpu_limit) {
                        let phase = if saw_progress {
                            TimeoutPhase::TestCpu
                        } else {
                            TimeoutPhase::StartupCpu
                        };
                        (Duration::ZERO, phase)
                    } else {
                        let phase = if saw_progress {
                            TimeoutPhase::TestWall
                        } else {
                            TimeoutPhase::StartupWall
                        };
                        (wall_limit.saturating_sub(wall_elapsed), phase)
                    }
                }
            };
            if remaining.is_zero() {
                child.kill()?;
                break (child.wait()?, Some(phase));
            }
            thread::sleep(poll.min(remaining));
            poll = (poll * 2).min(Duration::from_millis(50));
        };
        let timed_out = timeout_phase.is_some();

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

        if let Some(rx) = &progress_rx {
            for event in rx.try_iter() {
                if let Some(progress) = observe(event.progress) {
                    latest_progress = Some(progress);
                }
            }
        }
        Ok(ProcessResult {
            output: std::process::Output {
                status,
                stdout,
                stderr,
            },
            timed_out,
            timeout_phase,
            progress: latest_progress,
            progress_stopped: timeout_phase == Some(TimeoutPhase::ProgressInactivity),
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
    Progress(ProgressWatchdog),
    Cpu {
        cpu_limit: Duration,
        wall_limit: Duration,
    },
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ProgressWatchdog {
    pub startup: Duration,
    pub running: Duration,
    pub inactivity: bool,
    pub launch_cap: Option<(Instant, Duration)>,
}

impl ProgressWatchdog {
    fn remaining(
        self,
        now: Instant,
        started: Instant,
        first: Option<Instant>,
        latest: Instant,
    ) -> (Duration, TimeoutPhase) {
        let (anchor, limit, phase) = match first {
            None => (started, self.startup, TimeoutPhase::Startup),
            Some(_) if self.inactivity => (latest, self.running, TimeoutPhase::ProgressInactivity),
            Some(first) => (first, self.running, TimeoutPhase::ProgressBudget),
        };
        if let Some((launch, cap)) = self.launch_cap
            && cap <= limit.saturating_add(anchor.duration_since(launch))
        {
            return (
                cap.saturating_sub(now.duration_since(launch)),
                TimeoutPhase::TotalBudget,
            );
        }
        (limit.saturating_sub(now.duration_since(anchor)), phase)
    }
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
    at: Instant,
    progress: P,
    child_cpu: Option<Duration>,
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

#[cfg(windows)]
fn child_process_cpu_time(pid: u32) -> Option<Duration> {
    use winapi::shared::minwindef::FILETIME;
    use winapi::um::handleapi::CloseHandle;
    use winapi::um::processthreadsapi::{GetProcessTimes, OpenProcess};
    use winapi::um::winnt::PROCESS_QUERY_LIMITED_INFORMATION;

    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return None;
    }
    let mut times = [unsafe { std::mem::zeroed::<FILETIME>() }; 4];
    let [created, exited, kernel, user] = &mut times;
    let read = unsafe { GetProcessTimes(handle, created, exited, kernel, user) };
    unsafe { CloseHandle(handle) };
    if read == 0 {
        return None;
    }
    let ticks =
        |time: &FILETIME| (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
    let nanos = ticks(kernel).checked_add(ticks(user))?.checked_mul(100)?;
    Some(Duration::from_nanos(nanos))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn child_process_cpu_time(_pid: u32) -> Option<Duration> {
    // Platforms without a live child-CPU query use the wall-clock backstop.
    None
}

#[derive(Debug)]
pub struct ProcessResult<P = ()> {
    pub output: std::process::Output,
    pub timed_out: bool,
    pub timeout_phase: Option<TimeoutPhase>,
    pub progress: Option<P>,
    pub progress_stopped: bool,
}

/// Thread count requested by `ROCK_NUM_THREADS`, else `RAYON_NUM_THREADS`.
/// It sizes both the compiler's global rayon pool and the C test pool.
/// `0` asks for the default, as in rayon. A value that is not a count is an
/// error: ignoring it would silently run with a different pool size.
pub(crate) fn thread_count_request() -> std::result::Result<Option<usize>, String> {
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

/// [`thread_count_request`] for pool construction. `rock_cli::main` rejects an
/// invalid value at startup, so reaching the panic means a pool was built
/// outside the CLI process lifecycle with a bad environment.
#[cfg(not(feature = "sequential"))]
pub(crate) fn requested_thread_count() -> Option<usize> {
    thread_count_request().unwrap_or_else(|error| panic!("{error}"))
}

#[cfg(test)]
mod tests {
    use super::{Instant, ProgressWatchdog};
    use std::time::Duration;

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
    fn watchdog_uses_the_earliest_deadline() {
        use super::TimeoutPhase::*;
        let secs = Duration::from_secs;
        let launch = Instant::now();
        let started = launch + secs(1);
        let first = launch + secs(3);
        let latest = launch + secs(4);
        let now = launch + secs(5);
        let watchdog = ProgressWatchdog {
            startup: secs(6),
            running: secs(3),
            inactivity: false,
            launch_cap: Some((launch, secs(8))),
        };
        for (first_progress, inactivity, cap, expected) in [
            (None, false, 8, (secs(2), Startup)),
            (None, false, 2, (Duration::ZERO, TotalBudget)),
            (Some(first), false, 8, (secs(1), ProgressBudget)),
            (Some(first), true, 8, (secs(2), ProgressInactivity)),
            (Some(first), true, 6, (secs(1), TotalBudget)),
            (Some(first), true, 4, (Duration::ZERO, TotalBudget)),
        ] {
            assert_eq!(
                ProgressWatchdog {
                    inactivity,
                    launch_cap: Some((launch, secs(cap))),
                    ..watchdog
                }
                .remaining(now, started, first_progress, latest),
                expected
            );
        }
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
        let result = super::ProcessManager::wait_with_progress(
            &mut child,
            ProgressWatchdog {
                startup: Duration::from_secs(30),
                running: Duration::from_secs(30),
                inactivity: false,
                launch_cap: None,
            },
            decode_panics,
        );
        let error = result.expect_err("a panicked reader must fail the wait");
        assert!(error.to_string().contains("stderr"), "got: {error}");
    }

    #[test]
    fn short_lived_child_is_reaped_without_a_full_poll_interval() {
        let fastest = (0..5)
            .map(|_| {
                let mut child = std::process::Command::new("sleep")
                    .arg("0.01")
                    .spawn()
                    .expect("spawn sleep");
                let start = Instant::now();
                let result = super::ProcessManager::wait_with_timeout(
                    &mut child,
                    std::time::Duration::from_secs(30),
                )
                .expect("wait");
                assert!(result.output.status.success() && !result.timed_out);
                start.elapsed()
            })
            .min()
            .unwrap();
        assert!(
            fastest < std::time::Duration::from_millis(40),
            "fastest wait took {fastest:?}"
        );
    }
}
