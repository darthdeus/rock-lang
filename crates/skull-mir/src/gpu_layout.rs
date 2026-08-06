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
    /// The kind's code on the registration wire.
    ///
    /// Written out rather than derived from the discriminant: a member reordered
    /// in the enum would otherwise renumber every kind silently, and the host
    /// reads these numbers to decide how many components a payload element has
    /// and whether it is written through the float or the int view. Pinned
    /// against the host's own table by `wire_kind_codes.rs`.
    pub fn code(self) -> u32 {
        match self {
            WireKind::F32 => 0,
            WireKind::F32x2 => 1,
            WireKind::F32x3 => 2,
            WireKind::F32x4 => 3,
            WireKind::I32 => 4,
            WireKind::Bool32 => 5,
            WireKind::Mat4 => 6,
        }
    }

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

    /// The field path each of this kind's primitives sits at, relative to the
    /// member. Empty for a scalar (the member IS the leaf).
    ///
    /// These are a CLAIM about stdlib's types, not a reading of them:
    /// [`wire_kind`] matches `Vec2`/`Mat4` by nominal name, so nothing stops
    /// their fields being reordered or renamed underneath. Leaves are bound to
    /// std140 slots positionally, so that would permute bytes while keeping
    /// every count equal. `uniform_block_checked` compares this against the
    /// value walker, which is what turns the claim into a checked one.
    fn leaf_suffixes(self) -> Vec<String> {
        match self {
            WireKind::F32 | WireKind::I32 | WireKind::Bool32 => vec![String::new()],
            WireKind::F32x2 => ["x", "y"].iter().map(|k| format!(".{k}")).collect(),
            WireKind::F32x3 => ["x", "y", "z"].iter().map(|k| format!(".{k}")).collect(),
            WireKind::F32x4 => ["x", "y", "z", "w"].iter().map(|k| format!(".{k}")).collect(),
            WireKind::Mat4 => (0..4)
                .flat_map(|c| ["x", "y", "z", "w"].map(move |k| format!(".c{c}.{k}")))
                .collect(),
        }
    }

    /// The view a leaf of this kind stores through. A block `bool` is a 32-bit
    /// value, so it rides the int view alongside `int`.
    fn view(self) -> WireView {
        match self {
            WireKind::I32 | WireKind::Bool32 => WireView::Int,
            _ => WireView::Float,
        }
    }
}

/// Which typed view a leaf store goes through. Every std140 kind a block can
/// hold is a run of 4-byte components, so one `float*` and one `int32_t*` view
/// over the block's single allocation reach every member.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireView {
    Float,
    Int,
}

/// One primitive store the packer emits: a byte offset into the block and the
/// view it goes through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LeafStore {
    pub offset: u32,
    pub view: WireView,
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
}

impl BlockMember {
    /// The member's primitive stores, in the order the value side presents its
    /// leaves. A flat 4-byte run is sound for every kind a block can hold --
    /// including `Mat4`, whose std140 column stride of 16 is exactly four
    /// packed floats, and whose stdlib form is already four column-major
    /// `Vec4`s. Change either and this stops being a run.
    pub fn leaf_stores(&self) -> Vec<LeafStore> {
        let view = self.kind.view();
        (0..self.kind.size() / 4)
            .map(|i| LeafStore { offset: self.offset + i * 4, view })
            .collect()
    }
}

