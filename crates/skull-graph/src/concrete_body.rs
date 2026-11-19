use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use bone_core::hir::{Body, BodyOwner, Expr, TypeExpr};
use bone_core::prelude::TyId;
use bone_core::workspace::{BodyInference, Workspace};
use comfy_la_arena::{ArenaMap, Idx};
use rustc_hash::{FxHashMap, FxHashSet};
use skull_mir::concrete::{
    ConcreteAlwaysPlans, ConcreteBodyPlan, ConcreteCallee, ConcreteOwner, ConcretePlans,
    ConcreteSignature, ConcreteSignatureHandle, ConcreteTypeTables,
};
use skull_mir::mir::FuncRef;
use skull_mir::monoty::{MonoCtor, MonoInterner, MonoSubst, MonoTy, MonoTyId, MonoTyReason, TyEnv};
use ustr::Ustr;

use crate::instance_graph::{
    ready_body, BoundaryClass, Instance, InstanceGraph, Owner, Site, SiteDomain, SiteOp,
    SiteOutcome,
};
use crate::target_projection::CpuTargetSpec;

/// A callee whose body skull does not lower.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct BoundaryId {
    pub declaration: FuncRef,
    pub class: BoundaryClass,
    pub target: CpuTargetSpec,
    pub symbol: String,
}

type TypeConversionMemo = FxHashMap<TyId, Result<MonoTyId, MonoTyReason>>;

#[derive(Clone, Default)]
pub struct ConcreteBodies {
    pub plans: Arc<ConcretePlans>,
    pub always_plans: ConcreteAlwaysPlans,
    instance_stats: FxHashMap<Instance, ConcreteBuildStats>,
    always_stats: FxHashMap<ConcreteOwner, ConcreteBuildStats>,
    global_build_stats: ConcreteBuildStats,
    plan_failure_index: BTreeMap<Instance, MonoTyReason>,
    always_plan_failure_index: FxHashMap<ConcreteOwner, MonoTyReason>,
    candidate_keys: BTreeSet<Instance>,
    owner_candidates: FxHashMap<Instance, FxHashSet<Instance>>,
    candidate_index: std::sync::OnceLock<CandidateIndex>,
}

#[derive(Clone, Default)]
struct CandidateIndex {
    by_func: FxHashMap<FuncRef, FxHashSet<Instance>>,
    users: FxHashMap<Instance, FxHashSet<Instance>>,
}

#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConcreteBuildStats {
    pub plan_attempts: usize,
    pub plan_successes: usize,
    pub plan_failures: usize,
    pub always_plan_attempts: usize,
    pub always_plan_successes: usize,
    pub always_plan_failures: usize,
    pub source_expr_slots: usize,
    pub source_binding_slots: usize,
    pub callee_scans: usize,
    pub callee_rows_visited: usize,
    pub plan_callee_map_inserts: usize,
    pub plan_callee_signature_inserts: usize,
    pub type_nodes_examined: usize,
    pub type_node_memo_hits: usize,
    pub type_node_conversion_attempts: usize,
    pub type_node_unresolved: usize,
    pub struct_literals_examined: usize,
    pub struct_field_conversion_attempts: usize,
    pub foreign_domain_requests: usize,
    pub foreign_domain_unique: usize,
    pub signature_ns: u64,
    pub type_nodes_ns: u64,
    pub struct_fields_ns: u64,
    pub foreign_domains_ns: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConcreteUpdateStats {
    pub candidate_keys_added: BTreeSet<Instance>,
    pub candidate_keys_removed: BTreeSet<Instance>,
}

impl ConcreteBuildStats {
    fn add(&mut self, other: Self) {
        self.plan_attempts += other.plan_attempts;
        self.plan_successes += other.plan_successes;
        self.plan_failures += other.plan_failures;
        self.always_plan_attempts += other.always_plan_attempts;
        self.always_plan_successes += other.always_plan_successes;
        self.always_plan_failures += other.always_plan_failures;
        self.source_expr_slots += other.source_expr_slots;
        self.source_binding_slots += other.source_binding_slots;
        self.callee_scans += other.callee_scans;
        self.callee_rows_visited += other.callee_rows_visited;
        self.plan_callee_map_inserts += other.plan_callee_map_inserts;
        self.plan_callee_signature_inserts += other.plan_callee_signature_inserts;
        self.type_nodes_examined += other.type_nodes_examined;
        self.type_node_memo_hits += other.type_node_memo_hits;
        self.type_node_conversion_attempts += other.type_node_conversion_attempts;
        self.type_node_unresolved += other.type_node_unresolved;
        self.struct_literals_examined += other.struct_literals_examined;
        self.struct_field_conversion_attempts += other.struct_field_conversion_attempts;
        self.foreign_domain_requests += other.foreign_domain_requests;
        self.foreign_domain_unique += other.foreign_domain_unique;
        self.signature_ns += other.signature_ns;
        self.type_nodes_ns += other.type_nodes_ns;
        self.struct_fields_ns += other.struct_fields_ns;
        self.foreign_domains_ns += other.foreign_domains_ns;
    }
}

impl ConcreteBodies {
    pub fn plan_for(&self, instance: &Instance) -> Option<&ConcreteBodyPlan> {
        self.plans.get(instance).map(|plan| &**plan)
    }

    pub fn plan_for_owner(&self, owner: &ConcreteOwner) -> Option<&ConcreteBodyPlan> {
        self.always_plans.get(owner)
    }

    /// Clone the shared handle map before mutating; per-entry `Arc`s stay shared.
    pub fn detach_plan_store(&mut self) {
        if Arc::strong_count(&self.plans) > 1 {
            self.plans = Arc::new((*self.plans).clone());
        }
    }

    pub fn candidate_keys(&self) -> &BTreeSet<Instance> {
        &self.candidate_keys
    }

    pub fn candidate_keys_by_func(&self) -> &FxHashMap<FuncRef, FxHashSet<Instance>> {
        &self.derived_candidate_index().by_func
    }

    fn derived_candidate_index(&self) -> &CandidateIndex {
        self.candidate_index.get_or_init(|| {
            let mut index = CandidateIndex::default();
            for (owner, candidates) in &self.owner_candidates {
                for candidate in candidates {
                    index
                        .users
                        .entry(candidate.clone())
                        .or_default()
                        .insert(owner.clone());
                    index
                        .by_func
                        .entry(candidate.func)
                        .or_default()
                        .insert(candidate.clone());
                }
            }
            index
        })
    }

