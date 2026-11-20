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

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConcreteUpdateStats {
    pub candidate_keys_added: BTreeSet<Instance>,
    pub candidate_keys_removed: BTreeSet<Instance>,
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
        let results = build_instance_delta(ws, g, &targets);
        for built in results.into_iter().flatten() {
            let instance = built.instance.clone();
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
                    .insert(instance, Arc::new(plan));
            }
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
            self.always_plan_failure_index.remove(owner);
            self.always_plans.remove(owner);
        }
        let always_inputs: Vec<_> = always_subjects
            .iter()
            .map(|(module, body_owner, _)| prepare_always(ws, g, *module, *body_owner))
            .collect();
        if !always_inputs.is_empty() {
            let always_owners: BTreeSet<Owner> = always_inputs
                .iter()
                .map(|prepared| Owner::AlwaysEmitted(prepared.module, prepared.body_owner))
                .collect();
            let signatures = publish_signatures_local(ws, g, &targets, &always_owners);
            let domains = collect_foreign_domains_local(g, &targets, &always_owners);
            let published_domains = publish_foreign_domains(ws, g, domains);
            for prepared in always_inputs {
                self.install_always(produce_always(
                    ws,
                    g,
                    prepared,
                    &signatures,
                    &published_domains,
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

/// For graph-side accounting, which runs before `ConcreteBodies::build`
/// installs per-body tables.
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
    )?;
    let struct_field_types = concrete_struct_field_types(ws, g, body, &converted.types.expr_types)?;
    converted.types.ty_types = ty_types;
    converted.types.struct_field_types = struct_field_types;
    Ok(converted.types)
}

fn concrete_struct_field_types(
    ws: &Workspace,
    g: &InstanceGraph,
    body: &Body,
    expr_types: &ArenaMap<Idx<Expr>, MonoTyId>,
) -> Result<ArenaMap<Idx<Expr>, FxHashMap<Ustr, MonoTyId>>, MonoTyReason> {
    let mut out = ArenaMap::default();
    for (expr, node) in body.expr_arena.iter() {
        let Expr::StructLiteral { name, .. } = node else {
            continue;
        };
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
        let fields = def
            .fields
            .iter()
            .map(|field| {
                arena
                    .convert_decl(ws, field.ty, &subst)
                    .map(|ty| (field.name, ty))
            })
            .collect::<Result<FxHashMap<_, _>, _>>()?;
        out.insert(expr, fields);
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
    let arena = g.interner();
    nodes
        .into_iter()
        .filter_map(|ty| {
            let converted = memo
                .entry(ty)
                .or_insert_with(|| arena.convert(ws, inf, ty, subst))
                .clone();
            match converted {
                Ok(id) => Some(Ok((ty, id))),
                Err(MonoTyReason::UnresolvedInfer(_)) => None,
                Err(reason) => Some(Err(reason)),
            }
        })
        .collect()
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
) -> PublishedSignatures {
    use bone_core::prelude::*;
    // Keyed by instance, so the parallel derivation order is not observable.
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
}

fn publish(ws: &Workspace, g: &InstanceGraph) -> (PublishedSignatures, PublishedDomains) {
    let PublicationInputs { instances, domains } = collect_publication_inputs(g);
    let signatures = publish_signatures(ws, g, instances);
    (signatures, publish_foreign_domains(ws, g, domains))
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
) -> Vec<Option<BuiltInstance>> {
    if targets.is_empty() {
        return Vec::new();
    }
    let signatures = publish_signatures_local(ws, g, targets, &BTreeSet::new());
    let domains = collect_foreign_domains_local(g, targets, &BTreeSet::new());
    let published_domains = publish_foreign_domains(ws, g, domains);
    targets
        .iter()
        .cloned()
        .map(|instance| {
            prepare_instance(ws, g, instance)
                .map(|prepared| produce_instance(g, prepared, &signatures, &published_domains))
        })
        .collect()
}

fn publish_foreign_domains(
    ws: &Workspace,
    g: &InstanceGraph,
    domains: impl IntoIterator<Item = SiteDomain>,
) -> PublishedDomains {
    let domains: Vec<SiteDomain> = domains
        .into_iter()
        .filter(|domain| !matches!(domain, SiteDomain::Own))
        .collect();
    // Keyed by domain, so the parallel conversion order is not observable.
    let published: Vec<(SiteDomain, Result<ConcreteTypeTables, MonoTyReason>)> = {
        use bone_core::prelude::*;
        domains
            .into_par_iter()
            .map(|domain| {
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
                                )
                            })
                    }
                };
                (domain, result)
            })
            .collect()
    };
    published.into_iter().collect()
}

