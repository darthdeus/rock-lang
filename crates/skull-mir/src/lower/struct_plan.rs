//! The shared physical answer for one logical struct field.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use bone_semantic::hir::{FnBackend, TypeCtor, TypeExpr};
use bone_semantic::inference::Workspace;
use bone_semantic::prelude::TyId;
use ustr::Ustr;

use super::shape::{declared_value_storage, static_ty};
use super::{EnumFlatShape, EnumPlan, RepArena, RepError, RepId, ValueShape, ValueStorage};
use crate::mir::{Inst, MirFunc};

/// One physical answer for one logical field. Inline/nobox fields and flat
/// enum payloads share one inline layout; consumers only project the part
/// they need from the selected plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StructFieldPlan {
    Direct { child: Option<Ustr>, nullable: bool },
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
            Self::Inline { child, nullable, flat: None } => Some((*child, *nullable)),
            _ => None,
        }
    }

    pub fn inline_child(&self) -> Option<Ustr> {
        self.embed().map(|(child, _)| child)
    }

    pub fn flat_shape(&self) -> Option<EnumFlatShape> {
        match self {
            Self::Inline { flat: Some(shape), .. } => Some(shape.clone()),
            _ => None,
        }
    }

    pub fn nullable(&self) -> bool {
        match self {
            Self::Direct { nullable, .. } | Self::Inline { nullable, .. } => *nullable,
        }
    }
}

/// The build-owned struct registry input shared by late representation
/// consumers. Its roots are the structs this MIR can realize; closing over
/// declared child fields also owns any value representation needed to observe
/// a child that enters through a host boundary rather than a Rock constructor.
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

        let mut value_plans: BTreeMap<Ustr, RepId> = arena.value_plan_catalog().into_iter().collect();
        let mut queue: Vec<Ustr> = names.iter().copied().collect();
        while let Some(name) = queue.pop() {
            let sid = ws
                .struct_by_name
                .get(&name)
                .ok_or_else(|| plan_error(format!("struct registry type `{name}` is not declared")))?;
            let s = &ws.modules[sid.module.index()].structs[sid.def];
            if crate::lower::has_value_plan(s) && !value_plans.contains_key(&name) {
                value_plans.insert(name, arena.plan_nominal_value(ws, target, name)?);
            }
            for field in &s.fields {
                let plan = struct_field_plan(ws, target, name, field.name, false)?;
                if let Some(child) = plan.child().filter(|child| ws.struct_by_name.contains_key(child)) {
                    if names.insert(child) {
                        queue.push(child);
                    }
                }
            }
        }
        value_plans.retain(|name, _| names.contains(name));
        Ok(Some(Self { arena, names, value_plans, column_plans }))
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
        if ws.struct_by_name.contains_key(&name) {
            names.insert(name);
        }
    }
    for nested in &body.nested_fns {
        collect_struct_roots(ws, nested, arena, names)?;
    }
    Ok(())
}

fn plan_error(message: impl Into<String>) -> RepError {
    RepError { span: None, message: message.into() }
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

/// Plan the physical placement of one declared field. All consumers that
/// inspect or lower a struct field use this answer; a missing declaration or
/// unresolved field type is never interpreted as direct storage.
pub fn struct_field_plan(
    ws: &Workspace,
    target: FnBackend,
    sname: Ustr,
    field: Ustr,
    spliced: bool,
) -> Result<StructFieldPlan, RepError> {
    let Some(&sid) = ws.struct_by_name.get(&sname) else {
        return Err(plan_error(format!("struct `{sname}` has no field plan")));
    };
    let sd = &ws.modules[sid.module.index()].structs[sid.def];
    let Some(fdef) = sd.fields.iter().find(|f| f.name == field) else {
        return Err(plan_error(format!("struct `{sname}` has no field `{field}`")));
    };
    let (resolved, nullable) = resolved_field_type(ws, fdef.ty)?;
    let TypeExpr::App { ctor: TypeCtor::Nominal(child_name), .. } = resolved else {
        return Ok(StructFieldPlan::Direct { child: None, nullable });
    };
    let child = ws
        .struct_by_name
        .get(&child_name)
        .map(|sid| &ws.modules[sid.module.index()].structs[sid.def]);
    let known_nominal = bone_semantic::prelude::is_intrinsic_key(child_name, "Coroutine")
        || child.is_some()
        || ws.enums_by_name.contains_key(&child_name)
        || ws.newtypes_by_name.contains_key(&child_name);
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
            plan_error(format!("struct `{sname}` has no resolved value storage plan"))
        })?)
    } else {
        None
    };
    // Only a cdata parent nests a child whole; every table parent keys an
    // inline child's leaves by dotted path.
    if (fdef.is_embed || child.is_some_and(|s| s.is_nobox))
        && (spliced || !parent_is_value || parent_storage != Some(ValueStorage::CData))
    {
        return Ok(StructFieldPlan::Inline { child: child_name, nullable, flat: None });
    }
    if !parent_is_value {
        if let Some(shape) = EnumPlan::for_name(ws, child_name).and_then(|plan| plan.flat().cloned()) {
            return Ok(StructFieldPlan::Inline {
                child: child_name,
                nullable: false,
                flat: Some(shape),
            });
        }
    }
    Ok(StructFieldPlan::Direct { child: Some(child_name), nullable })
}

