/// Shared parallel test runner infrastructure.
///
/// Both Rock language tests (`test.rs`) and LSP tests (`lsp_test.rs`) need
/// to run files in parallel, collect results, and print them in deterministic
/// file order. This module provides the common scaffolding:
///
/// - A unified `FileTestResult` type
/// - `print_file_result()` for consistent output formatting
/// - `run_files_parallel()` which accepts a closure for per-file work
use colored::Colorize;
use std::path::Path;

/// Result of running all tests in a single file.
#[derive(Debug, Clone)]
pub(crate) struct FileTestResult {
    /// Display name for the file (may include subdirectory for disambiguation).
    pub file_name: String,
    pub passed: usize,
    pub failed: usize,
    pub ignored: usize,
    /// Wall-clock time for the entire file (compilation + execution).
    pub duration: std::time::Duration,
    /// Optional compilation time breakdown (Rock tests track this; LSP tests don't).
    /// When present, `print_file_result` can show `exec + compile` in verbose mode.
    pub compilation_time: Option<std::time::Duration>,
    /// (test_name, error_detail) pairs for failed tests.
    pub failed_test_details: Vec<(String, String)>,
}

/// Print a single file's test result in the standard compact format:
///   `filename ...... ok (10/12 tests, 45ms)`
pub(crate) fn print_file_result(r: &FileTestResult) {
    let total = r.passed + r.failed + r.ignored;
    let total_ms = r.duration.as_secs_f64() * 1000.0;
    let dots = ".".repeat(60_usize.saturating_sub(r.file_name.len().min(50)));

    let verbose_timing = std::env::var("ROCK_VERBOSE_TIMING").is_ok();

    let status = if r.failed == 0 {
        "ok".green().to_string()
    } else {
        "FAILED".red().to_string()
    };

    if verbose_timing {
        if let Some(compile_time) = r.compilation_time {
            let exec_time = r.duration.saturating_sub(compile_time);
            println!(
                "{} {} {} ({}/{} tests, {:.2}ms total, {:.2}ms exec + {:.2}ms compile)",
                r.file_name,
                dots,
                status,
                r.passed,
                total,
                total_ms,
                exec_time.as_secs_f64() * 1000.0,
                compile_time.as_secs_f64() * 1000.0,
            );
        } else {
            println!(
                "{} {} {} ({}/{} tests, {:.2}ms)",
                r.file_name, dots, status, r.passed, total, total_ms,
            );
        }
    } else {
        println!(
            "{} {} {} ({}/{} tests, {:.0}ms)",
            r.file_name, dots, status, r.passed, total, total_ms,
        );
    }

    if r.failed > 0 {
        for (test_name, error) in &r.failed_test_details {
            println!("  {} ... {}", test_name, "FAILED".red());
            for line in error.lines() {
                println!("    {}", line);
            }
        }
    }
}

