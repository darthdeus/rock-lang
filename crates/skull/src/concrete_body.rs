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

use bone_semantic::hir::{BindingInfo, Body, Expr};
use bone_semantic::inference::{BodyInference, Workspace};
use bone_semantic::prelude::Span;
use comfy_la_arena::{ArenaMap, Idx};
use skull_mir::monoty::{MonoArena, MonoSubst, MonoTyId, MonoTyReason};
use skull_mir::mir::FuncRef;

use crate::instance_graph::{
    boundary_class_for, function_body, is_spanless, BoundaryClass, FunctionBody, Instance,
    InstanceGraph, Site, SiteOp, SiteOutcome,
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
/// The callee tables are keyed by [`Site`] rather than by `Idx<Expr>` wherever
/// one expression carries more than one callee obligation -- a `for` head has
/// an `iter` and a `next`, a column-Map store emits a slot AND a free. Plan S 9
/// writes `Map<ExprId, ..>`; an expression key there would silently keep one of
/// them.
pub struct ConcreteBody {
    pub instance: Instance,
    pub expr_types: ArenaMap<Idx<Expr>, MonoTyId>,
    pub binding_types: ArenaMap<Idx<BindingInfo>, MonoTyId>,
    pub direct_calls: BTreeMap<Idx<Expr>, Callee>,
    pub fn_values: BTreeMap<Idx<Expr>, Callee>,
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
        for (e, c) in &self.direct_calls {
            out.push((Site { expr: *e, op: SiteOp::DirectCall }, c));
        }
        for (e, c) in &self.fn_values {
            out.push((Site { expr: *e, op: SiteOp::FnValue }, c));
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
    let Some(sites) = g.call_sites.get(inst) else { return };
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
                body.direct_calls.insert(site.expr, callee);
            }
            SiteOp::FnValue => {
                body.fn_values.insert(site.expr, callee);
            }
            SiteOp::Synthesized(_) => {
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

/// Build a [`ConcreteBody`] for every instance the walk expanded.
///
/// Takes the graph mutably because the tables intern into ITS arena: a second
/// arena would hand out ids that mean nothing beside `Instance::args`.
pub fn build(ws: &Workspace, g: &mut InstanceGraph) -> ConcreteBodies {
    let mut out = ConcreteBodies::default();
    let expanded: Vec<Instance> = g.expanded.iter().cloned().collect();
    for inst in expanded {
        if g.bound_tree_disagreements.iter().any(|d| d.instance == inst) {
            out.gaps.push(MissingBody { instance: inst, gap: BodyGap::InconsistentBounds });
            continue;
        }
        let Some(subst) = g.substitutions.get(&inst).cloned() else {
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
        let c = convert_body(&mut g.arena, ws, inf, body, &subst);
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
        out.bodies.insert(inst, concrete);
    }
    out
}
