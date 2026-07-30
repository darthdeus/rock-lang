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
//! # Two storage contexts, each with its own layout authority
//!
//! A buffer's wire format is a property of the BUFFER, not of the Rock struct
//! that feeds it -- the same value struct can feed either, and its own C layout
//! is never changed by being bound.
//!
//! **Vertex buffers are laid out by C rules**, so their wire format IS the
//! struct's checked C layout. This module contributes only the grouping the C
//! layout cannot express: the C layout flattens to primitive leaves (`pos.x`,
//! `pos.y`), while an attribute is the whole top-level field (`pos` = one
//! `F32x2` at location 0). Offsets and stride come from
//! `value_leaf_layout_checked`, whose numbers are not taken on trust --
//! `ffi.offsetof`/`ffi.sizeof` assertions are emitted per FFI-clean value
//! struct and abort at chunk load on drift. Computing a second set of vertex
//! offsets here is exactly the bypass that walker is private to prevent, and a
//! wrong vertex offset corrupts geometry silently rather than failing.
//!
//! **Uniform blocks are laid out by std140**, which is a foreign wire format no
//! C layout expresses: `Vec2` aligns to 8 rather than 4, `Vec3` and `Vec4` to
//! 16, the block rounds to 16, and a block `bool` is 32 bits where Rock's C
//! bool is one byte. So [`uniform_block`] DOES own byte offsets -- it is the
//! sole authority for that context, and the packer and the shader declaration
//! both read it rather than agreeing separately. The value struct keeps its
//! ordinary C representation everywhere it lives; std140 exists only in the
//! emitted block declaration and in the scratch bytes handed to the driver.
//!
//! Applying either family's rules to the other corrupts every buffer it
//! touches, which is why they are separate functions and not one parameterized
//! walk.

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

/// A block member's scalar encoding ON THE WIRE, which is not always its Rock
/// type: GL reads a uniform-block `bool` as a 32-bit value while Rock's C layout
/// models one as a single byte, so the packer has a conversion to perform and
/// [`WireKind::Bool32`] is where it is stated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireKind {
    F32,
    F32x2,
    F32x3,
    F32x4,
    I32,
    Bool32,
    Mat4,
}

impl WireKind {
    /// std140 base alignment (OpenGL 4.5 core, 7.6.2.2).
    fn align(self) -> u32 {
        match self {
            WireKind::F32 | WireKind::I32 | WireKind::Bool32 => 4,
            WireKind::F32x2 => 8,
            WireKind::F32x3 | WireKind::F32x4 | WireKind::Mat4 => 16,
        }
    }

    /// Bytes the member occupies. Note `F32x3` is TWELVE with alignment 16: a
    /// following scalar takes the fourth lane rather than starting a new one.
    fn size(self) -> u32 {
        match self {
            WireKind::F32 | WireKind::I32 | WireKind::Bool32 => 4,
            WireKind::F32x2 => 8,
            WireKind::F32x3 => 12,
            WireKind::F32x4 => 16,
            WireKind::Mat4 => 64,
        }
    }
}

/// One member of a uniform block, at its std140 offset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockMember {
    pub name: Ustr,
    pub kind: WireKind,
    pub offset: u32,
    /// Bytes between a matrix's columns; `None` for non-matrix members. A
    /// column-major matrix is an array of column vectors, so this is what the
    /// driver reports as `GL_UNIFORM_MATRIX_STRIDE` and what the link-time
    /// assertion compares against.
    pub matrix_stride: Option<u32>,
}

/// The std140 layout of one `@uniform` binding group.
///
/// The single authority for that block's byte layout: the GLSL block
/// declaration, the CPU packer and the link-time driver assertion all read
/// this, rather than each computing offsets and being expected to agree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UniformBlockPlan {
    pub struct_name: Ustr,
    pub members: Vec<BlockMember>,
    /// The block's total size, rounded up to 16.
    pub size: u32,
    /// Always at least 16 -- a std140 block's alignment is its largest member's
    /// alignment rounded up to a vec4.
    pub align: u32,
}

fn round_up(v: u32, align: u32) -> u32 {
    v.div_ceil(align) * align
}

/// The wire kind for a declared field type, or `None` if it has no std140 form.
fn wire_kind(ws: &Workspace, ty: TyId) -> Option<WireKind> {
    match ty_expr(ws, ty) {
        Some(TypeExpr::Builtin(BuiltinTy::Float)) => Some(WireKind::F32),
        Some(TypeExpr::Builtin(BuiltinTy::Int)) => Some(WireKind::I32),
        Some(TypeExpr::Builtin(BuiltinTy::Bool)) => Some(WireKind::Bool32),
        Some(TypeExpr::App { ctor: TypeCtor::Nominal(n), .. }) => match n.as_str() {
            "Vec2" => Some(WireKind::F32x2),
            "Vec3" => Some(WireKind::F32x3),
            "Vec4" => Some(WireKind::F32x4),
            "Mat4" => Some(WireKind::Mat4),
            _ => None,
        },
        _ => None,
    }
}

/// The std140 layout of the `@uniform` group struct named `struct_name`.
///
/// Field order is member order, the same contract `@attr` uses for locations
/// and `@textures` for slots.
pub fn uniform_block(ws: &Workspace, struct_name: Ustr) -> Result<UniformBlockPlan, CompilerError> {
    let Some(&sid) = ws.struct_by_name.get(&struct_name) else {
        return Err(CompilerError::new_no_location(format!(
            "uniform block: no struct named `{struct_name}`"
        )));
    };
    let sd = &ws.modules[sid.module.index()].structs[sid.def];

    if !sd.is_value {
        return Err(err(
            sd.name_span,
            format!(
                "uniform block: `{struct_name}` must be a `value` struct -- a uniform group is \
                 uploaded as bytes, and a GC-tier struct has no fixed layout to pack"
            ),
        ));
    }
    if sd.fields.is_empty() {
        return Err(err(
            sd.name_span,
            format!("uniform block: `{struct_name}` has no fields -- an empty block binds nothing"),
        ));
    }

    let mut members = Vec::with_capacity(sd.fields.len());
    let mut offset = 0u32;
    let mut align = 16u32;

    for fd in &sd.fields {
        let Some(kind) = wire_kind(ws, fd.ty) else {
            let ty_name = ty_expr(ws, fd.ty)
                .and_then(|t| match t {
                    TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } => Some(n.to_string()),
                    _ => None,
                })
                .unwrap_or_else(|| "that type".to_string());
            return Err(err(
                fd.span,
                format!(
                    "uniform block: `{struct_name}` field `{}` has type `{ty_name}`, which has no \
                     std140 form here (want float/int/bool/Vec2/Vec3/Vec4/Mat4). Nested structs \
                     and arrays are not packed yet -- see docs/shader-binding-layout-plan.md",
                    fd.name
                ),
            ));
        };
        offset = round_up(offset, kind.align());
        align = align.max(kind.align());
        members.push(BlockMember {
            name: fd.name,
            kind,
            offset,
            matrix_stride: matches!(kind, WireKind::Mat4).then_some(16),
        });
        offset += kind.size();
    }

    Ok(UniformBlockPlan { struct_name, members, size: round_up(offset, 16), align })
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
