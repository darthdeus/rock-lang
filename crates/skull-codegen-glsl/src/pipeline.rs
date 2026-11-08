//! Vertex/fragment pipelines: stage emission, collisions and planning.

use super::*;

fn emit_vertex_stage(
    ws: &Workspace,
    interner: &MonoInterner,
    stage: &StagePlan,
    v: &VertexShape,
    iface: &ShaderInterfacePlan,
    program: &ProgramAbi<'_>,
) -> Result<(String, Vec<FuncRef>), CompilerError> {
    let resolved = program.stage(IrStage::Vertex)?;
    let (vert_block, engine_block, array_block) = (
        resolved.uniform_block,
        resolved.engine_block,
        resolved.array_block,
    );
    let shape = EntryShape {
        global_params: v
            .attr_params
            .iter()
            .copied()
            .chain(v.engine_params.iter().copied())
            .chain(v.uniform_params.iter().copied())
            .chain(v.texture_params.iter().copied())
            .collect(),
        // Body-level typing of bare-global reads. Attribute types come from the
        // INTERFACE, spelled here -- `GlslTy` is the emitter's own working type
        // for body cells and does not travel back upward.
        uniform_fields: iface
            .vertex_inputs
            .iter()
            .flat_map(|i| i.attrs.iter().map(|(n, k)| (*n, attr_glsl_ty(*k))))
            .chain(stage_uniform_fields(iface, IrStage::Vertex))
            .collect(),
        sampler_params: v.samplers.iter().map(|&(i, _, _)| i).collect(),
        arrays: v.arrays.iter().map(|a| a.param).collect(),
        main_out: Some(MainOut {
            fields: iface
                .varyings
                .iter()
                .map(|v| (v.name, wire_glsl(v.kind)))
                .collect(),
            builtin: Some(("position", "gl_Position", GlslTy::Vec4)),
        }),
        reserved: entry_reserved_names(iface),
    };
    let (mf, helpers, dependencies) = (
        &stage.ir.body,
        stage.render_helpers(ws, interner)?,
        stage.dependencies(),
    );

    let mut out = String::new();
    for (n, t, loc) in &program.attributes {
        out.push_str(&format!(
            "layout (location = {loc}) in {} {};\n",
            t.name(),
            n
        ));
    }
    out.push('\n');
    // The engine block first: it binds at index 0 and both stages declare the
    // same one, so its declaration is identical in each.
    if let Some(plan) = engine_block {
        out.push_str(&uniform_block_decl(plan));
    }
    // No loose-uniform form: an interface group with no block has already been
    // rejected, so absence here means the stage declares no group at all.
    if let Some(plan) = vert_block {
        out.push_str(&uniform_block_decl(plan));
    }
    // The stage's `@size(N)` params, as ONE std140 block at their own reserved
    // binding. Separate from the scalar block above because their LIFETIME is:
    // arrays change per activation, the scalar group per draw.
    if let Some(plan) = array_block {
        out.push_str(&uniform_array_block_decl(plan));
    }
    // Samplers are opaque and cannot live in a uniform block, so each stage
    // declares the ones IT reads. Vertex texture fetch is legal in GL 3.3, and a stage that samples without declaring emits a reference to
    // an identifier the driver has never seen.
    for (n, t) in &resolved.samplers {
        let (n, t) = (n, *t);
        out.push_str(&format!(
            "uniform {} {};\n",
            sampler_glsl_ty(t).uniform_decl(),
            n
        ));
    }
    if vert_block.is_some() || engine_block.is_some() || array_block.is_some() {
        out.push('\n');
    }
    for (n, t, flat) in interface_varyings(iface) {
        let (n, t, flat) = (&n, &t, &flat);
        let q = if *flat { "flat " } else { "" };
        out.push_str(&format!("{q}out {} {};\n", t.name(), n));
    }
    out.push('\n');
    out.push_str(&helpers);
    let entry_plan = stage
        .concrete_plans
        .get(&stage.ir.entry.inst)
        .ok_or_else(|| {
            let f = &ws.module(stage.ir.entry.fref().module).functions[stage.ir.entry.fref().func];
            err(f.name_span, "shader profile: entry has no concrete plan")
        })?;
    out.push_str(&emit_fn_def_with(
        ws,
        interner,
        &stage.ir.entry,
        mf,
        Some(&shape),
        Some(entry_plan),
    )?);
    Ok((out, dependencies))
}

