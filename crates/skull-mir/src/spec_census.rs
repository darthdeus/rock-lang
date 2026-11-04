//! Concrete generic call identities recovered from typed call bindings, in a
//! form the instance graph can be checked against.
//!
//! This stays in `skull-mir` because the binding records are owned by lowering;
//! it exposes identity data without giving the diagnostic census a second
//! type conversion. The encoding is
//! [`MonoTy::canonical`](crate::monoty::MonoTy::canonical), which is
//! unambiguous, so the two sides are comparable and no pretty-printing
//! ambiguity can match two different types to each other.
//!
//! A call with an incomplete enclosing substitution has no concrete identity
//! and is omitted; it is counted as unresolved by the caller-side census.

use bone_semantic::hir::BodyOwner;
use bone_semantic::inference::Workspace;
use bone_semantic::prelude::ModuleId;

use crate::lower::shape_census_support::concrete_calls_in_body;
use crate::mir::FuncRef;

/// One concrete generic call identity recovered from typed bindings.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ConcreteCall {
    /// The body containing the call. This distinguishes an identity in a body
    /// the instance graph did not reach from an identity missing in a reached
    /// body.
    pub caller: FuncRef,
    pub func: FuncRef,
    /// Per declared generic, in declaration order. Total: a site with any
    /// unreified slot produces no key at all, so there is nothing partial to
    /// record here.
    pub slots: Vec<String>,
}

/// `FuncRef` is not `Ord`; sort by its raw indices so the census output is
/// order-stable.
fn key(t: &ConcreteCall) -> (u32, u32, u32, u32, &[String]) {
    (
        t.func.module.raw(),
        t.func.func.into_raw().into_u32(),
        t.caller.module.raw(),
        t.caller.func.into_raw().into_u32(),
        &t.slots,
    )
}

impl Ord for ConcreteCall {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        key(self).cmp(&key(other))
    }
}

impl PartialOrd for ConcreteCall {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Every complete generic call identity over `ws`, plus unresolved call sites.
///
/// The enclosing substitution is empty, matching the historical lowering
/// census: a call inside a generic body is read without an instantiation, so
/// this reports source-level identities independently of concrete body planning.
pub fn concrete_calls(ws: &Workspace) -> (Vec<ConcreteCall>, usize) {
    let mut out = Vec::new();
    let mut rejected = 0usize;
    for module in ws.modules.iter() {
        if module.id == ModuleId::builtin() {
            continue;
        }
        let Some(ir) = ws.inference(module.id) else {
            continue;
        };
        for (owner, inf) in ir.per_fn.iter() {
            let BodyOwner::Function(caller_fid) = owner else {
                continue;
            };
            let caller = FuncRef {
                module: module.id,
                func: *caller_fid,
            };
            let (specs, rej) = concrete_calls_in_body(ws, inf);
            rejected += rej;
            for (func, key) in specs {
                out.push(ConcreteCall {
                    caller,
                    func,
                    slots: key.iter().map(crate::monoty::MonoTy::canonical).collect(),
                });
            }
        }
    }
    out.sort();
    out.dedup();
    (out, rejected)
}