/// FNV-1a. Written out rather than reaching for `DefaultHasher` because this
/// value is EMITTED into the chunk: a toolchain bump must not silently change
/// it, and re-emission has to stay byte-stable.
fn fnv1a(seed: u64, bytes: &[u8]) -> u64 {
    let mut h = seed;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

impl UniformBlockPlan {
    /// A stable identity for this block's LAYOUT.
    ///
    /// The packer derives offsets from the uniform TYPE while the destination
    /// buffer comes from the shader, and nothing in either relates the two --
    /// `set_uniform_block(blur, HeatShimmerU { .. })` is well typed on both
    /// sides. This is what a bind and an upload compare to agree they mean the
    /// same block.
    ///
    /// Size is not enough, and neither is size plus offsets: `{float, Vec2}`
    /// and `{Vec2, float}` agree on size, on both offsets, on leaf count and on
    /// view sequence, differing only in the KIND at each offset. So the kind is
    /// hashed, and the names with it -- the driver resolves members by name.
    ///
    /// Folded to 53 bits because it is EMITTED as a plain numeric literal into
    /// the `__rock_shader_register` call, and LuaJIT and JS both represent
    /// integers exactly only that far -- a wider value would round on the way
    /// in, which is the one failure this value exists to prevent. Folding here
    /// rather than at the emission site is what keeps bind, upload and the
    /// tests reading ONE number instead of three that have to agree.
    pub fn fingerprint(&self) -> u64 {
        let mut h = fnv1a(0xcbf2_9ce4_8422_2325, self.struct_name.as_bytes());
        for m in &self.members {
            h = fnv1a(h, m.name.as_bytes());
            h = fnv1a(h, &[m.kind as u8]);
            h = fnv1a(h, &m.offset.to_le_bytes());
        }
        let h = fnv1a(h, &self.size.to_le_bytes());
        // XOR-fold the high bits down rather than truncating, so they still
        // contribute to the 53 that survive.
        (h ^ (h >> 53)) & ((1 << 53) - 1)
    }

    /// The primitive leaf this block expects at each slot, as a dotted field
    /// path, in the order [`Self::leaf_stores`] writes them. Compared against
    /// the value walker by `uniform_block_checked`.
    pub fn leaf_paths(&self) -> Vec<String> {
        self.members
            .iter()
            .flat_map(|m| m.kind.leaf_suffixes().into_iter().map(move |s| format!("{}{s}", m.name)))
            .collect()
    }

    /// Every store in the block, member order -- one per primitive leaf of the
    /// group struct. The packer pairs this with the value's flattened leaves,
    /// and the two lengths agreeing is what keeps the expander and its
    /// validator from each deriving the expansion and drifting.
    pub fn leaf_stores(&self) -> Vec<LeafStore> {
        self.members.iter().flat_map(|m| m.leaf_stores()).collect()
    }
}

fn round_up(v: u32, align: u32) -> u32 {
    v.div_ceil(align) * align
}

/// The wire kind for a declared field type, or `None` if it has no std140 form.
///
/// Public because a `UniformArray<T>`'s ELEMENT is classified through it too:
/// the array lane asking a different function what `Vec2` is on the wire is the
/// second layout authority the separate-plan design exists to avoid.
pub fn wire_kind(ws: &Workspace, ty: TyId) -> Option<WireKind> {
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
    // The declaring struct's own span, so an over-budget block still points at
    // the declaration rather than becoming location-free.
    let span = ws
        .struct_by_name
        .get(&struct_name)
        .map(|sid| ws.modules[sid.module.index()].structs[sid.def].name_span);
    std140_block(struct_name, &uniform_group_members(ws, struct_name)?, span)
}

/// A uniform group's members in declaration order, with neutral kinds and NO
/// places -- the logical half of a group, and the input a target lays out.
///
/// This is where the Workspace is read; [`std140_block`] never touches it, so a
/// layout is a function of the member list alone -- which is what lets a second
/// target lay the same members out its own way.
pub fn uniform_group_members(
    ws: &Workspace,
    struct_name: Ustr,
) -> Result<Vec<(Ustr, WireKind)>, CompilerError> {
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
    for fd in &sd.fields {
        let Some(kind) = wire_kind(ws, fd.ty) else {
            let ty_name = ty_expr(ws, fd.ty)
                .and_then(|t| match t {
                    TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } => Some(n.to_string()),
                    _ => None,
                })
                .unwrap_or_else(|| "that type".to_string());
            // A sampler is the one non-wire field with an obvious fix, and
            // putting the texture beside the scalars is a plausible mistake.
            if matches!(ty_name.as_str(), "Sampler2D" | "ISampler2D") {
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
                     and arrays are not packed yet -- see docs/shader-binding-layout-plan.md",
                    fd.name
                ),
            ));
        };
        members.push((fd.name, kind));
    }
    Ok(members)
}