    /// Release the per-owner stats; the plans can no longer be updated incrementally.
    pub fn take_build_stats(&mut self) -> impl Send + 'static {
        (
            std::mem::take(&mut self.instance_stats),
            std::mem::take(&mut self.always_stats),
        )
    }

    pub fn snapshot_build_stats(&self) -> ConcreteBuildStats {
        let mut stats = self.global_build_stats;
        for owner_stats in self
            .instance_stats
            .values()
            .chain(self.always_stats.values())
        {
            stats.add(*owner_stats);
        }
        stats
    }

    pub fn first_plan_failure(&self) -> Option<(&Instance, &MonoTyReason)> {
        self.plan_failure_index.first_key_value()
    }

    pub fn first_always_plan_failure(&self) -> Option<(&ConcreteOwner, &MonoTyReason)> {
        self.always_plan_failure_index
            .iter()
            .min_by(|(left, _), (right, _)| concrete_owner_order(left, right))
    }

    pub fn update_local(
        &mut self,
        ws: &Workspace,
        g: &InstanceGraph,
        revisited: &BTreeSet<Instance>,
        removed: &BTreeSet<Instance>,
        revisited_owners: &BTreeSet<Owner>,
    ) -> ConcreteUpdateStats {
        let mut instance_subjects = revisited.clone();
        instance_subjects.extend(removed.iter().cloned());
        let mut candidate_keys_added = BTreeSet::new();
        let mut candidate_keys_removed = BTreeSet::new();
        for instance in &instance_subjects {
            candidate_keys_removed.extend(self.remove_instance(instance));
        }
        let mut targets = revisited.clone();
        targets.extend(removed.iter().cloned());
        targets.retain(|instance| g.instances.contains(instance));
        let (results, _) = build_instance_delta(ws, g, &targets);
        for built in results.into_iter().flatten() {
            let instance = built.instance.clone();
            let stats = built.stats;
            let candidate_refs = built.candidate_refs;
            if let Err(reason) = &built.plan {
                self.plan_failure_index
                    .insert(instance.clone(), reason.clone());
            }
            if let Ok(plan) = built.plan {
                candidate_keys_added
                    .extend(self.install_candidate_refs_owned(&instance, candidate_refs));
                Arc::get_mut(&mut self.plans)
                    .expect("local concrete update lost its unique plan store")
                    .insert(instance.clone(), Arc::new(plan));
            }

            self.instance_stats.insert(instance, stats);
        }
        let mut always_subjects = Vec::new();
        for owner in revisited_owners {
            let Owner::AlwaysEmitted(module, body_owner) = owner else {
                continue;
            };
            let Some(owner) = concrete_owner_for(*module, *body_owner) else {
                continue;
            };
            always_subjects.push((*module, *body_owner, owner));
        }
        for (_, _, owner) in &always_subjects {
            self.always_stats.remove(owner);
            self.always_plan_failure_index.remove(owner);
            self.always_plans.remove(owner);
        }
        let timing_enabled = log::log_enabled!(log::Level::Debug);
        let always_inputs: Vec<_> = always_subjects
            .iter()
            .map(|(module, body_owner, _)| {
                prepare_always(ws, g, *module, *body_owner, timing_enabled)
            })
            .collect();
        if !always_inputs.is_empty() {
            let always_owners: BTreeSet<Owner> = always_inputs
                .iter()
                .map(|prepared| Owner::AlwaysEmitted(prepared.module, prepared.body_owner))
                .collect();
            let signatures = publish_signatures_local(ws, g, &targets, &always_owners);
            let mut publication_stats = ConcreteBuildStats::default();
            let domains = collect_foreign_domains_local(g, &targets, &always_owners);
            let published_domains = publish_foreign_domains(
                ws,
                g,
                domains,
                &mut publication_stats,
                log::log_enabled!(log::Level::Debug),
            );
            for prepared in always_inputs {
                self.install_always(produce_always(
                    ws,
                    g,
                    prepared,
                    &signatures,
                    &published_domains,
                    timing_enabled,
                ));
            }
        }
        ConcreteUpdateStats {
            candidate_keys_added,
            candidate_keys_removed,
        }
    }

    fn install_always(&mut self, result: BuiltAlways) {
        match result.plan {
            Ok(Some(plan)) => {
                self.always_plans.insert(plan.owner.clone(), plan);
            }
            Err(reason) => {
                if let Some(owner) = &result.owner {
                    self.always_plan_failure_index.insert(owner.clone(), reason);
                }
            }
            Ok(None) => {}
        }
        if let Some(owner) = result.owner {
            self.always_stats.insert(owner, result.stats);
        }
    }

    fn remove_instance(&mut self, instance: &Instance) -> BTreeSet<Instance> {
        let mut removed = BTreeSet::new();
        self.derived_candidate_index();
        if let Some(candidates) = self.owner_candidates.remove(instance) {
            let index = self
                .candidate_index
                .get_mut()
                .expect("the candidate index was derived above");
            for candidate in candidates {
                let Some(users) = index.users.get_mut(&candidate) else {
                    continue;
                };
                users.remove(instance);
                if users.is_empty() {
                    index.users.remove(&candidate);
                    self.candidate_keys.remove(&candidate);
                    if let Some(keys) = index.by_func.get_mut(&candidate.func) {
                        keys.remove(&candidate);
                        if keys.is_empty() {
                            index.by_func.remove(&candidate.func);
                        }
                    }
                    removed.insert(candidate);
                }
            }
        }
        self.instance_stats.remove(instance);
        self.plan_failure_index.remove(instance);
        Arc::get_mut(&mut self.plans)
            .expect("local concrete update lost its unique plan store")
            .remove(instance);
        removed
    }

    fn install_candidate_refs_owned(
        &mut self,
        owner: &Instance,
        refs: FxHashSet<Instance>,
    ) -> BTreeSet<Instance> {
        let mut added = BTreeSet::new();
        self.derived_candidate_index();
        let index = self
            .candidate_index
            .get_mut()
            .expect("the candidate index was derived above");
        for candidate in &refs {
            match index.users.entry(candidate.clone()) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    self.candidate_keys.insert(candidate.clone());
                    index
                        .by_func
                        .entry(candidate.func)
                        .or_default()
                        .insert(candidate.clone());
                    added.insert(candidate.clone());
                    let mut users = FxHashSet::default();
                    users.insert(owner.clone());
                    entry.insert(users);
                }
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    entry.get_mut().insert(owner.clone());
                }
            }
        }
        self.owner_candidates.insert(owner.clone(), refs);
        added
    }
}

#[derive(Default)]
struct Converted {
    types: ConcreteTypeTables,
    error: Option<MonoTyReason>,
    expr_examined: usize,
    binding_examined: usize,
    type_memo: TypeConversionMemo,
}

fn convert_body(
    arena: &MonoInterner,
    ws: &Workspace,
    inf: &BodyInference,
    body: &Body,
    subst: &MonoSubst,
) -> Converted {
    let mut c = Converted::default();
    let mut memo: TypeConversionMemo = FxHashMap::default();
    let mut convert = |ty: TyId| {
        memo.entry(ty)
            .or_insert_with(|| arena.convert(ws, inf, ty, subst))
            .clone()
    };
    for (expr, ty) in inf.expr_types.iter() {
        c.expr_examined += 1;
        match convert(*ty) {
            Ok(id) => {
                c.types.expr_types.insert(expr, id);
            }
            Err(MonoTyReason::UnresolvedInfer(_)) => {
                c.types.untyped_exprs.insert(expr);
            }
            Err(reason) => {
                c.error.get_or_insert(reason);
            }
        }
    }
    for (expr, _) in body.expr_arena.iter() {
        if inf.expr_types.get(expr).is_none() {
            c.types.untyped_exprs.insert(expr);
        }
    }
    for (binding, _) in body.binding_arena.iter() {
        if inf.binding_types.get(binding).is_none() {
            c.types.untyped_bindings.insert(binding);
        }
    }
    for (binding, ty) in inf.binding_types.iter() {
        c.binding_examined += 1;
        match convert(*ty) {
            Ok(id) => {
                c.types.binding_types.insert(binding, id);
            }
            Err(MonoTyReason::UnresolvedInfer(_)) => {
                c.types.untyped_bindings.insert(binding);
            }
            Err(reason) => {
                c.error.get_or_insert(reason);
            }
        }
    }
    c.type_memo = memo;
    c
}

