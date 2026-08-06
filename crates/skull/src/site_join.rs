//! **The MIR-call -> `ConcreteBody` join, measured** (plan S 9, Stage 3).
//!
//! Instance-driven emission has to answer "what does THIS call instruction
//! call" from the graph's own resolution rather than by resolving again. That
//! needs a join key, and `MirFunc::call_sites` is it: lowering stamps every
//! call form with the [`skull_mir::site::CallSite`] it came from, and the walk
//! recorded what each site resolved to.
//!
//! **The phase is POST-TRANSFORM, and that is load-bearing.** Emission renders
//! the body that comes out of [`crate::mir_emit::prepare_instance_body`] --
//! inlined, cleaned, closure-converted, FORL-recovered -- so that is the only
//! body whose calls are the ones actually emitted. Measuring the raw lowering
//! output would count calls inlining deletes and miss calls it splices in, and
//! the spliced ones are exactly the hard case: they belong to the CALLEE
//! instance and join through the callee's `ConcreteBody`.
//!
//! **Reachability is part of the population.** Cleanup leaves unreachable
//! blocks in the arena, and their instructions are never emitted. Counting them
//! would inflate the denominator with calls no backend renders.
//!
//! Every exclusion is reported BY CAUSE with a real count, never as a silent
//! `filter_map`: an exclusion with an unstated denominator is indistinguishable
//! from a join that quietly does not work.
//!
//! **The join FAILS CLOSED, and verifies the CALLEE.** [`resolve_owner`]
//! demands an exact `(declaration, substitution)` match against the graph's own
//! record, so an owner the graph does not have is reported rather than matched
//! to another instance of the same declaration. And a site resolving is not
//! enough: the resolved callee must equal the function the MIR call actually
//! targets ([`Unjoined::CalleeMismatch`]), because a right site pointing at a
//! wrong callee is the one failure instance-driven emission cannot tolerate.
//!
//! It is NOT yet an oracle. Current populations, the mutation results and the
//! structural blocker are recorded in `docs/parked/mono.md` -- a live number in
//! a source comment goes stale silently.

use std::collections::{BTreeMap, HashMap};

use bone_semantic::inference::Workspace;
use skull_mir::lower::ScalarCtx;
use skull_mir::mir::{Inst, MirFunc};
use skull_mir::monoty::MonoSubst;
use skull_mir::site::{CallSite, OwnerInstance, Site};

use crate::concrete_body::{Callee, ConcreteBodies};
use crate::instance_graph::{Instance, InstanceGraph};

/// Why a call instruction is not joined. Every arm is a real population with a
/// count, so "0 unjoined" is a claim about a stated space rather than about
/// whatever happened to survive a filter.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Unjoined {
    /// The call carries no site: it was emitted outside any expression.
    NoSite,
    /// The call's site names an owner index the body's owner table does not
    /// have. A splice that failed to re-intern its owner lands here rather than
    /// silently reading the caller's entry 0.
    OwnerNotInTable,
    /// The owner instance corresponds to no `Instance` in the graph -- the
    /// substitution a body was lowered under is not one the walk recorded.
    OwnerNotAnInstance,
    /// The owning instance has no `ConcreteBody` (the walk did not expand it).
    NoBody,
    /// The body has no callee recorded at this site. This is the arm that goes
    /// non-zero when lowering synthesizes a call the graph walk does not model.
    SiteNotInBody,
    /// The site resolved, but to a DIFFERENT function than the call actually
    /// targets. Without this arm a wrong callee carrying a right site counts as
    /// a join, which is the one failure mode instance-driven emission cannot
    /// tolerate: the site is only useful if it selects the callee the emitted
    /// code calls.
    CalleeMismatch,
    /// A call form `Builder::emit_cur` does not stamp (`CallBoundMulti`), so it
    /// can carry no site at all.
    UnstampedForm,
}

impl Unjoined {
    pub fn label(self) -> &'static str {
        match self {
            Self::NoSite => "no-site",
            Self::OwnerNotInTable => "owner-not-in-table",
            Self::OwnerNotAnInstance => "owner-not-an-instance",
            Self::NoBody => "no-body",
            Self::SiteNotInBody => "site-not-in-body",
            Self::CalleeMismatch => "callee-mismatch",
            Self::UnstampedForm => "unstamped-form",
        }
    }
}

