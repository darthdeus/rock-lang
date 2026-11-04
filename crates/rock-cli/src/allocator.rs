//! The process allocator: mimalloc, installed as `#[global_allocator]` in
//! `main.rs`.

// Links mimalloc's native library into every binary built from this crate,
// unit tests included, so `mi_collect` below always resolves.
extern crate mimalloc as _;

unsafe extern "C" {
    // mimalloc's C API; the `mimalloc` crate does not re-export it.
    fn mi_collect(force: bool);
    fn mi_option_set_default(option: std::ffi::c_int, value: std::ffi::c_long);
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

pub(crate) fn process_memory() -> serde_json::Value {
    let mut values = [0usize; 8];
    let [a, b, c, d, e, f, g, h] = &mut values;
    // SAFETY: all output pointers are valid, distinct usize values.
    unsafe { mi_process_info(a, b, c, d, e, f, g, h) };
    #[cfg(target_os = "linux")]
    let rss = match linux_current_rss() {
        Ok(rss) => Some(rss),
        Err(error) => {
            log::warn!(target: "COMPILE_STATS", "cannot sample current RSS: {error}");
            None
        }
    };
    #[cfg(not(target_os = "linux"))]
    let rss = Some(values[3] as u64);
    memory_sample(&values, rss)
}

fn memory_sample(values: &[usize; 8], rss: Option<u64>) -> serde_json::Value {
    // mimalloc casts its signed commit counters to size_t. Its approximate
    // arena accounting can drive the current counter below zero after large
    // frees, so a negative value is no byte count: report it unknown.
    let commit = |value: usize| u64::try_from(value as i64).ok();
    serde_json::json!({
        "rss_bytes": rss, "peak_rss_bytes": values[4],
        "commit_bytes": commit(values[5]), "peak_commit_bytes": commit(values[6]),
    })
}

// Bundled mimalloc leaves current_rss equal to allocator commit on Linux;
// its getrusage path only overwrites peak_rss. macOS uses task_info for both.
#[cfg(target_os = "linux")]
fn linux_current_rss() -> std::io::Result<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm")?;
    let pages = statm
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid resident page count",
            )
        })?;
    // SAFETY: sysconf takes no pointers; a non-positive result is rejected.
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page_size <= 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid OS page size",
        ));
    }
    pages.checked_mul(page_size as u64).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "RSS byte count overflow")
    })
}

// `mi_option_purge_delay` in the vendored mimalloc.h `mi_option_e`; the sys
// crate's `extended` constants omit it.
const MI_OPTION_PURGE_DELAY: std::ffi::c_int = 15;

/// Return freed mimalloc pages promptly; explicit environment options win.
/// Active compilation keeps the allocator's reuse delay to avoid purging and
/// faulting the same pages across workers. Hosts can request immediate purging
/// explicitly; `release_freed_memory` reclaims every worker before child waits.
pub fn purge_freed_pages_immediately() {
    // A set default yields to an option mimalloc read from the environment.
    // SAFETY: mi_option_set_default takes no pointers; the option is valid.
    unsafe { mi_option_set_default(MI_OPTION_PURGE_DELAY, 0) }
}

#[cfg(all(test, not(feature = "sequential")))]
use std::sync::atomic::{AtomicU64, Ordering};

/// Test witness recording which rayon worker indices ran the collect
/// broadcast. Tests reset it before `release_freed_memory` and read it
/// after; extra bits from concurrent releasers are harmless.
#[cfg(all(test, not(feature = "sequential")))]
static COLLECTED_WORKERS_MASK: AtomicU64 = AtomicU64::new(0);

#[cfg(all(test, not(feature = "sequential")))]
fn note_worker_collected() {
    if let Some(i) = rayon::current_thread_index() {
        if i < 64 {
            COLLECTED_WORKERS_MASK.fetch_or(1u64 << i, Ordering::SeqCst);
        }
    }
}

