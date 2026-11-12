//! Copied occupancy snapshots; no Lua or graphics state crosses this worker.
//!
//! The service keeps the occupancy last published for each caller-chosen
//! snapshot key. A job diffs new occupancy against it, and publishing a
//! finished job moves that job's copy in as the key's snapshot, so the caller
//! holds no copy and none is made on the publishing thread.
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::thread::JoinHandle;

const MAX_JOBS: usize = 16;
const MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_RESERVED: usize = 128 * 1024 * 1024;
struct Reservation {
    bytes: usize,
    total: Arc<AtomicUsize>,
}
impl Drop for Reservation {
    fn drop(&mut self) {
        self.total.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}
static NEXT: AtomicU64 = AtomicU64::new(1);
struct ResultData {
    pixels: Vec<u8>,
    region: [f32; 4],
}
struct Entry {
    snapshot: i64,
    cancelled: Arc<AtomicBool>,
    result: Option<ResultData>,
    _reservation: Arc<Reservation>,
}
struct Work {
    handle: u64,
    width: usize,
    samples: usize,
    border: i32,
    old: Option<Arc<Vec<u8>>>,
    next: Vec<u8>,
    cancelled: Arc<AtomicBool>,
    _reservation: Arc<Reservation>,
}
struct Service {
    entries: Arc<Mutex<HashMap<u64, Entry>>>,
    // Locked after `entries` wherever both are held.
    snapshots: Mutex<HashMap<i64, Arc<Vec<u8>>>>,
    sender: mpsc::SyncSender<Work>,
    worker: JoinHandle<()>,
    reserved: Arc<AtomicUsize>,
}
static SERVICE: OnceLock<Mutex<Option<Service>>> = OnceLock::new();
fn slot() -> &'static Mutex<Option<Service>> {
    SERVICE.get_or_init(|| Mutex::new(None))
}
fn start() -> Option<Service> {
    let entries = Arc::new(Mutex::new(HashMap::<u64, Entry>::new()));
    let output = Arc::clone(&entries);
    let (sender, receiver) = mpsc::sync_channel::<Work>(MAX_JOBS);
    let worker = std::thread::Builder::new()
        .name("stone-relight-worker".into())
        .spawn(move || {
            while let Ok(work) = receiver.recv() {
                let result = diff(
                    work.width,
                    work.samples,
                    work.border,
                    work.old.as_ref().map_or(&[][..], |old| old.as_slice()),
                    work.next,
                    &work.cancelled,
                );
                let mut entries = output.lock().unwrap();
                if let Some(entry) = entries.get_mut(&work.handle) {
                    if let Some(result) = result {
                        entry.result = Some(result);
                    }
                }
            }
        })
        .map_err(|error| crate::log_once!("RELIGHT", "worker start failed: {}", error))
        .ok()?;
    Some(Service {
        entries,
        snapshots: Mutex::new(HashMap::new()),
        sender,
        worker,
        reserved: Arc::new(AtomicUsize::new(0)),
    })
}
fn diff(
    width: usize,
    samples: usize,
    border: i32,
    old: &[u8],
    next: Vec<u8>,
    cancelled: &AtomicBool,
) -> Option<ResultData> {
    if cancelled.load(Ordering::Acquire) {
        return None;
    }
    if old.len() != next.len() {
        return Some(ResultData {
            pixels: next,
            region: [-1e9, -1e9, 1e9, 1e9],
        });
    }
    let height = next.len() / 4 / width;
    let mut region = [1e9f32, 1e9f32, -1e9f32, -1e9f32];
    for y in 0..height {
        if cancelled.load(Ordering::Acquire) {
            return None;
        }
        for x in 0..width {
            let index = (y * width + x) * 4;
            if old[index..index + 4] != next[index..index + 4] {
                let gx = (x / samples) as i32 - border;
                let gz = ((height - 1 - y) / samples) as i32 - border;
                region[0] = region[0].min(gx as f32);
                region[1] = region[1].min(gz as f32);
                region[2] = region[2].max(gx as f32);
                region[3] = region[3].max(gz as f32);
            }
        }
    }
    Some(ResultData {
        pixels: next,
        region,
    })
}

