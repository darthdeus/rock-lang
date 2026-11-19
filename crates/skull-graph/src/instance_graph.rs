//! Walk checked bodies from the supplied roots and collect concrete instances.
//! Each contribution belongs to one body and can be replaced on an edit.

use bone_core::prelude::Instant;
use parking_lot::Mutex;
use std::collections::{BTreeMap, BTreeSet};

use bone_core::hir::{Body, BodyOwner, Function, TypeExpr, VarId};
use bone_core::prelude::{BoundNext, IterWitness, ModuleId, Span, TyId};
use bone_core::workspace::{BodyInference, Workspace};
use comfy_la_arena::Idx;
use rustc_hash::{FxHashMap, FxHashSet};
use skull_mir::lower::RepArena;
use skull_mir::mangle::mangle_fn;
use skull_mir::mir::FuncRef;
use skull_mir::monoty::{MonoInterner, MonoSubst, MonoTyId, MonoTyReason};

/// The graph builds instances and shares their type arena with emission.
pub use skull_mir::instance::{DeclKey, Instance};

/// The boundary classifier lives below the graph, so artifact projection and
/// lowering-side rejection cannot drift into separate vocabularies.
pub use skull_mir::lower::BoundaryClass;

/// Boundary classification at the shared declaration, before target selection.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Boundary {
    pub class: BoundaryClass,
    pub symbol: String,
}

/// A helper symbol a coroutine instance OWNS. Not an [`Edge`]: the target is
/// not another instance.
///
/// Helper names carry the owning instance to distinguish instantiations of
/// one generic coroutine.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct CoroutineHelper {
    pub owner_identity: String,
    pub suffix: &'static str,
    /// What emission produces TODAY: mangled source name + suffix.
    pub symbol_today: String,
}

#[derive(Clone, Debug)]
pub struct Edge {
    pub to: Instance,
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
    fn site(self, domain: SiteDomain, expr: Idx<bone_core::hir::Expr>, path: Vec<usize>) -> Site {
        Site {
            domain,
            expr,
            op: match self {
                Self::For => SiteOp::ForNext(path.into()),
                Self::Call => SiteOp::BoundNext(path.into()),
            },
        }
    }
}

/// A construction the concrete-program model REFUSES, as opposed to one it
/// merely could not complete yet ([`Unresolved`]).
///
/// `ConcreteTypeUnavailable` records a compiler invariant; other variants
/// reject author programs.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum RejectionKind {
    /// A function that recurs at a DIFFERENT instantiation than it was reached
    /// at. Demand-driven instantiation does not terminate on it.
    ChangingInstanceRecursion,
    /// A generic function taken as a VALUE. There is no call site to key an
    /// instantiation on, and inferring one from the instances reachable
    /// elsewhere is explicitly not the rule.
    GenericFnValue,
    /// A concrete type required by a coroutine helper was unavailable.
    ConcreteTypeUnavailable,
    /// A type argument no constraint reaches, so it has no concrete type to
    /// instantiate at. Distinct from a declared generic read inside a shared
    /// body, which is [`Unresolved`] data.
    UnconstrainedTypeVar,
    /// An identity struct in an ordinary storage position that only the
    /// instance's substitution shows.
    /// Fatal for the package whose call site instantiated it.
    IdentityInOrdinaryStorage,
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
/// body.
#[derive(Clone, Debug)]
pub struct Unresolved {
    pub from: Instance,
    pub callee: FuncRef,
    pub reason: &'static str,
    pub span: Span,
}

/// The key of a body contribution. Globals and decorators have no Instance.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
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

/// All facts produced by walking one body.
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
    pub unresolved: Vec<Unresolved>,
    pub rejections: Vec<Rejection>,
    pub always_emitted_targets: BTreeSet<Instance>,
    /// Every instance this walk reached, with the substitution it reached it
    /// UNDER. [`InstanceGraph::substitutions`] is derived from these, so an
    /// instance whose only caller is gone loses its substitution with it.
    pub reached: Vec<(Instance, MonoSubst)>,
    /// Concrete conformance witnesses this walk supplied to its callees.
    pub bound_trees: Vec<(Instance, InstanceBoundTree)>,
    /// Source files referenced by recorded spans, for relocation on body edits.
    pub span_files: BTreeSet<ustr::Ustr>,
    /// Whether the walk ENTERED the body. False for a boundary, a body with no
    /// usable inference, and a depth-capped instance.
    pub expanded: bool,
}

#[derive(Clone)]
struct RecursionFinding {
    rejection: Rejection,
    /// Instances whose outgoing contributions the witness search inspected.
    /// Retraction is indexed by these subjects, so changing an intermediate
    /// body cannot leave a finding owned by an untouched start behind.
    subjects: BTreeSet<Instance>,
}

/// Scratch caches reused across an owner walk's sites.
#[derive(Default)]
struct WalkScratch {
    /// Where the walk asks how a type is represented.
    rep_arena: RepArena,
    identity_verdicts: std::cell::RefCell<crate::identity_storage::IdentityVerdicts>,
}

struct Walk<'a> {
    interner: &'a MonoInterner,
    scratch: &'a WalkScratch,
    owner_tree: Option<&'a InstanceBoundTree>,
    out: &'a mut Contribution,
    /// The file of the last span recorded: a body's spans are nearly all in
    /// one file, and the set insert is per fact.
    last_file: Option<ustr::Ustr>,
}

impl Walk<'_> {
    fn record_site(
        &mut self,
        from: Option<&Instance>,
        site: Site,
        outcome: SiteOutcome,
        span: Span,
    ) {
        self.saw(span);
        let sites = if from.is_some() {
            &mut self.out.call_sites
        } else {
            &mut self.out.always_call_sites
        };
        sites.insert(site, SiteRecord { outcome, span });
    }

    fn record_caller_witness(
        &mut self,
        from: Option<&Instance>,
        generic: usize,
        site: Site,
        span: Span,
    ) -> Option<InstanceBoundNext> {
        let resolved = from
            .and(self.owner_tree)
            .and_then(|tree| tree.next.get(&generic))
            .cloned();
        let outcome = match &resolved {
            Some(resolved) => {
                let callee = resolved.callee.clone();
                self.push_edge(Edge {
                    to: callee.clone(),
                    span,
                });
                SiteOutcome::Resolved(callee)
            }
            None => SiteOutcome::Failed("caller-generic-witness-unbound"),
        };
        self.record_site(from, site, outcome, span);
        resolved
    }

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
        if !is_spanless(&span) && self.last_file != Some(span.file) {
            self.last_file = Some(span.file);
            self.out.span_files.insert(span.file);
        }
    }
}

/// A root's substitution, which is empty rather than absent.
static EMPTY_SUBST: std::sync::LazyLock<MonoSubst> = std::sync::LazyLock::new(MonoSubst::default);

/// Shared indexes over contributions. When owners disagree, the first owner decides.
#[derive(Default, Debug, Clone)]
pub struct Indices {
    /// Instance -> who reached it and what they said. Hashed: every reached
    /// row of every install looks one up, and the ordered readers sort.
    pub subst: FxHashMap<Instance, Agreed<MonoSubst>>,
    /// Instance -> who supplied its conformance witnesses and what they said.
    pub trees: BTreeMap<Instance, Agreed<InstanceBoundTree>>,
    /// Instance -> the always-emitted bodies that root it.
    pub always_emitted: BTreeMap<Instance, BTreeSet<Owner>>,
    /// Declaration -> how many LIVE instances it has.
    ///
    /// The three per-declaration post-passes read the declaration set, not the
    /// instance set, so this is what tells them which declarations ENTERED or
    /// LEFT the graph -- the only ones whose answer can have moved.
    pub decls: FxHashMap<FuncRef, usize>,
    /// Predecessors for incremental pruning. Hashed because no reader needs edge order.
    pub preds: FxHashMap<Instance, FxHashSet<Instance>>,
    /// Declaration -> its live concrete instances. Body edits use this index
    /// to revisit every specialization without scanning the whole graph.
    pub decl_instances: FxHashMap<FuncRef, FxHashSet<Instance>>,
    /// File -> owners carrying spans in that file. Coordinate relocation uses
    /// this set; membership does not make an owner semantically stale.
    pub span_owners: FxHashMap<ustr::Ustr, BTreeSet<Owner>>,
}

/// The body edit's coordinate change; the graph moves its stored spans with
/// the same policy as the session and the lint cache.
pub use bone_core::text_edit::TextEditDelta;

/// Store one copy of an agreed fact and retain disagreements per owner.
#[derive(Clone, Debug)]
pub struct Agreed<T> {
    /// Owners asserting [`Self::value`].
    pub owners: BTreeSet<Owner>,
    pub value: T,
    /// Owners asserting something else, each with what it said. Empty on every
    /// valid program -- a non-empty one means the fact is not a function of the
    /// instance, so a body built from it would depend on which owner won.
    pub dissent: BTreeMap<Owner, T>,
}

impl<T: Clone + PartialEq> Agreed<T> {
    fn new(owner: Owner, value: T) -> Self {
        Agreed {
            owners: BTreeSet::from([owner]),
            value,
            dissent: BTreeMap::new(),
        }
    }

    /// A row for a fresh build, which records no owners; see
    /// [`InstanceGraph::ensure_incremental_indexes`].
    fn unowned(value: T) -> Self {
        Agreed {
            owners: BTreeSet::new(),
            value,
            dissent: BTreeMap::new(),
        }
    }

    fn assert_unowned(&mut self, value: &T) -> bool {
        *value == self.value
    }

    fn assert_by(&mut self, owner: &Owner, value: &T) {
        if *value == self.value {
            // A body reaching the same callee from several sites asserts the
            // fact once per site; only the first needs an owned key.
            if !self.owners.contains(owner) {
                self.owners.insert(owner.clone());
            }
        } else {
            self.dissent.insert(owner.clone(), value.clone());
        }
    }

    /// Drop one owner's assertions. Returns false when nothing is left.
    fn retract(&mut self, owner: &Owner) -> bool {
        self.owners.remove(owner);
        self.dissent.remove(owner);
        if self.owners.is_empty() {
            // The agreeing side is gone; a dissenter becomes the answer, or
            // there is no answer left at all.
            let Some(first) = self.dissent.keys().next().cloned() else {
                return false;
            };
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

fn assert_subst(
    index: &mut FxHashMap<Instance, Agreed<MonoSubst>>,
    owner: &Owner,
    to: &Instance,
    subst: &MonoSubst,
) {
    match index.get_mut(to) {
        Some(a) => a.assert_by(owner, subst),
        None => {
            index.insert(to.clone(), Agreed::new(owner.clone(), subst.clone()));
        }
    }
}

#[derive(Clone)]
pub struct InstanceGraph {
    /// Whether the update-only reverse indexes (`preds`, `decl_instances`,
    /// `span_owners`) are maintained. A fresh build has no reader for them;
    /// the first update derives them from the contributions and keeps them
    /// current from then on.
    incremental_indexes: bool,
    /// How many updates this graph has applied. A retained view derives its
    /// update-only rows from the graph it was realized from, so it checks the
    /// revision it saw rather than trusting the call order.
    revision: u64,
    /// The arena whose type IDs key instances and their lowering plans.
    interner: std::sync::Arc<MonoInterner>,
    /// The primary records. Every flat field below is derived from these plus
    /// [`Self::root_instances`]; nothing writes a flat field directly.
    pub contributions: BTreeMap<Owner, Box<Contribution>>,
    /// Instances production roots regardless of any call. A reachability walk
    /// starts here and at every always-emitted target.
    pub root_instances: BTreeSet<Instance>,
    pub instances: BTreeSet<Instance>,
    /// The last expansion census. It is diagnostic state, not part of graph
    /// identity; keeping it here makes the denominator available to tests and
    /// callers without a second graph walk.
    pub run_stats: RunStats,
    last_run_owners: BTreeSet<Owner>,
    /// Instances an always-emitted owner rooted before or after this run's
    /// re-walk. Its owner is not an instance, so without these the instances
    /// whose root status or fn-value status moved would not be revisited.
    last_run_always_targets: BTreeSet<Instance>,
    delta_added: BTreeSet<Instance>,
    delta_removed: BTreeSet<Instance>,
    /// Constructions the model refuses that no single body produced:
    /// [`detect_changing_instance_recursion`] is a WHOLE-GRAPH search over the
    /// edge set, so its findings belong to the graph rather than to an owner.
    /// Every other rejection lives in the contribution whose walk raised it;
    /// [`InstanceGraph::rejections`] is the two of them together.
    pub graph_rejections: Vec<Rejection>,
    recursion_findings: BTreeMap<Instance, RecursionFinding>,
    recursion_by_subject: BTreeMap<Instance, BTreeSet<Instance>>,
    recursion_by_decl: BTreeMap<DeclKey, BTreeSet<Instance>>,
    recursion_positions: BTreeMap<Instance, usize>,
    coroutine_positions: BTreeMap<Instance, usize>,
    /// Every reached boundary, classified by its calling contract.
    pub boundaries: BTreeMap<String, Boundary>,
    /// Helper symbols owned by coroutine instances.
    pub coroutine_helpers: BTreeSet<CoroutineHelper>,
    coroutine_helpers_by_owner: BTreeMap<String, BTreeSet<CoroutineHelper>>,
    /// Bare `@nomangle` declarations named by retained `inline(js)` bodies.
    /// The graph records the opaque-boundary root obligation; a target view
    /// filters it to the declarations it actually realizes.
    pub inline_body_nomangle_roots: BTreeMap<DeclKey, FxHashSet<FuncRef>>,
    /// Targets that may have lost reachability and need a pruning check.
    lost: BTreeSet<Instance>,
    /// **The cross-owner facts, maintained where a contribution is installed
    /// or removed.** Public for the same reason [`Self::contributions`] is --
    /// the oracle and its tests construct states directly -- but consumers ask
    /// through the accessors, which are the one place the agreed answer is
    /// decided.
    pub index: Indices,
}

/// Every compiler-owned runtime entry that a synthesized call or an opaque
/// body may name under its bare name. An entry need not serve every target:
/// `Builder::stdlib_funcref` rejects a call on a target the stdlib does not
/// serve.
pub const ERASED_RUNTIME_ENTRIES: &[&str] = &[
    "__descriptor_with",
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
    "__rock_materialize_strided_element",
    "__rock_stride_capacity",
    "__rock_stride_copy",
    "__rock_stride_insert",
    "__rock_stride_reserve",
    "__rock_stride_sort",
    "__rock_stride_sort_leaves",
    "__rock_strided_column_view",
    "__rock_vec_column_elements",
    "__rock_vec_strided_elements",
];

/// Whether `f` is an erased runtime entry. Classified by name: a call to one is
/// synthesized wherever lowering decides, so the entry has the same identity
/// in every program.
pub fn is_erased_runtime_entry(f: &Function) -> bool {
    ERASED_RUNTIME_ENTRIES.contains(&f.name.as_str())
}

/// Re-exported from [`skull_mir::monoty`], which owns the decision so the graph
/// and both emitters cannot drift. See `too_deep_to_instantiate` there.
pub use skull_mir::monoty::MAX_INSTANTIATION_DEPTH;

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
        // fixture.
        //
        // `(module, function index)` is stable within a build and deterministic
        // across builds (module ids come from source order), which is the same
        // contract [`ord_key`] states. The readable form is [`Self::symbol`];
        // this canonical form stays unambiguous when display syntax collides.
        let decl = DeclKey(i.func).raw();
        if i.args.is_empty() {
            return format!("{base}@{}.{}", decl.0, decl.1);
        }
        let args: Vec<String> = i.args.iter().map(|a| self.interner.canonical(*a)).collect();
        format!("{base}@{}.{}<{}>", decl.0, decl.1, args.join(","))
    }

    /// The instance's LOGICAL symbol.
    ///
    /// Shares [`Instance::symbol`]'s encoder with emission. The artifact's ABI
    /// owns the final spelling and checks injectivity within its view.
    pub fn logical_symbol(&self, ws: &Workspace, i: &Instance) -> String {
        i.symbol(ws, &self.interner)
    }
}

/// A span the walk could not fill in. `Span::unknown` is the only way one is
/// produced here, and its file sentinel is what distinguishes it -- a real span
/// with a zero line range is line 1 of a real file.
pub fn is_spanless(span: &Span) -> bool {
    span.file.as_str() == "<unknown>"
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
/// discovery instead of re-deriving it.
pub fn build(ws: &Workspace, roots: Vec<FuncRef>, _perturb: bool) -> InstanceGraph {
    build_in(ws, roots, false, std::sync::Arc::new(MonoInterner::new()))
}

/// [`build`] against an arena the caller already owns.
///
/// THE entry production uses: `skull::mir_emit::FullEmit` retains both the graph and the
/// `ScalarCtx` that lowers against it, and they must share one arena or an
/// `Instance` minted by the graph means nothing to emission.
pub fn build_in(
    ws: &Workspace,
    roots: Vec<FuncRef>,
    _perturb: bool,
    interner: std::sync::Arc<MonoInterner>,
) -> InstanceGraph {
    let t_build = Instant::now();
    let intern_before = interner.stats();
    let mut g = InstanceGraph::empty_in(interner);
    let t = Instant::now();
    g.seed_roots(ws, canonical_roots(ws, roots));
    let pending = g.always_emitted_owners(ws);
    let t_seed = t.elapsed();
    let t = Instant::now();
    let stats = g.run(ws, pending, true, false);
    g.run_stats = stats;
    let t_run = t.elapsed();
    let t = Instant::now();
    g.derive(ws);
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
    }
    g
}

/// Production's roots as the graph keys them: bone's collapsed representative,
/// deduplicated and ordered.
fn canonical_roots(ws: &Workspace, roots: Vec<FuncRef>) -> Vec<FuncRef> {
    let mut roots: Vec<FuncRef> = roots
        .into_iter()
        .map(|r| skull_mir::lower::canonical_variant(ws, r))
        .collect();
    roots.sort_by_key(|r| (r.module.raw(), r.func.into_raw().into_u32()));
    roots.dedup();
    roots
}

impl InstanceGraph {
    /// Every LIVE instance of one declaration.
    ///
    /// The retained declaration index is updated with each owner contribution,
    /// so a generic declaration maps to every live specialization without a
    /// workspace-sized scan on a body edit.
    pub fn instances_of(&self, fref: FuncRef) -> Box<dyn Iterator<Item = &Instance> + '_> {
        if !self.incremental_indexes {
            // Not derived yet: no update has run, so answer from the
            // contributions instead of building the index for one question.
            return Box::new(
                self.contributions
                    .keys()
                    .filter_map(Owner::instance)
                    .filter(move |i| i.func == fref),
            );
        }
        Box::new(
            self.index
                .decl_instances
                .get(&fref)
                .into_iter()
                .flat_map(|instances| instances.iter()),
        )
    }

