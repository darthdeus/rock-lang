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
use skull_codegen_shader::structure::{LoopRegion, Structure};
use skull_codegen_shader::{ShaderArtifactPlan, ShaderPlannerPolicy};
use skull_mir::gpu_layout::{AttrKind, SamplerKind, WireKind};
use skull_mir::mir::{Block, Cell, Inst, Lit, MirFunc, Term};
use skull_mir::monoty::MonoInterner;
use skull_mir::shader_ir::{
    ComputeAccess, ComputeAddressSpace, ComputeBuiltin, ComputeDispatchContract,
    ComputeInterfacePlan, ComputeResourceShape, ComputeValueKind, FragmentOutput, LogicalArrayGroup,
    LogicalComputeWorkgroup, LogicalTextureGroup, LogicalUniformGroup, ShaderIR, ShaderInstance,
    ShaderInterfacePlan,
    ShaderStage, ShaderStageIr, UniformRole, VertexRate,
};
use skull_parser::{CompilerError, CompilerErrorKind};
use skull_wgsl_abi::{
    WgslArtifactKind as SharedWgslArtifactKind, WgslComputeInfo as SharedWgslComputeInfo,
    WgslAbi as SharedWgslAbi, WgslArrayBlock as SharedWgslArrayBlock,
    WgslArrayMember as SharedWgslArrayMember, WgslFragmentIo as SharedWgslFragmentIo,
    WgslFragmentOutput as SharedWgslFragmentOutput, WgslFullscreenVertex as SharedWgslFullscreenVertex,
    WgslGeometryKind as SharedWgslGeometryKind, WgslPrimitiveTopology as SharedWgslPrimitiveTopology,
    WgslTextureBinding as SharedWgslTextureBinding, WgslTextureKind as SharedWgslTextureKind,
    WgslComputeBuiltin as SharedWgslComputeBuiltin,
    WgslComputeResource as SharedWgslComputeResource,
    WgslStorageAccess as SharedWgslStorageAccess,
    WgslStorageElementKind as SharedWgslStorageElementKind,
    WgslUniformBlock as SharedWgslUniformBlock, WgslUniformMember as SharedWgslUniformMember,
    WgslVertexAttribute as SharedWgslVertexAttribute, WgslVertexInput as SharedWgslVertexInput,
    WgslVarying as SharedWgslVarying, WgslVisibility as SharedWgslVisibility,
};
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use ustr::Ustr;

pub const WGSL_BIND_GROUP: u32 = 0;
pub const FULLSCREEN_VARYING_LOCATION: u32 = 0;
pub const FRAGMENT_OUTPUT_LOCATION: u32 = 0;
/// The name the fragment entry receives `@builtin(position)` under. WGSL has no
/// global fragment position, so `frag_coord()` resolves to this parameter.
pub const FRAG_COORD_PARAM: &str = "frag_coord_in";

fn err(span: Span, message: impl Into<String>) -> CompilerError {
    CompilerError::new_with_label(span, message.into())
}