struct PreparedInstance {
    instance: Instance,
    types: ConcreteTypeTables,
    type_error: Option<MonoTyReason>,
}

fn prepare_instance(ws: &Workspace, g: &InstanceGraph, inst: Instance) -> Option<PreparedInstance> {
    if g.has_bound_tree_disagreement(&inst) {
        return None;
    }
    let subst = g.substitution(&inst).cloned()?;
    let (inf, body) = ready_body(ws, inst.func)?;

    let converted = convert_body(g.interner(), ws, inf, body, &subst);
    let mut types = converted.types;
    let mut memo = converted.type_memo;
    let f = &ws.module(inst.func.module).functions[inst.func.func];
    let declared_types = f
        .param_decls
        .iter()
        .map(|param| param.ty)
        .chain(f.return_ty);
    let type_error = match concrete_ty_types(ws, g, inf, body, &subst, declared_types, &mut memo) {
        Ok(ty_types) => match concrete_struct_field_types(ws, g, body, &types.expr_types) {
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
    })
}

struct PreparedAlways {
    module: bone_core::prelude::ModuleId,
    body_owner: BodyOwner,
    types: Result<Option<ConcreteTypeTables>, MonoTyReason>,
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
) -> PreparedAlways {
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
        concrete_tables(ws, g, inf, body, &MonoSubst::default(), declared_type).map(Some)
    })();
    PreparedAlways {
        module,
        body_owner,
        types,
    }
}

fn foreign_domains_from_publication<I>(
    domains: I,
    published: &PublishedDomains,
) -> Result<BTreeMap<SiteDomain, ConcreteTypeTables>, MonoTyReason>
where
    I: IntoIterator<Item = SiteDomain>,
{
    let mut out = BTreeMap::new();
    for domain in domains {
        let SiteDomain::StructDefaults(..) = domain else {
            continue;
        };
        if out.contains_key(&domain) {
            continue;
        }
        let tables = published
            .get(&domain)
            .ok_or(MonoTyReason::NoNode)?
            .clone()?;
        out.insert(domain, tables);
    }
    Ok(out)
}

