//! The WGSL physical projection of [`ShaderInterfacePlan`].
//!
//! This crate owns no logical shader discovery and does not inspect a
//! `Workspace`. The interface plan is the only input to [`WgslAbiPlan::wgsl`];
//! WGSL's address-space layout, resource bindings and stage visibility are
//! answers made here. In particular, the texture view and sampler for one
//! logical slot stay adjacent in the WGSL binding space while the logical
//! slot remains the order published by `@textures`.

use bone_semantic::prelude::Span;
use skull_mir::gpu_layout::{SamplerKind, WireKind};
use skull_mir::shader_ir::{
    FragmentOutput, LogicalTextureGroup, LogicalUniformGroup, ShaderInterfacePlan, ShaderStage,
    UniformRole,
};
use skull_parser::CompilerError;
use ustr::Ustr;

pub const WGSL_BIND_GROUP: u32 = 0;
pub const FULLSCREEN_POSITION_LOCATION: u32 = 0;
pub const FULLSCREEN_UV_LOCATION: u32 = 1;
pub const FULLSCREEN_VARYING_LOCATION: u32 = 0;
pub const FRAGMENT_OUTPUT_LOCATION: u32 = 0;
pub const FULLSCREEN_VERTEX_STRIDE: u32 = 16;

fn err(span: Span, message: impl Into<String>) -> CompilerError {
    CompilerError::new_with_label(span, message.into())
}

fn round_up(value: u32, alignment: u32) -> u32 {
    value.div_ceil(alignment) * alignment
}

fn identifier(name: Ustr, what: &str, span: Span) -> Result<(), CompilerError> {
    let s = name.as_str();
    let first = s.chars().next();
    if s.is_empty()
        || !first.is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
        || !s.chars().all(|c| c == '_' || c.is_ascii_alphanumeric())
    {
        return Err(err(
            span,
            format!("WGSL {what} `{name}` is not an identifier"),
        ));
    }
    if WGSL_KEYWORDS.iter().any(|keyword| *keyword == s) {
        return Err(err(
            span,
            format!("WGSL {what} `{name}` is a reserved keyword"),
        ));
    }
    Ok(())
}

const WGSL_KEYWORDS: &[&str] = &[
    "alias",
    "break",
    "case",
    "const",
    "continue",
    "continuing",
    "default",
    "diagnostic",
    "discard",
    "else",
    "enable",
    "false",
    "fn",
    "for",
    "if",
    "let",
    "loop",
    "override",
    "requires",
    "return",
    "struct",
    "switch",
    "true",
    "var",
    "while",
    "bitcast",
    "bool",
    "f16",
    "f32",
    "i32",
    "mat2x2",
    "mat2x3",
    "mat2x4",
    "mat3x2",
    "mat3x3",
    "mat3x4",
    "mat4x2",
    "mat4x3",
    "mat4x4",
    "ptr",
    "sampler",
    "texture_2d",
    "u32",
    "vec2",
    "vec3",
    "vec4",
    "array",
    "atomic",
    "private",
    "function",
    "override",
    "storage",
    "uniform",
    "workgroup",
    "read",
    "write",
    "read_write",
    "vertex",
    "fragment",
    "compute",
    "location",
    "builtin",
    "group",
    "binding",
    "interpolate",
    "invariant",
    "workgroup_size",
    "align",
    "size",
    "stride",
    "id",
    "position",
    "front_facing",
    "frag_depth",
    "sample_index",
    "sample_mask",
    "global_invocation_id",
    "local_invocation_id",
    "local_invocation_index",
    "num_workgroups",
    "subgroup_invocation_id",
    "subgroup_size",
    "vertex_index",
    "instance_index",
];

/// The visibility of one physical binding. WGSL declarations use this as a
/// structural fact even though the initial projection only admits fragment
/// resources.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WgslStageVisibility {
    pub vertex: bool,
    pub fragment: bool,
}

