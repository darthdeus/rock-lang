//! Project the shared instance graph onto one backend. Select declarations,
//! check boundary delivery, and assign emitted symbols to retained instances.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use bone_core::hir::{FnBackend, Function};
use bone_core::workspace::Workspace;
use skull_mir::intrinsics::{self, BackendRealization};
use skull_mir::mir::FuncRef;

use crate::concrete_body::BoundaryId;
use crate::instance_graph::{
    self, BoundaryClass, Concreteness, DeclKey, Instance, InstanceGraph, Site, SiteOutcome,
};
use crate::target_projection::{supports, CpuTargetSpec, TargetSpec};
use rustc_hash::{FxHashMap, FxHashSet};

/// Native mappings are supplied by matching inline bodies or by the backend.
#[derive(Clone, Copy, Default)]
pub struct ArtifactMappings<'a> {
    /// The backend whose in-source body slot IS this artifact's mapping.
    pub inline_backend: Option<FnBackend>,
    /// This artifact's backend realization oracle, when it has one.
    pub supply: Option<&'a dyn BackendRealization>,
}

impl<'a> ArtifactMappings<'a> {
    /// A target that maps nothing natively.
    pub fn none() -> Self {
        Self::default()
    }

    pub fn for_backend(b: FnBackend) -> Self {
        Self {
            inline_backend: Some(b),
            supply: None,
        }
    }

    pub fn with_supply(mut self, s: &'a dyn BackendRealization) -> Self {
        self.supply = Some(s);
        self
    }

    /// Does this artifact's BACKEND supply `f` itself -- with no declared body
    /// emitted for it at all? Asked at the top of realization, ahead of sibling
    /// selection and ahead of every body branch, because that is where the
    /// backend asks it.
    fn supplies(&self, ws: &Workspace, f: &Function) -> bool {
        self.supply.is_some_and(|s| s.supplies(ws, f))
    }

    /// Does this artifact map `f` NATIVELY? A Rock body is not consulted --
    /// that is the portable rung, a different answer to a different question.
    pub fn maps(&self, ws: &Workspace, f: &Function) -> bool {
        if let Some(b) = self.inline_backend {
            if f.inline_body
                .is_some_and(|(be, _)| instance_graph::serves(be, b))
            {
                return true;
            }
        }
        self.supplies(ws, f)
    }
}

/// What an artifact SPELLS an instance.
///
/// Each artifact owns its emitted spelling. The default planner is
/// target-neutral; a renaming test planner checks that this is optional.
pub trait SymbolPlanner: Sync {
    fn symbol(&self, ws: &Workspace, g: &InstanceGraph, i: &Instance, realized: Realized)
        -> String;
    fn describe(&self) -> &'static str;
}

/// The default: today's logical encoder (`spec_fn_name`), which is
/// target-neutral. Injective over the instances it is given, which is exactly
/// what it CANNOT answer for an artifact holding two declarations that encode
/// alike -- hence the per-artifact collision check.
pub(crate) struct LogicalSymbols;

impl SymbolPlanner for LogicalSymbols {
    fn symbol(
        &self,
        ws: &Workspace,
        g: &InstanceGraph,
        i: &Instance,
        _realized: Realized,
    ) -> String {
        g.logical_symbol(ws, i)
    }
    fn describe(&self) -> &'static str {
        "logical"
    }
}

/// Spell an instance through the declaration this artifact selected. This is
/// the production choice for backends whose sibling declarations own different
/// publication conventions (`@nomangle` is the visible case).
pub struct RealizedSymbols;

impl SymbolPlanner for RealizedSymbols {
    fn symbol(
        &self,
        ws: &Workspace,
        g: &InstanceGraph,
        i: &Instance,
        realized: Realized,
    ) -> String {
        Instance {
            func: realized.decl,
            args: i.args.clone(),
        }
        .symbol(ws, g.interner())
    }

    fn describe(&self) -> &'static str {
        "realized-declaration"
    }
}

pub static REALIZED_SYMBOLS: RealizedSymbols = RealizedSymbols;

/// Everything one artifact view is taken through.
pub struct ArtifactRequest<'a> {
    /// The artifact's full target: platform, execution and optional shader.
    /// [`realize`] derives a platform+execution CPU target; shader packaging is
    /// carried and reported but cannot reach admission.
    pub spec: TargetSpec,
    pub mappings: ArtifactMappings<'a>,
    /// The artifact's emission roots, never re-derived from the graph's contents.
    pub roots: &'a [FuncRef],
    pub symbols: &'a dyn SymbolPlanner,
    scalar_context: Option<&'a skull_mir::lower::ScalarCtx>,
}

impl<'a> ArtifactRequest<'a> {
    /// The default request for `spec`: its own backend's mapping, no table, and
    /// the logical spelling.
    pub fn new(spec: TargetSpec, roots: &'a [FuncRef]) -> Self {
        Self {
            mappings: match spec.execution.backend {
                Some(b) => ArtifactMappings::for_backend(b),
                None => ArtifactMappings::none(),
            },
            spec,
            roots,
            symbols: &LogicalSymbols,
            scalar_context: None,
        }
    }

    pub fn with_scalar_context(mut self, context: &'a skull_mir::lower::ScalarCtx) -> Self {
        self.scalar_context = Some(context);
        self
    }

    fn scalar_context<'b>(
        &'b self,
        ws: &Workspace,
        g: &InstanceGraph,
        local: &'b std::cell::OnceCell<skull_mir::lower::ScalarCtx>,
    ) -> Option<&'b skull_mir::lower::ScalarCtx> {
        let backend = self.spec.execution.backend?;
        if let Some(context) = self.scalar_context {
            assert_eq!(
                context.target(),
                backend,
                "artifact scalar context backend mismatch"
            );
            assert!(
                std::ptr::eq(context.interner(), g.interner().as_ref()),
                "artifact scalar context must share the graph interner"
            );
            return Some(context);
        }
        // Standalone graph consumers have no emission context to retain.
        Some(local.get_or_init(|| {
            skull_mir::lower::ScalarCtx::new(ws)
                .with_target(backend)
                .sharing(g.interner().clone())
        }))
    }

    pub fn with_mappings(mut self, m: ArtifactMappings<'a>) -> Self {
        self.mappings = m;
        self
    }

    pub fn with_symbols(mut self, p: &'a dyn SymbolPlanner) -> Self {
        self.symbols = p;
        self
    }
}

/// How this artifact supplies a retained declaration: the same `@intrinsic`
/// declaration is [`Self::NativeMapping`] on one artifact and
/// [`Self::PortableFallback`] on another.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Realization {
    /// A Rock body this artifact lowers.
    RockBody,
    /// A foreign body this artifact emits verbatim.
    ForeignBody(BoundaryClass),
    /// An `@intrinsic` this artifact maps natively.
    NativeMapping,
    /// An `@intrinsic` with no mapping here, served by its portable Rock body.
    PortableFallback,
    /// A declaration with no `@intrinsic` marker that this artifact's backend
    /// supplies itself, with no declared body emitted at all -- the JS
    /// backend's `phantom`, `error` and `map_new` renderings. The same oracle
    /// answers for [`Self::NativeMapping`]; what separates them is whether the
    /// DECLARATION carries the marker.
    HostBinding,
    /// An `@intrinsic` whose emission is compile-time on every backend
    /// (`@struct_expand` and friends). It has no callable body to map or fall
    /// back to, so neither question applies.
    CompileTimeExpansion,
}

