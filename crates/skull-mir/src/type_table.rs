//! Facts come from the target's representation plans, so a walker reads values
//! in the layout that backend actually stores.
use bone_core::hir::{BuiltinTy, EnumDef, StructDef, TyId, TypeCtor, TypeExpr, VariantPayload};
use bone_core::workspace::Workspace;
use rustc_hash::FxHashMap;
use skull_parser::{CompilerError, JsonFieldOperation};
use ustr::Ustr;

use crate::lower::{
    ContainerFamily, ContainerPlan, ContainerStorage, EnumPlan, RepArena, StructFieldPlan,
};
use crate::mangle::type_symbol;
use crate::monoty::{MonoCtor, MonoTy};

/// A shape with no typed leaf is absent from [`type_shape`]; inside a typed
/// shape an unsupported part stays `Prim("any")`, which rebuilding walkers reject.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Shape {
    Prim(&'static str),
    Opt(Box<Shape>),
    Tuple(Vec<Shape>),
    Vec(Box<Shape>),
    Map(Box<Shape>, Box<Shape>),
    /// The stdlib `JsonField<T>` presence wrapper.
    Field(Box<Shape>),
    Nom(Ustr),
}

impl Shape {
    pub fn nominals(&self, out: &mut Vec<Ustr>) {
        match self {
            Shape::Prim(_) => {}
            Shape::Opt(inner) | Shape::Vec(inner) | Shape::Field(inner) => inner.nominals(out),
            Shape::Tuple(elems) => elems.iter().for_each(|e| e.nominals(out)),
            Shape::Map(key, val) => {
                key.nominals(out);
                val.nominals(out);
            }
            Shape::Nom(name) => out.push(*name),
        }
    }
}

pub fn type_shape(ws: &Workspace, ty: TyId) -> Option<Shape> {
    let (shape, found) = shape_of(ws, ty, 0);
    found.then_some(shape)
}

/// [`type_shape`] of a substituted type.
fn mono_shape(ws: &Workspace, ty: &MonoTy, depth: u32) -> Option<Shape> {
    const MAX_DEPTH: u32 = 12;
    if depth >= MAX_DEPTH {
        return None;
    }
    let nested = |ty| mono_shape(ws, ty, depth + 1).unwrap_or(Shape::Prim("any"));
    let found = |shape: Shape| {
        let mut nominals = Vec::new();
        shape.nominals(&mut nominals);
        (!nominals.is_empty() || shape_has_prim(&shape)).then_some(shape)
    };
    match ty {
        MonoTy::Int => Some(Shape::Prim("int")),
        MonoTy::Uint => Some(Shape::Prim("uint")),
        MonoTy::Float => Some(Shape::Prim("float")),
        MonoTy::Bool => Some(Shape::Prim("bool")),
        MonoTy::String => Some(Shape::Prim("string")),
        MonoTy::Nullable(inner) => found(Shape::Opt(Box::new(nested(inner)))),
        MonoTy::Tuple(parts) => found(Shape::Tuple(parts.iter().map(nested).collect())),
        MonoTy::App {
            ctor: MonoCtor::Array,
            params,
        } => found(Shape::Vec(Box::new(nested(params.first()?)))),
        MonoTy::App {
            ctor: MonoCtor::Map,
            params,
        } => found(Shape::Map(
            Box::new(nested(params.first()?)),
            Box::new(nested(params.get(1)?)),
        )),
        MonoTy::App {
            ctor: MonoCtor::Nominal(name),
            params,
        } if name.as_str() == "JsonField" && params.len() == 1 => {
            found(Shape::Field(Box::new(nested(&params[0]))))
        }
        MonoTy::App {
            ctor: MonoCtor::Nominal(name),
            ..
        } => Some(Shape::Nom(type_symbol(ws, *name))),
        _ => None,
    }
}

fn shape_has_prim(shape: &Shape) -> bool {
    match shape {
        Shape::Prim(name) => *name != "any",
        Shape::Opt(inner) | Shape::Vec(inner) | Shape::Field(inner) => shape_has_prim(inner),
        Shape::Tuple(parts) => parts.iter().any(shape_has_prim),
        Shape::Map(key, val) => shape_has_prim(key) || shape_has_prim(val),
        Shape::Nom(_) => true,
    }
}

