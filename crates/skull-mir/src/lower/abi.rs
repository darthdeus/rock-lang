//! **The backend ABI of one emitted body** (plan S 13, cut C+D step A\*).
//!
//! Every question about how a function's values cross the wire -- which params
//! splat to leaves, what the return carries, how many lanes a JS result
//! publishes, what an `inline(lua)` body's params are spelled -- is answered
//! here, for one [`Instance`](crate::instance::Instance) under one execution
//! target, by one [`BackendFnAbiPlan`].
//!
//! The policy itself ([`fn_scalar_abi_spec`]) is PRIVATE to this module. That
//! is the whole point of the module boundary: a definition's parameter list and
//! the call that targets it used to reach the policy independently, so they
//! agreed only by being asked the same question in two places. They cannot
//! diverge if there is no second place to ask from.
//!
//! The execution target is part of the key rather than a formality.
//! [`ScalarCtx`] carries `values_first_class`, `total_specialization` and the
//! codegen backend, and the derived facts are split across targets too --
//! `result_lanes` is a JS transport fact luajit never reads, and
//! `lua_param_names` spells a foreign body's params. One physical plan shared
//! across targets would be answering with another target's ABI.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use bone_semantic::hir::{FnBackend, Function};
use bone_semantic::inference::Workspace;
use bone_semantic::prelude::Span;
use ustr::Ustr;

use super::{slot_param_names, RetSlot, ScalarCtx, SpecSubst, TupleRetShape, ValueShape, ValueStorage};
use super::shape::declared_value_storage;
use crate::concrete::{ConcreteBodyPlan, ConcreteSignature};
use crate::mir::FuncRef;

/// The one declaration-level classification shared by the instance graph,
/// artifact projection, and representation-boundary validation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum BoundaryClass {
    Extern,
    InlineBody,
    Intrinsic,
    ErasedRuntimeEntry,
    Invalid,
}

impl BoundaryClass {
    pub fn name(self) -> &'static str {
        match self {
            Self::Extern => "extern",
            Self::InlineBody => "inline-body",
            Self::Intrinsic => "intrinsic",
            Self::ErasedRuntimeEntry => "erased-runtime-entry",
            Self::Invalid => "invalid",
        }
    }
}

/// Classify only declaration-owned boundaries. Target capability and erased
/// runtime membership are supplied by the graph/artifact owner; keeping those
/// inputs out of this function prevents `@inline` from becoming an identity or
/// representation decision.
pub fn declared_boundary_class(f: &Function) -> Option<BoundaryClass> {
    if f.is_extern {
        return Some(BoundaryClass::Extern);
    }
    if crate::intrinsics::expand_kind(f).is_some() {
        return Some(BoundaryClass::Intrinsic);
    }
    if f.annotations.iter().any(|a| a.name.name.as_str() == "intrinsic") {
        let native_required = crate::intrinsics::is_native_required(f);
        let has_rock_body = f.inline_body.is_none() && !f.is_extern;
        return (native_required || !has_rock_body).then_some(BoundaryClass::Intrinsic);
    }
    f.inline_body.is_some().then_some(BoundaryClass::InlineBody)
}

/// Whether a declaration is an opaque boundary for scalarized value transport.
/// `@scalar_abi` is a transport contract for inline text; it does not create an
/// instance or change the declaration's boundary class.
pub fn boundary_requires_boxed_representation(f: &Function) -> bool {
    match declared_boundary_class(f) {
        Some(BoundaryClass::Extern) => true,
        Some(BoundaryClass::InlineBody) => {
            !f.annotations.iter().any(|a| a.name.name.as_str() == "scalar_abi")
        }
        _ => false,
    }
}

/// An interned physical representation.  Its numeric identity is meaningful
/// only through the build-scoped arena carried by [`ScalarCtx`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RepId(u32);

impl RepId {
    pub fn raw(self) -> u32 {
        self.0
    }
}

/// The use site is part of a representation key: a return tuple's element is
/// not allowed to silently become a function parameter merely because its
/// payload has the same leaves.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RepUse {
    Parameter,
    Return,
    TupleElement,
    Value,
    Boundary,
}

#[derive(Clone, Debug)]
pub struct RepUseFacts {
    pub tuple_spelled: bool,
    pub span: Option<Span>,
}

impl Default for RepUseFacts {
    fn default() -> Self {
        Self { tuple_spelled: false, span: None }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RepGuarantees {
    pub nobox: bool,
    /// Function-value and mixed scalar ABIs may carry reference types in an
    /// explicit opaque slot while still flattening eligible value leaves.
    pub allow_opaque: bool,
}

#[derive(Clone, Debug)]
pub struct RepError {
    pub span: Option<Span>,
    pub message: String,
}

impl std::fmt::Display for RepError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RepError {}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum RepLayout {
    Opaque,
    Leaves(ValueShape),
    Tuple { slots: Vec<RepId>, nullable: bool },
    Foreign { reason: Ustr },
}

/// One concrete physical answer in the target/use context supplied by the
/// planner.  Backends may inspect it, but they do not derive another answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepPlan {
    id: RepId,
    target: FnBackend,
    use_context: RepUse,
    layout: RepLayout,
    storage: Option<ValueStorage>,
    column_eligible: bool,
    nobox: bool,
}

impl RepPlan {
    pub fn id(&self) -> RepId {
        self.id
    }

    pub fn target(&self) -> FnBackend {
        self.target
    }

    pub fn use_context(&self) -> RepUse {
        self.use_context
    }

    pub fn layout(&self) -> &RepLayout {
        &self.layout
    }

    pub fn value_storage(&self) -> Option<ValueStorage> {
        self.storage
    }

    pub fn column_eligible(&self) -> bool {
        self.column_eligible
    }

    pub fn nobox(&self) -> bool {
        self.nobox
    }

    pub fn leaf_paths(&self) -> Option<Vec<Ustr>> {
        match &self.layout {
            RepLayout::Leaves(shape) => Some(crate::lower::value_leaf_paths(shape)),
            RepLayout::Opaque | RepLayout::Tuple { .. } | RepLayout::Foreign { .. } => None,
        }
    }

    pub fn presence_flags(&self) -> Option<Vec<bool>> {
        match &self.layout {
            RepLayout::Leaves(shape) => Some(crate::lower::value_leaf_presence_flags(shape)),
            RepLayout::Opaque | RepLayout::Tuple { .. } | RepLayout::Foreign { .. } => None,
        }
    }

    pub fn lanes(&self, arena: &RepArena) -> usize {
        match &self.layout {
            RepLayout::Opaque | RepLayout::Foreign { .. } => 1,
            RepLayout::Leaves(shape) => shape.leaf_count(),
            RepLayout::Tuple { slots, nullable } => {
                slots.iter().map(|slot| arena.get(*slot).lanes(arena)).sum::<usize>()
                    + usize::from(*nullable)
            }
        }
    }

