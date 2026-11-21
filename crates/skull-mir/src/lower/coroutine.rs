use bone_core::workspace::Workspace;

use super::{EnumFlatShape, coroutine_yield_channel_mono};
use crate::concrete::{ConcreteBodyPlan, ConcreteSignature};
use crate::monoty::{MonoCtor, MonoInterner, MonoTy, MonoTyReason};

pub struct CoroutinePlan {
    base: String,
    pub result: Option<MonoTy>,
    /// `Some` -> the step writes yield leaves into reused `s.__y*` frame slots and
    /// the boxed `__resume` rebuilds a variant box from them.
    pub channel: Option<EnumFlatShape>,
}

impl CoroutinePlan {
    /// `base` is target-owned: Lua uses the instance symbol, JS the
    /// artifact-selected one.
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

    pub(crate) fn channel_for_concrete_type(ws: &Workspace, ty: &MonoTy) -> Option<EnumFlatShape> {
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

    pub fn base(&self) -> &str {
        &self.base
    }

    pub fn step(&self) -> String {
        format!("{}__step", self.base)
    }

    pub fn resume(&self) -> String {
        format!("{}__resume", self.base)
    }

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
}
