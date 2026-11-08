//! `ConcreteBody` type tables.
//!
//! An instance's body states its types CONCRETELY: every `Idx<Expr>` and every
//! `Idx<BindingInfo>` maps to a [`MonoTyId`], converted from the inferred `TyId`
//! under that instance's substitution. MIR lowering must not receive a
//! substitution map, so the substitution is consumed HERE and nowhere later.
//!
//! **"No free generic types" is not a structural check** -- [`MonoTy`] has no
//! generic constructor, so a table that exists cannot hold one. The assertion
//! has content only as a TOTALITY claim: every slot of every expanded instance
//! converts. A slot that would have held a free generic becomes a recorded
//! [`FreeType`] with a reason bucket and a span instead, and the census reports
//! those beside the number of slots examined -- a zero over an unstated
//! denominator says nothing.
//!
//! The population is [`InstanceGraph::expanded`]: the bodies the walk actually
//! entered, recorded rather than inferred from a proxy. A boundary has no Rock
//! body to state types for.
//!
//! **The callee tables come from the WALK, not from a second resolution.**
//! `direct_calls`, `fn_values`, `iterator_ops` and `synthesized_calls` are
//! [`InstanceGraph::call_sites`] projected per table: the walk already resolved
//! every operation under the instance's substitution, so the substitution is
//! consumed there and here and nowhere later. A second HIR resolver could
//! disagree with the graph, and correlating the two by span could not detect
//! that -- a `for` head's `iter` and `next` share one span by construction.
//!
//! [`MonoTy`]: skull_mir::monoty::MonoTy

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use bone_core::hir::{BindingInfo, Body, BodyOwner, Expr, TypeExpr};
use bone_core::prelude::{Span, TyId};
use bone_core::workspace::{BodyInference, Workspace};
use comfy_la_arena::{ArenaMap, Idx};
use rustc_hash::{FxHashMap, FxHashSet};
use skull_mir::concrete::{
    ConcreteAlwaysPlans, ConcreteBodyPlan, ConcreteOwner, ConcretePlans, ConcreteSignature,
    ConcreteSignatureHandle, ConcreteTypeTables,
};
use skull_mir::mir::FuncRef;
use skull_mir::monoty::{MonoCtor, MonoInterner, MonoSubst, MonoTy, MonoTyId, MonoTyReason, TyEnv};
use ustr::Ustr;

use crate::instance_graph::{
    boundary_class_for, function_body, is_spanless, BoundaryClass, FunctionBody, Instance,
    InstanceGraph, Owner, Site, SiteDomain, SiteOp, SiteOutcome, TextEditDelta,
};
use crate::target_projection::CpuTargetSpec;

/// Which table a slot belongs to. Kept apart in the census because the two have
/// independent denominators: a body with no locals still has expressions.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Slot {
    Expr(Idx<Expr>),
    Binding(Idx<BindingInfo>),
}

impl Slot {
    pub fn kind(self) -> &'static str {
        match self {
            Self::Expr(_) => "expr",
            Self::Binding(_) => "binding",
        }
    }
}

/// A callee whose body skull does not lower. The declaration preserves its
/// contract identity when backend siblings share a symbol; the target and
/// class make availability part of the identity rather than a later lookup.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct BoundaryId {
    pub declaration: FuncRef,
    pub class: BoundaryClass,
    pub target: CpuTargetSpec,
    pub symbol: String,
}

/// Deliberately NOT `Ord`. The hand-written ordering this replaces keyed on
/// `(module, func, symbol)` while `Eq` also compared the class and the target,
/// so two unequal identities compared `Equal` and any ordered collection keyed
/// by it would merge boundaries the contract says are distinct. Nothing orders
/// a `BoundaryId` today -- `ArtifactView::boundaries` is keyed by the
/// [`Instance`] that reached it -- so the inconsistency is removed rather than
/// re-derived. A consumer that needs an ordering should key on the instance, or
/// bring its own total order and a test that it agrees with `Eq`.
/// Plan S 9's `enum Callee { Rock(InstanceId), Boundary(BoundaryId) }`, in the
/// SHARED graph's target-neutral form.
///
/// There is no third arm and no absent arm. A call that resolved to neither is
/// a [`CalleeHole`], reported with its span, never an entry a consumer could
/// read as callable.
///
/// [`Self::Boundary`] carries the [`Instance`] and its DECLARED class, not a
/// [`BoundaryId`]: an id names a target and an emitted symbol, and a shared body
/// has neither. Minting one is `artifact::realize`'s job, which is also the only
/// place that knows the declaration this instance resolves to on a given target
/// and the planner that spells it. The class here is the declaration's own
/// (`boundary_class_for`); an artifact may realize the same declaration
/// differently, and that is its answer to make.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Callee {
    Rock(Instance),
    Boundary(Instance, BoundaryClass),
}

impl Callee {
    /// The instance either arm names. Both arms have one -- the split is about
    /// whether a body is LOWERED here, not about whether there is a callee.
    pub fn instance(&self) -> &Instance {
        match self {
            Self::Rock(i) | Self::Boundary(i, _) => i,
        }
    }
}

/// Which iterator operation an [`Site`] in `iterator_ops` is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IteratorOpKind {
    /// The `for` head's `iter()`.
    Iter,
    /// The `for` protocol's `next`, at any depth of its conformance tree.
    Next,
    /// An iterable argument coerced through `iter()` at an `Iterator`-bounded
    /// parameter.
    ImplicitIter,
    /// A `next` witness of a bounded CALL's conformance tree.
    BoundNext,
}

impl IteratorOpKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Iter => "iter",
            Self::Next => "next",
            Self::ImplicitIter => "implicit-iter",
            Self::BoundNext => "bound-next",
        }
    }
}

/// Plan S 9's `ConcreteIteratorOp`: a resolved iterator operation.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ConcreteIteratorOp {
    pub kind: IteratorOpKind,
    pub callee: Callee,
}

/// One instance's concrete callee tables.
///
/// Type tables live only on `ConcreteBodyPlan`: lowerers and artifact views
/// consume that plan, while this record remains the graph-side call-fact join.
///
/// EVERY callee table is keyed by [`Site`] rather than by `Idx<Expr>`. One
/// expression can carry more than one callee obligation -- a `for` head has an
/// `iter` and a `next`, a column-Map store emits a slot AND a free -- and an
/// index is only meaningful against ONE arena, which a struct-default
/// initializer's is not. Plan S 9 writes `Map<ExprId, ..>`; an expression key
/// there silently keeps one of each pair and reads a foreign index against the
/// owner's body.
#[derive(Clone)]
pub struct ConcreteBody {
    pub instance: Instance,
    pub direct_calls: BTreeMap<Site, Callee>,
    pub fn_values: BTreeMap<Site, Callee>,
    pub iterator_ops: BTreeMap<Site, ConcreteIteratorOp>,
    /// Calls no source expression contains. Not in `direct_calls`: the same
    /// expression can hold a source call and a synthesized one, and they are
    /// different obligations with different callees.
    pub synthesized_calls: BTreeMap<Site, Callee>,
}

type TypeConversionMemo = FxHashMap<TyId, Result<(MonoTyId, usize), MonoTyReason>>;

impl ConcreteBody {
    /// Every callee this body names, with the site that named it.
    pub fn callees(&self) -> impl Iterator<Item = (&Site, &Callee)> {
        self.direct_calls
            .iter()
            .chain(self.fn_values.iter())
            .chain(
                self.iterator_ops
                    .iter()
                    .map(|(site, op)| (site, &op.callee)),
            )
            .chain(self.synthesized_calls.iter())
    }
}

/// A site the walk could not resolve to any callee. The span is where the
/// Stage-3 compile error goes.
#[derive(Clone, Debug)]
pub struct CalleeHole {
    pub instance: Instance,
    pub site: Site,
    pub reason: &'static str,
    pub span: Span,
}

/// A slot whose inferred type did not convert under the instance's
/// substitution. The reason is the bucket, the span is where the compile error
/// goes at the Stage-3 cutover.
#[derive(Clone, Debug)]
pub struct FreeType {
    pub slot: Slot,
    pub reason: MonoTyReason,
    pub span: Span,
}

/// Why an expanded instance got no tables at all. Each is a defect, not a
/// tolerated absence: the walk expanded the body, so it had one, and it
/// recorded a substitution for every instance it reached.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BodyGap {
    /// No entry in [`InstanceGraph::substitutions`]. Building under an empty
    /// map instead would silently give a generic body a shared body's types.
    NoSubstitution,
    /// The module has no `BodyInference` for this function.
    NoInference,
    /// The module has no `Body` for this function.
    NoBody,
    /// The body was relowered after this inference was recorded.
    StaleInference,
    /// The same logical instance was reached with two different conformance
    /// witness trees, so its bounded calls have no discovery-order-free answer.
    InconsistentBounds,
}

impl BodyGap {
    pub fn label(self) -> &'static str {
        match self {
            Self::NoSubstitution => "no-substitution",
            Self::NoInference => "no-inference",
            Self::NoBody => "no-body",
            Self::StaleInference => "stale-inference",
            Self::InconsistentBounds => "inconsistent-bounds",
        }
    }
}

#[derive(Clone, Debug)]
pub struct MissingBody {
    pub instance: Instance,
    pub gap: BodyGap,
}

