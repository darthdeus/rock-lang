//! STONE_FRAME_TIMING selects a JSONL export at the first teardown. Events are
//! bounded in memory; overflow is counted. No query, wait or file I/O is added
//! to a frame. GPU timestamps share a device clock, not the CPU clock.
use std::{
    cell::RefCell,
    fs::File,
    io::{BufWriter, Write},
    path::PathBuf,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

const CAPACITY: usize = 65_536;

struct Record {
    event: &'static str,
    frame: u64,
    observed_frame: u64,
    ns: u64,
    cpu_ns: Option<u64>,
    gpu: Option<(u64, u64)>,
    tag: Option<u32>,
}

struct Recorder {
    path: PathBuf,
    origin: Instant,
    unix_ns: u128,
    records: Vec<Record>,
    dropped: u64,
}

impl Recorder {
    fn from_env() -> Option<Self> {
        let path = std::env::var_os("STONE_FRAME_TIMING").filter(|p| !p.is_empty())?;
        let origin = Instant::now();
        let unix_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_nanos();
        Some(Self {
            path: path.into(),
            origin,
            unix_ns,
            records: Vec::with_capacity(CAPACITY),
            dropped: 0,
        })
    }

    fn push(&mut self, record: Record) {
        if self.records.len() == CAPACITY {
            self.dropped += 1;
        } else {
            self.records.push(record);
        }
    }

    fn write(&self, out: &mut impl Write) -> std::io::Result<()> {
        writeln!(
            out,
            "{}",
            serde_json::json!({"version":1,"unix_origin_ns":self.unix_ns.to_string(),"capacity":CAPACITY,"dropped":self.dropped,
            "frame_identity":"native swap ordinal (starts at one)","gpu_clock":"device timestamps; compare durations, not CPU absolute time"})
        )?;
        for r in &self.records {
            writeln!(
                out,
                "{}",
                serde_json::json!({"event":r.event,"frame":r.frame,"observed_frame":r.observed_frame,
                "ns":r.ns,"cpu_ns":r.cpu_ns,"gpu_start_ns":r.gpu.map(|v|v.0),"gpu_end_ns":r.gpu.map(|v|v.1),"tag":r.tag})
            )?;
        }
        out.flush()
    }
}

thread_local! { static RECORDER: RefCell<Option<Recorder>> = RefCell::new(Recorder::from_env()); }

#[cfg(target_os = "linux")]
fn thread_cpu_ns() -> Option<u64> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) } != 0 {
        return None;
    }
    Some(time.tv_sec as u64 * 1_000_000_000 + time.tv_nsec as u64)
}
#[cfg(not(target_os = "linux"))]
fn thread_cpu_ns() -> Option<u64> {
    None
}

pub(crate) fn event(event: &'static str, frame: u64) {
    gpu(event, frame, None, None);
}

pub(crate) fn gpu(event: &'static str, frame: u64, gpu: Option<(u64, u64)>, tag: Option<u32>) {
    RECORDER.with(|cell| {
        let mut borrowed = cell.borrow_mut();
        let Some(recorder) = borrowed.as_mut() else {
            return;
        };
        let ns = recorder.origin.elapsed().as_nanos() as u64;
        let cpu_ns = thread_cpu_ns();
        recorder.push(Record {
            event,
            frame,
            observed_frame: crate::stone_gl_swap_count(),
            ns,
            cpu_ns,
            gpu,
            tag,
        });
    });
}

pub(crate) fn finish() {
    RECORDER.with(|cell| {
        let Some(recorder) = cell.borrow_mut().take() else {
            return;
        };
        let result =
            File::create(&recorder.path).and_then(|file| recorder.write(&mut BufWriter::new(file)));
        if let Err(error) = result {
            log::error!(target: "STONE", "native frame timing export failed: {error}");
        }
    });
}
