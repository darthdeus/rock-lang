//! Order-preserving parallel map and join, plus a work queue, sequential under
//! `--features sequential`.

/// Map `f` over `items`, in PARALLEL by default (rayon work-stealing pool) or
/// SEQUENTIALLY under `--features sequential` (clean single-thread sampling
/// profiles — mirrors bone's `sequential` shim). Order-preserving in both modes,
/// so the assembled Lua chunk is byte-identical regardless of the feature. Each
/// `emit_*_unit` is independent (read-only `ws`/`scx`/`cg`, owns its own
/// `MirFunc`), so the lower+cleanup+emit work fans out cleanly per definition.
#[cfg(not(feature = "sequential"))]
struct WorkerMode {
    inline_off: bool,
    allocation_report: bool,
    allocation_activation: bool,
}

#[cfg(not(feature = "sequential"))]
impl WorkerMode {
    fn capture() -> Self {
        Self {
            inline_off: skull_transform::inline_off_active(),
            allocation_report: crate::allocation_report::enabled(),
            allocation_activation: crate::allocation_report::activation_enabled(),
        }
    }

    fn enter(&self) -> WorkerScope {
        WorkerScope {
            _inline: skull_transform::inline_mode_guard(self.inline_off),
            _allocation_report: crate::allocation_report::scope(self.allocation_report),
            _allocation_activation: crate::allocation_report::activation_scope(
                self.allocation_activation,
            ),
        }
    }
}

#[cfg(not(feature = "sequential"))]
struct WorkerScope {
    _inline: skull_transform::InlineOffGuard,
    _allocation_report: crate::allocation_report::Scope,
    _allocation_activation: crate::allocation_report::ActivationScope,
}

pub(super) fn par_map<T: Send, R: Send>(items: Vec<T>, f: impl Fn(T) -> R + Sync + Send) -> Vec<R> {
    par_map_with(items, || (), move |_, item| f(item))
}

/// Like [`par_map`], but threads a per-worker mutable state `S` (built by `init`)
/// into `f` — for the function fan-out, an [`InlineCache`](skull_transform::InlineCache)
/// reused across every fn a rayon worker handles, so each callee lowers + classifies
/// at most once per worker. PARALLEL by default (`map_init`, one `S` per worker
/// thread); SEQUENTIALLY under `--features sequential` ONE `S` is shared across all
/// items, exactly reproducing the original whole-workspace cache. Order-preserving
/// in both modes.
#[cfg(not(feature = "sequential"))]
pub(super) fn par_map_with<T: Send, R: Send, S>(
    items: Vec<T>,
    init: impl Fn() -> S + Sync + Send,
    f: impl Fn(&mut S, T) -> R + Sync + Send,
) -> Vec<R> {
    use rayon::prelude::*;
    let mode = WorkerMode::capture();
    items
        .into_par_iter()
        .map_init(
            move || (init(), mode.enter()),
            move |state, item| f(&mut state.0, item),
        )
        .collect()
}

#[cfg(feature = "sequential")]
pub(super) fn par_map_with<T, R, S>(
    items: Vec<T>,
    init: impl Fn() -> S,
    f: impl Fn(&mut S, T) -> R,
) -> Vec<R> {
    let mut state = init();
    items.into_iter().map(|t| f(&mut state, t)).collect()
}

/// A work queue on the pool: `f` handles one item with a per-worker state
/// (built by `init`, reused across the items a worker handles, as in
/// [`par_map_with`]) and may push follow-up items, which are queued the moment
/// the item is done rather than at the end of a wave. Returns once the queue
/// has drained. Unordered: a caller that needs a deterministic result sorts
/// what it collected. SEQUENTIALLY under `--features sequential`, a LIFO
/// worklist over one state.
#[cfg(not(feature = "sequential"))]
pub(super) fn par_drain<T: Send, S: Send>(
    items: Vec<T>,
    init: impl Fn() -> S + Sync,
    f: impl Fn(&mut S, T, &mut Vec<T>) + Sync,
) {
    struct Drain<'f, T, S, I: Fn() -> S, F: Fn(&mut S, T, &mut Vec<T>)> {
        init: &'f I,
        f: &'f F,
        mode: WorkerMode,
        states: std::sync::Mutex<Vec<S>>,
        _item: std::marker::PhantomData<fn(T)>,
    }

    impl<T: Send, S: Send, I: Fn() -> S + Sync, F: Fn(&mut S, T, &mut Vec<T>) + Sync>
        Drain<'_, T, S, I, F>
    {
        fn spawn<'scope>(&'scope self, scope: &rayon::Scope<'scope>, item: T)
        where
            T: 'scope,
        {
            scope.spawn(move |scope| {
                let _scope = self.mode.enter();
                let mut state = self
                    .states
                    .lock()
                    .expect("drain state pool")
                    .pop()
                    .unwrap_or_else(|| (self.init)());
                let mut follow_ups = Vec::new();
                (self.f)(&mut state, item, &mut follow_ups);
                self.states.lock().expect("drain state pool").push(state);
                for item in follow_ups {
                    self.spawn(scope, item);
                }
            });
        }
    }

    let drain = Drain {
        init: &init,
        f: &f,
        mode: WorkerMode::capture(),
        states: std::sync::Mutex::new(Vec::new()),
        _item: std::marker::PhantomData,
    };
    rayon::scope(|scope| {
        for item in items {
            drain.spawn(scope, item);
        }
    });
}

