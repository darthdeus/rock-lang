//! The shader ABI: uniform blocks, samplers, attributes and their validation.

use super::*;

/// A program's uniform blocks -- scalar AND array -- flattened into the
/// PARALLEL LISTS the registration call carries.
///
/// One entry per block in `names`/`bindings`/`sizes`/`fingerprints`/`counts`,
/// with the members of every block concatenated into the `member_*` lists and
/// `counts[i]` saying how many belong to block `i`. A hand-written driver never
/// has to nest, which is why the shape is this and not a table.
///
/// **Built once, read by both hosts.** The Lua and JS registration lines are
/// written by different emitters, and a second flattener is how the two would
/// come to disagree about member order -- the offsets the host packs against
/// are positional, so a divergence there is silent wrong bytes rather than an
/// error.
///
/// Array blocks are appended AFTER the scalar ones. They travel the same wire
/// deliberately: a block is a block to the driver -- bind, verify against the
/// plan, allocate scratch, upload whole -- and giving arrays their own
/// registration call would split a registration that has to be transactional.
/// The three per-member lists are what tells them apart:
///
/// - `member_kinds` is the [`WireKind::code`] of every member, array or not;
/// - `member_capacities` is `0` for a scalar member and `N` for a `@size(N)`
///   array -- so "how many elements may be written here" answers "is this an
///   array" without a separate flag;
/// - `member_strides` is `0` for a scalar member and the std140 array stride
///   otherwise. Sent rather than re-derived host-side: it is 16 for every kind
///   an array may hold today, and a host that recomputed it would be a second
///   layout authority the moment that stops being true.
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

/// Uniform block binding indices, assigned by param ROLE so the slot is
/// structural rather than a consequence of how many groups a program declares.
/// Slot 0 is RESERVED for `@engine` whether or not the program has one, which is
/// what lets the engine reject a foreign block there at registration.
///
/// The array slots are reserved on the same terms. ES 3.0 guarantees 12 uniform
/// blocks per stage and 24 combined, so five reserved indices are affordable.
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

