//! Caller-owned arrays passed across the C ABI: validated as address spans so
//! outputs can be proven disjoint from inputs before any slice exists.

use std::mem::{align_of, size_of};

#[derive(Clone, Copy)]
pub(crate) struct Span {
    start: usize,
    end: usize,
}

impl Span {
    /// None for a null or misaligned non-empty array or one too long to slice.
    pub(crate) fn of<T>(pointer: *const T, len: usize) -> Option<Span> {
        if len == 0 {
            return Some(Span { start: 0, end: 0 });
        }
        if pointer.is_null() || !(pointer as usize).is_multiple_of(align_of::<T>()) {
            return None;
        }
        let bytes = len.checked_mul(size_of::<T>())?;
        if bytes > isize::MAX as usize {
            return None;
        }
        let start = pointer as usize;
        Some(Span {
            start,
            end: start.checked_add(bytes)?,
        })
    }

    pub(crate) fn overlaps(self, other: Span) -> bool {
        self.start < other.end && other.start < self.end
    }
}

/// # Safety
/// `Span::of(pointer, len)` succeeded and the caller guarantees the memory is readable.
pub(crate) unsafe fn slice<'a, T>(pointer: *const T, len: usize) -> &'a [T] {
    if len == 0 {
        &[]
    } else {
        std::slice::from_raw_parts(pointer, len)
    }
}

/// # Safety
/// As `slice`, plus the memory is writable and overlaps no other live slice.
pub(crate) unsafe fn slice_mut<'a, T>(pointer: *mut T, len: usize) -> &'a mut [T] {
    if len == 0 {
        &mut []
    } else {
        std::slice::from_raw_parts_mut(pointer, len)
    }
}
