//! Bounded native distance-field jobs.
//!
//! The Lua side owns published buffers and submits copied flow walkability or
//! terrain solidity snapshots. Both kernels share one bounded worker and result
//! transport; no Lua state crosses into the worker thread.

use std::collections::HashMap;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::{self, JoinHandle};

#[path = "terrain_field.rs"]
mod terrain_field;

const FLOW_UNREACHABLE: f32 = 1_000_000.0;
const MAX_FLOW_JOBS: usize = 64;
const MAX_FLOW_CELLS: usize = 1_000_000;
const MAX_FLOW_BYTES: usize = 128 * 1024 * 1024;
const MAX_FLOW_HANDLE: u64 = (1 << 53) - 1;

static NEXT_FLOW_HANDLE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, PartialEq)]
struct FlowResult {
    distances: Vec<f32>,
    truncated: bool,
}

#[derive(Debug)]
enum EntryState {
    Pending,
    Ready(FlowResult),
}

struct FlowEntry {
    cancellation: Arc<AtomicBool>,
    state: EntryState,
    reserved_bytes: usize,
}

struct FlowData {
    entries: HashMap<u64, FlowEntry>,
    reserved_bytes: usize,
}

impl FlowData {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            reserved_bytes: 0,
        }
    }

    fn remove(&mut self, handle: u64) -> Option<FlowEntry> {
        let entry = self.entries.remove(&handle)?;
        self.reserved_bytes = self.reserved_bytes.saturating_sub(entry.reserved_bytes);
        Some(entry)
    }
}

struct FlowState {
    data: Mutex<FlowData>,
    wake: Condvar,
}

impl FlowState {
    fn new() -> Self {
        Self {
            data: Mutex::new(FlowData::new()),
            wake: Condvar::new(),
        }
    }
}

struct FlowWork {
    handle: u64,
    width: usize,
    height: usize,
    goal_x: i64,
    goal_y: i64,
    cap: usize,
    walkable: Vec<u8>,
    cancellation: Arc<AtomicBool>,
    reserved_bytes: usize,
    kernel: FieldKernel,
}

enum FieldKernel {
    Flow,
    Terrain { sub: usize },
}

struct FlowService {
    sender: Mutex<Option<SyncSender<FlowWork>>>,
    state: Arc<FlowState>,
    stopping: Arc<AtomicBool>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

static FLOW_SERVICE: OnceLock<Mutex<Option<Arc<FlowService>>>> = OnceLock::new();

fn flow_service_slot() -> &'static Mutex<Option<Arc<FlowService>>> {
    FLOW_SERVICE.get_or_init(|| Mutex::new(None))
}

fn active_flow_service() -> Option<Arc<FlowService>> {
    flow_service_slot().lock().unwrap().as_ref().cloned()
}

fn allocate_flow_handle() -> Option<u64> {
    NEXT_FLOW_HANDLE
        .try_update(Ordering::AcqRel, Ordering::Acquire, |next| {
            if next == 0 || next > MAX_FLOW_HANDLE {
                None
            } else {
                next.checked_add(1)
            }
        })
        .ok()
}

impl FlowService {
    fn new() -> Option<Arc<Self>> {
        let (sender, receiver) = mpsc::sync_channel(MAX_FLOW_JOBS);
        let state = Arc::new(FlowState::new());
        let stopping = Arc::new(AtomicBool::new(false));
        let worker_state = Arc::clone(&state);
        let worker_stopping = Arc::clone(&stopping);
        let worker = match thread::Builder::new()
            .name("stone-flow-worker".to_string())
            .spawn(move || flow_worker(receiver, worker_state, worker_stopping))
        {
            Ok(worker) => worker,
            Err(error) => {
                crate::log_once!("FLOW", "failed to start flow worker: {}", error);
                return None;
            }
        };
        Some(Arc::new(Self {
            sender: Mutex::new(Some(sender)),
            state,
            stopping,
            worker: Mutex::new(Some(worker)),
        }))
    }

    fn reserve(&self, work: &FlowWork) -> bool {
        let mut data = self.state.data.lock().unwrap();
        if data.entries.len() >= MAX_FLOW_JOBS
            || data
                .reserved_bytes
                .checked_add(work.reserved_bytes)
                .map_or(true, |bytes| bytes > MAX_FLOW_BYTES)
        {
            return false;
        }
        data.reserved_bytes += work.reserved_bytes;
        data.entries.insert(
            work.handle,
            FlowEntry {
                cancellation: Arc::clone(&work.cancellation),
                state: EntryState::Pending,
                reserved_bytes: work.reserved_bytes,
            },
        );
        true
    }

