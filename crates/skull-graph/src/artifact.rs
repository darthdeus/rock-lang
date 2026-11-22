use std::collections::{BTreeMap, BTreeSet, VecDeque};

use bone_core::hir::{FnBackend, Function};
use bone_core::workspace::Workspace;
use skull_mir::intrinsics::{self, BackendRealization};
use skull_mir::lower::{BackendFnAbiPlan, ScalarCtx};
use skull_mir::mir::FuncRef;
use skull_mir::monoty::MonoTyReason;

use crate::instance_graph::{
    self, DeclKey, Instance, InstanceGraph, Site, SiteOutcome, UpdateStats,
};
use crate::target_projection::CpuTargetSpec;
use rustc_hash::{FxHashMap, FxHashSet};

pub struct ArtifactRequest<'a> {
    pub target: CpuTargetSpec,
    pub roots: &'a [FuncRef],
    pub scx: &'a ScalarCtx,
    pub supply: Option<&'a dyn BackendRealization>,
}

impl ArtifactRequest<'_> {
    fn check_context(&self, graph: &InstanceGraph) {
        assert_eq!(self.scx.target(), self.target.backend);
        assert!(std::ptr::eq(self.scx.interner(), graph.interner().as_ref()));
    }

    fn supplies(&self, ws: &Workspace, f: &Function) -> bool {
        self.supply.is_some_and(|source| source.supplies(ws, f))
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Realization {
    RockBody,
    InlineBody,
    Boundary,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Realized {
    pub decl: FuncRef,
    pub kind: Realization,
}

#[derive(Clone)]
pub struct ArtifactView {
    /// Realizable emission roots, plus callees of always-emitted bodies.
    pub roots: BTreeSet<Instance>,
    /// Realizable request roots. Their definitions are required even when
    /// every call is inlined.
    pub declared_roots: BTreeSet<Instance>,
    /// Function values need addressable definitions even with no direct calls.
    pub fn_value_targets: BTreeSet<Instance>,
    pub realization: BTreeMap<Instance, Realized>,
    pub publisher_lanes: BTreeMap<Instance, usize>,
    /// Required `@nomangle` definitions keyed by the exact declaration. Calls can
    /// still resolve via a sibling in `realization`.
    pub publications: BTreeMap<Instance, String>,
    index: ArtifactIndex,
}

#[derive(Clone, Default)]
struct ArtifactIndex {
    realizable: FxHashMap<Instance, Realized>,
    always_fn_values: FxHashSet<Instance>,
    publication_candidates: BTreeMap<Instance, String>,
    publication_users: BTreeMap<String, BTreeSet<Instance>>,
    root_decls: BTreeSet<DeclKey>,
}

impl ArtifactIndex {
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
    pub fn update_local_with_root_delta(
        &mut self,
        ws: &Workspace,
        g: &InstanceGraph,
        req: &ArtifactRequest<'_>,
        update: &UpdateStats,
        root_added: &[FuncRef],
        root_removed: &[FuncRef],
    ) {
        req.check_context(g);
        let canonical = |roots: &[FuncRef]| -> BTreeSet<DeclKey> {
            roots
                .iter()
                .map(|r| DeclKey(skull_mir::lower::canonical_variant(ws, *r)))
                .collect()
        };
        let root_added = &canonical(root_added);
        let root_removed = &canonical(root_removed);
        let mut subjects = update.revisited.clone();
        subjects.extend(update.removed.iter().cloned());
        subjects.extend(update.changed_calls.keys().cloned());
        for decl in root_added.iter().chain(root_removed) {
            subjects.extend(g.instances_of(decl.0).cloned());
        }
        if subjects.is_empty() && root_added.is_empty() && root_removed.is_empty() {
            return;
        }

        for decl in root_removed {
            self.index.root_decls.remove(decl);
        }
        self.index.root_decls.extend(root_added);

        let mut admission_changed = false;
        for i in &subjects {
            let live = g.instances.contains(i);
            let now = live.then(|| realize_decl(ws, i.func, req)).flatten();
            let was = match now {
                Some(realization) => self.index.realizable.insert(i.clone(), realization),
                None => self.index.realizable.remove(i),
            };
            let realizable = now.is_some();
            let declared = realizable && declared_root(ws, &self.index.root_decls, i);
            let root = declared || (realizable && g.is_always_emitted_target(i));
            let always_value = realizable && g.is_always_emitted_fn_value(i);
            let was_root = self.roots.contains(i);
            let was_always_value = self.index.always_fn_values.contains(i);

            if root {
                self.roots.insert(i.clone());
            } else {
                self.roots.remove(i);
            }
            if declared {
                self.declared_roots.insert(i.clone());
            } else {
                self.declared_roots.remove(i);
            }
            if always_value {
                self.index.always_fn_values.insert(i.clone());
            } else {
                self.index.always_fn_values.remove(i);
            }

            admission_changed |= was != now || was_root != root || was_always_value != always_value;
        }
        if !admission_changed
            && update.changed_calls.is_empty()
            && update.added.is_empty()
            && update.removed.is_empty()
            && root_added.is_empty()
            && root_removed.is_empty()
        {
            return;
        }

        let mut dirty = subjects.clone();
        let mut queue: VecDeque<Instance> = subjects.iter().cloned().collect();
        while let Some(i) = queue.pop_front() {
            let neighbours = g
                .resolved_sites(&i)
                .map(|(_, callee)| callee)
                .chain(g.predecessors(&i))
                .chain(update.changed_calls.get(&i).into_iter().flatten());
            for neighbour in neighbours {
                if dirty.insert(neighbour.clone()) {
                    queue.push_back(neighbour.clone());
                }
            }
        }

        for i in &dirty {
            self.realization.remove(i);
            self.publisher_lanes.remove(i);
            self.fn_value_targets.remove(i);
        }
        let seeds = self
            .roots
            .iter()
            .filter(|i| dirty.contains(*i))
            .filter_map(|i| g.instances.get(i));
        let (rows, values) = reachable_rows(&self.index, seeds, |i| g.resolved_sites(i));
        self.install_rows(ws, g, req, rows, values, false);

        let mut changed_publication_symbols: BTreeSet<String> = dirty
            .iter()
            .filter_map(|instance| self.index.publication_candidates.get(instance).cloned())
            .collect();
        let publication_symbols =
            self.refresh_publication_candidates(ws, req, root_added, root_removed);
        changed_publication_symbols.extend(publication_symbols);
        self.refresh_publications_local(g, &changed_publication_symbols);
    }

    fn install_rows(
        &mut self,
        ws: &Workspace,
        g: &InstanceGraph,
        req: &ArtifactRequest<'_>,
        rows: Vec<(&Instance, Realized)>,
        values: BTreeSet<Instance>,
        cold: bool,
    ) {
        self.fn_value_targets.extend(values);
        for (instance, realized) in rows {
            self.realization.insert(instance.clone(), realized);
            match publisher_lanes(ws, g, req.scx, instance, realized) {
                Ok(Some(lanes)) => {
                    self.publisher_lanes.insert(instance.clone(), lanes);
                }
                Err(reason) if cold => panic!(
                    "retained inline body {instance:?} has no concrete publisher signature: {reason:?}"
                ),
                _ => {}
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
        let target = req.target;
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

    pub fn retains_rock_body(&self, i: &Instance) -> bool {
        self.realization
            .get(i)
            .is_some_and(|row| row.kind == Realization::RockBody)
    }

    pub fn retains(&self, i: &Instance) -> bool {
        self.realization.contains_key(i)
    }
}

fn realize_decl(ws: &Workspace, rep: FuncRef, request: &ArtifactRequest<'_>) -> Option<Realized> {
    let target = &request.target;
    let f = &ws.module(rep.module).functions[rep.func];
    if intrinsics::expand_kind(f).is_some() || request.supplies(ws, f) {
        return Some(Realized {
            decl: rep,
            kind: Realization::Boundary,
        });
    }
    let decl = skull_mir::lower::backend_variant(ws, f.name, rep, target.backend).ok()?;
    let f = &ws.module(decl.module).functions[decl.func];
    let kind = if instance_graph::is_erased_runtime_entry(f) {
        Some(Realization::Boundary)
    } else if f.is_extern {
        (f.backend == FnBackend::Js && target.backend == FnBackend::Js || target.foreign_symbols)
            .then_some(Realization::Boundary)
    } else if intrinsics::is_intrinsic(f) {
        if intrinsics::expand_kind(f).is_some()
            || f.inline_body
                .is_some_and(|(backend, _)| instance_graph::serves(backend, target.backend))
            || request.supplies(ws, f)
        {
            Some(Realization::Boundary)
        } else {
            (!intrinsics::is_native_required(f) && has_rock_body(ws, decl))
                .then_some(Realization::RockBody)
        }
    } else if f.inline_body.is_some() {
        Some(Realization::InlineBody)
    } else {
        Some(Realization::RockBody)
    }?;
    Some(Realized { decl, kind })
}

fn has_rock_body(ws: &Workspace, decl: FuncRef) -> bool {
    ws.module(decl.module)
        .bodies
        .contains_key(&bone_core::hir::BodyOwner::Function(decl.func))
}

fn publisher_lanes(
    ws: &Workspace,
    graph: &InstanceGraph,
    scx: &ScalarCtx,
    instance: &Instance,
    realized: Realized,
) -> Result<Option<usize>, MonoTyReason> {
    if realized.kind != Realization::InlineBody || graph.substitution(instance).is_none() {
        return Ok(None);
    }
    let selected = Instance {
        func: realized.decl,
        args: instance.args.clone(),
    };
    debug_assert_eq!(
        ws.module(selected.func.module).functions[selected.func.func]
            .generics
            .len(),
        ws.module(instance.func.module).functions[instance.func.func]
            .generics
            .len(),
        "a backend sibling must declare the same generics as the instance it serves",
    );
    let signature = crate::concrete_body::concrete_signature_for_instance(ws, graph, instance)?;
    Ok(Some(
        BackendFnAbiPlan::for_concrete_signature(ws, scx, &selected, &signature).result_lanes(),
    ))
}

/// Realize existing graph instances for the artifact request.
pub fn realize(ws: &Workspace, g: &InstanceGraph, req: &ArtifactRequest<'_>) -> ArtifactView {
    req.check_context(g);
    let target = req.target;
    let mut realizable: FxHashMap<Instance, Realized> = FxHashMap::default();
    // A declaration's instances all realize alike, so each declaration is asked once.
    let mut by_decl: FxHashMap<FuncRef, Option<Realized>> = FxHashMap::default();
    for i in &g.instances {
        let realized = *by_decl
            .entry(i.func)
            .or_insert_with(|| realize_decl(ws, i.func, req));
        if let Some(realized) = realized {
            realizable.insert(i.clone(), realized);
        }
    }

    // Always-emitted callees are roots: they have no caller instance.
    let root_decls: BTreeSet<DeclKey> = req
        .roots
        .iter()
        .map(|r| DeclKey(skull_mir::lower::canonical_variant(ws, *r)))
        .collect();
    let mut index = ArtifactIndex {
        realizable,
        root_decls,
        ..Default::default()
    };
    let mut roots: BTreeSet<Instance> = BTreeSet::new();
    let mut declared_roots: BTreeSet<Instance> = BTreeSet::new();
    for i in &g.instances {
        let declared = declared_root(ws, &index.root_decls, i);
        let is_root = declared || g.is_always_emitted_target(i);
        if is_root && index.realizable.contains_key(i) {
            roots.insert(i.clone());
            if declared {
                declared_roots.insert(i.clone());
            }
        }
    }

    // Site records, not graph edges, identify each caller-to-callee relation.
    let site_tables: FxHashMap<&Instance, &BTreeMap<Site, instance_graph::SiteRecord>> =
        g.call_sites().collect();

    index.always_fn_values = roots
        .iter()
        .filter(|r| g.is_always_emitted_fn_value(r))
        .cloned()
        .collect();
    let seeds = g.instances.iter().filter(|i| roots.contains(*i));
    let (rows, values) = reachable_rows(&index, seeds, |i| {
        site_tables.get(i).copied().into_iter().flat_map(|sites| {
            sites
                .iter()
                .filter_map(|(site, record)| match &record.outcome {
                    SiteOutcome::Resolved(callee) => Some((site, callee)),
                    _ => None,
                })
        })
    });
    drop(site_tables);

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

    let mut view = ArtifactView {
        publisher_lanes: BTreeMap::new(),
        publications,
        roots,
        declared_roots,
        fn_value_targets: BTreeSet::new(),
        realization: BTreeMap::new(),
        index,
    };
    view.install_rows(ws, g, req, rows, values, true);
    view
}

fn reachable_rows<'g, Sites>(
    index: &ArtifactIndex,
    seeds: impl IntoIterator<Item = &'g Instance>,
    sites: impl Fn(&'g Instance) -> Sites,
) -> (Vec<(&'g Instance, Realized)>, BTreeSet<Instance>)
where
    Sites: IntoIterator<Item = (&'g Site, &'g Instance)>,
{
    let mut reached = FxHashSet::default();
    let mut rows = Vec::new();
    let mut follow = VecDeque::new();
    let mut values = BTreeSet::new();
    let mut add = |instance, follow: &mut VecDeque<_>| {
        let Some(realized) = index.realizable.get(instance).copied() else {
            return false;
        };
        if reached.insert(instance) {
            rows.push((instance, realized));
            if realized.kind == Realization::RockBody {
                follow.push_back(instance);
            }
        }
        true
    };
    for root in seeds {
        if add(root, &mut follow) && index.always_fn_values.contains(root) {
            values.insert(root.clone());
        }
    }
    while let Some(caller) = follow.pop_front() {
        for (site, callee) in sites(caller) {
            if add(callee, &mut follow) && site.op == skull_mir::site::SiteOp::FnValue {
                values.insert(callee.clone());
            }
        }
    }
    rows.sort_unstable_by(|a, b| a.0.cmp(b.0));
    (rows, values)
}
