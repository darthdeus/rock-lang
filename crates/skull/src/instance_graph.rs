//! The SHADOW instance graph (`docs/monomorphization-plan.md` Stage 0d).
//!
//! Read-only with respect to emission: it observes what a monomorphizing
//! compiler WOULD instantiate, and changes nothing about what is emitted today.
//!
//! An [`Instance`] is `(FuncRef, Vec<MonoTyId>)` -- a function plus the total
//! logical types of its generic arguments, keyed on
//! [`MonoTy`](skull_mir::monoty::MonoTy) from Stage 0c rather than on `SpecKey`.
//! That is the whole point of the probe: `SpecKey` is partial (plan S 2a), so
//! `f<Vec2, unit>` and `f<Vec2, any>` share its identity while they get distinct
//! `InstanceId`s here.
//!
//! **Roots come from production.** [`super::mir_emit::is_emission_root`] is the
//! same predicate `reachable_fns` seeds its walk with; re-deriving it would make
//! any difference between this graph and the real one an artifact of the copy.
//!
//! **Transitive instantiation is the core.** Walking a callee's body under the
//! CALLER's substitution is what makes `count<I>` at `I = VecIter<Vec2>` a
//! different instance from the same fn at `VecIter<int>`, and it is what today's
//! conditional specializer only does when `any_value` happens to hold.
//!
//! **Two outcomes are not the same.** [`Unresolved`] is a construction the walk
//! could not complete -- data. [`Rejection`] is one the concrete-program model
//! REFUSES (plan S 0h), carrying a span and a message for the compile error
//! Stage 3's cutover attaches it to. Nothing here is fatal yet.
//!
//! **Where the graph is still incomplete**, stated so a reader does not mistake
//! the histogram for the whole graph: `CompilerGenerated` OVER-approximates, and
//! a coroutine's `__defer`/`__on_*` helpers exist only after the MIR transform so
//! nothing here can count them. Every edge recorded is real;
//! [`InstanceGraph::uninstantiated_roots`] records the generic roots no
//! instantiation reached. Full accounting: `docs/mono-stage0d-manifest.md`.

use std::collections::{BTreeMap, BTreeSet};

use bone_semantic::hir::{Body, BodyOwner, Function};
use bone_semantic::inference::{BodyInference, Workspace};
use bone_semantic::prelude::{BoundNext, IterWitness, ModuleId, Span};
use comfy_la_arena::Idx;
use rustc_hash::FxHashMap;
use skull_mir::mangle::mangle_fn;
use skull_mir::monoty::{MonoArena, MonoSubst, MonoTyId, MonoTyReason};
use skull_mir::mir::FuncRef;

/// A function at one concrete instantiation. Non-generic functions get an
/// instance too (empty `args`), so no consumer can reintroduce `if generic`.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Instance {
    pub func: FuncRef,
    pub args: Vec<MonoTyId>,
}

/// A `FuncRef` as an ORDERED map key.
///
/// `FuncRef` is not `Ord`, and ordering by it would be ordering by arena
/// position anyway. This is `(module, function index)` -- stable within a build
/// and used only to make iteration deterministic, the same contract
/// [`ord_key`] states for an [`Instance`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DeclKey(pub FuncRef);

impl DeclKey {
    fn raw(self) -> (u32, u32) {
        (self.0.module.raw(), self.0.func.into_raw().into_u32())
    }
}

impl Ord for DeclKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.raw().cmp(&other.raw())
    }
}

impl PartialOrd for DeclKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// `FuncRef` is not `Ord`, and ordering by it would be ordering by arena
/// position anyway. The key is `(module, function index, args)` -- stable within
/// a build, and only ever used to make iteration order deterministic; the
/// MANIFEST is sorted by rendered symbol, which is stable across builds.
fn ord_key(i: &Instance) -> (u32, u32, &[MonoTyId]) {
    (i.func.module.raw(), i.func.func.into_raw().into_u32(), &i.args)
}

impl Ord for Instance {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        ord_key(self).cmp(&ord_key(other))
    }
}

impl PartialOrd for Instance {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Why one instance depends on another. The plan's edge kinds; the histogram of
/// these is what makes an absent edge class visible.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum EdgeKind {
    /// A statically resolved direct call.
    DirectCall,
    /// A direct call whose callee is generic and whose instantiation came from
    /// the caller's substitution rather than from the call site alone. The
    /// transitive-instantiation edge.
    GenericCall,
    /// The `iter()` half of a desugared `for`.
    IteratorIter,
    /// The `next()` witness of a desugared `for`, resolved at instantiation
    /// (plan S 12).
    IteratorNext,
    /// A Rock function taken as a VALUE rather than called (`let f = adder`, a
    /// callback argument, a `func:` struct field).
    ///
    /// The discriminant is skull's OWN, not bone's: `lower/expr.rs:75` lowers a
    /// fn value from `Expr::Global(name)` where `ws.functions_by_name` has the
    /// name, and resolves the symbol with `fn_value_symbol`. bone records
    /// nothing in `resolved_overload_calls` for it unless the name is
    /// overloaded, so scanning that map finds none -- measured, and the reason
    /// `bbc06d767` withdrew the first attempt at this edge. `bone-lints`'
    /// `body_edges` reaches the same conclusion independently and states it:
    /// "never a call and never in the inference maps... the two nets are
    /// complementary".
    FnValue,
    /// An iterable argument coerced through its `iter()` at an
    /// `Iterator`-bounded param (`v.map(f)` with no explicit `.iter()`), from
    /// `implicit_iter_args`. The call-argument analog of [`Self::IteratorIter`].
    ImplicitIter,
    /// The `next` witness of a call to a BOUNDED generic fn, from the call
    /// site's conformance tree (`bound_call_trees`). Distinct from
    /// [`Self::IteratorNext`], which is the `for` desugar's witness.
    BoundNextWitness,
    /// A call no source expression contains: lowering synthesizes it and
    /// resolves the callee by LITERAL NAME through `Builder::stdlib_funcref` /
    /// `call_spine` (`docs/boundary-contracts.md` S 1.4). Over-approximated --
    /// see [`COMPILER_GENERATED_TRIGGERS`].
    CompilerGenerated,
}

impl EdgeKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::DirectCall => "direct-call",
            Self::GenericCall => "generic-call",
            Self::IteratorIter => "iterator-iter",
            Self::IteratorNext => "iterator-next",
            Self::FnValue => "fn-value",
            Self::ImplicitIter => "implicit-iter",
            Self::BoundNextWitness => "bound-next-witness",
            Self::CompilerGenerated => "compiler-generated",
        }
    }
}

/// The boundary classes of `docs/boundary-contracts.md` Part 1. A boundary is a
/// callee whose body skull does not lower to MIR; plan S 9 requires every call
/// to resolve to exactly one `InstanceId` OR one `BoundaryId`.
///
/// [`BoundaryClass::Invalid`] is not a body kind but a required OUTCOME -- the
/// class a boundary takes on a target that cannot supply its contract. The
/// shader-interface class (S 1.5) has no member here: GLSL is not a `FnBackend`
/// the instance walk runs for, so no call in this graph resolves to one.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
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

/// One reached boundary, classified for the graph's target.
///
/// `symbol` is the name emission must produce: bare for `extern`/`@nomangle`,
/// mangled otherwise -- the rule `skull_mir::mangle` applies, read from it
/// rather than restated.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Boundary {
    pub class: BoundaryClass,
    pub symbol: String,
    /// The target has no body serving this boundary, so plan S 10's
    /// `Invalid boundary` outcome is required: reject with a span, never a stub.
    pub invalid_on_target: bool,
}

/// A helper symbol a coroutine instance OWNS. Not an [`Edge`]: the target is
/// not another instance.
///
/// Stage 3's requirement is visible in `symbol_today`. `emit_coroutine` derives
/// every helper from `mangled(ws, self_ref)` (`coroutine_emit.rs:21`) -- the
/// SOURCE FuncRef -- so two instances of one generic `co fn` claim the same
/// helper names. The edge Stage 3 needs must therefore carry the OWNING
/// INSTANCE, so the base becomes [`InstanceGraph::symbol`] instead of the
/// source mangling.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct CoroutineHelper {
    pub owner_identity: String,
    pub suffix: &'static str,
    /// What emission produces TODAY: mangled source name + suffix.
    pub symbol_today: String,
}

#[derive(Clone, Debug)]
pub struct Edge {
    pub from: Instance,
    pub to: Instance,
    pub kind: EdgeKind,
    pub span: Span,
}

/// WHICH operation of a body a callee was resolved for.
///
/// An [`Idx<Expr>`] alone is not a site identity: one `for` header carries both
/// an `iter` and a `next` witness, one bounded call carries a whole conformance
/// tree, and one column-Map index can emit two runtime entries. Each of those is
/// a separate callee obligation at the same expression, so the operation is part
/// of the key.
///
/// This exists so `ConcreteBody` can read the callee the WALK resolved instead
/// of resolving one again. A second resolver could disagree with the graph, and
/// correlating the two by span could not even detect that -- several of these
/// share one span by construction.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum SiteOp {
    /// A call expression bone resolved (`resolved_overload_calls`).
    DirectCall,
    /// An `Expr::Global` naming a function, taken as a value.
    FnValue,
    /// The `for` head's `iter()` witness.
    ForIter,
    /// The `for` protocol's `next` witness, and the conformance path beneath it
    /// (`BoundNext::sub`). Empty path is the outermost witness.
    ForNext(Vec<usize>),
    /// An iterable argument coerced through `iter()` at an `Iterator`-bounded
    /// parameter.
    ImplicitIter,
    /// A witness from a bounded CALL's conformance tree, by path. Kept apart
    /// from [`Self::ForNext`] because a `for` over a bounded call has both, at
    /// the same expression and with the same paths.
    BoundNext(Vec<usize>),
    /// A call lowering synthesizes, named EXACTLY -- see
    /// [`COMPILER_GENERATED_TRIGGERS`].
    Synthesized(&'static str),
}

impl SiteOp {
    pub fn name(&self) -> &'static str {
        match self {
            Self::DirectCall => "direct-call",
            Self::FnValue => "fn-value",
            Self::ForIter => "for-iter",
            Self::ForNext(_) => "for-next",
            Self::ImplicitIter => "implicit-iter",
            Self::BoundNext(_) => "bound-next",
            Self::Synthesized(_) => "synthesized",
        }
    }
}

/// One callee obligation of one body: an expression plus the operation at it.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Site {
    pub expr: Idx<bone_semantic::hir::Expr>,
    pub op: SiteOp,
}

/// What the walk resolved a site to. `Failed` is kept in the SAME table as a
/// success rather than only in [`InstanceGraph::unresolved`]: a consumer asking
/// "what does this operation call" must be able to see that the walk tried and
/// could not, otherwise an absent key means both "no such operation" and "the
/// operation has no callee".
#[derive(Clone, Debug)]
pub enum SiteOutcome {
    Resolved(Instance),
    Failed(&'static str),
}

#[derive(Clone, Debug)]
pub struct SiteRecord {
    pub outcome: SiteOutcome,
    pub span: Span,
}

/// One bounded generic's concrete `next` witness at an instance.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct InstanceBoundNext {
    pub callee: Instance,
    pub sub: InstanceBoundTree,
}

/// The concrete witnesses supplied for an instance's bounded generics.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct InstanceBoundTree {
    pub next: BTreeMap<usize, InstanceBoundNext>,
}

#[derive(Clone, Debug)]
pub struct BoundTreeDisagreement {
    pub instance: Instance,
    pub recorded: InstanceBoundTree,
    pub incoming: InstanceBoundTree,
}

/// Which of the two bounded-witness roots a conformance tree hangs under.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BoundRoot {
    For,
    Call,
}

impl BoundRoot {
    fn site(self, expr: Idx<bone_semantic::hir::Expr>, path: Vec<usize>) -> Site {
        Site {
            expr,
            op: match self {
                Self::For => SiteOp::ForNext(path),
                Self::Call => SiteOp::BoundNext(path),
            },
        }
    }
}

/// A construction the concrete-program model REFUSES, as opposed to one it
/// merely could not complete yet ([`Unresolved`]).
///
/// Three variants reject author programs under
/// `docs/monomorphization-plan.md` S 0h. `ConcreteTypeUnavailable` records a
/// compiler invariant instead. They become fatal at Stage 3's emission cutover.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum RejectionKind {
    /// A function that recurs at a DIFFERENT instantiation than it was reached
    /// at. Demand-driven instantiation does not terminate on it (plan S 7).
    ChangingInstanceRecursion,
    /// A generic function taken as a VALUE. There is no call site to key an
    /// instantiation on, and inferring one from the instances reachable
    /// elsewhere is explicitly not the rule (plan S 0h).
    GenericFnValue,
    /// A COMPILER INVARIANT, not a language decision: a type that should have
    /// been concrete here is recovery poison, names no node, or resolves
    /// cyclically. Unlike the author-call variants, no valid program admits
    /// this -- the program should have been rejected upstream. It is a rejection rather than
    /// a silent negative because the alternative is picking a representation for
    /// a type the compiler could not resolve.
    ConcreteTypeUnavailable,
    /// A type argument no constraint reaches, so it has no concrete type to
    /// instantiate at. Distinct from a declared generic read inside a shared
    /// body, which is [`Unresolved`] data.
    UnconstrainedTypeVar,
}