fn concrete_signature(
    ws: &Workspace,
    g: &InstanceGraph,
    inst: &Instance,
    subst: &MonoSubst,
) -> Result<ConcreteSignature, MonoTyReason> {
    let f = &ws.module(inst.func.module).functions[inst.func.func];
    let arena = g.interner();
    let params = f
        .param_decls
        .iter()
        .map(|param| arena.convert_decl(ws, param.ty, subst))
        .collect::<Result<Vec<_>, _>>()?;
    let param_tuple_spelled = f
        .param_decls
        .iter()
        .map(|param| declared_tuple_param(ws, param.ty))
        .collect::<Result<Vec<_>, _>>()?;
    let result = f
        .return_ty
        .map(|ty| arena.convert_decl(ws, ty, subst))
        .transpose()?;
    Ok(ConcreteSignature {
        params,
        param_tuple_spelled,
        result,
    })
}

fn declared_tuple_param(ws: &Workspace, ty: TyId) -> Result<bool, MonoTyReason> {
    match TyEnv::Decl.decl_node(ws, ty)? {
        TypeExpr::Tuple { .. } => Ok(true),
        TypeExpr::Nullable(inner) => Ok(matches!(
            TyEnv::Decl.decl_node(ws, ty.with_idx(inner))?,
            TypeExpr::Tuple { .. }
        )),
        TypeExpr::Builtin(_)
        | TypeExpr::App { .. }
        | TypeExpr::Generic(..)
        | TypeExpr::Infer(..)
        | TypeExpr::Function { .. } => Ok(false),
    }
}

pub(crate) fn concrete_signature_for_instance(
    ws: &Workspace,
    g: &InstanceGraph,
    inst: &Instance,
) -> Result<ConcreteSignature, MonoTyReason> {
    let subst = g.substitution(inst).ok_or(MonoTyReason::NoNode)?;
    concrete_signature(ws, g, inst, subst)
}

fn concrete_tables(
    ws: &Workspace,
    g: &InstanceGraph,
    inf: &BodyInference,
    body: &Body,
    subst: &MonoSubst,
    declared_types: impl IntoIterator<Item = TyId>,
    stats: &mut ConcreteBuildStats,
    timing_enabled: bool,
) -> Result<ConcreteTypeTables, MonoTyReason> {
    let mut converted = convert_body(g.interner(), ws, inf, body, subst);
    if let Some(reason) = converted.error {
        return Err(reason);
    }
    let ty_types = concrete_ty_types(
        ws,
        g,
        inf,
        body,
        subst,
        declared_types,
        &mut converted.type_memo,
        stats,
        timing_enabled,
    )?;
    let struct_field_types = concrete_struct_field_types(
        ws,
        g,
        body,
        &converted.types.expr_types,
        stats,
        timing_enabled,
    )?;
    converted.types.ty_types = ty_types;
    converted.types.struct_field_types = struct_field_types;
    Ok(converted.types)
}

fn concrete_struct_field_types(
    ws: &Workspace,
    g: &InstanceGraph,
    body: &Body,
    expr_types: &ArenaMap<Idx<Expr>, MonoTyId>,
    stats: &mut ConcreteBuildStats,
    timing_enabled: bool,
) -> Result<ArenaMap<Idx<Expr>, FxHashMap<Ustr, MonoTyId>>, MonoTyReason> {
    let mut out = ArenaMap::default();
    let started = timing_enabled.then(bone_core::prelude::Instant::now);
    for (expr, node) in body.expr_arena.iter() {
        let Expr::StructLiteral { name, .. } = node else {
            continue;
        };
        stats.struct_literals_examined += 1;
        let sid = *ws.struct_by_name.get(name).ok_or(MonoTyReason::NoNode)?;
        let def = &ws.modules[sid.module.index()].structs[sid.def];
        let params = match expr_types.get(expr).copied() {
            Some(ty_id) => {
                let concrete = g.interner().get(ty_id);
                let (payload, _) = concrete.peel_nullable();
                let MonoTy::App {
                    ctor: MonoCtor::Nominal(actual),
                    params,
                } = payload
                else {
                    return Err(MonoTyReason::NoNode);
                };
                if *actual != *name || def.generics.len() != params.len() {
                    return Err(MonoTyReason::NoNode);
                }
                params.to_vec()
            }
            None if def.generics.is_empty() => Vec::new(),
            None => return Err(MonoTyReason::NoNode),
        };
        let mut subst = MonoSubst::default();
        for (generic, param) in def.generics.iter().zip(params) {
            let TypeExpr::Generic(var, _) = TyEnv::Decl.decl_node(ws, *generic)? else {
                return Err(MonoTyReason::NoNode);
            };
            subst.insert(var, param.clone());
        }
        let arena = g.interner();
        let fields = {
            def.fields
                .iter()
                .map(|field| {
                    stats.struct_field_conversion_attempts += 1;
                    arena
                        .convert_decl(ws, field.ty, &subst)
                        .map(|ty| (field.name, ty))
                })
                .collect::<Result<FxHashMap<_, _>, _>>()
        }?;
        out.insert(expr, fields);
    }
    if let Some(started) = started {
        stats.struct_fields_ns += started.elapsed().as_nanos() as u64;
    }
    Ok(out)
}

fn concrete_ty_types(
    ws: &Workspace,
    g: &InstanceGraph,
    inf: &BodyInference,
    body: &Body,
    subst: &MonoSubst,
    declared_types: impl IntoIterator<Item = TyId>,
    memo: &mut TypeConversionMemo,
    stats: &mut ConcreteBuildStats,
    timing_enabled: bool,
) -> Result<rustc_hash::FxHashMap<TyId, MonoTyId>, MonoTyReason> {
    let mut nodes = FxHashSet::default();
    for ty in inf.expr_types.values().chain(inf.binding_types.values()) {
        collect_type_nodes(ws, inf, *ty, &mut nodes);
    }
    for ty in inf.fn_field_call_types.values() {
        collect_type_nodes(ws, inf, *ty, &mut nodes);
    }
    for ty in declared_types {
        collect_type_nodes(ws, inf, ty, &mut nodes);
    }
    for (_, binding) in body.binding_arena.iter() {
        if let Some(ty) = binding.declared_ty {
            collect_type_nodes(ws, inf, ty, &mut nodes);
        }
    }
    for (_, expr) in body.expr_arena.iter() {
        if let Expr::As { target, .. } = expr {
            collect_type_nodes(ws, inf, *target, &mut nodes);
        }
    }
    stats.type_nodes_examined += nodes.len();
    let started = timing_enabled.then(bone_core::prelude::Instant::now);
    let arena = g.interner();
    {
        nodes
            .into_iter()
            .filter_map(|ty| {
                let converted = if let Some(cached) = memo.get(&ty) {
                    stats.type_node_memo_hits += 1;
                    cached.clone()
                } else {
                    stats.type_node_conversion_attempts += 1;
                    let converted = arena.convert(ws, inf, ty, subst);
                    memo.insert(ty, converted.clone());
                    converted
                };
                match converted {
                    Ok(id) => Some(Ok((ty, id))),
                    Err(MonoTyReason::UnresolvedInfer(_)) => {
                        stats.type_node_unresolved += 1;
                        None
                    }
                    Err(reason) => Some(Err(reason)),
                }
            })
            .collect::<Result<rustc_hash::FxHashMap<_, _>, _>>()
    }
    .inspect(|_| {
        if let Some(started) = started {
            stats.type_nodes_ns += started.elapsed().as_nanos() as u64;
        }
    })
}

