//! The process allocator: mimalloc, installed as `#[global_allocator]` in
//! `main.rs`.

// Links mimalloc's native library into every binary built from this crate,
// unit tests included, so `mi_collect` below always resolves.
extern crate mimalloc as _;

unsafe extern "C" {
    // mimalloc's C API; the `mimalloc` crate does not re-export it.
    fn mi_collect(force: bool);
}

/// Return memory freed so far to the OS now instead of on mimalloc's lazy
/// purge, which needs later allocator traffic to run. For a host that frees a
/// large compile and then only waits on a child process.
pub fn release_freed_memory() {
    // SAFETY: mi_collect takes no pointers and is safe to call at any time.
    unsafe { mi_collect(true) }
}