fn concrete_plan_from_sites(
    g: &InstanceGraph,
    inst: &Instance,
    types: ConcreteTypeTables,
    type_error: Option<MonoTyReason>,
    signatures: &PublishedSignatures,
    domains: &PublishedDomains,
) -> Result<(ConcreteBodyPlan, FxHashSet<Instance>), MonoTyReason> {
    let mut callees = BTreeMap::new();
    let mut candidate_refs = FxHashSet::default();
    let mut failed_sites = Vec::new();
    let mut site_domains = Vec::new();
    let mut callee_signature_error: Option<(u8, Site, MonoTyReason)> = None;
    if let Some(sites) = g.sites_of(inst) {
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
    let foreign_domains =
        foreign_domains_from_publication(default_domains.chain(site_domains), domains)?;
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
) -> BuiltInstance {
    let PreparedInstance {
        instance: inst,
        types,
        type_error,
    } = prepared;
    let (plan, candidate_refs) =
        match concrete_plan_from_sites(g, &inst, types, type_error, signatures, domains) {
            Ok((plan, candidate_refs)) => (Ok(plan), candidate_refs),
            Err(reason) => (Err(reason), FxHashSet::default()),
        };
    BuiltInstance {
        instance: inst,
        plan,
        candidate_refs,
    }
}

struct BuiltAlways {
    owner: Option<ConcreteOwner>,
    plan: Result<Option<ConcreteBodyPlan>, MonoTyReason>,
}

fn produce_always(
    ws: &Workspace,
    g: &InstanceGraph,
    prepared: PreparedAlways,
    signatures: &PublishedSignatures,
    domains: &PublishedDomains,
) -> BuiltAlways {
    let PreparedAlways {
        module,
        body_owner,
        types,
    } = prepared;
    let owner = concrete_owner_for(module, body_owner);
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
        let foreign_domains =
            foreign_domains_from_publication(default_domains.chain(site_domains), domains)?;
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
    BuiltAlways { owner, plan }
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
) -> Vec<Option<BuiltInstance>> {
    concrete_map(prepared, |prepared| {
        prepared.map(|instance| produce_instance(g, instance, signatures, domains))
    })
}

/// Build a concrete lowering plan for every instance the walk expanded.
/// The tables intern into the graph's arena: a second arena would hand out ids
/// that mean nothing beside `Instance::args`.
pub fn build(ws: &Workspace, g: &InstanceGraph) -> ConcreteBodies {
    let expanded: Vec<Instance> = g.expanded().cloned().collect();
    // Both collects keep input order, so the published tables do not depend on the schedule.
    let always_owners = g
        .contributions
        .keys()
        .filter_map(|owner| match owner {
            Owner::AlwaysEmitted(module, body_owner) => Some((*module, *body_owner)),
            Owner::Instance(_) => None,
        })
        .collect::<Vec<_>>();
    let ((signatures, published_domains), (prepared, always_inputs)) =
        bone_core::prelude::join_in_place(
            || publish(ws, g),
            || {
                use bone_core::prelude::*;
                let prepared = expanded
                    .into_par_iter()
                    .map(|instance| prepare_instance(ws, g, instance))
                    .collect::<Vec<_>>();
                let always_inputs = always_owners
                    .into_par_iter()
                    .map(|(module, body_owner)| prepare_always(ws, g, module, body_owner))
                    .collect::<Vec<_>>();
                (prepared, always_inputs)
            },
        );

    let mut results = produce_instances(g, prepared, &signatures, &published_domains);
    results.sort_by(|left, right| {
        left.as_ref()
            .map(|b| &b.instance)
            .cmp(&right.as_ref().map(|b| &b.instance))
    });

    let mut out = ConcreteBodies::default();
    let mut owner_candidates: FxHashMap<Instance, FxHashSet<Instance>> = FxHashMap::default();
    owner_candidates.reserve(results.len());
    for built in results.iter_mut().flatten() {
        if built.plan.is_ok() {
            owner_candidates.insert(
                built.instance.clone(),
                std::mem::take(&mut built.candidate_refs),
            );
        }
    }
    let collect_candidate_keys = || {
        let mut keys: Vec<&Instance> = owner_candidates
            .values()
            .flatten()
            .collect::<FxHashSet<_>>()
            .into_iter()
            .collect();
        keys.sort();
        keys.into_iter().cloned().collect::<BTreeSet<Instance>>()
    };
    let mut plan_rows: Vec<(Instance, Arc<skull_mir::concrete::ConcreteBodyPlan>)> = Vec::new();
    let (candidate_keys, ()) = bone_core::prelude::join_in_place(collect_candidate_keys, || {
        for built in results.into_iter().flatten() {
            match built.plan {
                Ok(plan) => plan_rows.push((built.instance, Arc::new(plan))),
                Err(reason) => {
                    out.plan_failure_index.insert(built.instance, reason);
                }
            }
        }
    });
    out.candidate_keys = candidate_keys;
    out.owner_candidates = owner_candidates;
    let plans: skull_mir::concrete::ConcretePlans = plan_rows.into_iter().collect();

    let always_results = concrete_map(always_inputs, |prepared| {
        produce_always(ws, g, prepared, &signatures, &published_domains)
    });
    for result in always_results {
        out.install_always(result);
    }
    out.plans = Arc::new(plans);
    out
}
