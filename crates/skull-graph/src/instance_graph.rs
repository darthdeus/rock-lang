use bone_core::prelude::Instant;
use parking_lot::Mutex;
use std::collections::{BTreeMap, BTreeSet};

use bone_core::hir::{Body, BodyOwner, Function, TypeExpr, VarId};
use bone_core::hir_walk::HirChild;
use bone_core::prelude::{BoundNext, IterWitness, ModuleId, Span, TyId};
use bone_core::workspace::{BodyInference, Workspace};
use comfy_la_arena::Idx;
use rustc_hash::{FxHashMap, FxHashSet};
use skull_mir::lower::RepArena;
use skull_mir::mangle::mangle_fn;
use skull_mir::mir::FuncRef;
use skull_mir::monoty::{MonoInterner, MonoSubst, MonoTyId, MonoTyReason};

pub use skull_mir::instance::{DeclKey, Instance};

pub use skull_mir::lower::BoundaryClass;

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Boundary {
    pub class: BoundaryClass,
    pub symbol: String,
}

#[derive(Clone, Debug)]
pub struct Edge {
    pub to: Instance,
    pub span: Span,
}

pub use skull_mir::site::{Site, SiteDomain, SiteOp};

/// `Failed` is kept beside successes, so an absent site means no such operation.
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

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct InstanceBoundNext {
    pub callee: Instance,
    pub sub: InstanceBoundTree,
}

#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct InstanceBoundTree {
    pub next: BTreeMap<usize, InstanceBoundNext>,
}

impl InstanceBoundTree {
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

/// A construction the model refuses, as opposed to one it could not complete ([`Unresolved`]).
/// `ChangingInstanceRecursion` recurs at a different instantiation, which
/// demand-driven instantiation does not terminate on. `GenericFnValue` has no
/// call site to key an instantiation. `ConcreteTypeUnavailable` is a compiler
/// invariant break. `UnconstrainedTypeVar` is a type argument no constraint
/// reaches (a generic read in a shared body is [`Unresolved`] data instead).
/// `IdentityInOrdinaryStorage` is fatal for the instantiating package.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum RejectionKind {
    ChangingInstanceRecursion,
    GenericFnValue,
    ConcreteTypeUnavailable,
    UnconstrainedTypeVar,
    IdentityInOrdinaryStorage,
}

#[derive(Clone, Debug)]
pub struct Rejection {
    pub kind: RejectionKind,
    pub at: Instance,
    pub span: Span,
    pub message: String,
}

/// Data, not an error: the caller may itself be an uninstantiated shared body.
#[derive(Clone, Debug)]
pub struct Unresolved {
    pub from: Instance,
    pub callee: FuncRef,
    pub reason: &'static str,
    pub span: Span,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum Owner {
    Instance(Instance),
    AlwaysEmitted(ModuleId, BodyOwner),
}

impl Owner {
    /// Neither `ModuleId` nor `BodyOwner` is `Ord`.
    /// Owners are ordered so derivation is a function of the contribution set,
    /// not of discovery order.
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

/// Exhaustive on purpose: a new variant must not collapse onto an existing tag.
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

#[derive(Clone, Default, Debug)]
pub struct Contribution {
    /// Empty for an always-emitted body, whose callees are roots, not obligations.
    pub edges: Vec<Edge>,
    pub call_sites: BTreeMap<Site, SiteRecord>,
    pub always_call_sites: BTreeMap<Site, SiteRecord>,
    pub struct_default_domains: BTreeSet<SiteDomain>,
    pub unresolved: Vec<Unresolved>,
    pub rejections: Vec<Rejection>,
    pub always_emitted_targets: BTreeSet<Instance>,
    /// Substitutions are derived from these, so an instance loses its
    /// substitution with its last caller.
    pub reached: Vec<(Instance, MonoSubst)>,
    pub bound_trees: Vec<(Instance, InstanceBoundTree)>,
    pub span_files: BTreeSet<ustr::Ustr>,
    /// False for a boundary, a body without usable inference, and a capped instance.
    pub expanded: bool,
}

#[derive(Clone)]
struct RecursionFinding {
    rejection: Rejection,
    /// Every instance the witness search inspected; retraction is keyed by these.
    subjects: BTreeSet<Instance>,
}

#[derive(Default)]
struct WalkScratch {
    rep_arena: RepArena,
    identity_verdicts: std::cell::RefCell<crate::identity_storage::IdentityVerdicts>,
}

struct Walk<'a> {
    interner: &'a MonoInterner,
    scratch: &'a WalkScratch,
    owner_tree: Option<&'a InstanceBoundTree>,
    out: &'a mut Contribution,
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

