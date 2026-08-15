//! The target-neutral instance graph (`docs/monomorphization-plan.md` Stage 0d).
//!
//! It owns logical instance reachability below the target artifact views.
//!
//! An [`Instance`] is `(FuncRef, Vec<MonoTyId>)` -- a function plus the total
//! logical types of its generic arguments, keyed on
//! [`MonoTy`](skull_mir::monoty::MonoTy) from Stage 0c rather than on an
//! emission-private key. That is the whole point of the probe: such a key was
//! partial (plan S 2a), so
//! `f<Vec2, unit>` and `f<Vec2, any>` share its identity while they get distinct
//! `InstanceId`s here.
//!
//! **Roots come from production.** [`super::mir_emit::is_emission_root`] is the
//! same predicate `reachable_fns` seeds its walk with; re-deriving it would make
//! any difference between this graph and the real one an artifact of the copy.
//!
//! **Transitive instantiation is the core.** Walking a callee's body under the
//! CALLER's substitution is what makes `count<I>` at `I = VecIter<Vec2>` a
//! different instance from the same fn at `VecIter<int>`.
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
use std::time::Instant;

use bone_semantic::hir::{Body, BodyOwner, Function, TypeExpr, VarId};
use bone_semantic::inference::{BodyInference, Workspace};
use bone_semantic::prelude::{BoundNext, IterWitness, ModuleId, Span, TyId};
use comfy_la_arena::Idx;
use rustc_hash::{FxHashMap, FxHashSet};
use skull_mir::mangle::mangle_fn;
use skull_mir::lower::{ContainerFamily, ContainerPlan, RepArena};
use skull_mir::monoty::{MonoArena, MonoInterner, MonoSubst, MonoTyId, MonoTyReason};
use skull_mir::mir::FuncRef;

/// The identity vocabulary now lives in `skull-mir` ([`skull_mir::instance`]),
/// low enough that emission can key its bodies on it. This module remains the
/// AUTHORITY that builds instances and owns the [`MonoArena`] their `args`
/// index into; only the type's placement moved.
pub use skull_mir::instance::{DeclKey, Instance};

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

/// The boundary classifier lives below the graph, so artifact projection and
/// lowering-side rejection cannot drift into separate vocabularies.
pub use skull_mir::lower::BoundaryClass;

/// One reached boundary, classified from the DECLARATION alone.
///
/// `symbol` is the name emission must produce: bare for `extern`/`@nomangle`,
/// mangled otherwise -- the rule `skull_mir::mangle` applies, read from it
/// rather than restated.
///
/// Whether a target can SERVE it is deliberately absent. That is plan S 10's
/// `Invalid boundary`, and it is a property of an artifact, which is where it
/// is answered (`artifact::Unrealizable::ForeignToArtifact`, reported per CALL
/// as a `Severed` rather than per declaration).
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Boundary {
    pub class: BoundaryClass,
    pub symbol: String,
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

/// The neutral callee-obligation identity, owned by
/// [`skull_mir::site`] -- the lowest crate MIR metadata, this graph and
/// [`crate::concrete_body`] all share. Re-exported here so the graph's
/// consumers keep one spelling; it is the SAME type MIR records, never a
/// translation of it.
pub use skull_mir::site::{Site, SiteDomain, SiteOp};

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

impl InstanceBoundTree {
    /// Convert the graph's witness tree to the vocabulary standalone lowering consumes.
    pub fn to_resolved(&self) -> skull_mir::bound::ResolvedBoundTree {
        skull_mir::bound::ResolvedBoundTree {
            next: self
                .next
                .iter()
                .map(|(index, witness)| {
                    (
                        *index,
                        skull_mir::bound::ResolvedBoundNext::Concrete {
                            instance: witness.callee.clone(),
                            sub: witness.sub.to_resolved(),
                        },
                    )
                })
                .collect(),
        }
    }
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
    fn site(
        self,
        domain: SiteDomain,
        expr: Idx<bone_semantic::hir::Expr>,
        path: Vec<usize>,
    ) -> Site {
        Site {
            domain,
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

/// WHICH body's walk produced a set of facts.
///
/// The two kinds are not a labelling of one thing: an instance's callees are
/// its own obligations and carry an [`Edge`] and a [`Site`], while an
/// always-emitted body's callees are ROOTS with neither -- production emits
/// that body regardless of any call (`mir_emit::reachable_fns`'s
/// `extra_roots`), so nothing points at them.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Owner {
    /// One instance's own body, plus the struct-field defaults it injects.
    Instance(Instance),
    /// A global initializer or a decorator definition.
    AlwaysEmitted(ModuleId, BodyOwner),
}

impl Owner {
    /// A total order. Neither `ModuleId` nor `BodyOwner` has one, and owners are
    /// ordered so that derivation from the contribution set is a function of the
    /// SET rather than of the order the walk discovered it in.
    fn ord_key(&self) -> (u8, Option<&Instance>, usize, u8, u32) {
        match self {
            Self::Instance(i) => (0, Some(i), 0, 0, 0),
            Self::AlwaysEmitted(m, o) => {
                let (tag, idx) = body_owner_key(*o);
                (1, None, m.index(), tag, idx)
            }
        }
    }

    pub fn instance(&self) -> Option<&Instance> {
        match self {
            Self::Instance(i) => Some(i),
            Self::AlwaysEmitted(..) => None,
        }
    }
}

impl Ord for Owner {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.ord_key().cmp(&other.ord_key())
    }
}

impl PartialOrd for Owner {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// A `BodyOwner`'s discriminant and index, so owners can be ordered and
/// compared. Total over the enum on purpose -- a new variant is a compile
/// error here rather than a silent collapse onto an existing tag.
fn body_owner_key(o: BodyOwner) -> (u8, u32) {
    match o {
        BodyOwner::Function(i) => (0, i.into_raw().into_u32()),
        BodyOwner::Decorator(i) => (1, i.into_raw().into_u32()),
        BodyOwner::Global(i) => (2, i.into_raw().into_u32()),
        BodyOwner::StructDefaults(i) => (3, i.into_raw().into_u32()),
        BodyOwner::Enum(i) => (4, i.into_raw().into_u32()),
        BodyOwner::Newtype(i) => (5, i.into_raw().into_u32()),
    }
}

/// **Everything ONE body's walk produced**, kept as the primary record the
/// graph's flat fields are DERIVED from rather than accumulated into them.
///
/// A re-walk REPLACES a contribution wholesale and the flat fields are rebuilt
/// from the whole set, which is what makes removal expressible at all: an
/// accumulated total can only be added to, and a subtraction from one cannot be
/// checked against the population it summarizes.
#[derive(Clone, Default, Debug)]
pub struct Contribution {
    /// Outgoing edges. Empty for an always-emitted body, whose callees are
    /// roots rather than obligations -- see [`Self::always_emitted_targets`].
    pub edges: Vec<Edge>,
    pub call_sites: BTreeMap<Site, SiteRecord>,
    /// Resolved sites from a global/decorator body. These have no instance key
    /// for the graph's cross-owner index, but lowering still needs their owner-local
    /// concrete callee identity.
    pub always_call_sites: BTreeMap<Site, SiteRecord>,
    /// Struct-default arenas this body injects, including defaults with no
    /// callee site of their own.
    pub struct_default_domains: BTreeSet<SiteDomain>,
    pub duplicate_sites: Vec<Site>,
    pub unresolved: Vec<Unresolved>,
    pub rejections: Vec<Rejection>,
    pub storage_failures: Vec<MonoTyReason>,
    pub erased_entries: BTreeSet<String>,
    pub always_emitted_targets: BTreeSet<Instance>,
    /// Always-emitted call sites that could not instantiate, by callee name.
    pub always_emitted_holes: Vec<(String, AlwaysEmittedFailure)>,
    /// Every instance this walk reached, with the substitution it reached it
    /// UNDER. [`InstanceGraph::substitutions`] is derived from these, so an
    /// instance whose only caller is gone loses its substitution with it.
    pub reached: Vec<(Instance, MonoSubst)>,
    /// Concrete conformance witnesses this walk supplied to its callees.
    pub bound_trees: Vec<(Instance, InstanceBoundTree)>,
    /// The FILES this contribution's spans point into.
    ///
    /// It is not always the owner's own file. A struct literal that omits a
    /// field injects that field's initializer from the DECLARING struct's
    /// module, so the injecting instance records sites spanned in a file it
    /// does not live in -- and an edit anywhere in that file shifts them.
    pub span_files: BTreeSet<ustr::Ustr>,
    /// Whether the walk ENTERED the body. False for a boundary, a body with no
    /// usable inference, and a depth-capped instance.
    pub expanded: bool,
    /// The expansion was cut by [`MAX_INSTANTIATION_DEPTH`].
    pub depth_capped: bool,
    pub sites_attempted: usize,
    pub body_exprs_scanned: usize,
    pub resolved_overload_rows: usize,
    pub fn_value_rows: usize,
    pub generated_rows: usize,
    pub dispatched_rows: usize,
    pub implicit_iter_rows: usize,
    pub bound_tree_rows: usize,
    pub bound_call_rows: usize,
    pub iterator_rows: usize,
    pub always_emitted_seeds: usize,
    pub synthesized_examined: usize,
    pub synthesized_approximated: usize,
    pub(crate) timing: ExpandTiming,
}

/// Conditional timings for one owner walk. The fields are zero unless the
/// debug/census path is enabled; the counters and their denominators remain
/// durable.
#[derive(Clone, Copy, Debug, Default)]
pub struct ExpandTiming {
    pub admissibility_ns: u64,
    pub body_prep_ns: u64,
    pub signature_type_conversion_ns: u64,
    pub interner_ns: u64,
    pub call_site_resolution_ns: u64,
    pub witness_bound_tree_ns: u64,
    pub walk_body_edges_ns: u64,
    pub walk_struct_default_edges_ns: u64,
}

fn graph_timing_enabled() -> bool {
    cfg!(debug_assertions)
        || log::log_enabled!(log::Level::Debug)
        || std::env::var_os("ROCK_GRAPH_CENSUS").is_some()
}

fn elapsed_ns(start: Option<Instant>) -> u64 {
    start.map_or(0, |start| start.elapsed().as_nanos() as u64)
}

fn add_elapsed(slot: &mut u64, start: Option<Instant>) {
    *slot += elapsed_ns(start);
}

fn percentile_usize(values: &mut [usize], percentile: usize) -> usize {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    values[(values.len() - 1) * percentile / 100]
}

fn percentile_u64(values: &mut [u64], percentile: usize) -> u64 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    values[(values.len() - 1) * percentile / 100]
}

/// **The one walk context.** A full build and an incremental re-walk both run
/// through it, so neither can record a fact the other does not.
///
/// It holds the interner (shared and monotonic), the owner's OWN
/// conformance witnesses -- the single cross-owner read the walk makes -- and
/// the contribution being produced. Nothing else of the graph is reachable
/// from here, which is what stops a walk from depending on what some other
/// owner happened to record first.
struct Walk<'a> {
    interner: &'a MonoInterner,
    owner_tree: Option<&'a InstanceBoundTree>,
    out: &'a mut Contribution,
    timing_enabled: bool,
}

impl Walk<'_> {
    fn timer(&self) -> Option<Instant> {
        self.timing_enabled.then(Instant::now)
    }

    fn with_interner_mut<R>(&mut self, f: impl FnOnce(&mut MonoArena) -> R) -> R {
        if !self.timing_enabled {
            return self.interner.with_mut(f);
        }
        let started = Instant::now();
        let result = self.interner.with_mut(f);
        self.out.timing.interner_ns += started.elapsed().as_nanos() as u64;
        result
    }

    /// Record what one source operation resolved to.
    ///
    /// Every recorder funnels through here, so a site cannot be recorded in one
    /// place and resolved in another. A repeat key is kept as a finding rather
    /// than overwritten: silently replacing it would make the second operation
    /// vanish from the tables while the counts still looked total.
    fn record_site(
        &mut self,
        from: Option<&Instance>,
        site: Site,
        outcome: SiteOutcome,
        span: Span,
    ) {
        self.out.sites_attempted += 1;
        self.saw(span);
        if from.is_none() {
            let prev = self
                .out
                .always_call_sites
                .insert(site.clone(), SiteRecord { outcome, span });
            if prev.is_some() {
                self.out.duplicate_sites.push(site);
            }
            return;
        }
        let prev = self.out.call_sites.insert(site.clone(), SiteRecord { outcome, span });
        if prev.is_some() {
            self.out.duplicate_sites.push(site);
        }
    }

    /// Reach `to` at `subst`.
    ///
    /// The substitution must be a FUNCTION OF THE INSTANCE -- a specialized
    /// body's shape comes from declarations plus the instance substitution, so
    /// two callers reaching one instance must agree on it. That is not
    /// self-evident: the `VarId` keys come from `generic_var_of` reading the
    /// CALLER's `BodyInference`, so nothing structural forces agreement. Every
    /// reach is kept HERE, with its own caller; the comparison that decides
    /// agreement happens once, over the whole contribution set
    /// ([`InstanceGraph::derive`]), because the walk is no longer the thing
    /// that owns the answer.
    fn reach(&mut self, to: Instance, subst: MonoSubst) {
        self.out.reached.push((to, subst));
    }

    fn record_bound_tree(&mut self, instance: &Instance, incoming: InstanceBoundTree) {
        self.out.bound_trees.push((instance.clone(), incoming));
    }

    fn push_edge(&mut self, e: Edge) {
        self.saw(e.span);
        self.out.edges.push(e);
    }

    fn push_rejection(&mut self, r: Rejection) {
        self.saw(r.span);
        self.out.rejections.push(r);
    }

    fn push_unresolved(&mut self, u: Unresolved) {
        self.saw(u.span);
        self.out.unresolved.push(u);
    }

    /// Note that a fact was recorded at `span`. Every span that reaches a
    /// contribution goes through here or through [`Self::record_site`], so the
    /// invalidation set cannot miss a file the walk quoted.
    fn saw(&mut self, span: Span) {
        if !is_spanless(&span) {
            self.out.span_files.insert(span.file);
        }
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
        self.saw(span);
        self.out.always_emitted_holes.push((name, AlwaysEmittedFailure { outcome, span }));
    }
}

/// A root's substitution, which is empty rather than absent.
static EMPTY_SUBST: std::sync::LazyLock<MonoSubst> =
    std::sync::LazyLock::new(MonoSubst::default);

/// **Everything derived from more than one contribution.**
///
/// A substitution is recorded by the CALLER, not by the instance whose body it
/// belongs to, so "what is this instance's substitution" is not a question a
/// single record can answer. Keeping the per-owner answers rather than one
/// merged value is what makes removal expressible: dropping a caller drops its
/// opinion and the remaining ones still decide.
///
/// The agreed answer is the FIRST owner's in [`Owner`] order, which makes it a
/// function of the contribution SET rather than of the order the walk
/// discovered it in -- so a fresh build and an update cannot pick differently.
#[derive(Default, Debug)]
pub struct Indices {
    /// Instance -> who reached it and what they said.
    pub subst: BTreeMap<Instance, Agreed<MonoSubst>>,
    /// Instance -> who supplied its conformance witnesses and what they said.
    pub trees: BTreeMap<Instance, Agreed<InstanceBoundTree>>,
    /// Runtime entry name -> the owners whose walk named it. Membership is a
    /// non-empty owner set, never a count, so removing the last namer removes
    /// the entry and a stale one cannot survive as a positive number.
    pub erased: BTreeMap<String, BTreeSet<Owner>>,
    /// Instance -> the always-emitted bodies that root it.
    pub always_emitted: BTreeMap<Instance, BTreeSet<Owner>>,
    /// Declaration -> how many LIVE instances it has.
    ///
    /// The three per-declaration post-passes read the declaration set, not the
    /// instance set, so this is what tells them which declarations ENTERED or
    /// LEFT the graph -- the only ones whose answer can have moved.
    pub decls: BTreeMap<DeclKey, usize>,
    /// Instance -> the instances with an edge INTO it.
    ///
    /// Forward adjacency needs no index -- an owner's successors are its own
    /// contribution's edges. The reverse is what pruning needs and what no
    /// single record holds: to decide whether a stranded instance still has a
    /// way in, you have to ask who points AT it.
    ///
    /// HASHED, not ordered. It is written once per EDGE -- 32k times per build
    /// on mag -- and an `Instance`'s `Ord` compares a `Vec<MonoTyId>` element by
    /// element, which cost 65 ms of a 127 ms build. Nothing reads it in order:
    /// the only question asked of it is whether ANY predecessor lies outside a
    /// region.
    pub preds: FxHashMap<Instance, FxHashSet<Instance>>,
}

/// **One fact several owners assert, with the agreement kept rather than
/// assumed.**
///
/// The value is stored ONCE for every owner that agrees on it: they are equal
/// by definition, so keeping a copy per owner would be 32k substitution clones
/// per build on mag to represent 5k distinct answers. An owner that says
/// something DIFFERENT is kept in full, because that is the finding.
///
/// `total` is the number of times the fact was asserted, which is not
/// `owners.len()`: one body can reach one callee from several sites, and the
/// agreement counters are per ASSERTION.
#[derive(Clone, Debug)]
pub struct Agreed<T> {
    /// Owners asserting [`Self::value`].
    pub owners: BTreeSet<Owner>,
    pub value: T,
    /// Owners asserting something else, each with what it said. Empty on every
    /// valid program -- a non-empty one means the fact is not a function of the
    /// instance, so a body built from it would depend on which owner won.
    pub dissent: BTreeMap<Owner, T>,
    pub total: usize,
}

impl<T: Clone + PartialEq> Agreed<T> {
    fn new(owner: Owner, value: T) -> Self {
        Agreed { owners: BTreeSet::from([owner]), value, dissent: BTreeMap::new(), total: 1 }
    }

    fn assert_by(&mut self, owner: Owner, value: &T) {
        self.total += 1;
        if *value == self.value {
            self.owners.insert(owner);
        } else {
            self.dissent.insert(owner, value.clone());
        }
    }

    /// Drop one owner's assertions. Returns false when nothing is left.
    fn retract(&mut self, owner: &Owner, times: usize) -> bool {
        self.total = self.total.saturating_sub(times);
        self.owners.remove(owner);
        self.dissent.remove(owner);
        if self.owners.is_empty() {
            // The agreeing side is gone; a dissenter becomes the answer, or
            // there is no answer left at all.
            let Some(first) = self.dissent.keys().next().cloned() else { return false };
            let value = self.dissent.remove(&first).expect("just read");
            self.owners.insert(first);
            self.value = value;
        }
        true
    }

    /// The answer, decided by OWNER order rather than by arrival, so a fresh
    /// build and an update cannot pick differently.
    fn decided(&self) -> &T {
        let agreed = self.owners.iter().next();
        match (agreed, self.dissent.iter().next()) {
            (Some(a), Some((d, v))) if d < a => v,
            _ => &self.value,
        }
    }
}

