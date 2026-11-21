use super::*;

pub(crate) struct EntryShape {
    pub(crate) global_params: Vec<usize>,
    pub(crate) uniform_fields: Vec<(Ustr, GlslTy)>,
    pub(crate) sampler_params: Vec<usize>,
    pub(crate) arrays: Vec<usize>,
    pub(crate) main_out: Option<MainOut>,
    pub(crate) reserved: Vec<Ustr>,
}

pub(crate) const FULLSCREEN_VERTEX: &str = "layout (location = 0) in vec2 aPos;
layout (location = 1) in vec2 aTexCoord;

out vec2 TexCoord;

void main() {
    gl_Position = vec4(aPos, 0.0, 1.0);
    TexCoord = aTexCoord;
}
";

pub(crate) fn nominal_name(ws: &Workspace, ty: TyId) -> Option<Ustr> {
    match ty_expr(ws, ty)? {
        TypeExpr::App {
            ctor: TypeCtor::Nominal(n),
            ..
        } => Some(n),
        _ => None,
    }
}

pub struct ShaderEmission {
    pub source: String,
    pub dependencies: Vec<FuncRef>,
    pub interface: ShaderInterfacePlan,
    /// GLSL's physical answer: bindings, std140 layouts, fingerprints and
    /// texture units. The engine binds against THIS -- it never parses `source`
    /// and never recomputes a layout.
    pub abi: ShaderAbiPlan,
    pub ir: ShaderIR,
}

pub(crate) struct StagePlan {
    pub(crate) ir: ShaderStageIr,
    pub(crate) dependencies: Vec<FuncRef>,
}

impl StagePlan {
    pub(crate) fn render(
        &self,
        out: &mut String,
        ws: &Workspace,
        interner: &MonoInterner,
        plans: &ConcretePlans,
        shape: &EntryShape,
    ) -> Result<(), CompilerError> {
        out.push_str(&helper_prelude(
            self.ir.bodies.values().chain([&self.ir.body]),
        ));
        for href in &self.ir.order {
            let body_plan = plans.get(&href.inst).ok_or_else(|| {
                let f = &ws.module(href.fref().module).functions[href.fref().func];
                err(f.name_span, "shader profile: helper has no concrete plan")
            })?;
            out.push_str(&emit_fn_def_with(
                ws,
                interner,
                href,
                &self.ir.bodies[href],
                None,
                body_plan,
            )?);
            out.push('\n');
        }
        let entry = &self.ir.entry;
        let plan = plans.get(&entry.inst).ok_or_else(|| {
            let f = &ws.module(entry.fref().module).functions[entry.fref().func];
            err(f.name_span, "shader profile: entry has no concrete plan")
        })?;
        out.push_str(&emit_fn_def_with(
            ws,
            interner,
            entry,
            &self.ir.body,
            Some(shape),
            plan,
        )?);
        Ok(())
    }
}

pub(crate) fn plan_stage_from_view(
    ws: &Workspace,
    scx: &ScalarCtx,
    view: &ShaderStageView,
    expected: ShaderStage,
    concrete_plans: &ConcretePlans,
) -> Result<StagePlan, CompilerError> {
    if view.stage != expected {
        return Err(CompilerError::new_no_location(format!(
            "shader graph/view stage mismatch: expected {expected:?}, got {:?}",
            view.stage
        )));
    }
    let order_set: BTreeSet<_> = view.order.iter().cloned().collect();
    if order_set.len() != view.order.len()
        || order_set != view.helpers
        || order_set.contains(&view.entry)
    {
        return Err(CompilerError::new_no_location(
            "shader graph/view helper order is not a unique retained helper population".to_string(),
        ));
    }
    let mut expected_retained = order_set;
    expected_retained.insert(view.entry.clone());
    if view.retained != expected_retained {
        return Err(CompilerError::new_no_location(
            "shader graph/view retained population is not entry plus helpers".to_string(),
        ));
    }
    let entry = ShaderInstance::new(view.entry.clone());
    let mut const_globals = HashMap::default();
    let mut builtins = BTreeSet::new();
    let mut calls = HashMap::default();
    let mut lower = |instance: &ShaderInstance, is_entry| -> Result<MirFunc, CompilerError> {
        let (body, callees) = lower_profiled_body(
            ws,
            scx,
            &instance.inst,
            concrete_plans,
            is_entry,
            Some(&mut const_globals),
        )?;
        let callees = checked_body_calls(
            ws,
            instance,
            callees,
            &view.retained,
            scx.interner(),
            &mut builtins,
            "shader graph/view",
        )?;
        calls.insert(instance.clone(), callees);
        Ok(body)
    };
    let body = lower(&entry, true)?;
    let mut bodies = HashMap::default();
    let order: Vec<_> = view
        .order
        .iter()
        .cloned()
        .map(ShaderInstance::new)
        .collect();
    for helper in &order {
        bodies.insert(helper.clone(), lower(helper, false)?);
    }
    let declarations = |instances: &BTreeSet<Instance>| -> HashSet<FuncRef> {
        instances.iter().map(|instance| instance.func).collect()
    };
    if declarations(&builtins) != declarations(&view.builtins) {
        return Err(CompilerError::new_no_location(
            "shader graph/view builtin declaration population differs from lowered calls"
                .to_string(),
        ));
    }
    reject_population_cycles(ws, &calls)?;
    let span = ws.module(entry.fref().module).functions[entry.fref().func].name_span;
    reject_stage_violations(ws, view.stage, bodies.values().chain([&body]), span)?;
    let ir = ShaderStageIr {
        stage: view.stage,
        entry,
        body,
        order,
        bodies,
        const_globals,
    };
    if ir.declarations() != view.dependencies {
        return Err(CompilerError::new_no_location(
            "shader graph/view hot-reload dependencies differ from lowered order".to_string(),
        ));
    }
    Ok(StagePlan {
        ir,
        dependencies: view.dependencies.clone(),
    })
}