    /// **The arena this graph's `MonoTyId`s index**, as the shared handle.
    ///
    /// Handed to `ScalarCtx` so emission lowers against the same ids the graph
    /// minted. Cloning the `Arc` shares the arena; there is deliberately no way
    /// to get an owned `MonoArena` out, because a copy is a second identity.
    pub fn interner(&self) -> &std::sync::Arc<MonoInterner> {
        &self.interner
    }

    /// Release one-shot build records while keeping the shared type arena
    /// alive. Returns the released records so the caller chooses where they
    /// are freed.
    pub fn take_records(&mut self) -> Self {
        let mut empty = Self::empty_in(std::sync::Arc::clone(&self.interner));
        empty.run_stats = self.run_stats;
        empty.incremental_indexes = self.incremental_indexes;
        empty.revision = self.revision;
        std::mem::replace(self, empty)
    }

    fn empty_in(interner: std::sync::Arc<MonoInterner>) -> Self {
        InstanceGraph {
            incremental_indexes: false,
            revision: 0,
            interner,
            contributions: BTreeMap::new(),
            lost: BTreeSet::new(),
            root_instances: BTreeSet::new(),
            instances: BTreeSet::new(),
            run_stats: RunStats::default(),
            last_run_owners: BTreeSet::new(),
            last_run_always_targets: BTreeSet::new(),
            delta_added: BTreeSet::new(),
            delta_removed: BTreeSet::new(),
            index: Indices::default(),
            graph_rejections: Vec::new(),
            recursion_findings: BTreeMap::new(),
            recursion_by_subject: BTreeMap::new(),
            recursion_by_decl: BTreeMap::new(),
            recursion_positions: BTreeMap::new(),
            coroutine_positions: BTreeMap::new(),
            boundaries: BTreeMap::new(),
            coroutine_helpers: BTreeSet::new(),
            coroutine_helpers_by_owner: BTreeMap::new(),
            inline_body_nomangle_roots: BTreeMap::new(),
        }
    }

    fn seed_roots(&mut self, ws: &Workspace, roots: Vec<FuncRef>) {
        self.root_instances = roots
            .into_iter()
            .filter(|r| ws.module(r.module).functions[r.func].generics.is_empty())
            .map(Instance::base)
            .collect();
    }