    fn submit(&self, work: FlowWork) -> bool {
        if self.stopping.load(Ordering::Acquire) {
            return false;
        }

        // Holding this lock across reservation and try_send makes shutdown
        // unable to detach the sender between those two operations.
        let sender = self.sender.lock().unwrap();
        if self.stopping.load(Ordering::Acquire) || !self.reserve(&work) {
            return false;
        }
        let Some(sender) = sender.as_ref() else {
            let mut data = self.state.data.lock().unwrap();
            data.remove(work.handle);
            return false;
        };
        match sender.try_send(work) {
            Ok(()) => true,
            Err(TrySendError::Full(work)) | Err(TrySendError::Disconnected(work)) => {
                let mut data = self.state.data.lock().unwrap();
                data.remove(work.handle);
                false
            }
        }
    }

    fn poll(&self, handle: u64) -> i32 {
        let data = self.state.data.lock().unwrap();
        match data.entries.get(&handle) {
            Some(entry) if entry.cancellation.load(Ordering::Acquire) => -1,
            Some(entry) => match &entry.state {
                EntryState::Pending => 0,
                EntryState::Ready(_) => 1,
            },
            None => -1,
        }
    }

    fn wait(&self, handle: u64) -> i32 {
        let mut data = self.state.data.lock().unwrap();
        loop {
            match data.entries.get(&handle) {
                Some(entry) if entry.cancellation.load(Ordering::Acquire) => return -1,
                Some(entry) => match &entry.state {
                    EntryState::Pending => {
                        data = self.state.wake.wait(data).unwrap();
                    }
                    EntryState::Ready(_) => return 1,
                },
                None => return -1,
            }
        }
    }

    fn copy(&self, handle: u64, output: *mut f32, output_len: i64) -> i32 {
        if output_len < 0 {
            return -1;
        }
        let output_len = output_len as usize;
        let mut data = self.state.data.lock().unwrap();
        let (required, truncated) = match data.entries.get(&handle) {
            Some(entry) if entry.cancellation.load(Ordering::Acquire) => return -1,
            Some(entry) => match &entry.state {
                EntryState::Pending => return -2,
                EntryState::Ready(result) => (result.distances.len(), result.truncated),
            },
            None => return -1,
        };
        if output.is_null() || output_len < required {
            crate::log_once!(
                "FLOW",
                "stone_flow_copy: output buffer is too small (need {}, got {})",
                required,
                output_len
            );
            return -1;
        }
        let entry = data.remove(handle).expect("ready flow entry disappeared");
        let EntryState::Ready(result) = entry.state else {
            return -2;
        };
        // The output pointer is part of the C ABI contract and is validated by
        // the caller. Keep the state lock until the result is consumed so a
        // concurrent cancel cannot release it during the copy.
        unsafe {
            ptr::copy_nonoverlapping(result.distances.as_ptr(), output, required);
        }
        self.state.wake.notify_all();
        if truncated {
            1
        } else {
            0
        }
    }

    fn cancel(&self, handle: u64) {
        let mut data = self.state.data.lock().unwrap();
        let remove_ready = match data.entries.get(&handle) {
            Some(entry) => {
                entry.cancellation.store(true, Ordering::Release);
                matches!(&entry.state, EntryState::Ready(_))
            }
            None => false,
        };
        if remove_ready {
            data.remove(handle);
        }
        self.state.wake.notify_all();
    }

    fn cancel_all(&self) {
        let mut data = self.state.data.lock().unwrap();
        let ready_handles: Vec<u64> = data
            .entries
            .iter()
            .filter_map(|(handle, entry)| {
                if matches!(&entry.state, EntryState::Ready(_)) {
                    Some(*handle)
                } else {
                    None
                }
            })
            .collect();
        for entry in data.entries.values() {
            entry.cancellation.store(true, Ordering::Release);
        }
        // Ready results have no worker message left to drain, so release them
        // now. Pending/running entries retain their reservation until the
        // worker consumes their canceled snapshot.
        for handle in ready_handles {
            data.remove(handle);
        }
        self.state.wake.notify_all();
    }