/// Nominal leaves stop recursion; structural nesting has a finite depth bound.
fn shape_of(ws: &Workspace, ty: TyId, depth: u32) -> (Shape, bool) {
    const MAX_DEPTH: u32 = 12;
    const ANY: (Shape, bool) = (Shape::Prim("any"), false);
    if depth >= MAX_DEPTH {
        return ANY;
    }
    let te = match ty {
        TyId::Static(m, idx) => match ws.modules.get(m.index()) {
            Some(module) => module.ty_arena[idx],
            None => return ANY,
        },
        // A local arena type never reaches a declaration's descriptor.
        TyId::Local(_) => return ANY,
    };
    let nested = |inner| shape_of(ws, ty.with_idx(inner), depth + 1);
    match te {
        TypeExpr::Builtin(kind) => {
            let name = match kind {
                BuiltinTy::Int => "int",
                BuiltinTy::Uint => "uint",
                BuiltinTy::Float => "float",
                BuiltinTy::Bool => "bool",
                BuiltinTy::String => "string",
                _ => "any",
            };
            (Shape::Prim(name), name != "any")
        }
        TypeExpr::Generic(..) | TypeExpr::Infer(..) | TypeExpr::Function { .. } => ANY,
        TypeExpr::Nullable(inner) => {
            let (inner, found) = nested(inner);
            (Shape::Opt(Box::new(inner)), found)
        }
        TypeExpr::Tuple { elements } => {
            let mut found = false;
            let elems = elements
                .into_iter()
                .map(|e| {
                    let (shape, f) = nested(e);
                    found |= f;
                    shape
                })
                .collect();
            (Shape::Tuple(elems), found)
        }
        TypeExpr::App { ctor, params } => match ctor {
            TypeCtor::Array => {
                let Some(elem) = params.into_iter().next() else {
                    return ANY;
                };
                let (elem, found) = nested(elem);
                (Shape::Vec(Box::new(elem)), found)
            }
            TypeCtor::Map => {
                let mut params = params.into_iter();
                let (Some(key), Some(val)) = (params.next(), params.next()) else {
                    return ANY;
                };
                let (key, key_found) = nested(key);
                let (val, val_found) = nested(val);
                (
                    Shape::Map(Box::new(key), Box::new(val)),
                    key_found || val_found,
                )
            }
            TypeCtor::Ref => {
                unreachable!("frame references are rejected from reflected storage")
            }
            TypeCtor::Nominal(name) => {
                if name.as_str() == "JsonField"
                    && ws.nominals.struct_id(name).is_some_and(|sid| {
                        bone_core::modules::is_stdlib_module_file(
                            ws.module(sid.module).file.as_str(),
                        )
                    })
                {
                    let Some(inner) = params.into_iter().next() else {
                        return ANY;
                    };
                    let (inner, found) = nested(inner);
                    return (Shape::Field(Box::new(inner)), found);
                }
                // A struct's own fields are reached through its own entry.
                // Over an unbound generic, the declaration names the type.
                let layout = crate::lower::static_layout_nominal(ws, ty).unwrap_or(name);
                (Shape::Nom(type_symbol(ws, layout)), true)
            }
        },
    }
}

/// The string matches `to_simple_string` for int/float/string/bool, a nominal
/// name and `[..]`; anything else is "any".
pub fn reflect_field_type(ws: &Workspace, ty: TyId) -> (String, bool) {
    let resolve = |t: TyId| -> Option<TypeExpr> {
        match t {
            TyId::Static(mid, idx) => ws.modules.get(mid.index()).map(|m| m.ty_arena[idx]),
            TyId::Local(_) => None,
        }
    };
    match resolve(ty) {
        Some(TypeExpr::Builtin(BuiltinTy::Int)) => ("int".into(), false),
        Some(TypeExpr::Builtin(BuiltinTy::Uint)) => ("uint".into(), false),
        Some(TypeExpr::Builtin(BuiltinTy::Float)) => ("float".into(), false),
        Some(TypeExpr::Builtin(BuiltinTy::Bool)) => ("bool".into(), false),
        Some(TypeExpr::Builtin(BuiltinTy::String)) => ("string".into(), false),
        Some(TypeExpr::Nullable(_)) => ("any".into(), true),
        Some(TypeExpr::App {
            ctor: TypeCtor::Nominal(name),
            ..
        }) => {
            let layout = crate::lower::static_layout_nominal(ws, ty).unwrap_or(name);
            (type_symbol(ws, layout).as_str().to_string(), false)
        }
        Some(TypeExpr::App {
            ctor: TypeCtor::Array,
            ..
        }) => ("[]".into(), false),
        _ => ("any".into(), false),
    }
}