/// The tables for one graph, plus everything that did not convert.
#[derive(Clone)]
pub struct ConcreteBodies {
    pub bodies: BTreeMap<Instance, ConcreteBody>,
    /// Compatibility projections below are refreshed only for a full-build
    /// snapshot. Local repair reads the owner records instead; this is the
    /// non-owner instrumentation contribution that has no body to attach to.
    /// The plan production lowering consumes. It is stored beside the census
    /// body so a graph update can retract a plan with its instance. Behind an
    /// `Arc` so the shader artifact view and the JS session can consume the
    /// SAME table instead of rebuilding it from the graph.
    pub plans: std::sync::Arc<ConcretePlans>,
    pub always_plans: ConcreteAlwaysPlans,
    pub plan_failures: Vec<(Instance, MonoTyReason)>,
    pub always_plan_failures: Vec<(ConcreteOwner, MonoTyReason)>,
    /// Compatibility projection of the owner-local free-type findings. The
    /// per-owner records below are authoritative during local repair.
    pub free_types: Vec<(Instance, FreeType)>,
    pub gaps: Vec<MissingBody>,
    pub expr_examined: usize,
    pub binding_examined: usize,
    /// See [`Converted::generics_bound`].
    pub generics_bound: usize,
    /// See [`Converted::untyped_exprs`]. An expression with no inferred type is
    /// a slot the tables never saw, so it cannot appear as a free type either.
    pub untyped: Vec<(Instance, UntypedExpr)>,
    pub untyped_bindings: usize,
    /// The arenas the two above are measured against.
    pub expr_slots: usize,
    pub binding_slots: usize,
    /// Bodies built under a NON-EMPTY substitution. The population in which a
    /// free generic is even possible.
    pub generic_bodies: usize,
    /// Sites the walk recorded for the instances that got a body -- the
    /// denominator for every callee count below.
    pub sites_examined: usize,
    pub callee_holes: Vec<CalleeHole>,
    /// A `Callee::Rock` whose instance is not in the graph. Must be empty: the
    /// walk `reach`es every callee it resolves, so a missing one means the
    /// tables and the graph disagree about what exists.
    pub unknown_rock_callees: Vec<(Instance, Site)>,
    /// Untyped expression slots that are ALSO a callee-table key, by expression
    /// kind. This is the measurement the type-table slice deferred: a kind with
    /// a non-zero count here has a callee entry built for an expression with no
    /// inferred type, so the entry names the oracle that answered instead.
    pub untyped_as_callee_key: BTreeMap<&'static str, usize>,
    /// Signature-memo effectiveness over this build: questions asked vs
    /// signatures actually derived. Derivations strictly below requests is the
    /// witness that per-site callee questions were served from the memo.
    pub signature_requests: usize,
    pub signature_derivations: usize,
    pub signature_slots_cloned: usize,
    /// Conversions the per-body TyId memos actually ran over the EXPANDED
    /// instances (always-emitted bodies excluded); `expr_examined +
    /// binding_examined` is the per-slot denominator it deduplicates.
    pub slot_conversions: usize,
    /// Last explicitly requested compatibility snapshot of the owner records.
    /// Ordinary construction and owner-local repair leave this field untouched.
    pub build_stats: ConcreteBuildStats,
    eager_owner_fact_scans: usize,
    instance_facts: FxHashMap<Instance, OwnerFacts>,
    always_facts: FxHashMap<ConcreteOwner, OwnerFacts>,
    /// Foreign-domain publication is a shared full-build contribution. It is
    /// kept separately because it has no single owner; local body facts never
    /// mutate it or fold a second copy into the aggregate.
    global_build_stats: ConcreteBuildStats,
    plan_failure_index: BTreeMap<Instance, MonoTyReason>,
    always_plan_failure_index: FxHashMap<ConcreteOwner, MonoTyReason>,
    candidate_keys: BTreeSet<Instance>,
    candidate_keys_by_func: FxHashMap<FuncRef, FxHashSet<Instance>>,
    owner_candidates: FxHashMap<Instance, FxHashSet<Instance>>,
    candidate_users: FxHashMap<Instance, FxHashSet<Instance>>,
}

impl Default for ConcreteBodies {
    fn default() -> Self {
        CONCRETE_DEFAULT_INVOCATIONS.with(|count| count.set(count.get() + 1));
        Self {
            bodies: BTreeMap::new(),
            plans: std::sync::Arc::new(ConcretePlans::new()),
            always_plans: ConcreteAlwaysPlans::default(),
            plan_failures: Vec::new(),
            always_plan_failures: Vec::new(),
            free_types: Vec::new(),
            gaps: Vec::new(),
            expr_examined: 0,
            binding_examined: 0,
            generics_bound: 0,
            untyped: Vec::new(),
            untyped_bindings: 0,
            expr_slots: 0,
            binding_slots: 0,
            generic_bodies: 0,
            sites_examined: 0,
            callee_holes: Vec::new(),
            unknown_rock_callees: Vec::new(),
            untyped_as_callee_key: BTreeMap::new(),
            signature_requests: 0,
            signature_derivations: 0,
            signature_slots_cloned: 0,
            slot_conversions: 0,
            build_stats: ConcreteBuildStats::default(),
            eager_owner_fact_scans: 0,
            instance_facts: FxHashMap::default(),
            always_facts: FxHashMap::default(),
            global_build_stats: ConcreteBuildStats::default(),
            plan_failure_index: BTreeMap::new(),
            always_plan_failure_index: FxHashMap::default(),
            candidate_keys: BTreeSet::new(),
            candidate_keys_by_func: FxHashMap::default(),
            owner_candidates: FxHashMap::default(),
            candidate_users: FxHashMap::default(),
        }
    }
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
    pub before: usize,
    pub derived: usize,
    pub reused: usize,
    pub retracted: usize,
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
    fn aggregate_facts(&self) -> OwnerFacts {
        let mut facts = OwnerFacts::default();
        for owner in self.instance_facts.values() {
            owner.accumulate_into(&mut facts);
        }
        for owner in self.always_facts.values() {
            owner.accumulate_into(&mut facts);
        }
        facts.build_stats.add(self.global_build_stats);
        facts.sort_for_output();
        facts
    }

    /// Materialize compatibility vectors for an explicit diagnostic/census
    /// request. Ordinary fresh construction keeps owner facts authoritative.
    pub fn refresh_flat_views(&mut self) {
        self.eager_owner_fact_scans += self.instance_facts.len() + self.always_facts.len();
        let facts = self.aggregate_facts();
        self.plan_failures = facts.plan_failures;
        self.always_plan_failures = facts.always_plan_failures;
        self.free_types = facts.free_types;
        self.gaps = facts.gaps;
        self.expr_examined = facts.expr_examined;
        self.binding_examined = facts.binding_examined;
        self.generics_bound = facts.generics_bound;
        self.untyped = facts.untyped;
        self.untyped_bindings = facts.untyped_bindings;
        self.expr_slots = facts.expr_slots;
        self.binding_slots = facts.binding_slots;
        self.generic_bodies = facts.generic_bodies;
        self.sites_examined = facts.sites_examined;
        self.callee_holes = facts.callee_holes;
        self.unknown_rock_callees = facts.unknown_rock_callees;
        self.untyped_as_callee_key = facts.untyped_as_callee_key;
        self.signature_requests = facts.signature_requests;
        self.signature_slots_cloned = facts.signature_slots_cloned;
        self.slot_conversions = facts.slot_conversions;
        self.build_stats = facts.build_stats;
    }

    pub fn plan_for(&self, instance: &Instance) -> Option<&ConcreteBodyPlan> {
        self.plans.get(instance).map(|plan| &**plan)
    }

    pub fn plan_for_owner(&self, owner: &ConcreteOwner) -> Option<&ConcreteBodyPlan> {
        self.always_plans.get(owner)
    }

    /// A staged cache shares the outer table with its last-good owner until it
    /// crosses this boundary. Clone only the ordered handle map; unchanged
    /// concrete bodies remain shared by their per-entry `Arc`.
    pub fn detach_plan_store(&mut self) {
        if Arc::strong_count(&self.plans) > 1 {
            self.plans = Arc::new((*self.plans).clone());
        }
    }

    pub fn candidate_keys(&self) -> &BTreeSet<Instance> {
        &self.candidate_keys
    }

    pub fn candidate_keys_by_func(&self) -> &FxHashMap<FuncRef, FxHashSet<Instance>> {
        &self.candidate_keys_by_func
    }

    /// Fold the authoritative owner records for an explicit census request.
    /// The repair path does not call this method.
    pub fn snapshot_build_stats(&mut self) -> ConcreteBuildStats {
        let stats = self.aggregate_facts().build_stats;
        self.build_stats = stats;
        stats
    }

    pub fn eager_owner_fact_scans(&self) -> usize {
        self.eager_owner_fact_scans
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
        self.update_local_with_span_edits(ws, g, revisited, removed, revisited_owners, &[])
    }

