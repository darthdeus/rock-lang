use crate::ffi_array::{slice, slice_mut, Span};

const LANES_PER_RECORD: usize = 5;

/// `records` has five i32 lanes per slot: `[active, x, y, generation, installed]`.
/// `output` receives `[matched_count, examined_count, matching_slot...]`, slots ascending.
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
    let (Ok(record_lanes), Ok(candidate_count), Ok(output_capacity)) = (
        usize::try_from(len),
        usize::try_from(candidate_len),
        usize::try_from(output_len),
    ) else {
        return false;
    };
    let slot_count = record_lanes / LANES_PER_RECORD;
    if !record_lanes.is_multiple_of(LANES_PER_RECORD)
        || slot_count > i32::MAX as usize
        || candidate_count > slot_count
        || output_capacity < candidate_count + 2
    {
        return false;
    }

    // Read-only inputs may alias each other; the full output capacity must be disjoint from both.
    let (Some(records_span), Some(candidates_span), Some(output_span)) = (
        Span::of(records, record_lanes),
        Span::of(candidates, candidate_count),
        Span::of(output.cast_const(), output_capacity),
    ) else {
        return false;
    };
    if records_span.overlaps(output_span) || candidates_span.overlaps(output_span) {
        return false;
    }
    let (records, candidates) = unsafe {
        (
            slice(records, record_lanes),
            slice(candidates, candidate_count),
        )
    };
    // Malformed candidates leave output untouched.
    if candidates
        .iter()
        .any(|&slot| slot < 0 || slot as usize >= slot_count)
    {
        return false;
    }

    let result = unsafe { slice_mut(output, candidate_count + 2) };
    let mut matched_count = 0usize;
    let mut examined_count = 0usize;
    for &slot in candidates {
        let record = &records[slot as usize * LANES_PER_RECORD..][..LANES_PER_RECORD];
        if record[0] == 0 || (!include_uninstalled && record[4] == 0) {
            continue;
        }
        examined_count += 1;
        let (x, y) = (record[1], record[2]);
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

    fn query(
        records: &[i32],
        candidates: &[i32],
        rect: (i32, i32, i32, i32),
        include_uninstalled: bool,
        output: &mut [i32],
    ) -> bool {
        stone_actor_query_rect(
            records.as_ptr(),
            records.len() as i64,
            candidates.as_ptr(),
            candidates.len() as i64,
            rect.0,
            rect.1,
            rect.2,
            rect.3,
            include_uninstalled,
            output.as_mut_ptr(),
            output.len() as i64,
        )
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

        assert!(query(
            &records,
            &candidates,
            (0, 0, 5, 5),
            false,
            &mut output
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

        assert!(query(
            &records,
            &candidates,
            (0, 0, 3, 3),
            false,
            &mut output
        ));
        assert_eq!(&output[..3], &[1, 2, 1]);

        assert!(query(
            &records,
            &candidates,
            (0, 0, 3, 3),
            true,
            &mut output
        ));
        assert_eq!(&output[..4], &[2, 3, 0, 1]);
    }

    #[test]
    fn reversed_bounds_still_count_active_candidates() {
        let records = [record(true, 1, 1, 1, true), record(true, 2, 2, 2, true)].concat();
        let candidates = [1, 0];
        let mut output = [-1; 4];

        assert!(query(
            &records,
            &candidates,
            (3, 0, 1, 3),
            false,
            &mut output
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