// A positive job handle, 0 when the worker is full (retry later), -1 for
// invalid arguments. The job compares `next` with the snapshot published for
// `snapshot`; a key with no snapshot of this length reports the whole domain.
#[no_mangle]
pub unsafe extern "C" fn stone_relight_diff_submit(
    snapshot: i64,
    width: i32,
    samples: i32,
    border: i32,
    next: *const u8,
    len: i64,
) -> i64 {
    if width <= 0
        || samples <= 0
        || border < 0
        || len <= 0
        || len as u64 > MAX_BYTES as u64
        || len % (width as i64 * 4) != 0
        || width % samples != 0
        || (len / (width as i64 * 4)) % samples as i64 != 0
        || next.is_null()
    {
        return -1;
    }
    let mut service_slot = slot().lock().unwrap();
    if service_slot.is_none() {
        *service_slot = start();
    }
    let Some(service) = service_slot.as_ref() else {
        return -1;
    };
    let mut entries = service.entries.lock().unwrap();
    if entries.len() >= MAX_JOBS {
        return 0;
    }
    let bytes = len as usize;
    if service
        .reserved
        .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
            used.checked_add(bytes).filter(|next| *next <= MAX_RESERVED)
        })
        .is_err()
    {
        return 0;
    }
    let reservation = Arc::new(Reservation {
        bytes,
        total: Arc::clone(&service.reserved),
    });
    let handle = NEXT.fetch_add(1, Ordering::Relaxed);
    if handle == 0 || handle >= (1 << 53) {
        return 0;
    }
    let cancelled = Arc::new(AtomicBool::new(false));
    let old = service
        .snapshots
        .lock()
        .unwrap()
        .get(&snapshot)
        .filter(|old| old.len() == len as usize)
        .cloned();
    let work = Work {
        handle,
        width: width as usize,
        samples: samples as usize,
        border,
        old,
        next: std::slice::from_raw_parts(next, len as usize).to_vec(),
        cancelled: Arc::clone(&cancelled),
        _reservation: Arc::clone(&reservation),
    };
    entries.insert(
        handle,
        Entry {
            snapshot,
            cancelled,
            result: None,
            _reservation: reservation,
        },
    );
    if service.sender.try_send(work).is_err() {
        entries.remove(&handle);
        return 0;
    }
    handle as i64
}

