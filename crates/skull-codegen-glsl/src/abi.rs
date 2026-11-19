//! Shader ABI layout and stage declarations.

use super::*;

/// Flatten the scalar blocks then the array blocks for Lua and JS registration.
/// Counts group the members by block. Scalar members have zero capacity and stride.
#[derive(Debug, Default)]
pub struct BlockWire<'a> {
    pub names: Vec<&'a str>,
    pub bindings: Vec<i64>,
    pub sizes: Vec<i64>,
    pub fingerprints: Vec<i64>,
    pub counts: Vec<i64>,
    pub member_names: Vec<&'a str>,
    pub member_offsets: Vec<i64>,
    pub member_kinds: Vec<i64>,
    pub member_capacities: Vec<i64>,
    pub member_strides: Vec<i64>,
}

impl<'a> BlockWire<'a> {
    pub fn of(
        scalar: impl Iterator<Item = &'a (u32, GlslUniformBlock)>,
        arrays: impl Iterator<Item = &'a (u32, UniformArrayBlockPlan)>,
    ) -> Self {
        let mut w = BlockWire::default();
        for (binding, block) in scalar {
            let p = &block.plan;
            w.names.push(block.name.as_str());
            w.bindings.push(*binding as i64);
            w.sizes.push(p.size as i64);
            w.fingerprints.push(p.fingerprint() as i64);
            w.counts.push(p.members.len() as i64);
            for m in &p.members {
                w.member_names.push(m.name.as_str());
                w.member_offsets.push(m.offset as i64);
                w.member_kinds.push(m.kind.code() as i64);
                w.member_capacities.push(0);
                w.member_strides.push(0);
            }
        }
        for (binding, p) in arrays {
            w.names.push(p.block_name.as_str());
            w.bindings.push(*binding as i64);
            w.sizes.push(p.size as i64);
            w.fingerprints.push(p.fingerprint() as i64);
            w.counts.push(p.members.len() as i64);
            for m in &p.members {
                w.member_names.push(m.name.as_str());
                w.member_offsets.push(m.offset as i64);
                w.member_kinds.push(m.elem.code() as i64);
                w.member_capacities.push(m.capacity as i64);
                w.member_strides.push(m.stride as i64);
            }
        }
        w
    }
}

pub(crate) fn logical_array_group(
    stage: IrStage,
    owner: Ustr,
    arrays: &[ArrayParam],
) -> Option<LogicalArrayGroup> {
    (!arrays.is_empty()).then(|| LogicalArrayGroup {
        stage,
        owner,
        members: arrays.iter().map(|a| a.decl).collect(),
    })
}

/// The neutral wire kind of a tier-0 GLSL type. Total over what an INTERFACE
/// position can hold -- a varying or fragment output is one of these by
/// construction, since anything else is refused when the stage is classified.
pub(crate) fn wire_kind_of(t: GlslTy) -> WireKind {
    match t {
        GlslTy::Float => WireKind::F32,
        GlslTy::Int => WireKind::I32,
        GlslTy::Uint => WireKind::U32,
        GlslTy::Bool => WireKind::Bool32,
        GlslTy::Vec2 => WireKind::F32x2,
        GlslTy::Vec3 | GlslTy::Bvec3 => WireKind::F32x3,
        GlslTy::Vec4 => WireKind::F32x4,
        GlslTy::Mat4 => WireKind::Mat4,
        GlslTy::Sampler2D | GlslTy::ISampler2D => WireKind::I32,
    }
}

pub(crate) fn stage_uniform_fields(
    iface: &ShaderInterfacePlan,
    stage: IrStage,
) -> impl Iterator<Item = (Ustr, GlslTy)> + '_ {
    iface
        .uniform_group(UniformRole::Engine)
        .into_iter()
        .chain(iface.uniform_group(UniformRole::Stage(stage)))
        .flat_map(|g| g.members.iter().map(|(n, k)| (*n, wire_glsl(*k))))
}

/// A neutral wire kind spelled as a GLSL type. The inverse of `wire_kind_of`,
/// and the only place the interface's vocabulary becomes GLSL's.
pub(crate) fn wire_glsl(k: WireKind) -> GlslTy {
    match k {
        WireKind::F32 => GlslTy::Float,
        WireKind::I32 => GlslTy::Int,
        WireKind::U32 => GlslTy::Uint,
        WireKind::Bool32 => GlslTy::Bool,
        WireKind::F32x2 => GlslTy::Vec2,
        WireKind::F32x3 => GlslTy::Vec3,
        WireKind::F32x4 => GlslTy::Vec4,
        WireKind::Mat4 => GlslTy::Mat4,
    }
}

