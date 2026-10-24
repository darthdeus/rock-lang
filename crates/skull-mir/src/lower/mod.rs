//! bone HIR -> skull MIR lowering (Phase 1: the straight-line spine).
//!
//! A type-directed mechanical walk over bone's already-resolved HIR (names,
//! overloads, operators, and types are all decided by `semantic_pass`). A
//! `Builder` holds a cursor on the "current block" and appends instructions;
//! `lower_expr` returns the [`Idx<Inst>`] that holds the expression's value.
//!
//! Variables (`let`/param) become mutable [`Cell`]s; a `Var` read is an
//! [`Inst::Load`], a binding's initializer an [`Inst::Store`]. Arithmetic
//! arrives as `__op_*` calls (rock's desugar) and is dispatched to **typed**
//! ops at build time (decision 8): `int`+`int` -> [`Inst::IntAdd`], `float` ->
//! [`Inst::FloatAdd`], `string`+`+` -> [`Inst::StringConcat`].
//!
//! Phase 1 is straight-line ONLY: control flow (`if`/`&&`/`??`/loops/`match`),
//! coroutines, calls-to-`FuncRef`, and aggregates are not here yet — they
//! `gap` (return a clear error) and land in later phases. The gate is the real
//! corpus, not this comment.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;
use std::sync::OnceLock;
use bone_core::prelude::Instant;

use bone_semantic::hir::{
    BinOp, BindingInfo, BindingPattern, Body, BodyOwner, BuiltinTy, Callee, Expr, Function,
    MatchArm, Pattern, Statement, TyId, TypeCtor, TypeExpr, VariantArgs, VariantPayload,
};
use ustr::Ustr;
use bone_semantic::inference::{BodyInference, Workspace};
use bone_semantic::prelude::{BoundNext, ForIterInfo, ModuleId, Span};
use comfy_la_arena::{Idx, IdxRange, RawIdx};
use skull_parser::CompilerError;

use crate::intrinsics::ExpandKind;
use crate::mir::*;
use crate::concrete::{ConcreteBodyPlan, ConcreteOwner, ConcretePlans, ConcreteTypeTables};
use crate::monoty::{MonoCtor, MonoTy, MonoTyId};

type ConcreteTypeCache = RefCell<rustc_hash::FxHashMap<MonoTyId, Rc<MonoTy>>>;

