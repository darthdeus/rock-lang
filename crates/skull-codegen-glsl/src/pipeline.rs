//! Vertex/fragment pipelines: stage emission, collisions and planning.

use super::*;

/// One stage's varyings, as `in` or `out` declarations.
fn varying_decls(iface: &ShaderInterfacePlan, dir: &str) -> String {
    let mut out = String::new();
    for (n, t, flat) in interface_varyings(iface) {
        let q = if flat { "flat " } else { "" };
        out.push_str(&format!("{q}{dir} {} {n};\n", t.name()));
    }
    out
}

fn emit_vertex_stage(
    ws: &Workspace,
    interner: &MonoInterner,
    stage: &StagePlan,
    v: &VertexShape,
    iface: &ShaderInterfacePlan,
    abi: &ShaderAbiPlan,
) -> Result<String, CompilerError> {
    let resolved = stage_abi(iface, abi, IrStage::Vertex);
    let shape = EntryShape {
        global_params: v
            .attr_params
            .iter()
            .copied()
            .chain(v.res.global_params())
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
        sampler_params: v.res.samplers.iter().map(|&(i, _, _)| i).collect(),
        arrays: v.res.arrays.iter().map(|a| a.param).collect(),
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

    let mut out = String::new();
    for (loc, (name, kind)) in iface
        .vertex_inputs
        .iter()
        .flat_map(|input| &input.attrs)
        .enumerate()
    {
        out.push_str(&format!(
            "layout (location = {loc}) in {} {name};\n",
            attr_glsl_ty(*kind).name(),
        ));
    }
    out.push('\n');
    // Vertex texture fetch is legal in GL 3.3, so the vertex stage declares the
    // samplers it reads like the fragment stage does.
    out.push_str(&resolved.declarations());
    if resolved.uniform_block.is_some()
        || resolved.engine_block.is_some()
        || resolved.array_block.is_some()
    {
        out.push('\n');
    }
    out.push_str(&varying_decls(iface, "out"));
    out.push('\n');
    out.push_str(&stage.render_helpers(ws, interner)?);
    out.push_str(&stage.render_entry(ws, interner, &shape)?);
    Ok(out)
}

fn emit_fragment_stage(
    ws: &Workspace,
    interner: &MonoInterner,
    stage: &StagePlan,
    fs: &FragShape,
    iface: &ShaderInterfacePlan,
    abi: &ShaderAbiPlan,
) -> Result<String, CompilerError> {
    let fref = stage.ir.entry.fref();
    let entry_f = &ws.module(fref.module).functions[fref.func];
    let resolved = stage_abi(iface, abi, IrStage::Fragment);
    // MRT: the entry is emitted as `void main()` destructuring its returned
    // output struct into the `layout(location=N) out` globals (like the vertex
    // varyings path). Single-output keeps the `vec4 <fn>()` + wrapper form.
    let mrt_outputs = interface_mrt_outputs(iface);
    let main_out = mrt_outputs.as_ref().map(|outs| MainOut {
        fields: outs.iter().copied().collect(),
        builtin: iface
            .frag_depth
            .then_some((FRAG_DEPTH_FIELD, "gl_FragDepth", GlslTy::Float)),
    });
    let shape = EntryShape {
        global_params: std::iter::once(VARYINGS_PARAM)
            .chain(fs.res.global_params())
            .collect(),
        uniform_fields: iface
            .varyings
            .iter()
            .map(|v| (v.name, wire_glsl(v.kind)))
            .chain(stage_uniform_fields(iface, IrStage::Fragment))
            .collect(),
        sampler_params: fs.res.samplers.iter().map(|&(i, _, _)| i).collect(),
        arrays: fs.res.arrays.iter().map(|a| a.param).collect(),
        main_out,
        reserved: entry_reserved_names(iface),
    };

    let mut out = varying_decls(iface, "in");
    match &mrt_outputs {
        None => {
            // `None` here means the interface declares no colour output while a
            // FRAGMENT stage is being emitted -- an inconsistent plan, not a
            // shape to guess a `vec4` for.
            let FragmentOutput::Single(k) = iface.fragment_output else {
                return Err(err(
                    entry_f.name_span,
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
    out.push_str(&resolved.declarations());
    out.push('\n');
    out.push_str(&stage.render_helpers(ws, interner)?);
    out.push_str(&stage.render_entry(ws, interner, &shape)?);
    // Single-output: the entry fn returns Vec4; main() forwards it to FragColor.
    // MRT: the entry IS `void main()` (destructured), so no wrapper.
    if mrt_outputs.is_none() {
        out.push_str(&format!(
            "\nvoid main() {{\n  FragColor = {}();\n}}\n",
            symbol_of_key(entry_f.name)
        ));
    }
    Ok(out)
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

/// Reject two interface names that would land in GLSL's ONE cross-stage
/// namespace.
///
/// An unnamed-instance block's members are bare globals, so the `@engine`
/// block's members sit in that namespace too, and a missed pair is a raw driver
/// error with no Rock span. The census is the INTERFACE (see
/// `interface_global_names`), so the engine block, declared in both stages but
/// ONE block, is listed once.
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

/// The retained vertex and fragment plans and their shared interface.
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
    let vertex = program_stage(view, ShaderStage::Vertex, "a pipeline")?;
    let fragment = program_stage(view, ShaderStage::Fragment, "a pipeline")?;
    let (vfref, ffref) = (vertex.entry.func, fragment.entry.func);
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

    // A shared vertex entry can pair with fragments that use different uniform types.
    let vert_group = vshape
        .res
        .uniform_group
        .map(|g| logical_uniform_group(ws, UniformRole::Stage(IrStage::Vertex), g, vf.name_span))
        .transpose()?;
    let frag_group = fshape
        .res
        .uniform_group
        .map(|g| logical_uniform_group(ws, UniformRole::Stage(IrStage::Fragment), g, ff.name_span))
        .transpose()?;

    // Both stages agree on one engine block. The host checks its layout fingerprint.
    let engine_group = match (vshape.res.engine_group, fshape.res.engine_group) {
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
    let vert_array_group = logical_array_group(IrStage::Vertex, vf.name, &vshape.res.arrays);
    let frag_array_group = logical_array_group(IrStage::Fragment, ff.name, &fshape.res.arrays);

    // `@textures` is one group per PROGRAM (texture units are program-scoped),
    // unlike `@uniform`, which is one block per STAGE.
    let group = match (vshape.res.texture_group, fshape.res.texture_group) {
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
    let bare = vshape.res.has_bare_sampler() || fshape.res.has_bare_sampler();
    // WHICH stages read the group -- a stage that samples must declare the
    // sampler in its own section, so "the program has one" cannot drive
    // emission.
    let group_stages: Vec<IrStage> = [
        vshape
            .res
            .texture_group
            .is_some()
            .then_some(IrStage::Vertex),
        fshape
            .res
            .texture_group
            .is_some()
            .then_some(IrStage::Fragment),
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
        bare_samplers: vshape
            .res
            .bare_samplers(IrStage::Vertex)
            .into_iter()
            .chain(fshape.res.bare_samplers(IrStage::Fragment))
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

    let concrete_plans = &view.concrete_plans;
    let vert = plan_stage_from_view(ws, scx, vertex, IrStage::Vertex, concrete_plans)?;
    let frag = plan_stage_from_view(ws, scx, fragment, IrStage::Fragment, concrete_plans)?;
    reject_position_read(
        &frag.ir.body,
        frag.ir.body.params[VARYINGS_PARAM],
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

pub(crate) fn render_shader_pipeline(
    ws: &Workspace,
    interner: &MonoInterner,
    plan: &PipelinePlan,
    profile: GlslProfile,
) -> Result<ShaderEmission, CompilerError> {
    let vertex_block = emit_vertex_stage(
        ws,
        interner,
        &plan.vert,
        &plan.vshape,
        &plan.interface,
        &plan.abi,
    )?;
    let pixel_block = emit_fragment_stage(
        ws,
        interner,
        &plan.frag,
        &plan.fshape,
        &plan.interface,
        &plan.abi,
    )?;

    let version = profile_header(profile);
    let source = format!(
        "{version}\n\n#ifdef VERTEX\n\n{vertex_block}\n#endif\n\n#ifdef PIXEL\n\n{pixel_block}\n#endif\n"
    );
    let dependencies = plan
        .vert
        .dependencies
        .iter()
        .chain(&plan.frag.dependencies)
        .copied()
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