    fn shutdown(&self) {
        self.stopping.store(true, Ordering::Release);
        {
            let mut sender = self.sender.lock().unwrap();
            sender.take();
        }
        self.cancel_all();
        let worker = self.worker.lock().unwrap().take();
        if let Some(worker) = worker {
            if worker.join().is_err() {
                crate::log_once!("FLOW", "flow worker terminated with a panic");
            }
        }
    }
}

fn ensure_flow_service() -> Option<Arc<FlowService>> {
    let slot = flow_service_slot();
    let mut service = slot.lock().unwrap();
    if let Some(existing) = service.as_ref() {
        return Some(Arc::clone(existing));
    }
    let created = FlowService::new()?;
    *service = Some(Arc::clone(&created));
    Some(created)
}

fn flow_worker(receiver: Receiver<FlowWork>, state: Arc<FlowState>, stopping: Arc<AtomicBool>) {
    while let Ok(work) = receiver.recv() {
        if stopping.load(Ordering::Acquire) || work.cancellation.load(Ordering::Acquire) {
            let mut data = state.data.lock().unwrap();
            data.remove(work.handle);
            state.wake.notify_all();
            continue;
        }
        let result = match work.kernel {
            FieldKernel::Flow => compute_flow_with_cancel(
                work.width,
                work.height,
                work.goal_x,
                work.goal_y,
                work.cap,
                &work.walkable,
                &work.cancellation,
                &stopping,
            ),
            FieldKernel::Terrain { sub } => terrain_field::compute(
                work.width,
                work.height,
                sub,
                &work.walkable,
                &work.cancellation,
                &stopping,
            )
            .map(|distances| FlowResult {
                distances,
                truncated: false,
            }),
        };
        let Some(result) = result else {
            let mut data = state.data.lock().unwrap();
            data.remove(work.handle);
            state.wake.notify_all();
            continue;
        };

        let mut data = state.data.lock().unwrap();
        let remove = stopping.load(Ordering::Acquire)
            || work.cancellation.load(Ordering::Acquire)
            || !data.entries.contains_key(&work.handle);
        if remove {
            data.remove(work.handle);
        } else if let Some(entry) = data.entries.get_mut(&work.handle) {
            entry.state = EntryState::Ready(result);
        }
        state.wake.notify_all();
    }
}

fn request_shape(
    width: i32,
    height: i32,
    goal_x: i32,
    goal_y: i32,
    cap: i32,
    walkable_len: i64,
) -> Option<(usize, usize, i64, i64, usize, usize)> {
    if width <= 0 || height <= 0 || cap < 0 {
        return None;
    }
    let width = width as usize;
    let height = height as usize;
    let goal_x = goal_x as i64;
    let goal_y = goal_y as i64;
    let padded_width = width.checked_add(2)?;
    let padded_height = height.checked_add(2)?;
    let output_cells = width.checked_mul(height)?;
    let padded_cells = padded_width.checked_mul(padded_height)?;
    if output_cells > MAX_FLOW_CELLS {
        return None;
    }
    let expected_len = i64::try_from(padded_cells).ok()?;
    if walkable_len != expected_len {
        return None;
    }
    Some((width, height, goal_x, goal_y, cap as usize, padded_cells))
}

fn reserved_bytes(input_len: usize, output_cells: usize) -> Option<usize> {
    let per_cell = std::mem::size_of::<f32>().checked_add(std::mem::size_of::<usize>())?;
    input_len.checked_add(output_cells.checked_mul(per_cell)?)
}

#[cfg(test)]
fn compute_flow(
    width: usize,
    height: usize,
    goal_x: i64,
    goal_y: i64,
    cap: usize,
    walkable: &[u8],
) -> FlowResult {
    let cancellation = AtomicBool::new(false);
    let stopping = AtomicBool::new(false);
    compute_flow_with_cancel(
        width,
        height,
        goal_x,
        goal_y,
        cap,
        walkable,
        &cancellation,
        &stopping,
    )
    .expect("non-cancellable flow computation was cancelled")
}

// One byte per padded cell: bit 0 set where a body may stand, bits 1-4 the
// corners a blocked cell's static wall leaves open (corner i is bit i + 1;
// 0 = (0,0), 1 = (1,0), 2 = (1,1), 3 = (0,1)). A diagonal needs each elbow
// open or leaving the corner it passes open: MAG's one rule,
// `diagonal_elbow_clear` in mag-simulation/movement.rock.
const CELL_WALKABLE: u8 = 1;