/// Why an expanded instance contributed no calls at all.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum BodySkip {
    /// No substitution recorded, so there is nothing to lower under.
    NoSubstitution,
    /// Lowering rejected the body.
    LowerFailed,
    /// The transform sequence rejected the body.
    PrepareFailed,
    /// A coroutine. Its emission path is a different sequence
    /// (`emit_coroutine`, no inlining) and this chunk does not claim it.
    Coroutine,
    /// A declaration skull does not lower a body for at all (extern, an opaque
    /// `inline(lua)`/`inline(js)` body, or an `Iterator`-bounded fn that exists
    /// only inlined at its call sites).
    NoLoweredBody,
}

impl BodySkip {
    pub fn label(self) -> &'static str {
        match self {
            Self::NoSubstitution => "no-substitution",
            Self::LowerFailed => "lower-failed",
            Self::PrepareFailed => "prepare-failed",
            Self::Coroutine => "coroutine",
            Self::NoLoweredBody => "no-lowered-body",
        }
    }
}

#[derive(Clone, Default, Debug)]
pub struct SiteJoinCensus {
    /// Instances whose body was lowered AND prepared -- the space the call
    /// counts below are over.
    pub bodies_measured: usize,
    /// Expanded instances that contributed no calls, by cause. Reported rather
    /// than skipped: a shrinking denominator flatters every ratio.
    pub bodies_skipped: BTreeMap<BodySkip, usize>,
    /// REACHABLE call instructions examined. The denominator.
    pub call_insts: usize,
    /// Call instructions that joined to exactly one callee.
    pub joined: usize,
    /// Joined calls by what they resolved to, so the join is not satisfied
    /// entirely by one arm.
    pub joined_rock: usize,
    pub joined_boundary: usize,
    /// Joined calls whose owner is NOT the body being measured -- i.e. spliced
    /// in by inlining and joined through the callee instance's own body. A zero
    /// here over a corpus that inlines means the hard case is untested.
    pub joined_spliced: usize,
    /// Joined calls whose resolved callee was CHECKED against the FuncRef the
    /// MIR call actually targets, and agreed. The subset of `joined` that is
    /// evidence about callee identity rather than only about site presence.
    pub joined_callee_verified: usize,
    /// Joined calls whose target could not be checked: an indirect call names a
    /// value, not a declaration. Reported so `joined_callee_verified` has a
    /// stated complement rather than an implied one.
    pub joined_callee_unverifiable: usize,
    /// Unjoined calls, by cause.
    pub unjoined: BTreeMap<Unjoined, usize>,
    /// Unjoined calls broken down by the OPERATION they carry, so an unjoined
    /// population cannot hide behind one summary number.
    pub unjoined_by_op: BTreeMap<&'static str, usize>,
    /// A few unjoined examples, for diagnosis. Never the whole finding.
    pub examples: Vec<String>,
}

impl SiteJoinCensus {
    pub fn unjoined_total(&self) -> usize {
        self.unjoined.values().sum()
    }

    /// The accounting identity: every examined call instruction is joined or
    /// has exactly one stated cause.
    pub fn accounts(&self) -> bool {
        self.joined + self.unjoined_total() == self.call_insts
    }

    pub fn merge(&mut self, other: &SiteJoinCensus) {
        self.bodies_measured += other.bodies_measured;
        self.call_insts += other.call_insts;
        self.joined += other.joined;
        self.joined_rock += other.joined_rock;
        self.joined_boundary += other.joined_boundary;
        self.joined_spliced += other.joined_spliced;
        self.joined_callee_verified += other.joined_callee_verified;
        self.joined_callee_unverifiable += other.joined_callee_unverifiable;
        for (k, v) in &other.bodies_skipped {
            *self.bodies_skipped.entry(*k).or_insert(0) += v;
        }
        for (k, v) in &other.unjoined {
            *self.unjoined.entry(*k).or_insert(0) += v;
        }
        for (k, v) in &other.unjoined_by_op {
            *self.unjoined_by_op.entry(k).or_insert(0) += v;
        }
        for e in &other.examples {
            if self.examples.len() < 12 {
                self.examples.push(e.clone());
            }
        }
    }
}

/// What one reachable call instruction contributes to the census: the site
/// recorded for it (if any) and the FuncRef it actually targets.
///
/// `target` is `None` for an indirect call (`CallValue`), which names a value
/// rather than a declaration, and for `CallBoundMulti`, whose target is a
/// placeholder the inliner resolves.
struct CallRow {
    site: Option<CallSite>,
    target: Option<skull_mir::mir::FuncRef>,
    /// A call form `Builder::emit_cur` does not stamp at all. Counted here so
    /// it is a NAMED exclusion rather than an invisible one -- it was outside
    /// the denominator entirely until it was measured.
    unstamped_form: bool,
}