fn collect_type_nodes(ws: &Workspace, inf: &BodyInference, ty: TyId, out: &mut FxHashSet<TyId>) {
    if !out.insert(ty) {
        return;
    }
    let node = match ty {
        TyId::Static(mid, idx) => ws.module(mid).ty_arena[idx],
        TyId::Local(idx) => inf.ty_arena[idx],
    };
    match node {
        TypeExpr::Nullable(inner) => collect_type_nodes(ws, inf, ty.with_idx(inner), out),
        TypeExpr::App { params, .. } => {
            for param in params {
                collect_type_nodes(ws, inf, ty.with_idx(param), out);
            }
        }
        TypeExpr::Function { params, ret } => {
            for param in params {
                collect_type_nodes(ws, inf, ty.with_idx(param), out);
            }
            collect_type_nodes(ws, inf, ty.with_idx(ret), out);
        }
        TypeExpr::Tuple { elements } => {
            for element in elements {
                collect_type_nodes(ws, inf, ty.with_idx(element), out);
            }
        }
        TypeExpr::Infer(var, _) => {
            if let Some(next) = inf.substitutions.get(var) {
                collect_type_nodes(ws, inf, next, out);
            }
        }
        TypeExpr::Builtin(_) | TypeExpr::Generic(..) => {}
    }
}

struct BuiltInstance {
    instance: Instance,
    plan: Result<ConcreteBodyPlan, MonoTyReason>,
    candidate_refs: FxHashSet<Instance>,
    stats: ConcreteBuildStats,
    convert_time: std::time::Duration,
    callee_time: std::time::Duration,
    plan_time: std::time::Duration,
    elapsed: std::time::Duration,
    slot_tyids: usize,
    unique_slot_tyids: usize,
}

fn concrete_owner_order(left: &ConcreteOwner, right: &ConcreteOwner) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let tag = |owner: &ConcreteOwner| match owner {
        ConcreteOwner::Function(_) => 0,
        ConcreteOwner::Global { .. } => 1,
        ConcreteOwner::Decorator { .. } => 2,
    };
    tag(left)
        .cmp(&tag(right))
        .then_with(|| match (left, right) {
            (ConcreteOwner::Function(left), ConcreteOwner::Function(right)) => left.cmp(right),
            (
                ConcreteOwner::Global {
                    module: left_module,
                    index: left_index,
                },
                ConcreteOwner::Global {
                    module: right_module,
                    index: right_index,
                },
            )
            | (
                ConcreteOwner::Decorator {
                    module: left_module,
                    index: left_index,
                },
                ConcreteOwner::Decorator {
                    module: right_module,
                    index: right_index,
                },
            ) => left_module
                .index()
                .cmp(&right_module.index())
                .then(left_index.cmp(right_index)),
            _ => Ordering::Equal,
        })
}

type PublishedSignatures = FxHashMap<Instance, Result<ConcreteSignatureHandle, MonoTyReason>>;
type PublishedDomains = BTreeMap<SiteDomain, Result<ConcreteTypeTables, MonoTyReason>>;

struct PublicationInputs<'g> {
    instances: Vec<&'g Instance>,
    domains: BTreeSet<SiteDomain>,
}

fn collect_publication_inputs(g: &InstanceGraph) -> PublicationInputs<'_> {
    let owners: Vec<(&Owner, &crate::instance_graph::Contribution)> =
        g.contributions.iter().map(|(o, c)| (o, &**c)).collect();
    #[cfg(not(feature = "sequential"))]
    let (instances, domains) = {
        use rayon::prelude::*;
        owners
            .into_par_iter()
            .fold(
                PublicationSets::default,
                |mut sets, (owner, contribution)| {
                    sets.add(owner, contribution);
                    sets
                },
            )
            .reduce(PublicationSets::default, PublicationSets::merge)
            .into_parts()
    };
    #[cfg(feature = "sequential")]
    let (instances, domains) = {
        let mut sets = PublicationSets::default();
        for (owner, contribution) in owners {
            sets.add(owner, contribution);
        }
        sets.into_parts()
    };
    PublicationInputs {
        instances: instances.into_iter().collect(),
        domains: domains.into_iter().cloned().collect(),
    }
}

#[derive(Default)]
struct PublicationSets<'g> {
    instances: FxHashSet<&'g Instance>,
    domains: FxHashSet<&'g SiteDomain>,
}

impl<'g> PublicationSets<'g> {
    fn add(&mut self, owner: &'g Owner, contribution: &'g crate::instance_graph::Contribution) {
        let mut domains: Vec<&'g SiteDomain> = Vec::new();
        if let Owner::Instance(i) = owner {
            if contribution.expanded {
                self.instances.insert(i);
                domains.extend(contribution.struct_default_domains.iter());
                for (site, record) in &contribution.call_sites {
                    if let SiteOutcome::Resolved(target) = &record.outcome {
                        self.instances.insert(target);
                        domains.push(&site.domain);
                    }
                }
            }
        }
        for record in contribution.always_call_sites.values() {
            if let SiteOutcome::Resolved(target) = &record.outcome {
                self.instances.insert(target);
            }
        }
        if matches!(owner, Owner::AlwaysEmitted(..)) {
            domains.extend(contribution.struct_default_domains.iter());
            domains.extend(
                contribution
                    .always_call_sites
                    .keys()
                    .map(|site| &site.domain),
            );
        }
        self.domains.extend(
            domains
                .into_iter()
                .filter(|domain| !matches!(domain, SiteDomain::Own)),
        );
    }

    #[cfg(not(feature = "sequential"))]
    fn merge(mut self, mut other: Self) -> Self {
        if self.instances.len() < other.instances.len() {
            std::mem::swap(&mut self.instances, &mut other.instances);
        }
        self.instances.extend(other.instances);
        self.domains.extend(other.domains);
        self
    }

    fn into_parts(self) -> (FxHashSet<&'g Instance>, FxHashSet<&'g SiteDomain>) {
        (self.instances, self.domains)
    }
}

fn publish_signatures(
    ws: &Workspace,
    g: &InstanceGraph,
    instances: Vec<&Instance>,
) -> (PublishedSignatures, std::time::Duration) {
    let started = bone_core::prelude::Instant::now();
    let published: PublishedSignatures = {
        use bone_core::prelude::*;
        instances
            .into_par_iter()
            .map(|instance| {
                let result = g
                    .substitution(instance)
                    .cloned()
                    .ok_or(MonoTyReason::NoNode)
                    .and_then(|subst| concrete_signature(ws, g, instance, &subst))
                    .map(std::sync::Arc::new);
                (instance.clone(), result)
            })
            .collect::<Vec<_>>()
            .into_iter()
            .collect()
    };
    let elapsed = started.elapsed();
    (published, elapsed)
}

struct Publication {
    signatures: PublishedSignatures,
    domains: PublishedDomains,
    stats: ConcreteBuildStats,
    signature_time: std::time::Duration,
    foreign_time: std::time::Duration,
}

fn publish(ws: &Workspace, g: &InstanceGraph, timing_enabled: bool) -> Publication {
    let started = bone_core::prelude::Instant::now();
    let PublicationInputs { instances, domains } = collect_publication_inputs(g);
    let (signatures, signature_cpu) = publish_signatures(ws, g, instances);
    let signature_time = started.elapsed();
    let mut stats = ConcreteBuildStats::default();
    let started = bone_core::prelude::Instant::now();
    let domains = publish_foreign_domains(ws, g, domains, &mut stats, timing_enabled);
    stats.signature_ns += signature_cpu.as_nanos() as u64;
    Publication {
        signatures,
        domains,
        stats,
        signature_time,
        foreign_time: started.elapsed(),
    }
}