pub(crate) fn emit_stage_resources(
    out: &mut String,
    iface: &ShaderInterfacePlan,
    abi: &ShaderAbiPlan,
    stage: IrStage,
) {
    let blocks = [
        block_at(&abi.uniform_blocks, UNIFORM_BLOCK_BINDING_ENGINE),
        block_at(
            &abi.uniform_blocks,
            ShaderAbiPlan::binding_of(UniformRole::Stage(stage)),
        ),
    ];
    let arrays = block_at(
        &abi.uniform_array_blocks,
        ShaderAbiPlan::array_binding_of(stage),
    );
    for block in blocks.into_iter().flatten() {
        out.push_str(&format!("layout(std140) uniform {} {{\n", block.name));
        for m in &block.plan.members {
            out.push_str(&format!("    {} {};\n", wire_glsl(m.kind).name(), m.name));
        }
        out.push_str("};\n");
    }
    if let Some(plan) = arrays {
        out.push_str(&format!("layout(std140) uniform {} {{\n", plan.block_name));
        for m in &plan.members {
            out.push_str(&format!(
                "    {} {}[{}];\n",
                wire_glsl(m.elem).name(),
                m.name,
                m.capacity
            ));
        }
        out.push_str("};\n");
    }
    for (name, kind) in iface.stage_samplers(stage) {
        let ty = sampler_glsl_ty(kind).uniform_decl();
        out.push_str(&format!("uniform {ty} {name};\n"));
    }
    if stage == IrStage::Fragment || arrays.is_some() || blocks.into_iter().any(|b| b.is_some()) {
        out.push('\n');
    }
}

fn block_at<T>(blocks: &[(u32, T)], binding: u32) -> Option<&T> {
    blocks.iter().find(|(b, _)| *b == binding).map(|(_, p)| p)
}

pub(crate) fn logical_uniform_group(
    ws: &Workspace,
    role: UniformRole,
    struct_name: Ustr,
    span: Span,
) -> Result<LogicalUniformGroup, CompilerError> {
    Ok(LogicalUniformGroup {
        role,
        struct_name: NominalType::of(ws, struct_name),
        members: skull_mir::gpu_layout::uniform_group_members(ws, struct_name)
            .map_err(|e| err(span, e.message))?,
    })
}

pub(crate) fn logical_textures(
    ws: &Workspace,
    vertex: Option<&TextureSlots>,
    fragment: Option<&TextureSlots>,
    has_bare_sampler: bool,
    span: Span,
) -> Result<Option<LogicalTextureGroup>, CompilerError> {
    if let (Some(v), Some(f)) = (vertex, fragment)
        && v.struct_name != f.struct_name
    {
        return Err(err(
            span,
            format!(
                "shader pipeline: the stages declare different @textures groups (`{}` and `{}`) -- texture units are program-scoped, so one program has one group",
                v.struct_name, f.struct_name
            ),
        ));
    }
    let Some(group) = vertex.or(fragment) else {
        return Ok(None);
    };
    let name = group.struct_name;
    if has_bare_sampler {
        return Err(err(
            span,
            format!(
                "shader: `{name}` is a @textures group, so every sampler must join it -- a bare Sampler2D param keeps a CPU-assigned unit, which can collide with a compiler-assigned slot"
            ),
        ));
    }
    Ok(Some(LogicalTextureGroup {
        struct_name: Some(NominalType::of(ws, name)),
        samplers: group.slots.iter().map(|s| (s.name, s.kind)).collect(),
        stages: vertex
            .map(|_| IrStage::Vertex)
            .into_iter()
            .chain(fragment.map(|_| IrStage::Fragment))
            .collect(),
    }))
}

/// Physical layout and binding assignments for the logical shader interface.
/// The engine consumes these assignments directly.
#[derive(Clone, Debug, Default)]
pub struct ShaderAbiPlan {
    pub uniform_blocks: Vec<(u32, GlslUniformBlock)>,
    pub uniform_array_blocks: Vec<(u32, UniformArrayBlockPlan)>,
    /// Sampler names in UNIT order -- unit N is index N. GLSL 3.30 has no
    /// `layout(binding=)`, so the engine needs each name once to point the
    /// sampler uniform at its unit at link time; after that it binds by slot.
    pub texture_units: Vec<Ustr>,
}

