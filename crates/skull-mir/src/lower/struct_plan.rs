use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use bone_core::hir::{FnBackend, TypeCtor, TypeExpr};
use bone_core::prelude::TyId;
use bone_core::workspace::Workspace;
use ustr::Ustr;

use super::shape::{GcFieldEq, PayloadTy, declared_value_storage, static_ty};
use super::{EnumFlatShape, EnumPlan, RepArena, RepError, RepId, ValueShape, ValueStorage};
use crate::lua_nesting::{LuaDisplayPart, display_record};
use crate::mir::{Inst, MirFunc};

/// One physical answer for one logical field: inline/nobox fields and flat
/// enum payloads share one inline layout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StructFieldPlan {
    Direct {
        child: Option<Ustr>,
        nullable: bool,
    },
    Inline {
        child: Ustr,
        nullable: bool,
        flat: Option<EnumFlatShape>,
    },
}

impl StructFieldPlan {
    pub fn direct_child(&self) -> Option<Ustr> {
        match self {
            Self::Direct { child, .. } => *child,
            Self::Inline { .. } => None,
        }
    }

    pub fn child(&self) -> Option<Ustr> {
        self.direct_child().or_else(|| self.inline_child())
    }

    pub fn is_direct(&self) -> bool {
        matches!(self, Self::Direct { .. })
    }

    pub fn embed(&self) -> Option<(Ustr, bool)> {
        match self {
            Self::Inline {
                child,
                nullable,
                flat: None,
            } => Some((*child, *nullable)),
            _ => None,
        }
    }

    pub fn inline_child(&self) -> Option<Ustr> {
        self.embed().map(|(child, _)| child)
    }

    pub(crate) fn flat_shape(&self) -> Option<EnumFlatShape> {
        match self {
            Self::Inline {
                flat: Some(shape), ..
            } => Some(shape.clone()),
            _ => None,
        }
    }

    pub fn flat_storage_keys(&self, field: Ustr) -> Option<Vec<Ustr>> {
        match self {
            Self::Inline {
                flat: Some(shape), ..
            } => Some(super::flat_slot_names(field, shape)),
            _ => None,
        }
    }

    pub fn nullable(&self) -> bool {
        match self {
            Self::Direct { nullable, .. } | Self::Inline { nullable, .. } => *nullable,
        }
    }
}

/// The keys a table parent stores an inline `nobox` child under, relative to
/// the field: the same leaves embed readers and writers address.
pub fn nobox_leaf_paths(ws: &Workspace, name: Ustr) -> Option<Vec<Ustr>> {
    if !super::shape::is_nobox_nominal(ws, name) {
        return None;
    }
    Some(super::value_leaf_paths(
        &super::shape::value_shape_of_nominal(ws, name)?,
    ))
}

pub const CDATA_LEAF_PREFIX: &str = "__rock_leaf_";

/// A native primitive projection aliases the original nested bytes. Hex paths
/// keep dots injective; avoid the entire declared-field prefix namespace.
pub fn cdata_leaf_alias(ws: &Workspace, owner: Ustr, path: &str) -> Ustr {
    let sid = ws.nominals.struct_id(owner).unwrap();
    let fields = &ws.modules[sid.module.index()].structs[sid.def].fields;
    let mut prefix = CDATA_LEAF_PREFIX.to_string();
    while fields
        .iter()
        .any(|field| field.name.as_str().starts_with(&prefix))
    {
        prefix.push('_');
    }
    use std::fmt::Write as _;
    for byte in path.bytes() {
        let _ = write!(prefix, "{byte:02x}");
    }
    crate::names::ustr(&prefix)
}

/// Closing over declared child fields also owns the value representation of a
/// child that enters through a host boundary rather than a Rock constructor.
pub struct StructRegistryPlan {
    arena: Arc<RepArena>,
    names: BTreeSet<Ustr>,
    value_plans: BTreeMap<Ustr, RepId>,
    column_plans: BTreeMap<Ustr, (bool, RepId)>,
}

