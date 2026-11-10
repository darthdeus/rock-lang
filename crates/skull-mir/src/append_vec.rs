//! A push-only vector whose elements never move, so any thread can read an
//! element by index without a lock while another thread appends.
//!
//! Build-scoped tables that lowering workers read on every expression (the
//! type arena, the representation arena) are monotonic: entries are appended
//! and never changed or removed. Storing them in doubling segments that are
//! never reallocated lets a reader hand out `&T` tied to the table itself,
//! instead of copying entries into per-thread caches or taking a read lock.

use std::alloc::{Layout, alloc, dealloc, handle_alloc_error};
use std::sync::Mutex;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

/// Slots in the first segment; segment `k` holds `FIRST << k` slots.
const FIRST: usize = 64;
const SEGMENTS: usize = 40;

pub struct AppendVec<T> {
    segments: [AtomicPtr<T>; SEGMENTS],
    len: AtomicUsize,
    push_lock: Mutex<()>,
}

// Elements are only written before `len` publishes them and are never mutated
// afterwards, so sharing the vector shares `&T` across threads.
unsafe impl<T: Send> Send for AppendVec<T> {}
unsafe impl<T: Send + Sync> Sync for AppendVec<T> {}

impl<T> Default for AppendVec<T> {
    fn default() -> Self {
        Self::new()
    }
}

fn locate(index: usize) -> (usize, usize) {
    let block = index / FIRST + 1;
    let segment = (usize::BITS - 1 - block.leading_zeros()) as usize;
    (segment, index - FIRST * ((1 << segment) - 1))
}

fn segment_layout<T>(segment: usize) -> Layout {
    Layout::array::<T>(FIRST << segment).expect("append vector segment overflows")
}

impl<T> AppendVec<T> {
    pub fn new() -> Self {
        assert!(
            std::mem::size_of::<T>() != 0,
            "AppendVec does not hold ZSTs"
        );
        Self {
            segments: std::array::from_fn(|_| AtomicPtr::new(std::ptr::null_mut())),
            len: AtomicUsize::new(0),
            push_lock: Mutex::new(()),
        }
    }

    pub fn len(&self) -> usize {
        self.len.load(Ordering::Acquire)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Append `value` and return its index.
    pub fn push(&self, value: T) -> usize {
        let _guard = self.push_lock.lock().unwrap_or_else(|e| e.into_inner());
        let index = self.len.load(Ordering::Relaxed);
        let (segment, offset) = locate(index);
        let mut base = self.segments[segment].load(Ordering::Relaxed);
        if base.is_null() {
            let layout = segment_layout::<T>(segment);
            // SAFETY: the layout is non-zero sized (T is not a ZST, FIRST > 0).
            base = unsafe { alloc(layout) }.cast::<T>();
            if base.is_null() {
                handle_alloc_error(layout);
            }
            self.segments[segment].store(base, Ordering::Release);
        }
        // SAFETY: `offset` is inside the segment and the slot is unpublished,
        // so no reader can observe it until `len` is released below.
        unsafe { base.add(offset).write(value) };
        self.len.store(index + 1, Ordering::Release);
        index
    }

    pub fn get(&self, index: usize) -> Option<&T> {
        if index >= self.len() {
            return None;
        }
        let (segment, offset) = locate(index);
        let base = self.segments[segment].load(Ordering::Acquire);
        // SAFETY: the Acquire load of `len` observed this slot's publication,
        // which happened after its segment was allocated and the slot written.
        Some(unsafe { &*base.add(offset) })
    }

    pub fn iter(&self) -> impl Iterator<Item = &T> + '_ {
        (0..self.len()).map(|index| self.get(index).expect("index below len"))
    }
}

impl<T> std::ops::Index<usize> for AppendVec<T> {
    type Output = T;

    fn index(&self, index: usize) -> &T {
        self.get(index).expect("AppendVec index out of bounds")
    }
}

impl<T> Drop for AppendVec<T> {
    fn drop(&mut self) {
        let len = *self.len.get_mut();
        for index in 0..len {
            let (segment, offset) = locate(index);
            // SAFETY: every index below `len` was written exactly once.
            unsafe { std::ptr::drop_in_place(self.segments[segment].get_mut().add(offset)) };
        }
        for (segment, base) in self.segments.iter_mut().enumerate() {
            let base = *base.get_mut();
            if !base.is_null() {
                // SAFETY: allocated in `push` with this same layout.
                unsafe { dealloc(base.cast(), segment_layout::<T>(segment)) };
            }
        }
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for AppendVec<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indices_cross_segment_boundaries() {
        let v = AppendVec::new();
        for i in 0..5000usize {
            assert_eq!(v.push(i.to_string()), i);
        }
        assert_eq!(v.len(), 5000);
        for i in [0, 63, 64, 191, 192, 4999] {
            assert_eq!(v[i], i.to_string());
        }
        assert!(v.get(5000).is_none());
    }

    #[test]
    fn readers_see_published_elements_while_others_push() {
        let v = std::sync::Arc::new(AppendVec::new());
        std::thread::scope(|s| {
            for t in 0..4 {
                let v = &v;
                s.spawn(move || {
                    for i in 0..2000usize {
                        let at = v.push(vec![t, i]);
                        assert_eq!(v[at], vec![t, i]);
                    }
                });
            }
        });
        assert_eq!(v.len(), 8000);
    }
}
