use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use bone_core::hir::{BodyOwner, TypeExpr};
use bone_core::prelude::TyId;
use bone_core::workspace::Workspace;
use rustc_hash::{FxHashMap, FxHashSet};
use skull_mir::concrete::{
    ConcreteAlwaysPlans, ConcreteBodyPlan, ConcreteCallee, ConcreteOwner, ConcretePlans,
    ConcreteSignature, ConcreteSignatureHandle, ConcreteTypeTables,
};
use skull_mir::mir::FuncRef;
use skull_mir::monoty::{MonoSubst, MonoTyReason, TyEnv};

use crate::instance_graph::{
    ready_body, BoundaryClass, Contribution, Instance, InstanceGraph, Owner, Site, SiteDomain,
    SiteOp, SiteOutcome,
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
        let mut targets = instance_subjects;
        targets.retain(|instance| g.instances.contains(instance));
        let always_subjects: Vec<_> = revisited_owners
            .iter()
            .filter_map(|owner| match owner {
                Owner::AlwaysEmitted(module, body) => {
                    Some((owner, *module, *body, concrete_owner_for(*module, *body)?))
                }
                Owner::Instance(_) => None,
            })
            .collect();
        let mut publication = PublicationSets::default();
        for instance in &targets {
            publication.instances.insert(instance);
            if let Some(contribution) = g.contributions.get(&Owner::Instance(instance.clone())) {
                publication.add_sites(contribution, true);
            }
        }
        for (owner, _, _, _) in &always_subjects {
            if let Some(contribution) = g.contributions.get(*owner) {
                publication.add_sites(contribution, true);
            }
        }
        let (signatures, domains) = publication.publish_local(ws, g);
        for instance in targets {
            let Some(prepared) = prepare_instance(ws, g, instance) else {
                continue;
            };
            let built = produce_instance(g, prepared, &signatures, &domains);
            match built.plan {
                Err(reason) => {
                    self.plan_failure_index.insert(built.instance, reason);
                }
                Ok(plan) => {
                    candidate_keys_added.extend(
                        self.install_candidate_refs_owned(&built.instance, built.candidate_refs),
                    );
                    Arc::get_mut(&mut self.plans)
                        .expect("local concrete update lost its unique plan store")
                        .insert(built.instance, Arc::new(plan));
                }
            }
        }
        for (_, module, body, owner) in always_subjects {
            self.always_plan_failure_index.remove(&owner);
            self.always_plans.remove(&owner);
            let prepared = prepare_always(ws, g, module, body);
            self.install_always(produce_always(ws, g, prepared, &signatures, &domains));
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

fn collect_publication_inputs(g: &InstanceGraph) -> PublicationSets<'_> {
    let owners: Vec<(&Owner, &Contribution)> =
        g.contributions.iter().map(|(o, c)| (o, &**c)).collect();
    #[cfg(not(feature = "sequential"))]
    {
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
    }
    #[cfg(feature = "sequential")]
    {
        let mut sets = PublicationSets::default();
        for (owner, contribution) in owners {
            sets.add(owner, contribution);
        }
        sets
    }
}

#[derive(Default)]
struct PublicationSets<'g> {
    instances: FxHashSet<&'g Instance>,
    domains: FxHashSet<&'g SiteDomain>,
}

impl<'g> PublicationSets<'g> {
    fn add(&mut self, owner: &'g Owner, contribution: &'g Contribution) {
        match owner {
            Owner::Instance(instance) if contribution.expanded => {
                self.instances.insert(instance);
                self.add_sites(contribution, false);
            }
            Owner::AlwaysEmitted(..) => self.add_sites(contribution, true),
            Owner::Instance(_) => {}
        }
    }

    fn add_sites(&mut self, contribution: &'g Contribution, include_failed_domains: bool) {
        self.domains.extend(&contribution.struct_default_domains);
        for (site, record) in &contribution.call_sites {
            let resolved = if let SiteOutcome::Resolved(target) = &record.outcome {
                self.instances.insert(target);
                true
            } else {
                false
            };
            if (resolved || include_failed_domains) && !matches!(site.domain, SiteDomain::Own) {
                self.domains.insert(&site.domain);
            }
        }
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

    fn publish_local(
        self,
        ws: &Workspace,
        g: &InstanceGraph,
    ) -> (PublishedSignatures, PublishedDomains) {
        let mut instances: Vec<_> = self.instances.into_iter().collect();
        instances.sort_unstable();
        let signatures = instances
            .into_iter()
            .map(|instance| signature_row(ws, g, instance))
            .collect();
        let domains = self.domains.into_iter().cloned().collect::<BTreeSet<_>>();
        (signatures, publish_foreign_domains(ws, g, domains))
    }
}

fn signature_row(
    ws: &Workspace,
    g: &InstanceGraph,
    instance: &Instance,
) -> (Instance, Result<ConcreteSignatureHandle, MonoTyReason>) {
    let result = g
        .substitution(instance)
        .ok_or(MonoTyReason::NoNode)
        .and_then(|subst| concrete_signature(ws, g, instance, subst))
        .map(Arc::new);
    (instance.clone(), result)
}

fn publish(ws: &Workspace, g: &InstanceGraph) -> (PublishedSignatures, PublishedDomains) {
    use bone_core::prelude::*;

    let PublicationSets { instances, domains } = collect_publication_inputs(g);
    let signatures = instances
        .into_iter()
        .collect::<Vec<_>>()
        .into_par_iter()
        .map(|instance| signature_row(ws, g, instance))
        .collect::<Vec<_>>()
        .into_iter()
        .collect();
    let domains = domains.into_iter().cloned().collect::<BTreeSet<_>>();
    (signatures, publish_foreign_domains(ws, g, domains))
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
                                ConcreteTypeTables::publish(
                                    ws,
                                    g.interner(),
                                    (*module, owner),
                                    (defaults_inf, defaults_body),
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
    types: Result<ConcreteTypeTables, MonoTyReason>,
}

fn prepare_instance(ws: &Workspace, g: &InstanceGraph, inst: Instance) -> Option<PreparedInstance> {
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
    Some(PreparedInstance {
        instance: inst,
        types,
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
    types: Result<ConcreteTypeTables, MonoTyReason>,
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
    let types = types?;

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
    } = prepared;
    let (plan, candidate_refs) =
        match concrete_plan_from_sites(g, &inst, types, signatures, domains) {
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
        for (site, record) in &contribution.call_sites {
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
            .call_sites
            .keys()
            .map(|site| site.domain.clone());
        let foreign_domains =
            foreign_domains_from_publication(default_domains.chain(site_domains), domains)?;
        let module_data = ws.module(module);
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