#[derive(Clone)]
pub(crate) enum CallParamDefault {
    Scalar(Lit),
    Vector { shape: ValueShape, literals: Vec<Lit> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum ConcreteMonoConsumer {
    GlobalPlan = 0,
    CallParameter = 1,
    CoroutineChannel = 2,
    ReturnValidation = 3,
    ReturnLowering = 4,
    NoboxBoundary = 5,
    Dispatch = 6,
}

impl ConcreteMonoConsumer {
    pub const COUNT: usize = 7;
    pub const ALL: [Self; Self::COUNT] = [
        Self::GlobalPlan,
        Self::CallParameter,
        Self::CoroutineChannel,
        Self::ReturnValidation,
        Self::ReturnLowering,
        Self::NoboxBoundary,
        Self::Dispatch,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::GlobalPlan => "global-plan",
            Self::CallParameter => "call-parameter",
            Self::CoroutineChannel => "coroutine-channel",
            Self::ReturnValidation => "return-validation",
            Self::ReturnLowering => "return-lowering",
            Self::NoboxBoundary => "nobox-boundary",
            Self::Dispatch => "dispatch",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ConcreteMonoConsumerStats {
    pub requests: usize,
    pub cloned_nodes: usize,
    pub explicit_owned_escapes: usize,
}

fn gap(msg: impl std::fmt::Display) -> CompilerError {
    CompilerError::new_no_location(format!("skull-mir: lowering gap: {msg}"))
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LowerAttribution {
    pub concrete_mono_requests: usize,
    pub concrete_mono_cache_hits: usize,
    pub concrete_mono_derivations: usize,
    pub concrete_mono_unique_ids: usize,
    pub concrete_mono_cloned_nodes: usize,
    pub concrete_mono_interner_miss_cloned_nodes: usize,
    pub concrete_mono_cache_insertion_cloned_nodes: usize,
    pub concrete_mono_cache_hit_cloned_nodes: usize,
    pub concrete_mono_explicit_owned_escape_cloned_nodes: usize,
    pub concrete_mono_explicit_owned_escapes: usize,
    pub concrete_mono_lookup_ns: u64,
    pub concrete_mono_cache_lookup_ns: u64,
    pub concrete_mono_shared_handle_requests: usize,
    pub concrete_mono_clone_self_ns: u64,
    pub concrete_mono_timed_requests: usize,
    pub concrete_mono_by_consumer: [ConcreteMonoConsumerStats; ConcreteMonoConsumer::COUNT],
    pub abi_param_copy_volume: usize,
    pub abi_param_shared_views: usize,
    pub mir_instruction_allocations: usize,
    pub mir_instruction_reallocations: usize,
    pub mir_instruction_reallocation_ns: u64,
    pub mir_instruction_capacity: usize,
    pub mir_cell_allocations: usize,
    pub mir_cell_reallocations: usize,
    pub mir_cell_reallocation_ns: u64,
    pub mir_cell_capacity: usize,
    pub mir_block_allocations: usize,
    pub mir_block_reallocations: usize,
    pub mir_block_reallocation_ns: u64,
    pub mir_block_capacity: usize,
}

impl LowerAttribution {
    pub(crate) fn add_assign(&mut self, other: &Self) {
        self.concrete_mono_requests += other.concrete_mono_requests;
        self.concrete_mono_cache_hits += other.concrete_mono_cache_hits;
        self.concrete_mono_derivations += other.concrete_mono_derivations;
        self.concrete_mono_unique_ids += other.concrete_mono_unique_ids;
        self.concrete_mono_cloned_nodes += other.concrete_mono_cloned_nodes;
        self.concrete_mono_interner_miss_cloned_nodes +=
            other.concrete_mono_interner_miss_cloned_nodes;
        self.concrete_mono_cache_insertion_cloned_nodes +=
            other.concrete_mono_cache_insertion_cloned_nodes;
        self.concrete_mono_cache_hit_cloned_nodes += other.concrete_mono_cache_hit_cloned_nodes;
        self.concrete_mono_explicit_owned_escape_cloned_nodes +=
            other.concrete_mono_explicit_owned_escape_cloned_nodes;
        self.concrete_mono_explicit_owned_escapes += other.concrete_mono_explicit_owned_escapes;
        self.concrete_mono_lookup_ns += other.concrete_mono_lookup_ns;
        self.concrete_mono_cache_lookup_ns += other.concrete_mono_cache_lookup_ns;
        self.concrete_mono_shared_handle_requests += other.concrete_mono_shared_handle_requests;
        self.concrete_mono_clone_self_ns += other.concrete_mono_clone_self_ns;
        self.concrete_mono_timed_requests += other.concrete_mono_timed_requests;
        for (total, child) in self
            .concrete_mono_by_consumer
            .iter_mut()
            .zip(other.concrete_mono_by_consumer.iter())
        {
            total.requests += child.requests;
            total.cloned_nodes += child.cloned_nodes;
            total.explicit_owned_escapes += child.explicit_owned_escapes;
        }
        self.abi_param_copy_volume += other.abi_param_copy_volume;
        self.abi_param_shared_views += other.abi_param_shared_views;
        self.mir_instruction_allocations += other.mir_instruction_allocations;
        self.mir_instruction_reallocations += other.mir_instruction_reallocations;
        self.mir_instruction_reallocation_ns += other.mir_instruction_reallocation_ns;
        self.mir_instruction_capacity += other.mir_instruction_capacity;
        self.mir_cell_allocations += other.mir_cell_allocations;
        self.mir_cell_reallocations += other.mir_cell_reallocations;
        self.mir_cell_reallocation_ns += other.mir_cell_reallocation_ns;
        self.mir_cell_capacity += other.mir_cell_capacity;
        self.mir_block_allocations += other.mir_block_allocations;
        self.mir_block_reallocations += other.mir_block_reallocations;
        self.mir_block_reallocation_ns += other.mir_block_reallocation_ns;
        self.mir_block_capacity += other.mir_block_capacity;
    }

    fn record_concrete_mono_request(&mut self, consumer: ConcreteMonoConsumer) {
        self.concrete_mono_requests += 1;
        let stats = &mut self.concrete_mono_by_consumer[consumer as usize];
        stats.requests += 1;
    }

    fn record_concrete_mono_cache_hit(
        &mut self,
        consumer: ConcreteMonoConsumer,
        cloned_nodes: usize,
        clone_self_ns: u64,
        cache_lookup_ns: u64,
        lookup_ns: u64,
    ) {
        self.concrete_mono_cache_hits += 1;
        self.concrete_mono_cloned_nodes += cloned_nodes;
        self.concrete_mono_cache_hit_cloned_nodes += cloned_nodes;
        self.concrete_mono_cache_lookup_ns += cache_lookup_ns;
        self.concrete_mono_lookup_ns += lookup_ns;
        self.concrete_mono_clone_self_ns += clone_self_ns;
        let stats = &mut self.concrete_mono_by_consumer[consumer as usize];
        stats.cloned_nodes += cloned_nodes;
    }

    fn record_concrete_mono_derivation(
        &mut self,
        consumer: ConcreteMonoConsumer,
        interner_miss_nodes: usize,
        insertion_nodes: usize,
        lookup_ns: u64,
        cache_lookup_ns: u64,
        clone_self_ns: u64,
    ) {
        self.concrete_mono_derivations += 1;
        self.concrete_mono_unique_ids += 1;
        self.concrete_mono_cloned_nodes += interner_miss_nodes + insertion_nodes;
        self.concrete_mono_interner_miss_cloned_nodes += interner_miss_nodes;
        self.concrete_mono_cache_insertion_cloned_nodes += insertion_nodes;
        self.concrete_mono_lookup_ns += lookup_ns;
        self.concrete_mono_cache_lookup_ns += cache_lookup_ns;
        self.concrete_mono_clone_self_ns += clone_self_ns;
        let stats = &mut self.concrete_mono_by_consumer[consumer as usize];
        stats.cloned_nodes += interner_miss_nodes + insertion_nodes;
    }

    fn record_concrete_mono_owned_escape(
        &mut self,
        consumer: ConcreteMonoConsumer,
        cloned_nodes: usize,
        clone_self_ns: u64,
    ) {
        self.concrete_mono_cloned_nodes += cloned_nodes;
        self.concrete_mono_explicit_owned_escape_cloned_nodes += cloned_nodes;
        self.concrete_mono_explicit_owned_escapes += 1;
        self.concrete_mono_clone_self_ns += clone_self_ns;
        let stats = &mut self.concrete_mono_by_consumer[consumer as usize];
        stats.cloned_nodes += cloned_nodes;
        stats.explicit_owned_escapes += 1;
    }
}

pub(crate) fn lower_attribution_timing_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("ROCK_LOWER_CENSUS").is_ok_and(|value| value == "1")
            || std::env::var("ROCK_LOG").is_ok_and(|value| value == "debug")
    })
}

fn mono_ty_node_count(ty: &MonoTy) -> usize {
    match ty {
        MonoTy::Int
        | MonoTy::Uint
        | MonoTy::Float
        | MonoTy::Bool
        | MonoTy::String
        | MonoTy::Any
        | MonoTy::Unit
        | MonoTy::Never => 1,
        MonoTy::Nullable(inner) => 1 + mono_ty_node_count(inner),
        MonoTy::Tuple(parts) | MonoTy::App { params: parts, .. } => {
            1 + parts.iter().map(mono_ty_node_count).sum::<usize>()
        }
        MonoTy::Function { params, ret } => {
            1 + params.iter().map(mono_ty_node_count).sum::<usize>() + mono_ty_node_count(ret)
        }
    }
}

fn clone_mono_ty_timed(ty: &MonoTy, timing: bool) -> (MonoTy, u64) {
    let start = timing.then(Instant::now);
    let cloned = ty.clone();
    let elapsed = start
        .map(|start| start.elapsed().as_nanos() as u64)
        .unwrap_or_default();
    (cloned, elapsed)
}

fn finish_lower_attribution(mut f: MirFunc, mut stats: LowerAttribution) -> MirFunc {
    stats.mir_instruction_allocations += f.insts.len();
    stats.mir_instruction_capacity += f.insts.capacity();
    stats.mir_cell_allocations += f.cells.len();
    stats.mir_cell_capacity += f.cells.capacity();
    stats.mir_block_allocations += f.blocks.len();
    stats.mir_block_capacity += f.blocks.capacity();
    stats.mir_instruction_reallocations += f.lower_attribution.mir_instruction_reallocations;
    stats.mir_instruction_reallocation_ns += f.lower_attribution.mir_instruction_reallocation_ns;
    stats.mir_cell_reallocations += f.lower_attribution.mir_cell_reallocations;
    stats.mir_cell_reallocation_ns += f.lower_attribution.mir_cell_reallocation_ns;
    stats.mir_block_reallocations += f.lower_attribution.mir_block_reallocations;
    stats.mir_block_reallocation_ns += f.lower_attribution.mir_block_reallocation_ns;
    f.lower_attribution = stats;
    f
}

pub(crate) fn required_wire_slot<T>(
    slots: &[T],
    index: usize,
    variadic: bool,
    context: impl std::fmt::Display,
    to_slot: impl Fn(&T) -> RetSlot,
    missing: impl FnOnce(String) -> CompilerError,
) -> Result<RetSlot, CompilerError> {
    slots
        .get(index)
        .or_else(|| variadic.then(|| slots.last()).flatten())
        .map(to_slot)
        .ok_or_else(|| missing(format!("{context} has no ABI slot {index}")))
}
// --- scalarized value ABI (rock parity) -------------------------------------
//
// A `value` struct is passed/returned as its flattened primitive LEAF scalars
// (no per-call cdata at the ABI boundary), reconstructed where a real value is
// needed. The static shape drives flatten/rebuild; the runtime carrier `ValueRepr`
// threads the unboxed form through lowering; `pack` is the single place a Group
// becomes a real `<name>_ct(...)` cdata (an escape). See the plan doc.

/// Whether a fn with backend `b` can serve codegen `target`: an `Any` (portable
/// Rock) body serves every target, else the backend must match exactly.
fn serves_target(b: bone_semantic::hir::FnBackend, target: bone_semantic::hir::FnBackend) -> bool {
    use bone_semantic::hir::FnBackend;
    b == FnBackend::Any || b == target
}

/// `Some(k)` iff `name` is a GC-free `value` struct whose every flattened
/// primitive leaf is `float` (recursively), where `k` is the leaf count; `None`
/// otherwise (not such a struct, or any int/bool/enum leaf). The JS backend uses
/// it to lower a `NativeBuffer<Vertex>` (all-float value struct, e.g. the vertex
/// pool) to one flat `Float32Array(n * k)` -- JS has no per-struct ctype.

/// Value/enum REPRESENTATION analysis over a `Workspace` -- what a type looks
/// like, as opposed to how a body lowers. Re-exported flat so the ~168 external
/// `skull_mir::lower::*` call sites keep their paths.
mod parameter_demand;
mod shape;
pub use shape::*;

/// The backend ABI of one emitted body. The scalar-ABI POLICY lives inside and
/// is private to it, so a definition and the call that targets it cannot reach
/// it from two places.
mod abi;
pub use abi::{
    boundary_requires_boxed_representation, declared_boundary_class, owner_abi_plan,
    owner_abi_plan_for_body, requires_concrete_body_plan, selected_sibling_abi_plan,
    AbiPlanKey, AbiPlanMemoStats, BackendFnAbiPlan, BackendFnAbiPlanHandle, BoundaryClass,
    FunctionRepPlan, GlobalPlan,
    RepArena, RepArenaStats, RepError,
    RepId, RepLayout,
    RepGuarantees, RepPlan, RepUse, RepUseFacts,
    SIGNATURE_SCALAR_LANE_BUDGET, WIDE_VALUE_PAYLOAD_LEAF_CUT,
};

mod struct_plan;
pub use struct_plan::{cdata_leaf_alias, gc_table_fields, struct_field_plan, GcTableField, StructFieldPlan, StructRegistryPlan};

mod coroutine;
pub use coroutine::CoroutinePlan;

/// The `impl Builder` walk, split by topic. Each is a plain `impl Builder<'_>`
/// block in a child module -- Rust allows the impl to be spread across the
/// crate, and a child sees this module's private items, so nothing needed
/// widening to make the split.
mod call;
mod container;
pub use container::{ContainerFamily, ContainerPlan, ContainerStorage, PlaceBundle};
mod control;
mod dispatch;
pub use dispatch::{
    dispatched_callee_decl, dispatched_callee_decls,
    DispatchCtx, EqRoute, FormatPlan, HolePlan, StringifyRoute, StructuralRender,
};
mod embed;
mod expr;
mod flat;
mod flat_ref;
use flat_ref::{FlatArgCapture, FlatCallOut, FlatRefKey, FlatStorage, FlatVariants, FlatVariantsHandle};
mod format;
pub use format::{format_tree, FmtNode, FmtPayload, FmtVariant};
mod interp;
mod kinds;
mod loops;
mod nobox;
mod pattern;
mod resolve;
pub use resolve::{
    backend_variant, backend_variant_group, backend_variant_instance, canonical_variant,
    sibling_variant, NoServingBinding,
};
mod stmt;
mod storage;
mod structs;
mod types;

// A flattened child field's physical slots are DOTTED parent keys, the same
// encoding `embed` uses. `.` is
// not an identifier character, so a slot key can never equal a legal user field
// name -- which the `__`-based encoding this replaced could: an enum field `sig`
// and a user field `sig__tag` both wrote `sig__tag`, so the table ctor emitted
// the key twice and Lua's last-write-wins clobbered the discriminant, silently
// breaking every match on that enum.
//
// The three forms are disjoint by their suffix: `#` for the discriminant (not an
// identifier character and not a digit), all-digits for a positional payload (an
// identifier cannot start with a digit), an identifier for a named payload. They
// cannot collide with an `embed` leaf key either, because both are prefixed by
// the owning parent FIELD name and a field name denotes exactly one field.

/// Re-key relative embed leaf paths under an owning field: `[("x", v)]` under
/// `p` becomes `[("p.x", v)]`. Relative paths let a leaf list be computed once
/// and attached wherever the child is spliced.
fn prefixed(field: Ustr, leaves: Vec<(Ustr, Idx<Inst>)>) -> Vec<(Ustr, Idx<Inst>)> {
    leaves
        .into_iter()
        .map(|(k, v)| (Ustr::from(&format!("{field}.{k}")), v))
        .collect()
}

/// The parent-table slot holding a flattened enum field's discriminant (variant
/// name string), e.g. field `sig` -> `sig.#`.
fn flat_tag_slot(field: Ustr) -> Ustr {
    Ustr::from(format!("{field}.#").as_str())
}

/// The parent-table slot holding a flattened enum field's `i`th positional payload,
/// e.g. field `sig` payload 0 -> `sig.0`.
fn flat_payload_slot(field: Ustr, i: usize) -> Ustr {
    Ustr::from(format!("{field}.{i}").as_str())
}

/// The parent-table slot holding a flattened enum field's named (record) payload,
/// e.g. field `shape` record field `r` -> `shape.r`.
fn flat_named_slot(field: Ustr, rec_field: Ustr) -> Ustr {
    Ustr::from(format!("{field}.{rec_field}").as_str())
}

/// Every relative slot a flattened enum field occupies: discriminant, positional
/// payloads, named payloads. A flat-enum field is MULTI-SLOT, so any site walking
/// a struct's physical leaves must expand it -- treating the field name as one
/// leaf silently splits the field across two encodings (the store writes
/// `f.#`/`f.0`, the walker touches `f`).
fn flat_slot_names(field: Ustr, shape: &EnumFlatShape) -> Vec<Ustr> {
    let mut out = vec![flat_tag_slot(field)];
    for i in 0..shape.tuple_width() {
        out.push(flat_payload_slot(field, i));
    }
    for rf in &shape.record_fields {
        out.push(flat_named_slot(field, *rf));
    }
    out
}

/// A flattened enum value's runtime cells: the discriminant + every union slot
/// (positional tuple slots and named record slots), nil where unused.
#[derive(Clone)]
struct FlatCells {
    tag: Idx<Inst>,
    positional: Vec<Idx<Inst>>,
    named: Vec<(Ustr, Idx<Inst>)>,
}

/// One positional payload of a flat-enum construction: a source argument,
/// lowered in place so it keeps its evaluation order, or a value the compiler
/// already lowered (a `@struct_expand` field).
enum FlatPayload {
    Expr(Idx<Expr>),
    Repr(ValueRepr),
}

/// A `match`/`if let`/`while let` scrutinee: either an ordinary lowered value, or a
/// FLATTENED enum field still living as `parent.{prefix}__tag`/`__i` slots. The
/// flat form lets the tag test + payload binds read the parent's slots directly --
/// no box -- and only materializes (`scrut_value`) on a whole-value path.
#[derive(Clone)]
enum Scrut {
    Val(Idx<Inst>),
    /// A tuple expression whose elements still carry their scalar
    /// representations. Tuple patterns consume these directly; a whole-value
    /// bind materializes the ordinary tuple and any escaping element.
    Tuple(Vec<Scrut>),
    Flat {
        parent: Idx<Inst>,
        prefix: Ustr,
        shape: EnumFlatShape,
    },
    /// A flat enum already threaded as scalar cells (a scalarized param/return or a
    /// constructed-and-matched temporary) -- tag + payload values held directly, so
    /// the tag test + payload binds need no box and no slot read.
    Cells {
        tag: Idx<Inst>,
        positional: Vec<Idx<Inst>>,
        named: Vec<(Ustr, Idx<Inst>)>,
        shape: EnumFlatShape,
    },
}

/// Workspace-wide scalar-ABI context. Address-taken functions use the
/// type-derived fn-value ABI at their definitions and direct calls, so all three
/// call forms agree without callee-identity analysis.
pub struct ScalarCtx {
    taken_by_ref: HashSet<Ustr>,
    /// Per-body contribution: each body's set of fn-names it references by value.
    /// Keyed by `(module, owner)` so a body edit can diff its OLD vs NEW set
    /// without rescanning the whole workspace. Empty-contribution bodies are
    /// omitted (absent == empty).
    per_body: HashMap<(ModuleId, BodyOwner), HashSet<Ustr>>,
    /// Number of distinct bodies referencing each name by value.
    /// `taken_by_ref == { name : count > 0 }`. Only a membership flip (0<->1)
    /// changes scalar-ABI eligibility, so [`update_body`](Self::update_body)
    /// reports exactly those.
    ref_counts: HashMap<Ustr, u32>,
    /// When set, the scalarized value ABI is disabled workspace-wide: every fn
    /// keeps value structs as first-class single values (params/returns/args),
    /// never splatting them to leaf scalars. Used by the GLSL backend, where a
    /// `value` struct is a first-class `vecN`, not the luajit-shaped scalar leaves. The toggle is
    /// contained in the ABI
    /// policy behind [`BackendFnAbiPlan`], which definitions and direct calls
    /// both read.
    values_first_class: bool,
    native_flat_enums: bool,
    /// Keep value-struct Vec elements as first-class rows. Native backends use
    /// their concrete aggregate layout directly instead of Lua's scalar-column
    /// storage representation.
    row_value_containers: bool,
    /// The codegen backend this lowering feeds. Drives backend-variant selection
    /// at every resolved call ([`Builder::resolve_funcref`]): `Lua` rejects a
    /// call whose only binding is a JS decl; `Js` swaps a call to its JS sibling
    /// when the collapsed representative is a luajit-native variant. `Any`
    /// disables the filtering entirely (the GLSL backend + generic/test callers).
    target: bone_semantic::hir::FnBackend,
    /// **The build's one type arena.**
    ///
    /// An [`Instance`](crate::instance::Instance) is an identity only while one
    /// arena is reachable from everything that mints or reads one, and lowering
    /// is where a callee instantiation is first named. It lives here because
    /// this is the build-scoped context every lowering entry point already
    /// carries -- threading a second parameter through them would put the same
    /// object in two places and let a caller supply the wrong one.
    ///
    /// It is a SHARED HANDLE, not an owned arena. The retained
    /// `InstanceGraph` mints instances against the same object, and neither
    /// side can own it outright: moving the arena here would leave the graph's
    /// `MonoTyId`s dangling, and copying it would be the second identity this
    /// stage exists to remove. [`ScalarCtx::sharing`] is how the two are joined.
    interner: std::sync::Arc<crate::monoty::MonoInterner>,
    /// The build's one physical representation arena. It follows this context
    /// so target-specific ABI identities survive from body entry through calls
    /// and publishers.
    rep_arena: std::sync::Arc<abi::RepArena>,
    /// Memoized [`BackendFnAbiPlan`] derivations. Lives here because a plan is
    /// only valid under this context's target/value-mode/taken-by-ref state,
    /// and [`ScalarCtx::update_body`] -- the one mutation crossing emissions --
    /// clears it.
    abi_plans: abi::AbiPlanCache,
}

impl Clone for ScalarCtx {
    fn clone(&self) -> Self {
        Self {
            taken_by_ref: self.taken_by_ref.clone(),
            per_body: self.per_body.clone(),
            ref_counts: self.ref_counts.clone(),
            values_first_class: self.values_first_class,
            native_flat_enums: self.native_flat_enums,
            row_value_containers: self.row_value_containers,
            target: self.target,
            interner: std::sync::Arc::clone(&self.interner),
            rep_arena: std::sync::Arc::clone(&self.rep_arena),
            abi_plans: self.abi_plans.clone(),
        }
    }
}

/// The fn-names a single body references by value (`Expr::Global(name)` whose
/// `name` is a function) — the way `let f = v2_sum` / passing `f` as an arg
/// surfaces in HIR (a direct call's callee is a `Callee`, not an `Expr`).
fn scan_body_value_refs(ws: &Workspace, body: &Body) -> HashSet<Ustr> {
    let mut set = HashSet::new();
    for (_, expr) in body.expr_arena.iter() {
        if let Expr::Global(name) = expr {
            if ws.functions_by_name.contains_key(name) {
                set.insert(*name);
            }
        }
    }
    set
}

impl ScalarCtx {
    pub fn new(ws: &Workspace) -> Self {
        let mut taken_by_ref = HashSet::new();
        let mut per_body: HashMap<(ModuleId, BodyOwner), HashSet<Ustr>> = HashMap::new();
        let mut ref_counts: HashMap<Ustr, u32> = HashMap::new();
        for module in ws.modules.iter() {
            for (owner, body) in module.bodies.iter() {
                let set = scan_body_value_refs(ws, body);
                for name in &set {
                    *ref_counts.entry(*name).or_insert(0) += 1;
                    taken_by_ref.insert(*name);
                }
                if !set.is_empty() {
                    per_body.insert((module.id, *owner), set);
                }
            }
            // Generated `@sim` registration passes each sim as a fn value, which
            // no body scan sees. Its reference is permanent (adding or removing
            // `@sim` is an interface edit), so it holds one count that no body
            // edit can release.
            for (_, f) in module.functions.iter() {
                if f.annotations.iter().any(|a| a.is_builtin("sim")) {
                    *ref_counts.entry(f.name).or_insert(0) += 1;
                    taken_by_ref.insert(f.name);
                }
            }
        }
        // Default target is Lua: skull-codegen-lua is the primary consumer and its
        // full + incremental paths build a `ScalarCtx::new`. The JS driver and the
        // GLSL backend override via `with_target`.
        Self {
            taken_by_ref,
            per_body,
            ref_counts,
            values_first_class: false,
            native_flat_enums: false,
            row_value_containers: false,
            target: bone_semantic::hir::FnBackend::Lua,
            interner: std::sync::Arc::new(crate::monoty::MonoInterner::new()),
            rep_arena: std::sync::Arc::new(abi::RepArena::default()),
            abi_plans: abi::AbiPlanCache::default(),
        }
    }

    /// **The build's one type arena**, through which every [`Instance`] is
    /// minted and read.
    ///
    /// [`Instance`]: crate::instance::Instance
    pub fn interner(&self) -> &crate::monoty::MonoInterner {
        &self.interner
    }

    /// Share the arena a retained `InstanceGraph` mints against, so the graph's
    /// ids and emission's ids are the same ids.
    ///
    /// Takes the HANDLE, not an arena: the graph keeps minting after this, so a
    /// snapshot would go stale the moment an incremental update expanded it.
    /// The arena is monotonic, so emission naming a type the graph never
    /// reached simply extends the shared one.
    pub fn sharing(mut self, interner: std::sync::Arc<crate::monoty::MonoInterner>) -> Self {
        self.interner = interner;
        self
    }

    /// The shared handle, for a consumer that must mint against the SAME arena
    /// later -- an `InstanceGraph` built on demand rather than up front.
    pub fn interner_handle(&self) -> std::sync::Arc<crate::monoty::MonoInterner> {
        std::sync::Arc::clone(&self.interner)
    }

    pub fn rep_arena(&self) -> std::sync::Arc<abi::RepArena> {
        std::sync::Arc::clone(&self.rep_arena)
    }

    pub(crate) fn abi_plan_cached(&self, key: &AbiPlanKey) -> Option<BackendFnAbiPlanHandle> {
        self.abi_plans.lookup(key)
    }

    pub(crate) fn abi_plan_store(&self, key: AbiPlanKey, plan: &BackendFnAbiPlanHandle) {
        self.abi_plans.store(key, plan);
    }

    pub(crate) fn abi_plan_record_derivation(&self, started: Option<bone_core::prelude::Instant>) {
        self.abi_plans.record_derivation(started);
    }

    /// ABI-plan memo counters. A full emission over a real workspace must show
    /// hits: the duplicate-derivation population it exists to remove is the
    /// normal case, not an optimization corner.
    pub fn abi_plan_stats(&self) -> AbiPlanMemoStats {
        self.abi_plans.stats()
    }

    /// Derive every ABI question the concrete plans can present to production
    /// lowering, in deterministic key order, then publish the successful plans
    /// as immutable handles for the parallel fanout. Errors are intentionally
    /// not cached: the original consumer still re-derives them at its own span.
    pub fn warm_abi_plans(
        &mut self,
        ws: &Workspace,
        plans: &crate::concrete::ConcretePlans,
        always: &crate::concrete::ConcreteAlwaysPlans,
    ) {
        let mut keys = Vec::new();
        keys.extend(plans.keys().cloned().map(AbiPlanKey::Owner));
        for body in plans.values().map(|plan| &**plan).chain(always.values()) {
            keys.extend(body.callees.values().cloned().map(AbiPlanKey::Callee));
        }
        keys.sort();
        keys.dedup();

        let warm_keys = keys.len() as u64;
        let mut warm_failures = 0;
        for key in &keys {
            let result = match key {
                AbiPlanKey::Owner(owner) => owner_abi_plan(ws, self, plans, owner).map(|_| ()),
                AbiPlanKey::Callee(callee) => {
                    abi::callee_abi_plan_for_instance(ws, self, plans, callee).map(|_| ())
                }
            };
            if result.is_err() {
                warm_failures += 1;
            }
        }
        self.abi_plans.publish(warm_keys, warm_failures);
    }

    /// Move a completed ABI warm into the context that will lower and emit.
    /// The representation arena travels with the plans because their `RepId`s
    /// are scoped to that arena; both contexts must share the graph interner.
    pub fn adopt_warmed_abi(&mut self, warmed: Self) {
        assert!(
            self.values_first_class == warmed.values_first_class
                && self.native_flat_enums == warmed.native_flat_enums
                && self.native_flat_enums == warmed.native_flat_enums
                && self.row_value_containers == warmed.row_value_containers
                && self.target == warmed.target
                && std::sync::Arc::ptr_eq(&self.interner, &warmed.interner),
            "ABI warm context does not match the emission context"
        );
        self.rep_arena = warmed.rep_arena;
        self.abi_plans = warmed.abi_plans;
    }

    /// Traffic counters of this context's representation arena.
    pub fn rep_arena_stats(&self) -> abi::RepArenaStats {
        self.rep_arena.stats()
    }

    /// Set the codegen backend this lowering feeds (see [`target`]).
    /// The backend this context lowers for. Read by consumers that must ask the
    /// EMITTER's own declaration selection rather than restate it -- the site
    /// join compares a target-selected sibling against the target-neutral
    /// graph's representative through `backend_variant`.
    pub fn target(&self) -> bone_semantic::hir::FnBackend {
        self.target
    }

    pub fn with_target(mut self, target: bone_semantic::hir::FnBackend) -> Self {
        self.target = target;
        self
    }

    pub fn with_row_value_containers(mut self) -> Self {
        self.row_value_containers = true;
        self
    }

    /// Like [`new`](Self::new) but with the scalarized value ABI disabled: value
    /// structs stay first-class single values everywhere (the GLSL backend's
    /// requirement -- a `vecN` is not scalar leaves). See [`values_first_class`].
    /// The GLSL backend does not participate in Lua/JS backend-variant selection,
    /// so its target is `Any` (filtering disabled).
    pub fn new_values_first_class(ws: &Workspace) -> Self {
        let mut scx = Self::new(ws);
        scx.values_first_class = true;
        scx.target = bone_semantic::hir::FnBackend::Any;
        scx
    }

    /// Share this context's representation arena while selecting the
    /// first-class value ABI for a caller that emits into the same target.
    /// Global initializers use this for calls, while retaining their separate
    /// scalar storage policy.
    pub fn with_values_first_class(mut self) -> Self {
        self.values_first_class = true;
        self
    }

    pub fn with_native_flat_enums(mut self) -> Self {
        self.native_flat_enums = true;
        self
    }

    /// Incrementally fold one re-inferred body's value-reference contribution into
    /// the cached set, and return the names whose GLOBAL taken-by-ref membership
    /// FLIPPED (entered or left the set). A flip means a fn's scalar-ABI
    /// eligibility changed, so the named fn(s) and every direct caller must
    /// re-emit -- the caller adds the flipped names to the affected set.
    /// Body edits keep the body SET fixed (add/remove fn is interface-tier ->
    /// full rebuild), so diffing only the re-inferred bodies is complete.
    pub fn update_body(&mut self, ws: &Workspace, mid: ModuleId, owner: BodyOwner) -> Vec<Ustr> {
        // An edit can flip taken-by-ref membership, and the memoized ABI plans
        // read it -- drop them all rather than tracking which flips reach which
        // plan. Cheaper than one stale plan is worth.
        self.abi_plans.clear();
        let key = (mid, owner);
        let old = self.per_body.remove(&key).unwrap_or_default();
        let new = match ws.module(mid).bodies.get(&owner) {
            Some(body) => scan_body_value_refs(ws, body),
            None => HashSet::new(),
        };
        let mut flipped = Vec::new();
        for name in old.difference(&new) {
            if let Some(c) = self.ref_counts.get_mut(name) {
                *c -= 1;
                if *c == 0 {
                    self.ref_counts.remove(name);
                    self.taken_by_ref.remove(name);
                    flipped.push(*name);
                }
            }
        }
        for name in new.difference(&old) {
            let c = self.ref_counts.entry(*name).or_insert(0);
            *c += 1;
            if *c == 1 {
                self.taken_by_ref.insert(*name);
                flipped.push(*name);
            }
        }
        if !new.is_empty() {
            self.per_body.insert(key, new);
        }
        flipped
    }
}

/// A hoisted nested `fn`: the private top-level name it resolves to, and the
/// value ABI its definition and every call site both read (so the two can never
/// drift, the same single-gate discipline as [`BackendFnAbiPlan`]).
#[derive(Clone)]
struct NestedFn {
    hoisted: Ustr,
    /// Declared param names, so a call site can place named args into slots
    /// (`slotted_call_args`) exactly as a top-level callee's would be.
    param_names: Vec<Ustr>,
    representation: FunctionRepPlan,
    logical_type: MonoTy,
}

impl NestedFn {
    fn uses_scalar_abi(&self) -> bool {
        self.representation.uses_scalar_abi()
    }

    pub(crate) fn param_slots(&self, arena: &RepArena) -> Vec<RetSlot> {
        self.representation.params().iter().map(|id| arena.ret_slot(*id)).collect()
    }

    pub(crate) fn result_shape(&self, arena: &RepArena) -> Option<ValueShape> {
        self.representation.result().and_then(|id| arena.value_shape(id))
    }
}

/// THE invariant: a function's emitted Lua parameter list must be INJECTIVE.
///
/// The scalar-ABI leaf encoding joins nested field names with `__`, and `__` is
/// legal in a Rock identifier, so the join cannot be made injective on its own.
/// Two distinct sources can land on one emitted name:
///   - two leaves of the same value param (`value Bad { tl__x: float, tl: V2 }`
///     flattens both `tl__x` and `tl.x` to `r__tl__x`), or
///   - a leaf and a SIBLING param (`fn f(r: V2, r__x: float)` emits
///     `f(r__x, r__y, r__x)`).
/// Either way Lua binds the later one and the body silently reads the wrong
/// value. Reject rather than emit the duplicate -- same discipline as
/// [`crate::mangle::collision_errors`].
///
/// Keyed on the SIGNATURE, at the point leaves actually become parameter names --
/// not on the value declaration. A declaration key is both unsound and
/// over-broad: it misses the sibling-param collision entirely (the struct is
/// clean there), and it rejects a program whose ambiguous shape never reaches a
/// param list at all, which is legal and observably correct today.
///
/// The names come from [`scalar_abi_lua_param_names`] -- the very list the
/// emitter writes -- so the checker cannot disagree with what is emitted.
///
/// Scoped to `inline(lua)` bodies because that is the ONLY emission that names
/// leaves: an ordinary Rock fn's params are emitted as `c<N>` cells, positional
/// and injective by construction, so an ambiguous shape flowing through one is
/// harmless and must keep compiling.
pub fn scalar_abi_param_collisions(
    ws: &Workspace,
    scx: &ScalarCtx,
    fref: FuncRef,
) -> Vec<(String, CompilerError)> {
    let f = &ws.module(fref.module).functions[fref.func];
    if f.inline_body.is_none() {
        return Vec::new();
    }
    let plan = BackendFnAbiPlan::for_boundary_declaration(ws, scx, fref);
    let Some(names) = plan.lua_param_names(ws) else {
        return Vec::new();
    };
    let origins = scalar_abi_param_origins(ws, &plan);
    debug_assert_eq!(names.len(), origins.len(), "one derivation, two spellings");

    let mut out = Vec::new();
    let mut seen: HashMap<&str, &str> = HashMap::new();
    let mut reported: HashSet<&str> = HashSet::new();
    for (name, origin) in names.iter().zip(origins.iter()) {
        match seen.get(name.as_str()) {
            Some(prev) if !reported.contains(name.as_str()) => {
                reported.insert(name.as_str());
                let fname = f.name;
                let msg = format!(
                    "`{fname}` would emit the Lua parameter `{name}` twice: it is both {prev} and \
                     {origin}. The scalar ABI joins nested value fields with `__`, which is legal \
                     in an identifier too, so the two are indistinguishable and the body would \
                     read whichever Lua bound last -- rename one of them"
                );
                out.push((
                    format!("{fname}.{name}"),
                    CompilerError::new_with_label(f.name_span, msg),
                ));
            }
            Some(_) => {}
            None => {
                seen.insert(name.as_str(), origin.as_str());
            }
        }
    }
    out
}

/// A human description per emitted parameter, positionally parallel to
/// [`scalar_abi_lua_param_names`]: either the declared param itself, or which
/// leaf of which value param it flattens from (dotted, so it is readable and
/// unambiguous). Built by the same walk under the `.` separator, over the SAME
/// plan the names come from -- a second plan here would be free to describe a
/// different parameter list than the one being checked.
fn scalar_abi_param_origins(ws: &Workspace, plan: &BackendFnAbiPlan) -> Vec<String> {
    if !plan.uses_scalar_abi() {
        return Vec::new();
    }
    let shapes = plan.param_shapes();
    let decl = plan.decl();
    let f = &ws.module(decl.module).functions[decl.func];
    let mut out = Vec::new();
    for (pd, slot) in f.param_decls.iter().zip(shapes.iter()) {
        slot_param_origins(pd.name.as_str(), slot, &mut out);
    }
    out
}

/// Append `<base>__<leaf>` for every primitive leaf of `shape`, depth-first.
/// `__` rather than `.` because these are Lua identifiers -- which is exactly
/// what makes the encoding non-injective and needs
/// [`value_leaf_name_collisions`] to guard it.
fn leaf_param_names(base: &str, shape: &ValueShape, out: &mut Vec<String>) {
    out.extend(leaf_names(shape, base, LEAF_PARAM_SEP));
}

/// The emitted param names ONE declared param contributes, per its wire slot.
/// A tuple slot indexes into the base (`pq__0__x`) using the same `__` join the
/// leaf encoding already uses, so the whole list stays one namespace and the
/// injectivity check below sees tuple leaves like any other.
fn slot_param_names(base: &str, slot: &RetSlot, out: &mut Vec<String>) {
    match slot {
        RetSlot::Opaque => out.push(base.to_string()),
        RetSlot::Leaves(shape) => leaf_param_names(base, shape, out),
        RetSlot::Nested(inner) => {
            // The presence leads the slots, so it takes the FIRST name -- the
            // order `bind_param_slot` binds cells in and `tuple_repr_to_slots`
            // writes values in. Skipping it emits a list one short of
            // `slot_count`, which Lua nil-fills rather than rejects.
            if inner.nullable {
                out.push(format!("{base}{LEAF_PARAM_SEP}p"));
            }
            for (i, s) in inner.slots.iter().enumerate() {
                slot_param_names(&format!("{base}{LEAF_PARAM_SEP}{i}"), s, out);
            }
        }
    }
}

/// [`slot_param_names`] in the human-facing spelling the collision diagnostic
/// uses -- the same traversal, so a reported origin always names a real emitted
/// param.
fn slot_param_origins(base: &str, slot: &RetSlot, out: &mut Vec<String>) {
    match slot {
        RetSlot::Opaque => out.push(format!("the declared param `{base}`")),
        RetSlot::Leaves(shape) => out.extend(
            leaf_names(shape, base, ".")
                .into_iter()
                .map(|p| format!("the leaf `{p}` of value param `{base}`")),
        ),
        RetSlot::Nested(inner) => {
            if inner.nullable {
                out.push(format!("the presence of nullable tuple param `{base}`"));
            }
            for (i, s) in inner.slots.iter().enumerate() {
                slot_param_origins(&format!("{base}.{i}"), s, out);
            }
        }
    }
}

/// The Lua leaf-param separator, and the reason a `__`-bearing field name can
/// collide with a nested path.
const LEAF_PARAM_SEP: &str = "__";

/// A chunk of a `string.format` conversion string: a static literal, or a
/// runtime int expression (dynamic width/precision) stringified into the format.
enum FmtPiece {
    Lit(String),
    DynInt(Idx<Expr>),
}

/// Decompose an interpolation spec into `string.format` conversion pieces
/// (`%[flags][width][.precision]type`). All-`Lit` => a static conversion; any
/// `DynInt` => assemble at runtime. `inferred_ty` supplies the conversion letter
/// when the spec omits one. `skip_width` leaves width out (the caller pads after
/// grouping); `skip_pad_attrs` leaves alignment/fill to the pad helper. `None`
/// for specs with no printf equivalent (custom fill / center alignment) — the
/// caller gaps so we never silently mis-format. Mirrors old skull `fmt_pieces`.
fn fmt_pieces(
    spec: &bone_semantic::hir::InterpSpec,
    inferred_ty: Option<char>,
    skip_width: bool,
    skip_pad_attrs: bool,
) -> Option<Vec<FmtPiece>> {
    if spec.fill.is_some() && !skip_pad_attrs {
        return None;
    }
    let ty = spec.format_type.or(inferred_ty)?;

    let mut pieces = Vec::new();
    let mut buf = String::from("%");
    if !skip_pad_attrs {
        match spec.alignment {
            None | Some('>') => {}       // right-align is printf's default
            Some('<') => buf.push('-'),  // left-align
            _ => return None,            // center etc. — no printf equivalent
        }
    }
    match spec.sign {
        Some('+') => buf.push('+'),
        Some(' ') => buf.push(' '),
        _ => {}
    }
    if spec.alternate {
        buf.push('#');
    }
    // Zero-pad only with a STATIC width (a dynamic width can't bake `0`+width
    // into the literal; falls back to space pad).
    if spec.zero_pad && spec.width_expr.is_none() {
        buf.push('0');
    }
    if skip_width {
    } else if let Some(w) = spec.width {
        buf.push_str(&w.to_string());
    } else if let Some(we) = spec.width_expr {
        pieces.push(FmtPiece::Lit(std::mem::take(&mut buf)));
        pieces.push(FmtPiece::DynInt(we));
    }
    if spec.precision.is_some() || spec.precision_expr.is_some() {
        buf.push('.');
        if let Some(p) = spec.precision {
            buf.push_str(&p.to_string());
        } else if let Some(pe) = spec.precision_expr {
            pieces.push(FmtPiece::Lit(std::mem::take(&mut buf)));
            pieces.push(FmtPiece::DynInt(pe));
        }
    }
    buf.push(ty);
    pieces.push(FmtPiece::Lit(buf));
    Some(pieces)
}

// A short tag for an unhandled expr variant, so a lowering gap names the form it
// hit (`...: InterpolatedString`) -- the corpus coverage lens buckets on it.
fn expr_tag(e: &Expr) -> &'static str {
    match e {
        Expr::As { .. } => "As",
        Expr::IncDec { .. } => "IncDec",
        Expr::InterpolatedString(_) => "InterpolatedString",
        Expr::Lambda { .. } => "Lambda",
        Expr::Range { .. } => "Range",
        Expr::Return(_) => "Return",
        Expr::SafeNav { .. } => "SafeNav",
        Expr::SafeNavCall { .. } => "SafeNavCall",
        Expr::SafeNavLet { .. } => "SafeNavLet",
        Expr::SiblingRef { .. } => "SiblingRef",
        Expr::Spread(_) => "Spread",
        Expr::Yield(_) => "Yield",
        Expr::YieldDelegate(_) => "YieldDelegate",
        _ => "other",
    }
}

fn raw(b: Idx<BindingInfo>) -> u32 {
    b.into_raw().into()
}

/// The `NativeBuffer*` nominal type names (T-typed + phantom-size variants).
fn is_native_buffer(name: Ustr) -> bool {
    bone_semantic::prelude::is_intrinsic_key_in(
        name,
        &[
            "NativeBuffer",
            "NativeBufferU8",
            "NativeBufferI8",
            "NativeBufferU16",
            "NativeBufferI16",
            "NativeBufferU32",
            "NativeBufferI32",
        ],
    )
}

/// The scalar class of an operand, for the native-arithmetic dispatch.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ArithTy {
    Int,
    Uint,
    Float,
    Str,
}