impl WgslStageVisibility {
    fn for_interface(iface: &ShaderInterfacePlan) -> Self {
        Self {
            vertex: iface.stages.contains(&ShaderStage::Vertex),
            fragment: iface.stages.contains(&ShaderStage::Fragment),
        }
    }
}

/// One member in a WGSL uniform block's host-shareable layout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WgslUniformMember {
    pub name: Ustr,
    pub kind: WireKind,
    pub offset: u32,
    pub align: u32,
    pub size: u32,
}

/// The WGSL layout and binding of one logical uniform group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WgslUniformBlock {
    pub role: UniformRole,
    pub struct_name: Ustr,
    pub binding: u32,
    pub size: u32,
    pub members: Vec<WgslUniformMember>,
    pub visibility: WgslStageVisibility,
}

/// The paired WGSL resources for one logical texture slot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WgslTextureBinding {
    /// The logical `@textures` declaration order. This is not a physical
    /// binding number and is what keeps CPU `GpuBinding.slot` meaningful.
    pub slot: u32,
    pub name: Ustr,
    pub kind: SamplerKind,
    pub texture_binding: u32,
    pub sampler_binding: u32,
    pub visibility: WgslStageVisibility,
}

/// The compiler-owned geometry contract for the fullscreen vertex stage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WgslFullscreenVertex {
    pub position_location: u32,
    pub uv_location: u32,
    pub varying_location: u32,
    pub stride: u32,
}

/// The fragment input/output locations for the admitted fullscreen shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WgslFragmentIo {
    pub uv_location: u32,
    pub output_location: u32,
    pub output: WireKind,
}

/// WGSL's physical answer to one target-neutral fullscreen interface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WgslAbiPlan {
    pub bind_group: u32,
    pub fullscreen_vertex: WgslFullscreenVertex,
    pub fragment: WgslFragmentIo,
    pub uniform_blocks: Vec<WgslUniformBlock>,
    pub textures: Vec<WgslTextureBinding>,
}

impl WgslAbiPlan {
    /// Derive WGSL's layout from the neutral interface alone.
    pub fn wgsl(iface: &ShaderInterfacePlan, span: Span) -> Result<Self, CompilerError> {
        validate_fullscreen_interface(iface, span)?;

        let visibility = WgslStageVisibility::for_interface(iface);
        let mut groups = iface.uniform_groups.clone();
        groups.sort_by_key(|g| uniform_role_key(g.role));

        let mut uniform_blocks = Vec::with_capacity(groups.len());
        for (binding, group) in groups.iter().enumerate() {
            uniform_blocks.push(layout_uniform_group(
                group,
                binding as u32,
                visibility,
                span,
            )?);
        }

        let texture_base = uniform_blocks.len() as u32;
        let textures = iface
            .textures
            .as_ref()
            .map(|group| layout_textures(group, texture_base, visibility, span))
            .transpose()?
            .unwrap_or_default();

        Ok(Self {
            bind_group: WGSL_BIND_GROUP,
            fullscreen_vertex: WgslFullscreenVertex {
                position_location: FULLSCREEN_POSITION_LOCATION,
                uv_location: FULLSCREEN_UV_LOCATION,
                varying_location: FULLSCREEN_VARYING_LOCATION,
                stride: FULLSCREEN_VERTEX_STRIDE,
            },
            fragment: WgslFragmentIo {
                uv_location: FULLSCREEN_VARYING_LOCATION,
                output_location: FRAGMENT_OUTPUT_LOCATION,
                output: WireKind::F32x4,
            },
            uniform_blocks,
            textures,
        })
    }

    /// Alias named after the target, for call sites that construct several
    /// physical projections beside one interface.
    pub fn from_interface(iface: &ShaderInterfacePlan, span: Span) -> Result<Self, CompilerError> {
        Self::wgsl(iface, span)
    }