fn emit_fragment_stage(
    ws: &Workspace,
    interner: &MonoInterner,
    stage: &StagePlan,
    fs: &FragShape,
    iface: &ShaderInterfacePlan,
    program: &ProgramAbi<'_>,
) -> Result<(String, Vec<FuncRef>), CompilerError> {
    let span =
        ws.module(stage.ir.entry.fref().module).functions[stage.ir.entry.fref().func].name_span;
    let resolved = program.stage(IrStage::Fragment)?;
    let (frag_block, engine_block, array_block) = (
        resolved.uniform_block,
        resolved.engine_block,
        resolved.array_block,
    );
    let fref = stage.ir.entry.fref();
    // MRT: the entry is emitted as `void main()` destructuring its returned
    // output struct into the `layout(location=N) out` globals (like the vertex
    // varyings path). Single-output keeps the `vec4 <fn>()` + wrapper form.
    let main_out = interface_mrt_outputs(iface).as_ref().map(|outs| MainOut {
        fields: outs.iter().copied().collect(),
        builtin: iface
            .frag_depth
            .then_some((FRAG_DEPTH_FIELD, "gl_FragDepth", GlslTy::Float)),
    });
    let shape = EntryShape {
        global_params: std::iter::once(fs.varyings_param)
            .chain(fs.engine_params.iter().copied())
            .chain(fs.uniform_params.iter().copied())
            .chain(fs.texture_params.iter().copied())
            .collect(),
        uniform_fields: iface
            .varyings
            .iter()
            .map(|v| (v.name, wire_glsl(v.kind)))
            .chain(stage_uniform_fields(iface, IrStage::Fragment))
            .collect(),
        sampler_params: fs.samplers.iter().map(|&(i, _, _)| i).collect(),
        arrays: fs.arrays.iter().map(|a| a.param).collect(),
        main_out,
        reserved: entry_reserved_names(iface),
    };
    let (mf, helpers, dependencies) = (
        &stage.ir.body,
        stage.render_helpers(ws, interner)?,
        stage.dependencies(),
    );

    let mut out = String::new();
    for (n, t, flat) in interface_varyings(iface) {
        let (n, t, flat) = (&n, &t, &flat);
        let q = if *flat { "flat " } else { "" };
        out.push_str(&format!("{q}in {} {};\n", t.name(), n));
    }
    match &interface_mrt_outputs(iface) {
        None => {
            // `None` here means the interface declares no colour output while a
            // FRAGMENT stage is being emitted -- an inconsistent plan, not a
            // shape to guess a `vec4` for.
            let FragmentOutput::Single(k) = iface.fragment_output else {
                return Err(err(
                    span,
                    "internal: the interface declares no fragment output, but a fragment stage is being emitted",
                ));
            };
            out.push_str(&format!("out {} FragColor;\n\n", wire_glsl(k).name()))
        }
        Some(outs) => {
            for (loc, (n, t)) in outs.iter().enumerate() {
                out.push_str(&format!("layout(location = {loc}) out {} {n};\n", t.name()));
            }
            out.push('\n');
        }
    }
    // The engine block first -- same declaration the vertex stage emits.
    if let Some(plan) = engine_block {
        out.push_str(&uniform_block_decl(plan));
    }
    if let Some(plan) = frag_block {
        out.push_str(&uniform_block_decl(plan));
    }
    // The fragment stage's array block -- see the vertex stage.
    if let Some(plan) = array_block {
        out.push_str(&uniform_array_block_decl(plan));
    }
    for (n, t) in &resolved.samplers {
        out.push_str(&format!(
            "uniform {} {};\n",
            sampler_glsl_ty(*t).uniform_decl(),
            n
        ));
    }
    out.push('\n');
    out.push_str(&helpers);
    let entry_plan = stage
        .concrete_plans
        .get(&stage.ir.entry.inst)
        .ok_or_else(|| err(span, "shader profile: entry has no concrete plan"))?;
    out.push_str(&emit_fn_def_with(
        ws,
        interner,
        &stage.ir.entry,
        mf,
        Some(&shape),
        Some(entry_plan),
    )?);
    // Single-output: the entry fn returns Vec4; main() forwards it to FragColor.
    // MRT: the entry IS `void main()` (destructured), so no wrapper.
    if !matches!(iface.fragment_output, FragmentOutput::Mrt(_)) {
        out.push('\n');
        out.push_str(&format!(
            "void main() {{\n  FragColor = {}();\n}}\n",
            symbol_of_key(ws.module(fref.module).functions[fref.func].name)
        ));
    }
    Ok((out, dependencies))
}

