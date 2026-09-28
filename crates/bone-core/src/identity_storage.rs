//! Identity storage (docs/identity-storage-plan.md): a nominal `struct` has
//! identity and exists only as an Arena row or a Scene/Session root. Any other
//! storage position must hold values or handles.
//!
//! A type argument is a storage position iff its constructor stores that
//! parameter in a field, transitively. Phantom parameters (`Arena<T>`,
//! `Entity<T>`, `Scene<T>`) are admitted by structure. The one privileged edge
//! is a Vec field annotated `@identity_storage` (`ArenaStore<T>.data`), whose
//! direct elements may be identities after substitution; what those elements
//! contain is still checked.

use crate::hir::{StructFieldDef, TyId, TypeCtor, TypeExpr, VariantPayload};
use crate::prelude::*;
use crate::workspace::Workspace;

pub const IDENTITY_STORAGE_ANNOTATION: &str = "identity_storage";

/// How an identity reached the storage position that rejects it.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum IdentityStorageVia {
    /// Directly, or under a nullable wrapper.
    Direct,
    /// Inside a Vec, Map, tuple, or a stored type argument.
    Container,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct IdentityStorageViolation {
    pub identity: Ustr,
    pub via: IdentityStorageVia,
}

/// Whether `name` is a nominal identity type: a `struct`, not a `value`.
pub fn is_identity_struct(ws: &Workspace, name: Ustr) -> bool {
    ws.struct_by_name
        .get(&name)
        .is_some_and(|sid| !ws.module(sid.module).structs[sid.def].is_value)
}

pub fn is_identity_storage_field(field: &StructFieldDef) -> bool {
    field
        .annotations
        .iter()
        .any(|annotation| annotation.is_builtin(IDENTITY_STORAGE_ANNOTATION))
}

/// The first identity stored by a value of `ty` held in an ordinary storage
/// position.
pub fn identity_storage_violation(ws: &Workspace, ty: TyId) -> Option<IdentityStorageViolation> {
    visit(ws, ty, Top::Stored, IdentityStorageVia::Direct, &mut ParamClasses::default())
}

/// [`identity_storage_violation`] for a root position (a global's type): the
/// top itself may be an identity, but what it stores through its type
/// arguments may not.
pub fn identity_root_storage_violation(ws: &Workspace, ty: TyId) -> Option<IdentityStorageViolation> {
    visit(ws, ty, Top::Admitted, IdentityStorageVia::Direct, &mut ParamClasses::default())
}

/// [`identity_storage_violation`] for an `@identity_storage` field: the Vec's
/// direct elements are rows, and everything below them is ordinary storage.
pub fn identity_storage_field_violation(ws: &Workspace, ty: TyId) -> Option<IdentityStorageViolation> {
    let mut classes = ParamClasses::default();
    match static_expr(ws, ty) {
        Some(TypeExpr::App {
            ctor: TypeCtor::Array,
            mut params,
        }) => params.find_map(|p| {
            visit(ws, ty.with_idx(p), Top::Admitted, IdentityStorageVia::Container, &mut classes)
        }),
        _ => visit(ws, ty, Top::Stored, IdentityStorageVia::Direct, &mut classes),
    }
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum Top {
    Stored,
    Admitted,
}

/// What a nominal constructor does with one of its type parameters. Ordered
/// by strength so occurrences join with `max`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum ParamClass {
    Phantom,
    /// Stored only as the direct element of an `@identity_storage` field.
    Row,
    Stored,
}

#[derive(Default)]
struct ParamClasses {
    done: FxHashMap<(Ustr, usize), ParamClass>,
    in_progress: FxHashSet<(Ustr, usize)>,
}

fn static_expr(ws: &Workspace, ty: TyId) -> Option<TypeExpr> {
    // Declaration types live in module arenas. Body-local types are the body
    // check's concern (S0b), which reads them through the body's own arena.
    match ty {
        TyId::Static(mid, idx) => Some(ws.module(mid).ty_arena[idx]),
        TyId::Local(_) => None,
    }
}

fn visit(
    ws: &Workspace,
    ty: TyId,
    top: Top,
    via: IdentityStorageVia,
    classes: &mut ParamClasses,
) -> Option<IdentityStorageViolation> {
    match static_expr(ws, ty)? {
        TypeExpr::Builtin(_) | TypeExpr::Generic(..) | TypeExpr::Infer(..) => None,
        TypeExpr::Function { .. } => None,
        TypeExpr::Nullable(inner) => visit(ws, ty.with_idx(inner), Top::Stored, via, classes),
        TypeExpr::Tuple { mut elements } => elements.find_map(|e| {
            visit(ws, ty.with_idx(e), Top::Stored, IdentityStorageVia::Container, classes)
        }),
        TypeExpr::App {
            ctor: TypeCtor::Ref, ..
        } => None,
        TypeExpr::App {
            ctor: TypeCtor::Array | TypeCtor::Map,
            mut params,
        } => params.find_map(|p| {
            visit(ws, ty.with_idx(p), Top::Stored, IdentityStorageVia::Container, classes)
        }),
        TypeExpr::App {
            ctor: TypeCtor::Nominal(name),
            params,
        } => {
            if top == Top::Stored && is_identity_struct(ws, name) {
                return Some(IdentityStorageViolation { identity: name, via });
            }
            params.enumerate().find_map(|(i, p)| {
                let arg_top = match param_class(ws, name, i, classes).0 {
                    ParamClass::Phantom => return None,
                    ParamClass::Row => Top::Admitted,
                    ParamClass::Stored => Top::Stored,
                };
                visit(ws, ty.with_idx(p), arg_top, IdentityStorageVia::Container, classes)
            })
        }
    }
}