#[cfg(feature = "sequential")]
pub(super) fn par_drain<T, S>(
    items: Vec<T>,
    init: impl Fn() -> S,
    f: impl Fn(&mut S, T, &mut Vec<T>),
) {
    let mut state = init();
    let mut pending = items;
    while let Some(item) = pending.pop() {
        f(&mut state, item, &mut pending);
    }
}

#[cfg(not(feature = "sequential"))]
pub(super) fn parallel_join<A: Send, B: Send>(
    left: impl FnOnce() -> A + Send,
    right: impl FnOnce() -> B + Send,
) -> (A, B) {
    // Either branch can run on another pool thread, away from the caller's
    // thread-local compile modes.
    let mode = WorkerMode::capture();
    rayon::join(
        || {
            let _scope = mode.enter();
            left()
        },
        || {
            let _scope = mode.enter();
            right()
        },
    )
}

#[cfg(feature = "sequential")]
pub(super) fn parallel_join<A, B>(left: impl FnOnce() -> A, right: impl FnOnce() -> B) -> (A, B) {
    (left(), right())
}

/// Run `spawned` on a pool worker while `here` runs on the CALLING thread, and
/// return both results. Unlike [`parallel_join`], the calling thread never
/// migrates into the pool, so thread-local witnesses `here` increments (the
/// graph and concrete build counters) stay on the thread that reads them.
#[cfg(not(feature = "sequential"))]
pub(super) fn join_in_place<A: Send, B>(
    spawned: impl FnOnce() -> A + Send,
    here: impl FnOnce() -> B,
) -> (A, B) {
    let slot = std::sync::Mutex::new(None);
    let b = rayon::in_place_scope(|scope| {
        scope.spawn(|_| {
            *slot.lock().expect("spawned setup lane result slot") = Some(spawned());
        });
        here()
    });
    let a = slot
        .into_inner()
        .expect("spawned setup lane result slot")
        .expect("spawned setup lane finished within its scope");
    (a, b)
}

#[cfg(feature = "sequential")]
pub(super) fn join_in_place<A, B>(spawned: impl FnOnce() -> A, here: impl FnOnce() -> B) -> (A, B) {
    (spawned(), here())
}

/// Drop `value` off the calling thread. One-shot builds release large caches
/// in the middle of serial assembly; freeing them on an idle worker keeps that
/// path short. Inline where there are no worker threads to hand it to.
#[cfg(all(not(feature = "sequential"), not(target_arch = "wasm32")))]
pub(crate) fn drop_in_background<T: Send + 'static>(value: T) {
    rayon::spawn(move || drop(value));
}

#[cfg(any(feature = "sequential", target_arch = "wasm32"))]
pub(crate) fn drop_in_background<T>(value: T) {
    drop(value);
}

#[cfg(all(test, not(feature = "sequential")))]
mod tests {
    use super::*;

    #[test]
    fn parallel_join_carries_compile_modes_into_both_branches() {
        let _inline = skull_transform::inline_off_guard();
        let _report = crate::allocation_report::scope(true);
        let _activation = crate::allocation_report::activation_scope(true);
        let modes = || {
            (
                skull_transform::inline_off_active(),
                crate::allocation_report::enabled(),
                crate::allocation_report::activation_enabled(),
            )
        };
        assert_eq!(
            parallel_join(modes, modes),
            ((true, true, true), (true, true, true))
        );
    }

    /// A pool thread waiting inside one compile's join can steal an item of
    /// another compile, so an item's mode must not depend on what its host
    /// thread currently holds.
    #[test]
    fn an_item_runs_under_its_own_inline_mode_on_a_thread_holding_another() {
        let normal = WorkerMode::capture();
        assert!(!normal.inline_off);
        let _other_compile = skull_transform::inline_off_guard();
        {
            let _item = normal.enter();
            assert!(!skull_transform::inline_off_active());
        }
        assert!(skull_transform::inline_off_active());
    }
}
