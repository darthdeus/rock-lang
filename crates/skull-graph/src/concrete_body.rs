//! `ConcreteBody` type tables (`docs/monomorphization-plan.md` S 9).
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

use bone_semantic::hir::{BindingInfo, Body, BodyOwner, Expr, TypeExpr};
use bone_semantic::inference::{BodyInference, Workspace};
use bone_semantic::prelude::{Span, TyId};
use comfy_la_arena::{ArenaMap, Idx};
use skull_mir::concrete::{
    ConcreteAlwaysPlans, ConcreteBodyPlan, ConcreteOwner, ConcreteSignature, ConcreteTypeTables,
};
use skull_mir::monoty::{MonoArena, MonoCtor, MonoSubst, MonoTy, MonoTyId, MonoTyReason, TyEnv};
use skull_mir::mir::FuncRef;
use rustc_hash::{FxHashMap, FxHashSet};
use ustr::Ustr;

use crate::instance_graph::{
    boundary_class_for, function_body, is_spanless, BoundaryClass, FunctionBody, Instance,
    InstanceGraph, Owner, Site, SiteDomain, SiteOp, SiteOutcome,
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

/// One instance's concrete type and callee tables.
///
/// EVERY callee table is keyed by [`Site`] rather than by `Idx<Expr>`. One
/// expression can carry more than one callee obligation -- a `for` head has an
/// `iter` and a `next`, a column-Map store emits a slot AND a free -- and an
/// index is only meaningful against ONE arena, which a struct-default
/// initializer's is not. Plan S 9 writes `Map<ExprId, ..>`; an expression key
/// there silently keeps one of each pair and reads a foreign index against the
/// owner's body.
pub struct ConcreteBody {
    pub instance: Instance,
    pub expr_types: ArenaMap<Idx<Expr>, MonoTyId>,
    pub binding_types: ArenaMap<Idx<BindingInfo>, MonoTyId>,
    pub direct_calls: BTreeMap<Site, Callee>,
    pub fn_values: BTreeMap<Site, Callee>,
    pub iterator_ops: BTreeMap<Site, ConcreteIteratorOp>,
    /// Calls no source expression contains. Not in `direct_calls`: the same
    /// expression can hold a source call and a synthesized one, and they are
    /// different obligations with different callees.
    pub synthesized_calls: BTreeMap<Site, Callee>,
}

impl ConcreteBody {
    /// Every callee this body names, with the site that named it.
    pub fn callees(&self) -> Vec<(Site, &Callee)> {
        let mut out: Vec<(Site, &Callee)> = Vec::new();
        for (s, c) in &self.direct_calls {
            out.push((s.clone(), c));
        }
        for (s, c) in &self.fn_values {
            out.push((s.clone(), c));
        }
        for (s, op) in &self.iterator_ops {
            out.push((s.clone(), &op.callee));
        }
        for (s, c) in &self.synthesized_calls {
            out.push((s.clone(), c));
        }
        out
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
#[derive(Default)]
pub struct ConcreteBodies {
    pub bodies: BTreeMap<Instance, ConcreteBody>,
    /// The plan production lowering consumes. It is stored beside the census
    /// body so a graph update can retract a plan with its instance.
    pub plans: BTreeMap<Instance, ConcreteBodyPlan>,
    pub always_plans: ConcreteAlwaysPlans,
    pub plan_failures: Vec<(Instance, MonoTyReason)>,
    pub always_plan_failures: Vec<(ConcreteOwner, MonoTyReason)>,
    /// Slots that stayed non-concrete, WITH their instance. Flat rather than
    /// per-body because a body that failed a slot still has a table for the
    /// rest, and the failures are the finding.
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
}

impl ConcreteBodies {
    pub fn plan_for(&self, instance: &Instance) -> Option<&ConcreteBodyPlan> {
        self.plans.get(instance)
    }

    pub fn plan_for_owner(&self, owner: &ConcreteOwner) -> Option<&ConcreteBodyPlan> {
        self.always_plans.get(owner)
    }

    pub fn census(&self) -> BodyCensus {
        let mut c = BodyCensus {
            bodies: self.bodies.len(),
            expr_examined: self.expr_examined,
            binding_examined: self.binding_examined,
            generics_bound: self.generics_bound,
            generic_bodies: self.generic_bodies,
            untyped_exprs: self.untyped.len(),
            untyped_bindings: self.untyped_bindings,
            expr_slots: self.expr_slots,
            binding_slots: self.binding_slots,
            sites_examined: self.sites_examined,
            unknown_rock_callees: self.unknown_rock_callees.len(),
            untyped_as_callee_key: self.untyped_as_callee_key.clone(),
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
        for h in &self.callee_holes {
            let k = c.callee_holes.entry(h.reason).or_default();
            k.0 += 1;
            k.1 += usize::from(is_spanless(&h.span));
        }
        for (_, f) in &self.free_types {
            *c.free_by_reason.entry(f.reason.bucket()).or_insert(0) += 1;
            *c.free_by_slot.entry(f.slot.kind()).or_insert(0) += 1;
            c.spanless += usize::from(is_spanless(&f.span));
        }
        for (_, u) in &self.untyped {
            *c.untyped_by_kind.entry(u.kind).or_insert(0) += 1;
        }
        for g in &self.gaps {
            *c.gaps.entry(g.gap.label()).or_insert(0) += 1;
        }
        c
    }

    /// Untyped slots as `<instance> <expr kind> at <file>:<line>`.
    pub fn render_untyped(&self, ws: &Workspace, g: &InstanceGraph) -> Vec<String> {
        self.untyped
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
            .collect()
    }

    /// Callee holes as `<instance> <op> <reason> at <file>:<line>`.
    pub fn render_callee_holes(&self, ws: &Workspace, g: &InstanceGraph) -> Vec<String> {
        self.callee_holes
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
            .collect()
    }

    /// Free types as `<instance> <slot-kind> <bucket> at <file>:<line>`, for a
    /// report or a failing assertion.
    pub fn render_free_types(&self, ws: &Workspace, g: &InstanceGraph) -> Vec<String> {
        self.free_types
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
            .collect()
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
}

impl BodyCensus {
    pub fn free_total(&self) -> usize {
        self.free_by_reason.values().sum()
    }

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
    let head = d.split(|c: char| !c.is_alphanumeric() && c != '_').next().unwrap_or("?");
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
    arena: &mut MonoArena,
    ws: &Workspace,
    inf: &BodyInference,
    body: &Body,
    subst: &MonoSubst,
) -> Converted {
    let mut c = Converted::default();
    for (e, ty) in inf.expr_types.iter() {
        c.expr_examined += 1;
        match arena.convert_counting(ws, inf, *ty, subst, &mut c.generics_bound) {
            Ok(id) => {
                c.expr_types.insert(e, id);
            }
            Err(reason) => c.failures.push(FreeType {
                slot: Slot::Expr(e),
                reason,
                span: body.expr_spans.get(e).copied().unwrap_or_else(Span::unknown),
            }),
        }
    }

    for (e, expr) in body.expr_arena.iter() {
        if inf.expr_types.get(e).is_none() {
            c.untyped_exprs.push(UntypedExpr {
                expr: e,
                kind: expr_kind(expr),
                span: body.expr_spans.get(e).copied().unwrap_or_else(Span::unknown),
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
        match arena.convert_counting(ws, inf, *ty, subst, &mut c.generics_bound) {
            Ok(id) => {
                c.binding_types.insert(b, id);
            }
            Err(reason) => c.failures.push(FreeType {
                slot: Slot::Binding(b),
                reason,
                span: body.binding_spans.get(b).copied().unwrap_or_else(Span::unknown),
            }),
        }
    }
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
    out: &mut ConcreteBodies,
) {
    let Some(sites) = g.sites_of(inst) else { return };
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
                    ConcreteIteratorOp { kind: IteratorOpKind::Iter, callee },
                );
            }
            SiteOp::ForNext(_) => {
                body.iterator_ops.insert(
                    site.clone(),
                    ConcreteIteratorOp { kind: IteratorOpKind::Next, callee },
                );
            }
            SiteOp::ImplicitIter => {
                body.iterator_ops.insert(
                    site.clone(),
                    ConcreteIteratorOp { kind: IteratorOpKind::ImplicitIter, callee },
                );
            }
            SiteOp::BoundNext(_) => {
                body.iterator_ops.insert(
                    site.clone(),
                    ConcreteIteratorOp { kind: IteratorOpKind::BoundNext, callee },
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
    g.interner().with_mut(|arena| {
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
        Ok(ConcreteSignature { params, param_tuple_spelled, result })
    })
}

fn declared_tuple_param(ws: &Workspace, ty: TyId) -> Result<bool, MonoTyReason> {
    match TyEnv::Decl.decl_node(ws, ty)? {
        TypeExpr::Tuple { .. } => Ok(true),
        TypeExpr::Nullable(inner) => {
            Ok(matches!(TyEnv::Decl.decl_node(ws, ty.with_idx(inner))?, TypeExpr::Tuple { .. }))
        }
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
) -> Result<ConcreteTypeTables, MonoTyReason> {
    let converted = g.interner().with_mut(|arena| convert_body(arena, ws, inf, body, subst));
    if let Some(failure) = converted.failures.first() {
        return Err(failure.reason.clone());
    }
    let ty_types = concrete_ty_types(ws, g, inf, subst)?;
    let struct_field_types = concrete_struct_field_types(ws, g, body, &converted.expr_types)?;
    Ok(ConcreteTypeTables {
        expr_types: converted.expr_types,
        binding_types: converted.binding_types,
        ty_types,
        struct_field_types,
    })
}

fn concrete_struct_field_types(
    ws: &Workspace,
    g: &InstanceGraph,
    body: &Body,
    expr_types: &ArenaMap<Idx<Expr>, MonoTyId>,
) -> Result<ArenaMap<Idx<Expr>, FxHashMap<Ustr, MonoTyId>>, MonoTyReason> {
    let mut out = ArenaMap::default();
    for (expr, node) in body.expr_arena.iter() {
        let Expr::StructLiteral { name, .. } = node else { continue };
        let sid = *ws.struct_by_name.get(&name).ok_or(MonoTyReason::NoNode)?;
        let def = &ws.modules[sid.module.index()].structs[sid.def];
        let params = match expr_types.get(expr).copied() {
            Some(ty_id) => {
                let concrete = g.interner().get(ty_id);
                let (payload, _) = concrete.peel_nullable();
                let MonoTy::App { ctor: MonoCtor::Nominal(actual), params } = payload else {
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
        let fields = g.interner().with_mut(|arena| {
            def.fields
                .iter()
                .map(|field| {
                    arena
                        .convert_decl(ws, field.ty, &subst)
                        .map(|ty| (field.name, ty))
                })
                .collect::<Result<FxHashMap<_, _>, _>>()
        })?;
        out.insert(expr, fields);
    }
    Ok(out)
}

fn concrete_ty_types(
    ws: &Workspace,
    g: &InstanceGraph,
    inf: &BodyInference,
    subst: &MonoSubst,
) -> Result<rustc_hash::FxHashMap<TyId, MonoTyId>, MonoTyReason> {
    let mut nodes = FxHashSet::default();
    for ty in inf.expr_types.values().chain(inf.binding_types.values()) {
        collect_type_nodes(ws, inf, *ty, &mut nodes);
    }
    g.interner().with_mut(|arena| {
        nodes
            .into_iter()
            .map(|ty| arena.convert(ws, inf, ty, subst).map(|id| (ty, id)))
            .collect::<Result<rustc_hash::FxHashMap<_, _>, _>>()
    })
}

fn collect_type_nodes(
    ws: &Workspace,
    inf: &BodyInference,
    ty: TyId,
    out: &mut FxHashSet<TyId>,
) {
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

fn foreign_domain_tables<I>(
    ws: &Workspace,
    g: &InstanceGraph,
    domains: I,
) -> Result<BTreeMap<SiteDomain, ConcreteTypeTables>, MonoTyReason>
where
    I: IntoIterator<Item = SiteDomain>,
{
    let mut out = BTreeMap::new();
    for domain in domains {
        let SiteDomain::StructDefaults(module, def, args) = domain else { continue };
        let owner = BodyOwner::StructDefaults(def);
        let defaults_body = ws
            .module(module)
            .bodies
            .get(&owner)
            .ok_or(MonoTyReason::NoNode)?;
        let defaults_inf = ws
            .inference(module)
            .and_then(|ir| ir.per_fn.get(&owner))
            .ok_or(MonoTyReason::NoNode)?;
        let defaults_subst = skull_mir::struct_defaults::slot_subst(Some(defaults_inf), &args);
        let tables = concrete_tables(ws, g, defaults_inf, defaults_body, &defaults_subst)?;
        out.insert(SiteDomain::StructDefaults(module, def, args), tables);
    }
    Ok(out)
}

fn concrete_plan(
    ws: &Workspace,
    g: &InstanceGraph,
    inst: &Instance,
    subst: &MonoSubst,
    inf: &BodyInference,
    hir_body: &Body,
    body: &ConcreteBody,
) -> Result<ConcreteBodyPlan, MonoTyReason> {
    let mut callees = BTreeMap::new();
    let mut callee_signatures = BTreeMap::new();
    for (site, callee) in body.callees() {
        let instance = callee.instance().clone();
        let subst = g.substitution(&instance).cloned().ok_or(MonoTyReason::NoNode)?;
        callee_signatures.insert(site.clone(), concrete_signature(ws, g, &instance, &subst)?);
        callees.insert(site, instance);
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
    let ty_types = concrete_ty_types(ws, g, inf, subst)?;
    let struct_field_types = concrete_struct_field_types(ws, g, hir_body, &body.expr_types)?;
    let default_domains = g
        .contributions
        .get(&Owner::Instance(inst.clone()))
        .into_iter()
        .flat_map(|c| c.struct_default_domains.iter().cloned());
    let site_domains = body.callees().into_iter().map(|(site, _)| site.domain);
    let foreign_domains = foreign_domain_tables(ws, g, default_domains.chain(site_domains))?;
    Ok(ConcreteBodyPlan {
        owner: ConcreteOwner::Function(inst.clone()),
        types: ConcreteTypeTables {
            expr_types: body.expr_types.clone(),
            binding_types: body.binding_types.clone(),
            ty_types,
            struct_field_types,
        },
        foreign_domains,
        signature: concrete_signature(ws, g, inst, subst)?,
        callees,
        callee_signatures,
        failed_sites,
        bound_tree: g.bound_tree(inst).map_or_else(Default::default, |tree| tree.to_resolved()),
    })
}

fn concrete_always_plan(
    ws: &Workspace,
    g: &InstanceGraph,
    module: bone_semantic::prelude::ModuleId,
    owner: BodyOwner,
) -> Result<Option<ConcreteBodyPlan>, MonoTyReason> {
    let body = ws.module(module).bodies.get(&owner).ok_or(MonoTyReason::NoNode)?;
    if matches!(owner, BodyOwner::Global(_)) && body.root.is_none() {
        return Ok(None);
    }
    let inf = ws
        .inference(module)
        .and_then(|ir| ir.per_fn.get(&owner))
        .ok_or(MonoTyReason::NoNode)?;
    let types = concrete_tables(ws, g, inf, body, &MonoSubst::default())?;
    let contribution = g
        .contributions
        .get(&Owner::AlwaysEmitted(module, owner))
        .ok_or(MonoTyReason::NoNode)?;
    let mut callees = BTreeMap::new();
    let mut callee_signatures = BTreeMap::new();
    let mut failed_sites = BTreeMap::new();
    for (site, record) in &contribution.always_call_sites {
        match &record.outcome {
            SiteOutcome::Resolved(instance) => {
                let subst = g.substitution(instance).cloned().ok_or(MonoTyReason::NoNode)?;
                callee_signatures.insert(
                    site.clone(),
                    concrete_signature(ws, g, instance, &subst)?,
                );
                callees.insert(site.clone(), instance.clone());
            }
            SiteOutcome::Failed(reason) => {
                failed_sites.insert(site.clone(), *reason);
            }
        }
    }
    let default_domains = contribution.struct_default_domains.iter().cloned();
    let site_domains = contribution.always_call_sites.keys().map(|site| site.domain.clone());
    let foreign_domains = foreign_domain_tables(ws, g, default_domains.chain(site_domains))?;
    let signature = match owner {
        BodyOwner::Global(gid) => {
            let result = g.interner().with_mut(|arena| {
                arena.convert_decl(ws, ws.module(module).globals[gid].ty, &MonoSubst::default())
            })?;
            ConcreteSignature {
                params: Vec::new(),
                param_tuple_spelled: Vec::new(),
                result: Some(result),
            }
        }
        BodyOwner::Decorator(did) => {
            let mut params = Vec::new();
            if let Some(target) = body.deco_target {
                params.push(*types.binding_types.get(target).ok_or(MonoTyReason::NoNode)?);
            }
            for binding in body.param_bindings {
                params.push(*types.binding_types.get(binding).ok_or(MonoTyReason::NoNode)?);
            }
            let _ = did;
            ConcreteSignature {
                param_tuple_spelled: vec![false; params.len()],
                params,
                result: None,
            }
        }
        _ => return Ok(None),
    };
    let owner = match owner {
        BodyOwner::Global(index) => ConcreteOwner::Global {
            module,
            index: index.into_raw().into_u32(),
        },
        BodyOwner::Decorator(index) => ConcreteOwner::Decorator {
            module,
            index: index.into_raw().into_u32(),
        },
        _ => unreachable!(),
    };
    Ok(Some(ConcreteBodyPlan {
        owner,
        types,
        foreign_domains,
        signature,
        callees,
        callee_signatures,
        failed_sites,
        bound_tree: Default::default(),
    }))
}

/// Build a [`ConcreteBody`] for every instance the walk expanded.
///
/// The tables intern into the graph's arena: a second arena would hand out ids
/// that mean nothing beside `Instance::args`.
pub fn build(ws: &Workspace, g: &InstanceGraph) -> ConcreteBodies {
    let mut out = ConcreteBodies::default();
    let expanded: Vec<Instance> = g.expanded().cloned().collect();
    for inst in expanded {
        if g.has_bound_tree_disagreement(&inst) {
            out.gaps.push(MissingBody { instance: inst, gap: BodyGap::InconsistentBounds });
            continue;
        }
        let Some(subst) = g.substitution(&inst).cloned() else {
            out.gaps.push(MissingBody { instance: inst, gap: BodyGap::NoSubstitution });
            continue;
        };
        let (inf, body) = match function_body(ws, inst.func) {
            FunctionBody::Ready { inf, body } => (inf, body),
            FunctionBody::NoInference => {
                out.gaps.push(MissingBody { instance: inst, gap: BodyGap::NoInference });
                continue;
            }
            FunctionBody::NoBody => {
                out.gaps.push(MissingBody { instance: inst, gap: BodyGap::NoBody });
                continue;
            }
            FunctionBody::StaleInference => {
                out.gaps.push(MissingBody { instance: inst, gap: BodyGap::StaleInference });
                continue;
            }
        };
        let c = g.interner().with_mut(|a| convert_body(a, ws, inf, body, &subst));
        out.expr_examined += c.expr_examined;
        out.binding_examined += c.binding_examined;
        out.generics_bound += c.generics_bound;
        out.generic_bodies += usize::from(!subst.is_empty());
        let untyped = c.untyped_exprs;
        out.untyped_bindings += c.untyped_bindings.len();
        out.expr_slots += body.expr_arena.len();
        out.binding_slots += body.binding_arena.len();
        for f in c.failures {
            out.free_types.push((inst.clone(), f));
        }
        let mut concrete = ConcreteBody {
            instance: inst.clone(),
            expr_types: c.expr_types,
            binding_types: c.binding_types,
            direct_calls: BTreeMap::new(),
            fn_values: BTreeMap::new(),
            iterator_ops: BTreeMap::new(),
            synthesized_calls: BTreeMap::new(),
        };
        callee_tables(ws, g, &inst, &mut concrete, &mut out);

        // The question the type-table slice deferred: does a callee entry need
        // an expression type for a slot inference recorded none for? Answered by
        // intersecting the two populations rather than by reasoning about the
        // kinds -- a kind absent here needs no type because no table asks about
        // it, and a kind present here is answered by the site's own oracle.
        let keys: BTreeSet<Idx<Expr>> =
            concrete.callees().into_iter().map(|(s, _)| s.expr).collect();
        for u in &untyped {
            if keys.contains(&u.expr) {
                *out.untyped_as_callee_key.entry(u.kind).or_insert(0) += 1;
            }
        }
        for u in untyped {
            out.untyped.push((inst.clone(), u));
        }
        match concrete_plan(ws, g, &inst, &subst, inf, body, &concrete) {
            Ok(plan) => {
                out.plans.insert(inst.clone(), plan);
            }
            Err(reason) => out.plan_failures.push((inst.clone(), reason)),
        }
        out.bodies.insert(inst, concrete);
    }
    for owner in g.contributions.keys() {
        let Owner::AlwaysEmitted(module, body_owner) = owner else { continue };
        match concrete_always_plan(ws, g, *module, *body_owner) {
            Ok(Some(plan)) => {
                out.always_plans.insert(plan.owner.clone(), plan);
            }
            Ok(None) => {}
            Err(reason) => {
                let owner = match body_owner {
                    BodyOwner::Global(index) => ConcreteOwner::Global {
                        module: *module,
                        index: index.into_raw().into_u32(),
                    },
                    BodyOwner::Decorator(index) => ConcreteOwner::Decorator {
                        module: *module,
                        index: index.into_raw().into_u32(),
                    },
                    _ => continue,
                };
                out.always_plan_failures.push((owner, reason));
            }
        }
    }
    out
}