/// The generic parameters and the stored field types of a nominal, with
/// whether each field is an `@identity_storage` edge. `None` for a name with
/// no declaration (compiler intrinsics such as `FieldRef`), which stores
/// nothing.
fn nominal_storage(ws: &Workspace, name: Ustr) -> Option<(Vec<TyId>, Vec<(TyId, bool)>)> {
    if let Some(sid) = ws.struct_by_name.get(&name) {
        let def = &ws.module(sid.module).structs[sid.def];
        let fields = def.fields.iter().map(|f| (f.ty, is_identity_storage_field(f))).collect();
        return Some((def.generics.clone(), fields));
    }
    if let Some(eid) = ws.enums_by_name.get(&name) {
        let def = &ws.module(eid.module).enums[eid.def];
        let mut fields = Vec::new();
        for variant in &def.variants {
            match &variant.payload {
                VariantPayload::Unit => {}
                VariantPayload::Tuple(types) => fields.extend(types.iter().map(|t| (*t, false))),
                VariantPayload::Record(record) => fields.extend(record.iter().map(|f| (f.ty, false))),
            }
        }
        return Some((def.generics.clone(), fields));
    }
    if let Some(nid) = ws.newtypes_by_name.get(&name) {
        let def = &ws.module(nid.module).newtypes[nid.def];
        return Some((def.generics.clone(), vec![(def.wrapped, false)]));
    }
    None
}

/// Returns the class and whether it was computed while some constructor was
/// still in progress (a recursive type), in which case it is not cached: the
/// in-progress entry read as `Phantom` may have been an underestimate.
fn param_class(ws: &Workspace, name: Ustr, index: usize, classes: &mut ParamClasses) -> (ParamClass, bool) {
    let key = (name, index);
    if let Some(class) = classes.done.get(&key) {
        return (*class, false);
    }
    if !classes.in_progress.insert(key) {
        return (ParamClass::Phantom, true);
    }
    let mut class = ParamClass::Phantom;
    let mut provisional = false;
    if let Some((generics, fields)) = nominal_storage(ws, name)
        && let Some(param) = generics.get(index).and_then(|g| generic_name(ws, *g))
    {
        for (field_ty, is_row_edge) in fields {
            let (occurrence, cyclic) = if is_row_edge {
                row_edge_occurrence(ws, field_ty, param, classes)
            } else {
                occurrence(ws, field_ty, param, false, classes)
            };
            class = class.max(occurrence);
            provisional |= cyclic;
        }
    }
    classes.in_progress.remove(&key);
    if !provisional {
        classes.done.insert(key, class);
    }
    (class, provisional)
}

fn generic_name(ws: &Workspace, ty: TyId) -> Option<Ustr> {
    match static_expr(ws, ty)? {
        TypeExpr::Generic(_, name) => Some(name),
        _ => None,
    }
}

fn row_edge_occurrence(ws: &Workspace, ty: TyId, param: Ustr, classes: &mut ParamClasses) -> (ParamClass, bool) {
    match static_expr(ws, ty) {
        Some(TypeExpr::App {
            ctor: TypeCtor::Array,
            params,
        }) => join(params.map(|p| occurrence(ws, ty.with_idx(p), param, true, classes))),
        _ => occurrence(ws, ty, param, false, classes),
    }
}

/// How `param` occurs in a field type. `row` marks the direct element of an
/// `@identity_storage` edge.
fn occurrence(ws: &Workspace, ty: TyId, param: Ustr, row: bool, classes: &mut ParamClasses) -> (ParamClass, bool) {
    let Some(expr) = static_expr(ws, ty) else {
        return (ParamClass::Phantom, false);
    };
    match expr {
        TypeExpr::Generic(_, name) if name == param => {
            (if row { ParamClass::Row } else { ParamClass::Stored }, false)
        }
        TypeExpr::Builtin(_) | TypeExpr::Generic(..) | TypeExpr::Infer(..) | TypeExpr::Function { .. } => {
            (ParamClass::Phantom, false)
        }
        TypeExpr::App {
            ctor: TypeCtor::Ref, ..
        } => (ParamClass::Phantom, false),
        TypeExpr::Nullable(inner) => occurrence(ws, ty.with_idx(inner), param, false, classes),
        TypeExpr::Tuple { elements } => {
            join(elements.map(|e| occurrence(ws, ty.with_idx(e), param, false, classes)))
        }
        TypeExpr::App {
            ctor: TypeCtor::Array | TypeCtor::Map,
            params,
        } => join(params.map(|p| occurrence(ws, ty.with_idx(p), param, false, classes))),
        TypeExpr::App {
            ctor: TypeCtor::Nominal(name),
            params,
        } => join(params.enumerate().map(|(i, p)| {
            let (arg_class, cyclic) = param_class(ws, name, i, classes);
            let (inner, inner_cyclic) = match arg_class {
                ParamClass::Phantom => (ParamClass::Phantom, false),
                ParamClass::Row => occurrence(ws, ty.with_idx(p), param, true, classes),
                ParamClass::Stored => occurrence(ws, ty.with_idx(p), param, false, classes),
            };
            (inner, cyclic || inner_cyclic)
        })),
    }
}

fn join(items: impl Iterator<Item = (ParamClass, bool)>) -> (ParamClass, bool) {
    items.fold((ParamClass::Phantom, false), |(class, cyclic), (c, y)| (class.max(c), cyclic || y))
}
