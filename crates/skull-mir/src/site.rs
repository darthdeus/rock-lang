use bone_core::hir::Expr;
use bone_core::prelude::Idx;

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SiteOp {
    DirectCall,
    FnValue,
    ForIter,
    ForNext(Box<[usize]>),
    ImplicitIter,
    BoundNext(Box<[usize]>),
    Synthesized(&'static str),
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

#[derive(Clone, PartialEq, Eq, Hash)]
pub enum SiteDomain {
    Own,
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

pub fn record_call_owner(
    func: &mut crate::mir::MirFunc,
    call: Idx<crate::mir::Inst>,
    inst: &crate::instance::Instance,
) {
    func.callee_instances.insert(call, inst.clone());
}

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
