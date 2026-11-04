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

use bone_core::prelude::Instant;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use bone_semantic::hir::{FnBackend, Function};
use bone_semantic::inference::Workspace;
use bone_semantic::prelude::Span;
use ustr::Ustr;

use super::{
    FieldShape, RetSlot, ScalarCtx, SpecSubst, TupleRetShape, ValueShape, ValueStorage,
    slot_param_names,
};
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
    if f.annotations.iter().any(|a| a.is_builtin("intrinsic")) {
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
            !f.annotations.iter().any(|a| a.is_builtin("scalar_abi"))
        }
        _ => false,
    }
}

/// An interned physical representation.  Its numeric identity is meaningful
/// only through the build-scoped arena carried by [`ScalarCtx`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RepId {
    arena: u64,
    index: u32,
}

impl RepId {
    pub fn raw(self) -> u32 {
        self.index
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
        Self {
            tuple_spelled: false,
            span: None,
        }
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

#[derive(Clone, Debug, PartialEq, Eq)]
enum RepPlanLayout {
    Inline(RepLayout),
    Shared(Arc<RepLayout>),
}

impl RepPlanLayout {
    fn new(layout: RepLayout) -> Self {
        match layout {
            RepLayout::Leaves(_) | RepLayout::Tuple { .. } => Self::Shared(Arc::new(layout)),
            // Scalar plans have no allocation to share or refcount to update.
            RepLayout::Opaque | RepLayout::Foreign { .. } => Self::Inline(layout),
        }
    }

    fn as_layout(&self) -> &RepLayout {
        match self {
            Self::Inline(layout) => layout,
            Self::Shared(layout) => layout,
        }
    }
}

/// One concrete physical answer in the target/use context supplied by the
/// planner.  Backends may inspect it, but they do not derive another answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepPlan {
    id: RepId,
    target: FnBackend,
    use_context: RepUse,
    layout: RepPlanLayout,
    storage: Option<ValueStorage>,
    column_eligible: bool,
    nobox: bool,
    /// Whether the lane budget may replace this plan with its boxed form.
    /// False when boxing would IMPLICITLY materialize a `nobox` child
    /// ([`box_would_materialize_nobox`]), which the `nobox` guarantee forbids.
    /// Derived once, from the concrete type being planned, and retained here so
    /// that budgeting reads the planner's own answer instead of a parallel
    /// per-signature flag list.
    boxable: bool,
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
        self.layout.as_layout()
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

    /// Whether boxing this value is legal at all: an ordinary value whose box
    /// would materialize a `nobox` descendant is not, so the lane budget must
    /// leave it on the scalar wire. A `nobox` plan is already excluded by
    /// [`RepPlan::nobox`], so this fact does not repeat that guarantee.
    pub fn boxable(&self) -> bool {
        self.boxable
    }

    pub fn leaf_paths(&self) -> Option<Vec<Ustr>> {
        match self.layout() {
            RepLayout::Leaves(shape) => Some(crate::lower::value_leaf_paths(shape)),
            RepLayout::Opaque | RepLayout::Tuple { .. } | RepLayout::Foreign { .. } => None,
        }
    }

    pub fn presence_flags(&self) -> Option<Vec<bool>> {
        match self.layout() {
            RepLayout::Leaves(shape) => Some(crate::lower::value_leaf_presence_flags(shape)),
            RepLayout::Opaque | RepLayout::Tuple { .. } | RepLayout::Foreign { .. } => None,
        }
    }

    pub fn lanes(&self, arena: &RepArena) -> usize {
        match self.layout() {
            RepLayout::Opaque | RepLayout::Foreign { .. } => 1,
            RepLayout::Leaves(shape) => shape.leaf_count(),
            RepLayout::Tuple { slots, nullable } => {
                slots
                    .iter()
                    .map(|slot| arena.get(*slot).lanes(arena))
                    .sum::<usize>()
                    + usize::from(*nullable)
            }
        }
    }

    pub(crate) fn splats(&self, arena: &RepArena) -> bool {
        match self.layout() {
            RepLayout::Leaves(_) => true,
            RepLayout::Tuple { slots, .. } => {
                slots.iter().any(|slot| arena.get(*slot).splats(arena))
            }
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
        let ty = crate::monoty::convert_decl(ws, ty, &SpecSubst::default()).map_err(|reason| {
            RepError {
                span: None,
                message: format!("global type cannot be planned: {reason:?}"),
            }
        })?;
        Self::for_mono_type(ws, scx, ty)
    }

    fn for_mono_type(
        ws: &Workspace,
        scx: &ScalarCtx,
        ty: crate::monoty::MonoTy,
    ) -> Result<Self, RepError> {
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
                RepGuarantees {
                    nobox: true,
                    allow_opaque: true,
                },
            )?
        };
        Ok(Self {
            representation,
            arena,
        })
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

    /// A build-independent description of the stored global representation.
    /// `RepId` and the arena allocation order are deliberately absent: this is
    /// the logical layout fingerprint, not a backend publication name.
    pub fn logical_fingerprint(&self) -> String {
        rep_logical_fingerprint(&self.arena, self.representation)
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

    /// A build-independent description of the function-value wire. `RepId`s
    /// are only lookup handles into `arena`; the returned string contains the
    /// recursive layouts they name, so a later artifact can compare it without
    /// depending on arena allocation order.
    pub fn logical_fingerprint(&self, arena: &RepArena) -> String {
        let params = self
            .params
            .iter()
            .map(|id| rep_logical_fingerprint(arena, *id))
            .collect::<Vec<_>>()
            .join(",");
        let result = self
            .result
            .map(|id| rep_logical_fingerprint(arena, id))
            .unwrap_or_else(|| "none".to_string());
        format!(
            "params=[{params}];result={result};lanes={};scalar={}",
            self.return_lanes, self.scalar_abi
        )
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
    *ENABLED.get_or_init(|| std::env::var("ROCK_LOG").is_ok_and(|value| value == "debug"))
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
        for (dst, src) in self
            .plan_size_histogram
            .iter_mut()
            .zip(other.plan_size_histogram)
        {
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

impl Clone for AbiPlanCache {
    fn clone(&self) -> Self {
        use std::sync::atomic::Ordering;

        let copy = |value: &std::sync::atomic::AtomicU64| {
            std::sync::atomic::AtomicU64::new(value.load(Ordering::Relaxed))
        };
        Self {
            plans: Mutex::new(self.plans.lock().expect("abi plan cache poisoned").clone()),
            published: self.published.clone(),
            epoch: copy(&self.epoch),
            owner_hits: copy(&self.owner_hits),
            owner_misses: copy(&self.owner_misses),
            local_hits: copy(&self.local_hits),
            callee_hits: copy(&self.callee_hits),
            callee_misses: copy(&self.callee_misses),
            stores: copy(&self.stores),
            duplicate_cold_stores: copy(&self.duplicate_cold_stores),
            cloned_param_slots: copy(&self.cloned_param_slots),
            shared_lookups: copy(&self.shared_lookups),
            shared_hits: copy(&self.shared_hits),
            shared_misses: copy(&self.shared_misses),
            plan_clones: copy(&self.plan_clones),
            plan_clone_param_slots: copy(&self.plan_clone_param_slots),
            shared_lock_ns: copy(&self.shared_lock_ns),
            local_clone_ns: copy(&self.local_clone_ns),
            shared_clone_ns: copy(&self.shared_clone_ns),
            derivation_ns: copy(&self.derivation_ns),
            warm_keys: copy(&self.warm_keys),
            warm_failures: copy(&self.warm_failures),
            published_hits: copy(&self.published_hits),
            published_misses: copy(&self.published_misses),
            published_keys: copy(&self.published_keys),
            plan_size_histogram: std::array::from_fn(|i| copy(&self.plan_size_histogram[i])),
            plan_size_max: copy(&self.plan_size_max),
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
        let replaced = self
            .plans
            .lock()
            .expect("abi plan cache poisoned")
            .insert(key.clone(), Arc::clone(plan));
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
        self.epoch
            .store(next_abi_epoch(), std::sync::atomic::Ordering::Release);
        self.plans.lock().expect("abi plan cache poisoned").clear();
        self.published = None;
        self.warm_keys
            .store(0, std::sync::atomic::Ordering::Relaxed);
        self.published_keys
            .store(0, std::sync::atomic::Ordering::Relaxed);
        self.warm_failures
            .store(0, std::sync::atomic::Ordering::Relaxed);
    }

    pub(super) fn publish(&mut self, warm_keys: u64, warm_failures: u64) {
        let plans = std::mem::take(&mut *self.plans.lock().expect("abi plan cache poisoned"));
        self.published_keys
            .store(plans.len() as u64, std::sync::atomic::Ordering::Relaxed);
        self.warm_keys
            .store(warm_keys, std::sync::atomic::Ordering::Relaxed);
        self.warm_failures
            .store(warm_failures, std::sync::atomic::Ordering::Relaxed);
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
            unique_keys: self
                .published
                .as_ref()
                .map_or(0, |plans| plans.len() as u64)
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

#[derive(Debug, Default)]
struct RepArenaInner {
    plans: Vec<RepPlan>,
    by_key: HashMap<RepKey, RepId>,
}

impl PartialEq for RepArenaInner {
    fn eq(&self, other: &Self) -> bool {
        // Input aliases are lookup history, not additional physical plans.
        self.plans == other.plans
    }
}

impl Eq for RepArenaInner {}

/// The one build-scoped physical representation arena.  `ScalarCtx` owns the
/// handle and every ABI plan references the same arena, so equal answers are
/// equal identities rather than merely equal cloned shapes.
///
/// The counters ride outside `inner` so [`RepArenaInner`]'s plan equality
/// (the [`BackendFnAbiPlan`] identity check) never compares them.
///
/// READS ARE SERVED THROUGH A WORKER-LOCAL CACHE. The arena is monotonic
/// (plans push-only, `by_key` insert-only, nothing published ever changes), so
/// a thread may keep private copies of the `(RepId -> RepPlan)` and
/// `(key -> RepId)` facts it has already paid the mutex for; a cold id or key
/// reads through to the lock. Aggregate layout payloads are immutable handles,
/// so plan reads do not recursively copy their fields or tuple slots.
/// `arena_gen` is a process-unique id keying that
/// cache, so two live arenas (the Lua and shader contexts) or an arena
/// allocated at a reused address can never serve each other's plans.
#[derive(Debug)]
pub struct RepArena {
    inner: Mutex<RepArenaInner>,
    arena_id: u64,
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
            arena_id: ARENA_GEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            get_calls: Default::default(),
            local_get_hits: Default::default(),
            intern_attempts: Default::default(),
            local_intern_hits: Default::default(),
            intern_new: Default::default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum RepKey {
    Layout(
        FnBackend,
        RepUse,
        RepLayout,
        Option<ValueStorage>,
        bool,
        bool,
    ),
    Concrete {
        target: FnBackend,
        use_context: RepUse,
        ty: crate::monoty::MonoTy,
        tuple_spelled: bool,
        nobox: bool,
        allow_opaque: bool,
    },
    ValueShape(FnBackend, RepUse, ValueShape),
}

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
        let idx = id.index as usize;
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
/// remainder paid a lock acquisition.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RepArenaStats {
    pub get_calls: u64,
    pub local_get_hits: u64,
    pub intern_attempts: u64,
    pub local_intern_hits: u64,
    pub intern_new: u64,
    pub plans: u64,
}

impl PartialEq for RepArena {
    fn eq(&self, other: &Self) -> bool {
        self.equivalent(other)
    }
}

impl Eq for RepArena {}

fn value_shape_facts(
    ws: &Workspace,
    ty: &crate::monoty::MonoTy,
    shape: &ValueShape,
) -> (Option<ValueStorage>, bool, bool) {
    let (payload, _) = ty.peel_nullable();
    let crate::monoty::MonoTy::App {
        ctor: crate::monoty::MonoCtor::Nominal(name),
        ..
    } = payload
    else {
        return (None, false, true);
    };
    assert_eq!(
        *name, shape.name,
        "value shape must describe its nominal type"
    );
    let Some(plan) = super::shape::planned_value(ws, *name) else {
        return (None, false, true);
    };
    let column_eligible = ws.struct_by_name.get(name).is_some_and(|sid| {
        super::shape::has_value_plan(&ws.modules[sid.module.index()].structs[sid.def])
    }) && (plan.shape.is_some()
        || (declared_nobox(ws, ty) && plan.gc_shape.is_some()));
    (
        Some(plan.storage),
        column_eligible,
        !plan_box_materializes_nobox(ws, &plan),
    )
}

/// The Lua wide-value calling convention. An ORDINARY `value`
/// whose PAYLOAD leaf count is greater than this crosses a Rock-to-Rock Lua
/// call as ONE slot -- its existing boxed form -- instead of N scalar lanes.
///
/// The PAYLOAD count is compared, not [`ValueShape::leaf_count`], which also
/// counts the nullable presence flag, so `T` and `T?` take the same wire
/// instead of `T?` boxing one leaf sooner than `T`.
///
/// `nobox` values are exempt (decision 3): they keep scalar lanes at any width.
///
/// 48 sits ABOVE every ordinary `value` that crosses a call in a game today
/// (the widest is `EngineU` at 41), and below the 58-leaf uniform this ABI
/// exists to make loadable. That placement is deliberate, not incidental:
/// LuaJIT sinks a box of 37 leaves or more not at all, so any per-frame type
/// this cut boxed would pay one real allocation per crossing, and the defect
/// being fixed is a load-time frame overflow, not an allocation. A cut that
/// boxes nothing a game passes fixes it for free.
///
/// It also pairs exactly with [`SIGNATURE_SCALAR_LANE_BUDGET`]: a signature
/// that passes and returns one value AT the cut is `2 * K` = 96 lanes, which
/// the budget permits, so the two rules meet at the boundary instead of
/// overlapping.
pub const WIDE_VALUE_PAYLOAD_LEAF_CUT: usize = 48;

/// The per-signature lane budget. Per-value `K` does not bound a signature's
/// width -- five values at the cut are `5 * K` lanes with every one of them
/// legal -- so after the per-value cut a signature whose surviving lanes exceed this boxes
/// its widest remaining ORDINARY values, widest first, until it fits.
///
/// Lane accounting: the sum over every parameter of that parameter's
/// [`RepPlan::lanes`] plus the result's, each at its post-cut width. A boxed
/// value is one lane; a `nobox` value's real lanes count, presence flags
/// included.
///
/// 96 is `2 * WIDE_VALUE_PAYLOAD_LEAF_CUT`, so one value at the cut passed and
/// returned is exactly at the budget rather than over it. Worst case a call
/// that passes and receives a full budget costs 2 x 96 + 2 = 194 of LuaJIT's
/// 250 frame slots; the emit-time frame-limit check is the backstop for a body
/// that still does not fit, which is what makes a generous budget safe.
///
/// `nobox` values are never candidates and are never rejected by the budget: a
/// signature whose `nobox` lanes alone exceed it keeps them, and the emit-time
/// frame-limit check (`skull_codegen_lua::frame_limits`) is its only diagnostic.
pub const SIGNATURE_SCALAR_LANE_BUDGET: usize = 96;

/// The leaf count the per-value cut compares: the payload, without the nullable
/// presence flag [`ValueShape::leaf_count`] includes.
fn payload_leaf_count(shape: &ValueShape) -> usize {
    shape.leaf_count() - usize::from(shape.nullable)
}

/// Whether boxing `ty` would IMPLICITLY materialize a `nobox` child, which the
/// `nobox` guarantee forbids.
///
/// Two of the three value tiers have an answer already. A CDATA box is
/// normalized by `flatten_nobox_nested_ctors` (`crates/skull-transform`), which
/// splices a `nobox` child's leaves into the parent's physical fields so the
/// child is never built; a LEAF-TABLE box is assembled from the parent's leaves
/// by `pack` for the same reason. A GC-TABLE box keeps its DECLARED keys, so a
/// `nobox` child inside one is a real box and
/// `nobox_materialization_violations` rejects it at its own span.
///
/// The ABI cut therefore leaves a GC-tier value carrying a `nobox` descendant
/// on the scalar wire. Its bodies keep today's frame cost and today's spanned
/// frame-limit diagnostic; what they do not get is the new wire.
fn box_would_materialize_nobox(ws: &Workspace, ty: &crate::monoty::MonoTy) -> bool {
    let crate::monoty::MonoTy::App {
        ctor: crate::monoty::MonoCtor::Nominal(name),
        ..
    } = ty.peel_nullable().0
    else {
        return false;
    };
    super::shape::planned_value(ws, *name)
        .is_some_and(|plan| plan_box_materializes_nobox(ws, &plan))
}

fn plan_box_materializes_nobox(ws: &Workspace, plan: &super::shape::PlannedValue) -> bool {
    use super::shape::PlannedField;
    if plan.storage != ValueStorage::GcTable {
        return false;
    }
    plan.fields.iter().any(|(_, field)| match field {
        PlannedField::Nested(child) | PlannedField::Nullable(child) => {
            crate::lower::is_nobox_nominal(ws, child.name) || plan_box_materializes_nobox(ws, child)
        }
        PlannedField::Primitive(_) | PlannedField::FlatEnum(_) | PlannedField::Opaque => false,
    })
}

/// **The one per-value wire choice**, consulted by BOTH ABI modes.
/// `Some(shape)` is the scalar wire; `None` is the aggregate wire, which the
/// caller renders as the same [`RepLayout::Opaque`] the boxed ABI already uses.
///
/// - A `nobox` value is never boxed, in any mode, at any width (decision 3).
/// - An ordinary `value` is boxed when the mode demands it -- `nobox_only`
///   (a taken-by-ref body, whose indirect callers can only derive `nobox`
///   shapes from the fn type) or a tuple element -- and, on a direct call
///   wire, when its payload is wider than [`WIDE_VALUE_PAYLOAD_LEAF_CUT`].
/// - Off the call wire (a global's storage, a container element) width decides
///   nothing: those are not frames.
///
/// Boxing an ordinary value is SILENT (decision 3). There is no shape the wire
/// cannot handle: a value either has a planned leaf shape or is already opaque.
fn wire_value_shape(
    ws: &Workspace,
    ty: &crate::monoty::MonoTy,
    use_context: RepUse,
    guarantees: RepGuarantees,
    tuple_element: bool,
) -> Option<ValueShape> {
    if let Some(shape) = crate::lower::mono_nobox_value_shape(ws, ty) {
        return Some(shape);
    }
    if guarantees.nobox || tuple_element {
        return None;
    }
    let shape = crate::lower::mono_value_shape(ws, ty)?;
    if matches!(use_context, RepUse::Parameter | RepUse::Return)
        && payload_leaf_count(&shape) > WIDE_VALUE_PAYLOAD_LEAF_CUT
        && !box_would_materialize_nobox(ws, ty)
    {
        return None;
    }
    Some(shape)
}

fn declared_nobox(ws: &Workspace, ty: &crate::monoty::MonoTy) -> bool {
    let (payload, _) = ty.peel_nullable();
    let crate::monoty::MonoTy::App {
        ctor: crate::monoty::MonoCtor::Nominal(name),
        ..
    } = payload
    else {
        return false;
    };
    crate::lower::is_nobox_nominal(ws, *name)
}

impl RepArena {
    fn lookup_key(&self, key: &RepKey) -> Option<RepId> {
        REP_READ_CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();
            cache.reset_for(self.arena_id);
            if let Some(id) = cache.by_key.get(key) {
                return Some(*id);
            }
            let id = self
                .inner
                .lock()
                .expect("RepArena lock poisoned")
                .by_key
                .get(key)
                .copied();
            if let Some(id) = id {
                cache.by_key.insert(key.clone(), id);
            }
            id
        })
    }

    fn alias_key(&self, key: RepKey, id: RepId) {
        self.inner
            .lock()
            .expect("RepArena lock poisoned")
            .by_key
            .insert(key.clone(), id);
        REP_READ_CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();
            cache.reset_for(self.arena_id);
            cache.by_key.insert(key, id);
        });
    }

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
        // Production mints a `Leaves` plan only through
        // [`Self::intern_value_shape_plan`], which derives boxing legality from
        // the concrete type; every other layout is not a lane-budget candidate,
        // so the fact is vacuous here.
        self.intern_with_facts(target, use_context, layout, storage, false, false, true)
    }

    fn intern_with_facts(
        &self,
        target: FnBackend,
        use_context: RepUse,
        layout: RepLayout,
        storage: Option<ValueStorage>,
        column_eligible: bool,
        nobox: bool,
        boxable: bool,
    ) -> RepId {
        self.intern_attempts
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let key = RepKey::Layout(target, use_context, layout, storage, column_eligible, nobox);
        REP_READ_CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();
            cache.reset_for(self.arena_id);
            if let Some(id) = cache.by_key.get(&key) {
                self.local_intern_hits
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return *id;
            }
            let id = {
                let mut inner = self.inner.lock().expect("RepArena lock poisoned");
                match inner.by_key.get(&key) {
                    Some(id) => *id,
                    None => {
                        self.intern_new
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let id = RepId {
                            arena: self.arena_id,
                            index: inner.plans.len() as u32,
                        };
                        let RepKey::Layout(
                            target,
                            use_context,
                            layout,
                            storage,
                            column_eligible,
                            nobox,
                        ) = key.clone()
                        else {
                            unreachable!()
                        };
                        inner.plans.push(RepPlan {
                            id,
                            target,
                            use_context,
                            layout: RepPlanLayout::new(layout),
                            storage,
                            column_eligible,
                            nobox,
                            boxable,
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

    fn intern_value_shape_plan(
        &self,
        ws: &Workspace,
        target: FnBackend,
        use_context: RepUse,
        ty: &crate::monoty::MonoTy,
        shape: ValueShape,
    ) -> RepId {
        let key = RepKey::ValueShape(target, use_context, shape.clone());
        if let Some(id) = self.lookup_key(&key) {
            return id;
        }
        let (storage, column_eligible, boxable) = value_shape_facts(ws, ty, &shape);
        let nobox = declared_nobox(ws, ty);
        // Boxing legality is a function of the interned layout: a `Leaves`
        // shape names its nominal type (`value_shape_facts` asserts it), and
        // this predicate reads only that name and the workspace, so the plan
        // needs no extra key slot to keep two answers apart.
        let id = self.intern_with_facts(
            target,
            use_context,
            RepLayout::Leaves(shape.clone()),
            storage,
            column_eligible,
            nobox,
            boxable,
        );
        self.intern_nested_value_plans(ws, target, &shape);
        self.alias_key(key, id);
        id
    }

    fn intern_nested_value_plans(&self, ws: &Workspace, target: FnBackend, shape: &ValueShape) {
        for (_, field) in &shape.fields {
            let FieldShape::Nested(child) = field else {
                continue;
            };
            let child_ty = crate::monoty::MonoTy::App {
                ctor: crate::monoty::MonoCtor::Nominal(child.name),
                params: Vec::new(),
            };
            self.intern_value_shape_plan(ws, target, RepUse::Value, &child_ty, child.clone());
        }
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
            RepGuarantees {
                nobox: declared_nobox(ws, ty),
                allow_opaque: false,
            },
        )
    }

    pub fn get(&self, id: RepId) -> RepPlan {
        self.get_calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            id.arena, self.arena_id,
            "RepId must belong to its build arena"
        );
        REP_READ_CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();
            cache.reset_for(self.arena_id);
            if let Some(Some(plan)) = cache.plans.get(id.index as usize) {
                self.local_get_hits
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return plan.clone();
            }
            let plan = self
                .inner
                .lock()
                .expect("RepArena lock poisoned")
                .plans
                .get(id.index as usize)
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
            plans: self
                .inner
                .lock()
                .expect("RepArena lock poisoned")
                .plans
                .len() as u64,
        }
    }

    pub fn try_get(&self, id: RepId) -> Option<RepPlan> {
        if id.arena != self.arena_id {
            return None;
        }
        self.inner
            .lock()
            .expect("RepArena lock poisoned")
            .plans
            .get(id.index as usize)
            .cloned()
    }

    /// The value plans selected during this build. Late consumers receive this
    /// catalog and retain the plan ID they use; they do not reconstruct one
    /// from a nominal declaration.
    pub fn value_plan_catalog(&self) -> Vec<(Ustr, RepId)> {
        let inner = self.inner.lock().expect("RepArena lock poisoned");
        inner
            .plans
            .iter()
            .filter_map(|plan| match plan.layout() {
                RepLayout::Leaves(shape) => Some((shape.name, plan.id)),
                RepLayout::Opaque | RepLayout::Tuple { .. } | RepLayout::Foreign { .. } => None,
            })
            .collect()
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
            RepGuarantees {
                nobox: false,
                allow_opaque: true,
            },
        )
    }

    pub fn nominal_value_storage_required(
        &self,
        ws: &Workspace,
        target: FnBackend,
        name: Ustr,
    ) -> Result<ValueStorage, RepError> {
        let id = self.plan_nominal_value(ws, target, name)?;
        self.get(id).value_storage().ok_or_else(|| RepError {
            span: None,
            message: format!("`{name}` has no physical value storage plan"),
        })
    }

    pub fn native_buffer_element_ctype(
        &self,
        ws: &Workspace,
        target: FnBackend,
        element: &crate::monoty::MonoTy,
    ) -> Result<(Ustr, Option<RepId>), RepError> {
        let element_name = element.render();
        let (element, nullable) = element.peel_nullable();
        let reject = |reason: String| RepError {
            span: None,
            message: format!(
                "`native_buffer_alloc<{element_name}>` requires a valid native-buffer element ctype: {reason}"
            ),
        };
        if nullable {
            return Err(reject(
                "nullable elements have no native C-array ctype".into(),
            ));
        }
        match element {
            crate::monoty::MonoTy::Int => Ok((Ustr::from("int"), None)),
            crate::monoty::MonoTy::Uint => Ok((Ustr::from("uint32_t"), None)),
            crate::monoty::MonoTy::Float => Ok((Ustr::from("float"), None)),
            crate::monoty::MonoTy::Bool => Ok((Ustr::from("bool"), None)),
            crate::monoty::MonoTy::App {
                ctor: crate::monoty::MonoCtor::Nominal(name),
                ..
            } => {
                let id = self
                    .plan(
                        ws,
                        target,
                        element,
                        RepUse::Boundary,
                        &RepUseFacts::default(),
                        RepGuarantees {
                            nobox: false,
                            allow_opaque: true,
                        },
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
                Ok((*name, Some(id)))
            }
            _ => Err(reject(format!("`{element_name}` is not C-mappable"))),
        }
    }

    pub fn nominal_value_leaf_paths_required(
        &self,
        ws: &Workspace,
        target: FnBackend,
        name: Ustr,
    ) -> Result<Vec<Ustr>, RepError> {
        let id = self.plan_nominal_value(ws, target, name)?;
        self.get(id).leaf_paths().ok_or_else(|| RepError {
            span: None,
            message: format!("`{name}` has no physical leaf-path plan"),
        })
    }

    pub fn nominal_value_leaf_presence_flags_required(
        &self,
        ws: &Workspace,
        target: FnBackend,
        name: Ustr,
    ) -> Result<Vec<bool>, RepError> {
        let id = self.plan_nominal_value(ws, target, name)?;
        self.get(id).presence_flags().ok_or_else(|| RepError {
            span: None,
            message: format!("`{name}` has no physical presence-flag plan"),
        })
    }

    pub fn nominal_leaf_addresses(
        &self,
        ws: &Workspace,
        target: FnBackend,
        name: Ustr,
    ) -> Result<Vec<crate::lower::LeafAddr>, RepError> {
        let id = self.plan_nominal_value(ws, target, name)?;
        self.leaf_addresses(ws, id)?.ok_or_else(|| RepError {
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
        self.nominal_leaf_layout(ws, target, name)
            .map(|(_, size, _)| size)
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
        let (leaves, size, align) =
            crate::lower::value_leaf_layout(ws, shape.name).ok_or_else(|| RepError {
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

    pub fn leaf_size(&self, ws: &Workspace, id: RepId) -> Result<usize, RepError> {
        self.leaf_layout(ws, id).map(|(_, size, _)| size)
    }

    pub(crate) fn leaf_addresses(
        &self,
        ws: &Workspace,
        id: RepId,
    ) -> Result<Option<Vec<crate::lower::LeafAddr>>, RepError> {
        if !self.column_eligible(id) {
            return Ok(None);
        }
        let (leaves, size, _) = self.leaf_layout(ws, id)?;
        let mut out = Vec::with_capacity(leaves.len());
        for leaf in leaves {
            let Some((leaf_size, _)) = crate::lower::c_prim_size_align(leaf.cty) else {
                return Err(RepError {
                    span: None,
                    message: format!("leaf `{}` has no primitive C layout", leaf.path),
                });
            };
            if size % leaf_size != 0 || leaf.byte_off % leaf_size != 0 {
                return Err(RepError {
                    span: None,
                    message: format!(
                        "leaf `{}` is not aligned to its column stride (size {}, offset {}, leaf {})",
                        leaf.path, size, leaf.byte_off, leaf_size
                    ),
                });
            }
            out.push(crate::lower::LeafAddr {
                path: leaf.path,
                view_field: crate::lower::leaf_view_field(leaf.cty),
                stride: (size / leaf_size) as i64,
                offset: (leaf.byte_off / leaf_size) as i64,
            });
        }
        Ok(Some(out))
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

        if !matches!(
            ty,
            MonoTy::Nullable(_)
                | MonoTy::Tuple(_)
                | MonoTy::App {
                    ctor: MonoCtor::Nominal(_),
                    ..
                }
        ) {
            return self.plan_uncached(ws, target, ty, use_context, facts, guarantees);
        }
        // The arena belongs to one build's declarations. Index successful
        // requests before shape selection, retaining the same canonical RepId.
        // Errors are re-derived so their span always belongs to this use site.
        let key = RepKey::Concrete {
            target,
            use_context,
            ty: ty.clone(),
            tuple_spelled: facts.tuple_spelled,
            nobox: guarantees.nobox,
            allow_opaque: guarantees.allow_opaque,
        };
        if let Some(id) = self.lookup_key(&key) {
            return Ok(id);
        }
        let id = self.plan_uncached(ws, target, ty, use_context, facts, guarantees)?;
        self.alias_key(key, id);
        Ok(id)
    }

    fn plan_uncached(
        &self,
        ws: &Workspace,
        target: FnBackend,
        ty: &crate::monoty::MonoTy,
        use_context: RepUse,
        facts: &RepUseFacts,
        guarantees: RepGuarantees,
    ) -> Result<RepId, RepError> {
        use crate::monoty::{MonoCtor, MonoTy};

        let reject = |message: String| RepError {
            span: facts.span,
            message,
        };
        let tuple_element_shape = matches!(use_context, RepUse::TupleElement);
        match ty {
            MonoTy::Nullable(inner) => match inner.as_ref() {
                MonoTy::Tuple(parts) => {
                    let slots = parts
                        .iter()
                        .map(|part| {
                            self.plan(ws, target, part, RepUse::TupleElement, facts, guarantees)
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
                            RepLayout::Tuple {
                                slots,
                                nullable: true,
                            },
                        ));
                    }
                    if guarantees.nobox && !guarantees.allow_opaque {
                        return Err(reject("nullable tuple cannot cross a nobox boundary without a physical tuple plan".into()));
                    }
                    Ok(self.intern(target, use_context, RepLayout::Opaque))
                }
                _ => {
                    let shape =
                        wire_value_shape(ws, ty, use_context, guarantees, tuple_element_shape);
                    if let Some(shape) = shape {
                        return Ok(self.intern_value_shape_plan(
                            ws,
                            target,
                            use_context,
                            ty,
                            shape,
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
                        self.plan(ws, target, part, RepUse::TupleElement, facts, guarantees)
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
                        RepLayout::Tuple {
                            slots,
                            nullable: false,
                        },
                    ));
                }
                if guarantees.nobox && !facts.tuple_spelled {
                    return Ok(self.intern(target, use_context, RepLayout::Opaque));
                }
                Ok(self.intern(target, use_context, RepLayout::Opaque))
            }
            MonoTy::App {
                ctor: MonoCtor::Nominal(_),
                ..
            } => {
                let shape = wire_value_shape(ws, ty, use_context, guarantees, tuple_element_shape);
                if let Some(shape) = shape {
                    return Ok(self.intern_value_shape_plan(ws, target, use_context, ty, shape));
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
                    return Err(reject(format!(
                        "{} cannot cross a nobox boundary",
                        ty.render()
                    )));
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
            | MonoTy::Function { .. } => Ok(self.intern(target, use_context, RepLayout::Opaque)),
        }
    }

    /// Apply [`SIGNATURE_SCALAR_LANE_BUDGET`] to one signature, after the
    /// per-value cut has already run: while the signature's lanes exceed the
    /// budget, box the widest remaining ORDINARY value.
    ///
    /// The result is a pure function of the positions' representations, so the
    /// definition and every caller reach the same answer: candidates are ranked
    /// by lane count and, for a tie, by wire position (parameters in order, the
    /// result last), and the loop stops as soon as the budget is met. A
    /// `nobox` value is never a candidate, so a signature the budget cannot
    /// satisfy keeps its lanes rather than acquiring a diagnostic here.
    /// Legality comes from each position's own [`RepPlan::boxable`], derived
    /// once when its concrete type was planned: false for an ordinary value
    /// whose box would materialize a `nobox` child
    /// ([`box_would_materialize_nobox`]); a `nobox` value is excluded
    /// separately, by [`RepPlan::nobox`].
    fn apply_lane_budget(
        &self,
        target: FnBackend,
        params: &mut [RepId],
        result: &mut Option<RepId>,
    ) {
        let lanes = |id: &RepId| self.get(*id).lanes(self);
        let total = |params: &[RepId], result: &Option<RepId>| {
            params.iter().map(lanes).sum::<usize>() + result.as_ref().map_or(0, lanes)
        };
        if total(params, result) <= SIGNATURE_SCALAR_LANE_BUDGET {
            return;
        }
        loop {
            let widest = params
                .iter()
                .enumerate()
                .map(|(index, id)| (index, *id, RepUse::Parameter))
                .chain(result.map(|id| (params.len(), id, RepUse::Return)))
                .filter(|(_, id, _)| {
                    let plan = self.get(*id);
                    matches!(plan.layout(), RepLayout::Leaves(_)) && !plan.nobox() && plan.boxable()
                })
                .max_by_key(|(index, id, _)| (lanes(id), std::cmp::Reverse(*index)));
            let Some((index, _, use_context)) = widest else {
                return;
            };
            let boxed = self.intern(target, use_context, RepLayout::Opaque);
            match use_context {
                RepUse::Return => *result = Some(boxed),
                _ => params[index] = boxed,
            }
            if total(params, result) <= SIGNATURE_SCALAR_LANE_BUDGET {
                return;
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

    pub fn function_value_first_class(
        &self,
        ws: &Workspace,
        target: FnBackend,
        ty: &crate::monoty::MonoTy,
    ) -> Result<FunctionRepPlan, RepError> {
        self.function_value_with_policy(ws, target, ty, false, true)
    }

    pub fn function_value_with_mode(
        &self,
        ws: &Workspace,
        target: FnBackend,
        ty: &crate::monoty::MonoTy,
        nobox_only: bool,
    ) -> Result<FunctionRepPlan, RepError> {
        self.function_value_with_policy(ws, target, ty, nobox_only, false)
    }

    fn function_value_with_policy(
        &self,
        ws: &Workspace,
        target: FnBackend,
        ty: &crate::monoty::MonoTy,
        nobox_only: bool,
        first_class: bool,
    ) -> Result<FunctionRepPlan, RepError> {
        let (ty, _) = ty.peel_nullable();
        let crate::monoty::MonoTy::Function {
            params: fn_params,
            ret,
        } = ty
        else {
            return Err(RepError {
                span: None,
                message: "a function-value representation requires a function type".into(),
            });
        };
        let mut params = fn_params
            .iter()
            .map(|param| {
                let tuple_spelled =
                    if matches!(param.peel_nullable().0, crate::monoty::MonoTy::Tuple(_)) {
                        let id = self.plan(
                            ws,
                            target,
                            param,
                            RepUse::Return,
                            &RepUseFacts::default(),
                            RepGuarantees {
                                nobox: true,
                                allow_opaque: true,
                            },
                        )?;
                        self.get(id).splats(self)
                    } else {
                        false
                    };
                if first_class && !tuple_spelled {
                    Ok(self.intern(target, RepUse::Parameter, RepLayout::Opaque))
                } else {
                    self.plan(
                        ws,
                        target,
                        param,
                        RepUse::Parameter,
                        &RepUseFacts {
                            tuple_spelled,
                            span: None,
                        },
                        RepGuarantees {
                            nobox: nobox_only,
                            allow_opaque: true,
                        },
                    )
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut result = if matches!(ret.as_ref(), crate::monoty::MonoTy::Unit) {
            None
        } else if first_class && !matches!(ret.peel_nullable().0, crate::monoty::MonoTy::Tuple(_)) {
            Some(self.intern(target, RepUse::Return, RepLayout::Opaque))
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
                RepGuarantees {
                    nobox: nobox_only,
                    allow_opaque: true,
                },
            )?)
        };
        self.apply_lane_budget(target, &mut params, &mut result);
        let scalar_abi = params
            .iter()
            .any(|id| !matches!(self.get(*id).layout(), RepLayout::Opaque))
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
        let mut params: Vec<RepId> = Vec::with_capacity(f.param_decls.len());
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
                    RepGuarantees {
                        nobox: mode.nobox_only_for(&ty),
                        allow_opaque: true,
                    },
                )?,
                (None, Some(ty)) if declared_tuple_spelled(ws, param.ty) => self.plan(
                    ws,
                    target,
                    &ty,
                    RepUse::Parameter,
                    &RepUseFacts {
                        tuple_spelled: true,
                        span: None,
                    },
                    RepGuarantees {
                        nobox: false,
                        allow_opaque: true,
                    },
                )?,
                _ => self.intern(target, RepUse::Parameter, RepLayout::Opaque),
            };
            params.push(id);
        }

        let mut result = f
            .return_ty
            .map(|return_ty| {
                let ty = declared_mono_type(ws, return_ty, subst)?;
                if ty
                    .as_ref()
                    .is_some_and(|ty| matches!(ty, crate::monoty::MonoTy::Unit))
                {
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
                            &RepUseFacts {
                                tuple_spelled: tuple_spelled || is_tuple,
                                span: None,
                            },
                            RepGuarantees {
                                nobox: mode.nobox_only_for(&ty),
                                allow_opaque: true,
                            },
                        )?
                    } else if is_tuple {
                        self.plan(
                            ws,
                            target,
                            &ty,
                            RepUse::Return,
                            &RepUseFacts {
                                tuple_spelled: true,
                                span: None,
                            },
                            RepGuarantees::default(),
                        )?
                    } else {
                        self.intern(target, RepUse::Return, RepLayout::Opaque)
                    }
                } else {
                    self.intern(target, RepUse::Return, RepLayout::Opaque)
                };
                Ok(Some(id))
            })
            .transpose()?
            .flatten();
        self.apply_lane_budget(target, &mut params, &mut result);
        let scalar_abi = mode.is_some()
            && (params.iter().any(|id| {
                matches!(
                    self.get(*id).layout(),
                    RepLayout::Leaves(_) | RepLayout::Tuple { .. }
                )
            }) || result
                .is_some_and(|id| matches!(self.get(id).layout(), RepLayout::Leaves(_))));
        let return_lanes = result.map_or(0, |id| self.get(id).lanes(self));
        Ok(FunctionRepPlan {
            params,
            result,
            return_lanes,
            scalar_abi,
        })
    }

    pub(crate) fn boxed_function(&self, target: FnBackend, param_count: usize) -> FunctionRepPlan {
        let params = (0..param_count)
            .map(|_| self.intern(target, RepUse::Parameter, RepLayout::Opaque))
            .collect();
        let result = self.intern(target, RepUse::Return, RepLayout::Opaque);
        FunctionRepPlan {
            params,
            result: Some(result),
            return_lanes: 1,
            scalar_abi: false,
        }
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
        let mut params: Vec<RepId> = if let Some(mode) = mode {
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
                        &RepUseFacts {
                            tuple_spelled,
                            span: None,
                        },
                        RepGuarantees {
                            nobox: mode.nobox_only_for(ty),
                            allow_opaque: true,
                        },
                    )
                    .expect("concrete ABI planner must accept a validated parameter")
                })
                .collect()
        } else {
            params
                .iter()
                .zip(param_tuple_spelled)
                .map(|(ty, &tuple_spelled)| {
                    if tuple_spelled {
                        self.plan(
                            ws,
                            target,
                            ty,
                            RepUse::Parameter,
                            &RepUseFacts {
                                tuple_spelled: true,
                                span: None,
                            },
                            RepGuarantees {
                                nobox: false,
                                allow_opaque: true,
                            },
                        )
                        .expect("concrete ABI planner must accept a validated tuple parameter")
                    } else {
                        self.intern(target, RepUse::Parameter, RepLayout::Opaque)
                    }
                })
                .collect()
        };
        let mut result = result_ty.map(|ty| {
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
                    &RepUseFacts {
                        tuple_spelled,
                        span: None,
                    },
                    RepGuarantees {
                        nobox: mode.is_some_and(|mode| mode.nobox_only_for(ty)),
                        allow_opaque: true,
                    },
                )
                .expect("concrete ABI planner must accept a validated result")
            } else {
                self.intern(target, RepUse::Return, RepLayout::Opaque)
            }
        });
        self.apply_lane_budget(target, &mut params, &mut result);
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
        let plan = in_hand
            .or_else(|| plans.get(owner).map(|plan| &**plan))
            .ok_or_else(|| {
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
        if let Some(at) =
            crate::monoty::too_deep_to_instantiate(inst.arg_types(scx.interner()).iter())
        {
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
    Arc::new(BackendFnAbiPlan::for_concrete_signature(
        ws,
        scx,
        selected,
        &canonical.signature,
    ))
}

pub fn requires_concrete_body_plan(ws: &Workspace, inst: &crate::instance::Instance) -> bool {
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
    fixed_nullable_transport: bool,
}

impl AbiMode {
    fn nobox_only_for(self, ty: &crate::monoty::MonoTy) -> bool {
        // Declared boundaries keep their nullable nominal transport. Only
        // Rock bodies can adopt the ordinary value presence/leaf return.
        self.nobox_only
            || (self.fixed_nullable_transport
                && matches!(ty,
            crate::monoty::MonoTy::Nullable(inner) if matches!(inner.as_ref(),
                crate::monoty::MonoTy::App { ctor: crate::monoty::MonoCtor::Nominal(_), .. })))
    }
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
    let has_scalar_abi = f.annotations.iter().any(|a| a.is_builtin("scalar_abi"));
    let name_is_fixed = f
        .annotations
        .iter()
        .any(|a| (a.is_builtin("nomangle") && subst.is_empty()) || a.is_builtin("test"));
    let nobox_only = if has_scalar_abi {
        false
    } else {
        if f.inline_body.is_some() || f.name.as_str() == "main" || name_is_fixed {
            return None;
        }
        scx.taken_by_ref.contains(&f.name)
    };
    Some(AbiMode {
        nobox_only,
        fixed_nullable_transport: f.inline_body.is_some()
            || name_is_fixed
            || f.name.as_str() == "main",
    })
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
    transport: Option<ParameterTransport>,
    body_dependent_parameters: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ParameterTransport {
    original: Vec<RepId>,
    lanes: Vec<usize>,
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
            && self.transport == other.transport
            && self.body_dependent_parameters == other.body_dependent_parameters
            && self.arena.equivalent(&other.arena)
    }
}

impl Eq for BackendFnAbiPlan {}

pub(super) fn expanded_nullable_parameter_shape(ws: &Workspace, shape: &ValueShape) -> ValueShape {
    let mut expanded = shape.clone();
    if shape.nullable {
        return expanded;
    }
    if let Some(sid) = ws.struct_by_name.get(&shape.name) {
        let sd = &ws.modules[sid.module.index()].structs[sid.def];
        for (name, field) in &mut expanded.fields {
            if !matches!(field, FieldShape::Prim) {
                continue;
            }
            let Some(decl) = sd.fields.iter().find(|decl| decl.name == *name) else {
                continue;
            };
            let Some((child, true)) = super::shape::nominal_name_from_ty(ws, decl.ty) else {
                continue;
            };
            let Some(mut child) = super::shape::planned_value(ws, child).and_then(|p| p.shape)
            else {
                continue;
            };
            if child.leaf_count() > WIDE_VALUE_PAYLOAD_LEAF_CUT {
                continue;
            }
            child.nullable = true;
            *field = FieldShape::Nested(child);
        }
    }
    expanded
}

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
    pub fn for_concrete_plan(ws: &Workspace, scx: &ScalarCtx, plan: &ConcreteBodyPlan) -> Self {
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
        let mut abi = Self {
            subject: inst.clone(),
            target: scx.target,
            representation,
            arena,
            transport: None,
            body_dependent_parameters: false,
        };
        if !params.iter().any(|ty| {
            matches!(
                ty,
                crate::monoty::MonoTy::App {
                    ctor: crate::monoty::MonoCtor::Ref,
                    ..
                }
            )
        }) {
            abi.expand_nullable_parameter_children(ws, scx);
            abi.narrow_parameter_transport(ws, scx);
        }
        abi
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
        if scx.values_first_class {
            scx.rep_arena()
                .function_value_first_class(ws, scx.target, ty)
        } else {
            scx.rep_arena().function_value(ws, scx.target, ty)
        }
    }

    pub fn for_function_value_with_mode(
        ws: &Workspace,
        scx: &ScalarCtx,
        ty: &crate::monoty::MonoTy,
        nobox_only: bool,
    ) -> Result<FunctionRepPlan, RepError> {
        scx.rep_arena()
            .function_value_with_mode(ws, scx.target, ty, nobox_only)
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
            transport: None,
            body_dependent_parameters: false,
        }
    }

    fn permits_parameter_transport(
        &self,
        ws: &Workspace,
        scx: &ScalarCtx,
        with_defaults: bool,
    ) -> bool {
        let f = &ws.module(self.subject.func.module).functions[self.subject.func.func];
        !(scx.values_first_class
            || !matches!(self.target, FnBackend::Lua | FnBackend::Any)
            || scx.taken_by_ref.contains(&f.name)
            || f.is_extern
            || f.inline_body.is_some()
            || f.is_coroutine
            || f.is_variadic
            || !f.annotations.is_empty()
            || (!with_defaults && f.num_required != f.param_decls.len())
            || !self.uses_scalar_abi())
    }

    // Parameter transport can snapshot compact nullable children as scalar
    // lanes without changing their field storage or widening the ABI budget.
    // A defaulted parameter keeps its declared wire, which its default fill and
    // omitted-argument nils are built against; the others still expand.
    fn expand_nullable_parameter_children(&mut self, ws: &Workspace, scx: &ScalarCtx) {
        if !self.permits_parameter_transport(ws, scx, true) {
            return;
        }
        let f = &ws.module(self.subject.func.module).functions[self.subject.func.func];
        let mut shapes = Vec::new();
        let mut lanes = 0;
        for (i, &id) in self.representation.params.iter().enumerate() {
            let rep = self.arena.get(id);
            let defaulted = f.param_decls.get(i).is_none_or(|param| param.has_default);
            let (RepLayout::Leaves(shape), false) = (rep.layout(), defaulted) else {
                lanes += rep.lanes(&self.arena);
                shapes.push(None);
                continue;
            };
            let expanded = expanded_nullable_parameter_shape(ws, shape);
            lanes += expanded.leaf_count();
            shapes.push((expanded != *shape).then_some(expanded));
        }
        if lanes + 1 > SIGNATURE_SCALAR_LANE_BUDGET {
            return;
        }
        for (i, shape) in shapes.into_iter().enumerate() {
            let Some(shape) = shape else { continue };
            let rep = self.arena.get(self.representation.params[i]);
            self.representation.params[i] = self.arena.intern_with_facts(
                self.target,
                RepUse::Parameter,
                RepLayout::Leaves(shape),
                rep.storage,
                false,
                rep.nobox,
                false,
            );
        }
    }

    fn narrow_parameter_transport(&mut self, ws: &Workspace, scx: &ScalarCtx) {
        use bone_semantic::hir::BodyOwner;
        // Field demand does not read default expressions, so defaults keep the
        // full transport here.
        if !self.permits_parameter_transport(ws, scx, false) {
            return;
        }
        self.body_dependent_parameters = self.representation.params.iter().any(|id| {
            matches!(self.arena.get(*id).layout(), RepLayout::Leaves(shape)
                if !shape.nullable && ws.struct_by_name.contains_key(&shape.name))
        });
        if !self.body_dependent_parameters {
            return;
        }
        let Some(body) = ws
            .module(self.subject.func.module)
            .bodies
            .get(&BodyOwner::Function(self.subject.func.func))
        else {
            return;
        };
        let Some(demand) = super::parameter_demand::fields(body) else {
            return;
        };
        if demand.len() != self.representation.params.len() {
            return;
        }
        let original = self.representation.params.clone();
        let mut lanes = Vec::new();
        let mut offset = 0;
        for (i, &id) in original.iter().enumerate() {
            let rep = self.arena.get(id);
            let width = rep.lanes(&self.arena);
            if let RepLayout::Leaves(shape) = rep.layout()
                && !shape.nullable
                && ws.struct_by_name.contains_key(&shape.name)
                && let Some(fields) = &demand[i]
                && fields
                    .iter()
                    .all(|field| shape.fields.iter().any(|(name, _)| name == field))
            {
                let mut narrowed = shape.clone();
                narrowed.fields.clear();
                let mut leaf = offset;
                for (name, field) in &shape.fields {
                    let n = match field {
                        FieldShape::Prim => 1,
                        FieldShape::Nested(s) => s.leaf_count(),
                    };
                    if fields.contains(name) {
                        narrowed.fields.push((*name, field.clone()));
                        lanes.extend(leaf..leaf + n);
                    }
                    leaf += n;
                }
                if narrowed.fields.len() != shape.fields.len() {
                    self.representation.params[i] = self.arena.intern_with_facts(
                        self.target,
                        RepUse::Parameter,
                        RepLayout::Leaves(narrowed),
                        rep.storage,
                        false,
                        rep.nobox,
                        false,
                    );
                }
            } else {
                lanes.extend(offset..offset + width);
            }
            offset += width;
        }
        if lanes.len() != offset {
            self.transport = Some(ParameterTransport { original, lanes });
        }
    }

    /// Evaluate the original argument layout before dropping unused transport.
    /// A projected layout is not a complete value and must never be boxed.
    pub(super) fn argument_shapes(&self) -> Vec<RetSlot> {
        self.transport.as_ref().map_or_else(
            || self.param_shapes(),
            |transport| {
                transport
                    .original
                    .iter()
                    .map(|id| self.arena.ret_slot(*id))
                    .collect()
            },
        )
    }

    pub(super) fn transport_arguments<T: Copy>(
        &self,
        args: Vec<T>,
    ) -> Result<Vec<T>, skull_parser::CompilerError> {
        match &self.transport {
            Some(transport) => {
                let expected: usize = transport
                    .original
                    .iter()
                    .map(|id| self.arena.get(*id).lanes(&self.arena))
                    .sum();
                if args.len() != expected {
                    return Err(skull_parser::CompilerError::new_no_location(format!(
                        "argument transport expected {expected} original lanes, received {}",
                        args.len()
                    )));
                }
                Ok(transport.lanes.iter().map(|&lane| args[lane]).collect())
            }
            None => Ok(args),
        }
    }

    /// Body edits can change this direct-call layout even when it is currently full.
    pub fn body_dependent_parameters(&self) -> bool {
        self.body_dependent_parameters
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

    /// The ABI compatibility witness for a code slot. It deliberately expands
    /// the representation through the arena instead of retaining a `RepId`.
    pub fn logical_fingerprint(&self) -> String {
        format!(
            "target={:?};{}",
            self.target,
            self.representation.logical_fingerprint(&self.arena)
        )
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
        self.representation
            .result()
            .and_then(|id| self.arena.value_shape(id))
    }

    /// The declared return's tuple shape under this body's substitution.
    pub(crate) fn ret_tuple(&self) -> Option<TupleRetShape> {
        self.representation
            .result()
            .and_then(|id| self.arena.tuple_shape(id))
    }

    /// Physical values on the result wire: 0 with no return, 1 for an ordinary
    /// one, more for the two shapes that splat -- a tuple return (each element
    /// its own slot, a `nobox` element into its leaves) and a bare `nobox`
    /// value return under the scalar ABI.
    ///
    /// **A JS transport fact.** Plan S 13b: leaf 0 rides the ordinary JS
    /// `return`, lanes 1..N ride the module-scope scratch bank. Luajit has native multi-return and
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
        self.uses_scalar_abi().then(|| {
            self.param_shapes()
                .iter()
                .map(RetSlot::slot_count)
                .collect()
        })
    }
}
pub(crate) fn rep_logical_fingerprint(arena: &RepArena, id: RepId) -> String {
    let plan = arena.get(id);
    let layout = match plan.layout() {
        RepLayout::Opaque => "opaque".to_string(),
        RepLayout::Foreign { reason } => format!("foreign:{reason}"),
        RepLayout::Leaves(_) => format!(
            "leaves:{:?}:{:?}",
            plan.leaf_paths().unwrap_or_default(),
            plan.presence_flags().unwrap_or_default()
        ),
        RepLayout::Tuple { slots, nullable } => format!(
            "tuple(nullable={nullable};[{}])",
            slots
                .iter()
                .map(|slot| rep_logical_fingerprint(arena, *slot))
                .collect::<Vec<_>>()
                .join(",")
        ),
    };
    format!(
        "{layout};storage={:?};column={};nobox={};lanes={}",
        plan.value_storage(),
        plan.column_eligible(),
        plan.nobox(),
        plan.lanes(arena)
    )
}

#[cfg(test)]
mod wide_value_cut_tests {
    use super::*;

    fn shape(leaves: usize, nullable: bool) -> ValueShape {
        ValueShape {
            name: Ustr::from("V"),
            fields: (0..leaves)
                .map(|k| (Ustr::from(&format!("f{k}")), FieldShape::Prim))
                .collect(),
            nullable,
        }
    }

    /// The cut compares the PAYLOAD, so `T` and `T?` answer alike. Reading
    /// `leaf_count()` instead would make a value at exactly the cut box only
    /// when it is nullable -- the census's "Observed: `leaf_count()` counts the
    /// nullable presence flag".
    #[test]
    fn the_cut_reads_the_payload_not_the_presence_flag() {
        let cut = WIDE_VALUE_PAYLOAD_LEAF_CUT;
        assert_eq!(payload_leaf_count(&shape(cut, false)), cut);
        assert_eq!(payload_leaf_count(&shape(cut, true)), cut);
        assert_eq!(
            shape(cut, true).leaf_count(),
            cut + 1,
            "the fixture must carry a presence flag"
        );
        assert_eq!(payload_leaf_count(&shape(cut + 1, true)), cut + 1);
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
            RepLayout::Foreign {
                reason: Ustr::from("test"),
            },
        );
        assert_eq!(
            ia.raw(),
            ib.raw(),
            "both arenas must mint local id 0 for the fixture to discriminate"
        );
        assert_ne!(ia, ib, "arena ownership must distinguish equal local ids");
        for _ in 0..2 {
            assert!(matches!(a.get(ia).layout(), RepLayout::Opaque));
            assert!(matches!(b.get(ib).layout(), RepLayout::Foreign { .. }));
        }
        let ib2 = b.intern(FnBackend::Lua, RepUse::Return, RepLayout::Opaque);
        assert_ne!(
            ib, ib2,
            "b's opaque return must not be served a's local mapping"
        );
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

    #[test]
    fn repeated_shape_reads_share_immutable_fields() {
        let arena = RepArena::default();
        let id = arena.intern(
            FnBackend::Lua,
            RepUse::Value,
            RepLayout::Leaves(ValueShape {
                name: Ustr::from("Pair"),
                fields: vec![
                    (Ustr::from("x"), FieldShape::Prim),
                    (Ustr::from("y"), FieldShape::Prim),
                ],
                nullable: false,
            }),
        );
        let first = arena.get(id);
        let second = arena.get(id);
        let (RepLayout::Leaves(left), RepLayout::Leaves(right)) = (first.layout(), second.layout())
        else {
            panic!("shape representation expected");
        };
        assert_eq!(left, right);
        assert_eq!(
            left.fields.as_ptr(),
            right.fields.as_ptr(),
            "immutable shape reads must not copy fields"
        );
    }

    #[test]
    fn repeated_tuple_reads_share_immutable_slots() {
        let arena = RepArena::default();
        let scalar = arena.intern(FnBackend::Lua, RepUse::Value, RepLayout::Opaque);
        let id = arena.intern(
            FnBackend::Lua,
            RepUse::Return,
            RepLayout::Tuple {
                slots: vec![scalar, scalar],
                nullable: false,
            },
        );
        let first = arena.get(id);
        let second = arena.get(id);
        let (RepLayout::Tuple { slots: left, .. }, RepLayout::Tuple { slots: right, .. }) =
            (first.layout(), second.layout())
        else {
            panic!("tuple representation expected");
        };
        assert_eq!(left, right);
        assert_eq!(
            left.as_ptr(),
            right.as_ptr(),
            "immutable tuple reads must not copy slots"
        );
    }
}
