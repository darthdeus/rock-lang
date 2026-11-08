//! The totality ablation.
//!
//! An EXPERIMENTAL build mode, off by default, in which the compiler is held to
//! this target invariant:
//!
//! > Every executable Rock body entering MIR has a concrete logical type for
//! > every expression, binding, call, and function value. Anything that cannot
//! > satisfy that is an explicit foreign/erased boundary, never a shared generic
//! > Rock body.
//!
//! Three things change under the mode, and nothing else:
//!
//! 1. instance DISCOVERY ignores the conditional gates (`any_value`, the
//!    `@inline`/`@pure` opt-in, the target-storage trigger). The gates are NOT
//!    deleted -- they are bypassed, so the default path is untouched;
//! 2. a generic Rock body that would be lowered SHARED is recorded as a fatal,
//!    classified into exactly one category with a span;
//! 3. inlining is disabled, so no splice can hide a missing standalone instance.
//!
//! **It COLLECTS rather than aborting.** A mode that failed at the first fatal
//! would report one call site per run, and the artifact this stage owes is a
//! taxonomy over the whole corpus. The owning compilation context collects the
//! records and the driver reports the census at the end.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use bone_semantic::prelude::Span;
use ustr::Ustr;

/// The five categories. **There is deliberately no `Other`/`Misc` variant**
/// -- S 0g requires every fatal to land in exactly one category, and an escape
/// hatch is how a taxonomy stops being evidence.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum FatalCategory {
    /// The graph cannot name the instantiation: no recorded bindings, a binding
    /// naming a generic the enclosing substitution does not bind, or a
    /// synthesized call with no binding record. These are Stage-2 work -- the
    /// record has to be added.
    MissingGraphEdge,
    /// The callee is legitimately a foreign body (`extern`, `inline(lua)`,
    /// `inline(js)`). Under totality it must become an explicit `BoundaryId`
    /// rather than a shared generic Rock body.
    TargetBoundary,
    /// The instantiation type grows through recursion.
    PolymorphicGrowth,
    /// An invariant the compiler already claims to hold did not: an arity
    /// mismatch, a declared generic that is not a type variable.
    CompilerInvariant,
    /// A valid language surface that has no concrete form today -- a generic
    /// `co fn`, an `Iterator`-bounded body that exists only as an inline splice.
    UnsupportedSurface,
}

impl FatalCategory {
    pub fn name(self) -> &'static str {
        match self {
            Self::MissingGraphEdge => "missing-graph-edge",
            Self::TargetBoundary => "target-boundary",
            Self::PolymorphicGrowth => "polymorphic-growth",
            Self::CompilerInvariant => "compiler-invariant",
            Self::UnsupportedSurface => "unsupported-surface",
        }
    }
}

/// One recorded fatal. `caller` and `span` are what make it actionable: a
/// report needs the body, the caller and the span, plus a smallest repro.
#[derive(Clone, Debug)]
pub struct Fatal {
    pub category: FatalCategory,
    /// The generic body that would have been lowered shared.
    pub callee: String,
    /// The body containing the call.
    pub caller: String,
    pub span: Span,
    /// Why this category and not another. Free text, but it must state the
    /// discriminant rather than restate the category name.
    pub detail: String,
}

struct ContextState {
    enabled: AtomicBool,
    examined: AtomicUsize,
    registry: Mutex<Vec<Fatal>>,
}

impl ContextState {
    fn new(enabled: bool) -> Self {
        Self {
            enabled: AtomicBool::new(enabled),
            examined: AtomicUsize::new(0),
            registry: Mutex::new(Vec::new()),
        }
    }
}

/// The ablation state for one compilation or test scope. A context is
/// explicitly installed in compiler worker threads, so concurrent callers do
/// not share a mode or census.
#[derive(Clone)]
pub struct Context(Arc<ContextState>);

impl Context {
    fn fresh(enabled: bool) -> Self {
        Self(Arc::new(ContextState::new(enabled)))
    }

    fn from_environment() -> Self {
        Self::fresh(std::env::var_os("ROCK_MONO_ABLATION").is_some())
    }
}

thread_local! {
    static CURRENT: RefCell<Context> = RefCell::new(Context::from_environment());
}

/// The context active on the calling thread.
pub fn current_context() -> Context {
    CURRENT.with(|slot| slot.borrow().clone())
}

/// Install a context for the calling thread until the returned guard drops.
pub fn enter_context(context: Context) -> ContextGuard {
    let previous = CURRENT.with(|slot| slot.replace(context));
    ContextGuard { previous }
}

pub struct ContextGuard {
    previous: Context,
}

impl Drop for ContextGuard {
    fn drop(&mut self) {
        CURRENT.with(|slot| {
            let _ = slot.replace(self.previous.clone());
        });
    }
}

/// Scope an isolated ablation census to the calling thread and any compiler
/// workers that explicitly inherit its context.
pub struct Scope {
    _context: ContextGuard,
}

pub fn scope(on: bool) -> Scope {
    let context = Context::fresh(on);
    Scope {
        _context: enter_context(context),
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        clear();
    }
}

