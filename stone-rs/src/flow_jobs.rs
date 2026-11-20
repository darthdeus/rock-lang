//! No Lua state crosses into the worker thread; it reads copied snapshots.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::jobs::{Jobs, Refused, Taken};

#[path = "terrain_field.rs"]
mod terrain_field;

const FLOW_UNREACHABLE: f32 = 1_000_000.0;
const MAX_FLOW_CELLS: usize = 1_000_000;
const MAX_FLOW_BYTES: usize = 128 * 1024 * 1024;

static FIELD_JOBS: Jobs<FieldWork, FlowResult> =
    Jobs::new("stone-flow-worker", 64, MAX_FLOW_BYTES, run_field);

#[derive(Debug, PartialEq)]
struct FlowResult {
    distances: Vec<f32>,
    truncated: bool,
}

struct FieldWork {
    width: usize,
    height: usize,
    snapshot: Vec<u8>,
    kernel: FieldKernel,
}

enum FieldKernel {
    Flow {
        goal_x: i64,
        goal_y: i64,
        cap: usize,
    },
    Terrain {
        sub: usize,
    },
}

fn run_field(work: FieldWork, cancelled: &AtomicBool) -> Option<FlowResult> {
    match work.kernel {
        FieldKernel::Flow {
            goal_x,
            goal_y,
            cap,
        } => compute_flow_with_cancel(
            work.width,
            work.height,
            goal_x,
            goal_y,
            cap,
            &work.snapshot,
            cancelled,
        ),
        FieldKernel::Terrain { sub } => {
            terrain_field::compute(work.width, work.height, sub, &work.snapshot, cancelled).map(
                |distances| FlowResult {
                    distances,
                    truncated: false,
                },
            )
        }
    }
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
    compute_flow_with_cancel(
        width,
        height,
        goal_x,
        goal_y,
        cap,
        walkable,
        &AtomicBool::new(false),
    )
    .expect("non-cancellable flow computation was cancelled")
}

// One byte per padded cell: bit 0 walkable; bits 1-4 the corners a blocked cell's wall
// leaves open (corner i is bit i + 1; 0 = (0,0), 1 = (1,0), 2 = (1,1), 3 = (0,1)).
const CELL_WALKABLE: u8 = 1;

fn elbow_clear(cell: u8, corner: u32) -> bool {
    cell & CELL_WALKABLE != 0 || cell & (2 << corner) != 0
}

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
    cancelled: &AtomicBool,
) -> Option<FlowResult> {
    let output_cells = width * height;
    let padded_width = width + 2;
    let mut distances = vec![FLOW_UNREACHABLE; output_cells];
    let unreached = |distances| {
        Some(FlowResult {
            distances,
            truncated: false,
        })
    };
    if goal_x < 0 || goal_y < 0 || goal_x >= width as i64 || goal_y >= height as i64 {
        return unreached(distances);
    }
    let goal_x = goal_x as usize;
    let goal_y = goal_y as usize;
    let goal_index = goal_y * width + goal_x;
    let goal_padded_index = (goal_y + 1) * padded_width + goal_x + 1;
    if walkable[goal_padded_index] & CELL_WALKABLE == 0 {
        return unreached(distances);
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
        if expanded & 63 == 0 && cancelled.load(Ordering::Acquire) {
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
    if width <= 0 || height <= 0 || cap < 0 {
        crate::log_once!("FLOW", "stone_flow_submit: invalid request shape");
        return 0;
    }
    let (width, height) = (width as usize, height as usize);
    let output_cells = width * height;
    let padded_cells = (width + 2) * (height + 2);
    if output_cells > MAX_FLOW_CELLS || i64::try_from(padded_cells) != Ok(walkable_len) {
        crate::log_once!("FLOW", "stone_flow_submit: invalid request shape");
        return 0;
    }
    if walkable.is_null() {
        crate::log_once!("FLOW", "stone_flow_submit: null walkability snapshot");
        return 0;
    }
    let reserved_bytes = padded_cells + output_cells * (size_of::<f32>() + size_of::<usize>());
    if reserved_bytes > MAX_FLOW_BYTES {
        crate::log_once!("FLOW", "stone_flow_submit: request exceeds bounded storage");
        return 0;
    }
    let kernel = FieldKernel::Flow {
        goal_x: goal_x.into(),
        goal_y: goal_y.into(),
        cap: cap as usize,
    };
    submit_field(
        width,
        height,
        walkable,
        padded_cells,
        reserved_bytes,
        kernel,
    )
}

#[no_mangle]
pub extern "C" fn stone_terrain_submit(
    width: i32,
    height: i32,
    sub: i32,
    solid: *const u8,
    solid_len: i64,
) -> u64 {
    if width <= 0 || height <= 0 || sub <= 0 {
        crate::log_once!("FIELD", "stone_terrain_submit: invalid request shape");
        return 0;
    }
    let (width, height, sub) = (width as usize, height as usize, sub as usize);
    let cells = width * height;
    let bytes = cells
        .checked_mul(sub)
        .and_then(|n| n.checked_mul(sub))
        .filter(|&samples| samples <= MAX_FLOW_CELLS)
        .map(|samples| cells + samples * size_of::<f32>());
    if i64::try_from(cells) != Ok(solid_len) || bytes.is_none_or(|bytes| bytes > MAX_FLOW_BYTES) {
        crate::log_once!("FIELD", "stone_terrain_submit: invalid request shape");
        return 0;
    }
    if solid.is_null() {
        crate::log_once!("FIELD", "stone_terrain_submit: null solid snapshot");
        return 0;
    }
    let kernel = FieldKernel::Terrain { sub };
    submit_field(width, height, solid, cells, bytes.unwrap(), kernel)
}

fn submit_field(
    width: usize,
    height: usize,
    snapshot: *const u8,
    snapshot_len: usize,
    reserved_bytes: usize,
    kernel: FieldKernel,
) -> u64 {
    let work = FieldWork {
        width,
        height,
        snapshot: unsafe { std::slice::from_raw_parts(snapshot, snapshot_len) }.to_vec(),
        kernel,
    };
    match FIELD_JOBS.submit(work, reserved_bytes) {
        Ok(handle) => handle,
        Err(Refused::Busy) => {
            crate::log_once!("FIELD", "field submit: bounded worker queue is full");
            0
        }
        Err(Refused::Handles) => {
            crate::log_once!("FIELD", "field submit: handle space exhausted");
            0
        }
        Err(Refused::Worker(error)) => {
            crate::log_once!("FLOW", "failed to start flow worker: {}", error);
            crate::log_once!("FIELD", "field submit: worker unavailable");
            0
        }
    }
}

#[no_mangle]
pub extern "C" fn stone_flow_poll(handle: u64) -> i32 {
    FIELD_JOBS.poll(handle)
}

#[no_mangle]
pub extern "C" fn stone_flow_wait(handle: u64) -> i32 {
    FIELD_JOBS.wait(handle)
}

#[no_mangle]
pub extern "C" fn stone_flow_copy(handle: u64, output: *mut f32, output_len: i64) -> i32 {
    if output_len < 0 {
        return -1;
    }
    let fits = |result: &FlowResult| {
        let required = result.distances.len();
        let fits = !output.is_null() && output_len as usize >= required;
        if !fits {
            crate::log_once!(
                "FLOW",
                "stone_flow_copy: output buffer is too small (need {}, got {})",
                required,
                output_len
            );
        }
        fits
    };
    match FIELD_JOBS.take(handle, fits) {
        Taken::Missing | Taken::Rejected => -1,
        Taken::Pending => -2,
        Taken::Ready(result) => {
            // The output pointer and its length are part of the C ABI contract.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    result.distances.as_ptr(),
                    output,
                    result.distances.len(),
                );
            }
            i32::from(result.truncated)
        }
    }
}

