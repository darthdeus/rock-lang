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
    // mimalloc pages belong to the thread that allocated them, and a collect
    // only reclaims the calling thread's heap. The compiler's parallel passes
    // allocate on the rayon workers, so each worker collects its own.
    // SAFETY: mi_collect takes no pointers and is safe to call at any time.
    rayon::broadcast(|_| unsafe { mi_collect(true) });
    // SAFETY: as above.
    unsafe { mi_collect(true) }
}

#[cfg(test)]
mod tests {
    use std::ffi::c_void;

    unsafe extern "C" {
        fn mi_malloc(size: usize) -> *mut c_void;
        fn mi_free(p: *mut c_void);
        fn mi_process_info(
            elapsed_msecs: *mut usize,
            user_msecs: *mut usize,
            system_msecs: *mut usize,
            current_rss: *mut usize,
            peak_rss: *mut usize,
            current_commit: *mut usize,
            peak_commit: *mut usize,
            page_faults: *mut usize,
        );
    }

    fn mimalloc_committed() -> usize {
        let mut v = [0usize; 8];
        let [a, b, c, d, e, f, g, h] = &mut v;
        // SAFETY: every out-pointer is a valid, distinct usize.
        unsafe { mi_process_info(a, b, c, d, e, f, g, h) };
        v[5]
    }

    /// The compiler allocates on rayon workers and the host frees the result
    /// on its own thread before it waits on a child. Those pages belong to the
    /// workers' heaps, so a collect on the calling thread alone keeps them.
    #[test]
    fn release_returns_pages_freed_into_pool_thread_heaps() {
        // The unit-test binary keeps the system global allocator, so this
        // allocates through mimalloc's C API as the `rock` binary does.
        const PER_THREAD: usize = 64 << 20;
        const BLOCK: usize = 1024;
        let threads = rayon::current_num_threads();
        assert!(threads > 1, "needs a multi-thread rayon pool, got {threads}");
        let blocks: Vec<Vec<usize>> = rayon::broadcast(|_| {
            (0..PER_THREAD / BLOCK)
                .map(|_| {
                    // SAFETY: a fresh BLOCK-byte allocation, written in bounds.
                    unsafe {
                        let p = mi_malloc(BLOCK);
                        assert!(!p.is_null());
                        std::ptr::write_bytes(p.cast::<u8>(), 1, BLOCK);
                        p as usize
                    }
                })
                .collect()
        });
        for p in blocks.into_iter().flatten() {
            // SAFETY: each pointer came from mi_malloc above and is freed once.
            unsafe { mi_free(p as *mut c_void) };
        }
        let before = mimalloc_committed();
        super::release_freed_memory();
        let after = mimalloc_committed();
        let allocated = threads * PER_THREAD;
        assert!(
            before.saturating_sub(after) >= allocated / 2,
            "release kept worker-heap pages: committed {} MB before, {} MB after, {} MB freed on {threads} workers",
            before >> 20,
            after >> 20,
            allocated >> 20,
        );
    }
}
