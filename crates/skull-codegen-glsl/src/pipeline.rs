use super::*;

fn emit_varyings(out: &mut String, iface: &ShaderInterfacePlan, dir: &str) {
    for v in &iface.varyings {
        let flat = if v.flat { "flat " } else { "" };
        out.push_str(&format!(
            "{flat}{dir} {} {};\n",
            wire_glsl(v.kind).name(),
            v.name
        ));
    }
}

pub(crate) fn emit_vertex_stage(
    ws: &Workspace,
    interner: &MonoInterner,
    plan: &ShaderPlan,
    v: &VertexStage,
) -> Result<String, CompilerError> {
    let iface = &plan.interface;
    let shape = EntryShape {
        global_params: v
            .attr_params
            .iter()
            .copied()
            .chain(v.resources.global_params())
            .collect(),
        uniform_fields: iface
            .vertex_inputs
            .iter()
            .flat_map(|i| i.attrs.iter().map(|(n, k)| (*n, attr_glsl_ty(*k))))
            .chain(stage_uniform_fields(iface, IrStage::Vertex))
            .collect(),
        sampler_params: v.resources.sampler_params().collect(),
        arrays: v.resources.arrays.iter().map(|a| a.param).collect(),
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
    emit_stage_resources(&mut out, iface, &plan.abi, IrStage::Vertex);
    emit_varyings(&mut out, iface, "out");
    out.push('\n');
    v.plan
        .render(&mut out, ws, interner, &plan.concrete_plans, &shape)?;
    Ok(out)
}

pub(crate) fn emit_fragment_stage(
    ws: &Workspace,
    interner: &MonoInterner,
    plan: &ShaderPlan,
) -> Result<String, CompilerError> {
    let (stage, res) = &plan.fragment;
    let iface = &plan.interface;
    let mut out = String::new();
    let fullscreen = plan.vertex.is_none();
    if fullscreen {
        out.push_str("in vec2 TexCoord;\n");
    } else {
        emit_varyings(&mut out, iface, "in");
    }
    let main_out = match &iface.fragment_output {
        FragmentOutput::Single(kind) => {
            out.push_str(&format!("out {} FragColor;\n\n", wire_glsl(*kind).name()));
            None
        }
        FragmentOutput::Mrt(outs) => {
            for (loc, (name, kind)) in outs.iter().enumerate() {
                let ty = wire_glsl(*kind).name();
                out.push_str(&format!("layout(location = {loc}) out {ty} {name};\n"));
            }
            out.push('\n');
            Some(MainOut {
                fields: outs
                    .iter()
                    .map(|&(name, kind)| (name, wire_glsl(kind)))
                    .collect(),
                builtin: iface.frag_depth.then_some((
                    FRAG_DEPTH_FIELD,
                    "gl_FragDepth",
                    GlslTy::Float,
                )),
            })
        }
        FragmentOutput::None => unreachable!("a planned fragment stage has an output"),
    };
    let shape = EntryShape {
        global_params: (!fullscreen)
            .then_some(VARYINGS_PARAM)
            .into_iter()
            .chain(res.global_params())
            .collect(),
        uniform_fields: iface
            .varyings
            .iter()
            .map(|v| (v.name, wire_glsl(v.kind)))
            .chain(stage_uniform_fields(iface, IrStage::Fragment))
            .collect(),
        sampler_params: res.sampler_params().collect(),
        arrays: res.arrays.iter().map(|a| a.param).collect(),
        main_out,
        reserved: entry_reserved_names(iface),
    };

    emit_stage_resources(&mut out, iface, &plan.abi, IrStage::Fragment);
    stage.render(&mut out, ws, interner, &plan.concrete_plans, &shape)?;
    if shape.main_out.is_none() {
        let fref = stage.ir.entry.fref();
        let entry_f = &ws.module(fref.module).functions[fref.func];
        let args = if fullscreen { "TexCoord" } else { "" };
        out.push_str(&format!(
            "\nvoid main() {{\n  FragColor = {}({args});\n}}\n",
            symbol_of_key(entry_f.name)
        ));
    }
    Ok(out)
}

pub(crate) fn interface_global_names(
    iface: &ShaderInterfacePlan,
) -> impl Iterator<Item = Ustr> + '_ {
    let outputs = match &iface.fragment_output {
        FragmentOutput::Mrt(outs) => outs.as_slice(),
        _ => &[],
    };
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
        .chain(outputs.iter().map(|(name, _)| *name))
}

pub(crate) fn entry_reserved_names(iface: &ShaderInterfacePlan) -> Vec<Ustr> {
    interface_global_names(iface)
        .chain([
            Ustr::from("TexCoord"),
            Ustr::from("FragColor"),
            Ustr::from("gl_Position"),
            Ustr::from("gl_FragDepth"),
        ])
        .collect()
}

/// Unnamed block members share the cross-stage GLSL global namespace; the
/// engine block is declared in both stages but counted only once.
fn check_pipeline_collisions(iface: &ShaderInterfacePlan, span: Span) -> Result<(), CompilerError> {
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

/// Block names have a separate GLSL namespace. Generated array block names
/// must not collide with user-defined block names.
fn check_block_name_collisions<'a>(
    scalar: impl Iterator<Item = &'a GlslUniformBlock>,
    arrays: impl Iterator<Item = &'a UniformArrayBlockPlan>,
    span: Span,
) -> Result<(), CompilerError> {
    let mut seen: HashSet<Ustr> = HashSet::default();
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

pub(crate) fn plan_shader_pipeline_from_view(
    ws: &Workspace,
    scx: &ScalarCtx,
    view: &ShaderProgramView,
) -> Result<ShaderPlan, CompilerError> {
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

    let vert_array_group = logical_array_group(IrStage::Vertex, vf.name, &vshape.res.arrays);
    let frag_array_group = logical_array_group(IrStage::Fragment, ff.name, &fshape.res.arrays);

    let bare_samplers: Vec<_> = vshape
        .res
        .bare_samplers(IrStage::Vertex)
        .chain(fshape.res.bare_samplers(IrStage::Fragment))
        .collect();
    let textures = logical_textures(
        ws,
        vshape.res.textures(),
        fshape.res.textures(),
        !bare_samplers.is_empty(),
        vf.name_span,
    )?;

    let interface = ShaderInterfacePlan {
        stages: vec![IrStage::Vertex, IrStage::Fragment],
        vertex_inputs: vshape.inputs,
        varyings,
        fragment_output: fshape.output,
        frag_depth: fshape.frag_depth,
        bare_samplers,
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

    Ok(ShaderPlan {
        interface,
        abi,
        concrete_plans: Arc::clone(concrete_plans),
        interner: scx.interner_handle(),
        vertex: Some(VertexStage {
            plan: vert,
            attr_params: vshape.attr_params,
            resources: vshape.res,
        }),
        fragment: (frag, fshape.res),
    })
}