/// Every interface-declared name landing in GLSL's ONE cross-stage namespace:
/// attributes, uniform/engine members, array members, varyings, samplers and
/// MRT outs. The completeness argument lives here once: every name a program
/// declares is in the interface exactly once, so a role added to the interface
/// joins every consumer by existing rather than by someone remembering to
/// chain it.
fn interface_global_names(iface: &ShaderInterfacePlan) -> Vec<Ustr> {
    iface
        .vertex_inputs
        .iter()
        .flat_map(|i| i.attrs.iter().map(|(n, _)| *n))
        .chain(
            iface
                .uniform_groups
                .iter()
                .flat_map(|g| g.members.iter().map(|(n, _)| *n)),
        )
        .chain(
            iface
                .array_groups
                .iter()
                .flat_map(|g| g.members.iter().map(|m| m.name)),
        )
        .chain(iface.varyings.iter().map(|v| v.name))
        .chain(
            iface
                .textures
                .iter()
                .flat_map(|t| t.samplers.iter().map(|(n, _)| *n)),
        )
        .chain(iface.bare_samplers.iter().map(|s| s.name))
        .chain(match &iface.fragment_output {
            FragmentOutput::Mrt(outs) => outs.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            _ => Vec::new(),
        })
        .collect()
}

/// Every global name an entry's locals and value params must not take: the
/// interface's whole census above, plus the stage I/O the interface never
/// claims (A1's `TexCoord`, single-output `FragColor`) and the two GL builtins
/// a `void main()` entry writes (`gl_Position`, `gl_FragDepth`). One set for
/// every entry: over-reservation is semantics-preserving (a renamed local keeps
/// every use through its one `cell_name`), while under-reservation is the
/// silent shadowing miscompile, so this errs wide. Mangled helper names and
/// block names are out: a local spelling one is unreachable in practice, and a
/// collision there is a loud driver error (calling a non-function), never a
/// silent wrong read.
pub(crate) fn entry_reserved_names(iface: &ShaderInterfacePlan) -> Vec<Ustr> {
    interface_global_names(iface)
        .into_iter()
        .chain([
            Ustr::from("TexCoord"),
            Ustr::from("FragColor"),
            Ustr::from("gl_Position"),
            Ustr::from("gl_FragDepth"),
        ])
        .collect()
}

/// Reject duplicate global names across BOTH stages (decision 7): attributes,
/// uniforms, varyings, samplers and the reserved gl_Position/FragColor share
/// one GLSL namespace -- the linked program has a single uniform namespace, so
/// a vertex/fragment same-name pair is a link error (or a silent alias), never
/// two distinct bindings. Varyings are one name in both stages (inserted once).
///
/// THE ENGINE BLOCK'S MEMBERS BELONG HERE TOO. An unnamed-instance block's
/// members are bare globals, so they sit in the same namespace as everything
/// above. A stage's own `@uniform` group already arrives through `uniforms`
/// whether or not it became a block, but `@engine` is a separate ROLE landing in
/// `engine_uniforms`, so it was outside this census entirely -- and a missed
/// pair is a raw driver error carrying no Rock span (`declaration of "uTime"
/// conflicts with previous declaration`), which is exactly the class this
/// exists to convert into a spanned rejection.
///
/// Taken from the PLAN rather than from both shapes' `engine_uniforms`: the
/// engine block is declared in both stages but is ONE block, so listing it per
/// stage would collide with itself.
/// Reject two interface names that would land in GLSL's ONE cross-stage
/// namespace.
///
/// Censuses the INTERFACE, which is what makes the list complete by
/// construction: every name a program declares is in there exactly once, so a
/// role added to the interface joins this check by existing rather than by
/// someone remembering to chain it.
fn check_pipeline_collisions(iface: &ShaderInterfacePlan, span: Span) -> Result<(), CompilerError> {
    // gl_Position is always reserved. `FragColor` is reserved only for the
    // single-output form -- with MRT the outputs are explicit named fields (one
    // of which may itself be `FragColor`), so they go through the collision check
    // like any other name rather than being pre-seeded.
    let mut seen: HashSet<Ustr> = [Ustr::from("gl_Position")].into_iter().collect();
    if !matches!(iface.fragment_output, FragmentOutput::Mrt(_)) {
        seen.insert(Ustr::from("FragColor"));
    }
    for n in interface_global_names(iface) {
        if !seen.insert(n) {
            return Err(err(
                span,
                format!(
                    "shader pipeline: name `{n}` would collide (attributes, uniforms, engine-block members, varyings, samplers and reserved names share one GLSL namespace across both stages)"
                ),
            ));
        }
    }
    Ok(())
}

