use rustc_hash::FxHashMap as HashMap;
use std::sync::{Arc, OnceLock, RwLock};

use bone_core::hir::{FnBackend, Function};
use bone_core::prelude::Span;
use bone_core::workspace::Workspace;
use ustr::Ustr;

use super::{FieldShape, ScalarCtx, SpecSubst, ValueShape, ValueStorage, leaf_names};
use crate::concrete::{ConcreteBodyPlan, ConcreteSignature};
use crate::mir::FuncRef;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum BoundaryClass {
    Extern,
    InlineBody,
    Intrinsic,
    ErasedRuntimeEntry,
}

impl BoundaryClass {
    pub fn name(self) -> &'static str {
        match self {
            Self::Extern => "extern",
            Self::InlineBody => "inline-body",
            Self::Intrinsic => "intrinsic",
            Self::ErasedRuntimeEntry => "erased-runtime-entry",
        }
    }
}

/// Declaration-owned boundaries only. Keeping target capability out of this
/// function stops `@inline` from becoming an identity or representation decision.
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

/// `@scalar_abi` is a transport contract for inline text; it does not create an
/// instance or change the boundary class.
pub fn boundary_requires_boxed_representation(f: &Function) -> bool {
    match declared_boundary_class(f) {
        Some(BoundaryClass::Extern) => true,
        Some(BoundaryClass::InlineBody) => {
            !f.annotations.iter().any(|a| a.is_builtin("scalar_abi"))
        }
        _ => false,
    }
}

/// Meaningful only through the build-scoped arena carried by `ScalarCtx`.
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

/// Part of the representation key: a return tuple's element must not become a
/// parameter just because its payload has the same leaves.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RepUse {
    Parameter,
    Return,
    TupleElement,
    Value,
    Boundary,
}

#[derive(Clone, Debug, Default)]
pub struct RepUseFacts {
    pub tuple_spelled: bool,
    pub span: Option<Span>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RepGuarantees {
    pub nobox: bool,
    /// Function-value and mixed scalar ABIs may carry reference types in an opaque
    /// slot while still flattening eligible value leaves.
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
    Tuple(TupleLayout),
}

/// A nullable tuple transports its presence before its elements' lanes.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TupleLayout {
    pub slots: Vec<RepId>,
    pub nullable: bool,
}

impl TupleLayout {
    pub fn slot_count(&self, arena: &RepArena) -> usize {
        self.slots
            .iter()
            .map(|&id| arena.get(id).lanes(arena))
            .sum::<usize>()
            + usize::from(self.nullable)
    }

    pub(super) fn splats(&self, arena: &RepArena) -> bool {
        self.slots.iter().any(|&id| arena.get(id).layout().splats())
    }

    pub(super) fn can_fill(&self, expected: &Self, arena: &RepArena) -> bool {
        if self == expected {
            return true;
        }
        (!self.nullable || expected.nullable)
            && self.slots.len() == expected.slots.len()
            && self
                .slots
                .iter()
                .zip(&expected.slots)
                .all(
                    |(&actual, &expected)| match (arena.layout(actual), arena.layout(expected)) {
                        (RepLayout::Opaque, RepLayout::Opaque) => true,
                        (RepLayout::Leaves(actual), RepLayout::Leaves(expected)) => {
                            super::value_shape_can_fill(actual, expected)
                        }
                        (RepLayout::Tuple(actual), RepLayout::Tuple(expected)) => {
                            actual.can_fill(expected, arena)
                        }
                        _ => false,
                    },
                )
    }
}

impl RepLayout {
    pub(super) fn slot_count(&self, arena: &RepArena) -> usize {
        match self {
            RepLayout::Opaque => 1,
            RepLayout::Leaves(shape) => shape.leaf_count(),
            RepLayout::Tuple(tuple) => tuple.slot_count(arena),
        }
    }

    pub(super) fn splats(&self) -> bool {
        !matches!(self, RepLayout::Opaque)
    }
}

/// Backends may inspect a plan but must not derive another answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepPlan {
    id: RepId,
    target: FnBackend,
    use_context: RepUse,
    layout: RepLayout,
    storage: Option<ValueStorage>,
    column_eligible: bool,
    nobox: bool,
    /// Passing a narrow GC box may allocate its ordinary children in hot calls.
    prefer_scalar: bool,
}

impl RepPlan {
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

    /// A direct, nonnullable value can rebuild its leaf ctor from these fields.
    pub fn plain_leaf_fields(&self) -> Option<(Ustr, impl ExactSizeIterator<Item = Ustr> + '_)> {
        let RepLayout::Leaves(shape) = self.layout() else {
            return None;
        };
        (!shape.nullable
            && shape
                .fields
                .iter()
                .all(|(_, field)| matches!(field, FieldShape::Prim)))
        .then(|| (shape.name, shape.fields.iter().map(|&(name, _)| name)))
    }

    pub fn leaf_paths(&self) -> Option<Vec<Ustr>> {
        match self.layout() {
            RepLayout::Leaves(shape) => Some(crate::lower::value_leaf_paths(shape)),
            RepLayout::Opaque | RepLayout::Tuple(_) => None,
        }
    }

    pub(crate) fn presence_flags(&self) -> Option<Vec<bool>> {
        match self.layout() {
            RepLayout::Leaves(shape) => Some(crate::lower::value_leaf_presence_flags(shape)),
            RepLayout::Opaque | RepLayout::Tuple(_) => None,
        }
    }

    pub fn lanes(&self, arena: &RepArena) -> usize {
        self.layout().slot_count(arena)
    }

