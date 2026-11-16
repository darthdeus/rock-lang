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
