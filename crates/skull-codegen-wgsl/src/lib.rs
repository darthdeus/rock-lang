//! The WGSL physical projection of [`ShaderInterfacePlan`].
//!
//! This crate owns no logical shader discovery and does not inspect a
//! `Workspace`. The interface plan is the only input to [`WgslAbiPlan::wgsl`];
//! WGSL's address-space layout, resource bindings and stage visibility are
//! answers made here. In particular, the texture view and sampler for one
//! logical slot stay adjacent in the WGSL binding space while the logical
//! slot remains the order published by `@textures`.

use bone_semantic::prelude::Span;
use bone_semantic::{
    hir::{BuiltinTy, Function, TyId, TypeCtor, TypeExpr},
    inference::Workspace,
};
use comfy_la_arena::Idx;
use skull_codegen_shader::{ShaderArtifactPlan, ShaderPlannerPolicy};
use skull_mir::gpu_layout::{AttrKind, SamplerKind, WireKind};
use skull_mir::mir::{Inst, Lit, MirFunc, Term};
use skull_mir::shader_ir::{
    FragmentOutput, LogicalTextureGroup, LogicalUniformGroup, ShaderIR, ShaderInstance,
    ShaderInterfacePlan, ShaderStage, ShaderStageIr, UniformRole, VertexRate,
};
use skull_parser::CompilerError;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use ustr::Ustr;

pub const WGSL_BIND_GROUP: u32 = 0;
pub const FULLSCREEN_VARYING_LOCATION: u32 = 0;
pub const FRAGMENT_OUTPUT_LOCATION: u32 = 0;
pub const WGSL_ABI_WIRE_VERSION: &str = "WGSL_ABI_V1";