fn unsupported_err(
    span: Span,
    capability: impl Into<String>,
    message: impl Into<String>,
) -> CompilerError {
    err(span, message).with_kind(CompilerErrorKind::UnsupportedWgslProjection {
        capability: capability.into(),
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WgslOperator {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

impl WgslOperator {
    fn symbol(self) -> &'static str {
        match self {
            Self::Add => "+",
            Self::Sub => "-",
            Self::Mul => "*",
            Self::Div => "/",
            Self::Mod => "%",
        }
    }
}

/// What an admitted builtin call evaluates to. `FirstArg` is the elementwise
/// shape (`clamp`, `min`, `pow`); the rest name their own result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResultKind {
    FirstArg,
    Fixed(ValueKind),
}

/// The WGSL tier-0 builtin table: one row per (name, arity) -> WGSL spelling and
/// result shape. This is the twin of `GLSL_INTRINSICS`, and it is deliberately
/// NOT a superset: a GLSL row with no faithful WGSL spelling (`isnan`, `bvec3`,
/// `vertex_id`) is absent so the call REJECTS rather than emitting something
/// that merely parses.
const WGSL_BUILTINS: &[(&str, usize, &str, ResultKind)] = &[
    ("sin", 1, "sin", ResultKind::FirstArg),
    ("cos", 1, "cos", ResultKind::FirstArg),
    ("tan", 1, "tan", ResultKind::FirstArg),
    ("sqrt", 1, "sqrt", ResultKind::FirstArg),
    ("exp", 1, "exp", ResultKind::FirstArg),
    ("abs", 1, "abs", ResultKind::FirstArg),
    ("floor", 1, "floor", ResultKind::FirstArg),
    ("ceil", 1, "ceil", ResultKind::FirstArg),
    ("fract", 1, "fract", ResultKind::FirstArg),
    ("sign", 1, "sign", ResultKind::FirstArg),
    ("fwidth", 1, "fwidth", ResultKind::FirstArg),
    ("pow", 2, "pow", ResultKind::FirstArg),
    ("atan2", 2, "atan2", ResultKind::FirstArg),
    ("min", 2, "min", ResultKind::FirstArg),
    ("max", 2, "max", ResultKind::FirstArg),
    ("clamp", 3, "clamp", ResultKind::FirstArg),
    ("lerp", 3, "mix", ResultKind::FirstArg),
    ("smoothstep", 3, "smoothstep", ResultKind::FirstArg),
    ("dot", 2, "dot", ResultKind::Fixed(ValueKind::F32)),
    ("length", 1, "length", ResultKind::Fixed(ValueKind::F32)),
    ("distance", 2, "distance", ResultKind::Fixed(ValueKind::F32)),
    ("int_to_float", 1, "f32", ResultKind::Fixed(ValueKind::F32)),
    ("float", 1, "f32", ResultKind::Fixed(ValueKind::F32)),
    ("to_float", 1, "f32", ResultKind::Fixed(ValueKind::F32)),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WgslIntrinsic {
    GlobalInvocationId,
    LocalInvocationId,
    WorkgroupId,
    LocalInvocationIndex,
    NumWorkgroups,
    StorageLoad,
    StorageStore,
    WorkgroupArray,
    WorkgroupLoad,
    WorkgroupStore,
    AtomicAdd,
    WorkgroupBarrier,
    VertexIndex,
    Texture,
    TextureSize,
    At,
    FragCoord,
    Discard,
    VectorConstructor,
    Zero,
    /// GLSL pins `round` to `floor(x + 0.5)` because half-way rounding is
    /// implementation-defined; WGSL's `round` is round-half-to-even, so the same
    /// pin is emitted here rather than trusting the two to agree.
    Round,
    Builtin(&'static str, ResultKind),
    Operator(WgslOperator),
    UnsupportedOperator,
}

impl WgslIntrinsic {
    fn classify(function: &Function) -> Option<Self> {
        let arity = function.param_decls.len();
        let intrinsic = Self::classify_name_arity(function.name.as_str(), arity);
        if matches!(intrinsic, Some(Self::Operator(_))) && arity != 2 {
            return Some(Self::UnsupportedOperator);
        }
        intrinsic
    }

    fn classify_name_arity(name: &str, arity: usize) -> Option<Self> {
        if let Some((_, _, spelling, result)) = WGSL_BUILTINS
            .iter()
            .find(|(candidate, ar, _, _)| *candidate == name && *ar == arity)
        {
            return Some(Self::Builtin(spelling, *result));
        }
        Some(match (name, arity) {
            ("global_invocation_id", 0) => Self::GlobalInvocationId,
            ("local_invocation_id", 0) => Self::LocalInvocationId,
            ("workgroup_id", 0) => Self::WorkgroupId,
            ("local_invocation_index", 0) => Self::LocalInvocationIndex,
            ("num_workgroups", 0) => Self::NumWorkgroups,
            ("storage_load", 2) => Self::StorageLoad,
            ("storage_store", 3) => Self::StorageStore,
            ("workgroup_array", 1) => Self::WorkgroupArray,
            ("workgroup_load", 2) => Self::WorkgroupLoad,
            ("workgroup_store", 3) => Self::WorkgroupStore,
            ("atomic_add", 3) => Self::AtomicAdd,
            ("workgroup_barrier", 0) => Self::WorkgroupBarrier,
            ("vertex_id", 0) => Self::VertexIndex,
            ("round", 1) => Self::Round,
            ("v", 0..=4) => Self::VectorConstructor,
            ("zero", 0) => Self::Zero,
            ("texture", 2) => Self::Texture,
            ("texture_size", 1) => Self::TextureSize,
            ("frag_coord", 0) => Self::FragCoord,
            ("discard", 0) => Self::Discard,
            ("at", 2) => Self::At,
            ("__op_add", _) => Self::Operator(WgslOperator::Add),
            ("__op_sub", _) => Self::Operator(WgslOperator::Sub),
            ("__op_mul", _) => Self::Operator(WgslOperator::Mul),
            ("__op_div", _) => Self::Operator(WgslOperator::Div),
            ("__op_idiv", _) => Self::Operator(WgslOperator::Div),
            ("__op_mod", _) => Self::Operator(WgslOperator::Mod),
            (name, _) if name.starts_with("__op_") => Self::UnsupportedOperator,
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

    fn unsupported_capability(&self, function: &Function) -> Option<String> {
        match WgslIntrinsic::classify(function) {
            Some(WgslIntrinsic::UnsupportedOperator) => {
                Some(format!("unsupported WGSL operator `{}`", function.name))
            }
            Some(_) => None,
            None => match function.name.as_str() {
                "isnan" | "isinf" => Some("non-finite predicate intrinsic".to_string()),
                "any" => Some("boolean-vector reduction intrinsic".to_string()),
                "texture_id" => Some("integer texture sampling intrinsic".to_string()),
                "greater_than" => Some("boolean-vector comparison intrinsic".to_string()),
                "bvec3" => Some("boolean-vector constructor".to_string()),
                "to_vec3" => Some("boolean-vector conversion intrinsic".to_string()),
                _ => None,
            },
        }
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

/// The fragment input locations for the admitted fullscreen shape. The colour
/// outputs live in [`WgslAbiPlan::fragment_outputs`], which is one list for the
/// single and MRT shapes alike.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WgslFragmentIo {
    pub uv_location: u32,
}

/// One `@location(n)` colour output of the fragment stage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WgslFragmentOutput {
    pub name: Ustr,
    pub location: u32,
    pub kind: WireKind,
}

/// One `UniformArray<T>` member in a WGSL array block.
///
/// `stride` is the std140 stride the CPU writer already fills, and WGSL's
/// uniform address space demands the same 16: an `array<f32, N>` is illegal
/// there, so every element is declared as a 4-component vector and read through
/// [`array_element_projection`]. The offset comes from the shared layout in
/// `skull_mir::gpu_layout`, not from a second computation here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WgslArrayMember {
    pub name: Ustr,
    pub elem: WireKind,
    pub capacity: u32,
    pub offset: u32,
    pub stride: u32,
}

/// One stage's `UniformArray<T>` params at their own binding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WgslArrayBlock {
    pub stage: ShaderStage,
    pub logical_name: Ustr,
    pub struct_name: Ustr,
    pub binding: u32,
    pub size: u32,
    pub members: Vec<WgslArrayMember>,
    pub visibility: WgslStageVisibility,
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
    pub artifact: SharedWgslArtifactKind,
    pub semantic_fingerprint: u64,
    pub bind_group: u32,
    pub geometry: WgslGeometryKind,
    pub vertex_entry: String,
    pub fragment_entry: String,
    pub vertex_inputs: Vec<WgslVertexInput>,
    pub varyings: Vec<WgslVarying>,
    pub fullscreen_vertex: WgslFullscreenVertex,
    pub fragment: WgslFragmentIo,
    pub fragment_outputs: Vec<WgslFragmentOutput>,
    pub fullscreen_uniform: WgslUniformBlock,
    pub uniform_blocks: Vec<WgslUniformBlock>,
    pub array_blocks: Vec<WgslArrayBlock>,
    pub textures: Vec<WgslTextureBinding>,
    pub compute: Option<SharedWgslComputeInfo>,
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
            artifact: SharedWgslArtifactKind::Graphics,
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
            },
            fragment_outputs: vec![WgslFragmentOutput {
                name: Ustr::from("color"),
                location: FRAGMENT_OUTPUT_LOCATION,
                kind: WireKind::F32x4,
            }],
            fullscreen_uniform,
            uniform_blocks,
            array_blocks: Vec::new(),
            textures,
            compute: None,
        })
    }

    /// Derive the explicit triangle ABI from the same neutral
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
        let mut array_groups = iface.array_groups.clone();
        array_groups.sort_by_key(|group| stage_key(group.stage));
        let mut array_blocks = Vec::with_capacity(array_groups.len());
        for (index, group) in array_groups.iter().enumerate() {
            array_blocks.push(layout_array_group(
                group,
                uniform_blocks.len() as u32 + index as u32,
                span,
            )?);
        }
        let texture_base = uniform_blocks.len() as u32 + array_blocks.len() as u32;
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
        let fragment_outputs = match &iface.fragment_output {
            FragmentOutput::Single(kind) => vec![WgslFragmentOutput {
                name: Ustr::from("color"),
                location: FRAGMENT_OUTPUT_LOCATION,
                kind: *kind,
            }],
            FragmentOutput::Mrt(outputs) => outputs
                .iter()
                .enumerate()
                .map(|(location, (name, kind))| WgslFragmentOutput {
                    name: *name,
                    location: location as u32,
                    kind: *kind,
                })
                .collect(),
            FragmentOutput::None => {
                unreachable!("validate_explicit_interface requires a fragment output")
            }
        };
        Ok(Self {
            artifact: SharedWgslArtifactKind::Graphics,
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
            fragment: WgslFragmentIo { uv_location: 0 },
            fragment_outputs,
            fullscreen_uniform: WgslUniformBlock {
                role: UniformRole::Stage(ShaderStage::Vertex),
                struct_name: Ustr::from("FullscreenU"),
                binding: 0,
                size: 16,
                members: vec![],
                visibility: WgslStageVisibility { vertex: false, fragment: false },
            },
            uniform_blocks,
            array_blocks,
            textures,
            compute: None,
        })
    }

    /// Derive the compute ABI from the neutral compute interface. The binding
    /// order is the declaration order: storage resources first, then the
    /// stage uniform block. No runtime type dispatch is needed because the
    /// interface has already fixed every resource's logical kind.
    pub fn compute(
        iface: &ShaderInterfacePlan,
        entry: &str,
        span: Span,
    ) -> Result<Self, CompilerError> {
        let Some(compute) = iface.compute.as_ref() else {
            return Err(err(span, "WGSL compute projection requires a compute interface"));
        };
        if iface.stages != [ShaderStage::Compute]
            || iface.fragment_output != FragmentOutput::None
            || !iface.vertex_inputs.is_empty()
            || !iface.varyings.is_empty()
            || !iface.array_groups.is_empty()
            || iface.textures.is_some()
            || !iface.bare_samplers.is_empty()
        {
            return Err(err(span, "WGSL compute projection received graphics interface records"));
        }
        if entry.is_empty() {
            return Err(err(span, "WGSL compute entry name cannot be empty"));
        }
        identifier(Ustr::from(entry), "compute entry", span)?;
        compute
            .validate()
            .map_err(|message| err(span, format!("invalid compute interface: {message}")))?;
        let dispatch_dimensions = match compute.dispatch {
            ComputeDispatchContract::Runtime { dimensions } => dimensions,
            ComputeDispatchContract::Fixed { .. } => {
                return Err(err(span, "WGSL compute ABI requires a runtime dispatch contract"));
            }
        };
        let mut resources = Vec::with_capacity(compute.resources.len());
        for (binding, resource) in compute.resources.iter().enumerate() {
            if resource.address_space != ComputeAddressSpace::Storage {
                return Err(err(span, "WGSL compute ABI only admits storage resources"));
            }
            let ComputeResourceShape::Array(kind) = resource.shape else {
                return Err(err(span, "WGSL compute storage resources must be arrays"));
            };
            let element = match kind {
                ComputeValueKind::F32 => SharedWgslStorageElementKind::F32,
                ComputeValueKind::I32 => SharedWgslStorageElementKind::I32,
                ComputeValueKind::U32 => SharedWgslStorageElementKind::U32,
                ComputeValueKind::AtomicU32 => SharedWgslStorageElementKind::AtomicU32,
                ComputeValueKind::Bool => return Err(err(span, "WGSL compute storage resources require a numeric element kind")),
            };
            let access = match resource.access {
                ComputeAccess::Read => SharedWgslStorageAccess::Read,
                ComputeAccess::ReadWrite => SharedWgslStorageAccess::ReadWrite,
                ComputeAccess::Write => {
                    return Err(err(span, "WGSL storage resources do not admit write-only access"));
                }
            };
            identifier(resource.name, "compute storage resource", span)?;
            resources.push(SharedWgslComputeResource {
                binding: binding as u32,
                name: resource.name.to_string(),
                access,
                element,
            });
        }
        let mut groups = iface
            .uniform_groups
            .iter()
            .filter(|group| group.role == UniformRole::Stage(ShaderStage::Compute))
            .cloned()
            .collect::<Vec<_>>();
        groups.sort_by_key(|group| uniform_role_key(group.role));
        let uniform_blocks = groups
            .iter()
            .enumerate()
            .map(|(binding, group)| {
                layout_uniform_group(
                    group,
                    resources.len() as u32 + binding as u32,
                    WgslStageVisibility { vertex: false, fragment: false },
                    span,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        let builtins = compute
            .builtins
            .iter()
            .map(|builtin| match builtin {
                ComputeBuiltin::GlobalInvocationId => SharedWgslComputeBuiltin::GlobalInvocationId,
                ComputeBuiltin::LocalInvocationId => SharedWgslComputeBuiltin::LocalInvocationId,
                ComputeBuiltin::WorkgroupId => SharedWgslComputeBuiltin::WorkgroupId,
                ComputeBuiltin::LocalInvocationIndex => SharedWgslComputeBuiltin::LocalInvocationIndex,
                ComputeBuiltin::NumWorkgroups => SharedWgslComputeBuiltin::NumWorkgroups,
            })
            .collect();
        Ok(Self {
            artifact: SharedWgslArtifactKind::Compute,
            semantic_fingerprint: iface.semantic_fingerprint(false),
            bind_group: WGSL_BIND_GROUP,
            geometry: WgslGeometryKind::Triangles,
            vertex_entry: String::new(),
            fragment_entry: String::new(),
            vertex_inputs: Vec::new(),
            varyings: Vec::new(),
            fullscreen_vertex: WgslFullscreenVertex {
                topology: WgslPrimitiveTopology::TriangleStrip,
                vertex_count: 0,
                varying_location: 0,
            },
            fragment: WgslFragmentIo { uv_location: 0 },
            fragment_outputs: Vec::new(),
            fullscreen_uniform: WgslUniformBlock {
                role: UniformRole::Stage(ShaderStage::Compute),
                struct_name: Ustr::from("FullscreenU"),
                binding: 0,
                size: 0,
                members: Vec::new(),
                visibility: WgslStageVisibility { vertex: false, fragment: false },
            },
            uniform_blocks,
            array_blocks: Vec::new(),
            textures: Vec::new(),
            compute: Some(SharedWgslComputeInfo {
                entry: entry.to_string(),
                workgroup_size: [
                    compute.workgroup_size.x,
                    compute.workgroup_size.y,
                    compute.workgroup_size.z,
                ],
                dispatch_dimensions,
                builtins,
                resources,
            }),
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
        if self.artifact == SharedWgslArtifactKind::Compute {
            let entry = self
                .compute
                .as_ref()
                .ok_or_else(|| err(span, "compute ABI plan is missing compute metadata"))?
                .entry
                .clone();
            let expected = Self::compute(iface, &entry, span)?;
            if self != &expected {
                return Err(err(span, "WGSL compute ABI plan no longer matches its interface projection"));
            }
            return Ok(());
        }
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
    pub fn to_shared_abi(&self) -> SharedWgslAbi {
        SharedWgslAbi {
            artifact: self.artifact,
            semantic_fingerprint: self.semantic_fingerprint,
            bind_group: self.bind_group,
            geometry: match self.geometry {
                WgslGeometryKind::Fullscreen => SharedWgslGeometryKind::Fullscreen,
                WgslGeometryKind::Triangles => SharedWgslGeometryKind::Triangles,
            },
            vertex_entry: self.vertex_entry.clone(),
            fragment_entry: self.fragment_entry.clone(),
            vertex_inputs: self
                .vertex_inputs
                .iter()
                .enumerate()
                .map(|(index, input)| SharedWgslVertexInput {
                    index: index as u32,
                    struct_name: input.struct_name.to_string(),
                    rate: match input.rate {
                        VertexRate::PerVertex => "vertex".into(),
                        VertexRate::PerInstance => "instance".into(),
                    },
                    attrs: input
                        .attrs
                        .iter()
                        .map(|attr| SharedWgslVertexAttribute {
                            location: attr.location,
                            name: attr.name.to_string(),
                            kind: attr_kind_wire(attr.kind).into(),
                        })
                        .collect(),
                })
                .collect(),
            varyings: self
                .varyings
                .iter()
                .map(|varying| SharedWgslVarying {
                    location: varying.location,
                    name: varying.name.to_string(),
                    kind: varying.kind.code(),
                    flat: varying.flat,
                })
                .collect(),
            fullscreen_vertex: SharedWgslFullscreenVertex {
                topology: match self.fullscreen_vertex.topology {
                    WgslPrimitiveTopology::TriangleStrip => SharedWgslPrimitiveTopology::TriangleStrip,
                },
                vertex_count: self.fullscreen_vertex.vertex_count,
                varying_location: self.fullscreen_vertex.varying_location,
            },
            fragment: SharedWgslFragmentIo {
                uv_location: self.fragment.uv_location,
            },
            fragment_outputs: self
                .fragment_outputs
                .iter()
                .map(|output| SharedWgslFragmentOutput {
                    name: output.name.to_string(),
                    location: output.location,
                    kind: output.kind.code(),
                })
                .collect(),
            fullscreen_uniform: if self.geometry == WgslGeometryKind::Fullscreen {
                shared_uniform_block_with_role(&self.fullscreen_uniform, "fullscreen")
            } else {
                SharedWgslUniformBlock {
                    role: "fullscreen".into(),
                    struct_name: "FullscreenU".into(),
                    binding: 0,
                    size: 0,
                    visibility: SharedWgslVisibility { vertex: false, fragment: false },
                    members: Vec::new(),
                }
            },
            uniform_blocks: self.uniform_blocks.iter().map(shared_uniform_block).collect(),
            array_blocks: self.array_blocks.iter().map(shared_array_block).collect(),
            textures: self.textures.iter().map(shared_texture_binding).collect(),
            compute: self.compute.clone(),
        }
    }

    /// Serialize through the one shared producer/consumer wire owner.
    pub fn wire(&self) -> String {
        self.to_shared_abi()
            .serialize()
            .expect("validated WGSL ABI plan must serialize")
    }
}

fn shared_visibility(visibility: WgslStageVisibility) -> SharedWgslVisibility {
    SharedWgslVisibility {
        vertex: visibility.vertex,
        fragment: visibility.fragment,
    }
}

fn shared_uniform_block_with_role(block: &WgslUniformBlock, role: &str) -> SharedWgslUniformBlock {
    SharedWgslUniformBlock {
        role: role.into(),
        struct_name: block.struct_name.to_string(),
        binding: block.binding,
        size: block.size,
        visibility: shared_visibility(block.visibility),
        members: block
            .members
            .iter()
            .map(|member| SharedWgslUniformMember {
                name: member.name.to_string(),
                kind: member.kind.code(),
                offset: member.offset,
                align: member.align,
                size: member.size,
            })
            .collect(),
    }
}

fn shared_uniform_block(block: &WgslUniformBlock) -> SharedWgslUniformBlock {
    shared_uniform_block_with_role(block, uniform_role_wire(block.role))
}

fn shared_array_block(block: &WgslArrayBlock) -> SharedWgslArrayBlock {
    SharedWgslArrayBlock {
        stage: block.stage.name().into(),
        logical_name: block.logical_name.to_string(),
        struct_name: block.struct_name.to_string(),
        binding: block.binding,
        size: block.size,
        visibility: shared_visibility(block.visibility),
        members: block
            .members
            .iter()
            .map(|member| SharedWgslArrayMember {
                name: member.name.to_string(),
                elem: member.elem.code(),
                capacity: member.capacity,
                offset: member.offset,
                stride: member.stride,
            })
            .collect(),
    }
}

fn shared_texture_binding(texture: &WgslTextureBinding) -> SharedWgslTextureBinding {
    SharedWgslTextureBinding {
        slot: texture.slot,
        name: texture.name.to_string(),
        kind: match texture.kind {
            SamplerKind::Float => SharedWgslTextureKind::Float,
            SamplerKind::Int => SharedWgslTextureKind::Int,
        },
        texture_binding: texture.texture_binding,
        sampler_binding: texture.sampler_binding,
        visibility: shared_visibility(texture.visibility),
    }
}

fn uniform_role_wire(role: UniformRole) -> &'static str {
    match role {
        UniformRole::Engine => "engine",
        UniformRole::Stage(ShaderStage::Fragment) => "fragment",
        UniformRole::Stage(ShaderStage::Vertex) => "vertex",
        UniformRole::Stage(ShaderStage::Compute) => "compute",
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

fn uniform_role_key(role: UniformRole) -> u8 {
    match role {
        UniformRole::Engine => 0,
        UniformRole::Stage(ShaderStage::Fragment) => 1,
        UniformRole::Stage(ShaderStage::Vertex) => 2,
        UniformRole::Stage(ShaderStage::Compute) => 3,
    }
}

fn validate_fullscreen_interface(
    iface: &ShaderInterfacePlan,
    span: Span,
) -> Result<(), CompilerError> {
    if iface.compute.is_some() || iface.stages.contains(&ShaderStage::Compute) {
        return Err(err(
            span,
            "WGSL graphics projection does not admit compute interfaces yet",
        ));
    }
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
        return Err(unsupported_err(
            span,
            "fullscreen uniform arrays",
            "WGSL fullscreen projection does not accept uniform arrays yet",
        ));
    }
    if !iface.bare_samplers.is_empty() {
        return Err(unsupported_err(
            span,
            "fullscreen bare sampler bindings",
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
            return Err(unsupported_err(
                span,
                "non-RGBA fullscreen fragment output",
                format!(
                    "WGSL fullscreen projection requires vec4<f32> output, found {}",
                    kind.code()
                ),
            ));
        }
        FragmentOutput::Mrt(_) => {
            return Err(unsupported_err(
                span,
                "fullscreen multiple render targets",
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
        UniformRole::Stage(ShaderStage::Compute) => {
            panic!("compute uniform roles must be lowered through the compute ABI")
        }
    }
}

fn validate_explicit_interface(
    iface: &ShaderInterfacePlan,
    span: Span,
) -> Result<(), CompilerError> {
    if iface.compute.is_some() || iface.stages.contains(&ShaderStage::Compute) {
        return Err(err(
            span,
            "WGSL graphics projection does not admit compute interfaces yet",
        ));
    }
    if iface.stages != [ShaderStage::Vertex, ShaderStage::Fragment] {
        return Err(err(
            span,
            "WGSL explicit projection admits one vertex stage followed by one fragment stage",
        ));
    }
    if iface.vertex_inputs.is_empty() || iface.vertex_inputs.len() > 2 {
        return Err(unsupported_err(
            span,
            "multiple vertex input streams",
            "WGSL explicit projection admits one per-vertex stream and at most one per-instance stream",
        ));
    }
    if iface.vertex_inputs[0].rate != VertexRate::PerVertex
        || iface.vertex_inputs[0].attrs.is_empty()
    {
        return Err(err(span, "WGSL explicit projection requires a non-empty per-vertex stream first"));
    }
    for input in &iface.vertex_inputs {
        identifier(input.struct_name, "vertex input name", span)?;
        if input.attrs.is_empty() {
            return Err(err(span, "WGSL explicit projection requires non-empty vertex streams"));
        }
        for (name, kind) in &input.attrs {
            identifier(*name, "vertex attribute name", span)?;
            if matches!(kind, AttrKind::I32) {
                return Err(err(
                    span,
                    "WGSL explicit projection currently admits float vertex attributes only",
                ));
            }
        }
    }
    if iface.vertex_inputs.len() == 2 && iface.vertex_inputs[1].rate != VertexRate::PerInstance {
        return Err(err(span, "WGSL explicit projection requires the second stream to be per-instance"));
    }
    if iface.varyings.is_empty() {
        return Err(unsupported_err(
            span,
            "fragment varyings",
            "WGSL explicit projection requires fragment varyings",
        ));
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
    if !iface.bare_samplers.is_empty() {
        return Err(unsupported_err(
            span,
            "explicit bare sampler bindings",
            "WGSL explicit projection does not admit bare samplers",
        ));
    }
    for stage in [ShaderStage::Vertex, ShaderStage::Fragment] {
        if iface
            .array_groups
            .iter()
            .filter(|group| group.stage == stage)
            .count()
            > 1
        {
            return Err(err(
                span,
                format!(
                    "WGSL interface contains more than one {} uniform array group",
                    stage.name()
                ),
            ));
        }
    }
    for role in [
        UniformRole::Engine,
        UniformRole::Stage(ShaderStage::Vertex),
        UniformRole::Stage(ShaderStage::Fragment),
    ] {
        if iface
            .uniform_groups
            .iter()
            .filter(|group| group.role == role)
            .count()
            > 1
        {
            return Err(err(
                span,
                format!(
                    "WGSL interface contains more than one {} uniform group",
                    uniform_role_wire(role)
                ),
            ));
        }
    }
    match &iface.fragment_output {
        FragmentOutput::Single(WireKind::F32x4) => {}
        FragmentOutput::Single(kind) => {
            return Err(unsupported_err(
                span,
                "non-RGBA single fragment output",
                format!(
                    "WGSL explicit projection requires a vec4<f32> single fragment output, found {}",
                    kind.code()
                ),
            ));
        }
        FragmentOutput::Mrt(outputs) => {
            if outputs.is_empty() {
                return Err(err(span, "WGSL explicit MRT output has no members"));
            }
            for (name, kind) in outputs {
                identifier(*name, "fragment output name", span)?;
                if wgsl_type(*kind).is_none() {
                    return Err(err(
                        span,
                        format!(
                            "WGSL fragment output `{name}` has unsupported wire kind {}",
                            kind.code()
                        ),
                    ));
                }
            }
        }
        FragmentOutput::None => {
            return Err(err(span, "WGSL explicit projection requires a fragment output"));
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

/// The WGSL block name for a stage's array block, minted from its owner exactly
/// as the GLSL lane does -- one program's two stages cannot collide, and a clash
/// with a user `@uniform` group is rejected by `identifier`/emission rather than
/// renamed around.
fn array_block_name(owner: Ustr) -> Ustr {
    Ustr::from(&format!("{owner}_Arrays"))
}

fn layout_array_group(
    group: &LogicalArrayGroup,
    binding: u32,
    span: Span,
) -> Result<WgslArrayBlock, CompilerError> {
    let struct_name = array_block_name(group.owner);
    identifier(struct_name, "uniform array block name", span)?;
    // One layout authority: capacities come from `@size(N)` in program text, and
    // this is where they are computed wide and rejected before narrowing.
    let plan = skull_mir::gpu_layout::uniform_array_block(struct_name, &group.members)?;
    let mut members = Vec::with_capacity(plan.members.len());
    for member in &plan.members {
        identifier(member.name, "uniform array member name", span)?;
        if members
            .iter()
            .any(|seen: &WgslArrayMember| seen.name == member.name)
        {
            return Err(err(
                span,
                format!(
                    "WGSL uniform array member `{}` is declared more than once",
                    member.name
                ),
            ));
        }
        if array_element_type(member.elem).is_none() {
            return Err(err(
                span,
                format!(
                    "WGSL uniform array member `{}` has unsupported element kind {}",
                    member.name,
                    member.elem.code()
                ),
            ));
        }
        members.push(WgslArrayMember {
            name: member.name,
            elem: member.elem,
            capacity: member.capacity,
            offset: member.offset,
            stride: member.stride,
        });
    }
    Ok(WgslArrayBlock {
        stage: group.stage,
        logical_name: group.owner,
        struct_name,
        binding,
        size: plan.size,
        members,
        visibility: match group.stage {
            ShaderStage::Vertex => WgslStageVisibility { vertex: true, fragment: false },
            ShaderStage::Fragment => WgslStageVisibility { vertex: false, fragment: true },
            ShaderStage::Compute => {
                return Err(err(span, "WGSL uniform array blocks do not admit compute stages yet"));
            }
        },
    })
}

/// The declared WGSL element type of a uniform array. WGSL's uniform address
/// space requires a 16-byte element stride, which std140 already gives every
/// admitted kind, so each element is declared as its 4-component vector and the
/// read projects back down.
pub fn array_element_type(elem: WireKind) -> Option<&'static str> {
    match elem {
        WireKind::F32 | WireKind::F32x2 | WireKind::F32x4 => Some("vec4<f32>"),
        WireKind::I32 => Some("vec4<i32>"),
        WireKind::U32 => Some("vec4<u32>"),
        WireKind::F32x3 | WireKind::Mat4 | WireKind::Bool32 => None,
    }
}

/// The swizzle that recovers one element's declared value from its padded slot.
pub fn array_element_projection(elem: WireKind) -> Option<&'static str> {
    match elem {
        WireKind::F32 | WireKind::I32 | WireKind::U32 => Some(".x"),
        WireKind::F32x2 => Some(".xy"),
        WireKind::F32x4 => Some(""),
        WireKind::F32x3 | WireKind::Mat4 | WireKind::Bool32 => None,
    }
}

fn stage_key(stage: ShaderStage) -> u8 {
    match stage {
        ShaderStage::Vertex => 0,
        ShaderStage::Fragment => 1,
        ShaderStage::Compute => 2,
    }
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
        WireKind::F32 | WireKind::I32 | WireKind::U32 => Some((4, 4)),
        WireKind::F32x2 => Some((8, 8)),
        WireKind::F32x3 => Some((16, 12)),
        WireKind::F32x4 => Some((16, 16)),
        WireKind::Mat4 => Some((16, 64)),
        // WGSL has no `bool` in the uniform address space -- a `bool` member is
        // the 4-byte integer the CPU already writes, compared at every read.
        WireKind::Bool32 => Some((4, 4)),
    }
}

/// A uniform block member's declared WGSL type. Distinct from [`wgsl_type`]
/// because the uniform address space cannot hold a `bool`: the member is `u32`
/// there and only the READ recovers the boolean.
pub fn uniform_member_type(kind: WireKind) -> Option<&'static str> {
    match kind {
        WireKind::Bool32 => Some("u32"),
        other => wgsl_type(other),
    }
}

pub fn wgsl_type(kind: WireKind) -> Option<&'static str> {
    match kind {
        WireKind::F32 => Some("f32"),
        WireKind::F32x2 => Some("vec2<f32>"),
        WireKind::F32x3 => Some("vec3<f32>"),
        WireKind::F32x4 => Some("vec4<f32>"),
        WireKind::I32 => Some("i32"),
        WireKind::U32 => Some("u32"),
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

type SamplerParams = HashSet<Idx<Cell>>;
type StageSamplerParams = HashMap<ShaderInstance, SamplerParams>;

fn sampler_params_for_body(
    ws: &Workspace,
    instance: &ShaderInstance,
    body: &MirFunc,
    span: Span,
) -> Result<SamplerParams, CompilerError> {
    let function = function_for_instance(ws, instance);
    if function.param_decls.len() != body.params.len() {
        return Err(err(
            span,
            format!(
                "WGSL helper `{}` and MIR parameter populations disagree",
                function.name
            ),
        ));
    }
    Ok(body
        .params
        .iter()
        .enumerate()
        .filter_map(|(index, cell)| {
            matches!(value_kind_for_type(ws, function.param_decls[index].ty), Some(ValueKind::Sampler(_)))
                .then_some(*cell)
        })
        .collect())
}

fn sampler_params_for_stage(
    ws: &Workspace,
    stage: &ShaderStageIr,
    span: Span,
) -> Result<StageSamplerParams, CompilerError> {
    let mut params = StageSamplerParams::new();
    for instance in &stage.order {
        let body = stage.bodies.get(instance).ok_or_else(|| {
            err(
                span,
                format!(
                    "WGSL sampler lowering has no body for `{}`",
                    function_for_instance(ws, instance).name
                ),
            )
        })?;
        let cells = sampler_params_for_body(ws, instance, body, span)?;
        params.insert(instance.clone(), cells);
    }
    let entry_cells = sampler_params_for_body(ws, &stage.entry, &stage.body, span)?;
    params.insert(stage.entry.clone(), entry_cells);
    Ok(params)
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
    let sampler_params = sampler_params_for_stage(ws, stage, span)?;

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
                    instance.inst.symbol(ws, &ir.interner)
                ),
            )
        })?;
        let f = function_for_instance(ws, instance);
        let instance_sampler_params = sampler_params.get(instance).ok_or_else(|| {
            err(
                span,
                format!(
                    "WGSL sampler lowering has no parameter record for `{}`",
                    f.name
                ),
            )
        })?;
        let emitter = FunctionEmitter::new(
            ws,
            &ir.interner,
            stage,
            instance,
            body,
            abi,
            HashMap::new(),
            span,
        )?.with_sampler_params(
            instance_sampler_params,
            &sampler_params,
        );
        source.push_str(&emitter.emit_function(f, false)?);
    }

    let entry_sampler_params = sampler_params.get(&stage.entry).ok_or_else(|| {
        err(
            span,
            format!(
                "WGSL sampler lowering has no parameter record for `{}`",
                entry_fn.name
            ),
        )
    })?;
    let emitter = FunctionEmitter::new(
        ws,
        &ir.interner,
        stage,
        &stage.entry,
        &stage.body,
        abi,
        entry_roles,
        span,
    )?
    .with_sampler_params(entry_sampler_params, &sampler_params);
    source.push_str(&emitter.emit_function(entry_fn, true)?);
    emit_fullscreen_entries(
        &mut source,
        abi,
        stage.entry.inst.symbol(ws, &ir.interner),
    );
    validate_wgsl_source(&source, span)?;

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
    emit_explicit_types(&mut source, ws, vertex_fn, fragment_fn, abi, span)?;
    emit_explicit_uniforms(&mut source, abi);
    emit_array_blocks(&mut source, abi, span)?;
    emit_textures(&mut source, abi);

    for stage in [vertex, fragment] {
        for instance in &stage.order {
            let body = stage.bodies.get(instance).ok_or_else(|| {
                err(
                    span,
                    format!(
                        "WGSL IR order names {} without a lowered body",
                        instance.inst.symbol(ws, &ir.interner)
                    ),
                )
            })?;
            let function = function_for_instance(ws, instance);
            let emitter = FunctionEmitter::new(
                ws,
                &ir.interner,
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
        &ir.interner,
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
        &ir.interner,
        fragment,
        &fragment.entry,
        &fragment.body,
        abi,
        fragment_roles,
        span,
    )?
    .with_frag_coord(
        fragment_reads_frag_coord(ws, fragment).then(|| FRAG_COORD_PARAM.to_string()),
    );
    source.push_str(&emitter.emit_function(fragment_fn, true)?);
    emit_explicit_entries(&mut source, ws, &ir.interner, vertex, fragment, abi, span)?;

    Ok(WgslShaderEmission {
        source,
        dependencies: ir.declarations(),
        interface: iface.clone(),
        abi: abi.clone(),
        ir: ir.clone(),
    })
}

/// Emit one compute module from the retained, concrete stage plan. The logical
/// entry receives only the builtin packet; storage and uniform parameters are
/// physical globals owned by this ABI projection.
pub fn emit_compute_shader(
    ws: &Workspace,
    ir: &ShaderIR,
    iface: &ShaderInterfacePlan,
    abi: &WgslAbiPlan,
    span: Span,
) -> Result<WgslShaderEmission, CompilerError> {
    if abi.artifact != SharedWgslArtifactKind::Compute {
        return Err(err(span, "WGSL compute emission requires a compute ABI"));
    }
    abi.validate(iface, span)?;
    validate_compute_ir_shape(ir, span)?;
    let stage = &ir.stages[0];
    let expected_entry = format!("{}_compute", stage.entry.inst.symbol(ws, &ir.interner));
    let entry_fn = function_for_instance(ws, &stage.entry);
    let entry_roles = compute_entry_param_roles(ws, entry_fn, &stage.body, iface, abi, span)?;
    validate_known_bodies(stage, span)?;
    let compute = abi
        .compute
        .as_ref()
        .ok_or_else(|| err(span, "WGSL compute ABI is missing compute metadata"))?;
    let logical_compute = iface
        .compute
        .as_ref()
        .ok_or_else(|| err(span, "WGSL compute interface is missing logical metadata"))?;
    if compute.entry != expected_entry {
        return Err(err(
            span,
            format!(
                "WGSL compute entry `{}` does not name retained instance `{expected_entry}`",
                compute.entry
            ),
        ));
    }

    let mut source = String::new();
    source.push_str("struct GlobalInvocationId {\n");
    source.push_str("  x: i32,\n  y: i32,\n  z: i32,\n}\n\n");
    for resource in &compute.resources {
        let access = match resource.access {
            SharedWgslStorageAccess::Read => "read",
            SharedWgslStorageAccess::ReadWrite => "read_write",
        };
        let element = match resource.element {
            SharedWgslStorageElementKind::F32 => "f32",
            SharedWgslStorageElementKind::I32 => "i32",
            SharedWgslStorageElementKind::U32 => "u32",
            SharedWgslStorageElementKind::AtomicU32 => "atomic<u32>",
        };
        writeln!(
            source,
            "@group({}) @binding({}) var<storage, {}> s_{}: array<{}>;",
            abi.bind_group, resource.binding, access, resource.name, element
        )
        .unwrap();
    }
    if !compute.resources.is_empty() {
        source.push('\n');
    }
    emit_explicit_uniforms(&mut source, abi);

    for workgroup in &logical_compute.workgroup {
        let kind = match workgroup.kind {
            ComputeValueKind::F32 => ValueKind::F32,
            ComputeValueKind::I32 => ValueKind::I32,
            ComputeValueKind::U32 => ValueKind::U32,
            ComputeValueKind::Bool => ValueKind::Bool,
            ComputeValueKind::AtomicU32 => {
                return Err(err(span, "workgroup arrays cannot contain atomic values"));
            }
        };
        let ty = wgsl_value_type(kind).ok_or_else(|| {
            err(span, format!("compute workgroup `{}` has no WGSL element type", workgroup.name))
        })?;
        identifier(workgroup.name, "compute workgroup", span)?;
        writeln!(source, "var<workgroup> wg_{}: array<{}, {}>;", workgroup.name, ty, workgroup.size)
            .unwrap();
    }
    if !logical_compute.workgroup.is_empty() {
        source.push('\n');
    }

    for instance in &stage.order {
        let body = stage.bodies.get(instance).ok_or_else(|| {
            err(
                span,
                format!(
                    "WGSL IR order names `{}` without a lowered body",
                    instance.inst.symbol(ws, &ir.interner)
                ),
            )
        })?;
        let function = function_for_instance(ws, instance);
        if body.insts.values().any(|instruction| {
            matches!(instruction, Inst::Call { func, .. } if WgslIntrinsic::classify(&ws.module(func.module).functions[func.func]) == Some(WgslIntrinsic::GlobalInvocationId))
        }) {
            return Err(err(
                span,
                "global_invocation_id is only available in the compute entry",
            ));
        }
        let helper_roles = compute_helper_param_roles(ws, function, body, logical_compute, span)?;
        let emitter = FunctionEmitter::new(
            ws,
            &ir.interner,
            stage,
            instance,
            body,
            abi,
            helper_roles,
            span,
        )?
        .with_workgroup_cells(compute_workgroup_cells(body, &logical_compute.workgroup));
        source.push_str(&emitter.emit_function(function, false)?);
    }

    let emitter = FunctionEmitter::new(
        ws,
        &ir.interner,
        stage,
        &stage.entry,
        &stage.body,
        abi,
        entry_roles,
        span,
    )?
    .with_compute_invocation(
        logical_compute
            .builtins
            .contains(&ComputeBuiltin::GlobalInvocationId)
            .then(|| "global_id".to_string()),
    )
    .with_compute_builtins(&logical_compute.builtins)
    .with_workgroup_cells(compute_workgroup_cells(&stage.body, &logical_compute.workgroup));
    source.push_str(&emitter.emit_function(entry_fn, true)?);
    let mut wrapper_params = Vec::new();
    let mut wrapper_args = Vec::new();
    for builtin in &logical_compute.builtins {
        let (attribute, name, ty) = match builtin {
            ComputeBuiltin::GlobalInvocationId => {
                ("global_invocation_id", "global_id", "vec3<u32>")
            }
            ComputeBuiltin::LocalInvocationId => {
                ("local_invocation_id", "local_id", "vec3<u32>")
            }
            ComputeBuiltin::WorkgroupId => ("workgroup_id", "workgroup_id", "vec3<u32>"),
            ComputeBuiltin::LocalInvocationIndex => {
                ("local_invocation_index", "local_index", "u32")
            }
            ComputeBuiltin::NumWorkgroups => ("num_workgroups", "num_workgroups", "vec3<u32>"),
        };
        wrapper_params.push(format!("@builtin({attribute}) {name}: {ty}"));
        wrapper_args.push(name);
    }
    writeln!(
        source,
        "@compute @workgroup_size({}, {}, {})\nfn {}({}) {{",
        compute.workgroup_size[0],
        compute.workgroup_size[1],
        compute.workgroup_size[2],
        compute.entry,
        wrapper_params.join(", ")
    )
    .unwrap();
    writeln!(
        source,
        "  {}({});",
        stage.entry.inst.symbol(ws, &ir.interner),
        wrapper_args.join(", ")
    )
    .unwrap();
    source.push_str("}\n");
    validate_wgsl_source(&source, span)?;

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

fn validate_compute_ir_shape(ir: &ShaderIR, span: Span) -> Result<(), CompilerError> {
    if ir.stages.len() != 1 || ir.stages[0].stage != ShaderStage::Compute {
        return Err(err(
            span,
            "WGSL compute emission requires exactly one compute IR stage",
        ));
    }
    Ok(())
}

fn validate_wgsl_source(source: &str, span: Span) -> Result<(), CompilerError> {
    let module = naga::front::wgsl::parse_str(source)
        .map_err(|error| err(span, format!("WGSL parse failed: {error}")))?;
    naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    )
    .validate(&module)
    .map_err(|error| err(span, format!("WGSL validation failed: {error:?}")))?;
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
    fragment_fn: &Function,
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
        let interpolate = if field.flat { "@interpolate(flat) " } else { "" };
        writeln!(
            out,
            "  @location({}) {interpolate}{}: {},",
            field.location, field.name, ty
        )
        .unwrap();
    }
    out.push_str("}\n\n");
    // The fragment's own returned struct: an MRT entry returns a named value
    // struct, and the located target struct is built from it in the wrapper.
    if let Some(name) = fragment_fn.return_ty.and_then(|ty| nominal_name(ws, ty)) {
        if value_struct_kind(name) == Some(ValueKind::Struct(name)) && name != varying {
            let fields = explicit_struct_fields(ws, name, span)?;
            emit_value_struct(out, name, &fields, span)?;
        }
    }
    Ok(())
}

fn emit_array_blocks(
    out: &mut String,
    abi: &WgslAbiPlan,
    span: Span,
) -> Result<(), CompilerError> {
    for block in &abi.array_blocks {
        writeln!(out, "struct {} {{", block.struct_name).unwrap();
        for member in &block.members {
            let elem = array_element_type(member.elem).ok_or_else(|| {
                err(
                    span,
                    format!(
                        "WGSL uniform array member `{}` has no emitted element type",
                        member.name
                    ),
                )
            })?;
            writeln!(
                out,
                "  {}: array<{}, {}>,",
                member.name, elem, member.capacity
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
    if !abi.array_blocks.is_empty() {
        out.push('\n');
    }
    Ok(())
}

fn emit_explicit_uniforms(out: &mut String, abi: &WgslAbiPlan) {
    for block in &abi.uniform_blocks {
        writeln!(out, "struct {} {{", block.struct_name).unwrap();
        for member in &block.members {
            writeln!(out, "  {}: {},", member.name, uniform_member_type(member.kind).unwrap()).unwrap();
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
    interner: &MonoInterner,
    vertex: &ShaderStageIr,
    fragment: &ShaderStageIr,
    abi: &WgslAbiPlan,
    span: Span,
) -> Result<(), CompilerError> {
    let varying = varying_output_name(ws, vertex, span)?;
    let output = format!("{varying}Out");
    writeln!(out, "@vertex\nfn {}(", abi.vertex_entry).unwrap();
    for input in &abi.vertex_inputs {
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
    }
    out.push_str("  @builtin(vertex_index) vertex_index: u32,\n");
    writeln!(out, ") -> {output} {{").unwrap();
    let mut args = Vec::new();
    for input in &abi.vertex_inputs {
        let input_args = input
            .attrs
            .iter()
            .map(|attr| format!("input_{}", attr.location))
            .collect::<Vec<_>>()
            .join(", ");
        let name = format!("{}_attrs", input.struct_name);
        writeln!(out, "  let {name} = {}({input_args});", input.struct_name).unwrap();
        args.push(name);
    }
    args.push("vertex_index".to_string());
    writeln!(out, "  let result = {}({});", vertex.entry.inst.symbol(ws, interner), args.join(", ")).unwrap();
    writeln!(out, "  var output: {output};").unwrap();
    out.push_str("  output.position = result.position;\n");
    for field in &abi.varyings {
        writeln!(out, "  output.{} = result.{};", field.name, field.name).unwrap();
    }
    out.push_str("  return output;\n}\n\n");

    let outputs = &abi.fragment_outputs;
    let mrt = outputs.len() > 1;
    let result_ty = if mrt {
        let targets = format!("{}Targets", fragment_result_struct(ws, fragment, span)?);
        writeln!(out, "struct {targets} {{").unwrap();
        for output in outputs {
            writeln!(
                out,
                "  @location({}) {}: {},",
                output.location,
                output.name,
                wgsl_type(output.kind).ok_or_else(|| err(
                    span,
                    format!("WGSL fragment output `{}` has no emitted type", output.name)
                ))?
            )
            .unwrap();
        }
        out.push_str("}\n\n");
        targets
    } else {
        let output = &outputs[0];
        format!(
            "@location({}) {}",
            output.location,
            wgsl_type(output.kind).ok_or_else(|| err(
                span,
                "WGSL single fragment output has no emitted type"
            ))?
        )
    };
    writeln!(
        out,
        "@fragment\nfn {}(input: {output}) -> {result_ty} {{",
        abi.fragment_entry,
    )
    .unwrap();
    let mut varying_args = std::iter::once("input.position".to_string())
        .chain(abi.varyings.iter().map(|field| format!("input.{}", field.name)))
        .collect::<Vec<_>>();
    writeln!(out, "  let vary = {varying}({});", varying_args.join(", ")).unwrap();
    varying_args.clear();
    let call_args = if fragment_reads_frag_coord(ws, fragment) {
        "vary, input.position".to_string()
    } else {
        "vary".to_string()
    };
    if mrt {
        writeln!(out, "  let result = {}({call_args});", fragment.entry.inst.symbol(ws, interner)).unwrap();
        writeln!(out, "  var targets: {result_ty};").unwrap();
        for output in outputs {
            writeln!(out, "  targets.{0} = result.{0};", output.name).unwrap();
        }
        out.push_str("  return targets;\n}\n");
    } else {
        writeln!(out, "  return {}({call_args});", fragment.entry.inst.symbol(ws, interner)).unwrap();
        out.push_str("}\n");
    }
    Ok(())
}

/// The named value struct the fragment entry returns, which is what an MRT
/// target struct is minted from.
fn fragment_result_struct(
    ws: &Workspace,
    fragment: &ShaderStageIr,
    span: Span,
) -> Result<Ustr, CompilerError> {
    function_for_instance(ws, &fragment.entry)
        .return_ty
        .and_then(|ty| nominal_name(ws, ty))
        .ok_or_else(|| err(span, "WGSL MRT fragment entry does not return a named struct"))
}

/// Whether the fragment stage calls `frag_coord()`. WGSL has no global fragment
/// position, so the entry takes it as a parameter and only when it is read --
/// an unused parameter would otherwise sit in every emitted pipeline.
fn fragment_reads_frag_coord(ws: &Workspace, fragment: &ShaderStageIr) -> bool {
    fragment.body.insts.values().any(|inst| match inst {
        Inst::Call { func, .. } => {
            let function = &ws.module(func.module).functions[func.func];
            WgslIntrinsic::classify(function) == Some(WgslIntrinsic::FragCoord)
        }
        _ => false,
    })
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
            uniform_member_type(member.kind).unwrap()
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
                uniform_member_type(member.kind).unwrap()
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
    out.push_str("@vertex\nfn fullscreen_vertex(@builtin(vertex_index) vertex_index: u32) -> FullscreenVertexOut {\n");
    out.push_str("  var output: FullscreenVertexOut;\n");
    out.push_str("  let x = f32(vertex_index & 1u) * 2.0 - 1.0;\n");
    out.push_str("  let y = f32(vertex_index >> 1u) * 2.0 - 1.0;\n");
    out.push_str("  output.position = vec4<f32>(x, y, 0.0, 1.0);\n");
    out.push_str("  let corner = vec2<f32>(f32(vertex_index & 1u), f32(vertex_index >> 1u));\n");
    out.push_str("  output.uv = u_FullscreenU.uv0 + corner * (u_FullscreenU.uv1 - u_FullscreenU.uv0);\n");
    out.push_str("  return output;\n}\n\n");
    let output = &abi.fragment_outputs[0];
    writeln!(
        out,
        "@fragment\nfn fragment_main(input: FullscreenVertexOut) -> @location({}) {} {{",
        output.location,
        wgsl_type(output.kind).unwrap()
    )
    .unwrap();
    writeln!(out, "  return {}(input.uv);", entry).unwrap();
    out.push_str("}\n");
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ValueKind {
    F32,
    I32,
    U32,
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
            Self::U32 => Some(WireKind::U32),
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
            Self::F32 | Self::I32 | Self::U32 | Self::Bool => 1,
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
    Instance,
    Varying,
    Storage,
    Texture,
    Uniform(UniformRole),
    UniformArray,
    TexCoord,
}

fn function_for_instance<'a>(ws: &'a Workspace, instance: &ShaderInstance) -> &'a Function {
    &ws.module(instance.fref().module).functions[instance.fref().func]
}

fn compute_workgroup_cells(
    body: &MirFunc,
    workgroups: &[LogicalComputeWorkgroup],
) -> HashMap<Idx<Cell>, (String, ValueKind)> {
    body.cells
        .iter()
        .filter_map(|(cell, data)| {
            let source_name = data.name?;
            let logical = workgroups.iter().find(|workgroup| workgroup.name == source_name)?;
            let kind = match logical.kind {
                ComputeValueKind::F32 => ValueKind::F32,
                ComputeValueKind::I32 => ValueKind::I32,
                ComputeValueKind::U32 => ValueKind::U32,
                ComputeValueKind::Bool => ValueKind::Bool,
                ComputeValueKind::AtomicU32 => return None,
            };
            Some((cell, (format!("wg_{source_name}"), kind)))
        })
        .collect()
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
        TypeExpr::Builtin(BuiltinTy::Uint) => Some(ValueKind::U32),
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

/// The WGSL spelling of a value in FUNCTION scope. Deliberately not the uniform
/// spelling: `bool` is a first-class local and has no uniform representation at
/// all, which is why [`uniform_member_type`] answers that question separately.
fn wgsl_value_type(kind: ValueKind) -> Option<String> {
    match kind {
        ValueKind::Struct(name) => Some(name.to_string()),
        ValueKind::Bool => Some("bool".to_string()),
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
        WireKind::U32 => Some(ValueKind::U32),
        WireKind::Mat4 => Some(ValueKind::Mat4),
        WireKind::Bool32 => Some(ValueKind::Bool),
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
) -> Result<HashMap<Idx<Cell>, ParamRole>, CompilerError> {
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
) -> Result<HashMap<Idx<Cell>, ParamRole>, CompilerError> {
    if function.param_decls.len() != body.params.len() {
        return Err(err(
            span,
            "WGSL explicit entry declaration and IR parameter populations disagree",
        ));
    }
    let mut roles = HashMap::new();
    for (index, cell) in body.params.iter().enumerate() {
        let param = &function.param_decls[index];
        let role = if stage == ShaderStage::Vertex
            && (has_anno(function, index, "attr") || has_anno(function, index, "instance"))
        {
            let rate = if has_anno(function, index, "attr") {
                VertexRate::PerVertex
            } else {
                VertexRate::PerInstance
            };
            let name = nominal_name(ws, param.ty).unwrap_or_default();
            if !iface.vertex_inputs.iter().any(|input| input.struct_name == name && input.rate == rate) {
                return Err(err(span, "WGSL explicit vertex input parameter is absent from the interface plan"));
            }
            if rate == VertexRate::PerInstance {
                ParamRole::Instance
            } else {
                ParamRole::Attribute
            }
        } else if has_anno(function, index, "engine") {
            check_uniform_param(ws, param.ty, UniformRole::Engine, iface, param_span(function, index))?;
            ParamRole::Uniform(UniformRole::Engine)
        } else if stage == ShaderStage::Fragment && index == 0 {
            if nominal_name(ws, param.ty).is_none() {
                return Err(err(span, "WGSL explicit fragment entry does not take a varying struct"));
            }
            ParamRole::Varying
        } else if nominal_name(ws, param.ty) == Some(Ustr::from("UniformArray")) {
            let Some(group) = iface.array_group(stage) else {
                return Err(err(
                    param_span(function, index),
                    format!(
                        "WGSL uniform array parameter `{}` has no array group in the interface plan",
                        param.name
                    ),
                ));
            };
            if !group.members.iter().any(|member| member.name == param.name) {
                return Err(err(
                    param_span(function, index),
                    format!(
                        "WGSL uniform array parameter `{}` is absent from the interface plan",
                        param.name
                    ),
                ));
            }
            ParamRole::UniformArray
        } else if has_anno(function, index, "uniform") {
            check_uniform_param(
                ws,
                param.ty,
                UniformRole::Stage(stage),
                iface,
                param_span(function, index),
            )?;
            ParamRole::Uniform(UniformRole::Stage(stage))
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

fn compute_entry_param_roles(
    ws: &Workspace,
    function: &Function,
    body: &MirFunc,
    iface: &ShaderInterfacePlan,
    abi: &WgslAbiPlan,
    span: Span,
) -> Result<HashMap<Idx<Cell>, ParamRole>, CompilerError> {
    if function.param_decls.len() != body.params.len() {
        return Err(err(
            span,
            "WGSL compute entry declaration and IR parameter populations disagree",
        ));
    }
    let compute = iface
        .compute
        .as_ref()
        .ok_or_else(|| err(span, "WGSL compute entry has no compute interface"))?;
    let mut roles = HashMap::new();
    for (index, cell) in body.params.iter().enumerate() {
        let param = &function.param_decls[index];
        if has_anno(function, index, "storage") {
            if !compute.resources.iter().any(|resource| resource.name == param.name) {
                return Err(err(
                    param_span(function, index),
                    format!("WGSL compute storage parameter `{}` is absent from the interface", param.name),
                ));
            }
            roles.insert(*cell, ParamRole::Storage);
        } else if has_anno(function, index, "uniform") {
            check_uniform_param(
                ws,
                param.ty,
                UniformRole::Stage(ShaderStage::Compute),
                iface,
                param_span(function, index),
            )?;
            if abi.uniform_block(UniformRole::Stage(ShaderStage::Compute)).is_none() {
                return Err(err(
                    param_span(function, index),
                    format!("WGSL compute uniform `{}` is absent from the ABI", param.name),
                ));
            }
            roles.insert(*cell, ParamRole::Uniform(UniformRole::Stage(ShaderStage::Compute)));
        } else {
            return Err(err(
                param_span(function, index),
                format!("WGSL compute parameter `{}` has no storage or uniform role", param.name),
            ));
        }
    }
    Ok(roles)
}

fn compute_helper_param_roles(
    ws: &Workspace,
    function: &Function,
    body: &MirFunc,
    iface: &ComputeInterfacePlan,
    span: Span,
) -> Result<HashMap<Idx<Cell>, ParamRole>, CompilerError> {
    if function.param_decls.len() != body.params.len() {
        return Err(err(
            span,
            "WGSL compute helper declaration and IR parameter populations disagree",
        ));
    }
    let mut roles = HashMap::new();
    for (index, cell) in body.params.iter().enumerate() {
        let param = &function.param_decls[index];
        if !is_compute_resource_type(ws, param.ty) {
            continue;
        }
        if !iface.resources.iter().any(|resource| resource.name == param.name) {
            return Err(err(
                param_span(function, index),
                format!(
                    "WGSL compute helper resource `{}` is absent from the entry interface",
                    param.name
                ),
            ));
        }
        roles.insert(*cell, ParamRole::Storage);
    }
    Ok(roles)
}

fn is_compute_resource_type(ws: &Workspace, ty: TyId) -> bool {
    nominal_name(ws, ty).is_some_and(|name| matches!(name.as_str(), "StorageBuffer" | "AtomicStorageBuffer"))
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

/// The enclosing loop's `continue`/`break` targets, threaded through the
/// structured walk.
#[derive(Clone, Copy, Default)]
struct LoopCtx {
    cont: Option<Idx<Block>>,
    brk: Option<Idx<Block>>,
}

/// A recognized `for i in lo..hi` header.
#[derive(Clone, Copy)]
struct LoopInfo {
    slot: Idx<Cell>,
    bound: Idx<Inst>,
    inclusive: bool,
    body_entry: Idx<Block>,
    latch: Idx<Block>,
    exit: Idx<Block>,
}

struct FunctionEmitter<'a> {
    ws: &'a Workspace,
    interner: &'a MonoInterner,
    stage: &'a ShaderStageIr,
    instance: &'a ShaderInstance,
    body: &'a MirFunc,
    abi: &'a WgslAbiPlan,
    param_roles: HashMap<Idx<Cell>, ParamRole>,
    sampler_params: SamplerParams,
    stage_sampler_params: StageSamplerParams,
    cell_names: HashMap<Idx<Cell>, String>,
    cell_kinds: HashMap<Idx<Cell>, ValueKind>,
    span: Span,
    /// The identifier `frag_coord()` renders to, threaded in as a parameter by
    /// the entry wrapper. `None` in every body that cannot receive it, which is
    /// what makes a helper's `frag_coord()` a rejection rather than a guess.
    frag_coord: Option<String>,
    vertex_index: Option<String>,
    compute_invocation: Option<String>,
    compute_local_invocation: Option<String>,
    compute_workgroup_id: Option<String>,
    compute_local_index: Option<String>,
    compute_num_workgroups: Option<String>,
    workgroup_cells: HashMap<Idx<Cell>, (String, ValueKind)>,
    unit_return: bool,
    out: String,
    indent: usize,
    visited: HashSet<Idx<Block>>,
    active: HashSet<Idx<Block>>,
    structure: Structure,
    counted: HashMap<Idx<Block>, LoopInfo>,
    general: HashMap<Idx<Block>, LoopRegion>,
    /// The blocks reachable from the entry. A tier-0 body carries an
    /// unreachable trailing `return <result cell>` that nothing ever stores, so
    /// typing or emitting it would reject a body that is perfectly well formed.
    reachable: HashSet<Idx<Block>>,
}

impl<'a> FunctionEmitter<'a> {
    fn new(
        ws: &'a Workspace,
        interner: &'a MonoInterner,
        stage: &'a ShaderStageIr,
        instance: &'a ShaderInstance,
        body: &'a MirFunc,
        abi: &'a WgslAbiPlan,
        param_roles: HashMap<Idx<Cell>, ParamRole>,
        span: Span,
    ) -> Result<Self, CompilerError> {
        let mut cell_names = HashMap::new();
        let mut cell_kinds = HashMap::new();
        // Two cells can share a source name (one `falloff` per arm of an `if`),
        // and every cell is declared at function scope, so the name has to be
        // deduped or the second declaration silently shadows the first.
        let mut used: HashSet<String> = HashSet::new();
        for (cell, data) in body.cells.iter() {
            let base = data.name.map_or_else(
                || format!("cell_{}", cell.into_raw().into_u32()),
                |name| name.to_string(),
            );
            let base = if base.starts_with("__") {
                format!("rock{base}")
            } else {
                base
            };
            let mut name = base.clone();
            let mut suffix = 0;
            while !used.insert(name.clone()) {
                suffix += 1;
                name = format!("{base}_{suffix}");
            }
            cell_names.insert(cell, name);
            if let Some(ty) = data.ty.and_then(|ty| value_kind_for_type(ws, ty)) {
                cell_kinds.insert(cell, ty);
            }
        }
        let reachable = reachable_blocks(body);
        let structure = skull_codegen_shader::structure::analyze(body);
        let mut counted = HashMap::new();
        let mut general = HashMap::new();
        for (header, region) in structure.loops.iter() {
            match match_counted_loop(body, *header) {
                Some(info) => {
                    counted.insert(*header, info);
                }
                None => {
                    general.insert(*header, region.clone());
                }
            }
        }
        let mut emitter = Self {
            ws,
            interner,
            stage,
            instance,
            body,
            abi,
            param_roles,
            sampler_params: SamplerParams::new(),
            stage_sampler_params: StageSamplerParams::new(),
            cell_names,
            cell_kinds,
            span,
            frag_coord: None,
            vertex_index: None,
            compute_invocation: None,
            compute_local_invocation: None,
            compute_workgroup_id: None,
            compute_local_index: None,
            compute_num_workgroups: None,
            workgroup_cells: HashMap::new(),
            unit_return: false,
            out: String::new(),
            indent: 1,
            visited: HashSet::new(),
            active: HashSet::new(),
            structure,
            counted,
            general,
            reachable,
        };
        emitter.infer_local_kinds()?;
        Ok(emitter)
    }

    fn with_sampler_params(
        mut self,
        sampler_params: &SamplerParams,
        stage_sampler_params: &StageSamplerParams,
    ) -> Self {
        self.sampler_params = sampler_params.clone();
        self.stage_sampler_params = stage_sampler_params.clone();
        self
    }

    fn with_frag_coord(mut self, name: Option<String>) -> Self {
        self.frag_coord = name;
        self
    }

    fn with_compute_invocation(mut self, name: Option<String>) -> Self {
        self.compute_invocation = name;
        self
    }

    fn with_compute_builtins(mut self, builtins: &[ComputeBuiltin]) -> Self {
        if builtins.contains(&ComputeBuiltin::LocalInvocationId) {
            self.compute_local_invocation = Some("local_id".to_string());
        }
        if builtins.contains(&ComputeBuiltin::WorkgroupId) {
            self.compute_workgroup_id = Some("workgroup_id".to_string());
        }
        if builtins.contains(&ComputeBuiltin::LocalInvocationIndex) {
            self.compute_local_index = Some("local_index".to_string());
        }
        if builtins.contains(&ComputeBuiltin::NumWorkgroups) {
            self.compute_num_workgroups = Some("num_workgroups".to_string());
        }
        self
    }

    fn with_workgroup_cells(
        mut self,
        cells: HashMap<Idx<Cell>, (String, ValueKind)>,
    ) -> Self {
        self.workgroup_cells = cells;
        self
    }

    fn infer_local_kinds(&mut self) -> Result<(), CompilerError> {
        for (bid, block) in self.body.blocks.iter() {
            if !self.reachable.contains(&bid) {
                continue;
            }
            for iid in &block.insts {
                if let Inst::Store { cell, value } = self.body.insts[*iid] {
                    if matches!(self.body.insts[value], Inst::Const(Lit::Nil)) {
                        continue;
                    }
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

    /// The declared result of every `Return` in the body. They must agree: a
    /// body whose returns disagree has no one WGSL result type, and picking the
    /// first would emit a coercion nothing asked for.
    fn resolve_return_kind(&self) -> Result<Option<ValueKind>, CompilerError> {
        if self.unit_return {
            return Ok(None);
        }
        let mut kind: Option<ValueKind> = None;
        let mut seen = false;
        for (bid, block) in self.body.blocks.iter() {
            if !self.reachable.contains(&bid) {
                continue;
            }
            let Term::Return(values) = &block.term else {
                continue;
            };
            let found = match values.as_slice() {
                [] => None,
                [value] if self.is_void_value(*value) => None,
                [value] => Some(self.value_kind(*value)?),
                _ => return Err(err(self.span, "WGSL does not admit a multi-value return")),
            };
            if seen && kind != found {
                return Err(err(
                    self.span,
                    "WGSL function returns values of disagreeing types",
                ));
            }
            kind = found;
            seen = true;
        }
        Ok(kind)
    }

    fn push(&mut self, line: &str) {
        for _ in 0..self.indent {
            self.out.push_str("  ");
        }
        self.out.push_str(line);
        self.out.push('\n');
    }

    fn emit_function(mut self, function: &Function, entry: bool) -> Result<String, CompilerError> {
        if entry && self.stage.stage == ShaderStage::Vertex {
            self.vertex_index = Some("vertex_index".to_string());
        }
        self.unit_return = function.return_ty.is_none();
        let return_kind = self.resolve_return_kind()?;
        let signature = match return_kind {
            Some(kind) => {
                let ty = wgsl_value_type(kind).ok_or_else(|| {
                    err(
                        self.span,
                        format!(
                            "WGSL function `{}` returns an unsupported value",
                            function.name
                        ),
                    )
                })?;
                format!(" -> {ty}")
            }
            None => String::new(),
        };
        let name = self.instance.inst.symbol(self.ws, self.interner);
        let params = self.render_params(entry)?;

        let mut head = String::new();
        writeln!(head, "fn {}({}){} {{", name, params, signature).unwrap();
        let parameters: HashSet<_> = self.body.params.iter().copied().collect();
        let mut declarations: Vec<Idx<Cell>> = Vec::new();
        for (cell, _) in self.body.cells.iter() {
            if self.cell_kinds.contains_key(&cell)
                && !parameters.contains(&cell)
                && !self.workgroup_cells.contains_key(&cell)
                && self.is_stored_cell(cell)
            {
                declarations.push(cell);
            }
        }
        for cell in declarations {
            let kind = self.cell_kinds[&cell];
            let ty = wgsl_value_type(kind).ok_or_else(|| {
                err(
                    self.span,
                    format!(
                        "WGSL local `{}` has no emitted type",
                        self.cell_names[&cell]
                    ),
                )
            })?;
            let line = format!("  var {}: {} = {};", self.cell_names[&cell], ty, zero_value(kind));
            head.push_str(&line);
            head.push('\n');
        }

        self.emit_seq(self.body.entry, None, LoopCtx::default())?;
        head.push_str(&self.out);
        head.push_str("}\n\n");
        Ok(head)
    }

    fn render_params(&self, entry: bool) -> Result<String, CompilerError> {
        if entry
            && self
                .param_roles
                .values()
                .any(|role| matches!(role, ParamRole::TexCoord))
        {
            return Ok("uv: vec2<f32>".to_string());
        }
        let mut rendered = Vec::new();
        for cell in &self.body.params {
            if self.sampler_params.contains(cell) {
                let kind = self.cell_kinds.get(cell).copied().ok_or_else(|| {
                    err(
                        self.span,
                        format!(
                            "WGSL sampler parameter `{}` has no neutral type",
                            self.cell_names[cell]
                        ),
                    )
                })?;
                let ValueKind::Sampler(sampler_kind) = kind else {
                    return Err(err(
                        self.span,
                        format!(
                            "WGSL sampler parameter `{}` is not a sampler parameter",
                            self.cell_names[cell]
                        ),
                    ));
                };
                let (texture, sampler) = self.sampler_param_names(*cell);
                rendered.push(format!(
                    "{texture}: {}, {sampler}: sampler",
                    texture_type(sampler_kind)
                ));
                continue;
            }
            if matches!(self.param_roles.get(cell), Some(ParamRole::Storage))
                || (entry
                    && matches!(
                        self.param_roles.get(cell),
                        Some(
                            ParamRole::Texture
                                | ParamRole::Uniform(_)
                                | ParamRole::UniformArray
                        )
                    ))
            {
                continue;
            }
            let kind = self.cell_kinds.get(cell).copied().ok_or_else(|| {
                err(
                    self.span,
                    format!(
                        "WGSL parameter `{}` has no neutral type",
                        self.cell_names[cell]
                    ),
                )
            })?;
            let Some(ty) = wgsl_value_type(kind) else {
                if matches!(kind, ValueKind::Sampler(_)) {
                    return Err(unsupported_err(
                        self.span,
                        "sampler-valued helper parameters",
                        format!(
                            "WGSL parameter `{}` is not a value parameter",
                            self.cell_names[cell]
                        ),
                    ));
                }
                return Err(err(
                    self.span,
                    format!(
                        "WGSL parameter `{}` is not a value parameter",
                        self.cell_names[cell]
                    ),
                ));
            };
            rendered.push(format!("{}: {}", self.cell_names[cell], ty));
        }
        if let Some(name) = &self.frag_coord {
            rendered.push(format!("{name}: vec4<f32>"));
        }
        if let Some(name) = &self.vertex_index {
            rendered.push(format!("{name}: u32"));
        }
        if let Some(name) = &self.compute_invocation {
            rendered.push(format!("{name}: vec3<u32>"));
        }
        if let Some(name) = &self.compute_local_invocation {
            rendered.push(format!("{name}: vec3<u32>"));
        }
        if let Some(name) = &self.compute_workgroup_id {
            rendered.push(format!("{name}: vec3<u32>"));
        }
        if let Some(name) = &self.compute_local_index {
            rendered.push(format!("{name}: u32"));
        }
        if let Some(name) = &self.compute_num_workgroups {
            rendered.push(format!("{name}: vec3<u32>"));
        }
        Ok(rendered.join(", "))
    }

    /// Emit a structured sequence of blocks from `start`, stopping before
    /// `stop`. Straight-line code, `if` diamonds, counted `for` loops and
    /// general loops; anything else rejects rather than emitting a guess.
    fn emit_seq(
        &mut self,
        start: Idx<Block>,
        stop: Option<Idx<Block>>,
        ctx: LoopCtx,
    ) -> Result<(), CompilerError> {
        let mut cur = start;
        loop {
            if Some(cur) == stop {
                return Ok(());
            }
            if !self.active.contains(&cur) {
                if let Some(info) = self.counted.get(&cur).copied() {
                    self.emit_counted(info)?;
                    cur = info.exit;
                    continue;
                }
                if let Some(region) = self.general.get(&cur).cloned() {
                    self.emit_general(&region, ctx)?;
                    cur = region.exit;
                    continue;
                }
            }
            if !self.visited.insert(cur) {
                return Err(err(
                    self.span,
                    "WGSL emission reached unstructured control flow",
                ));
            }
            self.emit_block_effects(cur)?;
            match self.body.blocks[cur].term.clone() {
                Term::Return(values) => {
                    self.emit_return(&values)?;
                    return Ok(());
                }
                Term::Unreachable => return Ok(()),
                Term::Goto(target) => {
                    if Some(target) == ctx.cont {
                        self.push("continue;");
                        return Ok(());
                    }
                    if Some(target) == ctx.brk {
                        self.push("break;");
                        return Ok(());
                    }
                    if Some(target) == stop {
                        return Ok(());
                    }
                    cur = target;
                }
                Term::Branch { cond, then_bb, else_bb } => {
                    match self.emit_branch(cur, cond, then_bb, else_bb, stop, ctx)? {
                        Some(merge) => cur = merge,
                        None => return Ok(()),
                    }
                }
                Term::Switch { .. } => {
                    return Err(err(self.span, "WGSL emission does not admit `match`"));
                }
                Term::Yield { .. } => {
                    return Err(err(self.span, "WGSL emission does not admit `yield`"));
                }
            }
        }
    }

    /// Whether `b` produces no statement and just forwards to another block.
    fn empty_forward(&self, b: Idx<Block>) -> Option<Idx<Block>> {
        if let Term::Goto(target) = self.body.blocks[b].term {
            let has_effect = self.body.blocks[b].insts.iter().any(|iid| {
                matches!(self.body.insts[*iid], Inst::Store { .. }) || self.is_void_call(*iid)
            });
            if !has_effect {
                return Some(target);
            }
        }
        None
    }

    fn is_void_call(&self, iid: Idx<Inst>) -> bool {
        match &self.body.insts[iid] {
            Inst::Call { func, .. } => {
                self.ws.module(func.module).functions[func.func]
                    .return_ty
                    .is_none()
            }
            _ => false,
        }
    }

    fn is_void_value(&self, iid: Idx<Inst>) -> bool {
        self.is_void_call(iid) || matches!(self.body.insts[iid], Inst::Const(Lit::Nil))
    }

    fn emit_branch(
        &mut self,
        blk: Idx<Block>,
        cond: Idx<Inst>,
        then_bb: Idx<Block>,
        else_bb: Idx<Block>,
        stop: Option<Idx<Block>>,
        ctx: LoopCtx,
    ) -> Result<Option<Idx<Block>>, CompilerError> {
        let cond_s = self.render_val(cond)?;
        if let Some(join) = self.empty_forward(else_bb) {
            self.push(&format!("if {cond_s} {{"));
            self.indent += 1;
            self.emit_seq(then_bb, Some(join), ctx)?;
            self.indent -= 1;
            self.push("}");
            return Ok(Some(join));
        }
        if let Some(join) = self.empty_forward(then_bb) {
            self.push(&format!("if !({cond_s}) {{"));
            self.indent += 1;
            self.emit_seq(else_bb, Some(join), ctx)?;
            self.indent -= 1;
            self.push("}");
            return Ok(Some(join));
        }
        let merge = self
            .structure
            .ipdom(blk)
            .filter(|m| Some(*m) != ctx.cont && Some(*m) != ctx.brk);
        let arm_stop = merge.or(stop);
        self.push(&format!("if {cond_s} {{"));
        self.indent += 1;
        self.emit_seq(then_bb, arm_stop, ctx)?;
        self.indent -= 1;
        self.push("} else {");
        self.indent += 1;
        self.emit_seq(else_bb, arm_stop, ctx)?;
        self.indent -= 1;
        self.push("}");
        Ok(merge)
    }

    fn emit_block_effects(&mut self, b: Idx<Block>) -> Result<(), CompilerError> {
        for iid in self.body.blocks[b].insts.clone() {
            let line = match &self.body.insts[iid] {
                Inst::Store { cell, value } => {
                    let (cell, value) = (*cell, *value);
                    if matches!(self.body.insts[value], Inst::Const(Lit::Nil)) {
                        None
                    } else if self.workgroup_cells.contains_key(&cell) {
                        if matches!(self.body.insts[value], Inst::Call { func, .. }
                            if WgslIntrinsic::classify(&self.ws.module(func.module).functions[func.func])
                                == Some(WgslIntrinsic::WorkgroupArray))
                        {
                            None
                        } else {
                            return Err(err(
                                self.span,
                                "workgroup array handles cannot be assigned after declaration",
                            ));
                        }
                    } else {
                        Some(format!(
                            "{} = {};",
                            self.cell_names[&cell],
                            self.render_val(value)?
                        ))
                    }
                }
                Inst::Call { func, args } => {
                    let target = &self.ws.module(func.module).functions[func.func];
                    if target.return_ty.is_none() {
                        Some(format!("{};", self.render_call(iid, *func, args)?))
                    } else {
                        None
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
                | Inst::UintAdd(..)
                | Inst::UintSub(..)
                | Inst::UintMul(..)
                | Inst::UintDiv(..)
                | Inst::UintMod(..)
                | Inst::FloatAdd(..)
                | Inst::FloatSub(..)
                | Inst::FloatMul(..)
                | Inst::FloatDiv(..)
                | Inst::FloatMod(..)
                | Inst::IntLt(..)
                | Inst::IntLeq(..)
                | Inst::IntGt(..)
                | Inst::IntGeq(..)
                | Inst::UintLt(..)
                | Inst::UintLeq(..)
                | Inst::UintGt(..)
                | Inst::UintGeq(..)
                | Inst::FloatLt(..)
                | Inst::FloatLeq(..)
                | Inst::FloatGt(..)
                | Inst::FloatGeq(..)
                | Inst::Eq(..)
                | Inst::NotEq(..)
                | Inst::FloatToInt(_)
                | Inst::IntToFloat(_)
                | Inst::UintToFloat(_)
                | Inst::IntToUint(_)
                | Inst::UintToInt(_)
                | Inst::FloatToUint(_)
                | Inst::UintBitcastInt(_)
                | Inst::IntBitcastUint(_)
                | Inst::Neg(_)
                | Inst::Not(_)
                | Inst::UintBitNot(_)
                | Inst::UintBitAnd(..)
                | Inst::UintBitOr(..)
                | Inst::UintBitXor(..)
                | Inst::UintShl(..)
                | Inst::UintShr(..) => None,
                other => {
                    return Err(err(
                        self.span,
                        format!(
                            "WGSL emission reaches unsupported MIR operation `{}`",
                            mir_kind(other)
                        ),
                    ));
                }
            };
            if let Some(line) = line {
                self.push(&line);
            }
        }
        Ok(())
    }

    fn emit_return(&mut self, values: &[Idx<Inst>]) -> Result<(), CompilerError> {
        if self.unit_return {
            if let [value] = values {
                if let Inst::Call { func, args } = &self.body.insts[*value] {
                    let target = &self.ws.module(func.module).functions[func.func];
                    if target.return_ty.is_some() {
                        self.push(&format!("{};", self.render_call(*value, *func, args)?));
                    }
                }
            }
            self.push("return;");
            return Ok(());
        }
        match values {
            [] => self.push("return;"),
            [value] => {
                if self.is_void_value(*value) {
                    self.push("return;");
                    return Ok(());
                }
                let rendered = self.render_val(*value)?;
                self.push(&format!("return {rendered};"));
            }
            _ => return Err(err(self.span, "WGSL does not admit a multi-value return")),
        }
        Ok(())
    }

    fn emit_counted(&mut self, info: LoopInfo) -> Result<(), CompilerError> {
        let slot = self.cell_names[&info.slot].clone();
        let bound = self.render_val(info.bound)?;
        let cmp = if info.inclusive { "<=" } else { "<" };
        self.push(&format!("for (; {slot} {cmp} {bound}; {slot} = {slot} + 1) {{"));
        self.indent += 1;
        let body_len = self.out.len();
        let ctx = LoopCtx { cont: Some(info.latch), brk: Some(info.exit) };
        self.emit_seq(info.body_entry, None, ctx)?;
        self.trim_trailing_continue(body_len);
        self.indent -= 1;
        self.push("}");
        Ok(())
    }

    /// A general loop: `loop { <cond region>; if !(cond) { break; } <body> }`.
    /// WGSL has no `while`, so the condition is always the explicit break.
    fn emit_general(
        &mut self,
        region: &LoopRegion,
        outer: LoopCtx,
    ) -> Result<(), CompilerError> {
        let header = region.header;
        let (test, cond, body_start, break_when_true) = self.find_test(region)?;

        self.active.insert(header);
        self.push("loop {");
        self.indent += 1;

        self.emit_seq(header, Some(test), outer)?;
        self.emit_block_effects(test)?;
        self.visited.insert(test);

        let cond_s = self.render_val(cond)?;
        if break_when_true {
            self.push(&format!("if {cond_s} {{ break; }}"));
        } else {
            self.push(&format!("if !({cond_s}) {{ break; }}"));
        }

        let ctx = LoopCtx { cont: Some(header), brk: Some(region.exit) };
        let body_len = self.out.len();
        self.emit_seq(body_start, None, ctx)?;
        self.trim_trailing_continue(body_len);

        self.indent -= 1;
        self.push("}");
        self.active.remove(&header);
        Ok(())
    }

    fn find_test(
        &self,
        region: &LoopRegion,
    ) -> Result<(Idx<Block>, Idx<Inst>, Idx<Block>, bool), CompilerError> {
        let mut body: Vec<Idx<Block>> = region.body.iter().copied().collect();
        body.sort_by_key(|b| b.into_raw().into_u32());
        for b in body {
            if let Term::Branch { cond, then_bb, else_bb } = self.body.blocks[b].term {
                if then_bb == region.exit {
                    return Ok((b, cond, else_bb, true));
                }
                if else_bb == region.exit {
                    return Ok((b, cond, then_bb, false));
                }
            }
        }
        Err(err(
            self.span,
            "WGSL emission reached a loop with no single condition test",
        ))
    }

    fn trim_trailing_continue(&mut self, start: usize) {
        let line = format!("{}continue;\n", "  ".repeat(self.indent));
        if self.out.len() >= start + line.len() && self.out.ends_with(&line) {
            let cut = self.out.len() - line.len();
            if cut >= start && (cut == 0 || self.out.as_bytes()[cut - 1] == b'\n') {
                self.out.truncate(cut);
            }
        }
    }

    fn is_stored_cell(&self, cell: Idx<Cell>) -> bool {
        self.body.blocks.iter().any(|(_, block)| {
            block
                .insts
                .iter()
                .any(|iid| matches!(self.body.insts[*iid], Inst::Store { cell: stored, .. } if stored == cell))
        })
    }

    fn value_kind(&self, iid: Idx<Inst>) -> Result<ValueKind, CompilerError> {
        match &self.body.insts[iid] {
            Inst::Const(Lit::Float(_)) => Ok(ValueKind::F32),
            Inst::Const(Lit::Int(_)) => Ok(ValueKind::I32),
            Inst::Const(Lit::Uint(_)) => Ok(ValueKind::U32),
            Inst::Const(Lit::Bool(_)) => Ok(ValueKind::Bool),
            Inst::Const(Lit::Nil) | Inst::Const(Lit::Str(_)) => Err(err(
                self.span,
                "WGSL emission does not support null or string literals",
            )),
            Inst::Load(cell) => self.cell_kinds.get(cell).copied().ok_or_else(|| {
                err(
                    self.span,
                    format!(
                        "WGSL load of `{}` in `{}` has no neutral type",
                        self.cell_names[cell],
                        self.instance.inst.symbol(self.ws, self.interner)
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
            Inst::UintAdd(..)
            | Inst::UintSub(..)
            | Inst::UintMul(..)
            | Inst::UintDiv(..)
            | Inst::UintMod(..) => Ok(ValueKind::U32),
            Inst::FloatAdd(..)
            | Inst::FloatSub(..)
            | Inst::FloatMul(..)
            | Inst::FloatDiv(..)
            | Inst::FloatMod(..) => Ok(ValueKind::F32),
            Inst::IntLt(..)
            | Inst::IntLeq(..)
            | Inst::IntGt(..)
            | Inst::IntGeq(..)
            | Inst::UintLt(..)
            | Inst::UintLeq(..)
            | Inst::UintGt(..)
            | Inst::UintGeq(..)
            | Inst::FloatLt(..)
            | Inst::FloatLeq(..)
            | Inst::FloatGt(..)
            | Inst::FloatGeq(..)
            | Inst::Eq(..)
            | Inst::NotEq(..)
            | Inst::Not(_) => Ok(ValueKind::Bool),
            Inst::FloatToInt(_) => Ok(ValueKind::I32),
            Inst::IntToFloat(_) => Ok(ValueKind::F32),
            Inst::UintToFloat(_) => Ok(ValueKind::F32),
            Inst::IntToUint(_) | Inst::FloatToUint(_) | Inst::IntBitcastUint(_) => {
                Ok(ValueKind::U32)
            }
            Inst::UintToInt(_) | Inst::UintBitcastInt(_) => Ok(ValueKind::I32),
            Inst::UintBitNot(_)
            | Inst::UintBitAnd(..)
            | Inst::UintBitOr(..)
            | Inst::UintBitXor(..)
            | Inst::UintShl(..)
            | Inst::UintShr(..) => Ok(ValueKind::U32),
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

    fn field_kind(&self, base: Idx<Inst>, field: Ustr) -> Result<ValueKind, CompilerError> {
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
                Some(ParamRole::UniformArray) => {
                    return Err(err(
                        self.span,
                        format!(
                            "WGSL uniform array `{}` has no field `{field}`; read it with `at`",
                            self.cell_names[&cell]
                        ),
                    ));
                }
                Some(ParamRole::Storage) => {
                    return Err(err(
                        self.span,
                        format!("WGSL storage resource `{}` is only accessed through storage intrinsics", self.cell_names[&cell]),
                    ));
                }
                Some(ParamRole::Attribute)
                | Some(ParamRole::Instance)
                | Some(ParamRole::Varying)
                | Some(ParamRole::TexCoord)
                | None => {
                    if let Some(ty) = self.body.cells[cell].ty {
                        if let Some(kind) = struct_field_kind(self.ws, ty, field) {
                            return Ok(kind);
                        }
                    }
                }
            }
        }
        let base_kind = self.value_kind(base)?;
        if let Some(kind) = swizzle_kind(base_kind, field.as_str()) {
            return Ok(kind);
        }
        match base_kind {
            ValueKind::Struct(name) => struct_field_kind_by_name(self.ws, name, field).ok_or_else(|| {
                err(
                    self.span,
                    format!("WGSL field `{field}` is not present in value struct `{name}`"),
                )
            }),
            _ => Err(err(
                self.span,
                format!("WGSL field `{field}` is not a supported projection"),
            )),
        }
    }

    fn call_kind(
        &self,
        iid: Idx<Inst>,
        func: skull_mir::mir::FuncRef,
        args: &[Idx<Inst>],
    ) -> Result<ValueKind, CompilerError> {
        let function = &self.ws.module(func.module).functions[func.func];
        match WgslIntrinsic::classify(function) {
            Some(
                WgslIntrinsic::GlobalInvocationId
                | WgslIntrinsic::LocalInvocationId
                | WgslIntrinsic::WorkgroupId
                | WgslIntrinsic::NumWorkgroups,
            ) => {
                Ok(ValueKind::Struct(Ustr::from("GlobalInvocationId")))
            }
            Some(WgslIntrinsic::LocalInvocationIndex) => Ok(ValueKind::U32),
            Some(WgslIntrinsic::StorageLoad) => {
                let cell = self.direct_resource_cell(args, "storage_load")?;
                match self.compute_resource(cell)?.element {
                    SharedWgslStorageElementKind::F32 => Ok(ValueKind::F32),
                    SharedWgslStorageElementKind::I32 => Ok(ValueKind::I32),
                    SharedWgslStorageElementKind::U32 => Ok(ValueKind::U32),
                    SharedWgslStorageElementKind::AtomicU32 => Err(err(
                        self.span,
                        "storage_load cannot read an AtomicStorageBuffer; use atomic_add",
                    )),
                }
            }
            Some(WgslIntrinsic::StorageStore) => Ok(ValueKind::I32),
            Some(WgslIntrinsic::WorkgroupArray) => {
                Ok(ValueKind::Struct(Ustr::from("WorkgroupArray")))
            }
            Some(WgslIntrinsic::WorkgroupLoad) => {
                let cell = self.direct_workgroup_cell(args, "workgroup_load")?;
                Ok(self.workgroup_cells[&cell].1)
            }
            Some(WgslIntrinsic::WorkgroupStore) => Ok(ValueKind::I32),
            Some(WgslIntrinsic::AtomicAdd) => Ok(ValueKind::U32),
            Some(WgslIntrinsic::WorkgroupBarrier) => Err(err(
                self.span,
                "workgroup_barrier produces no value",
            )),
            Some(WgslIntrinsic::VertexIndex) => Ok(ValueKind::I32),
            Some(WgslIntrinsic::Texture) => Ok(ValueKind::Vec4),
            Some(WgslIntrinsic::TextureSize) => Ok(ValueKind::Vec2),
            Some(WgslIntrinsic::FragCoord) => Ok(ValueKind::Vec4),
            Some(WgslIntrinsic::Discard) => Err(err(
                self.span,
                "WGSL `discard()` produces no value and cannot be used in an expression",
            )),
            Some(WgslIntrinsic::At) => {
                let member = self.array_member(args)?;
                wire_value_kind(member.elem).ok_or_else(|| {
                    err(
                        self.span,
                        format!("WGSL uniform array `{}` has no emitted element", member.name),
                    )
                })
            }
            Some(WgslIntrinsic::Zero) => Ok(ValueKind::Vec2),
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
            Some(WgslIntrinsic::Round) => self.first_arg_kind(args),
            Some(WgslIntrinsic::Builtin(_, ResultKind::Fixed(kind))) => Ok(kind),
            Some(WgslIntrinsic::Builtin(_, ResultKind::FirstArg)) => {
                // `clamp(vec, 0.0, 6.0)` splats its scalar bounds, so the widest
                // operand -- not positionally the first -- is the result.
                let mut widest = self.first_arg_kind(args)?;
                for arg in args {
                    let kind = self.value_kind(*arg)?;
                    if kind.components() > widest.components() {
                        widest = kind;
                    }
                }
                Ok(widest)
            }
            Some(WgslIntrinsic::Operator(operator)) => self.operator_kind(operator, args),
            Some(WgslIntrinsic::UnsupportedOperator) => Err(unsupported_err(
                self.span,
                format!("unsupported WGSL operator `{}`", function.name),
                format!("WGSL operator `{}` is not admitted", function.name),
            )),
            None => {
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

    fn first_arg_kind(&self, args: &[Idx<Inst>]) -> Result<ValueKind, CompilerError> {
        self.value_kind(
            args.first()
                .copied()
                .ok_or_else(|| err(self.span, "WGSL intrinsic call has no operand"))?,
        )
    }

    /// The result of a non-scalar `__op_*`. `mat4 * vec4` is the case a
    /// first-operand rule gets wrong, and a matrix product is the case a
    /// widest-operand rule gets wrong, so both are named.
    fn operator_kind(
        &self,
        operator: WgslOperator,
        args: &[Idx<Inst>],
    ) -> Result<ValueKind, CompilerError> {
        if args.len() != 2 {
            return Err(err(self.span, "WGSL operator requires two operands"));
        }
        let a = self.value_kind(args[0])?;
        let b = self.value_kind(args[1])?;
        Ok(match (operator, a, b) {
            (WgslOperator::Mul, ValueKind::Mat4, ValueKind::Mat4) => ValueKind::Mat4,
            (WgslOperator::Mul, ValueKind::Mat4, other) | (WgslOperator::Mul, other, ValueKind::Mat4) => {
                other
            }
            _ if a.components() >= b.components() => a,
            _ => b,
        })
    }

    fn array_member(&self, args: &[Idx<Inst>]) -> Result<&WgslArrayMember, CompilerError> {
        let Some(&target) = args.first() else {
            return Err(err(self.span, "WGSL `at` has no array operand"));
        };
        let Inst::Load(cell) = self.body.insts[target] else {
            return Err(err(
                self.span,
                "WGSL `at` did not receive a uniform array parameter",
            ));
        };
        if !matches!(self.param_roles.get(&cell), Some(ParamRole::UniformArray)) {
            return Err(err(
                self.span,
                "WGSL `at` did not receive a uniform array parameter",
            ));
        }
        let name = Ustr::from(&self.cell_names[&cell]);
        self.array_block()?
            .members
            .iter()
            .find(|member| member.name == name)
            .ok_or_else(|| {
                err(
                    self.span,
                    format!("WGSL uniform array `{name}` is absent from the ABI plan"),
                )
            })
    }

    fn array_block(&self) -> Result<&WgslArrayBlock, CompilerError> {
        self.abi
            .array_blocks
            .iter()
            .find(|block| block.stage == self.stage.stage)
            .ok_or_else(|| {
                err(
                    self.span,
                    "WGSL stage reads a uniform array with no array block in the ABI plan",
                )
            })
    }

    fn resolve_callee(
        &self,
        iid: Idx<Inst>,
        func: skull_mir::mir::FuncRef,
    ) -> Result<ShaderInstance, CompilerError> {
        let function = &self.ws.module(func.module).functions[func.func];
        let instance = self
            .body
            .callee_instances
            .get(&iid)
            .cloned()
            .map(ShaderInstance::new)
            .or_else(|| function.generics.is_empty().then(|| ShaderInstance::plain(func)));
        if let Some(instance) = instance {
            if self.stage.order.iter().any(|candidate| candidate == &instance) {
                return Ok(instance);
            }
        }
        if let Some(capability) = WgslShaderPolicy.unsupported_capability(function) {
            return Err(unsupported_err(
                self.span,
                capability,
                format!(
                    "WGSL call to `{}` has no concrete emitted instance",
                    function.name
                ),
            ));
        }
        Err(err(
            self.span,
            format!(
                "WGSL call to `{}` has no concrete emitted instance",
                function.name
            ),
        ))
    }

    fn render_val(&self, iid: Idx<Inst>) -> Result<String, CompilerError> {
        match &self.body.insts[iid] {
            Inst::Const(Lit::Float(value)) => Ok(format_float(*value)),
            Inst::Const(Lit::Int(value)) => Ok(value.to_string()),
            Inst::Const(Lit::Uint(value)) => Ok(format!("{value}u")),
            Inst::Const(Lit::Bool(value)) => Ok(value.to_string()),
            Inst::Const(Lit::Nil) | Inst::Const(Lit::Str(_)) => Err(err(
                self.span,
                "WGSL emission does not support null or string literals",
            )),
            Inst::Load(cell) => {
                if let Some((name, _)) = self.workgroup_cells.get(cell) {
                    return Ok(name.clone());
                }
                if self.sampler_params.contains(cell) {
                    return Ok(self.sampler_param_names(*cell).0);
                }
                match self.param_roles.get(cell).copied() {
                Some(ParamRole::TexCoord) => Ok("uv".to_string()),
                Some(ParamRole::Storage)
                | Some(ParamRole::Texture)
                | Some(ParamRole::Uniform(_)) => Err(err(
                    self.span,
                    format!(
                        "WGSL resource cell `{}` must be accessed through a field",
                        self.cell_names[cell]
                    ),
                )),
                Some(ParamRole::UniformArray) => Err(err(
                    self.span,
                    format!(
                        "WGSL uniform array `{}` must be read through `at`",
                        self.cell_names[cell]
                    ),
                )),
                Some(ParamRole::Attribute) | Some(ParamRole::Instance) | Some(ParamRole::Varying) | None => {
                    Ok(self.cell_names[cell].clone())
                }
                }
            }
            Inst::Field { base, field } => {
                if let Inst::Load(cell) = self.body.insts[*base] {
                    match self.param_roles.get(&cell).copied() {
                        Some(ParamRole::Texture) => {
                            self.abi
                                .textures
                                .iter()
                                .find(|texture| texture.name == *field)
                                .ok_or_else(|| {
                                    err(
                                        self.span,
                                        format!(
                                            "WGSL texture field `{field}` is absent from the ABI plan"
                                        ),
                                    )
                                })?;
                            return Ok(format!("tex_{field}"));
                        }
                        Some(ParamRole::Uniform(role)) => {
                            let block = self.abi.uniform_block(role).ok_or_else(|| {
                                err(
                                    self.span,
                                    format!(
                                        "WGSL uniform role for `{field}` is absent from the ABI plan"
                                    ),
                                )
                            })?;
                            let member = block
                                .members
                                .iter()
                                .find(|member| member.name == *field)
                                .ok_or_else(|| {
                                    err(
                                        self.span,
                                        format!(
                                            "WGSL uniform field `{field}` is absent from the ABI plan"
                                        ),
                                    )
                                })?;
                            let read = format!("u_{}.{}", block.struct_name, field);
                            // A `bool` uniform crosses as the u32 the CPU wrote;
                            // the boolean is recovered here and nowhere else.
                            return Ok(match member.kind {
                                WireKind::Bool32 => format!("({read} != 0u)"),
                                _ => read,
                            });
                        }
                        Some(ParamRole::Storage) => {
                            return Err(err(
                                self.span,
                                format!("WGSL storage resource `{}` is only accessed through storage intrinsics", self.cell_names[&cell]),
                            ));
                        }
                        Some(ParamRole::Attribute)
                        | Some(ParamRole::Instance)
                        | Some(ParamRole::Varying)
                        | Some(ParamRole::TexCoord)
                        | Some(ParamRole::UniformArray)
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
                let ordered = self.ordered_struct_args(*name, kind, fields)?;
                Ok(format!("{}({})", wgsl_value_type(kind).unwrap(), ordered.join(", ")))
            }
            Inst::Call { func, args } => self.render_call(iid, *func, args),
            Inst::IntAdd(a, b) | Inst::FloatAdd(a, b) => self.binary(*a, "+", *b),
            Inst::UintAdd(a, b) => self.binary(*a, "+", *b),
            Inst::IntSub(a, b) | Inst::FloatSub(a, b) => self.binary(*a, "-", *b),
            Inst::UintSub(a, b) => self.binary(*a, "-", *b),
            Inst::IntMul(a, b) | Inst::FloatMul(a, b) => self.binary(*a, "*", *b),
            Inst::UintMul(a, b) => self.binary(*a, "*", *b),
            Inst::IntDiv(a, b) | Inst::FloatDiv(a, b) => self.binary(*a, "/", *b),
            Inst::UintDiv(a, b) => self.binary(*a, "/", *b),
            Inst::IntMod(a, b) | Inst::FloatMod(a, b) => self.binary(*a, "%", *b),
            Inst::UintMod(a, b) => self.binary(*a, "%", *b),
            Inst::IntLt(a, b) | Inst::FloatLt(a, b) => self.binary(*a, "<", *b),
            Inst::UintLt(a, b) => self.binary(*a, "<", *b),
            Inst::IntLeq(a, b) | Inst::FloatLeq(a, b) => self.binary(*a, "<=", *b),
            Inst::UintLeq(a, b) => self.binary(*a, "<=", *b),
            Inst::IntGt(a, b) | Inst::FloatGt(a, b) => self.binary(*a, ">", *b),
            Inst::UintGt(a, b) => self.binary(*a, ">", *b),
            Inst::IntGeq(a, b) | Inst::FloatGeq(a, b) => self.binary(*a, ">=", *b),
            Inst::UintGeq(a, b) => self.binary(*a, ">=", *b),
            Inst::UintBitAnd(a, b) => self.binary(*a, "&", *b),
            Inst::UintBitOr(a, b) => self.binary(*a, "|", *b),
            Inst::UintBitXor(a, b) => self.binary(*a, "^", *b),
            Inst::UintShl(a, b) => self.binary(*a, "<<", *b),
            Inst::UintShr(a, b) => self.binary(*a, ">>", *b),
            Inst::Eq(a, b) => self.binary(*a, "==", *b),
            Inst::NotEq(a, b) => self.binary(*a, "!=", *b),
            Inst::FloatToInt(value) => {
                if self.stage.stage == ShaderStage::Compute {
                    Ok(format!("i32(floor({}))", self.render_val(*value)?))
                } else {
                    Ok(format!("i32({})", self.render_val(*value)?))
                }
            }
            Inst::IntToFloat(value) => {
                self.reject_compute_numeric_conversion("int as float", *value)?;
                Ok(format!("f32({})", self.render_val(*value)?))
            }
            Inst::UintToFloat(value) => {
                self.reject_compute_numeric_conversion("uint as float", *value)?;
                Ok(format!("f32({})", self.render_val(*value)?))
            }
            Inst::IntToUint(value) => {
                self.reject_compute_numeric_conversion("int as uint", *value)?;
                Ok(format!("u32({})", self.render_val(*value)?))
            }
            Inst::FloatToUint(value) => {
                self.reject_compute_numeric_conversion("float as uint", *value)?;
                Ok(format!("u32({})", self.render_val(*value)?))
            }
            Inst::UintToInt(value) => {
                self.reject_compute_numeric_conversion("uint as int", *value)?;
                Ok(format!("i32({})", self.render_val(*value)?))
            }
            Inst::UintBitcastInt(value) => {
                Ok(format!("bitcast<i32>({})", self.render_val(*value)?))
            }
            Inst::IntBitcastUint(value) => {
                Ok(format!("bitcast<u32>({})", self.render_val(*value)?))
            }
            Inst::Neg(value) => Ok(format!("(-{})", self.render_val(*value)?)),
            Inst::Not(value) => Ok(format!("(!{})", self.render_val(*value)?)),
            Inst::UintBitNot(value) => Ok(format!("(~{})", self.render_val(*value)?)),
            other => Err(err(
                self.span,
                format!(
                    "WGSL reaches unsupported value operation `{}`",
                    mir_kind(other)
                ),
            )),
        }
    }

    fn reject_compute_numeric_conversion(
        &self,
        conversion: &str,
        value: Idx<Inst>,
    ) -> Result<(), CompilerError> {
        if self.stage.stage == ShaderStage::Compute && !self.safe_compute_numeric_constant(conversion, value) {
            return Err(err(
                self.span,
                format!(
                    "WGSL compute does not support dynamic numeric conversion `{conversion}`; use bitcast<int> or bitcast<uint> for bit reinterpretation"
                ),
            ));
        }
        Ok(())
    }

    fn safe_compute_numeric_constant(&self, conversion: &str, value: Idx<Inst>) -> bool {
        match (conversion, &self.body.insts[value]) {
            ("float as int", Inst::Const(Lit::Float(value))) => {
                value.is_finite() && value.fract() == 0.0 && *value >= 0.0 && *value <= i32::MAX as f64
            }
            ("int as float", Inst::Const(Lit::Int(value))) => {
                (-16_777_216..=16_777_216).contains(value)
            }
            ("uint as float", Inst::Const(Lit::Uint(value))) => *value <= 16_777_216,
            ("int as uint", Inst::Const(Lit::Int(value))) => (0..=u32::MAX as i64).contains(value),
            ("float as uint", Inst::Const(Lit::Float(value))) => {
                value.is_finite() && value.fract() == 0.0 && *value >= 0.0 && *value <= 16_777_216.0
            }
            ("uint as int", Inst::Const(Lit::Uint(value))) => *value <= i32::MAX as u32,
            _ => false,
        }
    }

    /// A WGSL struct constructor is POSITIONAL, so the literal's field order is
    /// not enough -- the declaration's is. A field the declaration does not name
    /// rejects rather than landing in a neighbouring slot.
    fn ordered_struct_args(
        &self,
        name: Ustr,
        kind: ValueKind,
        fields: &[(Ustr, Idx<Inst>)],
    ) -> Result<Vec<String>, CompilerError> {
        let ValueKind::Struct(_) = kind else {
            return fields
                .iter()
                .map(|(_, value)| self.render_val(*value))
                .collect();
        };
        let declared = explicit_struct_fields(self.ws, name, self.span)?;
        if declared.len() != fields.len() {
            return Err(err(
                self.span,
                format!(
                    "WGSL value struct `{name}` literal has {} fields, declaration has {}",
                    fields.len(),
                    declared.len()
                ),
            ));
        }
        declared
            .iter()
            .map(|(field, _)| {
                let value = fields
                    .iter()
                    .find(|(candidate, _)| candidate == field)
                    .ok_or_else(|| {
                        err(
                            self.span,
                            format!("WGSL value struct `{name}` literal is missing field `{field}`"),
                        )
                    })?;
                self.render_val(value.1)
            })
            .collect()
    }

    fn binary(&self, a: Idx<Inst>, op: &str, b: Idx<Inst>) -> Result<String, CompilerError> {
        Ok(format!(
            "({} {} {})",
            self.render_val(a)?,
            op,
            self.render_val(b)?
        ))
    }

    fn compute_resource(
        &self,
        cell: Idx<Cell>,
    ) -> Result<&SharedWgslComputeResource, CompilerError> {
        let name = &self.cell_names[&cell];
        self.abi
            .compute
            .as_ref()
            .and_then(|compute| compute.resources.iter().find(|resource| resource.name == *name))
            .ok_or_else(|| {
                err(
                    self.span,
                    format!("WGSL compute resource `{name}` is absent from the ABI"),
                )
            })
    }

    fn direct_resource_cell(
        &self,
        args: &[Idx<Inst>],
        intrinsic: &str,
    ) -> Result<Idx<Cell>, CompilerError> {
        let Some(first) = args.first() else {
            return Err(err(self.span, format!("WGSL {intrinsic} has no resource argument")));
        };
        let Inst::Load(cell) = self.body.insts[*first] else {
            return Err(err(
                self.span,
                format!("WGSL {intrinsic} requires a direct storage parameter"),
            ));
        };
        if !matches!(self.param_roles.get(&cell), Some(ParamRole::Storage)) {
            return Err(err(
                self.span,
                format!("WGSL {intrinsic} requires a storage parameter"),
            ));
        }
        Ok(cell)
    }

    fn direct_workgroup_cell(
        &self,
        args: &[Idx<Inst>],
        intrinsic: &str,
    ) -> Result<Idx<Cell>, CompilerError> {
        let Some(first) = args.first() else {
            return Err(err(self.span, format!("WGSL {intrinsic} has no workgroup argument")));
        };
        let Inst::Load(cell) = self.body.insts[*first] else {
            return Err(err(
                self.span,
                format!("WGSL {intrinsic} requires a direct WorkgroupArray value"),
            ));
        };
        if !self.workgroup_cells.contains_key(&cell) {
            return Err(err(
                self.span,
                format!("WGSL {intrinsic} received a non-workgroup value"),
            ));
        }
        Ok(cell)
    }

    fn render_call(
        &self,
        iid: Idx<Inst>,
        func: skull_mir::mir::FuncRef,
        args: &[Idx<Inst>],
    ) -> Result<String, CompilerError> {
        let function = &self.ws.module(func.module).functions[func.func];
        let name = function.name.as_str();
        match WgslIntrinsic::classify(function) {
            Some(
                WgslIntrinsic::GlobalInvocationId
                | WgslIntrinsic::LocalInvocationId
                | WgslIntrinsic::WorkgroupId
                | WgslIntrinsic::NumWorkgroups,
            ) => {
                if !args.is_empty() {
                    return Err(err(self.span, format!("WGSL {name} takes no arguments")));
                }
                let invocation = match WgslIntrinsic::classify(function) {
                    Some(WgslIntrinsic::GlobalInvocationId) => self.compute_invocation.as_ref(),
                    Some(WgslIntrinsic::LocalInvocationId) => self.compute_local_invocation.as_ref(),
                    Some(WgslIntrinsic::WorkgroupId) => self.compute_workgroup_id.as_ref(),
                    Some(WgslIntrinsic::NumWorkgroups) => self.compute_num_workgroups.as_ref(),
                    _ => None,
                };
                let Some(invocation) = invocation else {
                    return Err(err(
                        self.span,
                        format!("{name} is only available in a compute entry"),
                    ));
                };
                Ok(format!(
                    "GlobalInvocationId(i32({invocation}.x), i32({invocation}.y), i32({invocation}.z))"
                ))
            }
            Some(WgslIntrinsic::LocalInvocationIndex) => {
                if !args.is_empty() {
                    return Err(err(self.span, "WGSL local_invocation_index takes no arguments"));
                }
                self.compute_local_index.clone().ok_or_else(|| {
                    err(self.span, "local_invocation_index is only available in a compute entry")
                })
            }
            Some(WgslIntrinsic::StorageLoad) => {
                let [_buffer, index] = args else {
                    return Err(err(self.span, "WGSL storage_load requires a buffer and index"));
                };
                let cell = self.direct_resource_cell(args, "storage_load")?;
                let resource = self.compute_resource(cell)?;
                if resource.element == SharedWgslStorageElementKind::AtomicU32 {
                    return Err(err(self.span, "storage_load cannot access AtomicStorageBuffer"));
                }
                Ok(format!("s_{}[{}]", resource.name, self.render_val(*index)?))
            }
            Some(WgslIntrinsic::StorageStore) => {
                let [_buffer, index, value] = args else {
                    return Err(err(self.span, "WGSL storage_store requires a buffer, index, and value"));
                };
                let cell = self.direct_resource_cell(args, "storage_store")?;
                let resource = self.compute_resource(cell)?;
                if resource.element == SharedWgslStorageElementKind::AtomicU32 {
                    return Err(err(self.span, "storage_store cannot access AtomicStorageBuffer"));
                }
                if resource.access != SharedWgslStorageAccess::ReadWrite {
                    return Err(err(
                        self.span,
                        format!("WGSL storage_store requires read_write resource `{}`", resource.name),
                    ));
                }
                Ok(format!(
                    "s_{}[{}] = {}",
                    resource.name,
                    self.render_val(*index)?,
                    self.render_val(*value)?
                ))
            }
            Some(WgslIntrinsic::WorkgroupArray) => Err(err(
                self.span,
                "WorkgroupArray handles may only initialize a named local workgroup declaration",
            )),
            Some(WgslIntrinsic::WorkgroupLoad) => {
                let [_array, index] = args else {
                    return Err(err(self.span, "WGSL workgroup_load requires an array and index"));
                };
                let cell = self.direct_workgroup_cell(args, "workgroup_load")?;
                let (name, _) = &self.workgroup_cells[&cell];
                Ok(format!("{}[{}]", name, self.render_val(*index)?))
            }
            Some(WgslIntrinsic::WorkgroupStore) => {
                let [_array, index, value] = args else {
                    return Err(err(self.span, "WGSL workgroup_store requires an array, index, and value"));
                };
                let cell = self.direct_workgroup_cell(args, "workgroup_store")?;
                let (name, _) = &self.workgroup_cells[&cell];
                Ok(format!("{}[{}] = {}", name, self.render_val(*index)?, self.render_val(*value)?))
            }
            Some(WgslIntrinsic::AtomicAdd) => {
                let [_buffer, index, value] = args else {
                    return Err(err(self.span, "WGSL atomic_add requires a buffer, index, and value"));
                };
                let cell = self.direct_resource_cell(args, "atomic_add")?;
                let resource = self.compute_resource(cell)?;
                if resource.element != SharedWgslStorageElementKind::AtomicU32 {
                    return Err(err(self.span, "atomic_add requires AtomicStorageBuffer<uint>"));
                }
                if resource.access != SharedWgslStorageAccess::ReadWrite {
                    return Err(err(self.span, "atomic_add requires a read_write atomic resource"));
                }
                Ok(format!(
                    "atomicAdd(&s_{}[{}], {})",
                    resource.name,
                    self.render_val(*index)?,
                    self.render_val(*value)?
                ))
            }
            Some(WgslIntrinsic::WorkgroupBarrier) => {
                if !args.is_empty() {
                    return Err(err(self.span, "WGSL workgroup_barrier takes no arguments"));
                }
                Ok("workgroupBarrier()".to_string())
            }
            Some(WgslIntrinsic::Texture) => {
                if args.len() != 2 {
                    return Err(err(
                        self.span,
                        "WGSL texture intrinsic requires one logical sampler and one UV",
                    ));
                }
                if self.value_kind(args[0])? != ValueKind::Sampler(SamplerKind::Float) {
                    return Err(err(
                        self.span,
                        "WGSL emission does not yet admit integer texture sampling",
                    ));
                }
                let (texture_arg, sampler_arg) = self.sampler_expressions(args[0])?;
                Ok(format!(
                    "textureSample({texture_arg}, {sampler_arg}, {})",
                    self.render_val(args[1])?
                ))
            }
            Some(WgslIntrinsic::TextureSize) => {
                let (texture_arg, _) = self.sampler_expressions(args[0])?;
                Ok(format!("vec2<f32>(textureDimensions({texture_arg}))"))
            }
            Some(WgslIntrinsic::FragCoord) => self.frag_coord.clone().ok_or_else(|| {
                err(
                    self.span,
                    "WGSL `frag_coord()` is only available in the fragment entry, which is where \
                     the builtin position enters; a helper cannot reach it",
                )
            }),
            Some(WgslIntrinsic::VertexIndex) => {
                if !args.is_empty() {
                    return Err(err(self.span, "WGSL vertex_id() takes no arguments"));
                }
                self.vertex_index
                    .as_ref()
                    .map(|name| format!("i32({name})"))
                    .ok_or_else(|| err(self.span, "WGSL vertex_id() is only available in the vertex entry"))
            }
            Some(WgslIntrinsic::Discard) => Ok("discard".to_string()),
            Some(WgslIntrinsic::At) => {
                let member = self.array_member(args)?;
                let block = self.array_block()?;
                let projection = array_element_projection(member.elem).ok_or_else(|| {
                    err(
                        self.span,
                        format!(
                            "WGSL uniform array `{}` has no element projection",
                            member.name
                        ),
                    )
                })?;
                Ok(format!(
                    "u_{}.{}[{}]{}",
                    block.struct_name,
                    member.name,
                    self.render_val(args[1])?,
                    projection
                ))
            }
            Some(WgslIntrinsic::Zero) => Ok("vec2<f32>(0.0)".to_string()),
            Some(WgslIntrinsic::VectorConstructor) => {
                let kind = self.call_kind(iid, func, args)?;
                let rendered = args
                    .iter()
                    .map(|arg| self.render_val(*arg))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(format!("{}({})", wgsl_value_type(kind).unwrap(), rendered.join(", ")))
            }
            Some(WgslIntrinsic::Round) => {
                Ok(format!("floor({} + 0.5)", self.render_val(args[0])?))
            }
            Some(WgslIntrinsic::Builtin(spelling, _)) => {
                let target = self.call_kind(iid, func, args)?;
                let rendered = args
                    .iter()
                    .map(|arg| self.render_intrinsic_arg(*arg, target))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(format!("{}({})", spelling, rendered.join(", ")))
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
            Some(WgslIntrinsic::UnsupportedOperator) => Err(unsupported_err(
                self.span,
                format!("unsupported WGSL operator `{name}`"),
                format!("WGSL operator `{name}` is not admitted"),
            )),
            None => {
                let instance = self.resolve_callee(iid, func)?;
                let self_name = instance.inst.symbol(self.ws, self.interner);
                let target_body = self.stage.bodies.get(&instance).ok_or_else(|| {
                    err(
                        self.span,
                        format!("WGSL call target `{}` has no emitted body", function.name),
                    )
                })?;
                let target_sampler_params = self.stage_sampler_params.get(&instance);
                let mut rendered = Vec::new();
                for (index, arg) in args.iter().enumerate() {
                    let compute_resource = function
                        .param_decls
                        .get(index)
                        .is_some_and(|param| is_compute_resource_type(self.ws, param.ty));
                    if compute_resource {
                        continue;
                    }
                    let sampler_resource = target_body
                        .params
                        .get(index)
                        .is_some_and(|cell| {
                            target_sampler_params
                                .is_some_and(|params| params.contains(cell))
                        });
                    if sampler_resource {
                        let (texture, sampler) = self.sampler_expressions(*arg)?;
                        rendered.push(texture);
                        rendered.push(sampler);
                    } else {
                        rendered.push(self.render_val(*arg)?);
                    }
                }
                Ok(format!("{}({})", self_name, rendered.join(", ")))
            }
        }
    }

    fn render_intrinsic_arg(
        &self,
        arg: Idx<Inst>,
        target: ValueKind,
    ) -> Result<String, CompilerError> {
        let rendered = self.render_val(arg)?;
        if target.components() > 1 && self.value_kind(arg)?.components() == 1 {
            Ok(format!("{}({})", wgsl_value_type(target).unwrap(), rendered))
        } else {
            Ok(rendered)
        }
    }

    fn sampler_binding(&self, iid: Idx<Inst>) -> Result<&WgslTextureBinding, CompilerError> {
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

    fn sampler_param_names(&self, cell: Idx<Cell>) -> (String, String) {
        let suffix = cell.into_raw().into_u32();
        (
            format!("rock_sampler_tex_{suffix}"),
            format!("rock_sampler_samp_{suffix}"),
        )
    }

    fn sampler_expressions(&self, iid: Idx<Inst>) -> Result<(String, String), CompilerError> {
        if let Inst::Load(cell) = self.body.insts[iid] {
            if self.sampler_params.contains(&cell) {
                let (texture, sampler) = self.sampler_param_names(cell);
                return Ok((texture, sampler));
            }
        }
        let texture = self.sampler_binding(iid)?;
        Ok((
            format!("tex_{}", texture.name),
            format!("samp_{}", texture.name),
        ))
    }
}

/// `Some(LoopInfo)` iff `header` is a `for i in lo..hi` header, mirroring the
/// GLSL lane's recognition: a `Branch` on `IntLt/IntLeq(load slot, bound)` whose
/// slot carries `lower_for_range`'s `loop_binding` provenance, entered by a
/// pure-increment latch.
fn match_counted_loop(f: &MirFunc, header: Idx<Block>) -> Option<LoopInfo> {
    let Term::Branch { cond, then_bb, else_bb } = f.blocks[header].term else {
        return None;
    };
    let (slot, bound, inclusive) = match &f.insts[cond] {
        Inst::IntLt(a, b) => (load_cell(f, *a)?, *b, false),
        Inst::IntLeq(a, b) => (load_cell(f, *a)?, *b, true),
        _ => return None,
    };
    if !f.cells[slot].loop_binding {
        return None;
    }
    let mut latch = None;
    for (bid, block) in f.blocks.iter() {
        if block.term == Term::Goto(header) && is_incr_latch(f, bid, slot) {
            latch = Some(bid);
        }
    }
    Some(LoopInfo {
        slot,
        bound,
        inclusive,
        body_entry: then_bb,
        latch: latch?,
        exit: else_bb,
    })
}

/// The blocks reachable from the entry, by forward walk.
fn reachable_blocks(f: &MirFunc) -> HashSet<Idx<Block>> {
    let mut seen = HashSet::new();
    let mut stack = vec![f.entry];
    while let Some(b) = stack.pop() {
        if !seen.insert(b) {
            continue;
        }
        match &f.blocks[b].term {
            Term::Goto(t) => stack.push(*t),
            Term::Branch { then_bb, else_bb, .. } => {
                stack.push(*then_bb);
                stack.push(*else_bb);
            }
            Term::Switch { cases, default, .. } => {
                stack.extend(cases.iter().map(|(_, b)| *b));
                stack.push(*default);
            }
            Term::Yield { resume, .. } => stack.push(*resume),
            Term::Return(_) | Term::Unreachable => {}
        }
    }
    seen
}

fn load_cell(f: &MirFunc, iid: Idx<Inst>) -> Option<Idx<Cell>> {
    match f.insts[iid] {
        Inst::Load(cell) => Some(cell),
        _ => None,
    }
}

fn is_incr_latch(f: &MirFunc, p: Idx<Block>, slot: Idx<Cell>) -> bool {
    for &iid in &f.blocks[p].insts {
        if let Inst::Store { cell, value } = f.insts[iid] {
            if cell == slot {
                if let Inst::IntAdd(a, b) = f.insts[value] {
                    let is_one = |x: Idx<Inst>| matches!(f.insts[x], Inst::Const(Lit::Int(1)));
                    let loads_slot = |x: Idx<Inst>| matches!(f.insts[x], Inst::Load(c) if c == slot);
                    return (loads_slot(a) && is_one(b)) || (loads_slot(b) && is_one(a));
                }
                return false;
            }
        }
    }
    false
}

/// The kind of `base.<swizzle>` for a vector base, over both the `xyzw` and
/// `rgba` component sets. A mixed or out-of-range swizzle is not a projection.
fn swizzle_kind(base: ValueKind, field: &str) -> Option<ValueKind> {
    let width = match base {
        ValueKind::Vec2 => 2,
        ValueKind::Vec3 => 3,
        ValueKind::Vec4 => 4,
        _ => return None,
    };
    if field.is_empty() || field.len() > 4 {
        return None;
    }
    let position = |c: char| "xyzw".find(c).or_else(|| "rgba".find(c));
    let mut set = None;
    for c in field.chars() {
        let in_xyzw = "xyzw".contains(c);
        let in_rgba = "rgba".contains(c);
        if !in_xyzw && !in_rgba {
            return None;
        }
        match set {
            Some(previous) if previous != in_xyzw => return None,
            _ => set = Some(in_xyzw),
        }
        if position(c)? >= width {
            return None;
        }
    }
    match field.len() {
        1 => Some(ValueKind::F32),
        2 => Some(ValueKind::Vec2),
        3 => Some(ValueKind::Vec3),
        _ => Some(ValueKind::Vec4),
    }
}

fn struct_field_kind_by_name(ws: &Workspace, name: Ustr, field: Ustr) -> Option<ValueKind> {
    let sid = *ws.struct_by_name.get(&name)?;
    let def = &ws.modules[sid.module.index()].structs[sid.def];
    def.fields
        .iter()
        .find(|candidate| candidate.name == field)
        .and_then(|candidate| value_kind_for_type(ws, candidate.ty))
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
        ValueKind::U32 => "0u".into(),
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
    use super::{ResultKind, ValueKind, WgslIntrinsic, WgslOperator, swizzle_kind};

    #[test]
    fn naga_accepts_opaque_sampler_function_parameters() {
        let source = r#"
@group(0) @binding(0) var source_tex: texture_2d<f32>;
@group(0) @binding(1) var source_sampler: sampler;

fn sample_helper(tex: texture_2d<f32>, samp: sampler, uv: vec2<f32>) -> vec4<f32> {
    return textureSample(tex, samp, uv);
}

@fragment
fn main(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    return sample_helper(source_tex, source_sampler, position.xy);
}
"#;
        let module = naga::front::wgsl::parse_str(source).expect("WGSL grammar accepts the probe");
        naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::all(),
        )
        .validate(&module)
        .expect("WGSL resources may cross a helper function boundary");
    }

    #[test]
    fn intrinsic_classifier_matches_the_admitted_wgsl_surface() {
        assert!(
            WgslIntrinsic::classify_name_arity("texture", 2).is_some_and(WgslIntrinsic::is_admitted)
        );
        assert!(WgslIntrinsic::classify_name_arity("v", 2).is_some_and(WgslIntrinsic::is_admitted));
        assert_eq!(
            WgslIntrinsic::classify_name_arity("clamp", 3),
            Some(WgslIntrinsic::Builtin("clamp", ResultKind::FirstArg))
        );
        assert_eq!(
            WgslIntrinsic::classify_name_arity("length", 1),
            Some(WgslIntrinsic::Builtin(
                "length",
                ResultKind::Fixed(ValueKind::F32)
            ))
        );
        assert_eq!(
            WgslIntrinsic::classify_name_arity("__op_pow", 2),
            Some(WgslIntrinsic::UnsupportedOperator)
        );
        assert_eq!(
            WgslIntrinsic::classify_name_arity("__op_add", 2),
            Some(WgslIntrinsic::Operator(WgslOperator::Add))
        );
        assert_eq!(WgslIntrinsic::classify_name_arity("helper", 1), None);
        // Present in the GLSL table with no faithful WGSL spelling: absent here
        // so the call rejects rather than emitting a plausible substitute.
        for absent in ["isnan", "isinf", "any", "texture_id", "greater_than"] {
            for arity in 0..=3 {
                assert_eq!(
                    WgslIntrinsic::classify_name_arity(absent, arity),
                    None,
                    "`{absent}` must not be admitted"
                );
            }
        }
        assert!(WgslIntrinsic::classify_name_arity("vertex_id", 0).is_some());
    }

    #[test]
    fn swizzles_mix_neither_component_set_nor_exceed_the_base() {
        assert_eq!(swizzle_kind(ValueKind::Vec4, "rgb"), Some(ValueKind::Vec3));
        assert_eq!(swizzle_kind(ValueKind::Vec4, "xy"), Some(ValueKind::Vec2));
        assert_eq!(swizzle_kind(ValueKind::Vec4, "w"), Some(ValueKind::F32));
        assert_eq!(swizzle_kind(ValueKind::Vec2, "z"), None);
        assert_eq!(swizzle_kind(ValueKind::Vec4, "xg"), None);
        assert_eq!(swizzle_kind(ValueKind::F32, "x"), None);
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
