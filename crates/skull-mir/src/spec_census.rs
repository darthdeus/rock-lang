//! What today's conditional specializer ACTUALLY produces, in a form the
//! Stage-0d shadow graph can be reconciled against
//! (`docs/monomorphization-plan.md` S 15a: "every current specialized edge
//! appears in the complete graph, and every additional graph edge is
//! classified").
//!
//! Lives in `skull-mir` because `compute_call_spec` is internal to
//! `lower::shape`; this exposes the RESULT encoded as strings rather than
//! widening that module. The encoding is
//! [`MonoTy::canonical`](crate::monoty::MonoTy::canonical), which is
//! unambiguous, so the two sides are comparable and no pretty-printing
//! ambiguity can match two different types to each other.
//!
//! A `None` slot means the key names no instantiation for that generic, which
//! now happens only inside a SHARED generic body (the enclosing substitution
//! does not bind it). It is treated as a WILDCARD when reconciling -- the shadow
//! side is total, so it always has a concrete type where this side has a hole.

use bone_semantic::hir::{BodyOwner, FnBackend};
use bone_semantic::inference::Workspace;
use bone_semantic::prelude::ModuleId;

use crate::lower::shape_census_support::specializations_in_body;
use crate::mir::FuncRef;

/// One specialization today's `compute_call_spec` decides to perform.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TodaySpec {
    /// The body the call site lives in. Needed to classify a reconciliation
    /// miss: a specialization inside a body the shadow walk never REACHED is a
    /// difference in reachability, not a missing instance.
    pub caller: FuncRef,
    pub func: FuncRef,
    /// Per declared generic, in declaration order. `None` = no concrete type
    /// reaches this slot, i.e. the enclosing body is a shared generic snapshot.
    pub slots: Vec<Option<String>>,
}

/// `FuncRef` is not `Ord`; sort by its raw indices so the census output is
/// order-stable.
fn key(t: &TodaySpec) -> (u32, u32, u32, u32, &[Option<String>]) {
    (
        t.func.module.raw(),
        t.func.func.into_raw().into_u32(),
        t.caller.module.raw(),
        t.caller.func.into_raw().into_u32(),
        &t.slots,
    )
}

impl Ord for TodaySpec {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        key(self).cmp(&key(other))
    }
}

impl PartialOrd for TodaySpec {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Every specialization the CURRENT machinery produces over `ws`, plus the
/// number of call sites whose spec decision was an outright rejection.
///
/// The enclosing substitution is empty, matching the site-aware validator
/// (`native_buffer_alloc_reject`): a call inside a generic body is read without
/// an instantiation, which is exactly the shared-body condition being measured.
pub fn today_specializations(ws: &Workspace, target: FnBackend) -> (Vec<TodaySpec>, usize) {
    let mut out = Vec::new();
    let mut rejected = 0usize;
    for module in ws.modules.iter() {
        if module.id == ModuleId::builtin() {
            continue;
        }
        let Some(ir) = ws.inference(module.id) else { continue };
        for (owner, inf) in ir.per_fn.iter() {
            let BodyOwner::Function(caller_fid) = owner else { continue };
            let caller = FuncRef { module: module.id, func: *caller_fid };
            let (specs, rej) = specializations_in_body(ws, inf, target);
            rejected += rej;
            for (func, key) in specs {
                out.push(TodaySpec {
                    caller,
                    func,
                    slots: key.iter().map(|s| s.as_ref().map(crate::monoty::MonoTy::canonical)).collect(),
                });
            }
        }
    }
    out.sort();
    out.dedup();
    (out, rejected)
}
