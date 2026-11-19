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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Shape {
    Prim(&'static str),
    Opt(Box<Shape>),
    Tuple(Vec<Shape>),
    Vec(Box<Shape>),
    Map(Box<Shape>, Box<Shape>),
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
                    && ws.struct_by_name.get(&name).is_some_and(|sid| {
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
                (Shape::Nom(type_symbol(ws, name)), true)
            }
        },
    }
}

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
        }) => (type_symbol(ws, name).as_str().to_string(), false),
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
    pub ty: String,
    pub nullable: bool,
    pub embed: bool,
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
    pub leaves: Option<Vec<Ustr>>,
}

#[derive(Clone, Debug)]
pub enum VariantEntry {
    Positional(Vec<PositionEntry>),
    Named(Vec<(Ustr, Shape)>),
}

#[derive(Clone, Debug)]
pub struct CEnumPayload {
    pub name: String,
    pub slot: String,
    pub ty: String,
}

#[derive(Clone, Debug)]
pub enum Entry {
    StridedRows {
        name: Ustr,
        paths: Vec<Ustr>,
    },
    Struct {
        name: Ustr,
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
        variants: Vec<(Ustr, VariantEntry)>,
    },
    CEnum {
        name: Ustr,
        slots: Vec<(Ustr, &'static str)>,
        variants: Vec<(Ustr, Vec<CEnumPayload>)>,
    },
}

impl Entry {
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
                        VariantEntry::Named(named) => {
                            named.iter().for_each(|(_, s)| s.nominals(&mut out))
                        }
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
                ContainerStorage::Columns { .. } => vec_columns = Some(paths),
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
        let factory = |op| factories.get(&(s.name, f.name, op)).cloned();
        fields.push(FieldEntry {
            name: f.name,
            ty,
            nullable: plan.nullable(),
            embed,
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

pub fn enum_entries(ws: &Workspace, e: &EnumDef) -> Vec<Entry> {
    let mut out = Vec::new();
    let name = type_symbol(ws, e.name);
    let plan = EnumPlan::for_name(ws, e.name);
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
    let Some(shape) = plan.and_then(|plan| plan.cdata().cloned()) else {
        if let Some(variants) = boxed_variants(ws, e) {
            out.push(Entry::Enum { name, variants });
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

fn boxed_variants(ws: &Workspace, e: &EnumDef) -> Option<Vec<(Ustr, VariantEntry)>> {
    let flat = EnumPlan::for_name(ws, e.name).and_then(|plan| plan.flat().cloned());
    let mut any = false;
    let mut variants = Vec::new();
    for variant in e.variants.iter() {
        let entry = match &variant.payload {
            VariantPayload::Unit => VariantEntry::Positional(Vec::new()),
            VariantPayload::Tuple(types) => {
                let entries: Vec<PositionEntry> = types
                    .iter()
                    .enumerate()
                    .map(|(i, ty)| {
                        let mut entry = PositionEntry {
                            shape: type_shape(ws, *ty),
                            slot: None,
                            leaves: None,
                        };
                        if let Some(flat) = &flat {
                            entry.slot = Some(flat.slot_offset(i));
                            if let TyId::Static(mid, idx) = *ty
                                && let TypeExpr::App {
                                    ctor: TypeCtor::Nominal(name),
                                    ..
                                } = ws.modules[mid.index()].ty_arena[idx]
                            {
                                entry.leaves = flat.position_leaf_paths(i, name);
                            }
                        }
                        entry
                    })
                    .collect();
                if entries.is_empty() {
                    continue;
                }
                VariantEntry::Positional(entries)
            }
            VariantPayload::Record(fields) => {
                let entries: Vec<(Ustr, Shape)> = fields
                    .iter()
                    .filter_map(|f| Some((f.name, type_shape(ws, f.ty)?)))
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