impl RejectionKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::ChangingInstanceRecursion => "changing-instance-recursion",
            Self::GenericFnValue => "generic-fn-value",
            Self::UnconstrainedTypeVar => "unconstrained-type-var",
            Self::ConcreteTypeUnavailable => "concrete-type-unavailable",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Rejection {
    pub kind: RejectionKind,
    /// The instance whose construction the rejection happened in.
    pub at: Instance,
    pub span: Span,
    pub message: String,
}

/// A call the walk could not turn into an instance. Data, not an error: an
/// unbound generic here means the caller is itself an uninstantiated shared
/// body, which is exactly the surface Stage 2 must supply.
#[derive(Clone, Debug)]
pub struct Unresolved {
    pub from: Instance,
    pub callee: FuncRef,
    pub reason: &'static str,
    pub span: Span,
}

pub struct InstanceGraph {
    pub arena: MonoArena,
    pub instances: BTreeSet<Instance>,
    pub edges: Vec<Edge>,
    pub unresolved: Vec<Unresolved>,
    /// Constructions the model refuses. See [`RejectionKind`].
    pub rejections: Vec<Rejection>,
    /// Names with no concrete instance covering the obligation that put them
    /// here, BY CAUSE. See [`RootHole`]: one set carrying several different
    /// facts could only ever be read as the weakest of them.
    pub uninstantiated_roots: BTreeMap<String, RootHole>,
    /// Instances whose expansion was cut by [`MAX_INSTANTIATION_DEPTH`].
    pub depth_capped: Vec<Instance>,
    /// Instances whose Rock body the walk actually entered and enumerated the
    /// callees of. Recorded, never inferred from a proxy.
    pub expanded: BTreeSet<Instance>,
    /// The execution target this graph was built for. Instance IDENTITY is
    /// target-neutral (plan S 20a); the target decides which backend variant a
    /// call resolves to and which boundaries are available.
    pub target: bone_semantic::hir::FnBackend,
    /// Every reached boundary, classified per `docs/boundary-contracts.md`.
    pub boundaries: BTreeMap<String, Boundary>,
    /// Helper symbols owned by coroutine instances.
    pub coroutine_helpers: BTreeSet<CoroutineHelper>,
    /// Which of the 15 literal-name `stdlib_funcref` entries the walk actually
    /// reached (`docs/boundary-contracts.md` S 1.4). A name absent here is one
    /// the corpus never triggers, not one that does not exist.
    pub erased_entries: BTreeSet<String>,
    /// Runtime entries an opaque `inline(lua)` body calls: the owning
    /// DECLARATION -> the entries its text names. Emission must emit these even
    /// though no Rock call site reaches them.
    ///
    /// Keyed by `FuncRef`, not by mangled symbol: two `@nomangle` backend
    /// siblings share one symbol, so a symbol key merges a Lua body's
    /// dependencies with a JS body's and no artifact can tell which are its
    /// own. The ENTRY names stay names here -- resolving one to a declaration
    /// is a per-artifact decision (`crate::artifact`), because the serving
    /// declaration differs by target.
    pub inline_body_deps: BTreeMap<DeclKey, BTreeSet<&'static str>>,
    /// Every storage query that could not decide, by reason. Kept beside the
    /// rejection list because a rejection KIND aggregates several reasons, so
    /// the kind histogram cannot answer "how many storage queries failed, and
    /// why".
    pub storage_failures: Vec<MonoTyReason>,
    /// Storage-condition queries the walk ran, the denominator for the
    /// synthesized-call population. Without it a zero synthesized-edge count
    /// cannot be told apart from a walk that asked nothing.
    pub synthesized_examined: usize,
    /// Synthesized sites whose exact entry could NOT be decided, so more than
    /// one name was kept. The residual over-approximation, as a number rather
    /// than a paragraph.
    pub synthesized_approximated: usize,
    /// Each instance's substitution: callee `VarId` -> its concrete type. What
    /// a `ConcreteBody`'s type tables are built under (plan S 9). Kept because
    /// the walk computed it and then dropped it, leaving nothing able to state
    /// an instance's types concretely.
    pub substitutions: BTreeMap<Instance, MonoSubst>,
    /// Instances two callers reached with DIFFERENT substitutions. Non-empty
    /// means the substitution is not a function of the instance, so a
    /// `ConcreteBody` built from it would depend on discovery order.
    pub subst_disagreements: Vec<SubstDisagreement>,
    /// How many times the walk actually COMPARED an instance's substitution
    /// against a recorded one. The denominator for `subst_disagreements`.
    pub subst_comparisons: usize,
    /// The same, PER INSTANCE. A global count cannot say that any PARTICULAR
    /// instance was reached from two callers, which is what the agreement
    /// property is about.
    pub subst_compared: BTreeMap<Instance, usize>,
    /// Concrete conformance witnesses for each bounded instance. This replaces
    /// the splice-time `CallerGeneric` environment for consumers of the graph.
    pub bound_trees: BTreeMap<Instance, InstanceBoundTree>,
    /// One logical instance reached with two different witness trees. Empty is
    /// required: otherwise its body would depend on discovery order.
    pub bound_tree_disagreements: Vec<BoundTreeDisagreement>,
    /// Re-reaches whose witness trees were actually compared.
    pub bound_tree_comparisons: usize,
    /// Per instance, the callee EACH of its source operations resolved to.
    /// `ConcreteBody`'s callee tables are this record projected per table; the
    /// walk is the one resolver (plan S 9).
    ///
    /// Keyed only for instances -- an always-emitted body has no instance for a
    /// site to belong to, and its failures are already recorded per site in
    /// [`RootHole::always_emitted_sites`].
    pub call_sites: BTreeMap<Instance, BTreeMap<Site, SiteRecord>>,
    /// Sites recorded more than once for one instance. Must be empty: a body is
    /// walked exactly once per instance, so a repeat means two operations
    /// collapsed onto one key and one of them is invisible.
    pub duplicate_sites: Vec<(Instance, Site)>,
    /// Every site the walk ATTEMPTED, including the ones with no `from`
    /// instance to record against. The denominator for `call_sites`.
    pub sites_attempted: usize,
    /// Callees seeded from an ALWAYS-EMITTED body (a decorator definition or a
    /// global initializer) rather than from a function root. Production treats
    /// their call edges as reachability roots (`mir_emit::reachable_fns`'s
    /// `extra_roots`/`extra_root_names`).
    pub always_emitted_seeds: usize,
}

/// The storage condition a synthesized-call arm is gated on in
/// `crates/skull-mir/src/lower/storage.rs`. Every arm there opens with
/// `let Some(..) = self.<oracle>(recv) else { return Ok(None) }`, so the trigger
/// name alone never decides whether the call is emitted.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StorageCond {
    /// The receiver's `[E]` element is column-stored.
    VecColumn,
    /// The receiver's `Map<K, V>` value is column-stored.
    MapColumn,
    /// No storage condition: the debug-mode `NativeBuffer` bounds check fires on
    /// the receiver type alone.
    Always,
}

/// Calls lowering SYNTHESIZES, keyed by the source operation that triggers them:
/// `(trigger name, storage condition, the `stdlib_funcref`/`call_spine` names
/// the arm emits WHEN THAT CONDITION HOLDS)`. Read off
/// `crates/skull-mir/src/lower/storage.rs`; the complete 15-name set is
/// catalogued in `docs/boundary-contracts.md` S 1.4.
///
/// The condition uses the same
/// `skull_mir::lower::shape::{vec_column_elem_mono, map_column_value_mono}`
/// oracle as lowering. The names are the column-path ones only -- a non-column
/// receiver takes a different route entirely, so `table_len` (the boxed
/// `__vec_data_capacity` path) is not an edge this arm produces.
///
/// A trigger with more than one name here is one whose arm SELECTS between
/// them; the selection is made per site, not kept as a set. `__vec_data_sort`
/// splits on the element ([`sort_entry`]) and a Map index on the index's
/// position and stored value ([`map_index_entries`]), so the names below are the
/// alternatives a trigger can produce, never the ones a site does.
///
/// Calls made from inside an `inline(lua)` body are invisible here by design --
/// the body is opaque (`BoundaryClass::InlineBody`). `__rock_vec_column_elements`
/// is the corpus's example: emission calls it once per project, from the Lua
/// text of the reflection walker, not from any lowering arm.
pub const COMPILER_GENERATED_TRIGGERS: &[(&str, StorageCond, &[&str])] = &[
    ("__vec_data_capacity", StorageCond::VecColumn, &["__rock_col_capacity"]),
    ("__vec_data_insert", StorageCond::VecColumn, &["__rock_col_insert"]),
    ("__vec_data_clone", StorageCond::VecColumn, &["__rock_col_copy"]),
    ("__vec_data_sort", StorageCond::VecColumn, &["__rock_col_sort", "__rock_col_sort_leaves"]),
    ("__rock_vec_to_lua_array", StorageCond::VecColumn, &["__rock_vec_column_elements"]),
    ("table_unset", StorageCond::MapColumn, &["__map_col_free"]),
    ("len", StorageCond::MapColumn, &["__map_col_len"]),
    ("map_len", StorageCond::MapColumn, &["__map_col_len"]),
    ("contains", StorageCond::MapColumn, &["__map_col_contains"]),
    ("clear", StorageCond::MapColumn, &["__map_col_clear"]),
    ("get", StorageCond::MapColumn, &["__map_col_slot_of"]),
];

/// The synthesized entries reached from an INDEX rather than from a named call:
/// `m[k]` / `m[k] = v` on a column Map (`column_index_place`,
/// `map_column_store`, `map_column_index_repr`) and the debug-mode
/// `NativeBuffer` bounds check (`nb_checked_index`).
pub const INDEX_TRIGGERED: &[(&str, StorageCond, &[&str])] = &[
    ("Map", StorageCond::MapColumn, &["__map_col_slot", "__map_col_slot_checked", "__map_col_free"]),
    ("NativeBuffer", StorageCond::Always, &["__nb_checked_index"]),
];

/// Every runtime entry a synthesized call or an opaque body can name
/// (`docs/boundary-contracts.md` S 1.4). Kept as ONE list so the inline-body
/// scan and the synthesized-call table cannot drift about what the catalogue is.
pub const ERASED_RUNTIME_ENTRIES: &[&str] = &[
    "__map_col_clear",
    "__map_col_contains",
    "__map_col_free",
    "__map_col_len",
    "__map_col_slot",
    "__map_col_slot_checked",
    "__map_col_slot_of",
    "__nb_checked_index",
    "__rock_col_capacity",
    "__rock_col_copy",
    "__rock_col_insert",
    "__rock_col_sort",
    "__rock_col_sort_leaves",
    "__rock_vec_column_elements",
    "table_len",
];

/// Polymorphic recursion (`f<T> -> f<[T]>`) does not terminate under
/// demand-driven instantiation (plan S 7). The cap turns divergence into a
/// REPORT: a capped instance is a changing-instance recursion candidate.
///
/// It caps the STRUCTURAL DEPTH of the instantiation types, not the path length
/// from a root. A path length is a property of how the walk arrived, so capping
/// on it makes graph MEMBERSHIP depend on discovery order -- measured: with a
/// path cap, `games/little-knights` produced two different manifests under
/// forward and reversed root order, failing plan S 20g.4. Type depth is a
/// property of the instance itself, so the same instance is capped or not
/// regardless of how it was reached.
pub const MAX_INSTANTIATION_DEPTH: u32 = 12;

impl InstanceGraph {
    /// The READABLE symbol for an instance: the function's mangled name plus its
    /// rendered logical arguments. For display only -- two distinct instances can
    /// share one rendering ([`MonoTy::render`] is a pretty type syntax, not an
    /// identity), so deduplicating or matching on this would merge them. Key on
    /// [`Self::identity`].
    pub fn symbol(&self, ws: &Workspace, i: &Instance) -> String {
        let f = &ws.module(i.func.module).functions[i.func.func];
        let base = mangle_fn(ws, f);
        if i.args.is_empty() {
            return base;
        }
        let args: Vec<String> = i.args.iter().map(|a| self.arena.render(*a)).collect();
        format!("{base}<{}>", args.join(", "))
    }

    /// The instance's IDENTITY: the mangled name plus its arguments' canonical
    /// encodings. Unambiguous, so sorting and dedup on it cannot merge two
    /// distinct instances.
    pub fn identity(&self, ws: &Workspace, i: &Instance) -> String {
        let f = &ws.module(i.func.module).functions[i.func.func];
        let base = mangle_fn(ws, f);
        if i.args.is_empty() {
            return base;
        }
        let args: Vec<String> = i.args.iter().map(|a| self.arena.canonical(*a)).collect();
        format!("{base}<{}>", args.join(","))
    }

