//! A generic struct or enum instantiated at type arguments that change its
//! layout gets a name of its own, so every name-keyed layout table plans it on
//! its own. `Result<P>` with a value struct `P` flattens `P` into the enum's
//! slots and `Box<N>` with a `nobox` `N` keeps `N`'s leaves in the box, while
//! `Result<int>` and `Box<int>` keep the declaration's layout and its name.
//!
//! The name is minted where a type becomes a [`MonoTy`], so types, value
//! shapes, field plans and enum plans all agree on it. Declaration lookups
//! resolve it to its declaration; display and canonical spellings show the
//! declaration.

use std::sync::{Arc, LazyLock, RwLock};

use bone_core::hir::{TyId, TypeExpr};
use bone_core::workspace::Workspace;
use rustc_hash::FxHashMap;
use ustr::Ustr;

use crate::monoty::{MonoSubst, MonoTy};

#[derive(Debug, PartialEq, Eq, Hash)]
pub struct LayoutInstance {
    pub base: Ustr,
    pub args: Vec<MonoTy>,
}

/// Process-wide: a name always denotes the same declaration and arguments,
/// whichever workspace minted it. Whether a workspace uses the name is decided
/// per workspace by [`layout_name`].
static INSTANCES: LazyLock<RwLock<FxHashMap<Ustr, Arc<LayoutInstance>>>> =
    LazyLock::new(Default::default);

pub fn instance(name: Ustr) -> Option<Arc<LayoutInstance>> {
    if !name.as_str().ends_with('>') {
        return None;
    }
    INSTANCES
        .read()
        .expect("layout instances poisoned")
        .get(&name)
        .cloned()
}

/// The declaration a nominal name refers to; any other name is returned as is.
pub fn decl_name(name: Ustr) -> Ustr {
    instance(name).map_or(name, |inst| inst.base)
}

/// The source spelling of a layout: an instance shows its declaration.
pub fn display_name(name: Ustr) -> &'static str {
    bone_core::modules::display_name(decl_name(name).as_str())
}

/// The generic substitution an instance plans its fields or payloads under.
pub(crate) fn instance_subst(ws: &Workspace, inst: &LayoutInstance) -> Option<MonoSubst> {
    let generics = generics_of(ws, inst.base)?;
    if generics.len() != inst.args.len() {
        return None;
    }
    let mut subst = MonoSubst::default();
    for (generic, arg) in generics.iter().zip(&inst.args) {
        let Some(TypeExpr::Generic(var, _)) = crate::lower::static_ty(ws, *generic) else {
            return None;
        };
        subst.insert(var, arg.clone());
    }
    Some(subst)
}

/// The substitution a layout name plans under; `None` for a declaration.
pub(crate) fn layout_subst(ws: &Workspace, name: Ustr) -> Option<MonoSubst> {
    instance_subst(ws, &*instance(name)?)
}

fn generics_of(ws: &Workspace, base: Ustr) -> Option<&[TyId]> {
    if let Some(id) = ws.nominals.enum_id(base) {
        return Some(&ws.modules[id.module.index()].enums[id.def].generics);
    }
    let id = ws.nominals.struct_id(base)?;
    Some(&ws.modules[id.module.index()].structs[id.def].generics)
}

/// An argument whose value is one slot of plain data or a reference, laid out
/// as a declaration's generic field already is.
fn is_layout_neutral(ws: &Workspace, arg: &MonoTy) -> bool {
    use crate::monoty::MonoCtor;
    match arg {
        MonoTy::Int | MonoTy::Uint | MonoTy::Float | MonoTy::Bool | MonoTy::String => true,
        MonoTy::App {
            ctor: MonoCtor::Array | MonoCtor::Map,
            ..
        } => true,
        MonoTy::App {
            ctor: MonoCtor::Nominal(name),
            ..
        } => ws.nominals.struct_id(*name).is_some_and(|id| {
            !crate::lower::has_value_plan(&ws.modules[id.module.index()].structs[id.def])
        }),
        _ => false,
    }
}