/// Every REACHABLE call instruction in `mf` and its nested (hoisted) bodies.
///
/// Nested fns are included because closure conversion splits them out of this
/// body and they are emitted as its siblings, so their calls are part of the
/// same instance's emitted output.
fn reachable_call_rows(mf: &MirFunc, out: &mut Vec<CallRow>) {
    for b in skull_mir::cfg::reachable_blocks_from(mf, mf.entry) {
        for &id in &mf.blocks[b].insts {
            let (target, unstamped_form) = match &mf.insts[id] {
                Inst::Call { func, .. } | Inst::CallMulti { func, .. } => (Some(*func), false),
                Inst::CallValue { .. } | Inst::CallValueMulti { .. } => (None, false),
                // Emitted by a bounded loop and NOT stamped by `emit_cur`, so
                // it can never carry a site. In the denominator regardless: a
                // call form outside the population is a silent hole, not a
                // clean measurement.
                Inst::CallBoundMulti { .. } => (None, true),
                _ => continue,
            };
            out.push(CallRow { site: mf.call_sites.get(&id).cloned(), target, unstamped_form });
        }
    }
    for nf in &mf.nested_fns {
        reachable_call_rows(nf, out);
    }
}

/// `(declaration, substitution) -> Instance`, so an owner recorded by lowering
/// can be resolved to the graph's instance identity.
///
/// Built from `InstanceGraph::substitutions`, which is the graph's own record,
/// so this is a lookup rather than a second derivation of instance identity.
fn owner_index(g: &InstanceGraph) -> HashMap<skull_mir::mir::FuncRef, Vec<(MonoSubst, Instance)>> {
    let mut ix: HashMap<skull_mir::mir::FuncRef, Vec<(MonoSubst, Instance)>> = HashMap::new();
    for (inst, subst) in &g.substitutions {
        ix.entry(inst.func).or_default().push((subst.clone(), inst.clone()));
    }
    ix
}

fn resolve_owner<'a>(
    ix: &'a HashMap<skull_mir::mir::FuncRef, Vec<(MonoSubst, Instance)>>,
    owner: &OwnerInstance,
) -> Option<&'a Instance> {
    ix.get(&owner.func)?.iter().find(|(s, _)| *s == owner.subst).map(|(_, i)| i)
}

/// Whether skull lowers a standalone body for this declaration at all.
///
/// Delegates to [`crate::mir_emit::is_cached_body`], the definition loop's OWN
/// admission rule, plus the presence of a body to lower. A restatement of that
/// rule here would be free to be weaker and silently drift, which is how the
/// census would end up measuring a population emission does not have.
fn has_lowered_body(ws: &Workspace, fref: skull_mir::mir::FuncRef) -> bool {
    let f = &ws.module(fref.module).functions[fref.func];
    crate::mir_emit::is_cached_body(f)
        && ws
            .module(fref.module)
            .bodies
            .contains_key(&bone_semantic::hir::BodyOwner::Function(fref.func))
}