/// **The whole GLSL ABI plan checked against the interface it claims to
/// answer**, before any stage looks at it.
///
/// Presence at a binding is not identity: a block of the right shape at the
/// right index emits the wrong declarations if it is a DIFFERENT group's, and
/// every per-binding presence check passes while it does. So this compares the
/// block's own name and its complete member list. It runs over the PROGRAM
/// rather than per stage, because an unknown binding belongs to no stage and
/// would otherwise be nobody's to reject.
pub(crate) fn validate_abi(
    iface: &ShaderInterfacePlan,
    abi: &ShaderAbiPlan,
    span: Span,
) -> Result<(), CompilerError> {
    // --- scalar blocks: exactly the interface's groups, at their own bindings.
    let mut expected: Vec<(u32, &LogicalUniformGroup)> = iface
        .uniform_groups
        .iter()
        .map(|g| (ShaderAbiPlan::binding_of(g.role), g))
        .collect();
    expected.sort_by_key(|(b, _)| *b);
    // Two groups claiming one role makes the binding ambiguous, and the defect
    // is the interface's rather than the plan's.
    if let Some(w) = expected.windows(2).find(|w| w[0].0 == w[1].0) {
        return Err(err(
            span,
            format!(
                "internal: the interface declares two uniform groups at binding {} (`{}` and `{}`)",
                w[0].0, w[0].1.struct_name.key, w[1].1.struct_name.key
            ),
        ));
    }
    let mut seen: HashSet<u32> = HashSet::default();
    for (b, block) in &abi.uniform_blocks {
        let plan = &block.plan;
        if !seen.insert(*b) {
            return Err(err(
                span,
                format!(
                    "internal: the GLSL ABI plan holds more than one uniform block at binding {b}"
                ),
            ));
        }
        let Some((_, g)) = expected.iter().find(|(eb, _)| eb == b) else {
            return Err(err(
                span,
                format!(
                    "internal: the GLSL ABI plan holds a uniform block at binding {b}, which no \
                 interface group claims"
                ),
            ));
        };
        if (plan.struct_name, block.name) != (g.struct_name.key, g.struct_name.symbol) {
            return Err(err(
                span,
                format!(
                    "internal: binding {b} holds block `{}` (`{}`) but the interface's group there is \
                 `{}` (`{}`)",
                    plan.struct_name, block.name, g.struct_name.key, g.struct_name.symbol
                ),
            ));
        }
        let laid: Vec<(Ustr, WireKind)> = plan.members.iter().map(|m| (m.name, m.kind)).collect();
        if laid != g.members {
            return Err(err(
                span,
                format!(
                    "internal: block `{}` at binding {b} was laid out over {laid:?}, but the \
                 interface declares {:?}",
                    plan.struct_name, g.members
                ),
            ));
        }
    }
    for (b, g) in &expected {
        if !seen.contains(b) {
            return Err(err(
                span,
                format!(
                    "internal: the interface declares group `{}` at binding {b}, which the GLSL ABI \
                 plan has no block for -- its members would be referenced without being declared",
                    g.struct_name.key
                ),
            ));
        }
    }

    // --- array blocks: same rule, including each member's CAPACITY, which is
    // what an `@size(N)` change moves and what a name check alone would miss.
    let mut expected_arrays: Vec<(u32, &LogicalArrayGroup)> = iface
        .array_groups
        .iter()
        .map(|g| (ShaderAbiPlan::array_binding_of(g.stage), g))
        .collect();
    expected_arrays.sort_by_key(|(b, _)| *b);
    if let Some(w) = expected_arrays.windows(2).find(|w| w[0].0 == w[1].0) {
        return Err(err(
            span,
            format!(
                "internal: the interface declares two array groups for one stage at binding {} \
             (owned by `{}` and `{}`)",
                w[0].0, w[0].1.owner, w[1].1.owner
            ),
        ));
    }
    let mut seen_arrays: HashSet<u32> = HashSet::default();
    for (b, plan) in &abi.uniform_array_blocks {
        if !seen_arrays.insert(*b) {
            return Err(err(
                span,
                format!(
                    "internal: the GLSL ABI plan holds more than one array block at binding {b}"
                ),
            ));
        }
        let Some((_, g)) = expected_arrays.iter().find(|(eb, _)| eb == b) else {
            return Err(err(
                span,
                format!(
                    "internal: the GLSL ABI plan holds an array block at binding {b}, which no \
                 interface group claims"
                ),
            ));
        };
        if plan.block_name != g.block_name() {
            return Err(err(
                span,
                format!(
                    "internal: binding {b} holds array block `{}`, but the interface's group there \
                 is owned by `{}`",
                    plan.block_name, g.owner
                ),
            ));
        }
        let laid: Vec<(Ustr, WireKind, u32)> = plan
            .members
            .iter()
            .map(|m| (m.name, m.elem, m.capacity))
            .collect();
        let want: Vec<(Ustr, WireKind, u32)> = g
            .members
            .iter()
            .map(|m| (m.name, m.elem, m.capacity))
            .collect();
        if laid != want {
            return Err(err(
                span,
                format!(
                    "internal: array block `{}` was laid out over {laid:?}, but the interface \
                 declares {want:?}",
                    plan.block_name
                ),
            ));
        }
    }
    for (b, g) in &expected_arrays {
        if !seen_arrays.contains(b) {
            return Err(err(
                span,
                format!(
                    "internal: the interface declares an array group owned by `{}` at binding {b}, \
                 which the GLSL ABI plan has no block for",
                    g.owner
                ),
            ));
        }
    }

    // --- attribute locations: a bijection on names AND non-overlapping
    // location RANGES. Two attributes at one location is a silent aliasing
    // that a name bijection alone accepts.
    let attrs: Vec<(Ustr, AttrKind)> = iface
        .vertex_inputs
        .iter()
        .flat_map(|i| i.attrs.iter().copied())
        .collect();
    if attrs.len() != abi.attribute_locations.len() {
        return Err(err(
            span,
            format!(
                "internal: the interface declares {} vertex attribute(s) and the GLSL ABI plan \
             assigns {} location(s)",
                attrs.len(),
                abi.attribute_locations.len()
            ),
        ));
    }
    let mut occupied: HashMap<u32, Ustr> = HashMap::default();
    for (name, kind) in &attrs {
        let Some((_, loc)) = abi.attribute_locations.iter().find(|(an, _)| an == name) else {
            return Err(err(
                span,
                format!("internal: vertex attribute `{name}` has no location in the GLSL ABI plan"),
            ));
        };
        let Some(end) = loc.checked_add(attr_location_span(*kind)) else {
            return Err(err(
                span,
                format!(
                    "internal: vertex attribute `{name}` at location {loc} spans past the end of the \
                 location space"
                ),
            ));
        };
        for slot in *loc..end {
            if let Some(other) = occupied.insert(slot, *name) {
                return Err(err(
                    span,
                    format!(
                        "internal: vertex attributes `{other}` and `{name}` both occupy location \
                     {slot}"
                    ),
                ));
            }
        }
    }

    // --- texture units: the same bijection the stage join needs, checked once
    // for the program so an unused stage cannot leave it unasked.
    let group = iface.sampler_names();
    let mut units: HashSet<Ustr> = HashSet::default();
    for u in &abi.texture_units {
        if !units.insert(*u) {
            return Err(err(
                span,
                format!("internal: the GLSL ABI plan assigns sampler `{u}` more than one unit"),
            ));
        }
        if !group.contains(u) {
            return Err(err(
                span,
                format!(
                    "internal: the GLSL ABI plan assigns a unit to `{u}`, which the interface's \
                 @textures group does not name"
                ),
            ));
        }
    }
    for n in &group {
        if !units.contains(n) {
            return Err(err(
                span,
                format!(
                    "internal: the interface's @textures group names `{n}`, which the GLSL ABI plan \
                 gives no unit -- it would be referenced without being declared"
                ),
            ));
        }
    }
    Ok(())
}

