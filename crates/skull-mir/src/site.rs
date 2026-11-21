//! An `Idx<Expr>` alone is not a site identity: one expression can carry several
//! callee obligations (a `for` head's `iter` and `next`), so the operation is
//! part of the key.
use bone_core::hir::Expr;
use bone_core::prelude::Idx;

/// Paths and the rare dispatched payload are boxed so a [`Site`] stays small.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SiteOp {
    DirectCall,
    FnValue,
    ForIter,
    /// The `for` protocol's `next` witness and its conformance path; an empty path
    /// is the outermost witness.
    ForNext(Box<[usize]>),
    ImplicitIter,
    /// Kept apart from [`Self::ForNext`]: a `for` over a bounded call has both, at
    /// the same expression and with the same paths.
    BoundNext(Box<[usize]>),
    Synthesized(&'static str),
    /// `path` keeps several same-named obligations of one hole distinct (one per
    /// leaf field of a nobox value); an empty path is the hole itself.
    Dispatched(Box<DispatchedOp>),
}

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

// Spelled as the unboxed variants were, so rendering does not depend on storage.
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

/// A struct literal that omits a field lowers the default from another body's
/// arena; the domain is part of the key so equal indices cannot collide.
#[derive(Clone, PartialEq, Eq, Hash)]
pub enum SiteDomain {
    Own,
    /// The arguments are part of the key: `Box<int> {}` and `Box<string> {}` inject
    /// the same initializers but resolve different callees.
    StructDefaults(Box<StructDefaultsDomain>),
}

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
    /// For reproducible reports, not identity. Arguments order by their canonical
    /// encoding, so domains compare equal here exactly when they are equal.
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
        if matches!((self, other), (Self::Own, Self::Own)) {
            return Ordering::Equal;
        }
        let (tag, module, def, args) = self.ord_key();
        let (other_tag, other_module, other_def, other_args) = other.ord_key();
        let head = (tag, module, def).cmp(&(other_tag, other_module, other_def));
        if head != Ordering::Equal {
            return head;
        }
        for (a, b) in args.iter().zip(other_args) {
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

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Site {
    pub domain: SiteDomain,
    pub expr: Idx<Expr>,
    pub op: SiteOp,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct OwnerRef(pub u32);

/// The one find-or-append for lowering and the inliner: two routines could
/// disagree about identity and join a call through the wrong concrete body.
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

/// Includes a non-generic witness whose delivery map has no specialized entry.
pub fn record_call_owner(
    func: &mut crate::mir::MirFunc,
    call: Idx<crate::mir::Inst>,
    inst: &crate::instance::Instance,
) {
    func.callee_instances.insert(call, inst.clone());
}

/// Inlined calls remain the callee body's obligations and index its arena, so a
/// map keyed only by `Site` would mix arenas after a splice.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CallSite {
    pub owner: OwnerRef,
    pub site: Site,
}