    /// Every recorded span must pass through here, so `span_files` misses no file.
    fn saw(&mut self, span: Span) {
        if !is_spanless(&span) && self.last_file != Some(span.file) {
            self.last_file = Some(span.file);
            self.out.span_files.insert(span.file);
        }
    }
}

static EMPTY_SUBST: std::sync::LazyLock<MonoSubst> = std::sync::LazyLock::new(MonoSubst::default);

/// When owners disagree, the first owner in owner order decides.
#[derive(Default, Debug, Clone)]
pub struct Indices {
    pub subst: FxHashMap<Instance, Agreed<MonoSubst>>,
    pub trees: BTreeMap<Instance, Agreed<InstanceBoundTree>>,
    pub always_emitted: BTreeMap<Instance, BTreeSet<Owner>>,
    /// Tells the per-declaration post-passes which declarations entered or left.
    pub decls: FxHashMap<FuncRef, usize>,
    pub preds: FxHashMap<Instance, FxHashSet<Instance>>,
    pub decl_instances: FxHashMap<FuncRef, FxHashSet<Instance>>,
    /// For span relocation only; membership does not make an owner stale.
    pub span_owners: FxHashMap<ustr::Ustr, FxHashSet<Owner>>,
}

impl Indices {
    fn insert_update_rows(&mut self, owner: &Owner, c: &Contribution) {
        for (to, subst) in &c.reached {
            assert_subst(&mut self.subst, owner, to, subst);
        }
        if let Some(from) = owner.instance() {
            for e in &c.edges {
                self.preds
                    .entry(e.to.clone())
                    .or_default()
                    .insert(from.clone());
            }
            self.decl_instances
                .entry(from.func)
                .or_default()
                .insert(from.clone());
        }
        for file in &c.span_files {
            self.span_owners
                .entry(*file)
                .or_default()
                .insert(owner.clone());
        }
    }

    fn remove_update_rows(&mut self, owner: &Owner, old: &Contribution) {
        for (to, _) in &old.reached {
            if let Some(a) = self.subst.get_mut(to) {
                if !a.retract(owner) {
                    self.subst.remove(to);
                }
            }
        }
        if let Some(from) = owner.instance() {
            for e in &old.edges {
                remove_from_set(&mut self.preds, &e.to, from);
            }
            remove_from_set(&mut self.decl_instances, &from.func, from);
        }
        for file in &old.span_files {
            remove_from_set(&mut self.span_owners, file, owner);
        }
    }
}

fn remove_from_set<K: std::hash::Hash + Eq, V: std::hash::Hash + Eq>(
    index: &mut FxHashMap<K, FxHashSet<V>>,
    key: &K,
    value: &V,
) {
    if let Some(set) = index.get_mut(key) {
        set.remove(value);
        if set.is_empty() {
            index.remove(key);
        }
    }
}

pub use bone_core::text_edit::TextEditDelta;

#[derive(Clone, Debug)]
pub struct Agreed<T> {
    pub owners: BTreeSet<Owner>,
    pub value: T,
    /// Empty on every valid program.
    /// A non-empty one means a body built from the fact depends on which owner won.
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
            // A body asserts the fact once per site reaching the callee.
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
            // The agreeing side is gone: a dissenter becomes the answer, if any.
            let Some(first) = self.dissent.keys().next().cloned() else {
                return false;
            };
            let value = self.dissent.remove(&first).expect("just read");
            self.owners.insert(first);
            self.value = value;
        }
        true
    }

    /// Decided by owner order, not arrival, so a fresh build and an update agree.
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
    incremental_indexes: bool,
    /// A retained view checks the revision it was realized from instead of
    /// trusting call order.
    revision: u64,
    interner: std::sync::Arc<MonoInterner>,
    /// The primary records; every derived field is computed from these and `root_instances`.
    pub contributions: BTreeMap<Owner, Box<Contribution>>,
    pub root_instances: BTreeSet<Instance>,
    pub instances: BTreeSet<Instance>,
    last_run_owners: BTreeSet<Owner>,
    /// An always-emitted owner is not an instance, so these are what revisit the
    /// instances whose root or fn-value status moved.
    last_run_always_targets: BTreeSet<Instance>,
    delta_added: BTreeSet<Instance>,
    delta_removed: BTreeSet<Instance>,
    /// Whole-graph findings with no single owner, from the recursion search.
    pub graph_rejections: Vec<Rejection>,
    recursion_findings: BTreeMap<Instance, RecursionFinding>,
    recursion_by_subject: BTreeMap<Instance, BTreeSet<Instance>>,
    recursion_by_decl: BTreeMap<DeclKey, BTreeSet<Instance>>,
    recursion_positions: BTreeMap<Instance, usize>,
    coroutine_positions: BTreeMap<Instance, usize>,
    pub boundaries: BTreeMap<String, Boundary>,
    pub inline_body_nomangle_roots: BTreeMap<DeclKey, FxHashSet<FuncRef>>,
    lost: BTreeSet<Instance>,
    pub index: Indices,
}

