//! Vertex buffers use the struct's C layout. Uniform blocks use std140: `Vec2`
//! aligns to 8, `Vec3` and `Vec4` to 16, a block rounds to 16, and a block
//! `bool` is 32 bits where Rock's C bool is one byte.
//! Vertex offsets and stride come only from the checked C layout; never compute
//! a second set here. Applying either family's rules to the other corrupts buffers.

use bone_core::hir::{BuiltinTy, TypeCtor, TypeExpr};

use crate::{fnv1a, fold53};
use bone_core::prelude::{Span, TyId};
use bone_core::workspace::Workspace;
use skull_parser::CompilerError;
use ustr::Ustr;

fn err(span: Span, msg: impl Into<String>) -> CompilerError {
    CompilerError::new_with_label(span, msg.into())
}

/// Int attributes need `glVertexAttribIPointer`; uploading one down the float
/// path is a silent wrong value, not an error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttrKind {
    F32,
    F32x2,
    F32x3,
    F32x4,
    I32,
}

impl AttrKind {
    /// A count of components (an attribute pointer's `size`), not bytes.
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attr {
    pub name: Ustr,
    pub kind: AttrKind,
    /// Recorded so the engine binds against the same numbering the shader declares.
    pub location: u32,
}

/// Not a layout: it carries no offsets and no stride.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VertexAttrs {
    pub struct_name: Ustr,
    pub attrs: Vec<Attr>,
}

impl VertexAttrs {
    /// The `attr_sizes` argument of `gpu_mesh_new` / `gpu_mesh_set_instances`.
    pub fn attr_sizes(&self) -> Vec<u32> {
        self.attrs.iter().map(|a| a.kind.components()).collect()
    }
}

