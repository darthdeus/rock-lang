//! A generic enum instantiated at payload types that change its layout gets a
//! name of its own, so every name-keyed layout table plans it on its own.
//! `Result<P>` with a value struct `P` flattens `P` into the enum's slots, while
//! `Result<int>` keeps the declaration's layout and its name.
//!
//! The name is minted where a type becomes a [`MonoTy`], so types, value
//! shapes and enum plans all agree on it. Declaration lookups go through
//! [`decl_name`]; display and canonical spellings show the declaration.

use std::sync::{Arc, LazyLock, RwLock};

use bone_core::hir::TypeExpr;
use bone_core::workspace::Workspace;
use rustc_hash::FxHashMap;
use ustr::Ustr;

use crate::monoty::{MonoSubst, MonoTy};

#[derive(Debug, PartialEq, Eq, Hash)]
pub struct EnumInstance {
    pub base: Ustr,
    pub args: Vec<MonoTy>,
}

/// Process-wide: a name always denotes the same declaration and arguments,
/// whichever workspace minted it. Whether a workspace uses the name is decided
/// per workspace by [`layout_name`].
static INSTANCES: LazyLock<RwLock<FxHashMap<Ustr, Arc<EnumInstance>>>> =
    LazyLock::new(Default::default);

pub fn instance(name: Ustr) -> Option<Arc<EnumInstance>> {
    if !name.as_str().ends_with('>') {
        return None;
    }
    INSTANCES
        .read()
        .expect("enum instances poisoned")
        .get(&name)
        .cloned()
}

/// The declaration an enum name refers to; any other name is returned as is.
pub fn decl_name(name: Ustr) -> Ustr {
    instance(name).map_or(name, |inst| inst.base)
}

/// The generic substitution an instance plans its payloads under.
pub(crate) fn instance_subst(ws: &Workspace, inst: &EnumInstance) -> Option<MonoSubst> {
    let id = ws.nominals.enum_id(inst.base)?;
    let ed = &ws.modules[id.module.index()].enums[id.def];
    if ed.generics.len() != inst.args.len() {
        return None;
    }
    let mut subst = MonoSubst::default();
    for (generic, arg) in ed.generics.iter().zip(&inst.args) {
        let Some(TypeExpr::Generic(var, _)) = crate::lower::static_ty(ws, *generic) else {
            return None;
        };
        subst.insert(var, arg.clone());
    }
    Some(subst)
}

fn spelled(base: Ustr, args: &[MonoTy]) -> Ustr {
    let mut name = String::from(base.as_str());
    name.push('<');
    for (i, arg) in args.iter().enumerate() {
        if i > 0 {
            name.push(',');
        }
        arg.write_canonical(&mut name);
    }
    name.push('>');
    Ustr::from(&name)
}

/// The nominal a type `base<args>` is planned under: the declaration's own name
/// unless the arguments change the enum's layout.
pub(crate) fn layout_name(ws: &Workspace, base: Ustr, args: &[MonoTy]) -> Ustr {
    if args.is_empty() || instance(base).is_some() {
        return base;
    }
    let Some(id) = ws.nominals.enum_id(base) else {
        return base;
    };
    if ws.modules[id.module.index()].enums[id.def]
        .generics
        .is_empty()
    {
        return base;
    }
    let name = spelled(base, args);
    if !INSTANCES
        .read()
        .expect("enum instances poisoned")
        .contains_key(&name)
    {
        INSTANCES
            .write()
            .expect("enum instances poisoned")
            .entry(name)
            .or_insert_with(|| {
                Arc::new(EnumInstance {
                    base,
                    args: args.to_vec(),
                })
            });
    }
    // A recursive payload (`Cons(T, List<T>)`) asks again while the instance is
    // being planned; that payload is a cycle and planned opaque either way.
    let entered = DECIDING.with(|deciding| deciding.borrow_mut().insert(name));
    if !entered {
        return base;
    }
    let same = crate::lower::EnumPlan::instance_matches_declaration(ws, name);
    DECIDING.with(|deciding| deciding.borrow_mut().remove(&name));
    if same { base } else { name }
}

thread_local! {
    static DECIDING: std::cell::RefCell<rustc_hash::FxHashSet<Ustr>> = Default::default();
}

/// The symbol an instance's runtime type tables and box tags use.
pub(crate) fn instance_symbol(inst: &EnumInstance) -> Ustr {
    let mut out = crate::mangle::symbol_of_key(inst.base).as_str().to_owned();
    out.push_str("_of");
    for arg in &inst.args {
        out.push('_');
        arg.write_canonical_as(&mut out, crate::monoty::NominalSpelling::Symbol);
    }
    Ustr::from(&out)
}
