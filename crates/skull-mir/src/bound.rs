//! A bound-conformance tree with every leaf carried in terms an emitted body
//! can read on its own.
//!
//! [`bone_core::prelude::BoundTree`] is stated in the CALLER's vocabulary:
//! `BoundNext::Concrete::next_generics` maps a callee `VarId` to a `TyId` in the
//! caller's own type arena. That is exactly what a splice needs -- it runs where
//! the caller's [`BodyInference`] is in scope -- and it is why a raw tree cannot
//! be the thing that travels to a standalone emission: the consumer would have
//! to recover which body's arena the ids belong to, and two callers' trees for
//! ONE instantiation are not comparable at all.
//!
//! [`ResolvedBoundTree`] is the same fact with the arena-dependent half already
//! spent: each concrete leaf carries the concrete [`Instance`] its own
//! instantiation resolves to. It is therefore self-contained, `Eq`, and
//! a function of the instantiation rather than of the caller -- which is what
//! makes it sound to key by emitted body and to CHECK for agreement when two
//! sites demand the same one.

use bone_core::workspace::{BodyInference, Workspace};

use crate::instance::Instance;
use crate::monoty::MonoInterner;

/// How one bounded generic index of a callee resolves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolvedBoundNext {
    /// A concrete `next` witness and how its own bounded generics resolve in
    /// turn. The non-generic case is `Instance::base`, so every consumer reads
    /// one identity rather than re-pairing a declaration with an optional key.
    Concrete {
        instance: Instance,
        sub: ResolvedBoundTree,
    },
    /// Still the enclosing body's own bounded generic. Resolvable only where
    /// that body's incoming tree is known -- at its splice, or at its
    /// standalone instantiation.
    CallerGeneric(usize),
}

/// Per bounded-generic index of a callee: how its placeholder `next` calls
/// resolve. Empty for an unbounded callee.
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

/// A specialized body some emitted body asks for: which declaration, at which
/// instantiation, and -- when it is `Iterator`-bounded -- the tree its own
/// placeholder `next` calls resolve through.
///
/// The tree is a function of the INSTANTIATION, not of the caller, which is the
/// invariant `skull::instance_graph`'s per-instance `bound_trees` measures. It
/// still travels here rather than being looked up there, because emission has
/// no instance graph; the agreement CHECK on the demand map is what keeps the
/// invariant from being taken on faith.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpecDemand {
    /// The concrete body demanded -- declaration plus instantiation, as one
    /// identity rather than as a pair a consumer has to re-pair.
    pub inst: Instance,
    pub tree: ResolvedBoundTree,
}

#[derive(Clone, Copy)]
pub struct BoundCtx<'a> {
    pub inf: Option<&'a BodyInference>,
    pub incoming: &'a ResolvedBoundTree,
    /// The build's one arena, because a resolved leaf NAMES a concrete body and
    /// naming one is interning one.
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