/// Reject two blocks of one program sharing a BLOCK name.
///
/// Block names are their own GLSL namespace -- distinct from the member names
/// `check_pipeline_collisions` censuses, which is why this is a second check and
/// not a line in that one. It matters because an array block's name is MINTED
/// (`<entry>_Arrays`) rather than written by the author: a user struct that
/// happens to spell it would otherwise produce two `layout(std140) uniform
/// <same>` declarations, which is a link error carrying no Rock span. Minting a
/// different name instead is the fallback this repo does not take -- the
/// compiler's job is to refuse the program.
fn check_block_name_collisions<'a>(
    scalar: impl Iterator<Item = &'a GlslUniformBlock>,
    arrays: impl Iterator<Item = &'a UniformArrayBlockPlan>,
    span: Span,
) -> Result<(), CompilerError> {
    let mut seen: HashSet<Ustr> = HashSet::default();
    // The engine block is declared in BOTH stages but is ONE block, so the
    // scalar list is deduped by identity before the census rather than after --
    // otherwise it collides with itself.
    let names = scalar
        .map(|p| (p.plan.struct_name, p.name))
        .collect::<HashSet<_>>()
        .into_iter()
        .chain(arrays.map(|p| (p.block_name, p.block_name)));
    for (logical, physical) in names {
        if !seen.insert(physical) {
            return Err(err(
                span,
                format!(
                    "shader pipeline: uniform block name `{logical}` is declared twice -- a stage's array block is named `<entry>_Arrays`, so a @uniform group of that name collides with it"
                ),
            ));
        }
    }
    Ok(())
}

/// A2: compile a `@shader(vertex)` + `@shader(fragment)` pair into one
/// `#ifdef VERTEX`/`#ifdef PIXEL` file. The vertex entry's
/// return type (the varyings value struct) must be exactly the fragment entry's
/// first param type. Vertex params are classified by annotation: `@attr` (per-
/// vertex attribute struct), `@instance` (per-instance attribute divisor), and
/// `@uniform` (uniform block); the varyings `position: Vec4` field drives
/// `gl_Position` and is suppressed as a varying.
pub fn emit_shader_pipeline(
    ws: &Workspace,
    mid: ModuleId,
    vert_root: &str,
    frag_root: &str,
    profile: GlslProfile,
) -> Result<String, CompilerError> {
    Ok(emit_shader_pipeline_with_dependencies(ws, mid, vert_root, frag_root, profile)?.source)
}

/// **The one validated plan for an A2 `(vertex, fragment)` program.**
///
/// Everything emission decides before it writes GLSL, in emission's own order:
/// both stage annotations, the vertex shape and its varyings, the fragment
/// classified AGAINST those varyings (not as a standalone entry, where a
/// varyings param has no meaning), the cross-stage collisions, the per-stage
/// `@uniform` and program `@engine` std140 layouts, each stage's own lowering +
/// tier-0 + graph/view population + stage-builtin pass, and the program's
/// `@textures` slot assignment.
///
/// [`check_pipeline_profile`] builds this and DROPS it; emission builds it and
/// renders it.
pub(crate) struct PipelinePlan {
    vshape: VertexShape,
    fshape: FragShape,
    vert: StagePlan,
    frag: StagePlan,
    /// What the program's interface MEANS -- roles, names, order, neutral
    /// kinds. Target-neutral, and the sole input to `abi`.
    pub(crate) interface: ShaderInterfacePlan,
    /// GLSL's physical answer to `interface`.
    abi: ShaderAbiPlan,
}