/// Measure the join over `g`'s expanded instances, at the phase emission
/// consumes.
pub fn census(
    ws: &Workspace,
    scx: &ScalarCtx,
    g: &InstanceGraph,
    bodies: &ConcreteBodies,
) -> SiteJoinCensus {
    let mut c = SiteJoinCensus::default();
    let ix = owner_index(g);
    let mut cache = skull_transform::InlineCache::default();

    for inst in &g.expanded {
        let f = &ws.module(inst.func.module).functions[inst.func.func];
        let skip = |c: &mut SiteJoinCensus, why: BodySkip| {
            *c.bodies_skipped.entry(why).or_insert(0) += 1;
        };
        if f.is_coroutine {
            skip(&mut c, BodySkip::Coroutine);
            continue;
        }
        if !has_lowered_body(ws, inst.func) {
            skip(&mut c, BodySkip::NoLoweredBody);
            continue;
        }
        let Some(subst) = g.substitutions.get(inst) else {
            skip(&mut c, BodySkip::NoSubstitution);
            continue;
        };
        let mut mf = match skull_mir::lower::lower_function_spec(
            ws,
            scx,
            inst.func.module,
            inst.func.func,
            subst,
        ) {
            Ok(mf) => mf,
            Err(_) => {
                skip(&mut c, BodySkip::LowerFailed);
                continue;
            }
        };
        // THE emitted body: the same sequence `emit_specialized_callees` runs.
        let name = skull_mir::mangle::mangle_fn(ws, f);
        if crate::mir_emit::prepare_instance_body(
            ws,
            scx,
            inst.func,
            subst,
            &name,
            &mut mf,
            &mut cache,
        )
        .is_err()
        {
            skip(&mut c, BodySkip::PrepareFailed);
            continue;
        }
        c.bodies_measured += 1;

        let mut rows: Vec<CallRow> = Vec::new();
        reachable_call_rows(&mf, &mut rows);
        for row in rows {
            c.call_insts += 1;
            let (cause, op) = classify(&mut c, &mf, &ix, bodies, inst, &row);
            let cs = &row.site;
            if let Some(cause) = cause {
                *c.unjoined.entry(cause).or_insert(0) += 1;
                *c.unjoined_by_op.entry(op).or_insert(0) += 1;
                let per_cause = c.examples.iter().filter(|e| e.starts_with(cause.label())).count();
                if per_cause < 3 {
                    let target_name = row
                        .target
                        .map(|t| ws.module(t.module).functions[t.func].name.to_string())
                        .unwrap_or_else(|| "<indirect>".into());
                    let detail = match &cs {
                        Some(cs) => {
                            let owner = mf.site_owners.get(cs.owner.0 as usize).map(|o| {
                                let f = &ws.module(o.func.module).functions[o.func.func];
                                format!(
                                    "owner `{}` subst {}",
                                    f.name,
                                    if o.subst.is_empty() {
                                        "EMPTY".to_string()
                                    } else {
                                        format!("{}", o.subst.len())
                                    }
                                )
                            });
                            format!(
                                "{:?} op {} ({})",
                                cs.site.expr,
                                cs.site.op.name(),
                                owner.unwrap_or_else(|| "<owner missing>".into())
                            )
                        }
                        None => "<no site>".to_string(),
                    };
                    c.examples.push(format!(
                        "{} in `{}`: {detail} -> target `{target_name}`",
                        cause.label(),
                        g.identity(ws, inst),
                    ));
                }
            }
        }
    }
    c
}

/// Resolve one call instruction, counting a success into `c` and returning the
/// cause plus the operation label when it does not join.
///
/// A join is only a join when the resolved callee AGREES with the function the
/// call actually targets. Checking only that the site exists in the owner's
/// tables would accept a right site pointing at a wrong callee, and that is the
/// exact defect instance-driven emission would inherit.
fn classify(
    c: &mut SiteJoinCensus,
    mf: &MirFunc,
    ix: &HashMap<skull_mir::mir::FuncRef, Vec<(MonoSubst, Instance)>>,
    bodies: &ConcreteBodies,
    measured: &Instance,
    row: &CallRow,
) -> (Option<Unjoined>, &'static str) {
    if row.unstamped_form {
        return (Some(Unjoined::UnstampedForm), "<unstamped>");
    }
    let Some(cs) = &row.site else { return (Some(Unjoined::NoSite), "<none>") };
    let op = cs.site.op.name();
    let Some(owner) = mf.site_owners.get(cs.owner.0 as usize) else {
        return (Some(Unjoined::OwnerNotInTable), op);
    };
    let Some(owner_inst) = resolve_owner(ix, owner) else {
        return (Some(Unjoined::OwnerNotAnInstance), op);
    };
    let Some(body) = bodies.bodies.get(owner_inst) else {
        return (Some(Unjoined::NoBody), op);
    };
    let resolved: BTreeMap<Site, &Callee> = body.callees().into_iter().collect();
    let Some(callee) = resolved.get(&cs.site) else {
        return (Some(Unjoined::SiteNotInBody), op);
    };
    // The identity check. `Callee::Rock` and `Callee::Boundary` both name an
    // instance, and that instance's declaration is what the emitted call must
    // target.
    if let Some(target) = row.target {
        if callee.instance().func != target {
            return (Some(Unjoined::CalleeMismatch), op);
        }
        c.joined_callee_verified += 1;
    } else {
        c.joined_callee_unverifiable += 1;
    }
    c.joined += 1;
    match callee {
        Callee::Rock(_) => c.joined_rock += 1,
        Callee::Boundary(..) => c.joined_boundary += 1,
    }
    if owner_inst != measured {
        c.joined_spliced += 1;
    }
    (None, op)
}
