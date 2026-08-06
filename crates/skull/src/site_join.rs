//! **The MIR-call -> `ConcreteBody` join, measured** (plan S 9, Stage 3).
//!
//! Instance-driven emission has to answer "what does THIS call instruction
//! call" from the graph's own resolution rather than by resolving again. That
//! needs a join key, and `MirFunc::call_sites` is it: lowering stamps every
//! call form with the [`skull_mir::site::CallSite`] it came from, and the walk
//! recorded what each site resolved to.
//!
//! This module measures whether that join is TOTAL over the population it
//! claims -- and reports what is outside it BY CAUSE with real counts, never as
//! a silent `filter_map`. An exclusion with an unstated denominator is
//! indistinguishable from a join that quietly does not work.
//!
//! The population is production's own: the instances `InstanceGraph` expanded,
//! each lowered under ITS substitution exactly as `lower_function_spec` lowers
//! it for emission. It is deliberately not "every raw MIR call" -- a global
//! initializer and a decorator body are lowered too, and they are not function
//! instances at all.

use std::collections::BTreeMap;

use bone_semantic::inference::Workspace;
use skull_mir::lower::ScalarCtx;
use skull_mir::mir::{Inst, MirFunc};
use skull_mir::site::{CallSite, Site};

use crate::concrete_body::{Callee, ConcreteBodies};
use crate::instance_graph::InstanceGraph;

/// Why a call instruction is not joined. Every arm is a real population with a
/// count, so "0 unjoined" is a claim about a stated space rather than about
/// whatever happened to survive a filter.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Unjoined {
    /// The call carries no site: it was emitted outside any expression. A fn
    /// prologue does this.
    NoSite,
    /// The call's site names an owner that is not the body being lowered.
    /// Before inlining this cannot happen; it is checked rather than assumed
    /// because a splice makes it possible and the census would otherwise read
    /// the wrong body's tables.
    ForeignOwner,
    /// The owning instance has no `ConcreteBody` (the walk did not expand it).
    NoBody,
    /// The body has no callee recorded at this site. This is the arm that goes
    /// non-zero when lowering synthesizes a call the graph walk does not model.
    SiteNotInBody,
}

impl Unjoined {
    pub fn label(self) -> &'static str {
        match self {
            Self::NoSite => "no-site",
            Self::ForeignOwner => "foreign-owner",
            Self::NoBody => "no-body",
            Self::SiteNotInBody => "site-not-in-body",
        }
    }
}

#[derive(Clone, Default, Debug)]
pub struct SiteJoinCensus {
    /// Instances whose body was lowered -- the space the counts below are over.
    pub bodies_lowered: usize,
    /// Instances the graph expanded but whose body would not lower. Reported
    /// rather than skipped: a shrinking denominator flatters every ratio.
    pub bodies_unlowerable: usize,
    /// CALL INSTRUCTIONS examined. The denominator.
    pub call_insts: usize,
    /// Call instructions that joined to exactly one callee.
    pub joined: usize,
    /// Joined calls by what they resolved to, so the join is not satisfied
    /// entirely by one arm.
    pub joined_rock: usize,
    pub joined_boundary: usize,
    /// Unjoined calls, by cause.
    pub unjoined: BTreeMap<Unjoined, usize>,
    /// A few unjoined examples, for diagnosis. Never the whole finding: the
    /// counts above are.
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
}

/// Every call instruction in `mf` and its nested (hoisted) bodies, with the
/// site recorded for it. Nested fns are included because they are emitted as
/// sibling definitions of the same source body, so their calls are part of the
/// same instance's obligation.
fn call_insts_with_sites(mf: &MirFunc, out: &mut Vec<Option<CallSite>>) {
    for (id, inst) in mf.insts.iter() {
        if matches!(
            inst,
            Inst::Call { .. }
                | Inst::CallMulti { .. }
                | Inst::CallValue { .. }
                | Inst::CallValueMulti { .. }
        ) {
            out.push(mf.call_sites.get(&id).cloned());
        }
    }
    for nf in &mf.nested_fns {
        call_insts_with_sites(nf, out);
    }
}

/// Measure the join over `g`'s expanded instances.
pub fn census(
    ws: &Workspace,
    scx: &ScalarCtx,
    g: &InstanceGraph,
    bodies: &ConcreteBodies,
) -> SiteJoinCensus {
    let mut c = SiteJoinCensus::default();
    for inst in &g.expanded {
        let Some(subst) = g.substitutions.get(inst) else {
            c.bodies_unlowerable += 1;
            continue;
        };
        let mf = match skull_mir::lower::lower_function_spec(
            ws,
            scx,
            inst.func.module,
            inst.func.func,
            subst,
        ) {
            Ok(mf) => mf,
            Err(_) => {
                c.bodies_unlowerable += 1;
                continue;
            }
        };
        c.bodies_lowered += 1;

        // The site -> callee map the graph already resolved for THIS instance.
        let resolved: Option<BTreeMap<Site, &Callee>> = bodies
            .bodies
            .get(inst)
            .map(|b| b.callees().into_iter().collect());

        let mut sites: Vec<Option<CallSite>> = Vec::new();
        call_insts_with_sites(&mf, &mut sites);
        for cs in sites {
            c.call_insts += 1;
            let cause = match (&cs, &resolved) {
                (None, _) => Some(Unjoined::NoSite),
                (Some(cs), _) if cs.owner != inst.func => Some(Unjoined::ForeignOwner),
                (Some(_), None) => Some(Unjoined::NoBody),
                (Some(cs), Some(map)) => match map.get(&cs.site) {
                    Some(Callee::Rock(_)) => {
                        c.joined += 1;
                        c.joined_rock += 1;
                        None
                    }
                    Some(Callee::Boundary(..)) => {
                        c.joined += 1;
                        c.joined_boundary += 1;
                        None
                    }
                    None => Some(Unjoined::SiteNotInBody),
                },
            };
            if let Some(cause) = cause {
                *c.unjoined.entry(cause).or_insert(0) += 1;
                if c.examples.len() < 12 {
                    let where_ = match &cs {
                        Some(cs) => format!("{:?} op {}", cs.site.expr, cs.site.op.name()),
                        None => "<no site>".to_string(),
                    };
                    c.examples.push(format!(
                        "{} in `{}`: {}",
                        cause.label(),
                        g.identity(ws, inst),
                        where_
                    ));
                }
            }
        }
    }
    c
}