/// Compiler-owned runtime entries a synthesized call or opaque body may name by bare name.
/// An entry need not serve every target: a call on an unserved target is
/// rejected where it is synthesized.
const ERASED_RUNTIME_ENTRIES: &[&str] = &[
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

/// Classified by name, because lowering synthesizes calls to these anywhere.
pub(crate) fn is_erased_runtime_entry(f: &Function) -> bool {
    ERASED_RUNTIME_ENTRIES.contains(&f.name.as_str())
}

pub use skull_mir::monoty::MAX_INSTANTIATION_DEPTH;

/// `Concrete` (no logical arguments, empty substitution) is the only case
/// where declaration-based passes are exact; `Missing` is never `Concrete`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Concreteness {
    Concrete,
    Generic,
    Missing,
}

impl InstanceGraph {
    /// The one substitution oracle, shared by publisher lanes and the GLSL passes.
    pub fn concreteness(&self, i: &Instance) -> Concreteness {
        match self.substitution(i) {
            None => Concreteness::Missing,
            Some(s) if s.is_empty() && i.args.is_empty() => Concreteness::Concrete,
            Some(_) => Concreteness::Generic,
        }
    }

    /// For display only: distinct instances can render alike. Key on [`Self::identity`].
    pub fn symbol(&self, ws: &Workspace, i: &Instance) -> String {
        let f = &ws.module(i.func.module).functions[i.func.func];
        let base = mangle_fn(ws, f);
        if i.args.is_empty() {
            return base;
        }
        let args: Vec<String> = i.args.iter().map(|a| self.interner.render(*a)).collect();
        format!("{base}<{}>", args.join(", "))
    }

    /// Unambiguous, so sorting and dedup cannot merge instances. Module ids follow
    /// source order, so it is deterministic across builds.
    pub fn identity(&self, ws: &Workspace, i: &Instance) -> String {
        let f = &ws.module(i.func.module).functions[i.func.func];
        let base = mangle_fn(ws, f);
        // `@nomangle` backend siblings mangle identically, so the declaration is part of the key.
        let decl = DeclKey(i.func).raw();
        if i.args.is_empty() {
            return format!("{base}@{}.{}", decl.0, decl.1);
        }
        let args: Vec<String> = i.args.iter().map(|a| self.interner.canonical(*a)).collect();
        format!("{base}@{}.{}<{}>", decl.0, decl.1, args.join(","))
    }
}

/// The file sentinel distinguishes it: a zero line range is line 1 of a file.
pub fn is_spanless(span: &Span) -> bool {
    span.file.as_str() == "<unknown>"
}

/// Build the shared instance graph for `ws`.
/// Target-neutral: callees are bone's group representatives and roots are
/// canonicalized to them; `artifact::realize` selects the sibling per target.
pub fn build(ws: &Workspace, roots: Vec<FuncRef>) -> InstanceGraph {
    build_in(ws, roots, std::sync::Arc::new(MonoInterner::new()))
}