impl Realization {
    /// The boundary class this realization is, when it is one. A `Rock`
    /// callee has none; a foreign or erased one does, and its class is part of
    /// the [`BoundaryId`] a dependency must terminate in.
    pub fn boundary_class(self) -> Option<BoundaryClass> {
        match self {
            Self::ForeignBody(c) => Some(c),
            // A hand-written backend binding is class 1.3 by content -- a
            // mapping that lives in Rust, per backend, rather than in the
            // source -- so it names the same contract an `@intrinsic` mapping
            // does.
            Self::NativeMapping | Self::CompileTimeExpansion | Self::HostBinding => {
                Some(BoundaryClass::Intrinsic)
            }
            Self::RockBody | Self::PortableFallback => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Realized {
    pub decl: FuncRef,
    pub kind: Realization,
}

/// Why a declaration in the shared graph is not realizable on this artifact.
/// Each keeps its cause, because
/// "unavailable" collapses three different diagnostics into one word.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Unrealizable {
    /// The declaration's backend does not serve this artifact. The
    /// `@nomangle`-sibling case: a JS body in a Lua walk.
    ForeignToArtifact,
    /// The artifact's capabilities do not supply this boundary CLASS
    /// (`extern` with no foreign symbols, a verbatim body with no inline
    /// backend).
    ClassUnsupported(BoundaryClass),
    /// `@native_required`, and this artifact has no native mapping for it.
    /// A Rock fallback does not answer: the marker's whole content is that the
    /// portable rung is skipped.
    NativeMappingMissing,
}

/// Why an instance the shared graph holds is not in this artifact. The two are
/// different findings: an unrealizable declaration is a REJECTION the artifact
/// would have to report if anything called it, while an unreachable one is
/// simply not this artifact's program.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Exclusion {
    Unrealizable(Unrealizable),
    /// Realizable here, but no root of this artifact reaches it.
    Unreachable,
}

/// An instance the shared graph holds and this artifact does not.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Excluded {
    pub instance: Instance,
    pub cause: Exclusion,
}

/// A call from a RETAINED instance whose concrete callee this artifact cannot
/// realize. The diagnostic a declaration filter structurally cannot produce:
/// the caller survives, so emission would emit a call to a body that is not in
/// the artifact.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Severed {
    pub caller: Instance,
    pub site: Site,
    pub callee: Instance,
    pub cause: Unrealizable,
}

/// A call target with its backend-specific boundary identity.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RealizedCallee {
    Rock(Instance),
    Boundary(BoundaryId),
}

/// One opaque-body dependency, terminating in a concrete callee.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct InlineDep {
    /// The opaque body that names it.
    pub owner: FuncRef,
    pub entry: &'static str,
    /// [`RealizedCallee::Boundary`] for a runtime entry, [`RealizedCallee::Rock`]
    /// for a declaration this artifact lowers. Never a bare `FuncRef`: that
    /// loses the boundary class and cannot name WHICH instance when a
    /// declaration has more than one.
    pub callee: RealizedCallee,
}

/// An opaque-body dependency this artifact cannot resolve to one callee. A
/// located rejection: the owner is retained and its text calls a name nothing
/// here supplies (or supplies twice over), so emission would produce a call to
/// a symbol that does not exist or cannot be chosen.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct UnresolvedInlineDep {
    pub owner: FuncRef,
    pub entry: &'static str,
    pub reason: &'static str,
}

/// Two retained instances that spell alike.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SymbolCollision {
    pub symbol: String,
    pub instances: Vec<Instance>,
}

/// Two PUBLICATION obligations (see [`ArtifactView::publications`]) claiming
/// one physical symbol. Rejected rather than resolved by last-definition-wins,
/// which is what a renderer that did not model this distinctly would silently
/// do to two `@nomangle` declarations sharing a bare name.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PublicationCollision {
    pub symbol: String,
    pub instances: Vec<Instance>,
}

/// One target's realization of a graph it did not build.
#[derive(Clone)]
pub struct ArtifactView {
    pub spec: TargetSpec,
    /// The artifact's root instances -- realizable emission roots, plus the
    /// callees of always-emitted bodies, which have no incoming edge.
    pub roots: BTreeSet<Instance>,
    /// Realizable request roots. Their definitions are required even when
    /// every call is inlined.
    pub declared_roots: BTreeSet<Instance>,
    /// Function values need addressable definitions even with no direct calls.
    pub fn_value_targets: BTreeSet<Instance>,
    /// Retained ROCK instances: reachable from `roots`, realizable here, and
    /// lowered from a Rock body. A SUBSET of `g.instances`.
    ///
    /// A boundary has no body the artifact lowers, so enqueueing it as an
    /// instance would ask for callees it does not have.
    pub instances: BTreeSet<Instance>,
    /// Boundary identities keyed by shared instance, including generic arguments.
    pub boundaries: BTreeMap<Instance, BoundaryId>,
    /// How each retained instance is supplied here, and by which declaration.
    pub realization: BTreeMap<Instance, Realized>,
    pub excluded: Vec<Excluded>,
    pub severed: Vec<Severed>,
    pub sites_walked: usize,
    /// One symbol per retained instance. Total and injective WITHIN this
    /// artifact; nothing here claims another artifact spells it the same.
    pub symbols: BTreeMap<Instance, String>,
    pub symbol_collisions: Vec<SymbolCollision>,
    pub inline_deps: Vec<InlineDep>,
    pub unresolved_inline_deps: Vec<UnresolvedInlineDep>,
    /// Bare-name roots recorded by the shared graph for retained `inline(js)`
    /// declarations. These are opaque-boundary obligations, not MIR delivery
    /// edges; JS adds the targets to its next graph/view construction.
    pub opaque_boundary_roots: BTreeMap<DeclKey, FxHashSet<FuncRef>>,
    /// Foreign result widths derived from each concrete instance ABI.
    pub publisher_lanes: BTreeMap<Instance, usize>,
    /// Retained instances whose substitution is missing from the shared graph.
    pub missing_substitutions: BTreeSet<Instance>,
    /// The shared population this view was taken over -- the denominator.
    pub shared_instances: usize,
    pub deferred: Vec<&'static str>,
    /// Required `@nomangle` definitions keyed by the exact declaration. Calls can
    /// still resolve via a sibling in `realization`.
    pub publications: BTreeMap<Instance, String>,
    /// Publication obligations with no exact expansion in the shared graph.
    pub unrepresented_publications: Vec<FuncRef>,
    pub publication_collisions: Vec<PublicationCollision>,
    index: ArtifactIndex,
}

#[derive(Clone, Default)]
struct ArtifactIndex {
    realizable: FxHashMap<Instance, Result<Realized, Unrealizable>>,
    /// The graph revision the view's decisions were taken over.
    graph_revision: u64,
    /// Derived before the first edit, while the graph still matches this view.
    update_rows: bool,
    outgoing: FxHashMap<Instance, FxHashMap<Site, Instance>>,
    incoming: FxHashMap<Instance, FxHashSet<Instance>>,
    site_counts: FxHashMap<Instance, usize>,
    /// Realizable instances an always-emitted body takes as a value. They have
    /// no caller instance whose sites record them.
    always_fn_values: FxHashSet<Instance>,
    severed_by_caller: FxHashMap<Instance, Vec<Severed>>,
    exclusions: FxHashMap<Instance, Exclusion>,
    symbol_users: FxHashMap<String, FxHashSet<Instance>>,
    realization_by_decl: FxHashMap<FuncRef, FxHashSet<Instance>>,
    instances_by_decl: FxHashMap<FuncRef, FxHashSet<Instance>>,
    inline_consumers: BTreeMap<DeclKey, BTreeSet<DeclKey>>,
    inline_dependencies_by_owner: BTreeMap<DeclKey, BTreeSet<DeclKey>>,
    publication_candidates: BTreeMap<Instance, String>,
    publication_users: BTreeMap<String, BTreeSet<Instance>>,
    root_decls: BTreeSet<DeclKey>,
}

#[derive(Default)]
struct ArtifactSiteRows<'g> {
    site_count: usize,
    outgoing: FxHashMap<Site, Instance>,
    callees: FxHashSet<&'g Instance>,
}

impl<'g> ArtifactSiteRows<'g> {
    fn new(sites: &'g BTreeMap<Site, instance_graph::SiteRecord>) -> Self {
        let mut outgoing = FxHashMap::default();
        let mut callees = FxHashSet::default();
        for (site, record) in sites {
            let SiteOutcome::Resolved(callee) = &record.outcome else {
                continue;
            };
            outgoing.insert(site.clone(), callee.clone());
            callees.insert(callee);
        }
        Self {
            site_count: sites.len(),
            outgoing,
            callees,
        }
    }
}

impl ArtifactIndex {
    fn record_inline_dependency(&mut self, owner: DeclKey, dependency: DeclKey) {
        self.inline_consumers
            .entry(dependency)
            .or_default()
            .insert(owner);
        self.inline_dependencies_by_owner
            .entry(owner)
            .or_default()
            .insert(dependency);
    }

