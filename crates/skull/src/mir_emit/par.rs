//! Order-preserving parallel map and join, sequential under `--features sequential`.

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
            _inline: self.inline_off.then(skull_transform::inline_off_guard),
            _allocation_report: crate::allocation_report::scope(self.allocation_report),
            _allocation_activation: crate::allocation_report::activation_scope(
                self.allocation_activation,
            ),
        }
    }
}

#[cfg(not(feature = "sequential"))]
struct WorkerScope {
    _inline: Option<skull_transform::InlineOffGuard>,
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

#[cfg(not(feature = "sequential"))]
pub(super) fn parallel_join<A: Send, B: Send>(
    left: impl FnOnce() -> A + Send,
    right: impl FnOnce() -> B + Send,
) -> (A, B) {
    rayon::join(left, right)
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