/// The typed native arithmetic instruction for an `__op_*` over two scalar
/// values of the given classes (the build-time int/float/string split, D8).
fn arith_inst(op: &str, lt: ArithTy, rt: ArithTy, lv: Idx<Inst>, rv: Idx<Inst>) -> Result<Inst, CompilerError> {
    let float = lt == ArithTy::Float || rt == ArithTy::Float;
    Ok(match op {
        "__op_add" if lt == ArithTy::Str || rt == ArithTy::Str => Inst::StringConcat(lv, rv),
        "__op_add" if float => Inst::FloatAdd(lv, rv),
        "__op_add" if lt == ArithTy::Uint && rt == ArithTy::Uint => Inst::UintAdd(lv, rv),
        "__op_add" => Inst::IntAdd(lv, rv),
        "__op_sub" if float => Inst::FloatSub(lv, rv),
        "__op_sub" if lt == ArithTy::Uint && rt == ArithTy::Uint => Inst::UintSub(lv, rv),
        "__op_sub" => Inst::IntSub(lv, rv),
        "__op_mul" if float => Inst::FloatMul(lv, rv),
        "__op_mul" if lt == ArithTy::Uint && rt == ArithTy::Uint => Inst::UintMul(lv, rv),
        "__op_mul" => Inst::IntMul(lv, rv),
        // `/` is always float division (Lua `/` promotes int operands);
        // integer floor division is the separate `./` -> `__op_idiv`, which
        // codegen renders `math.floor(a / b)`.
        "__op_div" => Inst::FloatDiv(lv, rv),
        "__op_idiv" if lt == ArithTy::Uint && rt == ArithTy::Uint => Inst::UintDiv(lv, rv),
        "__op_idiv" => Inst::IntDiv(lv, rv),
        "__op_mod" if float => Inst::FloatMod(lv, rv),
        "__op_mod" if lt == ArithTy::Uint && rt == ArithTy::Uint => Inst::UintMod(lv, rv),
        "__op_mod" => Inst::IntMod(lv, rv),
        _ => return Err(gap(format!("native arith op {op}"))),
    })
}

/// Physical owner shared by a concrete callable and its flat variants.
/// Declaration names + rendered param types preserve non-generic cell names;
/// generic instances use their canonical concrete symbol to keep cells distinct.
fn static_owner_tag(
    ws: &Workspace,
    scx: &ScalarCtx,
    inf: Option<&BodyInference>,
    inst: &crate::instance::Instance,
) -> Ustr {
    use std::hash::{Hash, Hasher};
    let func = &ws.module(inst.func.module).functions[inst.func.func];
    let name = if inst.args.is_empty() {
        func.name
    } else {
        Ustr::from(&inst.symbol(ws, scx.interner()))
    };
    let mut h = std::collections::hash_map::DefaultHasher::new();
    name.as_str().hash(&mut h);
    for pd in &func.param_decls {
        ws.render(inf, pd.ty).hash(&mut h);
    }
    Ustr::from(&format!("{}_{:06x}", name, h.finish() & 0xff_ffff))
}

/// Content hash of a `static` initializer -- the reload key's init half. Hashes
/// names / literals / operator kinds and recurses, but NEVER an arena `Idx`
/// (those shift when unrelated code in the body is edited). So it changes iff
/// the initializer's meaning changes, and editing code elsewhere in the fn
/// leaves it untouched (the cell is preserved across that reload).
fn hash_init_expr(body: &Body, e: Idx<Expr>, h: &mut std::collections::hash_map::DefaultHasher) {
    use bone_semantic::hir::{Callee, Expr, InterpPart, VariantArgs};
    use std::hash::Hash;
    macro_rules! go {
        ($x:expr) => {
            hash_init_expr(body, $x, h)
        };
    }
    macro_rules! tag {
        ($n:expr) => {
            ($n as u16).hash(h)
        };
    }
    match body.expr_arena[e] {
        Expr::Unit => tag!(0),
        Expr::Null => tag!(1),
        Expr::Hole(n) => {
            tag!(2);
            n.hash(h);
        }
        Expr::Int(v) => {
            tag!(3);
            v.hash(h);
        }
        Expr::Float(v) => {
            tag!(4);
            v.hash(h);
        }
        Expr::Bool(v) => {
            tag!(5);
            v.hash(h);
        }
        Expr::String(s) => {
            tag!(6);
            s.hash(h);
        }
        Expr::Var(b) => {
            tag!(7);
            body.binding_arena[b].name.hash(h);
        }
        Expr::Global(n) => {
            tag!(8);
            n.hash(h);
        }
        Expr::SiblingRef(n) => {
            tag!(9);
            n.hash(h);
        }
        Expr::Tuple(es) | Expr::Array(es) => {
            tag!(10);
            for x in es {
                go!(x);
            }
        }
        Expr::InterpolatedString(parts) => {
            tag!(13);
            for p in parts {
                match body.interp_part_arena[p] {
                    InterpPart::Literal(s) => {
                        tag!(130);
                        s.hash(h);
                    }
                    InterpPart::Value { expr, .. } => {
                        tag!(131);
                        go!(expr);
                    }
                }
            }
        }
        Expr::Not(i, _)
        | Expr::BitNot(i)
        | Expr::UnaryMinus(i, _)
        | Expr::Spread(i)
        | Expr::YieldDelegate(i) => {
            tag!(14);
            go!(i);
        }
        Expr::Return(o) => {
            tag!(19);
            if let Some(i) = o {
                go!(i);
            }
        }
        Expr::Yield(o) => {
            tag!(20);
            if let Some(i) = o {
                go!(i);
            }
        }
        Expr::Break => tag!(21),
        Expr::Continue => tag!(22),
        Expr::As { value, target, kind } => {
            tag!(23);
            target.hash(h);
            kind.hash(h);
            go!(value);
        }
        Expr::TupleIndex { tuple, index } => {
            tag!(24);
            index.hash(h);
            go!(tuple);
        }
        Expr::FieldAccess { base, field } | Expr::SafeNav { base, field } => {
            tag!(25);
            field.hash(h);
            go!(base);
        }
        Expr::IncDec { target, decrement, postfix } => {
            tag!(27);
            decrement.hash(h);
            postfix.hash(h);
            go!(target);
        }
        Expr::Index { base, index } => {
            tag!(28);
            go!(base);
            go!(index);
        }
        Expr::Range { low, high } => {
            tag!(29);
            go!(low);
            go!(high);
        }
        Expr::BinOp { op, lhs, rhs } => {
            tag!(30);
            std::mem::discriminant(&op).hash(h);
            go!(lhs);
            go!(rhs);
        }
        Expr::Assign { target, value } => {
            tag!(31);
            go!(target);
            go!(value);
        }
        Expr::CompoundAssign { target, op_fn, value, .. } => {
            tag!(32);
            op_fn.hash(h);
            go!(target);
            go!(value);
        }
        Expr::If { cond, then_branch, else_branch } => {
            tag!(33);
            go!(cond);
            go!(then_branch);
            if let Some(x) = else_branch {
                go!(x);
            }
        }
        Expr::IfLet { value, then_branch, else_branch, .. } => {
            tag!(34);
            go!(value);
            go!(then_branch);
            if let Some(x) = else_branch {
                go!(x);
            }
        }
        Expr::Lambda { body: b, .. } => {
            tag!(35);
            go!(b);
        }
        Expr::Block { statements, tail } => {
            tag!(36);
            for s in statements {
                hash_init_stmt(body, s, h);
            }
            if let Some(t) = tail {
                go!(t);
            }
        }
        Expr::Match { scrutinee, arms } => {
            tag!(37);
            go!(scrutinee);
            for a in arms {
                let arm = body.match_arm_arena[a];
                if let Some(g) = arm.guard {
                    go!(g);
                }
                go!(arm.body);
            }
        }
        Expr::Call { callee, args, .. } => {
            tag!(38);
            match callee {
                Callee::Unresolved { name, .. } => {
                    tag!(380);
                    name.hash(h);
                }
                Callee::Local(b) => {
                    tag!(381);
                    body.binding_arena[b].name.hash(h);
                }
                Callee::Value(v) => {
                    tag!(382);
                    go!(v);
                }
            }
            for x in args {
                go!(x);
            }
        }
        Expr::SafeNavCall { receiver, method, args, .. } => {
            tag!(39);
            method.hash(h);
            go!(receiver);
            for x in args {
                go!(x);
            }
        }
        Expr::SafeNavLet { receiver, body: b, .. } => {
            tag!(40);
            go!(receiver);
            go!(b);
        }
        Expr::StructLiteral { name, fields, bases } => {
            tag!(41);
            name.hash(h);
            for b in bases {
                go!(b);
            }
            for fld in fields {
                let f = body.struct_lit_field_arena[fld];
                f.name.hash(h);
                go!(f.value);
            }
        }
        Expr::EnumVariant { enum_name, variant, args, .. } => {
            tag!(42);
            enum_name.hash(h);
            variant.hash(h);
            match args {
                VariantArgs::Unit => tag!(420),
                VariantArgs::Tuple(es) => {
                    tag!(421);
                    for x in es {
                        go!(x);
                    }
                }
                VariantArgs::Record(fs) => {
                    tag!(422);
                    for fld in fs {
                        let f = body.struct_lit_field_arena[fld];
                        f.name.hash(h);
                        go!(f.value);
                    }
                }
            }
        }
    }
}

fn hash_init_stmt(body: &Body, s: Idx<Statement>, h: &mut std::collections::hash_map::DefaultHasher) {
    use bone_semantic::hir::Statement;
    use std::hash::Hash;
    macro_rules! go {
        ($x:expr) => {
            hash_init_expr(body, $x, h)
        };
    }
    match body.statement_arena[s] {
        Statement::Let { value, .. }
        | Statement::LetTuple { value, .. }
        | Statement::LetNewtype { value, .. }
        | Statement::Expr(value)
        | Statement::Defer(value)
        | Statement::Return(Some(value))
        | Statement::Static { value, .. } => go!(value),
        Statement::Fn { body: b, .. } => go!(b),
        Statement::For { iter, body: b, .. } => {
            go!(iter);
            go!(b);
        }
        Statement::While { cond, body: b } => {
            go!(cond);
            go!(b);
        }
        Statement::WhileLet { value, body: b, .. } => {
            go!(value);
            go!(b);
        }
        Statement::Return(None) | Statement::Break | Statement::Continue | Statement::Comment(_) => {
            (b'x').hash(h)
        }
    }
}

pub fn lower_function_with_plans(
    ws: &Workspace,
    scx: &ScalarCtx,
    plan: &ConcreteBodyPlan,
    concrete_plans: &ConcretePlans,
) -> Result<MirFunc, CompilerError> {
    lower_instance_with_plans(ws, scx, plan, concrete_plans)
}

/// Lower an ordinary function while resolving every concrete callee ABI from
/// the same graph-owned plan set. A missing selected callee is an error rather
/// than a fallback to declaration-shaped ABI facts.
pub fn lower_instance_with_plans(
    ws: &Workspace,
    scx: &ScalarCtx,
    plan: &ConcreteBodyPlan,
    concrete_plans: &ConcretePlans,
) -> Result<MirFunc, CompilerError> {
    lower_instance_with_plan_inner(ws, scx, plan, concrete_plans)
}

/// Reject a specialized bounded body whose witness tree was lost before any
/// lowering consumer can interpret its placeholder calls.
pub fn validate_concrete_bound_plan(
    func: &bone_semantic::hir::Function,
    plan: &ConcreteBodyPlan,
) -> Result<(), CompilerError> {
    if func.generic_bounds.iter().any(Option::is_some) && plan.bound_tree.is_empty() {
        return Err(CompilerError::new_with_label(
            func.name_span,
            "concrete callee bound plan is empty for its site".to_string(),
        ));
    }
    Ok(())
}

fn lower_instance_with_plan_inner(
    ws: &Workspace,
    scx: &ScalarCtx,
    plan: &ConcreteBodyPlan,
    concrete_plans: &ConcretePlans,
) -> Result<MirFunc, CompilerError> {
    let inst = plan
        .function_instance()
        .ok_or_else(|| gap("concrete plan does not own a function instance"))?;
    let (mid, fid) = (inst.func.module, inst.func.func);
    let module = ws.module(mid);
    let func = &module.functions[fid];
    validate_concrete_bound_plan(func, plan)?;
    let body = module
        .bodies
        .get(&BodyOwner::Function(fid))
        .ok_or_else(|| gap("concrete plan function has no body"))?;
    let inf = ws
        .inference(mid)
        .and_then(|ir| ir.per_fn.get(&BodyOwner::Function(fid)));
    if let Some(inf) = inf {
        validate_concrete_type_tables(
            ws,
            inf,
            body,
            &plan.types,
            "concrete plan",
            func.param_decls
                .iter()
                .map(|param| param.ty)
                .chain(func.return_ty),
        )?;
        for (expr, _) in inf.expr_types.iter() {
            if plan.types.expr_type(expr).is_none() && !plan.types.is_untyped_expr(expr) {
                return Err(gap(format!("concrete plan is missing expression type {expr:?}")));
            }
        }
        for (binding, _) in inf.binding_types.iter() {
            if plan.types.binding_type(binding).is_none()
                && !plan.types.is_untyped_binding(binding)
            {
                return Err(gap(format!("concrete plan is missing binding type {binding:?}")));
            }
        }
    }
    for site in plan.callees.keys() {
        if !plan.callee_signatures.contains_key(site) {
            return Err(gap(format!(
                "concrete plan is missing the callee signature for site {:?}",
                site.op
            )));
        }
        if !matches!(site.domain, crate::site::SiteDomain::Own)
            && plan.domain_types(&site.domain).is_none()
        {
            return Err(gap(format!(
                "concrete plan is missing type tables for site domain {:?}",
                site.domain
            )));
        }
    }
    validate_concrete_callee_plans(ws, scx, plan, concrete_plans)?;
    lower_body(ws, scx, inst.func, inst.clone(), plan, concrete_plans)
}

fn lower_body(
    ws: &Workspace,
    scx: &ScalarCtx,
    self_ref: FuncRef,
    subject: crate::instance::Instance,
    concrete_plan: &ConcreteBodyPlan,
    concrete_plans: &ConcretePlans,
) -> Result<MirFunc, CompilerError> {
    let (mid, fid) = (self_ref.module, self_ref.func);
    let module = ws.module(mid);
    let func = &module.functions[fid];
    if func.inline_body.is_some() {
        return Err(gap("inline(lua) body"));
    }
    let body = module
        .bodies
        .get(&BodyOwner::Function(fid))
        .ok_or_else(|| gap("function has no body (extern?)"))?;
    let inf = ws
        .inference(mid)
        .and_then(|ir| ir.per_fn.get(&BodyOwner::Function(fid)));

    let mut f = MirFunc::new(func.name);
    f.rep_arena = Some(scx.rep_arena());
    // A `co fn` lowers to an ordinary CFG with `Term::Yield`s + a resume-arg cell
    // (the value `yield e` evaluates to). `skull-transform`'s coroutine pass then
    // rewrites it into the stackless state machine.
    let resume_arg = func.is_coroutine.then(|| {
        let cell = f.new_cell(Some(Ustr::from("__resume_arg")));
        f.cells[cell].capture_identity = Some(crate::code_identity::CaptureIdentity::Generated {
            role: Ustr::from("coroutine-resume-arg"),
        });
        cell
    });
    f.is_coroutine = func.is_coroutine;
    f.resume_arg = resume_arg;
    let cur = f.entry();
    // Scalar value ABI: when this fn participates, each value param expands to
    // leaf cells and a value return becomes a multi-value `Return`. This body's
    // own ABI is the one Owner-keyed plan the calls that target it read; the
    // plan in hand rides along because a standalone specialization's plan may
    // not be in the shared map.
    let abi = owner_abi_plan_for_body(ws, scx, concrete_plans, &subject, concrete_plan)?;
    f.abi_plan = Some(abi.clone());
    f.code_identity = Some(crate::code_identity::CodeIdentity::Named(subject.clone()));
    f.code_abi_fingerprint = Some(abi.logical_fingerprint());
    let param_rep_count = abi.representation().params().len();
    let ret_rep = abi.representation().result();
    let mut b = Builder {
        ws,
        scx,
        call_scx: scx,
        concrete_plan,
        concrete_plans,
        concrete_type_cache: RefCell::new(rustc_hash::FxHashMap::default()),
        lower_attribution: RefCell::new(LowerAttribution {
            abi_param_copy_volume: 0,
            abi_param_shared_views: usize::from(param_rep_count > 0),
            ..LowerAttribution::default()
        }),
        lower_timing_enabled: lower_attribution_timing_enabled(),
        generic_bounds: &func.generic_bounds,
        body,
        inf,
        f,
        cur,
        binds: HashMap::new(),
        loop_targets: Vec::new(),
        used_break_targets: HashSet::default(),
        defers: Vec::new(),
        defer_mode: stmt::DeferMode::for_body(func.is_coroutine, scx.target),
        in_defer_body: false,
        resume_arg,
        // A coroutine body emits via `emit_coroutine` before `inline_functions`
        // runs, so it is never spliced -- box value args (see field docs).
        param_reps: Vec::new(),
        param_plan: Some(abi.clone()),
        ret_rep,
        fn_name_span: Some(func.name_span),
        cur_span: None,
        cur_expr: None,
        cur_domain: crate::site::SiteDomain::Own,
        cur_op: crate::site::SiteOp::DirectCall,
        site_owner: Some(subject.clone()),
        group_binds: HashMap::new(),
        owned_value_binds: HashSet::new(),
        ref_borrowed_binds: std::cell::OnceCell::new(),
        value_copy_stack: Vec::new(),
        borrowed_value_children: HashMap::new(),
        lending_pack: false,
        f32_round_exprs: HashSet::new(),
        tuple_binds: HashMap::new(),
        static_cells: HashMap::new(),
        // Lazy: only pay the param-type render/hash when the body has a static or
        // a nested fn (both bake the owner into a private global name that must be
        // distinct per overload). Keeps the tag off every other fn's compile.
        static_owner: if body
            .statement_arena
            .iter()
            .any(|(_, s)| matches!(s, Statement::Static { .. } | Statement::Fn { .. }))
        {
            static_owner_tag(ws, scx, inf, &subject)
        } else {
            func.name
        },
        static_counters: HashMap::new(),
        nested_fn_globals: HashMap::new(),
        nested_fn_counters: HashMap::new(),
        presence_only: None,
        flat_ref_binds: HashMap::new(),
        flat_ref_cells: HashMap::new(),
        flat_scan_lets: HashMap::new(),
        flat_ret: None,
        flat_variants: FlatVariants::for_root(subject.clone()),
        flat_arg_captures: Vec::new(),
    };
    b.flat_ret = b.own_flat_return(&subject)?;
    b.lower_params()?;
    b.fill_param_defaults()?;
    let params: Vec<Idx<BindingInfo>> = b.body.param_bindings.into_iter().collect();
    b.copy_ref_borrowed_params(&params)?;
    b.lower_root()?;
    let variants = b.flat_variants.borrow_mut().take_funcs();
    b.f.nested_fns.extend(variants);
    let stats = b.lower_attribution.into_inner();
    Ok(finish_lower_attribution(b.f, stats))
}

/// Call a catalog factory through its owner ABI and pack its complete result.
pub fn lower_definition_factory(
    ws: &Workspace,
    scx: &ScalarCtx,
    concrete_plans: &ConcretePlans,
    factory: &crate::instance::Instance,
) -> Result<MirFunc, CompilerError> {
    let concrete_plan = concrete_plans.get(factory)
        .ok_or_else(|| gap("definition factory has no concrete body plan"))?;
    let module = ws.module(factory.func.module);
    let body = module.bodies.get(&BodyOwner::Function(factory.func.func))
        .ok_or_else(|| gap("definition factory has no body"))?;
    let abi = owner_abi_plan(ws, scx, concrete_plans, factory)?;
    let mut f = MirFunc::new(Ustr::from("__rock_definition_materialize"));
    f.rep_arena = Some(scx.rep_arena());
    let parameter = f.new_cell(Some(Ustr::from("factory")));
    f.params.push(parameter);
    let mut b = Builder::generated(ws, scx, scx, concrete_plan, concrete_plans, body, None, f);
    let callee = b.emit_cur(Inst::Load(parameter));
    let result = b.indirect_call_by_plan(abi.representation().clone(), callee, &[])?;
    let result = b.pack(result)?;
    b.f.set_term(b.cur, Term::Return(vec![result]));
    Ok(b.f)
}

/// Lower an always-emitted global from its graph-owned concrete plan.
pub fn lower_global_with_plans(
    ws: &Workspace,
    scx: &ScalarCtx,
    plan: &ConcreteBodyPlan,
    concrete_plans: &ConcretePlans,
    name: Ustr,
) -> Result<Option<MirFunc>, CompilerError> {
    lower_global_with_call_context(ws, scx, scx, plan, concrete_plans, name)
}

/// Lower a global with a storage context and a possibly different call ABI
/// context. Native C globals keep their scalarized storage while calls into
/// ordinary C bodies use those bodies' first-class value ABI.
pub fn lower_global_with_call_context(
    ws: &Workspace,
    scx: &ScalarCtx,
    call_scx: &ScalarCtx,
    plan: &ConcreteBodyPlan,
    concrete_plans: &ConcretePlans,
    name: Ustr,
) -> Result<Option<MirFunc>, CompilerError> {
    let ConcreteOwner::Global { module, index } = &plan.owner else {
        return Err(gap("concrete plan does not own a global"));
    };
    let module_ref = ws.module(*module);
    if *index as usize >= module_ref.globals.len() {
        return Err(gap("concrete global plan has an invalid global index"));
    }
    let gid = Idx::from_raw(RawIdx::from_u32(*index));
    validate_always_plan(ws, scx, *module, BodyOwner::Global(gid), plan, concrete_plans)?;
    lower_global_inner(ws, scx, call_scx, *module, gid, name, plan, concrete_plans)
}

fn lower_global_inner(
    ws: &Workspace,
    scx: &ScalarCtx,
    call_scx: &ScalarCtx,
    mid: ModuleId,
    gid: Idx<bone_semantic::hir::GlobalDef>,
    name: Ustr,
    concrete_plan: &ConcreteBodyPlan,
    concrete_plans: &ConcretePlans,
) -> Result<Option<MirFunc>, CompilerError> {
    let module = ws.module(mid);
    let body = module
        .bodies
        .get(&BodyOwner::Global(gid))
        .ok_or_else(|| gap("global has no body"))?;
    if body.root.is_none() {
        return Ok(None);
    }
    let inf = ws
        .inference(mid)
        .and_then(|ir| ir.per_fn.get(&BodyOwner::Global(gid)));
    let mut f = MirFunc::new(name);
    f.code_identity = Some(crate::code_identity::CodeIdentity::Artifact(
        crate::code_identity::CodeOwner::Artifact {
            kind: crate::code_identity::CodeOwnerKind::Global,
            module: module.file,
            name: module.globals[gid].name,
        },
    ));
    f.rep_arena = Some(scx.rep_arena());
    let mut b = Builder::generated(ws, scx, call_scx, concrete_plan, concrete_plans, body, inf, f);
    let global_plan = GlobalPlan::for_concrete_result(
        ws,
        scx,
        concrete_plan
            .signature
            .result
            .map(|id| b.concrete_mono_owned(id, ConcreteMonoConsumer::GlobalPlan)),
    )
    .map_err(|error| match error.span.or(b.cur_span) {
        Some(span) => CompilerError::new_with_label(span, error.message),
        None => CompilerError::new_no_location(error.message),
    })?;
    b.f.global_plan = Some(global_plan);
    // A `nobox` global is category-2 storage: its concrete plan owns the layout,
    // so the initializer RETURNS leaves and `mir_emit` binds one Lua global per
    // leaf. Without this the body packs a box the verifier then rejects.
    b.ret_rep = (!scx.values_first_class)
        .then(|| b.f.global_plan.as_ref().expect("global plan").representation());
    b.lower_root()?;
    let stats = b.lower_attribution.into_inner();
    Ok(Some(finish_lower_attribution(b.f, stats)))
}