pub struct InstanceGraph {
    /// **The build's one arena, SHARED rather than owned.**
    ///
    /// A retained graph and the emission that consumes it both mint and read
    /// [`Instance`]s, so neither can own the arena outright: moving it into
    /// `ScalarCtx` would leave this graph's `MonoTyId`s dangling, and keeping a
    /// copy here would be the second identity this stage exists to remove. The
    /// handle is the durable owner; both sides hold a clone of it.
    interner: std::sync::Arc<MonoInterner>,
    /// The primary records. Every flat field below is derived from these plus
    /// [`Self::root_instances`]; nothing writes a flat field directly.
    pub contributions: BTreeMap<Owner, Contribution>,
    /// Instances production roots regardless of any call. A reachability walk
    /// starts here and at every always-emitted target.
    pub root_instances: BTreeSet<Instance>,
    pub instances: BTreeSet<Instance>,
    /// The last expansion census. It is diagnostic state, not part of graph
    /// identity; keeping it here makes the denominator available to tests and
    /// callers without a second graph walk.
    pub run_stats: RunStats,
    /// Constructions the model refuses that no single body produced:
    /// [`detect_changing_instance_recursion`] is a WHOLE-GRAPH search over the
    /// edge set, so its findings belong to the graph rather than to an owner.
    /// Every other rejection lives in the contribution whose walk raised it;
    /// [`InstanceGraph::rejections`] is the two of them together.
    pub graph_rejections: Vec<Rejection>,
    /// Names with no concrete instance covering the obligation that put them
    /// here, BY CAUSE. See [`RootHole`]: one set carrying several different
    /// facts could only ever be read as the weakest of them.
    pub uninstantiated_roots: BTreeMap<String, RootHole>,
    /// Generic emission roots the narrowing in [`generic_root_obligation`] ruled
    /// out, keyed by the reason. Recorded, not dropped: the population a check
    /// EXCLUDED is the only way to tell a narrowed gate from a dead one, and a
    /// filter whose rejects are invisible reads as "there were none".
    pub excused_generic_roots: BTreeMap<&'static str, BTreeSet<String>>,
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
    /// Bare `@nomangle` declarations named by retained `inline(js)` bodies.
    /// The graph records the opaque-boundary root obligation; a target view
    /// filters it to the declarations it actually realizes.
    pub inline_body_nomangle_roots: BTreeMap<DeclKey, FxHashSet<FuncRef>>,
    /// Storage-condition queries the walk ran, the denominator for the
    /// synthesized-call population. Without it a zero synthesized-edge count
    /// cannot be told apart from a walk that asked nothing.
    pub synthesized_examined: usize,
    /// Synthesized sites whose exact entry could NOT be decided, so more than
    /// one name was kept. The residual over-approximation, as a number rather
    /// than a paragraph.
    pub synthesized_approximated: usize,
    /// Instances that lost an incoming edge since the last [`Self::derive`].
    ///
    /// The prune candidates, and the reason an update does not have to walk
    /// the whole graph to find them: only an instance whose in-edges changed
    /// can have become unreachable. Cleared by `derive`.
    ///
    /// A re-installed owner takes its own targets back OUT: re-walking a body
    /// that still calls what it called before loses nothing, and leaving them
    /// in made the suspect region the whole reachable graph -- 3.3 ms of a 7 ms
    /// update on match3, where 33 owners were re-walked and almost every edge
    /// came back identical.
    lost: BTreeSet<Instance>,
    /// **The cross-owner facts, maintained where a contribution is installed
    /// or removed.** Public for the same reason [`Self::contributions`] is --
    /// the oracle and its tests construct states directly -- but consumers ask
    /// through the accessors, which are the one place the agreed answer is
    /// decided.
    pub index: Indices,
    /// Every site the walk ATTEMPTED, including the ones with no `from`
    /// instance to record against. The denominator for `call_sites`.
    pub sites_attempted: usize,
    /// Instances [`detect_changing_instance_recursion`] actually searched FROM.
    ///
    /// Published for the same reason `sites_attempted` is: the pass is a
    /// whole-graph search whose cost is invisible from its own output, which is
    /// EMPTY on every program without polymorphic recursion -- so a
    /// rejection-output test passes whether the search is bounded or
    /// exhaustive. This is an OPERATIONAL counter, describing work performed
    /// rather than the graph produced, so it belongs in a census assertion and
    /// never in a comparison of two graphs.
    pub recursion_starts: usize,
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
/// The condition uses the same `skull_mir::lower::ContainerPlan` as lowering.
/// The names are the column-path ones only -- a non-column
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
    ("map_new", StorageCond::MapColumn, &["__map_col_new_spine"]),
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
    "__map_col_new_spine",
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

/// Why a generic emission root carries no uncovered bare-name obligation, or
/// `None` when it does.
///
/// The obligation is "exist under a BARE name that opaque target text can call"
/// (`docs/boundary-contracts.md` 1.2, 1.4), so it is a property of the EMITTED
/// SYMBOL, not of genericity. Two things discharge it, and both are enforced
/// rather than promised:
///
/// - **mangled**: under ALWAYS-MANGLE the emitted symbol is
///   `name_param_ret_result`, which no opaque body can spell -- a Lua string
///   naming it would have to restate a mangling the compiler owns, and a
///   mangling change would break it silently. The obligation exists only for the
///   [`skull_mir::mangle::is_exempt`] set, which is exactly the set of
///   declarations whose emitted symbol IS their source name.
/// - **stdlib-funcref**: the [`ERASED_RUNTIME_ENTRIES`] catalogue is reached
///   through `Builder::stdlib_funcref`, which REJECTS at compile time when the
///   stdlib has no body serving the active target. Existence-per-target is the
///   contract, and it is checked, not asserted (1.4).
///
/// The catalogue is a set of NAMES, and a name is not a declaration. Membership
/// alone is a PROXY for "this is the entry the compiler resolves to", and a
/// weaker question than the one being asked: a user `@nomangle` generic sharing
/// a catalogue name is not that entry, and excusing it would falsely close a
/// program. So the excusal asks the same resolver `stdlib_funcref` does
/// ([`stdlib_entry_representative`]) and excuses only the declaration it
/// selects. When that resolver is ambiguous -- exactly what `stdlib_funcref`
/// itself rejects on -- nothing is excused and the obligation stands.
///
/// Deliberately NOT a discharge: having concrete instances from Rock call
/// sites. Those cover the call sites and not the bare-name entry, which is the
/// falsely-green reading [`RootHole`] exists to prevent.
fn generic_root_obligation(ws: &Workspace, r: FuncRef, f: &Function) -> Option<&'static str> {
    if !skull_mir::mangle::is_exempt(f) {
        return Some("mangled");
    }
    // `r` is already canonicalized by `build`, so this compares like for like.
    if ERASED_RUNTIME_ENTRIES.contains(&f.name.as_str())
        && stdlib_entry_representative(ws, f.name.as_str()) == Some(r)
    {
        return Some("stdlib-funcref");
    }
    None
}

/// Re-exported from [`skull_mir::monoty`], which owns the decision so the graph
/// and both emitters cannot drift. See `too_deep_to_instantiate` there.
pub use skull_mir::monoty::MAX_INSTANTIATION_DEPTH;

/// Whether an instance's substitution makes it a CONCRETE shape -- the one
/// question every declaration-based pass has to ask before it may answer.
///
/// Three answers, not two. `unwrap_or(true)` on a missing record collapsed
/// [`Self::Missing`] into [`Self::Concrete`], so an instance whose substitution
/// the walk never recorded was handed to a declaration-based oracle as if the
/// declaration WERE its shape. `reach` records a substitution for every
/// instance it reaches -- roots included, whose substitution is EMPTY and not
/// absent -- so a missing record is an INVARIANT BREAK, and the honest answer
/// to "is this concrete" is that nothing knows.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Concreteness {
    /// The declaration IS the shape: no logical arguments and an empty recorded
    /// substitution. Declaration-based passes are exact here and nowhere else.
    Concrete,
    /// A real substitution the declaration does not carry.
    Generic,
    /// No substitution record at all. Never `Concrete`.
    Missing,
}

impl Concreteness {
    pub fn label(self) -> &'static str {
        match self {
            Self::Concrete => "concrete",
            Self::Generic => "generic",
            Self::Missing => "missing-substitution",
        }
    }
}