impl StructRegistryPlan {
    pub fn for_bodies(
        ws: &Workspace,
        target: FnBackend,
        bodies: &[&MirFunc],
    ) -> Result<Option<Self>, RepError> {
        if bodies.is_empty() {
            return Ok(None);
        }
        let arena = bodies
            .iter()
            .find_map(|body| body.rep_arena.as_ref().cloned())
            .ok_or_else(|| plan_error("struct registry has no representation arena"))?;
        let mut names = BTreeSet::new();
        let mut column_plans = BTreeMap::new();
        for body in bodies {
            collect_struct_roots(ws, body, &arena, &mut names)?;
            crate::mir::column_elem_rep_plans(body, &mut column_plans);
        }
        names.extend(column_plans.keys().copied());

        let mut value_plans: BTreeMap<Ustr, RepId> =
            arena.value_plan_catalog().into_iter().collect();
        let mut queue: Vec<Ustr> = names.iter().copied().collect();
        while let Some(name) = queue.pop() {
            let sid = ws.nominals.struct_id(name).ok_or_else(|| {
                plan_error(format!("struct registry type `{name}` is not declared"))
            })?;
            let s = &ws.modules[sid.module.index()].structs[sid.def];
            if crate::lower::has_value_plan(s) && !value_plans.contains_key(&name) {
                value_plans.insert(name, arena.plan_nominal_value(ws, target, name)?);
            }
            for field in &s.fields {
                let plan = struct_field_plan(ws, target, name, field.name, false)?;
                if let Some(child) = plan
                    .child()
                    .filter(|child| ws.nominals.struct_id(*child).is_some())
                {
                    if names.insert(child) {
                        queue.push(child);
                    }
                }
            }
        }
        value_plans.retain(|name, _| names.contains(name));
        Ok(Some(Self {
            arena,
            names,
            value_plans,
            column_plans,
        }))
    }

    pub fn arena(&self) -> &RepArena {
        &self.arena
    }

    pub fn names(&self) -> &BTreeSet<Ustr> {
        &self.names
    }

    pub fn value_plans(&self) -> &BTreeMap<Ustr, RepId> {
        &self.value_plans
    }

    pub fn column_plans(&self) -> &BTreeMap<Ustr, (bool, RepId)> {
        &self.column_plans
    }
}

fn collect_struct_roots(
    ws: &Workspace,
    body: &MirFunc,
    arena: &Arc<RepArena>,
    names: &mut BTreeSet<Ustr>,
) -> Result<(), RepError> {
    let local = body
        .rep_arena
        .as_ref()
        .ok_or_else(|| plan_error(format!("`{}` has no representation arena", body.name)))?;
    if !Arc::ptr_eq(local, arena) {
        return Err(plan_error(format!(
            "`{}` carries a representation plan from another arena",
            body.name
        )));
    }
    names.extend(body.struct_registry_roots.iter().copied());
    for inst in body.insts.values() {
        let name = match inst {
            Inst::MakeStruct { ty: Some(name), .. }
            | Inst::MakeValueStruct { name, .. }
            | Inst::MaterializeValueStruct { name, .. } => *name,
            _ => continue,
        };
        if ws.nominals.struct_id(name).is_some() {
            names.insert(name);
        }
    }
    for nested in &body.nested_fns {
        collect_struct_roots(ws, nested, arena, names)?;
    }
    Ok(())
}

fn plan_error(message: impl Into<String>) -> RepError {
    RepError {
        span: None,
        message: message.into(),
    }
}

fn resolved_field_type(ws: &Workspace, ty: TyId) -> Result<(TypeExpr, bool), RepError> {
    let mut ty = ty;
    let mut nullable = false;
    loop {
        let Some(node) = static_ty(ws, ty) else {
            return Err(plan_error("struct field has no resolved type"));
        };
        match node {
            TypeExpr::Nullable(inner) => {
                nullable = true;
                ty = ty.with_idx(inner);
            }
            TypeExpr::Infer(..) => {
                return Err(plan_error("struct field has an unresolved type"));
            }
            other => return Ok((other, nullable)),
        }
    }
}