    pub fn update_local_with_span_edits(
        &mut self,
        ws: &Workspace,
        g: &InstanceGraph,
        revisited: &BTreeSet<Instance>,
        removed: &BTreeSet<Instance>,
        revisited_owners: &BTreeSet<Owner>,
        edits: &[TextEditDelta],
    ) -> ConcreteUpdateStats {
        self.shift_fact_spans(edits);
        let before = self.instance_facts.len() + self.always_facts.len();
        let mut instance_subjects = revisited.clone();
        instance_subjects.extend(removed.iter().cloned());
        let mut retracted = 0;
        let mut candidate_keys_added = BTreeSet::new();
        let mut candidate_keys_removed = BTreeSet::new();
        for instance in &instance_subjects {
            if self.instance_facts.contains_key(instance)
                || self.bodies.contains_key(instance)
                || self.plans.get(instance).is_some()
            {
                retracted += 1;
            }
            let (removed_candidates, _scans) = self.remove_instance(instance);
            candidate_keys_removed.extend(removed_candidates);
        }
        let mut targets = revisited.clone();
        targets.extend(removed.iter().cloned());
        targets.retain(|instance| g.instances.contains(instance));
        let (results, publication_stats) = build_instance_delta(ws, g, &targets);
        let _local_publication_stats = publication_stats;
        let mut derived = results.len();
        for result in results {
            match result {
                InstanceResult::Gap(gap) => {
                    let mut facts = OwnerFacts::default();
                    facts.gaps.push(gap.clone());
                    self.instance_facts.insert(gap.instance, facts);
                }
                InstanceResult::Built(built) => {
                    let instance = built.instance.clone();
                    let facts = built.facts;
                    let candidate_refs = built.candidate_refs;
                    if let Ok(plan) = built.plan {
                        candidate_keys_added
                            .extend(self.install_candidate_refs_owned(&instance, candidate_refs));
                        Arc::get_mut(&mut self.plans)
                            .expect("local concrete update lost its unique plan store")
                            .insert(instance.clone(), Arc::new(plan));
                    }
                    self.bodies.insert(instance.clone(), built.concrete);
                    self.install_failure_index(&facts);
                    self.instance_facts.insert(instance, facts);
                }
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
            if self.always_facts.remove(owner).is_some() {
                retracted += 1;
            }
            self.always_plan_failure_index.remove(owner);
            self.always_plans.remove(owner);
        }
        let always_inputs: Vec<_> = always_subjects
            .iter()
            .enumerate()
            .map(|(order, (module, body_owner, _))| {
                prepare_always(
                    ws,
                    g,
                    order,
                    *module,
                    *body_owner,
                    log::log_enabled!(log::Level::Debug),
                )
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
            let _local_publication_stats = publication_stats;
            let mut results: Vec<_> = always_inputs
                .into_iter()
                .map(|prepared| {
                    produce_always(
                        ws,
                        g,
                        prepared,
                        &signatures,
                        &published_domains,
                        log::log_enabled!(log::Level::Debug),
                    )
                })
                .collect();
            results.sort_by_key(|result| result.order);
            for result in results {
                let owner = result.owner.clone();
                let mut raw_facts = result.facts;
                if let (Some(owner), Err(reason)) = (&owner, &result.plan) {
                    raw_facts
                        .always_plan_failures
                        .push((owner.clone(), reason.clone()));
                }
                let facts = raw_facts;
                match result.plan {
                    Ok(Some(plan)) => {
                        self.always_plans.insert(plan.owner.clone(), plan);
                    }
                    Ok(None) => {}
                    Err(_) => {}
                }
                if let Some(owner) = owner {
                    self.install_failure_index(&facts);
                    self.always_facts.insert(owner, facts);
                }
            }
            derived += always_subjects.len();
        }
        ConcreteUpdateStats {
            derived,
            retracted,
            reused: before.saturating_sub(retracted),
            before,
            candidate_keys_added,
            candidate_keys_removed,
        }
    }

    fn shift_fact_spans(&mut self, edits: &[TextEditDelta]) {
        for edit in edits {
            for facts in self.instance_facts.values_mut() {
                facts.shift_spans(edit);
            }
            for facts in self.always_facts.values_mut() {
                facts.shift_spans(edit);
            }
        }
    }

    fn remove_instance(&mut self, instance: &Instance) -> (BTreeSet<Instance>, usize) {
        let mut removed = BTreeSet::new();
        if let Some(candidates) = self.owner_candidates.remove(instance) {
            for candidate in candidates {
                let Some(users) = self.candidate_users.get_mut(&candidate) else {
                    continue;
                };
                users.remove(instance);
                if users.is_empty() {
                    self.candidate_users.remove(&candidate);
                    self.candidate_keys.remove(&candidate);
                    if let Some(keys) = self.candidate_keys_by_func.get_mut(&candidate.func) {
                        keys.remove(&candidate);
                        if keys.is_empty() {
                            self.candidate_keys_by_func.remove(&candidate.func);
                        }
                    }
                    removed.insert(candidate);
                }
            }
        }
        if self.instance_facts.remove(instance).is_some() {
            self.plan_failure_index.remove(instance);
        } else {
            self.bodies.remove(instance);
            Arc::get_mut(&mut self.plans)
                .expect("local concrete update lost its unique plan store")
                .remove(instance);
            return (removed, 0);
        }
        self.bodies.remove(instance);
        Arc::get_mut(&mut self.plans)
            .expect("local concrete update lost its unique plan store")
            .remove(instance);
        (removed, 0)
    }

    fn install_failure_index(&mut self, facts: &OwnerFacts) {
        for (instance, reason) in &facts.plan_failures {
            self.plan_failure_index
                .insert(instance.clone(), reason.clone());
        }
        for (owner, reason) in &facts.always_plan_failures {
            self.always_plan_failure_index
                .insert(owner.clone(), reason.clone());
        }
    }

    fn install_candidate_refs_owned(
        &mut self,
        owner: &Instance,
        refs: FxHashSet<Instance>,
    ) -> BTreeSet<Instance> {
        let mut added = BTreeSet::new();
        for candidate in &refs {
            match self.candidate_users.entry(candidate.clone()) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    self.candidate_keys.insert(candidate.clone());
                    self.candidate_keys_by_func
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

    pub fn census(&self) -> BodyCensus {
        let facts = self.aggregate_facts();
        let mut c = BodyCensus {
            bodies: self.bodies.len(),
            expr_examined: facts.expr_examined,
            binding_examined: facts.binding_examined,
            generics_bound: facts.generics_bound,
            generic_bodies: facts.generic_bodies,
            untyped_exprs: facts.untyped.len(),
            untyped_bindings: facts.untyped_bindings,
            expr_slots: facts.expr_slots,
            binding_slots: facts.binding_slots,
            sites_examined: facts.sites_examined,
            unknown_rock_callees: facts.unknown_rock_callees.len(),
            untyped_as_callee_key: facts.untyped_as_callee_key.clone(),
            type_nodes_examined: facts.build_stats.type_nodes_examined,
            type_node_memo_hits: facts.build_stats.type_node_memo_hits,
            type_node_conversion_attempts: facts.build_stats.type_node_conversion_attempts,
            type_node_unresolved: facts.build_stats.type_node_unresolved,
            struct_literals_examined: facts.build_stats.struct_literals_examined,
            struct_field_conversion_attempts: facts.build_stats.struct_field_conversion_attempts,
            foreign_domain_requests: facts.build_stats.foreign_domain_requests,
            foreign_domain_unique: facts.build_stats.foreign_domain_unique,
            ..Default::default()
        };
        for b in self.bodies.values() {
            c.direct_calls += b.direct_calls.len();
            c.fn_values += b.fn_values.len();
            c.synthesized_calls += b.synthesized_calls.len();
            for op in b.iterator_ops.values() {
                *c.iterator_ops.entry(op.kind.label()).or_insert(0) += 1;
            }
            for (_, callee) in b.callees() {
                match callee {
                    Callee::Rock(_) => c.rock_callees += 1,
                    Callee::Boundary(..) => c.boundary_callees += 1,
                }
            }
        }
        for h in &facts.callee_holes {
            let k = c.callee_holes.entry(h.reason).or_default();
            k.0 += 1;
            k.1 += usize::from(is_spanless(&h.span));
        }
        for (_, f) in &facts.free_types {
            *c.free_by_reason.entry(f.reason.bucket()).or_insert(0) += 1;
            *c.free_by_slot.entry(f.slot.kind()).or_insert(0) += 1;
            c.spanless += usize::from(is_spanless(&f.span));
        }
        for (_, u) in &facts.untyped {
            *c.untyped_by_kind.entry(u.kind).or_insert(0) += 1;
        }
        for g in &facts.gaps {
            *c.gaps.entry(g.gap.label()).or_insert(0) += 1;
        }
        c
    }

    /// Untyped slots as `<instance> <expr kind> at <file>:<line>`.
    pub fn render_untyped(&self, ws: &Workspace, g: &InstanceGraph) -> Vec<String> {
        let rows: Vec<String> = self
            .aggregate_facts()
            .untyped
            .iter()
            .map(|(i, u)| {
                format!(
                    "{} {} at {}:{}",
                    g.identity(ws, i),
                    u.kind,
                    u.span.file,
                    u.span.line_range.0 + 1
                )
            })
            .collect();
        rows
    }

    /// Callee holes as `<instance> <op> <reason> at <file>:<line>`.
    pub fn render_callee_holes(&self, ws: &Workspace, g: &InstanceGraph) -> Vec<String> {
        let rows: Vec<String> = self
            .aggregate_facts()
            .callee_holes
            .iter()
            .map(|h| {
                format!(
                    "{} {} {} at {}:{}",
                    g.identity(ws, &h.instance),
                    h.site.op.name(),
                    h.reason,
                    h.span.file,
                    h.span.line_range.0 + 1
                )
            })
            .collect();
        rows
    }

    /// Free types as `<instance> <slot-kind> <bucket> at <file>:<line>`, for a
    /// report or a failing assertion.
    pub fn render_free_types(&self, ws: &Workspace, g: &InstanceGraph) -> Vec<String> {
        let rows: Vec<String> = self
            .aggregate_facts()
            .free_types
            .iter()
            .map(|(i, f)| {
                format!(
                    "{} {} {} at {}:{}",
                    g.identity(ws, i),
                    f.slot.kind(),
                    f.reason.bucket(),
                    f.span.file,
                    f.span.line_range.0 + 1
                )
            })
            .collect();
        rows
    }

    /// Stable, complete diagnostic identity for comparing a local update with
    /// a fresh build. This is intentionally a request-time census over the
    /// authoritative owner records, not a mutation-maintained global vector.
    pub fn diagnostic_snapshot(&self, ws: &Workspace, g: &InstanceGraph) -> Vec<String> {
        let facts = self.aggregate_facts();
        let mut rows = Vec::new();
        rows.extend(facts.plan_failures.iter().map(|(instance, reason)| {
            format!(
                "plan-failure:{}:{}",
                g.identity(ws, instance),
                reason.bucket()
            )
        }));
        rows.extend(
            facts.always_plan_failures.iter().map(|(owner, reason)| {
                format!("always-plan-failure:{owner:?}:{}", reason.bucket())
            }),
        );
        rows.extend(
            self.render_free_types(ws, g)
                .into_iter()
                .map(|row| format!("free-type:{row}")),
        );
        rows.extend(
            self.render_untyped(ws, g)
                .into_iter()
                .map(|row| format!("untyped:{row}")),
        );
        rows.extend(
            self.render_callee_holes(ws, g)
                .into_iter()
                .map(|row| format!("callee-hole:{row}")),
        );
        rows.extend(
            facts
                .gaps
                .iter()
                .map(|gap| format!("gap:{}:{}", g.identity(ws, &gap.instance), gap.gap.label())),
        );
        rows.extend(facts.unknown_rock_callees.iter().map(|(instance, site)| {
            format!(
                "unknown-callee:{}:{}",
                g.identity(ws, instance),
                site.op.name()
            )
        }));
        rows.sort();
        rows
    }
}

#[derive(Default, Debug)]
pub struct BodyCensus {
    pub bodies: usize,
    pub expr_examined: usize,
    pub binding_examined: usize,
    pub generics_bound: usize,
    pub generic_bodies: usize,
    pub untyped_exprs: usize,
    pub untyped_bindings: usize,
    pub untyped_by_kind: BTreeMap<&'static str, usize>,
    pub expr_slots: usize,
    pub binding_slots: usize,
    pub free_by_reason: BTreeMap<&'static str, usize>,
    pub free_by_slot: BTreeMap<&'static str, usize>,
    pub spanless: usize,
    pub gaps: BTreeMap<&'static str, usize>,
    /// Sites the walk recorded for the instances that got a body. Every callee
    /// count below is measured against this.
    pub sites_examined: usize,
    pub direct_calls: usize,
    pub fn_values: usize,
    pub synthesized_calls: usize,
    pub iterator_ops: BTreeMap<&'static str, usize>,
    pub rock_callees: usize,
    pub boundary_callees: usize,
    /// Reason -> (holes, of those with no span).
    pub callee_holes: BTreeMap<&'static str, (usize, usize)>,
    pub unknown_rock_callees: usize,
    pub untyped_as_callee_key: BTreeMap<&'static str, usize>,
    pub type_nodes_examined: usize,
    pub type_node_memo_hits: usize,
    pub type_node_conversion_attempts: usize,
    pub type_node_unresolved: usize,
    pub struct_literals_examined: usize,
    pub struct_field_conversion_attempts: usize,
    pub foreign_domain_requests: usize,
    pub foreign_domain_unique: usize,
}

impl BodyCensus {
    /// Entries across all four callee tables.
    pub fn callee_entries(&self) -> usize {
        self.direct_calls
            + self.fn_values
            + self.synthesized_calls
            + self.iterator_ops.values().sum::<usize>()
    }

    pub fn callee_hole_total(&self) -> usize {
        self.callee_holes.values().map(|(n, _)| n).sum()
    }
}

/// An expression the body's arena holds and inference recorded no type for.
/// The kind is what makes the population diagnosable: "3,446 slots" is a
/// number, "3,446 `Block` tails" is a class.
#[derive(Clone, Debug)]
pub struct UntypedExpr {
    pub expr: Idx<Expr>,
    pub kind: &'static str,
    pub span: Span,
}

/// The expression's constructor name. `Debug`'s leading token, because the HIR
/// exposes no discriminant name and a hand-written match would drift from the
/// enum silently.
fn expr_kind(e: &Expr) -> &'static str {
    let d = format!("{e:?}");
    let head = d
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .next()
        .unwrap_or("?");
    // Leaked once per DISTINCT kind, not per slot: the set is the enum's
    // variants, so this is bounded by the enum and not by the corpus.
    static KINDS: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<&'static str>>> =
        std::sync::LazyLock::new(Default::default);
    let mut set = KINDS.lock().expect("expr-kind interner");
    if let Some(k) = set.get(head) {
        return k;
    }
    let leaked: &'static str = Box::leak(head.to_string().into_boxed_str());
    set.insert(leaked);
    leaked
}

/// One body's tables under one substitution.
#[derive(Default)]
pub struct Converted {
    pub expr_types: ArenaMap<Idx<Expr>, MonoTyId>,
    pub binding_types: ArenaMap<Idx<BindingInfo>, MonoTyId>,
    pub failures: Vec<FreeType>,
    pub expr_examined: usize,
    pub binding_examined: usize,
    /// Expressions in the BODY's arena with no `expr_types` entry, and the same
    /// for bindings. The tables can only be total over what inference recorded,
    /// so the arena is the denominator that says whether that population is the
    /// whole body.
    pub untyped_exprs: Vec<UntypedExpr>,
    pub untyped_bindings: Vec<Idx<BindingInfo>>,
    /// Declared generics the conversions resolved THROUGH the substitution.
    /// Zero failures over a body that never consulted `subst` is a zero
    /// measured over the wrong space -- both a body that was already concrete
    /// and one whose generics were all supplied report no failures.
    pub generics_bound: usize,
    /// Conversions the per-body TyId memo actually RAN (misses). Examined
    /// counts stay per-slot; this strictly below them is the memo's
    /// effectiveness witness.
    pub conversions: usize,
    pub(crate) type_memo: TypeConversionMemo,
}

impl Converted {
    /// Slots the conversion actually ran over -- the denominator for
    /// [`Self::failures`].
    pub fn examined(&self) -> usize {
        self.expr_examined + self.binding_examined
    }
}

/// Convert one body's inferred types under `subst`.
///
/// Public because the substitution is the load-bearing input: a caller can pass
/// a different one and watch the generic slots go unbound, which is the only
/// perturbation that moves this arm without moving the walk that produced it.
pub fn convert_body(
    arena: &MonoInterner,
    ws: &Workspace,
    inf: &BodyInference,
    body: &Body,
    subst: &MonoSubst,
) -> Converted {
    let mut c = Converted::default();
    // Slots repeat inferred TyIds ~4x within a body (measured on a large corpus:
    // 139,559 slots over 34,237 unique ids), so the conversion is memoized per
    // body. The `generics_bound` census counts PER SLOT exactly as before --
    // each hit replays the cached delta -- and an error replays its reason, so
    // per-slot failure entries are unchanged.
    let mut memo: TypeConversionMemo = FxHashMap::default();
    let mut conversions = 0usize;
    let conversions_ref = &mut conversions;
    let mut convert_memo =
        |arena: &MonoInterner, ty: TyId, generics_bound: &mut usize| match memo.entry(ty) {
            std::collections::hash_map::Entry::Occupied(hit) => match hit.get() {
                Ok((id, delta)) => {
                    *generics_bound += delta;
                    Ok(*id)
                }
                Err(reason) => Err(reason.clone()),
            },
            std::collections::hash_map::Entry::Vacant(slot) => {
                *conversions_ref += 1;
                let mut delta = 0usize;
                let converted = arena.convert_counting(ws, inf, ty, subst, &mut delta);
                *generics_bound += delta;
                slot.insert(converted.clone().map(|id| (id, delta)));
                converted
            }
        };
    for (e, ty) in inf.expr_types.iter() {
        c.expr_examined += 1;
        match convert_memo(arena, *ty, &mut c.generics_bound) {
            Ok(id) => {
                c.expr_types.insert(e, id);
            }
            Err(reason) => c.failures.push(FreeType {
                slot: Slot::Expr(e),
                reason,
                span: body
                    .expr_spans
                    .get(e)
                    .copied()
                    .unwrap_or_else(Span::unknown),
            }),
        }
    }

    for (e, expr) in body.expr_arena.iter() {
        if inf.expr_types.get(e).is_none() {
            c.untyped_exprs.push(UntypedExpr {
                expr: e,
                kind: expr_kind(expr),
                span: body
                    .expr_spans
                    .get(e)
                    .copied()
                    .unwrap_or_else(Span::unknown),
            });
        }
    }
    for (b, _) in body.binding_arena.iter() {
        if inf.binding_types.get(b).is_none() {
            c.untyped_bindings.push(b);
        }
    }

    for (b, ty) in inf.binding_types.iter() {
        c.binding_examined += 1;
        match convert_memo(arena, *ty, &mut c.generics_bound) {
            Ok(id) => {
                c.binding_types.insert(b, id);
            }
            Err(reason) => c.failures.push(FreeType {
                slot: Slot::Binding(b),
                reason,
                span: body
                    .binding_spans
                    .get(b)
                    .copied()
                    .unwrap_or_else(Span::unknown),
            }),
        }
    }
    c.conversions = conversions;
    c.type_memo = memo;
    c
}

/// One instance's callee tables, from the sites the walk recorded for it.
///
/// The classification is [`boundary_class_for`] -- the graph's own, and the same
/// one `closure` asks -- so "is this callee a boundary" cannot be answered two
/// ways. It runs here rather than in the walk because `erased_entries` is
/// complete only after the walk, and it is the FULL classification, of which the
/// walk's expansion test is a subset.
fn callee_tables(
    ws: &Workspace,
    g: &InstanceGraph,
    inst: &Instance,
    body: &mut ConcreteBody,
    out: &mut OwnerFacts,
) {
    let Some(sites) = g.sites_of(inst) else {
        return;
    };
    for (site, rec) in sites {
        out.sites_examined += 1;
        let target = match &rec.outcome {
            SiteOutcome::Resolved(t) => t,
            SiteOutcome::Failed(reason) => {
                out.callee_holes.push(CalleeHole {
                    instance: inst.clone(),
                    site: site.clone(),
                    reason,
                    span: rec.span,
                });
                continue;
            }
        };
        let f = &ws.module(target.func.module).functions[target.func.func];
        let callee = match boundary_class_for(f, &g.erased_entries) {
            // Whether a target can SERVE this boundary is not asked here: the
            // graph names no target. A call to a boundary no artifact body
            // supplies is that artifact's `Severed` call, located at this same
            // site (`artifact::Unrealizable::ForeignToArtifact`).
            Some(class) => Callee::Boundary(target.clone(), class),
            None => {
                if !g.instances.contains(target) {
                    out.unknown_rock_callees.push((inst.clone(), site.clone()));
                }
                Callee::Rock(target.clone())
            }
        };
        match &site.op {
            SiteOp::DirectCall => {
                body.direct_calls.insert(site.clone(), callee);
            }
            SiteOp::FnValue => {
                body.fn_values.insert(site.clone(), callee);
            }
            // Both synthesized families share one table: they are keyed by the
            // whole `Site`, so a named runtime entry and a conditional dispatch
            // arm at one expression stay distinct without a second map.
            SiteOp::Synthesized(_) | SiteOp::Dispatched { .. } => {
                body.synthesized_calls.insert(site.clone(), callee);
            }
            SiteOp::ForIter => {
                body.iterator_ops.insert(
                    site.clone(),
                    ConcreteIteratorOp {
                        kind: IteratorOpKind::Iter,
                        callee,
                    },
                );
            }
            SiteOp::ForNext(_) => {
                body.iterator_ops.insert(
                    site.clone(),
                    ConcreteIteratorOp {
                        kind: IteratorOpKind::Next,
                        callee,
                    },
                );
            }
            SiteOp::ImplicitIter => {
                body.iterator_ops.insert(
                    site.clone(),
                    ConcreteIteratorOp {
                        kind: IteratorOpKind::ImplicitIter,
                        callee,
                    },
                );
            }
            SiteOp::BoundNext(_) => {
                body.iterator_ops.insert(
                    site.clone(),
                    ConcreteIteratorOp {
                        kind: IteratorOpKind::BoundNext,
                        callee,
                    },
                );
            }
        }
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
    {
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

/// Resolve the concrete signature a graph instance owns before a consumer
/// needs a full body plan. This is used by graph-side helper accounting, whose
/// pass runs before `ConcreteBodies::build` installs per-body tables.
pub fn concrete_signature_for_instance(
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
    for failure in &converted.failures {
        if !matches!(failure.reason, MonoTyReason::UnresolvedInfer(_)) {
            return Err(failure.reason.clone());
        }
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
    let struct_field_types =
        concrete_struct_field_types(ws, g, body, &converted.expr_types, stats, timing_enabled)?;
    let mut untyped_exprs: FxHashSet<_> = converted.untyped_exprs.iter().map(|u| u.expr).collect();
    let mut untyped_bindings: FxHashSet<_> = converted.untyped_bindings.iter().copied().collect();
    for failure in &converted.failures {
        match (&failure.slot, &failure.reason) {
            (Slot::Expr(expr), MonoTyReason::UnresolvedInfer(_)) => {
                untyped_exprs.insert(*expr);
            }
            (Slot::Binding(binding), MonoTyReason::UnresolvedInfer(_)) => {
                untyped_bindings.insert(*binding);
            }
            _ => {}
        }
    }
    Ok(ConcreteTypeTables {
        expr_types: converted.expr_types,
        binding_types: converted.binding_types,
        untyped_exprs,
        untyped_bindings,
        ty_types,
        struct_field_types,
    })
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
                    memo.insert(ty, converted.clone().map(|id| (id, 0)));
                    converted.map(|id| (id, 0))
                };
                match converted {
                    Ok((id, _)) => Some(Ok((ty, id))),
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

fn counted_callees<'a>(
    body: &'a ConcreteBody,
    stats: &mut ConcreteBuildStats,
) -> impl Iterator<Item = (&'a Site, &'a Callee)> + use<'a> {
    stats.callee_scans += 1;
    stats.callee_rows_visited += body.direct_calls.len()
        + body.fn_values.len()
        + body.iterator_ops.len()
        + body.synthesized_calls.len();
    body.callees()
}

struct BuiltInstance {
    instance: Instance,
    concrete: ConcreteBody,
    plan: Result<ConcreteBodyPlan, MonoTyReason>,
    candidate_refs: FxHashSet<Instance>,
    facts: OwnerFacts,
    convert_time: std::time::Duration,
    callee_time: std::time::Duration,
    plan_time: std::time::Duration,
    elapsed: std::time::Duration,
    slot_tyids: usize,
    unique_slot_tyids: usize,
}

#[expect(
    clippy::large_enum_variant,
    reason = "Keep successful results inline rather than adding a per-instance allocation"
)]
enum InstanceResult {
    Gap(MissingBody),
    Built(BuiltInstance),
}

impl InstanceResult {
    fn instance(&self) -> &Instance {
        match self {
            Self::Gap(gap) => &gap.instance,
            Self::Built(built) => &built.instance,
        }
    }
}

#[derive(Clone, Default)]
struct OwnerFacts {
    plan_failures: Vec<(Instance, MonoTyReason)>,
    always_plan_failures: Vec<(ConcreteOwner, MonoTyReason)>,
    free_types: Vec<(Instance, FreeType)>,
    gaps: Vec<MissingBody>,
    expr_examined: usize,
    binding_examined: usize,
    generics_bound: usize,
    untyped: Vec<(Instance, UntypedExpr)>,
    untyped_bindings: usize,
    expr_slots: usize,
    binding_slots: usize,
    generic_bodies: usize,
    sites_examined: usize,
    callee_holes: Vec<CalleeHole>,
    unknown_rock_callees: Vec<(Instance, Site)>,
    untyped_as_callee_key: BTreeMap<&'static str, usize>,
    signature_requests: usize,
    signature_derivations: usize,
    signature_slots_cloned: usize,
    slot_conversions: usize,
    build_stats: ConcreteBuildStats,
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

impl OwnerFacts {
    fn shift_spans(&mut self, edit: &TextEditDelta) {
        for (_, failure) in &mut self.free_types {
            edit.shift_span(&mut failure.span);
        }
        for (_, untyped) in &mut self.untyped {
            edit.shift_span(&mut untyped.span);
        }
        for hole in &mut self.callee_holes {
            edit.shift_span(&mut hole.span);
        }
    }

    fn accumulate_into(&self, out: &mut OwnerFacts) {
        out.plan_failures.extend(self.plan_failures.iter().cloned());
        out.always_plan_failures
            .extend(self.always_plan_failures.iter().cloned());
        out.free_types.extend(self.free_types.iter().cloned());
        out.gaps.extend(self.gaps.iter().cloned());
        out.expr_examined += self.expr_examined;
        out.binding_examined += self.binding_examined;
        out.generics_bound += self.generics_bound;
        out.untyped.extend(self.untyped.iter().cloned());
        out.untyped_bindings += self.untyped_bindings;
        out.expr_slots += self.expr_slots;
        out.binding_slots += self.binding_slots;
        out.generic_bodies += self.generic_bodies;
        out.sites_examined += self.sites_examined;
        out.callee_holes.extend(self.callee_holes.iter().cloned());
        out.unknown_rock_callees
            .extend(self.unknown_rock_callees.iter().cloned());
        for (kind, count) in &self.untyped_as_callee_key {
            *out.untyped_as_callee_key.entry(kind).or_insert(0) += count;
        }
        out.signature_requests += self.signature_requests;
        out.signature_derivations += self.signature_derivations;
        out.signature_slots_cloned += self.signature_slots_cloned;
        out.slot_conversions += self.slot_conversions;
        out.build_stats.add(self.build_stats);
    }

    fn sort_for_output(&mut self) {
        self.free_types.sort_by(|left, right| left.0.cmp(&right.0));
        self.gaps
            .sort_by(|left, right| left.instance.cmp(&right.instance));
        self.plan_failures
            .sort_by(|left, right| left.0.cmp(&right.0));
        self.always_plan_failures
            .sort_by(|left, right| concrete_owner_order(&left.0, &right.0));
        self.untyped.sort_by(|left, right| left.0.cmp(&right.0));
        self.callee_holes.sort_by(|left, right| {
            left.instance
                .cmp(&right.instance)
                .then_with(|| left.site.cmp(&right.site))
                .then_with(|| left.reason.cmp(right.reason))
        });
        self.unknown_rock_callees
            .sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    }
}

type PublishedSignatures = FxHashMap<Instance, Result<ConcreteSignatureHandle, MonoTyReason>>;
type PublishedDomains = BTreeMap<SiteDomain, Result<ConcreteTypeTables, MonoTyReason>>;

fn publish_signatures(
    ws: &Workspace,
    g: &InstanceGraph,
    expanded: &[Instance],
) -> (PublishedSignatures, usize, std::time::Duration) {
    let mut instances = BTreeSet::new();
    for inst in expanded {
        instances.insert(inst.clone());
        if let Some(sites) = g.sites_of(inst) {
            for record in sites.values() {
                if let SiteOutcome::Resolved(target) = &record.outcome {
                    instances.insert(target.clone());
                }
            }
        }
    }
    for contribution in g.contributions.values() {
        for record in contribution.always_call_sites.values() {
            if let SiteOutcome::Resolved(target) = &record.outcome {
                instances.insert(target.clone());
            }
        }
    }
    let started = bone_core::prelude::Instant::now();
    let mut published = FxHashMap::default();
    for instance in instances {
        let result = g
            .substitution(&instance)
            .cloned()
            .ok_or(MonoTyReason::NoNode)
            .and_then(|subst| concrete_signature(ws, g, &instance, &subst))
            .map(std::sync::Arc::new);
        published.insert(instance, result);
    }
    let elapsed = started.elapsed();
    let derivations = published.len();
    (published, derivations, elapsed)
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

fn collect_foreign_domains(g: &InstanceGraph, expanded: &[Instance]) -> BTreeSet<SiteDomain> {
    let mut domains = BTreeSet::new();
    for inst in expanded {
        if let Some(contribution) = g.contributions.get(&Owner::Instance(inst.clone())) {
            domains.extend(contribution.struct_default_domains.iter().cloned());
        }
        if let Some(sites) = g.sites_of(inst) {
            for (site, record) in sites {
                if matches!(&record.outcome, SiteOutcome::Resolved(_)) {
                    domains.insert(site.domain.clone());
                }
            }
        }
    }
    for (owner, contribution) in &g.contributions {
        if !matches!(owner, Owner::AlwaysEmitted(..)) {
            continue;
        }
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
) -> (Vec<InstanceResult>, ConcreteBuildStats) {
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
        .map(
            |instance| match prepare_instance(ws, g, instance, timing_enabled) {
                PreparedInstanceResult::Gap(gap) => InstanceResult::Gap(gap),
                PreparedInstanceResult::Ready(prepared) => InstanceResult::Built(produce_instance(
                    ws,
                    g,
                    prepared,
                    &signatures,
                    &published_domains,
                    timing_enabled,
                )),
            },
        )
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
    let mut published = BTreeMap::new();
    for domain in domains {
        let result = match domain.clone() {
            SiteDomain::Own => continue,
            SiteDomain::StructDefaults(module, def, args) => {
                let owner = BodyOwner::StructDefaults(def);
                let result = ws
                    .module(module)
                    .bodies
                    .get(&owner)
                    .ok_or(MonoTyReason::NoNode)
                    .and_then(|defaults_body| {
                        let defaults_inf = ws
                            .inference(module)
                            .and_then(|ir| ir.per_fn.get(&owner))
                            .ok_or(MonoTyReason::NoNode)?;
                        let defaults_subst =
                            skull_mir::struct_defaults::slot_subst(Some(defaults_inf), &args);
                        concrete_tables(
                            ws,
                            g,
                            defaults_inf,
                            defaults_body,
                            &defaults_subst,
                            std::iter::empty(),
                            stats,
                            timing_enabled,
                        )
                    });
                result
            }
        };
        published.insert(domain, result);
    }
    if let Some(started) = started {
        stats.foreign_domains_ns += started.elapsed().as_nanos() as u64;
    }
    published
}

struct PreparedInstance {
    instance: Instance,
    types: ConcreteTypeTables,
    untyped: Vec<UntypedExpr>,
    type_error: Option<MonoTyReason>,
    facts: OwnerFacts,
    convert_time: std::time::Duration,
    elapsed: std::time::Duration,
    slot_tyids: usize,
    unique_slot_tyids: usize,
}

#[expect(
    clippy::large_enum_variant,
    reason = "Keep successful results inline rather than adding a per-instance allocation"
)]
enum PreparedInstanceResult {
    Gap(MissingBody),
    Ready(PreparedInstance),
}

impl PreparedInstanceResult {
    fn instance(&self) -> &Instance {
        match self {
            Self::Gap(gap) => &gap.instance,
            Self::Ready(prepared) => &prepared.instance,
        }
    }
}

fn prepare_instance(
    ws: &Workspace,
    g: &InstanceGraph,
    inst: Instance,
    timing_enabled: bool,
) -> PreparedInstanceResult {
    let t_inst = bone_core::prelude::Instant::now();
    if g.has_bound_tree_disagreement(&inst) {
        return PreparedInstanceResult::Gap(MissingBody {
            instance: inst,
            gap: BodyGap::InconsistentBounds,
        });
    }
    let Some(subst) = g.substitution(&inst).cloned() else {
        return PreparedInstanceResult::Gap(MissingBody {
            instance: inst,
            gap: BodyGap::NoSubstitution,
        });
    };
    let (inf, body) = match function_body(ws, inst.func) {
        FunctionBody::Ready { inf, body } => (inf, body),
        FunctionBody::NoInference => {
            return PreparedInstanceResult::Gap(MissingBody {
                instance: inst,
                gap: BodyGap::NoInference,
            });
        }
        FunctionBody::NoBody => {
            return PreparedInstanceResult::Gap(MissingBody {
                instance: inst,
                gap: BodyGap::NoBody,
            });
        }
        FunctionBody::StaleInference => {
            return PreparedInstanceResult::Gap(MissingBody {
                instance: inst,
                gap: BodyGap::StaleInference,
            });
        }
    };

    let mut facts = OwnerFacts::default();
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
    facts.expr_examined = converted.expr_examined;
    facts.binding_examined = converted.binding_examined;
    facts.build_stats.source_expr_slots = converted.expr_examined;
    facts.build_stats.source_binding_slots = converted.binding_examined;
    facts.generics_bound = converted.generics_bound;
    facts.slot_conversions = converted.conversions;
    facts.generic_bodies = usize::from(!subst.is_empty());
    let untyped = converted.untyped_exprs;
    facts.untyped_bindings = converted.untyped_bindings.len();
    facts.expr_slots = body.expr_arena.len();
    facts.binding_slots = body.binding_arena.len();
    let failures = converted.failures;
    facts.free_types = failures
        .iter()
        .cloned()
        .map(|failure| (inst.clone(), failure))
        .collect();
    let mut types = ConcreteTypeTables {
        expr_types: converted.expr_types,
        binding_types: converted.binding_types,
        untyped_exprs: untyped.iter().map(|u| u.expr).collect(),
        untyped_bindings: converted.untyped_bindings.iter().copied().collect(),
        ..Default::default()
    };
    for failure in failures {
        match (failure.slot, failure.reason) {
            (Slot::Expr(expr), MonoTyReason::UnresolvedInfer(_)) => {
                types.untyped_exprs.insert(expr);
            }
            (Slot::Binding(binding), MonoTyReason::UnresolvedInfer(_)) => {
                types.untyped_bindings.insert(binding);
            }
            _ => {}
        }
    }
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
        &mut facts.build_stats,
        timing_enabled,
    ) {
        Ok(ty_types) => match concrete_struct_field_types(
            ws,
            g,
            body,
            &types.expr_types,
            &mut facts.build_stats,
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
    PreparedInstanceResult::Ready(PreparedInstance {
        instance: inst,
        types,
        untyped,
        type_error,
        facts,
        convert_time,
        elapsed: t_inst.elapsed(),
        slot_tyids,
        unique_slot_tyids,
    })
}

struct PreparedAlways {
    order: usize,
    module: bone_core::prelude::ModuleId,
    body_owner: BodyOwner,
    owner: Option<ConcreteOwner>,
    types: Option<ConcreteTypeTables>,
    type_error: Option<MonoTyReason>,
    skip: bool,
    facts: OwnerFacts,
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
    order: usize,
    module: bone_core::prelude::ModuleId,
    body_owner: BodyOwner,
    timing_enabled: bool,
) -> PreparedAlways {
    let owner = concrete_owner_for(module, body_owner);
    let Some(body) = ws.module(module).bodies.get(&body_owner) else {
        return PreparedAlways {
            order,
            module,
            body_owner,
            owner,
            types: None,
            type_error: Some(MonoTyReason::NoNode),
            skip: false,
            facts: OwnerFacts::default(),
        };
    };
    if matches!(body_owner, BodyOwner::Global(_)) && body.root.is_none() {
        return PreparedAlways {
            order,
            module,
            body_owner,
            owner,
            types: None,
            type_error: None,
            skip: true,
            facts: OwnerFacts::default(),
        };
    }
    let Some(inf) = ws
        .inference(module)
        .and_then(|ir| ir.per_fn.get(&body_owner))
    else {
        return PreparedAlways {
            order,
            module,
            body_owner,
            owner,
            types: None,
            type_error: Some(MonoTyReason::NoNode),
            skip: false,
            facts: OwnerFacts::default(),
        };
    };
    let mut facts = OwnerFacts::default();
    let declared_types = match body_owner {
        BodyOwner::Global(index) => std::iter::once(ws.module(module).globals[index].ty)
            .collect::<Vec<_>>()
            .into_iter(),
        BodyOwner::Decorator(_)
        | BodyOwner::Function(_)
        | BodyOwner::StructDefaults(_)
        | BodyOwner::Enum(_)
        | BodyOwner::Newtype(_) => Vec::new().into_iter(),
    };
    let result = concrete_tables(
        ws,
        g,
        inf,
        body,
        &MonoSubst::default(),
        declared_types,
        &mut facts.build_stats,
        timing_enabled,
    );
    let (types, type_error) = match result {
        Ok(types) => (Some(types), None),
        Err(reason) => (None, Some(reason)),
    };
    PreparedAlways {
        order,
        module,
        body_owner,
        owner,
        types,
        type_error,
        skip: false,
        facts,
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
        // An own-body site needs no foreign table; it is never inserted, so
        // counting it here would make every such request look unique.
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

fn concrete_plan_from_publication(
    g: &InstanceGraph,
    inst: &Instance,
    body: &ConcreteBody,
    types: ConcreteTypeTables,
    type_error: Option<MonoTyReason>,
    signatures: &PublishedSignatures,
    domains: &PublishedDomains,
    stats: &mut ConcreteBuildStats,
    timing_enabled: bool,
) -> Result<(ConcreteBodyPlan, FxHashSet<Instance>), MonoTyReason> {
    let mut callees = BTreeMap::new();
    let mut callee_signatures = BTreeMap::new();
    let mut candidate_refs = FxHashSet::default();
    let signature_started = timing_enabled.then(bone_core::prelude::Instant::now);
    let callee_rows = counted_callees(body, stats);
    for (site, callee) in callee_rows {
        let instance = callee.instance().clone();
        candidate_refs.insert(instance.clone());
        let signature = signatures
            .get(&instance)
            .ok_or(MonoTyReason::NoNode)?
            .clone()?;
        callee_signatures.insert(site.clone(), signature);
        callees.insert(site.clone(), instance);
    }
    let signature = signatures.get(inst).ok_or(MonoTyReason::NoNode)?.clone()?;
    if let Some(started) = signature_started {
        stats.signature_ns += started.elapsed().as_nanos() as u64;
    }
    let failed_sites = g
        .sites_of(inst)
        .into_iter()
        .flat_map(|sites| sites.iter())
        .filter_map(|(site, record)| match &record.outcome {
            SiteOutcome::Failed(reason) => Some((site.clone(), *reason)),
            SiteOutcome::Resolved(_) => None,
        })
        .collect();
    if let Some(reason) = type_error {
        return Err(reason);
    }
    let default_domains = g
        .contributions
        .get(&Owner::Instance(inst.clone()))
        .into_iter()
        .flat_map(|contribution| contribution.struct_default_domains.iter().cloned());
    let site_domains = counted_callees(body, stats).map(|(site, _)| site.domain.clone());
    let foreign_domains = foreign_domains_from_publication(
        default_domains.chain(site_domains),
        domains,
        stats,
        timing_enabled,
    )?;
    stats.plan_callee_map_inserts += callees.len();
    stats.plan_callee_signature_inserts += callee_signatures.len();
    Ok((
        ConcreteBodyPlan {
            owner: ConcreteOwner::Function(inst.clone()),
            code_identity: Some(skull_mir::code_identity::CodeIdentity::Named(inst.clone())),
            types,
            foreign_domains,
            signature,
            callees,
            callee_signatures,
            failed_sites,
            bound_tree: g
                .bound_tree(inst)
                .map_or_else(Default::default, |tree| tree.to_resolved()),
        },
        candidate_refs,
    ))
}

fn produce_instance(
    ws: &Workspace,
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
        untyped,
        type_error,
        mut facts,
        convert_time,
        elapsed: preparation_time,
        slot_tyids,
        unique_slot_tyids,
    } = prepared;
    let mut concrete = ConcreteBody {
        instance: inst.clone(),
        direct_calls: BTreeMap::new(),
        fn_values: BTreeMap::new(),
        iterator_ops: BTreeMap::new(),
        synthesized_calls: BTreeMap::new(),
    };
    let started = bone_core::prelude::Instant::now();
    callee_tables(ws, g, &inst, &mut concrete, &mut facts);
    let callee_time = started.elapsed();
    let keys: BTreeSet<Idx<Expr>> = counted_callees(&concrete, &mut facts.build_stats)
        .map(|(site, _)| site.expr)
        .collect();
    for untyped_expr in &untyped {
        if keys.contains(&untyped_expr.expr) {
            *facts
                .untyped_as_callee_key
                .entry(untyped_expr.kind)
                .or_insert(0) += 1;
        }
    }
    facts.untyped.extend(
        untyped
            .into_iter()
            .map(|untyped_expr| (inst.clone(), untyped_expr)),
    );
    facts.signature_requests += concrete.direct_calls.len()
        + concrete.fn_values.len()
        + concrete.iterator_ops.len()
        + concrete.synthesized_calls.len()
        + 1;
    let started = bone_core::prelude::Instant::now();
    facts.build_stats.plan_attempts += 1;
    let (plan, candidate_refs) = match concrete_plan_from_publication(
        g,
        &inst,
        &concrete,
        types,
        type_error,
        signatures,
        domains,
        &mut facts.build_stats,
        timing_enabled,
    ) {
        Ok((plan, candidate_refs)) => (Ok(plan), candidate_refs),
        Err(reason) => (Err(reason), FxHashSet::default()),
    };
    let plan_time = started.elapsed();
    if let Err(reason) = &plan {
        facts.build_stats.plan_failures += 1;
        facts.plan_failures.push((inst.clone(), reason.clone()));
    } else {
        facts.build_stats.plan_successes += 1;
    }
    BuiltInstance {
        instance: inst,
        concrete,
        plan,
        candidate_refs,
        facts,
        convert_time,
        callee_time,
        plan_time,
        elapsed: preparation_time + started_instance.elapsed(),
        slot_tyids,
        unique_slot_tyids,
    }
}

struct BuiltAlways {
    order: usize,
    owner: Option<ConcreteOwner>,
    plan: Result<Option<ConcreteBodyPlan>, MonoTyReason>,
    facts: OwnerFacts,
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
        order,
        module,
        body_owner,
        owner,
        types,
        type_error,
        skip,
        mut facts,
    } = prepared;
    facts.build_stats.always_plan_attempts += 1;
    if skip {
        return BuiltAlways {
            order,
            owner,
            plan: Ok(None),
            facts,
        };
    }
    let Some(types) = types else {
        return BuiltAlways {
            order,
            owner,
            plan: Err(type_error.unwrap_or(MonoTyReason::NoNode)),
            facts,
        };
    };
    if let Some(reason) = type_error {
        return BuiltAlways {
            order,
            owner,
            plan: Err(reason),
            facts,
        };
    }
    let contribution = match g
        .contributions
        .get(&Owner::AlwaysEmitted(module, body_owner))
    {
        Some(contribution) => contribution,
        None => {
            return BuiltAlways {
                order,
                owner,
                plan: Err(MonoTyReason::NoNode),
                facts,
            };
        }
    };
    let mut callees = BTreeMap::new();
    let mut callee_signatures = BTreeMap::new();
    let mut failed_sites = BTreeMap::new();
    let signature_started = timing_enabled.then(bone_core::prelude::Instant::now);
    for (site, record) in &contribution.always_call_sites {
        match &record.outcome {
            SiteOutcome::Resolved(instance) => {
                facts.signature_requests += 1;
                let signature = match signatures.get(instance) {
                    Some(Ok(signature)) => signature.clone(),
                    Some(Err(reason)) => {
                        return BuiltAlways {
                            order,
                            owner,
                            plan: Err(reason.clone()),
                            facts,
                        };
                    }
                    None => {
                        return BuiltAlways {
                            order,
                            owner,
                            plan: Err(MonoTyReason::NoNode),
                            facts,
                        };
                    }
                };
                callee_signatures.insert(site.clone(), signature);
                callees.insert(site.clone(), instance.clone());
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
    let foreign_domains = match foreign_domains_from_publication(
        default_domains.chain(site_domains),
        domains,
        &mut facts.build_stats,
        timing_enabled,
    ) {
        Ok(foreign_domains) => foreign_domains,
        Err(reason) => {
            return BuiltAlways {
                order,
                owner,
                plan: Err(reason),
                facts,
            };
        }
    };
    let signature = match body_owner {
        BodyOwner::Global(gid) => {
            let ty = ws.module(module).globals[gid].ty;
            let result = match types.ty_types.get(&ty).copied() {
                Some(result) => result,
                None => {
                    return BuiltAlways {
                        order,
                        owner,
                        plan: Err(MonoTyReason::NoNode),
                        facts,
                    };
                }
            };
            std::sync::Arc::new(ConcreteSignature {
                params: Vec::new(),
                param_tuple_spelled: Vec::new(),
                result: Some(result),
            })
        }
        BodyOwner::Decorator(_) => {
            let body = match ws.module(module).bodies.get(&body_owner) {
                Some(body) => body,
                None => {
                    return BuiltAlways {
                        order,
                        owner,
                        plan: Err(MonoTyReason::NoNode),
                        facts,
                    };
                }
            };
            let mut params = Vec::new();
            if let Some(target) = body.deco_target {
                let Some(ty) = types.binding_types.get(target).copied() else {
                    return BuiltAlways {
                        order,
                        owner,
                        plan: Err(MonoTyReason::NoNode),
                        facts,
                    };
                };
                params.push(ty);
            }
            for binding in body.param_bindings {
                let Some(ty) = types.binding_types.get(binding).copied() else {
                    return BuiltAlways {
                        order,
                        owner,
                        plan: Err(MonoTyReason::NoNode),
                        facts,
                    };
                };
                params.push(ty);
            }
            std::sync::Arc::new(ConcreteSignature {
                param_tuple_spelled: vec![false; params.len()],
                params,
                result: None,
            })
        }
        BodyOwner::Function(_)
        | BodyOwner::StructDefaults(_)
        | BodyOwner::Enum(_)
        | BodyOwner::Newtype(_) => {
            return BuiltAlways {
                order,
                owner,
                plan: Ok(None),
                facts,
            }
        }
    };
    if let Some(started) = signature_started {
        facts.build_stats.signature_ns += started.elapsed().as_nanos() as u64;
    }
    let plan = ConcreteBodyPlan {
        owner: owner
            .clone()
            .expect("always-emitted owner was not global or decorator"),
        code_identity: None,
        types,
        foreign_domains,
        signature,
        callees,
        callee_signatures,
        failed_sites,
        bound_tree: Default::default(),
    };
    facts.build_stats.always_plan_successes += 1;
    BuiltAlways {
        order,
        owner,
        plan: Ok(Some(plan)),
        facts,
    }
}

/// Which threads ran a fanout. Off unless a traced test entry point asks, so
/// the production fanout takes no shared lock per item.
struct WorkerCensus {
    workers: Option<std::sync::Mutex<std::collections::HashSet<std::thread::ThreadId>>>,
}

impl WorkerCensus {
    fn new(tracking: bool) -> Self {
        Self {
            workers: tracking.then(Default::default),
        }
    }

    #[inline]
    fn record(&self) {
        if let Some(workers) = &self.workers {
            workers
                .lock()
                .expect("worker census poisoned")
                .insert(std::thread::current().id());
        }
    }

    fn worker_count(&self) -> usize {
        self.workers.as_ref().map_or(0, |workers| {
            workers.lock().expect("worker census poisoned").len()
        })
    }
}

fn concrete_map<T, U, F>(items: Vec<T>, census: &WorkerCensus, f: F) -> Vec<U>
where
    T: Send,
    U: Send,
    F: Fn(T) -> U + Send + Sync,
{
    use bone_core::prelude::*;

    items
        .into_par_iter()
        .map(|item| {
            census.record();
            f(item)
        })
        .collect()
}

#[cfg(not(feature = "sequential"))]
struct CompletionScheduler {
    ranks: BTreeMap<Instance, usize>,
    state: std::sync::Mutex<CompletionState>,
    ready: std::sync::Condvar,
}

#[cfg(not(feature = "sequential"))]
struct CompletionState {
    arrived: Vec<Instance>,
    selected: Vec<Instance>,
    next: usize,
    completion_order: Vec<Instance>,
    workers: std::collections::HashSet<std::thread::ThreadId>,
    scheduler_entries: usize,
}

#[cfg(not(feature = "sequential"))]
impl CompletionScheduler {
    fn new(ready: &[PreparedInstance]) -> Self {
        let ranks = ready
            .iter()
            .enumerate()
            .map(|(rank, prepared)| (prepared.instance.clone(), rank))
            .collect();
        Self {
            ranks,
            state: std::sync::Mutex::new(CompletionState {
                arrived: Vec::new(),
                selected: Vec::new(),
                next: 0,
                completion_order: Vec::new(),
                workers: std::collections::HashSet::new(),
                scheduler_entries: 0,
            }),
            ready: std::sync::Condvar::new(),
        }
    }

    fn enter(&self, instance: &Instance) {
        let mut state = self.state.lock().expect("completion scheduler poisoned");
        state.scheduler_entries += 1;
        state.workers.insert(std::thread::current().id());
        if state.selected.is_empty() && !state.arrived.iter().any(|arrived| arrived == instance) {
            state.arrived.push(instance.clone());
            if state.arrived.len() == 2 {
                state.arrived.sort_by_key(|arrived| self.ranks[arrived]);
                state.arrived.reverse();
                state.selected = state.arrived.clone();
                self.ready.notify_all();
            }
        }
        loop {
            if state.selected.len() < 2 {
                state = self
                    .ready
                    .wait(state)
                    .expect("completion scheduler poisoned");
                continue;
            }
            if let Some(position) = state
                .selected
                .iter()
                .position(|selected| selected == instance)
            {
                if state.next == position {
                    return;
                }
            } else if state.next == state.selected.len() {
                return;
            }
            state = self
                .ready
                .wait(state)
                .expect("completion scheduler poisoned");
        }
    }

    fn finish(&self, instance: Instance) {
        let mut state = self.state.lock().expect("completion scheduler poisoned");
        state.completion_order.push(instance.clone());
        if let Some(position) = state
            .selected
            .iter()
            .position(|selected| selected == &instance)
        {
            assert_eq!(
                state.next, position,
                "completion scheduler observed a selected result out of order"
            );
            state.next += 1;
            self.ready.notify_all();
        }
    }

    fn trace(&self) -> BuildTrace {
        let state = self.state.lock().expect("completion scheduler poisoned");
        BuildTrace {
            completion_order: state.completion_order.clone(),
            worker_count: state.workers.len(),
            always_worker_count: 0,
            scheduler_entries: state.scheduler_entries,
        }
    }
}

fn produce_instances(
    ws: &Workspace,
    g: &InstanceGraph,
    ready: Vec<PreparedInstance>,
    signatures: &PublishedSignatures,
    domains: &PublishedDomains,
    timing_enabled: bool,
    census: &WorkerCensus,
    schedule: BuildSchedule,
) -> (Vec<InstanceResult>, Option<BuildTrace>) {
    #[cfg(not(feature = "sequential"))]
    use rayon::prelude::*;

    match schedule {
        BuildSchedule::Default => {
            let results = concrete_map(ready, census, |instance| {
                InstanceResult::Built(produce_instance(
                    ws,
                    g,
                    instance,
                    signatures,
                    domains,
                    timing_enabled,
                ))
            });
            (
                results,
                Some(BuildTrace {
                    completion_order: Vec::new(),
                    worker_count: census.worker_count(),
                    always_worker_count: 0,
                    scheduler_entries: 0,
                }),
            )
        }
        #[cfg(not(feature = "sequential"))]
        BuildSchedule::ReverseCompletion => {
            let scheduler = std::sync::Arc::new(CompletionScheduler::new(&ready));
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(2)
                .build()
                .expect("reverse-completion test pool");
            let results = pool.install(|| {
                ready
                    .into_par_iter()
                    .map(|instance| {
                        let key = instance.instance.clone();
                        census.record();
                        scheduler.enter(&key);
                        let result =
                            produce_instance(ws, g, instance, signatures, domains, timing_enabled);
                        scheduler.finish(key);
                        InstanceResult::Built(result)
                    })
                    .collect()
            });
            (results, Some(scheduler.trace()))
        }
    }
}

/// Build a [`ConcreteBody`] for every instance the walk expanded.
///
/// The tables intern into the graph's arena: a second arena would hand out ids
/// that mean nothing beside `Instance::args`.
/// Process-lifetime count of [`build`] invocations. A normal full emission
/// builds the table exactly ONCE and shares it (the shader view and the JS
/// session consume `ConcreteBodies::plans` by handle); a second build inside
/// one emission is the duplicate-work regression this counter exists to gate.
pub fn build_invocations() -> u64 {
    BUILD_INVOCATIONS.with(std::cell::Cell::get)
}

thread_local! {
    static BUILD_INVOCATIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static CONCRETE_DEFAULT_INVOCATIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

pub fn build(ws: &Workspace, g: &InstanceGraph) -> ConcreteBodies {
    build_impl(
        ws,
        g,
        BuildResultOrder::Canonical,
        BuildSchedule::Default,
        false,
    )
    .bodies
}

pub fn concrete_default_invocations() -> u64 {
    CONCRETE_DEFAULT_INVOCATIONS.with(std::cell::Cell::get)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildResultOrder {
    Canonical,
    Reverse,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BuildSchedule {
    Default,
    #[cfg(not(feature = "sequential"))]
    ReverseCompletion,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildTrace {
    pub completion_order: Vec<Instance>,
    pub worker_count: usize,
    pub always_worker_count: usize,
    pub scheduler_entries: usize,
}

struct BuildRun {
    bodies: ConcreteBodies,
    merge_order: Vec<Instance>,
    trace: Option<BuildTrace>,
}

pub fn build_with_result_order(
    ws: &Workspace,
    g: &InstanceGraph,
    order: BuildResultOrder,
) -> (ConcreteBodies, Vec<Instance>) {
    let run = build_impl(ws, g, order, BuildSchedule::Default, true);
    (run.bodies, run.merge_order)
}

/// Default-schedule build that also returns the worker trace. The sequential
/// build's test uses it to prove both fanouts stayed on one worker.
#[cfg(feature = "sequential")]
pub fn build_with_default_trace(ws: &Workspace, g: &InstanceGraph) -> (ConcreteBodies, BuildTrace) {
    let run = build_impl(
        ws,
        g,
        BuildResultOrder::Canonical,
        BuildSchedule::Default,
        true,
    );
    (
        run.bodies,
        run.trace
            .expect("default concrete run did not produce a trace"),
    )
}

#[cfg(not(feature = "sequential"))]
pub fn build_with_reverse_completion(
    ws: &Workspace,
    g: &InstanceGraph,
) -> (ConcreteBodies, Vec<Instance>, BuildTrace) {
    let run = build_impl(
        ws,
        g,
        BuildResultOrder::Canonical,
        BuildSchedule::ReverseCompletion,
        true,
    );
    (
        run.bodies,
        run.merge_order,
        run.trace
            .expect("reverse-completion test run did not produce a trace"),
    )
}

fn build_impl(
    ws: &Workspace,
    g: &InstanceGraph,
    order: BuildResultOrder,
    schedule: BuildSchedule,
    traced: bool,
) -> BuildRun {
    build_published_impl(ws, g, order, schedule, traced)
}

fn build_published_impl(
    ws: &Workspace,
    g: &InstanceGraph,
    order: BuildResultOrder,
    schedule: BuildSchedule,
    traced: bool,
) -> BuildRun {
    BUILD_INVOCATIONS.with(|count| count.set(count.get() + 1));
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
    // Each preparation converts one body's types; the interner takes its
    // write lock only for a type it has not seen, so the bodies convert in
    // parallel. Both collects keep the input order, so the published tables
    // do not depend on the schedule.
    let always_owners = g
        .contributions
        .keys()
        .enumerate()
        .filter_map(|(order, owner)| match owner {
            Owner::AlwaysEmitted(module, body_owner) => Some((order, *module, *body_owner)),
            Owner::Instance(_) => None,
        })
        .collect::<Vec<_>>();
    let (mut prepared, always_inputs) = {
        use bone_core::prelude::*;
        let prepared = expanded
            .into_par_iter()
            .map(|instance| prepare_instance(ws, g, instance, timing_enabled))
            .collect::<Vec<_>>();
        let always_inputs = always_owners
            .into_par_iter()
            .map(|(order, module, body_owner)| {
                prepare_always(ws, g, order, module, body_owner, timing_enabled)
            })
            .collect::<Vec<_>>();
        (prepared, always_inputs)
    };
    let preparation_time = t_preparation.elapsed();

    let publication_instances = prepared
        .iter()
        .map(PreparedInstanceResult::instance)
        .cloned()
        .collect::<Vec<_>>();
    let t_signature_publication = bone_core::prelude::Instant::now();
    let (signatures, signature_derivations, signature_time) =
        publish_signatures(ws, g, &publication_instances);
    let signature_publication_time = t_signature_publication.elapsed();
    let mut publication_stats = ConcreteBuildStats::default();
    let t_foreign_publication = bone_core::prelude::Instant::now();
    let domains = collect_foreign_domains(g, &publication_instances);
    let published_domains =
        publish_foreign_domains(ws, g, domains, &mut publication_stats, timing_enabled);
    publication_stats.signature_ns += signature_time.as_nanos() as u64;
    let foreign_publication_time = t_foreign_publication.elapsed();

    let t_partition = bone_core::prelude::Instant::now();
    if matches!(order, BuildResultOrder::Reverse) {
        prepared.reverse();
    }
    let merge_order = if traced {
        prepared
            .iter()
            .map(|result| result.instance().clone())
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let signatures = std::sync::Arc::new(signatures);
    let published_domains = std::sync::Arc::new(published_domains);
    let mut gaps = Vec::new();
    let mut ready = Vec::new();
    for result in prepared {
        match result {
            PreparedInstanceResult::Gap(gap) => gaps.push(gap),
            PreparedInstanceResult::Ready(instance) => ready.push(instance),
        }
    }
    let partition_time = t_partition.elapsed();
    let ordinary_census = WorkerCensus::new(traced);
    let t_ordinary_parallel = bone_core::prelude::Instant::now();
    let (mut results, completion_trace) = produce_instances(
        ws,
        g,
        ready,
        signatures.as_ref(),
        published_domains.as_ref(),
        timing_enabled,
        &ordinary_census,
        schedule,
    );
    let ordinary_parallel_time = t_ordinary_parallel.elapsed();
    results.extend(gaps.into_iter().map(InstanceResult::Gap));
    results.sort_by(|left, right| left.instance().cmp(right.instance()));

    let t_ordinary_merge = bone_core::prelude::Instant::now();
    let mut out = ConcreteBodies {
        global_build_stats: publication_stats,
        ..ConcreteBodies::default()
    };
    let mut plans = skull_mir::concrete::ConcretePlans::new();
    let mut per_inst = Vec::new();
    let mut t_convert = std::time::Duration::ZERO;
    let mut t_callee = std::time::Duration::ZERO;
    let mut t_plan = std::time::Duration::ZERO;
    let mut candidate_install_time = std::time::Duration::ZERO;
    let mut plan_insert_time = std::time::Duration::ZERO;
    let mut body_insert_time = std::time::Duration::ZERO;
    let mut failure_index_time = std::time::Duration::ZERO;
    let mut owner_fact_insert_time = std::time::Duration::ZERO;
    let mut candidate_ref_rows = 0usize;
    let mut slot_tyids = 0usize;
    let mut unique_slot_tyids = 0usize;
    for result in results {
        match result {
            InstanceResult::Gap(gap) => {
                let mut facts = OwnerFacts::default();
                facts.gaps.push(gap.clone());
                out.instance_facts.insert(gap.instance, facts);
            }
            InstanceResult::Built(built) => {
                let instance = built.instance.clone();
                t_convert += built.convert_time;
                t_callee += built.callee_time;
                t_plan += built.plan_time;
                per_inst.push((instance.clone(), built.elapsed));
                slot_tyids += built.slot_tyids;
                unique_slot_tyids += built.unique_slot_tyids;
                let facts = built.facts;
                let candidate_refs = built.candidate_refs;
                candidate_ref_rows += candidate_refs.len();
                if let Ok(plan) = built.plan {
                    let started = timing_enabled.then(bone_core::prelude::Instant::now);
                    out.install_candidate_refs_owned(&instance, candidate_refs);
                    if let Some(started) = started {
                        candidate_install_time += started.elapsed();
                    }
                    let started = timing_enabled.then(bone_core::prelude::Instant::now);
                    plans.insert(instance.clone(), std::sync::Arc::new(plan));
                    if let Some(started) = started {
                        plan_insert_time += started.elapsed();
                    }
                }
                let started = timing_enabled.then(bone_core::prelude::Instant::now);
                out.bodies.insert(instance.clone(), built.concrete);
                if let Some(started) = started {
                    body_insert_time += started.elapsed();
                }
                let started = timing_enabled.then(bone_core::prelude::Instant::now);
                out.install_failure_index(&facts);
                if let Some(started) = started {
                    failure_index_time += started.elapsed();
                }
                let started = timing_enabled.then(bone_core::prelude::Instant::now);
                out.instance_facts.insert(instance, facts);
                if let Some(started) = started {
                    owner_fact_insert_time += started.elapsed();
                }
            }
        }
    }
    let ordinary_merge_time = t_ordinary_merge.elapsed();

    let t_always_parallel = bone_core::prelude::Instant::now();
    let always_census = WorkerCensus::new(traced);
    let mut always_results = concrete_map(always_inputs, &always_census, |prepared| {
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
    always_results.sort_by_key(|result| result.order);
    for result in always_results {
        let owner = result.owner.clone();
        let mut raw_facts = result.facts;
        if let (Some(owner), Err(reason)) = (&owner, &result.plan) {
            raw_facts
                .always_plan_failures
                .push((owner.clone(), reason.clone()));
        }
        let facts = raw_facts;
        match result.plan {
            Ok(Some(plan)) => {
                out.always_plans.insert(plan.owner.clone(), plan);
            }
            Ok(None) => {}
            Err(_) => {}
        }
        if let Some(owner) = owner {
            out.install_failure_index(&facts);
            out.always_facts.insert(owner, facts);
        }
    }
    let always_merge_time = t_always_merge.elapsed();
    let completion_trace = completion_trace.map(|mut trace| {
        trace.always_worker_count = always_census.worker_count();
        trace
    });
    out.plans = std::sync::Arc::new(plans);
    out.signature_derivations = signature_derivations;
    out.signature_slots_cloned = 0;
    if log::log_enabled!(log::Level::Debug) {
        let intern_after = g.interner().stats();
        let total_time = t_build.elapsed();
        let accounted = expansion_time
            + preparation_time
            + signature_publication_time
            + foreign_publication_time
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
        // The owner records are authoritative; the compatibility fields on
        // `out` are only filled by an explicit snapshot, so read the fold.
        let totals = out.aggregate_facts();
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
            "concrete build: {} expanded over {} decls ({} generic bodies), {} plans, {} gaps; \
             slots {} expr + {} binding; convert {:?}, callee tables {:?}, plans {:?}, \
             total {:?}; phases expansion {:?}, preparation {:?}, signature publication {:?}, \
             foreign publication {:?}, dispatch {:?}, ordinary fanout {:?}, ordinary merge {:?}, \
             always fanout {:?}, always merge {:?}, orchestration {:?}; \
             per-instance p50 {:?} p95 {:?} max {:?}, top [{}]; \
             slot tyids {} / {} unique per-body; mono interns {} attempts -> {} new during build; \
             type nodes {}/{} ({} memo hits, {} unresolved, {:?}), struct literals {} fields {} ({:?}), \
             foreign domains {}/{} ({:?}), signatures {}/{} cloned-slots {} ({:?})",
            per_inst.len(),
            unique_decls.len(),
            totals.generic_bodies,
            out.plans.len(),
            totals.gaps.len(),
            totals.expr_examined,
            totals.binding_examined,
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
            totals.build_stats.type_nodes_examined,
            totals.build_stats.type_node_conversion_attempts,
            totals.build_stats.type_node_memo_hits,
            totals.build_stats.type_node_unresolved,
            std::time::Duration::from_nanos(totals.build_stats.type_nodes_ns),
            totals.build_stats.struct_literals_examined,
            totals.build_stats.struct_field_conversion_attempts,
            std::time::Duration::from_nanos(totals.build_stats.struct_fields_ns),
            totals.build_stats.foreign_domain_requests,
            totals.build_stats.foreign_domain_unique,
            std::time::Duration::from_nanos(totals.build_stats.foreign_domains_ns),
            totals.signature_requests,
            out.signature_derivations,
            totals.signature_slots_cloned,
            std::time::Duration::from_nanos(totals.build_stats.signature_ns),
        );
        log::debug!(
            "concrete merge detail: candidate refs {} rows {:?}, plans {:?}, bodies {:?}, \
             failures {:?}, owner facts {:?}",
            candidate_ref_rows,
            candidate_install_time,
            plan_insert_time,
            body_insert_time,
            failure_index_time,
            owner_fact_insert_time,
        );
    }
    BuildRun {
        bodies: out,
        merge_order,
        trace: completion_trace,
    }
}
