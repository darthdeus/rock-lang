use bone_core::workspace::Workspace;

use super::{EnumFlatShape, coroutine_yield_channel_mono};
use crate::concrete::{ConcreteBodyPlan, ConcreteSignature};
use crate::monoty::{MonoCtor, MonoInterner, MonoTy, MonoTyReason};

pub struct CoroutinePlan {
    base: String,
    pub result: Option<MonoTy>,
    pub channel: Option<EnumFlatShape>,
}

impl CoroutinePlan {
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

    pub fn hook(&self, slot: &str) -> String {
        format!("{}{slot}", self.base)
    }

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
        let event_name = ws
            .defs
            .iter()
            .find_map(|(key, meta)| (meta.name.as_str() == "Event").then_some(*key))
            .expect("Event definition");
        let event = interner.intern(MonoTy::App {
            ctor: MonoCtor::Nominal(event_name),
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
