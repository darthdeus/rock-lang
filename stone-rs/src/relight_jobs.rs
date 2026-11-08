//! Copied occupancy snapshots; no Lua or graphics state crosses this worker.
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
    cancelled: Arc<AtomicBool>,
    result: Option<ResultData>,
    _reservation: Arc<Reservation>,
}
struct Work {
    handle: u64,
    width: usize,
    samples: usize,
    border: i32,
    old: Vec<u8>,
    next: Vec<u8>,
    cancelled: Arc<AtomicBool>,
    _reservation: Arc<Reservation>,
}
struct Service {
    entries: Arc<Mutex<HashMap<u64, Entry>>>,
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
                    &work.old,
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

#[no_mangle]
pub unsafe extern "C" fn stone_relight_diff_submit(
    width: i32,
    samples: i32,
    border: i32,
    old: *const u8,
    old_len: i64,
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
        || (old_len != 0 && old_len != len)
        || next.is_null()
        || (old_len > 0 && old.is_null())
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
    let bytes = len as usize + old_len as usize;
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
    let work = Work {
        handle,
        width: width as usize,
        samples: samples as usize,
        border,
        old: if old_len == 0 {
            Vec::new()
        } else {
            std::slice::from_raw_parts(old, old_len as usize).to_vec()
        },
        next: std::slice::from_raw_parts(next, len as usize).to_vec(),
        cancelled: Arc::clone(&cancelled),
        _reservation: Arc::clone(&reservation),
    };
    entries.insert(
        handle,
        Entry {
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

// 0 pending, 1 copied and consumed, -1 invalid. Failed copies retain the result.
#[no_mangle]
pub unsafe extern "C" fn stone_relight_diff_copy(
    handle: u64,
    pixels: *mut u8,
    len: i64,
    region: *mut f32,
) -> i32 {
    let service_slot = slot().lock().unwrap();
    let Some(service) = service_slot.as_ref() else {
        return -1;
    };
    let mut entries = service.entries.lock().unwrap();
    let Some(entry) = entries.get(&handle) else {
        return -1;
    };
    let Some(result) = entry.result.as_ref() else {
        return 0;
    };
    if pixels.is_null() || region.is_null() || len < 0 || len as usize != result.pixels.len() {
        return -1;
    }
    std::ptr::copy_nonoverlapping(result.pixels.as_ptr(), pixels, result.pixels.len());
    std::ptr::copy_nonoverlapping(result.region.as_ptr(), region, 4);
    entries.remove(&handle);
    1
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
    #[test]
    fn copied_snapshot_lifecycle_and_cancellation() {
        let old = vec![0; 8 * 8 * 4];
        let mut next = old.clone();
        next[0] = 7;
        let handle = unsafe {
            stone_relight_diff_submit(
                8,
                2,
                1,
                old.as_ptr(),
                old.len() as i64,
                next.as_ptr(),
                next.len() as i64,
            )
        };
        assert_ne!(handle, 0);
        next.fill(9);
        let mut output = vec![0; old.len()];
        let mut region = [0.0; 4];
        for _ in 0..2000 {
            let result = unsafe {
                stone_relight_diff_copy(
                    handle as u64,
                    output.as_mut_ptr(),
                    output.len() as i64,
                    region.as_mut_ptr(),
                )
            };
            assert!(result >= 0);
            if result == 1 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(output[0], 7);
        assert!(output[1..].iter().all(|v| *v == 0));
        assert_eq!(region, [-1.0, 2.0, -1.0, 2.0]);
        assert_eq!(
            unsafe {
                stone_relight_diff_copy(
                    handle as u64,
                    output.as_mut_ptr(),
                    output.len() as i64,
                    region.as_mut_ptr(),
                )
            },
            -1
        );
        let pending = unsafe {
            stone_relight_diff_submit(
                8,
                2,
                1,
                old.as_ptr(),
                old.len() as i64,
                next.as_ptr(),
                next.len() as i64,
            )
        };
        stone_relight_diff_cancel(pending as u64);
        assert_eq!(
            unsafe {
                stone_relight_diff_copy(
                    pending as u64,
                    output.as_mut_ptr(),
                    output.len() as i64,
                    region.as_mut_ptr(),
                )
            },
            -1
        );
        shutdown();
        assert_eq!(
            unsafe {
                stone_relight_diff_submit(
                    8,
                    0,
                    1,
                    old.as_ptr(),
                    old.len() as i64,
                    next.as_ptr(),
                    next.len() as i64,
                )
            },
            -1
        );
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