/// **One stage's resolved ABI**: every physical entry the interface says this
/// stage has, and nothing else.
///
/// Projected by [`project_stage_abi`] from a plan [`validate_abi`] has already
/// checked. An `Option` here means the INTERFACE declares no such group; it
/// never means one was declared and not found, because that rejects. A shader
/// referencing what it never declared is a driver compile error with no
/// compiler diagnostic at all.
pub(crate) struct StageAbi<'a> {
    pub(crate) uniform_block: Option<&'a GlslUniformBlock>,
    pub(crate) engine_block: Option<&'a GlslUniformBlock>,
    pub(crate) array_block: Option<&'a UniformArrayBlockPlan>,
    pub(crate) samplers: Vec<(Ustr, SamplerKind)>,
}

/// The one block the interface says exists at `binding`, or a rejection.
///
/// `want` is whether the INTERFACE has the group, so `None` means "not
/// declared" and never "declared but not found". Missing, extra and duplicate
/// are each their own error: "the ABI omitted it", "the ABI invented it" and
/// "the ABI has it twice" are different defects and a count comparison alone
/// cannot tell the second from the third.
fn require_block<'a, T>(
    blocks: &'a [(u32, T)],
    binding: u32,
    want: bool,
    what: &str,
    span: Span,
) -> Result<Option<&'a T>, CompilerError> {
    let mut found = blocks.iter().filter(|(b, _)| *b == binding);
    let first = found.next();
    if found.next().is_some() {
        return Err(err(
            span,
            format!("internal: the GLSL ABI plan holds more than one {what} at binding {binding}"),
        ));
    }
    match (want, first) {
        (true, Some((_, p))) => Ok(Some(p)),
        (false, None) => Ok(None),
        (true, None) => Err(err(
            span,
            format!(
                "internal: the interface declares a {what} but the GLSL ABI plan has none at \
             binding {binding} -- its members would be referenced without being declared"
            ),
        )),
        (false, Some(_)) => Err(err(
            span,
            format!(
                "internal: the GLSL ABI plan holds a {what} at binding {binding} that the interface \
             does not declare"
            ),
        )),
    }
}

/// The samplers ONE STAGE declares, in unit order, then its bare ones.
///
/// [`ShaderInterfacePlan::stage_samplers`] is the source; this adds the target's
/// unit ORDER and validates the join. The check is a real BIJECTION, not length
/// plus membership: `[a, a]` against an interface of `[a, b]` has the right
/// length and every element is a member, and it emits `a` twice while dropping
/// `b` entirely.
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

/// **A GLSL ABI plan checked against its interface, with each stage's slice
/// already projected.**
///
/// Validation is a PROGRAM property -- an unknown binding belongs to no stage,
/// and re-running the whole check per stage says nothing the first run did not.
/// Built once by [`ProgramAbi::of`]; the emitters receive projections.
pub(crate) struct ProgramAbi<'a> {
    stages: Vec<(IrStage, StageAbi<'a>)>,
    /// Anchors a projection failure, so a lookup miss is a spanned rejection
    /// rather than a panic.
    span: Span,
    /// The vertex attribute declarations, resolved once. Only the vertex stage
    /// renders them, and the name/location join they need is exactly what
    /// validation just established.
    pub(crate) attributes: Vec<(Ustr, GlslTy, u32)>,
}

