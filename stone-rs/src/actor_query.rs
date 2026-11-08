//! Synchronous native rectangle query over a caller-owned actor snapshot.
//!
//! Records are indexed by arena slot and use five i32 lanes:
//! active, x, y, generation, installed. The caller supplies the sparse slots
//! collected from the rectangle's coarse spatial buckets. Results contain
//! counts followed by matching slots, sorted into ascending arena-slot order.

use std::mem::{align_of, size_of};

const LANES_PER_RECORD: usize = 5;

fn pointer_range<T>(pointer: *const T, len: usize) -> Option<(usize, usize)> {
    if len == 0 {
        let address = pointer as usize;
        return Some((address, address));
    }
    if pointer.is_null() || !(pointer as usize).is_multiple_of(align_of::<T>()) {
        return None;
    }
    let bytes = len.checked_mul(size_of::<T>())?;
    if bytes > isize::MAX as usize {
        return None;
    }
    let start = pointer as usize;
    Some((start, start.checked_add(bytes)?))
}

fn ranges_overlap(a: (usize, usize), b: (usize, usize)) -> bool {
    a.0 < b.1 && b.0 < a.1
}

/// Query active actors from caller-supplied coarse-bucket candidates and write
/// exact matches to `output` in ascending arena-slot order.
///
/// `records` is indexed by slot with five i32 lanes:
/// `[active, x, y, generation, installed]`. `candidates` contains slots
/// collected by the caller from the rectangle's coarse spatial buckets.
/// `output` receives `[matched_count, examined_count, matching_slot...]`.
/// Examination counts active candidates after the optional installed filter,
/// before the exact inclusive rectangle test.
#[no_mangle]
pub extern "C" fn stone_actor_query_rect(
    records: *const i32,
    len: i64,
    candidates: *const i32,
    candidate_len: i64,
    min_x: i32,
    min_y: i32,
    max_x: i32,
    max_y: i32,
    include_uninstalled: bool,
    output: *mut i32,
    output_len: i64,
) -> bool {
    if len < 0 || candidate_len < 0 || output_len < 0 {
        return false;
    }
    if len % LANES_PER_RECORD as i64 != 0 {
        return false;
    }

    let Ok(record_lanes) = usize::try_from(len) else {
        return false;
    };
    let slot_count = record_lanes / LANES_PER_RECORD;
    if slot_count > i32::MAX as usize {
        return false;
    }
    let Ok(candidate_count) = usize::try_from(candidate_len) else {
        return false;
    };
    if candidate_count > slot_count {
        return false;
    }
    let Some(required_output) = candidate_count.checked_add(2) else {
        return false;
    };
    let Ok(output_capacity) = usize::try_from(output_len) else {
        return false;
    };
    if output_capacity < required_output {
        return false;
    }

    // Validate every caller-provided range before constructing slices or
    // writing anything. The read-only inputs may alias each other, while the
    // full output capacity must be disjoint from both inputs.
    let Some(records_range) = pointer_range(records, record_lanes) else {
        return false;
    };
    let Some(candidates_range) = pointer_range(candidates, candidate_count) else {
        return false;
    };
    let Some(output_range) = pointer_range(output as *const i32, output_capacity) else {
        return false;
    };
    if ranges_overlap(records_range, output_range) || ranges_overlap(candidates_range, output_range)
    {
        return false;
    }

    let record_data: &[i32] = if record_lanes == 0 {
        &[]
    } else {
        // SAFETY: range, alignment, and length were validated above; the
        // caller guarantees that this range points to readable data.
        unsafe { std::slice::from_raw_parts(records, record_lanes) }
    };
    let candidate_data: &[i32] = if candidate_count == 0 {
        &[]
    } else {
        // SAFETY: range, alignment, and length were validated above; the
        // caller guarantees that this range points to readable data.
        unsafe { std::slice::from_raw_parts(candidates, candidate_count) }
    };

    // Validate all slot indexes before creating the mutable output slice, so
    // malformed candidates leave output untouched.
    for &slot in candidate_data {
        if slot < 0 || slot as usize >= slot_count {
            return false;
        }
    }

    // SAFETY: range, alignment, capacity, and disjointness from both inputs
    // were validated above; the caller guarantees writable output memory.
    let result = unsafe { std::slice::from_raw_parts_mut(output, required_output) };
    let mut matched_count = 0usize;
    let mut examined_count = 0usize;

    for &slot in candidate_data {
        let slot_index = slot as usize;
        let base = slot_index * LANES_PER_RECORD;
        if record_data[base] == 0 {
            continue;
        }
        let installed = record_data[base + 4] != 0;
        if !include_uninstalled && !installed {
            continue;
        }

        examined_count += 1;
        let x = record_data[base + 1];
        let y = record_data[base + 2];
        if x >= min_x && x <= max_x && y >= min_y && y <= max_y {
            result[2 + matched_count] = slot;
            matched_count += 1;
        }
    }

    result[2..2 + matched_count].sort_unstable();
    result[0] = matched_count as i32;
    result[1] = examined_count as i32;
    true
}

