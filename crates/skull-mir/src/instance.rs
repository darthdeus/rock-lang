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

use bone_semantic::inference::Workspace;

use crate::mir::FuncRef;
use crate::monoty::{MonoInterner, MonoSubst, MonoTy, MonoTyId};

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

impl Instance {
    /// The one instance of a NON-GENERIC declaration.
    ///
    /// Named rather than spelled `Instance { func, args: vec![] }` at each site
    /// so the empty argument list reads as what it is. It is not a sentinel: a
    /// non-generic function has no type arguments, and asking whether a
    /// declaration is generic is a question about the DECLARATION
    /// ([`Self::is_generic`]), never about how many arguments happen to be here.
    pub fn base(func: FuncRef) -> Self {
        Self { func, args: Vec::new() }
    }

    /// Whether the DECLARATION this instantiates is generic.
    ///
    /// Deliberately reads `f.generics`, not `self.args`. The two agree for every
    /// instance the graph builds, and the point of asking the declaration is
    /// that an argument list that disagreed would be a defect rather than a
    /// different answer.
    pub fn is_generic(&self, ws: &Workspace) -> bool {
        !ws.module(self.func.module).functions[self.func.func].generics.is_empty()
    }

    /// The instance's type arguments as the types they are, in declaration
    /// order.
    ///
    /// The arena direction that is always safe: ids in, owned types out. The
    /// other direction ([`Self::intern`]) is the one that must go through the
    /// build's single interner.
    pub fn arg_types(&self, interner: &MonoInterner) -> Vec<MonoTy> {
        interner.get_all(&self.args)
    }

    /// Build an instance from a declaration and the concrete types it is
    /// instantiated at, interning each argument into the build's arena.
    ///
    /// THE only way an instance is minted outside the graph's own walk, and it
    /// takes the interner precisely so it cannot be done without one.
    pub fn intern(interner: &MonoInterner, func: FuncRef, args: Vec<MonoTy>) -> Self {
        Self { func, args: interner.intern_all(args) }
    }

    /// The substitution this instance's body lowers under: each declared
    /// generic's `VarId` bound to its concrete type.
    ///
    /// The declaration supplies the ORDER, which is the only thing a
    /// substitution lacks. Empty for a non-generic instance, which is the
    /// substitution its body has always lowered under.
    pub fn subst(&self, ws: &Workspace, interner: &MonoInterner) -> MonoSubst {
        if self.args.is_empty() {
            return MonoSubst::default();
        }
        crate::lower::spec_subst_from_key(ws, self.func, &self.arg_types(interner))
    }

    /// **The emitted symbol for this instance**, and the only encoder any
    /// consumer may ask.
    ///
    /// Two cases, both explicit. A non-generic instance IS the declaration's
    /// base mangling -- not a fallback for a failed lookup. A generic one always
    /// has concrete arguments, so [`crate::mangle::spec_fn_name`] cannot
    /// decline, and an `unwrap_or_else` back to the base name would hand a
    /// specialization the declaration's symbol, which is the miscompile
    /// instance-owned emission exists to remove.
    pub fn symbol(&self, ws: &Workspace, interner: &MonoInterner) -> String {
        let f = &ws.module(self.func.module).functions[self.func.func];
        if self.args.is_empty() {
            return crate::mangle::mangle_fn(ws, f);
        }
        crate::mangle::spec_fn_name(ws, f, &self.arg_types(interner))
            .expect("an instance with arguments always has a specialized symbol")
    }
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