pub fn json_field_factories(ws: &Workspace) -> FxHashMap<(Ustr, Ustr, JsonFieldOperation), String> {
    ws.modules
        .iter()
        .flat_map(|m| m.functions.iter())
        .filter_map(|(_, f)| {
            f.json_field_for
                .map(|owner| (owner, crate::mangle::mangle_fn(ws, f)))
        })
        .collect()
}

#[derive(Clone, Debug)]
pub struct FieldEntry {
    pub name: Ustr,
    /// The reflection type string, or the inline child's type symbol.
    pub ty: String,
    pub nullable: bool,
    /// The child's leaves live in the parent under `<name>.<leaf>` keys.
    pub embed: bool,
    /// Only once this struct is spliced into a holder do the child's leaves
    /// live under `<name>.<leaf>` keys; standalone it keeps the child whole.
    pub leaf: bool,
    /// The parent's physical keys of a flattened enum field: tag, then slots.
    pub flat: Option<Vec<Ustr>>,
    pub shape: Option<Shape>,
    pub json_name: Option<Ustr>,
    pub json_default: Option<String>,
    pub json_omit: Option<String>,
    pub json_skip: bool,
}

#[derive(Clone, Debug)]
pub struct PositionEntry {
    pub shape: Option<Shape>,
    pub slot: Option<usize>,
    /// Leaf paths of a value payload spread across consecutive slots.
    pub leaves: Option<Vec<Ustr>>,
    /// Indices of nested presence slots, relative to `leaves`.
    pub presence: Vec<usize>,
}

impl PositionEntry {
    fn from_layout(
        shape: Option<Shape>,
        slot: Option<usize>,
        layout: Option<&crate::lower::ValueShape>,
        track_presence: bool,
    ) -> Self {
        let leaves = layout.map(crate::lower::value_leaf_paths);
        let presence = layout
            .filter(|_| track_presence)
            .map_or_else(Vec::new, |layout| {
                crate::lower::value_leaf_presence_flags(layout)
                    .into_iter()
                    .enumerate()
                    .filter_map(|(i, presence)| presence.then_some(i))
                    .collect()
            });
        Self {
            shape,
            slot,
            leaves,
            presence,
        }
    }
}

#[derive(Clone, Debug)]
pub enum VariantEntry {
    Positional(Vec<PositionEntry>),
    Named(Vec<(Ustr, PositionEntry)>),
}

impl VariantEntry {
    pub fn has_inline_record_payload(&self) -> bool {
        match self {
            Self::Positional(positions) => positions.iter().any(|p| {
                p.leaves.is_some()
                    && (matches!(p.shape, Some(Shape::Opt(_))) || !p.presence.is_empty())
            }),
            Self::Named(fields) => fields.iter().any(|(_, p)| p.leaves.is_some()),
        }
    }
}

#[derive(Clone, Debug)]
pub struct CEnumPayload {
    pub name: String,
    pub slot: String,
    pub ty: String,
}

