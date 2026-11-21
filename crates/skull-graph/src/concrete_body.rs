use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use bone_core::hir::{BodyOwner, TypeExpr};
use bone_core::prelude::TyId;
use bone_core::workspace::Workspace;
use rustc_hash::FxHashMap;
use skull_mir::concrete::{
    ConcreteAlwaysPlans, ConcreteBodyPlan, ConcreteCallee, ConcreteOwner, ConcretePlans,
    ConcreteSignature, ConcreteSignatureHandle, ConcreteTypeTables,
};
use skull_mir::mir::FuncRef;
use skull_mir::monoty::{MonoSubst, MonoTyReason, TyEnv};

use crate::instance_graph::{
    ready_body, Instance, InstanceGraph, Owner, Site, SiteDomain, SiteOp, SiteOutcome,
};

#[derive(Clone, Default)]
pub struct ConcreteBodies {
    pub plans: Arc<ConcretePlans>,
    pub always_plans: ConcreteAlwaysPlans,
    plan_failure_index: BTreeMap<Instance, MonoTyReason>,
    always_plan_failure_index: FxHashMap<ConcreteOwner, MonoTyReason>,
    // Count every callee site of a successful function plan.
    candidate_counts: BTreeMap<Instance, usize>,
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

    pub fn candidate_keys(&self) -> impl Iterator<Item = &Instance> {
        self.candidate_counts.keys()
    }

