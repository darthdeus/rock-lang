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
#[derive(Clone, Copy)]
pub struct ArtifactMappings<'a> {
    pub inline_backend: FnBackend,
    pub supply: Option<&'a dyn BackendRealization>,
}

impl<'a> ArtifactMappings<'a> {
    pub fn for_backend(b: FnBackend) -> Self {
        Self {
            inline_backend: b,
            supply: None,
        }
    }

    pub fn with_supply(mut self, s: &'a dyn BackendRealization) -> Self {
        self.supply = Some(s);
        self
    }

    fn supplies(&self, ws: &Workspace, f: &Function) -> bool {
        self.supply.is_some_and(|s| s.supplies(ws, f))
    }

    /// Native mapping only; a Rock body is the portable rung, a separate question.
    pub fn maps(&self, ws: &Workspace, f: &Function) -> bool {
        f.inline_body
            .is_some_and(|(be, _)| instance_graph::serves(be, self.inline_backend))
            || self.supplies(ws, f)
    }
}

#[derive(Clone, Copy)]
pub enum SymbolPlanner {
    Logical,
    Realized,
}

impl SymbolPlanner {
    fn symbol(
        &self,
        ws: &Workspace,
        graph: &InstanceGraph,
        instance: &Instance,
        realized: Realized,
    ) -> String {
        match self {
            Self::Logical => instance.symbol(ws, graph.interner()),
            Self::Realized => Instance {
                func: realized.decl,
                args: instance.args.clone(),
            }
            .symbol(ws, graph.interner()),
        }
    }
}

pub static REALIZED_SYMBOLS: SymbolPlanner = SymbolPlanner::Realized;

pub struct ArtifactRequest<'a> {
    pub spec: TargetSpec,
    pub mappings: ArtifactMappings<'a>,
    pub roots: &'a [FuncRef],
    pub symbols: &'a SymbolPlanner,
    scalar_context: Option<&'a skull_mir::lower::ScalarCtx>,
}

