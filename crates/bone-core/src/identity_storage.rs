//! Identity storage: a nominal `struct` has
//! identity and exists only as an Arena row or a Scene/process root. Any other
//! storage position must hold values or handles.
//!
//! A type argument is a storage position iff its constructor stores that
//! parameter in a field, transitively. Phantom parameters (`Arena<T>`,
//! `Entity<T>`, `Scene<T>`) are admitted by structure. The one privileged edge
//! is a Vec field annotated `@identity_storage` (`ArenaStore<T>.data`), whose
//! direct elements may be identities after substitution; what those elements
//! contain is still checked.

use crate::hir::{Module, StructFieldDef, TyId, TypeCtor, TypeExpr, VariantPayload};
use crate::prelude::*;
use crate::workspace::{BodyInference, Workspace};
use skull_parser::IdentityStorage;

mod sites;
pub use sites::*;

pub(crate) const IDENTITY_STORAGE_ANNOTATION: &str = "identity_storage";
/// The Arena and Scene registries' erasure capability: a stdlib function whose
/// body may erase an identity to `any`.
pub(crate) const IDENTITY_ERASURE_ANNOTATION: &str = "identity_erasure";

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
pub(crate) fn is_identity_struct(ws: &Workspace, name: Ustr) -> bool {
    ws.struct_by_name
        .get(&name)
        .is_some_and(|sid| !ws.module(sid.module).structs[sid.def].is_value)
}

pub(crate) fn is_identity_storage_field(field: &StructFieldDef) -> bool {
    field
        .annotations
        .iter()
        .any(|annotation| annotation.is_builtin(IDENTITY_STORAGE_ANNOTATION))
}

/// The first identity stored by a value of `ty` held in an ordinary storage
/// position.
pub(crate) fn identity_storage_violation(
    ws: &Workspace,
    ty: TyId,
) -> Option<IdentityStorageViolation> {
    identity_body_storage_violation(ws, None, ty)
}

/// [`identity_storage_violation`] for a type that may be local to a body
/// (`bi` owns its `TyId::Local` arena and substitutions): a closure capture or
/// a `co fn` parameter, which the closure or coroutine frame stores.
pub(crate) fn identity_body_storage_violation(
    ws: &Workspace,
    bi: Option<&BodyInference>,
    ty: TyId,
) -> Option<IdentityStorageViolation> {
    visit(
        ws,
        Types { ws, bi },
        ty,
        Top::Stored,
        IdentityStorageVia::Direct,
        &mut ParamClasses::default(),
    )
}

/// [`identity_storage_violation`] for a root position (a global's type): the
/// top itself may be an identity, or a nullable identity (an optional one-row
/// process root), but what it stores through its type arguments may not.
pub(crate) fn identity_root_storage_violation(
    ws: &Workspace,
    ty: TyId,
) -> Option<IdentityStorageViolation> {
    visit(
        ws,
        Types { ws, bi: None },
        ty,
        Top::Root,
        IdentityStorageVia::Direct,
        &mut ParamClasses::default(),
    )
}

/// [`identity_storage_violation`] for a body's local, parameter or temporary:
/// an identity at the top, under a nullable, or as a tuple element is a
/// borrow; one stored by a Vec, Map or constructor type argument is storage.
pub(crate) fn identity_local_storage_violation(
    ws: &Workspace,
    bi: &BodyInference,
    ty: TyId,
) -> Option<IdentityStorageViolation> {
    visit(
        ws,
        Types { ws, bi: Some(bi) },
        ty,
        Top::Borrow,
        IdentityStorageVia::Direct,
        &mut ParamClasses::default(),
    )
}