/// **The std140 layout of a member list.** GL's physical answer, and a pure
/// function of `members` -- no Workspace, no struct lookup.
///
/// `span` anchors the over-budget rejection when the caller has one; without it
/// the error is location-free rather than pointing at an unrelated declaration.
pub fn std140_block(
    struct_name: Ustr,
    members: &[(Ustr, WireKind)],
    span: Option<Span>,
) -> Result<UniformBlockPlan, CompilerError> {
    if members.is_empty() {
        let msg = format!("uniform block: `{struct_name}` has no members -- an empty block binds nothing");
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
    if size > GLES3_MAX_UNIFORM_BLOCK_SIZE {
        let msg = format!(
            "uniform block: `{struct_name}` needs {size} std140 bytes, over the {GLES3_MAX_UNIFORM_BLOCK_SIZE} \
             an ES 3.0 driver is required to offer. The GLSL profile follows the runtime GL \
             context, so this has to fit the tightest target the program can land on -- and \
             nothing downstream would catch it, because `shader-validate`'s 300es leg links \
             against the host driver's own (much larger) limit"
        );
        return Err(match span {
            Some(sp) => err(sp, msg),
            None => CompilerError::new_no_location(msg),
        });
    }

    Ok(UniformBlockPlan { struct_name, members: out, size })
}

/// One `UniformArray<T>` member as DECLARED: the shader entry's `@size(N)` param.
/// Offsets are the layout's answer, not the caller's, so they are absent here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArrayMemberDecl {
    pub name: Ustr,
    pub elem: WireKind,
    pub capacity: u32,
    /// The `@size(N)` annotation this member's capacity came from. Every
    /// rejection below is spanned here rather than at the entry: the fix for an
    /// over-budget block or an unsupported element is always to change one
    /// param's annotation, and a whole-fn span makes the reader find which.
    pub span: Span,
}

/// One laid-out array member of an array block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArrayMember {
    pub name: Ustr,
    pub elem: WireKind,
    pub capacity: u32,
    pub offset: u32,
    /// std140 array stride: the element's base alignment rounded up to 16. That
    /// is 16 for every kind an array may hold, so it is recorded rather than
    /// assumed — a backend that gains a wider element kind reads this, and the
    /// scalar-array waste (a `float[16]` costing 256 bytes) is visible in it.
    pub stride: u32,
}

/// A stage's ARRAY block: the `UniformArray<T>` params of one shader entry, laid
/// out std140 at their own binding.
///
/// Deliberately NOT members of [`UniformBlockPlan`]. That type's contract carries
/// a value-struct producer — `lower::shape::uniform_block_checked` requires its
/// `leaf_paths()` to equal `value_leaf_paths` over the same struct, matched by
/// position — and an array member emits leaves no value struct can produce. One
/// std140 authority (the [`WireKind`] rules and `round_up` below), two producers.
///
/// Separate from the scalar block PHYSICALLY too, not just as a type: the scalar
/// group changes per draw while arrays change per activation, so sharing one
/// buffer would force either a per-draw copy of the whole array payload or
/// partial-update state. See `docs/shader-binding-layout-plan.md`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UniformArrayBlockPlan {
    pub block_name: Ustr,
    pub members: Vec<ArrayMember>,
    /// Total size, rounded up to 16. Already a multiple of 16 given every
    /// member's stride is, but rounded for the same reason the scalar block is.
    pub size: u32,
}

impl UniformArrayBlockPlan {
    /// A stable identity for this block's LAYOUT, the array lane's twin of
    /// [`UniformBlockPlan::fingerprint`] and folded to 53 bits for the same
    /// reason: it is emitted as a plain numeric literal, and LuaJIT and JS
    /// represent integers exactly only that far.
    ///
    /// Seeded differently from the scalar fingerprint so an array block and a
    /// scalar block cannot hash alike — they are independent bindings uploaded
    /// separately, and a bind that compared the wrong one would accept a
    /// mismatched buffer.
    pub fn fingerprint(&self) -> u64 {
        let mut h = fnv1a(0xa5a5_5a5a_c3c3_3c3c, self.block_name.as_bytes());
        for m in &self.members {
            h = fnv1a(h, m.name.as_bytes());
            h = fnv1a(h, &[m.elem as u8]);
            h = fnv1a(h, &m.offset.to_le_bytes());
            h = fnv1a(h, &m.capacity.to_le_bytes());
            h = fnv1a(h, &m.stride.to_le_bytes());
        }
        let h = fnv1a(h, &self.size.to_le_bytes());
        (h ^ (h >> 53)) & ((1 << 53) - 1)
    }
}