    /// The instance's LOGICAL symbol: `skull_mir::mangle::spec_fn_name`, the
    /// encoder today's specializer already spells a specialized body with.
    /// A non-generic instance keeps its base mangling; a generic one is suffixed
    /// with its arguments' canonical encodings, which is why this is injective
    /// where [`Self::symbol`] is not.
    ///
    /// **This is not the final emitted spelling.** Plan S 0h puts that under the
    /// ARTIFACT's ABI/boundary plan -- one symbol per `(artifact, instance)` --
    /// so a target may legally spell this instance differently. What is tested
    /// here is the ENCODER: distinct logical instances must not encode alike.
    /// Per-artifact injectivity is a property of each artifact view and is
    /// asserted there, not here.
    pub fn logical_symbol(&self, ws: &Workspace, i: &Instance) -> String {
        let f = &ws.module(i.func.module).functions[i.func.func];
        // Two cases, both explicit. A non-generic instance IS the base symbol --
        // not a fallback for a failed lookup. A generic one always has concrete
        // arguments, so `spec_fn_name` cannot decline (it returns `None` only for
        // an all-`None` key, which an instance can never build), and an
        // `unwrap_or_else` back to the base name would silently give a
        // specialization the shared body's symbol.
        if i.args.is_empty() {
            return mangle_fn(ws, f);
        }
        let key: Vec<Option<skull_mir::monoty::MonoTy>> =
            i.args.iter().map(|a| Some(self.arena.get(*a).clone())).collect();
        skull_mir::mangle::spec_fn_name(ws, f, &key)
            .expect("an instance with arguments always has a specialized symbol")
    }

    /// Plan S 2's Stage-2 gate, as data: what is NOT closed about this graph.
    ///
    /// Closure is not "every edge target is an instance" -- `record` inserts the
    /// target, so that holds by construction and would assert nothing. The
    /// question is whether every instance was EXPANDED: one whose body the walk
    /// never entered has callees missing from the graph, and it is closed only
    /// if it has no body to enter (a boundary) or is recorded as cut.
    pub fn closure(&self, ws: &Workspace) -> Closure {
        // `expanded` records bodies the walk entered. A boundary has no Rock
        // body to enter; every other unexpanded instance is a closure hole.
        let erased = &self.erased_entries;
        let unexpanded: BTreeSet<String> = self
            .instances
            .iter()
            .filter(|i| !self.expanded.contains(*i))
            .filter(|i| {
                boundary_class_for(&ws.module(i.func.module).functions[i.func.func], erased)
                    .is_none()
            })
            .map(|i| self.identity(ws, i))
            .collect();
        // Encoder injectivity: two DISTINCT instances must not encode to one
        // logical symbol. Keyed on identity, never on the display symbol, which
        // is ambiguous by construction.
        let mut by_symbol: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for i in &self.instances {
            by_symbol.entry(self.logical_symbol(ws, i)).or_default().insert(self.identity(ws, i));
        }
        let colliding: BTreeMap<String, BTreeSet<String>> =
            by_symbol.into_iter().filter(|(_, ids)| ids.len() > 1).collect();
        Closure {
            instances: self.instances.len(),
            unexpanded,
            depth_capped: self.depth_capped.iter().map(|i| self.identity(ws, i)).collect(),
            // Every hole, whatever its cause. A root WITH instances is still a
            // hole: the instances discharge its Rock call sites, never the
            // bare-name obligation that made it a root.
            uninstantiated_roots: self.uninstantiated_roots.keys().cloned().collect(),
            colliding_symbols: colliding,
            expanded: self.expanded.len(),
        }
    }

    /// Manifest lines, sorted and deduplicated by IDENTITY: `<identity>\t<symbol>`.
    ///
    /// The identity is IN the line, not merely the sort key. A caller that
    /// diffed, hashed or set-compared display symbols alone would collapse two
    /// instances whose arguments happen to render alike, which is exactly the
    /// property the identity encoding exists to prevent.
    pub fn manifest_lines(&self, ws: &Workspace) -> Vec<String> {
        let mut v: Vec<String> = self
            .instances
            .iter()
            .map(|i| format!("{}\t{}", self.identity(ws, i), self.symbol(ws, i)))
            .collect();
        v.sort();
        v.dedup();
        v
    }

    /// Reach `to` at `subst`: record the instance, keep its substitution, and
    /// worklist it the first time.
    ///
    /// The substitution must be a FUNCTION OF THE INSTANCE -- a specialized
    /// body's shape comes from declarations plus the instance substitution, so
    /// two callers reaching one instance must agree on it. That is not
    /// self-evident: the `VarId` keys come from `generic_var_of` reading the
    /// CALLER's `BodyInference`, so nothing structural forces agreement, and
    /// the worklist keeps only whichever arrived first. Disagreements are
    /// recorded rather than resolved by arrival order.
    fn reach(
        &mut self,
        work: &mut Vec<(Instance, MonoSubst, u32)>,
        to: Instance,
        subst: MonoSubst,
        depth: u32,
    ) {
        // `depth` is the depth to ENTER `to` at, so a root enters at 0 and a
        // callee at its caller's depth + 1. Roots come through here too: a
        // non-generic instance's substitution is EMPTY, not absent, and
        // seeding them directly is what left corpus instances with no
        // recorded substitution at all.
        match self.substitutions.get(&to) {
            Some(prev) => {
                // The COMPARISONS are the denominator, not the recorded
                // substitutions: an instance reached exactly once never runs
                // this arm, and counting recordings would report agreement the
                // walk never tested.
                self.subst_comparisons += 1;
                *self.subst_compared.entry(to.clone()).or_insert(0) += 1;
                if *prev != subst {
                    self.subst_disagreements.push(SubstDisagreement {
                        instance: to.clone(),
                        kept: prev.clone(),
                        discarded: subst.clone(),
                    });
                }
            }
            None => {
                self.substitutions.insert(to.clone(), subst.clone());
            }
        }
        if self.instances.insert(to.clone()) {
            work.push((to, subst, depth));
        }
    }

    /// Instances with no recorded substitution. Must be empty: a non-generic
    /// instance's substitution is EMPTY, not absent, and a `ConcreteBody`
    /// cannot be built for a body whose substitution the walk never kept.
    pub fn instances_without_substitution(&self, ws: &Workspace) -> BTreeSet<String> {
        self.instances
            .iter()
            .filter(|i| !self.substitutions.contains_key(*i))
            .map(|i| self.identity(ws, i))
            .collect()
    }

    /// Record what one source operation resolved to.
    ///
    /// Every recorder funnels through here, so a site cannot be recorded in one
    /// place and resolved in another. A repeat key is kept as a finding rather
    /// than overwritten: silently replacing it would make the second operation
    /// vanish from the tables while the counts still looked total.
    fn record_site(&mut self, from: Option<&Instance>, site: Site, outcome: SiteOutcome, span: Span) {
        self.sites_attempted += 1;
        let Some(from) = from else { return };
        let prev = self
            .call_sites
            .entry(from.clone())
            .or_default()
            .insert(site.clone(), SiteRecord { outcome, span });
        if prev.is_some() {
            self.duplicate_sites.push((from.clone(), site));
        }
    }

    fn record_bound_tree(&mut self, instance: &Instance, incoming: InstanceBoundTree) {
        match self.bound_trees.get(instance) {
            Some(recorded) if recorded != &incoming => {
                self.bound_tree_comparisons += 1;
                self.bound_tree_disagreements.push(BoundTreeDisagreement {
                    instance: instance.clone(),
                    recorded: recorded.clone(),
                    incoming,
                });
            }
            Some(_) => {
                self.bound_tree_comparisons += 1;
            }
            None => {
                self.bound_trees.insert(instance.clone(), incoming);
            }
        }
    }

    /// Whether no body serves this DECLARATION on the graph's target -- plan
    /// S 10's `Invalid boundary`.
    ///
    /// Asked of the declaration a site actually resolved to, never of
    /// [`Self::boundaries`], which is keyed by mangled SYMBOL: two `@nomangle`
    /// backend siblings share one key there, so one overwrites the other and
    /// the surviving entry answers for both. Measured: reading that map made
    /// every `__nb_checked_index` call on Lua an invalid boundary.
    pub fn boundary_invalid_on_target(&self, f: &Function) -> bool {
        !serves(f.backend, self.target)
    }

    /// A generic function production emits regardless of any call, that no call
    /// instantiates.
    fn record_generic_root(&mut self, name: String) {
        self.uninstantiated_roots.entry(name).or_default().generic_root = true;
    }

    /// A call from an always-emitted body whose instantiation failed. There is
    /// no `from` instance to file an [`Unresolved`] or a [`Rejection`] against,
    /// but the REASON and the SPAN are the same ones those carry and are kept.
    ///
    /// Per SITE, not per reason: two failures of one callee are two diagnostics
    /// at two places, and a set keyed on the reason reports them as one fact
    /// with no location.
    fn record_always_emitted_hole(
        &mut self,
        name: String,
        outcome: AlwaysEmittedOutcome,
        span: Span,
    ) {
        self.uninstantiated_roots
            .entry(name)
            .or_default()
            .always_emitted_sites
            .push(AlwaysEmittedFailure { outcome, span });
    }

    /// Which recorded facts could produce a located error and which could not
    /// (plan S 2: "retain each edge's source span, for good errors at unresolved
    /// boundaries").
    ///
    /// Reported with its denominator per kind. A spanless COUNT alone cannot be
    /// told apart from a kind the walk never produced, and the fallback that
    /// makes an edge spanless -- `expr_spans` missing the key, or no body at all
    /// -- is silent by construction.
    pub fn span_census(&self) -> SpanCensus {
        let mut c = SpanCensus::default();
        for e in &self.edges {
            let k = c.edges.entry(e.kind.name()).or_default();
            k.examined += 1;
            k.spanless += usize::from(is_spanless(&e.span));
        }
        for r in &self.rejections {
            let k = c.rejections.entry(r.kind.name()).or_default();
            k.examined += 1;
            k.spanless += usize::from(is_spanless(&r.span));
        }
        for u in &self.unresolved {
            let k = c.unresolved.entry(u.reason).or_default();
            k.examined += 1;
            k.spanless += usize::from(is_spanless(&u.span));
        }
        // An always-emitted failure is the same diagnostic as an `Unresolved`
        // or a `Rejection` with no `from` instance to hang it on, so it is
        // measured on the same axis and keyed by which of the two it is.
        // Leaving it out was how a whole population of facts stayed spanless
        // while the census reported full coverage.
        for hole in self.uninstantiated_roots.values() {
            for site in &hole.always_emitted_sites {
                let k = c.always_emitted.entry(site.outcome.label()).or_default();
                k.examined += 1;
                k.spanless += usize::from(is_spanless(&site.span));
            }
        }
        c
    }

    pub fn edge_histogram(&self) -> BTreeMap<&'static str, usize> {
        let mut h = BTreeMap::new();
        for e in &self.edges {
            *h.entry(e.kind.name()).or_insert(0) += 1;
        }
        h
    }

    /// Recursive strongly-connected components over the instance graph, split
    /// by whether the recursion PRESERVES its instance arguments.
    ///
    /// Same-instance recursion is ordinary and finite. Changing-instance
    /// recursion is the unbounded case the plan proposes to reject; it shows up
    /// here as a depth cap rather than as a cycle, because the walk never
    /// returns to the same instance.
    pub fn recursion_census(&self, ws: &Workspace) -> RecursionCensus {
        // Adjacency over instances.
        let mut succ: FxHashMap<&Instance, Vec<&Instance>> = FxHashMap::default();
        for e in &self.edges {
            succ.entry(&e.from).or_default().push(&e.to);
        }
        let mut direct = BTreeSet::new();
        let mut mutual = BTreeSet::new();

        // Direct: an instance that calls itself.
        for (from, tos) in &succ {
            if tos.iter().any(|t| *t == *from) {
                direct.insert(self.identity(ws, from));
            }
        }
        // Mutual: any cycle of length > 1, found by an iterative DFS with a
        // path stack. Bounded by the instance count, which is small.
        let mut color: FxHashMap<&Instance, u8> = FxHashMap::default();
        let mut path: Vec<&Instance> = Vec::new();
        let mut order: Vec<&Instance> = self.instances.iter().collect();
        order.sort();
        for start in order {
            if color.get(start).copied().unwrap_or(0) != 0 {
                continue;
            }
            dfs(start, &succ, &mut color, &mut path, &mut |cycle: &[&Instance]| {
                if cycle.len() > 1 {
                    for n in cycle {
                        mutual.insert(self.identity(ws, n));
                    }
                }
            });
        }
        // A symbol in both is reported as direct only -- the stronger fact.
        for d in &direct {
            mutual.remove(d);
        }
        RecursionCensus {
            direct_same_instance: direct,
            mutual_same_instance: mutual,
            changing_instance: self
                .rejections
                .iter()
                .filter(|r| r.kind == RejectionKind::ChangingInstanceRecursion)
                .map(|r| self.identity(ws, &r.at))
                .collect(),
        }
    }
}

