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
//! named by cause. It is not yet total; the live populations and the blocker
//! live in `docs/parked/mono.md`, not here.
//!
//! Correlating them by span could not even detect a disagreement -- several of
//! these share one span, which is the whole reason the operation is in the key.

use bone_semantic::hir::Expr;
use bone_semantic::prelude::Idx;

/// WHICH operation of a body a callee was resolved for.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum SiteOp {
    /// A call expression bone resolved (`resolved_overload_calls`).
    DirectCall,
    /// An `Expr::Global` naming a function, taken as a value.
    FnValue,
    /// The `for` head's `iter()` witness.
    ForIter,
    /// The `for` protocol's `next` witness, and the conformance path beneath it
    /// (`BoundNext::sub`). Empty path is the outermost witness.
    ForNext(Vec<usize>),
    /// An iterable argument coerced through `iter()` at an `Iterator`-bounded
    /// parameter.
    ImplicitIter,
    /// A witness from a bounded CALL's conformance tree, by path. Kept apart
    /// from [`Self::ForNext`] because a `for` over a bounded call has both, at
    /// the same expression and with the same paths.
    BoundNext(Vec<usize>),
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
    Dispatched { name: &'static str, path: Vec<u32> },
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
            Self::Dispatched { .. } => "dispatched",
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
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
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
    StructDefaults(
        bone_semantic::prelude::ModuleId,
        Idx<bone_semantic::hir::StructDef>,
        Vec<crate::monoty::MonoTy>,
    ),
}

impl SiteDomain {
    /// A total order. `ModuleId` has none of its own, so the key is its index;
    /// sites are sorted for reproducible reports, not for identity.
    ///
    /// The arguments order by their CANONICAL encoding -- the same string
    /// identity is keyed on, so two domains compare equal here exactly when
    /// they are equal.
    fn ord_key(&self) -> (u8, usize, u32, Vec<String>) {
        match self {
            Self::Own => (0, 0, 0, Vec::new()),
            Self::StructDefaults(m, d, args) => (
                1,
                m.index(),
                d.into_raw().into_u32(),
                args.iter().map(|a| a.canonical()).collect(),
            ),
        }
    }
}

impl Ord for SiteDomain {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.ord_key().cmp(&other.ord_key())
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

impl Site {
    pub fn direct_call(expr: Idx<Expr>) -> Self {
        Self { domain: SiteDomain::Own, expr, op: SiteOp::DirectCall }
    }
}

/// The body that OWNS a set of sites: a declaration plus the substitution it
/// was lowered under -- i.e. a concrete INSTANCE, in lowering's vocabulary.
///
/// The declaration alone is not enough. Inlining splices a *specialized*
/// snapshot, so `identity<int>` and `identity<string>` can both be spliced into
/// one caller; their sites index the same expression arena and would join
/// through whichever instance was looked up first. The substitution is what
/// makes the owner concrete, and it is the same `MonoSubst` the graph records
/// per instance, so the two can be compared without a translation.
#[derive(Clone, PartialEq, Debug)]
pub struct OwnerInstance {
    pub func: crate::mir::FuncRef,
    pub subst: crate::monoty::MonoSubst,
}

/// An index into [`crate::mir::MirFunc::site_owners`].
///
/// Interned rather than stored inline because a substitution is a map: after
/// inlining a body can hold thousands of sites over a handful of owners, and
/// cloning the map per call site would dominate both memory and comparison.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct OwnerRef(pub u32);

/// Find-or-append `owner` in an interning table, returning its [`OwnerRef`].
///
/// ONE oracle for both writers -- lowering, which interns the body's own owner,
/// and the inliner, which interns a spliced callee's. Two find-or-push routines
/// would be free to disagree about identity (a subst compared by pointer, a
/// declaration compared without it) and the disagreement would surface as a
/// call joining through the wrong instance, which reads as a plausible answer.
pub fn intern_owner(table: &mut Vec<OwnerInstance>, owner: OwnerInstance) -> OwnerRef {
    if let Some(i) = table.iter().position(|o| *o == owner) {
        return OwnerRef(i as u32);
    }
    table.push(owner);
    OwnerRef((table.len() - 1) as u32)
}

/// Record that `call` is served by the concrete body `inst`.
///
/// The writer for the RETARGETING path. There are deliberately two writers of
/// this pair, not one: lowering derives the instance INDEPENDENTLY of the
/// delivery decision (`Builder::callee_instance_for`) and writes both through
/// `record_call`, while this path has no independent source and derives the
/// substitution from the instance through the documented `spec_subst_from_key`
/// round-trip. Keeping the second opinion is worth more than collapsing to a
/// single writer.
///
/// What the two must never do is write one record and not the other. The
/// delivery key and the callee instance are one fact, so a path that writes
/// half of it leaves a site whose instantiation is knowable and unrecorded.
/// Measured over the 18 bin projects plus `tests/`: **128,956 specialized
/// demand sites agreed on both records and 373 had no instance at all**, every
/// one of them a bounded `next` retargeted here, back when this wrote only the
/// key.
///
/// What makes the two writers an AGREEMENT rather than a convention is the
/// assertion in [`crate::reach::spec_bound_calls`], which sees every site
/// either writer produced.
pub fn record_specialized_call(
    func: &mut crate::mir::MirFunc,
    ws: &bone_semantic::inference::Workspace,
    interner: &crate::monoty::MonoInterner,
    call: Idx<crate::mir::Inst>,
    inst: crate::instance::Instance,
) {
    record_call_owner(func, ws, interner, call, &inst);
    func.spec_bindings.insert(call, inst);
}

/// Record the concrete owner of a call, including a non-generic witness whose
/// delivery map has no specialized entry.
pub fn record_call_owner(
    func: &mut crate::mir::MirFunc,
    ws: &bone_semantic::inference::Workspace,
    interner: &crate::monoty::MonoInterner,
    call: Idx<crate::mir::Inst>,
    inst: &crate::instance::Instance,
) {
    let subst = inst.subst(ws, interner);
    func.callee_instances.insert(call, OwnerInstance { func: inst.func, subst });
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
