//! GPU memory layout computed from a value struct's fields.
//!
//! The first slice of the target-neutral interface artifact
//! (`docs/shader-binding-layout-plan.md`). ONE computation feeds the cdata
//! declaration, the attribute-pointer setup, and the shader's attribute
//! locations -- so the three cannot disagree. Two implementations that agree
//! today is the silent-divergence trap; a disagreement here is garbage
//! geometry, not a compile error.
//!
//! Target-neutral on purpose: [`AttrKind`] is a scalar kind + arity, not a
//! GLSL type name, so a WGSL or Metal backend spells it its own way from the
//! same layout. Lives in `skull-mir` because all three codegen backends
//! already depend on it and none of them owns the interface.
//!
//! **Vertex layout is tightly packed** -- no std140 rounding. A `Vec3` field
//! occupies 12 bytes and the next field follows immediately. The 16-byte
//! alignment rules belong to the uniform family; applying them here would
//! silently corrupt every vertex buffer.

use bone_semantic::hir::{BuiltinTy, TypeCtor, TypeExpr};
use bone_semantic::inference::Workspace;
use bone_semantic::prelude::{Span, TyId};
use skull_parser::CompilerError;
use ustr::Ustr;

fn err(span: Span, msg: impl Into<String>) -> CompilerError {
    CompilerError::new_with_label(span, msg.into())
}

/// A vertex attribute's scalar kind and arity.
///
/// Int attributes are distinct because they need a different pointer setup
/// (`glVertexAttribIPointer`); uploading one down the float path is a silent
/// wrong-value class, not an error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttrKind {
    F32,
    F32x2,
    F32x3,
    F32x4,
    I32,
}

impl AttrKind {
    /// Component count -- the `size` argument of an attribute pointer, and the
    /// per-attribute entries of `gpu_mesh_set_instances`' `attr_sizes`.
    pub fn components(self) -> u32 {
        match self {
            AttrKind::F32 | AttrKind::I32 => 1,
            AttrKind::F32x2 => 2,
            AttrKind::F32x3 => 3,
            AttrKind::F32x4 => 4,
        }
    }

    /// Size in bytes. Every component is 4 bytes today (f32 / i32).
    pub fn size_bytes(self) -> u32 {
        self.components() * 4
    }

    pub fn is_integer(self) -> bool {
        matches!(self, AttrKind::I32)
    }
}

/// One attribute of a vertex or instance buffer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attr {
    pub name: Ustr,
    pub kind: AttrKind,
    /// Attribute location. RECORDED here rather than materialized as an
    /// `enumerate()` index while printing GLSL, so the engine can bind against
    /// the same numbering the shader declares.
    pub location: u32,
    /// Byte offset from the start of the vertex.
    pub offset_bytes: u32,
}

/// The layout of one vertex or instance buffer element.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VertexLayout {
    pub struct_name: Ustr,
    pub attrs: Vec<Attr>,
    pub stride_bytes: u32,
}

impl VertexLayout {
    /// Stride in floats -- the unit the engine's mesh draws speak today.
    pub fn stride_floats(&self) -> u32 {
        self.stride_bytes / 4
    }

    /// Per-attribute component counts, in location order.
    pub fn attr_sizes(&self) -> Vec<u32> {
        self.attrs.iter().map(|a| a.kind.components()).collect()
    }
}

fn ty_expr(ws: &Workspace, ty: TyId) -> Option<TypeExpr> {
    match ty {
        TyId::Static(m, idx) => Some(ws.module(m).ty_arena[idx]),
        TyId::Local(_) => None,
    }
}

/// The attribute kind for a declared field type, or `None` if it cannot live
/// in a vertex buffer.
fn attr_kind(ws: &Workspace, ty: TyId) -> Option<AttrKind> {
    match ty_expr(ws, ty) {
        Some(TypeExpr::Builtin(BuiltinTy::Float)) => Some(AttrKind::F32),
        Some(TypeExpr::Builtin(BuiltinTy::Int)) => Some(AttrKind::I32),
        Some(TypeExpr::App { ctor: TypeCtor::Nominal(n), .. }) => match n.as_str() {
            "Vec2" => Some(AttrKind::F32x2),
            "Vec3" => Some(AttrKind::F32x3),
            "Vec4" => Some(AttrKind::F32x4),
            _ => None,
        },
        _ => None,
    }
}

/// Compute the vertex layout of the value struct named `struct_name`.
///
/// Field order is attribute order is location order -- the same contract
/// `@attr` already carries. Fields are packed tightly, with no alignment
/// padding inserted between them.
pub fn vertex_layout(ws: &Workspace, struct_name: Ustr) -> Result<VertexLayout, CompilerError> {
    let Some(&sid) = ws.struct_by_name.get(&struct_name) else {
        return Err(CompilerError::new_no_location(format!(
            "vertex layout: no struct named `{struct_name}`"
        )));
    };
    let sd = &ws.modules[sid.module.index()].structs[sid.def];

    if !sd.is_value {
        return Err(err(
            sd.name_span,
            format!(
                "vertex layout: `{struct_name}` must be a `value` struct -- only value structs have a \
                 guaranteed C layout to upload"
            ),
        ));
    }
    if sd.fields.is_empty() {
        return Err(err(sd.name_span, format!("vertex layout: `{struct_name}` has no fields")));
    }

    let mut attrs = Vec::with_capacity(sd.fields.len());
    let mut offset = 0u32;
    for (i, fd) in sd.fields.iter().enumerate() {
        let Some(kind) = attr_kind(ws, fd.ty) else {
            return Err(err(
                fd.span,
                format!(
                    "vertex layout: `{struct_name}` field `{}` is not a vertex-attribute type \
                     (want float/int/Vec2/Vec3/Vec4)",
                    fd.name
                ),
            ));
        };
        attrs.push(Attr { name: fd.name, kind, location: i as u32, offset_bytes: offset });
        offset += kind.size_bytes();
    }

    Ok(VertexLayout { struct_name, attrs, stride_bytes: offset })
}