fn dfs<'a>(
    node: &'a Instance,
    succ: &FxHashMap<&'a Instance, Vec<&'a Instance>>,
    color: &mut FxHashMap<&'a Instance, u8>,
    path: &mut Vec<&'a Instance>,
    on_cycle: &mut impl FnMut(&[&'a Instance]),
) {
    color.insert(node, 1);
    path.push(node);
    for &next in succ.get(node).map(|v| v.as_slice()).unwrap_or(&[]) {
        match color.get(next).copied().unwrap_or(0) {
            0 => dfs(next, succ, color, path, on_cycle),
            1 => {
                if let Some(at) = path.iter().position(|p| *p == next) {
                    on_cycle(&path[at..]);
                }
            }
            _ => {}
        }
    }
    path.pop();
    color.insert(node, 2);
}

/// What the instance graph does not yet close over. Every field is a NAMED set,
/// not a count: a gate that compares counts cannot tell a closed gap from a new
/// one that replaced it.
#[derive(Debug, Default)]
pub struct Closure {
    pub instances: usize,
    /// Instances whose body the walk actually entered.
    pub expanded: usize,
    /// Reached, has no body, and is not a boundary -- its callees are missing.
    pub unexpanded: BTreeSet<String>,
    /// Expansion cut by [`MAX_INSTANTIATION_DEPTH`].
    pub depth_capped: BTreeSet<String>,
    /// Names with an UNCOVERED BARE-NAME OBLIGATION, whatever their
    /// [`RootHole`] cause. Production emits these as shared generic bodies
    /// callable under a bare name, so a non-empty set is the shared-body
    /// surface, stated by name.
    ///
    /// NOT "roots no call instantiates": six of the corpus's have concrete
    /// instances from Rock call sites. Those instances discharge the call
    /// sites and not the bare-name entry, so the name stays here -- dropping
    /// the instanced ones is the falsely-green reading.
    pub uninstantiated_roots: BTreeSet<String>,
    /// LOGICAL symbol -> the distinct instance identities encoding to it. Any
    /// entry means the encoder is not injective. The FINAL emitted spelling is
    /// an artifact-ABI-plan property (plan S 0h), asserted per artifact view.
    ///
    /// **The corpus zero here is weak evidence and must not be read as strong.**
    /// Replacing the encoder with the AMBIGUOUS display rendering leaves the
    /// corpus at 0 too -- measured -- so the 26 projects simply contain no
    /// render-colliding instance pair. Only
    /// `instances_that_render_alike_get_distinct_logical_symbols` separates the
    /// two encoders.
    pub colliding_symbols: BTreeMap<String, BTreeSet<String>>,
}

impl Closure {
    /// Every hole, including a generic emission root whose bare-name
    /// obligation nothing discharges.
    pub fn is_closed(&self) -> bool {
        self.unexpanded.is_empty()
            && self.depth_capped.is_empty()
            && self.colliding_symbols.is_empty()
            && self.uninstantiated_roots.is_empty()
    }
}

/// What the same failure WOULD have been recorded as if the body it happened in
/// had an instance to file it against.
///
/// The distinction is the classification itself, not a label: an
/// [`Unresolved`] is data the walk could not complete, a [`Rejection`] is a
/// construction the model refuses (plan S 0h) and becomes a compile error at
/// the Stage-3 cutover. Storing only a reason string forced Stage 3 to
/// reconstruct which one it was by parsing that string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlwaysEmittedOutcome {
    Unresolved(&'static str),
    Rejection(RejectionKind),
}

impl AlwaysEmittedOutcome {
    /// Census key. Reason bucket for the unresolved case, kind name for the
    /// rejection case -- the two never collide because the kind names are the
    /// `RejectionKind` catalogue.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Unresolved(r) => r,
            Self::Rejection(k) => k.name(),
        }
    }
}

/// One call site in an always-emitted body whose instantiation failed. Carries
/// what a [`Rejection`] or an [`Unresolved`] at the same failure would: the
/// classification, and the span the Stage-3 diagnostic points at.
#[derive(Debug, Clone)]
pub struct AlwaysEmittedFailure {
    pub outcome: AlwaysEmittedOutcome,
    pub span: Span,
}

/// Why one name has no concrete instance covering its obligation.
///
/// The two causes are genuinely different obligations and were previously one
/// unlabelled set of names:
///
/// - a generic EMISSION ROOT is a body production emits regardless of any call
///   (`@nomangle`, `extern`, `@test`/`@sim`, a decorator root), so its
///   obligation is to exist under a BARE name that opaque Lua text can call.
///   Instances cannot discharge it; a boundary contract has to.
/// - a call from an ALWAYS-EMITTED body (a global initializer or a decorator
///   definition) that failed to instantiate is an ordinary unresolved call that
///   merely has no `from` instance to be filed against. Its reason is the same
///   `Fail::reason` an instantiated body's call would carry.
///
/// `instances` is the finding the note asked for: a root can be listed here and
/// still have concrete instances from OTHER call sites. That does not close the
/// hole -- the bare-name obligation survives -- but it separates "no concrete
/// body of this function exists at all" from "the Rock calls are covered and
/// only the opaque-caller entry is not".
#[derive(Debug, Default, Clone)]
pub struct RootHole {
    /// A generic function that is an emission root: production emits its body
    /// regardless of any call, so its obligation is to exist under a BARE name.
    /// Concrete instances do not discharge that, which is why this coexists
    /// with a non-zero [`Self::instances`].
    pub generic_root: bool,
    /// Every SITE at which a call from an always-emitted body could not
    /// instantiate it, with the span the diagnostic points at. Empty when
    /// [`Self::generic_root`] is the only cause.
    pub always_emitted_sites: Vec<AlwaysEmittedFailure>,
    /// Instances of this same function the walk created from other call sites.
    pub instances: usize,
}

/// One instance reached at two different substitutions.
#[derive(Debug, Clone)]
pub struct SubstDisagreement {
    pub instance: Instance,
    pub kept: MonoSubst,
    pub discarded: MonoSubst,
}

/// A spanless population and the population it was measured over.
#[derive(Debug, Default, Clone, Copy)]
pub struct SpanCoverage {
    pub examined: usize,
    pub spanless: usize,
}

/// Span coverage of everything the graph records that a Stage-3 diagnostic would
/// have to point at.
#[derive(Debug, Default)]
pub struct SpanCensus {
    pub edges: BTreeMap<&'static str, SpanCoverage>,
    pub rejections: BTreeMap<&'static str, SpanCoverage>,
    pub unresolved: BTreeMap<&'static str, SpanCoverage>,
    /// Failed calls from always-emitted bodies. Same diagnostic obligation as
    /// [`Self::unresolved`]; they differ only in having no `from` instance.
    pub always_emitted: BTreeMap<&'static str, SpanCoverage>,
}

impl SpanCensus {
    pub fn total_spanless(&self) -> usize {
        let sum = |m: &BTreeMap<&'static str, SpanCoverage>| {
            m.values().map(|c| c.spanless).sum::<usize>()
        };
        sum(&self.edges) + sum(&self.rejections) + sum(&self.unresolved)
            + sum(&self.always_emitted)
    }
}

/// A span the walk could not fill in. `Span::unknown` is the only way one is
/// produced here, and its file sentinel is what distinguishes it -- a real span
/// with a zero line range is line 1 of a real file.
pub fn is_spanless(span: &Span) -> bool {
    span.file.as_str() == "<unknown>"
}

#[derive(Debug, Default)]
pub struct RecursionCensus {
    pub direct_same_instance: BTreeSet<String>,
    pub mutual_same_instance: BTreeSet<String>,
    /// Instances a [`RejectionKind::ChangingInstanceRecursion`] names.
    pub changing_instance: BTreeSet<String>,
}

/// Build the shadow graph for `ws`.
///
/// `roots` is supplied by the caller so this shares production's root
/// discovery instead of re-deriving it. `perturb` reverses the seeding order;
/// the manifest must be identical either way (plan S 20g.4).
pub fn build(ws: &Workspace, roots: Vec<FuncRef>, perturb: bool) -> InstanceGraph {
    build_for_target(ws, roots, perturb, bone_semantic::hir::FnBackend::Lua)
}

/// [`build`], for an explicit execution target.
///
/// The target changes which backend VARIANT each call resolves to
/// (`skull_mir::lower::backend_variant`, production's own selector) and which
/// boundaries are available. It must NOT change logical instance identity --
/// that is plan S 20a's separation, and
/// `logical_identity_is_stable_across_targets` is the assertion.
pub fn build_for_target(
    ws: &Workspace,
    mut roots: Vec<FuncRef>,
    perturb: bool,
    target: bone_semantic::hir::FnBackend,
) -> InstanceGraph {
    roots.sort_by_key(|r| (r.module.raw(), r.func.into_raw().into_u32()));
    roots.dedup();
    if perturb {
        roots.reverse();
    }

    let mut g = InstanceGraph {
        arena: MonoArena::new(),
        instances: BTreeSet::new(),
        edges: Vec::new(),
        unresolved: Vec::new(),
        rejections: Vec::new(),
        uninstantiated_roots: BTreeMap::new(),
        depth_capped: Vec::new(),
        expanded: BTreeSet::new(),
        target,
        boundaries: BTreeMap::new(),
        coroutine_helpers: BTreeSet::new(),
        erased_entries: BTreeSet::new(),
        inline_body_deps: BTreeMap::new(),
        storage_failures: Vec::new(),
        synthesized_examined: 0,
        synthesized_approximated: 0,
        always_emitted_seeds: 0,
        call_sites: BTreeMap::new(),
        duplicate_sites: Vec::new(),
        sites_attempted: 0,
        substitutions: BTreeMap::new(),
        subst_disagreements: Vec::new(),
        subst_comparisons: 0,
        subst_compared: BTreeMap::new(),
        bound_trees: BTreeMap::new(),
        bound_tree_disagreements: Vec::new(),
        bound_tree_comparisons: 0,
    };

    // A root that is generic has no instantiation of its own; it is a shared
    // body until a call supplies one. Recorded rather than invented.
    let mut work: Vec<(Instance, MonoSubst, u32)> = Vec::new();
    for r in roots {
        let f = &ws.module(r.module).functions[r.func];
        if !f.generics.is_empty() {
            g.record_generic_root(mangle_fn(ws, f));
            continue;
        }
        let inst = Instance { func: r, args: Vec::new() };
        g.reach(&mut work, inst, MonoSubst::default(), 0);
    }

    // Globals and decorators have no `Instance` origin, so their body edges
    // seed the same worklist as function roots.
    for module in ws.modules.iter().filter(|m| m.id != ModuleId::builtin()) {
        let Some(inference) = ws.inference(module.id) else { continue };
        let mut owners: Vec<BodyOwner> = module
            .bodies
            .keys()
            .filter(|o| matches!(o, BodyOwner::Decorator(_) | BodyOwner::Global(_)))
            .copied()
            .collect();
        owners.sort_by_key(|o| format!("{o:?}"));
        for owner in owners {
            let Some(inf) = inference.per_fn.get(&owner) else { continue };
            let body = module.bodies.get(&owner);
            walk_body_edges(
                &mut g, &mut work, ws, inf, body, None, &MonoSubst::default(), 0, target,
            );
        }
    }

    while let Some((inst, subst, depth)) = work.pop() {
        let ty_depth =
            inst.args.iter().map(|a| g.arena.depth(*a)).max().unwrap_or(0);
        if ty_depth > MAX_INSTANTIATION_DEPTH {
            g.depth_capped.push(inst);
            continue;
        }
        let FunctionBody::Ready { inf, body } = function_body(ws, inst.func) else {
            continue;
        };
        let module = ws.module(inst.func.module);
        // A boundary's Rock body is not the one emission uses -- it emits the
        // foreign text -- so the walk STOPS here. Marking it unexpanded while
        // continuing to enumerate its calls was the worst of both: the closure
        // gate excused the instance as having no callees while the graph carried
        // them, and a `@native_required` fallback body reached callees emission
        // never routes through.
        let f = &module.functions[inst.func.func];
        if declared_boundary(f).is_some() {
            continue;
        }
        g.expanded.insert(inst.clone());

        walk_body_edges(
            &mut g, &mut work, ws, inf, Some(body), Some(&inst), &subst, depth, target,
        );
    }

    // Which holes nevertheless have concrete instances from other call sites.
    // A post-pass because a root is recorded before the worklist runs, so at
    // insertion time no instance of it exists yet -- which is how a name a
    // later call fully instantiated went on reading as having no concrete body
    // at all.
    if !g.uninstantiated_roots.is_empty() {
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for i in &g.instances {
            let f = &ws.module(i.func.module).functions[i.func.func];
            *counts.entry(mangle_fn(ws, f)).or_default() += 1;
        }
        for (name, hole) in g.uninstantiated_roots.iter_mut() {
            hole.instances = counts.get(name).copied().unwrap_or(0);
        }
    }

    record_inline_body_deps(&mut g, ws);
    detect_changing_instance_recursion(&mut g, ws);
    classify_boundaries(&mut g, ws);
    record_coroutine_helpers(&mut g, ws);
    g
}