impl InstanceGraph {
    /// **The one substitution oracle.** Both the publisher-ABI lanes and the
    /// GLSL artifact passes read this, so the two cannot classify one instance
    /// differently.
    pub fn concreteness(&self, i: &Instance) -> Concreteness {
        match self.substitution(i) {
            None => Concreteness::Missing,
            Some(s) if s.is_empty() && i.args.is_empty() => Concreteness::Concrete,
            Some(_) => Concreteness::Generic,
        }
    }

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
        let args: Vec<String> = i.args.iter().map(|a| self.interner.render(*a)).collect();
        format!("{base}<{}>", args.join(", "))
    }

    /// The instance's IDENTITY: the mangled name plus its arguments' canonical
    /// encodings. Unambiguous, so sorting and dedup on it cannot merge two
    /// distinct instances.
    pub fn identity(&self, ws: &Workspace, i: &Instance) -> String {
        let f = &ws.module(i.func.module).functions[i.func.func];
        let base = mangle_fn(ws, f);
        // The DECLARATION, not just its mangled name. Two `@nomangle` backend
        // siblings mangle identically -- `__nb_checked_index` is declared
        // `inline(lua)` in `unified.rock` and `inline(js)` in `js.rock` -- and
        // both are emission roots, so both are instances. Keying on the name
        // alone MERGED them: measured at 13 of 96 instances in a one-line
        // fixture, and `manifest_lines` DEDUPLICATES on this string, so the
        // manifest was one line short per colliding pair.
        //
        // `(module, function index)` is stable within a build and deterministic
        // across builds (module ids come from source order), which is the same
        // contract [`ord_key`] states. The readable form is [`Self::symbol`];
        // the manifest carries both, precisely so this one can be unambiguous
        // rather than pretty.
        let decl = DeclKey(i.func).raw();
        if i.args.is_empty() {
            return format!("{base}@{}.{}", decl.0, decl.1);
        }
        let args: Vec<String> = i.args.iter().map(|a| self.interner.canonical(*a)).collect();
        format!("{base}@{}.{}<{}>", decl.0, decl.1, args.join(","))
    }

    /// Instances that share an [`Self::identity`], and -- for contrast -- the
    /// ones that would share the NAME-KEYED identity it replaced.
    ///
    /// The first is 0 by construction now, which is exactly why the second is
    /// reported beside it: a zero whose only content is "the key includes the
    /// declaration" says nothing about how much the old key was merging.
    pub fn colliding_identities(&self, ws: &Workspace) -> (usize, usize) {
        let mut by_id: BTreeMap<String, usize> = BTreeMap::new();
        let mut by_name: BTreeMap<String, usize> = BTreeMap::new();
        for i in &self.instances {
            *by_id.entry(self.identity(ws, i)).or_insert(0) += 1;
            let f = &ws.module(i.func.module).functions[i.func.func];
            let args: Vec<String> = i.args.iter().map(|a| self.interner.canonical(*a)).collect();
            *by_name.entry(format!("{}<{}>", mangle_fn(ws, f), args.join(","))).or_insert(0) += 1;
        }
        let merged = |m: BTreeMap<String, usize>| -> usize {
            m.values().filter(|n| **n > 1).map(|n| n - 1).sum()
        };
        (merged(by_id), merged(by_name))
    }

    /// The instance's LOGICAL symbol.
    ///
    /// DELEGATES to [`Instance::symbol`] -- it does not restate it. The two
    /// stood side by side, each reaching `spec_fn_name` with its own
    /// declaration lookup and its own non-generic special case, which is two
    /// derivations of one encoder and exactly the shape that drifts. Emission
    /// spells a definition with `Instance::symbol`; a graph view that answered
    /// differently would be describing a body nothing emits.
    ///
    /// **This is not the final emitted spelling.** Plan S 0h puts that under the
    /// ARTIFACT's ABI/boundary plan -- one symbol per `(artifact, instance)` --
    /// so a target may legally spell this instance differently. What is shared
    /// here is the ENCODER, and per-artifact injectivity stays a property of
    /// each artifact view.
    pub fn logical_symbol(&self, ws: &Workspace, i: &Instance) -> String {
        i.symbol(ws, &self.interner)
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
            .filter(|i| !self.is_expanded(i))
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
            depth_capped: self.depth_capped().map(|i| self.identity(ws, i)).collect(),
            // Every hole, whatever its cause. A root WITH instances is still a
            // hole: the instances discharge its Rock call sites, never the
            // bare-name obligation that made it a root.
            uninstantiated_roots: self.uninstantiated_roots.keys().cloned().collect(),
            colliding_symbols: colliding,
            expanded: self.expanded().count(),
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

    /// Instances with no recorded substitution. Must be empty: a non-generic
    /// instance's substitution is EMPTY, not absent, and a `ConcreteBody`
    /// cannot be built for a body whose substitution the walk never kept.
    pub fn instances_without_substitution(&self, ws: &Workspace) -> BTreeSet<String> {
        self.instances
            .iter()
            .filter(|i| self.substitution(i).is_none())
            .map(|i| self.identity(ws, i))
            .collect()
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
        for e in self.edges() {
            let k = c.edges.entry(e.kind.name()).or_default();
            k.examined += 1;
            k.spanless += usize::from(is_spanless(&e.span));
        }
        for r in self.rejections() {
            let k = c.rejections.entry(r.kind.name()).or_default();
            k.examined += 1;
            k.spanless += usize::from(is_spanless(&r.span));
        }
        for u in self.unresolved() {
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
        for e in self.edges() {
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
        for e in self.edges() {
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
                .rejections()
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

/// Build the ONE shared instance graph for `ws`.
///
/// **Target-neutral, and there is no per-target builder.** Every callee is
/// recorded as bone's collapsed GROUP REPRESENTATIVE -- which is what
/// `resolved_overload_calls` already holds -- and roots, which production hands
/// over as raw declarations, are canonicalized to the same representative
/// (`skull_mir::lower::canonical_variant`). Selecting the sibling that serves a
/// target is `artifact::realize`'s job, so one graph and one [`MonoArena`] serve
/// every artifact and cross-artifact identity is the shared [`Instance`] rather
/// than a comparison between two arenas.
///
/// `roots` is supplied by the caller so this shares production's root
/// discovery instead of re-deriving it. `perturb` reverses the seeding order;
/// the manifest must be identical either way (plan S 20g.4).
pub fn build(ws: &Workspace, roots: Vec<FuncRef>, perturb: bool) -> InstanceGraph {
    build_in(ws, roots, perturb, std::sync::Arc::new(MonoInterner::new()))
}

/// [`build`] against an arena the caller already owns.
///
/// THE entry production uses: `emit_full_in` retains both the graph and the
/// `ScalarCtx` that lowers against it, and they must share one arena or an
/// `Instance` minted by the graph means nothing to emission.
pub fn build_in(
    ws: &Workspace,
    roots: Vec<FuncRef>,
    perturb: bool,
    interner: std::sync::Arc<MonoInterner>,
) -> InstanceGraph {
    let t_build = std::time::Instant::now();
    let intern_before = interner.stats();
    let mut g = InstanceGraph::empty_in(interner);
    let t = std::time::Instant::now();
    g.seed_roots(ws, canonical_roots(ws, roots, perturb));
    let pending = g.always_emitted_owners(ws);
    let t_seed = t.elapsed();
    let t = std::time::Instant::now();
    let stats = g.run(ws, pending);
    g.run_stats = stats;
    let t_run = t.elapsed();
    let t = std::time::Instant::now();
    g.derive(ws, false);
    let t_derive = t.elapsed();
    if log::log_enabled!(log::Level::Debug) {
        let intern_after = g.interner().stats();
        let sites: usize = g.contributions.values().map(|c| c.call_sites.len()).sum();
        log::debug!(
            "graph build: seed {:?}, run {:?} ({} owner walks, {} settle rounds), \
             derive {:?}, total {:?}; {} instances, {} contributions, {} call sites, \
             {} rejections; mono interns {} attempts -> {} new during build",
            t_seed,
            t_run,
            stats.expanded,
            stats.settle_rounds,
            t_derive,
            t_build.elapsed(),
            g.instances.len(),
            g.contributions.len(),
            sites,
            g.rejections().count(),
            intern_after.intern_attempts - intern_before.intern_attempts,
            intern_after.intern_new - intern_before.intern_new,
        );
        log::debug!(
            "graph census: first {} ({:.3}ms), duplicate-pending {} ({:.3}ms), \
             suppressed {} duplicate pending owners, \
             witness-settlement {} ({:.3}ms), incremental-forced {} ({:.3}ms), \
             stranded-skipped {}; phases admissibility {:.3}ms, body-prep {:.3}ms, \
             signature/type {:.3}ms, call-resolution {:.3}ms, witness/bound {:.3}ms, \
             body-walk {:.3}ms, struct-default-walk {:.3}ms, uninstall {:.3}ms, \
             index-maintenance {:.3}ms [subst {:.3}, trees {:.3}, erased {:.3}, \
             always {:.3}, preds {:.3}, decls {:.3}, commit {:.3}]",
            stats.first_owner_visits,
            stats.first_owner_ns as f64 / 1_000_000.0,
            stats.duplicate_pending_visits,
            stats.duplicate_pending_ns as f64 / 1_000_000.0,
            stats.duplicate_pending_suppressed,
            stats.witness_settlement_rewalks,
            stats.witness_settlement_ns as f64 / 1_000_000.0,
            stats.incremental_forced_rewalks,
            stats.incremental_forced_ns as f64 / 1_000_000.0,
            stats.skipped_stranded_owners,
            stats.admissibility_ns as f64 / 1_000_000.0,
            stats.body_prep_ns as f64 / 1_000_000.0,
            stats.signature_type_conversion_ns as f64 / 1_000_000.0,
            stats.call_site_resolution_ns as f64 / 1_000_000.0,
            stats.witness_bound_tree_ns as f64 / 1_000_000.0,
            stats.walk_body_edges_ns as f64 / 1_000_000.0,
            stats.walk_struct_default_edges_ns as f64 / 1_000_000.0,
            stats.uninstall_ns as f64 / 1_000_000.0,
            stats.index_maintenance_ns as f64 / 1_000_000.0,
            stats.index_subst_ns as f64 / 1_000_000.0,
            stats.index_trees_ns as f64 / 1_000_000.0,
            stats.index_erased_ns as f64 / 1_000_000.0,
            stats.index_always_emitted_ns as f64 / 1_000_000.0,
            stats.index_preds_ns as f64 / 1_000_000.0,
            stats.index_decls_ns as f64 / 1_000_000.0,
            stats.index_commit_ns as f64 / 1_000_000.0,
        );
        log::debug!(
            "graph body-fact census: expression rows {}, overload rows {}, fn-value rows {}, \
             generated rows {}, dispatched rows {}, implicit-iter rows {}, bound-tree rows {}, \
             bound-call rows {}, iterator rows {}",
            stats.body_exprs_scanned,
            stats.resolved_overload_rows,
            stats.fn_value_rows,
            stats.generated_rows,
            stats.dispatched_rows,
            stats.implicit_iter_rows,
            stats.bound_tree_rows,
            stats.bound_call_rows,
            stats.iterator_rows,
        );
        log::debug!(
            "graph wavefront census: {} frontiers, owners p50/p95/max {}/{}/{}, singleton {}; \
             ordinary {}, always {}, ready {}, same-frontier dependency {}; expansion us total/p50/p95/max \
             {}/{}/{}/{}; mono attempts/new/read/write {}/{}/{}/{}",
            stats.frontier_count,
            stats.frontier_owner_p50,
            stats.frontier_owner_p95,
            stats.frontier_owner_max,
            stats.singleton_frontiers,
            stats.ordinary_owner_visits,
            stats.always_emitted_owner_visits,
            stats.frontier_ready_owners,
            stats.frontier_same_frontier_dependency_owners,
            stats.expansion_ns_total / 1_000,
            stats.expansion_ns_p50 / 1_000,
            stats.expansion_ns_p95 / 1_000,
            stats.expansion_ns_max / 1_000,
            stats.mono_intern_attempts,
            stats.mono_new_types,
            stats.mono_read_acqs,
            stats.mono_write_acqs,
        );
        log::debug!(
            "graph contribution/index census: reached {}, bound trees {}, edges {}, sites {}, \
             struct-default domains {}, rejections {}, synthesized examined {}, failures {}, \
             always targets {}; unique index keys subst/trees/erased/always/preds/decls \
             {}/{}/{}/{}/{}/{}; phases uninstall/index/derive/post-pass {:.3}/{:.3}/{:.3}/{:.3}ms",
            stats.contribution_reached,
            stats.contribution_bound_trees,
            stats.contribution_edges,
            stats.contribution_sites,
            stats.contribution_struct_default_domains,
            stats.contribution_rejections,
            stats.contribution_synthesized_examined,
            stats.contribution_failures,
            stats.contribution_always_emitted_targets,
            stats.index_subst_unique,
            stats.index_trees_unique,
            stats.index_erased_unique,
            stats.index_always_emitted_unique,
            stats.index_preds_unique,
            stats.index_decls_unique,
            stats.uninstall_ns as f64 / 1_000_000.0,
            stats.index_maintenance_ns as f64 / 1_000_000.0,
            stats.derive_ns as f64 / 1_000_000.0,
            stats.post_pass_ns as f64 / 1_000_000.0,
        );
    }
    g
}

/// Production's roots as the graph keys them: bone's collapsed representative,
/// deduplicated and ordered. `perturb` reverses the seeding order; the graph
/// must mean the same thing either way (plan S 20g.4).
fn canonical_roots(ws: &Workspace, roots: Vec<FuncRef>, perturb: bool) -> Vec<FuncRef> {
    let mut roots: Vec<FuncRef> =
        roots.into_iter().map(|r| skull_mir::lower::canonical_variant(ws, r)).collect();
    roots.sort_by_key(|r| (r.module.raw(), r.func.into_raw().into_u32()));
    roots.dedup();
    if perturb {
        roots.reverse();
    }
    roots
}

impl InstanceGraph {
    /// Every LIVE instance of one declaration.
    ///
    /// A scan of the instance set rather than an index, deliberately: the only
    /// caller asks it once per CHANGED declaration on a body edit (a handful),
    /// and a maintained `DeclKey -> {Instance}` map would be a third reverse
    /// index to keep in step with `preds` and `decls` for no measured gain.
    pub fn instances_of(&self, fref: FuncRef) -> impl Iterator<Item = &Instance> {
        self.instances.iter().filter(move |i| i.func == fref)
    }

    /// **The arena this graph's `MonoTyId`s index**, as the shared handle.
    ///
    /// Handed to `ScalarCtx` so emission lowers against the same ids the graph
    /// minted. Cloning the `Arc` shares the arena; there is deliberately no way
    /// to get an owned `MonoArena` out, because a copy is a second identity.
    pub fn interner(&self) -> &std::sync::Arc<MonoInterner> {
        &self.interner
    }

    fn empty_in(interner: std::sync::Arc<MonoInterner>) -> Self {
        InstanceGraph {
        interner,
        contributions: BTreeMap::new(),
        lost: BTreeSet::new(),
        root_instances: BTreeSet::new(),
            instances: BTreeSet::new(),
            run_stats: RunStats::default(),
        index: Indices::default(),
        graph_rejections: Vec::new(),
        uninstantiated_roots: BTreeMap::new(),
        excused_generic_roots: BTreeMap::new(),
        boundaries: BTreeMap::new(),
        coroutine_helpers: BTreeSet::new(),
        erased_entries: BTreeSet::new(),
        inline_body_deps: BTreeMap::new(),
        inline_body_nomangle_roots: BTreeMap::new(),
        synthesized_examined: 0,
        synthesized_approximated: 0,
        always_emitted_seeds: 0,
        sites_attempted: 0,
        recursion_starts: 0,
        }
    }

    /// The instances production roots regardless of any call, and the generic
    /// roots no instantiation can discharge.
    ///
    /// Interface-tier state: it is a function of the ROOT SET, so a body edit
    /// leaves it alone and re-seeding it is cheap enough to do unconditionally
    /// rather than keep a second answer about when it moved.
    fn seed_roots(&mut self, ws: &Workspace, roots: Vec<FuncRef>) {
        self.root_instances.clear();
        self.uninstantiated_roots.clear();
        self.excused_generic_roots.clear();
        // A root that is generic has no instantiation of its own; it is a
        // shared body until a call supplies one. Recorded rather than invented.
        for r in roots {
            let f = &ws.module(r.module).functions[r.func];
            if !f.generics.is_empty() {
                match generic_root_obligation(ws, r, f) {
                    Some(reason) => {
                        self.excused_generic_roots
                            .entry(reason)
                            .or_default()
                            .insert(mangle_fn(ws, f));
                    }
                    None => {
                        self.uninstantiated_roots
                            .entry(mangle_fn(ws, f))
                            .or_default()
                            .generic_root = true;
                    }
                }
                continue;
            }
            self.root_instances.insert(Instance { func: r, args: Vec::new() });
        }
    }

    /// Globals and decorators have no `Instance` origin, so their bodies are
    /// owners in their own right.
    fn always_emitted_owners(&self, ws: &Workspace) -> Vec<Owner> {
        let mut out = Vec::new();
        for module in ws.modules.iter().filter(|m| m.id != ModuleId::builtin()) {
            let Some(inference) = ws.inference(module.id) else { continue };
            let mut owners: Vec<BodyOwner> = module
                .bodies
                .keys()
                .filter(|o| matches!(o, BodyOwner::Decorator(_) | BodyOwner::Global(_)))
                .copied()
                .collect();
            owners.sort_by_key(|o| body_owner_key(*o));
            for owner in owners {
                if inference.per_fn.contains_key(&owner) {
                    out.push(Owner::AlwaysEmitted(module.id, owner));
                }
            }
        }
        out
    }

    /// Expand owners until nothing new is reached.
    ///
    /// `pending` is the set to (re-)walk unconditionally; everything they reach
    /// that has no contribution yet joins them. A full build hands it the
    /// always-emitted bodies and lets the roots come through
    /// [`Self::merge_reached`]; an update hands it the owners whose bodies
    /// changed. One driver, so the two cannot walk differently.
    fn run(&mut self, ws: &Workspace, mut pending: Vec<Owner>) -> RunStats {
        let mut stats = RunStats::default();
        let mono_before = self.interner.stats();
        for i in self.root_instances.clone() {
            if !self.contributions.contains_key(&Owner::Instance(i.clone())) {
                pending.push(Owner::Instance(i));
            }
        }
        let timing_enabled = graph_timing_enabled();
        stats.census_enabled = timing_enabled;
        let mut queued = BTreeSet::new();
        let mut unique_pending = Vec::with_capacity(pending.len());
        for owner in pending {
            if queued.insert(owner.clone()) {
                unique_pending.push(owner);
            } else {
                stats.duplicate_pending_suppressed += 1;
            }
        }
        pending = unique_pending;
        let mut frontier_sizes = if timing_enabled && !pending.is_empty() {
            vec![pending.len()]
        } else {
            Vec::new()
        };
        let mut frontier_depths: BTreeMap<Owner, usize> = if timing_enabled {
            pending
                .iter()
                .cloned()
                .map(|owner| (owner, 0))
                .collect()
        } else {
            BTreeMap::new()
        };
        let mut same_frontier_marks = BTreeSet::new();
        let initially_forced: BTreeSet<Owner> = pending
            .iter()
            .filter(|owner| self.contributions.contains_key(*owner))
            .cloned()
            .collect();
        let mut first_pass_seen = BTreeSet::new();
        // A conformance tree is supplied by an owner's CALLER, so an owner
        // walked before its caller was re-walked read a stale one. Recorded
        // here and re-checked below rather than assumed away by walk order.
        let mut walked_with: BTreeMap<Instance, InstanceBoundTree> = BTreeMap::new();
        let mut rounds = 0usize;
        let mut expansion_times = Vec::new();
        loop {
            while let Some(owner) = pending.pop() {
                let current_frontier = timing_enabled
                    .then(|| frontier_depths.get(&owner).copied().unwrap_or(0))
                    .unwrap_or(0);
                queued.remove(&owner);
                stats.owner_visits += 1;
                if timing_enabled {
                    match owner {
                        Owner::Instance(_) => stats.ordinary_owner_visits += 1,
                        Owner::AlwaysEmitted(..) => stats.always_emitted_owner_visits += 1,
                    }
                }
                // **An instance that lost its last namer DURING this run is not
                // walkable, and is not an invariant break.**
                //
                // The prune runs in `derive`, AFTER this loop, so a stranded
                // instance can still be sitting in `pending` -- an edit
                // invalidates every owner that QUOTES the edited file, which in
                // a single-file program is every owner, including ones the same
                // edit is about to strand. Its substitution was retracted by its
                // caller's re-walk a moment ago, so `expand` would find none and
                // the invariant assertion would fire on a body that is simply
                // gone.
                //
                // Skipping leaves it with its old contribution, which `derive`
                // then prunes: membership is root reachability, and nothing
                // points at it any more. Found by driving a real body edit
                // through `update_chunk_cache` -- the graph fixtures live in a
                // multi-file project, where the stranded owner does not quote
                // the edited file and never reaches this loop.
                if let Owner::Instance(i) = &owner {
                    if self.substitution(i).is_none() {
                        if timing_enabled {
                            same_frontier_marks.remove(&owner);
                        }
                        stats.skipped_stranded_owners += 1;
                        continue;
                    }
                    walked_with.insert(i.clone(), self.bound_tree(i).cloned().unwrap_or_default());
                }
                let visit_kind = if rounds > 0 {
                    VisitKind::WitnessSettlement
                } else if !first_pass_seen.insert(owner.clone()) {
                    VisitKind::DuplicatePending
                } else if initially_forced.contains(&owner) {
                    VisitKind::IncrementalForced
                } else {
                    VisitKind::FirstOwner
                };
                let visit_start = timing_enabled.then(Instant::now);
                let c = self.expand(ws, &owner, timing_enabled);
                let visit_ns = elapsed_ns(visit_start);
                if timing_enabled {
                    expansion_times.push(visit_ns);
                }
                stats.expanded += 1;
                if timing_enabled {
                    if same_frontier_marks.remove(&owner) {
                        stats.frontier_same_frontier_dependency_owners += 1;
                    } else {
                        stats.frontier_ready_owners += 1;
                    }
                }
                match visit_kind {
                    VisitKind::FirstOwner => {
                        stats.first_owner_visits += 1;
                        stats.first_owner_ns += visit_ns;
                    }
                    VisitKind::DuplicatePending => {
                        stats.duplicate_pending_visits += 1;
                        stats.duplicate_pending_ns += visit_ns;
                    }
                    VisitKind::WitnessSettlement => {
                        stats.witness_settlement_rewalks += 1;
                        stats.witness_settlement_ns += visit_ns;
                    }
                    VisitKind::IncrementalForced => {
                        stats.incremental_forced_rewalks += 1;
                        stats.incremental_forced_ns += visit_ns;
                    }
                }
                stats.admissibility_ns += c.timing.admissibility_ns;
                stats.body_prep_ns += c.timing.body_prep_ns;
                stats.signature_type_conversion_ns += c.timing.signature_type_conversion_ns;
                stats.mono_elapsed_ns += c.timing.interner_ns;
                stats.call_site_resolution_ns += c.timing.call_site_resolution_ns;
                stats.witness_bound_tree_ns += c.timing.witness_bound_tree_ns;
                stats.walk_body_edges_ns += c.timing.walk_body_edges_ns;
                stats.walk_struct_default_edges_ns += c.timing.walk_struct_default_edges_ns;
                stats.body_exprs_scanned += c.body_exprs_scanned;
                stats.resolved_overload_rows += c.resolved_overload_rows;
                stats.fn_value_rows += c.fn_value_rows;
                stats.generated_rows += c.generated_rows;
                stats.dispatched_rows += c.dispatched_rows;
                stats.implicit_iter_rows += c.implicit_iter_rows;
                stats.bound_tree_rows += c.bound_tree_rows;
                stats.bound_call_rows += c.bound_call_rows;
                stats.iterator_rows += c.iterator_rows;
                if timing_enabled {
                    stats.contribution_reached += c.reached.len();
                    stats.contribution_bound_trees += c.bound_trees.len();
                    stats.contribution_edges += c.edges.len();
                    stats.contribution_sites += c.call_sites.len() + c.always_call_sites.len();
                    stats.contribution_struct_default_domains += c.struct_default_domains.len();
                    stats.contribution_rejections += c.rejections.len();
                    stats.contribution_synthesized_examined += c.synthesized_examined;
                    stats.contribution_failures += c.rejections.len()
                        + c.unresolved.len()
                        + c.storage_failures.len()
                        + c.always_emitted_holes.len();
                    stats.contribution_always_emitted_targets += c.always_emitted_targets.len();
                }
                let (demanded, install_timing) = self.install(owner, c, timing_enabled);
                stats.uninstall_ns += install_timing.uninstall_ns;
                stats.index_maintenance_ns += install_timing.index.total_ns();
                stats.index_subst_ns += install_timing.index.subst_ns;
                stats.index_trees_ns += install_timing.index.trees_ns;
                stats.index_erased_ns += install_timing.index.erased_ns;
                stats.index_always_emitted_ns += install_timing.index.always_emitted_ns;
                stats.index_preds_ns += install_timing.index.preds_ns;
                stats.index_decls_ns += install_timing.index.decls_ns;
                stats.index_commit_ns += install_timing.index.commit_ns;
                stats.index_subst_updates += install_timing.index.subst_updates;
                stats.index_trees_updates += install_timing.index.trees_updates;
                stats.index_erased_updates += install_timing.index.erased_updates;
                stats.index_always_emitted_updates += install_timing.index.always_emitted_updates;
                stats.index_preds_updates += install_timing.index.preds_updates;
                stats.index_decls_updates += install_timing.index.decls_updates;
                for owner in demanded {
                    if timing_enabled
                        && queued.contains(&owner)
                        && frontier_depths.get(&owner) == Some(&current_frontier)
                    {
                        same_frontier_marks.insert(owner.clone());
                    }
                    if queued.insert(owner.clone()) {
                        if timing_enabled {
                            let next_frontier = current_frontier.saturating_add(1);
                            frontier_depths.insert(owner.clone(), next_frontier);
                            if frontier_sizes.len() <= next_frontier {
                                frontier_sizes.resize(next_frontier + 1, 0);
                            }
                            frontier_sizes[next_frontier] += 1;
                        }
                        pending.push(owner);
                    } else {
                        stats.duplicate_pending_suppressed += 1;
                    }
                }
            }
            // The fixpoint: an owner whose witnesses moved while it was already
            // walked must be walked again, or its callees are the ones the old
            // witnesses selected.
            let stale: Vec<Owner> = walked_with
                .iter()
                .filter(|(i, used)| {
                    self.contributions.contains_key(&Owner::Instance((*i).clone()))
                        && self.bound_tree(i).unwrap_or(&InstanceBoundTree::default()) != *used
                })
                .map(|(i, _)| Owner::Instance(i.clone()))
                .collect();
            if stale.is_empty() {
                break;
            }
            rounds += 1;
            stats.settle_rounds = rounds;
            assert!(
                rounds < 64,
                "conformance witnesses did not settle after {rounds} rounds; \
                 {} owners still disagree with what they were walked with",
                stale.len()
            );
            if timing_enabled {
                let settlement_frontier = frontier_sizes.len();
                frontier_sizes.push(stale.len());
                for owner in &stale {
                    frontier_depths.insert(owner.clone(), settlement_frontier);
                }
            }
            pending = stale;
            queued = pending.iter().cloned().collect();
        }
        assert_eq!(
            stats.owner_visits,
            stats.first_owner_visits
                + stats.duplicate_pending_visits
                + stats.witness_settlement_rewalks
                + stats.incremental_forced_rewalks
                + stats.skipped_stranded_owners,
            "graph census owner-visit denominator lost a population"
        );
        assert_eq!(
            stats.expanded,
            stats.first_owner_visits
                + stats.duplicate_pending_visits
                + stats.witness_settlement_rewalks
                + stats.incremental_forced_rewalks,
            "graph census expanded denominator lost a population"
        );
        let mono_after = self.interner.stats();
        stats.mono_intern_attempts = mono_after
            .intern_attempts
            .saturating_sub(mono_before.intern_attempts);
        stats.mono_new_types = mono_after.intern_new.saturating_sub(mono_before.intern_new);
        stats.mono_read_acqs = mono_after.read_acqs.saturating_sub(mono_before.read_acqs);
        stats.mono_write_acqs = mono_after.write_acqs.saturating_sub(mono_before.write_acqs);
        if timing_enabled {
            stats.frontier_count = frontier_sizes.len();
            stats.frontier_owner_p50 = percentile_usize(&mut frontier_sizes, 50);
            stats.frontier_owner_p95 = percentile_usize(&mut frontier_sizes, 95);
            stats.frontier_owner_max = frontier_sizes.iter().copied().max().unwrap_or(0);
            stats.singleton_frontiers = frontier_sizes.iter().filter(|n| **n == 1).count();
            stats.expansion_ns_total = expansion_times.iter().sum();
            stats.expansion_ns_p50 = percentile_u64(&mut expansion_times, 50);
            stats.expansion_ns_p95 = percentile_u64(&mut expansion_times, 95);
            stats.expansion_ns_max = expansion_times.iter().copied().max().unwrap_or(0);
            stats.mono_elapsed_ns = stats.mono_elapsed_ns.max(stats.expansion_ns_total);
            stats.index_subst_unique = self.index.subst.len();
            stats.index_trees_unique = self.index.trees.len();
            stats.index_erased_unique = self.index.erased.len();
            stats.index_always_emitted_unique = self.index.always_emitted.len();
            stats.index_preds_unique = self.index.preds.len();
            stats.index_decls_unique = self.index.decls.len();
        }
        stats
    }

    /// Walk ONE owner's body. The only producer of a [`Contribution`], so a
    /// full build and an incremental re-walk cannot record different facts.
    fn expand(&mut self, ws: &Workspace, owner: &Owner, timing_enabled: bool) -> Contribution {
        let mut out = Contribution::default();
        match owner {
            Owner::AlwaysEmitted(module, body_owner) => {
                let prep_start = timing_enabled.then(Instant::now);
                let Some(inf) =
                    ws.inference(*module).and_then(|ir| ir.per_fn.get(body_owner))
                else {
                    add_elapsed(&mut out.timing.body_prep_ns, prep_start);
                    return out;
                };
                let body = ws.module(*module).bodies.get(body_owner);
                add_elapsed(&mut out.timing.body_prep_ns, prep_start);
                let mut w = Walk {
                    interner: &self.interner,
                    owner_tree: None,
                    out: &mut out,
                    timing_enabled,
                };
                let walk_start = w.timer();
                walk_body_edges(
                    &mut w, ws, inf, body, None, &MonoSubst::default(), &SiteDomain::Own, None,
                );
                add_elapsed(&mut w.out.timing.walk_body_edges_ns, walk_start);
                // An always-emitted body's struct literals omit fields too --
                // `global POLLER: CoroutinePoller = CoroutinePoller {}` reaches
                // `coroutines: Arena<CoState> = arena_new()` exactly as an
                // ordinary instance's literal would. `walk_struct_default_edges`
                // was instance-only, so a struct default reached ONLY from a
                // global initializer or a decorator body was invisible to the
                // graph while emission (which lowers the same literal) still
                // built and emitted it -- found by the Phase-2 population audit,
                // identically on every corpus project (`arena_new<CoState>` and
                // its own `__arena_new_owned<int>` call).
                if let Some(body) = body {
                    let defaults_start = w.timer();
                    walk_struct_default_edges(&mut w, ws, *module, body, *body_owner, None, &MonoSubst::default());
                    add_elapsed(&mut w.out.timing.walk_struct_default_edges_ns, defaults_start);
                }
            }
            Owner::Instance(inst) => {
                // The one admissibility oracle, shared with both emitters. The
                // graph DECLINES to expand where they reject: a capped instance
                // is a changing-instance-recursion candidate, not yet a verdict.
                let admissibility_start = timing_enabled.then(Instant::now);
                let too_deep = skull_mir::monoty::too_deep_to_instantiate(
                    inst.args.iter().map(|a| self.interner.get(*a)).collect::<Vec<_>>().iter(),
                )
                .is_some();
                if too_deep {
                    add_elapsed(&mut out.timing.admissibility_ns, admissibility_start);
                    out.depth_capped = true;
                    return out;
                }
                let f = &ws.module(inst.func.module).functions[inst.func.func];
                let is_boundary = declared_boundary(f).is_some();
                add_elapsed(&mut out.timing.admissibility_ns, admissibility_start);
                if is_boundary {
                    return out;
                }
                let prep_start = timing_enabled.then(Instant::now);
                let FunctionBody::Ready { inf, body } = function_body(ws, inst.func) else {
                    add_elapsed(&mut out.timing.body_prep_ns, prep_start);
                    return out;
                };
                // A boundary's Rock body is not the one emission uses -- it
                // emits the foreign text -- so the walk STOPS here. Marking it
                // unexpanded while continuing to enumerate its calls was the
                // worst of both: the closure gate excused the instance as having
                // no callees while the graph carried them, and a
                // `@native_required` fallback body reached callees emission
                // never routes through.
                out.expanded = true;
                // A body's own substitution is the one its caller reached it
                // under, which the contribution set already answers. An
                // instance with no record is an invariant break, not a body to
                // walk with an empty one.
                let subst = self
                    .substitution(inst)
                    .cloned()
                    .expect("every reached instance has a recorded substitution");
                let owner_tree = self.bound_tree(inst).cloned().unwrap_or_default();
                add_elapsed(&mut out.timing.body_prep_ns, prep_start);
                let mut w = Walk {
                    interner: &self.interner,
                    owner_tree: Some(&owner_tree),
                    out: &mut out,
                    timing_enabled,
                };
                let walk_start = w.timer();
                walk_body_edges(
                    &mut w, ws, inf, Some(body), Some(inst), &subst, &SiteDomain::Own, None,
                );
                add_elapsed(&mut w.out.timing.walk_body_edges_ns, walk_start);
                let defaults_start = w.timer();
                walk_struct_default_edges(
                    &mut w,
                    ws,
                    inst.func.module,
                    body,
                    BodyOwner::Function(inst.func.func),
                    Some(inst),
                    &subst,
                );
                add_elapsed(&mut w.out.timing.walk_struct_default_edges_ns, defaults_start);
            }
        }
        out
    }

    /// **Install a contribution, replacing whatever the owner had.**
    ///
    /// The ONE indexing path. A full build only ever installs; an update
    /// installs over an existing record and prunes through
    /// [`Self::uninstall`]. Both go through here, so an index cannot be
    /// maintained one way on one path and another way on the other -- which is
    /// what `derive` re-deriving the substitution maps behind `commit`'s back
    /// used to be.
    ///
    /// Returns the owners the contribution newly demands.
    fn install(
        &mut self,
        owner: Owner,
        c: Contribution,
        timing_enabled: bool,
    ) -> (Vec<Owner>, InstallTiming) {
        let uninstall_start = timing_enabled.then(Instant::now);
        self.uninstall(&owner);
        let uninstall_ns = elapsed_ns(uninstall_start);
        let mut index = IndexMaintenanceTiming::default();
        // Every callee is in `reached`, an always-emitted body's included --
        // `always_emitted_targets` marks which of them are ROOTS, it is not a
        // second population.
        let lane_start = timing_enabled.then(Instant::now);
        for (to, subst) in &c.reached {
            index.subst_updates += 1;
            match self.index.subst.get_mut(to) {
                Some(a) => a.assert_by(owner.clone(), subst),
                None => {
                    self.index.subst.insert(to.clone(), Agreed::new(owner.clone(), subst.clone()));
                }
            }
        }
        index.subst_ns = elapsed_ns(lane_start);
        let lane_start = timing_enabled.then(Instant::now);
        for (i, tree) in &c.bound_trees {
            index.trees_updates += 1;
            match self.index.trees.get_mut(i) {
                Some(a) => a.assert_by(owner.clone(), tree),
                None => {
                    self.index.trees.insert(i.clone(), Agreed::new(owner.clone(), tree.clone()));
                }
            }
        }
        index.trees_ns = elapsed_ns(lane_start);
        let lane_start = timing_enabled.then(Instant::now);
        for name in &c.erased_entries {
            index.erased_updates += 1;
            self.index.erased.entry(name.clone()).or_default().insert(owner.clone());
        }
        index.erased_ns = elapsed_ns(lane_start);
        let lane_start = timing_enabled.then(Instant::now);
        for to in &c.always_emitted_targets {
            index.always_emitted_updates += 1;
            self.index.always_emitted.entry(to.clone()).or_default().insert(owner.clone());
        }
        index.always_emitted_ns = elapsed_ns(lane_start);
        let lane_start = timing_enabled.then(Instant::now);
        if let Some(from) = owner.instance() {
            for e in &c.edges {
                index.preds_updates += 1;
                self.index.preds.entry(e.to.clone()).or_default().insert(from.clone());
                self.lost.remove(&e.to);
            }
        }
        index.preds_ns = elapsed_ns(lane_start);
        let lane_start = timing_enabled.then(Instant::now);
        if let Some(from) = owner.instance() {
            index.decls_updates += 1;
            *self.index.decls.entry(DeclKey(from.func)).or_insert(0) += 1;
        }
        index.decls_ns = elapsed_ns(lane_start);
        let lane_start = timing_enabled.then(Instant::now);
        for to in &c.always_emitted_targets {
            self.lost.remove(to);
        }
        // The demand check runs AFTER the insert. `install` uninstalls first,
        // so a body that reaches ITSELF -- any recursive function -- would
        // otherwise find itself absent at the moment it asked and re-walk
        // forever.
        let reached: Vec<Instance> = c.reached.iter().map(|(to, _)| to.clone()).collect();
        self.contributions.insert(owner, c);
        let mut demanded: Vec<Owner> = reached
            .into_iter()
            .map(Owner::Instance)
            .filter(|o| !self.contributions.contains_key(o))
            .collect();
        demanded.sort();
        demanded.dedup();
        index.commit_ns = elapsed_ns(lane_start);
        (
            demanded,
            InstallTiming { uninstall_ns, index },
        )
    }

    /// Remove a contribution and everything the indices held on its behalf.
    ///
    /// An index entry disappears when its last owner does, never when a count
    /// reaches zero: the owner SET is the record, so a stale fact cannot
    /// survive as a positive number nobody can check.
    fn uninstall(&mut self, owner: &Owner) {
        let Some(old) = self.contributions.remove(owner) else { return };
        // One owner can assert one fact several times, so the retraction is by
        // COUNT: dropping it once would leave a total that outlives its owner.
        let mut reach_times: BTreeMap<&Instance, usize> = BTreeMap::new();
        for (to, _) in &old.reached {
            *reach_times.entry(to).or_insert(0) += 1;
        }
        for (to, times) in reach_times {
            if let Some(a) = self.index.subst.get_mut(to) {
                if !a.retract(owner, times) {
                    self.index.subst.remove(to);
                }
            }
        }
        let mut tree_times: BTreeMap<&Instance, usize> = BTreeMap::new();
        for (i, _) in &old.bound_trees {
            *tree_times.entry(i).or_insert(0) += 1;
        }
        for (i, times) in tree_times {
            if let Some(a) = self.index.trees.get_mut(i) {
                if !a.retract(owner, times) {
                    self.index.trees.remove(i);
                }
            }
        }
        for name in &old.erased_entries {
            if let Some(set) = self.index.erased.get_mut(name) {
                set.remove(owner);
                if set.is_empty() {
                    self.index.erased.remove(name);
                }
            }
        }
        for to in &old.always_emitted_targets {
            if let Some(set) = self.index.always_emitted.get_mut(to) {
                set.remove(owner);
                if set.is_empty() {
                    self.index.always_emitted.remove(to);
                }
            }
            // An always-emitted body ROOTS its callees, so withdrawing one is
            // the same kind of loss as deleting an edge: the instance may have
            // had no other way in. Found by the cross-check -- editing a const
            // initializer left its old callee live.
            self.lost.insert(to.clone());
        }
        if let Some(from) = owner.instance() {
            for e in &old.edges {
                if let Some(set) = self.index.preds.get_mut(&e.to) {
                    set.remove(from);
                    if set.is_empty() {
                        self.index.preds.remove(&e.to);
                    }
                }
            }
            // Its own edges are gone, so its successors may have lost their
            // last way in. Recorded for the caller to prune from.
            self.lost.extend(old.edges.iter().map(|e| e.to.clone()));
            if let std::collections::btree_map::Entry::Occupied(mut e) =
                self.index.decls.entry(DeclKey(from.func))
            {
                *e.get_mut() -= 1;
                if *e.get() == 0 {
                    e.remove();
                }
            }
        }
    }

    /// **Rebuild every flat field from the contribution set**, prune what no
    /// root reaches, and re-run the whole-graph post-passes.
    ///
    /// `uninstantiated_roots` is NOT reset here: [`Self::seed_roots`] owns that
    /// reset and runs first on both paths, so a clear in both places would be
    /// one mechanism twice -- mutation-verified inert.
    ///
    /// It is the only writer of the CONCATENATION fields -- `edges`,
    /// `call_sites`, `duplicate_sites`, `unresolved`, `rejections`,
    /// `storage_failures`, `depth_capped`, `expanded`, `always_emitted_targets`
    /// and the operational counters. It is NOT the only writer of the graph:
    ///
    /// - [`Self::seed_roots`] owns `root_instances`, `uninstantiated_roots` and
    ///   `excused_generic_roots`;
    /// - the four post-passes it calls each own their own field
    ///   (`inline_body_deps`, `boundaries`, `coroutine_helpers`,
    ///   `recursion_starts`, and the whole-graph `rejections` append);
    /// - `substitutions`, `bound_trees` and their agreement records are written
    ///   BOTH here and by [`Self::merge_reach`]/[`Self::merge_bound_tree`] from
    ///   [`Self::commit`] during [`Self::run`], because `expand` reads an
    ///   instance's substitution and witnesses before this ever runs.
    ///
    /// That last one is two writers for one fact and is the seam the
    /// derivation cut removes: one indexing path, taken by install and replace
    /// alike. Returns the instances it pruned.
    fn derive(&mut self, ws: &Workspace, incremental: bool) -> Prune {
        let derive_start = Instant::now();
        let lost: Vec<Instance> = std::mem::take(&mut self.lost).into_iter().collect();
        // A FRESH build prunes nothing by construction: `run` only ever walks
        // owners a root or an always-emitted seed demanded, so every owner it
        // installed is root-reachable. An update is where the question has an
        // answer, and the answer is searched over the affected region.
        let Prune { pruned, region } =
            if incremental { self.stranded(&lost) } else { Prune::default() };
        if std::env::var("ROCK_GRAPH_VERIFY").is_ok() {
            // Both claims against one whole-graph reachability walk, on demand:
            // that the delta found everything, and that a fresh build had
            // nothing to find. The compact fixtures run with it on, so a region
            // the delta failed to reach is a failure there rather than a stale
            // instance nobody looks at.
            let wholesale = self.unreachable_wholesale();
            assert_eq!(
                pruned, wholesale,
                "the affected-region prune and a whole-graph reachability walk disagree"
            );
        }
        let dead: Vec<Owner> = pruned.iter().cloned().map(Owner::Instance).collect();
        // Through `uninstall`, so a pruned owner's index entries go with it --
        // the arena is the ONLY thing not pruned, because an entry outliving
        // its last instance is what keeps ids stable across edits.
        for owner in &dead {
            self.uninstall(owner);
        }

        // What ENTERED and LEFT, which is what the per-declaration passes below
        // are refreshed over. A fresh build's `before` is empty, so they run
        // over everything -- the same code path, not a second one.
        let after: BTreeSet<Instance> =
            self.contributions.keys().filter_map(|o| o.instance()).cloned().collect();
        let entered: Vec<Instance> = after.difference(&self.instances).cloned().collect();
        let left: Vec<Instance> = self.instances.difference(&after).cloned().collect();
        self.instances = after;
        let decls_entered: Vec<DeclKey> = {
            let mut v: Vec<DeclKey> = entered.iter().map(|i| DeclKey(i.func)).collect();
            v.sort();
            v.dedup();
            v.retain(|d| self.index.decls.contains_key(d));
            v
        };
        let decls_left: Vec<DeclKey> = {
            let mut v: Vec<DeclKey> = left.iter().map(|i| DeclKey(i.func)).collect();
            v.sort();
            v.dedup();
            v.retain(|d| !self.index.decls.contains_key(d));
            v
        };
        for decl in &decls_left {
            for member in skull_mir::lower::backend_variant_group(ws, decl.0) {
                self.inline_body_deps.remove(&DeclKey(member));
                self.inline_body_nomangle_roots.remove(&DeclKey(member));
            }
            let f = &ws.module(decl.0.module).functions[decl.0.func];
            self.boundaries.remove(&mangle_fn(ws, f));
        }
        self.graph_rejections.clear();
        self.sites_attempted = 0;
        self.always_emitted_seeds = 0;
        self.synthesized_examined = 0;
        self.synthesized_approximated = 0;
        for name in self.uninstantiated_roots.values_mut() {
            name.always_emitted_sites.clear();
        }
        // Summed over the primary records rather than accumulated beside them.
        // O(owners) with no allocation, and `graph_contributions` asserts the
        // equality, so a maintained total that drifted would be caught rather
        // than believed.
        for c in self.contributions.values() {
            self.sites_attempted += c.sites_attempted;
            self.always_emitted_seeds += c.always_emitted_seeds;
            self.synthesized_examined += c.synthesized_examined;
            self.synthesized_approximated += c.synthesized_approximated;
        }
        let holes: Vec<(String, AlwaysEmittedFailure)> = self
            .contributions
            .values()
            .flat_map(|c| c.always_emitted_holes.iter().cloned())
            .collect();
        for (name, failure) in holes {
            self.uninstantiated_roots
                .entry(name)
                .or_default()
                .always_emitted_sites
                .push(failure);
        }

        // Which holes nevertheless have concrete instances from other call
        // sites. A post-pass because a root is recorded before any body is
        // walked, so at insertion time no instance of it exists yet -- which is
        // how a name a later call fully instantiated went on reading as having
        // no concrete body at all.
        if !self.uninstantiated_roots.is_empty() {
            let mut counts: BTreeMap<String, usize> = BTreeMap::new();
            for i in &self.instances {
                let f = &ws.module(i.func.module).functions[i.func.func];
                *counts.entry(mangle_fn(ws, f)).or_default() += 1;
            }
            for (name, hole) in self.uninstantiated_roots.iter_mut() {
                hole.instances = counts.get(name).copied().unwrap_or(0);
            }
        }

        self.run_stats.derive_ns = derive_start.elapsed().as_nanos() as u64;
        let post_pass_start = Instant::now();
        record_inline_body_deps(self, ws, &decls_entered);
        record_inline_body_nomangle_roots(self, ws, decls_entered.iter().map(|decl| decl.0));
        self.refresh_erased_entries();
        detect_changing_instance_recursion(self, ws);
        classify_boundaries(self, ws, &decls_entered);
        record_coroutine_helpers(self, ws, &entered, &left);
        self.run_stats.post_pass_ns = post_pass_start.elapsed().as_nanos() as u64;
        self.run_stats.derive_ns += self.run_stats.post_pass_ns;
        Prune { pruned, region }
    }

    /// **Update the graph for a BODY-ONLY change**, in place, against the same
    /// monotonic [`MonoArena`].
    ///
    /// `changed` is the reinferred `(ModuleId, BodyOwner)` population
    /// incremental inference already produces
    /// (`bone_incremental::UpdateResult::reinferred`). Only the owners those
    /// bodies belong to are re-walked; everything they newly reach is walked
    /// too, and everything no root reaches any more is PRUNED -- including
    /// groups of instances that keep each other alive, which a reference count
    /// could never drop to zero.
    ///
    /// It is the same walk, the same reachability and the same derivation a
    /// full [`build`] runs; what differs is only which owners are re-walked.
    /// The arena alone stays monotonic: a pruned instance's interned types go
    /// unreferenced, which is what keeps ids stable across edits.
    ///
    /// **INTERFACE-tier changes are not this function's job.** A changed
    /// signature moves the root set, the mangling and every caller's
    /// instantiation; `bone_incremental::UpdateTier` names that case and the
    /// caller must rebuild instead.
    pub fn update_bodies(
        &mut self,
        ws: &Workspace,
        roots: Vec<FuncRef>,
        changed: &[(ModuleId, BodyOwner)],
    ) -> UpdateStats {
        let before: BTreeSet<Instance> = self.instances.clone();
        // Re-derived and CHECKED, not assumed: the root set is a function of
        // the declarations, so a body-only edit cannot move it, and an edit
        // that did move it is an interface change the caller must rebuild for
        // rather than one this function can absorb.
        let previous_roots = std::mem::take(&mut self.root_instances);
        self.seed_roots(ws, canonical_roots(ws, roots, false));
        assert!(
            self.root_instances == previous_roots,
            "the root set moved during a body-only update ({} before, {} after) -- that is \
             an interface change",
            previous_roots.len(),
            self.root_instances.len()
        );

        // No removal step: `run` expands every pending owner unconditionally
        // and `commit` inserts over whatever was there, so an affected owner's
        // old contribution is REPLACED rather than merged with. A remove-first
        // loop stood here and was inert -- mutation-verified.
        let affected = self.affected_owners(ws, changed);
        let run = self.run(ws, affected.clone());
        self.run_stats = run;
        let prune = self.derive(ws, true);
        let changed_decls = changed.iter().filter_map(|(module, owner)| match owner {
            BodyOwner::Function(func) => Some(skull_mir::lower::canonical_variant(
                ws,
                FuncRef { module: *module, func: *func },
            )),
            _ => None,
        });
        record_inline_body_nomangle_roots(self, ws, changed_decls);
        UpdateStats {
            affected: affected.len(),
            expanded: run.expanded,
            pruned: prune.pruned.len(),
            suspect_region: prune.region,
            newly_reached: self.instances.difference(&before).count(),
            added: self.instances.difference(&before).cloned().collect(),
            removed: before.difference(&self.instances).cloned().collect(),
            owners: self.contributions.len(),
            instances: self.instances.len(),
            settle_rounds: run.settle_rounds,
            census: run,
        }
    }

    /// Which owners a changed body invalidates.
    ///
    /// Two causes, and the second is the one a "re-walk the edited body" rule
    /// misses. An owner's contribution is stale if
    ///
    /// - the edited body IS its body, or
    /// - it QUOTES the edited file. Every recorded fact carries a span, and an
    ///   edit anywhere in a file shifts the offsets of everything after it, so
    ///   an untouched body in the same file holds spans that no longer match
    ///   the workspace. It is not even confined to one file: a struct literal
    ///   that omits a field injects that field's initializer from the DECLARING
    ///   struct's module, so an instance records sites spanned in a file it does
    ///   not live in.
    ///
    /// The second cause was found by the always-emitted-hole fixture, whose
    /// edit made one identifier a character longer: every span in `main` moved
    /// by one byte and the updated graph kept the old ones.
    ///
    /// Total over [`BodyOwner`] on purpose: an arm that silently did nothing
    /// would leave a stale contribution behind and the graph would still look
    /// consistent with itself.
    fn affected_owners(&self, ws: &Workspace, changed: &[(ModuleId, BodyOwner)]) -> Vec<Owner> {
        let mut changed_files: BTreeSet<ustr::Ustr> = BTreeSet::new();
        let mut changed_decls: BTreeSet<DeclKey> = BTreeSet::new();
        let mut changed_seeds: Vec<Owner> = Vec::new();
        for (module, body_owner) in changed {
            changed_files.insert(ws.module(*module).file);
            match body_owner {
                // The graph keys instances on bone's collapsed representative,
                // so a backend sibling's body reaches its instances through the
                // same canonicalization `build` seeds roots with.
                BodyOwner::Function(f) => {
                    changed_decls.insert(DeclKey(skull_mir::lower::canonical_variant(
                        ws,
                        FuncRef { module: *module, func: *f },
                    )));
                }
                // An owner in its own right. Reachable through a `const`
                // initializer, whose VALUE is a body
                // (`bone_incremental::item_prints`); a mutable `global` keeps
                // whole-item interface, so an edit to one arrives as a tier
                // change instead and never here.
                BodyOwner::Global(_) => {
                    changed_seeds.push(Owner::AlwaysEmitted(*module, *body_owner));
                }
                // **Not reachable from a body-only edit, and rejected rather
                // than handled.** A struct, an enum, a newtype and a decorator
                // each hash their WHOLE item into the interface print, so any
                // edit to one is an interface change: it moves the root set, the
                // manglings and every caller's instantiation, none of which this
                // function claims to maintain. Handling the arm anyway would be
                // a mechanism with no way in, and a silent no-op would leave a
                // stale contribution behind while the graph still looked
                // consistent with itself. Pinned by
                // `a_struct_enum_newtype_or_decorator_edit_is_interface_tier`.
                BodyOwner::Decorator(_)
                | BodyOwner::StructDefaults(_)
                | BodyOwner::Enum(_)
                | BodyOwner::Newtype(_) => panic!(
                    "`update_bodies` was given {body_owner:?}, which no BODY-ONLY edit can \
                     produce -- that item kind hashes whole-item into its interface print. \
                     The caller is on an interface-tier change and must rebuild."
                ),
            }
        }
        let mut out: Vec<Owner> = changed_seeds;
        for (owner, c) in &self.contributions {
            let quotes_edit = c.span_files.iter().any(|f| changed_files.contains(f));
            let is_edited_body = owner.instance().is_some_and(|i| changed_decls.contains(&DeclKey(i.func)));
            if quotes_edit || is_edited_body {
                out.push(owner.clone());
            }
        }
        out.sort();
        out.dedup();
        out
    }

    /// One instance's primary record, or `None` if the graph does not hold it.
    ///
    /// Not public: the two owner-local questions consumers actually ask are
    /// [`Self::sites_of`] and [`Self::is_expanded`], and publishing the whole
    /// record would invite a consumer to answer a third one its own way.
    fn contribution(&self, i: &Instance) -> Option<&Contribution> {
        self.contributions.get(&Owner::Instance(i.clone()))
    }

    /// **Owner-local**: what each of this instance's operations resolved to.
    /// Read straight off its own record rather than through a graph-wide map,
    /// because that is the question being asked.
    pub fn sites_of(&self, i: &Instance) -> Option<&BTreeMap<Site, SiteRecord>> {
        self.contribution(i).map(|c| &c.call_sites).filter(|m| !m.is_empty())
    }

    /// Every edge in the graph, in owner order.
    ///
    /// An ITERATOR rather than a field: the rows already exist, once, in the
    /// contributions. Materializing a second copy cost 32k `Edge` clones per
    /// update on mag and served no production consumer -- `artifact` says in
    /// its own comment that it deliberately works from sites, not edges.
    pub fn edges(&self) -> impl Iterator<Item = &Edge> {
        self.contributions.values().flat_map(|c| c.edges.iter())
    }

    /// Calls the walk could not turn into an instance, in owner order.
    pub fn unresolved(&self) -> impl Iterator<Item = &Unresolved> {
        self.contributions.values().flat_map(|c| c.unresolved.iter())
    }

    /// Every storage query that could not decide, by reason.
    pub fn storage_failures(&self) -> impl Iterator<Item = &MonoTyReason> {
        self.contributions.values().flat_map(|c| c.storage_failures.iter())
    }

    /// Sites recorded twice for one instance. The OWNER is the instance, so the
    /// pair comes from the map key rather than from a copy stored in the row.
    pub fn duplicate_sites(&self) -> impl Iterator<Item = (&Instance, &Site)> {
        self.contributions.iter().filter_map(|(o, c)| o.instance().map(|i| (i, c))).flat_map(
            |(i, c)| c.duplicate_sites.iter().map(move |s| (i, s)),
        )
    }

    /// Instances whose expansion `MAX_INSTANTIATION_DEPTH` cut.
    pub fn depth_capped(&self) -> impl Iterator<Item = &Instance> {
        self.contributions
            .iter()
            .filter(|(_, c)| c.depth_capped)
            .filter_map(|(o, _)| o.instance())
    }

    /// Instances whose Rock body the walk entered.
    pub fn expanded(&self) -> impl Iterator<Item = &Instance> {
        self.contributions.iter().filter(|(_, c)| c.expanded).filter_map(|(o, _)| o.instance())
    }

    /// **Owner-local**: did the walk enter this instance's body? Read by
    /// [`Self::closure`]; consumers that want the whole population use
    /// [`Self::expanded`].
    pub fn is_expanded(&self, i: &Instance) -> bool {
        self.contribution(i).is_some_and(|c| c.expanded)
    }

    /// Every instance's site table, in owner order.
    pub fn call_sites(&self) -> impl Iterator<Item = (&Instance, &BTreeMap<Site, SiteRecord>)> {
        self.contributions
            .iter()
            .filter_map(|(o, c)| o.instance().map(|i| (i, &c.call_sites)))
            .filter(|(_, m)| !m.is_empty())
    }

    /// **Owner-local**: the substitution this instance's body is walked under.
    ///
    /// A ROOT has no caller to have recorded one, and its substitution is EMPTY
    /// rather than absent -- seeding roots directly is what once left corpus
    /// instances with no record at all.
    pub fn substitution(&self, i: &Instance) -> Option<&MonoSubst> {
        self.index
            .subst
            .get(i)
            .map(|a| a.decided())
            .or_else(|| self.root_instances.contains(i).then(|| &*EMPTY_SUBST))
    }

    /// Every instance's agreed substitution.
    pub fn substitutions(&self) -> impl Iterator<Item = (&Instance, &MonoSubst)> {
        self.index
            .subst
            .iter()
            .map(|(i, a)| (i, a.decided()))
            .chain(
                self.root_instances
                    .iter()
                    .filter(|i| !self.index.subst.contains_key(*i))
                    .map(|i| (i, &*EMPTY_SUBST)),
            )
    }

    /// Instances two owners reached with DIFFERENT substitutions. Non-empty
    /// means the substitution is not a function of the instance, so a
    /// `ConcreteBody` built from it would depend on which caller won.
    pub fn subst_disagreements(&self) -> impl Iterator<Item = SubstDisagreement> + '_ {
        self.index.subst.iter().flat_map(|(i, a)| {
            let kept = a.decided().clone();
            let out: Vec<SubstDisagreement> = a
                .dissent
                .values()
                .chain(std::iter::once(&a.value))
                .filter(|v| **v != kept)
                .map(|v| SubstDisagreement {
                    instance: i.clone(),
                    kept: kept.clone(),
                    discarded: v.clone(),
                })
                .collect();
            out
        })
    }

    /// **Owner-local**: the concrete witnesses supplied for this instance.
    pub fn bound_tree(&self, i: &Instance) -> Option<&InstanceBoundTree> {
        self.index.trees.get(i).map(|a| a.decided())
    }

    pub fn bound_trees(&self) -> impl Iterator<Item = (&Instance, &InstanceBoundTree)> {
        self.index.trees.iter().map(|(i, a)| (i, a.decided()))
    }

    /// **Owner-local**: was this instance reached with two different witness
    /// trees? Asked per instance by `concrete_body`, which used to answer it by
    /// scanning the whole disagreement vector.
    pub fn has_bound_tree_disagreement(&self, i: &Instance) -> bool {
        self.index.trees.get(i).is_some_and(|a| !a.dissent.is_empty())
    }

    pub fn bound_tree_disagreements(&self) -> impl Iterator<Item = BoundTreeDisagreement> + '_ {
        self.index.trees.iter().flat_map(|(i, a)| {
            let recorded = a.decided().clone();
            let out: Vec<BoundTreeDisagreement> = a
                .dissent
                .values()
                .chain(std::iter::once(&a.value))
                .filter(|v| **v != recorded)
                .map(|v| BoundTreeDisagreement {
                    instance: i.clone(),
                    recorded: recorded.clone(),
                    incoming: v.clone(),
                })
                .collect();
            out
        })
    }

    /// **Owner-local**: is this instance rooted by an always-emitted body?
    pub fn is_always_emitted_target(&self, i: &Instance) -> bool {
        self.index.always_emitted.contains_key(i)
    }

    pub fn always_emitted_targets(&self) -> impl Iterator<Item = &Instance> {
        self.index.always_emitted.keys()
    }

    /// Re-reaches the agreement check ran over. Derived from the index rather
    /// than accumulated: an instance reached N times was compared N-1 times.
    pub fn subst_comparisons(&self) -> usize {
        self.index.subst.values().map(|a| a.total - 1).sum()
    }

    pub fn subst_compared(&self, i: &Instance) -> usize {
        self.index.subst.get(i).map_or(0, |a| a.total - 1)
    }

    pub fn bound_tree_comparisons(&self) -> usize {
        self.index.trees.values().map(|a| a.total - 1).sum()
    }

    /// Every rejection: the per-body ones and the whole-graph ones.
    pub fn rejections(&self) -> impl Iterator<Item = &Rejection> {
        self.contributions
            .values()
            .flat_map(|c| c.rejections.iter())
            .chain(self.graph_rejections.iter())
    }

    /// The erased catalogue: the entries the WALK named plus the ones opaque
    /// inline bodies name. Bounded by `ERASED_RUNTIME_ENTRIES` (15 names), so
    /// it is rebuilt from its two indices rather than maintained -- one writer
    /// for a set this small beats two that have to agree.
    fn refresh_erased_entries(&mut self) {
        self.erased_entries = self
            .index
            .erased
            .keys()
            .cloned()
            .chain(self.inline_body_deps.values().flatten().map(|n| (*n).to_string()))
            .collect();
    }

    /// **What an edit stranded**, searched over the affected region only.
    ///
    /// Reachability, not reference counting: an edit can strand a whole group
    /// of instances that keep each other alive, and a cycle has no member whose
    /// count would ever drop to zero. But it does not need the whole graph
    /// either -- only an instance whose IN-EDGES changed can have become
    /// unreachable, so the search starts at those and stops at the boundary of
    /// what they can reach.
    ///
    /// Three steps, and the third is what makes cycles come out right:
    ///
    /// 1. the SUSPECT region is everything forward-reachable from the instances
    ///    that lost an in-edge -- if one is dead, everything only it reached is
    ///    dead too;
    /// 2. an ANCHOR is a member of that region a root still reaches without
    ///    going through it: a root instance, an always-emitted target, or the
    ///    target of an edge from a live instance OUTSIDE the region;
    /// 3. whatever the anchors reach INSIDE the region survives. A cycle with no
    ///    anchor keeps only itself alive, so nothing marks it and it goes.
    fn stranded(&self, lost: &[Instance]) -> Prune {
        // Owned nodes, one map lookup per node VISITED rather than one per edge
        // followed: `contributions` is keyed by `Owner`, so borrowing a target
        // back out of it costs an `Instance` clone either way.
        let succ = |i: &Instance| -> &[Edge] {
            self.contributions
                .get(&Owner::Instance(i.clone()))
                .map_or(&[][..], |c| c.edges.as_slice())
        };
        let mut region: BTreeSet<Instance> = BTreeSet::new();
        let mut queue: Vec<Instance> = lost.to_vec();
        while let Some(i) = queue.pop() {
            if !self.contributions.contains_key(&Owner::Instance(i.clone()))
                || !region.insert(i.clone())
            {
                continue;
            }
            queue.extend(succ(&i).iter().map(|e| e.to.clone()));
        }
        if region.is_empty() {
            return Prune::default();
        }
        let mut alive: Vec<Instance> = region
            .iter()
            .filter(|i| {
                self.root_instances.contains(*i)
                    || self.index.always_emitted.contains_key(*i)
                    || self
                        .index
                        .preds
                        .get(*i)
                        .is_some_and(|p| p.iter().any(|from| !region.contains(from)))
            })
            .cloned()
            .collect();
        let mut survives: BTreeSet<Instance> = BTreeSet::new();
        while let Some(i) = alive.pop() {
            if !survives.insert(i.clone()) {
                continue;
            }
            for e in succ(&i) {
                if region.contains(&e.to) && !survives.contains(&e.to) {
                    alive.push(e.to.clone());
                }
            }
        }
        Prune { pruned: region.difference(&survives).cloned().collect(), region: region.len() }
    }

    /// Instances the graph holds that no root reaches. The whole-graph
    /// counterpart of [`Self::stranded`].
    fn unreachable_wholesale(&self) -> BTreeSet<Instance> {
        let live = self.reachable_instances();
        self.contributions
            .keys()
            .filter_map(|o| o.instance())
            .filter(|i| !live.contains(*i))
            .cloned()
            .collect()
    }

    /// The instances a ROOT reaches, over the contributions' own edges.
    ///
    /// The whole-graph form. A fresh build needs it -- there is no prior state
    /// to take a delta against -- and it is the ORACLE the incremental prune is
    /// checked against under `ROCK_GRAPH_VERIFY`.
    pub fn reachable_instances(&self) -> BTreeSet<Instance> {
        let mut seeds: Vec<&Instance> = self.root_instances.iter().collect();
        for c in self.contributions.values() {
            seeds.extend(c.always_emitted_targets.iter());
        }
        // Over REFERENCES: an `Instance` owns its argument vector, so a walk
        // that clones every node it visits allocates once per edge.
        let mut live: BTreeSet<&Instance> = BTreeSet::new();
        while let Some(i) = seeds.pop() {
            if !live.insert(i) {
                continue;
            }
            let Some((_, c)) = self.contributions.get_key_value(&Owner::Instance(i.clone()))
            else {
                continue;
            };
            seeds.extend(c.edges.iter().map(|e| &e.to));
        }
        live.into_iter().cloned().collect()
    }
}

#[derive(Clone, Copy)]
enum VisitKind {
    FirstOwner,
    DuplicatePending,
    WitnessSettlement,
    IncrementalForced,
}

#[derive(Default)]
struct IndexMaintenanceTiming {
    subst_ns: u64,
    trees_ns: u64,
    erased_ns: u64,
    always_emitted_ns: u64,
    preds_ns: u64,
    decls_ns: u64,
    commit_ns: u64,
    subst_updates: usize,
    trees_updates: usize,
    erased_updates: usize,
    always_emitted_updates: usize,
    preds_updates: usize,
    decls_updates: usize,
}

impl IndexMaintenanceTiming {
    fn total_ns(&self) -> u64 {
        self.subst_ns
            + self.trees_ns
            + self.erased_ns
            + self.always_emitted_ns
            + self.preds_ns
            + self.decls_ns
            + self.commit_ns
    }
}

#[derive(Default)]
struct InstallTiming {
    uninstall_ns: u64,
    index: IndexMaintenanceTiming,
}

/// What one [`InstanceGraph::run`] actually walked. Operational, and the check
/// that an update touched the affected region rather than the program.
#[derive(Default, Debug, Clone, Copy)]
pub struct RunStats {
    pub census_enabled: bool,
    /// Every owner popped from the pending worklist, including stranded skips.
    pub owner_visits: usize,
    pub expanded: usize,
    pub frontier_count: usize,
    pub frontier_owner_p50: usize,
    pub frontier_owner_p95: usize,
    pub frontier_owner_max: usize,
    pub singleton_frontiers: usize,
    pub frontier_ready_owners: usize,
    pub frontier_same_frontier_dependency_owners: usize,
    pub ordinary_owner_visits: usize,
    pub always_emitted_owner_visits: usize,
    pub expansion_ns_total: u64,
    pub expansion_ns_p50: u64,
    pub expansion_ns_p95: u64,
    pub expansion_ns_max: u64,
    pub first_owner_visits: usize,
    pub duplicate_pending_visits: usize,
    pub duplicate_pending_suppressed: usize,
    pub witness_settlement_rewalks: usize,
    pub incremental_forced_rewalks: usize,
    pub skipped_stranded_owners: usize,
    pub first_owner_ns: u64,
    pub duplicate_pending_ns: u64,
    pub witness_settlement_ns: u64,
    pub incremental_forced_ns: u64,
    pub mono_intern_attempts: u64,
    pub mono_new_types: u64,
    pub mono_read_acqs: u64,
    pub mono_write_acqs: u64,
    pub mono_elapsed_ns: u64,
    pub contribution_reached: usize,
    pub contribution_bound_trees: usize,
    pub contribution_edges: usize,
    pub contribution_sites: usize,
    pub contribution_struct_default_domains: usize,
    pub contribution_rejections: usize,
    pub contribution_synthesized_examined: usize,
    pub contribution_failures: usize,
    pub contribution_always_emitted_targets: usize,
    pub admissibility_ns: u64,
    pub body_prep_ns: u64,
    pub signature_type_conversion_ns: u64,
    pub call_site_resolution_ns: u64,
    pub witness_bound_tree_ns: u64,
    pub walk_body_edges_ns: u64,
    pub walk_struct_default_edges_ns: u64,
    pub uninstall_ns: u64,
    pub index_maintenance_ns: u64,
    pub index_subst_ns: u64,
    pub index_trees_ns: u64,
    pub index_erased_ns: u64,
    pub index_always_emitted_ns: u64,
    pub index_preds_ns: u64,
    pub index_decls_ns: u64,
    pub index_commit_ns: u64,
    pub index_subst_updates: usize,
    pub index_trees_updates: usize,
    pub index_erased_updates: usize,
    pub index_always_emitted_updates: usize,
    pub index_preds_updates: usize,
    pub index_decls_updates: usize,
    pub index_subst_unique: usize,
    pub index_trees_unique: usize,
    pub index_erased_unique: usize,
    pub index_always_emitted_unique: usize,
    pub index_preds_unique: usize,
    pub index_decls_unique: usize,
    pub derive_ns: u64,
    pub post_pass_ns: u64,
    pub body_exprs_scanned: usize,
    pub resolved_overload_rows: usize,
    pub fn_value_rows: usize,
    pub generated_rows: usize,
    pub dispatched_rows: usize,
    pub implicit_iter_rows: usize,
    pub bound_tree_rows: usize,
    pub bound_call_rows: usize,
    pub iterator_rows: usize,
    /// How many times the conformance fixpoint had to walk owners again.
    ///
    /// Published because a guard with no population reads as load-bearing when
    /// it is not: this is 0 on every fixture and on all three real projects, so
    /// it is what makes the result independent of walk ORDER rather than a pass
    /// the answer depends on.
    pub settle_rounds: usize,
}

/// What [`InstanceGraph::derive`] pruned, and over how much.
///
/// The region is carried out beside the answer because it is the only thing
/// that distinguishes an affected-region search from a whole-graph one: both
/// return the same pruned set, and canonical equality is green either way.
#[derive(Default)]
struct Prune {
    pruned: BTreeSet<Instance>,
    /// How many instances the suspect walk VISITED to produce `pruned` -- the
    /// forward closure of what lost an in-edge.
    region: usize,
}

/// What one [`InstanceGraph::update_bodies`] did, as a structural census.
///
/// Canonical equality against a fresh build cannot tell an update from a hidden
/// full rebuild -- a rebuild reaches the right answer too. These are what
/// separate them, which is why they are reported rather than inferred from wall
/// time: every number here has a total beside it, so "small" is a comparison
/// rather than an adjective.
#[derive(Default, Debug, Clone)]
pub struct UpdateStats {
    /// Owners re-walked because their body changed.
    pub affected: usize,
    /// Owners walked in total: the affected ones plus everything newly
    /// reached, plus any re-walk the conformance fixpoint demanded.
    pub expanded: usize,
    /// Instances no root reaches any more.
    pub pruned: usize,
    /// **Instances the strand search VISITED**, i.e. the size of the suspect
    /// region -- the forward closure of everything that lost an in-edge.
    ///
    /// The prune's own cost, which its RESULT cannot report: a whole-graph
    /// search returns exactly the same `pruned` set and passes canonical
    /// equality, so nothing else here can tell the two apart. Asserted against
    /// `instances` so "affected region" is a comparison rather than a claim.
    pub suspect_region: usize,
    /// Instances the graph did not have before.
    pub newly_reached: usize,
    /// **The instances this update ADDED**, as the set rather than the count.
    ///
    /// Emission needs the members, not the size: each one needs a chunk built
    /// and becomes eligible for overlay backfill. `newly_reached` is its
    /// cardinality and is kept because the fixtures assert on it.
    pub added: BTreeSet<Instance>,
    /// **The instances this update REMOVED** -- pruned, or left with no root.
    /// Their chunks and reverse-dependency edges must go with them, or a full
    /// reassembly re-emits a definition nothing calls.
    pub removed: BTreeSet<Instance>,
    pub owners: usize,
    pub instances: usize,
    /// See [`RunStats::settle_rounds`].
    pub settle_rounds: usize,
    /// The same denominator and phase census retained on the graph after the
    /// update, so callers do not need to infer a full rebuild from `expanded`.
    pub census: RunStats,
}

#[allow(clippy::too_many_arguments)]
fn walk_body_edges(
    w: &mut Walk,
    ws: &Workspace,
    inf: &BodyInference,
    body: Option<&bone_semantic::hir::Body>,
    from: Option<&Instance>,
    subst: &MonoSubst,
    // WHICH arena the sites recorded here index, and -- for a foreign one --
    // which of its expressions this owner actually reaches. `None` is the whole
    // body, which is what an own-domain walk always wants.
    domain: &SiteDomain,
    only: Option<&FxHashSet<Idx<bone_semantic::hir::Expr>>>,
) {
    let reached = |e: Idx<bone_semantic::hir::Expr>| only.is_none_or(|s| s.contains(&e));
    if let Some(body) = body {
        w.out.body_exprs_scanned += body.expr_arena.len();
    }
    w.out.resolved_overload_rows += inf.resolved_overload_calls.len();
    w.out.implicit_iter_rows += inf.implicit_iter_args.len();
    w.out.bound_tree_rows += inf.bound_call_trees.len();
    w.out.bound_call_rows += inf.bound_calls.len();
    w.out.iterator_rows += inf.for_iter_protocol.len();
    let mut resolved_calls: FxHashMap<Idx<bone_semantic::hir::Expr>, Instance> =
        FxHashMap::default();
    let mut calls: Vec<(Idx<bone_semantic::hir::Expr>, (ModuleId, Idx<Function>))> =
        inf.resolved_overload_calls.iter().map(|(e, r)| (*e, *r)).collect();
    calls.sort_by_key(|(e, _)| *e);
    for (e, (m, f)) in calls {
        if !reached(e) {
            continue;
        }
        // bone's collapsed representative, verbatim. Selecting the sibling that
        // serves a target happens in `artifact::realize`, not here.
        let callee = FuncRef { module: m, func: f };
        let bindings = inf.call_generic_bindings.get(&e);
        let span = body.and_then(|b| b.expr_spans.get(e).copied()).unwrap_or_else(Span::unknown);
        let kind = if bindings.is_some() { EdgeKind::GenericCall } else { EdgeKind::DirectCall };
        if let Some(to) = record_call(
            w, ws, inf, from, subst, callee, bindings, kind, span,
            Site { domain: domain.clone(), expr: e, op: SiteOp::DirectCall },
        ) {
            resolved_calls.insert(e, to);
        }
    }

    if let Some(body) = body {
        let fn_values = fn_value_sites(ws, inf, body);
        w.out.fn_value_rows += fn_values.len();
        for (e, callee) in fn_values.into_iter().filter(|(e, _)| reached(*e)) {
            let span = body.expr_spans.get(e).copied().unwrap_or_else(Span::unknown);
            let cf = &ws.module(callee.module).functions[callee.func];
            if cf.generics.is_empty() {
                record_call(
                    w, ws, inf, from, subst, callee, None, EdgeKind::FnValue, span,
                    Site { domain: domain.clone(), expr: e, op: SiteOp::FnValue },
                );
            } else if let Some(from) = from {
                w.record_site(
                    Some(from),
                    Site { domain: domain.clone(), expr: e, op: SiteOp::FnValue },
                    SiteOutcome::Failed("generic-fn-value"),
                    span,
                );
                w.push_rejection(Rejection {
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
                w.out.always_emitted_seeds += 1;
                w.record_always_emitted_hole(
                    mangle_fn(ws, cf),
                    AlwaysEmittedOutcome::Rejection(RejectionKind::GenericFnValue),
                    span,
                );
            }
        }

        let mut failures: Vec<(Idx<bone_semantic::hir::Expr>, MonoTyReason)> = Vec::new();
        let conversion_start = w.timer();
        let sites = compiler_generated_sites(
            ws, inf, body, subst, &mut w.out.synthesized_examined, &mut failures,
            &mut w.out.synthesized_approximated,
        );
        w.out.generated_rows += sites.len();
        add_elapsed(&mut w.out.timing.signature_type_conversion_ns, conversion_start);
        for (e, reason) in failures {
            w.out.storage_failures.push(reason.clone());
            let Some(from) = from else { continue };
            let span = body.expr_spans.get(e).copied().unwrap_or_else(Span::unknown);
            let kind = match reason {
                MonoTyReason::UnresolvedInfer(_) | MonoTyReason::UnboundGeneric(_) => {
                    RejectionKind::UnconstrainedTypeVar
                }
                _ => RejectionKind::ConcreteTypeUnavailable,
            };
            w.push_rejection(Rejection {
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
        // The two families are resolved through the same lookup and recorded as
        // the same edge kind. They differ in ONE way and it is deliberate: a
        // named runtime entry joins `erased_entries` (it exists only because
        // lowering names it), while a conditional dispatch arm calls an
        // ORDINARY stdlib declaration whose own boundary class already answers
        // for it -- filing `inspect` as an erased runtime entry would relabel a
        // real `inline(lua)` body.
        let dispatched_sites = dispatched_sites(ws, inf, body, subst, domain);
        w.out.dispatched_rows += dispatched_sites.len();
        let dispatched = dispatched_sites
            .into_iter()
            .map(|(site, name)| (site, name, false));
        let generated = sites
            .into_iter()
            .map(|GeneratedSite { expr, name }| {
                (Site { domain: domain.clone(), expr, op: SiteOp::Synthesized(name) }, name, true)
            })
            .chain(dispatched)
            .filter(|(site, _, _)| reached(site.expr));
        for (site, name, erased) in generated {
            let e = site.expr;
            let span = body.expr_spans.get(e).copied().unwrap_or_else(Span::unknown);
            let resolved = if erased {
                stdlib_entry_representative(ws, name)
            } else {
                skull_mir::lower::dispatched_callee_decl(ws, name)
            };
            match resolved {
                Some(callee) => {
                    record_call(
                        w, ws, inf, from, subst, callee, None,
                        EdgeKind::CompilerGenerated, span, site,
                    );
                    if erased {
                        w.out.erased_entries.insert(name.to_string());
                    }
                }
                None => {
                    w.record_site(
                        from,
                        site,
                        SiteOutcome::Failed("compiler-generated-entry-unresolved"),
                        span,
                    );
                    if let Some(from) = from {
                        w.push_unresolved(Unresolved {
                            from: from.clone(),
                            callee: from.func,
                            reason: "compiler-generated-entry-unresolved",
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
    for (e, witness) in implicit.into_iter().filter(|(e, _)| reached(*e)) {
        let span = body.and_then(|b| b.expr_spans.get(e).copied()).unwrap_or_else(Span::unknown);
        record_witness(
            w, ws, inf, from, subst, witness, span, EdgeKind::ImplicitIter,
            Site { domain: domain.clone(), expr: e, op: SiteOp::ImplicitIter },
        );
    }

    let mut trees: Vec<(Idx<bone_semantic::hir::Expr>, &bone_semantic::prelude::BoundTree)> =
        inf.bound_call_trees.iter().map(|(e, tree)| (*e, tree)).collect();
    trees.sort_by_key(|(e, _)| *e);
    for (e, tree) in trees.into_iter().filter(|(e, _)| reached(*e)) {
        let span = body.and_then(|b| b.expr_spans.get(e).copied()).unwrap_or_else(Span::unknown);
        let concrete = walk_bound_tree(
            w, ws, inf, from, subst, tree, span, e, BoundRoot::Call, domain, &[],
        );
        if let Some(to) = resolved_calls.get(&e) {
            w.record_bound_tree(to, concrete);
        }
    }

    let mut bound_calls: Vec<(Idx<bone_semantic::hir::Expr>, usize)> =
        inf.bound_calls.iter().map(|(e, idx)| (*e, *idx)).collect();
    bound_calls.sort_by_key(|(e, _)| *e);
    for (e, gidx) in bound_calls.into_iter().filter(|(e, _)| reached(*e)) {
        let span = body.and_then(|b| b.expr_spans.get(e).copied()).unwrap_or_else(Span::unknown);
        let site = Site { domain: domain.clone(), expr: e, op: SiteOp::DirectCall };
        let owner_tree = w.owner_tree;
        let resolved =
            from.and(owner_tree).and_then(|tree| tree.next.get(&gidx)).cloned();
        let Some(resolved) = resolved else {
            w.record_site(from, site, SiteOutcome::Failed("caller-generic-witness-unbound"), span);
            continue;
        };
        if let Some(from) = from {
            w.push_edge(Edge {
                from: from.clone(),
                to: resolved.callee.clone(),
                kind: EdgeKind::BoundNextWitness,
                span,
            });
        }
        w.record_site(from, site, SiteOutcome::Resolved(resolved.callee), span);
    }

    let mut iters: Vec<(Idx<bone_semantic::hir::Expr>, &bone_semantic::prelude::ForIterInfo)> =
        inf.for_iter_protocol.iter().map(|(e, info)| (*e, info)).collect();
    iters.sort_by_key(|(e, _)| *e);
    for (e, info) in iters.into_iter().filter(|(e, _)| reached(*e)) {
        let span = body.and_then(|b| b.expr_spans.get(e).copied()).unwrap_or_else(Span::unknown);
        if let Some(witness) = &info.iter {
            record_witness(
                w, ws, inf, from, subst, witness, span,
                EdgeKind::IteratorIter, Site { domain: domain.clone(), expr: e, op: SiteOp::ForIter },
            );
        }
        let _ = record_bound_next(
            w, ws, inf, from, subst, &info.next, span,
            EdgeKind::IteratorNext, e, BoundRoot::For, domain, &[],
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn record_call(
    w: &mut Walk,
    ws: &Workspace,
    inf: &BodyInference,
    from: Option<&Instance>,
    subst: &MonoSubst,
    callee: FuncRef,
    bindings: Option<&Vec<bone_semantic::prelude::TyId>>,
    kind: EdgeKind,
    span: Span,
    site: Site,
) -> Option<Instance> {
    let started = w.timer();
    let result = record_call_impl(w, ws, inf, from, subst, callee, bindings, kind, span, site);
    add_elapsed(&mut w.out.timing.call_site_resolution_ns, started);
    result
}

#[allow(clippy::too_many_arguments)]
fn record_call_impl(
    w: &mut Walk,
    ws: &Workspace,
    inf: &BodyInference,
    from: Option<&Instance>,
    subst: &MonoSubst,
    callee: FuncRef,
    bindings: Option<&Vec<bone_semantic::prelude::TyId>>,
    kind: EdgeKind,
    span: Span,
    site: Site,
) -> Option<Instance> {
    if from.is_none() {
        w.out.always_emitted_seeds += 1;
    }
    let cf = &ws.module(callee.module).functions[callee.func];
    let conversion_start = w.timer();
    let instantiated = w.with_interner_mut(|a| {
        instantiate(a, ws, inf, callee.module, callee.func, cf, bindings, subst)
    });
    add_elapsed(&mut w.out.timing.signature_type_conversion_ns, conversion_start);
    let (args, callee_subst) = match instantiated {
        Ok(v) => v,
        // An always-emitted body has no `from` to file against, but the
        // CLASSIFICATION is the same one an instantiated body's call gets:
        // `Unconstrained` is the decided `UnconstrainedTypeVar` rejection
        // either way. Ordered before the classifying arms so these guards win.

        Err(Fail::Unconstrained(_)) if from.is_none() => {
            w.record_always_emitted_hole(
                mangle_fn(ws, cf),
                AlwaysEmittedOutcome::Rejection(RejectionKind::UnconstrainedTypeVar),
                span,
            );
            w.record_site(from, site, SiteOutcome::Failed("unconstrained-type-var"), span);
            return None;
        }
        Err(fail) if from.is_none() => {
            w.record_always_emitted_hole(
                mangle_fn(ws, cf),
                AlwaysEmittedOutcome::Unresolved(fail.reason()),
                span,
            );
            w.record_site(from, site, SiteOutcome::Failed(fail.reason()), span);
            return None;
        }
        Err(Fail::Unconstrained(_)) => {
            let name = mangle_fn(ws, cf);
            w.push_rejection(Rejection {
                kind: RejectionKind::UnconstrainedTypeVar,
                at: from.expect("guarded above").clone(),
                span,
                message: format!(
                    "`{name}` is called with a type argument no constraint reaches, so there is \
                     no concrete type to instantiate it at"
                ),
            });
            w.record_site(from, site, SiteOutcome::Failed("unconstrained-type-var"), span);
            return None;
        }
        Err(fail) => {
            w.push_unresolved(Unresolved {
                from: from.expect("guarded above").clone(),
                callee,
                reason: fail.reason(),
                span,
            });
            w.record_site(from, site, SiteOutcome::Failed(fail.reason()), span);
            return None;
        }
    };
    let to = Instance { func: callee, args };
    match from {
        Some(from) => w.push_edge(Edge { from: from.clone(), to: to.clone(), kind, span }),
        // No `from`, so no edge and no site: an always-emitted body's callee is
        // a ROOT, and only this record says so.
        None => {
            w.out.always_emitted_targets.insert(to.clone());
        }
    }
    w.record_site(from, site, SiteOutcome::Resolved(to.clone()), span);
    w.reach(to.clone(), callee_subst);
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
    // Adjacency by REFERENCE into the contributions. Looking a successor's
    // edges back out of `contributions` instead costs an `Instance` clone per
    // node visited, because the map is keyed by `Owner` -- measured at 3.8 ms
    // against 1.3 for the map on mag.
    let mut succ: FxHashMap<&Instance, &[Edge]> = FxHashMap::default();
    for (owner, c) in &g.contributions {
        if let Some(i) = owner.instance() {
            if !c.edges.is_empty() {
                succ.insert(i, c.edges.as_slice());
            }
        }
    }

    // A declaration with exactly ONE instance graph-wide can never satisfy the
    // predicate below -- the search looks for a DIFFERENT instantiation of the
    // start's own declaration, and the only reachable nodes are instances the
    // graph holds. Skipping those starts is exact, not an approximation.
    //
    // It is most of the work, because the `already` skip further down engages
    // only AFTER a finding: without this, a program with no polymorphic
    // recursion -- the normal case -- runs a full DFS from every instance with
    // an outgoing edge, over the whole edge set, O(V.E).
    //
    // Counted over ALL instances, not over `succ`: a declaration's second
    // instance can be a LEAF with no outgoing edges, so it never appears as a
    // candidate start while being exactly what makes the first one's search
    // worth running. Counting over the starts would silently re-admit nothing
    // and skip the pair that matters.
    //
    // Measurements in `docs/parked/mono.md`.
    // The per-declaration instance counts are a maintained index, not a fresh
    // pass over every instance.
    // Starts are instances with an OUTGOING edge -- a leaf can never reach a
    // different instantiation of itself -- while the per-declaration COUNT is
    // over every instance, because a declaration's second instance can be a
    // leaf and is exactly what makes the first one's search worth running.
    let mut order: Vec<&Instance> = succ
        .keys()
        .copied()
        .filter(|i| g.index.decls.get(&DeclKey(i.func)).is_some_and(|n| *n > 1))
        .collect();
    order.sort();
    let mut found: Vec<(Instance, Span, String)> = Vec::new();
    let mut already: BTreeSet<(u32, u32)> = BTreeSet::new();
    let mut starts = 0usize;
    for start in order {
        let key = (start.func.module.raw(), start.func.func.into_raw().into_u32());
        if already.contains(&key) {
            continue;
        }
        starts += 1;
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
            for e in succ.get(node).copied().unwrap_or(&[]) {
                if seen.insert(&e.to) {
                    queue.push((&e.to, e.span));
                }
            }
        }
    }
    g.recursion_starts = starts;
    for (at, span, message) in found {
        g.graph_rejections.push(Rejection {
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
    w: &mut Walk,
    ws: &Workspace,
    inf: &BodyInference,
    from: Option<&Instance>,
    subst: &MonoSubst,
    witness: &IterWitness,
    span: Span,
    kind: EdgeKind,
    site: Site,
) {
    let started = w.timer();
    if from.is_none() {
        w.out.always_emitted_seeds += 1;
    }
    let rep = FuncRef { module: witness.func.0, func: witness.func.1 };
    let resolved = &ws.module(rep.module).functions[rep.func];
    let callee = rep;
    let cf = &ws.module(callee.module).functions[callee.func];
    let conversion_start = w.timer();
    let instantiated = w.with_interner_mut(|a| {
        instantiate_witness(a, ws, inf, resolved, cf, &witness.generics, subst)
    });
    add_elapsed(&mut w.out.timing.signature_type_conversion_ns, conversion_start);
    match instantiated {
        Ok((args, callee_subst)) => {
            let to = Instance { func: callee, args };
            match from {
                Some(from) => {
                    w.push_edge(Edge { from: from.clone(), to: to.clone(), kind, span })
                }
                // See `record_call`: an always-emitted body's callee is a root.
                None => {
                    w.out.always_emitted_targets.insert(to.clone());
                }
            }
            w.record_site(from, site, SiteOutcome::Resolved(to.clone()), span);
            w.reach(to, callee_subst);
        }
        // See `record_call`: the classification survives the absence of a
        // `from` instance.
        Err(Fail::Unconstrained(_)) if from.is_none() => {
            w.record_always_emitted_hole(
                mangle_fn(ws, cf),
                AlwaysEmittedOutcome::Rejection(RejectionKind::UnconstrainedTypeVar),
                span,
            );
            w.record_site(from, site, SiteOutcome::Failed("unconstrained-type-var"), span);
        }
        Err(fail) if from.is_none() => {
            w.record_always_emitted_hole(
                mangle_fn(ws, cf),
                AlwaysEmittedOutcome::Unresolved(fail.reason()),
                span,
            );
            w.record_site(from, site, SiteOutcome::Failed(fail.reason()), span);
        }
        Err(Fail::Unconstrained(_)) => {
            w.push_rejection(Rejection {
                kind: RejectionKind::UnconstrainedTypeVar,
                at: from.expect("guarded above").clone(),
                span,
                message: format!(
                    "the `iter` witness `{}` binds a type argument no constraint reaches",
                    mangle_fn(ws, cf)
                ),
            });
            w.record_site(from, site, SiteOutcome::Failed("unconstrained-type-var"), span);
        }
        Err(fail) => {
            w.push_unresolved(Unresolved {
                from: from.expect("guarded above").clone(),
                callee,
                reason: fail.reason(),
                span,
            });
            w.record_site(from, site, SiteOutcome::Failed(fail.reason()), span);
        }
    }
    add_elapsed(&mut w.out.timing.witness_bound_tree_ns, started);
}

/// The callee obligations an instance picks up from the field DEFAULTS of the
/// struct literals it contains.
///
/// A literal that omits a field lowers that field's initializer -- from the
/// declaring struct's `BodyOwner::StructDefaults` body, in that struct's module
/// -- into this instance's CFG. Those calls are this instance's obligations,
/// but their expressions index the DEFAULTS arena, which is what
/// [`SiteDomain::StructDefaults`] states.
///
/// Only the OMITTED fields, and only the expressions their initializers
/// actually reach: walking the whole defaults body would make a callee
/// reachable because some other field of the same struct defaults to it, which
/// is an emitted body the program never calls.
///
/// A `..base` functional update writes every field the literal does not, so a
/// literal with a base omits nothing.
fn walk_struct_default_edges(
    w: &mut Walk,
    ws: &Workspace,
    module: ModuleId,
    body: &Body,
    top_owner: BodyOwner,
    from: Option<&Instance>,
    subst: &MonoSubst,
) {
    use bone_semantic::hir::Expr as E;
    // One entry per (struct, ARGUMENTS, omitted field) this instance reaches,
    // deduplicated: two literals of one struct omitting one field are one
    // obligation set, but `Box<int> {}` and `Box<string> {}` are two -- the
    // initializers are shared text resolving different callees.
    //
    // Keyed by module INDEX rather than `ModuleId`, which has no `Ord`, and by
    // the arguments' CANONICAL encoding, which is what identity keys on; the
    // `ModuleId` and the arguments themselves travel in the value so the domain
    // keeps them.
    type Key = (usize, u32, Vec<String>);
    let mut omitted: BTreeMap<
        Key,
        (
            ModuleId,
            Idx<bone_semantic::hir::StructDef>,
            Vec<skull_mir::monoty::MonoTy>,
            BTreeSet<ustr::Ustr>,
        ),
    > = BTreeMap::new();
    // A DEFAULT can itself be a struct literal that omits a field --
    // `timers: MobTimers = MobTimers {}` inside `Mob`, whose own
    // `idle_timer = random_range(1, 4)` is the call two arenas down -- so the
    // scan is a worklist over bodies, not one pass over the instance's own.
    //
    // Each entry carries the inference and SUBSTITUTION in force for that body,
    // exactly as `Builder::with_struct_default` nests them: a nested defaults
    // body resolves under its own struct's slot bindings, not the outer one's.
    let Some(top_inf) = ws.inference(module).and_then(|ir| ir.per_fn.get(&top_owner)) else {
        return;
    };
    let mut pending: Vec<(
        &Body,
        &BodyInference,
        MonoSubst,
        Option<FxHashSet<Idx<bone_semantic::hir::Expr>>>,
    )> = vec![(body, top_inf, subst.clone(), None)];
    // How many omitted fields each key was last scanned with. A second literal
    // of one struct can omit a field the first wrote, so a set that GREW must be
    // rescanned -- a plain visited-set would stop at the first literal and miss
    // the sub-literals only the wider set reaches.
    let mut scanned: BTreeMap<Key, usize> = BTreeMap::new();
    while let Some((scan, scan_inf, scan_subst, reached)) = pending.pop() {
        let mut fresh: Vec<Key> = Vec::new();
        for (e, expr) in scan.expr_arena.iter() {
            if reached.as_ref().is_some_and(|r| !r.contains(&e)) {
                continue;
            }
            // A `..base` functional update writes every field the literal does
            // not, so a literal with a base omits nothing.
            let E::StructLiteral { name, fields, bases } = expr else { continue };
            if !bases.is_empty() {
                continue;
            }
            let Some(sid) = ws.struct_by_name.get(name) else { continue };
            // The same oracle lowering stamps the domain with, INCLUDING its
            // shared-body fallback -- a literal keyed one way here and the
            // other way there would report a hole that is really a
            // disagreement. A hard error records no obligation: lowering
            // rejects that program outright, so there is nothing to join.
            let generics = skull_mir::struct_defaults::declared_generics(ws, *sid);
            let Ok(args) = skull_mir::struct_defaults::args_at(
                ws, scan_inf, &scan_subst, e, *name, generics,
            ) else {
                continue;
            };
            let args = args.to_vec();
            let written: BTreeSet<ustr::Ustr> =
                fields.clone().map(|f| scan.struct_lit_field_arena[f].name).collect();
            let decl = &ws.modules[sid.module.index()].structs[sid.def];
            let key: Key = (
                sid.module.index(),
                sid.def.into_raw().into_u32(),
                args.iter().map(|a| a.canonical()).collect(),
            );
            for f in &decl.fields {
                if !written.contains(&f.name) && f.has_default {
                    omitted
                        .entry(key.clone())
                        .or_insert_with(|| (sid.module, sid.def, args.clone(), BTreeSet::new()))
                        .3
                        .insert(f.name);
                }
            }
            let now = omitted.get(&key).map(|(_, _, _, f)| f.len()).unwrap_or(0);
            if now > 0 && scanned.insert(key.clone(), now).unwrap_or(0) < now {
                fresh.push(key);
            }
        }
        for key in fresh {
            let (module, def, args, fields) = &omitted[&key];
            let owner = BodyOwner::StructDefaults(*def);
            let Some(defaults) = ws.modules[module.index()].bodies.get(&owner) else { continue };
            let Some(dinf) = ws.inference(*module).and_then(|ir| ir.per_fn.get(&owner)) else {
                continue;
            };
            let mut sub: FxHashSet<Idx<bone_semantic::hir::Expr>> = FxHashSet::default();
            for f in fields {
                let Some(root) = defaults.field_defaults.get(f) else { continue };
                sub.extend(crate::expr_subtree::expr_subtree(defaults, *root));
            }
            let inner = skull_mir::struct_defaults::slot_subst(Some(dinf), args);
            pending.push((defaults, dinf, inner, Some(sub)));
        }
    }

    for (_, (module, def, args, fields)) in omitted {
        let owner = BodyOwner::StructDefaults(def);
        let Some(defaults) = ws.modules[module.index()].bodies.get(&owner) else { continue };
        let Some(inf) = ws.inference(module).and_then(|ir| ir.per_fn.get(&owner)) else {
            continue;
        };
        let mut reached: FxHashSet<Idx<bone_semantic::hir::Expr>> = FxHashSet::default();
        for f in &fields {
            let Some(root) = defaults.field_defaults.get(f) else { continue };
            reached.extend(crate::expr_subtree::expr_subtree(defaults, *root));
        }
        // The initializers resolve under the STRUCT's slot bindings, not the
        // enclosing instance's -- the two are keyed in different bodies.
        let inner = skull_mir::struct_defaults::slot_subst(Some(inf), &args);
        let domain = SiteDomain::StructDefaults(module, def, args);
        w.out.struct_default_domains.insert(domain.clone());
        walk_body_edges(
            w,
            ws,
            inf,
            Some(defaults),
            from,
            &inner,
            &domain,
            Some(&reached),
        );
    }
}

/// One `next` witness plus, recursively, the witnesses of ITS bounded generics.
///
/// A chained iterator resolves `next(MapIter<VecIter<..>>)` to MapIter's
/// `next`; only the outermost witness is named in `info.next`, so `sub` carries
/// the inner witnesses.
#[allow(clippy::too_many_arguments)]
fn record_bound_next(
    w: &mut Walk,
    ws: &Workspace,
    inf: &BodyInference,
    from: Option<&Instance>,
    subst: &MonoSubst,
    next: &BoundNext,
    span: Span,
    kind: EdgeKind,
    expr: Idx<bone_semantic::hir::Expr>,
    root: BoundRoot,
    domain: &SiteDomain,
    path: &[usize],
) -> Option<InstanceBoundNext> {
    let site = root.site(domain.clone(), expr, path.to_vec());
    if let BoundNext::CallerGeneric(gidx) = next {
        let owner_tree = w.owner_tree;
        let resolved = from.and(owner_tree).and_then(|tree| tree.next.get(gidx)).cloned();
        let Some(resolved) = resolved else {
            w.record_site(from, site, SiteOutcome::Failed("caller-generic-witness-unbound"), span);
            return None;
        };
        if let Some(from) = from {
            w.push_edge(Edge {
                from: from.clone(),
                to: resolved.callee.clone(),
                kind,
                span,
            });
        }
        w.record_site(from, site, SiteOutcome::Resolved(resolved.callee.clone()), span);
        return Some(resolved);
    }

    let BoundNext::Concrete { func: (m, f), next_generics, sub } = next else {
        unreachable!("CallerGeneric handled above")
    };
    let rep = FuncRef { module: *m, func: *f };
    let resolved = &ws.module(rep.module).functions[rep.func];
    let callee = rep;
    let cf = &ws.module(callee.module).functions[callee.func];
    if from.is_none() {
        w.out.always_emitted_seeds += 1;
    }
    let conversion_start = w.timer();
    let instantiated = w.with_interner_mut(|a| {
        instantiate_witness(a, ws, inf, resolved, cf, next_generics, subst)
    });
    add_elapsed(&mut w.out.timing.signature_type_conversion_ns, conversion_start);
    match instantiated {
        Ok((args, callee_subst)) => {
            let to = Instance { func: callee, args };
            match from {
                Some(from) => {
                    w.push_edge(Edge { from: from.clone(), to: to.clone(), kind, span })
                }
                // See `record_call`: an always-emitted body's callee is a root.
                None => {
                    w.out.always_emitted_targets.insert(to.clone());
                }
            }
            w.record_site(from, site, SiteOutcome::Resolved(to.clone()), span);
            w.reach(to.clone(), callee_subst);
            let concrete_sub = walk_bound_tree(
                w, ws, inf, from, subst, sub, span, expr, root, domain, path,
            );
            w.record_bound_tree(&to, concrete_sub.clone());
            Some(InstanceBoundNext { callee: to, sub: concrete_sub })
        }
        // See `record_call`: the classification survives the absence of a
        // `from` instance.
        Err(Fail::Unconstrained(_)) if from.is_none() => {
            w.record_always_emitted_hole(
                mangle_fn(ws, cf),
                AlwaysEmittedOutcome::Rejection(RejectionKind::UnconstrainedTypeVar),
                span,
            );
            w.record_site(from, site, SiteOutcome::Failed("unconstrained-type-var"), span);
            None
        }
        Err(fail) if from.is_none() => {
            w.record_always_emitted_hole(
                mangle_fn(ws, cf),
                AlwaysEmittedOutcome::Unresolved(fail.reason()),
                span,
            );
            w.record_site(from, site, SiteOutcome::Failed(fail.reason()), span);
            None
        }
        Err(Fail::Unconstrained(_)) => {
            w.push_rejection(Rejection {
                kind: RejectionKind::UnconstrainedTypeVar,
                at: from.expect("guarded above").clone(),
                span,
                message: format!(
                    "the `next` witness `{}` binds a type argument no constraint reaches",
                    mangle_fn(ws, cf)
                ),
            });
            w.record_site(from, site, SiteOutcome::Failed("unconstrained-type-var"), span);
            None
        }
        Err(fail) => {
            w.push_unresolved(Unresolved {
                from: from.expect("guarded above").clone(),
                callee,
                reason: fail.reason(),
                span,
            });
            w.record_site(from, site, SiteOutcome::Failed(fail.reason()), span);
            None
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn walk_bound_tree(
    w: &mut Walk,
    ws: &Workspace,
    inf: &BodyInference,
    from: Option<&Instance>,
    subst: &MonoSubst,
    tree: &bone_semantic::prelude::BoundTree,
    span: Span,
    expr: Idx<bone_semantic::hir::Expr>,
    root: BoundRoot,
    domain: &SiteDomain,
    path: &[usize],
) -> InstanceBoundTree {
    let started = w.timer();
    let mut resolved = InstanceBoundTree::default();
    let mut keys: Vec<&usize> = tree.next.keys().collect();
    keys.sort();
    for k in keys {
        // The conformance PATH, not the parameter index alone: two witnesses at
        // different depths of one tree can carry the same key.
        let mut child = path.to_vec();
        child.push(*k);
        if let Some(next) = record_bound_next(
            w, ws, inf, from, subst, &tree.next[k], span,
            EdgeKind::BoundNextWitness, expr, root, domain, &child,
        ) {
            resolved.next.insert(*k, next);
        }
    }
    add_elapsed(&mut w.out.timing.witness_bound_tree_ns, started);
    resolved
}

/// The callee's instantiation: its generic bindings converted to `MonoTy` under
/// the CALLER's substitution, plus the substitution to walk the callee's body
/// with. This is the transitive-instantiation step.
fn instantiate(
    arena: &mut MonoArena,
    ws: &Workspace,
    inf: &BodyInference,
    callee_module: ModuleId,
    callee_func: Idx<Function>,
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
        let ty = match skull_mir::monoty::convert(ws, inf, *b, caller_subst) {
            Ok(ty) => ty,
            Err(reason)
                if matches!(
                    reason,
                    MonoTyReason::UnresolvedInfer(_) | MonoTyReason::UnboundGeneric(_)
                ) && !generic_parameter_is_observable(ws, callee_module, callee_func, callee, var) =>
            {
                // A generic slot that appears nowhere in the callee's signature,
                // bounds, or inferred body cannot affect its concrete plan. Keep
                // the instance total with an explicit erased logical type; an
                // unresolved slot that the body can read still rejects below.
                skull_mir::monoty::MonoTy::Any
            }
            Err(reason) => return Err(convert_fail(reason)),
        };
        args.push(arena.intern(ty.clone()));
        callee_subst.insert(var, ty);
    }
    Ok((args, callee_subst))
}

fn generic_parameter_is_observable(
    ws: &Workspace,
    callee_module: ModuleId,
    callee_func: Idx<Function>,
    callee: &Function,
    var: VarId,
) -> bool {
    fn mentions(
        ws: &Workspace,
        inf: Option<&BodyInference>,
        ty: TyId,
        wanted: VarId,
        seen: &mut FxHashSet<TyId>,
    ) -> bool {
        if !seen.insert(ty) {
            return false;
        }
        let node = match ty {
            TyId::Static(mid, idx) => ws.module(mid).ty_arena[idx],
            TyId::Local(idx) => {
                let Some(inf) = inf else { return false };
                inf.ty_arena[idx]
            }
        };
        match node {
            TypeExpr::Generic(found, _) => found == wanted,
            TypeExpr::Nullable(inner) => mentions(ws, inf, ty.with_idx(inner), wanted, seen),
            TypeExpr::App { params, .. } => params
                .into_iter()
                .any(|param| mentions(ws, inf, ty.with_idx(param), wanted, seen)),
            TypeExpr::Function { params, ret } => {
                params
                    .into_iter()
                    .any(|param| mentions(ws, inf, ty.with_idx(param), wanted, seen))
                    || mentions(ws, inf, ty.with_idx(ret), wanted, seen)
            }
            TypeExpr::Tuple { elements } => elements
                .into_iter()
                .any(|element| mentions(ws, inf, ty.with_idx(element), wanted, seen)),
            TypeExpr::Infer(next, _) => inf
                .and_then(|inf| inf.substitutions.get(next))
                .is_some_and(|next| mentions(ws, inf, next, wanted, seen)),
            TypeExpr::Builtin(_) => false,
        }
    }

    let mut seen = FxHashSet::default();
    let mut declaration_types = callee
        .param_decls
        .iter()
        .map(|param| param.ty)
        .chain(callee.return_ty)
        .chain(callee.generic_bounds.iter().flatten().copied());
    if declaration_types.any(|ty| mentions(ws, None, ty, var, &mut seen)) {
        return true;
    }

    let owner = BodyOwner::Function(callee_func);
    let Some(inf) = ws.inference(callee_module).and_then(|ir| ir.per_fn.get(&owner)) else {
        return false;
    };
    if inf
        .expr_types
        .values()
        .chain(inf.binding_types.values())
        .any(|ty| mentions(ws, Some(inf), *ty, var, &mut seen))
    {
        return true;
    }
    ws.module(callee_module)
        .bodies
        .get(&owner)
        .into_iter()
        .flat_map(|body| body.binding_arena.iter().filter_map(|(_, binding)| binding.declared_ty))
        .any(|ty| mentions(ws, Some(inf), ty, var, &mut seen))
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
        // bone's own resolution is already the representative; the fallback
        // above picked an arbitrary member of a group that manglings proved
        // uniform, so it is canonicalized to the same one.
        out.push((e, skull_mir::lower::canonical_variant(ws, rep)));
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
fn sort_entry(elem: &skull_mir::mir::ColumnElem) -> &'static str {
    if elem.is_nobox() {
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
    examined: &mut usize,
    failures: &mut Vec<(Idx<bone_semantic::hir::Expr>, MonoTyReason)>,
    approximated: &mut usize,
) -> Vec<GeneratedSite> {
    use bone_semantic::hir::{Callee, Expr as E};
    let rep_arena = RepArena::default();
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
        let plan = match cond {
            // `Any`, stated rather than defaulted: this graph selects no
            // target sibling. The physical target projection happens later;
            // only the logical container plan is needed for the edge census.
            StorageCond::VecColumn => {
                if !ContainerPlan::matches_type(ContainerFamily::Vec, &mono) {
                    return CondVerdict::DoesNot;
                }
                ContainerPlan::for_type(
                    ws,
                    bone_semantic::hir::FnBackend::Any,
                    &rep_arena,
                    ContainerFamily::Vec,
                    &mono,
                )
            }
            StorageCond::MapColumn => {
                if !ContainerPlan::matches_type(ContainerFamily::Map, &mono) {
                    return CondVerdict::DoesNot;
                }
                ContainerPlan::for_type(
                    ws,
                    bone_semantic::hir::FnBackend::Any,
                    &rep_arena,
                    ContainerFamily::Map,
                    &mono,
                )
            }
            StorageCond::Always => return CondVerdict::Holds(None),
        };
        let elem = match plan {
            Ok(plan) => plan.column_elem(&rep_arena),
            Err(_) => return CondVerdict::Failed(MonoTyReason::NoNode),
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
            E::Call { callee: Callee::Unresolved { name, .. }, args, .. } => {
                let Some((trigger, cond, names)) =
                    COMPILER_GENERATED_TRIGGERS.iter().find(|(t, _, _)| *t == name.as_str())
                else {
                    continue;
                };
                if *trigger == "map_new" {
                    if args.clone().next().is_some() {
                        continue;
                    }
                    let Some(_) = settle(e, *cond, e) else { continue };
                    out.extend(names.iter().map(|n| GeneratedSite { expr: e, name: n }));
                    continue;
                }
                // The receiver is the UFCS first argument; an arm with no
                // receiver cannot be the column form.
                let Some(recv) = args.clone().next() else { continue };
                let Some(elem) = settle(recv, *cond, e) else { continue };
                // The one trigger with alternatives: the element decides which.
                if *trigger == "__vec_data_sort" {
                    let Some(elem) = elem else { continue };
                    out.push(GeneratedSite { expr: e, name: sort_entry(&elem) });
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

/// Every callee obligation a CONDITIONALLY-synthesized call carries in this
/// body, under this instance's substitution.
///
/// The two constructs -- an interpolation hole and `==`/`!=` -- name no callee,
/// so [`COMPILER_GENERATED_TRIGGERS`]' name key cannot reach them. Neither does
/// this function decide anything: it asks `skull_mir::lower::dispatch`, the
/// same plan lowering RENDERS, so the walk cannot be weaker than the pass.
fn dispatched_sites(
    ws: &Workspace,
    inf: &BodyInference,
    body: &bone_semantic::hir::Body,
    subst: &MonoSubst,
    domain: &SiteDomain,
) -> Vec<(Site, &'static str)> {
    use bone_semantic::hir::{BinOp, Expr as E, InterpPart};
    let rep_arena = RepArena::default();
    let dcx = skull_mir::lower::DispatchCtx {
        ws,
        inf: Some(inf),
        body,
        concrete_plan: None,
        concrete_types: None,
        concrete_type_cache: None,
        lower_attribution: None,
        interner: None,
        rep_arena: &rep_arena,
        target: bone_semantic::hir::FnBackend::Any,
        logical_subst: Some(subst),
        values_first_class: false,
    };
    let mut out: Vec<(Site, &'static str)> = Vec::new();
    for (e, expr) in body.expr_arena.iter() {
        match expr {
            E::InterpolatedString(parts) => {
                for p in *parts {
                    let InterpPart::Value { expr: hole, spec } = &body.interp_part_arena[p] else {
                        continue;
                    };
                    for (path, name) in dcx.hole_plan(*hole, spec.as_ref()).obligations() {
                        out.push((
                            Site { domain: domain.clone(), expr: *hole, op: SiteOp::Dispatched { name, path } },
                            name,
                        ));
                    }
                }
            }
            E::BinOp { op: op @ (BinOp::Eq | BinOp::NotEq), lhs, rhs } => {
                if let Some(name) = dcx.eq_route(*lhs, *rhs).synthesized_callee(*op) {
                    out.push((
                        Site { domain: domain.clone(), expr: e, op: SiteOp::Dispatched { name, path: Vec::new() } },
                        name,
                    ));
                }
            }
            _ => {}
        }
    }
    out.sort();
    out
}

/// The GROUP REPRESENTATIVE of the stdlib function `name` -- the target-neutral
/// form of the lookup `Builder::stdlib_funcref` performs, and it fails on the
/// same conditions (missing name, residual ambiguity).
///
/// A compiler-generated entry may be declared as a backend-variant group
/// (`inline(lua)` + `inline(js)` of one signature). Filtering candidates by a
/// TARGET would put target selection back into the walk, and filtering by `Any`
/// -- which serves everything -- would call every such group ambiguous. So every
/// declaration is canonicalized through the ONE grouping oracle and the answer
/// must be unanimous: a name whose declarations are NOT one backend-variant
/// group is the residual ambiguity `stdlib_funcref` also refuses.
pub fn stdlib_entry_representative(ws: &Workspace, name: &str) -> Option<FuncRef> {
    let decls = ws.functions_by_name.get(&ustr::Ustr::from(name))?;
    let mut reps = decls
        .iter()
        .map(|&(m, f)| skull_mir::lower::canonical_variant(ws, FuncRef { module: m, func: f }));
    let first = reps.next()?;
    reps.all(|r| r == first).then_some(first)
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
    skull_mir::lower::declared_boundary_class(f)
}

/// Whether the declaration carries a portable Rock body, as opposed to only a
/// foreign one. `has_body` on the module is the walk's own question; this is the
/// declaration-level form the classifier needs before any module lookup.
/// Runtime entries an opaque inline body CALLS.
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
fn record_inline_body_deps(g: &mut InstanceGraph, ws: &Workspace, decls: &[DeclKey]) {
    // METADATA ONLY so far: each sibling's dependency is recorded and joins
    // `erased_entries`, but it is not yet resolved to a `BoundaryId` or an
    // instance the way a call edge is. That resolution belongs to the artifact
    // view, which is what decides the emitted spelling and the retention set;
    // recording it here without a target would be a second, weaker reachability
    // mechanism. All members of a represented backend-variant group are scanned;
    // the artifact later reads only the declaration it selected.
    for decl in decls {
        for member in skull_mir::lower::backend_variant_group(ws, decl.0) {
            let member = DeclKey(member);
            let f = &ws.module(member.0.module).functions[member.0.func];
            let Some(body) = &f.inline_body else { continue };
            let text = body.1.as_str();
            for name in ERASED_RUNTIME_ENTRIES {
                if text.contains(name) {
                    g.inline_body_deps.entry(member).or_default().insert(name);
                }
            }
        }
    }
}

/// Resolve bare `@nomangle` names in JS opaque bodies into root declarations.
/// The body remains opaque to MIR; this is only the publication obligation for
/// symbols the text calls by source name.
pub fn opaque_boundary_roots_for_decls(
    ws: &Workspace,
    decls: impl IntoIterator<Item = FuncRef>,
) -> BTreeMap<DeclKey, FxHashSet<FuncRef>> {
    let mut roots = BTreeMap::new();
    for decl in decls {
        for member in skull_mir::lower::backend_variant_group(ws, decl) {
            let owner = DeclKey(member);
            let targets = roots.entry(owner).or_insert_with(FxHashSet::default);
            let f = &ws.module(member.module).functions[member.func];
            let Some((bone_semantic::hir::FnBackend::Js, body)) = f.inline_body else {
                continue;
            };
            let text = body.as_str();
            let bytes = text.as_bytes();
            let mut i = 0;
            while i < bytes.len() {
                let c = bytes[i] as char;
                if c.is_ascii_alphabetic() || c == '_' {
                    let start = i;
                    while i < bytes.len()
                        && ((bytes[i] as char).is_ascii_alphanumeric() || bytes[i] == b'_')
                    {
                        i += 1;
                    }
                    let name = ustr::Ustr::from(&text[start..i]);
                    if let Some(candidates) = ws.functions_by_name.get(&name) {
                        for &(target_module, target_func) in candidates {
                            let target = &ws.module(target_module).functions[target_func];
                            if target
                                .annotations
                                .iter()
                                .any(|annotation| annotation.name.name.as_str() == "nomangle")
                            {
                                targets.insert(FuncRef {
                                    module: target_module,
                                    func: target_func,
                                });
                            }
                        }
                    }
                } else {
                    i += 1;
                }
            }
        }
    }
    roots
}

fn record_inline_body_nomangle_roots(
    g: &mut InstanceGraph,
    ws: &Workspace,
    decls: impl IntoIterator<Item = FuncRef>,
) {
    g.inline_body_nomangle_roots
        .extend(opaque_boundary_roots_for_decls(ws, decls));
}

fn classify_boundaries(g: &mut InstanceGraph, ws: &Workspace, decls: &[DeclKey]) {
    let erased: BTreeSet<String> = g.erased_entries.clone();
    for decl in decls {
        let f = &ws.module(decl.0.module).functions[decl.0.func];
        let symbol = mangle_fn(ws, f);
        let Some(class) = boundary_class_for(f, &erased) else { continue };
        g.boundaries.insert(symbol.clone(), Boundary { class, symbol });
    }
}

/// The helper symbols each coroutine instance owns.
///
/// The suffix list comes from the instance's own [`CoroutinePlan`], which is
/// the emitters' input too. `__advance` exists only on the concrete frame-slot
/// channel, and the base is the instance symbol used by its emitted body.
///
/// Defers (`__defer<k>`) and lifecycle hooks (`__on_*`) are NOT counted: they
/// come from `CoroLowered`, which exists only after the MIR transform, so there
/// is no graph-side oracle for them.
/// Keyed by INSTANCE, so it is refreshed for the instances that entered or
/// left rather than for the whole graph. `removed` runs first: an instance that
/// was replaced appears in both lists and must end up recorded.
fn record_coroutine_helpers(
    g: &mut InstanceGraph,
    ws: &Workspace,
    added: &[Instance],
    removed: &[Instance],
) {
    for i in removed {
        let identity = g.identity(ws, i);
        g.coroutine_helpers.retain(|h| h.owner_identity != identity);
    }
    for i in added {
        let f = &ws.module(i.func.module).functions[i.func.func];
        if !f.is_coroutine {
            continue;
        }
        let plan = match crate::concrete_body::concrete_signature_for_instance(ws, g, i).and_then(
            |signature| {
                skull_mir::lower::CoroutinePlan::from_concrete_signature(
                    ws,
                    g.interner(),
                    &signature,
                    i.symbol(ws, g.interner()),
                )
            },
        ) {
            Ok(plan) => plan,
            Err(reason) => {
                g.graph_rejections.push(Rejection {
                    kind: RejectionKind::ConcreteTypeUnavailable,
                    at: i.clone(),
                    span: f.name_span,
                    message: format!(
                        "cannot build concrete coroutine plan for `{}`: {}",
                        f.name,
                        reason.bucket()
                    ),
                });
                continue;
            }
        };
        let owner_identity = g.identity(ws, i);
        for suffix in plan.declared_suffixes() {
            g.coroutine_helpers.insert(CoroutineHelper {
                owner_identity: owner_identity.clone(),
                suffix,
                symbol_today: format!("{}{suffix}", plan.base()),
            });
        }
    }
}

impl InstanceGraph {
    /// Helper symbols that more than one INSTANCE claims.
    ///
    /// A generic `co fn` whose call site SPECIALIZES gets its own base
    /// (`spec_fn_name`) and no longer collides; one whose specialization the
    /// lowerer declined still shares the declaration's, so this reports the
    /// declined population. E1 removes the category by making every instance an
    /// emitted body.
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

/// Coverage of concrete generic call identities by the instance graph.
#[derive(Default, Debug)]
pub struct CallIdentityCensus {
    /// Complete generic call identities recovered from typed bindings.
    pub calls: usize,
    /// ...of which the instance graph contains a matching instance.
    pub matched_instances: usize,
    /// ...of which it does NOT, and whose CALLER the instance graph did reach.
    /// Each of these is a failure, not a difference.
    pub missing: Vec<String>,
    /// ...of which it does not, but whose caller the instance graph never
    /// reached either. This is a reachability difference, not a missing
    /// instance.
    pub unreached_callers: usize,
    /// Call sites whose identity could not be stated from compiler state.
    pub unresolved_sites: usize,
    /// Graph instances with no counterpart in the direct-call census.
    pub graph_only: usize,
}

/// Both sides encode with `MonoTy::canonical`, so a pretty-printing ambiguity
/// cannot match two different types to each other.
fn slots_match(slots: &[String], instance: &[String]) -> bool {
    slots == instance
}

pub fn reconcile_call_identities(ws: &Workspace, g: &InstanceGraph) -> CallIdentityCensus {
    let (calls, unresolved_sites) = skull_mir::spec_census::concrete_calls(ws);

    // Graph instances grouped by function, as canonical argument lists.
    let mut by_func: FxHashMap<(u32, u32), Vec<Vec<String>>> = FxHashMap::default();
    for i in &g.instances {
        by_func
            .entry((i.func.module.raw(), i.func.func.into_raw().into_u32()))
            .or_default()
            .push(i.args.iter().map(|a| g.interner.canonical(*a)).collect());
    }

    let mut r = CallIdentityCensus {
        calls: calls.len(),
        unresolved_sites,
        ..Default::default()
    };
    let mut matched_instances: std::collections::HashSet<(u32, u32, Vec<String>)> =
        Default::default();
    for t in &calls {
        let k = (t.func.module.raw(), t.func.func.into_raw().into_u32());
        let hit = by_func
            .get(&k)
            .map(|args| args.iter().find(|a| slots_match(&t.slots, a)))
            .unwrap_or(None);
        match hit {
            Some(a) => {
                r.matched_instances += 1;
                matched_instances.insert((k.0, k.1, a.clone()));
            }
            None => {
                let ck = (t.caller.module.raw(), t.caller.func.into_raw().into_u32());
                if !by_func.contains_key(&ck) {
                    r.unreached_callers += 1;
                    continue;
                }
                let f = &ws.module(t.func.module).functions[t.func.func];
                let caller = &ws.module(t.caller.module).functions[t.caller.func];
                let slots: Vec<&str> = t.slots.iter().map(String::as_str).collect();
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
    r.graph_only = g
        .instances
        .iter()
        .filter(|i| {
            let k = (i.func.module.raw(), i.func.func.into_raw().into_u32());
            let args: Vec<String> = i.args.iter().map(|a| g.interner.canonical(*a)).collect();
            !matched_instances.contains(&(k.0, k.1, args))
        })
        .count();
    r
}