fn err(span: Span, message: impl Into<String>) -> CompilerError {
    CompilerError::new_with_label(span, message.into())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WgslBuiltin {
    Clamp,
    Floor,
    Fract,
    Max,
    Min,
    Abs,
}

impl WgslBuiltin {
    fn name(self) -> &'static str {
        match self {
            Self::Clamp => "clamp",
            Self::Floor => "floor",
            Self::Fract => "fract",
            Self::Max => "max",
            Self::Min => "min",
            Self::Abs => "abs",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WgslOperator {
    Add,
    Sub,
    Mul,
    Div,
}

impl WgslOperator {
    fn symbol(self) -> &'static str {
        match self {
            Self::Add => "+",
            Self::Sub => "-",
            Self::Mul => "*",
            Self::Div => "/",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WgslIntrinsic {
    Texture,
    VectorConstructor,
    Builtin(WgslBuiltin),
    Operator(WgslOperator),
    UnsupportedOperator,
}

impl WgslIntrinsic {
    fn classify(function: &Function) -> Option<Self> {
        let intrinsic = Self::classify_name(function.name.as_str());
        if matches!(intrinsic, Some(Self::Operator(_))) && function.param_decls.len() != 2 {
            return Some(Self::UnsupportedOperator);
        }
        intrinsic
    }

    fn classify_name(name: &str) -> Option<Self> {
        Some(match name {
            "texture" => Self::Texture,
            "v" => Self::VectorConstructor,
            "clamp" => Self::Builtin(WgslBuiltin::Clamp),
            "floor" => Self::Builtin(WgslBuiltin::Floor),
            "fract" => Self::Builtin(WgslBuiltin::Fract),
            "max" => Self::Builtin(WgslBuiltin::Max),
            "min" => Self::Builtin(WgslBuiltin::Min),
            "abs" => Self::Builtin(WgslBuiltin::Abs),
            "__op_add" => Self::Operator(WgslOperator::Add),
            "__op_sub" => Self::Operator(WgslOperator::Sub),
            "__op_mul" => Self::Operator(WgslOperator::Mul),
            "__op_div" => Self::Operator(WgslOperator::Div),
            name if name.starts_with("__op_") => Self::UnsupportedOperator,
            _ => return None,
        })
    }

    fn is_admitted(self) -> bool {
        !matches!(self, Self::UnsupportedOperator)
    }
}

/// The inline call surface this emitter actually knows how to render. The
/// neutral planner receives this policy instead of importing a GLSL mapping.
pub struct WgslShaderPolicy;

impl ShaderPlannerPolicy for WgslShaderPolicy {
    fn inline_call(&self, function: &Function) -> bool {
        WgslIntrinsic::classify(function).is_some_and(WgslIntrinsic::is_admitted)
    }

    fn backend_name(&self) -> &'static str {
        "WGSL"
    }
}

/// Plan a compiler-owned fullscreen fragment without involving another
/// backend's emitter or ABI.
pub fn plan_fullscreen_artifact(
    ws: &Workspace,
    root: skull_mir::mir::FuncRef,
) -> Result<ShaderArtifactPlan, CompilerError> {
    skull_codegen_shader::plan_fullscreen_fragment(ws, root, &WgslShaderPolicy)
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

/// The topology the compiler-owned vertex-index fullscreen stage needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WgslPrimitiveTopology {
    TriangleStrip,
}

/// The compiler-owned geometry contract for the fullscreen vertex stage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WgslFullscreenVertex {
    pub topology: WgslPrimitiveTopology,
    pub vertex_count: u32,
    pub varying_location: u32,
}

/// The fragment input/output locations for the admitted fullscreen shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WgslFragmentIo {
    pub uv_location: u32,
    pub output_location: u32,
    pub output: WireKind,
}

/// A compiler-assigned vertex attribute in an explicit pipeline. The byte
/// offset and stride stay with the submitted buffer: they are C-layout facts,
/// while this is the shader's logical location and kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WgslVertexAttribute {
    pub location: u32,
    pub name: Ustr,
    pub kind: AttrKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WgslVertexInput {
    pub struct_name: Ustr,
    pub rate: VertexRate,
    pub attrs: Vec<WgslVertexAttribute>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WgslVarying {
    pub location: u32,
    pub name: Ustr,
    pub kind: WireKind,
    pub flat: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WgslGeometryKind {
    Fullscreen,
    Triangles,
}

/// WGSL's physical answer to one target-neutral fullscreen interface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WgslAbiPlan {
    pub semantic_fingerprint: u64,
    pub bind_group: u32,
    pub geometry: WgslGeometryKind,
    pub vertex_entry: String,
    pub fragment_entry: String,
    pub vertex_inputs: Vec<WgslVertexInput>,
    pub varyings: Vec<WgslVarying>,
    pub fullscreen_vertex: WgslFullscreenVertex,
    pub fragment: WgslFragmentIo,
    pub fullscreen_uniform: WgslUniformBlock,
    pub uniform_blocks: Vec<WgslUniformBlock>,
    pub textures: Vec<WgslTextureBinding>,
}

impl WgslAbiPlan {
    /// Derive WGSL's layout from the neutral interface alone.
    pub fn wgsl(iface: &ShaderInterfacePlan, span: Span) -> Result<Self, CompilerError> {
        validate_fullscreen_interface(iface, span)?;
        let semantic_fingerprint = iface.semantic_fingerprint(true);

        let visibility = WgslStageVisibility::for_interface(iface);
        let fullscreen_uniform = WgslUniformBlock {
            role: UniformRole::Stage(ShaderStage::Vertex),
            struct_name: Ustr::from("FullscreenU"),
            binding: 0,
            size: 16,
            members: vec![
                WgslUniformMember {
                    name: Ustr::from("uv0"),
                    kind: WireKind::F32x2,
                    offset: 0,
                    align: 8,
                    size: 8,
                },
                WgslUniformMember {
                    name: Ustr::from("uv1"),
                    kind: WireKind::F32x2,
                    offset: 8,
                    align: 8,
                    size: 8,
                },
            ],
            visibility: WgslStageVisibility { vertex: true, fragment: false },
        };
        let mut groups = iface.uniform_groups.clone();
        groups.sort_by_key(|g| uniform_role_key(g.role));

        let mut uniform_blocks = Vec::with_capacity(groups.len());
        for (binding, group) in groups.iter().enumerate() {
            uniform_blocks.push(layout_uniform_group(
                group,
                binding as u32 + 1,
                visibility,
                span,
            )?);
        }

        let texture_base = uniform_blocks.len() as u32 + 1;
        let textures = iface
            .textures
            .as_ref()
            .map(|group| layout_textures(group, texture_base, visibility, span))
            .transpose()?
            .unwrap_or_default();

        Ok(Self {
            semantic_fingerprint,
            bind_group: WGSL_BIND_GROUP,
            geometry: WgslGeometryKind::Fullscreen,
            vertex_entry: "fullscreen_vertex".into(),
            fragment_entry: "fragment_main".into(),
            vertex_inputs: Vec::new(),
            varyings: Vec::new(),
            fullscreen_vertex: WgslFullscreenVertex {
                topology: WgslPrimitiveTopology::TriangleStrip,
                vertex_count: 4,
                varying_location: FULLSCREEN_VARYING_LOCATION,
            },
            fragment: WgslFragmentIo {
                uv_location: FULLSCREEN_VARYING_LOCATION,
                output_location: FRAGMENT_OUTPUT_LOCATION,
                output: WireKind::F32x4,
            },
            fullscreen_uniform,
            uniform_blocks,
            textures,
        })
    }

    /// Derive the explicit non-indexed triangle ABI from the same neutral
    /// interface used by the GLSL pipeline. Physical C offsets are supplied by
    /// the caller's checked vertex layout; this plan owns locations, kinds and
    /// stage visibility.
    pub fn explicit(
        iface: &ShaderInterfacePlan,
        vertex_entry: &str,
        fragment_entry: &str,
        span: Span,
    ) -> Result<Self, CompilerError> {
        validate_explicit_interface(iface, span)?;
        if vertex_entry.is_empty() || fragment_entry.is_empty() {
            return Err(err(span, "WGSL explicit pipeline entry names cannot be empty"));
        }
        let mut vertex_inputs = Vec::new();
        let mut next_location = 0u32;
        for input in &iface.vertex_inputs {
            let mut attrs = Vec::new();
            for (name, kind) in &input.attrs {
                attrs.push(WgslVertexAttribute {
                    location: next_location,
                    name: *name,
                    kind: *kind,
                });
                next_location += 1;
            }
            vertex_inputs.push(WgslVertexInput {
                struct_name: input.struct_name,
                rate: input.rate,
                attrs,
            });
        }
        let varyings = iface
            .varyings
            .iter()
            .enumerate()
            .map(|(location, varying)| WgslVarying {
                location: location as u32,
                name: varying.name,
                kind: varying.kind,
                flat: varying.flat,
            })
            .collect::<Vec<_>>();

        let mut groups = iface.uniform_groups.clone();
        groups.sort_by_key(|group| uniform_role_key(group.role));
        let mut uniform_blocks = Vec::with_capacity(groups.len());
        for (binding, group) in groups.iter().enumerate() {
            uniform_blocks.push(layout_uniform_group(
                group,
                binding as u32,
                visibility_for_role(group.role),
                span,
            )?);
        }
        let texture_base = uniform_blocks.len() as u32;
        let textures = iface
            .textures
            .as_ref()
            .map(|group| {
                layout_textures(
                    group,
                    texture_base,
                    WgslStageVisibility {
                        vertex: group.stages.contains(&ShaderStage::Vertex),
                        fragment: group.stages.contains(&ShaderStage::Fragment),
                    },
                    span,
                )
            })
            .transpose()?
            .unwrap_or_default();
        let output = match iface.fragment_output {
            FragmentOutput::Single(kind) => kind,
            _ => unreachable!("validate_explicit_interface admits only one output"),
        };
        Ok(Self {
            semantic_fingerprint: iface.semantic_fingerprint(false),
            bind_group: WGSL_BIND_GROUP,
            geometry: WgslGeometryKind::Triangles,
            vertex_entry: vertex_entry.to_string(),
            fragment_entry: fragment_entry.to_string(),
            vertex_inputs,
            varyings,
            fullscreen_vertex: WgslFullscreenVertex {
                topology: WgslPrimitiveTopology::TriangleStrip,
                vertex_count: 0,
                varying_location: 0,
            },
            fragment: WgslFragmentIo {
                uv_location: 0,
                output_location: 0,
                output,
            },
            fullscreen_uniform: WgslUniformBlock {
                role: UniformRole::Stage(ShaderStage::Vertex),
                struct_name: Ustr::from("FullscreenU"),
                binding: 0,
                size: 16,
                members: vec![],
                visibility: WgslStageVisibility { vertex: false, fragment: false },
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
        let expected = match self.geometry {
            WgslGeometryKind::Fullscreen => Self::wgsl(iface, span)?,
            WgslGeometryKind::Triangles => {
                Self::explicit(iface, &self.vertex_entry, &self.fragment_entry, span)?
            }
        };
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

    /// Serialize the physical projection for the runtime boundary.
    ///
    /// This is deliberately a small versioned wire rather than WGSL metadata
    /// recovered by the runtime. The checksum covers every physical field, so
    /// a stale or independently edited binding/order/offset/visibility answer
    /// is rejected before a backend can consume the source.
    pub fn wire(&self) -> String {
        let mut payload = String::new();
        writeln!(payload, "semantic {}", self.semantic_fingerprint).unwrap();
        writeln!(payload, "group {}", self.bind_group).unwrap();
        match self.geometry {
            WgslGeometryKind::Fullscreen => {
                let topology = match self.fullscreen_vertex.topology {
                    WgslPrimitiveTopology::TriangleStrip => "triangle_strip",
                };
                writeln!(
                    payload,
                    "vertex {} {} {}",
                    topology,
                    self.fullscreen_vertex.vertex_count,
                    self.fullscreen_vertex.varying_location
                )
                .unwrap();
                writeln!(
                    payload,
                    "fragment {} {} {}",
                    self.fragment.uv_location,
                    self.fragment.output_location,
                    self.fragment.output.code()
                )
                .unwrap();
                writeln!(payload, "fullscreen FullscreenU 0 16 v 2").unwrap();
                writeln!(payload, "member uv0 1 0 8 8").unwrap();
                writeln!(payload, "member uv1 1 8 8 8").unwrap();
            }
            WgslGeometryKind::Triangles => {
                writeln!(payload, "geometry triangles").unwrap();
                writeln!(payload, "entries {} {}", self.vertex_entry, self.fragment_entry).unwrap();
                writeln!(payload, "vertex_inputs {}", self.vertex_inputs.len()).unwrap();
                for (index, input) in self.vertex_inputs.iter().enumerate() {
                    let rate = match input.rate {
                        VertexRate::PerVertex => "vertex",
                        VertexRate::PerInstance => "instance",
                    };
                    writeln!(
                        payload,
                        "vertex_input {} {} {} {}",
                        index,
                        input.struct_name,
                        rate,
                        input.attrs.len()
                    )
                    .unwrap();
                    for attr in &input.attrs {
                        writeln!(
                            payload,
                            "attr {} {} {}",
                            attr.location,
                            attr.name,
                            attr_kind_wire(attr.kind)
                        )
                        .unwrap();
                    }
                }
                writeln!(payload, "varyings {}", self.varyings.len()).unwrap();
                for varying in &self.varyings {
                    writeln!(
                        payload,
                        "varying {} {} {} {}",
                        varying.location,
                        varying.name,
                        varying.kind.code(),
                        if varying.flat { 1 } else { 0 }
                    )
                    .unwrap();
                }
                writeln!(
                    payload,
                    "fragment {} {}",
                    self.fragment.output_location,
                    self.fragment.output.code()
                )
                .unwrap();
            }
        }
        writeln!(payload, "blocks {}", self.uniform_blocks.len()).unwrap();
        for block in &self.uniform_blocks {
            writeln!(
                payload,
                "block {} {} {} {} {} {}",
                uniform_role_wire(block.role),
                block.struct_name,
                block.binding,
                block.size,
                visibility_wire(block.visibility),
                block.members.len()
            )
            .unwrap();
            for member in &block.members {
                writeln!(
                    payload,
                    "member {} {} {} {} {}",
                    member.name,
                    member.kind.code(),
                    member.offset,
                    member.align,
                    member.size
                )
                .unwrap();
            }
        }
        writeln!(payload, "textures {}", self.textures.len()).unwrap();
        for texture in &self.textures {
            writeln!(
                payload,
                "texture {} {} {} {} {} {}",
                texture.slot,
                texture.name,
                sampler_kind_wire(texture.kind),
                texture.texture_binding,
                texture.sampler_binding,
                visibility_wire(texture.visibility)
            )
            .unwrap();
        }
        format!(
            "{}\nchecksum {}\n{}",
            WGSL_ABI_WIRE_VERSION,
            wire_checksum(payload.as_bytes()),
            payload
        )
    }
}

fn uniform_role_wire(role: UniformRole) -> &'static str {
    match role {
        UniformRole::Engine => "engine",
        UniformRole::Stage(ShaderStage::Fragment) => "fragment",
        UniformRole::Stage(ShaderStage::Vertex) => "vertex",
    }
}

fn sampler_kind_wire(kind: SamplerKind) -> &'static str {
    match kind {
        SamplerKind::Float => "float",
        SamplerKind::Int => "int",
    }
}

fn attr_kind_wire(kind: AttrKind) -> &'static str {
    match kind {
        AttrKind::F32 => "f32",
        AttrKind::F32x2 => "f32x2",
        AttrKind::F32x3 => "f32x3",
        AttrKind::F32x4 => "f32x4",
        AttrKind::I32 => "i32",
    }
}

fn visibility_wire(visibility: WgslStageVisibility) -> &'static str {
    match (visibility.vertex, visibility.fragment) {
        (false, false) => "none",
        (true, false) => "v",
        (false, true) => "f",
        (true, true) => "vf",
    }
}

fn wire_checksum(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
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

fn visibility_for_role(role: UniformRole) -> WgslStageVisibility {
    match role {
        UniformRole::Engine => WgslStageVisibility { vertex: true, fragment: true },
        UniformRole::Stage(ShaderStage::Vertex) => {
            WgslStageVisibility { vertex: true, fragment: false }
        }
        UniformRole::Stage(ShaderStage::Fragment) => {
            WgslStageVisibility { vertex: false, fragment: true }
        }
    }
}

fn validate_explicit_interface(
    iface: &ShaderInterfacePlan,
    span: Span,
) -> Result<(), CompilerError> {
    if iface.stages != [ShaderStage::Vertex, ShaderStage::Fragment] {
        return Err(err(
            span,
            "WGSL explicit projection admits one vertex stage followed by one fragment stage",
        ));
    }
    if iface.vertex_inputs.len() != 1 {
        return Err(err(
            span,
            "WGSL explicit projection currently admits one vertex input stream",
        ));
    }
    let input = &iface.vertex_inputs[0];
    if input.rate != VertexRate::PerVertex || input.attrs.is_empty() {
        return Err(err(
            span,
            "WGSL explicit projection currently admits non-empty per-vertex attributes only",
        ));
    }
    identifier(input.struct_name, "vertex input name", span)?;
    for (name, kind) in &input.attrs {
        identifier(*name, "vertex attribute name", span)?;
        if matches!(kind, AttrKind::I32) {
            return Err(err(
                span,
                "WGSL explicit projection currently admits float vertex attributes only",
            ));
        }
    }
    if iface.varyings.is_empty() {
        return Err(err(span, "WGSL explicit projection requires fragment varyings"));
    }
    for varying in &iface.varyings {
        identifier(varying.name, "varying name", span)?;
        if matches!(varying.kind, WireKind::Bool32 | WireKind::I32 | WireKind::F32x3) {
            return Err(err(
                span,
                "WGSL explicit projection currently admits float scalar/vector varyings other than vec3",
            ));
        }
    }
    if !iface.array_groups.is_empty() || !iface.bare_samplers.is_empty() {
        return Err(err(
            span,
            "WGSL explicit projection does not admit uniform arrays or bare samplers",
        ));
    }
    if iface
        .uniform_groups
        .iter()
        .filter(|group| group.role == UniformRole::Engine)
        .count()
        > 1
    {
        return Err(err(span, "WGSL interface contains more than one engine uniform group"));
    }
    if iface
        .uniform_groups
        .iter()
        .any(|group| matches!(group.role, UniformRole::Stage(_)))
    {
        return Err(err(
            span,
            "WGSL explicit projection currently admits the engine block only",
        ));
    }
    if !matches!(iface.fragment_output, FragmentOutput::Single(WireKind::F32x4)) {
        return Err(err(
            span,
            "WGSL explicit projection requires one vec4<f32> fragment output",
        ));
    }
    if let Some(group) = &iface.textures {
        validate_texture_group(group, span)?;
    } else {
        return Err(err(span, "WGSL explicit projection requires a grouped texture input"));
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

/// A WGSL module emitted from one compiler-owned fullscreen fragment stage.
#[derive(Clone)]
pub struct WgslShaderEmission {
    pub source: String,
    pub dependencies: Vec<skull_mir::mir::FuncRef>,
    pub interface: ShaderInterfacePlan,
    pub abi: WgslAbiPlan,
    pub ir: ShaderIR,
}

/// Emit a compiler-owned fullscreen module directly from neutral MIR.
///
/// `Workspace` supplies neutral declaration names and type metadata for MIR
/// calls. It does not participate in ABI projection: callers must provide the
/// `WgslAbiPlan` already derived from `ShaderInterfacePlan`.
pub fn emit_fullscreen_shader(
    ws: &Workspace,
    ir: &ShaderIR,
    iface: &ShaderInterfacePlan,
    abi: &WgslAbiPlan,
    span: Span,
) -> Result<WgslShaderEmission, CompilerError> {
    abi.validate(iface, span)?;
    validate_ir_shape(ir, span)?;
    let stage = &ir.stages[0];
    let entry_fn = function_for_instance(ws, &stage.entry);
    let entry_roles = entry_param_roles(ws, entry_fn, &stage.body, iface, abi, span)?;
    validate_known_bodies(stage, span)?;

    let mut source = String::new();
    emit_fullscreen_types(&mut source, abi);
    emit_uniforms(&mut source, abi);
    emit_textures(&mut source, abi);

    for instance in &stage.order {
        let body = stage.bodies.get(instance).ok_or_else(|| {
            err(
                span,
                format!(
                    "WGSL IR order names `{}` without a lowered body",
                    instance.symbol(ws)
                ),
            )
        })?;
        let f = function_for_instance(ws, instance);
        let emitter = FunctionEmitter::new(ws, stage, instance, body, abi, HashMap::new(), span)?;
        source.push_str(&emitter.emit_function(f, false)?);
    }

    let emitter =
        FunctionEmitter::new(ws, stage, &stage.entry, &stage.body, abi, entry_roles, span)?;
    source.push_str(&emitter.emit_function(entry_fn, true)?);
    emit_fullscreen_entries(&mut source, abi, stage.entry.symbol(ws));

    Ok(WgslShaderEmission {
        source,
        dependencies: ir.declarations(),
        interface: iface.clone(),
        abi: abi.clone(),
        ir: ir.clone(),
    })
}

pub fn emit_explicit_shader(
    ws: &Workspace,
    ir: &ShaderIR,
    iface: &ShaderInterfacePlan,
    abi: &WgslAbiPlan,
    span: Span,
) -> Result<WgslShaderEmission, CompilerError> {
    if abi.geometry != WgslGeometryKind::Triangles {
        return Err(err(span, "WGSL explicit emission requires the triangles ABI"));
    }
    abi.validate(iface, span)?;
    validate_explicit_ir_shape(ir, span)?;
    let vertex = &ir.stages[0];
    let fragment = &ir.stages[1];
    let vertex_fn = function_for_instance(ws, &vertex.entry);
    let fragment_fn = function_for_instance(ws, &fragment.entry);
    let vertex_roles = explicit_entry_param_roles(
        ws,
        vertex_fn,
        &vertex.body,
        iface,
        ShaderStage::Vertex,
        span,
    )?;
    let fragment_roles = explicit_entry_param_roles(
        ws,
        fragment_fn,
        &fragment.body,
        iface,
        ShaderStage::Fragment,
        span,
    )?;
    validate_known_bodies(vertex, span)?;
    validate_known_bodies(fragment, span)?;

    let mut source = String::new();
    emit_explicit_types(&mut source, ws, vertex_fn, abi, span)?;
    emit_explicit_uniforms(&mut source, abi);
    emit_textures(&mut source, abi);

    for stage in [vertex, fragment] {
        for instance in &stage.order {
            let body = stage.bodies.get(instance).ok_or_else(|| {
                err(
                    span,
                    format!(
                        "WGSL IR order names {} without a lowered body",
                        instance.symbol(ws)
                    ),
                )
            })?;
            let function = function_for_instance(ws, instance);
            let emitter = FunctionEmitter::new(
                ws,
                stage,
                instance,
                body,
                abi,
                HashMap::new(),
                span,
            )?;
            source.push_str(&emitter.emit_function(function, false)?);
        }
    }

    let emitter = FunctionEmitter::new(
        ws,
        vertex,
        &vertex.entry,
        &vertex.body,
        abi,
        vertex_roles,
        span,
    )?;
    source.push_str(&emitter.emit_function(vertex_fn, true)?);
    let emitter = FunctionEmitter::new(
        ws,
        fragment,
        &fragment.entry,
        &fragment.body,
        abi,
        fragment_roles,
        span,
    )?;
    source.push_str(&emitter.emit_function(fragment_fn, true)?);
    emit_explicit_entries(&mut source, ws, vertex, fragment, abi, span)?;

    Ok(WgslShaderEmission {
        source,
        dependencies: ir.declarations(),
        interface: iface.clone(),
        abi: abi.clone(),
        ir: ir.clone(),
    })
}

fn validate_explicit_ir_shape(ir: &ShaderIR, span: Span) -> Result<(), CompilerError> {
    if ir.stages.len() != 2
        || ir.stages[0].stage != ShaderStage::Vertex
        || ir.stages[1].stage != ShaderStage::Fragment
    {
        return Err(err(
            span,
            "WGSL explicit emission requires one vertex and one fragment IR stage",
        ));
    }
    Ok(())
}

fn explicit_struct_fields(
    ws: &Workspace,
    name: Ustr,
    span: Span,
) -> Result<Vec<(Ustr, ValueKind)>, CompilerError> {
    let Some(&sid) = ws.struct_by_name.get(&name) else {
        return Err(err(span, format!("WGSL value struct {name} is not declared")));
    };
    ws.modules[sid.module.index()].structs[sid.def]
        .fields
        .iter()
        .map(|field| {
            let kind = value_kind_for_type(ws, field.ty).ok_or_else(|| {
                err(
                    span,
                    format!(
                        "WGSL value struct {name} field {} has no admitted type",
                        field.name
                    ),
                )
            })?;
            Ok((field.name, kind))
        })
        .collect()
}

fn emit_value_struct(
    out: &mut String,
    name: Ustr,
    fields: &[(Ustr, ValueKind)],
    span: Span,
) -> Result<(), CompilerError> {
    writeln!(out, "struct {} {{", name).unwrap();
    for (field, kind) in fields {
        let ty = wgsl_value_type(*kind)
            .ok_or_else(|| err(span, format!("WGSL value struct field {field} has no type")))?;
        writeln!(out, "  {field}: {ty},").unwrap();
    }
    out.push_str("}\n\n");
    Ok(())
}

fn attr_wgsl_type(kind: AttrKind) -> &'static str {
    match kind {
        AttrKind::F32 => "f32",
        AttrKind::F32x2 => "vec2<f32>",
        AttrKind::F32x3 => "vec3<f32>",
        AttrKind::F32x4 => "vec4<f32>",
        AttrKind::I32 => "i32",
    }
}

fn varying_output_name(
    ws: &Workspace,
    vertex: &ShaderStageIr,
    span: Span,
) -> Result<String, CompilerError> {
    let function = function_for_instance(ws, &vertex.entry);
    let ty = function
        .return_ty
        .and_then(|ty| nominal_name(ws, ty))
        .ok_or_else(|| err(span, "WGSL explicit vertex return is not a named varying struct"))?;
    Ok(ty.to_string())
}

fn emit_explicit_types(
    out: &mut String,
    ws: &Workspace,
    vertex_fn: &Function,
    abi: &WgslAbiPlan,
    span: Span,
) -> Result<(), CompilerError> {
    for input in &abi.vertex_inputs {
        let fields = input
            .attrs
            .iter()
            .map(|attr| (attr.name, attr_value_kind(attr.kind)))
            .collect::<Vec<_>>();
        emit_value_struct(out, input.struct_name, &fields, span)?;
    }
    let varying = vertex_fn
        .return_ty
        .and_then(|ty| nominal_name(ws, ty))
        .ok_or_else(|| err(span, "WGSL explicit vertex return is not a varying struct"))?;
    let fields = explicit_struct_fields(ws, varying, span)?;
    emit_value_struct(out, varying, &fields, span)?;
    writeln!(out, "struct {varying}Out {{").unwrap();
    writeln!(out, "  @builtin(position) position: vec4<f32>,").unwrap();
    for field in &abi.varyings {
        let kind = fields
            .iter()
            .find(|(name, _)| *name == field.name)
            .map(|(_, kind)| *kind)
            .ok_or_else(|| err(span, format!("WGSL varying {} is absent from its struct", field.name)))?;
        let ty = wgsl_value_type(kind).unwrap();
        writeln!(out, "  @location({}) {}: {},", field.location, field.name, ty).unwrap();
    }
    out.push_str("}\n\n");
    Ok(())
}

fn emit_explicit_uniforms(out: &mut String, abi: &WgslAbiPlan) {
    for block in &abi.uniform_blocks {
        writeln!(out, "struct {} {{", block.struct_name).unwrap();
        for member in &block.members {
            writeln!(out, "  {}: {},", member.name, wgsl_type(member.kind).unwrap()).unwrap();
        }
        out.push_str("}\n\n");
        writeln!(
            out,
            "@group({}) @binding({}) var<uniform> u_{}: {};",
            abi.bind_group,
            block.binding,
            block.struct_name,
            block.struct_name
        )
        .unwrap();
    }
    if !abi.uniform_blocks.is_empty() {
        out.push('\n');
    }
}

fn emit_explicit_entries(
    out: &mut String,
    ws: &Workspace,
    vertex: &ShaderStageIr,
    fragment: &ShaderStageIr,
    abi: &WgslAbiPlan,
    span: Span,
) -> Result<(), CompilerError> {
    let varying = varying_output_name(ws, vertex, span)?;
    let output = format!("{varying}Out");
    let input = abi
        .vertex_inputs
        .first()
        .ok_or_else(|| err(span, "WGSL explicit ABI has no vertex input"))?;
    writeln!(out, "@vertex\nfn {}(", abi.vertex_entry).unwrap();
    for attr in &input.attrs {
        writeln!(
            out,
            "  @location({}) input_{}: {},",
            attr.location,
            attr.location,
            attr_wgsl_type(attr.kind)
        )
        .unwrap();
    }
    writeln!(out, ") -> {output} {{").unwrap();
    let args = input
        .attrs
        .iter()
        .map(|attr| format!("input_{}", attr.location))
        .collect::<Vec<_>>()
        .join(", ");
    writeln!(out, "  let attrs = {}({});", input.struct_name, args).unwrap();
    writeln!(out, "  let result = {}(attrs);", vertex.entry.symbol(ws)).unwrap();
    writeln!(out, "  var output: {output};").unwrap();
    out.push_str("  output.position = result.position;\n");
    for field in &abi.varyings {
        writeln!(out, "  output.{} = result.{};", field.name, field.name).unwrap();
    }
    out.push_str("  return output;\n}\n\n");

    writeln!(
        out,
        "@fragment\nfn {}(input: {output}) -> @location({}) {} {{",
        abi.fragment_entry,
        abi.fragment.output_location,
        wgsl_type(abi.fragment.output).unwrap()
    )
    .unwrap();
    let varying_args = std::iter::once("input.position".to_string())
        .chain(abi.varyings.iter().map(|field| format!("input.{}", field.name)))
        .collect::<Vec<_>>()
        .join(", ");
    writeln!(out, "  let vary = {varying}({varying_args});").unwrap();
    writeln!(out, "  return {}(vary);", fragment.entry.symbol(ws)).unwrap();
    out.push_str("}\n");
    Ok(())
}

fn validate_ir_shape(ir: &ShaderIR, span: Span) -> Result<(), CompilerError> {
    if ir.stages.len() != 1 || ir.stages[0].stage != ShaderStage::Fragment {
        return Err(err(
            span,
            "WGSL fullscreen emission requires exactly one fragment IR stage; explicit vertex pipelines are not supported yet",
        ));
    }
    Ok(())
}

fn validate_known_bodies(stage: &ShaderStageIr, span: Span) -> Result<(), CompilerError> {
    let order: HashSet<ShaderInstance> = stage.order.iter().cloned().collect();
    if order.len() != stage.order.len() || order.len() != stage.bodies.len() {
        return Err(err(
            span,
            "WGSL IR helper order and body population disagree",
        ));
    }
    if stage
        .bodies
        .keys()
        .any(|instance| !order.contains(instance))
    {
        return Err(err(
            span,
            "WGSL IR contains a helper body outside its emission order",
        ));
    }
    Ok(())
}

fn emit_fullscreen_types(out: &mut String, abi: &WgslAbiPlan) {
    let vertex = abi.fullscreen_vertex;
    out.push_str("struct FullscreenVertexOut {\n");
    writeln!(out, "  @builtin(position) position: vec4<f32>,").unwrap();
    writeln!(
        out,
        "  @location({}) uv: vec2<f32>,",
        vertex.varying_location
    )
    .unwrap();
    out.push_str("}\n\n");
}

fn emit_uniforms(out: &mut String, abi: &WgslAbiPlan) {
    let block = &abi.fullscreen_uniform;
    writeln!(out, "struct {} {{", block.struct_name).unwrap();
    for member in &block.members {
        writeln!(
            out,
            "  {}: {},",
            member.name,
            wgsl_type(member.kind).unwrap()
        )
        .unwrap();
    }
    out.push_str("}\n\n");
    writeln!(
        out,
        "@group({}) @binding({}) var<uniform> u_{}: {};",
        abi.bind_group, block.binding, block.struct_name, block.struct_name
    )
    .unwrap();
    out.push('\n');

    for block in &abi.uniform_blocks {
        writeln!(out, "struct {} {{", block.struct_name).unwrap();
        for member in &block.members {
            writeln!(
                out,
                "  {}: {},",
                member.name,
                wgsl_type(member.kind).unwrap()
            )
            .unwrap();
        }
        out.push_str("}\n\n");
        writeln!(
            out,
            "@group({}) @binding({}) var<uniform> u_{}: {};",
            abi.bind_group, block.binding, block.struct_name, block.struct_name
        )
        .unwrap();
    }
    if !abi.uniform_blocks.is_empty() {
        out.push('\n');
    }
}

fn emit_textures(out: &mut String, abi: &WgslAbiPlan) {
    for texture in &abi.textures {
        writeln!(
            out,
            "@group({}) @binding({}) var tex_{}: {};",
            abi.bind_group,
            texture.texture_binding,
            texture.name,
            texture_type(texture.kind)
        )
        .unwrap();
        writeln!(
            out,
            "@group({}) @binding({}) var samp_{}: sampler;",
            abi.bind_group, texture.sampler_binding, texture.name
        )
        .unwrap();
    }
    if !abi.textures.is_empty() {
        out.push('\n');
    }
}

fn emit_fullscreen_entries(out: &mut String, abi: &WgslAbiPlan, entry: String) {
    let fragment = abi.fragment;
    out.push_str("@vertex\nfn fullscreen_vertex(@builtin(vertex_index) vertex_index: u32) -> FullscreenVertexOut {\n");
    out.push_str("  var output: FullscreenVertexOut;\n");
    out.push_str("  let x = f32(vertex_index & 1u) * 2.0 - 1.0;\n");
    out.push_str("  let y = f32(vertex_index >> 1u) * 2.0 - 1.0;\n");
    out.push_str("  output.position = vec4<f32>(x, y, 0.0, 1.0);\n");
    out.push_str("  let corner = vec2<f32>(f32(vertex_index & 1u), f32(vertex_index >> 1u));\n");
    out.push_str("  output.uv = u_FullscreenU.uv0 + corner * (u_FullscreenU.uv1 - u_FullscreenU.uv0);\n");
    out.push_str("  return output;\n}\n\n");
    writeln!(
        out,
        "@fragment\nfn fragment_main(input: FullscreenVertexOut) -> @location({}) {} {{",
        fragment.output_location,
        wgsl_type(fragment.output).unwrap()
    )
    .unwrap();
    writeln!(out, "  return {}(input.uv);", entry).unwrap();
    out.push_str("}\n");
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ValueKind {
    F32,
    I32,
    Bool,
    Vec2,
    Vec3,
    Vec4,
    Mat4,
    Struct(Ustr),
    Sampler(SamplerKind),
}

impl ValueKind {
    fn wire(self) -> Option<WireKind> {
        match self {
            Self::F32 => Some(WireKind::F32),
            Self::I32 => Some(WireKind::I32),
            Self::Bool => None,
            Self::Vec2 => Some(WireKind::F32x2),
            Self::Vec3 => Some(WireKind::F32x3),
            Self::Vec4 => Some(WireKind::F32x4),
            Self::Mat4 => Some(WireKind::Mat4),
            Self::Struct(_) => None,
            Self::Sampler(_) => None,
        }
    }

    fn wgsl(self) -> Option<&'static str> {
        self.wire().and_then(wgsl_type)
    }

    fn components(self) -> usize {
        match self {
            Self::F32 | Self::I32 | Self::Bool => 1,
            Self::Vec2 => 2,
            Self::Vec3 => 3,
            Self::Vec4 => 4,
            Self::Mat4 | Self::Struct(_) | Self::Sampler(_) => 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ParamRole {
    Attribute,
    Varying,
    Texture,
    Uniform(UniformRole),
    TexCoord,
}

fn function_for_instance<'a>(ws: &'a Workspace, instance: &ShaderInstance) -> &'a Function {
    &ws.module(instance.fref.module).functions[instance.fref.func]
}

fn type_expr(ws: &Workspace, ty: TyId) -> Option<TypeExpr> {
    match ty {
        TyId::Static(module, index) => Some(ws.module(module).ty_arena[index]),
        TyId::Local(_) => None,
    }
}

fn nominal_name(ws: &Workspace, ty: TyId) -> Option<Ustr> {
    match type_expr(ws, ty)? {
        TypeExpr::App {
            ctor: TypeCtor::Nominal(name),
            ..
        } => Some(name),
        _ => None,
    }
}

fn value_kind_for_type(ws: &Workspace, ty: TyId) -> Option<ValueKind> {
    match type_expr(ws, ty)? {
        TypeExpr::Builtin(BuiltinTy::Float) => Some(ValueKind::F32),
        TypeExpr::Builtin(BuiltinTy::Int) => Some(ValueKind::I32),
        TypeExpr::Builtin(BuiltinTy::Bool) => Some(ValueKind::Bool),
        TypeExpr::App {
            ctor: TypeCtor::Nominal(name),
            ..
        } => match name.as_str() {
            "Vec2" => Some(ValueKind::Vec2),
            "Vec3" => Some(ValueKind::Vec3),
            "Vec4" => Some(ValueKind::Vec4),
            "Mat4" => Some(ValueKind::Mat4),
            "Sampler2D" => Some(ValueKind::Sampler(SamplerKind::Float)),
            "ISampler2D" => Some(ValueKind::Sampler(SamplerKind::Int)),
            _ => Some(ValueKind::Struct(name)),
        },
        _ => None,
    }
}

fn wgsl_value_type(kind: ValueKind) -> Option<String> {
    match kind {
        ValueKind::Struct(name) => Some(name.to_string()),
        other => other.wgsl().map(str::to_string),
    }
}

fn attr_value_kind(kind: AttrKind) -> ValueKind {
    match kind {
        AttrKind::F32 => ValueKind::F32,
        AttrKind::F32x2 => ValueKind::Vec2,
        AttrKind::F32x3 => ValueKind::Vec3,
        AttrKind::F32x4 => ValueKind::Vec4,
        AttrKind::I32 => ValueKind::I32,
    }
}

fn struct_field_kind(ws: &Workspace, ty: TyId, field: Ustr) -> Option<ValueKind> {
    let name = nominal_name(ws, ty)?;
    let sid = *ws.struct_by_name.get(&name)?;
    let def = &ws.modules[sid.module.index()].structs[sid.def];
    def.fields
        .iter()
        .find(|candidate| candidate.name == field)
        .and_then(|candidate| value_kind_for_type(ws, candidate.ty))
}

fn wire_value_kind(kind: WireKind) -> Option<ValueKind> {
    match kind {
        WireKind::F32 => Some(ValueKind::F32),
        WireKind::F32x2 => Some(ValueKind::Vec2),
        WireKind::F32x3 => Some(ValueKind::Vec3),
        WireKind::F32x4 => Some(ValueKind::Vec4),
        WireKind::I32 => Some(ValueKind::I32),
        WireKind::Mat4 => Some(ValueKind::Mat4),
        WireKind::Bool32 => None,
    }
}

fn has_anno(function: &Function, param: usize, name: &str) -> bool {
    function.param_decls[param]
        .annotations
        .iter()
        .any(|annotation| annotation.name.name.as_str() == name)
}

fn entry_param_roles(
    ws: &Workspace,
    function: &Function,
    body: &MirFunc,
    iface: &ShaderInterfacePlan,
    abi: &WgslAbiPlan,
    span: Span,
) -> Result<HashMap<Idx<skull_mir::mir::Cell>, ParamRole>, CompilerError> {
    if function.param_decls.len() != body.params.len() {
        return Err(err(
            span,
            "WGSL entry declaration and IR parameter populations disagree",
        ));
    }
    let mut roles = HashMap::new();
    let mut uv_count = 0;
    for (index, cell) in body.params.iter().enumerate() {
        let param = &function.param_decls[index];
        let role = if has_anno(function, index, "textures") {
            let name = nominal_name(ws, param.ty).ok_or_else(|| {
                err(
                    param_span(function, index),
                    "WGSL texture parameter is not a named group",
                )
            })?;
            let group_name = iface.textures.as_ref().and_then(|group| group.struct_name);
            if group_name != Some(name) {
                return Err(err(
                    param_span(function, index),
                    format!(
                        "WGSL texture parameter `{}` is absent from the interface plan",
                        param.name
                    ),
                ));
            }
            ParamRole::Texture
        } else if has_anno(function, index, "engine") {
            check_uniform_param(
                ws,
                param.ty,
                UniformRole::Engine,
                iface,
                param_span(function, index),
            )?;
            ParamRole::Uniform(UniformRole::Engine)
        } else if has_anno(function, index, "uniform") {
            check_uniform_param(
                ws,
                param.ty,
                UniformRole::Stage(ShaderStage::Fragment),
                iface,
                param_span(function, index),
            )?;
            ParamRole::Uniform(UniformRole::Stage(ShaderStage::Fragment))
        } else if value_kind_for_type(ws, param.ty) == Some(ValueKind::Vec2) {
            uv_count += 1;
            ParamRole::TexCoord
        } else {
            return Err(err(
                param_span(function, index),
                format!(
                    "WGSL fullscreen parameter `{}` is not a texture group, uniform group, or Vec2 UV",
                    param.name
                ),
            ));
        };
        roles.insert(*cell, role);
    }
    if uv_count != 1 {
        return Err(err(
            span,
            format!("WGSL fullscreen entry has {uv_count} UV parameters; exactly one is required"),
        ));
    }
    let _ = abi;
    Ok(roles)
}

fn explicit_entry_param_roles(
    ws: &Workspace,
    function: &Function,
    body: &MirFunc,
    iface: &ShaderInterfacePlan,
    stage: ShaderStage,
    span: Span,
) -> Result<HashMap<Idx<skull_mir::mir::Cell>, ParamRole>, CompilerError> {
    if function.param_decls.len() != body.params.len() {
        return Err(err(
            span,
            "WGSL explicit entry declaration and IR parameter populations disagree",
        ));
    }
    let mut roles = HashMap::new();
    for (index, cell) in body.params.iter().enumerate() {
        let param = &function.param_decls[index];
        let role = if stage == ShaderStage::Vertex && has_anno(function, index, "attr") {
            if iface.vertex_inputs.len() != 1
                || iface.vertex_inputs[0].struct_name != nominal_name(ws, param.ty).unwrap_or_default()
            {
                return Err(err(span, "WGSL explicit attribute parameter is absent from the interface plan"));
            }
            ParamRole::Attribute
        } else if has_anno(function, index, "engine") {
            check_uniform_param(ws, param.ty, UniformRole::Engine, iface, param_span(function, index))?;
            ParamRole::Uniform(UniformRole::Engine)
        } else if stage == ShaderStage::Fragment && index == 0 {
            if nominal_name(ws, param.ty).is_none() {
                return Err(err(span, "WGSL explicit fragment entry does not take a varying struct"));
            }
            ParamRole::Varying
        } else if stage == ShaderStage::Fragment && has_anno(function, index, "textures") {
            let name = nominal_name(ws, param.ty).ok_or_else(|| {
                err(param_span(function, index), "WGSL texture parameter is not a named group")
            })?;
            if iface.textures.as_ref().and_then(|group| group.struct_name) != Some(name) {
                return Err(err(span, "WGSL explicit texture parameter is absent from the interface plan"));
            }
            ParamRole::Texture
        } else {
            return Err(err(
                param_span(function, index),
                format!(
                    "WGSL explicit parameter `{}` has no admitted attribute, engine, varying, or texture role",
                    param.name
                ),
            ));
        };
        roles.insert(*cell, role);
    }
    Ok(roles)
}

fn param_span(function: &Function, _index: usize) -> Span {
    function.name_span
}

fn check_uniform_param(
    ws: &Workspace,
    ty: TyId,
    role: UniformRole,
    iface: &ShaderInterfacePlan,
    span: Span,
) -> Result<(), CompilerError> {
    let name = nominal_name(ws, ty)
        .ok_or_else(|| err(span, "WGSL uniform parameter is not a named group"))?;
    let Some(group) = iface.uniform_group(role) else {
        return Err(err(
            span,
            format!("WGSL uniform group `{name}` is absent from the interface plan"),
        ));
    };
    if group.struct_name != name {
        return Err(err(
            span,
            format!(
                "WGSL uniform parameter `{name}` does not match interface group `{}`",
                group.struct_name
            ),
        ));
    }
    Ok(())
}

struct FunctionEmitter<'a> {
    ws: &'a Workspace,
    stage: &'a ShaderStageIr,
    instance: &'a ShaderInstance,
    body: &'a MirFunc,
    abi: &'a WgslAbiPlan,
    param_roles: HashMap<Idx<skull_mir::mir::Cell>, ParamRole>,
    cell_names: HashMap<Idx<skull_mir::mir::Cell>, String>,
    cell_kinds: HashMap<Idx<skull_mir::mir::Cell>, ValueKind>,
    span: Span,
}

impl<'a> FunctionEmitter<'a> {
    fn new(
        ws: &'a Workspace,
        stage: &'a ShaderStageIr,
        instance: &'a ShaderInstance,
        body: &'a MirFunc,
        abi: &'a WgslAbiPlan,
        param_roles: HashMap<Idx<skull_mir::mir::Cell>, ParamRole>,
        span: Span,
    ) -> Result<Self, CompilerError> {
        let mut cell_names = HashMap::new();
        let mut cell_kinds = HashMap::new();
        for (cell, data) in body.cells.iter() {
            cell_names.insert(
                cell,
                data.name.map_or_else(
                    || format!("cell_{}", cell.into_raw().into_u32()),
                    |name| name.to_string(),
                ),
            );
            if let Some(ty) = data.ty.and_then(|ty| value_kind_for_type(ws, ty)) {
                cell_kinds.insert(cell, ty);
            }
        }
        let mut emitter = Self {
            ws,
            stage,
            instance,
            body,
            abi,
            param_roles,
            cell_names,
            cell_kinds,
            span,
        };
        emitter.infer_local_kinds()?;
        Ok(emitter)
    }

    fn infer_local_kinds(&mut self) -> Result<(), CompilerError> {
        for (_, block) in self.body.blocks.iter() {
            for iid in &block.insts {
                if let Inst::Store { cell, value } = self.body.insts[*iid] {
                    let kind = self.value_kind(value)?;
                    if let Some(old) = self.cell_kinds.insert(cell, kind) {
                        if old != kind {
                            return Err(err(
                                self.span,
                                format!("WGSL cell `{}` changes type", self.cell_names[&cell]),
                            ));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn emit_function(self, function: &Function, entry: bool) -> Result<String, CompilerError> {
        if self.body.blocks.len() != 1 {
            return Err(err(
                self.span,
                format!(
                    "WGSL function `{}` uses control flow outside the admitted straight-line slice",
                    function.name
                ),
            ));
        }
        let block = &self.body.blocks[self.body.entry];
        let Term::Return(values) = &block.term else {
            return Err(err(
                self.span,
                format!("WGSL function `{}` does not end in a return", function.name),
            ));
        };
        if values.len() != 1 {
            return Err(err(
                self.span,
                format!("WGSL function `{}` has a multi-value return", function.name),
            ));
        }
        let return_kind = self.value_kind(values[0])?;
        let return_ty = wgsl_value_type(return_kind).ok_or_else(|| {
            err(
                self.span,
                format!(
                    "WGSL function `{}` returns an unsupported value",
                    function.name
                ),
            )
        })?;
        let name = if entry {
            self.instance.symbol(self.ws)
        } else {
            self.instance.symbol(self.ws)
        };
        let param = if entry && self.param_roles.values().any(|role| matches!(role, ParamRole::TexCoord)) {
            "uv: vec2<f32>".to_string()
        } else {
            self.body
                .params
                .iter()
                .filter(|cell| {
                    !entry
                        || !matches!(
                            self.param_roles.get(cell),
                            Some(ParamRole::Texture | ParamRole::Uniform(_))
                        )
                })
                .map(|cell| {
                    let kind = self.cell_kinds.get(cell).copied().ok_or_else(|| {
                        err(
                            self.span,
                            format!(
                                "WGSL parameter `{}` has no neutral type",
                                self.cell_names[cell]
                            ),
                        )
                    })?;
                    let ty = wgsl_value_type(kind).ok_or_else(|| {
                        err(
                            self.span,
                            format!(
                                "WGSL parameter `{}` is not a value parameter",
                                self.cell_names[cell]
                            ),
                        )
                    })?;
                    Ok(format!("{}: {}", self.cell_names[cell], ty))
                })
                .collect::<Result<Vec<_>, CompilerError>>()?
                .join(", ")
        };

        let mut out = String::new();
        writeln!(out, "fn {}({}) -> {} {{", name, param, return_ty).unwrap();
        let parameters: HashSet<_> = self.body.params.iter().copied().collect();
        for (cell, _) in self.body.cells.iter() {
            let Some(kind) = self.cell_kinds.get(&cell).copied() else {
                continue;
            };
            if parameters.contains(&cell) || !self.is_stored_cell(cell) {
                continue;
            }
            writeln!(
                out,
                "  var {}: {} = {};",
                self.cell_names[&cell],
                wgsl_value_type(kind).unwrap(),
                zero_value(kind)
            )
            .unwrap();
        }
        for iid in &block.insts {
            match &self.body.insts[*iid] {
                Inst::Store { cell, value } => {
                    let rhs = self.render_val(*value)?;
                    writeln!(out, "  {} = {};", self.cell_names[cell], rhs).unwrap();
                }
                Inst::Call { func, args } => {
                    let target = &self.ws.module(func.module).functions[func.func];
                    if target.return_ty.is_none() {
                        writeln!(out, "  {};", self.render_call(*iid, *func, args)?).unwrap();
                    }
                }
                Inst::Const(_)
                | Inst::Load(_)
                | Inst::Field { .. }
                | Inst::MakeValueStruct { .. }
                | Inst::MaterializeValueStruct { .. }
                | Inst::IntAdd(..)
                | Inst::IntSub(..)
                | Inst::IntMul(..)
                | Inst::IntDiv(..)
                | Inst::IntMod(..)
                | Inst::FloatAdd(..)
                | Inst::FloatSub(..)
                | Inst::FloatMul(..)
                | Inst::FloatDiv(..)
                | Inst::FloatMod(..)
                | Inst::IntLt(..)
                | Inst::IntLeq(..)
                | Inst::IntGt(..)
                | Inst::IntGeq(..)
                | Inst::FloatLt(..)
                | Inst::FloatLeq(..)
                | Inst::FloatGt(..)
                | Inst::FloatGeq(..)
                | Inst::Eq(..)
                | Inst::NotEq(..)
                | Inst::FloatToInt(_)
                | Inst::IntToFloat(_)
                | Inst::Neg(_)
                | Inst::Not(_) => {}
                other => {
                    return Err(err(
                        self.span,
                        format!(
                            "WGSL function `{}` reaches unsupported MIR operation `{}`",
                            function.name,
                            mir_kind(other)
                        ),
                    ));
                }
            }
        }
        writeln!(out, "  return {};", self.render_val(values[0])?).unwrap();
        out.push_str("}\n\n");
        Ok(out)
    }

    fn is_stored_cell(&self, cell: Idx<skull_mir::mir::Cell>) -> bool {
        self.body.blocks.iter().any(|(_, block)| block.insts.iter().any(|iid| matches!(self.body.insts[*iid], Inst::Store { cell: stored, .. } if stored == cell)))
    }

    fn value_kind(&self, iid: Idx<skull_mir::mir::Inst>) -> Result<ValueKind, CompilerError> {
        match &self.body.insts[iid] {
            Inst::Const(Lit::Float(_)) => Ok(ValueKind::F32),
            Inst::Const(Lit::Int(_)) => Ok(ValueKind::I32),
            Inst::Const(Lit::Bool(_)) => Ok(ValueKind::Bool),
            Inst::Const(Lit::Nil) | Inst::Const(Lit::Str(_)) => Err(err(
                self.span,
                "WGSL fullscreen emission does not support null or string literals",
            )),
            Inst::Load(cell) => self.cell_kinds.get(cell).copied().ok_or_else(|| {
                err(
                    self.span,
                    format!(
                        "WGSL load of `{}` has no neutral type",
                        self.cell_names[cell]
                    ),
                )
            }),
            Inst::Field { base, field } => self.field_kind(*base, *field),
            Inst::MakeValueStruct { name, .. } | Inst::MaterializeValueStruct { name, .. } => {
                value_struct_kind(*name).ok_or_else(|| {
                    err(
                        self.span,
                        format!("WGSL does not support value struct `{name}`"),
                    )
                })
            }
            Inst::Call { func, args } => self.call_kind(iid, *func, args),
            Inst::IntAdd(..)
            | Inst::IntSub(..)
            | Inst::IntMul(..)
            | Inst::IntDiv(..)
            | Inst::IntMod(..) => Ok(ValueKind::I32),
            Inst::FloatAdd(..)
            | Inst::FloatSub(..)
            | Inst::FloatMul(..)
            | Inst::FloatDiv(..)
            | Inst::FloatMod(..) => Ok(ValueKind::F32),
            Inst::IntLt(..)
            | Inst::IntLeq(..)
            | Inst::IntGt(..)
            | Inst::IntGeq(..)
            | Inst::FloatLt(..)
            | Inst::FloatLeq(..)
            | Inst::FloatGt(..)
            | Inst::FloatGeq(..)
            | Inst::Eq(..)
            | Inst::NotEq(..)
            | Inst::Not(_) => Ok(ValueKind::Bool),
            Inst::FloatToInt(_) => Ok(ValueKind::I32),
            Inst::IntToFloat(_) => Ok(ValueKind::F32),
            Inst::Neg(value) => self.value_kind(*value),
            other => Err(err(
                self.span,
                format!(
                    "WGSL reaches unsupported value operation `{}`",
                    mir_kind(other)
                ),
            )),
        }
    }

    fn field_kind(
        &self,
        base: Idx<skull_mir::mir::Inst>,
        field: Ustr,
    ) -> Result<ValueKind, CompilerError> {
        if let Inst::Load(cell) = self.body.insts[base] {
            match self.param_roles.get(&cell).copied() {
                Some(ParamRole::Texture) => {
                    let texture = self
                        .abi
                        .textures
                        .iter()
                        .find(|texture| texture.name == field)
                        .ok_or_else(|| {
                            err(
                                self.span,
                                format!("WGSL texture field `{field}` is absent from the ABI plan"),
                            )
                        })?;
                    return Ok(ValueKind::Sampler(texture.kind));
                }
                Some(ParamRole::Uniform(role)) => {
                    let block = self.abi.uniform_block(role).ok_or_else(|| {
                        err(
                            self.span,
                            format!("WGSL uniform role for `{field}` is absent from the ABI plan"),
                        )
                    })?;
                    let member = block
                        .members
                        .iter()
                        .find(|member| member.name == field)
                        .ok_or_else(|| {
                            err(
                                self.span,
                                format!("WGSL uniform field `{field}` is absent from the ABI plan"),
                            )
                        })?;
                    return wire_value_kind(member.kind).ok_or_else(|| {
                        err(
                            self.span,
                            format!("WGSL uniform field `{field}` is not an emitted value"),
                        )
                    });
                }
                Some(ParamRole::Attribute) | Some(ParamRole::Varying) | Some(ParamRole::TexCoord) => {
                    if let Some(ty) = self.body.cells[cell].ty {
                        if let Some(kind) = struct_field_kind(self.ws, ty, field) {
                            return Ok(kind);
                        }
                    }
                }
                None => {
                    if let Some(ty) = self.body.cells[cell].ty {
                        if let Some(kind) = struct_field_kind(self.ws, ty, field) {
                            return Ok(kind);
                        }
                    }
                }
            }
        }
        match self.value_kind(base)? {
            ValueKind::Vec2 | ValueKind::Vec3 | ValueKind::Vec4
                if matches!(field.as_str(), "x" | "y" | "z" | "w") =>
            {
                Ok(ValueKind::F32)
            }
            ValueKind::Struct(_) => Err(err(
                self.span,
                format!("WGSL field `{field}` is not present in the compiler-owned value struct"),
            )),
            _ => Err(err(
                self.span,
                format!("WGSL field `{field}` is not a supported projection"),
            )),
        }
    }

    fn call_kind(
        &self,
        iid: Idx<skull_mir::mir::Inst>,
        func: skull_mir::mir::FuncRef,
        args: &[Idx<skull_mir::mir::Inst>],
    ) -> Result<ValueKind, CompilerError> {
        let function = &self.ws.module(func.module).functions[func.func];
        match WgslIntrinsic::classify(function) {
            Some(WgslIntrinsic::Texture) => Ok(ValueKind::Vec4),
            Some(WgslIntrinsic::VectorConstructor) => {
                let total: usize = args
                    .iter()
                    .map(|arg| self.value_kind(*arg).map(ValueKind::components))
                    .try_fold(0usize, |sum, n| n.map(|n| sum + n))?;
                match total {
                    2 => Ok(ValueKind::Vec2),
                    3 => Ok(ValueKind::Vec3),
                    4 => Ok(ValueKind::Vec4),
                    _ => Err(err(
                        self.span,
                        format!("WGSL v(...) has unsupported component count {total}"),
                    )),
                }
            }
            Some(WgslIntrinsic::Builtin(_)) => self.value_kind(
                args.first()
                    .copied()
                    .ok_or_else(|| err(self.span, "WGSL intrinsic call has no operand"))?,
            ),
            Some(WgslIntrinsic::Operator(_)) => self.value_kind(
                args.first()
                    .copied()
                    .ok_or_else(|| err(self.span, "WGSL operator call has no operand"))?,
            ),
            Some(WgslIntrinsic::UnsupportedOperator) => Err(err(
                self.span,
                format!("WGSL operator `{}` is not admitted", function.name),
            )),
            _ => {
                let instance = self.resolve_callee(iid, func)?;
                let called = self
                    .stage
                    .order
                    .iter()
                    .find(|candidate| **candidate == instance)
                    .ok_or_else(|| {
                        err(
                            self.span,
                            format!(
                                "WGSL call to `{}` has no emitted helper body",
                                function.name
                            ),
                        )
                    })?;
                let called_function = function_for_instance(self.ws, called);
                called_function
                    .return_ty
                    .and_then(|ty| value_kind_for_type(self.ws, ty))
                    .ok_or_else(|| {
                        err(
                            self.span,
                            format!(
                                "WGSL helper `{}` has no supported return type",
                                called_function.name
                            ),
                        )
                    })
            }
        }
    }

    fn resolve_callee(
        &self,
        iid: Idx<skull_mir::mir::Inst>,
        func: skull_mir::mir::FuncRef,
    ) -> Result<ShaderInstance, CompilerError> {
        let function = &self.ws.module(func.module).functions[func.func];
        let key = self
            .body
            .spec_bindings
            .get(&iid)
            .cloned()
            .unwrap_or_default();
        if let Some(instance) = self.stage.order.iter().find(|instance| {
            instance.fref == func && (instance.key == key || function.generics.is_empty())
        }) {
            return Ok(instance.clone());
        }
        Err(err(
            self.span,
            format!(
                "WGSL call to `{}` has no concrete emitted instance",
                function.name
            ),
        ))
    }

    fn render_val(&self, iid: Idx<skull_mir::mir::Inst>) -> Result<String, CompilerError> {
        match &self.body.insts[iid] {
            Inst::Const(Lit::Float(value)) => Ok(format_float(*value)),
            Inst::Const(Lit::Int(value)) => Ok(value.to_string()),
            Inst::Const(Lit::Bool(value)) => Ok(value.to_string()),
            Inst::Const(Lit::Nil) | Inst::Const(Lit::Str(_)) => Err(err(
                self.span,
                "WGSL fullscreen emission does not support null or string literals",
            )),
            Inst::Load(cell) => match self.param_roles.get(cell).copied() {
                Some(ParamRole::TexCoord) => Ok("uv".to_string()),
                Some(ParamRole::Texture) | Some(ParamRole::Uniform(_)) => Err(err(
                    self.span,
                    format!(
                        "WGSL resource cell `{}` must be accessed through a field",
                        self.cell_names[cell]
                    ),
                )),
                Some(ParamRole::Attribute) | Some(ParamRole::Varying) => {
                    Ok(self.cell_names[cell].clone())
                }
                None => Ok(self.cell_names[cell].clone()),
            },
            Inst::Field { base, field } => {
                if let Inst::Load(cell) = self.body.insts[*base] {
                    match self.param_roles.get(&cell).copied() {
                        Some(ParamRole::Texture) => {
                            self.abi.textures.iter().find(|texture| texture.name == *field).ok_or_else(|| err(self.span, format!("WGSL texture field `{field}` is absent from the ABI plan")))?;
                            return Ok(format!("tex_{field}"));
                        }
                        Some(ParamRole::Uniform(role)) => {
                            let block = self.abi.uniform_block(role).ok_or_else(|| err(self.span, format!("WGSL uniform role for `{field}` is absent from the ABI plan")))?;
                            if !block.members.iter().any(|member| member.name == *field) {
                                return Err(err(
                                    self.span,
                                    format!(
                                        "WGSL uniform field `{field}` is absent from the ABI plan"
                                    ),
                                ));
                            }
                            return Ok(format!("u_{}.{}", block.struct_name, field));
                        }
                        Some(ParamRole::Attribute)
                        | Some(ParamRole::Varying)
                        | Some(ParamRole::TexCoord)
                        | None => {}
                    }
                }
                Ok(format!("{}.{}", self.render_val(*base)?, field))
            }
            Inst::MakeValueStruct { name, fields }
            | Inst::MaterializeValueStruct { name, fields } => {
                let kind = value_struct_kind(*name).ok_or_else(|| {
                    err(self.span, format!("WGSL does not support value struct `{name}`"))
                })?;
                let args = fields
                    .iter()
                    .map(|(_, value)| self.render_val(*value))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(format!("{}({})", wgsl_value_type(kind).unwrap(), args.join(", ")))
            }
            Inst::Call { func, args } => self.render_call(iid, *func, args),
            Inst::IntAdd(a, b) | Inst::FloatAdd(a, b) => self.binary(*a, "+", *b),
            Inst::IntSub(a, b) | Inst::FloatSub(a, b) => self.binary(*a, "-", *b),
            Inst::IntMul(a, b) | Inst::FloatMul(a, b) => self.binary(*a, "*", *b),
            Inst::IntDiv(a, b) | Inst::FloatDiv(a, b) => self.binary(*a, "/", *b),
            Inst::IntMod(a, b) | Inst::FloatMod(a, b) => self.binary(*a, "%", *b),
            Inst::IntLt(a, b) | Inst::FloatLt(a, b) => self.binary(*a, "<", *b),
            Inst::IntLeq(a, b) | Inst::FloatLeq(a, b) => self.binary(*a, "<=", *b),
            Inst::IntGt(a, b) | Inst::FloatGt(a, b) => self.binary(*a, ">", *b),
            Inst::IntGeq(a, b) | Inst::FloatGeq(a, b) => self.binary(*a, ">=", *b),
            Inst::Eq(a, b) => self.binary(*a, "==", *b),
            Inst::NotEq(a, b) => self.binary(*a, "!=", *b),
            Inst::FloatToInt(value) => Ok(format!("i32({})", self.render_val(*value)?)),
            Inst::IntToFloat(value) => Ok(format!("f32({})", self.render_val(*value)?)),
            Inst::Neg(value) => Ok(format!("(-{})", self.render_val(*value)?)),
            Inst::Not(value) => Ok(format!("(!{})", self.render_val(*value)?)),
            other => Err(err(
                self.span,
                format!(
                    "WGSL reaches unsupported value operation `{}`",
                    mir_kind(other)
                ),
            )),
        }
    }

    fn binary(
        &self,
        a: Idx<skull_mir::mir::Inst>,
        op: &str,
        b: Idx<skull_mir::mir::Inst>,
    ) -> Result<String, CompilerError> {
        Ok(format!(
            "({} {} {})",
            self.render_val(a)?,
            op,
            self.render_val(b)?
        ))
    }

    fn render_call(
        &self,
        iid: Idx<skull_mir::mir::Inst>,
        func: skull_mir::mir::FuncRef,
        args: &[Idx<skull_mir::mir::Inst>],
    ) -> Result<String, CompilerError> {
        let function = &self.ws.module(func.module).functions[func.func];
        let name = function.name.as_str();
        match WgslIntrinsic::classify(function) {
            Some(WgslIntrinsic::Texture) => {
                if args.len() != 2 {
                    return Err(err(
                        self.span,
                        "WGSL texture intrinsic requires one logical sampler and one UV",
                    ));
                }
                let texture = self.sampler_binding(args[0])?;
                if texture.kind != SamplerKind::Float {
                    return Err(err(
                        self.span,
                        "WGSL fullscreen emission does not yet admit integer texture sampling",
                    ));
                }
                Ok(format!(
                    "textureSample(tex_{0}, samp_{0}, {1})",
                    texture.name,
                    self.render_val(args[1])?
                ))
            }
            Some(WgslIntrinsic::VectorConstructor) => {
                let kind = self.call_kind(iid, func, args)?;
                let rendered = args
                    .iter()
                    .map(|arg| self.render_val(*arg))
                    .collect::<Result<Vec<_>, _>>()?;
            Ok(format!("{}({})", wgsl_value_type(kind).unwrap(), rendered.join(", ")))
            }
            Some(WgslIntrinsic::Builtin(builtin)) => {
                let target = self.value_kind(
                    args.first()
                        .copied()
                        .ok_or_else(|| err(self.span, "WGSL intrinsic call has no operand"))?,
                )?;
                let rendered = args
                    .iter()
                    .map(|arg| self.render_intrinsic_arg(*arg, target))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(format!("{}({})", builtin.name(), rendered.join(", ")))
            }
            Some(WgslIntrinsic::Operator(operator)) => {
                if args.len() != 2 {
                    return Err(err(
                        self.span,
                        format!("WGSL operator `{name}` requires two operands"),
                    ));
                }
                self.binary(args[0], operator.symbol(), args[1])
            }
            Some(WgslIntrinsic::UnsupportedOperator) => Err(err(
                self.span,
                format!("WGSL operator `{name}` is not admitted"),
            )),
            _ => {
                let instance = self.resolve_callee(iid, func)?;
                let self_name = instance.symbol(self.ws);
                let rendered = args
                    .iter()
                    .map(|arg| self.render_val(*arg))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(format!("{}({})", self_name, rendered.join(", ")))
            }
        }
    }

    fn render_intrinsic_arg(
        &self,
        arg: Idx<skull_mir::mir::Inst>,
        target: ValueKind,
    ) -> Result<String, CompilerError> {
        let rendered = self.render_val(arg)?;
        if target.components() > 1 && self.value_kind(arg)? == ValueKind::F32 {
            Ok(format!("{}({})", wgsl_value_type(target).unwrap(), rendered))
        } else {
            Ok(rendered)
        }
    }

    fn sampler_binding(
        &self,
        iid: Idx<skull_mir::mir::Inst>,
    ) -> Result<&WgslTextureBinding, CompilerError> {
        let Inst::Field { base, field } = &self.body.insts[iid] else {
            return Err(err(
                self.span,
                "WGSL texture intrinsic did not receive a logical texture field",
            ));
        };
        let Inst::Load(cell) = self.body.insts[*base] else {
            return Err(err(
                self.span,
                "WGSL texture field base is not the compiler-owned texture group",
            ));
        };
        if !matches!(self.param_roles.get(&cell), Some(ParamRole::Texture)) {
            return Err(err(
                self.span,
                "WGSL texture field base is not the compiler-owned texture group",
            ));
        }
        self.abi
            .textures
            .iter()
            .find(|texture| texture.name == *field)
            .ok_or_else(|| {
                err(
                    self.span,
                    format!("WGSL texture slot `{field}` is absent from the ABI plan"),
                )
            })
    }
}

fn value_struct_kind(name: Ustr) -> Option<ValueKind> {
    match name.as_str() {
        "Vec2" => Some(ValueKind::Vec2),
        "Vec3" => Some(ValueKind::Vec3),
        "Vec4" => Some(ValueKind::Vec4),
        "Mat4" => Some(ValueKind::Mat4),
        _ => Some(ValueKind::Struct(name)),
    }
}

fn zero_value(kind: ValueKind) -> String {
    match kind {
        ValueKind::F32 => "0.0".into(),
        ValueKind::I32 => "0".into(),
        ValueKind::Bool => "false".into(),
        ValueKind::Vec2 => "vec2<f32>(0.0)".into(),
        ValueKind::Vec3 => "vec3<f32>(0.0)".into(),
        ValueKind::Vec4 => "vec4<f32>(0.0)".into(),
        ValueKind::Mat4 => "mat4x4<f32>(0.0)".into(),
        ValueKind::Struct(name) => format!("{}()", name),
        ValueKind::Sampler(_) => "0.0".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::{WgslIntrinsic, WgslOperator};

    #[test]
    fn intrinsic_classifier_matches_the_admitted_wgsl_surface() {
        assert!(WgslIntrinsic::classify_name("texture").is_some_and(WgslIntrinsic::is_admitted));
        assert!(WgslIntrinsic::classify_name("v").is_some_and(WgslIntrinsic::is_admitted));
        assert!(WgslIntrinsic::classify_name("clamp").is_some_and(WgslIntrinsic::is_admitted));
        assert!(WgslIntrinsic::classify_name("__op_add").is_some_and(WgslIntrinsic::is_admitted));
        assert_eq!(
            WgslIntrinsic::classify_name("__op_pow"),
            Some(WgslIntrinsic::UnsupportedOperator)
        );
        assert_eq!(
            WgslIntrinsic::classify_name("__op_add"),
            Some(WgslIntrinsic::Operator(WgslOperator::Add))
        );
        assert_eq!(WgslIntrinsic::classify_name("helper"), None);
    }
}

fn format_float(value: f64) -> String {
    let mut text = format!("{value}");
    if !text.contains('.') && !text.contains('e') && !text.contains('E') {
        text.push_str(".0");
    }
    text
}

fn mir_kind(inst: &Inst) -> &'static str {
    match inst {
        Inst::Global(_) => "global",
        Inst::CallMulti { .. }
        | Inst::CallValue { .. }
        | Inst::CallValueMulti { .. }
        | Inst::CallBoundMulti { .. } => "multi-value call",
        Inst::MakeTuple(_) | Inst::MakeVec(_) | Inst::MakeVecColumns { .. } => "aggregate",
        Inst::MakeStruct { .. } | Inst::MakeEnum { .. } | Inst::MakeEnumDyn { .. } => {
            "heap aggregate"
        }
        Inst::Index { .. } | Inst::Len { .. } | Inst::VecCheckedIndex { .. } => {
            "container operation"
        }
        Inst::StoreField { .. } | Inst::StoreIndex { .. } | Inst::StoreGlobal { .. } => "mutation",
        Inst::Lambda { .. } | Inst::MakeEnv { .. } => "closure",
        Inst::FfiNewArray { .. } | Inst::FfiViewCast { .. } => "FFI operation",
        Inst::BitAnd(..)
        | Inst::BitOr(..)
        | Inst::BitXor(..)
        | Inst::Shl(..)
        | Inst::Shr(..)
        | Inst::BitNot(_) => "bitwise operation",
        Inst::CoroSetInner(_)
        | Inst::CoroHook { .. }
        | Inst::MapNext { .. }
        | Inst::ArrayPush { .. } => "coroutine/iteration operation",
        _ => "unsupported operation",
    }
}