fn elbow_clear(cell: u8, corner: u32) -> bool {
    cell & CELL_WALKABLE != 0 || cell & (2 << corner) != 0
}

// The corner of the x elbow (x + dx, y) that a hop by (dx, dy) passes; the
// y elbow's is the opposite one (`diagonal_step_corner`).
fn step_corner(delta_x: i64, delta_y: i64) -> u32 {
    match (delta_x > 0, delta_y > 0) {
        (true, true) => 3,
        (true, false) => 0,
        (false, true) => 2,
        (false, false) => 1,
    }
}

fn compute_flow_with_cancel(
    width: usize,
    height: usize,
    goal_x: i64,
    goal_y: i64,
    cap: usize,
    walkable: &[u8],
    cancellation: &AtomicBool,
    stopping: &AtomicBool,
) -> Option<FlowResult> {
    let output_cells = width.checked_mul(height)?;
    let padded_width = width.checked_add(2)?;
    let padded_cells = padded_width.checked_mul(height.checked_add(2)?)?;
    if width == 0
        || height == 0
        || goal_x < 0
        || goal_y < 0
        || goal_x >= width as i64
        || goal_y >= height as i64
        || walkable.len() != padded_cells
    {
        return Some(FlowResult {
            distances: vec![FLOW_UNREACHABLE; output_cells],
            truncated: false,
        });
    }
    let mut distances = vec![FLOW_UNREACHABLE; output_cells];
    let goal_x = goal_x as usize;
    let goal_y = goal_y as usize;
    let goal_index = goal_y * width + goal_x;
    let goal_padded_index = (goal_y + 1) * padded_width + goal_x + 1;
    if walkable[goal_padded_index] & CELL_WALKABLE == 0 {
        return Some(FlowResult {
            distances,
            truncated: false,
        });
    }
    distances[goal_index] = 0.0;
    let mut queue = Vec::with_capacity(output_cells);
    queue.push(goal_index);
    let mut queue_head = 0usize;
    let mut expanded = 0usize;
    let mut truncated = false;
    let cap = cap as f32;

    while queue_head < queue.len() {
        let current = queue[queue_head];
        queue_head += 1;
        expanded += 1;
        if expanded & 63 == 0
            && (cancellation.load(Ordering::Acquire) || stopping.load(Ordering::Acquire))
        {
            return None;
        }
        let current_x = current % width;
        let current_y = current / width;
        let next_distance = distances[current] + 1.0;
        for delta_y in -1i64..=1 {
            for delta_x in -1i64..=1 {
                if delta_x == 0 && delta_y == 0 {
                    continue;
                }
                let neighbor_x = current_x as i64 + delta_x;
                let neighbor_y = current_y as i64 + delta_y;
                let neighbor_padded =
                    (neighbor_y + 1) as usize * padded_width + (neighbor_x + 1) as usize;
                if walkable[neighbor_padded] & CELL_WALKABLE == 0 {
                    continue;
                }
                if delta_x != 0 && delta_y != 0 {
                    let elbow_a =
                        (current_y as i64 + 1) as usize * padded_width + (neighbor_x + 1) as usize;
                    let elbow_b = (neighbor_y + 1) as usize * padded_width + (current_x + 1);
                    let corner = step_corner(delta_x, delta_y);
                    if !elbow_clear(walkable[elbow_a], corner)
                        || !elbow_clear(walkable[elbow_b], (corner + 2) % 4)
                    {
                        continue;
                    }
                }
                if neighbor_x < 0
                    || neighbor_y < 0
                    || neighbor_x >= width as i64
                    || neighbor_y >= height as i64
                {
                    truncated = true;
                    continue;
                }
                let neighbor = neighbor_y as usize * width + neighbor_x as usize;
                if distances[neighbor] != FLOW_UNREACHABLE {
                    continue;
                }
                if next_distance > cap {
                    truncated = true;
                    continue;
                }
                distances[neighbor] = next_distance;
                queue.push(neighbor);
            }
        }
    }

    Some(FlowResult {
        distances,
        truncated,
    })
}