    // The owner's old rows must be retracted before installing its replacement.
    fn install_site_rows(&mut self, owner: &Instance, rows: ArtifactSiteRows<'_>) {
        self.site_counts.insert(owner.clone(), rows.site_count);
        if !rows.outgoing.is_empty() {
            self.outgoing.insert(owner.clone(), rows.outgoing);
        }
        for callee in rows.callees {
            self.incoming
                .entry(callee.clone())
                .or_default()
                .insert(owner.clone());
        }
    }
}

fn inline_callee<'a>(
    mut instances: impl Iterator<Item = &'a Instance>,
    boundaries: &BTreeMap<Instance, BoundaryId>,
) -> Result<RealizedCallee, &'static str> {
    let Some(instance) = instances.next() else {
        return Err("the serving declaration is not retained by the artifact");
    };
    if instances.next().is_some() {
        return Err("the serving declaration has more than one retained instance");
    }
    Ok(match boundaries.get(instance) {
        Some(id) => RealizedCallee::Boundary(id.clone()),
        None => RealizedCallee::Rock(instance.clone()),
    })
}

fn severed_order(left: &Severed, right: &Severed) -> std::cmp::Ordering {
    left.caller
        .cmp(&right.caller)
        .then_with(|| left.site.cmp(&right.site))
        .then_with(|| left.callee.cmp(&right.callee))
        .then_with(|| left.cause.cmp(&right.cause))
}

fn func_ref_order(left: &FuncRef, right: &FuncRef) -> std::cmp::Ordering {
    left.module
        .index()
        .cmp(&right.module.index())
        .then_with(|| u32::from(left.func.into_raw()).cmp(&u32::from(right.func.into_raw())))
}

fn inline_dep_order(left: &InlineDep, right: &InlineDep) -> std::cmp::Ordering {
    func_ref_order(&left.owner, &right.owner).then_with(|| left.entry.cmp(right.entry))
}

fn unresolved_inline_dep_order(
    left: &UnresolvedInlineDep,
    right: &UnresolvedInlineDep,
) -> std::cmp::Ordering {
    func_ref_order(&left.owner, &right.owner).then_with(|| left.entry.cmp(right.entry))
}

/// Whether `i` is one of the request's own roots. A `@shader` stage entry is
/// registered as GLSL text, never named from Lua, so it is retained only when
/// ordinary code reaches it.
fn declared_root(ws: &Workspace, root_decls: &BTreeSet<DeclKey>, i: &Instance) -> bool {
    root_decls.contains(&DeclKey(i.func))
        && !skull_mir::lower::is_shader_entry(&ws.module(i.func.module).functions[i.func.func])
}

fn publication_candidate(
    ws: &Workspace,
    target: &CpuTargetSpec,
    declaration: FuncRef,
) -> Option<(Instance, String)> {
    let f = &ws.module(declaration.module).functions[declaration.func];
    (f.annotations
        .iter()
        .any(|a| a.name.name.as_str() == "nomangle")
        && instance_graph::serves(
            f.backend,
            target.execution.backend.unwrap_or(FnBackend::Any),
        ))
    .then(|| {
        (
            Instance::base(declaration),
            skull_mir::mangle::mangle_fn(ws, f),
        )
    })
}

fn publication_candidates(ws: &Workspace, req: &ArtifactRequest<'_>) -> BTreeMap<Instance, String> {
    let target = req.spec.cpu();
    req.roots
        .iter()
        .filter_map(|&root| publication_candidate(ws, &target, root))
        .collect()
}

impl ArtifactView {
    /// Derive the update-only rows from `g`, the graph this view was realized
    /// from, unless an earlier update already did. A caller about to update
    /// the graph calls this first: the rows describe the graph BEFORE the
    /// update, which is what the update retracts.
    pub fn prepare_update(&mut self, g: &InstanceGraph) {
        if self.index.update_rows {
            return;
        }
        assert_eq!(
            g.revision(),
            self.index.graph_revision,
            "an artifact view's update rows are derived from the graph it was realized \
             from; call `prepare_update` before updating the graph"
        );
        self.index.update_rows = true;
        // Each owner's row is its own; the rows are built in parallel and the
        // reverse rows, which join owners, are filled from them in owner order.
        // A callee is listed once per owner.
        let owner_rows: Vec<(&Instance, ArtifactSiteRows<'_>)> = {
            use bone_core::prelude::*;
            let owners: Vec<_> = g.call_sites().collect();
            owners
                .into_par_iter()
                .map(|(owner, sites)| (owner, ArtifactSiteRows::new(sites)))
                .collect()
        };
        let index = &mut self.index;
        for (owner, rows) in owner_rows {
            index.install_site_rows(owner, rows);
        }
        for (i, symbol) in &self.symbols {
            index
                .symbol_users
                .entry(symbol.clone())
                .or_default()
                .insert(i.clone());
        }
        for (i, r) in &self.realization {
            index
                .realization_by_decl
                .entry(r.decl)
                .or_default()
                .insert(i.clone());
            index
                .instances_by_decl
                .entry(i.func)
                .or_default()
                .insert(i.clone());
        }
    }

    /// Apply graph deltas to this target without re-realizing the workspace.
    /// The retained view owns the indexes needed to retract an old owner and
    /// install its replacement; unchanged owners keep their immutable output.
    pub fn update_local(
        &mut self,
        ws: &Workspace,
        g: &InstanceGraph,
        req: &ArtifactRequest<'_>,
        revisited: &BTreeSet<Instance>,
        added: &BTreeSet<Instance>,
        removed: &BTreeSet<Instance>,
    ) {
        let old_roots = self.index.root_decls.clone();
        let new_roots: BTreeSet<DeclKey> = req
            .roots
            .iter()
            .map(|r| DeclKey(skull_mir::lower::canonical_variant(ws, *r)))
            .collect();
        let root_added: BTreeSet<DeclKey> = new_roots.difference(&old_roots).cloned().collect();
        let root_removed: BTreeSet<DeclKey> = old_roots.difference(&new_roots).cloned().collect();

        self.update_local_with_decl_delta(
            ws,
            g,
            req,
            revisited,
            added,
            removed,
            &root_added,
            &root_removed,
        )
    }

    /// Apply a graph delta and an explicit root delta. Production callers keep
    /// the requested root set in their own cache and pass only changed roots;
    /// [`Self::update_local`] remains the full-request oracle for callers that
    /// do not retain that state.
    pub fn update_local_with_root_delta(
        &mut self,
        ws: &Workspace,
        g: &InstanceGraph,
        req: &ArtifactRequest<'_>,
        revisited: &BTreeSet<Instance>,
        added: &BTreeSet<Instance>,
        removed: &BTreeSet<Instance>,
        root_added: &[FuncRef],
        root_removed: &[FuncRef],
    ) {
        let root_added: BTreeSet<DeclKey> = root_added
            .iter()
            .map(|r| DeclKey(skull_mir::lower::canonical_variant(ws, *r)))
            .collect();
        let root_removed: BTreeSet<DeclKey> = root_removed
            .iter()
            .map(|r| DeclKey(skull_mir::lower::canonical_variant(ws, *r)))
            .collect();
        self.update_local_with_decl_delta(
            ws,
            g,
            req,
            revisited,
            added,
            removed,
            &root_added,
            &root_removed,
        )
    }

