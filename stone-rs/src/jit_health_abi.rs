//! Caller-owned LuaJIT health ABI v1, mirrored from gamelibs/luajit-health.h.
use std::ffi::{c_int, c_void};

pub const VERSION: u32 = 1;
pub const COUNTERS: u64 = 1;
pub const COALESCING: u64 = 2;
pub const VMCATEGORY: u64 = 4;
pub const COALESCING_RING: usize = 64;
pub const COALESCING_SLOTS: usize = 64;
pub type Read = unsafe extern "C" fn(*mut c_void, u32, *mut c_void, usize) -> c_int;
pub type Category = unsafe extern "C" fn(*mut c_void) -> c_int;
pub type Configure = unsafe extern "C" fn(*mut c_void, u32, u32) -> c_int;
pub type Coalescing = unsafe extern "C" fn(*mut c_void, u32, u64, *mut c_void, usize, u32) -> c_int;

pub fn subreason_name(subreason: u32) -> Option<&'static str> {
    match subreason {
        1 => Some("no reload register"),
        2 => Some("no GPR cycle temp"),
        3 => Some("no FPR cycle temp"),
        4 => Some("inherited slot limit"),
        _ => None,
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct HealthSlot {
    pub slot: u32,
    pub parent_ref: u32,
    pub parent_regsp: u32,
    pub child_ref: u32,
    pub child_type: u32,
    pub child_reg: u32,
    pub child_spill: u32,
    pub flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CoalescingEntry {
    pub sequence: u64,
    pub occurrences: u64,
    pub generation: u64,
    pub parent_uid: u64,
    pub start_proto: u64,
    pub failure_proto: u64,
    pub allow_gpr: u64,
    pub allow_fpr: u64,
    pub free_gpr: u64,
    pub free_fpr: u64,
    pub live_gpr: u64,
    pub live_fpr: u64,
    pub parent: u32,
    pub exitno: u32,
    pub subreason: u32,
    pub start_pc: u32,
    pub failure_pc: u32,
    pub framedepth: i32,
    pub baseslot: u32,
    pub maxslot: u32,
    pub snapshot: u32,
    pub snapshot_entries: u32,
    pub snapshot_slots: u32,
    pub inherited: u32,
    pub failed_ref: u32,
    pub failed_slot: u32,
    pub failed_type: u32,
    pub parent_regsp: u32,
    pub target_reg: u32,
    pub target_spill: u32,
    pub slot_count: u32,
    pub slots_truncated: u32,
    pub slots: [HealthSlot; COALESCING_SLOTS],
}

impl Default for CoalescingEntry {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Snapshot {
    pub version: u32,
    pub size: u32,
    pub capabilities: u64,
    pub build_id: [u8; 64],
    pub root_starts: u64,
    pub side_starts: u64,
    pub root_compiles: u64,
    pub side_compiles: u64,
    pub aborts: [u64; 64],
    pub flushes: u64,
    pub generation: u64,
    pub emitted_mcode_bytes: u64,
    pub live_traces: u64,
    pub live_mcode_bytes: u64,
    pub arena_bytes: u64,
    pub arena_occupied_bytes: u64,
    pub code_reserved_bytes: u64,
    pub code_current_free_bytes: u64,
    pub ring_next: u64,
    pub ring_overflow: u64,
}

impl Default for Snapshot {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

/// Per-frame native JIT delta layout, in order: root/side starts, root/side
/// compiles, total aborts, three (reason, count) movers by (delta desc, index
/// asc) padded with (-1, 0), flushes, emitted mcode bytes, live traces,
/// arena occupied bytes, code free bytes, coalescing next/overflow.
/// Monotonic counters saturate: a VM rebind resets them, and a wrap-sized
/// spike would misattribute a whole frame. Signed population/byte fields keep
/// their sign. Reason indices are pinned `jit.vmdef.traceerr` positions; only
/// the top three movers are kept, so a tail beyond them is truncated (the
/// total still accounts for it).
pub const FRAME_DELTA_COLUMNS: usize = 18;

pub fn frame_deltas(prev: &Snapshot, cur: &Snapshot) -> [f64; FRAME_DELTA_COLUMNS] {
    let mut movers: [(u64, usize); 64] = [(0, 0); 64];
    let mut total: u64 = 0;
    for (i, slot) in movers.iter_mut().enumerate() {
        let delta = cur.aborts[i].saturating_sub(prev.aborts[i]);
        *slot = (delta, i);
        total = total.saturating_add(delta);
    }
    movers.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let signed = |a: u64, b: u64| (a as i64).saturating_sub(b as i64) as f64;
    let mut out = [0.0; FRAME_DELTA_COLUMNS];
    out[0] = cur.root_starts.saturating_sub(prev.root_starts) as f64;
    out[1] = cur.side_starts.saturating_sub(prev.side_starts) as f64;
    out[2] = cur.root_compiles.saturating_sub(prev.root_compiles) as f64;
    out[3] = cur.side_compiles.saturating_sub(prev.side_compiles) as f64;
    out[4] = total as f64;
    for (k, (delta, index)) in movers.iter().take(3).enumerate() {
        out[5 + k * 2] = if *delta > 0 { *index as f64 } else { -1.0 };
        out[6 + k * 2] = *delta as f64;
    }
    out[11] = cur.flushes.saturating_sub(prev.flushes) as f64;
    out[12] = cur
        .emitted_mcode_bytes
        .saturating_sub(prev.emitted_mcode_bytes) as f64;
    out[13] = signed(cur.live_traces, prev.live_traces);
    out[14] = signed(cur.arena_occupied_bytes, prev.arena_occupied_bytes);
    out[15] = signed(cur.code_current_free_bytes, prev.code_current_free_bytes);
    out[16] = cur.ring_next.saturating_sub(prev.ring_next) as f64;
    out[17] = cur.ring_overflow.saturating_sub(prev.ring_overflow) as f64;
    out
}

pub unsafe fn read_snapshot(reader: Option<Read>, state: *mut c_void) -> Option<Snapshot> {
    if state.is_null() {
        return None;
    }
    let reader = reader?;
    let mut out = Snapshot::default();
    let size = std::mem::size_of::<Snapshot>();
    if reader(state, VERSION, (&mut out as *mut Snapshot).cast(), size) != 1
        || out.version != VERSION
        || out.size as usize != size
        || out.capabilities & COUNTERS == 0
    {
        return None;
    }
    Some(out)
}

/// Copy the bounded coalescing ring into Rust-owned memory. The native API
/// returns a row count, or a negative value for an unavailable/incompatible
/// reader. Its stride must exactly match the native structure size.
pub unsafe fn read_coalescing(
    reader: Option<Coalescing>,
    state: *mut c_void,
    first: u64,
) -> Option<Vec<CoalescingEntry>> {
    let reader = reader?;
    if state.is_null() {
        return None;
    }
    let mut out = [CoalescingEntry::default(); COALESCING_RING];
    let count = reader(
        state,
        VERSION,
        first,
        out.as_mut_ptr().cast(),
        std::mem::size_of::<CoalescingEntry>(),
        out.len() as u32,
    );
    if count < 0 || count as usize > out.len() {
        return None;
    }
    Some(out[..count as usize].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    unsafe extern "C" fn present(
        _: *mut c_void,
        version: u32,
        out: *mut c_void,
        size: usize,
    ) -> c_int {
        assert_eq!(version, VERSION);
        assert_eq!(size, std::mem::size_of::<Snapshot>());
        let s = &mut *out.cast::<Snapshot>();
        s.version = VERSION;
        s.size = size as u32;
        s.capabilities = COUNTERS | VMCATEGORY;
        s.root_starts = 11;
        s.side_compiles = 7;
        s.aborts[34] = 9;
        1
    }
    unsafe extern "C" fn unsupported(l: *mut c_void, v: u32, o: *mut c_void, n: usize) -> c_int {
        present(l, v, o, n);
        (*o.cast::<Snapshot>()).capabilities = VMCATEGORY;
        1
    }
    unsafe extern "C" fn wrong_version(l: *mut c_void, v: u32, o: *mut c_void, n: usize) -> c_int {
        present(l, v, o, n);
        (*o.cast::<Snapshot>()).version = 2;
        1
    }
    unsafe extern "C" fn wrong_size(l: *mut c_void, v: u32, o: *mut c_void, n: usize) -> c_int {
        present(l, v, o, n);
        (*o.cast::<Snapshot>()).size -= 8;
        1
    }
    unsafe extern "C" fn rejected(_: *mut c_void, _: u32, _: *mut c_void, _: usize) -> c_int {
        0
    }
    fn pair(mut f: impl FnMut(&mut Snapshot)) -> (Snapshot, Snapshot) {
        let prev = Snapshot {
            version: VERSION,
            ..Snapshot::default()
        };
        let mut cur = prev;
        f(&mut cur);
        (prev, cur)
    }
    #[test]
    fn frame_deltas_diff_counters_and_top_abort_reasons() {
        let (prev, mut cur) = pair(|_| {});
        cur.root_starts = 5;
        cur.side_starts = 3;
        cur.root_compiles = 2;
        cur.side_compiles = 1;
        cur.aborts[7] = 4;
        cur.aborts[2] = 9;
        cur.aborts[60] = 9;
        cur.aborts[0] = 1;
        cur.flushes = 2;
        cur.emitted_mcode_bytes = 512;
        cur.live_traces = 6;
        cur.arena_occupied_bytes = 1000;
        cur.code_current_free_bytes = 4000;
        cur.ring_next = 11;
        cur.ring_overflow = 1;
        let d = frame_deltas(&prev, &cur);
        assert_eq!(FRAME_DELTA_COLUMNS, 18);
        assert_eq!(&d[0..5], &[5.0, 3.0, 2.0, 1.0, 23.0]);
        // Top three abort reasons by (delta desc, index asc); ties prefer lower index.
        assert_eq!(&d[5..11], &[2.0, 9.0, 60.0, 9.0, 7.0, 4.0]);
        assert_eq!(&d[11..18], &[2.0, 512.0, 6.0, 1000.0, 4000.0, 11.0, 1.0]);
        let total: f64 = [4.0, 9.0, 9.0, 1.0].iter().sum();
        assert_eq!(d[4], total);
    }
    #[test]
    fn frame_deltas_pad_missing_reasons_and_keep_sign() {
        let (prev, mut cur) = pair(|_| {});
        cur.aborts[41] = 2;
        cur.live_traces = 3;
        let mut prev2 = cur;
        let mut cur2 = cur;
        cur2.aborts[41] = 5;
        cur2.live_traces = 1;
        cur2.arena_occupied_bytes = 100;
        prev2.arena_occupied_bytes = 250;
        prev2.code_current_free_bytes = 900;
        cur2.code_current_free_bytes = 300;
        let _ = prev;
        let d = frame_deltas(&prev2, &cur2);
        assert_eq!(&d[4..11], &[3.0, 41.0, 3.0, -1.0, 0.0, -1.0, 0.0]);
        assert_eq!(&d[13..16], &[-2.0, -150.0, -600.0]);
    }
    #[test]
    fn frame_deltas_saturate_reset_counters() {
        let (mut prev, mut cur) = pair(|_| {});
        prev.root_starts = 40;
        cur.root_starts = 3;
        prev.aborts[9] = 50;
        let d = frame_deltas(&prev, &cur);
        assert_eq!(d[0], 0.0);
        assert_eq!(d[4], 0.0);
        assert_eq!(&d[5..11], &[-1.0, 0.0, -1.0, 0.0, -1.0, 0.0]);
    }
    #[test]
    fn capability_absent_reports_unavailable() {
        unsafe {
            let state = std::ptr::dangling_mut::<u8>().cast();
            assert!(read_snapshot(None, state).is_none());
            assert!(read_snapshot(Some(unsupported), state).is_none());
            assert!(read_snapshot(Some(rejected), state).is_none());
            assert!(read_snapshot(Some(present), std::ptr::null_mut()).is_none());
        }
    }
    #[test]
    fn capability_present_reads_caller_owned_counters() {
        unsafe {
            let s = read_snapshot(Some(present), std::ptr::dangling_mut::<u8>().cast()).unwrap();
            assert_eq!((s.root_starts, s.side_compiles, s.aborts[34]), (11, 7, 9));
        }
    }
    #[test]
    fn incompatible_schema_reports_unavailable() {
        unsafe {
            let state = std::ptr::dangling_mut::<u8>().cast();
            assert!(read_snapshot(Some(wrong_version), state).is_none());
            assert!(read_snapshot(Some(wrong_size), state).is_none());
        }
    }

    unsafe extern "C" fn coalescing_present(
        state: *mut c_void,
        version: u32,
        first: u64,
        out: *mut c_void,
        stride: usize,
        capacity: u32,
    ) -> c_int {
        assert!(!state.is_null());
        assert_eq!(version, VERSION);
        assert_eq!(first, 0);
        assert_eq!(stride, std::mem::size_of::<CoalescingEntry>());
        assert_eq!(capacity as usize, COALESCING_RING);
        let rows = std::slice::from_raw_parts_mut(out.cast::<CoalescingEntry>(), capacity as usize);
        rows[0] = CoalescingEntry {
            sequence: 4,
            occurrences: 11,
            generation: 2,
            parent_uid: 0x1234,
            start_proto: 0x5678,
            failure_proto: 0x9abc,
            allow_gpr: 1,
            allow_fpr: 2,
            free_gpr: 3,
            free_fpr: 4,
            live_gpr: 5,
            live_fpr: 6,
            parent: 7,
            exitno: 8,
            subreason: 9,
            start_pc: 10,
            failure_pc: 11,
            framedepth: -12,
            baseslot: 13,
            maxslot: 14,
            snapshot: 15,
            snapshot_entries: 16,
            snapshot_slots: 17,
            inherited: 18,
            failed_ref: 19,
            failed_slot: 20,
            failed_type: 21,
            parent_regsp: 22,
            target_reg: 23,
            target_spill: 24,
            slot_count: 1,
            slots_truncated: 0,
            slots: [HealthSlot::default(); COALESCING_SLOTS],
        };
        rows[0].slots[0] = HealthSlot {
            slot: 25,
            parent_ref: 26,
            parent_regsp: 27,
            child_ref: 28,
            child_type: 29,
            child_reg: 30,
            child_spill: 31,
            flags: 32,
        };
        rows[1] = CoalescingEntry {
            sequence: 5,
            occurrences: 1,
            ..CoalescingEntry::default()
        };
        2
    }

    unsafe extern "C" fn coalescing_invalid_count(
        _: *mut c_void,
        _: u32,
        _: u64,
        _: *mut c_void,
        _: usize,
        _: u32,
    ) -> c_int {
        (COALESCING_RING + 1) as c_int
    }

    #[test]
    fn coalescing_reader_mirrors_context_layout_and_copies_rows() {
        unsafe {
            let state = std::ptr::dangling_mut::<u8>().cast();
            assert_eq!(std::mem::size_of::<HealthSlot>(), 32);
            assert_eq!(std::mem::size_of::<CoalescingEntry>(), 0x8b0);
            let rows = read_coalescing(Some(coalescing_present), state, 0).unwrap();
            assert_eq!(rows.len(), 2);
            assert_eq!(
                (rows[0].sequence, rows[0].occurrences, rows[0].subreason),
                (4, 11, 9)
            );
            assert_eq!(
                (rows[0].start_proto, rows[0].failure_proto),
                (0x5678, 0x9abc)
            );
            assert_eq!(
                (rows[0].start_pc, rows[0].failure_pc, rows[0].framedepth),
                (10, 11, -12)
            );
            assert_eq!(
                (
                    rows[0].slot_count,
                    rows[0].slots[0].slot,
                    rows[0].slots[0].flags
                ),
                (1, 25, 32)
            );
            assert_eq!((rows[1].sequence, rows[1].occurrences), (5, 1));
        }
    }

    #[test]
    fn coalescing_reader_rejects_missing_or_invalid_results() {
        unsafe {
            let state = std::ptr::dangling_mut::<u8>().cast();
            assert!(read_coalescing(None, state, 0).is_none());
            assert!(read_coalescing(Some(coalescing_present), std::ptr::null_mut(), 0).is_none());
            assert!(read_coalescing(Some(coalescing_invalid_count), state, 0).is_none());
        }
    }

    #[test]
    fn coalescing_subreason_names_match_native_header() {
        assert_eq!(subreason_name(1), Some("no reload register"));
        assert_eq!(subreason_name(2), Some("no GPR cycle temp"));
        assert_eq!(subreason_name(3), Some("no FPR cycle temp"));
        assert_eq!(subreason_name(4), Some("inherited slot limit"));
        assert_eq!(subreason_name(5), None);
    }
}