/// [`identity_storage_violation`] for an `@identity_storage` field: the Vec's
/// direct elements are rows, and everything below them is ordinary storage.
pub(crate) fn identity_storage_field_violation(
    ws: &Workspace,
    ty: TyId,
) -> Option<IdentityStorageViolation> {
    let mut classes = ParamClasses::default();
    match static_expr(ws, ty) {
        Some(TypeExpr::App {
            ctor: TypeCtor::Array,
            mut params,
        }) => params.find_map(|p| {
            visit(
                ws,
                Types { ws, bi: None },
                ty.with_idx(p),
                Top::Admitted,
                IdentityStorageVia::Container,
                &mut classes,
            )
        }),
        _ => visit(
            ws,
            Types { ws, bi: None },
            ty,
            Top::Stored,
            IdentityStorageVia::Direct,
            &mut classes,
        ),
    }
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum Top {
    Stored,
    /// An Arena row: the identity itself.
    Admitted,
    /// A process root: the identity, or a nullable one.
    Root,
    /// A body's borrow: the identity, under nullables and tuples.
    Borrow,
}

/// One node of a type as the predicate reads it. The predicate walks bone's
/// `TyId`s here and the instance graph's substituted types in skull-graph, so
/// both answer one rule.
pub enum IdentityNode<T, C: Iterator<Item = T>> {
    /// Stores no identity: a builtin, generic, inference variable, function
    /// or `ref`.
    Opaque,
    Nullable(T),
    Tuple(C),
    /// A Vec or Map: stores every parameter.
    Collection(C),
    Nominal(Ustr, C),
}

/// A type representation the predicate can walk.
pub trait IdentityTypes: Copy {
    type Ty: Copy;
    type Children: Iterator<Item = Self::Ty>;
    /// `None` for a type that does not resolve.
    fn node(self, ty: Self::Ty) -> Option<IdentityNode<Self::Ty, Self::Children>>;
}

/// Where a type sits, for [`identity_violation_in`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum IdentityPosition {
    /// Ordinary storage: the identity itself is a violation.
    Stored,
    /// A body's local, parameter or temporary: see
    /// [`identity_local_storage_violation`].
    Borrow,
    /// The Vec written to an `@identity_storage` field: its direct elements
    /// are rows, and everything below them is ordinary storage.
    Row,
}

/// The predicate over any [`IdentityTypes`] representation.
pub fn identity_violation_in<V: IdentityTypes>(
    ws: &Workspace,
    types: V,
    ty: V::Ty,
    position: IdentityPosition,
) -> Option<IdentityStorageViolation> {
    let top = match position {
        IdentityPosition::Stored => Top::Stored,
        IdentityPosition::Borrow => Top::Borrow,
        IdentityPosition::Row => {
            let mut classes = ParamClasses::default();
            return match types.node(ty)? {
                IdentityNode::Collection(mut params) => params.find_map(|p| {
                    visit(
                        ws,
                        types,
                        p,
                        Top::Admitted,
                        IdentityStorageVia::Container,
                        &mut classes,
                    )
                }),
                _ => visit(
                    ws,
                    types,
                    ty,
                    Top::Stored,
                    IdentityStorageVia::Direct,
                    &mut classes,
                ),
            };
        }
    };
    visit(
        ws,
        types,
        ty,
        top,
        IdentityStorageVia::Direct,
        &mut ParamClasses::default(),
    )
}

/// [`identity_violation_in`] over one body's inferred types.
pub(crate) fn identity_body_violation(
    ws: &Workspace,
    bi: &BodyInference,
    ty: TyId,
    position: IdentityPosition,
) -> Option<IdentityStorageViolation> {
    identity_violation_in(ws, Types { ws, bi: Some(bi) }, ty, position)
}

/// Whether `field` of a value of `owner` (a body type) is an
/// `@identity_storage` edge.
pub(crate) fn is_identity_storage_edge(
    ws: &Workspace,
    bi: &BodyInference,
    owner: TyId,
    field: Ustr,
) -> bool {
    let Some((
        _,
        TypeExpr::App {
            ctor: TypeCtor::Nominal(name),
            ..
        },
    )) = (Types { ws, bi: Some(bi) }).resolve(owner)
    else {
        return false;
    };
    is_identity_storage_struct_field(ws, name, field)
}

/// Whether `field` of the struct `name` is an `@identity_storage` edge.
pub(crate) fn is_identity_storage_struct_field(ws: &Workspace, name: Ustr, field: Ustr) -> bool {
    ws.struct_by_name.get(&name).is_some_and(|sid| {
        ws.module(sid.module).structs[sid.def]
            .fields
            .iter()
            .any(|f| f.name == field && is_identity_storage_field(f))
    })
}

/// Where a `TyId` is read: declaration arenas, plus one body's arena and
/// substitutions for `TyId::Local`.
#[derive(Copy, Clone)]
struct Types<'a> {
    ws: &'a Workspace,
    bi: Option<&'a BodyInference>,
}