    fn update_local_with_decl_delta(
        &mut self,
        ws: &Workspace,
        g: &InstanceGraph,
        req: &ArtifactRequest<'_>,
        revisited: &BTreeSet<Instance>,
        added: &BTreeSet<Instance>,
        removed: &BTreeSet<Instance>,
        root_added: &BTreeSet<DeclKey>,
        root_removed: &BTreeSet<DeclKey>,
    ) {
        // A caller that did not update the graph may skip `prepare_update`.
        self.prepare_update(g);
        self.index.graph_revision = g.revision();
        let mut root_subjects = BTreeSet::new();
        for decl in root_added.iter().chain(root_removed) {
            root_subjects.extend(
                self.index
                    .instances_by_decl
                    .get(&decl.0)
                    .into_iter()
                    .flat_map(|instances| instances.iter().cloned()),
            );
            let instances: Vec<_> = g.instances_of(decl.0).cloned().collect();
            root_subjects.extend(instances);
        }
        let mut subjects = revisited.clone();
        subjects.extend(added.iter().cloned());
        subjects.extend(removed.iter().cloned());
        subjects.extend(root_subjects.iter().cloned());
        if subjects.is_empty() && root_added.is_empty() && root_removed.is_empty() {
            self.shared_instances = g.instances.len();
            return;
        }

        let old_root_subjects: BTreeSet<Instance> = subjects
            .iter()
            .filter(|instance| self.roots.contains(*instance))
            .cloned()
            .collect();
        let old_realizable: FxHashMap<Instance, Result<Realized, Unrealizable>> = subjects
            .iter()
            .filter_map(|i| self.index.realizable.get(i).map(|r| (i.clone(), *r)))
            .collect();
        let old_outgoing: FxHashMap<Instance, FxHashMap<Site, Instance>> = subjects
            .iter()
            .filter_map(|i| {
                self.index
                    .outgoing
                    .get(i)
                    .map(|edges| (i.clone(), edges.clone()))
            })
            .collect();
        let old_predecessors: FxHashMap<Instance, FxHashSet<Instance>> = subjects
            .iter()
            .filter_map(|i| {
                self.index
                    .incoming
                    .get(i)
                    .map(|preds| (i.clone(), preds.clone()))
            })
            .collect();
        let old_symbols: BTreeMap<Instance, String> = subjects
            .iter()
            .filter_map(|i| {
                self.symbols
                    .get(i)
                    .map(|symbol| (i.clone(), symbol.clone()))
            })
            .collect();

        // Remove only the outgoing rows owned by the changed subjects. The
        // old rows remain in `old_outgoing` for the local dirty-region walk.
        for i in &subjects {
            if let Some(edges) = self.index.outgoing.remove(i) {
                for callee in edges.values() {
                    if let Some(preds) = self.index.incoming.get_mut(callee) {
                        preds.remove(i);
                        if preds.is_empty() {
                            self.index.incoming.remove(callee);
                        }
                    }
                }
            }
            self.index.severed_by_caller.remove(i);
        }

        let target = req.spec.cpu();
        for i in &subjects {
            if !g.instances.contains(i) {
                self.index.realizable.remove(i);
                continue;
            }
            self.index
                .realizable
                .insert(i.clone(), realize_decl(ws, i.func, &target, &req.mappings));
            let rows = g.sites_of(i).map(ArtifactSiteRows::new).unwrap_or_default();
            self.index.install_site_rows(i, rows);
        }

        for decl in root_removed {
            self.index.root_decls.remove(decl);
        }
        for decl in root_added {
            self.index.root_decls.insert(*decl);
        }
        for i in &subjects {
            self.roots.remove(i);
            self.declared_roots.remove(i);
        }
        let mut always_fn_values_changed = false;
        for i in &subjects {
            let was = self.index.always_fn_values.remove(i);
            let now = g.instances.contains(i)
                && g.is_always_emitted_fn_value(i)
                && self.index.realizable[i].is_ok();
            if now {
                self.index.always_fn_values.insert(i.clone());
            }
            always_fn_values_changed |= was != now;
        }
        for i in subjects.iter().filter(|i| g.instances.contains(*i)) {
            let declared = declared_root(ws, &self.index.root_decls, i);
            let structural = g.is_always_emitted_target(i);
            if (declared || structural) && self.index.realizable[i].is_ok() {
                self.roots.insert(i.clone());
                if declared {
                    self.declared_roots.insert(i.clone());
                }
            }
        }
        let root_delta_changed = !root_added.is_empty() || !root_removed.is_empty();

        let edge_changed = subjects
            .iter()
            .any(|i| old_outgoing.get(i) != self.index.outgoing.get(i));
        let realization_changed = subjects
            .iter()
            .any(|i| old_realizable.get(i) != self.index.realizable.get(i));
        let roots_changed = subjects
            .iter()
            .any(|i| old_root_subjects.contains(i) != self.roots.contains(i));
        if !edge_changed
            && !realization_changed
            && !roots_changed
            && !root_delta_changed
            && !always_fn_values_changed
            && added.is_empty()
            && removed.is_empty()
        {
            self.shared_instances = g.instances.len();
            return;
        }

        // Only the connected region around a changed owner can change
        // reachability. This uses the retained view's predecessor/successor
        // indexes; no graph-wide candidate scan is needed.
        let mut dirty = subjects.clone();
        let mut queue: VecDeque<Instance> = subjects.iter().cloned().collect();
        while let Some(i) = queue.pop_front() {
            let mut neighbours = Vec::new();
            if let Some(edges) = old_outgoing.get(&i) {
                neighbours.extend(edges.values().cloned());
            }
            if let Some(edges) = self.index.outgoing.get(&i) {
                neighbours.extend(edges.values().cloned());
            }
            if let Some(preds) = self.index.incoming.get(&i) {
                neighbours.extend(preds.iter().cloned());
            }
            if let Some(preds) = old_predecessors.get(&i) {
                neighbours.extend(preds.iter().cloned());
            }
            for neighbour in neighbours {
                if dirty.insert(neighbour.clone()) {
                    queue.push_back(neighbour);
                }
            }
        }

        let mut reachable_dirty = BTreeSet::new();
        let mut reach_queue: VecDeque<Instance> = self
            .roots
            .iter()
            .filter(|i| dirty.contains(*i))
            .cloned()
            .collect();
        while let Some(i) = reach_queue.pop_front() {
            if !dirty.contains(&i) || reachable_dirty.contains(&i) {
                continue;
            }
            let Some(Ok(realized)) = self.index.realizable.get(&i) else {
                continue;
            };
            reachable_dirty.insert(i.clone());
            if realized.kind.boundary_class().is_some() {
                continue;
            }
            if let Some(edges) = self.index.outgoing.get(&i) {
                for callee in edges.values() {
                    if dirty.contains(callee) {
                        reach_queue.push_back(callee.clone());
                    }
                }
            }
        }
        for i in &dirty {
            self.index.severed_by_caller.remove(i);
            self.remove_instance_output(i, self.index.site_counts.get(i).copied().unwrap_or(0));
            self.index.exclusions.remove(i);
        }
        for i in removed {
            self.index.site_counts.remove(i);
        }
        let scalar_context = std::cell::OnceCell::new();
        for i in dirty.iter().filter(|i| g.instances.contains(*i)) {
            let Some(result) = self.index.realizable.get(i).copied() else {
                continue;
            };
            if reachable_dirty.contains(i) {
                let realized = result.expect("retained artifact instance must be realizable");
                self.install_instance_output(ws, g, req, i, realized, &scalar_context);
                let declared = declared_root(ws, &self.index.root_decls, i);
                if declared || g.is_always_emitted_target(i) {
                    self.roots.insert(i.clone());
                    if declared {
                        self.declared_roots.insert(i.clone());
                    }
                }
                if realized.kind.boundary_class().is_none() {
                    let mut severed = Vec::new();
                    if let Some(sites) = self.index.outgoing.get(i) {
                        for (site, callee) in sites {
                            if let Some(Err(cause)) = self.index.realizable.get(callee) {
                                severed.push(Severed {
                                    caller: i.clone(),
                                    site: site.clone(),
                                    callee: callee.clone(),
                                    cause: *cause,
                                });
                            }
                        }
                    }
                    self.index.severed_by_caller.insert(i.clone(), severed);
                }
            } else {
                let cause = match result {
                    Ok(_) => Exclusion::Unreachable,
                    Err(reason) => Exclusion::Unrealizable(reason),
                };
                self.index.exclusions.insert(i.clone(), cause);
            }
        }

        let mut changed_decls = BTreeSet::new();
        for i in &dirty {
            changed_decls.insert(DeclKey(i.func));
            if let Some(Ok(realized)) = old_realizable.get(i) {
                changed_decls.insert(DeclKey(realized.decl));
            }
            if let Some(Ok(realized)) = self.index.realizable.get(i) {
                changed_decls.insert(DeclKey(realized.decl));
            }
        }
        self.refresh_inline_local(ws, g, &changed_decls);
        self.refresh_opaque_roots(g, &changed_decls);
        let mut changed_publication_symbols: BTreeSet<String> = dirty
            .iter()
            .filter_map(|instance| self.index.publication_candidates.get(instance).cloned())
            .collect();
        let publication_symbols =
            self.refresh_publication_candidates(ws, req, root_added, root_removed);
        changed_publication_symbols.extend(publication_symbols);
        self.refresh_publications_local(g, &changed_publication_symbols);
        self.refresh_excluded_local(&dirty);
        self.refresh_severed_local(&dirty);
        let mut changed_symbols: BTreeSet<String> = old_symbols.into_values().collect();
        changed_symbols.extend(dirty.iter().filter_map(|i| self.symbols.get(i).cloned()));
        self.refresh_symbol_collisions_local(&changed_symbols);
        self.shared_instances = g.instances.len();
    }