    /// Globals and decorators have no `Instance` origin, so their bodies are
    /// owners in their own right.
    fn always_emitted_owners(&self, ws: &Workspace) -> Vec<Owner> {
        let mut out = Vec::new();
        for module in ws.source_modules() {
            let Some(inference) = ws.inference(module.id) else {
                continue;
            };
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

    /// Walk pending owners unconditionally, then follow demands for missing owners.
    fn run(
        &mut self,
        ws: &Workspace,
        mut pending: Vec<Owner>,
        include_missing_roots: bool,
        track_incremental_state: bool,
    ) -> RunStats {
        let mut stats = RunStats::default();
        if track_incremental_state {
            self.last_run_owners.clear();
            self.last_run_always_targets.clear();
        }
        // A build with nothing installed can install from its walkers.
        // Debug logging selects the ordered path for comparison.
        let fresh = include_missing_roots
            && !track_incremental_state
            && !log::log_enabled!(log::Level::Debug)
            && !self.incremental_indexes
            && self.contributions.is_empty()
            && self.index.subst.is_empty()
            && pending.iter().all(|owner| owner.instance().is_none());
        if include_missing_roots {
            for i in self.root_instances.clone() {
                if !self.contributions.contains_key(&Owner::Instance(i.clone())) {
                    pending.push(Owner::Instance(i));
                }
            }
        }
        // A conformance tree is supplied by an owner's CALLER, so an owner
        // walked before its caller was re-walked read a stale one. Recorded
        // here and re-checked below rather than assumed away by walk order.
        let mut walked_with: FxHashMap<Instance, InstanceBoundTree> = FxHashMap::default();
        if fresh {
            pending = self.run_fresh_waves(ws, pending, &mut stats, &mut walked_with);
        }
        let mut queued: FxHashSet<Owner> = FxHashSet::default();
        let mut unique_pending = Vec::with_capacity(pending.len());
        for owner in pending {
            if queued.insert(owner.clone()) {
                unique_pending.push(owner);
            }
        }
        pending = unique_pending;
        let mut rounds = 0usize;
        let interner = self.interner.clone();
        loop {
            while !pending.is_empty() {
                let wave: Vec<_> = std::mem::take(&mut pending).into_iter().rev().collect();
                // Capture every walk's inputs before installing any result.
                // Rayon preserves this indexed order, so each result is
                // installed beside the owner whose snapshot produced it.
                let walks = {
                    let run = |owner: &Owner| {
                        let inputs = self.walk_inputs(owner);
                        let tree = inputs.tree.clone();
                        let outcome = OwnerWalk {
                            owner: owner.clone(),
                            inputs,
                        }
                        .run(ws, &interner);
                        (tree, outcome)
                    };
                    if cfg!(feature = "sequential") {
                        wave.iter().map(&run).collect::<Vec<_>>()
                    } else {
                        use bone_core::prelude::*;
                        wave.par_iter().map(&run).collect()
                    }
                };
                for (owner, (tree, outcome)) in wave.into_iter().zip(walks) {
                    let mut prepared_c =
                        outcome.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
                    let mut used_tree = owner.instance().map(|_| tree.unwrap_or_default());
                    queued.remove(&owner);
                    // Retraction can remove a pending instance's substitution mid-wave.
                    // Keep its old contribution until pruning checks its reachability.
                    if let Owner::Instance(i) = &owner {
                        if self.substitution(i).is_none() {
                            continue;
                        }
                        // An earlier install in this wave can restore a substitution
                        // that was absent at preparation. Its queued demand was
                        // suppressed, so expand it now with the current witnesses.
                        if prepared_c.is_none() {
                            used_tree = Some(self.bound_tree(i).cloned().unwrap_or_default());
                            prepared_c = Some(expand(
                                ws,
                                &interner,
                                &WalkScratch::default(),
                                &owner,
                                &self.walk_inputs(&owner),
                            ));
                        }
                        walked_with.insert(i.clone(), used_tree.expect("instance snapshot"));
                    }
                    if track_incremental_state {
                        self.last_run_owners.insert(owner.clone());
                    }
                    stats.expanded += 1;
                    let c = prepared_c.expect("walkable owner");
                    for owner in self.install(owner, c, track_incremental_state) {
                        if queued.insert(owner.clone()) {
                            pending.push(owner);
                        }
                    }
                }
            }
            // The fixpoint: an owner whose witnesses moved while it was already
            // walked must be walked again, or its callees are the ones the old
            // witnesses selected.
            let mut stale: Vec<Owner> = walked_with
                .iter()
                .filter(|(i, used)| {
                    self.bound_tree(i).unwrap_or(&InstanceBoundTree::default()) != *used
                        && self
                            .contributions
                            .contains_key(&Owner::Instance((*i).clone()))
                })
                .map(|(i, _)| Owner::Instance(i.clone()))
                .collect();
            if stale.is_empty() {
                break;
            }
            // Walked in owner order, as the ordered record used to yield them.
            stale.sort();
            rounds += 1;
            stats.settle_rounds = rounds;
            assert!(
                rounds < 64,
                "conformance witnesses did not settle after {rounds} rounds; \
                 {} owners still disagree with what they were walked with",
                stale.len()
            );
            pending = stale;
            queued = pending.iter().cloned().collect();
        }
        stats
    }

    /// Walk ONE owner's body. The only producer of a [`Contribution`], so a
    /// full build and an incremental re-walk cannot record different facts.
    /// What [`expand`] reads of the graph for `owner`.
    fn walk_inputs(&self, owner: &Owner) -> WalkInputs {
        match owner {
            Owner::Instance(i) => WalkInputs {
                subst: self.substitution(i).cloned(),
                tree: self.bound_tree(i).cloned(),
            },
            Owner::AlwaysEmitted(..) => WalkInputs::default(),
        }
    }
}

/// Shards of the substitution index while a fresh build's walkers fill it.
const SUBST_SHARDS: usize = 256;

type SubstShard = Mutex<FxHashMap<Instance, Agreed<MonoSubst>>>;

fn subst_shard(to: &Instance) -> usize {
    use std::hash::BuildHasher;
    let hash = rustc_hash::FxBuildHasher.hash_one(to);
    // Remixed: a shard's own table probes by the plain hash's bits.
    (hash.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 56) as usize
}

/// What one thread walked and indexed of a fresh build's wave.
#[derive(Default)]
struct WaveShare {
    installed: Vec<(Owner, Box<Contribution>)>,
    /// Positions in `installed` with rows for [`InstanceGraph::index_owned_rows`]
    /// or root holes.
    owned: Vec<usize>,
    /// The instances this thread reached first, as the next wave's walks.
    demanded: Vec<OwnerWalk>,
    walked_with: Vec<(Instance, InstanceBoundTree)>,
    disagreed: bool,
}

impl InstanceGraph {
    /// A fresh build's waves, with the walkers doing the install.
    ///
    /// Nothing is installed yet, so nothing is replaced and a wave's result is
    /// a function of the SET of owners in it: the substitution index only
    /// gains rows, the first reach of an instance is its demand, and the rows
    /// kept by owner are decided by owner order. The walkers therefore write
    /// the substitution index themselves, shard by shard, and each takes the
    /// instances it reached first as its share of the next wave. This thread
    /// installs the few owned rows between waves, so every walk of a wave
    /// still reads the conformance trees the wave began with.
    ///
    /// Returns the owners still pending when two reaches disagreed, which
    /// the ordered install has to decide.
    fn run_fresh_waves(
        &mut self,
        ws: &Workspace,
        pending: Vec<Owner>,
        stats: &mut RunStats,
        walked_with: &mut FxHashMap<Instance, InstanceBoundTree>,
    ) -> Vec<Owner> {
        let interner = self.interner.clone();
        let shards: Vec<SubstShard> = (0..SUBST_SHARDS).map(|_| Mutex::default()).collect();
        // Handed from wave to wave, so what a thread learned about the
        // program's types in one wave answers in the next.
        let scratches: Mutex<Vec<WalkScratch>> = Mutex::default();
        let mut wave: Vec<OwnerWalk> = pending
            .into_iter()
            .map(|owner| OwnerWalk {
                inputs: self.walk_inputs(&owner),
                owner,
            })
            .collect();
        let mut installed = Vec::new();
        let mut disagreed = false;
        while !wave.is_empty() && !disagreed {
            let helpers = if cfg!(feature = "sequential") {
                0
            } else {
                rayon::current_num_threads().min(wave.len() / WALK_TASK_OWNERS)
            };
            let units = Mutex::new(std::mem::take(&mut wave).into_iter());
            let shares = Mutex::new(Vec::with_capacity(helpers + 1));
            {
                let graph = &*self;
                let walk = || {
                    let share = graph.walk_wave_share(ws, &interner, &units, &shards, &scratches);
                    shares.lock().push(share);
                };
                rayon::in_place_scope(|scope| {
                    for _ in 0..helpers {
                        scope.spawn(|_| walk());
                    }
                    walk();
                });
            }
            for mut share in shares.into_inner() {
                disagreed |= share.disagreed;
                for &at in &share.owned {
                    let (owner, c) = &share.installed[at];
                    self.index_owned_rows(owner, c);
                }
                walked_with.extend(share.walked_with);
                installed.append(&mut share.installed);
                wave.append(&mut share.demanded);
            }
        }
        stats.expanded += installed.len();
        self.install_fresh(installed, shards);
        // A walk that read no tree is not in `walked_with`; it read the
        // default one, and only an instance with a tree now can differ.
        for i in self.index.trees.keys() {
            if !walked_with.contains_key(i) {
                walked_with.insert(i.clone(), InstanceBoundTree::default());
            }
        }
        if disagreed {
            // Rare, and only an invalid program: deciding between the sides
            // needs the owners.
            self.ensure_incremental_indexes();
        }
        wave.into_iter().map(|walk| walk.owner).collect()
    }

    /// Walk owners of a fresh build's wave until none is left, entering each
    /// walk's reaches in the substitution index.
    fn walk_wave_share(
        &self,
        ws: &Workspace,
        interner: &MonoInterner,
        units: &Mutex<std::vec::IntoIter<OwnerWalk>>,
        shards: &[SubstShard],
        scratches: &Mutex<Vec<WalkScratch>>,
    ) -> WaveShare {
        let mut share = WaveShare::default();
        let scratch = scratches.lock().pop().unwrap_or_default();
        let mut claimed = Vec::with_capacity(WALK_TASK_OWNERS);
        loop {
            claimed.extend(units.lock().by_ref().take(WALK_TASK_OWNERS));
            if claimed.is_empty() {
                scratches.lock().push(scratch);
                return share;
            }
            for OwnerWalk { owner, mut inputs } in claimed.drain(..) {
                if let Owner::Instance(i) = &owner {
                    inputs.tree = self.bound_tree(i).cloned();
                }
                let c = expand(ws, interner, &scratch, &owner, &inputs);
                for (to, subst) in &c.reached {
                    let mut shard = shards[subst_shard(to)].lock();
                    if let Some(agreed) = shard.get_mut(to) {
                        share.disagreed |= !agreed.assert_unowned(subst);
                        continue;
                    }
                    shard.insert(to.clone(), Agreed::unowned(subst.clone()));
                    drop(shard);
                    // The first reach of an instance is its demand. A root
                    // is in the first wave whether or not anything reaches it.
                    if !self.root_instances.contains(to) {
                        share.demanded.push(OwnerWalk {
                            owner: Owner::Instance(to.clone()),
                            inputs: WalkInputs {
                                subst: Some(subst.clone()),
                                tree: None,
                            },
                        });
                    }
                }
                if !(c.bound_trees.is_empty() && c.always_emitted_targets.is_empty()) {
                    share.owned.push(share.installed.len());
                }
                if let (Owner::Instance(i), Some(tree)) = (&owner, inputs.tree) {
                    share.walked_with.push((i.clone(), tree));
                }
                share.installed.push((owner, c));
            }
        }
    }

    /// Take a fresh build's contributions and substitution shards as the graph's records.
    fn install_fresh(
        &mut self,
        mut installed: Vec<(Owner, Box<Contribution>)>,
        shards: Vec<SubstShard>,
    ) {
        let ((contributions, decls), subst) = bone_core::prelude::par_join(
            || {
                if cfg!(feature = "sequential") {
                    installed.sort_unstable_by(|a, b| a.0.cmp(&b.0));
                } else {
                    use rayon::slice::ParallelSliceMut;
                    installed.par_sort_unstable_by(|a, b| a.0.cmp(&b.0));
                }
                let mut decls = FxHashMap::default();
                // Owner order groups an instance with the others of its
                // declaration.
                for decl in installed.chunk_by(|a, b| {
                    a.0.instance().map(|i| i.func) == b.0.instance().map(|i| i.func)
                }) {
                    if let Some(i) = decl[0].0.instance() {
                        decls.insert(i.func, decl.len());
                    }
                }
                let contributions: BTreeMap<_, _> = installed.into_iter().collect();
                (contributions, decls)
            },
            || {
                let shards: Vec<_> = shards.into_iter().map(Mutex::into_inner).collect();
                let mut subst = FxHashMap::default();
                subst.reserve(shards.iter().map(FxHashMap::len).sum());
                for shard in shards {
                    subst.extend(shard);
                }
                subst
            },
        );
        self.contributions = contributions;
        self.index.decls = decls;
        self.index.subst = subst;
    }
}

/// The contribution `owner` makes when walked with `inputs`. It reads nothing
/// else of the graph, so it can run while the graph installs other owners.
fn expand(
    ws: &Workspace,
    interner: &MonoInterner,
    scratch: &WalkScratch,
    owner: &Owner,
    inputs: &WalkInputs,
) -> Box<Contribution> {
    let mut out = expand_sites(ws, interner, scratch, owner, inputs);
    // A body reaching one callee from several sites asserts its
    // substitution once: the index records owners, not sites, and the
    // walk is parallel while the install is not, so the duplicates are
    // folded here.
    let mut seen: FxHashSet<&Instance> = FxHashSet::default();
    let mut keep = Vec::with_capacity(out.reached.len());
    for (to, _) in &out.reached {
        keep.push(seen.insert(to));
    }
    let mut keep = keep.into_iter();
    out.reached
        .retain(|_| keep.next().expect("one flag per reached row"));
    // Ordered by instance: the install demands the callees in this order.
    out.reached.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn expand_sites(
    ws: &Workspace,
    interner: &MonoInterner,
    scratch: &WalkScratch,
    owner: &Owner,
    inputs: &WalkInputs,
) -> Box<Contribution> {
    let mut out = Box::<Contribution>::default();
    match owner {
        Owner::AlwaysEmitted(module, body_owner) => {
            let Some(inf) = ws
                .inference(*module)
                .and_then(|ir| ir.per_fn.get(body_owner))
            else {
                return out;
            };
            let body = ws.module(*module).bodies.get(body_owner);
            let mut w = Walk {
                interner,
                scratch,
                owner_tree: None,
                out: &mut out,
                last_file: None,
            };
            walk_body_edges(
                &mut w,
                ws,
                inf,
                body,
                None,
                &MonoSubst::default(),
                &SiteDomain::Own,
                None,
            );
            // Global initializers and decorator bodies can reach struct field
            // defaults without an enclosing instance.
            if let Some(body) = body {
                walk_struct_default_edges(
                    &mut w,
                    ws,
                    *module,
                    body,
                    *body_owner,
                    None,
                    &MonoSubst::default(),
                );
            }
        }
        Owner::Instance(inst) => {
            // The one admissibility oracle, shared with both emitters. The
            // graph DECLINES to expand where they reject: a capped instance
            // is a changing-instance-recursion candidate, not yet a verdict.
            let too_deep = skull_mir::monoty::too_deep_to_instantiate(
                inst.args
                    .iter()
                    .map(|a| interner.get(*a))
                    .collect::<Vec<_>>()
                    .iter(),
            )
            .is_some();
            if too_deep {
                return out;
            }
            let f = &ws.module(inst.func.module).functions[inst.func.func];
            let is_boundary = declared_boundary(f).is_some();
            if is_boundary {
                return out;
            }
            let FunctionBody::Ready { inf, body } = function_body(ws, inst.func) else {
                return out;
            };
            // A boundary's Rock body is not the one emission uses: emission
            // uses its foreign text. A `@native_required` fallback body also
            // reaches callees emission never routes through.
            out.expanded = true;
            // A body's own substitution is the one its caller reached it
            // under, which the contribution set already answers. An
            // instance with no record is an invariant break, not a body to
            // walk with an empty one.
            let subst = inputs
                .subst
                .as_ref()
                .expect("every reached instance has a recorded substitution");
            let default_tree = InstanceBoundTree::default();
            let owner_tree = inputs.tree.as_ref().unwrap_or(&default_tree);
            let mut w = Walk {
                interner,
                scratch,
                owner_tree: Some(owner_tree),
                out: &mut out,
                last_file: None,
            };
            walk_body_edges(
                &mut w,
                ws,
                inf,
                Some(body),
                Some(inst),
                subst,
                &SiteDomain::Own,
                None,
            );
            for site in crate::identity_storage::substitution_only_sites(
                ws,
                f,
                body,
                inf,
                subst,
                &scratch.identity_verdicts,
            ) {
                w.push_rejection(Rejection {
                    kind: RejectionKind::IdentityInOrdinaryStorage,
                    at: inst.clone(),
                    span: site.span,
                    message: format!("in this instantiation of `{}`, {}", f.name, site.message),
                });
            }
            walk_struct_default_edges(
                &mut w,
                ws,
                inst.func.module,
                body,
                BodyOwner::Function(inst.func.func),
                Some(inst),
                subst,
            );
        }
    }
    out
}

/// What a walk of one owner reads of the graph: an instance's substitution
/// and bound tree. An always-emitted owner reads neither.
#[derive(Clone, Default)]
struct WalkInputs {
    subst: Option<MonoSubst>,
    tree: Option<InstanceBoundTree>,
}

/// Owners per pool task in a wave's walk.
const WALK_TASK_OWNERS: usize = 8;

/// One owner's walk over what the graph held when its wave began.
struct OwnerWalk {
    owner: Owner,
    inputs: WalkInputs,
}

/// A walk's result, or the panic of a walk that ran away from the thread
/// installing it.
type WalkOutcome = std::thread::Result<Option<Box<Contribution>>>;

impl OwnerWalk {
    fn run(&self, ws: &Workspace, interner: &MonoInterner) -> WalkOutcome {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // An instance whose substitution was retracted has nothing to
            // walk; the install decides what that means.
            if self.owner.instance().is_some() && self.inputs.subst.is_none() {
                return None;
            }
            Some(expand(
                ws,
                interner,
                &WalkScratch::default(),
                &self.owner,
                &self.inputs,
            ))
        }))
    }
}

impl InstanceGraph {
    /// Replace an owner's contribution and return owners it newly demands.
    fn install(
        &mut self,
        owner: Owner,
        c: Box<Contribution>,
        track_incremental_state: bool,
    ) -> Vec<Owner> {
        let replaced = self.contributions.contains_key(&owner);
        if track_incremental_state && owner.instance().is_none() {
            if let Some(old) = self.contributions.get(&owner) {
                self.last_run_always_targets
                    .extend(old.always_emitted_targets.iter().cloned());
            }
            self.last_run_always_targets
                .extend(c.always_emitted_targets.iter().cloned());
        }
        if replaced {
            self.uninstall(&owner);
        }
        // Every callee is in `reached`, an always-emitted body's included --
        // `always_emitted_targets` marks which of them are ROOTS, it is not a
        // second population.
        let mut disagreed = false;
        for (to, subst) in &c.reached {
            if self.incremental_indexes {
                assert_subst(&mut self.index.subst, &owner, to, subst);
            } else if let Some(a) = self.index.subst.get_mut(to) {
                disagreed |= !a.assert_unowned(subst);
            } else {
                self.index
                    .subst
                    .insert(to.clone(), Agreed::unowned(subst.clone()));
            }
        }
        self.index_owned_rows(&owner, &c);
        if let Some(from) = owner.instance() {
            for e in &c.edges {
                if self.incremental_indexes {
                    match self.index.preds.get_mut(&e.to) {
                        Some(preds) => {
                            if !preds.contains(from) {
                                preds.insert(from.clone());
                            }
                        }
                        None => {
                            self.index
                                .preds
                                .insert(e.to.clone(), FxHashSet::from_iter([from.clone()]));
                        }
                    }
                }
                self.lost.remove(&e.to);
            }
        }
        if let Some(from) = owner.instance() {
            *self.index.decls.entry(from.func).or_insert(0) += 1;
            if self.incremental_indexes {
                self.index
                    .decl_instances
                    .entry(from.func)
                    .or_default()
                    .insert(from.clone());
            }
        }
        if self.incremental_indexes {
            for file in &c.span_files {
                self.index
                    .span_owners
                    .entry(*file)
                    .or_default()
                    .insert(owner.clone());
            }
        }
        for to in &c.always_emitted_targets {
            self.lost.remove(to);
        }
        // `expand` folded repeated callees and ordered the rows by instance.
        // The owner itself is not demanded: `install` uninstalled it first,
        // so a body that reaches ITSELF -- any recursive function -- would
        // otherwise find itself absent and re-walk forever.
        let demanded: Vec<Owner> = c
            .reached
            .iter()
            .map(|(to, _)| Owner::Instance(to.clone()))
            .filter(|to| *to != owner && !self.contributions.contains_key(to))
            .collect();
        self.contributions.insert(owner.clone(), c);
        if disagreed {
            // Rare, and only an invalid program: deciding between the sides
            // needs the owners, so stop deferring them.
            self.ensure_incremental_indexes();
        }
        if track_incremental_state {
            if let Some(instance) = owner.instance() {
                if self.instances.insert(instance.clone()) {
                    if !self.delta_removed.remove(instance) {
                        self.delta_added.insert(instance.clone());
                    }
                }
            }
        }
        demanded
    }