#[no_mangle]
pub extern "C" fn stone_flow_cancel(handle: u64) {
    FIELD_JOBS.cancel(handle);
}

pub(crate) fn shutdown() {
    FIELD_JOBS.shutdown();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static FLOW_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn padded_snapshot(width: usize, height: usize, open: bool) -> Vec<u8> {
        let value = if open { 1 } else { 0 };
        vec![value; (width + 2) * (height + 2)]
    }

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
    fn diagonal_needs_both_elbows_open_or_their_facing_corners_open() {
        let width = 2;
        let height = 2;
        let padded_width = width + 2;
        let east = padded_width + 2;
        let south = 2 * padded_width + 1;
        let mut cells = padded_snapshot(width, height, true);
        cells[east] = 0;
        let result = compute_flow(width, height, 0, 0, 64, &cells);
        assert_eq!(result.distances[1 + width], 2.0);

        cells[east] = open_corner(3);
        cells[south] = open_corner(1);
        let result = compute_flow(width, height, 0, 0, 64, &cells);
        assert_eq!(result.distances[1 + width], 1.0);
        assert_eq!(
            result.distances[1], FLOW_UNREACHABLE,
            "a wall with open corners stays a wall"
        );
        assert_eq!(result.distances[width], FLOW_UNREACHABLE);

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
        let walkable = interior_snapshot(3, 1);
        let result = compute_flow(3, 1, 0, 0, 1, &walkable);

        assert!(result.truncated);
        assert_eq!(result.distances, vec![0.0, 1.0, FLOW_UNREACHABLE]);
    }

    #[test]
    fn blocked_goal_is_unreachable_without_truncation() {
        let width = 3;
        let height = 3;
        let mut walkable = padded_snapshot(width, height, true);
        walkable[1 + (width + 2)] = 0;
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
    fn open_grid_at_the_cap_matches_chebyshev_distances() {
        let size = 129;
        let center = size / 2;
        let walkable = interior_snapshot(size, size);
        let result = compute_flow(size, size, center as i64, center as i64, 64, &walkable);

        assert!(!result.truncated);
        for y in 0..size {
            for x in 0..size {
                let expected = x.abs_diff(center).max(y.abs_diff(center)) as f32;
                assert_eq!(result.distances[y * size + x], expected);
            }
        }
    }

    fn submit(size: (usize, usize), goal: (i32, i32), walkable: &[u8]) -> u64 {
        let handle = stone_flow_submit(
            size.0 as i32,
            size.1 as i32,
            goal.0,
            goal.1,
            64,
            walkable.as_ptr(),
            walkable.len() as i64,
        );
        assert_ne!(handle, 0);
        handle
    }

    #[test]
    fn ffi_copies_caller_owned_snapshot_and_consumes_result() {
        let _guard = FLOW_TEST_LOCK.lock().unwrap();
        shutdown();
        let width = 3;
        let height = 1;
        let mut walkable = interior_snapshot(width, height);
        let handle = submit((width, height), (0, 0), &walkable);
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
        shutdown();
    }

    #[test]
    fn ffi_cancel_makes_pending_handle_missing_immediately() {
        let _guard = FLOW_TEST_LOCK.lock().unwrap();
        shutdown();
        let walkable = interior_snapshot(129, 129);
        let handle = submit((129, 129), (64, 64), &walkable);
        stone_flow_cancel(handle);
        assert_eq!(stone_flow_poll(handle), -1);
        assert_eq!(stone_flow_wait(handle), -1);
        shutdown();
    }
}
