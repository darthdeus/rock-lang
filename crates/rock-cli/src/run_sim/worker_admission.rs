//! Admission leases reserve worker growth not yet present in GPU telemetry.
//! Existing allocations are already deducted from free VRAM; counting them
//! again would penalize a later suite for workers the host already admitted.
use super::{SIM_LANE_VRAM_MIB, SIM_VRAM_RESERVE_MIB};
use anyhow::{Context, Result};
use rock_core::clock::Instant;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, Write};
use std::path::Path;
use std::time::Duration;

#[derive(Clone, Debug)]
pub(super) struct GpuMemory {
    free: u64,
    processes: HashMap<u32, u64>,
}

type Probe = std::result::Result<Option<GpuMemory>, String>;

/// Probes are shared by threads, but leases are always counted afresh under
/// the host lock. A cached reading cannot spend the same capacity twice:
/// a new PID absent from that reading still reserves its whole estimate.
pub(super) fn gpu_memory() -> Probe {
    static CACHE: std::sync::Mutex<Option<(Instant, Probe)>> = std::sync::Mutex::new(None);
    let mut cache = CACHE.lock().unwrap();
    if let Some((when, reading)) = &*cache {
        if when.elapsed() < Duration::from_millis(250) {
            return reading.clone();
        }
    }
    let reading = probe();
    if let Err(reason) = &reading {
        if cache.as_ref().is_none_or(|(_, previous)| previous.is_ok()) {
            log::warn!(target: "SIM", "GPU admission probe failed: {reason}; admitting at most one host worker");
        }
    }
    *cache = Some((Instant::now(), reading.clone()));
    reading
}

fn probe() -> Probe {
    // PIDS includes graphics contexts; compute-apps queries omit GL workers.
    // Selective fields avoid the expensive full report; NVIDIA forbids -d with -x.
    probe_command(std::process::Command::new("nvidia-smi").args(["-q", "-d", "MEMORY,PIDS"]))
}

fn probe_command(command: &mut std::process::Command) -> Probe {
    let mut child = match command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    let result =
        crate::common::ProcessManager::wait_with_timeout(&mut child, Duration::from_secs(3))
            .map_err(|error| {
                let _ = child.kill();
                let _ = child.wait();
                error.to_string()
            })?;
    if result.timed_out {
        return Err("nvidia-smi did not complete within 3s".into());
    }
    if !result.output.status.success() {
        return Err(format!("nvidia-smi exited with {}", result.output.status));
    }
    parse(&String::from_utf8_lossy(&result.output.stdout)).map(Some)
}

// Scope to the first GPU, matching the planner and default GL device.
fn mib(value: &str) -> Option<u64> {
    value.strip_suffix(" MiB")?.parse().ok()
}
fn parse(text: &str) -> std::result::Result<GpuMemory, String> {
    let mut lines = text.lines().skip_while(|line| !line.starts_with("GPU "));
    lines.next().ok_or("missing first GPU")?;
    let mut section = "";
    let mut free = None;
    let mut saw_processes = false;
    let mut pid = None;
    let mut processes = HashMap::new();
    for line in lines.take_while(|line| !line.starts_with("GPU ")) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if line.starts_with("    ") && !line.starts_with("        ") {
            section = trimmed
                .split_once(':')
                .map_or(trimmed, |(key, _)| key.trim());
            saw_processes |= section == "Processes";
            continue;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        match (section, key.trim()) {
            ("FB Memory Usage", "Free") => {
                free = Some(mib(value.trim()).ok_or("invalid free GPU memory")?);
            }
            ("Processes", "Process ID") => {
                pid = Some(
                    value
                        .trim()
                        .parse::<u32>()
                        .map_err(|_| "invalid GPU process PID")?,
                );
            }
            ("Processes", "Used GPU Memory") => {
                let pid = pid.take().ok_or("missing GPU process PID")?;
                // Unavailable memory earns no credit against the pending lease.
                if let Some(used) = mib(value.trim()) {
                    processes.insert(pid, used);
                }
            }
            _ => {}
        }
    }
    if !saw_processes {
        return Err("missing GPU processes".into());
    }
    Ok(GpuMemory {
        free: free.ok_or("missing free GPU memory")?,
        processes,
    })
}