/// [`build`] against the caller's arena, which emission must share.
pub fn build_in(
    ws: &Workspace,
    roots: Vec<FuncRef>,
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
    let stats = g.run(ws, pending, false);
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
    pub fn instances_of(&self, fref: FuncRef) -> Box<dyn Iterator<Item = &Instance> + '_> {
        if !self.incremental_indexes {
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

    /// There is no owned copy on purpose: a copy would be a second type identity.
    pub fn interner(&self) -> &std::sync::Arc<MonoInterner> {
        &self.interner
    }

    /// Release one-shot build records, keeping the shared type arena.
    pub fn take_records(&mut self) -> Self {
        let mut empty = Self::empty_in(std::sync::Arc::clone(&self.interner));
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

    fn run(&mut self, ws: &Workspace, mut pending: Vec<Owner>, update: bool) -> RunStats {
        let mut stats = RunStats::default();
        // Conformance trees come from callers, so an owner may have been walked with a stale one.
        let mut walked_with: FxHashMap<Instance, InstanceBoundTree> = FxHashMap::default();
        if update {
            self.last_run_owners.clear();
            self.last_run_always_targets.clear();
        } else {
            pending.extend(self.root_instances.iter().cloned().map(Owner::Instance));
            pending = self.run_fresh_waves(ws, pending, &mut stats, &mut walked_with);
        }
        let mut queued: FxHashSet<Owner> = FxHashSet::default();
        pending.retain(|owner| queued.insert(owner.clone()));
        let mut rounds = 0usize;
        let interner = self.interner.clone();
        loop {
            while !pending.is_empty() {
                let wave: Vec<_> = std::mem::take(&mut pending).into_iter().rev().collect();
                // Capture every walk's inputs before installing any result.
                // Rayon keeps this order, so each result installs beside its snapshot's owner.
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
                    let mut prepared_c = outcome;
                    let mut used_tree = owner.instance().map(|_| tree.unwrap_or_default());
                    queued.remove(&owner);
                    // Retraction can remove a pending instance's substitution mid-wave.
                    // Keep its old contribution until pruning checks its reachability.
                    if let Owner::Instance(i) = &owner {
                        if self.substitution(i).is_none() {
                            continue;
                        }
                        // An earlier install in this wave restored the substitution.
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
                    if update {
                        self.last_run_owners.insert(owner.clone());
                    }
                    stats.expanded += 1;
                    let c = prepared_c.expect("walkable owner");
                    for owner in self.install(owner, c, update) {
                        if queued.insert(owner.clone()) {
                            pending.push(owner);
                        }
                    }
                }
            }
            // Re-walk owners whose witnesses moved after their walk.
            // Otherwise its callees are the ones the old witnesses selected.
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

    /// `expand` is the only producer of a contribution, so a full build and an
    /// incremental re-walk cannot record different facts.
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

const SUBST_SHARDS: usize = 256;

type SubstShard = Mutex<FxHashMap<Instance, Agreed<MonoSubst>>>;

fn subst_shard(to: &Instance) -> usize {
    use std::hash::BuildHasher;
    let hash = rustc_hash::FxBuildHasher.hash_one(to);
    // Remixed: a shard's own table probes by the plain hash's bits.
    (hash.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 56) as usize
}

#[derive(Default)]
struct WaveShare {
    installed: Vec<(Owner, Box<Contribution>)>,
    // Installed by the build thread between waves, so every walk of a wave reads
    // the conformance trees it began with.
    owned: Vec<usize>,
    demanded: Vec<OwnerWalk>,
    walked_with: Vec<(Instance, InstanceBoundTree)>,
    disagreed: bool,
}

impl InstanceGraph {
    /// Fresh-build waves whose walkers fill the substitution index directly. Sound
    /// only because nothing is installed yet: the index only gains rows and owner
    /// order decides the owned ones. Returns the owners still pending when two
    /// reaches disagreed.
    fn run_fresh_waves(
        &mut self,
        ws: &Workspace,
        pending: Vec<Owner>,
        stats: &mut RunStats,
        walked_with: &mut FxHashMap<Instance, InstanceBoundTree>,
    ) -> Vec<Owner> {
        let interner = self.interner.clone();
        let shards: Vec<SubstShard> = (0..SUBST_SHARDS).map(|_| Mutex::default()).collect();
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
        for i in self.index.trees.keys() {
            if !walked_with.contains_key(i) {
                walked_with.insert(i.clone(), InstanceBoundTree::default());
            }
        }
        if disagreed {
            self.ensure_incremental_indexes();
        }
        wave.into_iter().map(|walk| walk.owner).collect()
    }

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
                    // Roots are already in the first wave.
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
                // Owner order keeps a declaration's instances adjacent.
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

/// Reads nothing else of the graph, so it can run while other owners install.
fn expand(
    ws: &Workspace,
    interner: &MonoInterner,
    scratch: &WalkScratch,
    owner: &Owner,
    inputs: &WalkInputs,
) -> Box<Contribution> {
    let mut out = expand_sites(ws, interner, scratch, owner, inputs);
    let mut seen: FxHashSet<&Instance> = FxHashSet::default();
    let mut keep = Vec::with_capacity(out.reached.len());
    for (to, _) in &out.reached {
        keep.push(seen.insert(to));
    }
    let mut keep = keep.into_iter();
    out.reached
        .retain(|_| keep.next().expect("one flag per reached row"));
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
            // The admissibility oracle both emitters use. A capped instance is a
            // recursion candidate, not yet a verdict.
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
            // Emission uses a boundary's foreign text, so its Rock body is never walked.
            if skull_mir::lower::declared_boundary_class(f).is_some() {
                return out;
            }
            let Some((inf, body)) = ready_body(ws, inst.func) else {
                return out;
            };
            out.expanded = true;
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

#[derive(Clone, Default)]
struct WalkInputs {
    subst: Option<MonoSubst>,
    tree: Option<InstanceBoundTree>,
}

const WALK_TASK_OWNERS: usize = 8;

struct OwnerWalk {
    owner: Owner,
    inputs: WalkInputs,
}

impl OwnerWalk {
    fn run(&self, ws: &Workspace, interner: &MonoInterner) -> Option<Box<Contribution>> {
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
    }
}

impl InstanceGraph {
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
        let mut disagreed = false;
        if self.incremental_indexes {
            self.index.insert_update_rows(&owner, &c);
        } else {
            for (to, subst) in &c.reached {
                if let Some(a) = self.index.subst.get_mut(to) {
                    disagreed |= !a.assert_unowned(subst);
                } else {
                    self.index
                        .subst
                        .insert(to.clone(), Agreed::unowned(subst.clone()));
                }
            }
        }
        self.index_owned_rows(&owner, &c);
        if let Some(from) = owner.instance() {
            for e in &c.edges {
                self.lost.remove(&e.to);
            }
            *self.index.decls.entry(from.func).or_insert(0) += 1;
        }
        for to in &c.always_emitted_targets {
            self.lost.remove(to);
        }
        // The owner was just uninstalled; demanding it would re-walk a recursive body forever.
        let demanded: Vec<Owner> = c
            .reached
            .iter()
            .map(|(to, _)| Owner::Instance(to.clone()))
            .filter(|to| *to != owner && !self.contributions.contains_key(to))
            .collect();
        self.contributions.insert(owner.clone(), c);
        if disagreed {
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

    /// An index entry goes with its last owner, never with a count reaching zero,
    /// so a stale fact cannot survive as an unchecked number.
    fn uninstall(&mut self, owner: &Owner) {
        if !self.contributions.contains_key(owner) {
            return;
        }
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
        self.index.remove_update_rows(owner, &old);
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
            // Withdrawing an always-emitted root can strand its target like a deleted edge.
            self.lost.insert(to.clone());
        }
        if let Some(from) = owner.instance() {
            self.lost.extend(old.edges.iter().map(|e| e.to.clone()));
            if let Some(count) = self.index.decls.get_mut(&from.func) {
                *count -= 1;
                if *count == 0 {
                    self.index.decls.remove(&from.func);
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
        for instance in &touched {
            self.remove_coroutine_rejection(instance);
            if self
                .contributions
                .contains_key(&Owner::Instance(instance.clone()))
            {
                active.push(instance.clone());
            }
        }
        record_coroutine_rejections(self, ws, &active);
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

    fn derive(&mut self, ws: &Workspace) {
        self.lost.clear();
        self.instances = self
            .contributions
            .keys()
            .filter_map(Owner::instance)
            .cloned()
            .collect();
        // Owner order keeps a declaration's instances adjacent, so dedup suffices.
        let mut decls: Vec<DeclKey> = self.instances.iter().map(|i| DeclKey(i.func)).collect();
        decls.dedup();
        let coroutines: Vec<Instance> = self
            .instances
            .iter()
            .filter(|i| ws.module(i.func.module).functions[i.func.func].is_coroutine)
            .cloned()
            .collect();
        self.clear_graph_postpass_rejections();

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
        record_coroutine_rejections(self, ws, &coroutines);
    }

    fn derive_incremental(
        &mut self,
        ws: &Workspace,
        affected: &[Owner],
        changed_decls: &[DeclKey],
    ) {
        let lost: Vec<Instance> = std::mem::take(&mut self.lost).into_iter().collect();
        for instance in self.stranded(&lost) {
            self.uninstall(&Owner::Instance(instance));
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
        self.inline_body_nomangle_roots
            .extend(opaque_boundary_roots_for_decls(
                ws,
                active_decl_vec.iter().map(|decl| decl.0),
            ));
        classify_boundaries(self, ws, &active_decl_vec);
        self.refresh_coroutine_rejections(ws, touched_instances.iter().cloned());

        let mut previous_recursion_starts =
            self.retract_recursion_findings(&touched_instances, &touched_decls);
        // A full build reports one start per declaration, so re-search whole declarations.
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

    /// Update changed bodies and apply the root declarations that changed with them.
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
        let pending_roots: Vec<Owner> = canonical_roots(ws, root_added.to_vec())
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
        self.delta_added.clear();
        self.delta_removed.clear();
        self.shift_contribution_spans(edits);

        // No removal step: `run` replaces each pending owner's contribution.
        let mut affected = self.affected_owners(ws, changed);
        affected.extend(pending_roots);
        affected.sort();
        affected.dedup();
        self.run(ws, affected.clone(), true);
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
        self.derive_incremental(ws, &affected, &changed_decls);
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
            added,
            removed,
            revisited,
            revisited_owners: std::mem::take(&mut self.last_run_owners),
        }
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    fn ensure_incremental_indexes(&mut self) {
        if self.incremental_indexes {
            return;
        }
        self.incremental_indexes = true;
        self.index.subst.clear();
        for (owner, c) in &self.contributions {
            self.index.insert_update_rows(owner, c);
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

    fn affected_owners(&self, ws: &Workspace, changed: &[(ModuleId, BodyOwner)]) -> Vec<Owner> {
        let mut changed_seeds: Vec<Owner> = Vec::new();
        for (module, body_owner) in changed {
            match body_owner {
                // Instances are keyed on bone's representative, as `build` seeds roots.
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
                // A `const` initializer's value is a body; a mutable global's edit arrives
                // as an interface change instead.
                BodyOwner::Global(_) => {
                    changed_seeds.push(Owner::AlwaysEmitted(*module, *body_owner));
                }
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

    fn contribution(&self, i: &Instance) -> Option<&Contribution> {
        self.contributions
            .get(&Owner::Instance(i.clone()))
            .map(Box::as_ref)
    }

    pub fn sites_of(&self, i: &Instance) -> Option<&BTreeMap<Site, SiteRecord>> {
        self.contribution(i)
            .map(|c| &c.call_sites)
            .filter(|m| !m.is_empty())
    }

    pub fn edges(&self) -> impl Iterator<Item = (&Instance, &Edge)> {
        self.contributions
            .iter()
            .filter_map(|(owner, c)| owner.instance().map(|i| (i, c)))
            .flat_map(|(i, c)| c.edges.iter().map(move |edge| (i, edge)))
    }

    pub fn expanded(&self) -> impl Iterator<Item = &Instance> {
        self.contributions
            .iter()
            .filter(|(_, c)| c.expanded)
            .filter_map(|(o, _)| o.instance())
    }

    pub fn call_sites(&self) -> impl Iterator<Item = (&Instance, &BTreeMap<Site, SiteRecord>)> {
        self.contributions
            .iter()
            .filter_map(|(o, c)| o.instance().map(|i| (i, &c.call_sites)))
            .filter(|(_, m)| !m.is_empty())
    }

    /// A root's substitution is empty rather than absent.
    pub fn substitution(&self, i: &Instance) -> Option<&MonoSubst> {
        self.index
            .subst
            .get(i)
            .map(|a| a.decided())
            .or_else(|| self.root_instances.contains(i).then(|| &*EMPTY_SUBST))
    }

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

    pub fn bound_tree(&self, i: &Instance) -> Option<&InstanceBoundTree> {
        self.index.trees.get(i).map(|a| a.decided())
    }

    pub fn has_bound_tree_disagreement(&self, i: &Instance) -> bool {
        self.index
            .trees
            .get(i)
            .is_some_and(|a| !a.dissent.is_empty())
    }

    pub fn bound_tree_dissenters(&self) -> FxHashSet<Instance> {
        self.index
            .trees
            .iter()
            .filter(|(_, a)| !a.dissent.is_empty())
            .map(|(i, _)| i.clone())
            .collect()
    }

    pub fn is_always_emitted_target(&self, i: &Instance) -> bool {
        self.index.always_emitted.contains_key(i)
    }

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

    pub fn rejections(&self) -> impl Iterator<Item = &Rejection> {
        // Type ids follow parallel interning order, so order specializations by logical type.
        let mut ordinary = self.owner_rows(|c| &c.rejections);
        ordinary.sort_by_cached_key(|rejection| self.diagnostic_key(&rejection.at));
        let mut graph: Vec<_> = self.graph_rejections.iter().collect();
        graph.sort_by_cached_key(|rejection| self.diagnostic_key(&rejection.at));
        ordinary.into_iter().chain(graph)
    }

    /// Only the region forward-reachable from `lost` can be stranded.
    fn stranded(&self, lost: &[Instance]) -> BTreeSet<Instance> {
        let succ = |i: &Instance| -> &[Edge] {
            self.contributions
                .get(&Owner::Instance(i.clone()))
                .map_or(&[][..], |c| c.edges.as_slice())
        };
        let mut region: FxHashSet<&Instance> = FxHashSet::default();
        let mut queue: Vec<&Instance> = lost
            .iter()
            .filter(|i| self.instances.contains(*i))
            .collect();
        while let Some(i) = queue.pop() {
            if region.insert(i) {
                queue.extend(succ(i).iter().map(|e| &e.to));
            }
        }
        let anchored = |i: &&Instance| {
            self.root_instances.contains(*i)
                || self.index.always_emitted.contains_key(*i)
                || self
                    .index
                    .preds
                    .get(*i)
                    .is_some_and(|preds| preds.iter().any(|p| !region.contains(p)))
        };
        let mut alive: Vec<&Instance> = region.iter().copied().filter(anchored).collect();
        let mut survives: FxHashSet<&Instance> = FxHashSet::default();
        while let Some(i) = alive.pop() {
            if survives.insert(i) {
                alive.extend(
                    succ(i)
                        .iter()
                        .map(|e| &e.to)
                        .filter(|to| region.contains(to)),
                );
            }
        }
        region
            .into_iter()
            .filter(|i| !survives.contains(i))
            .cloned()
            .collect()
    }
}

#[derive(Default, Debug, Clone, Copy)]
struct RunStats {
    pub expanded: usize,
    pub settle_rounds: usize,
}

#[derive(Default, Debug, Clone)]
pub struct UpdateStats {
    pub added: BTreeSet<Instance>,
    pub removed: BTreeSet<Instance>,
    pub revisited: BTreeSet<Instance>,
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
            // Synthesized MIR may call the preflight helpers, so they need delivery plans.
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
    w.out.reached.push((to.clone(), subst));
    Some(to)
}

fn detect_changing_instance_recursion(g: &InstanceGraph, ws: &Workspace) -> Vec<RecursionFinding> {
    let mut succ: FxHashMap<&Instance, &[Edge]> = FxHashMap::default();
    for (owner, c) in &g.contributions {
        if let Some(i) = owner.instance() {
            if !c.edges.is_empty() {
                succ.insert(i, c.edges.as_slice());
            }
        }
    }

    // A start needs an outgoing edge, but the per-declaration count covers every
    // instance: a declaration's second instance may be a leaf.
    // Exact skip: a declaration with one instance cannot reach a different one.
    let starts = succ.keys().copied().collect();
    search_changing_instance_recursion(g, ws, starts, |node| succ.get(node).copied().unwrap_or(&[]))
}

fn detect_changing_instance_recursion_local(
    g: &InstanceGraph,
    ws: &Workspace,
    seeds: &BTreeSet<Instance>,
) -> Vec<RecursionFinding> {
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
    // Parallel existence check first; the ordered pass is only for stable reporting.
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
                // Only a witness discharges the declaration; another instance's path may
                // still recurse.
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
    // Keyed by canonical encoding rather than interner order.
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
    // A later literal can omit more fields, so rescan a key whose omitted set grew.
    let mut scanned: BTreeMap<Key, usize> = BTreeMap::new();
    while let Some((scan, scan_inf, scan_subst, reached)) = pending.pop() {
        let mut fresh: Vec<Key> = Vec::new();
        for (e, expr) in scan.expr_arena.iter() {
            if reached.as_ref().is_some_and(|r| !r.contains(&e)) {
                continue;
            }
            let E::StructLiteral {
                name,
                fields,
                bases,
            } = expr
            else {
                continue;
            };
            // A `..base` update writes every field the literal does not.
            if !bases.is_empty() {
                continue;
            }
            let Some(sid) = ws.struct_by_name.get(name) else {
                continue;
            };
            // Must key the domain as lowering does, including its shared-body fallback.
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
            // A struct literal lowers only the defaults it omits, so the defaults body's
            // calls are attributed per field.
            let mut sub: FxHashSet<Idx<bone_core::hir::Expr>> = FxHashSet::default();
            for f in fields {
                let Some(root) = defaults.field_defaults.get(f) else {
                    continue;
                };
                sub.extend(expr_subtree(defaults, *root));
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
            reached.extend(expr_subtree(defaults, *root));
        }
        // Defaults resolve under the struct's slot bindings, not the enclosing instance's.
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

fn expr_subtree(
    body: &Body,
    root: Idx<bone_core::hir::Expr>,
) -> FxHashSet<Idx<bone_core::hir::Expr>> {
    let mut out = FxHashSet::from_iter([root]);
    body.walk_descendants(HirChild::Expr(root), &mut |child| match child {
        HirChild::Expr(x) => out.insert(x),
        HirChild::Statement(_) | HirChild::Pattern(_) => true,
    });
    out
}

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
        // Keyed by path: witnesses at different depths can share a parameter index.
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

/// The callee's bindings under the caller's substitution: the transitive
/// instantiation step.
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
                // A slot the signature, bounds and body never mention cannot affect the plan.
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
        // The fallback picked an arbitrary member of a uniform group; canonicalize it.
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
    // Lua interpolates a `nobox value` as leaves and C as one aggregate; record both.
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
fn stdlib_entry_representative(ws: &Workspace, name: &str) -> Option<FuncRef> {
    let decls = ws.functions_by_name.get(&ustr::Ustr::from(name))?;
    let mut reps = decls
        .iter()
        .map(|&(m, f)| skull_mir::lower::canonical_variant(ws, FuncRef { module: m, func: f }));
    let first = reps.next()?;
    reps.all(|r| r == first).then_some(first)
}

/// Representatives of every entry lowering may synthesize a call to.
pub fn synthesized_entry_roots(ws: &Workspace) -> Vec<FuncRef> {
    skull_mir::lower::SYNTHESIZED_ENTRIES
        .iter()
        .filter_map(|name| stdlib_entry_representative(ws, name))
        .collect()
}

/// Whether a declaration with backend `decl` can be emitted for `target`.
/// The walk, the boundary pass and every view ask this one test.
pub fn serves(decl: bone_core::hir::FnBackend, target: bone_core::hir::FnBackend) -> bool {
    use bone_core::hir::FnBackend;
    decl == FnBackend::Any || target == FnBackend::Any || decl == target
}

/// The boundary class of one declaration, or `None` if it has a Rock body.
/// An `@intrinsic` with a Rock body is expanded; an expansion intrinsic, a
/// `@native_required` one and one without a body stay foreign.
fn boundary_class_for(f: &Function) -> Option<BoundaryClass> {
    if is_erased_runtime_entry(f) {
        return Some(BoundaryClass::ErasedRuntimeEntry);
    }
    skull_mir::lower::declared_boundary_class(f)
}

/// Resolve bare `@nomangle` names in JS opaque bodies into root declarations.
/// The body stays opaque to MIR; this is only the publication obligation.
pub fn opaque_boundary_roots_for_decls(
    ws: &Workspace,
    decls: impl IntoIterator<Item = FuncRef>,
) -> BTreeMap<DeclKey, FxHashSet<FuncRef>> {
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

fn classify_boundaries(g: &mut InstanceGraph, ws: &Workspace, decls: &[DeclKey]) {
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

fn record_coroutine_rejections(g: &mut InstanceGraph, ws: &Workspace, added: &[Instance]) {
    for i in added {
        let f = &ws.module(i.func.module).functions[i.func.func];
        if !f.is_coroutine {
            continue;
        }
        if let Err(reason) = crate::concrete_body::concrete_signature_for_instance(ws, g, i)
            .and_then(|signature| {
                skull_mir::lower::CoroutinePlan::from_concrete_signature(
                    ws,
                    g.interner(),
                    &signature,
                    i.symbol(ws, g.interner()),
                )
            })
        {
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
        }
    }
}

pub(crate) fn ready_body(ws: &Workspace, f: FuncRef) -> Option<(&BodyInference, &Body)> {
    let owner = BodyOwner::Function(f.func);
    let inf = ws.inference(f.module)?.per_fn.get(&owner)?;
    let body = ws.module(f.module).bodies.get(&owner)?;
    (inf.body_generation == body.generation).then_some((inf, body))
}