pub(crate) fn plan_shader_pipeline_from_view(
    ws: &Workspace,
    scx: &ScalarCtx,
    view: &ShaderProgramView,
) -> Result<PipelinePlan, CompilerError> {
    let vertex = view
        .stages
        .iter()
        .find(|stage| stage.stage == ShaderStage::Vertex)
        .ok_or_else(|| {
            CompilerError::new_no_location(
                "shader graph/view has no vertex stage for a pipeline".to_string(),
            )
        })?;
    let fragment = view
        .stages
        .iter()
        .find(|stage| stage.stage == ShaderStage::Fragment)
        .ok_or_else(|| {
            CompilerError::new_no_location(
                "shader graph/view has no fragment stage for a pipeline".to_string(),
            )
        })?;
    plan_shader_pipeline_inner(
        ws,
        scx,
        vertex.entry.func,
        fragment.entry.func,
        (vertex, fragment),
        &view.concrete_plans,
    )
}

fn plan_shader_pipeline_inner(
    ws: &Workspace,
    scx: &ScalarCtx,
    vfref: FuncRef,
    ffref: FuncRef,
    stage_views: (&ShaderStageView, &ShaderStageView),
    concrete_plans: &Arc<ConcretePlans>,
) -> Result<PipelinePlan, CompilerError> {
    let vf = &ws.module(vfref.module).functions[vfref.func];
    let ff = &ws.module(ffref.module).functions[ffref.func];
    if shader_stage(vf) != Some("vertex") {
        return Err(err(
            vf.name_span,
            format!(
                "shader pipeline: the vertex root `{}` must carry @shader(vertex)",
                vf.name
            ),
        ));
    }
    if shader_stage(ff) != Some("fragment") {
        return Err(err(
            ff.name_span,
            format!(
                "shader pipeline: the fragment root `{}` must carry @shader(fragment)",
                ff.name
            ),
        ));
    }

    let vshape = classify_vertex(ws, vf)?;
    let varyings = classify_varyings(ws, vshape.varyings_ty, vf.name_span)?;
    let fshape = classify_pipeline_fragment(ws, ff, vshape.varyings_ty)?;

    // One std140 block per STAGE, each at its own binding index. The stages
    // declare different `@uniform` types and cannot be made to share one: a
    // vertex entry is reused across several fragment entries (`ui_vert`
    // serves three), so a merged per-program type would need that one fn to
    // declare a different struct per program.
    //
    // A stage group holds only what that shader OWNS: the engine-authored
    // camera/time/screen band lives in `@engine` instead, which is what lets
    // instanced grass keep a shader-owned `entity_count` beside them.
    let vert_group = vshape
        .uniform_group
        .map(|g| logical_uniform_group(ws, UniformRole::Stage(IrStage::Vertex), g, vf.name_span))
        .transpose()?;
    let frag_group = fshape
        .uniform_group
        .map(|g| logical_uniform_group(ws, UniformRole::Stage(IrStage::Fragment), g, ff.name_span))
        .transpose()?;

    // `@engine` is ONE block per program, so unlike `@uniform` the stages must
    // agree: it is engine-owned and fixed, and both stages naming the same block
    // at index 0 is what lets the engine fill it once per pass rather than once
    // per stage.
    //
    // What this does NOT check is that the type IS the engine's own `EngineU` --
    // deliberately. A compiler that knows an engine type by name has engine
    // policy inside the language. The fingerprint the plan ships is what the
    // ENGINE compares at registration, which is the layer that owns the answer;
    // a game marking its own struct `@engine` gets a block nothing fills, and
    // that check belongs there rather than here.
    let engine_group = match (vshape.engine_group, fshape.engine_group) {
        (Some(v), Some(f)) if v != f => {
            return Err(err(
                vf.name_span,
                format!(
                    "shader pipeline: the stages declare different @engine groups (`{v}` and `{f}`) -- the engine block is one per program"
                ),
            ));
        }
        (v, f) => v.or(f),
    };
    let engine_logical = engine_group
        .map(|g| logical_uniform_group(ws, UniformRole::Engine, g, vf.name_span))
        .transpose()?;

    // One array block per stage that declares `@size(N)` params, at its own
    // reserved binding. Built from the SAME std140 authority as the scalar
    // blocks above and refusing the same element set, so the declaration and the
    // host's offsets cannot come from two different answers.
    let vert_array_group = logical_array_group(IrStage::Vertex, vf.name, &vshape.arrays);
    let frag_array_group = logical_array_group(IrStage::Fragment, ff.name, &fshape.arrays);

    // `@textures` is one group per PROGRAM (texture units are program-scoped),
    // unlike `@uniform`, which is one block per STAGE.
    let group = match (vshape.texture_group, fshape.texture_group) {
        (Some(v), Some(f)) if v != f => {
            return Err(err(
                vf.name_span,
                format!(
                    "shader pipeline: the stages declare different @textures groups (`{v}` and `{f}`) -- texture units are program-scoped, so one program has one group"
                ),
            ));
        }
        (v, f) => v.or(f),
    };
    let bare = has_bare_sampler(&vshape.samplers, &vshape.texture_params)
        || has_bare_sampler(&fshape.samplers, &fshape.texture_params);
    // WHICH stages read the group -- a stage that samples must declare the
    // sampler in its own section, so "the program has one" cannot drive
    // emission.
    let group_stages: Vec<IrStage> = [
        vshape.texture_group.is_some().then_some(IrStage::Vertex),
        fshape.texture_group.is_some().then_some(IrStage::Fragment),
    ]
    .into_iter()
    .flatten()
    .collect();
    let textures = logical_textures(ws, group, bare, group_stages, vf.name_span)?;

    // THE program's logical interface. Everything downstream -- the name
    // census, the ABI plan and every emitted declaration -- reads this.
    let interface = ShaderInterfacePlan {
        stages: vec![IrStage::Vertex, IrStage::Fragment],
        vertex_inputs: vshape.inputs.clone(),
        varyings: varyings.varyings.clone(),
        fragment_output: fshape.output.clone(),
        frag_depth: fshape.frag_depth,
        bare_samplers: bare_sampler_list(&vshape.samplers, &vshape.texture_params, IrStage::Vertex)
            .into_iter()
            .chain(bare_sampler_list(
                &fshape.samplers,
                &fshape.texture_params,
                IrStage::Fragment,
            ))
            .collect(),
        uniform_groups: engine_logical
            .into_iter()
            .chain(vert_group)
            .chain(frag_group)
            .collect(),
        array_groups: vert_array_group
            .into_iter()
            .chain(frag_array_group)
            .collect(),
        textures,
    };

    check_pipeline_collisions(&interface, vf.name_span)?;
    check_stage_sampler_budget(&interface, IrStage::Vertex, &vf.name, vf.name_span)?;
    check_stage_sampler_budget(&interface, IrStage::Fragment, &ff.name, ff.name_span)?;
    let abi = ShaderAbiPlan::glsl(&interface, vf.name_span)?;
    check_block_name_collisions(
        abi.uniform_blocks.iter().map(|(_, p)| p),
        abi.uniform_array_blocks.iter().map(|(_, p)| p),
        vf.name_span,
    )?;

    let vert = plan_stage_from_view(ws, scx, stage_views.0, Stage::Vertex, concrete_plans)?;
    let frag = plan_stage_from_view(ws, scx, stage_views.1, Stage::Fragment, concrete_plans)?;
    reject_position_read(
        ws,
        &frag.ir.body,
        frag.ir.body.params[fshape.varyings_param],
        ff.name_span,
    )?;

    Ok(PipelinePlan {
        vshape,
        fshape,
        vert,
        frag,
        interface,
        abi,
    })
}