/// Lower an always-emitted decorator from its graph-owned concrete plan.
pub fn lower_decorator_with_plans(
    ws: &Workspace,
    scx: &ScalarCtx,
    plan: &ConcreteBodyPlan,
    concrete_plans: &ConcretePlans,
    name: Ustr,
) -> Result<MirFunc, CompilerError> {
    let ConcreteOwner::Decorator { module, index } = &plan.owner else {
        return Err(gap("concrete plan does not own a decorator"));
    };
    let module_ref = ws.module(*module);
    if *index as usize >= module_ref.decorators.len() {
        return Err(gap("concrete decorator plan has an invalid decorator index"));
    }
    let did = Idx::from_raw(RawIdx::from_u32(*index));
    validate_always_plan(ws, scx, *module, BodyOwner::Decorator(did), plan, concrete_plans)?;
    lower_decorator_inner(ws, scx, *module, did, name, plan, concrete_plans)
}

fn validate_always_plan(
    ws: &Workspace,
    scx: &ScalarCtx,
    mid: ModuleId,
    owner: BodyOwner,
    plan: &ConcreteBodyPlan,
    concrete_plans: &ConcretePlans,
) -> Result<(), CompilerError> {
    let inf = ws
        .inference(mid)
        .and_then(|ir| ir.per_fn.get(&owner))
        .ok_or_else(|| gap("concrete always-emitted plan has no inference"))?;
    let body = ws
        .module(mid)
        .bodies
        .get(&owner)
        .ok_or_else(|| gap("concrete always-emitted plan has no body"))?;
    let declared_types = match owner {
        BodyOwner::Global(index) => vec![ws.module(mid).globals[index].ty],
        BodyOwner::Decorator(index) => ws.module(mid).decorators[index]
            .param_decls
            .iter()
            .map(|param| param.ty)
            .collect(),
        BodyOwner::Function(_)
        | BodyOwner::StructDefaults(_)
        | BodyOwner::Enum(_)
        | BodyOwner::Newtype(_) => Vec::new(),
    };
    validate_concrete_type_tables(
        ws,
        inf,
        body,
        &plan.types,
        "concrete always-emitted plan",
        declared_types,
    )?;
    for (expr, _) in inf.expr_types.iter() {
        if plan.types.expr_type(expr).is_none() && !plan.types.is_untyped_expr(expr) {
            return Err(gap(format!(
                "concrete always-emitted plan is missing expression type {expr:?}"
            )));
        }
    }
    for (binding, _) in inf.binding_types.iter() {
        if plan.types.binding_type(binding).is_none()
            && !plan.types.is_untyped_binding(binding)
        {
            return Err(gap(format!(
                "concrete always-emitted plan is missing binding type {binding:?}"
            )));
        }
    }
    for site in plan.callees.keys() {
        if !plan.callee_signatures.contains_key(site) {
            return Err(gap(format!(
                "concrete always-emitted plan is missing the callee signature for site {:?}",
                site.op
            )));
        }
        if !matches!(site.domain, crate::site::SiteDomain::Own)
            && plan.domain_types(&site.domain).is_none()
        {
            return Err(gap(format!(
                "concrete always-emitted plan is missing type tables for site domain {:?}",
                site.domain
            )));
        }
    }
    validate_concrete_callee_plans(ws, scx, plan, concrete_plans)?;
    Ok(())
}

fn validate_concrete_callee_plans(
    ws: &Workspace,
    scx: &ScalarCtx,
    plan: &ConcreteBodyPlan,
    concrete_plans: &ConcretePlans,
) -> Result<(), CompilerError> {
    for (site, owner) in &plan.callees {
        let needs_plan = requires_concrete_body_plan(ws, owner)
            && crate::monoty::too_deep_to_instantiate(owner.arg_types(scx.interner()).iter()).is_none();
        if !needs_plan {
            continue;
        }
        let callee_plan = concrete_plans.get(owner).ok_or_else(|| {
            gap(format!(
                "concrete callee body plan is missing for site {:?}",
                site.op
            ))
        })?;
        let recorded = plan.callee_signatures.get(site).ok_or_else(|| {
            gap(format!(
                "concrete plan is missing the callee signature for site {:?}",
                site.op
            ))
        })?;
        if recorded != &callee_plan.signature {
            return Err(gap(format!(
                "concrete callee signature disagrees with its body plan at site {:?}",
                site.op
            )));
        }
    }
    Ok(())
}

fn validate_concrete_type_tables(
    ws: &Workspace,
    inf: &BodyInference,
    body: &Body,
    tables: &ConcreteTypeTables,
    label: &str,
    declared_types: impl IntoIterator<Item = TyId>,
) -> Result<(), CompilerError> {
    fn visit(
        ws: &Workspace,
        inf: &BodyInference,
        tables: &ConcreteTypeTables,
        label: &str,
        ty: TyId,
        seen: &mut HashSet<TyId>,
    ) -> Result<(), CompilerError> {
        if !seen.insert(ty) {
            return Ok(());
        }
        if tables.ty_type(ty).is_none() {
            return Err(gap(format!("{label} is missing concrete type node {ty:?}")));
        }
        let node = match ty {
            TyId::Static(mid, idx) => ws.module(mid).ty_arena[idx],
            TyId::Local(idx) => inf.ty_arena[idx],
        };
        match node {
            TypeExpr::Nullable(inner) => visit(ws, inf, tables, label, ty.with_idx(inner), seen),
            TypeExpr::App { params, .. } => {
                for param in params {
                    visit(ws, inf, tables, label, ty.with_idx(param), seen)?;
                }
                Ok(())
            }
            TypeExpr::Function { params, ret } => {
                for param in params {
                    visit(ws, inf, tables, label, ty.with_idx(param), seen)?;
                }
                visit(ws, inf, tables, label, ty.with_idx(ret), seen)
            }
            TypeExpr::Tuple { elements } => {
                for element in elements {
                    visit(ws, inf, tables, label, ty.with_idx(element), seen)?;
                }
                Ok(())
            }
            TypeExpr::Infer(var, _) => match inf.substitutions.get(var) {
                Some(next) => visit(ws, inf, tables, label, next, seen),
                None => Ok(()),
            },
            TypeExpr::Builtin(_) | TypeExpr::Generic(..) => Ok(()),
        }
    }

    let mut seen = HashSet::new();
    for (expr, ty) in inf.expr_types.iter() {
        if tables.is_untyped_expr(expr) {
            continue;
        }
        visit(ws, inf, tables, label, *ty, &mut seen)?;
    }
    for (binding, ty) in inf.binding_types.iter() {
        if tables.is_untyped_binding(binding) {
            continue;
        }
        visit(ws, inf, tables, label, *ty, &mut seen)?;
    }
    for ty in inf.fn_field_call_types.values() {
        visit(ws, inf, tables, label, *ty, &mut seen)?;
    }
    for ty in declared_types {
        visit(ws, inf, tables, label, ty, &mut seen)?;
    }
    for (_, binding) in body.binding_arena.iter() {
        if let Some(ty) = binding.declared_ty {
            visit(ws, inf, tables, label, ty, &mut seen)?;
        }
    }
    for (_, node) in body.expr_arena.iter() {
        if let Expr::As { target, .. } = node {
            visit(ws, inf, tables, label, *target, &mut seen)?;
        }
    }
    for (expr, node) in body.expr_arena.iter() {
        let Expr::StructLiteral { name, .. } = node else { continue };
        let fields = tables.struct_field_types.get(expr).ok_or_else(|| {
            gap(format!(
                "{label} is missing concrete fields for struct literal `{name}` ({expr:?})"
            ))
        })?;
        let sid = ws
            .struct_by_name
            .get(&name)
            .ok_or_else(|| gap(format!("{label} names unknown struct `{name}`")))?;
        let def = &ws.modules[sid.module.index()].structs[sid.def];
        for field in &def.fields {
            if !fields.contains_key(&field.name) {
                return Err(gap(format!(
                    "{label} is missing concrete field `{name}.{}` ({expr:?})",
                    field.name
                )));
            }
        }
    }
    Ok(())
}

fn lower_decorator_inner(
    ws: &Workspace,
    scx: &ScalarCtx,
    mid: ModuleId,
    did: Idx<bone_semantic::hir::DecoratorDef>,
    name: Ustr,
    concrete_plan: &ConcreteBodyPlan,
    concrete_plans: &ConcretePlans,
) -> Result<MirFunc, CompilerError> {
    let module = ws.module(mid);
    let body = module
        .bodies
        .get(&BodyOwner::Decorator(did))
        .ok_or_else(|| gap("decorator has no body"))?;
    let inf = ws
        .inference(mid)
        .and_then(|ir| ir.per_fn.get(&BodyOwner::Decorator(did)));
    let target = body
        .deco_target
        .ok_or_else(|| gap("decorator body has no target binding"))?;
    let mut f = MirFunc::new(name);
    f.code_identity = Some(crate::code_identity::CodeIdentity::Artifact(
        crate::code_identity::CodeOwner::Artifact {
            kind: crate::code_identity::CodeOwnerKind::Decorator,
            module: module.file,
            name: module.decorators[did].def_key,
        },
    ));
    f.rep_arena = Some(scx.rep_arena());
    let representation = scx
        .rep_arena()
        .boxed_function(scx.target(), concrete_plan.signature.params.len());
    f.abi_representation = Some(representation.clone());
    let param_reps = representation.params().to_vec();
    let ret_rep = concrete_plan
        .signature
        .result
        .map(|_| representation.result().expect("boxed decorator result plan"));
    let mut b = Builder::generated(ws, scx, scx, concrete_plan, concrete_plans, body, inf, f);
    b.param_reps = param_reps;
    b.ret_rep = ret_rep;
    // `target` is param 0, then the decorator's declared params.
    let tcell = b.new_binding_cell(
        target,
        b.body.binding_arena[target].name,
        b.binding_ty(target),
        Ustr::from(""),
    );
    b.f.params.push(tcell);
    b.binds.insert(raw(target), tcell);
    b.lower_params()?;
    b.fill_param_defaults()?;
    let params: Vec<Idx<BindingInfo>> = b.body.param_bindings.into_iter().collect();
    b.copy_ref_borrowed_params(&params)?;
    b.lower_root()?;
    let stats = b.lower_attribution.into_inner();
    Ok(finish_lower_attribution(b.f, stats))
}

struct Builder<'a> {
    ws: &'a Workspace,
    scx: &'a ScalarCtx,
    call_scx: &'a ScalarCtx,
    concrete_plan: &'a ConcreteBodyPlan,
    concrete_plans: &'a ConcretePlans,
    concrete_type_cache: ConcreteTypeCache,
    lower_attribution: RefCell<LowerAttribution>,
    lower_timing_enabled: bool,
    /// The enclosing fn's `generic_bounds`, parallel to its `generics`:
    /// `Some(elem)` where a generic is declared `I: Iterator<elem>`. THE declared
    /// source for a placeholder `next`'s element shape -- the call's inferred
    /// type is checked once against the bound and does not carry the
    /// instantiation, so reading the element off it plans the wrong shape.
    generic_bounds: &'a [Option<TyId>],
    body: &'a Body,
    inf: Option<&'a BodyInference>,
    f: MirFunc,
    cur: Idx<Block>,
    /// raw binding index -> the variable cell it maps to.
    binds: HashMap<u32, Idx<Cell>>,
    /// Innermost-last stack of `(continue_target, break_target, defer depth)`:
    /// the depth is `defers.len()` at loop entry, so `break`/`continue` splice
    /// the defers of every scope they leave.
    loop_targets: Vec<(Idx<Block>, Idx<Block>, usize)>,
    /// Break targets some `break` actually jumped to. A `while true` whose exit
    /// was never targeted terminates the statement list (its fall-through is
    /// unreachable), which is what lets a `while true { ... return ... }` co fn
    /// body pass the non-unit fall-off rejection.
    used_break_targets: HashSet<Idx<Block>>,
    /// Innermost-last stack of pending `defer` bodies per block scope; spliced
    /// (LIFO) at block exit, before every `return` and at `break`/`continue`.
    /// Each entry records how dying (error, or a co fn kill) reaches it; see
    /// [`stmt::PendingDefer`].
    defers: Vec<Vec<(Idx<Expr>, stmt::PendingDefer)>>,
    /// How a `defer` in the body being lowered registers (see
    /// [`stmt::DeferMode`]). A lambda sets its own mode: inside a `co fn` it
    /// is still an ordinary fn. Global initializers and decorators are
    /// `Inline`: their units are not emitted under the unwind trampoline.
    defer_mode: stmt::DeferMode,
    /// True while lowering a `defer` body (inline splice or terminal copy) --
    /// `yield`/`await` inside a defer body is a compile error.
    in_defer_body: bool,
    /// In a `co fn`, the cell that `yield e` evaluates to (the value passed to
    /// the next `resume(arg)`). `None` outside a coroutine.
    resume_arg: Option<Idx<Cell>>,
    /// True while lowering a body whose calls this pass cannot give the
    /// specialized scalar ABI: a closure sub-CFG, a hoisted nested fn, a
    /// coroutine, a global init, or a decorator. A call here must NOT take the
    /// SPECIALIZED scalar ABI (splatting a value arg to leaves), which is only
    /// sound when the inliner replaces the call with the matching specialized
    /// body -- an unspliced residual would call the SHARED generic body (`T` = one
    /// boxed slot) with splatted args and drop every leaf but the first. Boxing
    /// the value arg matches the shared body exactly.
    /// Per-param physical representation identity, parallel to
    /// `body.param_bindings`. Shape structs are projected only at the
    /// flatten/rebuild seams; this is the body-owned ABI answer.
    param_reps: Vec<RepId>,
    /// The ordinary body's ABI owns this immutable parameter slice.
    param_plan: Option<BackendFnAbiPlanHandle>,
    /// The body's physical result representation identity, if it has one.
    ret_rep: Option<RepId>,
    /// The fn's name span, for spanned emit-time rejections that have no better
    /// anchor (the non-unit-coroutine fall-off check). `None` for non-fn bodies.
    fn_name_span: Option<Span>,
    /// The source span currently being lowered, maintained by [`Builder::lower_expr`].
    /// Instructions a post-transform check can reject stamp it into
    /// [`MirFunc::inst_spans`] so the diagnostic points at the construct rather
    /// than at the enclosing fn.
    cur_span: Option<Span>,
    /// The source EXPRESSION currently being lowered, maintained by
    /// [`Builder::lower_expr`] exactly beside [`Self::cur_span`]. Together with
    /// [`Self::cur_op`] this is the [`crate::site::Site`] every call
    /// instruction emitted from here down is stamped with, so the identity is
    /// recorded at ONE choke point ([`Builder::emit_cur`]) rather than at each
    /// of the call-emitting sites -- a per-site record would make a MISSING one
    /// indistinguishable from a legitimately siteless call.
    ///
    /// `None` only outside any expression (a fn prologue), which is the
    /// exclusion the site census reports by cause.
    cur_expr: Option<Idx<Expr>>,
    /// WHICH expression arena [`Self::cur_expr`] indexes. A struct-default
    /// initializer is lowered from the declaring struct's `StructDefaults` body
    /// into this CFG while `site_owner` still names the enclosing function, so
    /// its index is only meaningful against THAT arena -- and the walk records
    /// it under the same domain.
    cur_domain: crate::site::SiteDomain,
    /// The concrete INSTANCE being lowered, which OWNS every site recorded from
    /// this builder.
    /// `None` for the always-emitted units -- a global initializer and a
    /// decorator body are not functions and have no `FuncRef`, so their calls
    /// carry no site and the census reports them under that cause rather than
    /// losing them.
    site_owner: Option<crate::instance::Instance>,
    /// Which OPERATION of [`Self::cur_expr`] is being lowered. `DirectCall` is
    /// the default because a call emitted while lowering an expression is that
    /// expression's own call; the witness and synthesized routes override it
    /// for their sub-lowering through [`Builder::with_site_op`], because one
    /// expression carries several distinct callee obligations and they must
    /// stay distinguishable.
    cur_op: crate::site::SiteOp,
    /// A value PARAM binding -> (struct name, its leaf cells, shape). A read of
    /// the param reconstructs the unboxed `Group` from these; a field read
    /// projects a leaf directly (no cdata).
    group_binds: HashMap<u32, (Vec<Idx<Cell>>, ValueShape)>,
    /// Plain-cell `value` bindings first stored a freshly built box (a `let` of
    /// a ctor or pack). A reassignment copies a box it did not build, so every
    /// store stays a ctor and SROA can scalarize the binding. Whole-value uses
    /// still copy ([`Self::boxed_value_local`]): SROA may decline the cell (a
    /// closure touches it), and then its loads are the one shared box.
    owned_value_binds: HashSet<u32>,
    /// Bindings some `ref` borrow in this body is rooted at
    /// ([`Self::binding_ref_borrowed`]), collected on first use.
    ref_borrowed_binds: std::cell::OnceCell<HashSet<u32>>,
    /// Value types whose copy is being emitted, outermost first: a recursive
    /// `V?` child cannot be unrolled into its own copy ([`Self::enter_value_copy`]).
    value_copy_stack: Vec<Ustr>,
    /// Opaque value children a copy or a binding read left as its source's
    /// box, with their types ([`Self::owned_value_children`]).
    borrowed_value_children: HashMap<Idx<Inst>, Rc<MonoTy>>,
    /// Set while a `let` packs a place read into its own cell
    /// ([`Self::pack_lent`]).
    lending_pack: bool,
    /// Right-hand sides of assignments into an `f32` field. Every assignment
    /// path lowers its value through one of the expression entries, which
    /// round these, so no store path needs its own rounding.
    f32_round_exprs: HashSet<Idx<Expr>>,
    /// The tuple twin of [`Self::group_binds`]: a NAME bound to a splatting
    /// tuple holds its slot cells, so `if let t = <nullable tuple call>` can
    /// rebuild the unboxed repr on each read of `t` instead of packing it.
    /// Keyed the same way; the shape is stored non-nullable because presence
    /// was already consumed by the branch that bound it.
    tuple_binds: HashMap<u32, (Vec<Idx<Cell>>, TupleRetShape)>,
    /// `static` locals: raw binding idx -> the private global cell name a read /
    /// write of the binding resolves to (an `Inst::Global`/`StoreGlobal`, not a
    /// local cell). Populated by `lower_static`.
    static_cells: HashMap<u32, Ustr>,
    /// Owner tag baked into a static's cell name -- stable across body edits,
    /// distinct per overload (fn name + rendered param types). NOT the mangled
    /// name (skull-mir is upstream of the mangler); it only needs uniqueness +
    /// stability, which mir_emit reads back verbatim from `MirFunc::statics`.
    static_owner: Ustr,
    /// Per static source-name counter (source order) so shadowing duplicates get
    /// distinct cells and adding an unrelated static never renames siblings.
    static_counters: HashMap<Ustr, u32>,
    /// Nested `fn` names: raw binding idx -> the hoisted `__local_*` global a
    /// call / value-ref of the binding resolves to, plus its value ABI.
    /// Inherited (cloned) by a nested fn's own builder so it can reference
    /// itself + prior siblings.
    nested_fn_globals: HashMap<u32, NestedFn>,
    /// Per nested-fn source-name counter (source order), the `static_counters`
    /// analogue for shadowing duplicates.
    nested_fn_counters: HashMap<Ustr, u32>,
    /// The one expression currently being lowered for its PRESENCE only (`x ==
    /// null`): a `V?` place read there skips its copy-out, because a nil test
    /// answers the same question without a cdata
    /// ([`Builder::nullable_place_value_of`]). Exact node, never a subtree -- a
    /// nested read that feeds a real whole-value use still copies.
    presence_only: Option<Idx<Expr>>,
    /// `ref` bindings to a flat-stored container element (a flat variant's
    /// `ref` params and `let`s of a flat place), with the storage they address
    /// ([`flat_ref`]).
    flat_ref_binds: HashMap<u32, FlatRefKey>,
    /// The `(vec, slot)` cells of each [`Self::flat_ref_binds`] entry.
    flat_ref_cells: HashMap<u32, (Idx<Cell>, Idx<Cell>)>,
    /// `ref` lets of a body scanned for its flat return, by initializer.
    flat_scan_lets: HashMap<u32, Idx<Expr>>,
    /// Set when this body's `ref` result addresses a flat element: a return
    /// is `vec, slot`.
    flat_ret: Option<FlatRefKey>,
    /// The flat variants of the emitted root this builder lowers into.
    flat_variants: FlatVariantsHandle,
    /// Innermost-last flat arguments of the calls being lowered.
    flat_arg_captures: Vec<FlatArgCapture>,
}

impl<'a> Builder<'a> {
    fn generated(
        ws: &'a Workspace,
        scx: &'a ScalarCtx,
        call_scx: &'a ScalarCtx,
        concrete_plan: &'a ConcreteBodyPlan,
        concrete_plans: &'a ConcretePlans,
        body: &'a Body,
        inf: Option<&'a BodyInference>,
        f: MirFunc,
    ) -> Self {
        let cur = f.entry();
        let static_owner = f.name;
        Self {
            ws,
            scx,
            call_scx,
            concrete_plan,
            concrete_plans,
            concrete_type_cache: RefCell::new(rustc_hash::FxHashMap::default()),
            lower_attribution: RefCell::new(LowerAttribution::default()),
            lower_timing_enabled: lower_attribution_timing_enabled(),
            generic_bounds: &[],
            body,
            inf,
            f,
            cur,
            binds: HashMap::new(),
            loop_targets: Vec::new(),
            used_break_targets: HashSet::default(),
            defers: Vec::new(),
            defer_mode: stmt::DeferMode::Inline,
            in_defer_body: false,
            resume_arg: None,
            // Generated/init bodies never run `inline_functions` -- box value args.
            param_reps: Vec::new(),
            param_plan: None,
            ret_rep: None,
            fn_name_span: None,
            cur_span: None,
            cur_expr: None,
            cur_domain: crate::site::SiteDomain::Own,
            cur_op: crate::site::SiteOp::DirectCall,
            site_owner: None,
            group_binds: HashMap::new(),
            owned_value_binds: HashSet::new(),
            ref_borrowed_binds: std::cell::OnceCell::new(),
            value_copy_stack: Vec::new(),
            borrowed_value_children: HashMap::new(),
            lending_pack: false,
            f32_round_exprs: HashSet::new(),
            tuple_binds: HashMap::new(),
            static_cells: HashMap::new(),
            static_owner,
            static_counters: HashMap::new(),
            nested_fn_globals: HashMap::new(),
            nested_fn_counters: HashMap::new(),
            presence_only: None,
            flat_ref_binds: HashMap::new(),
            flat_ref_cells: HashMap::new(),
            flat_scan_lets: HashMap::new(),
            flat_ret: None,
            flat_variants: FlatVariantsHandle::default(),
            flat_arg_captures: Vec::new(),
        }
    }
}