// 0 pending, 1 published and consumed, -1 invalid. Publishing writes the
// changed tile bounds to `region` and makes the job's occupancy the snapshot
// later jobs for its key compare against.
#[no_mangle]
pub unsafe extern "C" fn stone_relight_diff_publish(handle: u64, region: *mut f32) -> i32 {
    let service_slot = slot().lock().unwrap();
    let Some(service) = service_slot.as_ref() else {
        return -1;
    };
    let mut entries = service.entries.lock().unwrap();
    let Some(entry) = entries.get(&handle) else {
        return -1;
    };
    if entry.result.is_none() {
        return 0;
    }
    if region.is_null() {
        return -1;
    }
    let entry = entries.remove(&handle).unwrap();
    let result = entry.result.unwrap();
    std::ptr::copy_nonoverlapping(result.region.as_ptr(), region, 4);
    service
        .snapshots
        .lock()
        .unwrap()
        .insert(entry.snapshot, Arc::new(result.pixels));
    1
}
// Forget a key's snapshot, so its next job reports the whole domain. The
// caller cancels the key's pending job first.
#[no_mangle]
pub extern "C" fn stone_relight_snapshot_drop(snapshot: i64) {
    if let Some(service) = slot().lock().unwrap().as_ref() {
        service.snapshots.lock().unwrap().remove(&snapshot);
    }
}
#[no_mangle]
pub extern "C" fn stone_relight_diff_cancel(handle: u64) {
    if let Some(service) = slot().lock().unwrap().as_ref() {
        if let Some(entry) = service.entries.lock().unwrap().remove(&handle) {
            entry.cancelled.store(true, Ordering::Release);
        }
    }
}
pub(crate) fn shutdown() {
    let Some(service) = slot().lock().unwrap().take() else {
        return;
    };
    for (_, entry) in service.entries.lock().unwrap().drain() {
        entry.cancelled.store(true, Ordering::Release);
    }
    drop(service.sender);
    let _ = service.worker.join();
}
#[cfg(test)]
mod tests {
    use super::*;
    fn publish(handle: i64) -> [f32; 4] {
        let mut region = [0.0; 4];
        for _ in 0..2000 {
            let result = unsafe { stone_relight_diff_publish(handle as u64, region.as_mut_ptr()) };
            assert!(result >= 0);
            if result == 1 {
                return region;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        panic!("relight diff never finished");
    }
    #[test]
    fn published_snapshot_lifecycle_and_cancellation() {
        const KEY: i64 = 3;
        const WHOLE: [f32; 4] = [-1e9, -1e9, 1e9, 1e9];
        let mut next = vec![0u8; 8 * 8 * 4];
        let submit = |next: &[u8], samples: i32| unsafe {
            stone_relight_diff_submit(KEY, 8, samples, 1, next.as_ptr(), next.len() as i64)
        };
        // A key with no snapshot reports the whole domain and publishes one.
        let first = submit(&next, 2);
        assert!(first > 0);
        assert_eq!(publish(first), WHOLE);
        // The job owns its copy: edits after submit are not part of it.
        next[0] = 7;
        let second = submit(&next, 2);
        assert!(second > 0);
        next.fill(9);
        let mut region = publish(second);
        assert_eq!(region, [-1.0, 2.0, -1.0, 2.0]);
        assert_eq!(
            unsafe { stone_relight_diff_publish(second as u64, region.as_mut_ptr()) },
            -1
        );
        // The second job's occupancy is now the snapshot; all nines differ
        // from it in every tile.
        let third = submit(&next, 2);
        assert_eq!(publish(third), [-1.0, -1.0, 2.0, 2.0]);
        let unchanged = submit(&next, 2);
        let region = publish(unchanged);
        assert!(region[0] > region[2]);
        let pending = submit(&next, 2);
        stone_relight_diff_cancel(pending as u64);
        let mut region = [0.0; 4];
        assert_eq!(
            unsafe { stone_relight_diff_publish(pending as u64, region.as_mut_ptr()) },
            -1
        );
        stone_relight_snapshot_drop(KEY);
        let dropped = submit(&next, 2);
        assert_eq!(publish(dropped), WHOLE);
        shutdown();
        assert_eq!(submit(&next, 0), -1);
    }
    #[test]
    fn occupancy_region_uses_rgba_and_reversed_sample_rows() {
        let old = vec![0; 8 * 8 * 4];
        let mut next = old.clone();
        next[(8 + 6) * 4 + 3] = 1;
        next[(6 * 8 + 1) * 4] = 1;
        let result = diff(8, 2, 1, &old, next, &AtomicBool::new(false)).unwrap();
        assert_eq!(result.region, [-1.0, -1.0, 2.0, 2.0]);
    }
    #[test]
    fn identical_snapshots_have_no_dirty_tiles_and_cancel_discards() {
        let pixels = vec![5; 4 * 4 * 4];
        let result = diff(4, 1, 0, &pixels, pixels.clone(), &AtomicBool::new(false)).unwrap();
        assert!(result.region[0] > result.region[2]);
        assert!(diff(4, 1, 0, &pixels, pixels.clone(), &AtomicBool::new(true)).is_none());
    }
}