/// One declared field of a GC-table (`<Name>_vt`) value box: a single key, or
/// an inline (`nobox`/embed) child whose leaves the box keys by dotted path
/// under the field's name, presence tag at the name itself -- the layout a
/// plain struct and a leaf-table value already give such a child.
#[derive(Clone, Debug)]
pub struct GcTableField {
    pub name: Ustr,
    pub ty: TyId,
    pub(crate) inline: Option<ValueShape>,
}

impl GcTableField {
    /// This field's box keys in ctor order, each flagged when it is a nullable
    /// child's presence tag rather than a data leaf.
    pub fn keys(&self) -> Vec<(Ustr, bool)> {
        let Some(sub) = &self.inline else {
            return vec![(self.name, false)];
        };
        let mut presence = Vec::new();
        super::shape::presence_flags_rec(sub, true, &mut presence);
        super::shape::leaf_names(sub, self.name.as_str(), ".")
            .into_iter()
            .map(|key| Ustr::from(&key))
            .zip(presence)
            .collect()
    }

    pub fn is_inline(&self) -> bool {
        self.inline.is_some()
    }

    /// Per key of [`Self::keys`]: whether `__eq` compares it with `==` rather
    /// than `rawequal`. A GC reference compares by identity, as the declared
    /// field of the same type does.
    pub fn keys_eq_structural(&self, ws: &Workspace) -> Vec<bool> {
        let Some(sub) = &self.inline else {
            return vec![super::shape::gc_eq_is_structural(ws, self.ty)];
        };
        let mut out = Vec::new();
        leaf_eq_structural(ws, sub, true, &mut out);
        out
    }

    /// The Lua expression rendering this field's value off box `v` for the
    /// box's `__tostring`: an inline child renders as its own box would.
    pub fn lua_tostring(&self) -> String {
        match &self.inline {
            None => format!("tostring(v.{})", self.name),
            Some(sub) => lua_tostring_inline(sub, self.name.as_str()),
        }
    }
}

fn leaf_eq_structural(ws: &Workspace, shape: &ValueShape, nested: bool, out: &mut Vec<bool>) {
    if shape.nullable && nested {
        out.push(true);
    }
    let decl = ws
        .struct_by_name
        .get(&shape.name)
        .map(|sid| &ws.modules[sid.module.index()].structs[sid.def]);
    for (fname, fs) in &shape.fields {
        match fs {
            super::FieldShape::Prim => out.push(
                decl.and_then(|s| s.fields.iter().find(|f| f.name == *fname))
                    .is_none_or(|f| super::shape::gc_eq_is_structural(ws, f.ty)),
            ),
            super::FieldShape::Nested(sub) => leaf_eq_structural(ws, sub, true, out),
        }
    }
}

fn lua_tostring_inline(shape: &ValueShape, prefix: &str) -> String {
    let parts: Vec<String> = shape
        .fields
        .iter()
        .map(|(fname, fs)| {
            let key = format!("{prefix}.{fname}");
            let value = match fs {
                super::FieldShape::Prim => format!("tostring(v[\"{key}\"])"),
                super::FieldShape::Nested(sub) => lua_tostring_inline(sub, &key),
            };
            format!("\"{fname}: \" .. {value}")
        })
        .collect();
    let body = if parts.is_empty() {
        format!("\"{} {{ }}\"", shape.name)
    } else {
        format!("\"{} {{ \" .. {} .. \" }}\"", shape.name, parts.join(" .. \", \" .. "))
    };
    if shape.nullable {
        format!("(v[\"{prefix}\"] == nil and \"nil\" or ({body}))")
    } else {
        format!("({body})")
    }
}

/// The declared fields of GC-table value `name` with their physical layout, in
/// declaration order. The concatenated [`GcTableField::keys`] are the
/// `<Name>_vt` ctor's parameters and the box's keys; every reader and writer of
/// the box takes them from here.
pub fn gc_table_fields(
    ws: &Workspace,
    target: FnBackend,
    name: Ustr,
) -> Result<Vec<GcTableField>, RepError> {
    let sid = ws
        .struct_by_name
        .get(&name)
        .ok_or_else(|| plan_error(format!("GC-table value `{name}` is not a declared struct")))?;
    let sd = &ws.modules[sid.module.index()].structs[sid.def];
    let shape = super::shape::value_shape_gc(ws, name)
        .ok_or_else(|| plan_error(format!("GC-table value `{name}` has no GC shape")))?;
    if shape.fields.len() != sd.fields.len() {
        return Err(plan_error(format!("GC-table value `{name}` shape does not match its fields")));
    }
    sd.fields
        .iter()
        .zip(shape.fields)
        .map(|(f, (_, fs))| {
            let inline = if struct_field_plan(ws, target, name, f.name, false)?.embed().is_some() {
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
            Ok(GcTableField { name: f.name, ty: f.ty, inline })
        })
        .collect()
}