    pub fn splats(&self, arena: &RepArena) -> bool {
        match self.layout() {
            RepLayout::Leaves(_) => true,
            RepLayout::Tuple(TupleLayout { slots, .. }) => {
                slots.iter().any(|slot| arena.get(*slot).splats(arena))
            }
            RepLayout::Opaque => false,
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

#[derive(Clone, Debug)]
pub struct GlobalPlan {
    representation: RepId,
    arena: Arc<RepArena>,
}

impl GlobalPlan {
    pub fn for_declared_type(
        ws: &Workspace,
        scx: &ScalarCtx,
        ty: bone_core::prelude::TyId,
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
        let arena = scx.rep_arena().clone();
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

    pub(crate) fn value_shape(&self) -> Option<&ValueShape> {
        self.arena.value_shape(self.representation)
    }

    pub fn value_storage(&self) -> Option<ValueStorage> {
        self.arena.get(self.representation).value_storage()
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

    pub(crate) fn logical_fingerprint(&self) -> String {
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

    pub fn return_lanes(&self) -> usize {
        self.return_lanes
    }

    pub fn uses_scalar_abi(&self) -> bool {
        self.scalar_abi
    }

    pub(crate) fn logical_fingerprint(&self, arena: &RepArena) -> String {
        let mut out = String::new();
        self.write_logical_fingerprint(arena, &mut out);
        out
    }

    fn write_logical_fingerprint(&self, arena: &RepArena, out: &mut String) {
        use std::fmt::Write as _;
        out.push_str("params=[");
        for (i, id) in self.params.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&rep_logical_fingerprint_shared(arena, *id));
        }
        out.push_str("];result=");
        match self.result {
            Some(id) => out.push_str(&rep_logical_fingerprint_shared(arena, id)),
            None => out.push_str("none"),
        }
        let _ = write!(
            out,
            ";lanes={};scalar={}",
            self.return_lanes, self.scalar_abi
        );
    }
}

/// One memo per question: `Callee` includes backend-variant selection, `Owner`
/// is the selected owner's own definition ABI.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum AbiQuestion {
    Callee,
    Owner,
}

#[derive(Clone, Default)]
struct AbiSlots {
    owner: OnceLock<BackendFnAbiPlanHandle>,
    callee: OnceLock<BackendFnAbiPlanHandle>,
}

impl AbiSlots {
    fn slot(&self, question: AbiQuestion) -> &OnceLock<BackendFnAbiPlanHandle> {
        match question {
            AbiQuestion::Owner => &self.owner,
            AbiQuestion::Callee => &self.callee,
        }
    }
}

/// Only successful plans are cached, so failures keep their source spans.
#[derive(Clone, Default)]
pub(super) struct AbiPlanCache {
    ids: Arc<crate::instance::InstanceIndex>,
    slots: crate::append_vec::SlotTable<AbiSlots>,
}

impl std::fmt::Debug for AbiPlanCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AbiPlanCache")
    }
}

impl AbiPlanCache {
    fn slots(&self, inst: &crate::instance::Instance) -> &AbiSlots {
        self.slots
            .get_or_init(self.ids.id(inst).index(), AbiSlots::default)
    }

    pub(super) fn lookup(
        &self,
        question: AbiQuestion,
        inst: &crate::instance::Instance,
    ) -> Option<BackendFnAbiPlanHandle> {
        self.slots(inst).slot(question).get().map(Arc::clone)
    }

    pub(super) fn store(
        &self,
        question: AbiQuestion,
        inst: &crate::instance::Instance,
        plan: &BackendFnAbiPlanHandle,
    ) {
        let _ = self.slots(inst).slot(question).set(Arc::clone(plan));
    }

    pub(super) fn clear(&mut self) {
        self.slots = Default::default();
    }
}

/// Published plans and keys are immutable and append-only, so worker-local caches
/// keyed by the arena id stay valid.
pub struct RepArena {
    /// New plans are pushed only while `by_key` is write-locked, so an index and
    /// its key are published together.
    plans: crate::append_vec::AppendVec<RepPlan>,
    by_key: RwLock<HashMap<RepKey, RepId>>,
    arena_id: u64,
    interner: Arc<crate::monoty::MonoInterner>,
    facts: crate::append_vec::SlotTable<TypeFacts>,
}

const TARGETS: usize = 3;
const REP_USES: usize = 5;
const REP_SLOTS: usize = TARGETS * REP_USES * 8;
const CONTAINER_FAMILIES: usize = 5;

fn target_index(target: FnBackend) -> usize {
    match target {
        FnBackend::Any => 0,
        FnBackend::Lua => 1,
        FnBackend::Js => 2,
    }
}

fn rep_slot(
    target: FnBackend,
    use_context: RepUse,
    tuple_spelled: bool,
    guarantees: RepGuarantees,
) -> usize {
    let use_index = match use_context {
        RepUse::Parameter => 0,
        RepUse::Return => 1,
        RepUse::TupleElement => 2,
        RepUse::Value => 3,
        RepUse::Boundary => 4,
    };
    let flags = usize::from(tuple_spelled)
        | usize::from(guarantees.nobox) << 1
        | usize::from(guarantees.allow_opaque) << 2;
    (target_index(target) * REP_USES + use_index) * 8 + flags
}

fn container_slot(target: FnBackend, family: super::container::ContainerFamily) -> usize {
    target_index(target) * CONTAINER_FAMILIES + family as usize
}

struct TypeFacts {
    /// Planned representation index plus one; zero is unplanned. Failed plans
    /// are not recorded, so their errors are re-derived with each use's span.
    reps: [std::sync::atomic::AtomicU32; REP_SLOTS],
    containers: [OnceLock<super::container::ContainerPlan>; TARGETS * CONTAINER_FAMILIES],
}

impl Default for TypeFacts {
    fn default() -> Self {
        Self {
            reps: std::array::from_fn(|_| std::sync::atomic::AtomicU32::new(0)),
            containers: std::array::from_fn(|_| OnceLock::new()),
        }
    }
}

impl RepArena {
    /// Unique per arena, so worker-local caches can key on it.
    pub(crate) fn id(&self) -> u64 {
        self.arena_id
    }
}

impl std::fmt::Debug for RepArena {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RepArena({} plans)", self.plans.len())
    }
}