impl Builder<'_> {
    fn new_binding_cell(
        &mut self,
        binding: Idx<BindingInfo>,
        name: Ustr,
        ty: Option<TyId>,
        projection: Ustr,
    ) -> Idx<Cell> {
        let cell = self.f.new_cell_typed(Some(name), ty);
        self.f.cells[cell].capture_identity =
            crate::code_identity::LambdaSiteId::from_span(self.body.binding_arena[binding].span)
                .map(|site| crate::code_identity::CaptureIdentity::Binding { site, projection });
        cell
    }

    fn rep_slot(&self, id: RepId) -> RetSlot {
        self.scx.rep_arena().ret_slot(id)
    }

    fn param_slot(&self, index: usize) -> Result<RetSlot, CompilerError> {
        if let Some(plan) = self.param_plan.as_ref() {
            if !plan.representation().uses_scalar_abi() {
                return Ok(RetSlot::Opaque);
            }
            return plan
                .representation()
                .params()
                .get(index)
                .copied()
                .map(|id| self.rep_slot(id))
                .ok_or_else(|| {
                    self.gap_here(format!("parameter {index} has no representation plan"))
                });
        }
        self.param_reps
            .get(index)
            .copied()
            .map(|id| self.rep_slot(id))
            .ok_or_else(|| self.gap_here(format!("parameter {index} has no representation plan")))
    }

    fn result_shape(&self) -> Option<ValueShape> {
        self.ret_rep.and_then(|id| self.scx.rep_arena().value_shape(id))
    }

    fn result_tuple(&self) -> Option<TupleRetShape> {
        self.ret_rep.and_then(|id| self.scx.rep_arena().tuple_shape(id))
    }

    /// [`gap`] anchored at the construct currently being lowered. Use wherever a
    /// gap names a user-visible type or expression -- an unspanned one leaves the
    /// reader with a whole project to search.
    fn gap_here(&self, msg: impl std::fmt::Display) -> CompilerError {
        match self.cur_span {
            Some(span) => CompilerError::new_with_label(
                span,
                format!("skull-mir: lowering gap: {msg}"),
            ),
            None => gap(msg),
        }
    }

    fn emit_cur(&mut self, inst: Inst) -> Idx<Inst> {
        let is_call = matches!(
            inst,
            Inst::Call { .. }
                | Inst::CallMulti { .. }
                | Inst::CallValue { .. }
                | Inst::CallValueMulti { .. }
                | Inst::CallBoundMulti { .. }
        );
        let id = self.f.emit(self.cur, inst);
        if let Some(span) = self.cur_span {
            self.f.inst_spans.insert(id, span);
        }
        let enum_name = match self.f.insts[id] {
            Inst::MakeEnum { ty, .. } | Inst::MakeEnumDyn { ty, .. } => Some(ty),
            _ => None,
        };
        if let (Some(enum_name), Some(expr)) = (enum_name, self.cur_expr) {
            if let Some(logical_ty) = self
                .concrete_plan
                .domain_types(&self.cur_domain)
                .and_then(|types| types.expr_type(expr))
            {
                let logical = self.scx.interner().get(logical_ty);
                let matches_owner = matches!(
                    logical.peel_nullable().0,
                    MonoTy::App {
                        ctor: MonoCtor::Nominal(name),
                        ..
                    } if *name == enum_name
                );
                if matches_owner {
                    self.f.inst_logical_types.entry(id).or_insert(logical_ty);
                }
            }
        }
        // THE choke point for call-site identity: every call FORM entering the
        // arena is stamped here, so a missing stamp is a property of this ONE
        // branch rather than of 48 call-emitting sites each remembering to
        // record. `CallBoundMulti` is included: it is the `for` protocol's
        // `next` witness on the caller-generic path, and its concrete sibling on
        // the other arm carries a site, so leaving it out made the SAME source
        // construct recorded or not depending on which arm it took. It is still
        // a call a body may not carry into emission, which is a separate
        // rejection.
        //
        // Not yet total: a call emitted outside any expression carries no site,
        // reported by `skull::site_join::census` rather than assumed away.
        // A fn-value MINT is stamped too. It is not a call, but it is a callee
        // OBLIGATION -- the one place a declaration becomes a spelled value --
        // and the graph records it at the same `Site { expr, FnValue }`. Keyed
        // on the operation rather than on the instruction, because
        // `Inst::Global` is also a module global and a `static` cell and those
        // name no callee.
        let is_mint = self.cur_op == crate::site::SiteOp::FnValue;
        if is_call || is_mint {
            if let (Some(owner), Some(expr)) = (self.site_owner.clone(), self.cur_expr) {
                let owner = crate::site::intern_owner(&mut self.f.site_owners, owner);
                self.f.call_sites.insert(
                    id,
                    crate::site::CallSite {
                        owner,
                        site: crate::site::Site {
                            domain: self.cur_domain.clone(),
                            expr,
                            op: self.cur_op.clone(),
                        },
                    },
                );
            }
        }
        id
    }

    /// The shared type/dispatch context this builder resolves through. Every
    /// predicate below is a delegate: `DispatchCtx` is the one implementation,
    /// so the instance walk asking the same question cannot get a second answer
    /// (`skull_mir::lower::dispatch`).
    pub(crate) fn concrete_mono_view(
        &self,
        id: MonoTyId,
        consumer: ConcreteMonoConsumer,
    ) -> Rc<MonoTy> {
        let timing = self.lower_timing_enabled;
        self.lower_attribution
            .borrow_mut()
            .record_concrete_mono_request(consumer);
        if self.lower_timing_enabled {
            self.lower_attribution
                .borrow_mut()
                .concrete_mono_timed_requests += 1;
        }
        let cache = &self.concrete_type_cache;
        let cache_lookup_start = timing.then(Instant::now);
        let hit = cache.borrow().get(&id).map(Rc::clone);
        let cache_lookup_ns = cache_lookup_start
            .map(|start| start.elapsed().as_nanos() as u64)
            .unwrap_or_default();
        if let Some(ty) = hit {
            let mut stats = self.lower_attribution.borrow_mut();
            stats.concrete_mono_shared_handle_requests += 1;
            stats.record_concrete_mono_cache_hit(consumer, 0, 0, cache_lookup_ns, 0);
            return ty;
        }
        let lookup_start = timing.then(Instant::now);
        let (ty, interner_cold) = self.scx.interner().get_shared_with_status(id);
        let lookup_ns = lookup_start
            .map(|start| start.elapsed().as_nanos() as u64)
            .unwrap_or_default();
        let mut stats = self.lower_attribution.borrow_mut();
        stats.concrete_mono_shared_handle_requests += 1;
        let nodes = interner_cold.then(|| mono_ty_node_count(&ty)).unwrap_or_default();
        cache.borrow_mut().insert(id, Rc::clone(&ty));
        stats.record_concrete_mono_derivation(consumer, nodes, 0, lookup_ns, cache_lookup_ns, 0);
        ty
    }

    pub(crate) fn concrete_mono_owned(
        &self,
        id: MonoTyId,
        consumer: ConcreteMonoConsumer,
    ) -> MonoTy {
        let ty = self.concrete_mono_view(id, consumer);
        let (owned, clone_self_ns) = clone_mono_ty_timed(&ty, self.lower_timing_enabled);
        let mut stats = self.lower_attribution.borrow_mut();
        stats.record_concrete_mono_owned_escape(
            consumer,
            mono_ty_node_count(&ty),
            clone_self_ns,
        );
        owned
    }

    pub(crate) fn dispatch(&self) -> dispatch::DispatchCtx<'_> {
        dispatch::DispatchCtx {
            ws: self.ws,
            inf: self.inf,
            body: self.body,
            concrete_plan: Some(self.concrete_plan),
            concrete_types: self.concrete_plan.domain_types(&self.cur_domain),
            concrete_type_cache: Some(&self.concrete_type_cache),
            lower_attribution: Some(&self.lower_attribution),
            lower_timing_enabled: self.lower_timing_enabled,
            interner: Some(self.scx.interner()),
            rep_arena: self.scx.rep_arena.as_ref(),
            target: self.scx.target,
            logical_subst: None,
            values_first_class: self.scx.values_first_class,
        }
    }

    /// Lower `f` with the site EXPRESSION overridden, restoring it after.
    ///
    /// The symmetric half of [`Self::with_site_op`]. Most calls inherit the
    /// cursor `lower_expr`/`lower_expr_repr` install, but a PLACE expression is
    /// never lowered as a value -- `lower_assign_effect` dispatches on
    /// `expr_arena[target]` directly -- so the calls its arms synthesize had no
    /// cursor at all in statement position, and the enclosing one in value
    /// position. Both are wrong against a walk that keys those obligations on
    /// the `Expr::Index` node itself.
    fn with_site_expr<T>(&mut self, expr: Idx<Expr>, f: impl FnOnce(&mut Self) -> T) -> T {
        let outer = self.cur_expr.replace(expr);
        let out = f(self);
        self.cur_expr = outer;
        out
    }

    /// Lower `f` with the site DOMAIN overridden, restoring it after.
    ///
    /// The third scoped half, beside the expression and the operation: a body
    /// lowered from another arena keeps its own indices, and the domain is what
    /// makes them readable. A flag rather than a cursor swap, because the nested
    /// `lower_expr` installs the foreign index itself.
    fn in_domain<T>(
        &mut self,
        domain: crate::site::SiteDomain,
        f: impl FnOnce(&mut Self) -> T,
    ) -> T {
        let outer = std::mem::replace(&mut self.cur_domain, domain);
        let out = f(self);
        self.cur_domain = outer;
        out
    }

    /// Lower `f` with the site OPERATION overridden, restoring it after. The
    /// witness and synthesized routes emit calls that belong to the enclosing
    /// expression but are NOT its own call, and a `for` head carries several at
    /// one expression -- so the operation has to be scoped, not global.
    fn with_site_op<T>(
        &mut self,
        op: crate::site::SiteOp,
        f: impl FnOnce(&mut Self) -> T,
    ) -> T {
        let outer = std::mem::replace(&mut self.cur_op, op);
        let out = f(self);
        self.cur_op = outer;
        out
    }

    /// Whether the current block is still open (no terminator set yet). A
    /// diverging sub-expression (`return`/`break`/`continue`) sets a real
    /// terminator, so after lowering it `is_open()` is false and the caller
    /// must NOT append a fall-through `Store`/`Goto`.
    fn is_open(&self) -> bool {
        matches!(self.f.blocks[self.cur].term, Term::Unreachable)
    }

    fn lower_params(&mut self) -> Result<(), CompilerError> {
        let pbs: Vec<Idx<BindingInfo>> = self.body.param_bindings.into_iter().collect();
        for (i, pb) in pbs.iter().enumerate() {
            let name = self.body.binding_arena[*pb].name;
            let slot = self.param_slot(i)?;
            self.bind_param_slot(*pb, name, &slot)?;
        }
        Ok(())
    }

    /// Give each boxed value parameter that this body `ref`-borrows its own
    /// copy on entry: the caller may have lent it the caller's box
    /// ([`Self::arg_values_for_slot`]), and the borrow writes in place.
    pub(crate) fn copy_ref_borrowed_params(
        &mut self,
        params: &[Idx<BindingInfo>],
    ) -> Result<(), CompilerError> {
        for &pb in params {
            let Some(&cell) = self.binds.get(&raw(pb)) else { continue };
            if self.scx.values_first_class || self.binding_is_ref(pb) || !self.binding_ref_borrowed(pb) {
                continue;
            }
            let Some(ty) = self.binding_ty(pb).and_then(|ty| self.concrete_ty_id(ty)) else {
                continue;
            };
            if !ty
                .peel_nullable()
                .0
                .nominal()
                .is_some_and(|name| self.is_value_struct(name) && !self.is_nobox_struct(name))
            {
                continue;
            }
            let v = self.emit_cur(Inst::Load(cell));
            let copy = self.copied_value_box(&ty, v)?;
            self.emit_cur(Inst::Store { cell, value: copy });
        }
        Ok(())
    }

    /// Bind one declared param to the cells its wire slot occupies, in the Lua
    /// parameter list.
    ///
    /// THE definition-side counterpart of `slot_param_names`, and the single
    /// implementation for all three definition forms (a top-level body, a
    /// hoisted nested fn, a lambda). They must agree with the caller on both the
    /// COUNT and the depth-first ORDER: Lua nil-fills a short call rather than
    /// rejecting it, so a disagreement here is a silent wrong answer, not an
    /// error. A splatting param has no single cell, so it binds as a GROUP (a
    /// value's leaves) or a TUPLE (a tuple's slots) and reads rebuild from them.
    pub(crate) fn bind_param_slot(
        &mut self,
        pb: Idx<BindingInfo>,
        name: Ustr,
        slot: &RetSlot,
    ) -> Result<(), CompilerError> {
        if !slot.splats() {
            let cty = self.binding_ty(pb);
            let cell = self.new_binding_cell(pb, name, cty, Ustr::from(""));
            self.f.params.push(cell);
            self.binds.insert(raw(pb), cell);
            return Ok(());
        }
        let cells = match slot {
            RetSlot::Leaves(shape) => {
                let owner = self
                    .binding_ty(pb)
                    .and_then(|ty| self.concrete_ty_id(ty));
                self.new_flat_enum_cells(pb, name, shape, owner.as_deref())?.unwrap_or_else(|| {
                    (0..slot.slot_count())
                        .map(|li| self.new_binding_cell(
                            pb,
                            Ustr::from(format!("{name}__{li}").as_str()),
                            None,
                            Ustr::from(li.to_string().as_str()),
                        ))
                        .collect()
                })
            }
            RetSlot::Nested(_) | RetSlot::Opaque => (0..slot.slot_count())
                .map(|li| self.new_binding_cell(
                    pb,
                    Ustr::from(format!("{name}__{li}").as_str()),
                    None,
                    Ustr::from(li.to_string().as_str()),
                ))
                .collect(),
        };
        if cells.len() != slot.slot_count() {
            return Err(self.gap_here(format!(
                "parameter {name} has {} cells for {} ABI slots",
                cells.len(),
                slot.slot_count()
            )));
        }
        self.f.params.extend(cells.iter().copied());
        match slot {
            RetSlot::Leaves(shape) => {
                self.group_binds.insert(raw(pb), (cells, shape.clone()));
            }
            RetSlot::Nested(inner) => {
                self.tuple_binds.insert(raw(pb), (cells, inner.clone()));
            }
            RetSlot::Opaque => unreachable!("guarded by slot.splats"),
        }
        Ok(())
    }

    /// One admission rule for call-site constants and callee default guards.
    /// `null` stays omitted; other unsupported expressions stay callee-bound.
    fn constant_param_default(
        &self,
        body: &Body,
        inf: Option<&BodyInference>,
        expr: Idx<Expr>,
        ty: Option<TyId>,
    ) -> Option<CallParamDefault> {
        let literal = match body.expr_arena[expr] {
            Expr::Int(v) if ty == Some(self.ws.builtins.uint()) => Lit::Uint(v as u32),
            Expr::Int(v) => Lit::Int(v),
            Expr::Float(v) => Lit::Float(v.0),
            Expr::Bool(v) => Lit::Bool(v),
            Expr::String(v) => Lit::Str(v),
            _ => {
                let TypeExpr::App { ctor: TypeCtor::Nominal(name), .. } = static_ty(self.ws, ty?)? else {
                    return None;
                };
                if name.as_str() != "Vec3" { return None; }
                return self.constant_vector_default(body, inf, expr);
            }
        };
        Some(CallParamDefault::Scalar(literal))
    }

    /// Recognize only a complete Vec3 literal or a constructor whose entire
    /// body returns one. Read both arenas in their lexical owners, never lower
    /// a foreign expression in the caller. Nonconstant inputs, spread/defaulted
    /// constructor arguments and bodies with effects remain callee-side.
    fn constant_vector_default(
        &self,
        body: &Body,
        inf: Option<&BodyInference>,
        expr: Idx<Expr>,
    ) -> Option<CallParamDefault> {
        let number = |body: &Body, e: Idx<Expr>| match body.expr_arena[e] {
            Expr::Float(v) => Some(Lit::Float(v.0)),
            Expr::Int(v) => Some(Lit::Float(v as f64)),
            _ => None,
        };
        let mut bindings = HashMap::new();
        let (body, expr) = match body.expr_arena[expr] {
            Expr::Call { callee: Callee::Unresolved { name, .. }, args, .. } => {
                let (module, func) = resolve_call_fn(self.ws, inf?, expr, name)?;
                let target = backend_variant(self.ws, name, FuncRef { module, func }, self.call_scx.target).ok()?;
                let module = self.ws.module(target.module);
                let decl = &module.functions[target.func];
                let ctor = module.bodies.get(&BodyOwner::Function(target.func))?;
                if decl.is_variadic || decl.is_coroutine || decl.is_extern || decl.inline_body.is_some()
                    || !decl.generics.is_empty()
                    || args.len() != decl.param_decls.len()
                    || ctor.param_bindings.len() != decl.param_decls.len()
                {
                    return None;
                }
                let params: Vec<_> = ctor.param_bindings.into_iter().collect();
                let mut next_pos = 0;
                for arg in args {
                    if body.spread_args.contains(&arg) { return None; }
                    let index = match body.arg_names.get(&arg) {
                        Some(name) => decl.param_decls.iter().position(|pd| pd.name == *name)?,
                        None => { let i = next_pos; next_pos += 1; i }
                    };
                    let binding = *params.get(index)?;
                    if bindings.insert(binding, number(body, arg)?).is_some() { return None; }
                }
                if bindings.len() != params.len() { return None; }
                let root = ctor.root?;
                let returned = match ctor.expr_arena[root] {
                    Expr::Block { statements, tail } => {
                        let statements: Vec<_> = statements.into_iter()
                            .filter(|&s| !matches!(ctor.statement_arena[s], Statement::Comment(_)))
                            .take(2)
                            .collect();
                        match (statements.as_slice(), tail) {
                            ([], Some(tail)) => tail,
                            ([statement], None) => match ctor.statement_arena[*statement] {
                                Statement::Return(Some(value)) => value,
                                _ => return None,
                            },
                            _ => return None,
                        }
                    }
                    Expr::Return(Some(value)) => value,
                    _ => root,
                };
                (ctor, returned)
            }
            _ => (body, expr),
        };
        let Expr::StructLiteral { name, fields, bases } = body.expr_arena[expr] else {
            return None;
        };
        if name.as_str() != "Vec3" || !bases.is_empty() { return None; }
        let shape = value_shape(self.ws, name)?;
        if shape.nullable || shape.fields.len() != 3 || fields.len() != shape.fields.len()
            || shape.fields.iter().any(|(_, field)| !matches!(field, FieldShape::Prim))
        {
            return None;
        }
        let mut values = HashMap::new();
        for field in fields {
            let field = &body.struct_lit_field_arena[field];
            let value = match body.expr_arena[field.value] {
                Expr::Var(binding) => bindings.get(&binding)?.clone(),
                _ => number(body, field.value)?,
            };
            if values.insert(field.name, value).is_some() { return None; }
        }
        let literals = shape.fields.iter().map(|(name, _)| values.remove(name)).collect::<Option<_>>()?;
        Some(CallParamDefault::Vector { shape, literals })
    }

    /// Constants read in the callee's lexical owner, independent of its wire ABI.
    /// Passing them explicitly avoids nil side exits with the caller's floats
    /// live. A default that is not a constant stays the callee's to evaluate:
    /// `None` passes nil, except that a nullable parameter with a non-null
    /// default gets [`Lit::Omitted`], because nil is its explicit `null`
    /// ([`param_null_overrides_default`]).
    pub(crate) fn call_param_defaults(&self, func: FuncRef) -> Vec<Option<CallParamDefault>> {
        let module = self.ws.module(func.module);
        let decl = &module.functions[func.func];
        let Some(body) = module.bodies.get(&BodyOwner::Function(func.func)) else { return Vec::new() };
        if decl.is_variadic { return Vec::new(); }
        let inf = self.ws.inference(func.module)
            .and_then(|ir| ir.per_fn.get(&BodyOwner::Function(func.func)));
        // A foreign wrapper fills only literal defaults and has no Rock
        // prologue to test the marker (`default_fill_prologue`).
        let rock_body = decl.inline_body.is_none() && !decl.is_extern;
        body.param_defaults.iter().enumerate().map(|(i, expr)| {
            let ty = decl.param_decls.get(i)?.ty;
            self.constant_param_default(body, inf, (*expr)?, Some(ty)).or_else(|| {
                (rock_body && param_null_overrides_default(self.ws, body, i, ty))
                    .then_some(CallParamDefault::Scalar(Lit::Omitted))
            })
        }).collect()
    }

    pub(crate) fn emit_param_default(
        &mut self,
        default: &CallParamDefault,
        slot: &RetSlot,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        match default {
            CallParamDefault::Scalar(lit) if slot.slot_count() == 1 => {
                Ok(vec![self.emit_cur(Inst::Const(lit.clone()))])
            }
            // A scalarized nullable value tells omitted from null by its
            // presence leaf, so an omitted one is nil in every leaf.
            CallParamDefault::Scalar(Lit::Omitted) => {
                Ok((0..slot.slot_count()).map(|_| self.emit_cur(Inst::Const(Lit::Nil))).collect())
            }
            CallParamDefault::Vector { shape, literals } => {
                let leaves: Vec<_> = literals.iter()
                    .map(|lit| self.emit_cur(Inst::Const(lit.clone()))).collect();
                match slot {
                    RetSlot::Leaves(expected) if expected == shape => Ok(leaves),
                    RetSlot::Opaque => {
                        let parts = leaves.into_iter().map(ValueRepr::Scalar).collect();
                        let value = self.pack(ValueRepr::Group { name: shape.name, parts, present: None })?;
                        Ok(vec![value])
                    }
                    _ => Err(self.gap_here("constant vector default does not match its parameter ABI")),
                }
            }
            _ => Err(self.gap_here("constant scalar default does not match its parameter ABI")),
        }
    }

    /// Explicit values for the parameters a positional call left off the end:
    /// a caller-supplied default ([`Self::call_param_defaults`]) becomes wire literals, a parameter without one keeps its
    /// nil (one per wire slot) so the callee fills it, and the trailing nils Lua
    /// would supply anyway are dropped. `param_shapes` is the scalar ABI's slot
    /// table; `None` is the boxed ABI (one slot each).
    pub(crate) fn trailing_default_args(
        &mut self,
        func: FuncRef,
        provided: usize,
        param_shapes: Option<&[RetSlot]>,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        let defaults = self.call_param_defaults(func);
        let slots_of = |i: usize| param_shapes.and_then(|s| s.get(i)).map_or(1, |s| s.slot_count());
        let last = (provided..defaults.len()).rev().find(|&i| defaults[i].is_some());
        let Some(last) = last else { return Ok(Vec::new()) };
        let mut out = Vec::new();
        for i in provided..=last {
            match &defaults[i] {
                Some(default) => {
                    let slot = param_shapes.and_then(|shapes| shapes.get(i)).unwrap_or(&RetSlot::Opaque);
                    out.extend(self.emit_param_default(default, slot)?);
                }
                _ => {
                    for _ in 0..slots_of(i) {
                        out.push(self.emit_cur(Inst::Const(Lit::Nil)));
                    }
                }
            }
        }
        Ok(out)
    }

    /// Callee-side default fill: an omitted argument arrives as Lua `nil`, so for
    /// each defaulted param emit `if cell == nil { cell = <default> }` at the top
    /// of the body (rock's convention — the default expr lives in THIS body's
    /// arena). Direct calls pass supported constants themselves; unsupported
    /// defaults and callers the emitter cannot see still need this prologue.
    /// A `null` default on a scalar cell emits nothing: omitted already is null.
    /// A nullable param with a non-null default tests for [`Lit::Omitted`]
    /// instead, so an explicit `null` stays null. A scalarized value param
    /// needs no marker: an explicit null value arrives with a `false` presence
    /// leaf, an omitted one with nil.
    fn fill_param_defaults(&mut self) -> Result<(), CompilerError> {
        let pbs: Vec<Idx<BindingInfo>> = self.body.param_bindings.into_iter().collect();
        let defaults = self.body.param_defaults.clone();
        for (i, def) in defaults.iter().enumerate() {
            let Some(def) = def else { continue };
            let Some(&pb) = pbs.get(i) else { continue };
            let ty = self.body.binding_arena[pb].declared_ty.or_else(|| self.binding_ty(pb));
            if self.constant_param_default(self.body, self.inf, *def, ty).is_some() {
                self.f.param_default_materialized.insert(i);
            }
            // A scalarized value param's omitted arg arrives as N nils — guard on
            // the FIRST leaf cell and fill all leaves from the default's leaves.
            if let Some((cells, shape)) = self.group_binds.get(&raw(pb)).cloned() {
                for &cell in &cells {
                    self.f.mark_default_param(cell);
                }
                let cur = self.emit_cur(Inst::Load(cells[0]));
                let nil = self.emit_cur(Inst::Const(Lit::Nil));
                let is_nil = self.emit_cur(Inst::Eq(cur, nil));
                self.f.param_default_guards.insert(is_nil, i);
                let then_bb = self.f.new_block();
                let join = self.f.new_block();
                self.f.set_term(self.cur, Term::Branch { cond: is_nil, then_bb, else_bb: join });
                self.cur = then_bb;
                let r = self.lower_expr_repr(*def)?;
                let leaves = self.repr_to_storage_leaves(&r, &shape, "struct default")?;
                self.f.param_default_values.insert(i, leaves.clone());
                for (c, v) in cells.iter().zip(leaves) {
                    self.emit_cur(Inst::Store { cell: *c, value: v });
                }
                if self.is_open() {
                    self.f.set_term(self.cur, Term::Goto(join));
                }
                self.cur = join;
                continue;
            }
            let Some(&cell) = self.binds.get(&raw(pb)) else { continue };
            if matches!(self.body.expr_arena[*def], Expr::Null) {
                continue;
            }
            let omitted = match self.body.binding_arena[pb].declared_ty {
                Some(ty) if param_null_overrides_default(self.ws, self.body, i, ty) => Lit::Omitted,
                _ => Lit::Nil,
            };
            self.f.mark_default_param(cell);
            let cur = self.emit_cur(Inst::Load(cell));
            let marker = self.emit_cur(Inst::Const(omitted));
            let is_omitted = self.emit_cur(Inst::Eq(cur, marker));
            self.f.param_default_guards.insert(is_omitted, i);
            let then_bb = self.f.new_block();
            let join = self.f.new_block();
            self.f.set_term(self.cur, Term::Branch { cond: is_omitted, then_bb, else_bb: join });
            self.cur = then_bb;
            let expected = self.binding_ty(pb).and_then(|ty| self.concrete_ty_id(ty));
            let dv = self.lower_expr_with_expected(*def, expected.as_deref())?;
            self.f.param_default_values.insert(i, vec![dv]);
            self.emit_cur(Inst::Store { cell, value: dv });
            self.f.set_term(self.cur, Term::Goto(join));
            self.cur = join;
        }
        Ok(())
    }

    /// Lower the body's root expression, ending the entry block in a `Return`.
    /// A top-level `Block` is special-cased so a tail-less body returns bare.
    fn lower_root(&mut self) -> Result<(), CompilerError> {
        let Some(root) = self.body.root else {
            self.f.set_term(self.cur, Term::Return(vec![]));
            return Ok(());
        };
        if let Expr::Block { statements, tail } = self.body.expr_arena[root] {
            self.defers.push(Vec::new());
            let terminated = self.lower_stmts(statements)?;
            if terminated {
                self.defers.pop();
                return Ok(());
            }
            // A coroutine result comes ONLY from explicit `return` statements
            // (bone discards a co fn's body tail without coercing it to R), so
            // ANY reachable body end -- bare fall-off or unchecked tail -- is a
            // fall-off for the non-unit-R rejection below.
            if self.f.is_coroutine {
                self.reject_nonunit_coroutine_falloff()?;
            }
            if !self.f.is_coroutine {
                self.lower_return(tail)?;
                self.defers.pop();
                return Ok(());
            }
            if let Some(tail) = tail {
                self.lower_stmt_expr(tail)?;
            }
            if !self.is_open() {
                self.defers.pop();
                return Ok(());
            }
            self.run_top_frame()?; // splice this scope's defers before the return
            self.defers.pop();
            self.f.set_term(self.cur, Term::Return(Vec::new()));
            Ok(())
        } else {
            self.lower_return(Some(root))
        }
    }

    /// Reject a REACHABLE fall-off in a `co fn` whose declared result R is a
    /// concrete non-unit, non-nullable type: bone coerces only explicit
    /// `return <expr>` statements to R (a coroutine's body tail is discarded,
    /// never coerced), so a fall-off would stamp the completed terminal with
    /// `__return` nil and `result()`/`take_result` would hand out a nil typed
    /// as R. Unit R (the default), nullable R (nil is a legal value), and
    /// generic R (unknown at the shared lowering; explicit `return` sites in
    /// instantiations are still checked by bone) all stay admitted. Called only
    /// when the body's end is reachable, so `while true` bodies never trip it.
    fn reject_nonunit_coroutine_falloff(&self) -> Result<(), CompilerError> {
        let plan = self.concrete_plan;
        let Some(result_id) = plan.signature.result else { return Ok(()) };
        let result = self.concrete_mono_view(result_id, ConcreteMonoConsumer::ReturnValidation);
        let crate::monoty::MonoTy::App {
            ctor: crate::monoty::MonoCtor::Nominal(name),
            params,
        } = &*result
        else {
            return Ok(())
        };
        if !bone_semantic::prelude::is_intrinsic_key(*name, "Coroutine") {
            return Ok(())
        }
        let Some(result) = params.get(1) else { return Ok(()) };
        let nonunit_concrete = match result {
            crate::monoty::MonoTy::Unit | crate::monoty::MonoTy::Nullable(_) => false,
            crate::monoty::MonoTy::Tuple(parts) => !parts.is_empty(),
            _ => true,
        };
        if nonunit_concrete {
            let msg = format!(
                "co fn `{}` can fall off the end of its body, but its coroutine result type is not `()` -- \
                 a completed coroutine would hand `result()` a nil typed as the declared result; \
                 add an explicit `return <value>` on every path",
                self.f.name
            );
            return Err(match self.fn_name_span {
                Some(span) => CompilerError::new_with_label(span, msg),
                None => CompilerError::new_no_location(msg),
            });
        }
        Ok(())
    }

    /// A return is an expression destination, rather than a consumer of a
    /// materialized expression value. Tail blocks and branches can forward that
    /// destination; only the leaves cross the existing return ABI boundary.
    ///
    /// Keep structured backends, coroutine completion and deferred scopes on
    /// value lowering. In particular, distributing a destination across a defer
    /// would duplicate cleanup and can move snapshot conversion across cleanup.
    fn lower_return(&mut self, opt: Option<Idx<Expr>>) -> Result<(), CompilerError> {
        let direct = self.scx.target == bone_semantic::hir::FnBackend::Lua
            && !self.f.is_coroutine
            && self.ret_rep.is_some()
            && self.defers.iter().all(Vec::is_empty);
        if direct && let Some(e) = opt {
            let outer = self.cur_span.replace(self.body.expr_spans[e]);
            let outer_expr = self.cur_expr.replace(e);
            let result = self.lower_expr_to_return(e);
            self.cur_span = outer;
            self.cur_expr = outer_expr;
            if let Some(result) = result {
                return result;
            }
        }
        let leaves = self.return_leaves(opt)?;
        if self.is_open() {
            self.run_all_defers()?;
            self.f.set_term(self.cur, Term::Return(leaves));
        }
        Ok(())
    }

    /// The return-value list for `opt`: under the scalar value ABI a value return
    /// flattens to its leaf scalars (`return v0, v1, ...`); otherwise it's the
    /// single (boxed) value, or empty for a bare return.
    fn return_leaves(&mut self, opt: Option<Idx<Expr>>) -> Result<Vec<Idx<Inst>>, CompilerError> {
        let Some(e) = opt else { return Ok(vec![]) };
        if self.ret_rep.is_none() {
            let gpu_context = self.site_owner.as_ref().is_some_and(|owner| {
                crate::intrinsics::is_gpu_context(
                    &self.ws.module(owner.func.module).functions[owner.func.func],
                )
            });
            if gpu_context {
                // Shader planning consumes value-form control flow to retain
                // GPU-only effects in a discarded tail expression.
                self.lower_expr(e)?;
            } else {
                self.lower_stmt_expr(e)?;
            }
            return Ok(vec![]);
        }
        if self
            .concrete_plan
            .signature
            .result
            .is_some_and(|result| {
                matches!(
                    self.concrete_mono_view(result, ConcreteMonoConsumer::ReturnLowering)
                        .peel_nullable()
                        .0,
                    MonoTy::App { ctor: MonoCtor::Ref, .. }
                )
            })
        {
            if self.flat_ret.is_some() {
                return self.flat_return_leaves(e);
            }
            return Ok(vec![self.lower_ref_borrow(e)?]);
        }
        if let Some(shape) = self.result_shape() {
            let is_phantom = match &self.body.expr_arena[e] {
                Expr::Call { callee: Callee::Unresolved { name, .. }, args, .. } => {
                    name.as_str() == "phantom" && args.clone().into_iter().next().is_none()
                }
                _ => false,
            };
            if is_phantom {
                self.lower_expr(e)?;
                return Ok((0..shape.leaf_count())
                    .map(|_| self.emit_cur(Inst::Const(Lit::Nil)))
                    .collect());
            }
            let r = self.lower_expr_repr(e)?;
            // A terminating block returns a placeholder, not an aggregate to project.
            if !self.is_open() { return Ok(Vec::new()); }
            if self.scx.rep_arena().value_shape(r.representation()).is_none() {
                return self.opaque_boundary_leaves(&r, &shape, "return boundary");
            }
            let r = ValueBundle::new(r.representation(), self.owned_value_children(r.into_value())?);
            return self.repr_to_leaves_bundle_after_presence(&r, &shape);
        }
        if let Some(shape) = self.result_tuple() {
            if shape.nullable {
                return self.nullable_tuple_return_slots(e, &shape);
            }
            if shape.splats() {
                let r = self.lower_expr_repr(e)?.into_value();
                if !self.is_open() { return Ok(Vec::new()); }
                let r = self.owned_value_children(r)?;
                return self.tuple_repr_to_slots(r, &shape);
            }
            if let Expr::Tuple(elems) = self.body.expr_arena[e] {
                return self.lower_tuple_operands(e, elems);
            }
            let v = self.lower_expr(e)?;
            if !self.is_open() { return Ok(Vec::new()); }
            return Ok(self.tuple_return_operands(v, shape.slots.len()));
        }
        let expected = self
            .concrete_plan
            .signature
            .result
            .map(|result| self.concrete_mono_view(result, ConcreteMonoConsumer::ReturnLowering));
        let v = self.lower_expr_with_expected(e, expected.as_deref())?;
        if !self.is_open() { return Ok(Vec::new()); }
        let v = match self.concrete_plan.signature.result {
            Some(result) => {
                let result = self.concrete_mono_view(result, ConcreteMonoConsumer::ReturnLowering);
                self.coerced_mono(v, e, &result)
            }
            None => v,
        };
        Ok(vec![v])
    }

    fn absent_tuple_slots(&mut self, shape: &TupleRetShape) -> Vec<Idx<Inst>> {
        let mut out = Vec::with_capacity(shape.slot_count());
        if shape.nullable {
            out.push(self.emit_cur(Inst::Const(Lit::Bool(false))));
        }
        while out.len() < shape.slot_count() {
            out.push(self.emit_cur(Inst::Const(Lit::Nil)));
        }
        out
    }

    /// The `present, <slots...>` wire for a `-> (..)?` return, the tuple analogue
    /// of the nullable arms in [`Self::repr_to_leaves`]: a literal `null` folds to
    /// `false, nil, ...` and a tuple value leads with `true`.
    ///
    /// A tuple-or-nil arriving as an already-packed SCALAR (a variable, an if-join)
    /// would need a runtime `~= nil` test and a per-slot branch. That is a real
    /// shape, just not one any in-tree caller writes, so it gaps rather than
    /// silently boxing the nobox element the wire exists to keep unboxed.
    fn nullable_tuple_return_slots(
        &mut self,
        e: Idx<Expr>,
        shape: &TupleRetShape,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        let r = self.lower_expr_repr(e)?.into_value();
        if !self.is_open() { return Ok(Vec::new()); }
        if let ValueRepr::Scalar(v) = &r {
            if !matches!(self.f.insts[*v], Inst::Const(Lit::Nil)) {
                return Err(gap(
                    "returning an already-packed nullable tuple through the splatting ABI \
                     (return the tuple literal or `null` directly)",
                ));
            }
            return Ok(self.absent_tuple_slots(shape));
        }
        self.tuple_repr_to_slots(r, shape)
    }

    /// The N return operands for a tuple-return `return <v>`. A fresh `MakeTuple`
    /// in return position (the common `return (a, b)` and the `return f()` forward
    /// where `f()` lowered to `MakeTuple(loads)`) is elided to its operands -- no
    /// table alloc. A tuple value from elsewhere (a var/field holding a packed
    /// table) is unpacked with `TupleIndex`.
    fn tuple_return_operands(&mut self, v: Idx<Inst>, n: usize) -> Vec<Idx<Inst>> {
        if let Inst::MakeTuple(vs) = &self.f.insts[v] {
            if vs.len() == n {
                return vs.clone();
            }
        }
        (0..n)
            .map(|i| self.emit_cur(Inst::TupleIndex { base: v, index: i }))
            .collect()
    }

    /// Flatten a tuple repr to the slot values `shape` describes -- the tuple
    /// analogue of [`Self::repr_to_leaves`]. A `Tuple` repr (a splatting call
    /// forwarded straight out, or a tuple literal) yields its parts; anything
    /// else is a packed tuple, so each element is read back with `TupleIndex`
    /// and a `Leaves` slot is field-read off the box.
    ///
    /// This owns the leading presence at a nullable shape, both arms, so it is
    /// the exact inverse of [`Self::tuple_from_slots`] -- one oracle for where
    /// presence sits on the wire. A repr that carries its OWN tag forwards that
    /// tag: `return maybe(b)` hands on the callee's presence, and synthesizing
    /// `true` here instead reported every absent forward as present.
    fn tuple_repr_to_slots(
        &mut self,
        r: ValueRepr,
        shape: &TupleRetShape,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        // A phantom tuple has no aggregate to project; every wire slot is absent.
        if let ValueRepr::Scalar(v) = &r {
            if matches!(self.f.insts[*v], Inst::Const(Lit::Nil)) {
                return Ok(self.absent_tuple_slots(shape));
            }
        }
        let present = match (shape.nullable, &r) {
            (false, _) => None,
            (true, ValueRepr::Tuple { present: Some(p), .. }) => Some(*p),
            (true, _) => Some(self.emit_cur(Inst::Const(Lit::Bool(true)))),
        };
        let parts: Vec<ValueRepr> = match r {
            ValueRepr::Tuple { parts, .. } if parts.len() == shape.slots.len() => parts,
            other => {
                let base = self.pack(other)?;
                (0..shape.slots.len())
                    .map(|index| ValueRepr::Scalar(self.emit_cur(Inst::TupleIndex { base, index })))
                    .collect()
            }
        };
        let mut out = Vec::with_capacity(shape.slot_count());
        out.extend(present);
        for (slot, part) in shape.slots.iter().zip(parts) {
            match slot {
                RetSlot::Opaque => out.push(self.pack(part)?),
                // A BOXED source at a non-nullable splatting slot is the iterator
                // protocol's "no element" placeholder (`return (false,
                // phantom())`), which is nil -- field-reading it would fault, and
                // the slot is dead anyway because the tuple's own flag guards it.
                // The nullable spread already branches on nil, so borrow it and
                // drop the presence leaf.
                RetSlot::Leaves(vs)
                    if !vs.nullable && !matches!(part, ValueRepr::Group { .. }) =>
                {
                    let mut nullable = vs.clone();
                    nullable.nullable = true;
                    let leaves = self.repr_to_leaves_shape(&part, &nullable)?;
                    out.extend(leaves.into_iter().skip(1));
                }
                RetSlot::Leaves(vs) => out.extend(self.repr_to_leaves_shape(&part, vs)?),
                RetSlot::Nested(inner) => out.extend(self.tuple_repr_to_slots(part, inner)?),
            }
        }
        Ok(out)
    }

    /// Rebuild the tuple repr `shape` describes from a flat slot list -- the
    /// inverse of [`Self::tuple_repr_to_slots`], used at a call site to hand a
    /// destructure its elements without ever boxing a `nobox` one.
    pub(crate) fn tuple_from_slots(
        &mut self,
        shape: &TupleRetShape,
        slots: &[Idx<Inst>],
        idx: &mut usize,
    ) -> ValueRepr {
        // Presence leads the wire, exactly as it does one level down in
        // `rebuild_from_leaves`, so it is consumed before the slots.
        let present = shape.nullable.then(|| {
            let v = slots[*idx];
            *idx += 1;
            v
        });
        let parts = shape
            .slots
            .clone()
            .iter()
            .map(|slot| self.slot_repr(slot, slots, idx))
            .collect();
        ValueRepr::Tuple { parts, present }
    }

    /// One element of [`Self::tuple_from_slots`], consuming its slots from `*idx`.
    fn slot_repr(&mut self, slot: &RetSlot, slots: &[Idx<Inst>], idx: &mut usize) -> ValueRepr {
        match slot {
            RetSlot::Opaque => {
                let v = slots[*idx];
                *idx += 1;
                ValueRepr::Scalar(v)
            }
            RetSlot::Leaves(vs) => self.rebuild_from_leaves(vs, slots, idx),
            RetSlot::Nested(inner) => self.tuple_from_slots(inner, slots, idx),
        }
    }

    /// The values ONE argument contributes to a flat call-argument list, per its
    /// param slot. Every calling convention routes through this so a caller and
    /// its callee cannot disagree about an argument's arity -- the failure mode
    /// there is silent, since a short list is nil-filled rather than rejected.
    pub(crate) fn arg_values_for_slot(
        &mut self,
        slot: &RetSlot,
        a: Idx<Expr>,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        if !slot.splats()
            && let Expr::Var(b) = self.body.expr_arena[a]
            && self.boxed_value_local(b, a).is_some()
            && !self.binding_ref_borrowed(b)
        {
            // An opaque slot lends the borrowed box: the callee copies its
            // parameter wherever the parameter escapes whole. Nothing writes a
            // box no `ref` reaches, so a callee may keep it past the call.
            return Ok(vec![self.emit_cur(Inst::Load(self.binds[&raw(b)]))]);
        }
        let r = self.lower_expr_repr(a)?;
        if !slot.splats() {
            return Ok(vec![self.pack_bundle(&r)?]);
        }
        self.repr_to_slot_values_bundle(&r, slot)
    }

    fn is_ref_call_arg(&self, call: Idx<Expr>, arg: Idx<Expr>) -> bool {
        self.inf
            .and_then(|inf| inf.ref_call_args.get(&call))
            .is_some_and(|args| args.contains(&arg))
            || self.is_flat_ref_call_arg(arg)
    }

    pub(crate) fn arg_values_for_call_slot(
        &mut self,
        call: Idx<Expr>,
        slot: &RetSlot,
        arg: Idx<Expr>,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        if self.is_ref_call_arg(call, arg) {
            return Ok(vec![self.lower_ref_borrow(arg)?]);
        }
        self.arg_values_for_slot(slot, arg)
    }

}