#[cfg(test)]
mod tests {
    use super::stone_actor_query_rect;

    fn record(active: bool, x: i32, y: i32, generation: i32, installed: bool) -> [i32; 5] {
        [active as i32, x, y, generation, installed as i32]
    }

    #[test]
    fn sparse_candidates_skip_holes_filter_exact_edges_and_sort_slots() {
        let records = [
            record(true, 5, 5, 10, true), // Slot 0: inclusive maximum corner.
            record(false, 2, 2, 11, true),
            record(true, 0, 0, 12, true), // Slot 2: inclusive minimum corner.
            record(true, 3, 4, 13, true),
            record(true, 6, 3, 14, true), // Candidate, outside exact rectangle.
            record(true, 4, 4, 15, true),
        ]
        .concat();
        let candidates = [5, 4, 1, 3, 0, 2];
        let mut output = [-1; 8];

        assert!(stone_actor_query_rect(
            records.as_ptr(),
            records.len() as i64,
            candidates.as_ptr(),
            candidates.len() as i64,
            0,
            0,
            5,
            5,
            false,
            output.as_mut_ptr(),
            output.len() as i64,
        ));
        assert_eq!(&output[..6], &[4, 5, 0, 2, 3, 5]);
        assert_eq!(&output[6..], &[-1; 2]);
    }

    #[test]
    fn installed_filter_precedes_examined_count() {
        let records = [
            record(true, 1, 1, 1, false),
            record(true, 2, 2, 2, true),
            record(false, 3, 3, 3, true),
            record(true, 4, 4, 4, true), // Active and installed, outside exact rect.
        ]
        .concat();
        let candidates = [3, 0, 2, 1];
        let mut output = [-1; 6];

        assert!(stone_actor_query_rect(
            records.as_ptr(),
            records.len() as i64,
            candidates.as_ptr(),
            candidates.len() as i64,
            0,
            0,
            3,
            3,
            false,
            output.as_mut_ptr(),
            output.len() as i64,
        ));
        assert_eq!(&output[..3], &[1, 2, 1]);

        assert!(stone_actor_query_rect(
            records.as_ptr(),
            records.len() as i64,
            candidates.as_ptr(),
            candidates.len() as i64,
            0,
            0,
            3,
            3,
            true,
            output.as_mut_ptr(),
            output.len() as i64,
        ));
        assert_eq!(&output[..4], &[2, 3, 0, 1]);
    }

    #[test]
    fn reversed_bounds_still_count_active_candidates() {
        let records = [record(true, 1, 1, 1, true), record(true, 2, 2, 2, true)].concat();
        let candidates = [1, 0];
        let mut output = [-1; 4];

        assert!(stone_actor_query_rect(
            records.as_ptr(),
            records.len() as i64,
            candidates.as_ptr(),
            candidates.len() as i64,
            3,
            0,
            1,
            3,
            false,
            output.as_mut_ptr(),
            output.len() as i64,
        ));
        assert_eq!(&output[..2], &[0, 2]);
    }