fn publish_signatures_local(
    ws: &Workspace,
    g: &InstanceGraph,
    seeds: &BTreeSet<Instance>,
    always_owners: &BTreeSet<Owner>,
) -> PublishedSignatures {
    let mut instances = seeds.clone();
    for inst in seeds {
        if let Some(sites) = g.sites_of(inst) {
            for record in sites.values() {
                if let SiteOutcome::Resolved(target) = &record.outcome {
                    instances.insert(target.clone());
                }
            }
        }
    }
    for owner in always_owners {
        let Some(contribution) = g.contributions.get(owner) else {
            continue;
        };
        for record in contribution.always_call_sites.values() {
            if let SiteOutcome::Resolved(target) = &record.outcome {
                instances.insert(target.clone());
            }
        }
    }
    instances
        .into_iter()
        .map(|instance| {
            let result = g
                .substitution(&instance)
                .cloned()
                .ok_or(MonoTyReason::NoNode)
                .and_then(|subst| concrete_signature(ws, g, &instance, &subst))
                .map(Arc::new);
            (instance, result)
        })
        .collect()
}

fn collect_foreign_domains_local(
    g: &InstanceGraph,
    instances: &BTreeSet<Instance>,
    always_owners: &BTreeSet<Owner>,
) -> BTreeSet<SiteDomain> {
    let mut domains = BTreeSet::new();
    for inst in instances {
        if let Some(contribution) = g.contributions.get(&Owner::Instance(inst.clone())) {
            domains.extend(contribution.struct_default_domains.iter().cloned());
        }
        if let Some(sites) = g.sites_of(inst) {
            domains.extend(sites.keys().map(|site| site.domain.clone()));
        }
    }
    for owner in always_owners {
        let Some(contribution) = g.contributions.get(owner) else {
            continue;
        };
        domains.extend(contribution.struct_default_domains.iter().cloned());
        domains.extend(
            contribution
                .always_call_sites
                .keys()
                .map(|site| site.domain.clone()),
        );
    }
    domains
}

fn build_instance_delta(
    ws: &Workspace,
    g: &InstanceGraph,
    targets: &BTreeSet<Instance>,
) -> (Vec<Option<BuiltInstance>>, ConcreteBuildStats) {
    if targets.is_empty() {
        return (Vec::new(), ConcreteBuildStats::default());
    }
    let timing_enabled = log::log_enabled!(log::Level::Debug);
    let signatures = publish_signatures_local(ws, g, targets, &BTreeSet::new());
    let mut publication_stats = ConcreteBuildStats::default();
    let domains = collect_foreign_domains_local(g, targets, &BTreeSet::new());
    let published_domains =
        publish_foreign_domains(ws, g, domains, &mut publication_stats, timing_enabled);
    let results = targets
        .iter()
        .cloned()
        .map(|instance| {
            prepare_instance(ws, g, instance, timing_enabled).map(|prepared| {
                produce_instance(g, prepared, &signatures, &published_domains, timing_enabled)
            })
        })
        .collect();
    (results, publication_stats)
}

fn publish_foreign_domains(
    ws: &Workspace,
    g: &InstanceGraph,
    domains: impl IntoIterator<Item = SiteDomain>,
    stats: &mut ConcreteBuildStats,
    timing_enabled: bool,
) -> PublishedDomains {
    let started = timing_enabled.then(bone_core::prelude::Instant::now);
    let domains: Vec<SiteDomain> = domains
        .into_iter()
        .filter(|domain| !matches!(domain, SiteDomain::Own))
        .collect();
    let published: Vec<(
        SiteDomain,
        Result<ConcreteTypeTables, MonoTyReason>,
        ConcreteBuildStats,
    )> = {
        use bone_core::prelude::*;
        domains
            .into_par_iter()
            .map(|domain| {
                let mut domain_stats = ConcreteBuildStats::default();
                let result = match &domain {
                    SiteDomain::Own => unreachable!("own domains are filtered above"),
                    SiteDomain::StructDefaults(defaults) => {
                        let (module, def, args) = (&defaults.module, &defaults.def, &defaults.args);
                        let owner = BodyOwner::StructDefaults(*def);
                        ws.module(*module)
                            .bodies
                            .get(&owner)
                            .ok_or(MonoTyReason::NoNode)
                            .and_then(|defaults_body| {
                                let defaults_inf = ws
                                    .inference(*module)
                                    .and_then(|ir| ir.per_fn.get(&owner))
                                    .ok_or(MonoTyReason::NoNode)?;
                                let defaults_subst = skull_mir::struct_defaults::slot_subst(
                                    Some(defaults_inf),
                                    args,
                                );
                                concrete_tables(
                                    ws,
                                    g,
                                    defaults_inf,
                                    defaults_body,
                                    &defaults_subst,
                                    std::iter::empty(),
                                    &mut domain_stats,
                                    timing_enabled,
                                )
                            })
                    }
                };
                (domain, result, domain_stats)
            })
            .collect()
    };
    let mut out = BTreeMap::new();
    for (domain, result, domain_stats) in published {
        stats.add(domain_stats);
        out.insert(domain, result);
    }
    if let Some(started) = started {
        stats.foreign_domains_ns += started.elapsed().as_nanos() as u64;
    }
    out
}

struct PreparedInstance {
    instance: Instance,
    types: ConcreteTypeTables,
    type_error: Option<MonoTyReason>,
    stats: ConcreteBuildStats,
    convert_time: std::time::Duration,
    elapsed: std::time::Duration,
    slot_tyids: usize,
    unique_slot_tyids: usize,
}

fn prepare_instance(
    ws: &Workspace,
    g: &InstanceGraph,
    inst: Instance,
    timing_enabled: bool,
) -> Option<PreparedInstance> {
    let t_inst = bone_core::prelude::Instant::now();
    if g.has_bound_tree_disagreement(&inst) {
        return None;
    }
    let subst = g.substitution(&inst).cloned()?;
    let (inf, body) = ready_body(ws, inst.func)?;

    let mut stats = ConcreteBuildStats::default();
    let (slot_tyids, unique_slot_tyids) = if timing_enabled {
        let tyids: FxHashSet<TyId> = inf
            .expr_types
            .iter()
            .map(|(_, ty)| *ty)
            .chain(inf.binding_types.iter().map(|(_, ty)| *ty))
            .chain(inf.fn_field_call_types.values().copied())
            .collect();
        (
            inf.expr_types.iter().count()
                + inf.binding_types.iter().count()
                + inf.fn_field_call_types.len(),
            tyids.len(),
        )
    } else {
        (0, 0)
    };
    let started = bone_core::prelude::Instant::now();
    let converted = convert_body(g.interner(), ws, inf, body, &subst);
    let convert_time = started.elapsed();
    stats.source_expr_slots = converted.expr_examined;
    stats.source_binding_slots = converted.binding_examined;
    let mut types = converted.types;
    let mut memo = converted.type_memo;
    let f = &ws.module(inst.func.module).functions[inst.func.func];
    let declared_types = f
        .param_decls
        .iter()
        .map(|param| param.ty)
        .chain(f.return_ty);
    let type_error = match concrete_ty_types(
        ws,
        g,
        inf,
        body,
        &subst,
        declared_types,
        &mut memo,
        &mut stats,
        timing_enabled,
    ) {
        Ok(ty_types) => match concrete_struct_field_types(
            ws,
            g,
            body,
            &types.expr_types,
            &mut stats,
            timing_enabled,
        ) {
            Ok(struct_field_types) => {
                types.ty_types = ty_types;
                types.struct_field_types = struct_field_types;
                None
            }
            Err(reason) => Some(reason),
        },
        Err(reason) => Some(reason),
    };
    Some(PreparedInstance {
        instance: inst,
        types,
        type_error,
        stats,
        convert_time,
        elapsed: t_inst.elapsed(),
        slot_tyids,
        unique_slot_tyids,
    })
}

