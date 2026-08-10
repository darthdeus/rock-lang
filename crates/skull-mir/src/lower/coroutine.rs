//! **The emission plan of one `co fn` body** (mono cut C+D step D-3).
//!
//! A transformed `co fn` is not one function: it is a step, a resume, an
//! optional advance, a defer per terminal-cleanup site and a body per lifecycle
//! hook, all minted from one base symbol. The graph and emitters consume the
//! instance-owned symbol and concrete result type so they agree on the helper
//! family for each instantiation.
//!
//! `from_instance` handles graph helper accounting before body tables exist;
//! emitted bodies use `from_concrete_plan`, whose result and channel come from
//! the selected concrete signature.

use bone_semantic::inference::Workspace;

use super::{coroutine_yield_channel_mono, YieldChannel};
use crate::concrete::ConcreteBodyPlan;
use crate::instance::Instance;
use crate::monoty::{MonoInterner, MonoTy, MonoTyReason};

/// Every symbol a transformed `co fn` owns, plus its yield channel.
pub struct CoroutinePlan {
    base: String,
    /// The concrete `Coroutine<yield, result, next>` type of this instance.
    /// Emitters use the derived channel below, while retaining the full result
    /// for consumers that need the coroutine contract itself.
    pub result: Option<MonoTy>,
    /// `Some` -> the step writes yield leaves into reused `s.__y*` frame slots
    /// and the boxed `__resume` rebuilds a variant box from them.
    pub channel: Option<YieldChannel>,
}

impl CoroutinePlan {
    /// Build the plan consumed by an emitter from the graph-owned concrete
    /// signature. `base` remains target-owned: Lua uses the instance symbol,
    /// while JS passes the artifact-selected symbol.
    pub fn from_concrete_plan(
        ws: &Workspace,
        interner: &MonoInterner,
        concrete: &ConcreteBodyPlan,
        base: String,
    ) -> Result<Self, MonoTyReason> {
        let result = concrete
            .signature
            .result
            .map(|id| interner.get(id));
        let channel = result.as_ref().and_then(|ty| coroutine_yield_channel_mono(ws, ty));
        Ok(Self { base, result, channel })
    }

    /// Build the graph-side helper plan from an instance's resolved signature.
    pub fn from_instance(
        ws: &Workspace,
        interner: &MonoInterner,
        inst: &Instance,
    ) -> Result<Self, MonoTyReason> {
        let f = &ws.module(inst.func.module).functions[inst.func.func];
        let subst = inst.subst(ws, interner);
        let result = f
            .return_ty
            .map(|ty| interner.with_mut(|arena| arena.convert_decl(ws, ty, &subst)))
            .transpose()?
            .map(|id| interner.get(id));
        let channel = result.as_ref().and_then(|ty| coroutine_yield_channel_mono(ws, ty));
        Ok(Self { base: inst.symbol(ws, interner), result, channel })
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

    /// The helper suffixes known before the MIR transform.
    pub fn declared_suffixes(&self) -> Vec<&'static str> {
        let mut out = vec!["__step", "__resume"];
        if self.channel.is_some() {
            out.push("__advance");
        }
        out
    }
}

/// Plan the emission of the `co fn` definition `inst` names.
///
/// The base symbol is the instance's own -- one encoder, so a specialization's
/// helpers (`__step`, `__resume`, `__advance`) hang off the name its call sites
/// already target rather than off the declaration's.
pub fn plan_coroutine(
    ws: &Workspace,
    interner: &MonoInterner,
    inst: &Instance,
) -> Result<CoroutinePlan, MonoTyReason> {
    CoroutinePlan::from_instance(ws, interner, inst)
}