fn with_current<R>(f: impl FnOnce(&ContextState) -> R) -> R {
    CURRENT.with(|slot| f(&slot.borrow().0))
}

/// Generic Rock calls the mode actually EXAMINED.
///
/// Without it, "no fatals" is ambiguous between "the build was clean" and "no
/// build happened" -- and a `rock sim` invocation that failed to resolve its
/// project printed the clean message while compiling nothing. A census whose
/// denominator is zero is not evidence of anything.
#[inline]
pub fn note_examined() {
    if enabled() {
        with_current(|context| {
            context.examined.fetch_add(1, Ordering::Relaxed);
        });
    }
}

pub fn examined() -> usize {
    with_current(|context| context.examined.load(Ordering::Relaxed))
}

/// Initialize from the environment. Called once by the driver; separate from
/// [`set_enabled`] so tests can toggle the mode in-process without touching the
/// environment of a parallel test.
pub fn init_from_env() {
    set_enabled(std::env::var_os("ROCK_MONO_ABLATION").is_some());
}

pub fn set_enabled(on: bool) {
    with_current(|context| context.enabled.store(on, Ordering::Relaxed));
}

/// Is the ablation active? Hot -- consulted at every generic call -- so it is a
/// relaxed atomic load rather than an env lookup.
#[inline]
pub fn enabled() -> bool {
    with_current(|context| context.enabled.load(Ordering::Relaxed))
}

/// Record a fatal. A no-op when the mode is off, so call sites need no guard of
/// their own beyond the discovery change itself.
pub fn record(
    category: FatalCategory,
    callee: impl Into<String>,
    caller: impl Into<String>,
    span: Span,
    detail: impl Into<String>,
) {
    if !enabled() {
        return;
    }
    let f = Fatal {
        category,
        callee: callee.into(),
        caller: caller.into(),
        span,
        detail: detail.into(),
    };
    with_current(|context| {
        context
            .registry
            .lock()
            .expect("ablation registry poisoned")
            .push(f);
    });
}

/// Drain the registry. The driver calls this once after the build.
pub fn take() -> Vec<Fatal> {
    with_current(|context| {
        std::mem::take(&mut *context.registry.lock().expect("ablation registry poisoned"))
    })
}

pub fn clear() {
    with_current(|context| {
        context.examined.store(0, Ordering::Relaxed);
        context
            .registry
            .lock()
            .expect("ablation registry poisoned")
            .clear();
    });
}

/// A record's CALL SITE: the source position, which is what makes two records
/// the same call rather than the same kind of call.
fn site_key(f: &Fatal) -> (Ustr, (usize, usize)) {
    (f.span.file, f.span.offset_range)
}

/// Distinct call SITES across all categories.
///
/// The registry holds one record per QUERY, not per call: `call_spec` is
/// consulted several times for one call site (the lowering itself, the
/// target-storage narrowing in `call_spec_for_target`, the tuple-return shape in
/// `call_ret_tuple_shape`), and each consultation that declines records again.
/// Counting records prices a call site by how many consumers happen to ask about
/// it, which is a property of the compiler's internal structure and not of the
/// corpus.
pub fn distinct_sites(fatals: &[Fatal]) -> usize {
    fatals
        .iter()
        .map(site_key)
        .collect::<std::collections::BTreeSet<_>>()
        .len()
}

/// Distinct callee NAMES per category. A category's pair count and its callee
/// count answer different questions -- 578 pairs over 23 callees is 23 things to
/// specify, not 578 -- and reporting only one of them is how a declarations
/// number and a pairs number get quoted for each other.
pub fn callees_by_category(fatals: &[Fatal]) -> std::collections::BTreeMap<FatalCategory, usize> {
    use std::collections::{BTreeMap, BTreeSet};
    let mut by: BTreeMap<FatalCategory, BTreeSet<&str>> = BTreeMap::new();
    for f in fatals {
        by.entry(f.category).or_default().insert(f.callee.as_str());
    }
    by.into_iter().map(|(c, s)| (c, s.len())).collect()
}

/// Fatals collapsed to `(category, callee, caller)` with a distinct-SITE count
/// and one representative span.
///
/// A single generic stdlib fn called from 300 places is one defect, not 300, so
/// the taxonomy is priced by distinct cause. The count carried alongside is
/// distinct sites (see [`distinct_sites`]) rather than raw records.
pub fn distinct(fatals: &[Fatal]) -> Vec<(FatalCategory, String, String, usize, Span)> {
    use std::collections::{BTreeMap, BTreeSet};
    let mut by: BTreeMap<
        (FatalCategory, String, String),
        (BTreeSet<(Ustr, (usize, usize))>, Span),
    > = BTreeMap::new();
    for f in fatals {
        let e = by
            .entry((f.category, f.callee.clone(), f.caller.clone()))
            .or_insert_with(|| (BTreeSet::new(), f.span));
        e.0.insert(site_key(f));
    }
    by.into_iter()
        .map(|((c, callee, caller), (n, s))| (c, callee, caller, n.len(), s))
        .collect()
}