#[no_mangle]
pub extern "C" fn stone_flow_submit(
    width: i32,
    height: i32,
    goal_x: i32,
    goal_y: i32,
    cap: i32,
    walkable: *const u8,
    walkable_len: i64,
) -> u64 {
    let Some((width, height, goal_x, goal_y, cap, padded_cells)) =
        request_shape(width, height, goal_x, goal_y, cap, walkable_len)
    else {
        crate::log_once!("FLOW", "stone_flow_submit: invalid request shape");
        return 0;
    };
    if walkable.is_null() {
        crate::log_once!("FLOW", "stone_flow_submit: null walkability snapshot");
        return 0;
    }
    let output_cells = width * height;
    let Some(reserved_bytes) = reserved_bytes(padded_cells, output_cells) else {
        crate::log_once!("FLOW", "stone_flow_submit: request size overflow");
        return 0;
    };
    if reserved_bytes > MAX_FLOW_BYTES {
        crate::log_once!("FLOW", "stone_flow_submit: request exceeds bounded storage");
        return 0;
    }
    let snapshot = unsafe { std::slice::from_raw_parts(walkable, padded_cells) }.to_vec();
    submit_field(
        width,
        height,
        goal_x,
        goal_y,
        cap,
        snapshot,
        reserved_bytes,
        FieldKernel::Flow,
    )
}

// Terrain uses the same bounded queue, result transport and lifetime as flow.
// The snapshot contains the game's wallish classification, not tile enum values.
#[no_mangle]
pub extern "C" fn stone_terrain_submit(
    width: i32,
    height: i32,
    sub: i32,
    solid: *const u8,
    solid_len: i64,
) -> u64 {
    let Some((width, height, sub, cells, bytes)) =
        terrain_request_shape(width, height, sub, solid_len)
    else {
        crate::log_once!("FIELD", "stone_terrain_submit: invalid request shape");
        return 0;
    };
    if solid.is_null() {
        crate::log_once!("FIELD", "stone_terrain_submit: null solid snapshot");
        return 0;
    }
    let snapshot = unsafe { std::slice::from_raw_parts(solid, cells) }.to_vec();
    submit_field(
        width,
        height,
        0,
        0,
        0,
        snapshot,
        bytes,
        FieldKernel::Terrain { sub },
    )
}

fn terrain_request_shape(
    width: i32,
    height: i32,
    sub: i32,
    len: i64,
) -> Option<(usize, usize, usize, usize, usize)> {
    if width <= 0 || height <= 0 || sub <= 0 {
        return None;
    }
    let (width, height, sub) = (width as usize, height as usize, sub as usize);
    let cells = width.checked_mul(height)?;
    let samples = cells.checked_mul(sub)?.checked_mul(sub)?;
    if samples > MAX_FLOW_CELLS || i64::try_from(cells).ok()? != len {
        return None;
    }
    let bytes = cells.checked_add(samples.checked_mul(std::mem::size_of::<f32>())?)?;
    if bytes > MAX_FLOW_BYTES {
        return None;
    }
    Some((width, height, sub, cells, bytes))
}

fn submit_field(
    width: usize,
    height: usize,
    goal_x: i64,
    goal_y: i64,
    cap: usize,
    snapshot: Vec<u8>,
    reserved_bytes: usize,
    kernel: FieldKernel,
) -> u64 {
    let Some(handle) = allocate_flow_handle() else {
        crate::log_once!("FIELD", "field submit: handle space exhausted");
        return 0;
    };
    let work = FlowWork {
        handle,
        width,
        height,
        goal_x,
        goal_y,
        cap,
        walkable: snapshot,
        cancellation: Arc::new(AtomicBool::new(false)),
        reserved_bytes,
        kernel,
    };
    let Some(service) = ensure_flow_service() else {
        crate::log_once!("FIELD", "field submit: worker unavailable");
        return 0;
    };
    if service.submit(work) {
        handle
    } else {
        crate::log_once!("FIELD", "field submit: bounded worker queue is full");
        0
    }
}

#[no_mangle]
pub extern "C" fn stone_flow_poll(handle: u64) -> i32 {
    active_flow_service().map_or(-1, |service| service.poll(handle))
}

#[no_mangle]
pub extern "C" fn stone_flow_wait(handle: u64) -> i32 {
    active_flow_service().map_or(-1, |service| service.wait(handle))
}

#[no_mangle]
pub extern "C" fn stone_flow_copy(handle: u64, output: *mut f32, output_len: i64) -> i32 {
    active_flow_service().map_or(-1, |service| service.copy(handle, output, output_len))
}