#[cfg(all(test, not(feature = "sequential")))]
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
    #[cfg(not(feature = "sequential"))]
    use std::ffi::c_void;

    #[cfg(not(feature = "sequential"))]
    unsafe extern "C" {
        fn mi_malloc(size: usize) -> *mut c_void;
        fn mi_free(p: *mut c_void);
    }

    #[cfg(not(feature = "sequential"))]
    fn mimalloc_committed() -> usize {
        use super::mi_process_info;
        let mut v = [0usize; 8];
        let [a, b, c, d, e, f, g, h] = &mut v;
        // SAFETY: every out-pointer is a valid, distinct usize.
        unsafe { mi_process_info(a, b, c, d, e, f, g, h) };
        v[5]
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn process_memory_reports_os_rss_not_mimalloc_commit() {
        // This test binary uses the system allocator. Its resident allocation
        // must appear in RSS even though mimalloc did not commit its pages.
        let data = vec![1u8; 16 << 20];
        std::hint::black_box(&data);
        let memory = super::process_memory();
        assert!(
            memory["rss_bytes"].as_u64().unwrap() >= data.len() as u64,
            "RSS omitted a resident system allocation: {memory}"
        );
        assert!(memory["peak_rss_bytes"].as_u64().unwrap() > 0);
    }

    #[test]
    fn negative_mimalloc_commit_counter_is_reported_unknown() {
        // Measured in a `rock test project` prepare phase after the workspace drop.
        let mut values = [0usize; 8];
        values[5] = -549_257_216i64 as usize;
        values[6] = 3_875_405_824;
        let memory = super::memory_sample(&values, Some(1_383_763_968));
        assert!(
            memory["commit_bytes"].is_null(),
            "wrapped commit counter: {memory}"
        );
        assert_eq!(memory["peak_commit_bytes"], 3_875_405_824u64);
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
        assert!(
            threads > 1,
            "needs a multi-thread rayon pool, got {threads}"
        );
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

    unsafe extern "C" {
        fn mi_option_get(option: std::ffi::c_int) -> std::ffi::c_long;
    }

    // Fresh processes isolate mimalloc startup options. The default and
    // explicit overrides read before the setter also pin the option index.
    #[test]
    fn purge_delay_defaults_to_immediate_and_env_overrides() {
        let exe = std::env::current_exe().expect("test binary path");
        for (env, before, after) in [
            (None, 1000, 0),
            (Some(("MIMALLOC_PURGE_DELAY", "7")), 7, 7),
            (Some(("mimalloc_purge_delay", "9")), 9, 9),
        ] {
            let mut child = std::process::Command::new(&exe);
            child
                .arg("--exact")
                .arg("allocator::tests::purge_delay_child")
                .arg("--nocapture")
                .env("ROCK_ALLOC_PURGE_CHILD", format!("{before} {after}"))
                .env_remove("MIMALLOC_PURGE_DELAY")
                .env_remove("mimalloc_purge_delay");
            if let Some((key, value)) = env {
                child.env(key, value);
            }
            let out = child.output().expect("spawn fresh test process");
            assert!(
                out.status.success(),
                "purge delay with MIMALLOC_PURGE_DELAY={env:?}: {}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr),
            );
        }
    }

    #[test]
    fn purge_delay_child() {
        let Some(expect) = std::env::var_os("ROCK_ALLOC_PURGE_CHILD") else {
            return;
        };
        let expect = expect.to_string_lossy().into_owned();
        // SAFETY: mi_option_get takes no pointers and accepts any valid option.
        let before = unsafe { mi_option_get(super::MI_OPTION_PURGE_DELAY) };
        super::purge_freed_pages_immediately();
        // SAFETY: as above.
        let after = unsafe { mi_option_get(super::MI_OPTION_PURGE_DELAY) };
        assert_eq!(
            format!("{before} {after}"),
            expect,
            "purge delay before and after"
        );
    }
}