/// Every consumer that inspects or lowers a struct field uses this answer; a
/// missing declaration or unresolved field type is never direct storage.
pub fn struct_field_plan(
    ws: &Workspace,
    target: FnBackend,
    sname: Ustr,
    field: Ustr,
    spliced: bool,
) -> Result<StructFieldPlan, RepError> {
    let Some(sid) = ws.nominals.struct_id(sname) else {
        return Err(plan_error(format!("struct `{sname}` has no field plan")));
    };
    let sd = &ws.modules[sid.module.index()].structs[sid.def];
    let Some(fdef) = sd.fields.iter().find(|f| f.name == field) else {
        return Err(plan_error(format!(
            "struct `{sname}` has no field `{field}`"
        )));
    };
    let subst = crate::layout_instance::layout_subst(ws, sname);
    let (child_name, nullable) = match PayloadTy::of(ws, fdef.ty, subst.as_ref()) {
        Some(PayloadTy::Mono(ty)) => {
            let (inner, nullable) = ty.peel_nullable();
            (inner.nominal(), nullable)
        }
        _ => {
            let (resolved, nullable) = resolved_field_type(ws, fdef.ty)?;
            let child = match resolved {
                TypeExpr::App {
                    ctor: TypeCtor::Nominal(_),
                    ..
                } => super::shape::nominal_name_from_ty(ws, fdef.ty).map(|(name, _)| name),
                _ => None,
            };
            (child, nullable)
        }
    };
    let Some(child_name) = child_name else {
        return Ok(StructFieldPlan::Direct {
            child: None,
            nullable,
        });
    };
    let child = ws
        .nominals
        .struct_id(child_name)
        .map(|sid| &ws.modules[sid.module.index()].structs[sid.def]);
    let known_nominal = bone_core::modules::is_intrinsic_key(child_name, "Coroutine")
        || child.is_some()
        || ws.nominals.enum_id(child_name).is_some()
        || ws.nominals.newtype_id(child_name).is_some();
    if !known_nominal {
        return Err(plan_error(format!(
            "struct `{sname}` field `{field}` has missing resolved type `{child_name}`"
        )));
    }
    if fdef.is_embed && child.is_none() {
        return Err(plan_error(format!(
            "struct `{sname}` field `{field}` embeds missing struct `{child_name}`"
        )));
    }

    let parent_is_value = crate::lower::has_value_plan(sd);
    let parent_storage = if parent_is_value && !spliced {
        Some(declared_value_storage(ws, target, sname).ok_or_else(|| {
            plan_error(format!(
                "struct `{sname}` has no resolved value storage plan"
            ))
        })?)
    } else {
        None
    };
    // Only a cdata parent nests a child whole; a table parent keys an inline
    // child's leaves by dotted path. Leaf-table storage and a value spliced into
    // its holder keep every planned nested value, enums included, in leaves.
    let nested_leaves = parent_is_value
        && (spliced || parent_storage == Some(ValueStorage::LeafTable))
        && super::shape::planned_field_is_nested(ws, sname, field);
    if (nested_leaves || !parent_is_value)
        && let Some(shape) =
            EnumPlan::for_name(ws, child_name).and_then(|plan| plan.flat().cloned())
    {
        return Ok(StructFieldPlan::Inline {
            child: child_name,
            nullable: false,
            flat: Some(shape),
        });
    }
    if (fdef.is_embed || child.is_some_and(|s| s.is_nobox) || nested_leaves)
        && (spliced || !parent_is_value || parent_storage != Some(ValueStorage::CData))
    {
        return Ok(StructFieldPlan::Inline {
            child: child_name,
            nullable,
            flat: None,
        });
    }
    Ok(StructFieldPlan::Direct {
        child: Some(child_name),
        nullable,
    })
}

/// Whether a struct instance lays out exactly like its declaration, which then
/// serves it under the declaration's name. A boxed child's type is not layout,
/// and a `@lua_named` struct's keys are a Lua contract, never respecialized.
pub(crate) fn struct_instance_matches_declaration(ws: &Workspace, name: Ustr) -> bool {
    let base = crate::layout_instance::decl_name(name);
    let Some(sid) = ws.nominals.struct_id(base) else {
        return true;
    };
    let sd = &ws.modules[sid.module.index()].structs[sid.def];
    if sd.lua_named {
        return true;
    }
    let physical = |owner: Ustr, field: Ustr| {
        struct_field_plan(ws, FnBackend::Lua, owner, field, false)
            .ok()
            .filter(|plan| !plan.is_direct())
    };
    let value = |owner: Ustr| {
        super::shape::planned_value(ws, owner).map(|plan| (plan.storage, plan.shape.fields.clone()))
    };
    sd.fields
        .iter()
        .all(|field| physical(name, field.name) == physical(base, field.name))
        && value(name) == value(base)
}