#[no_mangle]
pub extern "C" fn stone_flow_cancel(handle: u64) {
    if let Some(service) = active_flow_service() {
        service.cancel(handle);
    }
}

/// Cancel all handles without resetting the monotonic handle namespace.
#[no_mangle]
pub extern "C" fn stone_flow_cancel_all() {
    if let Some(service) = active_flow_service() {
        service.cancel_all();
    }
}

#[no_mangle]
pub extern "C" fn stone_flow_shutdown() {
    let service = flow_service_slot().lock().unwrap().take();
    if let Some(service) = service {
        service.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Instant;

    static FLOW_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn padded_snapshot(width: usize, height: usize, open: bool) -> Vec<u8> {
        let value = if open { 1 } else { 0 };
        vec![value; (width + 2) * (height + 2)]
    }

    // A blocked cell whose wall leaves `corner` open.
    fn open_corner(corner: u32) -> u8 {
        2 << corner
    }

    fn interior_snapshot(width: usize, height: usize) -> Vec<u8> {
        let mut snapshot = padded_snapshot(width, height, false);
        let padded_width = width + 2;
        for y in 0..height {
            for x in 0..width {
                snapshot[(y + 1) * padded_width + x + 1] = 1;
            }
        }
        snapshot
    }

    #[test]
    fn open_grid_routes_with_chebyshev_distances() {
        let width = 5;
        let height = 5;
        let walkable = interior_snapshot(width, height);
        let result = compute_flow(width, height, 2, 2, 64, &walkable);

        assert!(!result.truncated);
        assert_eq!(result.distances[2 + 2 * width], 0.0);
        assert_eq!(result.distances[0], 2.0);
        assert_eq!(result.distances[4 + 4 * width], 2.0);
    }

    #[test]
    fn diagonal_requires_both_elbows_open() {
        let width = 2;
        let height = 2;
        let mut walkable = padded_snapshot(width, height, true);
        // Block the east elbow. The southeast tile must be reached around it.
        walkable[(0 + 1) * (width + 2) + (1 + 1)] = 0;
        let result = compute_flow(width, height, 0, 0, 64, &walkable);

        assert_eq!(result.distances[1 + width], 2.0);
    }

    #[test]
    fn diagonal_passes_elbows_whose_walls_leave_the_corner_open() {
        let width = 2;
        let height = 2;
        let padded_width = width + 2;
        let mut cells = padded_snapshot(width, height, true);
        // The hop from (0,0) to (1,1) passes corner 3 of the east elbow and
        // corner 1 of the south elbow; both elbows are walls.
        let east = padded_width + 2;
        let south = 2 * padded_width + 1;
        cells[east] = open_corner(3);
        cells[south] = open_corner(1);
        let result = compute_flow(width, height, 0, 0, 64, &cells);
        assert_eq!(result.distances[1 + width], 1.0);
        assert_eq!(
            result.distances[1], FLOW_UNREACHABLE,
            "a wall with open corners stays a wall"
        );
        assert_eq!(result.distances[width], FLOW_UNREACHABLE);

        // One elbow covering its corner pinches the hop.
        cells[south] = open_corner(0);
        let result = compute_flow(width, height, 0, 0, 64, &cells);
        assert_eq!(result.distances[1 + width], FLOW_UNREACHABLE);
    }

    #[test]
    fn open_halo_marks_boundary_truncation() {
        let width = 2;
        let height = 1;
        let walkable = padded_snapshot(width, height, true);
        let result = compute_flow(width, height, 0, 0, 64, &walkable);

        assert!(result.truncated);
        assert_eq!(result.distances, vec![0.0, 1.0]);
    }

    #[test]
    fn cap_marks_truncation_and_keeps_reached_tiles() {
        let width = 3;
        let height = 1;
        let mut walkable = padded_snapshot(width, height, true);
        // Keep the one-cell halo closed so only the cap can truncate.
        let padded_width = width + 2;
        for y in [0, height + 1] {
            for x in 0..padded_width {
                walkable[y * padded_width + x] = 0;
            }
        }
        for y in 0..height + 2 {
            walkable[y * padded_width] = 0;
            walkable[y * padded_width + width + 1] = 0;
        }
        let result = compute_flow(width, height, 0, 0, 1, &walkable);

        assert!(result.truncated);
        assert_eq!(result.distances, vec![0.0, 1.0, FLOW_UNREACHABLE]);
    }

    #[test]
    fn blocked_goal_is_unreachable_without_truncation() {
        let width = 3;
        let height = 3;
        let mut walkable = padded_snapshot(width, height, true);
        walkable[1 + (1 * (width + 2))] = 0;
        let result = compute_flow(width, height, 0, 0, 64, &walkable);

        assert_eq!(result.distances[0], FLOW_UNREACHABLE);
        assert!(!result.truncated);
    }

    #[test]
    fn outside_goal_publishes_empty_result() {
        let width = 3;
        let height = 2;
        let walkable = interior_snapshot(width, height);
        let result = compute_flow(width, height, -1, 0, 64, &walkable);

        assert!(!result.truncated);
        assert_eq!(result.distances, vec![FLOW_UNREACHABLE; width * height]);
    }

    #[test]
    fn medium_open_grid_matches_full_chebyshev_reference() {
        let width = 129;
        let height = 129;
        let mut walkable = padded_snapshot(width, height, false);
        let padded_width = width + 2;
        for y in 0..height {
            for x in 0..width {
                walkable[(y + 1) * padded_width + x + 1] = 1;
            }
        }
        let started = Instant::now();
        let result = compute_flow(
            width,
            height,
            (width / 2) as i64,
            (height / 2) as i64,
            64,
            &walkable,
        );
        let elapsed = started.elapsed();

        assert!(!result.truncated);
        for y in 0..height {
            for x in 0..width {
                let expected = x.abs_diff(width / 2).max(y.abs_diff(height / 2)) as f32;
                assert_eq!(result.distances[y * width + x], expected);
            }
        }
        eprintln!("flow 129x129 cap64 kernel: {:?}", elapsed);
    }

    #[test]
    fn ffi_copies_caller_owned_snapshot_and_consumes_result() {
        let _guard = FLOW_TEST_LOCK.lock().unwrap();
        stone_flow_shutdown();
        let width = 3;
        let height = 1;
        let mut walkable = padded_snapshot(width, height, false);
        let padded_width = width + 2;
        for x in 1..=width {
            walkable[padded_width + x] = 1;
        }
        let handle = stone_flow_submit(
            width as i32,
            height as i32,
            0,
            0,
            64,
            walkable.as_ptr(),
            walkable.len() as i64,
        );
        assert_ne!(handle, 0);
        walkable.fill(0);
        assert_eq!(stone_flow_wait(handle), 1);
        assert_eq!(stone_flow_poll(handle), 1);
        let mut output = vec![0.0; width * height];
        assert_eq!(
            stone_flow_copy(handle, output.as_mut_ptr(), output.len() as i64),
            0
        );
        assert_eq!(output, vec![0.0, 1.0, 2.0]);
        assert_eq!(stone_flow_poll(handle), -1);
        stone_flow_shutdown();
    }

    #[test]
    fn ffi_cancel_all_expires_pending_handle() {
        let _guard = FLOW_TEST_LOCK.lock().unwrap();
        stone_flow_shutdown();
        let width = 129;
        let height = 129;
        let walkable = interior_snapshot(width, height);
        let handle = stone_flow_submit(
            width as i32,
            height as i32,
            (width / 2) as i32,
            (height / 2) as i32,
            64,
            walkable.as_ptr(),
            walkable.len() as i64,
        );
        assert_ne!(handle, 0);
        stone_flow_cancel_all();
        assert_eq!(stone_flow_wait(handle), -1);
        assert_eq!(stone_flow_poll(handle), -1);
        stone_flow_shutdown();
    }

    #[test]
    fn ffi_cancel_makes_pending_handle_missing_immediately() {
        let _guard = FLOW_TEST_LOCK.lock().unwrap();
        stone_flow_shutdown();
        let width = 129;
        let height = 129;
        let walkable = interior_snapshot(width, height);
        let handle = stone_flow_submit(
            width as i32,
            height as i32,
            (width / 2) as i32,
            (height / 2) as i32,
            64,
            walkable.as_ptr(),
            walkable.len() as i64,
        );
        assert_ne!(handle, 0);
        stone_flow_cancel(handle);
        assert_eq!(stone_flow_poll(handle), -1);
        assert_eq!(stone_flow_wait(handle), -1);
        stone_flow_shutdown();
    }
}
