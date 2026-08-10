//! **The one identity of a function at one instantiation.**
//!
//! This lives in `skull-mir` -- the lowest crate every consumer shares -- for
//! the same reason [`crate::site`] does: so there is ONE vocabulary rather than
//! two equivalent ones with a translation between them. It was in
//! `skull::instance_graph`, which put it above emission and left emission to key
//! its bodies on a parallel `SpecKey`.
//!
//! The placement is the only thing that moved. `skull::instance_graph` remains
//! the AUTHORITY: it builds instances against its [`crate::monoty::MonoArena`],
//! and an [`Instance`]'s `args` are ids into that arena, meaningless without it.
//! Nothing here mints an arena-free instance of the same shape -- that would be
//! the second identity this stage exists to remove.

use crate::mir::FuncRef;
use crate::monoty::MonoTyId;

/// A function at one concrete instantiation. Non-generic functions get an
/// instance too (empty `args`), so no consumer can reintroduce `if generic`.
///
/// Empty `args` is a NON-GENERIC instance and nothing else. It is not a
/// "shared body" sentinel; that conflation is what `SpecKey`'s narrowing
/// removed, and reintroducing it here would put it back one layer down.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Instance {
    pub func: FuncRef,
    pub args: Vec<MonoTyId>,
}

/// A `FuncRef` as an ORDERED map key.
///
/// `FuncRef` is not `Ord`, and ordering by it would be ordering by arena
/// position anyway. This is `(module, function index)` -- stable within a build
/// and used only to make iteration deterministic, the same contract
/// [`ord_key`] states for an [`Instance`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DeclKey(pub FuncRef);

impl DeclKey {
    pub fn raw(self) -> (u32, u32) {
        (self.0.module.raw(), self.0.func.into_raw().into_u32())
    }
}

impl Ord for DeclKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.raw().cmp(&other.raw())
    }
}

impl PartialOrd for DeclKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// `FuncRef` is not `Ord`, and ordering by it would be ordering by arena
/// position anyway. The key is `(module, function index, args)` -- stable within
/// a build, and only ever used to make iteration order deterministic; the
/// MANIFEST is sorted by rendered symbol, which is stable across builds.
fn ord_key(i: &Instance) -> (u32, u32, &[MonoTyId]) {
    (i.func.module.raw(), i.func.func.into_raw().into_u32(), &i.args)
}

impl Ord for Instance {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        ord_key(self).cmp(&ord_key(other))
    }
}

impl PartialOrd for Instance {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