impl Default for RepArena {
    fn default() -> Self {
        Self::new(Arc::new(crate::monoty::MonoInterner::new()))
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
    ValueShape(FnBackend, RepUse, ValueShape),
}

#[derive(Default)]
struct RepReadCache {
    arena_gen: u64,
    by_key: rustc_hash::FxHashMap<RepKey, RepId>,
    fingerprints: Vec<Option<Arc<str>>>,
}

impl RepReadCache {
    fn reset_for(&mut self, arena_gen: u64) {
        if self.arena_gen != arena_gen {
            self.arena_gen = arena_gen;
            self.by_key.clear();
            self.fingerprints.clear();
        }
    }
}

thread_local! {
    static REP_READ_CACHE: std::cell::RefCell<RepReadCache> =
        std::cell::RefCell::new(RepReadCache::default());
}

impl PartialEq for RepArena {
    fn eq(&self, other: &Self) -> bool {
        self.equivalent(other)
    }
}

impl Eq for RepArena {}

fn value_shape_facts(ws: &Workspace, name: Ustr) -> (Option<ValueStorage>, bool, bool) {
    let Some(plan) = super::shape::planned_value(ws, name) else {
        return (None, false, false);
    };
    let column_eligible = super::shape::is_value_struct(ws, name)
        && (plan.scalarizable || crate::lower::is_nobox_nominal(ws, name));
    (Some(plan.storage), column_eligible, plan.gc_contains_nobox)
}

/// Ordinary values above this width can use one boxed slot on Lua call wires.
/// GC boxes containing inline nobox children count their physical keys.
pub const WIDE_VALUE_PAYLOAD_LEAF_CUT: usize = 48;

/// Maximum scalar lanes across a signature, counting every parameter and
/// result lane including nullable presence.
pub const SIGNATURE_SCALAR_LANE_BUDGET: usize = 96;

/// Excludes the nullable presence flag that `ValueShape::leaf_count` includes.
fn payload_leaf_count(shape: &ValueShape) -> usize {
    shape.leaf_count() - usize::from(shape.nullable)
}

fn wide_gc_box(ws: &Workspace, target: FnBackend, name: Ustr) -> bool {
    super::shape::gc_table_inline_fields(ws, target, name)
        .ok()
        .flatten()
        .is_some_and(|fields| {
            fields
                .iter()
                .map(|field| field.inline.as_ref().map_or(1, ValueShape::leaf_count))
                .sum::<usize>()
                > WIDE_VALUE_PAYLOAD_LEAF_CUT
        })
}

/// `None` uses one opaque slot. Declared nobox values always use scalar lanes.
fn wire_value_shape(
    ws: &Workspace,
    target: FnBackend,
    ty: &crate::monoty::MonoTy,
    use_context: RepUse,
    guarantees: RepGuarantees,
) -> Option<ValueShape> {
    if let Some(shape) = crate::lower::mono_nobox_value_shape(ws, ty) {
        return Some(shape);
    }
    if guarantees.nobox || use_context == RepUse::TupleElement {
        return None;
    }
    let shape = crate::lower::mono_value_shape(ws, ty)?;
    if matches!(use_context, RepUse::Parameter | RepUse::Return)
        && payload_leaf_count(&shape) > WIDE_VALUE_PAYLOAD_LEAF_CUT
        && (!super::shape::planned_value(ws, shape.name).is_some_and(|plan| plan.gc_contains_nobox)
            || wide_gc_box(ws, target, shape.name))
    {
        return None;
    }
    Some(shape)
}

fn declared_nobox(ws: &Workspace, ty: &crate::monoty::MonoTy) -> bool {
    ty.peel_nullable()
        .0
        .nominal()
        .is_some_and(|name| crate::lower::is_nobox_nominal(ws, name))
}

impl RepArena {
    pub fn new(interner: Arc<crate::monoty::MonoInterner>) -> Self {
        static ARENA_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Self {
            plans: crate::append_vec::AppendVec::new(),
            by_key: RwLock::default(),
            arena_id: ARENA_GEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            interner,
            facts: Default::default(),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.plans.is_empty()
    }

    pub(crate) fn interner(&self) -> &crate::monoty::MonoInterner {
        &self.interner
    }

    fn type_facts(&self, ty: &crate::monoty::MonoTy) -> &TypeFacts {
        self.facts
            .get_or_init(self.interner.id_of(ty).index(), TypeFacts::default)
    }

    pub(crate) fn container_plan(
        &self,
        target: FnBackend,
        family: super::container::ContainerFamily,
        ty: &crate::monoty::MonoTy,
        derive: impl FnOnce() -> Result<super::container::ContainerPlan, RepError>,
    ) -> Result<super::container::ContainerPlan, RepError> {
        let slot = &self.type_facts(ty).containers[container_slot(target, family)];
        if let Some(plan) = slot.get() {
            return Ok(plan.clone());
        }
        let plan = derive()?;
        Ok(slot.get_or_init(|| plan).clone())
    }

    pub(crate) fn generation(&self) -> u64 {
        self.arena_id
    }

    fn lookup_key(&self, key: &RepKey) -> Option<RepId> {
        REP_READ_CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();
            cache.reset_for(self.arena_id);
            if let Some(id) = cache.by_key.get(key) {
                return Some(*id);
            }
            let id = self
                .by_key
                .read()
                .expect("RepArena lock poisoned")
                .get(key)
                .copied();
            if let Some(id) = id {
                cache.by_key.insert(key.clone(), id);
            }
            id
        })
    }

    fn alias_key(&self, key: RepKey, id: RepId) {
        self.by_key
            .write()
            .expect("RepArena lock poisoned")
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
        self.plans.len() == other.plans.len() && self.plans.iter().eq(other.plans.iter())
    }

    fn intern(&self, target: FnBackend, use_context: RepUse, layout: RepLayout) -> RepId {
        self.intern_with_facts(target, use_context, layout, None, false, false, false)
    }

