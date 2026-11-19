use bone_core::workspace::{BodyInference, Workspace};

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

pub fn concrete_body_ctx<'a>(
    ws: &'a Workspace,
    mid: bone_core::prelude::ModuleId,
    owner: bone_core::hir::BodyOwner,
    incoming: &'a ResolvedBoundTree,
    interner: &'a MonoInterner,
) -> BoundCtx<'a> {
    BoundCtx {
        inf: ws.inference(mid).and_then(|r| r.per_fn.get(&owner)),
        incoming,
        interner,
    }
}