    fn splats(&self, arena: &RepArena) -> bool {
        match &self.layout {
            RepLayout::Leaves(_) => true,
            RepLayout::Tuple { slots, .. } => slots.iter().any(|slot| arena.get(*slot).splats(arena)),
            RepLayout::Opaque | RepLayout::Foreign { .. } => false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionRepPlan {
    params: Vec<RepId>,
    result: Option<RepId>,
    return_lanes: usize,
    scalar_abi: bool,
}

/// The physical representation of an always-emitted global initializer.
/// Globals have no callable `Instance`, so their result plan is carried on the
/// lowered initializer itself rather than being smuggled through a function ABI.
#[derive(Clone, Debug)]
pub struct GlobalPlan {
    representation: RepId,
    arena: Arc<RepArena>,
}

impl PartialEq for GlobalPlan {
    fn eq(&self, other: &Self) -> bool {
        self.representation == other.representation && self.arena.equivalent(&other.arena)
    }
}

impl Eq for GlobalPlan {}

impl GlobalPlan {
    pub fn for_declared_type(
        ws: &Workspace,
        scx: &ScalarCtx,
        ty: bone_semantic::prelude::TyId,
    ) -> Result<Self, RepError> {
        let ty = crate::monoty::convert_decl(ws, ty, &SpecSubst::default()).map_err(|reason| RepError {
            span: None,
            message: format!("global type cannot be planned: {reason:?}"),
        })?;
        Self::for_mono_type(ws, scx, ty)
    }

    fn for_mono_type(ws: &Workspace, scx: &ScalarCtx, ty: crate::monoty::MonoTy) -> Result<Self, RepError> {
        let arena = scx.rep_arena();
        let representation = if scx.values_first_class {
            arena.intern(scx.target, RepUse::Value, RepLayout::Opaque)
        } else {
            arena.plan(
                ws,
                scx.target,
                &ty,
                RepUse::Value,
                &RepUseFacts::default(),
                RepGuarantees { nobox: true, allow_opaque: true },
            )?
        };
        Ok(Self { representation, arena })
    }

    pub(crate) fn for_concrete_result(
        ws: &Workspace,
        scx: &ScalarCtx,
        result: Option<crate::monoty::MonoTy>,
    ) -> Result<Self, RepError> {
        let ty = result.ok_or_else(|| RepError {
            span: None,
            message: "a global initializer must have a concrete result type".into(),
        })?;
        Self::for_mono_type(ws, scx, ty)
    }

    pub fn representation(&self) -> RepId {
        self.representation
    }

    pub fn use_context(&self) -> RepUse {
        self.arena.get(self.representation).use_context()
    }

    pub(crate) fn value_shape(&self) -> Option<ValueShape> {
        self.arena.value_shape(self.representation)
    }

    pub fn value_storage(&self) -> Option<ValueStorage> {
        self.arena.get(self.representation).value_storage()
    }

    pub fn leaf_paths(&self) -> Option<Vec<Ustr>> {
        self.arena.get(self.representation).leaf_paths()
    }

    pub fn presence_flags(&self) -> Option<Vec<bool>> {
        self.arena.get(self.representation).presence_flags()
    }

    pub fn leaf_names(&self, base: Ustr) -> Option<Vec<Ustr>> {
        let shape = self.value_shape()?;
        Some(
            (0..shape.leaf_count())
                .map(|leaf| crate::lower::nobox_global_leaf_name(base, leaf))
                .collect(),
        )
    }

    pub fn lanes(&self) -> usize {
        self.arena.get(self.representation).lanes(&self.arena)
    }
}

impl FunctionRepPlan {
    pub fn params(&self) -> &[RepId] {
        &self.params
    }

    pub fn result(&self) -> Option<RepId> {
        self.result
    }

    pub fn publisher_result(&self) -> Option<RepId> {
        self.result
    }

    pub fn return_lanes(&self) -> usize {
        self.return_lanes
    }

    pub fn uses_scalar_abi(&self) -> bool {
        self.scalar_abi
    }
}

/// One memo key per QUESTION, because the same instance answers two of them
/// differently: `Callee` is "the ABI a call site uses for the requested
/// instance" (backend-variant selection included), `Owner` is "the ABI of this
/// already-selected owner's own definition".
#[derive(Clone, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub enum AbiPlanKey {
    Callee(crate::instance::Instance),
    Owner(crate::instance::Instance),
}

/// ABI-plan memo counters, split by key space so each question's population
/// carries its own denominator: `requests = hits + misses` per space,
/// `stores` is the number of derivations committed (only `Ok` derivations
/// store), and `duplicate_cold_stores` counts a store that found the key
/// already present -- two workers racing the same cold key, each deriving.
/// `cloned_param_slots` is the `Vec<RepId>` slots cloned by memo hits, a cheap
/// proxy for what serving plans by handle instead of by clone would save.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AbiPlanMemoStats {
    pub owner_hits: u64,
    pub owner_misses: u64,
    pub callee_hits: u64,
    pub callee_misses: u64,
    /// Hits served by a worker's own mirror without touching the mutex
    /// (a subset of the per-space hit counts above).
    pub worker_local_hits: u64,
    pub stores: u64,
    pub duplicate_cold_stores: u64,
    pub unique_keys: u64,
    pub cloned_param_slots: u64,
    pub shared_lookups: u64,
    pub shared_hits: u64,
    pub shared_misses: u64,
    pub plan_clones: u64,
    pub plan_clone_param_slots: u64,
    pub shared_lock_ns: u64,
    pub local_clone_ns: u64,
    pub shared_clone_ns: u64,
    pub derivation_ns: u64,
    pub published_hits: u64,
    pub published_misses: u64,
    pub warm_keys: u64,
    pub warm_failures: u64,
    pub published_keys: u64,
    pub plan_size_histogram: [u64; ABI_PLAN_SIZE_BUCKETS],
    pub plan_size_max: u64,
}

const ABI_PLAN_SIZE_BUCKETS: usize = 256;

impl Default for AbiPlanMemoStats {
    fn default() -> Self {
        Self {
            owner_hits: 0,
            owner_misses: 0,
            callee_hits: 0,
            callee_misses: 0,
            worker_local_hits: 0,
            stores: 0,
            duplicate_cold_stores: 0,
            unique_keys: 0,
            cloned_param_slots: 0,
            shared_lookups: 0,
            shared_hits: 0,
            shared_misses: 0,
            plan_clones: 0,
            plan_clone_param_slots: 0,
            shared_lock_ns: 0,
            local_clone_ns: 0,
            shared_clone_ns: 0,
            derivation_ns: 0,
            published_hits: 0,
            published_misses: 0,
            warm_keys: 0,
            warm_failures: 0,
            published_keys: 0,
            plan_size_histogram: [0; ABI_PLAN_SIZE_BUCKETS],
            plan_size_max: 0,
        }
    }
}

pub(super) fn abi_plan_timing_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("STONE_LOG").is_ok_and(|value| value == "debug"))
}