struct PreparedAlways {
    module: bone_core::prelude::ModuleId,
    body_owner: BodyOwner,
    types: Result<Option<ConcreteTypeTables>, MonoTyReason>,
    stats: ConcreteBuildStats,
}

fn concrete_owner_for(
    module: bone_core::prelude::ModuleId,
    body_owner: BodyOwner,
) -> Option<ConcreteOwner> {
    match body_owner {
        BodyOwner::Global(index) => Some(ConcreteOwner::Global {
            module,
            index: index.into_raw().into_u32(),
        }),
        BodyOwner::Decorator(index) => Some(ConcreteOwner::Decorator {
            module,
            index: index.into_raw().into_u32(),
        }),
        BodyOwner::Function(_)
        | BodyOwner::StructDefaults(_)
        | BodyOwner::Enum(_)
        | BodyOwner::Newtype(_) => None,
    }
}

fn prepare_always(
    ws: &Workspace,
    g: &InstanceGraph,
    module: bone_core::prelude::ModuleId,
    body_owner: BodyOwner,
    timing_enabled: bool,
) -> PreparedAlways {
    let mut stats = ConcreteBuildStats::default();
    let types = (|| {
        let module_data = ws.module(module);
        let body = module_data
            .bodies
            .get(&body_owner)
            .ok_or(MonoTyReason::NoNode)?;
        if matches!(body_owner, BodyOwner::Global(_)) && body.root.is_none() {
            return Ok(None);
        }
        let inf = ws
            .inference(module)
            .and_then(|ir| ir.per_fn.get(&body_owner))
            .ok_or(MonoTyReason::NoNode)?;
        let declared_type = match body_owner {
            BodyOwner::Global(index) => Some(module_data.globals[index].ty),
            _ => None,
        };
        concrete_tables(
            ws,
            g,
            inf,
            body,
            &MonoSubst::default(),
            declared_type,
            &mut stats,
            timing_enabled,
        )
        .map(Some)
    })();
    PreparedAlways {
        module,
        body_owner,
        types,
        stats,
    }
}

fn foreign_domains_from_publication<I>(
    domains: I,
    published: &PublishedDomains,
    stats: &mut ConcreteBuildStats,
    timing_enabled: bool,
) -> Result<BTreeMap<SiteDomain, ConcreteTypeTables>, MonoTyReason>
where
    I: IntoIterator<Item = SiteDomain>,
{
    let started = timing_enabled.then(bone_core::prelude::Instant::now);
    let mut out = BTreeMap::new();
    for domain in domains {
        stats.foreign_domain_requests += 1;
        let SiteDomain::StructDefaults(..) = domain else {
            continue;
        };
        if out.contains_key(&domain) {
            continue;
        }
        stats.foreign_domain_unique += 1;
        let tables = published
            .get(&domain)
            .ok_or(MonoTyReason::NoNode)?
            .clone()?;
        out.insert(domain, tables);
    }
    if let Some(started) = started {
        stats.foreign_domains_ns += started.elapsed().as_nanos() as u64;
    }
    Ok(out)
}

fn concrete_plan_from_sites(
    g: &InstanceGraph,
    inst: &Instance,
    types: ConcreteTypeTables,
    type_error: Option<MonoTyReason>,
    stats: &mut ConcreteBuildStats,
    signatures: &PublishedSignatures,
    domains: &PublishedDomains,
    timing_enabled: bool,
) -> Result<(ConcreteBodyPlan, FxHashSet<Instance>), MonoTyReason> {
    let mut callees = BTreeMap::new();
    let mut candidate_refs = FxHashSet::default();
    let mut failed_sites = Vec::new();
    let mut site_domains = Vec::new();
    let mut callee_signature_error: Option<(u8, Site, MonoTyReason)> = None;
    let signature_started = timing_enabled.then(bone_core::prelude::Instant::now);

    stats.callee_scans += 1;
    if let Some(sites) = g.sites_of(inst) {
        stats.callee_rows_visited += sites.len();
        for (site, record) in sites {
            match &record.outcome {
                SiteOutcome::Failed(reason) => {
                    failed_sites.push((site.clone(), *reason));
                }
                SiteOutcome::Resolved(target) => {
                    let callee_instance = target.clone();
                    candidate_refs.insert(target.clone());
                    match published_signature(signatures, target) {
                        Ok(signature) => {
                            callees.insert(
                                site.clone(),
                                ConcreteCallee {
                                    instance: callee_instance,
                                    signature,
                                },
                            );
                        }
                        Err(reason) => {
                            let rank = callee_table_rank(site);
                            if callee_signature_error.as_ref().is_none_or(
                                |(previous_rank, previous_site, _)| {
                                    (rank, site) < (*previous_rank, previous_site)
                                },
                            ) {
                                callee_signature_error = Some((rank, site.clone(), reason));
                            }
                        }
                    }
                    site_domains.push(site.domain.clone());
                }
            }
        }
    }

    let owner_signature = published_signature(signatures, inst);
    if let Some(started) = signature_started {
        stats.signature_ns += started.elapsed().as_nanos() as u64;
    }
    if let Some((_, _, reason)) = callee_signature_error {
        return Err(reason);
    }
    let signature = owner_signature?;
    if let Some(reason) = type_error {
        return Err(reason);
    }

    let default_domains = g
        .contributions
        .get(&Owner::Instance(inst.clone()))
        .into_iter()
        .flat_map(|contribution| contribution.struct_default_domains.iter().cloned());
    let foreign_domains = foreign_domains_from_publication(
        default_domains.chain(site_domains),
        domains,
        stats,
        timing_enabled,
    )?;
    stats.plan_callee_map_inserts += callees.len();
    stats.plan_callee_signature_inserts += callees.len();
    Ok((
        ConcreteBodyPlan {
            owner: ConcreteOwner::Function(inst.clone()),
            code_identity: Some(skull_mir::code_identity::CodeIdentity::Named(inst.clone())),
            types,
            foreign_domains,
            signature,
            callees,
            failed_sites: failed_sites.into_iter().collect(),
            bound_tree: g
                .bound_tree(inst)
                .map_or_else(Default::default, |tree| tree.to_resolved()),
        },
        candidate_refs,
    ))
}

fn published_signature(
    signatures: &PublishedSignatures,
    instance: &Instance,
) -> Result<ConcreteSignatureHandle, MonoTyReason> {
    signatures
        .get(instance)
        .cloned()
        .ok_or(MonoTyReason::NoNode)?
}

/// Preserve diagnostic precedence when several callee signatures fail.
fn callee_table_rank(site: &Site) -> u8 {
    match &site.op {
        SiteOp::DirectCall => 0,
        SiteOp::FnValue => 1,
        SiteOp::ForIter | SiteOp::ForNext(_) | SiteOp::ImplicitIter | SiteOp::BoundNext(_) => 2,
        SiteOp::Synthesized(_) | SiteOp::Dispatched { .. } => 3,
    }
}