impl ShaderAbiPlan {
    /// Uniform bindings are fixed by the host registration ABI.
    fn binding_of(role: UniformRole) -> u32 {
        match role {
            UniformRole::Engine => UNIFORM_BLOCK_BINDING_ENGINE,
            UniformRole::Stage(IrStage::Vertex) => UNIFORM_BLOCK_BINDING_VERTEX,
            UniformRole::Stage(IrStage::Fragment) => UNIFORM_BLOCK_BINDING_FRAGMENT,
        }
    }

    fn array_binding_of(stage: IrStage) -> u32 {
        match stage {
            IrStage::Vertex => UNIFORM_BLOCK_BINDING_VERTEX_ARRAYS,
            IrStage::Fragment => UNIFORM_BLOCK_BINDING_FRAGMENT_ARRAYS,
        }
    }

    /// Lay out uniform blocks and assign texture units in declaration order.
    pub fn glsl(iface: &ShaderInterfacePlan, span: Span) -> Result<Self, CompilerError> {
        let binding = Self::binding_of;
        let mut uniform_blocks: Vec<(u32, GlslUniformBlock)> = Vec::new();
        for g in &iface.uniform_groups {
            let plan =
                skull_mir::gpu_layout::std140_block(g.struct_name.key, &g.members, Some(span))
                    .map_err(|e| err(span, e.message))?;
            uniform_blocks.push((
                binding(g.role),
                GlslUniformBlock {
                    name: g.struct_name.symbol,
                    plan,
                },
            ));
        }
        uniform_blocks.sort_by_key(|(b, _)| *b);

        let mut uniform_array_blocks: Vec<(u32, UniformArrayBlockPlan)> = Vec::new();
        for g in &iface.array_groups {
            let b = Self::array_binding_of(g.stage);
            uniform_array_blocks.push((b, uniform_array_block(g.block_name(), &g.members)?));
        }
        uniform_array_blocks.sort_by_key(|(b, _)| *b);

        let texture_units = iface.sampler_names();

        Ok(ShaderAbiPlan {
            uniform_blocks,
            uniform_array_blocks,
            texture_units,
        })
    }
}

#[derive(Clone, Debug)]
pub struct GlslUniformBlock {
    pub name: Ustr,
    pub plan: UniformBlockPlan,
}

/// Texture image units every GLSL target guarantees per stage: GL 4.1 core
/// (Apple's only desktop context) guarantees 16. Desktop Linux
/// drivers allow more, so a program over it linked there and failed to link
/// on a Mac; checking at compile time makes every host refuse it.
pub(crate) const GLSL_STAGE_SAMPLER_BUDGET: usize = 16;

/// Refuse a stage that declares more samplers than every GLSL target has
/// units for. Counts DECLARED samplers (group plus bare) rather than the ones
/// a driver keeps active: the budget has to hold before any optimizer runs.
pub(crate) fn check_stage_sampler_budget(
    iface: &ShaderInterfacePlan,
    stage: IrStage,
    entry: &str,
    span: Span,
) -> Result<(), CompilerError> {
    let count = iface.stage_samplers(stage).len();
    if count <= GLSL_STAGE_SAMPLER_BUDGET {
        return Ok(());
    }
    let name = stage.name();
    Err(err(
        span,
        format!(
            "shader `{entry}`: the {name} stage declares {count} samplers, over the \
         {GLSL_STAGE_SAMPLER_BUDGET} texture units GL 4.1 core (macOS) guarantee \
         per stage -- pack inputs into shared textures"
        ),
    ))
}

// Bindings are fixed by role; slot 0 is reserved for `@engine` whether or not the program has
// one, so the engine can reject a foreign block there. ES 3.0 guarantees 12 blocks per stage.
pub(crate) const UNIFORM_BLOCK_BINDING_ENGINE: u32 = 0;
pub(crate) const UNIFORM_BLOCK_BINDING_VERTEX: u32 = 1;
pub(crate) const UNIFORM_BLOCK_BINDING_FRAGMENT: u32 = 2;
pub const UNIFORM_BLOCK_BINDING_VERTEX_ARRAYS: u32 = 3;
pub const UNIFORM_BLOCK_BINDING_FRAGMENT_ARRAYS: u32 = 4;