/// Whether some field of the struct `name` holds a generic where its layout
/// could take the argument's leaves: the field itself, a nullable of it, or a
/// generic nominal that places it. A container or function of a generic stores
/// one reference whatever the argument. A cycle answers yes.
fn places_generic(ws: &Workspace, name: Ustr, visiting: &mut Vec<Ustr>) -> bool {
    let Some(id) = ws.nominals.struct_id(name) else {
        return true;
    };
    if visiting.contains(&name) {
        return true;
    }
    visiting.push(name);
    let sd = &ws.modules[id.module.index()].structs[id.def];
    let found = sd
        .fields
        .iter()
        .any(|f| field_places_generic(ws, f.ty, visiting));
    visiting.pop();
    found
}

fn field_places_generic(ws: &Workspace, ty: TyId, visiting: &mut Vec<Ustr>) -> bool {
    let TyId::Static(mid, idx) = ty else {
        return true;
    };
    match ws.module(mid).ty_arena[idx] {
        TypeExpr::Generic(..) | TypeExpr::Infer(..) => true,
        TypeExpr::Nullable(inner) => field_places_generic(ws, TyId::Static(mid, inner), visiting),
        TypeExpr::Tuple { elements } => elements
            .into_iter()
            .any(|e| field_places_generic(ws, TyId::Static(mid, e), visiting)),
        TypeExpr::App {
            ctor: bone_core::hir::TypeCtor::Nominal(name),
            params,
        } => {
            params
                .into_iter()
                .any(|p| crate::lower::mentions_generic(ws, TyId::Static(mid, p)))
                && (ws.nominals.struct_id(name).is_none() || places_generic(ws, name, visiting))
        }
        TypeExpr::App { .. } | TypeExpr::Function { .. } | TypeExpr::Builtin(_) => false,
    }
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
/// unless the arguments change its layout.
pub(crate) fn layout_name(ws: &Workspace, base: Ustr, args: &[MonoTy]) -> Ustr {
    if args.is_empty()
        || instance(base).is_some()
        || generics_of(ws, base).is_none_or(|generics| generics.is_empty())
    {
        return base;
    }
    if ws.nominals.struct_id(base).is_some()
        && (args.iter().all(|arg| is_layout_neutral(ws, arg))
            || !places_generic(ws, base, &mut Vec::new()))
    {
        return base;
    }
    let name = spelled(base, args);
    if !INSTANCES
        .read()
        .expect("layout instances poisoned")
        .contains_key(&name)
    {
        INSTANCES
            .write()
            .expect("layout instances poisoned")
            .entry(name)
            .or_insert_with(|| {
                Arc::new(LayoutInstance {
                    base,
                    args: args.to_vec(),
                })
            });
    }
    // A recursive payload (`Cons(T, List<T>)`) or field asks again while the
    // instance is being planned; that type is a cycle and planned opaque either way.
    let entered = DECIDING.with(|deciding| deciding.borrow_mut().insert(name));
    if !entered {
        return base;
    }
    let same = crate::lower::instance_matches_declaration(ws, name);
    DECIDING.with(|deciding| deciding.borrow_mut().remove(&name));
    if same { base } else { name }
}

thread_local! {
    static DECIDING: std::cell::RefCell<rustc_hash::FxHashSet<Ustr>> = Default::default();
}

/// The symbol an instance's runtime type tables and box tags use.
pub(crate) fn instance_symbol(inst: &LayoutInstance) -> Ustr {
    let mut out = crate::mangle::symbol_of_key(inst.base).as_str().to_owned();
    out.push_str("_of");
    for arg in &inst.args {
        out.push('_');
        arg.write_canonical_as(&mut out, crate::monoty::NominalSpelling::Symbol);
    }
    Ustr::from(&out)
}