#[derive(Clone, Debug)]
pub enum Entry {
    /// A `value` element whose Vec stores interleaved rows; paths in slot order.
    StridedRows {
        name: Ustr,
        paths: Vec<Ustr>,
    },
    Struct {
        name: Ustr,
        /// `struct`, `value_table` (GC-carrying value) or `c_struct` (GC-free value).
        kind: &'static str,
        fields: Vec<FieldEntry>,
        vec_columns: Option<Vec<Ustr>>,
    },
    Newtype {
        name: Ustr,
    },
    FlatLayout {
        name: Ustr,
        tuple_width: usize,
        record_fields: Vec<Ustr>,
    },
    EnumTag {
        name: Ustr,
        variants: Vec<Ustr>,
    },
    Enum {
        name: Ustr,
        /// An instance shows its declaration's name.
        display: Option<Ustr>,
        variants: Vec<(Ustr, VariantEntry)>,
    },
    CEnum {
        name: Ustr,
        slots: Vec<(Ustr, &'static str)>,
        variants: Vec<(Ustr, Vec<CEnumPayload>)>,
    },
}

impl Entry {
    /// Primitive type strings are included; a caller resolves only declared ones.
    pub fn referenced_types(&self) -> Vec<Ustr> {
        let mut out = Vec::new();
        match self {
            Entry::Struct { fields, .. } => {
                for f in fields {
                    if let Some(shape) = &f.shape {
                        shape.nominals(&mut out);
                    }
                    out.push(Ustr::from(f.ty.as_str()));
                }
            }
            Entry::Enum { variants, .. } => {
                for (_, v) in variants {
                    match v {
                        VariantEntry::Positional(pos) => pos
                            .iter()
                            .filter_map(|p| p.shape.as_ref())
                            .for_each(|s| s.nominals(&mut out)),
                        VariantEntry::Named(named) => named
                            .iter()
                            .filter_map(|(_, p)| p.shape.as_ref())
                            .for_each(|s| s.nominals(&mut out)),
                    }
                }
            }
            Entry::CEnum { variants, .. } => {
                for (_, payload) in variants {
                    out.extend(payload.iter().map(|p| Ustr::from(p.ty.as_str())));
                }
            }
            Entry::StridedRows { .. }
            | Entry::Newtype { .. }
            | Entry::FlatLayout { .. }
            | Entry::EnumTag { .. } => {}
        }
        out
    }
}

fn plan_error(message: String) -> CompilerError {
    CompilerError::new_no_location(message)
}

/// The strided row layout, if any, then the struct. `is_gc` names GC-carrying
/// `value` structs, which are tables rather than GC-free native values.
pub fn struct_entries(
    ws: &Workspace,
    target: bone_core::hir::FnBackend,
    arena: &RepArena,
    is_gc: &dyn Fn(Ustr) -> bool,
    factories: &FxHashMap<(Ustr, Ustr, JsonFieldOperation), String>,
    s: &StructDef,
) -> Result<Vec<Entry>, CompilerError> {
    let mut out = Vec::new();
    let mut vec_columns = None;
    if s.is_value {
        let ty = MonoTy::App {
            ctor: MonoCtor::Array,
            params: vec![MonoTy::App {
                ctor: MonoCtor::Nominal(s.name),
                params: Vec::new(),
            }],
        };
        let plan = ContainerPlan::for_type(ws, target, arena, ContainerFamily::Vec, &ty)
            .map_err(|e| plan_error(e.message))?;
        if let Some(shape) = plan.element_shape(arena) {
            let paths = crate::lower::value_leaf_paths(&shape);
            match plan.storage() {
                // Box reconstruction uses logical field paths. Escaped columns
                // belong to nobox elements, which can only be read as leaves.
                ContainerStorage::Columns { .. }
                    if crate::lower::column_leaf_paths(&shape) == paths =>
                {
                    vec_columns = Some(paths);
                }
                ContainerStorage::Strided { .. } => out.push(Entry::StridedRows {
                    name: type_symbol(ws, s.name),
                    paths,
                }),
                _ => {}
            }
        }
    }
    let kind = if !crate::lower::has_value_plan(s) {
        "struct"
    } else if is_gc(s.name) {
        "value_table"
    } else {
        "c_struct"
    };
    let mut fields = Vec::with_capacity(s.fields.len());
    for f in &s.fields {
        let plan = crate::lower::struct_field_plan(ws, target, s.name, f.name, false)
            .map_err(|e| plan_error(e.message))?;
        let (ty, embed) = match &plan {
            StructFieldPlan::Inline { child, flat, .. } => {
                (type_symbol(ws, *child).as_str().to_string(), flat.is_none())
            }
            _ => (reflect_field_type(ws, f.ty).0, false),
        };
        let leaf = plan.is_direct()
            && !crate::lower::struct_field_plan(ws, target, s.name, f.name, true)
                .map_err(|e| plan_error(e.message))?
                .is_direct();
        let factory = |op| factories.get(&(s.name, f.name, op)).cloned();
        fields.push(FieldEntry {
            name: f.name,
            ty,
            nullable: plan.nullable(),
            embed,
            leaf,
            flat: plan.flat_storage_keys(f.name),
            shape: type_shape(ws, f.ty),
            json_name: bone_core::annotations::json_field_name(&f.annotations),
            json_default: factory(JsonFieldOperation::Default),
            json_omit: factory(JsonFieldOperation::Omit),
            json_skip: f.annotations.iter().any(|a| a.is_builtin("json_skip")),
        });
    }
    out.push(Entry::Struct {
        name: type_symbol(ws, s.name),
        kind,
        fields,
        vec_columns,
    });
    Ok(out)
}

/// The flat layout, if any, then the enum when it has a walkable form. `name`
/// is a declaration or one of its instances.
pub fn enum_entries(ws: &Workspace, name: Ustr) -> Vec<Entry> {
    let mut out = Vec::new();
    let Some(e) = crate::lower::enum_decl(ws, name) else {
        return out;
    };
    let plan = EnumPlan::for_name(ws, name);
    let payload_shape = |ty: TyId| match crate::lower::instance_payload_ty(ws, name, ty) {
        Some(mono) => mono_shape(ws, &mono, 0),
        None => type_shape(ws, ty),
    };
    let display = (e.name != name).then(|| type_symbol(ws, e.name));
    let name = type_symbol(ws, name);
    if let Some(flat) = plan.as_ref().and_then(|plan| plan.flat()) {
        out.push(Entry::FlatLayout {
            name,
            tuple_width: flat.tuple_width(),
            record_fields: flat.record_fields().to_vec(),
        });
    }
    if plan.as_ref().is_none_or(|p| p.cdata().is_none())
        && !e.variants.is_empty()
        && e.variants
            .iter()
            .all(|v| matches!(v.payload, VariantPayload::Unit))
    {
        out.push(Entry::EnumTag {
            name,
            variants: e.variants.iter().map(|v| v.name).collect(),
        });
        return out;
    }
    let Some(shape) = plan.as_ref().and_then(|plan| plan.cdata().cloned()) else {
        let flat = plan.as_ref().and_then(|plan| plan.flat());
        if let Some(variants) = boxed_variants(ws, e, flat, payload_shape) {
            out.push(Entry::Enum {
                name,
                display,
                variants,
            });
        }
        return out;
    };
    let variants = e
        .variants
        .iter()
        .map(|variant| {
            let payload = match &variant.payload {
                VariantPayload::Unit => Vec::new(),
                VariantPayload::Tuple(types) => types
                    .iter()
                    .enumerate()
                    .map(|(index, ty)| CEnumPayload {
                        name: (index + 1).to_string(),
                        slot: format!("_{index}"),
                        ty: reflect_field_type(ws, *ty).0,
                    })
                    .collect(),
                VariantPayload::Record(fields) => fields
                    .iter()
                    .map(|field| CEnumPayload {
                        name: field.name.to_string(),
                        slot: format!("_{}", field.name),
                        ty: reflect_field_type(ws, field.ty).0,
                    })
                    .collect(),
            };
            (variant.name, payload)
        })
        .collect();
    out.push(Entry::CEnum {
        name,
        slots: shape.slots.clone(),
        variants,
    });
    out
}

/// Complete alternatives, including units and primitive payloads.
fn boxed_variants(
    ws: &Workspace,
    e: &EnumDef,
    flat: Option<&crate::lower::EnumFlatShape>,
    payload_shape: impl Fn(TyId) -> Option<Shape>,
) -> Option<Vec<(Ustr, VariantEntry)>> {
    let mut any = false;
    let mut variants = Vec::new();
    for variant in e.variants.iter() {
        let entry = match &variant.payload {
            VariantPayload::Unit => VariantEntry::Positional(Vec::new()),
            VariantPayload::Tuple(types) => {
                let entries: Vec<_> = types
                    .iter()
                    .enumerate()
                    .map(|(i, ty)| {
                        let layout = flat.and_then(|flat| flat.payload_shape(ws, variant.name, i));
                        PositionEntry::from_layout(
                            payload_shape(*ty),
                            flat.map(|flat| flat.slot_offset(i)),
                            layout,
                            layout
                                .is_some_and(|s| crate::lower::track_enum_payload_presence(ws, s)),
                        )
                    })
                    .collect();
                if entries.is_empty() {
                    continue;
                }
                VariantEntry::Positional(entries)
            }
            VariantPayload::Record(fields) => {
                let entries: Vec<_> = fields
                    .iter()
                    .filter_map(|f| {
                        let shape = payload_shape(f.ty)?;
                        let layout = flat
                            .and_then(|flat| flat.named_payload(variant.name, f.name))
                            .map(|payload| &payload.shape);
                        Some((
                            f.name,
                            PositionEntry::from_layout(Some(shape), None, layout, true),
                        ))
                    })
                    .collect();
                if entries.is_empty() {
                    continue;
                }
                VariantEntry::Named(entries)
            }
        };
        any = true;
        variants.push((variant.name, entry));
    }
    any.then_some(variants)
}