    pub fn candidate_keys_for(&self, func: FuncRef) -> impl Iterator<Item = &Instance> {
        self.candidate_counts
            .range(Instance::base(func)..)
            .map(|(instance, _)| instance)
            .take_while(move |instance| instance.func == func)
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
        let mut targets = instance_subjects;
        targets.retain(|instance| g.instances.contains(instance));
        let always_subjects: Vec<_> = revisited_owners
            .iter()
            .filter_map(|owner| match owner {
                Owner::AlwaysEmitted(module, body) => {
                    Some((*module, *body, concrete_owner_for(*module, *body)?))
                }
                Owner::Instance(_) => None,
            })
            .collect();
        let publication = Publication::new(ws, g, 1);
        for instance in targets {
            let Some(built) = plan_instance(&publication, instance) else {
                continue;
            };
            match built.plan {
                Err(reason) => {
                    self.plan_failure_index.insert(built.instance, reason);
                }
                Ok(plan) => {
                    for callee in plan.callees.values() {
                        let instance = &callee.instance;
                        let count = self.candidate_counts.entry(instance.clone()).or_default();
                        if *count == 0 {
                            candidate_keys_added.insert(instance.clone());
                        }
                        *count += 1;
                    }
                    Arc::get_mut(&mut self.plans)
                        .expect("local concrete update lost its unique plan store")
                        .insert(built.instance, Arc::new(plan));
                }
            }
        }
        for (module, body, owner) in always_subjects {
            self.always_plan_failure_index.remove(&owner);
            self.always_plans.remove(&owner);
            self.install_always(plan_always(&publication, module, body));
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
        self.plan_failure_index.remove(instance);
        if let Some(plan) = Arc::get_mut(&mut self.plans)
            .expect("local concrete update lost its unique plan store")
            .remove(instance)
        {
            for callee in plan.callees.values() {
                let instance = &callee.instance;
                let count = self
                    .candidate_counts
                    .get_mut(instance)
                    .expect("published callee");
                *count -= 1;
                if *count == 0 {
                    self.candidate_counts.remove(instance);
                    removed.insert(instance.clone());
                }
            }
        }
        removed
    }
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

struct BuiltInstance {
    instance: Instance,
    plan: Result<ConcreteBodyPlan, MonoTyReason>,
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

type Rows<K, V> = parking_lot::RwLock<FxHashMap<K, Result<V, MonoTyReason>>>;

struct Memo<K, V>(Vec<Rows<K, V>>);

impl<K: Clone + Eq + std::hash::Hash, V: Clone> Memo<K, V> {
    fn new(shards: usize) -> Self {
        debug_assert!(shards.is_power_of_two());
        Self((0..shards).map(|_| Rows::default()).collect())
    }

    fn get(
        &self,
        key: &K,
        build: impl FnOnce() -> Result<V, MonoTyReason>,
    ) -> Result<V, MonoTyReason> {
        use std::hash::BuildHasher;
        let hash = rustc_hash::FxBuildHasher.hash_one(key);
        let index = (hash >> 32) as usize & (self.0.len() - 1);
        let rows = &self.0[index];
        if let Some(row) = rows.read().get(key) {
            return row.clone();
        }
        rows.write()
            .entry(key.clone())
            .or_insert_with(build)
            .clone()
    }
}

struct Publication<'a> {
    ws: &'a Workspace,
    graph: &'a InstanceGraph,
    signatures: Memo<Instance, ConcreteSignatureHandle>,
    domains: Memo<SiteDomain, ConcreteTypeTables>,
}

impl<'a> Publication<'a> {
    fn new(ws: &'a Workspace, graph: &'a InstanceGraph, shards: usize) -> Self {
        Self {
            ws,
            graph,
            signatures: Memo::new(shards),
            domains: Memo::new(shards),
        }
    }

    fn signature(&self, instance: &Instance) -> Result<ConcreteSignatureHandle, MonoTyReason> {
        self.signatures.get(instance, || {
            concrete_signature_for_instance(self.ws, self.graph, instance).map(Arc::new)
        })
    }

    fn foreign_domains(
        &self,
        domains: impl IntoIterator<Item = SiteDomain>,
    ) -> Result<BTreeMap<SiteDomain, ConcreteTypeTables>, MonoTyReason> {
        let mut out = BTreeMap::new();
        for domain in domains {
            let SiteDomain::StructDefaults(defaults) = &domain else {
                continue;
            };
            if out.contains_key(&domain) {
                continue;
            }
            let (module, def, args) = (defaults.module, defaults.def, &defaults.args);
            let tables = self.domains.get(&domain, || {
                let owner = BodyOwner::StructDefaults(def);
                let body = self
                    .ws
                    .module(module)
                    .bodies
                    .get(&owner)
                    .ok_or(MonoTyReason::NoNode)?;
                let inf = self
                    .ws
                    .inference(module)
                    .and_then(|ir| ir.per_fn.get(&owner))
                    .ok_or(MonoTyReason::NoNode)?;
                let subst = skull_mir::struct_defaults::slot_subst(Some(inf), args);
                ConcreteTypeTables::publish(
                    self.ws,
                    self.graph.interner(),
                    (module, owner),
                    (inf, body),
                    &subst,
                    std::iter::empty(),
                )
            })?;
            out.insert(domain, tables);
        }
        Ok(out)
    }
}

fn plan_instance(publication: &Publication<'_>, inst: Instance) -> Option<BuiltInstance> {
    let (ws, g) = (publication.ws, publication.graph);
    if g.has_bound_tree_disagreement(&inst) {
        return None;
    }
    let subst = g.substitution(&inst).cloned()?;
    let (inf, body) = ready_body(ws, inst.func)?;

    let f = &ws.module(inst.func.module).functions[inst.func.func];
    let declared_types = f
        .param_decls
        .iter()
        .map(|param| param.ty)
        .chain(f.return_ty);
    let types = ConcreteTypeTables::publish(
        ws,
        g.interner(),
        (inst.func.module, BodyOwner::Function(inst.func.func)),
        (inf, body),
        &subst,
        declared_types,
    );
    Some(BuiltInstance {
        plan: concrete_plan_from_sites(publication, &inst, types),
        instance: inst,
    })
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

fn always_types(
    ws: &Workspace,
    g: &InstanceGraph,
    module: bone_core::prelude::ModuleId,
    body_owner: BodyOwner,
) -> Result<Option<ConcreteTypeTables>, MonoTyReason> {
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
    let (global_ty, params) = match body_owner {
        BodyOwner::Global(index) => (Some(module_data.globals[index].ty), &[][..]),
        BodyOwner::Decorator(index) => (None, &module_data.decorators[index].param_decls[..]),
        _ => (None, &[][..]),
    };
    ConcreteTypeTables::publish(
        ws,
        g.interner(),
        (module, body_owner),
        (inf, body),
        &MonoSubst::default(),
        global_ty
            .into_iter()
            .chain(params.iter().map(|param| param.ty)),
    )
    .map(Some)
}

fn concrete_plan_from_sites(
    publication: &Publication<'_>,
    inst: &Instance,
    types: Result<ConcreteTypeTables, MonoTyReason>,
) -> Result<ConcreteBodyPlan, MonoTyReason> {
    let mut callees = BTreeMap::new();
    let mut failed_sites = Vec::new();
    let mut site_domains = Vec::new();
    let mut callee_signature_error: Option<(u8, Site, MonoTyReason)> = None;
    let g = publication.graph;
    if let Some(sites) = g.sites_of(inst) {
        for (site, record) in sites {
            match &record.outcome {
                SiteOutcome::Failed(reason) => {
                    failed_sites.push((site.clone(), *reason));
                }
                SiteOutcome::Resolved(target) => {
                    let callee_instance = target.clone();
                    match publication.signature(target) {
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

    let owner_signature = publication.signature(inst);
    if let Some((_, _, reason)) = callee_signature_error {
        return Err(reason);
    }
    let signature = owner_signature?;
    let types = types?;

    let default_domains = g
        .contributions
        .get(&Owner::Instance(inst.clone()))
        .into_iter()
        .flat_map(|contribution| contribution.struct_default_domains.iter().cloned());
    let foreign_domains = publication.foreign_domains(default_domains.chain(site_domains))?;
    Ok(ConcreteBodyPlan {
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
    })
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

struct BuiltAlways {
    owner: Option<ConcreteOwner>,
    plan: Result<Option<ConcreteBodyPlan>, MonoTyReason>,
}

fn plan_always(
    publication: &Publication<'_>,
    module: bone_core::prelude::ModuleId,
    body_owner: BodyOwner,
) -> BuiltAlways {
    let owner = concrete_owner_for(module, body_owner);
    let plan = (|| {
        let Some(types) = always_types(publication.ws, publication.graph, module, body_owner)?
        else {
            return Ok(None);
        };
        let contribution = publication
            .graph
            .contributions
            .get(&Owner::AlwaysEmitted(module, body_owner))
            .ok_or(MonoTyReason::NoNode)?;
        let mut callees = BTreeMap::new();
        let mut failed_sites = BTreeMap::new();
        for (site, record) in &contribution.call_sites {
            match &record.outcome {
                SiteOutcome::Resolved(instance) => {
                    callees.insert(
                        site.clone(),
                        ConcreteCallee {
                            instance: instance.clone(),
                            signature: publication.signature(instance)?,
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
            .call_sites
            .keys()
            .map(|site| site.domain.clone());
        let foreign_domains = publication.foreign_domains(default_domains.chain(site_domains))?;
        let module_data = publication.ws.module(module);
        let (params, result) = match body_owner {
            BodyOwner::Global(gid) => {
                let ty = module_data.globals[gid].ty;
                let result = types.ty_type(ty).ok_or(MonoTyReason::NoNode)?;
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
                    .map(|binding| types.binding_type(binding).ok_or(MonoTyReason::NoNode))
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
    let publication = Publication::new(ws, g, 64);
    let (results, always_results) = bone_core::prelude::join_in_place(
        || concrete_map(expanded, |instance| plan_instance(&publication, instance)),
        || {
            concrete_map(always_owners, |(module, owner)| {
                plan_always(&publication, module, owner)
            })
        },
    );

    let mut out = ConcreteBodies::default();
    let mut plan_rows = Vec::new();
    let mut counts: FxHashMap<Instance, usize> = FxHashMap::default();
    for built in results.into_iter().flatten() {
        match built.plan {
            Ok(plan) => {
                for callee in plan.callees.values() {
                    *counts.entry(callee.instance.clone()).or_default() += 1;
                }
                plan_rows.push((built.instance, Arc::new(plan)));
            }
            Err(reason) => {
                out.plan_failure_index.insert(built.instance, reason);
            }
        }
    }
    out.candidate_counts = counts.into_iter().collect();
    let plans = plan_rows.into_iter().collect();

    for result in always_results {
        out.install_always(result);
    }
    out.plans = Arc::new(plans);
    out
}
