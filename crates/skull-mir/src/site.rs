//! **The neutral identity of one callee obligation** (plan S 9, Stage 3).
//!
//! An [`Idx<Expr>`] alone is not a site identity: one `for` header carries both
//! an `iter` and a `next` witness, one bounded call carries a whole conformance
//! tree, and one column-Map index can emit two runtime entries. Each of those
//! is a separate callee obligation at the same expression, so the operation is
//! part of the key.
//!
//! This type lives in `skull-mir` -- the lowest crate every consumer shares --
//! precisely so there is ONE site vocabulary rather than two equivalent ones
//! with a translation between them. Its consumers are:
//!
//! - `skull_mir::mir::MirFunc::call_sites`, the site a lowered call instruction
//!   came from;
//! - `skull::instance_graph::InstanceGraph::call_sites`, what the walk resolved
//!   each site to;
//! - `skull::concrete_body::ConcreteBody`'s callee tables, keyed by it.
//!
//! Those three are INTENDED to agree, and the agreement is MEASURED rather than
//! assumed: `skull::site_join::census` reports the join with every exclusion
//! named by cause. It is not yet total.
//!
//! Correlating them by span could not even detect a disagreement -- several of
//! these share one span, which is the whole reason the operation is in the key.

use bone_core::hir::Expr;
use bone_core::prelude::Idx;

/// WHICH operation of a body a callee was resolved for.
///
/// The paths are boxed slices and the rare dispatched payload is boxed, so a
/// [`Site`] stays small: the graph and every concrete plan key a map by it.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SiteOp {
    /// A call expression bone resolved (`resolved_overload_calls`).
    DirectCall,
    /// An `Expr::Global` naming a function, taken as a value.
    FnValue,
    /// The `for` head's `iter()` witness.
    ForIter,
    /// The `for` protocol's `next` witness, and the conformance path beneath it
    /// (`BoundNext::sub`). Empty path is the outermost witness.
    ForNext(Box<[usize]>),
    /// An iterable argument coerced through `iter()` at an `Iterator`-bounded
    /// parameter.
    ImplicitIter,
    /// A witness from a bounded CALL's conformance tree, by path. Kept apart
    /// from [`Self::ForNext`] because a `for` over a bounded call has both, at
    /// the same expression and with the same paths.
    BoundNext(Box<[usize]>),
    /// A call lowering synthesizes, named EXACTLY -- see
    /// `skull::instance_graph::COMPILER_GENERATED_TRIGGERS`.
    Synthesized(&'static str),
    /// A call synthesized on a CONDITIONAL arm of a construct that names no
    /// callee at all: an interpolation hole's render, a format spec's helpers,
    /// or structural equality. The plan both lowering and the walk read is
    /// `skull_mir::lower::dispatch`.
    ///
    /// `path` is not decoration. One hole can carry several obligations with
    /// ONE name: a nobox value renders one call per leaf FIELD, and the field
    /// index chain is what keeps them distinct. An empty path is the hole
    /// itself.
    Dispatched(Box<DispatchedOp>),
}

/// The payload of [`SiteOp::Dispatched`].
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DispatchedOp {
    pub name: &'static str,
    pub path: Vec<u32>,
}

impl SiteOp {
    pub fn dispatched(name: &'static str, path: Vec<u32>) -> Self {
        Self::Dispatched(Box::new(DispatchedOp { name, path }))
    }
}

// Spelled as the unboxed variants were, so a site's rendering does not depend
// on how it is stored.
impl std::fmt::Debug for SiteOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DirectCall => f.write_str("DirectCall"),
            Self::FnValue => f.write_str("FnValue"),
            Self::ForIter => f.write_str("ForIter"),
            Self::ForNext(path) => f.debug_tuple("ForNext").field(&path.to_vec()).finish(),
            Self::ImplicitIter => f.write_str("ImplicitIter"),
            Self::BoundNext(path) => f.debug_tuple("BoundNext").field(&path.to_vec()).finish(),
            Self::Synthesized(name) => f.debug_tuple("Synthesized").field(name).finish(),
            Self::Dispatched(op) => f
                .debug_struct("Dispatched")
                .field("name", &op.name)
                .field("path", &op.path)
                .finish(),
        }
    }
}

impl SiteOp {
    pub fn name(&self) -> &'static str {
        match self {
            Self::DirectCall => "direct-call",
            Self::FnValue => "fn-value",
            Self::ForIter => "for-iter",
            Self::ForNext(_) => "for-next",
            Self::ImplicitIter => "implicit-iter",
            Self::BoundNext(_) => "bound-next",
            Self::Synthesized(_) => "synthesized",
            Self::Dispatched(_) => "dispatched",
        }
    }
}

/// WHICH expression arena a [`Site`]'s index is meaningful against.
///
/// An `Idx<Expr>` names a slot in one body's arena and nothing else. Most sites
/// index the owner instance's own body -- but a struct literal that omits a
/// field lowers that field's DEFAULT initializer, which lives in the declaring
/// struct's `BodyOwner::StructDefaults` body, in that struct's module, into the
/// enclosing function's CFG. Its expression index means nothing against the
/// owner's arena: read there it selects whatever happens to sit at that slot,
/// which reads as a plausible answer rather than as a hole.
///
/// The domain is part of the KEY, not a flag beside it, so the two arenas
/// cannot collide even when they hand out the same numeric index.
#[derive(Clone, PartialEq, Eq, Hash)]
pub enum SiteDomain {
    /// The owner instance's own body.
    Own,
    /// A struct's field-default initializers, by declaring module and struct,
    /// plus the ARGUMENTS this injection instantiated the struct with -- in
    /// declared-generic order, empty for a non-generic struct.
    ///
    /// The arguments are part of the key because they select the callee.
    /// `Box<int> {}` and `Box<string> {}` inject the same declaration's
    /// initializers and resolve `seed<int>` and `seed<string>` from them, so a
    /// declaration-keyed domain collapses two obligations onto one and the
    /// second silently reads the first's answer.
    StructDefaults(Box<StructDefaultsDomain>),
}