fn produce_instance(
    g: &InstanceGraph,
    prepared: PreparedInstance,
    signatures: &PublishedSignatures,
    domains: &PublishedDomains,
    timing_enabled: bool,
) -> BuiltInstance {
    let started_instance = bone_core::prelude::Instant::now();
    let PreparedInstance {
        instance: inst,
        types,
        type_error,
        mut stats,
        convert_time,
        elapsed: preparation_time,
        slot_tyids,
        unique_slot_tyids,
    } = prepared;
    let callee_time = std::time::Duration::ZERO;
    let started = bone_core::prelude::Instant::now();
    stats.plan_attempts += 1;
    let plan_result = concrete_plan_from_sites(
        g,
        &inst,
        types,
        type_error,
        &mut stats,
        signatures,
        domains,
        timing_enabled,
    );
    let (plan, candidate_refs) = match plan_result {
        Ok((plan, candidate_refs)) => (Ok(plan), candidate_refs),
        Err(reason) => (Err(reason), FxHashSet::default()),
    };
    let plan_time = started.elapsed();
    if plan.is_err() {
        stats.plan_failures += 1;
    } else {
        stats.plan_successes += 1;
    }
    BuiltInstance {
        instance: inst,
        plan,
        candidate_refs,
        stats,
        convert_time,
        callee_time,
        plan_time,
        elapsed: preparation_time + started_instance.elapsed(),
        slot_tyids,
        unique_slot_tyids,
    }
}

struct BuiltAlways {
    owner: Option<ConcreteOwner>,
    plan: Result<Option<ConcreteBodyPlan>, MonoTyReason>,
    stats: ConcreteBuildStats,
}

