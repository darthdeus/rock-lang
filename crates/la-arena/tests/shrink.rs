//! Shrinking must hand the excess back without relying on the allocator's
//! `realloc`: mimalloc keeps a block in place when it shrinks by less than
//! half, which is every vector that grew by doubling.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use comfy_la_arena::{Arena, ArenaMap};

struct Counting;

static SHRINKING_REALLOCS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if new_size < layout.size() {
            SHRINKING_REALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

#[test]
fn shrinking_moves_into_an_exact_allocation() {
    let mut arena = Arena::new();
    let mut map = ArenaMap::default();
    for i in 0..1000u64 {
        let idx = arena.alloc(i);
        map.insert(idx, i * 2);
    }
    let first = arena.iter().next().map(|(idx, _)| idx).unwrap();
    assert!(arena.capacity() > arena.len());
    let before = SHRINKING_REALLOCS.load(Ordering::Relaxed);
    arena.shrink_to_fit();
    map.shrink_to_fit();
    assert_eq!(SHRINKING_REALLOCS.load(Ordering::Relaxed), before);
    assert_eq!(arena.capacity(), arena.len());
    assert_eq!(arena.len(), 1000);
    assert_eq!(arena[first], 0);
    assert_eq!(map.get(first), Some(&0));
    assert_eq!(map.values().copied().sum::<u64>(), 999 * 1000);
}