impl<'a> ProgramAbi<'a> {
    /// Validate `abi` against `iface`, then project exactly the stages the
    /// renderer will ask for.
    ///
    /// `required` is a census, not a hint: a renderer that emits a vertex
    /// stage the interface does not declare is a defect, and taking
    /// `iface.stages` on trust would turn it into a lookup miss later.
    pub(crate) fn of(
        iface: &ShaderInterfacePlan,
        abi: &'a ShaderAbiPlan,
        required: &[IrStage],
        span: Span,
    ) -> Result<Self, CompilerError> {
        validate_abi(iface, abi, span)?;
        let mut stages = Vec::with_capacity(required.len());
        for &stage in required {
            if !iface.stages.contains(&stage) {
                return Err(err(
                    span,
                    format!(
                        "internal: the program emits a {} stage, which its interface does not declare",
                        stage.name()
                    ),
                ));
            }
            stages.push((stage, project_stage_abi(iface, abi, stage, span)?));
        }
        Ok(ProgramAbi {
            stages,
            attributes: attribute_declarations(iface, abi, span)?,
            span,
        })
    }

    pub(crate) fn stage(&self, stage: IrStage) -> Result<&StageAbi<'a>, CompilerError> {
        self.stages
            .iter()
            .find(|(s, _)| *s == stage)
            .map(|(_, a)| a)
            .ok_or_else(|| {
                err(
                    self.span,
                    format!(
                        "internal: no {} stage was projected for this program",
                        stage.name()
                    ),
                )
            })
    }
}

/// One stage's slice of an already-validated plan.
///
/// [`validate_abi`] has established that every group the interface declares
/// has exactly one matching block, so these lookups should not fail -- and
/// "should not" is why each one REJECTS instead of resolving to `None`. An
/// absence that the interface did not ask for is the same defect as a missing
/// declaration, and turning it back into `None` here would reintroduce it
/// downstream of the check that exists to catch it.
fn project_stage_abi<'a>(
    iface: &ShaderInterfacePlan,
    abi: &'a ShaderAbiPlan,
    stage: IrStage,
    span: Span,
) -> Result<StageAbi<'a>, CompilerError> {
    Ok(StageAbi {
        uniform_block: require_block(
            &abi.uniform_blocks,
            ShaderAbiPlan::binding_of(UniformRole::Stage(stage)),
            iface.uniform_group(UniformRole::Stage(stage)).is_some(),
            "uniform block",
            span,
        )?,
        engine_block: require_block(
            &abi.uniform_blocks,
            UNIFORM_BLOCK_BINDING_ENGINE,
            iface.uniform_group(UniformRole::Engine).is_some(),
            "@engine block",
            span,
        )?,
        array_block: require_block(
            &abi.uniform_array_blocks,
            ShaderAbiPlan::array_binding_of(stage),
            iface.array_group(stage).is_some(),
            "uniform array block",
            span,
        )?,
        samplers: stage_samplers_in_unit_order(iface, abi, stage),
    })
}

