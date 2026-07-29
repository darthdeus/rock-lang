//! The GPU binding view of a struct: what each field binds as, and where.
//!
//! Two groups so far, both keyed on field ORDER -- vertex attributes and their
//! locations ([`vertex_attrs`], P1), and `@textures` samplers and their units
//! ([`texture_slots`], P2).
//!
//! Part of the target-neutral interface artifact
//! (`docs/shader-binding-layout-plan.md`). [`AttrKind`] and [`SamplerKind`] are
//! scalar kind + arity, never a GLSL type name (`GlslTy::name()` prints
//! `vec4`), so a WGSL or Metal backend spells the same binding its own way.
//!
//! # This module does NOT own memory layout
//!
//! Everything here is derived from field TYPES and field ORDER. Byte offsets,
//! struct size and stride are deliberately absent: those are the value struct's
//! **C layout**, which `skull-mir`'s layout walker already computes and
//! cross-checks against LuaJIT (`ffi.offsetof` / `ffi.sizeof` assertions emitted
//! per FFI-clean value struct, aborting at chunk load on drift -- the
//! `value_leaf_layout` / `value_leaf_layout_checked` pair on the `inline-lua`
//! branch). A second offset model is exactly the bypass that walker is private
//! to prevent, and a wrong vertex offset corrupts geometry silently rather than
//! failing.
//!
//! So when the vertex-attribute consumers need offsets and stride, they take
//! them from the checked C layout and use this module only for the grouping:
//! the C layout flattens to primitive leaves (`pos.x`, `pos.y`), while an
//! attribute is the whole top-level field (`pos` = one `F32x2` at location 0).
//!
//! Worth stating because it is the trap: a vertex buffer is laid out by C
//! rules, NOT std140. A `Vec3` occupies 12 bytes with the next field
//! immediately after; std140's 16-byte rounding belongs to the uniform family
//! and applying it to a vertex buffer would corrupt every one of them.

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
    ///
    /// A count of components, not a size in bytes: this says nothing about
    /// where the attribute sits.
    pub fn components(self) -> u32 {
        match self {
            AttrKind::F32 | AttrKind::I32 => 1,
            AttrKind::F32x2 => 2,
            AttrKind::F32x3 => 3,
            AttrKind::F32x4 => 4,
        }
    }

    pub fn is_integer(self) -> bool {
        matches!(self, AttrKind::I32)
    }
}

/// One attribute: a top-level field of the vertex/instance struct.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attr {
    pub name: Ustr,
    pub kind: AttrKind,
    /// Attribute location. RECORDED here rather than materialized as an
    /// `enumerate()` index while printing GLSL (the emitter's shape today,
    /// `skull-codegen-glsl/src/lib.rs:2255`), so the engine can bind against
    /// the same numbering the shader declares.
    pub location: u32,
}

/// The attributes of one vertex or instance struct, in location order.
///
/// Named for what it is: NOT a layout. It carries no offsets and no stride --
/// see the module docs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VertexAttrs {
    pub struct_name: Ustr,
    pub attrs: Vec<Attr>,
}

impl VertexAttrs {
    /// Per-attribute component counts, in location order -- the `attr_sizes`
    /// argument of `gpu_mesh_new` / `gpu_mesh_set_instances`.
    pub fn attr_sizes(&self) -> Vec<u32> {
        self.attrs.iter().map(|a| a.kind.components()).collect()
    }
}

/// A sampler's scalar family. Distinct because reading an integer texture
/// through a float sampler is undefined, not merely lossy -- the r32i id
/// canvases depend on the difference.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SamplerKind {
    Float,
    Int,
}

/// One texture binding: a field of the `@textures` group struct.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TexSlot {
    pub name: Ustr,
    pub kind: SamplerKind,
    /// Texture unit. Compiler-assigned from field order, which is the point of
    /// the group: a hand-numbered unit can collide with another and silently
    /// bind one texture over the other.
    pub slot: u32,
}

/// The texture bindings of one `@textures` group, in slot order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextureSlots {
    pub struct_name: Ustr,
    pub slots: Vec<TexSlot>,
}