fn open(path: &Path) -> Result<File> {
    File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("open sim admission {}", path.display()))
}

pub(super) fn publish_pid(root: &Path, slot: &mut File, pid: u32) -> Result<()> {
    let admission = open(&root.join("admission.lock"))?;
    admission.lock()?;
    write_pid(slot, pid)
}

fn write_pid(slot: &mut File, pid: u32) -> Result<()> {
    slot.rewind()?;
    slot.set_len(0)?;
    write!(slot, "{pid}")?;
    Ok(())
}

#[derive(Debug)]
pub(super) enum Admission {
    Granted(File),
    Denied(&'static str),
}

pub(super) fn try_acquire(
    root: &Path,
    slots: usize,
    probe: impl FnOnce() -> Probe,
) -> Result<Admission> {
    std::fs::create_dir_all(root)?;
    // Telemetry and its cache mutex must not block host reservations or PID
    // publication. Leases admitted after this snapshot reserve their full estimate.
    let reading = probe();
    // Serialize the read-and-reserve operation across processes. Slot leases
    // remain kernel locks, so crashes release capacity without stale cleanup.
    let admission = open(&root.join("admission.lock"))?;
    admission.lock()?;
    let mut candidate = None;
    let mut active = Vec::new();
    // Include higher-numbered leases held by callers with a larger CPU budget.
    let existing = std::fs::read_dir(root)?
        .filter_map(|entry| {
            let name = entry.ok()?.file_name().into_string().ok()?;
            name.strip_prefix("worker-")?
                .strip_suffix(".lock")?
                .parse::<usize>()
                .ok()
        })
        .max()
        .map_or(0, |last| last + 1);
    for index in 0..slots.max(existing) {
        let mut file = open(&root.join(format!("worker-{index}.lock")))?;
        match file.try_lock() {
            Ok(()) => {
                if index < slots && candidate.is_none() {
                    candidate = Some(file);
                }
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                let mut text = String::new();
                file.read_to_string(&mut text)?;
                active.push(text.parse::<u32>().ok());
            }
            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
        }
    }
    let Some(mut slot) = candidate else {
        return Ok(Admission::Denied("worker_slots"));
    };
    let denied = match reading {
        Ok(None) => None, // Hosts without NVIDIA retain CPU/RAM admission.
        Err(_) => (!active.is_empty()).then_some("gpu_probe_failed"),
        Ok(Some(gpu)) => {
            let pending: u64 = active
                .iter()
                .map(|pid| {
                    let used = pid
                        .and_then(|pid| gpu.processes.get(&pid))
                        .copied()
                        .unwrap_or(0);
                    SIM_LANE_VRAM_MIB.saturating_sub(used)
                })
                .sum();
            // Preserve the existing one-worker progress floor, now host-wide.
            (!active.is_empty() && gpu.free < SIM_VRAM_RESERVE_MIB + pending + SIM_LANE_VRAM_MIB)
                .then_some("gpu_headroom")
        }
    };
    if let Some(reason) = denied {
        return Ok(Admission::Denied(reason));
    }
    write_pid(&mut slot, 0)?;
    Ok(Admission::Granted(slot))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn granted(result: Result<Admission>) -> std::result::Result<File, &'static str> {
        match result.unwrap() {
            Admission::Granted(file) => Ok(file),
            Admission::Denied(reason) => Err(reason),
        }
    }
    fn gpu(free: u64, processes: &[(u32, u64)]) -> Probe {
        Ok(Some(GpuMemory {
            free,
            processes: processes.iter().copied().collect(),
        }))
    }
    // A child spawned by any concurrent test holds copies of this process's
    // descriptors until its exec closes them (O_CLOEXEC does not shorten that),
    // and a flock lives until its last copy closes. A lock dropped here,
    // including the free slots each scan locks and releases, can therefore read
    // as held for a moment. Production polls past this; these helpers do too.
    // Only denials are retried, so a wrong grant still fails at once.
    const LOCK_SETTLE: Duration = Duration::from_secs(10);
    fn acquired(mut attempt: impl FnMut() -> Result<Admission>) -> File {
        let started = Instant::now();
        loop {
            match granted(attempt()) {
                Ok(file) => return file,
                Err(reason) if started.elapsed() > LOCK_SETTLE => {
                    panic!("still denied after settling: {reason}")
                }
                Err(_) => std::thread::sleep(Duration::from_millis(5)),
            }
        }
    }
    fn denied(expected: &str, mut attempt: impl FnMut() -> Result<Admission>) {
        let started = Instant::now();
        loop {
            match granted(attempt()) {
                Ok(_) => panic!("expected denial {expected}, but the slot was granted"),
                Err(reason) if reason == expected => return,
                Err(reason) if started.elapsed() > LOCK_SETTLE => {
                    panic!("expected denial {expected}, still denied with {reason}")
                }
                Err(_) => std::thread::sleep(Duration::from_millis(5)),
            }
        }
    }
    #[test]
    fn telemetry_does_not_lock_out_reservations_or_pid_publication() {
        let root = tempfile::tempdir().unwrap();
        let mut concurrent = None;
        denied("gpu_headroom", || {
            try_acquire(root.path(), 2, || {
                if concurrent.is_none() {
                    let lock = open(&root.path().join("admission.lock")).unwrap();
                    lock.try_lock()
                        .expect("GPU telemetry must not hold the host admission lock");
                    drop(lock);
                    let mut slot = granted(try_acquire(root.path(), 2, || gpu(8192, &[]))).unwrap();
                    publish_pid(root.path(), &mut slot, 42).unwrap();
                    concurrent = Some(slot);
                }
                // This snapshot predates the concurrently admitted worker.
                gpu(8192, &[])
            })
        });
        drop(concurrent);
        acquired(|| try_acquire(root.path(), 2, || gpu(8192, &[])));
    }
    #[test]
    fn failures_and_missing_tooling_preserve_admission_contracts() {
        let root = tempfile::tempdir().unwrap();
        let failed = || try_acquire(root.path(), 2, || Err("probe failed".into()));
        let missing = || try_acquire(root.path(), 2, || Ok(None));
        let first = acquired(failed);
        denied("gpu_probe_failed", failed);
        let second = acquired(missing);
        denied("worker_slots", missing);
        drop(first);
        drop(second);
        let _first = acquired(missing);
        acquired(missing);
    }
    #[cfg(unix)]
    #[test]
    fn telemetry_larger_than_a_pipe_buffer_is_drained_before_waiting() {
        let reading = probe_command(std::process::Command::new("sh").args(["-c",
            "printf '%65536s' ''; printf '\nGPU 0\n    FB Memory Usage\n        Free : 8192 MiB\n    Processes : None\n'"]))
            .unwrap().unwrap();
        assert_eq!(reading.free, 8192);
    }
    #[test]
    fn parses_first_gpu_graphics_memory_and_rejects_bad_telemetry() {
        let report = "GPU 00000000:01:00.0
    FB Memory Usage
        Free : 8192 MiB
    BAR1 Memory Usage
        Free : 99999 MiB
    Processes
        Process ID : 42
            Type : G
            Name : renderer: GPU worker
            Used GPU Memory : 2048 MiB
        Process ID : 43
            Used GPU Memory : N/A
GPU 00000000:02:00.0
    FB Memory Usage
        Free : 1 MiB
    Processes : None
";
        let reading = parse(report).unwrap();
        assert_eq!(reading.free, 8192);
        assert_eq!(reading.processes.get(&42), Some(&2048));
        assert!(!reading.processes.contains_key(&43));
        assert!(parse("8192").is_err());
        assert!(parse(&report.replace("8192 MiB", "N/A")).is_err());
        assert!(parse(&report.replace("Process ID : 42", "Process ID : invalid")).is_err());
        assert!(parse("GPU 0\n    FB Memory Usage\n        Free : 8192 MiB\n").is_err());
        let empty =
            parse("GPU 0\n    FB Memory Usage\n        Free : 8192 MiB\n    Processes : None\n")
                .unwrap();
        assert!(empty.processes.is_empty());
    }
}
