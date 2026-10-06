//! The process allocator: mimalloc, installed as `#[global_allocator]` in
//! `main.rs`.

// Links mimalloc's native library into every binary built from this crate,
// unit tests included, so `mi_collect` below always resolves.
extern crate mimalloc as _;

unsafe extern "C" {
    // mimalloc's C API; the `mimalloc` crate does not re-export it.
    fn mi_collect(force: bool);
}

#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};

/// Test witness recording which rayon worker indices ran the collect
/// broadcast. Tests reset it before `release_freed_memory` and read it
/// after; extra bits from concurrent releasers are harmless.
#[cfg(test)]
static COLLECTED_WORKERS_MASK: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
fn note_worker_collected() {
    if let Some(i) = rayon::current_thread_index() {
        if i < 64 {
            COLLECTED_WORKERS_MASK.fetch_or(1u64 << i, Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
fn take_collected_workers_mask() -> u64 {
    COLLECTED_WORKERS_MASK.swap(0, Ordering::SeqCst)
}

/// Return memory freed so far to the OS now instead of on mimalloc's lazy
/// purge, which needs later allocator traffic to run. For a host that frees a
/// large compile and then only waits on a child process.
pub fn release_freed_memory() {
    // mimalloc pages belong to the thread that allocated them, and a collect
    // only reclaims the calling thread's heap. The compiler's parallel passes
    // allocate on the rayon workers, so each worker collects its own. A
    // `sequential` build has no pool, so only the caller collects there.
    // SAFETY: mi_collect takes no pointers and is safe to call at any time.
    #[cfg(not(feature = "sequential"))]
    rayon::broadcast(|_| {
        #[cfg(test)]
        note_worker_collected();
        // SAFETY: as above.
        unsafe { mi_collect(true) }
    });
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
    /// A `sequential` build has no pool-owned allocations to collect.
    #[cfg(not(feature = "sequential"))]
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
        super::take_collected_workers_mask();
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
        // Independent of the aggregate above: the collect dispatch reached
        // every worker index, so no worker's heap is silently skipped.
        assert!(threads <= 64, "worker mask holds 64 indices, got {threads}");
        let mask = super::take_collected_workers_mask();
        for i in 0..threads {
            assert!(
                mask & (1u64 << i) != 0,
                "worker {i} of {threads} never ran the release collect",
            );
        }
    }

    /// A `sequential` build never constructs a rayon pool (see main.rs), so
    /// releasing freed memory must not create one either. The child below
    /// runs in a fresh process: a test-runner process may already own a
    /// pool, which would hide the initialization this guards against.
    #[cfg(feature = "sequential")]
    #[test]
    fn sequential_release_does_not_create_rayon_pool() {
        let exe = std::env::current_exe().expect("test binary path");
        let out = std::process::Command::new(exe)
            .arg("--exact")
            .arg("allocator::tests::sequential_release_child")
            .arg("--nocapture")
            .env("ROCK_ALLOC_SEQ_CHILD", "1")
            .output()
            .expect("spawn fresh test process");
        assert!(
            out.status.success(),
            "release_freed_memory initialized a rayon pool in a fresh sequential process: {}",
            String::from_utf8_lossy(&out.stderr),
        );
    }

    /// Subprocess body for the parent above: a no-op unless spawned as the
    /// fresh process, so ordinary suite runs stay green either way.
    #[cfg(feature = "sequential")]
    #[test]
    fn sequential_release_child() {
        if std::env::var_os("ROCK_ALLOC_SEQ_CHILD").is_none() {
            return;
        }
        crate::allocator::release_freed_memory();
        assert!(
            rayon::ThreadPoolBuilder::new().build_global().is_ok(),
            "release_freed_memory initialized the rayon global pool under `sequential`",
        );
    }
}
