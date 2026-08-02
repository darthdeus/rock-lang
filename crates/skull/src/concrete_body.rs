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
//! [`MonoTy`]: skull_mir::monoty::MonoTy

use std::collections::BTreeMap;

use bone_semantic::hir::{BindingInfo, Body, Expr};
use bone_semantic::inference::{BodyInference, Workspace};
use bone_semantic::prelude::Span;
use comfy_la_arena::{ArenaMap, Idx};
use skull_mir::monoty::{MonoArena, MonoSubst, MonoTyId, MonoTyReason};

use crate::instance_graph::{function_body, is_spanless, FunctionBody, Instance, InstanceGraph};

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

/// One instance's concrete type tables.
pub struct ConcreteBody {
    pub instance: Instance,
    pub expr_types: ArenaMap<Idx<Expr>, MonoTyId>,
    pub binding_types: ArenaMap<Idx<BindingInfo>, MonoTyId>,
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
}

impl BodyGap {
    pub fn label(self) -> &'static str {
        match self {
            Self::NoSubstitution => "no-substitution",
            Self::NoInference => "no-inference",
            Self::NoBody => "no-body",
            Self::StaleInference => "stale-inference",
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
            ..Default::default()
        };
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
}

impl BodyCensus {
    pub fn free_total(&self) -> usize {
        self.free_by_reason.values().sum()
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

/// Build a [`ConcreteBody`] for every instance the walk expanded.
///
/// Takes the graph mutably because the tables intern into ITS arena: a second
/// arena would hand out ids that mean nothing beside `Instance::args`.
pub fn build(ws: &Workspace, g: &mut InstanceGraph) -> ConcreteBodies {
    let mut out = ConcreteBodies::default();
    let expanded: Vec<Instance> = g.expanded.iter().cloned().collect();
    for inst in expanded {
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
        for u in c.untyped_exprs {
            out.untyped.push((inst.clone(), u));
        }
        out.untyped_bindings += c.untyped_bindings.len();
        out.expr_slots += body.expr_arena.len();
        out.binding_slots += body.binding_arena.len();
        for f in c.failures {
            out.free_types.push((inst.clone(), f));
        }
        out.bodies.insert(
            inst.clone(),
            ConcreteBody {
                instance: inst,
                expr_types: c.expr_types,
                binding_types: c.binding_types,
            },
        );
    }
    out
}