    fn remove_instance_output(&mut self, i: &Instance, old_site_count: usize) {
        self.roots.remove(i);
        self.declared_roots.remove(i);
        if let Some(symbol) = self.symbols.remove(i) {
            if let Some(users) = self.index.symbol_users.get_mut(&symbol) {
                users.remove(i);
                if users.is_empty() {
                    self.index.symbol_users.remove(&symbol);
                }
            }
        }
        if let Some(realized) = self.realization.remove(i) {
            let decl = realized.decl;
            let remove_decl = self
                .index
                .realization_by_decl
                .get_mut(&decl)
                .is_some_and(|users| {
                    users.remove(i);
                    users.is_empty()
                });
            if remove_decl {
                self.index.realization_by_decl.remove(&decl);
            }
            let instance_decl = i.func;
            let remove_instance_decl = self
                .index
                .instances_by_decl
                .get_mut(&instance_decl)
                .is_some_and(|users| {
                    users.remove(i);
                    users.is_empty()
                });
            if remove_instance_decl {
                self.index.instances_by_decl.remove(&instance_decl);
            }
            self.boundaries.remove(i);
            if realized.kind.boundary_class().is_none() {
                self.instances.remove(i);
                self.sites_walked = self.sites_walked.saturating_sub(old_site_count);
            }
        }
        self.publisher_lanes.remove(i);
        self.missing_substitutions.remove(i);
        // The dirty region includes every caller of this target; their output
        // is installed again only after all dirty targets have been retracted.
        self.fn_value_targets.remove(i);
    }

    fn install_instance_output(
        &mut self,
        ws: &Workspace,
        g: &InstanceGraph,
        req: &ArtifactRequest<'_>,
        i: &Instance,
        realized: Realized,
        scalar_context: &std::cell::OnceCell<skull_mir::lower::ScalarCtx>,
    ) {
        let symbol = req.symbols.symbol(ws, g, i, realized);
        self.symbols.insert(i.clone(), symbol.clone());
        self.index
            .symbol_users
            .entry(symbol.clone())
            .or_default()
            .insert(i.clone());
        self.realization.insert(i.clone(), realized);
        self.index
            .realization_by_decl
            .entry(realized.decl)
            .or_default()
            .insert(i.clone());
        self.index
            .instances_by_decl
            .entry(i.func)
            .or_default()
            .insert(i.clone());
        if realized.kind.boundary_class().is_none() {
            self.instances.insert(i.clone());
            self.sites_walked += self.index.site_counts.get(i).copied().unwrap_or(0);
        }
        if let Some(class) = realized.kind.boundary_class() {
            self.boundaries.insert(
                i.clone(),
                BoundaryId {
                    declaration: realized.decl,
                    class,
                    target: req.spec.cpu(),
                    symbol,
                },
            );
        }
        if let Some(edges) = self.index.outgoing.get(i) {
            for (site, callee) in edges {
                if site.op == skull_mir::site::SiteOp::FnValue {
                    self.fn_value_targets.insert(callee.clone());
                }
            }
        }
        if self.index.always_fn_values.contains(i) {
            self.fn_value_targets.insert(i.clone());
        }
        if g.concreteness(i) == Concreteness::Missing {
            self.missing_substitutions.insert(i.clone());
        }
        if matches!(
            realized.kind,
            Realization::ForeignBody(BoundaryClass::InlineBody)
        ) {
            if let Some(scx) = req.scalar_context(ws, g, scalar_context) {
                if let Ok(signature) =
                    crate::concrete_body::concrete_signature_for_instance(ws, g, i)
                {
                    let selected = Instance {
                        func: realized.decl,
                        args: i.args.clone(),
                    };
                    self.publisher_lanes.insert(
                        i.clone(),
                        skull_mir::lower::BackendFnAbiPlan::for_concrete_signature(
                            ws, scx, &selected, &signature,
                        )
                        .result_lanes(),
                    );
                }
            }
        }
    }

    fn refresh_inline_local(
        &mut self,
        ws: &Workspace,
        g: &InstanceGraph,
        changed_decls: &BTreeSet<DeclKey>,
    ) {
        let mut owners = BTreeSet::new();
        for decl in changed_decls {
            if let Some(consumers) = self.index.inline_consumers.get(decl) {
                owners.extend(consumers.iter().cloned());
            }
            if g.inline_body_deps.contains_key(decl) {
                owners.insert(*decl);
            }
        }
        if owners.is_empty() {
            return;
        }
        for owner in &owners {
            self.remove_inline_rows(owner.0);
            if let Some(dependencies) = self.index.inline_dependencies_by_owner.remove(owner) {
                for dependency in dependencies {
                    if let Some(consumers) = self.index.inline_consumers.get_mut(&dependency) {
                        consumers.remove(owner);
                        if consumers.is_empty() {
                            self.index.inline_consumers.remove(&dependency);
                        }
                    }
                }
            }
            let Some(entries) = g.inline_body_deps.get(owner) else {
                continue;
            };
            if !self.index.realization_by_decl.contains_key(&owner.0)
                && !self.index.instances_by_decl.contains_key(&owner.0)
            {
                continue;
            }
            let mut inline = Vec::new();
            let mut unresolved = Vec::new();
            for entry in entries {
                let callee = match instance_graph::stdlib_entry_representative(ws, entry) {
                    Some(rep) => {
                        self.index.record_inline_dependency(*owner, DeclKey(rep));
                        inline_callee(
                            self.index.instances_by_decl.get(&rep).into_iter().flatten(),
                            &self.boundaries,
                        )
                    }
                    None => Err("no single declaration supplies this entry"),
                };
                match callee {
                    Ok(callee) => inline.push(InlineDep {
                        owner: owner.0,
                        entry,
                        callee,
                    }),
                    Err(reason) => unresolved.push(UnresolvedInlineDep {
                        owner: owner.0,
                        entry,
                        reason,
                    }),
                }
            }
            for dep in inline {
                let position = self
                    .inline_deps
                    .binary_search_by(|current| inline_dep_order(current, &dep))
                    .unwrap_or_else(|position| position);
                self.inline_deps.insert(position, dep);
            }
            for dep in unresolved {
                let position = self
                    .unresolved_inline_deps
                    .binary_search_by(|current| unresolved_inline_dep_order(current, &dep))
                    .unwrap_or_else(|position| position);
                self.unresolved_inline_deps.insert(position, dep);
            }
        }
    }

    fn remove_inline_rows(&mut self, owner: FuncRef) {
        while let Ok(position) = self
            .inline_deps
            .binary_search_by(|current| func_ref_order(&current.owner, &owner))
        {
            self.inline_deps.remove(position);
        }
        while let Ok(position) = self
            .unresolved_inline_deps
            .binary_search_by(|current| func_ref_order(&current.owner, &owner))
        {
            self.unresolved_inline_deps.remove(position);
        }
    }

    fn refresh_opaque_roots(&mut self, g: &InstanceGraph, changed_decls: &BTreeSet<DeclKey>) {
        for owner in changed_decls {
            self.opaque_boundary_roots.remove(owner);
            if self.index.realization_by_decl.contains_key(&owner.0)
                || self.index.instances_by_decl.contains_key(&owner.0)
            {
                if let Some(targets) = g.inline_body_nomangle_roots.get(owner) {
                    self.opaque_boundary_roots.insert(*owner, targets.clone());
                }
            }
        }
    }

    fn refresh_publication_candidates(
        &mut self,
        ws: &Workspace,
        req: &ArtifactRequest<'_>,
        root_added: &BTreeSet<DeclKey>,
        root_removed: &BTreeSet<DeclKey>,
    ) -> BTreeSet<String> {
        let mut changed = BTreeSet::new();
        for decl in root_removed {
            let instance = Instance::base(decl.0);
            let Some(symbol) = self.index.publication_candidates.remove(&instance) else {
                continue;
            };
            changed.insert(symbol.clone());
            if let Some(users) = self.index.publication_users.get_mut(&symbol) {
                users.remove(&instance);
                if users.is_empty() {
                    self.index.publication_users.remove(&symbol);
                }
            }
            self.publications.remove(&instance);
            if let Ok(position) = self
                .unrepresented_publications
                .binary_search_by(|current| func_ref_order(current, &instance.func))
            {
                self.unrepresented_publications.remove(position);
            }
        }
        let target = req.spec.cpu();
        for decl in root_added {
            let Some((instance, symbol)) = publication_candidate(ws, &target, decl.0) else {
                continue;
            };
            if self
                .index
                .publication_candidates
                .insert(instance.clone(), symbol.clone())
                .is_some()
            {
                continue;
            }
            self.index
                .publication_users
                .entry(symbol.clone())
                .or_default()
                .insert(instance);
            changed.insert(symbol);
        }
        changed
    }