/// Run a set of files in parallel, printing results in deterministic order.
///
/// `files` is a list of paths to process. `worker` is called once per file
/// on a thread pool and must return a `FileTestResult`. Results are printed
/// in the original file order as soon as all preceding files have completed
/// (streaming — no need to wait for the entire batch).
///
/// `num_threads_override` allows the caller to cap the thread count (e.g.
/// from a config option). Pass `None` to auto-detect from available CPUs.
///
/// Returns `(total, passed, failed, ignored)`.
pub(crate) fn run_files_parallel<F>(
    files: Vec<std::path::PathBuf>,
    num_threads_override: Option<usize>,
    worker: F,
) -> (usize, usize, usize, usize)
where
    F: Fn(&Path) -> FileTestResult + Send + Sync + 'static,
{
    if files.is_empty() {
        return (0, 0, 0, 0);
    }

    let num_threads = num_threads_override
        .map(|n| n.max(1))
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
        })
        .min(files.len());

    // Channel for streaming results to the printer thread
    let (tx, rx) = std::sync::mpsc::channel::<(usize, FileTestResult)>();

    // Work queue: (original_index, path)
    let work_queue = std::sync::Arc::new(parking_lot::Mutex::new(
        std::collections::VecDeque::from_iter(files.into_iter().enumerate()),
    ));

    let worker = std::sync::Arc::new(worker);

    // Printer thread: receives results and prints in file order.
    // Uses a BTreeMap to buffer out-of-order results and prints them
    // as soon as all predecessors are available.
    let printer_handle = {
        std::thread::spawn(move || {
            let mut total_passed = 0usize;
            let mut total_failed = 0usize;
            let mut total_ignored = 0usize;

            let mut pending: std::collections::BTreeMap<usize, FileTestResult> =
                std::collections::BTreeMap::new();
            let mut next_to_print: usize = 0;

            while let Ok((idx, result)) = rx.recv() {
                pending.insert(idx, result);

                // Print consecutive results starting from next_to_print
                while let Some(r) = pending.remove(&next_to_print) {
                    total_passed += r.passed;
                    total_failed += r.failed;
                    total_ignored += r.ignored;

                    if r.passed + r.failed + r.ignored > 0 {
                        print_file_result(&r);
                    }

                    next_to_print += 1;
                }
            }

            // Drain anything left (shouldn't happen, but defensive)
            for (_, r) in pending {
                total_passed += r.passed;
                total_failed += r.failed;
                total_ignored += r.ignored;
                if r.passed + r.failed + r.ignored > 0 {
                    print_file_result(&r);
                }
            }

            (total_passed, total_failed, total_ignored)
        })
    };

    // Spawn worker threads
    let mut handles = Vec::new();
    for _ in 0..num_threads {
        let tx = tx.clone();
        let queue = work_queue.clone();
        let worker = worker.clone();

        handles.push(std::thread::spawn(move || {
            loop {
                let item = queue.lock().pop_front();
                let (idx, path) = match item {
                    Some(item) => item,
                    None => break,
                };
                let result = worker(&path);
                let _ = tx.send((idx, result));
            }
        }));
    }

    // Drop our sender so the printer thread terminates after all workers finish
    drop(tx);

    for handle in handles {
        // A worker panic would otherwise vanish: its in-flight item
        // produces no result, the run reports fewer tests, and there's
        // no indication why. Surface it instead of discarding the join.
        if let Err(panic) = handle.join() {
            let msg = panic
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "<non-string panic payload>".to_string());
            eprintln!("parallel_runner: a worker thread panicked: {msg}");
        }
    }

    let (total_passed, total_failed, total_ignored) = printer_handle.join().unwrap();
    let total = total_passed + total_failed + total_ignored;

    (total, total_passed, total_failed, total_ignored)
}

/// Run `work` over `items` on `threads` dedicated workers and return the
/// results in input order.
///
/// Not the rayon pool: compiler passes fan out on it, and a pool thread
/// blocked in one unit's join may steal another whole unit, nesting a second
/// compile inside the first's thread-local state (compile records, caches).
/// Workers get the compiler stack.
pub(crate) fn map_in_order<T, R, F>(items: &[T], threads: usize, work: F) -> Vec<R>
where
    T: Sync,
    R: Send,
    F: Fn(&T) -> R + Sync,
{
    let threads = threads.clamp(1, items.len().max(1));
    let next = std::sync::atomic::AtomicUsize::new(0);
    let mut indexed: Vec<(usize, R)> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                let (next, work) = (&next, &work);
                std::thread::Builder::new()
                    .stack_size(rock_core::nesting::COMPILER_STACK_BYTES)
                    .spawn_scoped(scope, move || {
                        let mut done = Vec::new();
                        loop {
                            let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            let Some(item) = items.get(index) else { break };
                            done.push((index, work(item)));
                        }
                        done
                    })
                    .expect("cannot start a test worker thread")
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|worker| {
                worker
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            })
            .collect()
    });
    indexed.sort_unstable_by_key(|(index, _)| *index);
    indexed.into_iter().map(|(_, result)| result).collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn map_in_order_keeps_input_order_when_later_items_finish_first() {
        let items: Vec<u64> = (0..8).collect();
        let out = super::map_in_order(&items, 4, |&i| {
            std::thread::sleep(std::time::Duration::from_millis((8 - i) * 10));
            i
        });
        assert_eq!(out, items);
    }
}