impl Types<'_> {
    /// The type behind `ty`, following inference variables, with the id whose
    /// arena its child indices point into.
    fn resolve(self, ty: TyId) -> Option<(TyId, TypeExpr)> {
        let mut cur = ty;
        loop {
            let expr = match cur {
                TyId::Static(mid, idx) => self.ws.module(mid).ty_arena[idx],
                TyId::Local(idx) => self.bi?.ty_arena[idx],
            };
            match expr {
                TypeExpr::Infer(var, _) => match self.bi.and_then(|bi| bi.substitutions.get(var)) {
                    Some(next) => cur = next,
                    None => return Some((cur, expr)),
                },
                _ => return Some((cur, expr)),
            }
        }
    }
}

pub(crate) struct TyIdChildren {
    base: TyId,
    range: IdxRange<TypeExpr>,
}

impl Iterator for TyIdChildren {
    type Item = TyId;
    fn next(&mut self) -> Option<TyId> {
        self.range.next().map(|idx| self.base.with_idx(idx))
    }
}

impl IdentityTypes for Types<'_> {
    type Ty = TyId;
    type Children = TyIdChildren;
    fn node(self, ty: TyId) -> Option<IdentityNode<TyId, TyIdChildren>> {
        let (base, expr) = self.resolve(ty)?;
        let children = |range| TyIdChildren { base, range };
        Some(match expr {
            TypeExpr::Builtin(_)
            | TypeExpr::Generic(..)
            | TypeExpr::Infer(..)
            | TypeExpr::Function { .. } => IdentityNode::Opaque,
            TypeExpr::App {
                ctor: TypeCtor::Ref,
                ..
            } => IdentityNode::Opaque,
            TypeExpr::Nullable(inner) => IdentityNode::Nullable(base.with_idx(inner)),
            TypeExpr::Tuple { elements } => IdentityNode::Tuple(children(elements)),
            TypeExpr::App {
                ctor: TypeCtor::Array | TypeCtor::Map,
                params,
            } => IdentityNode::Collection(children(params)),
            TypeExpr::App {
                ctor: TypeCtor::Nominal(name),
                params,
            } => IdentityNode::Nominal(name, children(params)),
        })
    }
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

/// A declaration type. Constructor parameter classes are read from
/// declarations only.
fn static_expr(ws: &Workspace, ty: TyId) -> Option<TypeExpr> {
    match ty {
        TyId::Static(mid, idx) => Some(ws.module(mid).ty_arena[idx]),
        TyId::Local(_) => None,
    }
}