    fn refresh_publications_local(
        &mut self,
        g: &InstanceGraph,
        changed_symbols: &BTreeSet<String>,
    ) {
        for symbol in changed_symbols {
            let users = self
                .index
                .publication_users
                .get(symbol)
                .cloned()
                .unwrap_or_default();
            for instance in &users {
                self.publications.remove(instance);
                let fref = instance.func;
                if let Ok(position) = self
                    .unrepresented_publications
                    .binary_search_by(|current| func_ref_order(current, &fref))
                {
                    self.unrepresented_publications.remove(position);
                }
            }
            if let Ok(position) = self
                .publication_collisions
                .binary_search_by(|row| row.symbol.as_str().cmp(symbol.as_str()))
            {
                self.publication_collisions.remove(position);
            }
            for instance in &users {
                if !g.instances.contains(instance) {
                    let fref = instance.func;
                    let position = self
                        .unrepresented_publications
                        .binary_search_by(|current| func_ref_order(current, &fref))
                        .unwrap_or_else(|position| position);
                    self.unrepresented_publications.insert(position, fref);
                }
            }
            let represented: Vec<Instance> = users
                .iter()
                .filter(|instance| g.instances.contains(*instance))
                .cloned()
                .collect();
            if represented.len() > 1 {
                let row = PublicationCollision {
                    symbol: symbol.clone(),
                    instances: represented,
                };
                let position = self
                    .publication_collisions
                    .binary_search_by(|current| current.symbol.cmp(symbol))
                    .unwrap_or_else(|position| position);
                self.publication_collisions.insert(position, row);
            } else if let Some(instance) = represented.into_iter().next() {
                self.publications.insert(instance, symbol.clone());
            }
        }
    }

    fn refresh_excluded_local(&mut self, changed: &BTreeSet<Instance>) {
        for instance in changed {
            if let Ok(position) = self
                .excluded
                .binary_search_by(|row| row.instance.cmp(instance))
            {
                self.excluded.remove(position);
            }
            if let Some(&cause) = self.index.exclusions.get(instance) {
                let row = Excluded {
                    instance: instance.clone(),
                    cause,
                };
                let position = self
                    .excluded
                    .binary_search_by(|current| current.instance.cmp(instance))
                    .unwrap_or_else(|position| position);
                self.excluded.insert(position, row);
            }
        }
    }

    fn refresh_severed_local(&mut self, changed_callers: &BTreeSet<Instance>) {
        for caller in changed_callers {
            while let Ok(position) = self.severed.binary_search_by(|row| row.caller.cmp(caller)) {
                self.severed.remove(position);
            }
            if let Some(rows) = self.index.severed_by_caller.get(caller) {
                for row in rows {
                    let position = self
                        .severed
                        .binary_search_by(|current| severed_order(current, row))
                        .unwrap_or_else(|position| position);
                    self.severed.insert(position, row.clone());
                }
            }
        }
    }

    fn refresh_symbol_collisions_local(&mut self, changed_symbols: &BTreeSet<String>) {
        for symbol in changed_symbols {
            if let Ok(position) = self
                .symbol_collisions
                .binary_search_by(|row| row.symbol.cmp(symbol))
            {
                self.symbol_collisions.remove(position);
            }
            let Some(users) = self.index.symbol_users.get(symbol) else {
                continue;
            };
            if users.len() < 2 {
                continue;
            }
            let mut instances: Vec<Instance> = users.iter().cloned().collect();
            instances.sort();
            let row = SymbolCollision {
                symbol: symbol.clone(),
                instances,
            };
            let position = self
                .symbol_collisions
                .binary_search_by(|current| current.symbol.cmp(symbol))
                .unwrap_or_else(|position| position);
            self.symbol_collisions.insert(position, row);
        }
    }

    /// Compare every realized output except the target description itself.
    pub fn same_cpu_decisions(&self, other: &Self) -> bool {
        self.roots == other.roots
            && self.declared_roots == other.declared_roots
            && self.fn_value_targets == other.fn_value_targets
            && self.instances == other.instances
            && self.boundaries == other.boundaries
            && self.realization == other.realization
            && self.excluded == other.excluded
            && self.severed == other.severed
            && self.sites_walked == other.sites_walked
            && self.symbols == other.symbols
            && self.symbol_collisions == other.symbol_collisions
            && self.inline_deps == other.inline_deps
            && self.unresolved_inline_deps == other.unresolved_inline_deps
            && self.opaque_boundary_roots == other.opaque_boundary_roots
            && self.publisher_lanes == other.publisher_lanes
            && self.missing_substitutions == other.missing_substitutions
            && self.shared_instances == other.shared_instances
            && self.deferred == other.deferred
            && self.publications == other.publications
            && self.unrepresented_publications == other.unrepresented_publications
            && self.publication_collisions == other.publication_collisions
    }

    /// Every retained instance -- Rock bodies AND boundaries. The positive side
    /// of the partition, and what `symbols` is total over: a boundary still
    /// needs a spelling, it just has no body to traverse.
    pub fn retained(&self) -> impl Iterator<Item = &Instance> {
        self.realization.keys()
    }

    /// Whether this artifact retains `i`, however it is supplied.
    pub fn retains(&self, i: &Instance) -> bool {
        self.realization.contains_key(i)
    }
}

/// The emitter's precedence is native mapping, portable fallback, then rejection.
/// `@native_required` disables the fallback.
fn realize_decl(
    ws: &Workspace,
    rep: FuncRef,
    target: &CpuTargetSpec,
    mappings: &ArtifactMappings<'_>,
) -> Result<Realized, Unrealizable> {
    // Expansions and backend mappings must win before sibling selection.
    // They can serve a call even without a source body for this backend.
    let rep_f = &ws.module(rep.module).functions[rep.func];
    if intrinsics::expand_kind(rep_f).is_some() && supports(target, BoundaryClass::Intrinsic) {
        return Ok(Realized {
            decl: rep,
            kind: Realization::CompileTimeExpansion,
        });
    }
    if supports(target, BoundaryClass::Intrinsic) && mappings.supplies(ws, rep_f) {
        // ONE rung, ONE oracle. `@intrinsic` or not only decides which LABEL
        // the census carries -- both are the same backend-supplied contract
        // (`Realization::boundary_class`), and asking the two questions
        // separately is what let `map_new` reach a body branch.
        let kind = if intrinsics::is_intrinsic(rep_f) {
            Realization::NativeMapping
        } else {
            Realization::HostBinding
        };
        return Ok(Realized { decl: rep, kind });
    }

    let selected: Result<FuncRef, Unrealizable> = match target.execution.backend {
        Some(b) => {
            let name = ws.module(rep.module).functions[rep.func].name;
            skull_mir::lower::backend_variant(ws, name, rep, b)
                .map_err(|_| Unrealizable::ForeignToArtifact)
        }
        // A target without an execution backend can only serve portable declarations.
        None => {
            if ws.module(rep.module).functions[rep.func].backend != FnBackend::Any {
                Err(Unrealizable::ForeignToArtifact)
            } else {
                Ok(rep)
            }
        }
    };
    selected.and_then(|decl| {
        let f = &ws.module(decl.module).functions[decl.func];
        realize_body(ws, decl, f, target, mappings).map(|kind| Realized { decl, kind })
    })
}