/// The PIPELINE counterpart of [`check_entry_profile`]: emission's own
/// preparation for a `(vertex, fragment)` pair, run without emitting.
///
/// A pipeline fragment is NOT checkable as a single entry: `classify_entry`
/// refuses its varyings param outright, which is why the mono view must be
/// given the PAIRS production registers rather than a flat root list.
pub fn check_pipeline_profile(
    ws: &Workspace,
    vfref: FuncRef,
    ffref: FuncRef,
) -> Result<(), CompilerError> {
    let session = shader_program_session(
        ws,
        ShaderProgram {
            vert: Some(vfref),
            frag: ffref,
        },
    )?;
    check_pipeline_profile_with_view(ws, &session.scx, &session.view)
}

/// Validate one shader pipeline against the graph-owned concrete populations.
pub fn check_pipeline_profile_with_view(
    ws: &Workspace,
    scx: &ScalarCtx,
    view: &ShaderProgramView,
) -> Result<(), CompilerError> {
    if view.program.vert.is_none() {
        return Err(CompilerError::new_no_location(
            "shader graph/view single-entry program passed to pipeline GLSL validation".to_string(),
        ));
    }
    plan_shader_pipeline_from_view(ws, scx, view).map(|_| ())
}

pub fn emit_shader_pipeline_with_dependencies(
    ws: &Workspace,
    mid: ModuleId,
    vert_root: &str,
    frag_root: &str,
    profile: GlslProfile,
) -> Result<ShaderEmission, CompilerError> {
    let vfref = resolve_root(ws, mid, vert_root)?;
    let ffref = resolve_root(ws, mid, frag_root)?;
    let session = shader_program_session(
        ws,
        ShaderProgram {
            vert: Some(vfref),
            frag: ffref,
        },
    )?;
    emit_shader_pipeline_with_view(ws, &session.scx, &session.view, profile)
}

