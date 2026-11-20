//! A bounded set of cancellable jobs served in submission order by one worker
//! thread, started on the first submit and joined by `shutdown`. Reserved bytes
//! stay charged until the result is taken, or only until the job finishes when
//! results do not hold them.

use std::collections::{HashMap, VecDeque};
use std::hash::{BuildHasherDefault, DefaultHasher};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;

const MAX_HANDLE: u64 = i32::MAX as u64;

pub(crate) enum Refused {
    Busy,
    Handles,
    Worker(std::io::Error),
}

pub(crate) enum Taken<R> {
    Missing,
    Pending,
    Rejected,
    Ready(R),
}

struct Entry<R> {
    cancelled: Arc<AtomicBool>,
    result: Option<R>,
    bytes: usize,
}

impl<R> Entry<R> {
    fn status(&self) -> i32 {
        if self.cancelled.load(Ordering::Acquire) {
            -1
        } else {
            i32::from(self.result.is_some())
        }
    }
}

struct State<W, R> {
    // A running entry stays until the worker finishes it, so its bytes stay reserved.
    entries: HashMap<u64, Entry<R>, BuildHasherDefault<DefaultHasher>>,
    queue: VecDeque<(u64, W, Arc<AtomicBool>)>,
    reserved: usize,
    next_handle: u64,
    stopping: bool,
    worker: Option<JoinHandle<()>>,
}

impl<W, R> State<W, R> {
    fn remove(&mut self, handle: u64) -> Option<Entry<R>> {
        let entry = self.entries.remove(&handle)?;
        self.reserved -= entry.bytes;
        Some(entry)
    }

    fn status(&self, handle: u64) -> i32 {
        self.entries.get(&handle).map_or(-1, Entry::status)
    }
}

pub(crate) struct Jobs<W, R> {
    thread_name: &'static str,
    max_jobs: usize,
    max_bytes: usize,
    results_hold_bytes: bool,
    run: fn(W, &AtomicBool) -> Option<R>,
    state: Mutex<State<W, R>>,
    wake: Condvar,
}

