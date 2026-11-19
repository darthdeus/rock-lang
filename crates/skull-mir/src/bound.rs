//! A raw `BoundTree` names types in the caller's arena. A resolved tree carries
//! concrete instances instead, so it is a function of the instantiation and can
//! be keyed by emitted body and checked for agreement.
use bone_core::workspace::BodyInference;

use crate::instance::Instance;
use crate::monoty::MonoInterner;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolvedBoundNext {
    /// The non-generic case is `Instance::base`.
    Concrete {
        instance: Instance,
        sub: ResolvedBoundTree,
    },
    /// Resolvable only where the enclosing body's incoming tree is known: at its
    /// splice or its standalone instantiation.
    CallerGeneric(usize),
}

/// Empty for an unbounded callee.
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

/// The tree is a function of the instantiation, not of the caller; the
/// agreement check on the demand map enforces that.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpecDemand {
    pub inst: Instance,
    pub tree: ResolvedBoundTree,
}

#[derive(Clone, Copy)]
pub struct BoundCtx<'a> {
    pub inf: Option<&'a BodyInference>,
    pub incoming: &'a ResolvedBoundTree,
    /// A resolved leaf names a concrete body, and naming one interns it.
    pub interner: &'a MonoInterner,
}