/// EXPLICIT_NULL_DEFAULT: whether parameter `index` of `body`, declared `ty`,
/// is nullable with a default other than `null`. Only such a parameter tells
/// an omitted argument from an explicit `null`: its callers pass
/// [`Lit::Omitted`] (or the constant default) when they omit it, and its
/// default fill tests for that marker instead of nil. Any other omitted
/// argument is nil, because nil and the default agree there.
pub fn param_null_overrides_default(ws: &Workspace, body: &Body, index: usize, ty: TyId) -> bool {
    let Some(Some(default)) = body.param_defaults.get(index) else { return false };
    if matches!(body.expr_arena[*default], Expr::Null) {
        return false;
    }
    match ty {
        TyId::Static(mid, idx) => matches!(ws.module(mid).ty_arena[idx], TypeExpr::Nullable(_)),
        // Declared parameter types are lowered into the module's static arena.
        TyId::Local(_) => false,
    }
}

/// [`param_null_overrides_default`] for a declared function's parameter.
pub fn function_param_null_overrides_default(ws: &Workspace, func: FuncRef, index: usize) -> bool {
    let module = ws.module(func.module);
    let Some(body) = module.bodies.get(&BodyOwner::Function(func.func)) else { return false };
    module.functions[func.func]
        .param_decls
        .get(index)
        .is_some_and(|pd| param_null_overrides_default(ws, body, index, pd.ty))
}

// -- Map value-struct key rejection (a fatal emit-time pass; see
// `value_struct_map_key`). These mirror the `Lowerer` type helpers but take
// `(ws, inf)` directly so the check runs without building a full lowering
// context. --

pub(crate) fn ty_node(ws: &Workspace, inf: &BodyInference, ty: TyId) -> Option<TypeExpr> {
    match ty {
        TyId::Static(mid, idx) => Some(ws.module(mid).ty_arena[idx]),
        TyId::Local(idx) => Some(inf.ty_arena[idx]),
    }
}

fn resolve_tyid(ws: &Workspace, inf: &BodyInference, ty: TyId) -> Option<TyId> {
    match ty_node(ws, inf, ty)? {
        TypeExpr::Infer(var, _) => resolve_tyid(ws, inf, inf.substitutions.get(var)?),
        _ => Some(ty),
    }
}