    /// Recompute and compare the complete projection. This is intentionally a
    /// full comparison: changing one offset, pair binding or visibility must
    /// not be accepted merely because the rest of the plan still agrees.
    pub fn validate(&self, iface: &ShaderInterfacePlan, span: Span) -> Result<(), CompilerError> {
        let expected = Self::wgsl(iface, span)?;
        if self != &expected {
            return Err(err(
                span,
                "WGSL ABI plan no longer matches its interface projection",
            ));
        }
        Ok(())
    }

    pub fn uniform_block(&self, role: UniformRole) -> Option<&WgslUniformBlock> {
        self.uniform_blocks.iter().find(|block| block.role == role)
    }

    pub fn texture(&self, slot: u32) -> Option<&WgslTextureBinding> {
        self.textures.iter().find(|texture| texture.slot == slot)
    }
}

fn uniform_role_key(role: UniformRole) -> u8 {
    match role {
        UniformRole::Engine => 0,
        UniformRole::Stage(ShaderStage::Fragment) => 1,
        UniformRole::Stage(ShaderStage::Vertex) => 2,
    }
}

fn validate_fullscreen_interface(
    iface: &ShaderInterfacePlan,
    span: Span,
) -> Result<(), CompilerError> {
    if iface.stages != [ShaderStage::Fragment] {
        return Err(err(
            span,
            "WGSL fullscreen projection admits exactly a fragment IR stage; explicit vertex pipelines are not supported yet",
        ));
    }
    if !iface.vertex_inputs.is_empty() {
        return Err(err(
            span,
            "WGSL fullscreen projection does not accept explicit vertex inputs yet",
        ));
    }
    if !iface.varyings.is_empty() {
        return Err(err(
            span,
            "WGSL fullscreen projection owns its UV varying and does not accept explicit varyings yet",
        ));
    }
    if !iface.array_groups.is_empty() {
        return Err(err(
            span,
            "WGSL fullscreen projection does not accept uniform arrays yet",
        ));
    }
    if !iface.bare_samplers.is_empty() {
        return Err(err(
            span,
            "WGSL fullscreen projection requires grouped logical textures; bare samplers are unsupported",
        ));
    }
    if iface
        .uniform_groups
        .iter()
        .filter(|group| group.role == UniformRole::Engine)
        .count()
        > 1
    {
        return Err(err(
            span,
            "WGSL interface contains more than one engine uniform group",
        ));
    }
    if iface
        .uniform_groups
        .iter()
        .filter(|group| group.role == UniformRole::Stage(ShaderStage::Fragment))
        .count()
        > 1
    {
        return Err(err(
            span,
            "WGSL interface contains more than one fragment uniform group",
        ));
    }
    if iface
        .uniform_groups
        .iter()
        .any(|group| matches!(group.role, UniformRole::Stage(ShaderStage::Vertex)))
    {
        return Err(err(
            span,
            "WGSL fullscreen projection does not accept vertex uniform groups yet",
        ));
    }

    match &iface.fragment_output {
        FragmentOutput::Single(WireKind::F32x4) => {}
        FragmentOutput::Single(kind) => {
            return Err(err(
                span,
                format!(
                    "WGSL fullscreen projection requires vec4<f32> output, found {}",
                    kind.code()
                ),
            ));
        }
        FragmentOutput::Mrt(_) => {
            return Err(err(
                span,
                "WGSL fullscreen projection does not accept MRT output yet",
            ));
        }
        FragmentOutput::None => {
            return Err(err(
                span,
                "WGSL fullscreen projection requires a fragment output",
            ));
        }
    }

    if let Some(group) = &iface.textures {
        validate_texture_group(group, span)?;
    }
    Ok(())
}

