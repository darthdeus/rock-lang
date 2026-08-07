//! **The emission plan of one `co fn` body** (mono cut C+D step D-3).
//!
//! A transformed `co fn` is not one function: it is a step, a resume, an
//! optional advance, a defer per terminal-cleanup site and a body per lifecycle
//! hook, all minted from one base symbol. Both emitters used to spell that base
//! themselves -- the Lua driver from `mangled(ws, self_ref)`, the JS driver from
//! the name its caller happened to pass -- and each restated when an `__advance`
//! exists. A generic `co fn` therefore had one set of helper symbols for every
//! instantiation, which is the collision
//! `InstanceGraph::colliding_coroutine_helpers` reports.
//!
//! One [`CoroutinePlan`] per [`EmittedBodyId`] answers both: the base symbol is
//! the body's own (a specialization's `spec_fn_name`, not its declaration's),
//! and the yield channel is computed once.
//!
//! The channel is read from the DECLARED return type, not from the
//! instantiation. That is the standing rule -- `__advance` is the concrete
//! consumer's fast path, and a consumer selects it from the declared
//! `Coroutine<Y, ..>` it can see, so resolving `Y` per instantiation would let a
//! specialized body publish a channel its callers do not know to read.

use bone_semantic::inference::Workspace;

use super::{coroutine_yield_channel, EmittedBodyId, YieldChannel};
use crate::mir::FuncRef;

/// Every symbol a transformed `co fn` owns, plus its yield channel.
pub struct CoroutinePlan {
    base: String,
    /// `Some` -> the step writes yield leaves into reused `s.__y*` frame slots
    /// and the boxed `__resume` rebuilds a variant box from them.
    pub channel: Option<YieldChannel>,
}

impl CoroutinePlan {
    /// Plan the emission of `decl`'s body under `base`.
    ///
    /// The base is an argument because JS roots carry a spelling their caller
    /// assigns (`emit_with_closure` takes `(FuncRef, String)` pairs, so a `co fn`
    /// root can be emitted as `main`), while luajit derives it from the emitted
    /// body. What is NOT an argument is any other symbol: every helper is minted
    /// here, from this one base.
    pub fn new(ws: &Workspace, decl: FuncRef, base: String) -> Self {
        let f = &ws.module(decl.module).functions[decl.func];
        Self { base, channel: f.return_ty.and_then(|t| coroutine_yield_channel(ws, t)) }
    }

    /// The constructor's own name -- the symbol a call site targets.
    pub fn base(&self) -> &str {
        &self.base
    }

    pub fn step(&self) -> String {
        format!("{}__step", self.base)
    }

    pub fn resume(&self) -> String {
        format!("{}__resume", self.base)
    }

    /// `None` when there is no frame-slot channel: the constructor emits
    /// `__advance` only for one, and stating that here is what keeps the minting
    /// side and the emitting side from disagreeing about whether the symbol
    /// exists.
    pub fn advance(&self) -> Option<String> {
        self.channel.as_ref().map(|_| format!("{}__advance", self.base))
    }

    pub fn defer(&self, k: usize) -> String {
        format!("{}__defer{k}", self.base)
    }

    /// `slot` is the hook's frame field (`__on_killed`, ...), which is also its
    /// name suffix.
    pub fn hook(&self, slot: &str) -> String {
        format!("{}{slot}", self.base)
    }

    /// The suffixes this plan owns that are derivable WITHOUT the MIR transform:
    /// defers and hooks come from `CoroLowered`, so a HIR-level consumer
    /// (`skull::instance_graph`) can only name these.
    pub fn declared_suffixes(&self) -> Vec<&'static str> {
        let mut out = vec!["__step", "__resume"];
        if self.channel.is_some() {
            out.push("__advance");
        }
        out
    }
}

/// Plan the emission of the `co fn` definition `body` names.
///
/// `Shared` gets the declaration's mangled name -- today's shared snapshot --
/// and a specialization gets its own `spec_fn_name`, which is the name its call
/// sites already target.
pub fn plan_coroutine(ws: &Workspace, body: &EmittedBodyId) -> CoroutinePlan {
    let decl = body.decl();
    let f = &ws.module(decl.module).functions[decl.func];
    // Not `unwrap_or(mangle_fn)`: `EmittedBodyId::new` maps an all-`None` key to
    // `Shared`, so a `Specialized` id always has a name and a miss would mean the
    // two rules disagree.
    let base = match body.key() {
        None => crate::mangle::mangle_fn(ws, f),
        Some(key) => crate::mangle::spec_fn_name(ws, f, key)
            .expect("a Specialized body has a non-empty key, so it has a spec name"),
    };
    CoroutinePlan::new(ws, decl, base)
}