/// Why `K` cannot be a `Map` key, or `None` when it is admitted.
///
/// An ALLOW-LIST, not a scan for known-bad shapes. Lua keys a table by identity
/// for everything that is not a scalar, so the admitted set is the closed one --
/// int/float/string/bool are value-keyed, and a `newtype` is admitted exactly
/// when what it wraps is. Every other concrete key misses silently: a cdata
/// (`value` struct, cdata-tier payload enum) hashes by pointer, and a table
/// (tuple, array, plain struct, table-tier payload enum) by table identity.
///
/// An UNRESOLVED key is admitted, and that is load-bearing rather than lenient:
/// `fn Map<K, V>.insert` in the stdlib is generic, and rejecting a type
/// parameter would refuse the declaration of every Map method. Only a key that
/// resolves to a concrete non-admitted type is a finding.
fn map_key_rejection_reason(
    ws: &Workspace,
    inf: &BodyInference,
    k: TyId,
    depth: u32,
) -> Option<String> {
    // A newtype chain is finite, but a malformed self-wrapping one must not spin.
    if depth > 8 {
        return None;
    }
    match ty_node(ws, inf, k)? {
        TypeExpr::Builtin(
            BuiltinTy::Int
            | BuiltinTy::Uint
            | BuiltinTy::Float
            | BuiltinTy::F32
            | BuiltinTy::Bool
            | BuiltinTy::String
            // Not key shapes, but nothing here can decide against them either.
            | BuiltinTy::Any
            | BuiltinTy::Unit
            | BuiltinTy::Never
            | BuiltinTy::Error,
        ) => None,
        // A type parameter or an uninstantiated call-site var: undecidable here.
        TypeExpr::Generic(..) | TypeExpr::Infer(..) => None,
        TypeExpr::App { ctor: TypeCtor::Nominal(name), .. } => {
            if let Some(nid) = ws.newtypes_by_name.get(&name) {
                let wrapped = ws.modules[nid.module.index()].newtypes[nid.def].wrapped;
                let wrapped = resolve_tyid(ws, inf, wrapped)?;
                return map_key_rejection_reason(ws, inf, wrapped, depth + 1).map(|why| {
                    format!("`{name}` wraps {why}, and a newtype is keyed as what it wraps")
                });
            }
            // A payload-free enum lowers to an int ordinal, so it IS value-keyed.
            // One payload variant is enough to make the type cdata- or
            // table-carrying, and the key that misses is the payload-carrying
            // one -- a `Color::Red` control would hit and hide it.
            if let Some(eid) = ws.enums_by_name.get(&name) {
                let e = &ws.modules[eid.module.index()].enums[eid.def];
                return e
                    .variants
                    .iter()
                    .any(|v| !matches!(v.payload, VariantPayload::Unit))
                    .then(|| {
                        format!(
                            "`{name}` has payload-carrying variants, keyed by cdata or object \
                             identity (a payload-free enum is an int ordinal and stays allowed)"
                        )
                    });
            }
            if map_is_value_struct(ws, name) {
                Some(format!("`{name}` is a `value` struct, keyed by cdata identity"))
            } else {
                Some(format!("`{name}` is a struct, keyed by object identity"))
            }
        }
        TypeExpr::Tuple { .. } => {
            Some("a tuple is an aggregate, keyed by object identity".to_string())
        }
        TypeExpr::App { ctor: TypeCtor::Array, .. } => {
            Some("an array is an aggregate, keyed by object identity".to_string())
        }
        TypeExpr::App { ctor: TypeCtor::Map, .. } => {
            Some("a `Map` is an aggregate, keyed by object identity".to_string())
        }
        TypeExpr::App { ctor: TypeCtor::Ref, .. } => {
            Some("a frame reference cannot be stored as a map key".to_string())
        }
        TypeExpr::Nullable(..) => {
            Some("a nullable key cannot be distinguished from a missing entry".to_string())
        }
        TypeExpr::Function { .. } => {
            Some("a function is keyed by identity".to_string())
        }
    }
}

fn map_is_value_struct(ws: &Workspace, name: Ustr) -> bool {
    ws.struct_by_name
        .get(&name)
        .map(|sid| has_value_plan(&ws.modules[sid.module.index()].structs[sid.def]))
        .unwrap_or(false)
}

/// When `ty` resolves to `Map<K, V>` with an inadmissible `K`, returns why.
fn map_key_rejected(ws: &Workspace, inf: &BodyInference, ty: TyId) -> Option<String> {
    let ty = resolve_tyid(ws, inf, ty)?;
    let TypeExpr::App { ctor: TypeCtor::Map, params } = ty_node(ws, inf, ty)? else {
        return None;
    };
    let k = resolve_tyid(ws, inf, ty.with_idx(params.into_iter().next()?))?;
    map_key_rejection_reason(ws, inf, k, 0)
}

/// Scan module `mid`'s function bodies for a `Map<K, V>` whose key `K` is not
/// value-keyed, and return the first offending (message, span).
///
/// The failure this prevents is SILENT: a runtime-built key misses an entry
/// stored under an equal-by-value one, so `m.insert(k, v)` twice at equal `k`
/// grows the map instead of replacing (measured: `Map<(int,int), string>` -> len
/// 2, against a string control at len 1). Nothing at runtime reports it, and
/// `a == b` can be true on the very keys that miss.
pub fn map_key_rejection(ws: &Workspace, mid: ModuleId) -> Option<(String, Span)> {
    let module = ws.module(mid);
    let ir = ws.inference(mid)?;
    for (fid, _f) in module.functions.iter() {
        let owner = BodyOwner::Function(fid);
        let (Some(body), Some(inf)) = (module.bodies.get(&owner), ir.per_fn.get(&owner)) else {
            continue;
        };
        for (e, tyid) in inf.expr_types.iter() {
            if let Some(why) = map_key_rejected(ws, inf, *tyid) {
                return Some((why, body.expr_spans[e]));
            }
        }
    }
    None
}

/// A GC-carrying `value` struct (table repr, no C layout) named at a nominal
/// position. Not a legal C boundary type: no `ffi.cdef` typedef exists, so
/// `ffi.new`/`sizeof`/`ffi.istype`/the extern ABI cannot name it.
fn nominal_is_gc_value(ws: &Workspace, name: Ustr) -> bool {
    map_is_value_struct(ws, name)
        && !matches!(
            crate::lower::declared_value_storage(ws, bone_semantic::hir::FnBackend::Lua, name),
            Some(ValueStorage::CData)
        )
}

fn gc_value_under_newtypes(
    ws: &Workspace,
    ty: MonoTy,
) -> Result<Option<Ustr>, crate::monoty::NewtypeResolutionError> {
    let ty = crate::monoty::resolve_newtype_chain(ws, ty)?;
    let MonoTy::App { ctor: MonoCtor::Nominal(name), .. } = ty else {
        return Ok(None);
    };
    Ok(nominal_is_gc_value(ws, name).then_some(name))
}

/// Reject a GC-carrying value struct used at a genuine C boundary (an `extern`
/// signature) -- a spanned emit-time error, since
/// the cdef the boundary needs was never emitted for this tier. Replaces the old
/// silent skip-then-crash (unfuck #51). Returns `(message, span)` for the first
/// offender, scanning externs then body types. FFI-clean value structs are
/// unaffected (they have a cdef and stay boundary-legal).
pub fn gc_value_c_boundary(ws: &Workspace, mid: ModuleId) -> Option<(String, Span)> {
    let module = ws.module(mid);
    // 1. extern signatures: a param/return that is a GC-carrying value struct.
    for (_, f) in module.functions.iter() {
        if !f.is_extern {
            continue;
        }
        let tys = f
            .param_decls
            .iter()
            .map(|p| (p.ty, false))
            .chain(f.return_ty.map(|ty| (ty, true)));
        for (ty, is_return) in tys {
            if matches!(static_ty(ws, ty), Some(TypeExpr::App { ctor: TypeCtor::Nominal(_), .. })) {
                let normalized = crate::monoty::convert_decl(
                    ws,
                    ty,
                    &rustc_hash::FxHashMap::default(),
                )
                .map_err(|reason| crate::monoty::NewtypeResolutionError::Conversion(reason))
                .and_then(|ty| gc_value_under_newtypes(ws, ty));
                match normalized {
                    Ok(Some(n)) => {
                        return Some((format!(
                            "`{n}` is a GC-carrying `value` struct (immutable Lua table) and cannot \
                             cross an `extern` C boundary -- it has no C layout. Use an FFI-clean \
                             value struct (fields int/float/bool or nested FFI-clean value structs), \
                             or pass the fields individually."
                        ), f.name_span));
                    }
                    Ok(None) => {}
                    Err(reason) => {
                        let context = if is_return {
                            format!("extern `{}` returns {reason}", f.name)
                        } else {
                            format!("extern `{}` has a parameter type {reason}", f.name)
                        };
                        return Some((format!(
                            "{context}, which has no C representation. Declare a concrete type."
                        ), f.name_span));
                    }
                }
            }
        }
    }
    None
}

// -- compile-time expansion admission ---------------------------------------
// The lowering expander replaces a valid call; this pass is the FATAL gate that
// rejects source-independent misuse, since a per-fn lowering error would only be
// recorded as a lenient skip. Layout-dependent checks stay in the concrete
// expanders, where the retained instance substitution is available.

/// Resolve a direct call through inference first, falling back to a
/// unique workspace overload when inference did not record one.
fn resolve_call_fn(
    ws: &Workspace,
    inf: &BodyInference,
    e: Idx<Expr>,
    name: Ustr,
) -> Option<(ModuleId, Idx<Function>)> {
    if let Some(&r) = inf.resolved_overload_calls.get(&e) {
        return Some(r);
    }
    match ws.functions_by_name.get(&name).map(|l| l.as_slice()) {
        Some([single]) => Some(*single),
        _ => None,
    }
}

fn struct_expand_rejection_for_call(
    ws: &Workspace,
    body: &Body,
    call: Idx<Expr>,
    f: &Function,
    args: &[Idx<Expr>],
) -> Option<CompilerError> {
    let fname = f.name;
    let call_span = body.expr_spans[call];
    let reject = |span: Span, msg: String| Some(CompilerError::new_with_label(span, msg));

    let [_arg] = args else {
        return reject(call_span, format!("`@struct_expand` fn `{fname}` takes exactly one argument"));
    };

    if f.return_ty.and_then(|ret| pairs_return_enum(ws, ret)).is_none() {
        return reject(
            call_span,
            format!(
                "`@struct_expand` fn `{fname}` must return `[(string, <enum>)]`, got `{}`",
                f.return_ty.map(|r| ws.render(None, r)).unwrap_or_else(|| "()".to_string())
            ),
        );
    }
    // Field layout belongs to the retained concrete expander. Source preflight
    // keeps only the permanent arity and return-shape gate above.
    None
}

/// A resolved `rock_shader(...)` pipeline. Two forms:
/// - two-arg `rock_shader(vert, frag)`: `vert = Some(..)`, `key = "<vert>+<frag>"`;
///   the driver runs `emit_shader_pipeline`.
/// - one-arg fullscreen `rock_shader(frag)`: `vert = None`, `key = "<frag>"` (a
///   bare name, never containing '+', so it can never collide with a pair key);
///   the driver runs `emit_shader_entry`.
/// `key` is the pipeline name const `pipeline()` looks up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShaderPair {
    pub key: String,
    pub vert: Option<FuncRef>,
    pub frag: FuncRef,
}

pub struct ShaderCollection {
    pub pairs: Vec<ShaderPair>,
    pub callsites: Vec<(ModuleId, BodyOwner)>,
    pub contributions: HashMap<(ModuleId, BodyOwner), Vec<ShaderPair>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComputeEntry {
    pub key: String,
    pub entry: FuncRef,
}

pub struct ComputeCollection {
    pub entries: Vec<ComputeEntry>,
    pub callsites: Vec<(ModuleId, BodyOwner)>,
    pub contributions: HashMap<(ModuleId, BodyOwner), Vec<ComputeEntry>>,
}

/// The `@shader(<stage>)` stage of `f`, else `None`.
/// A local read of the annotation (skull-codegen-glsl has the same helper, but
/// skull-mir must not depend on it -- the layering constraint).
fn shader_stage_of(f: &Function) -> Option<&'static str> {
    let ann = f.annotations.iter().find(|a| a.is_builtin("shader"))?;
    match ann.args.first().map(|a| &a.value) {
        Some(bone_semantic::hir::AnnoValue::Ident(id)) if id.name.as_str() == "vertex" => Some("vertex"),
        Some(bone_semantic::hir::AnnoValue::Ident(id)) if id.name.as_str() == "fragment" => {
            Some("fragment")
        }
        Some(bone_semantic::hir::AnnoValue::Ident(id)) if id.name.as_str() == "compute" => {
            Some("compute")
        }
        _ => None,
    }
}

pub fn resolve_compute_entry(
    ws: &Workspace,
    body: &Body,
    call_expr: Idx<Expr>,
    args: &[Idx<Expr>],
) -> Result<FuncRef, CompilerError> {
    let span = body.expr_spans[call_expr];
    let reject = |message: String| CompilerError::new_with_label(span, message);
    let [arg] = args else {
        return Err(reject(
            "`rock_compute` takes exactly one direct @shader(compute) reference".to_string(),
        ));
    };
    let Expr::Global(name) = body.expr_arena[*arg] else {
        return Err(reject(
            "`rock_compute` argument must be a direct reference to a top-level @shader(compute) fn"
                .to_string(),
        ));
    };
    let fref = match ws.functions_by_name.get(&name).map(|list| list.as_slice()) {
        Some([(module, func)]) => FuncRef { module: *module, func: *func },
        _ => {
            return Err(reject(format!(
                "`rock_compute` argument `{name}` must be a direct reference to a unique top-level fn"
            )))
        }
    };
    let function = &ws.module(fref.module).functions[fref.func];
    match shader_stage_of(function) {
        Some("compute") => Ok(fref),
        Some(stage) => Err(reject(format!(
            "`rock_compute` expects a @shader(compute) fn here, but `{name}` is @shader({stage})"
        ))),
        None => Err(reject(format!(
            "`rock_compute` argument `{name}` is not a @shader(compute) fn"
        ))),
    }
}

/// Resolve one `rock_shader` argument to a direct reference to a top-level
/// `@shader(<want_stage>)` fn. Rejects (spanned) a non-`Expr::Global` arg (a
/// local, lambda, literal, call -- anything that is not a bare top-level fn
/// name), a name that is not a unique top-level fn, a non-`@shader` fn, or a
/// stage mismatch.
fn resolve_shader_arg(
    ws: &Workspace,
    body: &Body,
    arg: Idx<Expr>,
    want_stage: &str,
) -> Result<FuncRef, CompilerError> {
    let span = body.expr_spans[arg];
    let reject = |msg: String| CompilerError::new_with_label(span, msg);
    let Expr::Global(name) = body.expr_arena[arg] else {
        return Err(reject(format!(
            "`rock_shader` argument must be a direct reference to a top-level `@shader({want_stage})` fn"
        )));
    };
    let fref = match ws.functions_by_name.get(&name).map(|l| l.as_slice()) {
        Some([(module, func)]) => FuncRef { module: *module, func: *func },
        _ => {
            return Err(reject(format!(
                "`rock_shader` argument `{name}` is not a unique top-level fn"
            )))
        }
    };
    let f = &ws.module(fref.module).functions[fref.func];
    match shader_stage_of(f) {
        Some(stage) if stage == want_stage => {
            if let Some(annotation) = f
                .param_decls
                .iter()
                .flat_map(|param| param.annotations.iter())
                .find(|annotation| annotation.is_builtin("storage"))
            {
                return Err(CompilerError::new_with_label(
                    annotation.span,
                    format!(
                        "graphics entry `{}` cannot declare @storage resources; storage resources are compute-only",
                        f.name
                    ),
                ));
            }
            Ok(fref)
        }
        Some(other) => Err(reject(format!(
            "`rock_shader` expects a `@shader({want_stage})` fn here, but `{name}` is `@shader({other})`"
        ))),
        None => Err(reject(format!(
            "`rock_shader` argument `{name}` is not a `@shader` fn"
        ))),
    }
}

/// Resolve a `rock_shader(...)` call's args to a [`ShaderPair`], or the first
/// spanned rejection. Arity dispatches the form: one arg -> fullscreen
/// (`vert = None`, key = bare frag name), two args -> pair (key = `"<vert>+<frag>"`).
/// Any other arity rejects, naming BOTH legal forms. Shared by the lowering
/// expander ([`Builder::expand_shader_pipeline`]), the fatal gate
/// ([`shader_expand_rejection`]), and the driver collector
/// ([`collect_shader_pairs`]) -- one resolver, never forked.
pub fn resolve_shader_pair(
    ws: &Workspace,
    body: &Body,
    call_expr: Idx<Expr>,
    args: &[Idx<Expr>],
) -> Result<ShaderPair, CompilerError> {
    match args {
        [frag_arg] => {
            let frag = resolve_shader_arg(ws, body, *frag_arg, "fragment")?;
            let fname = ws.module(frag.module).functions[frag.func].name;
            // Bare frag name -- no '+', so it can never collide with a pair key.
            Ok(ShaderPair { key: fname.to_string(), vert: None, frag })
        }
        [vert_arg, frag_arg] => {
            let vert = resolve_shader_arg(ws, body, *vert_arg, "vertex")?;
            let frag = resolve_shader_arg(ws, body, *frag_arg, "fragment")?;
            let vname = ws.module(vert.module).functions[vert.func].name;
            let fname = ws.module(frag.module).functions[frag.func].name;
            Ok(ShaderPair { key: format!("{vname}+{fname}"), vert: Some(vert), frag })
        }
        _ => Err(CompilerError::new_with_label(
            body.expr_spans[call_expr],
            "`rock_shader` takes either one argument -- rock_shader(fragment_fn) \
             (fullscreen) -- or two -- rock_shader(vertex_fn, fragment_fn)"
                .to_string(),
        )),
    }
}

fn shader_expand_rejection_for_call(
    ws: &Workspace,
    body: &Body,
    call: Idx<Expr>,
    args: &[Idx<Expr>],
) -> Option<CompilerError> {
    resolve_shader_pair(ws, body, call, args).err()
}

/// Emit-time validation for source-independent `@uniform_expand` shape errors.
fn uniform_expand_rejection_for_call(
    ws: &Workspace,
    body: &Body,
    call: Idx<Expr>,
    f: &Function,
    args: &[Idx<Expr>],
) -> Option<CompilerError> {
    let fname = f.name;
    let reject = |span: Span, msg: String| Some(CompilerError::new_with_label(span, msg));

    match args {
        [_u] => {
            let returns_int = f
                .return_ty
                .map(|rt| ws.render(None, rt) == "int")
                .unwrap_or(false);
            if !returns_int {
                return reject(body.expr_spans[call], uniform_expand_arity_msg(fname));
            }
        }
        [_, _, _] => {}
        _ => return reject(body.expr_spans[call], uniform_expand_arity_msg(fname)),
    }
    // The uniform layout is derived only by the concrete expander.
    None
}

fn uniform_values_rejection_for_call(
    ws: &Workspace,
    inf: &BodyInference,
    body: &Body,
    call: Idx<Expr>,
    f: &Function,
    args: &[Idx<Expr>],
) -> Option<CompilerError> {
    let fname = f.name;
    let [_u] = args else {
        return Some(CompilerError::new_with_label(
            body.expr_spans[call],
            format!("`@uniform_values` fn `{fname}` takes exactly one uniform group"),
        ));
    };
    let Some(&ret_ty) = inf.expr_types.get(call) else {
        return Some(CompilerError::new_with_label(
            body.expr_spans[call],
            format!("`@uniform_values` fn `{fname}` has no inferred packet result"),
        ));
    };
    let Some(ret_ty) = resolve_tyid(ws, inf, ret_ty) else {
        return Some(CompilerError::new_with_label(
            body.expr_spans[call],
            format!("`@uniform_values` fn `{fname}` has no concrete packet result"),
        ));
    };
    let Some(TypeExpr::App { ctor: TypeCtor::Nominal(block_name), .. }) = static_ty(ws, ret_ty) else {
        return Some(CompilerError::new_with_label(
            body.expr_spans[call],
            format!("`@uniform_values` fn `{fname}` must return the canonical packet struct"),
        ));
    };
    if let Err(msg) = uniform_values_return_shape(ws, block_name) {
        return Some(CompilerError::new_with_label(
            body.expr_spans[call],
            format!("`@uniform_values`: {msg}"),
        ));
    }
    None
}

/// Resolve a `build_var("NAME")` / `embed_string("path")` call to its value. THE shared resolver:
/// the lowering expander
/// and the fatal gate both call it, so they cannot diverge on what they
/// accept. The argument must be a plain string literal -- interpolation or any
/// computed expression rejects, because the input has to be knowable without
/// running the body.
pub fn resolve_comptime_value(
    ws: &Workspace,
    body: &Body,
    call_expr: Idx<Expr>,
    args: &[Idx<Expr>],
    kind: ExpandKind,
) -> Result<ResolvedComptime, CompilerError> {
    let fname = match kind {
        ExpandKind::BuildVar => "build_var",
        ExpandKind::EmbedString => "embed_string",
        _ => unreachable!("resolve_comptime_value only serves the comptime-input kinds"),
    };
    let reject = |span: Idx<Expr>, msg: String| {
        CompilerError::new_with_label(body.expr_spans[span], msg)
    };
    let [arg] = args else {
        return Err(reject(call_expr, format!("`{fname}` takes exactly one argument")));
    };
    let Expr::String(key) = body.expr_arena[*arg] else {
        return Err(reject(
            *arg,
            format!("`{fname}` argument must be a plain string literal (no interpolation)"),
        ));
    };
    match kind {
        ExpandKind::BuildVar => match ws.comptime_inputs.vars.get(&key) {
            Some(var) => Ok(ResolvedComptime { value: var.value.clone(), file_abs: None }),
            // A fully-default inputs struct means resolution never RAN for
            // this compile (no stick.toml found, or an entry point that did
            // not stamp declared inputs) -- blaming the project's [build.vars]
            // there sent people at the wrong file. A stamped project always
            // carries at least its embed_root.
            None if ws.comptime_inputs.embed_root.is_none()
                && ws.comptime_inputs.vars.is_empty()
                && ws.comptime_inputs.channel.is_empty() =>
            {
                Err(reject(
                    call_expr,
                    format!(
                        "`build_var(\"{key}\")`: no [build.vars] were resolved for this \
                         compile (no stick.toml found at or above the entry point, or \
                         this rock entry point did not resolve declared inputs)"
                    ),
                ))
            }
            None => Err(reject(
                call_expr,
                format!("`build_var(\"{key}\")`: {key} is not declared in this project's [build.vars]"),
            )),
        },
        ExpandKind::EmbedString => {
            // Resolution root: the nearest stick.toml at/above the CALLING
            // file's dir: a library embeds its own files. Fall back to the
            // invocation's embed_root for synthetic/test module files.
            let call_file = body.expr_spans[call_expr].file;
            let files = &ws.comptime_inputs.embed_files;
            let package_root = files.package_root(call_file.as_str());
            let Some(root) = package_root.or_else(|| ws.comptime_inputs.embed_root.clone()) else {
                return Err(reject(
                    call_expr,
                    "`embed_string` needs a package root (a stick.toml at or above the \
                     calling file, or a project root); none is available in this compilation"
                        .to_string(),
                ));
            };
            let rel = std::path::Path::new(key.as_str());
            if rel.is_absolute() {
                return Err(reject(
                    *arg,
                    format!("`embed_string` path must be relative to the package root, got {key:?}"),
                ));
            }
            use bone_core::comptime_files::EmbedReadError;
            let (file, value) = files.read(&root, rel).map_err(|e| match e {
                EmbedReadError::Unreadable(e) => {
                    reject(*arg, format!("`embed_string(\"{key}\")`: cannot read {key:?}: {e}"))
                }
                EmbedReadError::RootUnreadable(e) => {
                    reject(call_expr, format!("`embed_string`: package root is unreadable: {e}"))
                }
                EmbedReadError::EscapesRoot => reject(
                    *arg,
                    format!("`embed_string(\"{key}\")`: path escapes the package root"),
                ),
            })?;
            Ok(ResolvedComptime { value, file_abs: Some(Ustr::from(&file)) })
        }
        _ => unreachable!(),
    }
}