    #[test]
    fn zero_records_accept_null_read_only_inputs() {
        let mut output = [-1; 2];
        assert!(stone_actor_query_rect(
            std::ptr::null(),
            0,
            std::ptr::null(),
            0,
            -3,
            -4,
            5,
            6,
            false,
            output.as_mut_ptr(),
            output.len() as i64,
        ));
        assert_eq!(output, [0, 0]);
    }

    #[test]
    fn invalid_inputs_leave_output_untouched() {
        let mut records = [record(true, 0, 0, 1, true)].concat();
        let candidates = [0];
        let mut output = [91; 4];
        let call = |records: *const i32,
                    len: i64,
                    candidates: *const i32,
                    candidate_len: i64,
                    output: *mut i32,
                    output_len: i64| {
            stone_actor_query_rect(
                records,
                len,
                candidates,
                candidate_len,
                0,
                0,
                1,
                1,
                false,
                output,
                output_len,
            )
        };

        assert!(!call(
            records.as_ptr(),
            -5,
            candidates.as_ptr(),
            1,
            output.as_mut_ptr(),
            4
        ));
        assert!(!call(
            records.as_ptr(),
            4,
            candidates.as_ptr(),
            1,
            output.as_mut_ptr(),
            4
        ));
        assert!(!call(
            records.as_ptr(),
            5,
            candidates.as_ptr(),
            -1,
            output.as_mut_ptr(),
            4
        ));
        assert!(!call(
            records.as_ptr(),
            5,
            candidates.as_ptr(),
            2,
            output.as_mut_ptr(),
            4
        ));
        assert!(!call(
            records.as_ptr(),
            5,
            candidates.as_ptr(),
            1,
            output.as_mut_ptr(),
            2
        ));
        assert!(!call(
            std::ptr::null(),
            5,
            candidates.as_ptr(),
            1,
            output.as_mut_ptr(),
            4
        ));
        assert!(!call(
            records.as_ptr(),
            5,
            std::ptr::null(),
            1,
            output.as_mut_ptr(),
            4
        ));
        assert!(!call(
            records.as_ptr(),
            5,
            [1].as_ptr(),
            1,
            output.as_mut_ptr(),
            4
        ));
        assert!(!call(
            records.as_ptr(),
            5,
            [-1].as_ptr(),
            1,
            output.as_mut_ptr(),
            4
        ));
        assert!(!call(
            records.as_ptr(),
            5,
            candidates.as_ptr(),
            1,
            output.as_mut_ptr(),
            i64::MAX,
        ));
        assert!(!call(
            records.as_ptr(),
            5,
            candidates.as_ptr(),
            1,
            std::ptr::null_mut(),
            3,
        ));

        // Output cannot alias either read-only input, even when its written
        // prefix would fit within the input allocation.
        assert!(!call(
            records.as_ptr(),
            5,
            candidates.as_ptr(),
            1,
            records.as_mut_ptr(),
            3,
        ));
        let mut candidate_and_output = [0, 91, 91];
        assert!(!call(
            records.as_ptr(),
            5,
            candidate_and_output.as_ptr(),
            1,
            candidate_and_output.as_mut_ptr(),
            3,
        ));

        let words = [0u32; 4];
        let misaligned = unsafe { words.as_ptr().cast::<u8>().add(1).cast::<i32>() };
        assert!(!call(
            misaligned,
            5,
            candidates.as_ptr(),
            1,
            output.as_mut_ptr(),
            4
        ));
        assert_eq!(output, [91; 4]);
        assert_eq!(records, [1, 0, 0, 1, 1]);
        assert_eq!(candidate_and_output, [0, 91, 91]);
    }
}
