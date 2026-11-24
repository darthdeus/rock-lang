//! Copied occupancy snapshots; no Lua or graphics state crosses this worker.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, DefaultHasher};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::jobs::{Jobs, Refused, Taken};

/// rock-ffi-array stone-native RELIGHT_REGION
type RelightRegion = [f32; 4];
/// rock-ffi-array stone-native RELIGHT_PIXEL
type RelightPixel = [u8; 4];
const REGION_LANES: usize = std::mem::size_of::<RelightRegion>() / std::mem::size_of::<f32>();
const PIXEL_BYTES: usize = std::mem::size_of::<RelightPixel>();

const MAX_BYTES: usize = 8 * 1024 * 1024;

static JOBS: Jobs<Work, ResultData> =
    Jobs::new("stone-relight-worker", 16, 128 * 1024 * 1024, true, run);
// Locked before `JOBS` wherever both are held.
static SNAPSHOTS: Mutex<HashMap<i64, Arc<Vec<u8>>, BuildHasherDefault<DefaultHasher>>> =
    Mutex::new(HashMap::with_hasher(BuildHasherDefault::new()));

struct ResultData {
    snapshot: i64,
    pixels: Vec<u8>,
    region: RelightRegion,
}
struct Work {
    snapshot: i64,
    width: usize,
    samples: usize,
    border: i32,
    old: Option<Arc<Vec<u8>>>,
    next: Vec<u8>,
}
fn run(work: Work, cancelled: &AtomicBool) -> Option<ResultData> {
    let (pixels, region) = diff(
        work.width,
        work.samples,
        work.border,
        work.old.as_ref().map_or(&[][..], |old| old.as_slice()),
        work.next,
        cancelled,
    )?;
    Some(ResultData {
        snapshot: work.snapshot,
        pixels,
        region,
    })
}
fn diff(
    width: usize,
    samples: usize,
    border: i32,
    old: &[u8],
    next: Vec<u8>,
    cancelled: &AtomicBool,
) -> Option<(Vec<u8>, RelightRegion)> {
    if cancelled.load(Ordering::Acquire) {
        return None;
    }
    if old.len() != next.len() {
        return Some((next, [-1e9, -1e9, 1e9, 1e9]));
    }
    let height = next.len() / PIXEL_BYTES / width;
    let mut region = [1e9f32, 1e9f32, -1e9f32, -1e9f32];
    for y in 0..height {
        if cancelled.load(Ordering::Acquire) {
            return None;
        }
        for x in 0..width {
            let index = (y * width + x) * PIXEL_BYTES;
            if old[index..index + PIXEL_BYTES] != next[index..index + PIXEL_BYTES] {
                let gx = (x / samples) as i32 - border;
                let gz = ((height - 1 - y) / samples) as i32 - border;
                region[0] = region[0].min(gx as f32);
                region[1] = region[1].min(gz as f32);
                region[2] = region[2].max(gx as f32);
                region[3] = region[3].max(gz as f32);
            }
        }
    }
    Some((next, region))
}

/// rock-ffi stone-native
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
        || len % (width as i64 * PIXEL_BYTES as i64) != 0
        || width % samples != 0
        || (len / (width as i64 * PIXEL_BYTES as i64)) % samples as i64 != 0
        || next.is_null()
    {
        return -1;
    }
    let len = len as usize;
    let snapshots = SNAPSHOTS.lock().unwrap();
    let work = Work {
        snapshot,
        width: width as usize,
        samples: samples as usize,
        border,
        old: snapshots
            .get(&snapshot)
            .filter(|old| old.len() == len)
            .cloned(),
        next: std::slice::from_raw_parts(next, len).to_vec(),
    };
    match JOBS.submit(work, len) {
        Ok(handle) => handle as i64,
        Err(Refused::Worker(error)) => {
            crate::log_once!("RELIGHT", "worker start failed: {}", error);
            -1
        }
        Err(Refused::Busy | Refused::Handles) => 0,
    }
}

/// rock-ffi stone-native
#[no_mangle]
pub unsafe extern "C" fn stone_relight_diff_publish(handle: u64, region: *mut f32) -> i32 {
    let mut snapshots = SNAPSHOTS.lock().unwrap();
    match JOBS.take(handle, |_| !region.is_null()) {
        Taken::Missing | Taken::Rejected => -1,
        Taken::Pending => 0,
        Taken::Ready(result) => {
            std::ptr::copy_nonoverlapping(result.region.as_ptr(), region, REGION_LANES);
            snapshots.insert(result.snapshot, Arc::new(result.pixels));
            1
        }
    }
}
// The caller cancels the key's pending job first.
/// rock-ffi stone-native
#[no_mangle]
pub extern "C" fn stone_relight_snapshot_drop(snapshot: i64) {
    SNAPSHOTS.lock().unwrap().remove(&snapshot);
}
/// rock-ffi stone-native
#[no_mangle]
pub extern "C" fn stone_relight_diff_cancel(handle: u64) {
    JOBS.cancel(handle);
}
pub(crate) fn shutdown() {
    JOBS.shutdown();
    SNAPSHOTS.lock().unwrap().clear();
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
        let first = submit(&next, 2);
        assert!(first > 0);
        assert_eq!(publish(first), WHOLE);
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
    fn occupancy_region_uses_rgba_and_reversed_sample_rows_unless_cancelled() {
        let old = vec![0; 8 * 8 * 4];
        let mut next = old.clone();
        next[(8 + 6) * 4 + 3] = 1;
        next[(6 * 8 + 1) * 4] = 1;
        let (_, region) = diff(8, 2, 1, &old, next.clone(), &AtomicBool::new(false)).unwrap();
        assert_eq!(region, [-1.0, -1.0, 2.0, 2.0]);
        assert!(diff(8, 2, 1, &old, next, &AtomicBool::new(true)).is_none());
    }
}