pub(crate) fn reject_population_cycles(
    ws: &Workspace,
    calls: &HashMap<ShaderInstance, Vec<ShaderInstance>>,
) -> Result<(), CompilerError> {
    fn visit(
        ws: &Workspace,
        node: &ShaderInstance,
        calls: &HashMap<ShaderInstance, Vec<ShaderInstance>>,
        active: &mut HashSet<ShaderInstance>,
        done: &mut HashSet<ShaderInstance>,
    ) -> Result<(), CompilerError> {
        if active.contains(node) {
            let f = &ws.module(node.fref().module).functions[node.fref().func];
            return Err(err(
                f.name_span,
                format!(
                    "shader profile: recursion through `{}` is not allowed on GLSL",
                    f.name
                ),
            ));
        }
        if !done.insert(node.clone()) {
            return Ok(());
        }
        active.insert(node.clone());
        if let Some(callees) = calls.get(node) {
            for callee in callees {
                visit(ws, callee, calls, active, done)?;
            }
        }
        active.remove(node);
        Ok(())
    }

    let mut active = HashSet::default();
    let mut done = HashSet::default();
    for node in calls.keys() {
        visit(ws, node, calls, &mut active, &mut done)?;
    }
    Ok(())
}

pub(crate) fn program_stage<'a>(
    view: &'a ShaderProgramView,
    stage: ShaderStage,
    form: &str,
) -> Result<&'a ShaderStageView, CompilerError> {
    view.stages
        .iter()
        .find(|s| s.stage == stage)
        .ok_or_else(|| {
            CompilerError::new_no_location(format!(
                "shader graph/view has no {} stage for {form}",
                stage.name()
            ))
        })
}

pub(crate) fn plan_shader_entry_program(
    ws: &Workspace,
    scx: &ScalarCtx,
    view: &ShaderProgramView,
) -> Result<ShaderPlan, CompilerError> {
    let stage_view = program_stage(view, ShaderStage::Fragment, "a single-entry shader")?;
    let fref = stage_view.entry.func;
    let f = &ws.module(fref.module).functions[fref.func];
    match shader_stage(f) {
        Some("fragment") => {}
        Some(stage) => {
            return Err(err(
                f.name_span,
                format!(
                    "shader entry: @shader({stage}) is not supported yet -- only @shader(fragment) (vertex entries are A2)"
                ),
            ));
        }
        None => {
            return Err(err(
                f.name_span,
                format!(
                    "shader entry: `{}` has no @shader(fragment) annotation",
                    f.name
                ),
            ));
        }
    }
    let res = classify_entry(ws, f)?;
    let stage = plan_stage_from_view(ws, scx, stage_view, IrStage::Fragment, &view.concrete_plans)?;
    let bare_samplers: Vec<_> = res.bare_samplers(IrStage::Fragment).collect();
    let interface = ShaderInterfacePlan {
        stages: vec![IrStage::Fragment],
        uniform_groups: res
            .uniform_group
            .map(|g| {
                logical_uniform_group(ws, UniformRole::Stage(IrStage::Fragment), g, f.name_span)
            })
            .transpose()?
            .into_iter()
            .collect(),
        textures: logical_textures(
            ws,
            None,
            res.textures(),
            !bare_samplers.is_empty(),
            f.name_span,
        )?,
        bare_samplers,
        fragment_output: FragmentOutput::Single(WireKind::F32x4),
        ..Default::default()
    };
    check_entry_collisions(&interface, f.name_span)?;
    check_stage_sampler_budget(&interface, IrStage::Fragment, &f.name, f.name_span)?;
    let abi = ShaderAbiPlan::glsl(&interface, f.name_span)?;
    Ok(ShaderPlan {
        interface,
        abi,
        concrete_plans: Arc::clone(&view.concrete_plans),
        interner: scx.interner_handle(),
        vertex: None,
        fragment: (stage, res),
    })
}