/// How the SELECTED declaration is supplied. Split from the selection above so
/// the two questions -- which declaration, and what kind of body it has -- cannot
/// be answered against different `FuncRef`s.
fn realize_body(
    ws: &Workspace,
    decl: FuncRef,
    f: &Function,
    target: &CpuTargetSpec,
    mappings: &ArtifactMappings<'_>,
) -> Result<Realization, Unrealizable> {
    // An erased runtime entry is an IDENTITY, not a body kind: S 1.4's contract
    // is that the entry exists under its bare name for every target skull
    // emits, so it is classified before anything reads a body slot.
    if crate::instance_graph::is_erased_runtime_entry(f) {
        return class_or_reject(target, BoundaryClass::ErasedRuntimeEntry);
    }
    if f.is_extern {
        // `extern(js)` names the browser/runtime surface, not a host C symbol;
        // the JS artifact owns that boundary even though the web platform does
        // not expose the desktop foreign-symbol capability.
        if f.backend == FnBackend::Js && target.execution.backend == Some(FnBackend::Js) {
            return Ok(Realization::ForeignBody(BoundaryClass::Extern));
        }
        return class_or_reject(target, BoundaryClass::Extern);
    }
    if intrinsics::is_intrinsic(f) {
        if !supports(target, BoundaryClass::Intrinsic) {
            return Err(Unrealizable::ClassUnsupported(BoundaryClass::Intrinsic));
        }
        // An expansion intrinsic has no callable body on ANY backend -- its
        // emission is compile-time at MIR lowering -- so neither the mapping
        // nor the fallback question applies to it.
        if intrinsics::expand_kind(f).is_some() {
            return Ok(Realization::CompileTimeExpansion);
        }
        if mappings.maps(ws, f) {
            return Ok(Realization::NativeMapping);
        }
        // THE discriminant. Asked of the declaration, before the fallback rung,
        // and never satisfied by the presence of a Rock body.
        if intrinsics::is_native_required(f) {
            return Err(Unrealizable::NativeMappingMissing);
        }
        if has_rock_body(ws, decl) {
            return Ok(Realization::PortableFallback);
        }
        return Err(Unrealizable::NativeMappingMissing);
    }
    if f.inline_body.is_some() {
        return class_or_reject(target, BoundaryClass::InlineBody);
    }
    Ok(Realization::RockBody)
}

fn class_or_reject(
    target: &CpuTargetSpec,
    class: BoundaryClass,
) -> Result<Realization, Unrealizable> {
    if supports(target, class) {
        Ok(Realization::ForeignBody(class))
    } else {
        Err(Unrealizable::ClassUnsupported(class))
    }
}

/// Whether the module actually HOLDS a Rock body for `decl`.
///
/// `instance_graph::has_rock_body` reads the declaration only -- "no foreign
/// body" -- which answers TRUE for an expansion intrinsic that has no body at
/// all. The portable rung needs the body itself.
fn has_rock_body(ws: &Workspace, decl: FuncRef) -> bool {
    ws.module(decl.module)
        .bodies
        .contains_key(&bone_core::hir::BodyOwner::Function(decl.func))
}

