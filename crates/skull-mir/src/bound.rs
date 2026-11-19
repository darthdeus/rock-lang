use bone_core::workspace::BodyInference;

use crate::instance::Instance;
use crate::monoty::MonoInterner;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolvedBoundNext {
    Concrete {
        instance: Instance,
        sub: ResolvedBoundTree,
    },
    CallerGeneric(usize),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResolvedBoundTree {
    pub next: std::collections::BTreeMap<usize, ResolvedBoundNext>,
}

impl ResolvedBoundTree {
    pub fn is_empty(&self) -> bool {
        self.next.is_empty()
    }

    pub fn get(&self, generic_idx: usize) -> Option<&ResolvedBoundNext> {
        self.next.get(&generic_idx)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpecDemand {
    pub inst: Instance,
    pub tree: ResolvedBoundTree,
}

#[derive(Clone, Copy)]
pub struct BoundCtx<'a> {
    pub inf: Option<&'a BodyInference>,
    pub incoming: &'a ResolvedBoundTree,
    pub interner: &'a MonoInterner,
}