fn add_elapsed(counter: &std::sync::atomic::AtomicU64, start: Option<Instant>) {
    if let Some(start) = start {
        counter.fetch_add(
            start.elapsed().as_nanos() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
    }
}

impl AbiPlanMemoStats {
    pub fn requests(&self) -> u64 {
        self.owner_hits + self.owner_misses + self.callee_hits + self.callee_misses
    }

    pub fn hits(&self) -> u64 {
        self.owner_hits + self.callee_hits
    }

    pub fn merge(&mut self, other: &Self) {
        self.owner_hits += other.owner_hits;
        self.owner_misses += other.owner_misses;
        self.callee_hits += other.callee_hits;
        self.callee_misses += other.callee_misses;
        self.worker_local_hits += other.worker_local_hits;
        self.stores += other.stores;
        self.duplicate_cold_stores += other.duplicate_cold_stores;
        self.unique_keys += other.unique_keys;
        self.cloned_param_slots += other.cloned_param_slots;
        self.shared_lookups += other.shared_lookups;
        self.shared_hits += other.shared_hits;
        self.shared_misses += other.shared_misses;
        self.plan_clones += other.plan_clones;
        self.plan_clone_param_slots += other.plan_clone_param_slots;
        self.shared_lock_ns += other.shared_lock_ns;
        self.local_clone_ns += other.local_clone_ns;
        self.shared_clone_ns += other.shared_clone_ns;
        self.derivation_ns += other.derivation_ns;
        self.published_hits += other.published_hits;
        self.published_misses += other.published_misses;
        self.warm_keys += other.warm_keys;
        self.warm_failures += other.warm_failures;
        self.published_keys += other.published_keys;
        for (dst, src) in self.plan_size_histogram.iter_mut().zip(other.plan_size_histogram) {
            *dst += src;
        }
        self.plan_size_max = self.plan_size_max.max(other.plan_size_max);
    }

    pub fn plan_size_samples(&self) -> u64 {
        self.plan_size_histogram.iter().sum()
    }

    fn plan_size_percentile(&self, numerator: u64, denominator: u64) -> u64 {
        let samples = self.plan_size_samples();
        if samples == 0 {
            return 0;
        }
        let rank = (samples * numerator).div_ceil(denominator).max(1);
        let mut seen = 0;
        for (size, count) in self.plan_size_histogram.iter().enumerate() {
            seen += count;
            if seen >= rank {
                return size as u64;
            }
        }
        (ABI_PLAN_SIZE_BUCKETS - 1) as u64
    }

    pub fn plan_size_p50(&self) -> u64 {
        self.plan_size_percentile(50, 100)
    }

    pub fn plan_size_p95(&self) -> u64 {
        self.plan_size_percentile(95, 100)
    }
}

/// Build-scoped memo for [`BackendFnAbiPlan`] derivations, owned by
/// [`ScalarCtx`] so every target/value-mode context has its own table and the
/// context's one mutation point ([`ScalarCtx::update_body`]) clears it. A plan
/// is a pure function of its key under a fixed (workspace, context, concrete
/// plans), so equal keys always store equal plans; caching only the `Ok` side
/// keeps every error path re-derived and spanned exactly as before.
/// A stored plan is immutable until [`AbiPlanCache::clear`], so each worker
/// mirrors the entries it has already paid the mutex for. The mirror is keyed
/// by a process-unique epoch that `clear()` REPLACES, which is what makes the
/// one invalidation point (`ScalarCtx::update_body`) reach every thread: a
/// stale mirror simply never matches the new epoch again.
#[derive(Default)]
struct AbiPlanLocalCache {
    epoch: u64,
    plans: rustc_hash::FxHashMap<AbiPlanKey, BackendFnAbiPlanHandle>,
}

impl AbiPlanLocalCache {
    fn reset_for(&mut self, epoch: u64) {
        if self.epoch != epoch {
            self.epoch = epoch;
            self.plans.clear();
        }
    }
}

thread_local! {
    static ABI_PLAN_LOCAL: std::cell::RefCell<AbiPlanLocalCache> =
        std::cell::RefCell::new(AbiPlanLocalCache::default());
}

fn next_abi_epoch() -> u64 {
    static EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

#[derive(Debug)]
pub(super) struct AbiPlanCache {
    plans: Mutex<HashMap<AbiPlanKey, BackendFnAbiPlanHandle>>,
    // Publication happens before the context enters the parallel fanout;
    // `clear` takes `&mut self`, so readers never race this pointer.
    published: Option<Arc<HashMap<AbiPlanKey, BackendFnAbiPlanHandle>>>,
    epoch: std::sync::atomic::AtomicU64,
    owner_hits: std::sync::atomic::AtomicU64,
    owner_misses: std::sync::atomic::AtomicU64,
    local_hits: std::sync::atomic::AtomicU64,
    callee_hits: std::sync::atomic::AtomicU64,
    callee_misses: std::sync::atomic::AtomicU64,
    stores: std::sync::atomic::AtomicU64,
    duplicate_cold_stores: std::sync::atomic::AtomicU64,
    cloned_param_slots: std::sync::atomic::AtomicU64,
    shared_lookups: std::sync::atomic::AtomicU64,
    shared_hits: std::sync::atomic::AtomicU64,
    shared_misses: std::sync::atomic::AtomicU64,
    plan_clones: std::sync::atomic::AtomicU64,
    plan_clone_param_slots: std::sync::atomic::AtomicU64,
    shared_lock_ns: std::sync::atomic::AtomicU64,
    local_clone_ns: std::sync::atomic::AtomicU64,
    shared_clone_ns: std::sync::atomic::AtomicU64,
    derivation_ns: std::sync::atomic::AtomicU64,
    warm_keys: std::sync::atomic::AtomicU64,
    warm_failures: std::sync::atomic::AtomicU64,
    published_hits: std::sync::atomic::AtomicU64,
    published_misses: std::sync::atomic::AtomicU64,
    published_keys: std::sync::atomic::AtomicU64,
    plan_size_histogram: [std::sync::atomic::AtomicU64; ABI_PLAN_SIZE_BUCKETS],
    plan_size_max: std::sync::atomic::AtomicU64,
}

impl Default for AbiPlanCache {
    fn default() -> Self {
        Self {
            plans: Mutex::default(),
            published: None,
            epoch: std::sync::atomic::AtomicU64::new(next_abi_epoch()),
            owner_hits: Default::default(),
            owner_misses: Default::default(),
            local_hits: Default::default(),
            callee_hits: Default::default(),
            callee_misses: Default::default(),
            stores: Default::default(),
            duplicate_cold_stores: Default::default(),
            cloned_param_slots: Default::default(),
            shared_lookups: Default::default(),
            shared_hits: Default::default(),
            shared_misses: Default::default(),
            plan_clones: Default::default(),
            plan_clone_param_slots: Default::default(),
            shared_lock_ns: Default::default(),
            local_clone_ns: Default::default(),
            shared_clone_ns: Default::default(),
            derivation_ns: Default::default(),
            warm_keys: Default::default(),
            warm_failures: Default::default(),
            published_hits: Default::default(),
            published_misses: Default::default(),
            published_keys: Default::default(),
            plan_size_histogram: std::array::from_fn(|_| std::sync::atomic::AtomicU64::new(0)),
            plan_size_max: Default::default(),
        }
    }
}

impl AbiPlanCache {
    fn record_plan_size(&self, plan: &BackendFnAbiPlan) {
        use std::sync::atomic::Ordering;
        let size = plan.representation.params.len();
        self.plan_size_histogram[size.min(ABI_PLAN_SIZE_BUCKETS - 1)]
            .fetch_add(1, Ordering::Relaxed);
        self.plan_size_max.fetch_max(size as u64, Ordering::Relaxed);
    }

    pub(super) fn lookup(&self, key: &AbiPlanKey) -> Option<BackendFnAbiPlanHandle> {
        use std::sync::atomic::Ordering;
        let published = match self.published.as_ref().and_then(|plans| plans.get(key)) {
            Some(plan) => {
                self.published_hits.fetch_add(1, Ordering::Relaxed);
                Some(Arc::clone(plan))
            }
            None if self.published.is_some() => {
                self.published_misses.fetch_add(1, Ordering::Relaxed);
                None
            }
            None => None,
        };
        let found = published.or_else(|| {
            let epoch = self.epoch.load(Ordering::Acquire);
            ABI_PLAN_LOCAL.with(|cache| {
                let mut cache = cache.borrow_mut();
                cache.reset_for(epoch);
                if let Some(plan) = cache.plans.get(key) {
                    self.local_hits.fetch_add(1, Ordering::Relaxed);
                    return Some(Arc::clone(plan));
                }
                self.shared_lookups.fetch_add(1, Ordering::Relaxed);
                let lock_start = abi_plan_timing_enabled().then(Instant::now);
                let found = {
                    let plans = self.plans.lock().expect("abi plan cache poisoned");
                    let Some(plan) = plans.get(key) else {
                        self.shared_misses.fetch_add(1, Ordering::Relaxed);
                        add_elapsed(&self.shared_lock_ns, lock_start);
                        return None;
                    };
                    Arc::clone(plan)
                };
                add_elapsed(&self.shared_lock_ns, lock_start);
                self.shared_hits.fetch_add(1, Ordering::Relaxed);
                cache.plans.insert(key.clone(), Arc::clone(&found));
                Some(found)
            })
        });
        let (hits, misses) = match key {
            AbiPlanKey::Owner(_) => (&self.owner_hits, &self.owner_misses),
            AbiPlanKey::Callee(_) => (&self.callee_hits, &self.callee_misses),
        };
        match &found {
            Some(plan) => {
                hits.fetch_add(1, Ordering::Relaxed);
                self.record_plan_size(plan.as_ref());
            }
            None => {
                misses.fetch_add(1, Ordering::Relaxed);
            }
        };
        found
    }

    pub(super) fn record_derivation(&self, elapsed: Option<Instant>) {
        self.derivation_ns.fetch_add(
            elapsed.map_or(0, |start| start.elapsed().as_nanos() as u64),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    pub(super) fn store(&self, key: AbiPlanKey, plan: &BackendFnAbiPlanHandle) {
        use std::sync::atomic::Ordering;
        let epoch = self.epoch.load(Ordering::Acquire);
        let replaced =
            self.plans.lock().expect("abi plan cache poisoned").insert(key.clone(), Arc::clone(plan));
        ABI_PLAN_LOCAL.with(|cache| {
            let mut cache = cache.borrow_mut();
            cache.reset_for(epoch);
            cache.plans.insert(key, Arc::clone(plan));
        });
        self.stores.fetch_add(1, Ordering::Relaxed);
        if replaced.is_some() {
            self.duplicate_cold_stores.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(super) fn clear(&mut self) {
        // New epoch FIRST: a worker that raced the old epoch into its mirror
        // re-reads the shared map (now cleared) rather than serving stale plans.
        self.epoch.store(next_abi_epoch(), std::sync::atomic::Ordering::Release);
        self.plans.lock().expect("abi plan cache poisoned").clear();
        self.published = None;
        self.warm_keys.store(0, std::sync::atomic::Ordering::Relaxed);
        self.published_keys.store(0, std::sync::atomic::Ordering::Relaxed);
        self.warm_failures.store(0, std::sync::atomic::Ordering::Relaxed);
    }

    pub(super) fn publish(&mut self, warm_keys: u64, warm_failures: u64) {
        let plans = std::mem::take(&mut *self.plans.lock().expect("abi plan cache poisoned"));
        self.published_keys.store(plans.len() as u64, std::sync::atomic::Ordering::Relaxed);
        self.warm_keys.store(warm_keys, std::sync::atomic::Ordering::Relaxed);
        self.warm_failures.store(warm_failures, std::sync::atomic::Ordering::Relaxed);
        self.published = Some(Arc::new(plans));
    }

    /// Lifetime counters plus the table's current population. Lookups are
    /// counted where they happen, so per-space `hits + misses` is the number of
    /// cached questions asked in that key space.
    pub(super) fn stats(&self) -> AbiPlanMemoStats {
        use std::sync::atomic::Ordering;
        AbiPlanMemoStats {
            owner_hits: self.owner_hits.load(Ordering::Relaxed),
            owner_misses: self.owner_misses.load(Ordering::Relaxed),
            callee_hits: self.callee_hits.load(Ordering::Relaxed),
            callee_misses: self.callee_misses.load(Ordering::Relaxed),
            worker_local_hits: self.local_hits.load(Ordering::Relaxed),
            stores: self.stores.load(Ordering::Relaxed),
            duplicate_cold_stores: self.duplicate_cold_stores.load(Ordering::Relaxed),
            unique_keys: self.published.as_ref().map_or(0, |plans| plans.len() as u64)
                + self.plans.lock().expect("abi plan cache poisoned").len() as u64,
            cloned_param_slots: self.cloned_param_slots.load(Ordering::Relaxed),
            shared_lookups: self.shared_lookups.load(Ordering::Relaxed),
            shared_hits: self.shared_hits.load(Ordering::Relaxed),
            shared_misses: self.shared_misses.load(Ordering::Relaxed),
            plan_clones: self.plan_clones.load(Ordering::Relaxed),
            plan_clone_param_slots: self.plan_clone_param_slots.load(Ordering::Relaxed),
            shared_lock_ns: self.shared_lock_ns.load(Ordering::Relaxed),
            local_clone_ns: self.local_clone_ns.load(Ordering::Relaxed),
            shared_clone_ns: self.shared_clone_ns.load(Ordering::Relaxed),
            derivation_ns: self.derivation_ns.load(Ordering::Relaxed),
            published_hits: self.published_hits.load(Ordering::Relaxed),
            published_misses: self.published_misses.load(Ordering::Relaxed),
            warm_keys: self.warm_keys.load(Ordering::Relaxed),
            warm_failures: self.warm_failures.load(Ordering::Relaxed),
            published_keys: self.published_keys.load(Ordering::Relaxed),
            plan_size_histogram: std::array::from_fn(|i| {
                self.plan_size_histogram[i].load(Ordering::Relaxed)
            }),
            plan_size_max: self.plan_size_max.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct RepArenaInner {
    plans: Vec<RepPlan>,
    by_key: HashMap<(FnBackend, RepUse, RepLayout, Option<ValueStorage>, bool, bool), RepId>,
}

/// The one build-scoped physical representation arena.  `ScalarCtx` owns the
/// handle and every ABI plan references the same arena, so equal answers are
/// equal identities rather than merely equal cloned shapes.
///
/// The counters ride outside `inner` so [`RepArenaInner`]'s derived equality
/// (the [`BackendFnAbiPlan`] identity check) never compares them.
///
/// READS ARE SERVED THROUGH A WORKER-LOCAL CACHE. The arena is monotonic
/// (plans push-only, `by_key` insert-only, nothing published ever changes), so
/// a thread may keep private copies of the `(RepId -> RepPlan)` and
/// `(key -> RepId)` facts it has already paid the mutex for; a cold id or key
/// reads through to the lock. `arena_gen` is a process-unique id keying that
/// cache, so two live arenas (the Lua and shader contexts) or an arena
/// allocated at a reused address can never serve each other's plans.
#[derive(Debug)]
pub struct RepArena {
    inner: Mutex<RepArenaInner>,
    arena_gen: u64,
    get_calls: std::sync::atomic::AtomicU64,
    local_get_hits: std::sync::atomic::AtomicU64,
    intern_attempts: std::sync::atomic::AtomicU64,
    local_intern_hits: std::sync::atomic::AtomicU64,
    intern_new: std::sync::atomic::AtomicU64,
}

impl Default for RepArena {
    fn default() -> Self {
        static ARENA_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Self {
            inner: Mutex::default(),
            arena_gen: ARENA_GEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            get_calls: Default::default(),
            local_get_hits: Default::default(),
            intern_attempts: Default::default(),
            local_intern_hits: Default::default(),
            intern_new: Default::default(),
        }
    }
}

type RepKey = (FnBackend, RepUse, RepLayout, Option<ValueStorage>, bool, bool);

#[derive(Default)]
struct RepReadCache {
    arena_gen: u64,
    plans: Vec<Option<RepPlan>>,
    by_key: rustc_hash::FxHashMap<RepKey, RepId>,
}

impl RepReadCache {
    fn reset_for(&mut self, arena_gen: u64) {
        if self.arena_gen != arena_gen {
            self.arena_gen = arena_gen;
            self.plans.clear();
            self.by_key.clear();
        }
    }

    fn record_plan(&mut self, id: RepId, plan: &RepPlan) {
        let idx = id.0 as usize;
        if self.plans.len() <= idx {
            self.plans.resize(idx + 1, None);
        }
        self.plans[idx] = Some(plan.clone());
    }
}

thread_local! {
    static REP_READ_CACHE: std::cell::RefCell<RepReadCache> =
        std::cell::RefCell::new(RepReadCache::default());
}

/// Lifetime traffic counters for one [`RepArena`]. `local_*_hits` are the
/// reads the worker-local cache served without touching the mutex; the
/// remainder paid a lock acquisition (gets also a deep [`RepPlan`] clone from
/// shared memory).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RepArenaStats {
    pub get_calls: u64,
    pub local_get_hits: u64,
    pub intern_attempts: u64,
    pub local_intern_hits: u64,
    pub intern_new: u64,
    pub plans: u64,
}

fn mono_value_storage(
    ws: &Workspace,
    target: FnBackend,
    ty: &crate::monoty::MonoTy,
) -> Option<ValueStorage> {
    let (payload, _) = ty.peel_nullable();
    let crate::monoty::MonoTy::App {
        ctor: crate::monoty::MonoCtor::Nominal(name), ..
    } = payload
    else {
        return None;
    };
    declared_value_storage(ws, target, *name)
}

fn declared_nobox(ws: &Workspace, ty: &crate::monoty::MonoTy) -> bool {
    let (payload, _) = ty.peel_nullable();
    let crate::monoty::MonoTy::App {
        ctor: crate::monoty::MonoCtor::Nominal(name), ..
    } = payload
    else {
        return false;
    };
    crate::lower::is_nobox_nominal(ws, *name)
}

fn column_eligible_for_shape(
    ws: &Workspace,
    ty: &crate::monoty::MonoTy,
    shape: &ValueShape,
) -> bool {
    matches!(ty.peel_nullable().0, crate::monoty::MonoTy::App {
        ctor: crate::monoty::MonoCtor::Nominal(_), ..
    }) && super::shape::value_shape_rec(ws, shape.name, &mut Vec::new()).is_some()
}

impl RepArena {
    fn equivalent(&self, other: &Self) -> bool {
        if std::ptr::eq(self, other) {
            return true;
        }
        let left = self.inner.lock().expect("RepArena lock poisoned");
        let right = other.inner.lock().expect("RepArena lock poisoned");
        *left == *right
    }

    fn intern(&self, target: FnBackend, use_context: RepUse, layout: RepLayout) -> RepId {
        self.intern_with_storage(target, use_context, layout, None)
    }

    fn intern_with_storage(
        &self,
        target: FnBackend,
        use_context: RepUse,
        layout: RepLayout,
        storage: Option<ValueStorage>,
    ) -> RepId {
        self.intern_with_facts(target, use_context, layout, storage, false, false)
    }

    fn intern_with_facts(
        &self,
        target: FnBackend,
        use_context: RepUse,
        layout: RepLayout,
        storage: Option<ValueStorage>,
        column_eligible: bool,
        nobox: bool,
    ) -> RepId {
        self.intern_attempts.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let key = (target, use_context, layout, storage, column_eligible, nobox);
        REP_READ_CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();
            cache.reset_for(self.arena_gen);
            if let Some(id) = cache.by_key.get(&key) {
                self.local_intern_hits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return *id;
            }
            let id = {
                let mut inner = self.inner.lock().expect("RepArena lock poisoned");
                match inner.by_key.get(&key) {
                    Some(id) => *id,
                    None => {
                        self.intern_new.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let id = RepId(inner.plans.len() as u32);
                        let (target, use_context, layout, storage, column_eligible, nobox) =
                            key.clone();
                        inner.plans.push(RepPlan {
                            id,
                            target,
                            use_context,
                            layout,
                            storage,
                            column_eligible,
                            nobox,
                        });
                        inner.by_key.insert(key.clone(), id);
                        id
                    }
                }
            };
            cache.by_key.insert(key, id);
            id
        })
    }

    pub(crate) fn column_eligible(&self, id: RepId) -> bool {
        self.get(id).column_eligible()
    }

    pub(crate) fn plan_container_element(
        &self,
        ws: &Workspace,
        target: FnBackend,
        ty: &crate::monoty::MonoTy,
    ) -> Result<RepId, RepError> {
        self.plan(
            ws,
            target,
            ty,
            RepUse::Boundary,
            &RepUseFacts::default(),
            RepGuarantees { nobox: declared_nobox(ws, ty), allow_opaque: false },
        )
    }

    pub fn get(&self, id: RepId) -> RepPlan {
        self.get_calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        REP_READ_CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();
            cache.reset_for(self.arena_gen);
            if let Some(Some(plan)) = cache.plans.get(id.0 as usize) {
                self.local_get_hits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return plan.clone();
            }
            let plan = self
                .inner
                .lock()
                .expect("RepArena lock poisoned")
                .plans
                .get(id.0 as usize)
                .cloned()
                .expect("RepId must belong to its build arena");
            cache.record_plan(id, &plan);
            plan
        })
    }

    pub fn stats(&self) -> RepArenaStats {
        use std::sync::atomic::Ordering;
        RepArenaStats {
            get_calls: self.get_calls.load(Ordering::Relaxed),
            local_get_hits: self.local_get_hits.load(Ordering::Relaxed),
            intern_attempts: self.intern_attempts.load(Ordering::Relaxed),
            local_intern_hits: self.local_intern_hits.load(Ordering::Relaxed),
            intern_new: self.intern_new.load(Ordering::Relaxed),
            plans: self.inner.lock().expect("RepArena lock poisoned").plans.len() as u64,
        }
    }

    /// Plan the physical value representation of a nominal without letting a
    /// backend recover storage from the declaration after lowering.
    pub fn plan_nominal_value(
        &self,
        ws: &Workspace,
        target: FnBackend,
        name: Ustr,
    ) -> Result<RepId, RepError> {
        let ty = crate::monoty::MonoTy::App {
            ctor: crate::monoty::MonoCtor::Nominal(name),
            params: Vec::new(),
        };
        self.plan(
            ws,
            target,
            &ty,
            RepUse::Value,
            &RepUseFacts::default(),
            RepGuarantees { nobox: false, allow_opaque: true },
        )
    }

    pub fn nominal_value_storage(
        &self,
        ws: &Workspace,
        target: FnBackend,
        name: Ustr,
    ) -> Option<ValueStorage> {
        let id = self.plan_nominal_value(ws, target, name).ok()?;
        self.get(id).value_storage()
    }

    pub fn native_buffer_element_ctype(
        &self,
        ws: &Workspace,
        target: FnBackend,
        element: &crate::monoty::MonoTy,
    ) -> Result<Ustr, RepError> {
        let element_name = element.render();
        let (element, nullable) = element.peel_nullable();
        let reject = |reason: String| RepError {
            span: None,
            message: format!(
                "`native_buffer_alloc<{element_name}>` requires a valid native-buffer element ctype: {reason}"
            ),
        };
        if nullable {
            return Err(reject("nullable elements have no native C-array ctype".into()));
        }
        match element {
            crate::monoty::MonoTy::Int => Ok(Ustr::from("int")),
            crate::monoty::MonoTy::Uint => Ok(Ustr::from("uint32_t")),
            crate::monoty::MonoTy::Float => Ok(Ustr::from("float")),
            crate::monoty::MonoTy::Bool => Ok(Ustr::from("bool")),
            crate::monoty::MonoTy::App {
                ctor: crate::monoty::MonoCtor::Nominal(name), ..
            } => {
                let id = self
                    .plan(
                        ws,
                        target,
                        element,
                        RepUse::Boundary,
                        &RepUseFacts::default(),
                        RepGuarantees { nobox: false, allow_opaque: true },
                    )
                    .map_err(|error| reject(error.message))?;
                let plan = self.get(id);
                if plan.value_storage() != Some(ValueStorage::CData)
                    || self.value_shape(id).is_none()
                {
                    return Err(reject(format!(
                        "`{name}` is not an FFI-clean value struct with a C leaf layout"
                    )));
                }
                self.leaf_layout(ws, id)
                    .map_err(|error| reject(error.message))?;
                Ok(*name)
            }
            _ => Err(reject(format!("`{element_name}` is not C-mappable"))),
        }
    }

    pub fn nominal_value_leaf_paths(
        &self,
        ws: &Workspace,
        target: FnBackend,
        name: Ustr,
    ) -> Option<Vec<Ustr>> {
        let id = self.plan_nominal_value(ws, target, name).ok()?;
        self.get(id).leaf_paths()
    }

    pub fn nominal_value_leaf_presence_flags(
        &self,
        ws: &Workspace,
        target: FnBackend,
        name: Ustr,
    ) -> Option<Vec<bool>> {
        let id = self.plan_nominal_value(ws, target, name).ok()?;
        self.get(id).presence_flags()
    }

    pub fn nominal_leaf_addresses(
        &self,
        ws: &Workspace,
        target: FnBackend,
        name: Ustr,
    ) -> Result<Vec<crate::lower::LeafAddr>, RepError> {
        let id = self.plan_nominal_value(ws, target, name)?;
        self.leaf_addresses(ws, id).ok_or_else(|| RepError {
            span: None,
            message: format!("{name} has no concrete leaf-addressed representation"),
        })
    }

    pub fn nominal_leaf_layout(
        &self,
        ws: &Workspace,
        target: FnBackend,
        name: Ustr,
    ) -> Result<(Vec<crate::lower::LeafSlot>, usize, usize), RepError> {
        let id = self.plan_nominal_value(ws, target, name)?;
        if self.value_shape(id).is_none() {
            return Err(RepError {
                span: None,
                message: format!("`{name}` has no value leaf layout"),
            });
        }
        self.leaf_layout(ws, id)
    }

    pub fn nominal_leaf_size(
        &self,
        ws: &Workspace,
        target: FnBackend,
        name: Ustr,
    ) -> Result<usize, RepError> {
        self.nominal_leaf_layout(ws, target, name).map(|(_, size, _)| size)
    }

    pub fn nominal_leaf_view_ctys(
        &self,
        ws: &Workspace,
        target: FnBackend,
        name: Ustr,
    ) -> Result<Vec<&'static str>, RepError> {
        let (leaves, _, _) = self.nominal_leaf_layout(ws, target, name)?;
        let mut out = Vec::new();
        for leaf in leaves {
            if !out.contains(&leaf.cty) {
                out.push(leaf.cty);
            }
        }
        Ok(out)
    }

    pub(crate) fn ret_slot(&self, id: RepId) -> RetSlot {
        match self.get(id).layout().clone() {
            RepLayout::Opaque | RepLayout::Foreign { .. } => RetSlot::Opaque,
            RepLayout::Leaves(shape) => RetSlot::Leaves(shape),
            RepLayout::Tuple { slots, nullable } => RetSlot::Nested(TupleRetShape {
                slots: slots.into_iter().map(|slot| self.ret_slot(slot)).collect(),
                nullable,
            }),
        }
    }

    pub(crate) fn value_shape(&self, id: RepId) -> Option<ValueShape> {
        match self.get(id).layout() {
            RepLayout::Leaves(shape) => Some(shape.clone()),
            _ => None,
        }
    }

    pub(crate) fn tuple_shape(&self, id: RepId) -> Option<TupleRetShape> {
        match self.get(id).layout().clone() {
            RepLayout::Tuple { slots, nullable } => Some(TupleRetShape {
                slots: slots.into_iter().map(|slot| self.ret_slot(slot)).collect(),
                nullable,
            }),
            _ => None,
        }
    }

    fn leaf_layout(
        &self,
        ws: &Workspace,
        id: RepId,
    ) -> Result<(Vec<crate::lower::LeafSlot>, usize, usize), RepError> {
        let plan = self.get(id);
        let shape = self.value_shape(id).ok_or_else(|| RepError {
            span: None,
            message: "the representation has no value leaf layout".into(),
        })?;
        if plan.value_storage() != Some(ValueStorage::CData) {
            return Err(RepError {
                span: None,
                message: format!("{} is not an FFI-clean CData representation", shape.name),
            });
        }
        let (leaves, size, align) = crate::lower::value_leaf_layout(ws, shape.name).ok_or_else(|| RepError {
            span: None,
            message: format!("`{}` has no C leaf layout", shape.name),
        })?;
        let want = crate::lower::value_leaf_paths(&shape);
        let got: Vec<Ustr> = leaves.iter().map(|leaf| leaf.path).collect();
        if want != got {
            return Err(RepError {
                span: None,
                message: format!(
                    "leaf walkers disagree for `{}`: value leaves {want:?} vs C layout {got:?}",
                    shape.name
                ),
            });
        }
        Ok((leaves, size, align))
    }

    pub(crate) fn leaf_addresses(
        &self,
        ws: &Workspace,
        id: RepId,
    ) -> Option<Vec<crate::lower::LeafAddr>> {
        if !self.column_eligible(id) {
            return None;
        }
        let (leaves, size, _) = self.leaf_layout(ws, id).ok()?;
        leaves
            .into_iter()
            .map(|leaf| {
                let (leaf_size, _) = crate::lower::c_prim_size_align(leaf.cty)?;
                if size % leaf_size != 0 || leaf.byte_off % leaf_size != 0 {
                    return None;
                }
                Some(crate::lower::LeafAddr {
                    path: leaf.path,
                    view_field: crate::lower::leaf_view_field(leaf.cty),
                    stride: (size / leaf_size) as i64,
                    offset: (leaf.byte_off / leaf_size) as i64,
                })
            })
            .collect()
    }

    /// Total physical planning for one concrete logical type. `Opaque` is an
    /// explicit representation, not an absent answer; only a guarantee that
    /// forbids that representation can produce an error.
    pub fn plan(
        &self,
        ws: &Workspace,
        target: FnBackend,
        ty: &crate::monoty::MonoTy,
        use_context: RepUse,
        facts: &RepUseFacts,
        guarantees: RepGuarantees,
    ) -> Result<RepId, RepError> {
        use crate::monoty::{MonoCtor, MonoTy};

        let reject = |message: String| RepError { span: facts.span, message };
        let tuple_element_shape = matches!(use_context, RepUse::TupleElement);
        match ty {
            MonoTy::Nullable(inner) => match inner.as_ref() {
                MonoTy::Tuple(parts) => {
                    let slots = parts
                        .iter()
                        .map(|part| {
                            self.plan(
                                ws,
                                target,
                                part,
                                RepUse::TupleElement,
                                facts,
                                guarantees,
                            )
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    let splats = slots.iter().any(|id| self.get(*id).lanes(self) > 1);
                    if splats
                        && (matches!(use_context, RepUse::Return | RepUse::TupleElement)
                            || facts.tuple_spelled)
                    {
                        return Ok(self.intern(
                            target,
                            use_context,
                            RepLayout::Tuple { slots, nullable: true },
                        ));
                    }
                    if guarantees.nobox && !guarantees.allow_opaque {
                        return Err(reject("nullable tuple cannot cross a nobox boundary without a physical tuple plan".into()));
                    }
                    Ok(self.intern(target, use_context, RepLayout::Opaque))
                }
                _ => {
                    let shape = if guarantees.nobox || tuple_element_shape {
                        crate::lower::mono_nobox_value_shape(ws, ty)
                    } else {
                        crate::lower::mono_value_shape(ws, ty)
                    };
                    if let Some(shape) = shape {
                        let storage = mono_value_storage(ws, target, ty);
                        let column_eligible = column_eligible_for_shape(ws, ty, &shape);
                        return Ok(self.intern_with_facts(
                            target,
                            use_context,
                            RepLayout::Leaves(shape),
                            storage,
                            column_eligible,
                            declared_nobox(ws, ty),
                        ));
                    }
                    if guarantees.nobox && !guarantees.allow_opaque {
                        return Err(reject(format!(
                            "{} has no concrete nobox representation",
                            ty.render()
                        )));
                    }
                    Ok(self.intern(target, use_context, RepLayout::Opaque))
                }
            },
            MonoTy::Tuple(parts) => {
                let slots = parts
                    .iter()
                    .map(|part| {
                        self.plan(
                            ws,
                            target,
                            part,
                            RepUse::TupleElement,
                            facts,
                            guarantees,
                        )
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let splats = slots.iter().any(|id| self.get(*id).lanes(self) > 1);
                if matches!(use_context, RepUse::Return)
                    || (splats
                        && (matches!(use_context, RepUse::TupleElement) || facts.tuple_spelled))
                {
                    return Ok(self.intern(
                        target,
                        use_context,
                        RepLayout::Tuple { slots, nullable: false },
                    ));
                }
                if guarantees.nobox && !facts.tuple_spelled {
                    return Ok(self.intern(target, use_context, RepLayout::Opaque));
                }
                Ok(self.intern(target, use_context, RepLayout::Opaque))
            }
            MonoTy::App { ctor: MonoCtor::Nominal(_), .. } => {
                let shape = if guarantees.nobox || tuple_element_shape {
                    crate::lower::mono_nobox_value_shape(ws, ty)
                } else {
                    crate::lower::mono_value_shape(ws, ty)
                };
                if let Some(shape) = shape {
                    let storage = mono_value_storage(ws, target, ty);
                    let column_eligible = column_eligible_for_shape(ws, ty, &shape);
                    return Ok(self.intern_with_facts(
                        target,
                        use_context,
                        RepLayout::Leaves(shape),
                        storage,
                        column_eligible,
                        declared_nobox(ws, ty),
                    ));
                }
                if guarantees.nobox && !guarantees.allow_opaque {
                    return Err(reject(format!(
                        "{} has no concrete nobox representation",
                        ty.render()
                    )));
                }
                Ok(self.intern(target, use_context, RepLayout::Opaque))
            }
            MonoTy::App { .. } => {
                if guarantees.nobox && !guarantees.allow_opaque {
                    return Err(reject(format!("{} cannot cross a nobox boundary", ty.render())));
                }
                Ok(self.intern(target, use_context, RepLayout::Opaque))
            }
            MonoTy::Int
            | MonoTy::Uint
            | MonoTy::Float
            | MonoTy::Bool
            | MonoTy::String
            | MonoTy::Any
            | MonoTy::Unit
            | MonoTy::Never
            | MonoTy::Function { .. } => {
                Ok(self.intern(target, use_context, RepLayout::Opaque))
            }
        }
    }

    pub fn function_value(
        &self,
        ws: &Workspace,
        target: FnBackend,
        ty: &crate::monoty::MonoTy,
    ) -> Result<FunctionRepPlan, RepError> {
        self.function_value_with_mode(ws, target, ty, true)
    }

    pub fn function_value_with_mode(
        &self,
        ws: &Workspace,
        target: FnBackend,
        ty: &crate::monoty::MonoTy,
        nobox_only: bool,
    ) -> Result<FunctionRepPlan, RepError> {
        let (ty, _) = ty.peel_nullable();
        let crate::monoty::MonoTy::Function { params, ret } = ty else {
            return Err(RepError {
                span: None,
                message: "a function-value representation requires a function type".into(),
            });
        };
        let params = params
            .iter()
            .map(|param| {
                let tuple_spelled = matches!(param.peel_nullable().0, crate::monoty::MonoTy::Tuple(_))
                    && self
                        .plan(
                            ws,
                            target,
                            param,
                            RepUse::Return,
                            &RepUseFacts::default(),
                            RepGuarantees { nobox: true, allow_opaque: true },
                        )
                        .map(|id| self.get(id).splats(self))
                        .unwrap_or(false);
                self.plan(
                    ws,
                    target,
                    param,
                    RepUse::Parameter,
                    &RepUseFacts { tuple_spelled, span: None },
                    RepGuarantees { nobox: nobox_only, allow_opaque: true },
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        let result = if matches!(ret.as_ref(), crate::monoty::MonoTy::Unit) {
            None
        } else {
            Some(self.plan(
                ws,
                target,
                ret,
                RepUse::Return,
                &RepUseFacts {
                    tuple_spelled: matches!(ret.peel_nullable().0, crate::monoty::MonoTy::Tuple(_)),
                    span: None,
                },
                RepGuarantees { nobox: nobox_only, allow_opaque: true },
            )?)
        };
        let scalar_abi = params.iter().any(|id| !matches!(self.get(*id).layout(), RepLayout::Opaque))
            || result.is_some_and(|id| matches!(self.get(id).layout(), RepLayout::Leaves(_)));
        let return_lanes = result.map_or(0, |id| self.get(id).lanes(self));
        Ok(FunctionRepPlan {
            params,
            result,
            return_lanes,
            scalar_abi,
        })
    }

    fn function_from_declaration(
        &self,
        ws: &Workspace,
        target: FnBackend,
        f: &Function,
        subst: &SpecSubst,
        mode: Option<AbiMode>,
    ) -> Result<FunctionRepPlan, RepError> {
        let mut params = Vec::with_capacity(f.param_decls.len());
        for param in &f.param_decls {
            let ty = declared_mono_type(ws, param.ty, subst)?;
            let id = match (mode, ty) {
                (Some(mode), Some(ty)) => self.plan(
                    ws,
                    target,
                    &ty,
                    RepUse::Parameter,
                    &RepUseFacts {
                        tuple_spelled: declared_tuple_spelled(ws, param.ty),
                        span: None,
                    },
                    RepGuarantees { nobox: mode.nobox_only, allow_opaque: true },
                )?,
                _ => self.intern(target, RepUse::Parameter, RepLayout::Opaque),
            };
            params.push(id);
        }

        let result = f.return_ty.map(|return_ty| {
            let ty = declared_mono_type(ws, return_ty, subst)?;
            if ty.as_ref().is_some_and(|ty| matches!(ty, crate::monoty::MonoTy::Unit)) {
                return Ok(None);
            }
            let tuple_spelled = declared_tuple_spelled(ws, return_ty);
            let id = if let Some(ty) = ty {
                let is_tuple = matches!(ty, crate::monoty::MonoTy::Tuple(_));
                if let Some(mode) = mode {
                    self.plan(
                        ws,
                        target,
                        &ty,
                        RepUse::Return,
                        &RepUseFacts { tuple_spelled: tuple_spelled || is_tuple, span: None },
                        RepGuarantees { nobox: mode.nobox_only, allow_opaque: true },
                    )?
                } else if is_tuple {
                    self.plan(
                        ws,
                        target,
                        &ty,
                        RepUse::Return,
                        &RepUseFacts { tuple_spelled: true, span: None },
                        RepGuarantees::default(),
                    )?
                } else {
                    self.intern(target, RepUse::Return, RepLayout::Opaque)
                }
            } else {
                self.intern(target, RepUse::Return, RepLayout::Opaque)
            };
            Ok(Some(id))
        }).transpose()?.flatten();
        let scalar_abi = mode.is_some()
            && (params
                .iter()
                .any(|id| matches!(self.get(*id).layout(), RepLayout::Leaves(_) | RepLayout::Tuple { .. }))
                || result.is_some_and(|id| matches!(self.get(id).layout(), RepLayout::Leaves(_))));
        let return_lanes = result.map_or(0, |id| self.get(id).lanes(self));
        Ok(FunctionRepPlan { params, result, return_lanes, scalar_abi })
    }

    pub(crate) fn boxed_function(
        &self,
        target: FnBackend,
        param_count: usize,
    ) -> FunctionRepPlan {
        let params = (0..param_count)
            .map(|_| self.intern(target, RepUse::Parameter, RepLayout::Opaque))
            .collect();
        let result = self.intern(target, RepUse::Return, RepLayout::Opaque);
        FunctionRepPlan { params, result: Some(result), return_lanes: 1, scalar_abi: false }
    }

    fn function_from_concrete(
        &self,
        ws: &Workspace,
        target: FnBackend,
        params: &[crate::monoty::MonoTy],
        result_ty: Option<&crate::monoty::MonoTy>,
        param_tuple_spelled: &[bool],
        mode: Option<AbiMode>,
    ) -> FunctionRepPlan {
        let params: Vec<RepId> = if let Some(mode) = mode {
            params
                .iter()
                .enumerate()
                .map(|(index, ty)| {
                    let tuple_spelled = *param_tuple_spelled
                        .get(index)
                        .expect("concrete signature is missing parameter ABI provenance");
                    self.plan(
                        ws,
                        target,
                        ty,
                        RepUse::Parameter,
                        &RepUseFacts { tuple_spelled, span: None },
                        RepGuarantees {
                            nobox: mode.nobox_only,
                            allow_opaque: true,
                        },
                    )
                    .expect("concrete ABI planner must accept a validated parameter")
                })
                .collect()
        } else {
            params
                .iter()
                .map(|_| self.intern(target, RepUse::Parameter, RepLayout::Opaque))
                .collect()
        };
        let result = result_ty.map(|ty| {
            let tuple_spelled = match ty {
                crate::monoty::MonoTy::Tuple(_) => true,
                crate::monoty::MonoTy::Nullable(inner) => {
                    matches!(inner.as_ref(), crate::monoty::MonoTy::Tuple(_))
                }
                _ => false,
            };
            if mode.is_some() || tuple_spelled {
                self.plan(
                    ws,
                    target,
                    ty,
                    RepUse::Return,
                    &RepUseFacts { tuple_spelled, span: None },
                    RepGuarantees {
                        nobox: mode.is_some_and(|mode| mode.nobox_only),
                        allow_opaque: true,
                    },
                )
                .expect("concrete ABI planner must accept a validated result")
            } else {
                self.intern(target, RepUse::Return, RepLayout::Opaque)
            }
        });
        let return_lanes = result.map_or(0, |id| self.get(id).lanes(self));
        let scalar_abi = mode.is_some()
            && (params
                .iter()
                .any(|id| !matches!(self.get(*id).layout(), RepLayout::Opaque))
                || result.is_some_and(|id| matches!(self.get(id).layout(), RepLayout::Leaves(_))));
        FunctionRepPlan {
            params,
            result,
            return_lanes,
            scalar_abi,
        }
    }
}

/// Whether a callee is a Rock body subject that the concrete registry must
/// contain. Foreign, compile-time expansion, and native-required intrinsic
/// declarations still have an ABI, but no body plan exists for them because
/// their backend or lowering arm supplies the body itself.
/// The ABI of an ALREADY-SELECTED owner instance -- its concrete plan when the
/// owner requires one, the opaque boundary otherwise. The one entry point for
/// consumers holding a resolved owner (bound rewrites, inline snapshots,
/// definition lowering); call-site resolution goes through
/// `Builder::callee_abi_plan_for_instance`, which also selects the backend
/// variant. Memoized under [`AbiPlanKey::Owner`].
pub fn owner_abi_plan(
    ws: &Workspace,
    scx: &ScalarCtx,
    plans: &crate::concrete::ConcretePlans,
    owner: &crate::instance::Instance,
) -> Result<BackendFnAbiPlanHandle, skull_parser::CompilerError> {
    owner_abi_plan_impl(ws, scx, plans, owner, None)
}

/// [`owner_abi_plan`] for the caller that already holds the owner's concrete
/// plan -- definition lowering, whose standalone-specialization plans are not
/// in the shared map. Same key, same memo, same derivation policy.
pub fn owner_abi_plan_for_body(
    ws: &Workspace,
    scx: &ScalarCtx,
    plans: &crate::concrete::ConcretePlans,
    owner: &crate::instance::Instance,
    body_plan: &ConcreteBodyPlan,
) -> Result<BackendFnAbiPlanHandle, skull_parser::CompilerError> {
    owner_abi_plan_impl(ws, scx, plans, owner, Some(body_plan))
}

fn owner_abi_plan_impl(
    ws: &Workspace,
    scx: &ScalarCtx,
    plans: &crate::concrete::ConcretePlans,
    owner: &crate::instance::Instance,
    in_hand: Option<&ConcreteBodyPlan>,
) -> Result<BackendFnAbiPlanHandle, skull_parser::CompilerError> {
    let key = AbiPlanKey::Owner(owner.clone());
    if let Some(plan) = scx.abi_plan_cached(&key) {
        return Ok(plan);
    }
    let derivation_start = abi_plan_timing_enabled().then(Instant::now);
    let derived = if !requires_concrete_body_plan(ws, owner) {
        BackendFnAbiPlan::for_boundary_instance(ws, scx, owner)
    } else {
        let plan = in_hand.or_else(|| plans.get(owner)).ok_or_else(|| {
            skull_parser::CompilerError::new_no_location(format!(
                "internal: concrete ABI plan is missing for owner `{}` ({})",
                ws.module(owner.func.module).functions[owner.func.func].name,
                owner.func.func.into_raw().into_u32(),
            ))
        })?;
        BackendFnAbiPlan::for_concrete_plan(ws, scx, plan)
    };
    let derived = Arc::new(derived);
    scx.abi_plan_record_derivation(derivation_start);
    scx.abi_plan_store(key, &derived);
    Ok(derived)
}

/// Resolve the declaration variant serving a requested call instance. JS keeps
/// host mappings that have no Rock declaration, so it deliberately retains the
/// requested instance for the emitter to handle later.
pub(super) fn selected_abi_instance(
    ws: &Workspace,
    scx: &ScalarCtx,
    inst: &crate::instance::Instance,
) -> Result<crate::instance::Instance, skull_parser::CompilerError> {
    match super::backend_variant_instance(ws, inst, scx.target) {
        Ok(selected) => Ok(selected),
        Err(_) if scx.target == FnBackend::Js => Ok(inst.clone()),
        Err(_) => Err(super::gap(format!(
            "callee `{}` has no body serving the {:?} target",
            ws.module(inst.func.module).functions[inst.func.func].name,
            scx.target,
        ))),
    }
}

/// The one memoized callee ABI question. It includes backend-variant selection,
/// polymorphic-recursion rejection, and the sibling-signature fallback, so a
/// warm lookup and a body-lowering lookup cannot silently ask different ABI
/// questions.
pub(super) fn callee_abi_plan_for_instance(
    ws: &Workspace,
    scx: &ScalarCtx,
    plans: &crate::concrete::ConcretePlans,
    inst: &crate::instance::Instance,
) -> Result<BackendFnAbiPlanHandle, skull_parser::CompilerError> {
    let key = AbiPlanKey::Callee(inst.clone());
    if let Some(plan) = scx.abi_plan_cached(&key) {
        return Ok(plan);
    }
    let derivation_start = abi_plan_timing_enabled().then(Instant::now);
    let selected = selected_abi_instance(ws, scx, inst)?;
    let derived = if !requires_concrete_body_plan(ws, inst) {
        Arc::new(BackendFnAbiPlan::for_boundary_instance(ws, scx, &selected))
    } else {
        if let Some(at) = crate::monoty::too_deep_to_instantiate(
            inst.arg_types(scx.interner()).iter(),
        ) {
            let f = &ws.module(inst.func.module).functions[inst.func.func];
            return Err(skull_parser::CompilerError::new_with_label(
                f.name_span,
                format!(
                    "polymorphic recursion: instantiating `{}` at `{}` exceeds the \
                     instantiation depth limit of {}. A recursive call whose type \
                     arguments GROW never terminates -- each call needs a new body -- so it \
                     is rejected rather than expanded. Same-instance recursion (type \
                     arguments unchanged) is unaffected.",
                    f.name,
                    at.render(),
                    crate::monoty::MAX_INSTANTIATION_DEPTH,
                ),
            ));
        }
        let plan = plans.get(inst).ok_or_else(|| {
            let name = ws.module(inst.func.module).functions[inst.func.func].name;
            super::gap(format!(
                "concrete ABI plan is missing for callee `{name}` ({})",
                inst.func.func.into_raw().into_u32()
            ))
        })?;
        if selected.func != inst.func {
            selected_sibling_abi_plan(ws, scx, plans, &selected, plan)
        } else {
            Arc::new(BackendFnAbiPlan::for_concrete_plan(ws, scx, plan))
        }
    };
    scx.abi_plan_record_derivation(derivation_start);
    scx.abi_plan_store(key, &derived);
    Ok(derived)
}

/// ABI of the target-selected SIBLING body serving a canonical instance: the
/// sibling's own concrete plan when the registry has one, the canonical plan's
/// signature under the sibling's declaration otherwise, and the opaque
/// boundary when the sibling needs no body plan. Shared by call-site
/// resolution (`Builder::callee_abi_plan_for_instance`'s selected arm) and
/// the JS emitter's scalar-ABI check. NOT memoized here: the signature rung's
/// answer depends on the canonical plan, which no single-instance key names --
/// the Builder memoizes the whole callee question under `Callee(inst)`, where
/// the canonical identity is part of the key.
pub fn selected_sibling_abi_plan(
    ws: &Workspace,
    scx: &ScalarCtx,
    plans: &crate::concrete::ConcretePlans,
    selected: &crate::instance::Instance,
    canonical: &ConcreteBodyPlan,
) -> BackendFnAbiPlanHandle {
    if !requires_concrete_body_plan(ws, selected) {
        return Arc::new(BackendFnAbiPlan::for_boundary_instance(ws, scx, selected));
    }
    if let Some(selected_plan) = plans.get(selected) {
        return Arc::new(BackendFnAbiPlan::for_concrete_plan(ws, scx, selected_plan));
    }
    Arc::new(BackendFnAbiPlan::for_concrete_signature(ws, scx, selected, &canonical.signature))
}

pub fn requires_concrete_body_plan(
    ws: &Workspace,
    inst: &crate::instance::Instance,
) -> bool {
    let f = &ws.module(inst.func.module).functions[inst.func.func];
    !f.is_extern
        && f.inline_body.is_none()
        && crate::intrinsics::expand_kind(f).is_none()
        && !(crate::intrinsics::is_intrinsic(f) && crate::intrinsics::is_native_required(f))
        && ws
            .module(inst.func.module)
            .bodies
            .contains_key(&bone_semantic::hir::BodyOwner::Function(inst.func.func))
}

#[derive(Clone, Copy, Debug)]
struct AbiMode {
    nobox_only: bool,
}

fn declared_tuple_spelled(ws: &Workspace, ty: bone_semantic::prelude::TyId) -> bool {
    match super::static_ty(ws, ty) {
        Some(bone_semantic::hir::TypeExpr::Tuple { .. }) => true,
        Some(bone_semantic::hir::TypeExpr::Nullable(inner)) => matches!(
            super::static_ty(ws, ty.with_idx(inner)),
            Some(bone_semantic::hir::TypeExpr::Tuple { .. })
        ),
        _ => false,
    }
}

fn declared_mono_type(
    ws: &Workspace,
    ty: bone_semantic::prelude::TyId,
    subst: &SpecSubst,
) -> Result<Option<crate::monoty::MonoTy>, RepError> {
    match crate::monoty::convert_decl(ws, ty, subst) {
        Ok(ty) => Ok(Some(ty)),
        Err(crate::monoty::MonoTyReason::UnboundGeneric(_)) => Ok(None),
        Err(reason) => Err(RepError {
            span: None,
            message: format!("declaration type cannot be planned: {reason:?}"),
        }),
    }
}

/// The one ABI policy. PRIVATE: every consumer reads a [`BackendFnAbiPlan`].
///
/// Address-taken Rock bodies narrow to the `nobox` shapes an indirect caller can
/// derive from the fn type; direct-only bodies may scalarize every eligible value.
fn abi_mode_spec(
    ws: &Workspace,
    scx: &ScalarCtx,
    fref: FuncRef,
    subst: &SpecSubst,
) -> Option<AbiMode> {
    if scx.values_first_class {
        return None;
    }
    let f = &ws.module(fref.module).functions[fref.func];
    if f.is_extern {
        return None;
    }
    let has_scalar_abi = f.annotations.iter().any(|a| a.name.name.as_str() == "scalar_abi");
    let nobox_only = if has_scalar_abi {
        false
    } else {
        let name_is_fixed = f.annotations.iter().any(|a| match a.name.name.as_str() {
            "nomangle" => subst.is_empty(),
            "test" => true,
            _ => false,
        });
        if f.inline_body.is_some() || f.name.as_str() == "main" || name_is_fixed {
            return None;
        }
        scx.taken_by_ref.contains(&f.name)
    };
    Some(AbiMode { nobox_only })
}

/// **The backend ABI of one emitted body**, under one execution target.
///
/// Built from a declaration and the substitution its [`AbiSubject`] resolves
/// to. The substitution is what makes a per-body answer different from a
/// per-declaration one rather than merely more precise: `T` is one boxed slot
/// on the declaration and two lanes at `T = (bool, float)`.
///
/// The derived spellings are METHODS, not fields, so the ordinary lowering path
/// -- which needs only the wire shapes -- never pays for the leaf-name strings
/// that exist for foreign bodies.
#[derive(Clone, Debug)]
pub struct BackendFnAbiPlan {
    pub subject: crate::instance::Instance,
    /// The codegen backend this plan was built for. Part of the key: the same
    /// body has a different ABI under `values_first_class` (GLSL) than under
    /// luajit, and `result_lanes` is a JS transport fact.
    pub target: FnBackend,
    /// The physical function signature. Every call, entry binder, return
    /// lowerer, and publisher keeps these identities rather than rebuilding a
    /// shape from the declaration.
    representation: FunctionRepPlan,
    arena: Arc<RepArena>,
}

/// Shared immutable ABI plan returned by cache-eligible consumers. The value
/// constructors remain owned because boundary-only callers do not enter the
/// build memo.
pub type BackendFnAbiPlanHandle = Arc<BackendFnAbiPlan>;

impl PartialEq for BackendFnAbiPlan {
    fn eq(&self, other: &Self) -> bool {
        self.subject == other.subject
            && self.target == other.target
            && self.representation == other.representation
            && self.arena.equivalent(&other.arena)
    }
}

impl Eq for BackendFnAbiPlan {}

impl BackendFnAbiPlan {
    /// ABI for an explicit opaque boundary instance.
    ///
    /// A boundary has no `ConcreteBodyPlan`, but its target-specific transport
    /// still belongs to the same RepPlan arena. Keeping this entry named for the
    /// boundary makes it impossible to mistake declaration substitution for a
    /// Rock-body planning path.
    pub fn for_boundary_instance(
        ws: &Workspace,
        scx: &ScalarCtx,
        inst: &crate::instance::Instance,
    ) -> Self {
        let subst = inst.subst(ws, scx.interner());
        Self::assemble(ws, scx, inst.clone(), &subst)
    }

    /// Build the ABI from the graph-owned concrete signature. No declaration
    /// substitution is reconstructed on this path.
    pub fn for_concrete_plan(
        ws: &Workspace,
        scx: &ScalarCtx,
        plan: &ConcreteBodyPlan,
    ) -> Self {
        let inst = plan
            .function_instance()
            .expect("a function ABI plan needs an ordinary concrete owner");
        Self::for_concrete_signature(ws, scx, inst, &plan.signature)
    }

    /// Build an ABI from a concrete signature owned by a target-selected
    /// declaration. This covers backend siblings whose body is selected by the
    /// artifact but whose logical signature was recorded on the canonical graph
    /// instance.
    pub fn for_concrete_signature(
        ws: &Workspace,
        scx: &ScalarCtx,
        inst: &crate::instance::Instance,
        signature: &ConcreteSignature,
    ) -> Self {
        let subst = inst.subst(ws, scx.interner());
        let mode = abi_mode_spec(ws, scx, inst.func, &subst);
        let params = signature
            .params
            .iter()
            .map(|id| scx.interner().get(*id))
            .collect::<Vec<_>>();
        let result_ty = signature
            .result
            .map(|id| scx.interner().get(id))
            .filter(|ty| !matches!(ty, crate::monoty::MonoTy::Unit));
        let arena = scx.rep_arena();
        let representation = arena.function_from_concrete(
            ws,
            scx.target,
            &params,
            result_ty.as_ref(),
            &signature.param_tuple_spelled,
            mode,
        );
        Self {
            subject: inst.clone(),
            target: scx.target,
            representation,
            arena,
        }
    }

    /// ABI for a declaration-owned opaque boundary with no instance arguments.
    pub fn for_boundary_declaration(ws: &Workspace, scx: &ScalarCtx, decl: FuncRef) -> Self {
        Self::for_boundary_instance(ws, scx, &crate::instance::Instance::base(decl))
    }

    pub fn for_function_value(
        ws: &Workspace,
        scx: &ScalarCtx,
        ty: &crate::monoty::MonoTy,
    ) -> Result<FunctionRepPlan, RepError> {
        scx.rep_arena().function_value(ws, scx.target, ty)
    }

    pub fn for_function_value_with_mode(
        ws: &Workspace,
        scx: &ScalarCtx,
        ty: &crate::monoty::MonoTy,
        nobox_only: bool,
    ) -> Result<FunctionRepPlan, RepError> {
        scx.rep_arena().function_value_with_mode(ws, scx.target, ty, nobox_only)
    }

    fn assemble(
        ws: &Workspace,
        scx: &ScalarCtx,
        subject: crate::instance::Instance,
        subst: &SpecSubst,
    ) -> Self {
        let decl = subject.func;
        let f = &ws.module(decl.module).functions[decl.func];
        let arena = scx.rep_arena();
        let mode = abi_mode_spec(ws, scx, decl, subst);
        let representation = arena
            .function_from_declaration(ws, scx.target, f, subst, mode)
            .expect("declaration ABI planning must accept a checked signature");
        Self {
            subject,
            target: scx.target,
            representation,
            arena,
        }
    }

    pub fn decl(&self) -> FuncRef {
        self.subject.func
    }

    /// The scalarized wire, or `None` for the boxed ABI.
    pub(super) fn uses_scalar_abi(&self) -> bool {
        self.representation.uses_scalar_abi()
    }

    pub fn representation(&self) -> &FunctionRepPlan {
        &self.representation
    }

    pub fn rep_plan(&self, id: RepId) -> RepPlan {
        self.arena.get(id)
    }

    /// Per declared param, what it occupies on the wire. Empty when this body
    /// keeps the boxed ABI, which is what the lowering path already treats as
    /// "no splatting".
    pub(super) fn param_shapes(&self) -> Vec<RetSlot> {
        if !self.uses_scalar_abi() {
            return Vec::new();
        }
        self.representation
            .params()
            .iter()
            .map(|id| self.arena.ret_slot(*id))
            .collect()
    }

    /// The per-declared-param wire slots, or `None` under the boxed ABI --
    /// distinct from [`Self::param_shapes`], which flattens the distinction
    /// away for the lowering path that treats both as "no splatting".
    pub(super) fn ret_shape(&self) -> Option<ValueShape> {
        self.representation.result().and_then(|id| self.arena.value_shape(id))
    }

    /// The declared return's tuple shape under this body's substitution.
    pub(crate) fn ret_tuple(&self) -> Option<TupleRetShape> {
        self.representation.result().and_then(|id| self.arena.tuple_shape(id))
    }

    /// Physical values on the result wire: 0 with no return, 1 for an ordinary
    /// one, more for the two shapes that splat -- a tuple return (each element
    /// its own slot, a `nobox` element into its leaves) and a bare `nobox`
    /// value return under the scalar ABI.
    ///
    /// **A JS transport fact.** Plan S 13b: leaf 0 rides the ordinary JS
    /// `return`, lanes 1..N ride the module-scope scratch bank
    /// (`docs/boundary-contracts.md` 1.2). Luajit has native multi-return and
    /// consults nothing here; the artifact view still records the width for
    /// every artifact, because it is a property of the wire rather than of the
    /// backend that happens to need it spelled out.
    pub fn result_lanes(&self) -> usize {
        self.representation.return_lanes()
    }

    /// Depth-first leaf names for a scalar-ABI FOREIGN body (`@scalar_abi`):
    /// each value param expands to `<base>__<leaf>`, the names the hand-written
    /// body references; other params keep their name. `None` when this body
    /// does not use the scalar ABI, and the caller emits the plain list. The
    /// leaf ORDER matches `repr_to_leaves`, so def params line up with the
    /// callsite splat positionally.
    pub fn lua_param_names(&self, ws: &Workspace) -> Option<Vec<String>> {
        if !self.uses_scalar_abi() {
            return None;
        }
        let shapes = self.param_shapes();
        let f = &ws.module(self.decl().module).functions[self.decl().func];
        let mut names = Vec::new();
        for (pd, slot) in f.param_decls.iter().zip(shapes.iter()) {
            slot_param_names(pd.name.as_str(), slot, &mut names);
        }
        Some(names)
    }

    /// Per-DECLARED-param leaf width (a value-struct param's flattened leaf
    /// count, 1 otherwise). Per declared param rather than per slot so a
    /// defaulted value-struct param counts as its whole leaf group.
    pub fn param_leaf_widths(&self) -> Option<Vec<usize>> {
        self.uses_scalar_abi()
            .then(|| self.param_shapes().iter().map(RetSlot::slot_count).collect())
    }
}

#[cfg(test)]
mod rep_read_cache_tests {
    use super::*;
    use bone_semantic::hir::FnBackend;

    /// Two live arenas mint the same numeric ids for different plans; the
    /// worker-local cache must never serve one arena's plan for the other's id.
    #[test]
    fn worker_local_reads_stay_arena_scoped() {
        let a = RepArena::default();
        let b = RepArena::default();
        let ia = a.intern(FnBackend::Lua, RepUse::Return, RepLayout::Opaque);
        let ib = b.intern(
            FnBackend::Lua,
            RepUse::Return,
            RepLayout::Foreign { reason: Ustr::from("test") },
        );
        assert_eq!(ia, ib, "both arenas must mint id 0 for the fixture to discriminate");
        for _ in 0..2 {
            assert!(matches!(a.get(ia).layout(), RepLayout::Opaque));
            assert!(matches!(b.get(ib).layout(), RepLayout::Foreign { .. }));
        }
        let ib2 = b.intern(FnBackend::Lua, RepUse::Return, RepLayout::Opaque);
        assert_ne!(ib, ib2, "b's opaque return must not be served a's local mapping");
    }

    /// An insert AFTER this thread warmed its local cache must still be
    /// readable -- a cold id reads through to the shared arena.
    #[test]
    fn a_post_warm_insert_reads_through() {
        let a = RepArena::default();
        let first = a.intern(FnBackend::Lua, RepUse::Return, RepLayout::Opaque);
        let _ = a.get(first);
        let second = a.intern(FnBackend::Lua, RepUse::Parameter, RepLayout::Opaque);
        assert_ne!(first, second);
        assert_eq!(a.get(second).use_context(), RepUse::Parameter);
        assert_eq!(a.get(second).id(), second);
    }
}