/// Realize existing graph instances for the artifact request.
pub fn realize(ws: &Workspace, g: &InstanceGraph, req: &ArtifactRequest<'_>) -> ArtifactView {
    // Shader packaging is absent from this value by construction. Platform
    // capabilities remain available for host boundary admission.
    let target = req.spec.cpu();
    // Realizability of every instance the graph holds, computed once. It is a
    // property of the DECLARATION and the artifact, independent of reachability.
    let mut realizable: FxHashMap<Instance, Result<Realized, Unrealizable>> = FxHashMap::default();
    // A declaration's instances all realize the same way, so answer each
    // declaration once; a generic-heavy program has several instances per decl.
    let mut by_decl: FxHashMap<FuncRef, Result<Realized, Unrealizable>> = FxHashMap::default();
    for i in &g.instances {
        let realized = *by_decl
            .entry(i.func)
            .or_insert_with(|| realize_decl(ws, i.func, &target, &req.mappings));
        realizable.insert(i.clone(), realized);
    }

    // Canonicalize request roots to the representatives used as instance keys.
    // Always-emitted callees are roots because they have no caller instance.
    let root_decls: BTreeSet<DeclKey> = req
        .roots
        .iter()
        .map(|r| DeclKey(skull_mir::lower::canonical_variant(ws, *r)))
        .collect();
    let mut index = ArtifactIndex {
        realizable,
        graph_revision: g.revision(),
        root_decls: root_decls.clone(),
        ..Default::default()
    };
    let mut roots: BTreeSet<Instance> = BTreeSet::new();
    let mut declared_roots: BTreeSet<Instance> = BTreeSet::new();
    for i in &g.instances {
        let declared = declared_root(ws, &root_decls, i);
        let is_root = declared || g.is_always_emitted_target(i);
        if is_root && index.realizable[i].is_ok() {
            roots.insert(i.clone());
            if declared {
                declared_roots.insert(i.clone());
            }
        }
    }

    // Reachability over the walk's OWN record of what each site resolved to.
    // Not over `g.edges`: an edge says two instances are related, a site says
    // WHICH operation related them, which is what a severed call has to name.
    // The site tables are read in place; the reverse rows an update needs
    // are derived by that update.
    let site_tables: FxHashMap<&Instance, &BTreeMap<Site, instance_graph::SiteRecord>> =
        g.call_sites().collect();
    let mut severed: Vec<Severed> = Vec::new();
    let mut sites_walked = 0usize;

    // Walk outgoing sites only for retained Rock bodies.
    struct Admission<'g> {
        admitted: FxHashSet<&'g Instance>,
        reached: Vec<(&'g Instance, Realized)>,
        queue: VecDeque<&'g Instance>,
    }
    impl<'g> Admission<'g> {
        fn admit(
            &mut self,
            i: &'g Instance,
            realizable: &FxHashMap<Instance, Result<Realized, Unrealizable>>,
        ) {
            if !self.admitted.insert(i) {
                return;
            }
            let r = *realizable[i].as_ref().expect("admitted means realizable");
            self.reached.push((i, r));
            if r.kind.boundary_class().is_none() {
                self.queue.push_back(i);
            }
        }
    }
    let mut walk = Admission {
        admitted: FxHashSet::default(),
        reached: Vec::new(),
        queue: VecDeque::new(),
    };
    let root_refs: Vec<&Instance> = g.instances.iter().filter(|i| roots.contains(*i)).collect();
    for r in root_refs {
        walk.admit(r, &index.realizable);
    }
    // A function named by a global initializer or a decorator body is a value
    // take with no caller instance, so the site walk below cannot see it.
    index.always_fn_values = roots
        .iter()
        .filter(|r| g.is_always_emitted_fn_value(r))
        .cloned()
        .collect();
    let mut fn_value_targets: BTreeSet<Instance> = index.always_fn_values.iter().cloned().collect();
    while let Some(caller) = walk.queue.pop_front() {
        let Some(sites) = site_tables.get(caller) else {
            continue;
        };
        sites_walked += sites.len();
        for (site, record) in sites.iter() {
            let SiteOutcome::Resolved(callee) = &record.outcome else {
                continue;
            };
            match index.realizable.get(callee) {
                Some(Ok(_)) => {
                    if site.op == skull_mir::site::SiteOp::FnValue {
                        fn_value_targets.insert(callee.clone());
                    }
                    walk.admit(callee, &index.realizable)
                }
                Some(Err(cause)) => {
                    let row = Severed {
                        caller: caller.clone(),
                        site: site.clone(),
                        callee: callee.clone(),
                        cause: *cause,
                    };
                    index
                        .severed_by_caller
                        .entry(caller.clone())
                        .or_default()
                        .push(row.clone());
                    severed.push(row);
                }
                // Missing graph instances are reported by concrete plan construction.
                None => {}
            }
        }
    }
    severed.sort_by(severed_order);
    for rows in index.severed_by_caller.values_mut() {
        rows.sort_by(severed_order);
    }
    let mut reached = walk.reached;
    reached.sort_unstable_by(|a, b| a.0.cmp(b.0));
    let realization: BTreeMap<Instance, Realized> =
        reached.iter().map(|(i, r)| ((*i).clone(), *r)).collect();
    let instances: BTreeSet<Instance> = reached
        .iter()
        .filter(|(_, r)| r.kind.boundary_class().is_none())
        .map(|(i, _)| (*i).clone())
        .collect();
    drop(site_tables);

    // The partition: every shared instance is a retained Rock instance, a
    // retained boundary, unrealizable, or unreachable. Reported as a partition
    // so a lost instance cannot hide.
    let excluded: Vec<Excluded> = g
        .instances
        .iter()
        .filter(|i| !realization.contains_key(*i))
        .map(|i| Excluded {
            instance: i.clone(),
            cause: match index.realizable[i] {
                Ok(_) => Exclusion::Unreachable,
                Err(u) => Exclusion::Unrealizable(u),
            },
        })
        .collect();
    for e in &excluded {
        index.exclusions.insert(e.instance.clone(), e.cause);
    }

    // One symbol per retained instance, from THIS artifact's planner.
    // The planner answers each instance independently; the names are
    // derived in parallel and recorded in instance order.
    let planned: Vec<(&Instance, String)> = {
        use bone_core::prelude::*;
        let rows: Vec<(&Instance, Realized)> = realization.iter().map(|(i, r)| (i, *r)).collect();
        rows.into_par_iter()
            .map(|(i, r)| (i, req.symbols.symbol(ws, g, i, r)))
            .collect()
    };
    let mut by_symbol: FxHashMap<&str, Vec<&Instance>> = FxHashMap::default();
    for (i, s) in &planned {
        by_symbol.entry(s.as_str()).or_default().push(i);
    }
    // Reported in symbol order, which the hashed grouping does not keep.
    let mut symbol_collisions: Vec<SymbolCollision> = by_symbol
        .into_iter()
        .filter(|(_, v)| v.len() > 1)
        .map(|(symbol, instances)| SymbolCollision {
            symbol: symbol.to_string(),
            instances: instances.into_iter().cloned().collect(),
        })
        .collect();
    symbol_collisions.sort_by(|a, b| a.symbol.cmp(&b.symbol));
    let symbols: BTreeMap<Instance, String> =
        planned.into_iter().map(|(i, s)| (i.clone(), s)).collect();

    // THE boundary identities, minted once, per retained instance. Every
    // consumer -- an ordinary call site and an opaque dependency alike -- reads
    // this map rather than constructing an id of its own, so an artifact cannot
    // name one boundary two ways. The symbol is the PLANNER's, which is what
    // makes a renaming planner visible here instead of silently ignored.
    let boundaries: BTreeMap<Instance, BoundaryId> = realization
        .iter()
        .filter_map(|(i, r)| {
            r.kind.boundary_class().map(|class| {
                (
                    i.clone(),
                    BoundaryId {
                        declaration: r.decl,
                        class,
                        target,
                        symbol: symbols[i].clone(),
                    },
                )
            })
        })
        .collect();

    // Opaque dependencies, terminating in a concrete callee of THIS artifact.
    let mut inline_deps = Vec::new();
    let mut unresolved_inline_deps = Vec::new();
    let retained_decls: BTreeSet<DeclKey> = realization.values().map(|r| DeclKey(r.decl)).collect();
    // The retained instances of the declarations an inline body depends on,
    // the only declarations this realization asks that question of.
    let dependency_decls: FxHashSet<FuncRef> = g
        .inline_body_deps
        .values()
        .flatten()
        .filter_map(|entry| instance_graph::stdlib_entry_representative(ws, entry))
        .collect();
    let mut instances_by_decl: FxHashMap<FuncRef, Vec<&Instance>> = FxHashMap::default();
    for i in realization.keys() {
        if dependency_decls.contains(&i.func) {
            instances_by_decl.entry(i.func).or_default().push(i);
        }
    }
    for (owner, entries) in &g.inline_body_deps {
        let owner_retained = retained_decls.contains(owner);
        for entry in entries {
            let representative = instance_graph::stdlib_entry_representative(ws, entry);
            if let Some(rep) = representative {
                index.record_inline_dependency(*owner, DeclKey(rep));
            }
            if !owner_retained {
                continue;
            }
            let callee = match representative {
                Some(rep) => inline_callee(
                    instances_by_decl.get(&rep).into_iter().flatten().copied(),
                    &boundaries,
                ),
                None => Err("no single declaration supplies this entry"),
            };
            match callee {
                Ok(callee) => inline_deps.push(InlineDep {
                    owner: owner.0,
                    entry,
                    callee,
                }),
                Err(reason) => unresolved_inline_deps.push(UnresolvedInlineDep {
                    owner: owner.0,
                    entry,
                    reason,
                }),
            }
        }
    }
    inline_deps.sort_by(inline_dep_order);
    unresolved_inline_deps.sort_by(unresolved_inline_dep_order);

    // The result-lane width of every foreign body this artifact emits. Asked of
    // the SELECTED declaration -- the one whose body is emitted -- under this
    // artifact's own execution backend.
    let scalar_context = std::cell::OnceCell::new();
    let scx = req.scalar_context(ws, g, &scalar_context);
    let mut publisher_lanes: BTreeMap<Instance, usize> = BTreeMap::new();
    if let Some(scx) = &scx {
        for (i, r) in &realization {
            if !matches!(r.kind, Realization::ForeignBody(BoundaryClass::InlineBody)) {
                continue;
            }
            // Use the instance arguments positionally for the selected sibling's ABI.
            if g.substitution(i).is_none() {
                continue;
            }
            let selected = Instance {
                func: r.decl,
                args: i.args.clone(),
            };
            debug_assert_eq!(
                ws.module(selected.func.module).functions[selected.func.func]
                    .generics
                    .len(),
                ws.module(i.func.module).functions[i.func.func]
                    .generics
                    .len(),
                "a backend sibling must declare the same generics as the instance it serves",
            );
            let signature = crate::concrete_body::concrete_signature_for_instance(ws, g, i)
                .unwrap_or_else(|reason| {
                    panic!(
                        "retained inline body {:?} has no concrete publisher signature: {reason:?}",
                        i
                    )
                });
            publisher_lanes.insert(
                i.clone(),
                skull_mir::lower::BackendFnAbiPlan::for_concrete_signature(
                    ws, scx, &selected, &signature,
                )
                .result_lanes(),
            );
        }
    }

    // The invariant finding, over EVERY retained instance rather than only the
    // foreign-body slice: a producer that bypassed `reach`'s recorder is a
    // graph defect wherever it lands, and the lane question is just the first
    // consumer that would have read the hole as "concrete".
    let missing_substitutions: BTreeSet<Instance> = realization
        .keys()
        .filter(|i| g.concreteness(i) == Concreteness::Missing)
        .cloned()
        .collect();

    // The exact declaration must have a graph expansion to publish its bare name.
    let publication_candidates = publication_candidates(ws, req);
    for (instance, symbol) in &publication_candidates {
        index
            .publication_users
            .entry(symbol.clone())
            .or_default()
            .insert(instance.clone());
        index
            .publication_candidates
            .insert(instance.clone(), symbol.clone());
    }
    let mut by_pub_symbol: BTreeMap<String, Vec<Instance>> = BTreeMap::new();
    let mut unrepresented_publications: Vec<FuncRef> = Vec::new();
    for (inst, symbol) in &publication_candidates {
        if !g.instances.contains(inst) {
            unrepresented_publications.push(inst.func);
            continue;
        }
        by_pub_symbol
            .entry(symbol.clone())
            .or_default()
            .push(inst.clone());
    }
    let mut publications: BTreeMap<Instance, String> = BTreeMap::new();
    let mut publication_collisions: Vec<PublicationCollision> = Vec::new();
    for (symbol, mut instances) in by_pub_symbol {
        instances.sort();
        instances.dedup();
        if instances.len() > 1 {
            publication_collisions.push(PublicationCollision { symbol, instances });
        } else {
            publications.insert(
                instances.into_iter().next().expect("non-empty group"),
                symbol,
            );
        }
    }
    unrepresented_publications.sort_by(func_ref_order);

    let opaque_boundary_roots = g
        .inline_body_nomangle_roots
        .iter()
        .filter(|(owner, _)| retained_decls.contains(owner))
        .map(|(owner, targets)| (*owner, targets.clone()))
        .collect();

    ArtifactView {
        shared_instances: g.instances.len(),
        publisher_lanes,
        missing_substitutions,
        publications,
        unrepresented_publications,
        publication_collisions,
        // An execution-axis question too: every obligation it names is about
        // memory, coroutines, widths or foreign symbols. A shader-derived
        // deferral would be a separate list on a separate axis.
        deferred: crate::target_projection::deferred_for(&target.execution),
        spec: req.spec,
        roots,
        declared_roots,
        fn_value_targets,
        instances,
        boundaries,
        realization,
        excluded,
        severed,
        sites_walked,
        symbols,
        symbol_collisions,
        inline_deps,
        unresolved_inline_deps,
        opaque_boundary_roots,
        index,
    }
}