/// Every vertex attribute's emitted declaration: its name and neutral kind from
/// the INTERFACE, its location from the target's ABI plan.
///
/// Rejects a missing location rather than skipping the attribute. A dropped
/// `in` declaration is a driver error with no compiler diagnostic, which is the
/// one outcome this path may not produce.
fn attribute_declarations(
    iface: &ShaderInterfacePlan,
    abi: &ShaderAbiPlan,
    span: Span,
) -> Result<Vec<(Ustr, GlslTy, u32)>, CompilerError> {
    let mut out = Vec::new();
    for (n, k) in iface.vertex_inputs.iter().flat_map(|i| i.attrs.iter()) {
        let Some((_, loc)) = abi.attribute_locations.iter().find(|(an, _)| an == n) else {
            return Err(err(
                span,
                format!(
                    "internal: vertex attribute `{n}` has no location in the GLSL ABI plan -- it \
                 would be referenced without being declared"
                ),
            ));
        };
        out.push((*n, attr_glsl_ty(*k), *loc));
    }
    Ok(out)
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

/// **The GLSL ABI of one program's interface.**
///
/// The physical answer to [`ShaderInterfacePlan`]'s logical question, and the
/// ONLY object that holds one: block binding indices, std140 byte offsets and
/// sizes, block fingerprints, array-block names and capacities, sampler units,
/// and attribute locations. Nothing upstream of here knows any of it, and the
/// engine reads this rather than parsing emitted GLSL or re-deriving a layout.
///
/// Built by [`Self::glsl`] from the interface plan alone. A second target
/// derives its own from the SAME interface and is free to answer differently --
/// which is what `hostile_target.rs` proves, and what would be vacuous if the
/// interface plan were a GLSL plan under another name.
#[derive(Clone, Debug, Default)]
pub struct ShaderAbiPlan {
    /// Each std140 block with its BINDING INDEX, in binding order.
    ///
    /// The index comes from the group's ROLE, not from position: `@engine` is
    /// 0, the vertex `@uniform` group 1, the fragment group 2. Position was the
    /// index until the engine block landed, which made 0 mean "engine block"
    /// only for programs that happened to have one -- reserving the slot is
    /// what makes "binding 0 is the engine's" enforceable.
    ///
    /// The single authority for each block's bytes: the GLSL declaration, the
    /// CPU packer and the link-time driver assertion all read it.
    pub uniform_blocks: Vec<(u32, GlslUniformBlock)>,
    /// `UniformArray<T>` blocks with their bindings -- one per stage that
    /// declares `@size(N)` params.
    ///
    /// A SEPARATE block from the stage's scalar group, for a LIFETIME reason
    /// rather than a typing one: a fragment uniform changes per DRAW while
    /// light arrays change per ACTIVATION, so one combined block would force
    /// either a large copy into every batch or partial-update state. Two blocks
    /// keep whole-block uploads and independent fingerprints.
    pub uniform_array_blocks: Vec<(u32, UniformArrayBlockPlan)>,
    /// Sampler names in UNIT order -- unit N is index N. GLSL 3.30 has no
    /// `layout(binding=)`, so the engine needs each name once to point the
    /// sampler uniform at its unit at link time; after that it binds by slot.
    pub texture_units: Vec<Ustr>,
    /// Vertex attribute locations, in location order.
    pub attribute_locations: Vec<(Ustr, u32)>,
}

impl ShaderAbiPlan {
    /// GLSL's answer to `iface`.
    /// Takes NO `Workspace`. The layout is a function of `iface` alone, which
    /// is what makes a perturbed interface produce a perturbed layout instead
    /// of quietly re-deriving the declaring struct's.
    /// The binding a uniform group's ROLE takes. One oracle, so the layout and
    /// the validation below cannot disagree about where a group belongs.
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

    pub fn glsl(iface: &ShaderInterfacePlan, span: Span) -> Result<Self, CompilerError> {
        let binding = Self::binding_of;
        // std140 over the INTERFACE's members. The Workspace is not read here:
        // a layout is a function of the member list, which is what lets a
        // perturbed interface produce a perturbed layout and a second target
        // lay the same members out its own way.
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

        let mut attribute_locations = Vec::new();
        let mut loc = 0u32;
        for vi in &iface.vertex_inputs {
            for (name, kind) in &vi.attrs {
                attribute_locations.push((*name, loc));
                loc += attr_location_span(*kind);
            }
        }

        Ok(ShaderAbiPlan {
            uniform_blocks,
            uniform_array_blocks,
            texture_units,
            attribute_locations,
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

/// How many attribute LOCATIONS one attribute kind occupies. Every kind
/// admitted today takes one; a `mat4` attribute would take four, which is why
/// this is a function rather than an `enumerate()` index.
fn attr_location_span(_k: AttrKind) -> u32 {
    1
}

/// Texture image units every GLSL target guarantees per stage: GL 4.1 core
/// (Apple's only desktop context) guarantees 16. Desktop Linux
/// drivers allow more, so a program over it linked there and failed to link
/// on a Mac; checking at compile time makes every host refuse it.
pub const GLSL_STAGE_SAMPLER_BUDGET: usize = 16;

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
    let name = match stage {
        IrStage::Vertex => "vertex",
        IrStage::Fragment => "fragment",
    };
    Err(err(
        span,
        format!(
            "shader `{entry}`: the {name} stage declares {count} samplers, over the \
         {GLSL_STAGE_SAMPLER_BUDGET} texture units GL 4.1 core (macOS) guarantee \
         per stage -- pack inputs into shared textures"
        ),
    ))
}

pub const UNIFORM_BLOCK_BINDING_ENGINE: u32 = 0;
pub const UNIFORM_BLOCK_BINDING_VERTEX: u32 = 1;
pub const UNIFORM_BLOCK_BINDING_FRAGMENT: u32 = 2;
pub const UNIFORM_BLOCK_BINDING_VERTEX_ARRAYS: u32 = 3;
pub const UNIFORM_BLOCK_BINDING_FRAGMENT_ARRAYS: u32 = 4;