impl<W: Send + 'static, R: Send + 'static> Jobs<W, R> {
    pub(crate) const fn new(
        thread_name: &'static str,
        max_jobs: usize,
        max_bytes: usize,
        results_hold_bytes: bool,
        run: fn(W, &AtomicBool) -> Option<R>,
    ) -> Self {
        Self {
            thread_name,
            max_jobs,
            max_bytes,
            results_hold_bytes,
            run,
            state: Mutex::new(State {
                entries: HashMap::with_hasher(BuildHasherDefault::new()),
                queue: VecDeque::new(),
                reserved: 0,
                next_handle: 1,
                stopping: false,
                worker: None,
            }),
            wake: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State<W, R>> {
        self.state.lock().unwrap()
    }

    pub(crate) fn reserve(&self, bytes: usize) -> bool {
        let mut state = self.lock();
        let fits = (state.reserved.checked_add(bytes)).is_some_and(|total| total <= self.max_bytes);
        if fits {
            state.reserved += bytes;
        }
        fits
    }

    pub(crate) fn release(&self, bytes: usize) {
        self.lock().reserved -= bytes;
    }

    pub(crate) fn submit(&'static self, work: W, bytes: usize) -> Result<u64, Refused> {
        if !self.reserve(bytes) {
            return Err(Refused::Busy);
        }
        self.submit_reserved(work, bytes).map_err(|(refused, _)| {
            self.release(bytes);
            refused
        })
    }

    /// Queues work whose `bytes` an earlier `reserve` charged; a refusal hands it back.
    pub(crate) fn submit_reserved(
        &'static self,
        work: W,
        bytes: usize,
    ) -> Result<u64, (Refused, W)> {
        let mut state = self.lock();
        if state.stopping || state.entries.len() >= self.max_jobs {
            return Err((Refused::Busy, work));
        }
        if state.next_handle > MAX_HANDLE {
            return Err((Refused::Handles, work));
        }
        if state.worker.is_none() {
            match std::thread::Builder::new()
                .name(self.thread_name.into())
                .spawn(move || self.serve())
            {
                Ok(worker) => state.worker = Some(worker),
                Err(error) => return Err((Refused::Worker(error), work)),
            }
        }
        let handle = state.next_handle;
        state.next_handle += 1;
        let cancelled = Arc::new(AtomicBool::new(false));
        state.entries.insert(
            handle,
            Entry {
                cancelled: Arc::clone(&cancelled),
                result: None,
                bytes,
            },
        );
        state.queue.push_back((handle, work, cancelled));
        self.wake.notify_all();
        Ok(handle)
    }

    fn serve(&self) {
        let mut state = self.lock();
        loop {
            let Some((handle, work, cancelled)) = state.queue.pop_front() else {
                if state.stopping {
                    return;
                }
                state = self.wake.wait(state).unwrap();
                continue;
            };
            drop(state);
            let result = if cancelled.load(Ordering::Acquire) {
                None
            } else {
                (self.run)(work, &cancelled)
            };
            state = self.lock();
            let locked = &mut *state;
            match (result, locked.entries.get_mut(&handle)) {
                (Some(result), Some(entry)) if !cancelled.load(Ordering::Acquire) => {
                    entry.result = Some(result);
                    if !self.results_hold_bytes {
                        locked.reserved -= std::mem::take(&mut entry.bytes);
                    }
                }
                _ => {
                    locked.remove(handle);
                }
            }
            self.wake.notify_all();
        }
    }

    /// -1 when the handle is unknown or cancelled, 0 while pending, 1 when ready.
    pub(crate) fn poll(&self, handle: u64) -> i32 {
        self.lock().status(handle)
    }

    pub(crate) fn wait(&self, handle: u64) -> i32 {
        let mut state = self.lock();
        loop {
            match state.status(handle) {
                0 => state = self.wake.wait(state).unwrap(),
                status => return status,
            }
        }
    }

    /// Removes a ready result when `accept` approves it.
    pub(crate) fn take(&self, handle: u64, accept: impl FnOnce(&R) -> bool) -> Taken<R> {
        let mut state = self.lock();
        match state.status(handle) {
            -1 => Taken::Missing,
            0 => Taken::Pending,
            _ if !accept(state.entries[&handle].result.as_ref().unwrap()) => Taken::Rejected,
            _ => {
                let entry = state.remove(handle).unwrap();
                self.wake.notify_all();
                Taken::Ready(entry.result.unwrap())
            }
        }
    }

    pub(crate) fn cancel(&self, handle: u64) {
        let mut state = self.lock();
        let Some(entry) = state.entries.get(&handle) else {
            return;
        };
        entry.cancelled.store(true, Ordering::Release);
        let ready = entry.result.is_some();
        // Only a running job keeps its slot until the worker sees the flag.
        let queued = state
            .queue
            .iter()
            .position(|(queued, ..)| *queued == handle);
        if let Some(index) = queued {
            state.queue.remove(index);
        }
        if ready || queued.is_some() {
            state.remove(handle);
        }
        self.wake.notify_all();
    }

    #[cfg(test)]
    pub(crate) fn reserved(&self) -> usize {
        self.lock().reserved
    }

    pub(crate) fn shutdown(&self) {
        let worker = {
            let mut state = self.lock();
            state.stopping = true;
            for entry in state.entries.values() {
                entry.cancelled.store(true, Ordering::Release);
            }
            self.wake.notify_all();
            state.worker.take()
        };
        if worker.is_some_and(|worker| worker.join().is_err()) {
            crate::log_once!("JOBS", "{} terminated with a panic", self.thread_name);
        }
        let mut state = self.lock();
        state.reserved -= state
            .entries
            .drain()
            .map(|(_, entry)| entry.bytes)
            .sum::<usize>();
        state.queue.clear();
        state.stopping = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static GATE: Mutex<()> = Mutex::new(());
    static JOBS: Jobs<u32, u32> = Jobs::new("stone-test-jobs", 2, 10, true, |work, cancelled| {
        drop(GATE.lock().unwrap());
        (!cancelled.load(Ordering::Acquire)).then_some(work * 2)
    });

    #[test]
    fn bounded_cancellable_jobs_restart_after_shutdown() {
        let gate = GATE.lock().unwrap();
        let first = JOBS.submit(1, 4).ok().unwrap();
        assert!(matches!(JOBS.submit(2, 7), Err(Refused::Busy)));
        let second = JOBS.submit(3, 6).ok().unwrap();
        assert!(matches!(JOBS.submit(4, 0), Err(Refused::Busy)));
        assert_eq!((JOBS.poll(first), JOBS.poll(second)), (0, 0));
        JOBS.cancel(second);
        assert_eq!(JOBS.poll(second), -1);
        let refill = JOBS.submit(8, 6).ok().unwrap();
        JOBS.cancel(refill);
        drop(gate);
        assert_eq!(JOBS.wait(first), 1);
        assert!(matches!(JOBS.take(first, |_| false), Taken::Rejected));
        assert!(matches!(JOBS.take(first, |&r| r == 2), Taken::Ready(2)));
        assert!(matches!(JOBS.take(first, |_| true), Taken::Missing));

        let ready = JOBS.submit(5, 10).ok().unwrap();
        assert_eq!(JOBS.wait(ready), 1);
        JOBS.cancel(ready);
        let gate = GATE.lock().unwrap();
        let pending = JOBS.submit(6, 10).ok().unwrap();
        drop(gate);
        JOBS.shutdown();
        assert_eq!(JOBS.poll(pending), -1);
        let restarted = JOBS.submit(7, 10).ok().unwrap();
        assert!(restarted > pending);
        assert_eq!(JOBS.wait(restarted), 1);
        JOBS.shutdown();
    }
}