/// The payload of [`SiteDomain::StructDefaults`], boxed because almost every
/// site is [`SiteDomain::Own`].
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct StructDefaultsDomain {
    pub module: bone_core::prelude::ModuleId,
    pub def: Idx<bone_core::hir::StructDef>,
    pub args: Vec<crate::monoty::MonoTy>,
}

impl SiteDomain {
    pub fn struct_defaults(
        module: bone_core::prelude::ModuleId,
        def: Idx<bone_core::hir::StructDef>,
        args: Vec<crate::monoty::MonoTy>,
    ) -> Self {
        Self::StructDefaults(Box::new(StructDefaultsDomain { module, def, args }))
    }
}

impl std::fmt::Debug for SiteDomain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Own => f.write_str("Own"),
            Self::StructDefaults(d) => f
                .debug_tuple("StructDefaults")
                .field(&d.module)
                .field(&d.def)
                .field(&d.args)
                .finish(),
        }
    }
}

impl SiteDomain {
    /// A total order. `ModuleId` has none of its own, so the key is its index;
    /// sites are sorted for reproducible reports, not for identity.
    ///
    /// The arguments order by their CANONICAL encoding -- the same string
    /// identity is keyed on, so two domains compare equal here exactly when
    /// they are equal.
    fn ord_key(&self) -> (u8, usize, u32, &[crate::monoty::MonoTy]) {
        match self {
            Self::Own => (0, 0, 0, &[]),
            Self::StructDefaults(d) => (1, d.module.index(), d.def.into_raw().into_u32(), &d.args),
        }
    }
}

impl Ord for SiteDomain {
    #[inline]
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        // Almost every site is in its owner's body, and a body's site table
        // compares those with each other.
        if matches!((self, other), (Self::Own, Self::Own)) {
            return Ordering::Equal;
        }
        let (tag, module, def, args) = self.ord_key();
        let (other_tag, other_module, other_def, other_args) = other.ord_key();
        let head = (tag, module, def).cmp(&(other_tag, other_module, other_def));
        if head != Ordering::Equal {
            return head;
        }
        // Lexicographic over the canonical spellings, spelled pairwise only
        // as far as the first difference: every site lookup compares domains,
        // and almost all of them part on the head.
        for (a, b) in args.iter().zip(other_args) {
            // Equal types spell alike, and sites of one domain meet often.
            if a == b {
                continue;
            }
            let arg = a.canonical().cmp(&b.canonical());
            if arg != Ordering::Equal {
                return arg;
            }
        }
        args.len().cmp(&other_args.len())
    }
}

impl PartialOrd for SiteDomain {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// One callee obligation of one body: an expression IN A DOMAIN, plus the
/// operation at it.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Site {
    pub domain: SiteDomain,
    pub expr: Idx<Expr>,
    pub op: SiteOp,
}

/// An index into [`crate::mir::MirFunc::site_owners`].
///
/// Interned rather than stored inline because a body can hold thousands of
/// sites over a handful of owners, and cloning the owner per call site would
/// dominate both memory and comparison.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct OwnerRef(pub u32);

/// Find-or-append `owner` in an interning table, returning its [`OwnerRef`].
///
/// ONE oracle for both writers -- lowering, which interns the body's own owner,
/// and the inliner, which interns a spliced callee's. Two find-or-push routines
/// would be free to disagree about identity and make a call join through the
/// wrong concrete body.
pub fn intern_owner(
    table: &mut Vec<crate::instance::Instance>,
    owner: crate::instance::Instance,
) -> OwnerRef {
    if let Some(i) = table.iter().position(|o| *o == owner) {
        return OwnerRef(i as u32);
    }
    table.push(owner);
    OwnerRef((table.len() - 1) as u32)
}

/// Record the concrete owner of a call, including a non-generic witness whose
/// delivery map has no specialized entry.
pub fn record_call_owner(
    func: &mut crate::mir::MirFunc,
    call: Idx<crate::mir::Inst>,
    inst: &crate::instance::Instance,
) {
    func.callee_instances.insert(call, inst.clone());
}

/// A [`Site`] plus the owner it belongs to.
///
/// The owner is not redundant with the MirFunc the entry lives in. Inlining
/// splices a callee's instructions into its caller, and those calls remain the
/// CALLEE body's obligations -- their `Site` indexes the callee's expression
/// arena and joins through the callee's `ConcreteBody`. A map keyed only by
/// `Site` would silently mix two expression arenas in one body after the first
/// splice, and every arena index would still look plausible.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CallSite {
    pub owner: OwnerRef,
    pub site: Site,
}

#[cfg(test)]
mod size {
    #[test]
    fn a_site_is_five_words() {
        assert_eq!(std::mem::size_of::<super::Site>(), 40);
    }
}