#[allow(clippy::too_many_arguments)]
fn walk_body_edges(
    g: &mut InstanceGraph,
    work: &mut Vec<(Instance, MonoSubst, u32)>,
    ws: &Workspace,
    inf: &BodyInference,
    body: Option<&bone_semantic::hir::Body>,
    from: Option<&Instance>,
    subst: &MonoSubst,
    depth: u32,
    target: bone_semantic::hir::FnBackend,
) {
    let mut resolved_calls: FxHashMap<Idx<bone_semantic::hir::Expr>, Instance> =
        FxHashMap::default();
    let mut calls: Vec<(Idx<bone_semantic::hir::Expr>, (ModuleId, Idx<Function>))> =
        inf.resolved_overload_calls.iter().map(|(e, r)| (*e, *r)).collect();
    calls.sort_by_key(|(e, _)| *e);
    for (e, (m, f)) in calls {
        let callee = variant_for(ws, FuncRef { module: m, func: f }, target);
        let bindings = inf.call_generic_bindings.get(&e);
        let span = body.and_then(|b| b.expr_spans.get(e).copied()).unwrap_or_else(Span::unknown);
        let kind = if bindings.is_some() { EdgeKind::GenericCall } else { EdgeKind::DirectCall };
        if let Some(to) = record_call(
            g, work, ws, inf, from, subst, depth, callee, bindings, kind, span,
            Site { expr: e, op: SiteOp::DirectCall },
        ) {
            resolved_calls.insert(e, to);
        }
    }

    if let Some(body) = body {
        for (e, callee) in fn_value_sites(ws, inf, body, target) {
            let span = body.expr_spans.get(e).copied().unwrap_or_else(Span::unknown);
            let cf = &ws.module(callee.module).functions[callee.func];
            if cf.generics.is_empty() {
                record_call(
                    g, work, ws, inf, from, subst, depth, callee, None, EdgeKind::FnValue, span,
                    Site { expr: e, op: SiteOp::FnValue },
                );
            } else if let Some(from) = from {
                g.record_site(
                    Some(from),
                    Site { expr: e, op: SiteOp::FnValue },
                    SiteOutcome::Failed("generic-fn-value"),
                    span,
                );
                g.rejections.push(Rejection {
                    kind: RejectionKind::GenericFnValue,
                    at: from.clone(),
                    span,
                    message: format!(
                        "`{}` is generic and is taken as a value rather than called, so no \
                         call site decides its instantiation",
                        mangle_fn(ws, cf)
                    ),
                });
            } else {
                g.always_emitted_seeds += 1;
                g.record_always_emitted_hole(
                    mangle_fn(ws, cf),
                    AlwaysEmittedOutcome::Rejection(RejectionKind::GenericFnValue),
                    span,
                );
            }
        }

        let mut failures: Vec<(Idx<bone_semantic::hir::Expr>, MonoTyReason)> = Vec::new();
        let sites = compiler_generated_sites(
            ws, inf, body, subst, target, &mut g.synthesized_examined, &mut failures,
            &mut g.synthesized_approximated,
        );
        for (e, reason) in failures {
            g.storage_failures.push(reason.clone());
            let Some(from) = from else { continue };
            let span = body.expr_spans.get(e).copied().unwrap_or_else(Span::unknown);
            let kind = match reason {
                MonoTyReason::UnresolvedInfer(_) | MonoTyReason::UnboundGeneric(_) => {
                    RejectionKind::UnconstrainedTypeVar
                }
                _ => RejectionKind::ConcreteTypeUnavailable,
            };
            g.rejections.push(Rejection {
                kind,
                at: from.clone(),
                span,
                message: format!(
                    "a synthesized-call receiver has no concrete type in an instantiated body \
                     ({}), so its storage cannot be decided",
                    reason.bucket()
                ),
            });
        }
        for GeneratedSite { expr: e, name } in sites {
            let span = body.expr_spans.get(e).copied().unwrap_or_else(Span::unknown);
            let site = Site { expr: e, op: SiteOp::Synthesized(name) };
            match stdlib_entry(ws, name, target) {
                Some(callee) => {
                    record_call(
                        g, work, ws, inf, from, subst, depth, callee, None,
                        EdgeKind::CompilerGenerated, span, site,
                    );
                    g.erased_entries.insert(name.to_string());
                }
                None => {
                    g.record_site(
                        from,
                        site,
                        SiteOutcome::Failed("compiler-generated-entry-missing-for-target"),
                        span,
                    );
                    if let Some(from) = from {
                        g.unresolved.push(Unresolved {
                            from: from.clone(),
                            callee: from.func,
                            reason: "compiler-generated-entry-missing-for-target",
                            span,
                        });
                    }
                }
            }
        }
    }

    let mut implicit: Vec<(Idx<bone_semantic::hir::Expr>, &IterWitness)> =
        inf.implicit_iter_args.iter().map(|(e, w)| (*e, w)).collect();
    implicit.sort_by_key(|(e, _)| *e);
    for (e, witness) in implicit {
        let span = body.and_then(|b| b.expr_spans.get(e).copied()).unwrap_or_else(Span::unknown);
        record_witness(
            g, work, ws, inf, from, subst, depth, witness, span, EdgeKind::ImplicitIter, target,
            Site { expr: e, op: SiteOp::ImplicitIter },
        );
    }

    let mut trees: Vec<(Idx<bone_semantic::hir::Expr>, &bone_semantic::prelude::BoundTree)> =
        inf.bound_call_trees.iter().map(|(e, tree)| (*e, tree)).collect();
    trees.sort_by_key(|(e, _)| *e);
    for (e, tree) in trees {
        let span = body.and_then(|b| b.expr_spans.get(e).copied()).unwrap_or_else(Span::unknown);
        let concrete = walk_bound_tree(
            g, work, ws, inf, from, subst, depth, tree, span, target, e, BoundRoot::Call, &[],
        );
        if let Some(to) = resolved_calls.get(&e) {
            g.record_bound_tree(to, concrete);
        }
    }

    let mut bound_calls: Vec<(Idx<bone_semantic::hir::Expr>, usize)> =
        inf.bound_calls.iter().map(|(e, idx)| (*e, *idx)).collect();
    bound_calls.sort_by_key(|(e, _)| *e);
    for (e, gidx) in bound_calls {
        let span = body.and_then(|b| b.expr_spans.get(e).copied()).unwrap_or_else(Span::unknown);
        let site = Site { expr: e, op: SiteOp::DirectCall };
        let resolved = from
            .and_then(|i| g.bound_trees.get(i))
            .and_then(|tree| tree.next.get(&gidx))
            .cloned();
        let Some(resolved) = resolved else {
            g.record_site(from, site, SiteOutcome::Failed("caller-generic-witness-unbound"), span);
            continue;
        };
        if let Some(from) = from {
            g.edges.push(Edge {
                from: from.clone(),
                to: resolved.callee.clone(),
                kind: EdgeKind::BoundNextWitness,
                span,
            });
        }
        g.record_site(from, site, SiteOutcome::Resolved(resolved.callee), span);
    }

    let mut iters: Vec<(Idx<bone_semantic::hir::Expr>, &bone_semantic::prelude::ForIterInfo)> =
        inf.for_iter_protocol.iter().map(|(e, info)| (*e, info)).collect();
    iters.sort_by_key(|(e, _)| *e);
    for (e, info) in iters {
        let span = body.and_then(|b| b.expr_spans.get(e).copied()).unwrap_or_else(Span::unknown);
        if let Some(witness) = &info.iter {
            record_witness(
                g, work, ws, inf, from, subst, depth, witness, span,
                EdgeKind::IteratorIter, target, Site { expr: e, op: SiteOp::ForIter },
            );
        }
        let _ = record_bound_next(
            g, work, ws, inf, from, subst, depth, &info.next, span,
            EdgeKind::IteratorNext, target, e, BoundRoot::For, &[],
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn record_call(
    g: &mut InstanceGraph,
    work: &mut Vec<(Instance, MonoSubst, u32)>,
    ws: &Workspace,
    inf: &BodyInference,
    from: Option<&Instance>,
    subst: &MonoSubst,
    depth: u32,
    callee: FuncRef,
    bindings: Option<&Vec<bone_semantic::prelude::TyId>>,
    kind: EdgeKind,
    span: Span,
    site: Site,
) -> Option<Instance> {
    if from.is_none() {
        g.always_emitted_seeds += 1;
    }
    let cf = &ws.module(callee.module).functions[callee.func];
    let (args, callee_subst) = match instantiate(&mut g.arena, ws, inf, cf, bindings, subst) {
        Ok(v) => v,
        // An always-emitted body has no `from` to file against, but the
        // CLASSIFICATION is the same one an instantiated body's call gets:
        // `Unconstrained` is the decided `UnconstrainedTypeVar` rejection
        // either way. Ordered before the classifying arms so these guards win.

        Err(Fail::Unconstrained(_)) if from.is_none() => {
            g.record_always_emitted_hole(
                mangle_fn(ws, cf),
                AlwaysEmittedOutcome::Rejection(RejectionKind::UnconstrainedTypeVar),
                span,
            );
            g.record_site(from, site, SiteOutcome::Failed("unconstrained-type-var"), span);
            return None;
        }
        Err(fail) if from.is_none() => {
            g.record_always_emitted_hole(
                mangle_fn(ws, cf),
                AlwaysEmittedOutcome::Unresolved(fail.reason()),
                span,
            );
            g.record_site(from, site, SiteOutcome::Failed(fail.reason()), span);
            return None;
        }
        Err(Fail::Unconstrained(_)) => {
            let name = mangle_fn(ws, cf);
            g.rejections.push(Rejection {
                kind: RejectionKind::UnconstrainedTypeVar,
                at: from.expect("guarded above").clone(),
                span,
                message: format!(
                    "`{name}` is called with a type argument no constraint reaches, so there is \
                     no concrete type to instantiate it at"
                ),
            });
            g.record_site(from, site, SiteOutcome::Failed("unconstrained-type-var"), span);
            return None;
        }
        Err(fail) => {
            g.unresolved.push(Unresolved {
                from: from.expect("guarded above").clone(),
                callee,
                reason: fail.reason(),
                span,
            });
            g.record_site(from, site, SiteOutcome::Failed(fail.reason()), span);
            return None;
        }
    };
    let to = Instance { func: callee, args };
    if let Some(from) = from {
        g.edges.push(Edge { from: from.clone(), to: to.clone(), kind, span });
    }
    g.record_site(from, site, SiteOutcome::Resolved(to.clone()), span);
    g.reach(work, to.clone(), callee_subst, depth + 1);
    Some(to)
}

/// Plan S 7's rejection: a recursive edge must preserve its instance arguments.
///
/// Polymorphic recursion does NOT appear as a cycle over instances: every
/// instance on the chain is distinct, which is exactly why it diverges. It
/// appears as a PATH from one instance of a function to a DIFFERENT instance of
/// that same function. That reachability test is the whole discriminant, and it
/// is what keeps two independent instantiations of one recursive function legal:
/// `hold<int>` and `hold<string>` never reach each other.
///
/// Reachability is searched over the WHOLE instance graph rather than over
/// recursive SCCs first. Restricting to an SCC is sound -- every function on an
/// `f`-to-`f` path lies on an `f`-to-`f` cycle, so no path crosses out of one --
/// but it buys nothing: the graph is leaf-heavy, so the full search costs 110k
/// traversal steps on the corpus's largest project (mag: 2,560 starts, 26,883
/// edges), and the whole 26-project census runs in 1.5s. Measured, after
/// writing the SCC pass and finding it inert.
fn detect_changing_instance_recursion(g: &mut InstanceGraph, ws: &Workspace) {
    // Instance-level adjacency, carrying each edge's span so a rejection can
    // name the call that changed the arguments.
    let mut succ: FxHashMap<&Instance, Vec<(&Instance, Span)>> = FxHashMap::default();
    for e in &g.edges {
        succ.entry(&e.from).or_default().push((&e.to, e.span));
    }

    let mut order: Vec<&Instance> = succ.keys().copied().collect();
    order.sort();
    let mut found: Vec<(Instance, Span, String)> = Vec::new();
    let mut already: BTreeSet<(u32, u32)> = BTreeSet::new();
    for start in order {
        let key = (start.func.module.raw(), start.func.func.into_raw().into_u32());
        if already.contains(&key) {
            continue;
        }
        let mut seen: BTreeSet<&Instance> = BTreeSet::new();
        let mut queue: Vec<(&Instance, Span)> = vec![(start, Span::unknown())];
        while let Some((node, via)) = queue.pop() {
            if node.func == start.func && node.args != start.args {
                already.insert(key);
                found.push((
                    start.clone(),
                    via,
                    format!(
                        "polymorphic recursion: `{}` recurs as `{}`; instantiation does not \
                         terminate on a recursive call whose type arguments change",
                        g.symbol(ws, start),
                        g.symbol(ws, node)
                    ),
                ));
                break;
            }
            for (next, span) in succ.get(node).map(|v| v.as_slice()).unwrap_or(&[]) {
                if seen.insert(next) {
                    queue.push((next, *span));
                }
            }
        }
    }
    for (at, span, message) in found {
        g.rejections.push(Rejection {
            kind: RejectionKind::ChangingInstanceRecursion,
            at,
            span,
            message,
        });
    }
}

/// A synthesized `iter` witness: the `for` head's or a coerced argument's.
///
/// It has no call expr, so it can never appear in `call_generic_bindings`, and
/// its instantiation comes from the substitution bone retained while resolving
/// it. That record has the same shape as `next_generics`, so the same
/// instantiation route serves both -- one oracle for what a synthesized call
/// instantiates at, rather than a second one per witness kind.
#[allow(clippy::too_many_arguments)]
fn record_witness(
    g: &mut InstanceGraph,
    work: &mut Vec<(Instance, MonoSubst, u32)>,
    ws: &Workspace,
    inf: &BodyInference,
    from: Option<&Instance>,
    subst: &MonoSubst,
    depth: u32,
    witness: &IterWitness,
    span: Span,
    kind: EdgeKind,
    target: bone_semantic::hir::FnBackend,
    site: Site,
) {
    if from.is_none() {
        g.always_emitted_seeds += 1;
    }
    let rep = FuncRef { module: witness.func.0, func: witness.func.1 };
    let resolved = &ws.module(rep.module).functions[rep.func];
    let callee = variant_for(ws, rep, target);
    let cf = &ws.module(callee.module).functions[callee.func];
    match instantiate_witness(&mut g.arena, ws, inf, resolved, cf, &witness.generics, subst) {
        Ok((args, callee_subst)) => {
            let to = Instance { func: callee, args };
            if let Some(from) = from {
                g.edges.push(Edge { from: from.clone(), to: to.clone(), kind, span });
            }
            g.record_site(from, site, SiteOutcome::Resolved(to.clone()), span);
            g.reach(work, to, callee_subst, depth + 1);
        }
        // See `record_call`: the classification survives the absence of a
        // `from` instance.
        Err(Fail::Unconstrained(_)) if from.is_none() => {
            g.record_always_emitted_hole(
                mangle_fn(ws, cf),
                AlwaysEmittedOutcome::Rejection(RejectionKind::UnconstrainedTypeVar),
                span,
            );
            g.record_site(from, site, SiteOutcome::Failed("unconstrained-type-var"), span);
        }
        Err(fail) if from.is_none() => {
            g.record_always_emitted_hole(
                mangle_fn(ws, cf),
                AlwaysEmittedOutcome::Unresolved(fail.reason()),
                span,
            );
            g.record_site(from, site, SiteOutcome::Failed(fail.reason()), span);
        }
        Err(Fail::Unconstrained(_)) => {
            g.rejections.push(Rejection {
                kind: RejectionKind::UnconstrainedTypeVar,
                at: from.expect("guarded above").clone(),
                span,
                message: format!(
                    "the `iter` witness `{}` binds a type argument no constraint reaches",
                    mangle_fn(ws, cf)
                ),
            });
            g.record_site(from, site, SiteOutcome::Failed("unconstrained-type-var"), span);
        }
        Err(fail) => {
            g.unresolved.push(Unresolved {
                from: from.expect("guarded above").clone(),
                callee,
                reason: fail.reason(),
                span,
            });
            g.record_site(from, site, SiteOutcome::Failed(fail.reason()), span);
        }
    }
}

/// One `next` witness plus, recursively, the witnesses of ITS bounded generics.
///
/// A chained iterator resolves `next(MapIter<VecIter<..>>)` to MapIter's
/// `next`; only the outermost witness is named in `info.next`, so `sub` carries
/// the inner witnesses.
#[allow(clippy::too_many_arguments)]
fn record_bound_next(
    g: &mut InstanceGraph,
    work: &mut Vec<(Instance, MonoSubst, u32)>,
    ws: &Workspace,
    inf: &BodyInference,
    from: Option<&Instance>,
    subst: &MonoSubst,
    depth: u32,
    next: &BoundNext,
    span: Span,
    kind: EdgeKind,
    target: bone_semantic::hir::FnBackend,
    expr: Idx<bone_semantic::hir::Expr>,
    root: BoundRoot,
    path: &[usize],
) -> Option<InstanceBoundNext> {
    let site = root.site(expr, path.to_vec());
    if let BoundNext::CallerGeneric(gidx) = next {
        let resolved = from
            .and_then(|i| g.bound_trees.get(i))
            .and_then(|tree| tree.next.get(gidx))
            .cloned();
        let Some(resolved) = resolved else {
            g.record_site(from, site, SiteOutcome::Failed("caller-generic-witness-unbound"), span);
            return None;
        };
        if let Some(from) = from {
            g.edges.push(Edge {
                from: from.clone(),
                to: resolved.callee.clone(),
                kind,
                span,
            });
        }
        g.record_site(from, site, SiteOutcome::Resolved(resolved.callee.clone()), span);
        return Some(resolved);
    }

    let BoundNext::Concrete { func: (m, f), next_generics, sub } = next else {
        unreachable!("CallerGeneric handled above")
    };
    let rep = FuncRef { module: *m, func: *f };
    let resolved = &ws.module(rep.module).functions[rep.func];
    let callee = variant_for(ws, rep, target);
    let cf = &ws.module(callee.module).functions[callee.func];
    if from.is_none() {
        g.always_emitted_seeds += 1;
    }
    match instantiate_witness(&mut g.arena, ws, inf, resolved, cf, next_generics, subst) {
        Ok((args, callee_subst)) => {
            let to = Instance { func: callee, args };
            if let Some(from) = from {
                g.edges.push(Edge { from: from.clone(), to: to.clone(), kind, span });
            }
            g.record_site(from, site, SiteOutcome::Resolved(to.clone()), span);
            g.reach(work, to.clone(), callee_subst, depth + 1);
            let concrete_sub = walk_bound_tree(
                g, work, ws, inf, from, subst, depth, sub, span, target, expr, root, path,
            );
            g.record_bound_tree(&to, concrete_sub.clone());
            Some(InstanceBoundNext { callee: to, sub: concrete_sub })
        }
        // See `record_call`: the classification survives the absence of a
        // `from` instance.
        Err(Fail::Unconstrained(_)) if from.is_none() => {
            g.record_always_emitted_hole(
                mangle_fn(ws, cf),
                AlwaysEmittedOutcome::Rejection(RejectionKind::UnconstrainedTypeVar),
                span,
            );
            g.record_site(from, site, SiteOutcome::Failed("unconstrained-type-var"), span);
            None
        }
        Err(fail) if from.is_none() => {
            g.record_always_emitted_hole(
                mangle_fn(ws, cf),
                AlwaysEmittedOutcome::Unresolved(fail.reason()),
                span,
            );
            g.record_site(from, site, SiteOutcome::Failed(fail.reason()), span);
            None
        }
        Err(Fail::Unconstrained(_)) => {
            g.rejections.push(Rejection {
                kind: RejectionKind::UnconstrainedTypeVar,
                at: from.expect("guarded above").clone(),
                span,
                message: format!(
                    "the `next` witness `{}` binds a type argument no constraint reaches",
                    mangle_fn(ws, cf)
                ),
            });
            g.record_site(from, site, SiteOutcome::Failed("unconstrained-type-var"), span);
            None
        }
        Err(fail) => {
            g.unresolved.push(Unresolved {
                from: from.expect("guarded above").clone(),
                callee,
                reason: fail.reason(),
                span,
            });
            g.record_site(from, site, SiteOutcome::Failed(fail.reason()), span);
            None
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn walk_bound_tree(
    g: &mut InstanceGraph,
    work: &mut Vec<(Instance, MonoSubst, u32)>,
    ws: &Workspace,
    inf: &BodyInference,
    from: Option<&Instance>,
    subst: &MonoSubst,
    depth: u32,
    tree: &bone_semantic::prelude::BoundTree,
    span: Span,
    target: bone_semantic::hir::FnBackend,
    expr: Idx<bone_semantic::hir::Expr>,
    root: BoundRoot,
    path: &[usize],
) -> InstanceBoundTree {
    let mut resolved = InstanceBoundTree::default();
    let mut keys: Vec<&usize> = tree.next.keys().collect();
    keys.sort();
    for k in keys {
        // The conformance PATH, not the parameter index alone: two witnesses at
        // different depths of one tree can carry the same key.
        let mut child = path.to_vec();
        child.push(*k);
        if let Some(next) = record_bound_next(
            g, work, ws, inf, from, subst, depth, &tree.next[k], span,
            EdgeKind::BoundNextWitness, target, expr, root, &child,
        ) {
            resolved.next.insert(*k, next);
        }
    }
    resolved
}

/// The callee's instantiation: its generic bindings converted to `MonoTy` under
/// the CALLER's substitution, plus the substitution to walk the callee's body
/// with. This is the transitive-instantiation step.
fn instantiate(
    arena: &mut MonoArena,
    ws: &Workspace,
    inf: &BodyInference,
    callee: &Function,
    bindings: Option<&Vec<bone_semantic::prelude::TyId>>,
    caller_subst: &MonoSubst,
) -> Result<(Vec<MonoTyId>, MonoSubst), Fail> {
    if callee.generics.is_empty() {
        return Ok((Vec::new(), MonoSubst::default()));
    }
    let Some(bindings) = bindings else { return Err(Fail::Unresolved("no-bindings")) };
    if bindings.len() != callee.generics.len() {
        return Err(Fail::Unresolved("arity-mismatch"));
    }
    let mut args = Vec::with_capacity(bindings.len());
    let mut callee_subst = MonoSubst::default();
    for (g, b) in callee.generics.iter().zip(bindings) {
        let Some(var) = skull_mir::monoty::generic_var_of(ws, inf, *g) else {
            return Err(Fail::Unresolved("declared-generic-not-generic"));
        };
        let ty = skull_mir::monoty::convert(ws, inf, *b, caller_subst).map_err(convert_fail)?;
        args.push(arena.intern(ty.clone()));
        callee_subst.insert(var, ty);
    }
    Ok((args, callee_subst))
}

/// A SYNTHESIZED call's instantiation, from the record bone kept for it: the
/// `for`-protocol `next` witness (`next_generics`), the `for`-head `iter` and
/// the coerced-argument `iter` (`IterWitness::generics`) all take this route.
///
/// Such a call has no call expr, so it can never appear in
/// `call_generic_bindings` (plan S 12: resolve the witness at instantiation).
/// Reading the callee's DECLARED generics and looking each up here is the same
/// route `spec_key_from_next_generics` takes -- a shape comes from a
/// declaration, never from the body's own inference.
fn instantiate_witness(
    arena: &mut MonoArena,
    ws: &Workspace,
    inf: &BodyInference,
    resolved: &Function,
    callee: &Function,
    record: &FxHashMap<bone_semantic::hir::VarId, bone_semantic::prelude::TyId>,
    caller_subst: &MonoSubst,
) -> Result<(Vec<MonoTyId>, MonoSubst), Fail> {
    if callee.generics.is_empty() {
        return Ok((Vec::new(), MonoSubst::default()));
    }
    // The record is keyed by the RESOLVED declaration's `VarId`s; the walk
    // instantiates the declaration the TARGET selected. Backend siblings are
    // separate functions with separate `VarId`s, so the two are read
    // POSITIONALLY -- generic `i` of the resolved decl supplies generic `i` of
    // the variant. Keying the lookup on the variant instead silently degrades a
    // generic witness to unbound on the non-default target alone.
    if resolved.generics.len() != callee.generics.len() {
        return Err(Fail::Unresolved("variant-generic-arity-mismatch"));
    }
    let mut args = Vec::with_capacity(callee.generics.len());
    let mut callee_subst = MonoSubst::default();
    for (rg, cg) in resolved.generics.iter().zip(&callee.generics) {
        let (Some(rvar), Some(cvar)) = (
            skull_mir::monoty::generic_var_of(ws, inf, *rg),
            skull_mir::monoty::generic_var_of(ws, inf, *cg),
        ) else {
            return Err(Fail::Unresolved("declared-generic-not-generic"));
        };
        let Some(bound) = record.get(&rvar) else {
            return Err(Fail::Unresolved("witness-generic-unbound"))
        };
        let ty =
            skull_mir::monoty::convert(ws, inf, *bound, caller_subst).map_err(convert_fail)?;
        args.push(arena.intern(ty.clone()));
        callee_subst.insert(cvar, ty);
    }
    Ok((args, callee_subst))
}

/// Why an instantiation produced no instance.
///
/// [`Fail::Unresolved`] is data: the walk had no binding to key an
/// instantiation on, which is a property of the RECORD, not of the types.
/// [`Fail::Unconstrained`] is a decided rejection
/// ([`RejectionKind::UnconstrainedTypeVar`]).
///
/// **`UnboundGeneric` is a rejection here, not data.** A shared generic root is
/// never put on the worklist -- it goes to
/// [`InstanceGraph::uninstantiated_roots`] -- so every body the walk enters is
/// INSTANTIATED, and a declared generic missing from the enclosing substitution
/// means that substitution is incomplete. The boxed answer belongs only to
/// legacy lowering, where shared snapshots actually exist.
enum Fail {
    Unresolved(&'static str),
    Unconstrained(&'static str),
}

impl Fail {
    fn reason(&self) -> &'static str {
        match self {
            Self::Unresolved(r) | Self::Unconstrained(r) => r,
        }
    }
}

fn convert_fail(r: MonoTyReason) -> Fail {
    match r {
        // Inference never bound the variable, so there is no type to
        // instantiate at -- not a missing record, an absent constraint.
        MonoTyReason::UnresolvedInfer(_) => Fail::Unconstrained(r.bucket()),
        // An incomplete substitution in an instantiated body. See [`Fail`].
        MonoTyReason::UnboundGeneric(_) => Fail::Unconstrained(r.bucket()),
        _ => Fail::Unresolved(r.bucket()),
    }
}

/// The backend variant of `rep` serving `target`, through production's own
/// selector. `Err` (nothing serves it) keeps `rep`: the boundary pass reports
/// that as an INVALID boundary for the target, which is the outcome plan S 10
/// requires, rather than dropping the edge and under-reporting the graph.
fn variant_for(
    ws: &Workspace,
    rep: FuncRef,
    target: bone_semantic::hir::FnBackend,
) -> FuncRef {
    let name = ws.module(rep.module).functions[rep.func].name;
    skull_mir::lower::backend_variant(ws, name, rep, target).unwrap_or(rep)
}

/// Every `Expr::Global(name)` in `body` that names a FUNCTION -- skull's own
/// fn-value discriminant (`lower/expr.rs:75`).
///
/// Resolution mirrors `fn_value_symbol`: bone's recorded overload wins;
/// otherwise the candidates are backend VARIANTS of one `(name, signature)` and
/// mangle identically, so any of them names the same instance. A genuine
/// unresolved overload (candidates with distinct manglings) is skipped here --
/// `fn_value_symbol` REJECTS it, so it cannot occur in a program that compiles.
///
/// A `Expr::Global` in `Callee::Value` position is an indirect CALL, not a value
/// take, and is excluded.
fn fn_value_sites(
    ws: &Workspace,
    inf: &BodyInference,
    body: &bone_semantic::hir::Body,
    target: bone_semantic::hir::FnBackend,
) -> Vec<(Idx<bone_semantic::hir::Expr>, FuncRef)> {
    use bone_semantic::hir::{Callee, Expr as E};
    let mut called_values: BTreeSet<u32> = BTreeSet::new();
    for (_, expr) in body.expr_arena.iter() {
        if let E::Call { callee: Callee::Value(ve), .. } = expr {
            called_values.insert(ve.into_raw().into());
        }
    }
    let mut out = Vec::new();
    for (e, expr) in body.expr_arena.iter() {
        let E::Global(name) = expr else { continue };
        if called_values.contains(&e.into_raw().into()) {
            continue;
        }
        let Some(cands) = ws.functions_by_name.get(name) else { continue };
        let rep = match inf.resolved_overload_calls.get(&e) {
            Some(&(m, f)) => FuncRef { module: m, func: f },
            None => {
                let mut manglings: Vec<String> = cands
                    .iter()
                    .map(|&(m, fi)| mangle_fn(ws, &ws.module(m).functions[fi]))
                    .collect();
                manglings.sort();
                manglings.dedup();
                if manglings.len() != 1 {
                    continue;
                }
                let (m, f) = cands[0];
                FuncRef { module: m, func: f }
            }
        };
        out.push((e, variant_for(ws, rep, target)));
    }
    out.sort_by_key(|(e, _)| *e);
    out
}

/// One synthesized-call site, resolved against its instance.
struct GeneratedSite {
    expr: Idx<bone_semantic::hir::Expr>,
    name: &'static str,
}

/// The verdict on one storage-condition query, kept as three outcomes rather
/// than a bool. A bool made every conversion failure read as "boxed", which is
/// only the right answer for ONE of them.
enum CondVerdict {
    /// The column element the receiver stores, when there is one.
    /// [`StorageCond::Always`] holds without naming an element.
    Holds(Option<skull_mir::mir::ColumnElem>),
    /// The receiver is not column-stored -- an ordinary negative.
    DoesNot,
    /// The receiver's type could not be made concrete. In an INSTANTIATED body
    /// this is an invariant violation, not a storage answer.
    Failed(MonoTyReason),
}

/// Which of `__vec_data_sort`'s two column arms a site emits
/// (`skull-mir/src/lower/storage.rs`, the `("__vec_data_sort", ..)` arm): a
/// `nobox` element applies the key to the LEAVES, everything else to the
/// materialized element.
///
/// Lowering asks `is_nobox_struct`, which is `is_nobox_nominal` plus the
/// `values_first_class` gate. That flag is set only by the GLSL emitter, and
/// GLSL is not an `FnBackend` this walk runs for, so the two agree on every
/// target an `InstanceGraph` is built for -- and on a target where they would
/// not, an `is_nobox_nominal` answer is the wrong one to bake in here.
fn sort_entry(ws: &Workspace, elem: &skull_mir::mir::ColumnElem) -> &'static str {
    if skull_mir::lower::is_nobox_nominal(ws, elem.name) {
        "__rock_col_sort_leaves"
    } else {
        "__rock_col_sort"
    }
}

/// Which runtime entries an INDEX on a column-stored `Map` emits.
///
/// Three arms in `storage.rs`, and the HIR says which one is taken:
/// `m[k] = v` is `map_column_store` (`__map_col_slot`, plus `__map_col_free`
/// on the absent arm when the stored value can be null); every other position
/// -- an rvalue read, or the place base of `m[k].f = v` -- resolves the key
/// through `__map_col_slot_checked`.
///
/// `nullable` is `None` when the stored value's type did not convert. Both
/// names are kept then and the site is counted as approximated: dropping
/// `__map_col_free` would drop a symbol emission requires, which is the
/// direction that breaks a build.
fn map_index_entries(store: Option<Option<bool>>) -> (Vec<&'static str>, bool) {
    match store {
        None => (vec!["__map_col_slot_checked"], false),
        Some(Some(true)) => (vec!["__map_col_slot", "__map_col_free"], false),
        Some(Some(false)) => (vec!["__map_col_slot"], false),
        Some(None) => (vec!["__map_col_slot", "__map_col_free"], true),
    }
}

/// Source operations whose lowering can SYNTHESIZE a `stdlib_funcref` /
/// `call_spine` call, paired with each name it can synthesize. Over-approximates
/// -- see [`COMPILER_GENERATED_TRIGGERS`].
fn compiler_generated_sites(
    ws: &Workspace,
    inf: &BodyInference,
    body: &bone_semantic::hir::Body,
    subst: &MonoSubst,
    target: bone_semantic::hir::FnBackend,
    examined: &mut usize,
    failures: &mut Vec<(Idx<bone_semantic::hir::Expr>, MonoTyReason)>,
    approximated: &mut usize,
) -> Vec<GeneratedSite> {
    use bone_semantic::hir::{Callee, Expr as E};
    // The receiver's CONCRETE type under this instance's substitution, which is
    // what the storage condition is asked about.
    let verdict = |recv: Idx<bone_semantic::hir::Expr>, cond: StorageCond| -> CondVerdict {
        if cond == StorageCond::Always {
            return CondVerdict::Holds(None);
        }
        let Some(ty) = inf.expr_types.get(recv) else {
            return CondVerdict::Failed(MonoTyReason::NoNode);
        };
        let mono = match skull_mir::monoty::convert(ws, inf, *ty, subst) {
            Ok(m) => m,
            Err(r) => return CondVerdict::Failed(r),
        };
        let elem = match cond {
            StorageCond::VecColumn => skull_mir::lower::vec_column_elem_mono(ws, target, &mono),
            StorageCond::MapColumn => skull_mir::lower::map_column_value_mono(ws, target, &mono),
            StorageCond::Always => return CondVerdict::Holds(None),
        };
        match elem {
            Some(elem) => CondVerdict::Holds(Some(elem)),
            None => CondVerdict::DoesNot,
        }
    };
    // EVERY failure is reported, including `UnboundGeneric`. The walk only ever
    // enters instantiated bodies -- a shared generic root goes to
    // `uninstantiated_roots` and is never worklisted -- so an unbound declared
    // generic here means the substitution is incomplete, not that the element is
    // legitimately opaque. Reading any of them as "boxed" would let a zero edge
    // count mean "the sites were discarded".
    let mut settle = |recv, cond, e: Idx<bone_semantic::hir::Expr>| -> Option<Option<_>> {
        *examined += 1;
        match verdict(recv, cond) {
            CondVerdict::Holds(elem) => Some(elem),
            CondVerdict::DoesNot => None,
            CondVerdict::Failed(r) => {
                failures.push((e, r));
                None
            }
        }
    };
    // `m[k] = v` is the one index position that lowers through
    // `map_column_store`; the value expression is what decides whether the
    // absent arm (`__map_col_free`) exists at all.
    let mut stored_through: FxHashMap<Idx<bone_semantic::hir::Expr>, Idx<bone_semantic::hir::Expr>> =
        FxHashMap::default();
    for (_, expr) in body.expr_arena.iter() {
        if let E::Assign { target, value } = expr {
            stored_through.insert(*target, *value);
        }
    }
    // Whether the stored value can be null. `None` means its type did not
    // convert, which keeps both arms -- see [`map_index_entries`].
    let stored_nullable = |value: Idx<bone_semantic::hir::Expr>| -> Option<bool> {
        let ty = inf.expr_types.get(value)?;
        let mono = skull_mir::monoty::convert(ws, inf, *ty, subst).ok()?;
        Some(mono.peel_nullable().1)
    };

    let mut out: Vec<GeneratedSite> = Vec::new();
    for (e, expr) in body.expr_arena.iter() {
        match expr {
            E::Call { callee: Callee::Unresolved { name, .. }, args } => {
                let Some((trigger, cond, names)) =
                    COMPILER_GENERATED_TRIGGERS.iter().find(|(t, _, _)| *t == name.as_str())
                else {
                    continue;
                };
                // The receiver is the UFCS first argument; an arm with no
                // receiver cannot be the column form.
                let Some(recv) = args.clone().next() else { continue };
                let Some(elem) = settle(recv, *cond, e) else { continue };
                // The one trigger with alternatives: the element decides which.
                if *trigger == "__vec_data_sort" {
                    let Some(elem) = elem else { continue };
                    out.push(GeneratedSite { expr: e, name: sort_entry(ws, &elem) });
                    continue;
                }
                out.extend(names.iter().map(|n| GeneratedSite { expr: e, name: n }));
            }
            // The index forms have no callee name to key on: the receiver's
            // CONSTRUCTOR is the trigger. Rendering is confined to index exprs,
            // which are a small minority of any body.
            E::Index { base, .. } => {
                let Some(ty) = inf.expr_types.get(*base) else { continue };
                // A `TyId::Local` renders only against ITS body's inference --
                // `None` panics rather than degrading, which is how this was
                // caught immediately.
                let r = ws.render(Some(inf), *ty);
                let Some((ctor, cond, names)) =
                    INDEX_TRIGGERED.iter().find(|(ctor, _, _)| r.starts_with(*ctor))
                else {
                    continue;
                };
                if settle(*base, *cond, e).is_none() {
                    continue;
                }
                if *ctor == "Map" {
                    let (names, approx) = map_index_entries(
                        stored_through.get(&e).map(|v| stored_nullable(*v)),
                    );
                    *approximated += usize::from(approx);
                    out.extend(names.into_iter().map(|n| GeneratedSite { expr: e, name: n }));
                    continue;
                }
                out.extend(names.iter().map(|n| GeneratedSite { expr: e, name: n }));
            }
            _ => {}
        }
    }
    out.sort_by_key(|s| (s.expr, s.name));
    out
}

/// The unique stdlib function `name` serving `target` -- the same lookup
/// `Builder::stdlib_funcref` performs, and it fails on the same conditions
/// (missing name, no serving body, residual ambiguity).
pub fn stdlib_entry(
    ws: &Workspace,
    name: &str,
    target: bone_semantic::hir::FnBackend,
) -> Option<FuncRef> {
    let decls = ws.functions_by_name.get(&ustr::Ustr::from(name))?;
    let serving: Vec<FuncRef> = decls
        .iter()
        .filter(|(m, f)| serves(ws.module(*m).functions[*f].backend, target))
        .map(|&(m, f)| FuncRef { module: m, func: f })
        .collect();
    match serving.as_slice() {
        [one] => Some(*one),
        _ => None,
    }
}

/// Whether a declaration with backend `decl` can be emitted for `target`.
/// THE compatibility test -- the walk, the boundary pass and every artifact
/// view ask this one, so "does this declaration serve me" cannot be answered
/// two ways.
pub fn serves(
    decl: bone_semantic::hir::FnBackend,
    target: bone_semantic::hir::FnBackend,
) -> bool {
    use bone_semantic::hir::FnBackend;
    decl == FnBackend::Any || target == FnBackend::Any || decl == target
}

/// Classify every reached callee that is a BOUNDARY, per
/// `docs/boundary-contracts.md` Part 1.
///
/// An instance is kept for a boundary as well as a Rock body -- Stage 2 splits
/// them (plan S 9: one `InstanceId` OR one `BoundaryId`), and keeping both here
/// makes the split's cost countable instead of changing the round-1 numbers
/// underneath it.
/// The boundary class of one declaration, or `None` if it has a Rock body the
/// walk enters. The ONE classifier: [`classify_boundaries`] and
/// [`InstanceGraph::closure`] both ask it, so "is this a boundary" cannot be
/// answered two ways.
pub fn boundary_class_for(
    f: &Function,
    erased: &BTreeSet<String>,
) -> Option<BoundaryClass> {
    if erased.contains(f.name.as_str()) {
        return Some(BoundaryClass::ErasedRuntimeEntry);
    }
    declared_boundary(f)
}

/// The part of the classification that reads only the DECLARATION. The walk uses
/// this rather than [`boundary_class_of`] because `erased_entries` is still
/// being discovered while it runs, and expansion must not depend on the order
/// two instances were reached in. It is a SUBSET of the full classification, so
/// nothing the walk calls expandable can later be excused as a boundary and
/// nothing the walk skips can become un-excused.
///
/// **`@intrinsic` is not unconditionally foreign.** Emit resolution is
/// `backend mapping > portable Rock fallback > spanned reject`
/// (`skull_mir::intrinsics`), so an intrinsic that HAS a Rock body has a body
/// the shared graph must expand -- the walk enumerates it either way, and
/// calling it a boundary made "expanded or boundary" a label rather than a
/// partition. Three declarations keep it foreign, each for its own reason:
///
/// - a compile-time expansion intrinsic (`@struct_expand`, `@shader_expand`,
///   `@vertex_expand`, `@uniform_expand`, `@materialize`) has no callable body
///   at all -- the annotation IS the declaration surface;
/// - `@native_required` skips the fallback rung, so its Rock body, if any, is
///   never the emitted one and a missing mapping is fatal rather than portable;
/// - an intrinsic with no Rock body has nothing to expand.
///
/// A target artifact view may still REPLACE an expanded fallback with that
/// target's mapping. That is an artifact decision, not a shared-graph one.
fn declared_boundary(f: &Function) -> Option<BoundaryClass> {
    if f.is_extern {
        return Some(BoundaryClass::Extern);
    }
    if skull_mir::intrinsics::expand_kind(f).is_some() {
        return Some(BoundaryClass::Intrinsic);
    }
    if f.annotations.iter().any(|a| a.name.name.as_str() == "intrinsic") {
        // `inline_body` is the luajit MAPPING; a Rock body is the fallback.
        return (skull_mir::intrinsics::is_native_required(f) || !has_rock_body(f))
            .then_some(BoundaryClass::Intrinsic);
    }
    if f.inline_body.is_some() {
        return Some(BoundaryClass::InlineBody);
    }
    None
}

/// Whether the declaration carries a portable Rock body, as opposed to only a
/// foreign one. `has_body` on the module is the walk's own question; this is the
/// declaration-level form the classifier needs before any module lookup.
fn has_rock_body(f: &Function) -> bool {
    f.inline_body.is_none() && !f.is_extern
}

/// Runtime entries an opaque `inline(lua)` body CALLS.
///
/// An inline body is foreign text, so the walk cannot lower it -- but emission
/// still has to emit every symbol it names, and "opaque by design" cannot make a
/// required symbol vanish from the graph. The corpus's case is
/// `__rock_vec_column_elements`: the reflection walker calls it from Lua and no
/// lowering arm synthesizes it.
///
/// The scan is over [`ERASED_RUNTIME_ENTRIES`] only. That is deliberate: a
/// general Lua parse is not what this needs, and the catalogue is the closed set
/// of compiler-owned runtime symbols an opaque body may depend on. A name
/// appearing in a comment or a string inside the body would be a false positive;
/// none exists in the corpus, and a false POSITIVE keeps a symbol alive, where a
/// false negative drops one emission requires.
fn record_inline_body_deps(g: &mut InstanceGraph, ws: &Workspace) {
    // METADATA ONLY so far: the dependency is recorded and joins
    // `erased_entries`, but it is not yet resolved to a `BoundaryId` or an
    // instance the way a call edge is. That resolution belongs to the artifact
    // view, which is what decides the emitted spelling and the retention set;
    // recording it here without a target would be a second, weaker reachability
    // mechanism.
    let mut deps: BTreeMap<DeclKey, BTreeSet<&'static str>> = BTreeMap::new();
    for i in &g.instances {
        let f = &ws.module(i.func.module).functions[i.func.func];
        let Some(body) = &f.inline_body else { continue };
        let text = body.1.as_str();
        for name in ERASED_RUNTIME_ENTRIES {
            if text.contains(name) {
                deps.entry(DeclKey(i.func)).or_default().insert(name);
                g.erased_entries.insert((*name).to_string());
            }
        }
    }
    g.inline_body_deps = deps;
}

fn classify_boundaries(g: &mut InstanceGraph, ws: &Workspace) {
    let erased: BTreeSet<String> = g.erased_entries.clone();
    let mut found: BTreeMap<String, Boundary> = BTreeMap::new();
    for i in &g.instances {
        let f = &ws.module(i.func.module).functions[i.func.func];
        let symbol = mangle_fn(ws, f);
        let Some(class) = boundary_class_for(f, &erased) else { continue };
        // `variant_for` kept the representative when nothing served the target,
        // so a decl whose backend does not serve it is the S 1.6 outcome:
        // reject with a span, never a stub.
        let invalid_on_target = !serves(f.backend, g.target);
        found.insert(symbol.clone(), Boundary { class, symbol, invalid_on_target });
    }
    g.boundaries = found;
}

/// The helper symbols each coroutine instance owns.
///
/// `__step` and `__resume` are unconditional; `__advance` exists only on the
/// frame-slot channel, which `coroutine_yield_channel` reads from the DECLARED
/// return type -- a declaration, not the body's own inference, per the standing
/// rule. Defers (`__defer<k>`) and lifecycle hooks (`__on_*`) are NOT counted:
/// they come from `CoroLowered`, which exists only after the MIR transform, so
/// there is no HIR-level oracle for them and guessing would be a fabricated
/// number.
fn record_coroutine_helpers(g: &mut InstanceGraph, ws: &Workspace) {
    let mut out = BTreeSet::new();
    for i in &g.instances {
        let f = &ws.module(i.func.module).functions[i.func.func];
        if !f.is_coroutine {
            continue;
        }
        let base = mangle_fn(ws, f);
        let owner_identity = g.identity(ws, i);
        let mut suffixes: Vec<&'static str> = vec!["__step", "__resume"];
        if f.return_ty
            .and_then(|t| skull_mir::lower::coroutine_yield_channel(ws, t))
            .is_some()
        {
            suffixes.push("__advance");
        }
        for suffix in suffixes {
            out.insert(CoroutineHelper {
                owner_identity: owner_identity.clone(),
                suffix,
                symbol_today: format!("{base}{suffix}"),
            });
        }
    }
    g.coroutine_helpers = out;
}

impl InstanceGraph {
    /// Helper symbols that more than one INSTANCE claims.
    ///
    /// Each is a Stage-3 collision: `emit_coroutine` names helpers from the
    /// source `FuncRef`, so every instance of one generic `co fn` produces the
    /// same `<name>__step` / `__resume` / `__advance`. The current specializer
    /// does not specialize coroutines, so the graph exposes collisions that the
    /// Stage-3 cutover must prevent.
    pub fn colliding_coroutine_helpers(&self) -> BTreeMap<String, BTreeSet<String>> {
        let mut by_symbol: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for h in &self.coroutine_helpers {
            by_symbol
                .entry(h.symbol_today.clone())
                .or_default()
                .insert(h.owner_identity.clone());
        }
        by_symbol.retain(|_, owners| owners.len() > 1);
        by_symbol
    }
}

pub(crate) enum FunctionBody<'a> {
    Ready { inf: &'a BodyInference, body: &'a Body },
    NoInference,
    NoBody,
    StaleInference,
}

pub(crate) fn function_body(ws: &Workspace, f: FuncRef) -> FunctionBody<'_> {
    let owner = BodyOwner::Function(f.func);
    let Some(inf) = ws.inference(f.module).and_then(|ir| ir.per_fn.get(&owner)) else {
        return FunctionBody::NoInference;
    };
    let Some(body) = ws.module(f.module).bodies.get(&owner) else {
        return FunctionBody::NoBody;
    };
    if inf.body_generation != body.generation {
        return FunctionBody::StaleInference;
    }
    FunctionBody::Ready { inf, body }
}

/// Every function the emitter would root, using production's own predicate.
pub fn emission_roots(ws: &Workspace, project_root: Option<&str>) -> Vec<FuncRef> {
    let (deco_map, _) = super::mir_emit::build_deco_map(ws);
    let mut roots = Vec::new();
    for module in ws.modules.iter() {
        if module.id == ModuleId::builtin() {
            continue;
        }
        for (fid, f) in module.functions.iter() {
            if super::mir_emit::is_emission_root(module.file.as_str(), project_root, f, &deco_map)
            {
                roots.push(FuncRef { module: module.id, func: fid });
            }
        }
    }
    roots
}

/// The compact manifest: one line per instance, sorted by identity, each line
/// carrying its identity and its display symbol.
pub fn render_manifest(ws: &Workspace, g: &InstanceGraph) -> String {
    let mut out = String::new();
    for s in g.manifest_lines(ws) {
        out.push_str(&s);
        out.push('\n');
    }
    out
}

/// A stable digest of the manifest, for cross-run and cross-projection
/// comparison without carrying the whole text.
pub fn manifest_digest(ws: &Workspace, g: &InstanceGraph) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in render_manifest(ws, g).bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

pub fn instance_count(g: &InstanceGraph) -> usize {
    g.instances.len()
}

/// Reconciliation of the current conditional specializer against the shadow graph
/// (plan S 15a). Equality is NOT expected -- the current mechanism is conditional,
/// so the shadow side is strictly larger. What must hold is that nothing today's
/// machinery produces is MISSING from the shadow graph; an absence in that
/// direction means the shadow walk cannot see an edge production already takes.
#[derive(Default, Debug)]
pub struct Reconciliation {
    /// Specializations the current machinery produces.
    pub today: usize,
    /// ...of which the shadow graph contains a matching instance.
    pub matched: usize,
    /// ...of which it does NOT, and whose CALLER the shadow walk did reach.
    /// Each of these is a failure, not a difference.
    pub missing: Vec<String>,
    /// ...of which it does not, but whose caller the shadow walk never reached
    /// either. A reachability difference, explained rather than unexplained.
    pub missing_unreached_caller: usize,
    /// Call sites whose current spec decision was a Stage-0b invariant
    /// rejection. Expected to be zero on a valid corpus.
    pub rejected_sites: usize,
    /// Shadow instances with no counterpart today. These are the EXPECTED
    /// difference: instances the conditional specializer declines to make.
    pub shadow_only: usize,
}

/// A today-slot matches a shadow arg when the slot is `None` (the partial key
/// erased it, so it constrains nothing) or the canonical types are equal. Both
/// sides encode with `MonoTy::canonical`, so a pretty-printing ambiguity cannot
/// match two different types to each other.
fn slots_match(slots: &[Option<String>], shadow: &[String]) -> bool {
    slots.len() == shadow.len()
        && slots
            .iter()
            .zip(shadow)
            .all(|(s, a)| s.as_ref().is_none_or(|s| s == a))
}

pub fn reconcile(
    ws: &Workspace,
    g: &InstanceGraph,
    target: bone_semantic::hir::FnBackend,
) -> Reconciliation {
    let (today, rejected_sites) = skull_mir::spec_census::today_specializations(ws, target);

    // Shadow instances grouped by function, as canonical argument lists.
    let mut by_func: FxHashMap<(u32, u32), Vec<Vec<String>>> = FxHashMap::default();
    for i in &g.instances {
        by_func
            .entry((i.func.module.raw(), i.func.func.into_raw().into_u32()))
            .or_default()
            .push(i.args.iter().map(|a| g.arena.canonical(*a)).collect());
    }

    let mut r = Reconciliation { today: today.len(), rejected_sites, ..Default::default() };
    let mut matched_shadow: std::collections::HashSet<(u32, u32, Vec<String>)> =
        Default::default();
    for t in &today {
        let k = (t.func.module.raw(), t.func.func.into_raw().into_u32());
        let hit = by_func
            .get(&k)
            .map(|args| args.iter().find(|a| slots_match(&t.slots, a)))
            .unwrap_or(None);
        match hit {
            Some(a) => {
                r.matched += 1;
                matched_shadow.insert((k.0, k.1, a.clone()));
            }
            None => {
                let ck = (t.caller.module.raw(), t.caller.func.into_raw().into_u32());
                if !by_func.contains_key(&ck) {
                    r.missing_unreached_caller += 1;
                    continue;
                }
                let f = &ws.module(t.func.module).functions[t.func.func];
                let caller = &ws.module(t.caller.module).functions[t.caller.func];
                let slots: Vec<&str> =
                    t.slots.iter().map(|s| s.as_deref().unwrap_or("<erased>")).collect();
                r.missing.push(format!(
                    "{}<{}> (called from `{}`)",
                    f.name,
                    slots.join(", "),
                    caller.name
                ));
            }
        }
    }
    r.missing.sort();
    r.missing.dedup();
    r.shadow_only = g
        .instances
        .iter()
        .filter(|i| {
            let k = (i.func.module.raw(), i.func.func.into_raw().into_u32());
            let args: Vec<String> = i.args.iter().map(|a| g.arena.canonical(*a)).collect();
            !matched_shadow.contains(&(k.0, k.1, args))
        })
        .count();
    r
}