fn round_up_u64(v: u64, align: u64) -> u64 {
    (v + align - 1) / align * align
}

/// The over-budget rejection, shared so every path that can produce an
/// unrepresentable size reports the same way rather than one of them truncating.
///
/// Spanned at the member that pushed the block over, which is the one whose
/// `@size(N)` the author can usefully shrink.
fn oversized(block_name: Ustr, member: Ustr, span: Span, size: u64) -> CompilerError {
    CompilerError::new_with_label(span, format!(
        "uniform array block: `{block_name}` needs {size} std140 bytes at `{member}`, over the \
         {GLES3_MAX_UNIFORM_BLOCK_SIZE} an ES 3.0 driver is required to offer. Array stride is 16 \
         for every element kind, so a `@size(N)` array costs `N * 16` whatever it holds -- the \
         budget is per BLOCK, and this one is the arrays alone"
    ))
}

/// Lay out one stage's `UniformArray<T>` params as a std140 array block.
///
/// The admitted element kinds are exactly the set the GLSL emitter admits
/// (`array_elem_kind` there): float, int, Vec2, Vec4. `Vec3` is refused because
/// its base alignment is 16 while its size is 12, so an array of them is neither
/// tightly packed nor stride-12; `Mat4` and `bool` are simply not offered yet.
/// Both sides refusing the same set is what stops the layout and the declaration
/// drifting into disagreeing about what an array may hold.
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
                // An array's stride is its element's base alignment rounded up
                // to 16. Every kind here rounds to 16; computed rather than
                // written as a literal so a new kind cannot inherit it silently.
                round_up(d.elem.align(), 16)
            }
            WireKind::F32x3 | WireKind::Mat4 | WireKind::Bool32 => {
                let name = match d.elem {
                    WireKind::F32x3 => "Vec3",
                    WireKind::Mat4 => "Mat4",
                    _ => "bool",
                };
                return Err(CompilerError::new_with_label(d.span, format!(
                    "uniform array block: `{block_name}` member `{}` has element type `{name}`, \
                     which a uniform array may not hold (allowed: float, int, Vec2, Vec4)",
                    d.name
                )));
            }
        };
        if d.capacity == 0 {
            return Err(CompilerError::new_with_label(d.span, format!(
                "uniform array block: `{block_name}` member `{}` has capacity 0 -- `@size(N)` \
                 declares how many elements the shader may index, so 0 declares an array nothing \
                 can read",
                d.name
            )));
        }

        // u64 throughout, and the budget is checked BEFORE anything narrows.
        // `capacity` is whatever `@size(N)` says, so `capacity * stride` is
        // source-controlled: in u32 it panics under overflow checks and WRAPS
        // without them, and a wrap lands on a small size that passes the budget
        // -- evading the one thing this function enforces.
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
        if offset > GLES3_MAX_UNIFORM_BLOCK_SIZE as u64 {
            return Err(oversized(block_name, d.name, d.span, round_up_u64(offset, 16)));
        }
    }

    // The last member is the one that reached the size, so it carries the span
    // for both remaining checks.
    let last = decls[decls.len() - 1];
    let size = round_up_u64(offset, 16);
    if size > GLES3_MAX_UNIFORM_BLOCK_SIZE as u64 {
        return Err(oversized(block_name, last.name, last.span, size));
    }

    // Unreachable given the check above, but narrowing is where a wrap would
    // hide, so it rejects rather than truncating.
    let size =
        u32::try_from(size).map_err(|_| oversized(block_name, last.name, last.span, size))?;
    Ok(UniformArrayBlockPlan { block_name, members, size })
}

/// The ES 3.0 guaranteed minimum for `MAX_UNIFORM_BLOCK_SIZE` (ES 3.0 spec table
/// 6.31), and the budget a block is held to.
///
/// Not the host driver's limit, deliberately: a desktop GL implementation
/// reports far more (65536 on the dev machine's NVIDIA driver), so a block
/// between the two numbers compiles and links on every gate in this repo and
/// fails only on a physical device. A size the block may REACH, not exceed.
pub const GLES3_MAX_UNIFORM_BLOCK_SIZE: u32 = 16384;

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