/// One declared field of a `<Name>_vt` box: a single key, or an inline child
/// keyed by dotted path with its presence tag at the field's own name.
#[derive(Clone, Debug)]
pub struct GcTableField {
    pub name: Ustr,
    eq: GcFieldEq,
    pub(crate) inline: Option<ValueShape>,
}

impl GcTableField {
    /// Box keys in ctor order, each flagged when it is a nullable child's presence
    /// tag rather than a data leaf.
    pub fn keys(&self) -> Vec<(Ustr, bool)> {
        let Some(sub) = &self.inline else {
            return vec![(self.name, false)];
        };
        super::shape::leaf_keys(sub, self.name.as_str())
    }

    pub fn is_inline(&self) -> bool {
        self.inline.is_some()
    }

    pub fn keys_eq(&self, ws: &Workspace) -> Vec<GcFieldEq> {
        let Some(sub) = &self.inline else {
            return vec![self.eq];
        };
        let mut out = Vec::new();
        leaf_eq(ws, sub, true, &mut out);
        out
    }

    /// An inline child renders as its own box would.
    pub fn lua_display_parts(&self, ws: &Workspace) -> Vec<LuaDisplayPart> {
        match &self.inline {
            None => vec![LuaDisplayPart::leaf(&format!("v.{}", self.name))],
            Some(sub) => lua_display_inline(ws, sub, self.name.as_str()),
        }
    }
}

fn leaf_eq(ws: &Workspace, shape: &ValueShape, nested: bool, out: &mut Vec<GcFieldEq>) {
    if shape.nullable && nested {
        out.push(GcFieldEq::Native);
    }
    if ws.nominals.enum_id(shape.name).is_some() {
        // Native equality on flat enum payload slots would skip the enum's contents walk.
        let payload_eq = if super::shape::flat_enum_equality_is_leafwise(ws, shape.name) {
            GcFieldEq::Native
        } else {
            GcFieldEq::Structural
        };
        out.push(GcFieldEq::Native);
        out.extend(std::iter::repeat_n(payload_eq, shape.leaf_count() - 1));
        return;
    }
    let decl = ws
        .nominals
        .struct_id(shape.name)
        .map(|sid| &ws.modules[sid.module.index()].structs[sid.def]);
    for (fname, fs) in &shape.fields {
        match fs {
            super::FieldShape::Prim => out.push(
                decl.and_then(|s| s.fields.iter().find(|f| f.name == *fname))
                    .map_or(GcFieldEq::Native, |f| {
                        super::shape::owner_field_eq(ws, shape.name, f.ty)
                    }),
            ),
            super::FieldShape::Nested(sub) => leaf_eq(ws, sub, true, out),
        }
    }
}

/// The labelled fields of a value whose leaves sit under `v["<prefix><field>"]`.
pub(super) fn lua_display_fields(
    ws: &Workspace,
    shape: &ValueShape,
    prefix: &str,
) -> Vec<(String, Vec<LuaDisplayPart>)> {
    shape
        .fields
        .iter()
        .map(|(fname, fs)| {
            let key = format!("{prefix}{fname}");
            let value = match fs {
                super::FieldShape::Prim => {
                    vec![LuaDisplayPart::leaf(&format!("v[\"{key}\"]"))]
                }
                super::FieldShape::Nested(sub) => lua_display_inline(ws, sub, &key),
            };
            (format!("{fname}: "), value)
        })
        .collect()
}

fn lua_display_inline(ws: &Workspace, shape: &ValueShape, prefix: &str) -> Vec<LuaDisplayPart> {
    if let Some(plan) = EnumPlan::for_name(ws, shape.name) {
        let leaf = |l: &str| format!("v[\"{prefix}.{l}\"]");
        let symbol = crate::mangle::type_symbol(ws, shape.name);
        let lua = if let Some(cdata) = plan.cdata() {
            let args: Vec<String> = cdata.field_names().iter().map(|l| leaf(l)).collect();
            format!("tostring({symbol}_ct({}))", args.join(", "))
        } else {
            let fields: Vec<String> = std::iter::once(format!("__variant = {}", leaf("#")))
                .chain(
                    shape
                        .fields
                        .iter()
                        .skip(1)
                        .map(|(l, _)| match l.parse::<usize>() {
                            Ok(_) => leaf(l),
                            Err(_) => format!("{l} = {}", leaf(l)),
                        }),
                )
                .collect();
            format!(
                "tostring(setmetatable({{ {} }}, __rock_box_mts[\"{symbol}\"]))",
                fields.join(", ")
            )
        };
        return vec![LuaDisplayPart::Enum {
            name: shape.name,
            prefix: prefix.to_string(),
            lua,
        }];
    }
    let name = crate::layout_instance::display_name(shape.name);
    let fields = lua_display_fields(ws, shape, &format!("{prefix}."));
    let body = if fields.is_empty() {
        vec![LuaDisplayPart::literal(&format!("{} {{ }}", name))]
    } else {
        display_record(&format!("{} {{ ", name), fields, " }")
    };
    if shape.nullable {
        vec![LuaDisplayPart::UnlessNil {
            value: format!("v[\"{prefix}\"]"),
            parts: body,
        }]
    } else {
        body
    }
}