impl<'a> ArtifactRequest<'a> {
    pub fn new(spec: TargetSpec, roots: &'a [FuncRef]) -> Self {
        Self {
            mappings: ArtifactMappings::for_backend(spec.cpu.backend),
            spec,
            roots,
            symbols: &SymbolPlanner::Logical,
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
    ) -> &'b skull_mir::lower::ScalarCtx {
        let backend = self.spec.cpu.backend;
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
            return context;
        }
        local.get_or_init(|| {
            skull_mir::lower::ScalarCtx::new(ws)
                .with_target(backend)
                .sharing(g.interner().clone())
        })
    }

    pub fn with_mappings(mut self, m: ArtifactMappings<'a>) -> Self {
        self.mappings = m;
        self
    }

    pub fn with_symbols(mut self, p: &'a SymbolPlanner) -> Self {
        self.symbols = p;
        self
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Realization {
    RockBody,
    ForeignBody(BoundaryClass),
    NativeMapping,
    PortableFallback,
    /// Supplied by the backend itself for a declaration without `@intrinsic`.
    HostBinding,
    CompileTimeExpansion,
}

impl Realization {
    pub fn boundary_class(self) -> Option<BoundaryClass> {
        match self {
            Self::ForeignBody(c) => Some(c),
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

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Unrealizable {
    /// The `@nomangle`-sibling case: a JS body in a Lua walk.
    ForeignToArtifact,
    ClassUnsupported(BoundaryClass),
    /// `@native_required`: a Rock fallback does not answer.
    NativeMappingMissing,
}

#[derive(Clone)]
pub struct ArtifactView {
    pub spec: TargetSpec,
    /// Realizable emission roots, plus callees of always-emitted bodies.
    pub roots: BTreeSet<Instance>,
    /// Realizable request roots. Their definitions are required even when
    /// every call is inlined.
    pub declared_roots: BTreeSet<Instance>,
    /// Function values need addressable definitions even with no direct calls.
    pub fn_value_targets: BTreeSet<Instance>,
    /// Retained instances lowered from a Rock body; boundaries are excluded.
    pub instances: BTreeSet<Instance>,
    pub boundaries: BTreeMap<Instance, BoundaryId>,
    pub realization: BTreeMap<Instance, Realized>,
    pub symbols: BTreeMap<Instance, String>,
    /// Bare-name roots of retained `inline(js)` declarations; not MIR delivery edges.
    pub opaque_boundary_roots: BTreeMap<DeclKey, FxHashSet<FuncRef>>,
    pub publisher_lanes: BTreeMap<Instance, usize>,
    pub missing_substitutions: BTreeSet<Instance>,
    /// Required `@nomangle` definitions keyed by the exact declaration. Calls can
    /// still resolve via a sibling in `realization`.
    pub publications: BTreeMap<Instance, String>,
    index: ArtifactIndex,
}

#[derive(Clone, Default)]
struct ArtifactIndex {
    realizable: FxHashMap<Instance, Result<Realized, Unrealizable>>,
    graph_revision: u64,
    update_rows: bool,
    outgoing: FxHashMap<Instance, FxHashMap<Site, Instance>>,
    incoming: FxHashMap<Instance, FxHashSet<Instance>>,
    always_fn_values: FxHashSet<Instance>,
    realization_by_decl: FxHashMap<FuncRef, FxHashSet<Instance>>,
    instances_by_decl: FxHashMap<FuncRef, FxHashSet<Instance>>,
    publication_candidates: BTreeMap<Instance, String>,
    publication_users: BTreeMap<String, BTreeSet<Instance>>,
    root_decls: BTreeSet<DeclKey>,
}

#[derive(Default)]
struct ArtifactSiteRows<'g> {
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
        Self { outgoing, callees }
    }
}

impl ArtifactIndex {
    // The owner's old rows must be retracted before installing its replacement.
    fn install_site_rows(&mut self, owner: &Instance, rows: ArtifactSiteRows<'_>) {
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

    fn add_publication_candidate(&mut self, instance: Instance, symbol: String) -> bool {
        if self
            .publication_candidates
            .insert(instance.clone(), symbol.clone())
            .is_some()
        {
            return false;
        }
        self.publication_users
            .entry(symbol)
            .or_default()
            .insert(instance);
        true
    }
}

/// A `@shader` stage entry is registered as GLSL text, so it is never a root.
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
        && instance_graph::serves(f.backend, target.backend))
    .then(|| {
        (
            Instance::base(declaration),
            skull_mir::mangle::mangle_fn(ws, f),
        )
    })
}

fn unique_publication<'a>(
    g: &InstanceGraph,
    candidates: &'a BTreeSet<Instance>,
) -> Option<&'a Instance> {
    let mut represented = candidates.iter().filter(|i| g.instances.contains(*i));
    let instance = represented.next()?;
    represented.next().is_none().then_some(instance)
}

impl ArtifactView {
    /// Call before updating the graph: the rows must describe the graph this
    /// view was realized from.
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
        // Rows are built in parallel; the reverse rows join owners in owner order.
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
        let canonical = |roots: &[FuncRef]| -> BTreeSet<DeclKey> {
            roots
                .iter()
                .map(|r| DeclKey(skull_mir::lower::canonical_variant(ws, *r)))
                .collect()
        };
        let root_added = &canonical(root_added);
        let root_removed = &canonical(root_removed);
        // A caller that did not update the graph may skip `prepare_update`.
        self.prepare_update(g);
        let mut subjects = revisited.clone();
        subjects.extend(added.iter().cloned());
        subjects.extend(removed.iter().cloned());
        for decl in root_added.iter().chain(root_removed) {
            subjects.extend(
                self.index
                    .instances_by_decl
                    .get(&decl.0)
                    .into_iter()
                    .flat_map(|instances| instances.iter().cloned()),
            );
            subjects.extend(g.instances_of(decl.0).cloned());
        }
        if subjects.is_empty() && root_added.is_empty() && root_removed.is_empty() {
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
        let old_predecessors: FxHashMap<Instance, FxHashSet<Instance>> = subjects
            .iter()
            .filter_map(|i| {
                self.index
                    .incoming
                    .get(i)
                    .map(|preds| (i.clone(), preds.clone()))
            })
            .collect();
        let mut old_outgoing = FxHashMap::default();
        for i in &subjects {
            // The removed rows stay in `old_outgoing` for the dirty-region walk.
            if let Some(edges) = self.index.outgoing.remove(i) {
                for callee in edges.values() {
                    if let Some(preds) = self.index.incoming.get_mut(callee) {
                        preds.remove(i);
                        if preds.is_empty() {
                            self.index.incoming.remove(callee);
                        }
                    }
                }
                old_outgoing.insert(i.clone(), edges);
            }
        }

        let target = req.spec.cpu;
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
            return;
        }