fn produce_always(
    ws: &Workspace,
    g: &InstanceGraph,
    prepared: PreparedAlways,
    signatures: &PublishedSignatures,
    domains: &PublishedDomains,
    timing_enabled: bool,
) -> BuiltAlways {
    let PreparedAlways {
        module,
        body_owner,
        types,
        mut stats,
    } = prepared;
    let owner = concrete_owner_for(module, body_owner);
    stats.always_plan_attempts += 1;
    let plan = (|| {
        let Some(types) = types? else {
            return Ok(None);
        };
        let contribution = g
            .contributions
            .get(&Owner::AlwaysEmitted(module, body_owner))
            .ok_or(MonoTyReason::NoNode)?;
        let mut callees = BTreeMap::new();
        let mut failed_sites = BTreeMap::new();
        let signature_started = timing_enabled.then(bone_core::prelude::Instant::now);
        for (site, record) in &contribution.always_call_sites {
            match &record.outcome {
                SiteOutcome::Resolved(instance) => {
                    callees.insert(
                        site.clone(),
                        ConcreteCallee {
                            instance: instance.clone(),
                            signature: published_signature(signatures, instance)?,
                        },
                    );
                }
                SiteOutcome::Failed(reason) => {
                    failed_sites.insert(site.clone(), *reason);
                }
            }
        }
        let default_domains = contribution.struct_default_domains.iter().cloned();
        let site_domains = contribution
            .always_call_sites
            .keys()
            .map(|site| site.domain.clone());
        let foreign_domains = foreign_domains_from_publication(
            default_domains.chain(site_domains),
            domains,
            &mut stats,
            timing_enabled,
        )?;
        let module_data = ws.module(module);
        let (params, result) = match body_owner {
            BodyOwner::Global(gid) => {
                let ty = module_data.globals[gid].ty;
                let result = types
                    .ty_types
                    .get(&ty)
                    .copied()
                    .ok_or(MonoTyReason::NoNode)?;
                (Vec::new(), Some(result))
            }
            BodyOwner::Decorator(_) => {
                let body = module_data
                    .bodies
                    .get(&body_owner)
                    .ok_or(MonoTyReason::NoNode)?;
                let params = body
                    .deco_target
                    .into_iter()
                    .chain(body.param_bindings)
                    .map(|binding| {
                        types
                            .binding_types
                            .get(binding)
                            .copied()
                            .ok_or(MonoTyReason::NoNode)
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                (params, None)
            }
            _ => return Ok(None),
        };
        let signature = Arc::new(ConcreteSignature {
            param_tuple_spelled: vec![false; params.len()],
            params,
            result,
        });
        if let Some(started) = signature_started {
            stats.signature_ns += started.elapsed().as_nanos() as u64;
        }
        stats.always_plan_successes += 1;
        Ok(Some(ConcreteBodyPlan {
            owner: owner
                .clone()
                .expect("always-emitted owner was not global or decorator"),
            code_identity: None,
            types,
            foreign_domains,
            signature,
            callees,
            failed_sites,
            bound_tree: Default::default(),
        }))
    })();
    BuiltAlways { owner, plan, stats }
}

fn concrete_map<T, U, F>(items: Vec<T>, f: F) -> Vec<U>
where
    T: Send,
    U: Send,
    F: Fn(T) -> U + Send + Sync,
{
    use bone_core::prelude::*;

    items.into_par_iter().map(f).collect()
}

fn produce_instances(
    g: &InstanceGraph,
    prepared: Vec<Option<PreparedInstance>>,
    signatures: &PublishedSignatures,
    domains: &PublishedDomains,
    timing_enabled: bool,
) -> Vec<Option<BuiltInstance>> {
    concrete_map(prepared, |prepared| {
        prepared.map(|instance| produce_instance(g, instance, signatures, domains, timing_enabled))
    })
}

/// Build a concrete lowering plan for every instance the walk expanded.
/// The tables intern into the graph's arena: a second arena would hand out ids
/// that mean nothing beside `Instance::args`.
pub fn build(ws: &Workspace, g: &InstanceGraph) -> ConcreteBodies {
    let t_build = bone_core::prelude::Instant::now();
    let intern_before = g.interner().stats();
    let timing_enabled = log::log_enabled!(log::Level::Debug);
    let t_expansion = bone_core::prelude::Instant::now();
    let expanded: Vec<Instance> = g.expanded().cloned().collect();
    let mut unique_decls: FxHashSet<FuncRef> = FxHashSet::default();
    for instance in &expanded {
        unique_decls.insert(instance.func);
    }
    let expansion_time = t_expansion.elapsed();

    let t_preparation = bone_core::prelude::Instant::now();
    // Both collects keep input order, so the published tables do not depend on the schedule.
    let always_owners = g
        .contributions
        .keys()
        .filter_map(|owner| match owner {
            Owner::AlwaysEmitted(module, body_owner) => Some((*module, *body_owner)),
            Owner::Instance(_) => None,
        })
        .collect::<Vec<_>>();
    let (publication, (prepared, always_inputs)) = bone_core::prelude::join_in_place(
        || publish(ws, g, timing_enabled),
        || {
            use bone_core::prelude::*;
            let prepared = expanded
                .into_par_iter()
                .map(|instance| prepare_instance(ws, g, instance, timing_enabled))
                .collect::<Vec<_>>();
            let always_inputs = always_owners
                .into_par_iter()
                .map(|(module, body_owner)| {
                    prepare_always(ws, g, module, body_owner, timing_enabled)
                })
                .collect::<Vec<_>>();
            (prepared, always_inputs)
        },
    );
    let preparation_time = t_preparation.elapsed();
    let Publication {
        signatures,
        domains: published_domains,
        stats: publication_stats,
        signature_time: signature_publication_time,
        foreign_time: foreign_publication_time,
    } = publication;

    let t_partition = bone_core::prelude::Instant::now();
    let signatures = std::sync::Arc::new(signatures);
    let published_domains = std::sync::Arc::new(published_domains);
    let partition_time = t_partition.elapsed();
    let t_ordinary_parallel = bone_core::prelude::Instant::now();
    let mut results = produce_instances(
        g,
        prepared,
        signatures.as_ref(),
        published_domains.as_ref(),
        timing_enabled,
    );
    let ordinary_parallel_time = t_ordinary_parallel.elapsed();
    results.sort_by(|left, right| {
        left.as_ref()
            .map(|b| &b.instance)
            .cmp(&right.as_ref().map(|b| &b.instance))
    });

    let t_ordinary_merge = bone_core::prelude::Instant::now();
    let mut out = ConcreteBodies {
        global_build_stats: publication_stats,
        ..ConcreteBodies::default()
    };
    out.instance_stats.reserve(results.len());
    let mut candidate_ref_rows = 0usize;
    let mut owner_candidates: FxHashMap<Instance, FxHashSet<Instance>> = FxHashMap::default();
    owner_candidates.reserve(results.len());
    for built in results.iter_mut().flatten() {
        candidate_ref_rows += built.candidate_refs.len();
        if built.plan.is_ok() {
            owner_candidates.insert(
                built.instance.clone(),
                std::mem::take(&mut built.candidate_refs),
            );
        }
    }
    let collect_candidate_keys = || {
        let started = bone_core::prelude::Instant::now();
        let mut keys: Vec<&Instance> = owner_candidates
            .values()
            .flatten()
            .collect::<FxHashSet<_>>()
            .into_iter()
            .collect();
        keys.sort();
        let keys: BTreeSet<Instance> = keys.into_iter().cloned().collect();
        (keys, started.elapsed())
    };
    let mut plan_rows: Vec<(Instance, Arc<skull_mir::concrete::ConcreteBodyPlan>)> = Vec::new();
    let mut per_inst = Vec::new();
    let mut t_convert = std::time::Duration::ZERO;
    let mut t_callee = std::time::Duration::ZERO;
    let mut t_plan = std::time::Duration::ZERO;
    let mut plan_insert_time = std::time::Duration::ZERO;
    let mut failure_index_time = std::time::Duration::ZERO;
    let mut owner_stats_insert_time = std::time::Duration::ZERO;
    let mut slot_tyids = 0usize;
    let mut unique_slot_tyids = 0usize;
    let ((candidate_keys, candidate_install_time), ()) =
        bone_core::prelude::join_in_place(collect_candidate_keys, || {
            for built in results.into_iter().flatten() {
                let instance = built.instance.clone();
                t_convert += built.convert_time;
                t_callee += built.callee_time;
                t_plan += built.plan_time;
                per_inst.push((instance.clone(), built.elapsed));
                slot_tyids += built.slot_tyids;
                unique_slot_tyids += built.unique_slot_tyids;
                let stats = built.stats;
                let started = timing_enabled.then(bone_core::prelude::Instant::now);
                if let Err(reason) = &built.plan {
                    out.plan_failure_index
                        .insert(instance.clone(), reason.clone());
                }
                if let Some(started) = started {
                    failure_index_time += started.elapsed();
                }
                if let Ok(plan) = built.plan {
                    let started = timing_enabled.then(bone_core::prelude::Instant::now);
                    plan_rows.push((instance.clone(), Arc::new(plan)));
                    if let Some(started) = started {
                        plan_insert_time += started.elapsed();
                    }
                }
                let started = timing_enabled.then(bone_core::prelude::Instant::now);
                out.instance_stats.insert(instance, stats);
                if let Some(started) = started {
                    owner_stats_insert_time += started.elapsed();
                }
            }
        });
    out.candidate_keys = candidate_keys;
    out.owner_candidates = owner_candidates;
    let started = timing_enabled.then(bone_core::prelude::Instant::now);
    let plans: skull_mir::concrete::ConcretePlans = plan_rows.into_iter().collect();
    if let Some(started) = started {
        plan_insert_time += started.elapsed();
    }
    let ordinary_merge_time = t_ordinary_merge.elapsed();

    let t_always_parallel = bone_core::prelude::Instant::now();
    let always_results = concrete_map(always_inputs, |prepared| {
        produce_always(
            ws,
            g,
            prepared,
            signatures.as_ref(),
            published_domains.as_ref(),
            timing_enabled,
        )
    });
    let always_parallel_time = t_always_parallel.elapsed();
    let t_always_merge = bone_core::prelude::Instant::now();
    for result in always_results {
        out.install_always(result);
    }
    let always_merge_time = t_always_merge.elapsed();
    out.plans = std::sync::Arc::new(plans);
    if log::log_enabled!(log::Level::Debug) {
        let intern_after = g.interner().stats();
        let total_time = t_build.elapsed();
        // The publication runs inside the preparation window.
        let accounted = expansion_time
            + preparation_time
            + partition_time
            + ordinary_parallel_time
            + ordinary_merge_time
            + always_parallel_time
            + always_merge_time;
        let orchestration_time = total_time.saturating_sub(accounted);
        per_inst.sort_by_key(|(_, duration)| *duration);
        let pct = |p: usize| {
            per_inst
                .get((per_inst.len().saturating_sub(1)) * p / 100)
                .map_or(std::time::Duration::ZERO, |(_, duration)| *duration)
        };
        let totals = out.snapshot_build_stats();
        let top: Vec<String> = per_inst
            .iter()
            .rev()
            .take(3)
            .map(|(inst, duration)| {
                format!(
                    "{}={:?}",
                    ws.module(inst.func.module).functions[inst.func.func].name,
                    duration
                )
            })
            .collect();
        log::debug!(
            "concrete build: {} expanded over {} decls, {} plans; \
             slots {} expr + {} binding; convert {:?}, callee tables {:?}, plans {:?}, \
             total {:?}; phases expansion {:?}, preparation {:?}, signature publication {:?}, \
             foreign publication {:?}, dispatch {:?}, ordinary fanout {:?}, ordinary merge {:?}, \
             always fanout {:?}, always merge {:?}, orchestration {:?}; \
             per-instance p50 {:?} p95 {:?} max {:?}, top [{}]; \
             slot tyids {} / {} unique per-body; mono interns {} attempts -> {} new during build; \
             type nodes {}/{} ({} memo hits, {} unresolved, {:?}), struct literals {} fields {} ({:?}), \
             foreign domains {}/{} ({:?}), signatures ({:?})",
            per_inst.len(),
            unique_decls.len(),
            out.plans.len(),
            totals.source_expr_slots,
            totals.source_binding_slots,
            t_convert,
            t_callee,
            t_plan,
            total_time,
            expansion_time,
            preparation_time,
            signature_publication_time,
            foreign_publication_time,
            partition_time,
            ordinary_parallel_time,
            ordinary_merge_time,
            always_parallel_time,
            always_merge_time,
            orchestration_time,
            pct(50),
            pct(95),
            pct(100),
            top.join(", "),
            slot_tyids,
            unique_slot_tyids,
            intern_after.intern_attempts - intern_before.intern_attempts,
            intern_after.intern_new - intern_before.intern_new,
            totals.type_nodes_examined,
            totals.type_node_conversion_attempts,
            totals.type_node_memo_hits,
            totals.type_node_unresolved,
            std::time::Duration::from_nanos(totals.type_nodes_ns),
            totals.struct_literals_examined,
            totals.struct_field_conversion_attempts,
            std::time::Duration::from_nanos(totals.struct_fields_ns),
            totals.foreign_domain_requests,
            totals.foreign_domain_unique,
            std::time::Duration::from_nanos(totals.foreign_domains_ns),
            std::time::Duration::from_nanos(totals.signature_ns),
        );
        log::debug!(
            "concrete merge detail: candidate refs {} rows {:?}, plans {:?}, \
             failures {:?}, owner stats {:?}",
            candidate_ref_rows,
            candidate_install_time,
            plan_insert_time,
            failure_index_time,
            owner_stats_insert_time,
        );
    }
    out
}