    fn intern_with_facts(
        &self,
        target: FnBackend,
        use_context: RepUse,
        layout: RepLayout,
        storage: Option<ValueStorage>,
        column_eligible: bool,
        nobox: bool,
        prefer_scalar: bool,
    ) -> RepId {
        let key = RepKey::Layout(target, use_context, layout, storage, column_eligible, nobox);
        REP_READ_CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();
            cache.reset_for(self.arena_id);
            if let Some(id) = cache.by_key.get(&key) {
                return *id;
            }
            let id = {
                let mut by_key = self.by_key.write().expect("RepArena lock poisoned");
                match by_key.get(&key) {
                    Some(id) => *id,
                    None => {
                        let id = RepId {
                            arena: self.arena_id,
                            index: self.plans.len() as u32,
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
                        self.plans.push(RepPlan {
                            id,
                            target,
                            use_context,
                            layout,
                            storage,
                            column_eligible,
                            nobox,
                            prefer_scalar,
                        });
                        by_key.insert(key.clone(), id);
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
        shape: ValueShape,
    ) -> RepId {
        let key = RepKey::ValueShape(target, use_context, shape.clone());
        if let Some(id) = self.lookup_key(&key) {
            return id;
        }
        // The facts read only the nominal name, so every worker records the same answer.
        let (storage, column_eligible, prefer_scalar) = value_shape_facts(ws, shape.name);
        let nobox = crate::lower::is_nobox_nominal(ws, shape.name);
        let id = self.intern_with_facts(
            target,
            use_context,
            RepLayout::Leaves(shape.clone()),
            storage,
            column_eligible,
            nobox,
            prefer_scalar,
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
            self.intern_value_shape_plan(ws, target, RepUse::Value, child.clone());
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

    pub fn get(&self, id: RepId) -> &RepPlan {
        assert_eq!(
            id.arena, self.arena_id,
            "RepId must belong to its build arena"
        );
        self.plans
            .get(id.index as usize)
            .expect("RepId must belong to its build arena")
    }

    pub fn try_get(&self, id: RepId) -> Option<&RepPlan> {
        if id.arena != self.arena_id {
            return None;
        }
        self.plans.get(id.index as usize)
    }

    /// Late consumers keep the plan id they use; they must not rebuild one from a
    /// nominal declaration.
    pub(crate) fn value_plan_catalog(&self) -> Vec<(Ustr, RepId)> {
        self.plans
            .iter()
            .filter_map(|plan| match plan.layout() {
                RepLayout::Leaves(shape) => Some((shape.name, plan.id)),
                RepLayout::Opaque | RepLayout::Tuple(_) => None,
            })
            .collect()
    }

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

    pub(crate) fn native_buffer_element_ctype(
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
            crate::monoty::MonoTy::Int => Ok((crate::names::ustr("int"), None)),
            crate::monoty::MonoTy::Uint => Ok((crate::names::ustr("uint32_t"), None)),
            crate::monoty::MonoTy::Float => Ok((crate::names::ustr("float"), None)),
            crate::monoty::MonoTy::Bool => Ok((crate::names::ustr("bool"), None)),
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

    /// The labelled `__tostring` fields of a leaf-table value, read through its leaves.
    pub fn nominal_value_display_fields_required(
        &self,
        ws: &Workspace,
        target: FnBackend,
        name: Ustr,
    ) -> Result<Vec<(String, Vec<crate::lua_nesting::LuaDisplayPart>)>, RepError> {
        let id = self.plan_nominal_value(ws, target, name)?;
        let shape = self.value_shape(id).ok_or_else(|| RepError {
            span: None,
            message: format!("`{name}` has no physical leaf plan"),
        })?;
        Ok(super::struct_plan::lua_display_fields(ws, shape, ""))
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

    pub(crate) fn nominal_leaf_view_ctys(
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

    pub(crate) fn value_shape(&self, id: RepId) -> Option<&ValueShape> {
        match self.get(id).layout() {
            RepLayout::Leaves(shape) => Some(shape),
            _ => None,
        }
    }

    pub(super) fn layout(&self, id: RepId) -> &RepLayout {
        self.get(id).layout()
    }

    pub(crate) fn tuple_shape(&self, id: RepId) -> Option<&TupleLayout> {
        match self.get(id).layout() {
            RepLayout::Tuple(tuple) => Some(tuple),
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
        let want = crate::lower::value_leaf_paths(shape);
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

    /// `Opaque` is an explicit representation, not an absent answer.
    pub fn plan(
        &self,
        ws: &Workspace,
        target: FnBackend,
        ty: &crate::monoty::MonoTy,
        use_context: RepUse,
        facts: &RepUseFacts,
        guarantees: RepGuarantees,
    ) -> Result<RepId, RepError> {
        let slot = &self.type_facts(ty).reps
            [rep_slot(target, use_context, facts.tuple_spelled, guarantees)];
        let planned = slot.load(std::sync::atomic::Ordering::Acquire);
        if planned != 0 {
            return Ok(RepId {
                arena: self.arena_id,
                index: planned - 1,
            });
        }
        let id = self.plan_uncached(ws, target, ty, use_context, facts, guarantees)?;
        slot.store(id.index + 1, std::sync::atomic::Ordering::Release);
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
        if let (MonoTy::Tuple(parts), nullable) = ty.peel_nullable() {
            let slots = parts
                .iter()
                .map(|part| self.plan(ws, target, part, RepUse::TupleElement, facts, guarantees))
                .collect::<Result<Vec<_>, _>>()?;
            let splats = slots.iter().any(|id| self.get(*id).lanes(self) > 1);
            let return_slot = matches!(use_context, RepUse::Return);
            if (return_slot && !nullable)
                || (splats
                    && (return_slot || use_context == RepUse::TupleElement || facts.tuple_spelled))
            {
                return Ok(self.intern(
                    target,
                    use_context,
                    RepLayout::Tuple(TupleLayout { slots, nullable }),
                ));
            }
            if nullable && guarantees.nobox && !guarantees.allow_opaque {
                return Err(reject(
                    "nullable tuple cannot cross a nobox boundary without a physical tuple plan"
                        .into(),
                ));
            }
            return Ok(self.intern(target, use_context, RepLayout::Opaque));
        }
        match ty {
            MonoTy::Tuple(_) => unreachable!(),
            MonoTy::Nullable(_)
            | MonoTy::App {
                ctor: MonoCtor::Nominal(_),
                ..
            } => {
                let shape = wire_value_shape(ws, target, ty, use_context, guarantees);
                if let Some(shape) = shape {
                    return Ok(self.intern_value_shape_plan(ws, target, use_context, shape));
                }
                if guarantees.nobox && !guarantees.allow_opaque {
                    return Err(reject(format!(
                        "{} has no concrete nobox representation",
                        ty.render()
                    )));
                }
            }
            MonoTy::App { .. } if guarantees.nobox && !guarantees.allow_opaque => {
                return Err(reject(format!(
                    "{} cannot cross a nobox boundary",
                    ty.render()
                )));
            }
            MonoTy::App { .. }
            | MonoTy::Int
            | MonoTy::Uint
            | MonoTy::Float
            | MonoTy::Bool
            | MonoTy::String
            | MonoTy::Any
            | MonoTy::Unit
            | MonoTy::Never
            | MonoTy::Function { .. } => {}
        }
        Ok(self.intern(target, use_context, RepLayout::Opaque))
    }

    /// Boxes the widest eligible ordinary values until the signature fits; ties pick
    /// earlier positions. Remaining overflow is left to the frame-limit diagnostic.
    fn apply_lane_budget(
        &self,
        target: FnBackend,
        params: &mut [RepId],
        result: &mut Option<RepId>,
    ) {
        let lanes = |id: &RepId| self.get(*id).lanes(self);
        while params.iter().map(lanes).sum::<usize>() + result.as_ref().map_or(0, lanes)
            > SIGNATURE_SCALAR_LANE_BUDGET
        {
            let widest = params
                .iter()
                .enumerate()
                .map(|(index, id)| (index, *id, RepUse::Parameter))
                .chain(result.map(|id| (params.len(), id, RepUse::Return)))
                .filter(|(_, id, _)| {
                    let plan = self.get(*id);
                    matches!(plan.layout(), RepLayout::Leaves(_))
                        && !plan.nobox()
                        && !plan.prefer_scalar
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
        }
    }

    fn function_value(
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
        // A tuple parameter is spelled as one only when its Return plan splats.
        let params = fn_params.iter().map(|param| {
            let tuple_spelled = matches!(param.peel_nullable().0, crate::monoty::MonoTy::Tuple(_))
                && self
                    .get(self.plan(
                        ws,
                        target,
                        param,
                        RepUse::Return,
                        &RepUseFacts::default(),
                        RepGuarantees {
                            nobox: true,
                            allow_opaque: true,
                        },
                    )?)
                    .splats(self);
            Ok((Some(param), tuple_spelled))
        });
        let result =
            (!matches!(ret.as_ref(), crate::monoty::MonoTy::Unit)).then_some(Some(ret.as_ref()));
        self.function_from_signature(
            ws,
            target,
            params,
            result,
            !first_class,
            |_| nobox_only,
            true,
        )
    }

    /// Positions stay Opaque unless `plan_all` is set or they are tuple-spelled; a
    /// result is tuple-spelled when it is a tuple. `result` is `None` for no result.
    #[allow(clippy::too_many_arguments)]
    fn function_from_signature<'t>(
        &self,
        ws: &Workspace,
        target: FnBackend,
        params: impl IntoIterator<Item = Result<(Option<&'t crate::monoty::MonoTy>, bool), RepError>>,
        result: Option<Option<&'t crate::monoty::MonoTy>>,
        plan_all: bool,
        nobox: impl Fn(&crate::monoty::MonoTy) -> bool,
        scalar_allowed: bool,
    ) -> Result<FunctionRepPlan, RepError> {
        let params = params
            .into_iter()
            .map(|param| match param? {
                (Some(ty), tuple_spelled) if plan_all || tuple_spelled => {
                    self.plan_position(ws, target, ty, RepUse::Parameter, tuple_spelled, nobox(ty))
                }
                _ => Ok(self.intern(target, RepUse::Parameter, RepLayout::Opaque)),
            })
            .collect::<Result<Vec<_>, _>>()?;
        let result = result
            .map(|ty| {
                let tuple_spelled = ty.is_some_and(|ty| {
                    matches!(ty.peel_nullable().0, crate::monoty::MonoTy::Tuple(_))
                });
                match ty {
                    Some(ty) if plan_all || tuple_spelled => {
                        self.plan_result(ws, target, ty, tuple_spelled, nobox(ty))
                    }
                    _ => Ok(self.intern(target, RepUse::Return, RepLayout::Opaque)),
                }
            })
            .transpose()?;
        Ok(self.finish_function_plan(target, params, result, scalar_allowed))
    }

    fn finish_function_plan(
        &self,
        target: FnBackend,
        mut params: Vec<RepId>,
        mut result: Option<RepId>,
        scalar_allowed: bool,
    ) -> FunctionRepPlan {
        self.apply_lane_budget(target, &mut params, &mut result);
        let scalar_abi = scalar_allowed
            && (params
                .iter()
                .any(|id| !matches!(self.get(*id).layout(), RepLayout::Opaque))
                || result.is_some_and(|id| matches!(self.get(id).layout(), RepLayout::Leaves(_))));
        FunctionRepPlan {
            params,
            result,
            return_lanes: result.map_or(0, |id| self.get(id).lanes(self)),
            scalar_abi,
        }
    }

    fn plan_position(
        &self,
        ws: &Workspace,
        target: FnBackend,
        ty: &crate::monoty::MonoTy,
        use_context: RepUse,
        tuple_spelled: bool,
        nobox: bool,
    ) -> Result<RepId, RepError> {
        let facts = RepUseFacts {
            tuple_spelled,
            span: None,
        };
        let guarantees = RepGuarantees {
            nobox,
            allow_opaque: true,
        };
        self.plan(ws, target, ty, use_context, &facts, guarantees)
    }

    /// A nullable tuple returns one box or nil, so a stored tuple passes through
    /// unrepacked and a fresh one is one table the trace can sink.
    fn plan_result(
        &self,
        ws: &Workspace,
        target: FnBackend,
        ty: &crate::monoty::MonoTy,
        tuple_spelled: bool,
        nobox: bool,
    ) -> Result<RepId, RepError> {
        if let crate::monoty::MonoTy::Nullable(inner) = ty
            && matches!(inner.as_ref(), crate::monoty::MonoTy::Tuple(_))
        {
            return Ok(self.intern(target, RepUse::Return, RepLayout::Opaque));
        }
        self.plan_position(ws, target, ty, RepUse::Return, tuple_spelled, nobox)
    }

    fn function_from_declaration(
        &self,
        ws: &Workspace,
        target: FnBackend,
        f: &Function,
        subst: &SpecSubst,
        mode: Option<AbiMode>,
    ) -> Result<FunctionRepPlan, RepError> {
        let params = f
            .param_decls
            .iter()
            .map(|param| {
                let ty = declared_mono_type(ws, param.ty, subst)?;
                Ok((ty, declared_tuple_spelled(ws, param.ty)))
            })
            .collect::<Result<Vec<_>, RepError>>()?;
        let result = f
            .return_ty
            .map(|return_ty| declared_mono_type(ws, return_ty, subst))
            .transpose()?
            .filter(|ty| !matches!(ty, Some(crate::monoty::MonoTy::Unit)));
        self.function_from_signature(
            ws,
            target,
            params
                .iter()
                .map(|(ty, tuple_spelled)| Ok((ty.as_ref(), *tuple_spelled))),
            result.as_ref().map(Option::as_ref),
            mode.is_some(),
            |ty| mode.is_some_and(|mode| mode.nobox_only_for(ty)),
            mode.is_some(),
        )
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
        assert!(
            param_tuple_spelled.len() >= params.len(),
            "concrete signature is missing parameter ABI provenance"
        );
        self.function_from_signature(
            ws,
            target,
            params
                .iter()
                .zip(param_tuple_spelled)
                .map(|(ty, tuple_spelled)| Ok((Some(ty), *tuple_spelled))),
            result_ty.map(Some),
            mode.is_some(),
            |ty| mode.is_some_and(|mode| mode.nobox_only_for(ty)),
            mode.is_some(),
        )
        .expect("concrete ABI planner must accept a validated signature")
    }
}

pub fn owner_abi_plan(
    ws: &Workspace,
    scx: &ScalarCtx,
    plans: &crate::concrete::ConcretePlans,
    owner: &crate::instance::Instance,
) -> Result<BackendFnAbiPlanHandle, skull_parser::CompilerError> {
    owner_abi_plan_impl(ws, scx, plans, owner, None)
}

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
    if let Some(plan) = scx.abi_plan_cached(AbiQuestion::Owner, owner) {
        return Ok(plan);
    }
    let derived = Arc::new(if !requires_concrete_body_plan(ws, owner) {
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
    });
    scx.abi_plan_store(AbiQuestion::Owner, owner, &derived);
    Ok(derived)
}

/// JS keeps host mappings with no Rock declaration, so it retains the requested
/// instance.
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

/// The one memoized callee ABI question, so two consumers cannot ask different
/// ones.
pub(super) fn callee_abi_plan_for_instance(
    ws: &Workspace,
    scx: &ScalarCtx,
    plans: &crate::concrete::ConcretePlans,
    inst: &crate::instance::Instance,
) -> Result<BackendFnAbiPlanHandle, skull_parser::CompilerError> {
    if let Some(plan) = scx.abi_plan_cached(AbiQuestion::Callee, inst) {
        return Ok(plan);
    }
    let selected = selected_abi_instance(ws, scx, inst)?;
    let derived = if !requires_concrete_body_plan(ws, inst) {
        Arc::new(BackendFnAbiPlan::for_boundary_instance(ws, scx, &selected))
    } else {
        if let Some(error) = inst.too_deep_error(ws, scx.interner()) {
            return Err(error);
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
            owner_abi_plan_impl(ws, scx, plans, inst, Some(plan))?
        }
    };
    scx.abi_plan_store(AbiQuestion::Callee, inst, &derived);
    Ok(derived)
}

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

/// Foreign, comptime-expansion and native-required intrinsic declarations have an
/// ABI but no body plan: their backend supplies the body.
pub fn requires_concrete_body_plan(ws: &Workspace, inst: &crate::instance::Instance) -> bool {
    let f = &ws.module(inst.func.module).functions[inst.func.func];
    !f.is_extern
        && f.inline_body.is_none()
        && crate::intrinsics::expand_kind(f).is_none()
        && !(crate::intrinsics::is_intrinsic(f) && crate::intrinsics::is_native_required(f))
        && ws
            .module(inst.func.module)
            .bodies
            .contains_key(&bone_core::hir::BodyOwner::Function(inst.func.func))
}

#[derive(Clone, Copy, Debug)]
struct AbiMode {
    nobox_only: bool,
    fixed_nullable_transport: bool,
}

impl AbiMode {
    fn nobox_only_for(self, ty: &crate::monoty::MonoTy) -> bool {
        self.nobox_only
            || (self.fixed_nullable_transport
                && matches!(ty,
            crate::monoty::MonoTy::Nullable(inner) if matches!(inner.as_ref(),
                crate::monoty::MonoTy::App { ctor: crate::monoty::MonoCtor::Nominal(_), .. })))
    }
}

fn declared_tuple_spelled(ws: &Workspace, ty: bone_core::prelude::TyId) -> bool {
    match super::static_ty(ws, ty) {
        Some(bone_core::hir::TypeExpr::Tuple { .. }) => true,
        Some(bone_core::hir::TypeExpr::Nullable(inner)) => matches!(
            super::static_ty(ws, ty.with_idx(inner)),
            Some(bone_core::hir::TypeExpr::Tuple { .. })
        ),
        _ => false,
    }
}

fn declared_mono_type(
    ws: &Workspace,
    ty: bone_core::prelude::TyId,
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

/// Address-taken Rock bodies narrow to the `nobox` shapes an indirect caller can
/// derive from the fn type.
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
        scx.value_references.taken_by_ref.contains(&f.name)
    };
    Some(AbiMode {
        nobox_only,
        fixed_nullable_transport: f.inline_body.is_some()
            || name_is_fixed
            || f.name.as_str() == "main",
    })
}

#[derive(Clone, Debug)]
pub struct BackendFnAbiPlan {
    pub subject: crate::instance::Instance,
    /// Part of the key: the same body has a different ABI under GLSL than LuaJIT.
    pub target: FnBackend,
    /// Consumers keep these identities rather than rebuilding a shape from the
    /// declaration.
    representation: FunctionRepPlan,
    arena: Arc<RepArena>,
    transport: Option<ParameterTransport>,
    parameter_demand: Option<u64>,
    fingerprint: std::sync::OnceLock<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ParameterTransport {
    original: Vec<RepId>,
    lanes: Vec<usize>,
}

pub type BackendFnAbiPlanHandle = Arc<BackendFnAbiPlan>;

impl PartialEq for BackendFnAbiPlan {
    fn eq(&self, other: &Self) -> bool {
        self.subject == other.subject
            && self.target == other.target
            && self.representation == other.representation
            && self.transport == other.transport
            && self.parameter_demand == other.parameter_demand
            && self.arena.equivalent(&other.arena)
    }
}

impl Eq for BackendFnAbiPlan {}

/// A boxed value child is read through its declared field position, which a
/// narrowed parameter group no longer has.
fn keeps_opaque_value_child(ws: &Workspace, shape: &ValueShape, kept: &[ustr::Ustr]) -> bool {
    let Some(sid) = ws.nominals.struct_id(shape.name) else {
        return false;
    };
    let sd = &ws.modules[sid.module.index()].structs[sid.def];
    shape.fields.iter().any(|(name, field)| {
        matches!(field, FieldShape::Prim)
            && kept.contains(name)
            && sd
                .fields
                .iter()
                .find(|decl| decl.name == *name)
                .and_then(|decl| super::shape::nominal_name_from_ty(ws, decl.ty))
                .is_some_and(|(child, _)| super::shape::planned_value(ws, child).is_some())
    })
}

pub(super) fn expanded_nullable_parameter_shape(ws: &Workspace, shape: &ValueShape) -> ValueShape {
    let mut expanded = shape.clone();
    if shape.nullable {
        return expanded;
    }
    if let Some(sid) = ws.nominals.struct_id(shape.name) {
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
            let Some(mut child) =
                super::shape::planned_value(ws, child).and_then(|p| p.scalar_shape().cloned())
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
    pub fn for_boundary_instance(
        ws: &Workspace,
        scx: &ScalarCtx,
        inst: &crate::instance::Instance,
    ) -> Self {
        let subst = inst.subst(ws, scx.interner());
        Self::assemble(ws, scx, inst.clone(), &subst)
    }

    pub fn for_concrete_plan(ws: &Workspace, scx: &ScalarCtx, plan: &ConcreteBodyPlan) -> Self {
        let inst = plan
            .function_instance()
            .expect("a function ABI plan needs an ordinary concrete owner");
        Self::for_concrete_signature(ws, scx, inst, &plan.signature)
    }

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
        let arena = scx.rep_arena().clone();
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
            parameter_demand: None,
            fingerprint: std::sync::OnceLock::new(),
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

    pub fn for_boundary_declaration(ws: &Workspace, scx: &ScalarCtx, decl: FuncRef) -> Self {
        Self::for_boundary_instance(ws, scx, &crate::instance::Instance::base(decl))
    }

    /// `nobox_only` narrows a non-first-class value to `nobox` shapes, as a
    /// function value taken by value requires.
    pub fn for_function_value(
        ws: &Workspace,
        scx: &ScalarCtx,
        ty: &crate::monoty::MonoTy,
        nobox_only: bool,
    ) -> Result<FunctionRepPlan, RepError> {
        let first_class = scx.values_first_class;
        scx.rep_arena()
            .function_value(ws, scx.target, ty, nobox_only && !first_class, first_class)
    }

    fn assemble(
        ws: &Workspace,
        scx: &ScalarCtx,
        subject: crate::instance::Instance,
        subst: &SpecSubst,
    ) -> Self {
        let decl = subject.func;
        let f = &ws.module(decl.module).functions[decl.func];
        let arena = scx.rep_arena().clone();
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
            parameter_demand: None,
            fingerprint: std::sync::OnceLock::new(),
        }
    }

    fn permits_parameter_transport(
        &self,
        ws: &Workspace,
        scx: &ScalarCtx,
        with_defaults: bool,
    ) -> bool {
        self.uses_scalar_abi() && self.transport_candidate(ws, scx, with_defaults)
    }

    fn transport_candidate(&self, ws: &Workspace, scx: &ScalarCtx, with_defaults: bool) -> bool {
        let f = &ws.module(self.subject.func.module).functions[self.subject.func.func];
        !(scx.values_first_class
            || crate::mangle::is_exempt(f)
            || !matches!(self.target, FnBackend::Lua | FnBackend::Any)
            || scx.value_references.taken_by_ref.contains(&f.name)
            || f.is_extern
            || f.inline_body.is_some()
            || f.is_coroutine
            || f.is_variadic
            || f.annotations.iter().any(|a| !a.is_builtin("noalloc"))
            || (!with_defaults && f.num_required != f.param_decls.len()))
    }

    // A defaulted parameter keeps its declared wire, which its default fill and
    // omitted-argument nils are built against.
    fn expand_nullable_parameter_children(&mut self, ws: &Workspace, scx: &ScalarCtx) {
        self.fingerprint = std::sync::OnceLock::new();
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
                rep.prefer_scalar,
            );
        }
    }

    fn narrow_parameter_transport(&mut self, ws: &Workspace, scx: &ScalarCtx) {
        self.fingerprint = std::sync::OnceLock::new();
        if !self.transport_candidate(ws, scx, false) || self.representation.params.is_empty() {
            return;
        }
        // Every call site is a direct call lowered against this plan, so the
        // transport may follow the body.
        let demand = super::parameter_demand::of_function(ws, self.subject.func);
        self.parameter_demand = Some(super::parameter_demand::key(&demand));
        let Some(demand) = demand else {
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
            let struct_leaves = match rep.layout() {
                RepLayout::Leaves(shape)
                    if !shape.nullable && ws.nominals.struct_id(shape.name).is_some() =>
                {
                    Some(shape)
                }
                _ => None,
            };
            match (&demand[i], struct_leaves) {
                (Some(fields), None) if fields.is_empty() => {
                    self.representation.params[i] = self.arena.intern_with_facts(
                        self.target,
                        RepUse::Parameter,
                        RepLayout::Tuple(TupleLayout {
                            slots: Vec::new(),
                            nullable: false,
                        }),
                        None,
                        false,
                        false,
                        false,
                    );
                }
                (Some(fields), Some(shape))
                    if fields
                        .iter()
                        .all(|field| shape.fields.iter().any(|(name, _)| name == field))
                        && !keeps_opaque_value_child(ws, shape, fields) =>
                {
                    let mut narrowed = shape.clone();
                    narrowed.fields.clear();
                    let mut leaf = offset;
                    for (name, field) in &shape.fields {
                        let n = field.leaf_count();
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
                            rep.prefer_scalar,
                        );
                    }
                }
                _ => lanes.extend(offset..offset + width),
            }
            offset += width;
        }
        if lanes.len() != offset {
            self.representation.scalar_abi = true;
            self.transport = Some(ParameterTransport { original, lanes });
        }
    }

    /// A projected layout is not a complete value and must never be boxed.
    pub(super) fn argument_shapes(&self) -> Vec<&RepLayout> {
        let reps = self.transport.as_ref().map_or_else(
            || self.param_reps(),
            |transport| transport.original.as_slice(),
        );
        reps.iter().map(|&id| self.arena.layout(id)).collect()
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

    pub(super) fn dropped_opaque_parameters(&self) -> Vec<usize> {
        let Some(transport) = &self.transport else {
            return Vec::new();
        };
        transport
            .original
            .iter()
            .zip(&self.representation.params)
            .enumerate()
            .filter(|(_, (original, current))| {
                matches!(self.arena.get(**original).layout(), RepLayout::Opaque)
                    && self.arena.get(**current).lanes(&self.arena) == 0
            })
            .map(|(i, _)| i)
            .collect()
    }

    pub fn parameter_demand_key(&self) -> Option<u64> {
        self.parameter_demand
    }

    pub fn decl(&self) -> FuncRef {
        self.subject.func
    }

    pub(super) fn uses_scalar_abi(&self) -> bool {
        self.representation.uses_scalar_abi()
    }

    pub fn representation(&self) -> &FunctionRepPlan {
        &self.representation
    }

    pub(crate) fn logical_fingerprint(&self) -> String {
        self.fingerprint
            .get_or_init(|| {
                use std::fmt::Write as _;
                let mut out = String::with_capacity(160);
                let _ = write!(out, "target={:?};", self.target);
                self.representation
                    .write_logical_fingerprint(&self.arena, &mut out);
                out
            })
            .clone()
    }

    pub fn rep_plan(&self, id: RepId) -> &RepPlan {
        self.arena.get(id)
    }

    pub(super) fn param_reps(&self) -> &[RepId] {
        if self.uses_scalar_abi() {
            self.representation.params()
        } else {
            &[]
        }
    }

    pub(super) fn ret_shape(&self) -> Option<&ValueShape> {
        self.representation
            .result()
            .and_then(|id| self.arena.value_shape(id))
    }

    pub(crate) fn ret_tuple(&self) -> Option<&TupleLayout> {
        self.representation
            .result()
            .and_then(|id| self.arena.tuple_shape(id))
    }

    pub fn bound_next_layout(
        &self,
        result: Option<crate::monoty::MonoTyId>,
    ) -> Result<&TupleLayout, RepError> {
        let reject = |message| RepError {
            span: None,
            message,
        };
        let result =
            result.ok_or_else(|| reject("a concrete bound witness has no result type".into()))?;
        let shape = self.ret_tuple().ok_or_else(|| {
            reject(format!(
                "bound witness result `{}` has no tuple return plan",
                self.arena.interner.get_ref(result).render()
            ))
        })?;
        match shape.slots.len() {
            0 => Err(reject(
                "bound witness return plan has no success flag and element slots".into(),
            )),
            1 => Err(reject(
                "bound witness return plan has no element slot".into(),
            )),
            _ => Ok(shape),
        }
    }

    /// Physical result lanes. JS transports lanes after the first through its
    /// scratch bank.
    pub fn result_lanes(&self) -> usize {
        self.representation.return_lanes()
    }

    /// Leaf names `<base>__<leaf>` that a hand-written `@scalar_abi` foreign body
    /// references, in `repr_to_leaves` order to match the call-site splat.
    pub fn lua_param_names(&self, ws: &Workspace) -> Option<Vec<String>> {
        self.scalar_param_labels(ws, false)
    }

    pub(super) fn scalar_param_labels(&self, ws: &Workspace, origins: bool) -> Option<Vec<String>> {
        if !self.uses_scalar_abi() {
            return None;
        }
        let f = &ws.module(self.decl().module).functions[self.decl().func];
        let mut labels = Vec::new();
        for (pd, &id) in f.param_decls.iter().zip(self.param_reps()) {
            slot_param_labels(&self.arena, pd.name.as_str(), id, origins, &mut labels);
        }
        Some(labels)
    }

    /// Per declared parameter, so a defaulted value parameter counts as its whole
    /// leaf group.
    pub fn param_leaf_widths(&self) -> Option<Vec<usize>> {
        self.uses_scalar_abi().then(|| {
            self.param_reps()
                .iter()
                .map(|&id| self.arena.get(id).lanes(&self.arena))
                .collect()
        })
    }
}

fn slot_param_labels(
    arena: &RepArena,
    base: &str,
    id: RepId,
    origins: bool,
    out: &mut Vec<String>,
) {
    let sep = if origins { "." } else { "__" };
    match arena.layout(id) {
        RepLayout::Opaque => out.push(if origins {
            format!("the declared param `{base}`")
        } else {
            base.to_string()
        }),
        RepLayout::Leaves(shape) => {
            out.extend(leaf_names(shape, base, sep).into_iter().map(|path| {
                if origins {
                    format!("the leaf `{path}` of value param `{base}`")
                } else {
                    path
                }
            }))
        }
        RepLayout::Tuple(inner) => {
            // The presence lane precedes the tuple's value slots.
            if inner.nullable {
                out.push(if origins {
                    format!("the presence of nullable tuple param `{base}`")
                } else {
                    format!("{base}{sep}p")
                });
            }
            for (i, &id) in inner.slots.iter().enumerate() {
                slot_param_labels(arena, &format!("{base}{sep}{i}"), id, origins, out);
            }
        }
    }
}

pub(crate) fn rep_logical_fingerprint(arena: &RepArena, id: RepId) -> String {
    rep_logical_fingerprint_shared(arena, id).to_string()
}

fn rep_logical_fingerprint_shared(arena: &RepArena, id: RepId) -> Arc<str> {
    let index = id.index as usize;
    let cached = REP_READ_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        cache.reset_for(arena.arena_id);
        cache.fingerprints.get(index).cloned().flatten()
    });
    if let Some(fingerprint) = cached {
        return fingerprint;
    }
    let fingerprint: Arc<str> = render_rep_logical_fingerprint(arena, id).into();
    REP_READ_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        cache.reset_for(arena.arena_id);
        if cache.fingerprints.len() <= index {
            cache.fingerprints.resize(index + 1, None);
        }
        cache.fingerprints[index] = Some(Arc::clone(&fingerprint));
    });
    fingerprint
}

fn render_rep_logical_fingerprint(arena: &RepArena, id: RepId) -> String {
    let plan = arena.get(id);
    let layout = match plan.layout() {
        RepLayout::Opaque => "opaque".to_string(),
        RepLayout::Leaves(_) => format!(
            "leaves:{:?}:{:?}",
            plan.leaf_paths().unwrap_or_default(),
            plan.presence_flags().unwrap_or_default()
        ),
        RepLayout::Tuple(TupleLayout { slots, nullable }) => format!(
            "tuple(nullable={nullable};[{}])",
            slots
                .iter()
                .map(|slot| rep_logical_fingerprint_shared(arena, *slot))
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
