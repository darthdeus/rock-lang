//! A wave of independent units of work whose results one thread takes in
//! wave order while pool threads run the units ahead of it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};

/// One unit of a wave: run once, by whichever thread claims it first.
pub(crate) struct Unit<W, T> {
    pub(crate) work: W,
    claimed: AtomicBool,
    slot: Mutex<Slot<T>>,
    posted: Condvar,
}

struct Slot<T> {
    result: Option<T>,
    awaited: bool,
}

impl<W, T> Unit<W, T> {
    pub(crate) fn new(work: W) -> Self {
        Unit {
            work,
            claimed: AtomicBool::new(false),
            slot: Mutex::new(Slot {
                result: None,
                awaited: false,
            }),
            posted: Condvar::new(),
        }
    }

    fn claim(&self) -> bool {
        !self.claimed.swap(true, Ordering::AcqRel)
    }

    /// Run the unit and post its result, unless another thread has it.
    pub(crate) fn run_unclaimed(&self, run: impl FnOnce(&W) -> T) {
        if self.claim() {
            self.post(run(&self.work));
        }
    }

    fn post(&self, result: T) {
        let awaited = {
            let mut slot = self.slot.lock().expect("unit result");
            slot.result = Some(result);
            slot.awaited
        };
        // Nearly every result is posted before its turn comes, and a wake
        // with nobody asleep is still a system call on some hosts.
        if awaited {
            self.posted.notify_one();
        }
    }

    fn posted(&self) -> Option<T> {
        self.slot.lock().expect("unit result").result.take()
    }

    fn wait(&self) -> T {
        let mut slot = self.slot.lock().expect("unit result");
        loop {
            if let Some(result) = slot.result.take() {
                return result;
            }
            slot.awaited = true;
            slot = self.posted.wait(slot).expect("unit result");
        }
    }
}

/// The result of `units[index]`, for the one thread that takes a wave's
/// results in order. An unclaimed unit runs right here. While another thread
/// holds it, this thread runs later unclaimed units instead of idling, and
/// sleeps once none is left: polling for the result made the work a build
/// does depend on how its threads were scheduled. `unclaimed_from` is the
/// caller's cursor for one wave, starting at 0.
pub(crate) fn take_in_order<W, T>(
    units: &[Unit<W, T>],
    index: usize,
    unclaimed_from: &mut usize,
    run: impl Fn(&W) -> T,
) -> T {
    let unit = &units[index];
    if unit.claim() {
        return run(&unit.work);
    }
    loop {
        if let Some(result) = unit.posted() {
            return result;
        }
        let mut next = (*unclaimed_from).max(index + 1);
        while next < units.len() && !units[next].claim() {
            next += 1;
        }
        let Some(ahead) = units.get(next) else {
            *unclaimed_from = next;
            return unit.wait();
        };
        *unclaimed_from = next + 1;
        ahead.post(run(&ahead.work));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread::ThreadId;
    use std::time::Duration;

    fn wave(len: usize) -> Vec<Unit<usize, (usize, ThreadId)>> {
        (0..len).map(Unit::new).collect()
    }

    fn here(index: &usize) -> (usize, ThreadId) {
        (*index, std::thread::current().id())
    }

    #[test]
    fn an_unclaimed_unit_runs_on_the_taking_thread() {
        let units = wave(2);
        let mut cursor = 0;
        for index in 0..units.len() {
            assert_eq!(
                take_in_order(&units, index, &mut cursor, here),
                (index, std::thread::current().id())
            );
        }
    }

    #[test]
    fn a_unit_posted_by_another_thread_is_taken_not_rerun() {
        let units = wave(1);
        let other = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    units[0].run_unclaimed(here);
                    std::thread::current().id()
                })
                .join()
                .unwrap()
        });
        let ran = take_in_order(&units, 0, &mut 0, |_| panic!("ran twice"));
        assert_eq!(ran, (0, other));
    }

    #[test]
    fn the_taking_thread_runs_later_units_while_its_own_is_held() {
        let units = wave(4);
        let (helped, release) = mpsc::channel();
        let me = std::thread::current().id();
        assert!(units[0].claim());
        std::thread::scope(|scope| {
            let held = &units[0];
            scope.spawn(move || {
                // Hold unit 0 until the taking thread has run the rest.
                let mut ran = 0;
                while ran < 3 && release.recv_timeout(Duration::from_secs(10)).is_ok() {
                    ran += 1;
                }
                held.post((ran, std::thread::current().id()));
            });
            let mut cursor = 0;
            let first = take_in_order(&units, 0, &mut cursor, |index| {
                helped.send(*index).unwrap();
                here(index)
            });
            assert_eq!(first.0, 3, "units 1..4 ran while unit 0 was held");
            assert_ne!(first.1, me);
            for index in 1..units.len() {
                let ran = take_in_order(&units, index, &mut cursor, |_| panic!("ran twice"));
                assert_eq!(ran, (index, me));
            }
        });
    }

    #[cfg(unix)]
    #[test]
    fn waiting_with_nothing_left_to_run_uses_no_cpu() {
        fn thread_cpu() -> Duration {
            let mut now = std::mem::MaybeUninit::<libc::timespec>::uninit();
            // SAFETY: `now` is a valid out-pointer for one timespec.
            let status =
                unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, now.as_mut_ptr()) };
            assert_eq!(status, 0);
            // SAFETY: the call succeeded, so it wrote the timespec.
            let now = unsafe { now.assume_init() };
            Duration::new(now.tv_sec as u64, now.tv_nsec as u32)
        }

        let units = wave(1);
        assert!(units[0].claim());
        let held = Duration::from_millis(300);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(held);
                units[0].post(here(&0));
            });
            let before = thread_cpu();
            take_in_order(&units, 0, &mut 0, |_| panic!("ran twice"));
            let spent = thread_cpu() - before;
            assert!(
                spent < held / 10,
                "the waiting thread used {spent:?} of CPU over a {held:?} wait"
            );
        });
    }
}