/// Emit a pipeline from the graph-owned stage populations in `view`.
pub fn emit_shader_pipeline_with_view(
    ws: &Workspace,
    scx: &ScalarCtx,
    view: &ShaderProgramView,
    profile: GlslProfile,
) -> Result<ShaderEmission, CompilerError> {
    if view.program.vert.is_none() {
        return Err(CompilerError::new_no_location(
            "shader graph/view single-entry program passed to pipeline GLSL emission".to_string(),
        ));
    }
    let plan = plan_shader_pipeline_from_view(ws, scx, view)?;
    render_shader_pipeline(ws, scx.interner(), &plan, profile)
}

pub(crate) fn render_shader_pipeline(
    ws: &Workspace,
    interner: &MonoInterner,
    plan: &PipelinePlan,
    profile: GlslProfile,
) -> Result<ShaderEmission, CompilerError> {
    // Validated ONCE for the program: an unknown binding belongs to no stage,
    // so re-running the check per stage says nothing the first run did not.
    let vspan = ws.module(plan.vert.ir.entry.fref().module).functions
        [plan.vert.ir.entry.fref().func]
        .name_span;
    let program = ProgramAbi::of(
        &plan.interface,
        &plan.abi,
        &[IrStage::Vertex, IrStage::Fragment],
        vspan,
    )?;

    let (vertex_block, vertex_dependencies) = emit_vertex_stage(
        ws,
        interner,
        &plan.vert,
        &plan.vshape,
        &plan.interface,
        &program,
    )?;
    let (pixel_block, pixel_dependencies) = emit_fragment_stage(
        ws,
        interner,
        &plan.frag,
        &plan.fshape,
        &plan.interface,
        &program,
    )?;

    let version = profile_header(profile);
    let source = format!(
        "{version}\n\n#ifdef VERTEX\n\n{vertex_block}\n#endif\n\n#ifdef PIXEL\n\n{pixel_block}\n#endif\n"
    );
    let dependencies = vertex_dependencies
        .into_iter()
        .chain(pixel_dependencies)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    // Each block carries its binding, taken from the param ROLE: the engine
    // block at 0 (both stages name it), the vertex group at 1, the fragment
    // group at 2. Unlike `@textures` -- one group per PROGRAM, since texture
    // units are program-scoped -- `@uniform` blocks are per STAGE, so differing
    // types there are correct rather than a collision.
    Ok(ShaderEmission {
        source,
        dependencies,
        interface: plan.interface.clone(),
        abi: plan.abi.clone(),
        ir: ShaderIR {
            interner: std::sync::Arc::clone(&plan.vert.concrete_interner),
            stages: vec![plan.vert.ir.clone(), plan.frag.ir.clone()],
        },
    })
}