        let mut dirty = subjects.clone();
        let mut queue: VecDeque<Instance> = subjects.iter().cloned().collect();
        while let Some(i) = queue.pop_front() {
            let successors = [old_outgoing.get(&i), self.index.outgoing.get(&i)]
                .into_iter()
                .flatten()
                .flat_map(FxHashMap::values);
            let predecessors = [self.index.incoming.get(&i), old_predecessors.get(&i)]
                .into_iter()
                .flatten()
                .flatten();
            let neighbours = successors.chain(predecessors);
            for neighbour in neighbours {
                if dirty.insert(neighbour.clone()) {
                    queue.push_back(neighbour.clone());
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
            self.remove_instance_output(i);
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
        self.refresh_opaque_roots(g, &changed_decls);
        let mut changed_publication_symbols: BTreeSet<String> = dirty
            .iter()
            .filter_map(|instance| self.index.publication_candidates.get(instance).cloned())
            .collect();
        let publication_symbols =
            self.refresh_publication_candidates(ws, req, root_added, root_removed);
        changed_publication_symbols.extend(publication_symbols);
        self.refresh_publications_local(g, &changed_publication_symbols);
    }

    fn remove_instance_output(&mut self, i: &Instance) {
        self.roots.remove(i);
        self.declared_roots.remove(i);
        self.symbols.remove(i);
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
            }
        }
        self.publisher_lanes.remove(i);
        self.missing_substitutions.remove(i);
        // Dirty callers are installed again only after every dirty target is retracted.
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
        match realized.kind.boundary_class() {
            None => {
                self.instances.insert(i.clone());
            }
            Some(class) => {
                self.boundaries.insert(
                    i.clone(),
                    BoundaryId {
                        declaration: realized.decl,
                        class,
                        target: req.spec.cpu,
                        symbol,
                    },
                );
            }
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
            let scx = req.scalar_context(ws, g, scalar_context);
            if let Ok(signature) = crate::concrete_body::concrete_signature_for_instance(ws, g, i) {
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
        }
        let target = req.spec.cpu;
        for decl in root_added {
            let Some((instance, symbol)) = publication_candidate(ws, &target, decl.0) else {
                continue;
            };
            if self
                .index
                .add_publication_candidate(instance, symbol.clone())
            {
                changed.insert(symbol);
            }
        }
        changed
    }

    fn refresh_publications_local(
        &mut self,
        g: &InstanceGraph,
        changed_symbols: &BTreeSet<String>,
    ) {
        for symbol in changed_symbols {
            let Some(users) = self.index.publication_users.get(symbol) else {
                continue;
            };
            for instance in users {
                self.publications.remove(instance);
            }
            if let Some(instance) = unique_publication(g, users) {
                self.publications.insert(instance.clone(), symbol.clone());
            }
        }
    }

    pub fn retains(&self, i: &Instance) -> bool {
        self.realization.contains_key(i)
    }
}

fn realize_decl(
    ws: &Workspace,
    rep: FuncRef,
    target: &CpuTargetSpec,
    mappings: &ArtifactMappings<'_>,
) -> Result<Realized, Unrealizable> {
    // Must precede sibling selection: these serve calls with no body for this backend.
    let rep_f = &ws.module(rep.module).functions[rep.func];
    if intrinsics::expand_kind(rep_f).is_some() && supports(target, BoundaryClass::Intrinsic) {
        return Ok(Realized {
            decl: rep,
            kind: Realization::CompileTimeExpansion,
        });
    }
    if supports(target, BoundaryClass::Intrinsic) && mappings.supplies(ws, rep_f) {
        // One rung, one oracle: `@intrinsic` only picks the label. Asking separately
        // is what once let `map_new` reach a body branch.
        let kind = if intrinsics::is_intrinsic(rep_f) {
            Realization::NativeMapping
        } else {
            Realization::HostBinding
        };
        return Ok(Realized { decl: rep, kind });
    }

    let name = ws.module(rep.module).functions[rep.func].name;
    let selected = skull_mir::lower::backend_variant(ws, name, rep, target.backend)
        .map_err(|_| Unrealizable::ForeignToArtifact);
    selected.and_then(|decl| {
        let f = &ws.module(decl.module).functions[decl.func];
        realize_body(ws, decl, f, target, mappings).map(|kind| Realized { decl, kind })
    })
}

/// Split from selection so both questions read the same `FuncRef`.
fn realize_body(
    ws: &Workspace,
    decl: FuncRef,
    f: &Function,
    target: &CpuTargetSpec,
    mappings: &ArtifactMappings<'_>,
) -> Result<Realization, Unrealizable> {
    // An erased runtime entry is an identity that exists under its bare name on
    // every target, so it is classified before any body slot is read.
    if crate::instance_graph::is_erased_runtime_entry(f) {
        return class_or_reject(target, BoundaryClass::ErasedRuntimeEntry);
    }
    if f.is_extern {
        // `extern(js)` names the browser surface, not a host symbol, so it needs
        // no foreign-symbol capability.
        if f.backend == FnBackend::Js && target.backend == FnBackend::Js {
            return Ok(Realization::ForeignBody(BoundaryClass::Extern));
        }
        return class_or_reject(target, BoundaryClass::Extern);
    }
    if intrinsics::is_intrinsic(f) {
        if !supports(target, BoundaryClass::Intrinsic) {
            return Err(Unrealizable::ClassUnsupported(BoundaryClass::Intrinsic));
        }
        // Expansion is compile-time on every backend: no body to map or fall back to.
        if intrinsics::expand_kind(f).is_some() {
            return Ok(Realization::CompileTimeExpansion);
        }
        if mappings.maps(ws, f) {
            return Ok(Realization::NativeMapping);
        }
        // Asked before the fallback rung, and never satisfied by a Rock body.
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

/// Unlike `instance_graph::has_rock_body`, requires the body itself, which an
/// expansion intrinsic lacks.
fn has_rock_body(ws: &Workspace, decl: FuncRef) -> bool {
    ws.module(decl.module)
        .bodies
        .contains_key(&bone_core::hir::BodyOwner::Function(decl.func))
}

/// Realize existing graph instances for the artifact request.
pub fn realize(ws: &Workspace, g: &InstanceGraph, req: &ArtifactRequest<'_>) -> ArtifactView {
    let target = req.spec.cpu;
    let mut realizable: FxHashMap<Instance, Result<Realized, Unrealizable>> = FxHashMap::default();
    // A declaration's instances all realize alike, so each declaration is asked once.
    let mut by_decl: FxHashMap<FuncRef, Result<Realized, Unrealizable>> = FxHashMap::default();
    for i in &g.instances {
        let realized = *by_decl
            .entry(i.func)
            .or_insert_with(|| realize_decl(ws, i.func, &target, &req.mappings));
        realizable.insert(i.clone(), realized);
    }

    // Always-emitted callees are roots: they have no caller instance.
    let root_decls: BTreeSet<DeclKey> = req
        .roots
        .iter()
        .map(|r| DeclKey(skull_mir::lower::canonical_variant(ws, *r)))
        .collect();
    let mut index = ArtifactIndex {
        realizable,
        graph_revision: g.revision(),
        root_decls,
        ..Default::default()
    };
    let mut roots: BTreeSet<Instance> = BTreeSet::new();
    let mut declared_roots: BTreeSet<Instance> = BTreeSet::new();
    for i in &g.instances {
        let declared = declared_root(ws, &index.root_decls, i);
        let is_root = declared || g.is_always_emitted_target(i);
        if is_root && index.realizable[i].is_ok() {
            roots.insert(i.clone());
            if declared {
                declared_roots.insert(i.clone());
            }
        }
    }

    // Site records, not graph edges, identify each caller-to-callee relation.
    let site_tables: FxHashMap<&Instance, &BTreeMap<Site, instance_graph::SiteRecord>> =
        g.call_sites().collect();

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
    for r in g.instances.iter().filter(|i| roots.contains(*i)) {
        walk.admit(r, &index.realizable);
    }
    // A value take in a global initializer or decorator has no caller instance,
    // so the site walk cannot see it.
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
        for (site, record) in sites.iter() {
            let SiteOutcome::Resolved(callee) = &record.outcome else {
                continue;
            };
            if !matches!(index.realizable.get(callee), Some(Ok(_))) {
                continue;
            }
            if site.op == skull_mir::site::SiteOp::FnValue {
                fn_value_targets.insert(callee.clone());
            }
            walk.admit(callee, &index.realizable);
        }
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

    // Derived in parallel, recorded in instance order.
    let planned: Vec<(&Instance, String)> = {
        use bone_core::prelude::*;
        let rows: Vec<(&Instance, Realized)> = realization.iter().map(|(i, r)| (i, *r)).collect();
        rows.into_par_iter()
            .map(|(i, r)| (i, req.symbols.symbol(ws, g, i, r)))
            .collect()
    };
    let symbols: BTreeMap<Instance, String> =
        planned.into_iter().map(|(i, s)| (i.clone(), s)).collect();

    // Minted once; every consumer reads this map, so one boundary cannot be named
    // two ways, and a renaming planner shows up here.
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

    let retained_decls: BTreeSet<DeclKey> = realization.values().map(|r| DeclKey(r.decl)).collect();

    // Lane widths are asked of the selected declaration, whose body is emitted.
    let scalar_context = std::cell::OnceCell::new();
    let scx = req.scalar_context(ws, g, &scalar_context);
    let mut publisher_lanes: BTreeMap<Instance, usize> = BTreeMap::new();
    for (i, r) in &realization {
        if !matches!(r.kind, Realization::ForeignBody(BoundaryClass::InlineBody)) {
            continue;
        }
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

    // Over every retained instance: a producer that bypassed the recorder is a
    // graph defect wherever it lands.
    let missing_substitutions: BTreeSet<Instance> = realization
        .keys()
        .filter(|i| g.concreteness(i) == Concreteness::Missing)
        .cloned()
        .collect();

    for (instance, symbol) in req
        .roots
        .iter()
        .filter_map(|&root| publication_candidate(ws, &target, root))
    {
        index.add_publication_candidate(instance, symbol);
    }
    let publications = index
        .publication_users
        .iter()
        .filter_map(|(symbol, candidates)| {
            let instance = unique_publication(g, candidates)?;
            Some((instance.clone(), symbol.clone()))
        })
        .collect();

    let opaque_boundary_roots = g
        .inline_body_nomangle_roots
        .iter()
        .filter(|(owner, _)| retained_decls.contains(owner))
        .map(|(owner, targets)| (*owner, targets.clone()))
        .collect();

    ArtifactView {
        publisher_lanes,
        missing_substitutions,
        publications,
        spec: req.spec,
        roots,
        declared_roots,
        fn_value_targets,
        instances,
        boundaries,
        realization,
        symbols,
        opaque_boundary_roots,
        index,
    }
}