/// A resolved comptime input: the value, plus (for embeds) the absolute path
/// the read landed on -- what invalidation re-reads and the watcher watches.
pub struct ResolvedComptime {
    pub value: String,
    pub file_abs: Option<Ustr>,
}

/// FATAL gate for `build_var` / `embed_string` misuse (non-literal argument,
/// undeclared var, unreadable or escaping path): a per-fn lowering error is
/// only a lenient skip, so validation lives here. Shares
/// [`resolve_comptime_value`] with the expander.
pub fn comptime_input_rejection(ws: &Workspace, mid: ModuleId) -> Option<CompilerError> {
    let module = ws.module(mid);
    let ir = ws.inference(mid)?;
    for (owner, body) in module.bodies.iter() {
        let Some(inf) = ir.per_fn.get(owner) else { continue };
        for (e, expr) in body.expr_arena.iter() {
            let Expr::Call { callee: Callee::Unresolved { name, .. }, args, .. } = expr else {
                continue;
            };
            let Some((fmod, fid)) = resolve_call_fn(ws, inf, e, *name) else { continue };
            let kind = crate::intrinsics::expand_kind(&ws.module(fmod).functions[fid]);
            let Some(kind @ (ExpandKind::BuildVar | ExpandKind::EmbedString)) = kind else {
                continue;
            };
            let arg_idxs: Vec<Idx<Expr>> = args.into_iter().collect();
            if let Err(err) = resolve_comptime_value(ws, body, e, &arg_idxs, kind) {
                return Some(err);
            }
        }
    }
    None
}

/// The ABSOLUTE path of every `embed_string` input the program reads, found
/// by the SAME resolution emission uses (`resolve_comptime_value` -- one
/// oracle, so this cannot drift from what actually gets baked). For watchers
/// over emitters that keep no chunk cache (the JS web watch); the Lua side
/// reads its `SkullEmitCache::comptime_file_hashes` instead.
pub fn comptime_embed_files(ws: &Workspace) -> Vec<Ustr> {
    let mut out: Vec<Ustr> = Vec::new();
    for module in ws.modules.iter() {
        if module.id == ModuleId::builtin() {
            continue;
        }
        let Some(ir) = ws.inference(module.id) else { continue };
        for (owner, body) in module.bodies.iter() {
            let Some(inf) = ir.per_fn.get(owner) else { continue };
            for (e, expr) in body.expr_arena.iter() {
                let Expr::Call { callee: Callee::Unresolved { name, .. }, args, .. } = expr else {
                    continue;
                };
                let Some((fmod, fid)) = resolve_call_fn(ws, inf, e, *name) else { continue };
                let kind = crate::intrinsics::expand_kind(&ws.module(fmod).functions[fid]);
                if !matches!(kind, Some(ExpandKind::EmbedString)) {
                    continue;
                }
                let arg_idxs: Vec<Idx<Expr>> = args.into_iter().collect();
                if let Ok(resolved) =
                    resolve_comptime_value(ws, body, e, &arg_idxs, ExpandKind::EmbedString)
                {
                    if let Some(path) = resolved.file_abs {
                        out.push(path);
                    }
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Emit-time validation for `@vertex_expand` calls: misuse is a spanned
/// compile error, never a call that reaches the fn's error body.
fn vertex_expand_rejection_for_call(
    ws: &Workspace,
    body: &Body,
    call: Idx<Expr>,
    f: &Function,
    args: &[Idx<Expr>],
) -> Option<CompilerError> {
    let fname = f.name;
    let call_span = body.expr_spans[call];
    let reject = |span: Span, msg: String| Some(CompilerError::new_with_label(span, msg));

    let [_arg] = args else {
        return reject(
            call_span,
            format!("`@vertex_expand` fn `{fname}` takes exactly one argument"),
        );
    };
    if f.return_ty.and_then(|r| vertex_expand_return(ws, r)).is_none() {
        return reject(call_span, vertex_expand_ret_msg(ws, fname, f.return_ty));
    }
    // The element type and vertex plan belong to the concrete expander.
    None
}

struct ExpansionRejections {
    struct_pairs: Option<CompilerError>,
    shader_pipeline: Option<CompilerError>,
    compute_artifact: Option<CompilerError>,
    vertex_layout: Option<CompilerError>,
    uniform_block: Option<CompilerError>,
    uniform_values: Option<CompilerError>,
}

impl ExpansionRejections {
    fn record(&mut self, kind: ExpandKind, rejection: Option<CompilerError>) {
        let Some(rejection) = rejection else { return };
        let slot = match kind {
            ExpandKind::StructPairs => &mut self.struct_pairs,
            ExpandKind::ShaderPipeline => &mut self.shader_pipeline,
            ExpandKind::ComputeArtifact => &mut self.compute_artifact,
            ExpandKind::VertexLayout => &mut self.vertex_layout,
            ExpandKind::UniformBlock => &mut self.uniform_block,
            ExpandKind::UniformValues => &mut self.uniform_values,
            ExpandKind::SceneTypeKey => return,
            ExpandKind::BuildVar | ExpandKind::EmbedString => return,
            ExpandKind::Materialize => return,
        };
        if slot.is_none() {
            *slot = Some(rejection);
        }
    }

    fn finish(self) -> Option<CompilerError> {
        self.struct_pairs
            .or(self.shader_pipeline)
            .or(self.compute_artifact)
            .or(self.vertex_layout)
            .or(self.uniform_block)
            .or(self.uniform_values)
    }
}

/// Scan module `mid` once for every direct expansion call. The first rejection
/// is retained per expansion category, then the categories are applied in the
/// historical priority order rather than in lexical order.
pub fn expansion_rejection(ws: &Workspace, mid: ModuleId) -> Option<CompilerError> {
    let module = ws.module(mid);
    let ir = ws.inference(mid)?;
    let mut rejections = ExpansionRejections {
        struct_pairs: None,
        shader_pipeline: None,
        compute_artifact: None,
        vertex_layout: None,
        uniform_block: None,
        uniform_values: None,
    };
    for (owner, body) in module.bodies.iter() {
        let Some(inf) = ir.per_fn.get(owner) else { continue };
        for (e, expr) in body.expr_arena.iter() {
            let Expr::Call { callee: Callee::Unresolved { name, .. }, args, .. } = expr else {
                continue;
            };
            let Some((fmod, fid)) = resolve_call_fn(ws, inf, e, *name) else { continue };
            let f = &ws.module(fmod).functions[fid];
            let Some(kind) = crate::intrinsics::expand_kind(f) else { continue };
            let arg_idxs: Vec<Idx<Expr>> = args.into_iter().collect();
            let rejection = match kind {
                ExpandKind::StructPairs => {
                    struct_expand_rejection_for_call(ws, body, e, f, &arg_idxs)
                }
                ExpandKind::ShaderPipeline => {
                    shader_expand_rejection_for_call(ws, body, e, &arg_idxs)
                }
                ExpandKind::ComputeArtifact => {
                    resolve_compute_entry(ws, body, e, &arg_idxs).err()
                }
                ExpandKind::VertexLayout => {
                    vertex_expand_rejection_for_call(ws, body, e, f, &arg_idxs)
                }
                ExpandKind::UniformBlock => {
                    uniform_expand_rejection_for_call(ws, body, e, f, &arg_idxs)
                }
                ExpandKind::UniformValues => {
                    uniform_values_rejection_for_call(ws, inf, body, e, f, &arg_idxs)
                }
                ExpandKind::SceneTypeKey => None,
                ExpandKind::BuildVar | ExpandKind::EmbedString => None,
                ExpandKind::Materialize => None,
            };
            rejections.record(kind, rejection);
        }
    }
    rejections.finish()
}

/// Every distinct `rock_shader` pair across the workspace, deduped by key and
/// sorted by key (deterministic registration order -> byte-stable re-emission).
/// The driver ([`skull::mir_emit`]) turns each into one top-level
/// `__rock_shader_register`. Any resolution failure surfaces as `Err` (the fatal
/// [`shader_expand_rejection`] gate runs first, so this is normally infallible).
pub fn collect_shader_pairs(ws: &Workspace) -> Result<Vec<ShaderPair>, CompilerError> {
    Ok(collect_shader_collection(ws)?.pairs)
}

pub fn body_has_shader_pipeline_call(
    ws: &Workspace,
    mid: ModuleId,
    owner: BodyOwner,
) -> bool {
    let module = ws.module(mid);
    let Some(body) = module.bodies.get(&owner) else { return false };
    let inf = ws.inference(mid).and_then(|ir| ir.per_fn.get(&owner));
    body.expr_arena.iter().any(|(e, expr)| {
        let Expr::Call { callee: Callee::Unresolved { name, .. }, .. } = expr else {
            return false;
        };
        let resolved = match inf {
            Some(inf) => resolve_call_fn(ws, inf, e, *name),
            None => match ws.functions_by_name.get(name).map(|l| l.as_slice()) {
                Some([single]) => Some(*single),
                _ => None,
            },
        };
        resolved.is_some_and(|(fmod, fid)| {
            matches!(
                crate::intrinsics::expand_kind(&ws.module(fmod).functions[fid]),
                Some(ExpandKind::ShaderPipeline)
            )
        })
    })
}

pub fn body_has_compute_artifact_call(
    ws: &Workspace,
    mid: ModuleId,
    owner: BodyOwner,
) -> bool {
    let module = ws.module(mid);
    let Some(body) = module.bodies.get(&owner) else { return false };
    let inf = ws.inference(mid).and_then(|ir| ir.per_fn.get(&owner));
    body.expr_arena.iter().any(|(e, expr)| {
        let Expr::Call { callee: Callee::Unresolved { name, .. }, .. } = expr else {
            return false;
        };
        let resolved = match inf {
            Some(inf) => resolve_call_fn(ws, inf, e, *name),
            None => match ws.functions_by_name.get(name).map(|list| list.as_slice()) {
                Some([single]) => Some(*single),
                _ => None,
            },
        };
        resolved.is_some_and(|(fmod, fid)| {
            matches!(
                crate::intrinsics::expand_kind(&ws.module(fmod).functions[fid]),
                Some(ExpandKind::ComputeArtifact)
            )
        })
    })
}

/// Collect the shader roots contributed by one body. Full construction calls
/// this for every body; incremental repair calls it only for re-inferred
/// bodies, so a body edit does not rediscover unrelated shader registrations.
pub fn collect_shader_body(
    ws: &Workspace,
    mid: ModuleId,
    owner: BodyOwner,
) -> Result<(Vec<ShaderPair>, bool), CompilerError> {
    let module = ws.module(mid);
    let Some(body) = module.bodies.get(&owner) else { return Ok((Vec::new(), false)) };
    let inf = ws.inference(mid).and_then(|ir| ir.per_fn.get(&owner));
    let mut by_key = BTreeMap::new();
    let mut callsite = false;
    for (e, expr) in body.expr_arena.iter() {
        let Expr::Call { callee: Callee::Unresolved { name, .. }, args, .. } = expr else {
            continue;
        };
        let resolved = match inf {
            Some(inf) => resolve_call_fn(ws, inf, e, *name),
            None => match ws.functions_by_name.get(name).map(|list| list.as_slice()) {
                Some([single]) => Some(*single),
                _ => None,
            },
        };
        let Some((fmod, fid)) = resolved else { continue };
        if !matches!(
            crate::intrinsics::expand_kind(&ws.module(fmod).functions[fid]),
            Some(ExpandKind::ShaderPipeline)
        ) {
            continue;
        }
        callsite = true;
        let arg_idxs: Vec<Idx<Expr>> = args.into_iter().collect();
        let pair = resolve_shader_pair(ws, body, e, &arg_idxs)?;
        by_key.entry(pair.key.clone()).or_insert(pair);
    }
    Ok((by_key.into_values().collect(), callsite))
}

/// Collect the compute roots contributed by one body. See
/// [`collect_shader_body`] for the locality contract.
pub fn collect_compute_body(
    ws: &Workspace,
    mid: ModuleId,
    owner: BodyOwner,
) -> Result<(Vec<ComputeEntry>, bool), CompilerError> {
    let module = ws.module(mid);
    let Some(body) = module.bodies.get(&owner) else { return Ok((Vec::new(), false)) };
    let inf = ws.inference(mid).and_then(|ir| ir.per_fn.get(&owner));
    let mut by_key = BTreeMap::new();
    let mut callsite = false;
    for (e, expr) in body.expr_arena.iter() {
        let Expr::Call { callee: Callee::Unresolved { name, .. }, args, .. } = expr else {
            continue;
        };
        let resolved = match inf {
            Some(inf) => resolve_call_fn(ws, inf, e, *name),
            None => match ws.functions_by_name.get(name).map(|list| list.as_slice()) {
                Some([single]) => Some(*single),
                _ => None,
            },
        };
        let Some((fmod, fid)) = resolved else { continue };
        if !matches!(
            crate::intrinsics::expand_kind(&ws.module(fmod).functions[fid]),
            Some(ExpandKind::ComputeArtifact)
        ) {
            continue;
        }
        callsite = true;
        let arg_idxs: Vec<Idx<Expr>> = args.into_iter().collect();
        let entry = resolve_compute_entry(ws, body, e, &arg_idxs)?;
        let key = ws.module(entry.module).functions[entry.func].name.to_string();
        by_key.entry(key.clone()).or_insert(ComputeEntry { key, entry });
    }
    Ok((by_key.into_values().collect(), callsite))
}

pub fn collect_shader_collection(ws: &Workspace) -> Result<ShaderCollection, CompilerError> {
    let mut by_key: BTreeMap<String, ShaderPair> = BTreeMap::new();
    let mut callsites: HashSet<(ModuleId, BodyOwner)> = HashSet::new();
    let mut contributions = HashMap::new();
    for (_, &mid) in ws.module_ids.iter() {
        let module = ws.module(mid);
        for (owner, _) in module.bodies.iter() {
            let key = (mid, *owner);
            let (pairs, callsite) = collect_shader_body(ws, mid, *owner)?;
            if callsite {
                callsites.insert(key);
            }
            for pair in &pairs {
                by_key.entry(pair.key.clone()).or_insert_with(|| pair.clone());
            }
            contributions.insert(key, pairs);
        }
    }
    Ok(ShaderCollection {
        pairs: by_key.into_values().collect(),
        callsites: callsites.into_iter().collect(),
        contributions,
    })
}

pub fn collect_compute_collection(ws: &Workspace) -> Result<ComputeCollection, CompilerError> {
    let mut by_key = BTreeMap::<String, ComputeEntry>::new();
    let mut callsites = HashSet::new();
    let mut contributions = HashMap::new();
    for (_, &mid) in ws.module_ids.iter() {
        let module = ws.module(mid);
        for (owner, _) in module.bodies.iter() {
            let key = (mid, *owner);
            let (entries, callsite) = collect_compute_body(ws, mid, *owner)?;
            if callsite {
                callsites.insert(key);
            }
            for entry in &entries {
                by_key.entry(entry.key.clone()).or_insert_with(|| entry.clone());
            }
            contributions.insert(key, entries);
        }
    }
    Ok(ComputeCollection {
        entries: by_key.into_values().collect(),
        callsites: callsites.into_iter().collect(),
        contributions,
    })
}

/// Reject CPU-side calls to `@gpu_only` intrinsics and references to GPU-side
/// functions as runtime fn values. Shader-pipeline expansion arguments are names,
/// not runtime values, and remain legal.
pub fn gpu_only_call_rejection(ws: &Workspace, mid: ModuleId) -> Option<CompilerError> {
    let module = ws.module(mid);
    let ir = ws.inference(mid)?;
    for (_, function) in module.functions.iter() {
        if function
            .param_decls
            .iter()
            .any(|param| is_workgroup_array_type(ws, param.ty))
        {
            return Some(CompilerError::new_with_label(
                function.name_span,
                "WorkgroupArray is workgroup memory and cannot cross a function boundary".to_string(),
            ));
        }
        if function.return_ty.is_some_and(|ty| is_workgroup_array_type(ws, ty)) {
            return Some(CompilerError::new_with_label(
                function.name_span,
                "WorkgroupArray is workgroup memory and cannot be returned".to_string(),
            ));
        }
    }
    for (_, global) in module.globals.iter() {
        if is_workgroup_array_type(ws, global.ty) {
            return Some(CompilerError::new_with_label(
                global.span,
                "WorkgroupArray is workgroup memory and cannot be stored globally".to_string(),
            ));
        }
    }
    for (owner, body) in module.bodies.iter() {
        let Some(inf) = ir.per_fn.get(owner) else { continue };
        let caller_is_gpu = match owner {
            BodyOwner::Function(fid) => {
                let function = &module.functions[*fid];
                crate::intrinsics::is_gpu_context(function) || function_has_gpu_resource_param(ws, function)
            }
            _ => false,
        };
        if caller_is_gpu {
            continue;
        }
        let mut shader_expansion_args = HashSet::new();
        for (e, expr) in body.expr_arena.iter() {
            let Expr::Call {
                callee: Callee::Unresolved { name, .. },
                args,
                ..
            } = expr
            else {
                continue;
            };
            let Some((fmod, fid)) = resolve_call_fn(ws, inf, e, *name) else {
                continue;
            };
            let f = &ws.module(fmod).functions[fid];
            if matches!(
                crate::intrinsics::expand_kind(f),
                Some(ExpandKind::ShaderPipeline | ExpandKind::ComputeArtifact)
            ) {
                shader_expansion_args.extend(args.clone().into_iter());
            }
        }
        for (e, expr) in body.expr_arena.iter() {
            match expr {
                Expr::Call {
                    callee: Callee::Unresolved { name, .. },
                    ..
                } => {
                    let Some((fmod, fid)) = resolve_call_fn(ws, inf, e, *name) else {
                        continue;
                    };
                    let f = &ws.module(fmod).functions[fid];
                    if !crate::intrinsics::is_gpu_only(f) && !function_has_gpu_resource_param(ws, f) {
                        continue;
                    }
                    let fname = f.name;
                    return Some(CompilerError::new_with_label(
                        body.expr_spans[e],
                        format!(
                            "`{fname}` is GPU-only and cannot be called from CPU (Lua) code. \
                             Shader helper code that calls `{fname}` must annotate its fn \
                             `@shader(...)` or `@gpu_only`."
                        ),
                    ));
                }
                Expr::Global(name) if !shader_expansion_args.contains(&e) => {
                    let Some((fmod, fid)) = resolve_call_fn(ws, inf, e, *name) else {
                        continue;
                    };
                    let f = &ws.module(fmod).functions[fid];
                    if !crate::intrinsics::is_gpu_context(f) && !function_has_gpu_resource_param(ws, f) {
                        continue;
                    }
                    let fname = f.name;
                    let kind =
                        if crate::intrinsics::is_gpu_only(f) { "GPU-only" } else { "a shader entry" };
                    return Some(CompilerError::new_with_label(
                        body.expr_spans[e],
                        format!(
                            "`{fname}` is {kind} and cannot be used as a fn value from CPU \
                             (Lua) code"
                        ),
                    ));
                }
                _ => {}
            }
        }
    }
    None
}

fn function_has_gpu_resource_param(ws: &Workspace, function: &Function) -> bool {
    function.param_decls.iter().any(|param| {
        let TyId::Static(module, index) = param.ty else { return false };
        let TypeExpr::App { ctor: TypeCtor::Nominal(name), .. } = ws.module(module).ty_arena[index] else {
            return false;
        };
        bone_semantic::prelude::is_intrinsic_key_in(
            name,
            &["StorageBuffer", "AtomicStorageBuffer", "WorkgroupArray"],
        )
    })
}

fn is_workgroup_array_type(ws: &Workspace, ty: TyId) -> bool {
    let TyId::Static(module, index) = ty else { return false };
    let TypeExpr::App { ctor: TypeCtor::Nominal(name), .. } = ws.module(module).ty_arena[index] else {
        return false;
    };
    bone_semantic::prelude::is_intrinsic_key(name, "WorkgroupArray")
}

fn const_int(body: &Body, e: Idx<Expr>) -> Option<i64> {
    match body.expr_arena[e] {
        Expr::Int(v) => Some(v),
        Expr::UnaryMinus(inner, _) => match body.expr_arena[inner] {
            Expr::Int(v) => Some(-v),
            _ => None,
        },
        _ => None,
    }
}

/// Scan module `mid`'s bodies for a `string_char(<literal>)` call whose literal
/// argument is outside ASCII 0..=127. `string_char` is `string.char` on luajit:
/// a code > 127 emits a lone high byte (invalid UTF-8 alone, a different code
/// unit on a UTF-16 backend); a negative code aborts `string.char` at runtime.
/// Portable only for 0..=127; for code points use `from_codepoint`. Dynamic
/// (non-literal) arguments are out of scope. Returns the offending arg's span.
pub fn string_char_literal_out_of_range(ws: &Workspace, mid: ModuleId) -> Option<Span> {
    let module = ws.module(mid);
    for (_owner, body) in module.bodies.iter() {
        for (_e, expr) in body.expr_arena.iter() {
            let Expr::Call { callee: Callee::Unresolved { name, .. }, args, .. } = *expr else {
                continue;
            };
            if name.as_str() != "string_char" {
                continue;
            }
            let Some(arg) = args.into_iter().next() else { continue };
            let Some(code) = const_int(body, arg) else { continue };
            if !(0..=127).contains(&code) {
                return Some(body.expr_spans[arg]);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests;

/// Support for `crate::spec_census`: expose concrete generic call identities
/// without giving the diagnostic census its own type conversion.
pub(crate) mod shape_census_support {
    use super::shape::{callee_instance_subst, spec_key_from_subst, SpecSubst};
    use crate::monoty::MonoTy;
    use crate::mir::FuncRef;
    use bone_semantic::inference::{BodyInference, Workspace};

    /// Every complete generic call identity in this body, plus the count of
    /// compiler-state sites that could not state one.
    pub(crate) fn concrete_calls_in_body(
        ws: &Workspace,
        inf: &BodyInference,
    ) -> (Vec<(FuncRef, Vec<MonoTy>)>, usize) {
        // caller is supplied by the census; this returns callee-side data only.
        let empty = SpecSubst::default();
        let mut out = Vec::new();
        let mut rejected = 0usize;
        let mut calls: Vec<_> = inf.resolved_overload_calls.iter().collect();
        calls.sort_by_key(|(e, _)| **e);
        for (e, (m, f)) in calls {
            let func = FuncRef { module: *m, func: *f };
            match callee_instance_subst(ws, inf, *e, func, &empty) {
                Ok(Some(subst)) => match spec_key_from_subst(ws, func, &subst) {
                    Some(key) => out.push((func, key)),
                    None => rejected += 1,
                },
                Ok(None) => {}
                Err(_) => rejected += 1,
            }
        }
        (out, rejected)
    }
}