impl TextureSlots {
    /// Sampler names in slot order -- what a GL backend needs to point each
    /// sampler uniform at its unit once, at link time.
    pub fn names(&self) -> Vec<Ustr> {
        self.slots.iter().map(|s| s.name).collect()
    }
}

fn sampler_kind(ws: &Workspace, ty: TyId) -> Option<SamplerKind> {
    match ty_expr(ws, ty) {
        Some(TypeExpr::App { ctor: TypeCtor::Nominal(n), .. }) => match n.as_str() {
            "Sampler2D" => Some(SamplerKind::Float),
            "ISampler2D" => Some(SamplerKind::Int),
            _ => None,
        },
        _ => None,
    }
}

/// The texture slots of the `@textures` group struct named `struct_name`.
///
/// Field order is slot order, the same contract `@attr` uses for locations.
pub fn texture_slots(ws: &Workspace, struct_name: Ustr) -> Result<TextureSlots, CompilerError> {
    let Some(&sid) = ws.struct_by_name.get(&struct_name) else {
        return Err(CompilerError::new_no_location(format!(
            "texture slots: no struct named `{struct_name}`"
        )));
    };
    let sd = &ws.modules[sid.module.index()].structs[sid.def];

    if sd.is_value {
        return Err(err(
            sd.name_span,
            format!(
                "texture slots: `{struct_name}` must be a plain struct, not a `value` struct -- \
                 samplers are opaque handles, not uploadable bytes, so the group has no C layout"
            ),
        ));
    }
    if sd.fields.is_empty() {
        return Err(err(
            sd.name_span,
            format!("texture slots: `{struct_name}` has no fields -- an empty texture group binds nothing"),
        ));
    }

    let mut slots = Vec::with_capacity(sd.fields.len());
    for (i, fd) in sd.fields.iter().enumerate() {
        let Some(kind) = sampler_kind(ws, fd.ty) else {
            return Err(err(
                fd.span,
                format!(
                    "texture slots: `{struct_name}` field `{}` is not a sampler (want \
                     Sampler2D/ISampler2D) -- a non-sampler field consumes no unit and shifts \
                     every slot after it",
                    fd.name
                ),
            ));
        };
        slots.push(TexSlot { name: fd.name, kind, slot: i as u32 });
    }

    Ok(TextureSlots { struct_name, slots })
}

fn ty_expr(ws: &Workspace, ty: TyId) -> Option<TypeExpr> {
    match ty {
        TyId::Static(m, idx) => Some(ws.module(m).ty_arena[idx]),
        TyId::Local(_) => None,
    }
}

/// The attribute kind for a declared field type, or `None` if it cannot be a
/// vertex attribute.
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

/// The vertex attributes of the value struct named `struct_name`.
///
/// Field order is attribute order is location order -- the contract `@attr`
/// already carries.
pub fn vertex_attrs(ws: &Workspace, struct_name: Ustr) -> Result<VertexAttrs, CompilerError> {
    let Some(&sid) = ws.struct_by_name.get(&struct_name) else {
        return Err(CompilerError::new_no_location(format!(
            "vertex attributes: no struct named `{struct_name}`"
        )));
    };
    let sd = &ws.modules[sid.module.index()].structs[sid.def];

    if !sd.is_value {
        return Err(err(
            sd.name_span,
            format!(
                "vertex attributes: `{struct_name}` must be a `value` struct -- only value structs \
                 have a C layout to upload"
            ),
        ));
    }
    if sd.fields.is_empty() {
        return Err(err(sd.name_span, format!("vertex attributes: `{struct_name}` has no fields")));
    }

    let mut attrs = Vec::with_capacity(sd.fields.len());
    for (i, fd) in sd.fields.iter().enumerate() {
        let Some(kind) = attr_kind(ws, fd.ty) else {
            return Err(err(
                fd.span,
                format!(
                    "vertex attributes: `{struct_name}` field `{}` is not a vertex-attribute type \
                     (want float/int/Vec2/Vec3/Vec4)",
                    fd.name
                ),
            ));
        };
        attrs.push(Attr { name: fd.name, kind, location: i as u32 });
    }

    Ok(VertexAttrs { struct_name, attrs })
}
