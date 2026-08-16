//! The shared physical answer for one logical struct field.

use bone_semantic::hir::{FnBackend, TypeCtor, TypeExpr};
use bone_semantic::inference::Workspace;
use bone_semantic::prelude::TyId;
use ustr::Ustr;

use super::shape::{declared_value_storage, static_ty};
use super::{EnumFlatShape, EnumPlan, RepError, ValueStorage};

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
    let known_nominal = child_name.as_str() == "Coroutine"
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

    let parent_is_value = sd.is_value;
    let parent_storage = if parent_is_value && !spliced {
        Some(declared_value_storage(ws, target, sname).ok_or_else(|| {
            plan_error(format!("struct `{sname}` has no resolved value storage plan"))
        })?)
    } else {
        None
    };
    if (fdef.is_embed || child.is_some_and(|s| s.is_nobox))
        && (spliced || !parent_is_value || parent_storage == Some(ValueStorage::LeafTable))
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