/// Reading an integer texture through a float sampler is undefined, not merely lossy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SamplerKind {
    Float,
    Int,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TexSlot {
    pub name: Ustr,
    pub kind: SamplerKind,
    /// Compiler-assigned from field order: a hand-numbered unit can collide and
    /// silently bind one texture over another.
    pub slot: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextureSlots {
    pub struct_name: Ustr,
    pub slots: Vec<TexSlot>,
}

fn sampler_kind(ws: &Workspace, ty: TyId) -> Option<SamplerKind> {
    match ty_expr(ws, ty) {
        Some(TypeExpr::App {
            ctor: TypeCtor::Nominal(n),
            ..
        }) => {
            if bone_core::modules::is_intrinsic_key(n, "Sampler2D") {
                Some(SamplerKind::Float)
            } else if bone_core::modules::is_intrinsic_key(n, "ISampler2D") {
                Some(SamplerKind::Int)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Field order is slot order, the same contract `@attr` uses for locations.
pub fn texture_slots(ws: &Workspace, struct_name: Ustr) -> Result<TextureSlots, CompilerError> {
    let Some(sid) = ws.nominals.struct_id(struct_name) else {
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
            format!(
                "texture slots: `{struct_name}` has no fields -- an empty texture group binds nothing"
            ),
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
        slots.push(TexSlot {
            name: fd.name,
            kind,
            slot: i as u32,
        });
    }

    Ok(TextureSlots { struct_name, slots })
}

/// GL reads a uniform-block `bool` as 32 bits; Rock's C layout uses one byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireKind {
    F32,
    F32x2,
    F32x3,
    F32x4,
    I32,
    U32,
    Bool32,
    Mat4,
}

impl WireKind {
    /// Written out, not derived from the discriminant: the host reads these codes,
    /// and `wire_kind_codes.rs` pins them against the host's table.
    pub fn code(self) -> u32 {
        match self {
            WireKind::F32 => 0,
            WireKind::F32x2 => 1,
            WireKind::F32x3 => 2,
            WireKind::F32x4 => 3,
            WireKind::I32 => 4,
            WireKind::U32 => 7,
            WireKind::Bool32 => 5,
            WireKind::Mat4 => 6,
        }
    }

    /// std140 base alignment (OpenGL 4.5 core, 7.6.2.2).
    fn align(self) -> u32 {
        match self {
            WireKind::F32 | WireKind::I32 | WireKind::U32 | WireKind::Bool32 => 4,
            WireKind::F32x2 => 8,
            WireKind::F32x3 | WireKind::F32x4 | WireKind::Mat4 => 16,
        }
    }

    /// Bytes the member occupies. Note `F32x3` is TWELVE with alignment 16: a
    /// following scalar takes the fourth lane rather than starting a new one.
    fn size(self) -> u32 {
        match self {
            WireKind::F32 | WireKind::I32 | WireKind::U32 | WireKind::Bool32 => 4,
            WireKind::F32x2 => 8,
            WireKind::F32x3 => 12,
            WireKind::F32x4 => 16,
            WireKind::Mat4 => 64,
        }
    }

    /// A claim about stdlib's `Vec2`/`Mat4` field order, bound to std140 slots
    /// positionally; `uniform_block_checked` checks it against the value walker.
    pub(crate) fn leaf_suffixes(self) -> Vec<String> {
        match self {
            WireKind::F32 | WireKind::I32 | WireKind::U32 | WireKind::Bool32 => vec![String::new()],
            WireKind::F32x2 => ["x", "y"].iter().map(|k| format!(".{k}")).collect(),
            WireKind::F32x3 => ["x", "y", "z"].iter().map(|k| format!(".{k}")).collect(),
            WireKind::F32x4 => ["x", "y", "z", "w"]
                .iter()
                .map(|k| format!(".{k}"))
                .collect(),
            WireKind::Mat4 => (0..4)
                .flat_map(|c| ["x", "y", "z", "w"].map(move |k| format!(".c{c}.{k}")))
                .collect(),
        }
    }

    /// A block `bool` is a 32-bit value, so it rides the int view.
    fn view(self) -> WireView {
        match self {
            WireKind::I32 | WireKind::Bool32 => WireView::Int,
            WireKind::U32 => WireView::Uint,
            _ => WireView::Float,
        }
    }
}

/// Every std140 kind a block can hold is a run of 4-byte components, so one
/// `float*` and one `int32_t*` view reach every member.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireView {
    Float,
    Int,
    Uint,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LeafStore {
    pub offset: u32,
    pub view: WireView,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockMember {
    pub name: Ustr,
    pub kind: WireKind,
    pub offset: u32,
    /// Bytes between a matrix's columns (`GL_UNIFORM_MATRIX_STRIDE`); `None` for
    /// non-matrix members.
    pub matrix_stride: Option<u32>,
}

/// The single authority for the block's byte layout: the GLSL declaration, the
/// CPU packer and the link-time driver assertion all read it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UniformBlockPlan {
    pub struct_name: Ustr,
    pub members: Vec<BlockMember>,
    /// The block's total size in bytes, rounded up to 16.
    pub size: u32,
}

impl BlockMember {
    /// A flat 4-byte run, in value-side leaf order: `Mat4`'s std140 column stride
    /// of 16 is four packed floats, and stdlib stores it as four column-major `Vec4`s.
    pub fn leaf_stores(&self) -> Vec<LeafStore> {
        let view = self.kind.view();
        (0..self.kind.size() / 4)
            .map(|i| LeafStore {
                offset: self.offset + i * 4,
                view,
            })
            .collect()
    }
}

impl UniformBlockPlan {
    /// Kinds and names are hashed: `{float, Vec2}` and `{Vec2, float}` agree on
    /// size and offsets, and the driver resolves members by name.
    pub fn fingerprint(&self) -> u64 {
        let mut h = fnv1a(
            crate::FNV_OFFSET,
            bone_core::modules::artifact_name(self.struct_name.as_str()).as_bytes(),
        );
        for m in &self.members {
            h = fnv1a(h, m.name.as_bytes());
            h = fnv1a(h, &[m.kind as u8]);
            h = fnv1a(h, &m.offset.to_le_bytes());
        }
        fold53(fnv1a(h, &self.size.to_le_bytes()))
    }

    /// In the order [`Self::leaf_stores`] writes them.
    pub fn leaf_paths(&self) -> Vec<String> {
        self.members
            .iter()
            .flat_map(|m| {
                m.kind
                    .leaf_suffixes()
                    .into_iter()
                    .map(move |s| format!("{}{s}", m.name))
            })
            .collect()
    }

    /// One store per primitive leaf; its length must match the value's flattened leaves.
    pub fn leaf_stores(&self) -> Vec<LeafStore> {
        self.members.iter().flat_map(|m| m.leaf_stores()).collect()
    }
}

fn round_up(v: u32, align: u32) -> u32 {
    v.div_ceil(align) * align
}

/// A `UniformArray<T>` element is classified here too, so there is one layout authority.
pub fn wire_kind(ws: &Workspace, ty: TyId) -> Option<WireKind> {
    match ty_expr(ws, ty) {
        Some(TypeExpr::Builtin(BuiltinTy::Float)) => Some(WireKind::F32),
        Some(TypeExpr::Builtin(BuiltinTy::Int)) => Some(WireKind::I32),
        Some(TypeExpr::Builtin(BuiltinTy::Uint)) => Some(WireKind::U32),
        Some(TypeExpr::Builtin(BuiltinTy::Bool)) => Some(WireKind::Bool32),
        Some(TypeExpr::App {
            ctor: TypeCtor::Nominal(n),
            ..
        }) => {
            if bone_core::modules::is_intrinsic_key(n, "Vec2")
                || bone_core::modules::is_intrinsic_key(n, "Vec2f")
            {
                Some(WireKind::F32x2)
            } else if bone_core::modules::is_intrinsic_key(n, "Vec3")
                || bone_core::modules::is_intrinsic_key(n, "Vec3f")
            {
                Some(WireKind::F32x3)
            } else if bone_core::modules::is_intrinsic_key(n, "Vec4")
                || bone_core::modules::is_intrinsic_key(n, "Vec4f")
            {
                Some(WireKind::F32x4)
            } else if bone_core::modules::is_intrinsic_key(n, "Mat4") {
                Some(WireKind::Mat4)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Field order is member order.
pub fn uniform_block(ws: &Workspace, struct_name: Ustr) -> Result<UniformBlockPlan, CompilerError> {
    let span = ws
        .nominals
        .struct_id(struct_name)
        .map(|sid| ws.modules[sid.module.index()].structs[sid.def].name_span);
    std140_block(struct_name, &uniform_group_members(ws, struct_name)?, span)
}

pub fn uniform_group_members(
    ws: &Workspace,
    struct_name: Ustr,
) -> Result<Vec<(Ustr, WireKind)>, CompilerError> {
    let Some(sid) = ws.nominals.struct_id(struct_name) else {
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
    for fd in &sd.fields {
        let Some(kind) = wire_kind(ws, fd.ty) else {
            let ty_name = ty_expr(ws, fd.ty)
                .and_then(|t| match t {
                    TypeExpr::App {
                        ctor: TypeCtor::Nominal(n),
                        ..
                    } => Some(n.to_string()),
                    _ => None,
                })
                .unwrap_or_else(|| "that type".to_string());
            if bone_core::modules::is_intrinsic_key_in(
                Ustr::from(ty_name.as_str()),
                &["Sampler2D", "ISampler2D"],
            ) {
                return Err(err(
                    fd.span,
                    format!(
                        "uniform block: `{struct_name}` field `{}` is a sampler, which cannot \
                         live in a uniform buffer on any API -- pass it as an fn param instead \
                         (`@textures` groups it and assigns the unit)",
                        fd.name
                    ),
                ));
            }
            return Err(err(
                fd.span,
                format!(
                    "uniform block: `{struct_name}` field `{}` has type `{ty_name}`, which has no \
                     std140 form here (want float/int/bool/Vec2/Vec3/Vec4/Mat4). Nested structs \
                     and arrays are not packed yet",
                    fd.name
                ),
            ));
        };
        members.push((fd.name, kind));
    }
    Ok(members)
}

/// A pure function of `members`; it never reads the Workspace.
pub fn std140_block(
    struct_name: Ustr,
    members: &[(Ustr, WireKind)],
    span: Option<Span>,
) -> Result<UniformBlockPlan, CompilerError> {
    if members.is_empty() {
        let msg = format!(
            "uniform block: `{struct_name}` has no members -- an empty block binds nothing"
        );
        return Err(match span {
            Some(sp) => err(sp, msg),
            None => CompilerError::new_no_location(msg),
        });
    }
    let mut out = Vec::with_capacity(members.len());
    let mut offset = 0u32;
    for &(name, kind) in members {
        offset = round_up(offset, kind.align());
        out.push(BlockMember {
            name,
            kind,
            offset,
            matrix_stride: matches!(kind, WireKind::Mat4).then_some(16),
        });
        offset += kind.size();
    }

    let size = round_up(offset, 16);
    if size > GL_MAX_UNIFORM_BLOCK_SIZE {
        let msg = format!(
            "uniform block: `{struct_name}` needs {size} std140 bytes, over the {GL_MAX_UNIFORM_BLOCK_SIZE} \
             portable OpenGL uniform-block budget"
        );
        return Err(match span {
            Some(sp) => err(sp, msg),
            None => CompilerError::new_no_location(msg),
        });
    }

    Ok(UniformBlockPlan {
        struct_name,
        members: out,
        size,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArrayMemberDecl {
    pub name: Ustr,
    pub elem: WireKind,
    pub capacity: u32,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArrayMember {
    pub name: Ustr,
    pub elem: WireKind,
    pub capacity: u32,
    pub offset: u32,
    /// std140 array stride: the element's base alignment rounded up to 16.
    pub stride: u32,
}

/// Not part of [`UniformBlockPlan`], whose leaf paths must match a value struct.
/// A separate buffer: scalars change per draw, arrays per activation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UniformArrayBlockPlan {
    pub block_name: Ustr,
    pub members: Vec<ArrayMember>,
    pub size: u32,
}

impl UniformArrayBlockPlan {
    /// Seeded apart from the scalar fingerprint so the two blocks never hash alike.
    pub fn fingerprint(&self) -> u64 {
        let mut h = fnv1a(
            0xa5a5_5a5a_c3c3_3c3c,
            bone_core::modules::artifact_name(self.block_name.as_str()).as_bytes(),
        );
        for m in &self.members {
            h = fnv1a(h, m.name.as_bytes());
            h = fnv1a(h, &[m.elem as u8]);
            h = fnv1a(h, &m.offset.to_le_bytes());
            h = fnv1a(h, &m.capacity.to_le_bytes());
            h = fnv1a(h, &m.stride.to_le_bytes());
        }
        fold53(fnv1a(h, &self.size.to_le_bytes()))
    }
}

fn round_up_u64(v: u64, align: u64) -> u64 {
    v.div_ceil(align) * align
}

fn oversized(block_name: Ustr, member: Ustr, span: Span, size: u64) -> CompilerError {
    CompilerError::new_with_label(
        span,
        format!(
            "uniform array block: `{block_name}` needs {size} std140 bytes at `{member}`, over the \
         {GL_MAX_UNIFORM_BLOCK_SIZE} portable OpenGL uniform-block budget. Array stride is 16 \
         for every element kind, so a `@size(N)` array costs `N * 16` whatever it holds -- the \
         budget is per BLOCK, and this one is the arrays alone"
        ),
    )
}

/// `Vec3` is refused: std140 aligns it to 16 but sizes it 12, so an array of
/// them is neither tightly packed nor stride-12.
/// Admits exactly the element kinds the host array lane can push; the GLSL
/// emitter declares whatever this admits, so this is the one gate.
pub fn uniform_array_block(
    block_name: Ustr,
    decls: &[ArrayMemberDecl],
) -> Result<UniformArrayBlockPlan, CompilerError> {
    if decls.is_empty() {
        return Err(CompilerError::new_no_location(format!(
            "uniform array block: `{block_name}` has no members -- an empty block binds nothing"
        )));
    }

    let mut members = Vec::with_capacity(decls.len());
    let mut offset = 0u64;

    for d in decls {
        let stride = match d.elem {
            WireKind::F32 | WireKind::I32 | WireKind::F32x2 | WireKind::F32x4 => {
                // Computed, not a literal, so a new kind cannot inherit the stride silently.
                round_up(d.elem.align(), 16)
            }
            WireKind::F32x3 | WireKind::Mat4 | WireKind::Bool32 | WireKind::U32 => {
                let name = match d.elem {
                    WireKind::F32x3 => "Vec3",
                    WireKind::Mat4 => "Mat4",
                    WireKind::U32 => "uint",
                    _ => "bool",
                };
                return Err(CompilerError::new_with_label(
                    d.span,
                    format!(
                        "uniform array block: `{block_name}` member `{}` has element type `{name}`, \
                     which a uniform array may not hold (allowed: float, int, Vec2, Vec4)",
                        d.name
                    ),
                ));
            }
        };
        if d.capacity == 0 {
            return Err(CompilerError::new_with_label(
                d.span,
                format!(
                    "uniform array block: `{block_name}` member `{}` has capacity 0 -- `@size(N)` \
                 declares how many elements the shader may index, so 0 declares an array nothing \
                 can read",
                    d.name
                ),
            ));
        }

        // u64, with the budget checked before narrowing: `capacity` comes from source,
        // and a u32 product could wrap to a small size that passes the budget.
        offset = round_up_u64(offset, stride as u64);
        let member_offset =
            u32::try_from(offset).map_err(|_| oversized(block_name, d.name, d.span, offset))?;
        members.push(ArrayMember {
            name: d.name,
            elem: d.elem,
            capacity: d.capacity,
            offset: member_offset,
            stride,
        });
        offset += d.capacity as u64 * stride as u64;
        if offset > GL_MAX_UNIFORM_BLOCK_SIZE as u64 {
            return Err(oversized(
                block_name,
                d.name,
                d.span,
                round_up_u64(offset, 16),
            ));
        }
    }

    let last = decls[decls.len() - 1];
    let size = round_up_u64(offset, 16);
    if size > GL_MAX_UNIFORM_BLOCK_SIZE as u64 {
        return Err(oversized(block_name, last.name, last.span, size));
    }

    // Narrowing is where a wrap would hide, so it rejects rather than truncating.
    let size =
        u32::try_from(size).map_err(|_| oversized(block_name, last.name, last.span, size))?;
    Ok(UniformArrayBlockPlan {
        block_name,
        members,
        size,
    })
}

/// OpenGL 3.3's guaranteed MAX_UNIFORM_BLOCK_SIZE minimum, so shaders stay
/// portable rather than accepting a larger limit on the build host.
pub(crate) const GL_MAX_UNIFORM_BLOCK_SIZE: u32 = 16384;

fn ty_expr(ws: &Workspace, ty: TyId) -> Option<TypeExpr> {
    match ty {
        TyId::Static(m, idx) => Some(ws.module(m).ty_arena[idx]),
        TyId::Local(_) => None,
    }
}

fn attr_kind(ws: &Workspace, ty: TyId) -> Option<AttrKind> {
    match ty_expr(ws, ty) {
        Some(TypeExpr::Builtin(BuiltinTy::Float)) => Some(AttrKind::F32),
        Some(TypeExpr::Builtin(BuiltinTy::Int)) => Some(AttrKind::I32),
        Some(TypeExpr::App {
            ctor: TypeCtor::Nominal(n),
            ..
        }) => {
            if bone_core::modules::is_intrinsic_key(n, "Vec2")
                || bone_core::modules::is_intrinsic_key(n, "Vec2f")
            {
                Some(AttrKind::F32x2)
            } else if bone_core::modules::is_intrinsic_key(n, "Vec3")
                || bone_core::modules::is_intrinsic_key(n, "Vec3f")
            {
                Some(AttrKind::F32x3)
            } else if bone_core::modules::is_intrinsic_key(n, "Vec4")
                || bone_core::modules::is_intrinsic_key(n, "Vec4f")
            {
                Some(AttrKind::F32x4)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Field order is attribute order is location order.
pub fn vertex_attrs(ws: &Workspace, struct_name: Ustr) -> Result<VertexAttrs, CompilerError> {
    let Some(sid) = ws.nominals.struct_id(struct_name) else {
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
        return Err(err(
            sd.name_span,
            format!("vertex attributes: `{struct_name}` has no fields"),
        ));
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
        attrs.push(Attr {
            name: fd.name,
            kind,
            location: i as u32,
        });
    }

    Ok(VertexAttrs { struct_name, attrs })
}