/// The concatenated [`GcTableField::keys`] are the `<Name>_vt` ctor's
/// parameters and the box's keys.
/// Every reader and writer of the box takes them from here.
pub fn gc_table_fields(
    ws: &Workspace,
    target: FnBackend,
    name: Ustr,
) -> Result<Vec<GcTableField>, RepError> {
    let sid = ws
        .nominals
        .struct_id(name)
        .ok_or_else(|| plan_error(format!("GC-table value `{name}` is not a declared struct")))?;
    let sd = &ws.modules[sid.module.index()].structs[sid.def];
    let shape = super::shape::value_shape_gc(ws, name)
        .ok_or_else(|| plan_error(format!("GC-table value `{name}` has no GC shape")))?;
    if shape.fields.len() != sd.fields.len() {
        return Err(plan_error(format!(
            "GC-table value `{name}` shape does not match its fields"
        )));
    }
    sd.fields
        .iter()
        .zip(shape.fields)
        .map(|(f, (_, fs))| {
            let inline = if struct_field_plan(ws, target, name, f.name, false)?
                .embed()
                .is_some()
            {
                match fs {
                    super::FieldShape::Nested(sub) => Some(sub),
                    super::FieldShape::Prim => {
                        return Err(plan_error(format!(
                            "GC-table value `{name}` inlines field `{}` with no leaf shape",
                            f.name
                        )));
                    }
                }
            } else {
                None
            };
            Ok(GcTableField {
                name: f.name,
                eq: super::shape::owner_field_eq(ws, name, f.ty),
                inline,
            })
        })
        .collect()
}

/// A heap record's physical keys in slot order: declared fields, an inline
/// child's dotted leaves and presence tag, and a flat enum's slots. `None` for
/// a self-embedding record. `name` is a declaration or one of its instances.
pub fn record_keys(ws: &Workspace, name: Ustr) -> Option<Vec<Ustr>> {
    let mut keys = Vec::new();
    record_fields(ws, name, "", &mut keys, &mut Vec::new())?;
    Some(keys)
}

fn record_fields(
    ws: &Workspace,
    name: Ustr,
    prefix: &str,
    out: &mut Vec<Ustr>,
    visiting: &mut Vec<Ustr>,
) -> Option<()> {
    if visiting.contains(&name) {
        return None;
    }
    let id = ws.nominals.struct_id(name)?;
    let def = &ws.module(id.module).structs[id.def];
    visiting.push(name);
    let key = |name: Ustr| {
        if prefix.is_empty() {
            name
        } else {
            Ustr::from(&format!("{prefix}{name}"))
        }
    };
    for field in &def.fields {
        let plan =
            struct_field_plan(ws, FnBackend::Lua, name, field.name, !prefix.is_empty()).ok()?;
        if let Some((name, nullable)) = plan.embed() {
            if let Some(leaves) = nobox_leaf_paths(ws, name) {
                let field = key(field.name);
                out.extend(
                    leaves
                        .iter()
                        .map(|leaf| Ustr::from(&format!("{field}.{leaf}"))),
                );
                if nullable {
                    out.push(field);
                }
                continue;
            }
            record_fields(ws, name, &format!("{}.", key(field.name)), out, visiting)?;
            if nullable {
                out.push(key(field.name));
            }
        } else if let Some(fields) = plan.flat_storage_keys(field.name) {
            out.extend(fields.into_iter().map(key));
        } else {
            out.push(key(field.name));
        }
    }
    visiting.pop();
    Some(())
}
