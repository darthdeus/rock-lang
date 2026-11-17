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
                // A scalar member takes no array payload, and the host's push
                // verbs reject a capacity-0 target rather than writing one
                // element into it.
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

/// A stage's `UniformArray<T>` members, as a logical group. `None` when the
/// stage declares none.
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
        // A sampler is a RESOURCE, never an interpolated or returned value; it
        // reaches the interface through `LogicalTextureGroup` instead.
        GlslTy::Sampler2D | GlslTy::ISampler2D => WireKind::I32,
    }
}

/// A stage's own uniform members plus the program's `@engine` members, spelled
/// for GLSL -- what a body needs to type a bare-global read.
///
/// From the INTERFACE. The shapes carried a second GLSL-typed copy of the same
/// fields, which is the drift this split exists to remove.
pub(crate) fn stage_uniform_fields(
    iface: &ShaderInterfacePlan,
    stage: IrStage,
) -> Vec<(Ustr, GlslTy)> {
    iface
        .uniform_group(UniformRole::Engine)
        .into_iter()
        .chain(iface.uniform_group(UniformRole::Stage(stage)))
        .flat_map(|g| g.members.iter().map(|(n, k)| (*n, wire_glsl(*k))))
        .collect()
}

/// The varyings a program passes between stages, spelled for GLSL. Names,
/// kinds and `@flat` come from the INTERFACE; only the spelling is this
/// target's.
pub(crate) fn interface_varyings(iface: &ShaderInterfacePlan) -> Vec<(Ustr, GlslTy, bool)> {
    iface
        .varyings
        .iter()
        .map(|v| (v.name, wire_glsl(v.kind), v.flat))
        .collect()
}

/// The fragment stage's MRT outputs, or `None` for the single implicit one.
pub(crate) fn interface_mrt_outputs(iface: &ShaderInterfacePlan) -> Option<Vec<(Ustr, GlslTy)>> {
    match &iface.fragment_output {
        FragmentOutput::Mrt(outs) => Some(outs.iter().map(|(n, k)| (*n, wire_glsl(*k))).collect()),
        FragmentOutput::Single(_) | FragmentOutput::None => None,
    }
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

/// ABI declarations used by one stage.
pub(crate) struct StageAbi<'a> {
    pub(crate) uniform_block: Option<&'a GlslUniformBlock>,
    pub(crate) engine_block: Option<&'a GlslUniformBlock>,
    pub(crate) array_block: Option<&'a UniformArrayBlockPlan>,
    pub(crate) samplers: Vec<(Ustr, SamplerKind)>,
}

impl StageAbi<'_> {
    /// The stage's uniform resources: the engine block first (it binds at
    /// index 0 and both stages declare the same one), then the stage's own
    /// block, its `@size(N)` array block, and its samplers.
    ///
    /// The array block is separate from the scalar one because its LIFETIME
    /// is: arrays change per activation, the scalar group per draw. Samplers
    /// are opaque, so GLSL forbids them inside a uniform block; they stay loose
    /// beside it and bind by unit -- the grouped ones in the ABI's unit order,
    /// then the bare ones.
    pub(crate) fn declarations(&self) -> String {
        let mut out = String::new();
        for block in self.engine_block.iter().chain(&self.uniform_block) {
            out.push_str(&uniform_block_decl(block));
        }
        if let Some(plan) = self.array_block {
            out.push_str(&uniform_array_block_decl(plan));
        }
        for (n, k) in &self.samplers {
            out.push_str(&format!(
                "uniform {} {n};\n",
                sampler_glsl_ty(*k).uniform_decl()
            ));
        }
        out
    }
}

/// Project grouped samplers in ABI order, then append bare samplers.
fn stage_samplers_in_unit_order(
    iface: &ShaderInterfacePlan,
    abi: &ShaderAbiPlan,
    stage: IrStage,
) -> Vec<(Ustr, SamplerKind)> {
    let group: Vec<Ustr> = iface.sampler_names();
    let mine = iface.stage_samplers(stage);
    let mut out: Vec<(Ustr, SamplerKind)> = Vec::new();
    for u in &abi.texture_units {
        if let Some(s) = mine.iter().find(|(n, _)| n == u) {
            out.push(*s);
        }
    }
    // The bare ones have no unit to sort by, so they keep declaration order.
    out.extend(mine.iter().filter(|(n, _)| !group.contains(n)).copied());
    out
}

fn block_at<T>(blocks: &[(u32, T)], binding: u32) -> Option<&T> {
    blocks.iter().find(|(b, _)| *b == binding).map(|(_, p)| p)
}

