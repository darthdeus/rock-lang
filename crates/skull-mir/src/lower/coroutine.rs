//! **The emission plan of one `co fn` body** (mono cut C+D step D-3).
//!
//! A transformed `co fn` is not one function: it is a step, a resume, an
//! optional advance, a defer per terminal-cleanup site and a body per lifecycle
//! hook, all minted from one base symbol. The graph and emitters consume the
//! instance-owned symbol and concrete result type so they agree on the helper
//! family for each instantiation.
//!
//! Graph helper accounting uses `from_concrete_signature` before body tables
//! exist; emitted bodies use `from_concrete_plan`, whose result and channel
//! come from the selected concrete signature.

use bone_semantic::inference::Workspace;

use super::{YieldChannel, coroutine_yield_channel_mono};
use crate::concrete::{ConcreteBodyPlan, ConcreteSignature};
use crate::monoty::{MonoCtor, MonoInterner, MonoTy, MonoTyReason};

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
        Self::from_concrete_signature(ws, interner, &concrete.signature, base)
    }

    pub fn from_concrete_signature(
        ws: &Workspace,
        interner: &MonoInterner,
        signature: &ConcreteSignature,
        base: String,
    ) -> Result<Self, MonoTyReason> {
        let result = signature.result.map(|id| interner.get(id));
        let channel = result
            .as_ref()
            .and_then(|ty| Self::channel_for_concrete_type(ws, ty));
        Ok(Self {
            base,
            result,
            channel,
        })
    }

    pub fn channel_for_concrete_type(ws: &Workspace, ty: &MonoTy) -> Option<YieldChannel> {
        coroutine_yield_channel_mono(ws, ty)
    }

    pub fn has_return_value(&self) -> bool {
        matches!(
            self.result.as_ref(),
            Some(MonoTy::App {
                ctor: MonoCtor::Nominal(name),
                params,
            }) if name.as_str() == "Coroutine"
                && params
                    .get(1)
                    .is_some_and(|result| !matches!(result, MonoTy::Unit))
        )
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
        self.channel
            .as_ref()
            .map(|_| format!("{}__advance", self.base))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monoty::MonoCtor;
    use ustr::Ustr;

    #[test]
    fn concrete_signature_owns_the_coroutine_yield_channel() {
        let ast = skull_parser::parse_module(
            "nobox value Payload { x: float, y: float }\n\
             enum Event { Tick(Payload) }",
            Ustr::from("<test>"),
        )
        .expect("parse");
        let ws = bone_semantic::semantic_passes::semantic_pass(&mut [ast]).expect("semantic");
        let interner = MonoInterner::new();
        let event = interner.intern(MonoTy::App {
            ctor: MonoCtor::Nominal(Ustr::from("Event")),
            params: Vec::new(),
        });
        let result = interner.intern(MonoTy::App {
            ctor: MonoCtor::Nominal(Ustr::from("Coroutine")),
            params: vec![interner.get(event)],
        });
        let signature = ConcreteSignature {
            params: Vec::new(),
            param_tuple_spelled: Vec::new(),
            result: Some(result),
        };
        let plan =
            CoroutinePlan::from_concrete_signature(&ws, &interner, &signature, "event".to_string())
                .expect("concrete coroutine signature");
        assert!(
            plan.channel.is_some(),
            "the concrete yield type must select a channel"
        );
    }
}