fn validate_texture_group(group: &LogicalTextureGroup, span: Span) -> Result<(), CompilerError> {
    let Some(struct_name) = group.struct_name else {
        return Err(err(
            span,
            "WGSL grouped texture interface is missing its struct name",
        ));
    };
    identifier(struct_name, "texture group name", span)?;
    if group.samplers.is_empty() {
        return Err(err(span, "WGSL grouped texture interface cannot be empty"));
    }
    if group.stages != [ShaderStage::Fragment] {
        return Err(err(
            span,
            "WGSL fullscreen textures must be read by the fragment stage only",
        ));
    }
    for (name, _) in &group.samplers {
        identifier(*name, "texture slot name", span)?;
    }
    for (i, (a, _)) in group.samplers.iter().enumerate() {
        if group.samplers[..i].iter().any(|(b, _)| a == b) {
            return Err(err(
                span,
                format!("WGSL texture slot `{a}` is declared more than once"),
            ));
        }
    }
    Ok(())
}

fn layout_uniform_group(
    group: &LogicalUniformGroup,
    binding: u32,
    visibility: WgslStageVisibility,
    span: Span,
) -> Result<WgslUniformBlock, CompilerError> {
    identifier(group.struct_name, "uniform block name", span)?;
    if group.members.is_empty() {
        return Err(err(
            span,
            format!("WGSL uniform block `{}` cannot be empty", group.struct_name),
        ));
    }
    let mut members = Vec::with_capacity(group.members.len());
    let mut offset = 0;
    let mut max_align = 16;
    for (name, kind) in &group.members {
        identifier(*name, "uniform member name", span)?;
        if members
            .iter()
            .any(|member: &WgslUniformMember| member.name == *name)
        {
            return Err(err(
                span,
                format!("WGSL uniform member `{name}` is declared more than once"),
            ));
        }
        let (align, size) = uniform_layout(*kind).ok_or_else(|| {
            err(
                span,
                format!(
                    "WGSL uniform member `{name}` has unsupported wire kind {}",
                    kind.code()
                ),
            )
        })?;
        offset = round_up(offset, align);
        max_align = max_align.max(align);
        members.push(WgslUniformMember {
            name: *name,
            kind: *kind,
            offset,
            align,
            size,
        });
        offset += size;
    }
    let size = round_up(offset, max_align.max(16));
    Ok(WgslUniformBlock {
        role: group.role,
        struct_name: group.struct_name,
        binding,
        size,
        members,
        visibility,
    })
}

fn layout_textures(
    group: &LogicalTextureGroup,
    base: u32,
    visibility: WgslStageVisibility,
    span: Span,
) -> Result<Vec<WgslTextureBinding>, CompilerError> {
    validate_texture_group(group, span)?;
    Ok(group
        .samplers
        .iter()
        .enumerate()
        .map(|(slot, (name, kind))| WgslTextureBinding {
            slot: slot as u32,
            name: *name,
            kind: *kind,
            texture_binding: base + slot as u32 * 2,
            sampler_binding: base + slot as u32 * 2 + 1,
            visibility,
        })
        .collect())
}

/// WGSL's host-shareable scalar/vector/matrix layout for the admitted wire
/// kinds. This is deliberately separate from `gpu_layout::std140_block` even
/// where the numbers coincide for this first population.
pub fn uniform_layout(kind: WireKind) -> Option<(u32, u32)> {
    match kind {
        WireKind::F32 | WireKind::I32 => Some((4, 4)),
        WireKind::F32x2 => Some((8, 8)),
        WireKind::F32x3 => Some((16, 12)),
        WireKind::F32x4 => Some((16, 16)),
        WireKind::Mat4 => Some((16, 64)),
        WireKind::Bool32 => None,
    }
}

pub fn wgsl_type(kind: WireKind) -> Option<&'static str> {
    match kind {
        WireKind::F32 => Some("f32"),
        WireKind::F32x2 => Some("vec2<f32>"),
        WireKind::F32x3 => Some("vec3<f32>"),
        WireKind::F32x4 => Some("vec4<f32>"),
        WireKind::I32 => Some("i32"),
        WireKind::Mat4 => Some("mat4x4<f32>"),
        WireKind::Bool32 => None,
    }
}

pub fn texture_type(kind: SamplerKind) -> &'static str {
    match kind {
        SamplerKind::Float => "texture_2d<f32>",
        SamplerKind::Int => "texture_2d<i32>",
    }
}