fn visit<V: IdentityTypes>(
    ws: &Workspace,
    types: V,
    ty: V::Ty,
    top: Top,
    via: IdentityStorageVia,
    classes: &mut ParamClasses,
) -> Option<IdentityStorageViolation> {
    match types.node(ty)? {
        IdentityNode::Opaque => None,
        IdentityNode::Nullable(inner) => {
            let inner_top = if matches!(top, Top::Root | Top::Borrow) {
                top
            } else {
                Top::Stored
            };
            visit(ws, types, inner, inner_top, via, classes)
        }
        IdentityNode::Tuple(mut elements) => {
            let (element_top, element_via) = if top == Top::Borrow {
                (Top::Borrow, via)
            } else {
                (Top::Stored, IdentityStorageVia::Container)
            };
            elements.find_map(|e| visit(ws, types, e, element_top, element_via, classes))
        }
        IdentityNode::Collection(mut params) => params.find_map(|p| {
            visit(
                ws,
                types,
                p,
                Top::Stored,
                IdentityStorageVia::Container,
                classes,
            )
        }),
        IdentityNode::Nominal(name, params) => {
            if top == Top::Stored && is_identity_struct(ws, name) {
                return Some(IdentityStorageViolation {
                    identity: name,
                    via,
                });
            }
            params.enumerate().find_map(|(i, p)| {
                let arg_top = match param_class(ws, name, i, classes).0 {
                    ParamClass::Phantom => return None,
                    ParamClass::Row => Top::Admitted,
                    ParamClass::Stored => Top::Stored,
                };
                visit(
                    ws,
                    types,
                    p,
                    arg_top,
                    IdentityStorageVia::Container,
                    classes,
                )
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
        let fields = def
            .fields
            .iter()
            .map(|f| (f.ty, is_identity_storage_field(f)))
            .collect();
        return Some((def.generics.clone(), fields));
    }
    if let Some(eid) = ws.enums_by_name.get(&name) {
        let def = &ws.module(eid.module).enums[eid.def];
        let mut fields = Vec::new();
        for variant in &def.variants {
            match &variant.payload {
                VariantPayload::Unit => {}
                VariantPayload::Tuple(types) => fields.extend(types.iter().map(|t| (*t, false))),
                VariantPayload::Record(record) => {
                    fields.extend(record.iter().map(|f| (f.ty, false)))
                }
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
fn param_class(
    ws: &Workspace,
    name: Ustr,
    index: usize,
    classes: &mut ParamClasses,
) -> (ParamClass, bool) {
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

fn row_edge_occurrence(
    ws: &Workspace,
    ty: TyId,
    param: Ustr,
    classes: &mut ParamClasses,
) -> (ParamClass, bool) {
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
fn occurrence(
    ws: &Workspace,
    ty: TyId,
    param: Ustr,
    row: bool,
    classes: &mut ParamClasses,
) -> (ParamClass, bool) {
    let Some(expr) = static_expr(ws, ty) else {
        return (ParamClass::Phantom, false);
    };
    match expr {
        TypeExpr::Generic(_, name) if name == param => (
            if row {
                ParamClass::Row
            } else {
                ParamClass::Stored
            },
            false,
        ),
        TypeExpr::Builtin(_)
        | TypeExpr::Generic(..)
        | TypeExpr::Infer(..)
        | TypeExpr::Function { .. } => (ParamClass::Phantom, false),
        TypeExpr::App {
            ctor: TypeCtor::Ref,
            ..
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
    items.fold((ParamClass::Phantom, false), |(class, cyclic), (c, y)| {
        (class.max(c), cyclic || y)
    })
}

/// The census label for files outside any package: `tests/` programs and
/// programs embedded in Rust tests.
pub(crate) const UNPACKAGED_PROGRAMS: &str = "unpackaged programs";
/// The census label for stdlib files.
pub(crate) const STDLIB_PACKAGE: &str = "std";

/// Programs embedded in Rust tests that declare a package of their own. They
/// belong to [`UNPACKAGED_PROGRAMS`] until S8 rewrites them; the census does
/// not count them.
pub(crate) const RUST_EMBEDDED_PACKAGES: &[&str] = &[
    "enum_json_v1",
    "legacyapp",
    "memclass_handle",
    "memclass_handle_marked",
    "memclass_handle_named",
    "memclass_handle_platform",
    "provider",
    "v1embed",
    "v1spec",
];

/// Why a module's sites are counted by the census instead of reported.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum IdentityStorageExemption {
    /// The compiler-owned stdlib, or a program outside any package.
    Builtin,
    /// Its package's manifest declares the whole package exempt.
    Package,
    /// Its package's manifest declares this module path prefix exempt.
    Module(Ustr),
}

/// The one exemption predicate. Stdlib and unpackaged programs are exempt
/// by the compiler (unpackaged ones unless the workspace enforces them);
/// every package answers with its own manifest's declaration, stamped on
/// the module. A module's path is the `mod` header of its file
/// (`Workspace.fragment_scopes`); a file without one matches no prefix.
pub fn identity_storage_exemption(
    ws: &Workspace,
    module: &Module,
) -> Option<IdentityStorageExemption> {
    match identity_storage_package(module) {
        STDLIB_PACKAGE => return Some(IdentityStorageExemption::Builtin),
        UNPACKAGED_PROGRAMS => {
            return (!ws.identity_storage_enforce_unpackaged)
                .then_some(IdentityStorageExemption::Builtin);
        }
        _ => {}
    }
    match &module.identity_storage {
        IdentityStorage::Enforced => None,
        IdentityStorage::Exempt => Some(IdentityStorageExemption::Package),
        IdentityStorage::ExemptModules(prefixes) => {
            let path = &ws.fragment_scopes.get(&module.file)?.module_path;
            prefixes
                .iter()
                .find(|prefix| module_path_under(path, prefix))
                .map(|prefix| IdentityStorageExemption::Module(Ustr::from(prefix)))
        }
    }
}

fn module_path_under(path: &[Ustr], prefix: &str) -> bool {
    let prefix: Vec<&str> = prefix.split("::").collect();
    path.len() >= prefix.len()
        && path
            .iter()
            .zip(&prefix)
            .all(|(segment, want)| segment.as_str() == *want)
}

/// The census label of the package that owns `module`.
pub(crate) fn identity_storage_package(module: &Module) -> &str {
    if crate::modules::is_stdlib_module_file(module.file.as_str()) {
        STDLIB_PACKAGE
    } else {
        match module.package {
            Some(p) if !RUST_EMBEDDED_PACKAGES.contains(&p.as_str()) => p.as_str(),
            _ => UNPACKAGED_PROGRAMS,
        }
    }
}