    /// The index rows that are kept with their owner: conformance trees,
    /// erased entries and always-emitted targets. Few bodies have any, and
    /// only the build thread writes them.
    fn index_owned_rows(&mut self, owner: &Owner, c: &Contribution) {
        for (i, tree) in &c.bound_trees {
            match self.index.trees.get_mut(i) {
                Some(a) => a.assert_by(owner, tree),
                None => {
                    self.index
                        .trees
                        .insert(i.clone(), Agreed::new(owner.clone(), tree.clone()));
                }
            }
        }
        for to in &c.always_emitted_targets {
            self.index
                .always_emitted
                .entry(to.clone())
                .or_default()
                .insert(owner.clone());
        }
    }

    /// Remove a contribution and everything the indices held on its behalf.
    ///
    /// An index entry disappears when its last owner does, never when a count
    /// reaches zero: the owner SET is the record, so a stale fact cannot
    /// survive as a positive number nobody can check.
    fn uninstall(&mut self, owner: &Owner) {
        if !self.contributions.contains_key(owner) {
            return;
        }
        // Retraction reads the owners a fresh build deferred.
        self.ensure_incremental_indexes();
        let old = self
            .contributions
            .remove(owner)
            .expect("presence was checked above");
        if let Some(instance) = owner.instance() {
            if self.instances.remove(instance) {
                if !self.delta_added.remove(instance) {
                    self.delta_removed.insert(instance.clone());
                }
            }
        }
        let reaches: BTreeSet<&Instance> = old.reached.iter().map(|(to, _)| to).collect();
        for to in reaches {
            if let Some(a) = self.index.subst.get_mut(to) {
                if !a.retract(owner) {
                    self.index.subst.remove(to);
                }
            }
        }
        let trees: BTreeSet<&Instance> = old.bound_trees.iter().map(|(i, _)| i).collect();
        for i in trees {
            if let Some(a) = self.index.trees.get_mut(i) {
                if !a.retract(owner) {
                    self.index.trees.remove(i);
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
            if self.incremental_indexes {
                for e in &old.edges {
                    if let Some(set) = self.index.preds.get_mut(&e.to) {
                        set.remove(from);
                        if set.is_empty() {
                            self.index.preds.remove(&e.to);
                        }
                    }
                }
            }
            // Its own edges are gone, so its successors may have lost their
            // last way in. Recorded for the caller to prune from.
            self.lost.extend(old.edges.iter().map(|e| e.to.clone()));
            if let Some(count) = self.index.decls.get_mut(&from.func) {
                *count -= 1;
                if *count == 0 {
                    self.index.decls.remove(&from.func);
                }
            }
            if let Some(instances) = self.index.decl_instances.get_mut(&from.func) {
                instances.remove(from);
                if instances.is_empty() {
                    self.index.decl_instances.remove(&from.func);
                }
            }
        }
        if self.incremental_indexes {
            for file in &old.span_files {
                if let Some(owners) = self.index.span_owners.get_mut(file) {
                    owners.remove(owner);
                    if owners.is_empty() {
                        self.index.span_owners.remove(file);
                    }
                }
            }
        }
    }

    fn remove_graph_rejection_position(&mut self, position: usize) {
        if position >= self.graph_rejections.len() {
            return;
        }
        self.graph_rejections.swap_remove(position);
        if position == self.graph_rejections.len() {
            return;
        }
        let moved = &self.graph_rejections[position];
        match moved.kind {
            RejectionKind::ChangingInstanceRecursion => {
                self.recursion_positions.insert(moved.at.clone(), position);
            }
            RejectionKind::ConcreteTypeUnavailable => {
                self.coroutine_positions.insert(moved.at.clone(), position);
            }
            _ => {}
        }
    }

    fn remove_recursion_finding(&mut self, at: &Instance) {
        let Some(finding) = self.recursion_findings.remove(at) else {
            return;
        };
        for subject in finding.subjects {
            if let Some(starts) = self.recursion_by_subject.get_mut(&subject) {
                starts.remove(at);
                if starts.is_empty() {
                    self.recursion_by_subject.remove(&subject);
                }
            }
        }
        let decl = DeclKey(at.func);
        if let Some(starts) = self.recursion_by_decl.get_mut(&decl) {
            starts.remove(at);
            if starts.is_empty() {
                self.recursion_by_decl.remove(&decl);
            }
        }
        if let Some(position) = self.recursion_positions.remove(at) {
            self.remove_graph_rejection_position(position);
        }
    }

    fn install_recursion_finding(&mut self, finding: RecursionFinding) {
        let at = finding.rejection.at.clone();
        self.remove_recursion_finding(&at);
        let position = self.graph_rejections.len();
        self.graph_rejections.push(finding.rejection.clone());
        self.recursion_positions.insert(at.clone(), position);
        self.recursion_by_decl
            .entry(DeclKey(at.func))
            .or_default()
            .insert(at.clone());
        for subject in &finding.subjects {
            self.recursion_by_subject
                .entry(subject.clone())
                .or_default()
                .insert(at.clone());
        }
        self.recursion_findings.insert(at, finding);
    }

    fn remove_coroutine_rejection(&mut self, at: &Instance) {
        if let Some(position) = self.coroutine_positions.remove(at) {
            self.remove_graph_rejection_position(position);
        }
    }

    fn install_coroutine_rejection(&mut self, rejection: Rejection) {
        let at = rejection.at.clone();
        self.remove_coroutine_rejection(&at);
        let position = self.graph_rejections.len();
        self.graph_rejections.push(rejection);
        self.coroutine_positions.insert(at, position);
    }

    fn clear_graph_postpass_rejections(&mut self) {
        self.graph_rejections.clear();
        self.recursion_findings.clear();
        self.recursion_by_subject.clear();
        self.recursion_by_decl.clear();
        self.recursion_positions.clear();
        self.coroutine_positions.clear();
    }

    fn retract_recursion_findings(
        &mut self,
        touched_instances: &BTreeSet<Instance>,
        touched_decls: &BTreeSet<DeclKey>,
    ) -> BTreeSet<Instance> {
        let mut starts = BTreeSet::new();
        for instance in touched_instances {
            if let Some(found) = self.recursion_by_subject.get(instance) {
                starts.extend(found.iter().cloned());
            }
        }
        for decl in touched_decls {
            if let Some(found) = self.recursion_by_decl.get(decl) {
                starts.extend(found.iter().cloned());
            }
        }
        for at in starts.iter() {
            self.remove_recursion_finding(at);
        }
        starts
    }

    fn refresh_coroutine_rejections(
        &mut self,
        ws: &Workspace,
        touched: impl IntoIterator<Item = Instance>,
    ) {
        let touched: BTreeSet<Instance> = touched.into_iter().collect();
        let mut active = Vec::new();
        let mut removed = Vec::new();
        for instance in &touched {
            let identity = self.identity(ws, instance);
            self.remove_coroutine_helpers(&identity);
            self.remove_coroutine_rejection(instance);
            if self
                .contributions
                .contains_key(&Owner::Instance(instance.clone()))
            {
                active.push(instance.clone());
            } else {
                removed.push(instance.clone());
            }
        }
        record_coroutine_helpers(self, ws, &active, &removed);
    }

    fn remove_coroutine_helpers(&mut self, owner_identity: &str) {
        let Some(helpers) = self.coroutine_helpers_by_owner.remove(owner_identity) else {
            return;
        };
        for helper in helpers {
            self.coroutine_helpers.remove(&helper);
        }
    }

    fn insert_coroutine_helper(&mut self, helper: CoroutineHelper) {
        self.coroutine_helpers.insert(helper.clone());
        self.coroutine_helpers_by_owner
            .entry(helper.owner_identity.clone())
            .or_default()
            .insert(helper);
    }

    fn apply_root_delta(
        &mut self,
        ws: &Workspace,
        root_added: &[FuncRef],
        root_removed: &[FuncRef],
    ) {
        let added = canonical_roots(ws, root_added.to_vec());
        let removed = canonical_roots(ws, root_removed.to_vec());
        for root in removed {
            if added.contains(&root)
                || !ws.module(root.module).functions[root.func]
                    .generics
                    .is_empty()
            {
                continue;
            }
            let instance = Instance::base(root);
            self.root_instances.remove(&instance);
            if self
                .contributions
                .contains_key(&Owner::Instance(instance.clone()))
            {
                self.lost.insert(instance);
            }
        }
        self.root_instances.extend(
            added
                .into_iter()
                .filter(|r| ws.module(r.module).functions[r.func].generics.is_empty())
                .map(Instance::base),
        );
    }

    /// Compute fresh-build postpasses. Every walked instance is already reachable.
    fn derive(&mut self, ws: &Workspace) {
        self.lost.clear();
        self.instances = self
            .contributions
            .keys()
            .filter_map(Owner::instance)
            .cloned()
            .collect();
        // Owner order is declaration order, so the declarations come out
        // sorted and each one's instances are adjacent.
        let mut decls: Vec<DeclKey> = self.instances.iter().map(|i| DeclKey(i.func)).collect();
        decls.dedup();
        let coroutines: Vec<Instance> = self
            .instances
            .iter()
            .filter(|i| ws.module(i.func.module).functions[i.func.func].is_coroutine)
            .cloned()
            .collect();
        self.clear_graph_postpass_rejections();

        // Both read only the workspace and the installed records, so
        // they run beside each other; what they found is recorded in order.
        let (nomangle_roots, recursion_findings) = {
            use bone_core::prelude::par_join;
            let graph = &*self;
            par_join(
                || opaque_boundary_roots_for_decls(ws, decls.iter().map(|decl| decl.0)),
                || detect_changing_instance_recursion(graph, ws),
            )
        };
        self.inline_body_nomangle_roots.extend(nomangle_roots);
        for finding in recursion_findings {
            self.install_recursion_finding(finding);
        }
        classify_boundaries(self, ws, &decls);
        record_coroutine_helpers(self, ws, &coroutines, &[]);
    }

    fn derive_incremental(
        &mut self,
        ws: &Workspace,
        affected: &[Owner],
        changed_decls: &[DeclKey],
    ) -> Prune {
        let lost: Vec<Instance> = std::mem::take(&mut self.lost).into_iter().collect();
        let prune = self.stranded(&lost);
        for instance in &prune.pruned {
            self.uninstall(&Owner::Instance(instance.clone()));
        }

        let entered: Vec<Instance> = self.delta_added.iter().cloned().collect();
        let left: Vec<Instance> = self.delta_removed.iter().cloned().collect();
        let mut touched_instances: BTreeSet<Instance> = affected
            .iter()
            .filter_map(Owner::instance)
            .cloned()
            .collect();
        touched_instances.extend(entered.iter().cloned());
        touched_instances.extend(left.iter().cloned());

        let mut touched_decls: BTreeSet<DeclKey> = changed_decls.iter().cloned().collect();
        touched_decls.extend(entered.iter().map(|i| DeclKey(i.func)));
        touched_decls.extend(left.iter().map(|i| DeclKey(i.func)));
        let mut active_decls: BTreeSet<DeclKey> = changed_decls
            .iter()
            .cloned()
            .chain(entered.iter().map(|i| DeclKey(i.func)))
            .collect();
        active_decls.extend(
            touched_decls
                .iter()
                .filter(|decl| self.index.decls.contains_key(&decl.0))
                .cloned(),
        );

        for decl in &touched_decls {
            for member in skull_mir::lower::backend_variant_group(ws, decl.0) {
                let member = DeclKey(member);
                self.inline_body_nomangle_roots.remove(&member);
                let f = &ws.module(member.0.module).functions[member.0.func];
                let name = mangle_fn(ws, f);
                self.boundaries.remove(&name);
            }
        }
        let active_decl_vec: Vec<DeclKey> = active_decls.iter().cloned().collect();
        record_inline_body_nomangle_roots(self, ws, active_decl_vec.iter().map(|decl| decl.0));
        classify_boundaries(self, ws, &active_decl_vec);
        self.refresh_coroutine_rejections(ws, touched_instances.iter().cloned());

        let mut previous_recursion_starts =
            self.retract_recursion_findings(&touched_instances, &touched_decls);
        // A full build reports only the first start of each declaration, so a
        // finding's declaration is the unit that must be re-searched: dropping
        // one can expose a disconnected start of the same declaration, and a
        // new one can displace a later start that nothing here touched.
        let mut recursion_decls: BTreeSet<DeclKey> = previous_recursion_starts
            .iter()
            .map(|start| DeclKey(start.func))
            .collect();
        let findings =
            loop {
                let mut recursion_seeds = touched_instances.clone();
                recursion_seeds.extend(previous_recursion_starts.iter().cloned());
                recursion_seeds.extend(touched_decls.iter().chain(&recursion_decls).flat_map(
                    |decl| {
                        self.index
                            .decl_instances
                            .get(&decl.0)
                            .into_iter()
                            .flatten()
                            .cloned()
                    },
                ));
                recursion_seeds = self.recursion_seed_closure(recursion_seeds);
                let findings = detect_changing_instance_recursion_local(self, ws, &recursion_seeds);
                let displaced: BTreeSet<DeclKey> = findings
                    .iter()
                    .map(|finding| DeclKey(finding.rejection.at.func))
                    .filter(|decl| !recursion_decls.contains(decl))
                    .collect();
                if displaced.is_empty() {
                    break findings;
                }
                for decl in displaced {
                    let retained: Vec<Instance> = self
                        .recursion_by_decl
                        .get(&decl)
                        .into_iter()
                        .flatten()
                        .cloned()
                        .collect();
                    for at in retained {
                        self.remove_recursion_finding(&at);
                        previous_recursion_starts.insert(at);
                    }
                    recursion_decls.insert(decl);
                }
            };
        for finding in findings {
            self.install_recursion_finding(finding);
        }
        Prune {
            pruned: prune.pruned,
            region: prune.region,
            seeds: prune.seeds,
            boundary_predecessor_edges_examined: prune.boundary_predecessor_edges_examined,
        }
    }

    fn recursion_seed_closure(&self, seeds: BTreeSet<Instance>) -> BTreeSet<Instance> {
        let mut out = BTreeSet::new();
        let mut pending: Vec<Instance> = seeds.into_iter().collect();
        while let Some(instance) = pending.pop() {
            if !out.insert(instance.clone()) {
                continue;
            }
            if let Some(preds) = self.index.preds.get(&instance) {
                pending.extend(preds.iter().cloned());
            }
        }
        out
    }

    /// Update a body while applying only the explicit root declarations that
    /// changed with it. Interface-derived roots remain in the retained graph;
    /// callers use this path after preserving their root cache.
    pub fn update_bodies_with_root_delta(
        &mut self,
        ws: &Workspace,
        root_added: &[FuncRef],
        root_removed: &[FuncRef],
        changed: &[(ModuleId, BodyOwner)],
        edits: &[TextEditDelta],
    ) -> UpdateStats {
        self.revision += 1;
        self.ensure_incremental_indexes();
        let pending_roots = canonical_roots(ws, root_added.to_vec())
            .into_iter()
            .filter(|root| {
                ws.module(root.module).functions[root.func]
                    .generics
                    .is_empty()
            })
            .map(|root| {
                Owner::Instance(Instance {
                    func: root,
                    args: Vec::new(),
                })
            })
            .collect();
        self.apply_root_delta(ws, root_added, root_removed);
        self.update_bodies_after_roots(ws, changed, edits, pending_roots)
    }

    /// How many updates this graph has applied since its build.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Derive the update-only reverse indexes from the contributions the
    /// build installed without them, once, before the first update reads
    /// them. Every install and uninstall after this keeps them current.
    pub fn ensure_incremental_indexes(&mut self) {
        if self.incremental_indexes {
            return;
        }
        self.incremental_indexes = true;
        self.index.subst.clear();
        for (owner, c) in &self.contributions {
            for (to, subst) in &c.reached {
                assert_subst(&mut self.index.subst, owner, to, subst);
            }
            if let Some(from) = owner.instance() {
                for e in &c.edges {
                    self.index
                        .preds
                        .entry(e.to.clone())
                        .or_default()
                        .insert(from.clone());
                }
                self.index
                    .decl_instances
                    .entry(from.func)
                    .or_default()
                    .insert(from.clone());
            }
            for file in &c.span_files {
                self.index
                    .span_owners
                    .entry(*file)
                    .or_default()
                    .insert(owner.clone());
            }
        }
    }

    fn update_bodies_after_roots(
        &mut self,
        ws: &Workspace,
        changed: &[(ModuleId, BodyOwner)],
        edits: &[TextEditDelta],
        pending_roots: Vec<Owner>,
    ) -> UpdateStats {
        self.delta_added.clear();
        self.delta_removed.clear();
        self.shift_contribution_spans(edits);

        // No removal step: `run` expands every pending owner unconditionally
        // and `commit` inserts over whatever was there, so an affected owner's
        // old contribution is REPLACED rather than merged with. A remove-first
        // loop stood here and was inert -- mutation-verified.
        let mut affected = self.affected_owners(ws, changed);
        affected.extend(pending_roots);
        affected.sort();
        affected.dedup();
        let run = self.run(ws, affected.clone(), false, true);
        self.run_stats = run;
        let revisited_owners = self.last_run_owners.clone();
        let changed_decls = changed.iter().filter_map(|(module, owner)| match owner {
            BodyOwner::Function(func) => Some(skull_mir::lower::canonical_variant(
                ws,
                FuncRef {
                    module: *module,
                    func: *func,
                },
            )),
            _ => None,
        });
        let changed_decls: Vec<DeclKey> = changed_decls.map(DeclKey).collect();
        let prune = self.derive_incremental(ws, &affected, &changed_decls);
        let added = self.delta_added.clone();
        let removed = self.delta_removed.clone();
        let mut revisited: BTreeSet<Instance> = affected
            .iter()
            .filter_map(Owner::instance)
            .filter(|instance| self.instances.contains(*instance))
            .cloned()
            .collect();
        revisited.extend(added.iter().cloned());
        revisited.extend(
            self.last_run_always_targets
                .iter()
                .filter(|instance| self.instances.contains(*instance))
                .cloned(),
        );
        UpdateStats {
            affected: affected.len(),
            expanded: run.expanded,
            pruned: prune.pruned.len(),
            suspect_region: prune.region,
            prune_seeds: prune.seeds,
            boundary_predecessor_edges_examined: prune.boundary_predecessor_edges_examined,
            newly_reached: added.len(),
            added,
            removed,
            owners: self.contributions.len(),
            instances: self.instances.len(),
            revisited,
            revisited_owners,
        }
    }

    fn shift_contribution_spans(&mut self, edits: &[TextEditDelta]) {
        for edit in edits
            .iter()
            .filter(|edit| edit.byte_delta != 0 || edit.line_delta != 0)
        {
            let owners: Vec<Owner> = self
                .index
                .span_owners
                .get(&edit.file)
                .into_iter()
                .flat_map(|owners| owners.iter().cloned())
                .collect();
            for owner in owners {
                let Some(contribution) = self.contributions.get_mut(&owner) else {
                    continue;
                };
                for edge in &mut contribution.edges {
                    edit.shift_span(&mut edge.span);
                }
                for record in contribution
                    .call_sites
                    .values_mut()
                    .chain(contribution.always_call_sites.values_mut())
                {
                    edit.shift_span(&mut record.span);
                }
                for unresolved in &mut contribution.unresolved {
                    edit.shift_span(&mut unresolved.span);
                }
                for rejection in &mut contribution.rejections {
                    edit.shift_span(&mut rejection.span);
                }
            }
        }
    }

    /// Revisit owners whose inference changed. Span shifts are handled separately.
    fn affected_owners(&self, ws: &Workspace, changed: &[(ModuleId, BodyOwner)]) -> Vec<Owner> {
        let mut changed_seeds: Vec<Owner> = Vec::new();
        for (module, body_owner) in changed {
            match body_owner {
                // The graph keys instances on bone's collapsed representative,
                // so a backend sibling's body reaches its instances through the
                // same canonicalization `build` seeds roots with.
                BodyOwner::Function(f) => {
                    let fref = skull_mir::lower::canonical_variant(
                        ws,
                        FuncRef {
                            module: *module,
                            func: *f,
                        },
                    );
                    if let Some(instances) = self.index.decl_instances.get(&fref) {
                        changed_seeds.extend(instances.iter().cloned().map(Owner::Instance));
                    }
                }
                // An owner in its own right. Reachable through a `const`
                // initializer, whose VALUE is a body
                // (`bone_incremental::item_prints`); a mutable `global` keeps
                // whole-item interface, so an edit to one arrives as a tier
                // change instead and never here.
                BodyOwner::Global(_) => {
                    changed_seeds.push(Owner::AlwaysEmitted(*module, *body_owner));
                }
                // These items hash their whole body into the interface.
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
        out.sort();
        out.dedup();
        out
    }

    /// One instance's primary record, or `None` if the graph does not hold it.
    ///
    fn contribution(&self, i: &Instance) -> Option<&Contribution> {
        self.contributions
            .get(&Owner::Instance(i.clone()))
            .map(Box::as_ref)
    }

    /// **Owner-local**: what each of this instance's operations resolved to.
    /// Read straight off its own record rather than through a graph-wide map,
    /// because that is the question being asked.
    pub fn sites_of(&self, i: &Instance) -> Option<&BTreeMap<Site, SiteRecord>> {
        self.contribution(i)
            .map(|c| &c.call_sites)
            .filter(|m| !m.is_empty())
    }

    /// Every edge and its source instance, in owner order.
    pub fn edges(&self) -> impl Iterator<Item = (&Instance, &Edge)> {
        self.contributions
            .iter()
            .filter_map(|(owner, c)| owner.instance().map(|i| (i, c)))
            .flat_map(|(i, c)| c.edges.iter().map(move |edge| (i, edge)))
    }

    /// Instances whose Rock body the walk entered.
    pub fn expanded(&self) -> impl Iterator<Item = &Instance> {
        self.contributions
            .iter()
            .filter(|(_, c)| c.expanded)
            .filter_map(|(o, _)| o.instance())
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
        let mut agreed: Vec<(&Instance, &MonoSubst)> = self
            .index
            .subst
            .iter()
            .map(|(i, a)| (i, a.decided()))
            .collect();
        agreed.sort_by(|left, right| left.0.cmp(right.0));
        agreed.into_iter().chain(
            self.root_instances
                .iter()
                .filter(|i| !self.index.subst.contains_key(*i))
                .map(|i| (i, &*EMPTY_SUBST)),
        )
    }

    /// **Owner-local**: the concrete witnesses supplied for this instance.
    pub fn bound_tree(&self, i: &Instance) -> Option<&InstanceBoundTree> {
        self.index.trees.get(i).map(|a| a.decided())
    }

    /// **Owner-local**: was this instance reached with two different witness
    /// trees? Asked per instance by `concrete_body`, which used to answer it by
    /// scanning the whole disagreement vector.
    pub fn has_bound_tree_disagreement(&self, i: &Instance) -> bool {
        self.index
            .trees
            .get(i)
            .is_some_and(|a| !a.dissent.is_empty())
    }

    /// Every instance [`Self::has_bound_tree_disagreement`] holds for.
    pub fn bound_tree_dissenters(&self) -> FxHashSet<Instance> {
        self.index
            .trees
            .iter()
            .filter(|(_, a)| !a.dissent.is_empty())
            .map(|(i, _)| i.clone())
            .collect()
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

    /// Does an always-emitted body take this instance as a VALUE
    /// (`SiteOp::FnValue`) rather than calling it? Such a site has no caller
    /// instance, so it never reaches an artifact's per-caller fn-value index.
    pub(crate) fn is_always_emitted_fn_value(&self, i: &Instance) -> bool {
        let Some(owners) = self.index.always_emitted.get(i) else {
            return false;
        };
        owners.iter().any(|owner| {
            self.contributions.get(owner).is_some_and(|c| {
                c.always_call_sites.iter().any(|(site, record)| {
                    site.op == SiteOp::FnValue
                        && matches!(&record.outcome, SiteOutcome::Resolved(to) if to == i)
                })
            })
        })
    }

    fn diagnostic_key(&self, instance: &Instance) -> ((u32, u32), Vec<String>) {
        (
            DeclKey(instance.func).raw(),
            instance
                .args
                .iter()
                .map(|arg| self.interner.canonical(*arg))
                .collect(),
        )
    }

    /// Every row of one per-owner list, in owner order. Few owners have
    /// any, and finding those is the work, so the pool looks.
    fn owner_rows<T: Sync>(&self, rows: impl Fn(&Contribution) -> &[T] + Sync) -> Vec<&T> {
        use bone_core::prelude::*;
        let owners: Vec<&Contribution> = self.contributions.values().map(Box::as_ref).collect();
        let found: Vec<&[T]> = owners
            .par_iter()
            .map(|c| rows(c))
            .filter(|rows| !rows.is_empty())
            .collect();
        found.into_iter().flatten().collect()
    }

    /// Every rejection: the per-body ones and the whole-graph ones.
    pub fn rejections(&self) -> impl Iterator<Item = &Rejection> {
        // Type IDs reflect parallel interning order. Preserve declaration and
        // per-body rejection order, but order specializations by logical type.
        let mut ordinary = self.owner_rows(|c| &c.rejections);
        ordinary.sort_by_cached_key(|rejection| self.diagnostic_key(&rejection.at));
        let mut graph: Vec<_> = self.graph_rejections.iter().collect();
        graph.sort_by_cached_key(|rejection| self.diagnostic_key(&rejection.at));
        ordinary.into_iter().chain(graph)
    }

    /// **What an edit stranded**, searched over the affected region only.
    ///
    /// Reachability, not reference counting: an edit can strand a whole group
    /// of instances that keep each other alive, and a cycle has no member whose
    /// count would ever drop to zero. But it does not need the whole graph
    /// either -- only an instance whose IN-EDGES changed can have become
    /// unreachable, so the search starts at those and stops at tentative
    /// external boundaries.
    ///
    /// The search keeps external boundaries tentative. If another lost seed
    /// later reaches a boundary's predecessor, that predecessor enters the
    /// region and the boundary is reopened; no rootward query is needed.
    fn stranded(&self, lost: &[Instance]) -> Prune {
        let succ = |i: &Instance| -> &[Edge] {
            self.contributions
                .get(&Owner::Instance(i.clone()))
                .map_or(&[][..], |c| c.edges.as_slice())
        };
        let seeds: BTreeSet<Instance> = lost.iter().cloned().collect();
        let mut region: BTreeSet<Instance> = lost
            .iter()
            .filter(|i| {
                self.contributions
                    .contains_key(&Owner::Instance((*i).clone()))
            })
            .cloned()
            .collect();
        let mut queue: Vec<Instance> = region.iter().cloned().collect();
        let mut pending: FxHashSet<Instance> = region.iter().cloned().collect();
        let mut expanded: FxHashSet<Instance> = FxHashSet::default();
        let mut boundary_preds: BTreeMap<Instance, BTreeSet<Instance>> = BTreeMap::new();
        let mut boundary_waiters: BTreeMap<Instance, BTreeSet<Instance>> = BTreeMap::new();
        let mut anchors: BTreeSet<Instance> = BTreeSet::new();
        let mut released: Vec<Instance> = Vec::new();
        let mut boundary_predecessor_edges_examined = 0;
        let mut support_component_cache: BTreeMap<Instance, (bool, usize)> = BTreeMap::new();
        let mut support_components: Vec<BTreeSet<Instance>> = Vec::new();
        let predecessor_blockers = |target: &Instance,
                                    region: &BTreeSet<Instance>,
                                    examined: &mut usize|
         -> BTreeSet<Instance> {
            let Some(preds) = self.index.preds.get(target) else {
                return BTreeSet::new();
            };
            *examined += preds.len();
            preds
                .iter()
                .filter(|pred| *pred != target && !region.contains(*pred))
                .cloned()
                .collect()
        };
        let local_support_component_reaches_anchor =
            |target: &Instance,
             region: &BTreeSet<Instance>,
             cache: &mut BTreeMap<Instance, (bool, usize)>,
             components: &mut Vec<BTreeSet<Instance>>| {
                if region.contains(target) {
                    return (false, usize::MAX);
                }
                if let Some((supported, component)) = cache.get(target) {
                    return (*supported, *component);
                }
                let mut pending = vec![target.clone()];
                let mut discovered = BTreeSet::new();
                let mut support_edges = Vec::new();
                let mut direct_anchors = BTreeSet::new();
                while let Some(i) = pending.pop() {
                    if region.contains(&i) {
                        continue;
                    }
                    if !discovered.insert(i.clone()) {
                        continue;
                    }
                    if self.root_instances.contains(&i)
                        || self.index.always_emitted.contains_key(&i)
                    {
                        direct_anchors.insert(i.clone());
                    }
                    let Some(preds) = self.index.preds.get(&i) else {
                        continue;
                    };
                    for pred in preds {
                        if pred == &i || region.contains(pred) {
                            continue;
                        }
                        if self.root_instances.contains(pred)
                            || self.index.always_emitted.contains_key(pred)
                        {
                            direct_anchors.insert(i.clone());
                        } else {
                            support_edges.push((i.clone(), pred.clone()));
                            pending.push(pred.clone());
                        }
                    }
                }

                let mut dependents: BTreeMap<Instance, BTreeSet<Instance>> = BTreeMap::new();
                for (source, predecessor) in &support_edges {
                    dependents
                        .entry(predecessor.clone())
                        .or_default()
                        .insert(source.clone());
                }
                let mut supported_nodes = direct_anchors;
                let mut support_queue: Vec<Instance> = supported_nodes.iter().cloned().collect();
                while let Some(supported) = support_queue.pop() {
                    let Some(dependents) = dependents.get(&supported) else {
                        continue;
                    };
                    for dependent in dependents {
                        if supported_nodes.insert(dependent.clone()) {
                            support_queue.push(dependent.clone());
                        }
                    }
                }

                let unsupported_nodes: BTreeSet<Instance> =
                    discovered.difference(&supported_nodes).cloned().collect();
                let mut unsupported_neighbors: BTreeMap<Instance, BTreeSet<Instance>> =
                    BTreeMap::new();
                for node in &unsupported_nodes {
                    unsupported_neighbors.entry(node.clone()).or_default();
                }
                for (source, predecessor) in &support_edges {
                    if unsupported_nodes.contains(source) && unsupported_nodes.contains(predecessor)
                    {
                        unsupported_neighbors
                            .entry(source.clone())
                            .or_default()
                            .insert(predecessor.clone());
                        unsupported_neighbors
                            .entry(predecessor.clone())
                            .or_default()
                            .insert(source.clone());
                    }
                }
                let mut remaining = unsupported_nodes;
                while let Some(start) = remaining.pop_first() {
                    let component_id = components.len();
                    let mut component = BTreeSet::new();
                    let mut component_queue = vec![start];
                    while let Some(i) = component_queue.pop() {
                        if !component.insert(i.clone()) {
                            continue;
                        }
                        remaining.remove(&i);
                        if let Some(neighbors) = unsupported_neighbors.get(&i) {
                            component_queue.extend(
                                neighbors.iter().filter(|n| remaining.contains(*n)).cloned(),
                            );
                        }
                    }
                    for i in &component {
                        cache.insert(i.clone(), (false, component_id));
                    }
                    components.push(component);
                }
                for i in &supported_nodes {
                    cache.insert(i.clone(), (true, usize::MAX));
                }
                if supported_nodes.contains(target) {
                    (true, usize::MAX)
                } else {
                    let component_id = components
                        .iter()
                        .position(|component| component.contains(target))
                        .expect("unsupported support target has no component");
                    (false, component_id)
                }
            };
        loop {
            while let Some(promoted) = released.pop() {
                let waiters = boundary_waiters.remove(&promoted).unwrap_or_default();
                for target in waiters {
                    let became_region = boundary_preds.get_mut(&target).is_some_and(|preds| {
                        preds.remove(&promoted);
                        preds.is_empty()
                    });
                    if !became_region {
                        continue;
                    }
                    boundary_preds.remove(&target);
                    anchors.remove(&target);
                    if region.insert(target.clone()) {
                        support_component_cache.clear();
                        support_components.clear();
                        released.push(target.clone());
                    }
                    if pending.insert(target.clone()) {
                        queue.push(target);
                    }
                }
            }
            let Some(i) = queue.pop() else { break };
            pending.remove(&i);
            if !region.contains(&i) || !expanded.insert(i.clone()) {
                continue;
            }
            let is_root = self.root_instances.contains(&i);
            let is_always = self.index.always_emitted.contains_key(&i);
            if is_root || is_always {
                anchors.insert(i);
                continue;
            }
            let blockers =
                predecessor_blockers(&i, &region, &mut boundary_predecessor_edges_examined);
            if !blockers.is_empty() {
                anchors.insert(i.clone());
                boundary_preds.insert(i.clone(), blockers.clone());
                for pred in &blockers {
                    boundary_waiters
                        .entry(pred.clone())
                        .or_default()
                        .insert(i.clone());
                }
            } else {
                anchors.remove(&i);
                boundary_preds.remove(&i);
            }
            // A lost seed with a locally supported boundary is tentative;
            // otherwise it must probe forward so an unanchored cycle is found.
            let mut unsupported_component_ids = BTreeSet::new();
            let has_direct_anchor = blockers.iter().any(|pred| {
                self.root_instances.contains(pred) || self.index.always_emitted.contains_key(pred)
            });
            let has_live_boundary = has_direct_anchor
                || (seeds.contains(&i)
                    && blockers.iter().any(|pred| {
                        let (supported, component_id) = local_support_component_reaches_anchor(
                            pred,
                            &region,
                            &mut support_component_cache,
                            &mut support_components,
                        );
                        if !supported {
                            unsupported_component_ids.insert(component_id);
                        }
                        supported
                    }));
            if !blockers.is_empty() && (!seeds.contains(&i) || has_live_boundary) {
                continue;
            }
            if seeds.contains(&i) && !blockers.is_empty() {
                anchors.remove(&i);
                boundary_preds.remove(&i);
                for blocker in &blockers {
                    let remove_waiter = boundary_waiters.get_mut(blocker).is_some_and(|waiters| {
                        waiters.remove(&i);
                        waiters.is_empty()
                    });
                    if remove_waiter {
                        boundary_waiters.remove(blocker);
                    }
                }
                let mut support_targets = BTreeSet::new();
                for component_id in unsupported_component_ids {
                    if let Some(component) = support_components.get(component_id) {
                        support_targets.extend(component.iter().cloned());
                    }
                }
                let mut entered_region = false;
                for support_target in support_targets {
                    if let Some(target_blockers) = boundary_preds.remove(&support_target) {
                        for blocker in target_blockers {
                            let remove_waiter =
                                boundary_waiters.get_mut(&blocker).is_some_and(|waiters| {
                                    waiters.remove(&support_target);
                                    waiters.is_empty()
                                });
                            if remove_waiter {
                                boundary_waiters.remove(&blocker);
                            }
                        }
                    }
                    anchors.remove(&support_target);
                    entered_region |= region.insert(support_target.clone());
                    let was_expanded = expanded.remove(&support_target);
                    released.push(support_target.clone());
                    if pending.insert(support_target.clone()) || was_expanded {
                        queue.push(support_target);
                    }
                }
                if entered_region {
                    support_component_cache.clear();
                    support_components.clear();
                }
            }
            for target in succ(&i).iter().map(|e| e.to.clone()) {
                if region.contains(&target) || boundary_preds.contains_key(&target) {
                    continue;
                }
                if self.root_instances.contains(&target)
                    || self.index.always_emitted.contains_key(&target)
                {
                    anchors.insert(target);
                    continue;
                }
                let blockers = predecessor_blockers(
                    &target,
                    &region,
                    &mut boundary_predecessor_edges_examined,
                );
                if blockers.is_empty() {
                    if region.insert(target.clone()) {
                        support_component_cache.clear();
                        support_components.clear();
                        released.push(target.clone());
                    }
                    if pending.insert(target.clone()) {
                        queue.push(target);
                    }
                } else {
                    anchors.insert(target.clone());
                    boundary_preds.insert(target.clone(), blockers.clone());
                    for pred in blockers {
                        boundary_waiters
                            .entry(pred)
                            .or_default()
                            .insert(target.clone());
                    }
                }
            }
        }
        if region.is_empty() {
            return Prune {
                seeds,
                boundary_predecessor_edges_examined,
                ..Prune::default()
            };
        }
        let mut alive: Vec<Instance> = anchors.iter().cloned().collect();
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
        Prune {
            pruned: region.difference(&survives).cloned().collect(),
            region: region.len(),
            seeds,
            boundary_predecessor_edges_examined,
        }
    }
}

/// Work performed by one graph build or update.
#[derive(Default, Debug, Clone, Copy)]
pub struct RunStats {
    pub expanded: usize,
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
    /// The initial instances whose incoming edges changed.
    seeds: BTreeSet<Instance>,
    /// Predecessor edges examined while classifying local boundary blockers.
    boundary_predecessor_edges_examined: usize,
}

/// Work and changes recorded during a body edit.
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
    pub suspect_region: usize,
    /// The initial instances whose incoming edges changed.
    pub prune_seeds: BTreeSet<Instance>,
    /// Predecessor edges examined while classifying local boundary blockers.
    pub boundary_predecessor_edges_examined: usize,
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
    /// Live instances whose owner records were revisited or newly installed.
    pub revisited: BTreeSet<Instance>,
    /// Owner contributions replaced by this update, including globals and
    /// decorators that have no instance key.
    pub revisited_owners: BTreeSet<Owner>,
}

#[allow(clippy::too_many_arguments)]
fn walk_body_edges(
    w: &mut Walk,
    ws: &Workspace,
    inf: &BodyInference,
    body: Option<&bone_core::hir::Body>,
    from: Option<&Instance>,
    subst: &MonoSubst,
    // WHICH arena the sites recorded here index, and -- for a foreign one --
    // which of its expressions this owner actually reaches. `None` is the whole
    // body, which is what an own-domain walk always wants.
    domain: &SiteDomain,
    only: Option<&FxHashSet<Idx<bone_core::hir::Expr>>>,
) {
    let reached = |e: Idx<bone_core::hir::Expr>| only.is_none_or(|s| s.contains(&e));
    let mut resolved_calls: FxHashMap<Idx<bone_core::hir::Expr>, Instance> = FxHashMap::default();
    let mut calls: Vec<(Idx<bone_core::hir::Expr>, (ModuleId, Idx<Function>))> = inf
        .resolved_overload_calls
        .iter()
        .map(|(e, r)| (*e, *r))
        .collect();
    calls.sort_by_key(|(e, _)| *e);
    if only.is_none() {
        // Nearly every resolved call becomes an edge and a reached row.
        resolved_calls.reserve(calls.len());
        w.out.reached.reserve(calls.len());
        if from.is_some() {
            w.out.edges.reserve(calls.len());
        }
    }
    for (e, (m, f)) in calls {
        if !reached(e)
            || body
                .is_some_and(|body| matches!(body.expr_arena[e], bone_core::hir::Expr::Global(_)))
        {
            continue;
        }
        // bone's collapsed representative, verbatim. Selecting the sibling that
        // serves a target happens in `artifact::realize`, not here.
        let callee = FuncRef { module: m, func: f };
        let bindings = inf.call_generic_bindings.get(&e);
        let span = body
            .and_then(|b| b.expr_spans.get(e).copied())
            .unwrap_or_else(Span::unknown);
        if let Some(to) = record_call(
            w,
            ws,
            inf,
            from,
            subst,
            callee,
            bindings,
            span,
            Site {
                domain: domain.clone(),
                expr: e,
                op: SiteOp::DirectCall,
            },
        ) {
            resolved_calls.insert(e, to);
        }
        let function = &ws.module(m).functions[f];
        if let Some(annotation) = function
            .annotations
            .iter()
            .find(|a| a.is_builtin("preflight"))
        {
            // MIR may move preparation before argument construction and call
            // the prepared tail. Both need concrete delivery plans, including
            // when their only caller is synthesized by that transformation.
            for (argument, site_name) in annotation.args.iter().zip(["preflight", "preflight_tail"])
            {
                let bone_core::hir::AnnoValue::String(helper_name) = argument.value else {
                    continue;
                };
                let matches: Vec<_> = ws
                    .module(m)
                    .functions
                    .iter()
                    .filter(|(_, candidate)| {
                        candidate.name.as_str().rsplit("::").next() == Some(helper_name.as_str())
                    })
                    .map(|(id, _)| id)
                    .collect();
                if let [helper_id] = matches.as_slice() {
                    record_call(
                        w,
                        ws,
                        inf,
                        from,
                        subst,
                        FuncRef {
                            module: m,
                            func: *helper_id,
                        },
                        None,
                        span,
                        Site {
                            domain: domain.clone(),
                            expr: e,
                            op: SiteOp::Synthesized(site_name),
                        },
                    );
                }
            }
        }
    }

    if let Some(body) = body {
        let fn_values = fn_value_sites(ws, inf, body);
        for (e, callee) in fn_values.into_iter().filter(|(e, _)| reached(*e)) {
            let span = body
                .expr_spans
                .get(e)
                .copied()
                .unwrap_or_else(Span::unknown);
            let cf = &ws.module(callee.module).functions[callee.func];
            if cf.generics.is_empty() {
                record_call(
                    w,
                    ws,
                    inf,
                    from,
                    subst,
                    callee,
                    None,
                    span,
                    Site {
                        domain: domain.clone(),
                        expr: e,
                        op: SiteOp::FnValue,
                    },
                );
            } else if let Some(from) = from {
                w.record_site(
                    Some(from),
                    Site {
                        domain: domain.clone(),
                        expr: e,
                        op: SiteOp::FnValue,
                    },
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
                w.saw(span);
            }
        }

        // Calls lowering synthesizes to named runtime entries are not modelled
        // per site: every build roots those entries. A conditional dispatch
        // arm calls an ORDINARY stdlib declaration, so it is recorded here.
        let dispatched_sites = dispatched_sites(ws, inf, body, subst, &w.scratch.rep_arena, domain);
        for (site, name) in dispatched_sites
            .into_iter()
            .filter(|(site, _)| reached(site.expr))
        {
            let e = site.expr;
            let span = body
                .expr_spans
                .get(e)
                .copied()
                .unwrap_or_else(Span::unknown);
            match skull_mir::lower::dispatched_callee_decl(ws, name) {
                Some(callee) => {
                    record_call(w, ws, inf, from, subst, callee, None, span, site);
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

    let mut implicit: Vec<(Idx<bone_core::hir::Expr>, &IterWitness)> = inf
        .implicit_iter_args
        .iter()
        .map(|(e, w)| (*e, w))
        .collect();
    implicit.sort_by_key(|(e, _)| *e);
    for (e, witness) in implicit.into_iter().filter(|(e, _)| reached(*e)) {
        let span = body
            .and_then(|b| b.expr_spans.get(e).copied())
            .unwrap_or_else(Span::unknown);
        record_witness(
            w,
            ws,
            inf,
            from,
            subst,
            witness,
            span,
            Site {
                domain: domain.clone(),
                expr: e,
                op: SiteOp::ImplicitIter,
            },
        );
    }

    let mut trees: Vec<(Idx<bone_core::hir::Expr>, &bone_core::prelude::BoundTree)> = inf
        .bound_call_trees
        .iter()
        .map(|(e, tree)| (*e, tree))
        .collect();
    trees.sort_by_key(|(e, _)| *e);
    for (e, tree) in trees.into_iter().filter(|(e, _)| reached(*e)) {
        let span = body
            .and_then(|b| b.expr_spans.get(e).copied())
            .unwrap_or_else(Span::unknown);
        let concrete = walk_bound_tree(
            w,
            ws,
            inf,
            from,
            subst,
            tree,
            span,
            e,
            BoundRoot::Call,
            domain,
            &[],
        );
        if let Some(to) = resolved_calls.get(&e) {
            w.record_bound_tree(to, concrete);
        }
    }

    let mut bound_calls: Vec<(Idx<bone_core::hir::Expr>, usize)> =
        inf.bound_calls.iter().map(|(e, idx)| (*e, *idx)).collect();
    bound_calls.sort_by_key(|(e, _)| *e);
    for (e, gidx) in bound_calls.into_iter().filter(|(e, _)| reached(*e)) {
        let span = body
            .and_then(|b| b.expr_spans.get(e).copied())
            .unwrap_or_else(Span::unknown);
        let site = Site {
            domain: domain.clone(),
            expr: e,
            op: SiteOp::DirectCall,
        };
        w.record_caller_witness(from, gidx, site, span);
    }

    let mut iters: Vec<(Idx<bone_core::hir::Expr>, &bone_core::prelude::ForIterInfo)> = inf
        .for_iter_protocol
        .iter()
        .map(|(e, info)| (*e, info))
        .collect();
    iters.sort_by_key(|(e, _)| *e);
    for (e, info) in iters.into_iter().filter(|(e, _)| reached(*e)) {
        let span = body
            .and_then(|b| b.expr_spans.get(e).copied())
            .unwrap_or_else(Span::unknown);
        if let Some(witness) = &info.iter {
            record_witness(
                w,
                ws,
                inf,
                from,
                subst,
                witness,
                span,
                Site {
                    domain: domain.clone(),
                    expr: e,
                    op: SiteOp::ForIter,
                },
            );
        }
        let _ = record_bound_next(
            w,
            ws,
            inf,
            from,
            subst,
            &info.next,
            span,
            e,
            BoundRoot::For,
            domain,
            &[],
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
    bindings: Option<&Vec<bone_core::prelude::TyId>>,
    span: Span,
    site: Site,
) -> Option<Instance> {
    let cf = &ws.module(callee.module).functions[callee.func];
    let instantiated = instantiate(
        w.interner,
        ws,
        inf,
        callee.module,
        callee.func,
        cf,
        bindings,
        subst,
    );
    record_instantiation(
        w,
        ws,
        from,
        callee,
        instantiated,
        span,
        site,
        CallKind::Direct,
    )
}

enum CallKind {
    Direct,
    Iter,
    Next,
}

impl CallKind {
    fn unconstrained_message(self, name: &str) -> String {
        match self {
            Self::Direct => format!(
                "`{name}` is called with a type argument no constraint reaches, so there is \
                 no concrete type to instantiate it at"
            ),
            Self::Iter => {
                format!("the `iter` witness `{name}` binds a type argument no constraint reaches")
            }
            Self::Next => {
                format!("the `next` witness `{name}` binds a type argument no constraint reaches")
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn record_instantiation(
    w: &mut Walk,
    ws: &Workspace,
    from: Option<&Instance>,
    callee: FuncRef,
    instantiated: Result<(Vec<MonoTyId>, MonoSubst), Fail>,
    span: Span,
    site: Site,
    kind: CallKind,
) -> Option<Instance> {
    let (args, subst) = match instantiated {
        Ok(result) => result,
        Err(fail) => {
            let reason = fail.reason();
            let name = || mangle_fn(ws, &ws.module(callee.module).functions[callee.func]);
            match (from, fail) {
                (None, _) => w.saw(span),
                (Some(from), Fail::Unconstrained) => w.push_rejection(Rejection {
                    kind: RejectionKind::UnconstrainedTypeVar,
                    at: from.clone(),
                    span,
                    message: kind.unconstrained_message(&name()),
                }),
                (Some(from), Fail::Unresolved(_)) => w.push_unresolved(Unresolved {
                    from: from.clone(),
                    callee,
                    reason,
                    span,
                }),
            }
            w.record_site(from, site, SiteOutcome::Failed(reason), span);
            return None;
        }
    };
    let to = Instance { func: callee, args };
    if from.is_some() {
        w.push_edge(Edge {
            to: to.clone(),
            span,
        });
    } else {
        w.out.always_emitted_targets.insert(to.clone());
    }
    w.record_site(from, site, SiteOutcome::Resolved(to.clone()), span);
    w.reach(to.clone(), subst);
    Some(to)
}

/// Polymorphic recursion reaches a different instance of the same declaration.
/// Independent specializations of ordinary recursion never reach each other.
fn detect_changing_instance_recursion(g: &InstanceGraph, ws: &Workspace) -> Vec<RecursionFinding> {
    // Borrow edges to avoid cloning instance keys at every step of the search.
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
    // The per-declaration instance counts are a maintained index, not a fresh
    // pass over every instance.
    // Starts are instances with an OUTGOING edge -- a leaf can never reach a
    // different instantiation of itself -- while the per-declaration COUNT is
    // over every instance, because a declaration's second instance can be a
    // leaf and is exactly what makes the first one's search worth running.
    let starts = succ.keys().copied().collect();
    search_changing_instance_recursion(g, ws, starts, |node| succ.get(node).copied().unwrap_or(&[]))
}

fn detect_changing_instance_recursion_local(
    g: &InstanceGraph,
    ws: &Workspace,
    seeds: &BTreeSet<Instance>,
) -> Vec<RecursionFinding> {
    // The local path probes only its reverse-reachable seed region; constructing
    // a whole-graph adjacency table here would make a small edit global work.
    let successors = |node: &Instance| {
        g.contributions
            .get(&Owner::Instance(node.clone()))
            .map_or(&[][..], |c| c.edges.as_slice())
    };
    let starts = seeds
        .iter()
        .filter(|node| !successors(node).is_empty())
        .collect();
    search_changing_instance_recursion(g, ws, starts, successors)
}

fn search_changing_instance_recursion<'a>(
    g: &InstanceGraph,
    ws: &Workspace,
    mut order: Vec<&'a Instance>,
    successors: impl Fn(&Instance) -> &'a [Edge] + Sync,
) -> Vec<RecursionFinding> {
    order.retain(|i| g.index.decls.get(&i.func).is_some_and(|n| *n > 1));
    // Each start's search reads only the graph, so they run in parallel. A
    // program without polymorphic recursion -- the normal case -- is then
    // done: the ordered pass below exists to report findings in a stable
    // order and to skip a declaration once it has one.
    let any_finding = {
        use bone_core::prelude::*;
        order.par_iter().any(|start| {
            let mut seen: FxHashSet<&Instance> = FxHashSet::default();
            let mut queue = vec![*start];
            while let Some(node) = queue.pop() {
                if node.func == start.func && node.args != start.args {
                    return true;
                }
                for edge in successors(node) {
                    if seen.insert(&edge.to) {
                        queue.push(&edge.to);
                    }
                }
            }
            false
        })
    };
    if !any_finding {
        return Vec::new();
    }
    order.sort_by_cached_key(|instance| g.diagnostic_key(instance));
    let mut found = Vec::new();
    let mut already = BTreeSet::new();
    // Membership is unordered; traversal order comes only from the sorted
    // starts and the edge stack. Reuse scratch across starts without copying
    // instances that will be discarded when a search finds no error.
    let mut seen: FxHashSet<&Instance> = FxHashSet::default();
    let mut subjects = Vec::new();
    let mut queue = Vec::new();
    for start in order {
        let key = DeclKey(start.func);
        if already.contains(&key) {
            continue;
        }
        seen.clear();
        subjects.clear();
        queue.clear();
        queue.push((start, Span::unknown()));
        while let Some((node, via)) = queue.pop() {
            subjects.push(node);
            if node.func == start.func && node.args != start.args {
                // Only a witness discharges this declaration. Another instance
                // can have changing recursion even when this one's path is finite.
                already.insert(key);
                found.push(RecursionFinding {
                    rejection: Rejection {
                        kind: RejectionKind::ChangingInstanceRecursion,
                        at: start.clone(),
                        span: via,
                        message: format!(
                            "polymorphic recursion: `{}` recurs as `{}`; instantiation does not \
                             terminate on a recursive call whose type arguments change",
                            g.symbol(ws, start),
                            g.symbol(ws, node)
                        ),
                    },
                    subjects: subjects.iter().copied().cloned().collect(),
                });
                break;
            }
            for edge in successors(node) {
                if seen.insert(&edge.to) {
                    queue.push((&edge.to, edge.span));
                }
            }
        }
    }
    found
}

/// Both for loops and implicit iterable arguments carry a witness substitution.
#[allow(clippy::too_many_arguments)]
fn record_witness(
    w: &mut Walk,
    ws: &Workspace,
    inf: &BodyInference,
    from: Option<&Instance>,
    subst: &MonoSubst,
    witness: &IterWitness,
    span: Span,
    site: Site,
) {
    let callee = FuncRef {
        module: witness.func.0,
        func: witness.func.1,
    };
    let cf = &ws.module(callee.module).functions[callee.func];
    let instantiated = instantiate_witness(w.interner, ws, inf, cf, &witness.generics, subst);
    record_instantiation(
        w,
        ws,
        from,
        callee,
        instantiated,
        span,
        site,
        CallKind::Iter,
    );
}

/// Walk only omitted default fields. Record sites under the declaring struct's
/// arena and type arguments. A struct update with a base omits no fields.
fn walk_struct_default_edges(
    w: &mut Walk,
    ws: &Workspace,
    module: ModuleId,
    body: &Body,
    top_owner: BodyOwner,
    from: Option<&Instance>,
    subst: &MonoSubst,
) {
    use bone_core::hir::Expr as E;
    // Distinct concrete argument lists can resolve different callees in one default.
    // Sort by canonical type encoding rather than interner insertion order.
    type Key = (usize, u32, Vec<String>);
    let mut omitted: BTreeMap<
        Key,
        (
            ModuleId,
            Idx<bone_core::hir::StructDef>,
            Vec<skull_mir::monoty::MonoTy>,
            BTreeSet<ustr::Ustr>,
        ),
    > = BTreeMap::new();
    // Defaults can contain literals that introduce further omitted fields.
    let Some(top_inf) = ws
        .inference(module)
        .and_then(|ir| ir.per_fn.get(&top_owner))
    else {
        return;
    };
    let mut pending: Vec<(
        &Body,
        &BodyInference,
        MonoSubst,
        Option<FxHashSet<Idx<bone_core::hir::Expr>>>,
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
            let E::StructLiteral {
                name,
                fields,
                bases,
            } = expr
            else {
                continue;
            };
            if !bases.is_empty() {
                continue;
            }
            let Some(sid) = ws.struct_by_name.get(name) else {
                continue;
            };
            // The same oracle lowering stamps the domain with, INCLUDING its
            // shared-body fallback -- a literal keyed one way here and the
            // other way there would report a hole that is really a
            // disagreement. A hard error records no obligation: lowering
            // rejects that program outright, so there is nothing to join.
            let generics = skull_mir::struct_defaults::declared_generics(ws, *sid);
            let Ok(args) =
                skull_mir::struct_defaults::args_at(ws, scan_inf, &scan_subst, e, *name, generics)
            else {
                continue;
            };
            let args = args.to_vec();
            let written: BTreeSet<ustr::Ustr> = (*fields)
                .map(|f| scan.struct_lit_field_arena[f].name)
                .collect();
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
            let Some(defaults) = ws.modules[module.index()].bodies.get(&owner) else {
                continue;
            };
            let Some(dinf) = ws.inference(*module).and_then(|ir| ir.per_fn.get(&owner)) else {
                continue;
            };
            let mut sub: FxHashSet<Idx<bone_core::hir::Expr>> = FxHashSet::default();
            for f in fields {
                let Some(root) = defaults.field_defaults.get(f) else {
                    continue;
                };
                sub.extend(crate::expr_subtree::expr_subtree(defaults, *root));
            }
            let inner = skull_mir::struct_defaults::slot_subst(Some(dinf), args);
            pending.push((defaults, dinf, inner, Some(sub)));
        }
    }

    for (_, (module, def, args, fields)) in omitted {
        let owner = BodyOwner::StructDefaults(def);
        let Some(defaults) = ws.modules[module.index()].bodies.get(&owner) else {
            continue;
        };
        let Some(inf) = ws.inference(module).and_then(|ir| ir.per_fn.get(&owner)) else {
            continue;
        };
        let mut reached: FxHashSet<Idx<bone_core::hir::Expr>> = FxHashSet::default();
        for f in &fields {
            let Some(root) = defaults.field_defaults.get(f) else {
                continue;
            };
            reached.extend(crate::expr_subtree::expr_subtree(defaults, *root));
        }
        // The initializers resolve under the STRUCT's slot bindings, not the
        // enclosing instance's -- the two are keyed in different bodies.
        let inner = skull_mir::struct_defaults::slot_subst(Some(inf), &args);
        let domain = SiteDomain::struct_defaults(module, def, args);
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
    expr: Idx<bone_core::hir::Expr>,
    root: BoundRoot,
    domain: &SiteDomain,
    path: &[usize],
) -> Option<InstanceBoundNext> {
    let site = root.site(domain.clone(), expr, path.to_vec());
    let (callee, next_generics, sub) = match next {
        BoundNext::CallerGeneric(gidx) => return w.record_caller_witness(from, *gidx, site, span),
        BoundNext::Concrete {
            func: (module, func),
            next_generics,
            sub,
        } => (
            FuncRef {
                module: *module,
                func: *func,
            },
            next_generics,
            sub,
        ),
    };
    let cf = &ws.module(callee.module).functions[callee.func];
    let instantiated = instantiate_witness(w.interner, ws, inf, cf, next_generics, subst);
    let to = record_instantiation(
        w,
        ws,
        from,
        callee,
        instantiated,
        span,
        site,
        CallKind::Next,
    )?;
    let concrete_sub =
        walk_bound_tree(w, ws, inf, from, subst, sub, span, expr, root, domain, path);
    w.record_bound_tree(&to, concrete_sub.clone());
    Some(InstanceBoundNext {
        callee: to,
        sub: concrete_sub,
    })
}

#[allow(clippy::too_many_arguments)]
fn walk_bound_tree(
    w: &mut Walk,
    ws: &Workspace,
    inf: &BodyInference,
    from: Option<&Instance>,
    subst: &MonoSubst,
    tree: &bone_core::prelude::BoundTree,
    span: Span,
    expr: Idx<bone_core::hir::Expr>,
    root: BoundRoot,
    domain: &SiteDomain,
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
            w,
            ws,
            inf,
            from,
            subst,
            &tree.next[k],
            span,
            expr,
            root,
            domain,
            &child,
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
    interner: &MonoInterner,
    ws: &Workspace,
    inf: &BodyInference,
    callee_module: ModuleId,
    callee_func: Idx<Function>,
    callee: &Function,
    bindings: Option<&Vec<bone_core::prelude::TyId>>,
    caller_subst: &MonoSubst,
) -> Result<(Vec<MonoTyId>, MonoSubst), Fail> {
    if callee.generics.is_empty() {
        return Ok((Vec::new(), MonoSubst::default()));
    }
    let Some(bindings) = bindings else {
        return Err(Fail::Unresolved("no-bindings"));
    };
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
                ) && !generic_parameter_is_observable(
                    ws,
                    callee_module,
                    callee_func,
                    callee,
                    var,
                ) =>
            {
                // A generic slot that appears nowhere in the callee's signature,
                // bounds, or inferred body cannot affect its concrete plan. Keep
                // the instance total with an explicit erased logical type; an
                // unresolved slot that the body can read still rejects below.
                skull_mir::monoty::MonoTy::Any
            }
            Err(reason) => return Err(convert_fail(reason)),
        };
        args.push(interner.intern(ty.clone()));
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
    let Some(inf) = ws
        .inference(callee_module)
        .and_then(|ir| ir.per_fn.get(&owner))
    else {
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
        .flat_map(|body| {
            body.binding_arena
                .iter()
                .filter_map(|(_, binding)| binding.declared_ty)
        })
        .any(|ty| mentions(ws, Some(inf), ty, var, &mut seen))
}

/// Instantiate a witness using the inference record keyed by its generic variables.
fn instantiate_witness(
    interner: &MonoInterner,
    ws: &Workspace,
    inf: &BodyInference,
    callee: &Function,
    record: &FxHashMap<bone_core::hir::VarId, bone_core::prelude::TyId>,
    caller_subst: &MonoSubst,
) -> Result<(Vec<MonoTyId>, MonoSubst), Fail> {
    if callee.generics.is_empty() {
        return Ok((Vec::new(), MonoSubst::default()));
    }
    let mut args = Vec::with_capacity(callee.generics.len());
    let mut callee_subst = MonoSubst::default();
    for generic in &callee.generics {
        let Some(var) = skull_mir::monoty::generic_var_of(ws, inf, *generic) else {
            return Err(Fail::Unresolved("declared-generic-not-generic"));
        };
        let Some(bound) = record.get(&var) else {
            return Err(Fail::Unresolved("witness-generic-unbound"));
        };
        let ty = skull_mir::monoty::convert(ws, inf, *bound, caller_subst).map_err(convert_fail)?;
        args.push(interner.intern(ty.clone()));
        callee_subst.insert(var, ty);
    }
    Ok((args, callee_subst))
}

/// An unresolved record and an unconstrained type have different diagnostics.
enum Fail {
    Unresolved(&'static str),
    Unconstrained,
}

impl Fail {
    fn reason(&self) -> &'static str {
        match self {
            Self::Unresolved(r) => r,
            Self::Unconstrained => "unconstrained-type-var",
        }
    }
}

fn convert_fail(r: MonoTyReason) -> Fail {
    match r {
        MonoTyReason::UnresolvedInfer(_) | MonoTyReason::UnboundGeneric(_) => Fail::Unconstrained,
        _ => Fail::Unresolved(r.bucket()),
    }
}

/// Collect function values, excluding global expressions used as indirect callees.
fn fn_value_sites(
    ws: &Workspace,
    inf: &BodyInference,
    body: &bone_core::hir::Body,
) -> Vec<(Idx<bone_core::hir::Expr>, FuncRef)> {
    use bone_core::hir::{Callee, Expr as E};
    let mut called_values: BTreeSet<u32> = BTreeSet::new();
    for (e, expr) in body.expr_arena.iter() {
        match expr {
            E::Call {
                callee: Callee::Value(ve),
                ..
            } => {
                called_values.insert(ve.into_raw().into());
            }
            E::Call {
                callee: Callee::Unresolved { name, .. },
                args,
                ..
            } if skull_mir::lower::is_shader_pipeline_call(ws, inf, e, *name) => {
                called_values.extend(args.into_iter().map(|a| u32::from(a.into_raw())));
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    for (e, expr) in body.expr_arena.iter() {
        let E::Global(name) = expr else { continue };
        if called_values.contains(&e.into_raw().into()) {
            continue;
        }
        let Some(cands) = ws.functions_by_name.get(name) else {
            continue;
        };
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

fn dispatched_sites(
    ws: &Workspace,
    inf: &BodyInference,
    body: &bone_core::hir::Body,
    subst: &MonoSubst,
    rep_arena: &RepArena,
    domain: &SiteDomain,
) -> Vec<(Site, &'static str)> {
    use bone_core::hir::{BinOp, Expr as E, InterpPart};
    let dcx = skull_mir::lower::DispatchCtx {
        ws,
        inf: Some(inf),
        body,
        concrete_plan: None,
        concrete_types: None,
        interner: None,
        rep_arena,
        target: bone_core::hir::FnBackend::Any,
        logical_subst: Some(subst),
        values_first_class: false,
    };
    // The shared graph serves both scalarized Lua bodies and native C bodies.
    // A `nobox value` interpolates as leaves in the former but as one aggregate
    // in the latter, so retain the union of the same oracle under both ABI
    // modes. The site key keeps the two forms distinct; each lowerer consumes
    // only the obligation its own mode emits.
    let dispatches = [
        dcx,
        skull_mir::lower::DispatchCtx {
            values_first_class: true,
            ..dcx
        },
    ];
    let mut out: Vec<(Site, &'static str)> = Vec::new();
    for (e, expr) in body.expr_arena.iter() {
        match expr {
            E::InterpolatedString(parts) => {
                for p in *parts {
                    let InterpPart::Value { expr: hole, spec } = &body.interp_part_arena[p] else {
                        continue;
                    };
                    for dispatch in &dispatches {
                        for (path, name) in dispatch.hole_plan(*hole, spec.as_ref()).obligations() {
                            out.push((
                                Site {
                                    domain: domain.clone(),
                                    expr: *hole,
                                    op: SiteOp::dispatched(name, path),
                                },
                                name,
                            ));
                        }
                    }
                }
            }
            E::Call { .. } => {
                for dispatch in &dispatches {
                    let routes = dispatch
                        .print_routes(e)
                        .into_iter()
                        .chain(dispatch.tostring_route(e));
                    for (arg, route) in routes {
                        for (path, name) in route.obligations() {
                            out.push((
                                Site {
                                    domain: domain.clone(),
                                    expr: arg,
                                    op: SiteOp::dispatched(name, path),
                                },
                                name,
                            ));
                        }
                    }
                }
            }
            E::BinOp {
                op: op @ (BinOp::Eq | BinOp::NotEq),
                lhs,
                rhs,
            } => {
                if let Some(name) = dcx.eq_route(*lhs, *rhs).synthesized_callee(*op) {
                    out.push((
                        Site {
                            domain: domain.clone(),
                            expr: e,
                            op: SiteOp::dispatched(name, Vec::new()),
                        },
                        name,
                    ));
                }
            }
            _ => {}
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Resolve a bare runtime entry to its target-neutral declaration.
pub fn stdlib_entry_representative(ws: &Workspace, name: &str) -> Option<FuncRef> {
    let decls = ws.functions_by_name.get(&ustr::Ustr::from(name))?;
    let mut reps = decls
        .iter()
        .map(|&(m, f)| skull_mir::lower::canonical_variant(ws, FuncRef { module: m, func: f }));
    let first = reps.next()?;
    reps.all(|r| r == first).then_some(first)
}

/// The representative of every entry lowering may synthesize a call to
/// ([`skull_mir::lower::SYNTHESIZED_ENTRIES`]). JS roots these in its graph;
/// Lua already roots every `@nomangle` one, and renders the rest as templates
/// at the call. A synthesized call resolves to the entry's base instance by
/// name, whatever storage decision made lowering emit it.
pub fn synthesized_entry_roots(ws: &Workspace) -> Vec<FuncRef> {
    skull_mir::lower::SYNTHESIZED_ENTRIES
        .iter()
        .filter_map(|name| stdlib_entry_representative(ws, name))
        .collect()
}

/// Whether a declaration with backend `decl` can be emitted for `target`.
/// THE compatibility test -- the walk, the boundary pass and every artifact
/// view ask this one, so "does this declaration serve me" cannot be answered
/// two ways.
pub fn serves(decl: bone_core::hir::FnBackend, target: bone_core::hir::FnBackend) -> bool {
    use bone_core::hir::FnBackend;
    decl == FnBackend::Any || target == FnBackend::Any || decl == target
}

/// The boundary class of one declaration, or `None` if it has a Rock body.
/// [`classify_boundaries`] uses this classifier so boundary reporting cannot
/// drift across declarations.
pub fn boundary_class_for(f: &Function) -> Option<BoundaryClass> {
    if is_erased_runtime_entry(f) {
        return Some(BoundaryClass::ErasedRuntimeEntry);
    }
    declared_boundary(f)
}

/// The classification without the erased-runtime-entry identity. It is a
/// SUBSET of the full classification, so nothing the walk calls expandable can
/// later be excused as a boundary and nothing the walk skips can become
/// un-excused.
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

/// Resolve bare `@nomangle` names in JS opaque bodies into root declarations.
/// The body remains opaque to MIR; this is only the publication obligation for
/// symbols the text calls by source name.
pub fn opaque_boundary_roots_for_decls(
    ws: &Workspace,
    decls: impl IntoIterator<Item = FuncRef>,
) -> BTreeMap<DeclKey, FxHashSet<FuncRef>> {
    // Each declaration's group is scanned independently, and few members
    // have an opaque JS body at all.
    let decls: Vec<FuncRef> = decls.into_iter().collect();
    let mut nomangle_roots: FxHashMap<ustr::Ustr, Vec<FuncRef>> = FxHashMap::default();
    for module in &ws.modules {
        for (func, function) in module.functions.iter() {
            if function
                .annotations
                .iter()
                .any(|annotation| annotation.is_builtin("nomangle"))
            {
                let name =
                    ustr::Ustr::from(skull_mir::mangle::declared_name(function.name.as_str()));
                nomangle_roots.entry(name).or_default().push(FuncRef {
                    module: module.id,
                    func,
                });
            }
        }
    }
    let found: Vec<(DeclKey, FxHashSet<FuncRef>)> = {
        use bone_core::prelude::*;
        decls
            .par_iter()
            .map(|decl| {
                skull_mir::lower::backend_variant_group(ws, *decl)
                    .into_iter()
                    .filter_map(|member| {
                        opaque_js_nomangle_targets(ws, member, &nomangle_roots)
                            .map(|t| (DeclKey(member), t))
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>()
            .into_iter()
            .flatten()
            .collect()
    };
    let mut roots: BTreeMap<DeclKey, FxHashSet<FuncRef>> = BTreeMap::new();
    for (owner, targets) in found {
        roots.entry(owner).or_default().extend(targets);
    }
    roots
}

/// The `@nomangle` declarations an opaque JS body names, or `None` when the
/// declaration has no such body. A reader takes an absent record as empty.
fn opaque_js_nomangle_targets(
    ws: &Workspace,
    member: FuncRef,
    nomangle_roots: &FxHashMap<ustr::Ustr, Vec<FuncRef>>,
) -> Option<FxHashSet<FuncRef>> {
    let f = &ws.module(member.module).functions[member.func];
    let Some((bone_core::hir::FnBackend::Js, body)) = f.inline_body else {
        return None;
    };
    let mut targets = FxHashSet::default();
    for name in crate::inline_js::bare_names(body.as_str()) {
        if let Some(candidates) = nomangle_roots.get(&ustr::Ustr::from(name)) {
            targets.extend(candidates.iter().copied());
        }
    }
    Some(targets)
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
    // Few declarations are boundaries; the pool finds them, and only those
    // are named, since a mangled name is a formatted string.
    let found: Vec<Boundary> = {
        use bone_core::prelude::*;
        decls
            .par_iter()
            .filter_map(|decl| {
                let f = &ws.module(decl.0.module).functions[decl.0.func];
                let class = boundary_class_for(f)?;
                Some(Boundary {
                    class,
                    symbol: mangle_fn(ws, f),
                })
            })
            .collect()
    };
    for boundary in found {
        g.boundaries.insert(boundary.symbol.clone(), boundary);
    }
}

fn record_coroutine_helpers(
    g: &mut InstanceGraph,
    ws: &Workspace,
    added: &[Instance],
    removed: &[Instance],
) {
    for i in removed {
        let identity = g.identity(ws, i);
        g.remove_coroutine_helpers(&identity);
        g.remove_coroutine_rejection(i);
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
                g.install_coroutine_rejection(Rejection {
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
            g.insert_coroutine_helper(CoroutineHelper {
                owner_identity: owner_identity.clone(),
                suffix,
                symbol_today: format!("{}{suffix}", plan.base()),
            });
        }
    }
}

pub(crate) enum FunctionBody<'a> {
    Ready {
        inf: &'a BodyInference,
        body: &'a Body,
    },
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