pub(crate) fn stage_abi<'a>(
    iface: &ShaderInterfacePlan,
    abi: &'a ShaderAbiPlan,
    stage: IrStage,
) -> StageAbi<'a> {
    StageAbi {
        uniform_block: block_at(
            &abi.uniform_blocks,
            ShaderAbiPlan::binding_of(UniformRole::Stage(stage)),
        ),
        engine_block: block_at(&abi.uniform_blocks, UNIFORM_BLOCK_BINDING_ENGINE),
        array_block: block_at(
            &abi.uniform_array_blocks,
            ShaderAbiPlan::array_binding_of(stage),
        ),
        samplers: stage_samplers_in_unit_order(iface, abi, stage),
    }
}

/// The samplers that are NOT part of a `@textures` group -- loose params whose
/// unit the CPU picks. `has_bare_sampler` answers whether any exist (a
/// rejection when a group is also present); this is the list itself.
pub(crate) fn bare_sampler_list(
    samplers: &[(usize, Ustr, SamplerKind)],
    grouped_params: &[usize],
    stage: IrStage,
) -> Vec<LogicalSampler> {
    samplers
        .iter()
        .filter(|(i, _, _)| !grouped_params.contains(i))
        .map(|(_, n, k)| LogicalSampler {
            name: *n,
            kind: *k,
            stage,
        })
        .collect()
}

/// The neutral member list of a uniform group.
///
/// `gpu_layout::uniform_group_members` is the ONE classifier -- the same one
/// `uniform_block` reads before laying anything out, so the interface and the
/// CPU packer cannot disagree about a kind. Places come later, from
/// `std140_block`, over exactly these members.
fn logical_members(
    ws: &Workspace,
    struct_name: Ustr,
    span: Span,
) -> Result<Vec<(Ustr, WireKind)>, CompilerError> {
    skull_mir::gpu_layout::uniform_group_members(ws, struct_name).map_err(|e| err(span, e.message))
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
        members: logical_members(ws, struct_name, span)?,
    })
}

pub(crate) fn logical_textures(
    ws: &Workspace,
    group: Option<Ustr>,
    has_bare_sampler: bool,
    stages: Vec<IrStage>,
    span: Span,
) -> Result<Option<LogicalTextureGroup>, CompilerError> {
    let Some(g) = group else { return Ok(None) };
    if has_bare_sampler {
        return Err(err(
            span,
            format!(
                "shader: `{g}` is a @textures group, so every sampler must join it -- a bare Sampler2D param keeps a CPU-assigned unit, which can collide with a compiler-assigned slot"
            ),
        ));
    }
    let ts = texture_slots(ws, g).map_err(|e| err(span, e.message))?;
    Ok(Some(LogicalTextureGroup {
        struct_name: Some(NominalType::of(ws, g)),
        samplers: ts.slots.iter().map(|s| (s.name, s.kind)).collect(),
        stages,
    }))
}

/// Physical layout and binding assignments for the logical shader interface.
/// The engine consumes these assignments directly.
#[derive(Clone, Debug, Default)]
pub struct ShaderAbiPlan {
    /// Scalar blocks in binding order. Bindings are fixed by role.
    pub uniform_blocks: Vec<(u32, GlslUniformBlock)>,
    /// Array blocks have their own bindings and upload lifetime.
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

    /// The binding a stage's array block takes.
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

        // Declaration order IS unit order, which is the contract `@attr`
        // already uses for locations.
        let texture_units = iface.sampler_names();

        Ok(ShaderAbiPlan {
            uniform_blocks,
            uniform_array_blocks,
            texture_units,
        })
    }
}

/// One std140 block of a GLSL program. `name` is the group type's symbol, which
/// the declaration, the block wire and the semantic wire all spell; the plan's
/// `struct_name` stays the DefKey, because the fingerprint hashes it and the CPU
/// packer's plan of the same type must hash the same bytes.
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

/// Uniform block binding indices, assigned by param ROLE so the slot is
/// structural rather than a consequence of how many groups a program declares.
/// Slot 0 is RESERVED for `@engine` whether or not the program has one, which is
/// what lets the engine reject a foreign block there at registration. The array
/// slots are reserved on the same terms: ES 3.0 guarantees 12 uniform blocks per
/// stage and 24 combined, so five reserved indices are affordable.
pub(crate) const UNIFORM_BLOCK_BINDING_ENGINE: u32 = 0;
pub(crate) const UNIFORM_BLOCK_BINDING_VERTEX: u32 = 1;
pub(crate) const UNIFORM_BLOCK_BINDING_FRAGMENT: u32 = 2;
pub const UNIFORM_BLOCK_BINDING_VERTEX_ARRAYS: u32 = 3;
pub const UNIFORM_BLOCK_BINDING_FRAGMENT_ARRAYS: u32 = 4;
