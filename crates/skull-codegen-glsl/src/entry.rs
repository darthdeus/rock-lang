//! Single-entry shaders: shapes, stage plans and rendering.

use super::*;

/// A classified shader entry signature (A1 fragment, or an A2 pipeline stage).
pub(crate) struct EntryShape {
    /// Param indices whose field reads render as bare GLSL globals and which are
    /// dropped from the emitted signature: the A1 uniform struct, or a pipeline
    /// stage's attribute / uniform / varyings structs.
    pub(crate) global_params: Vec<usize>,
    /// Combined global field name -> GLSL type (for `ty_of` on bare-global
    /// reads). Names are collision-checked unique across all global structs.
    pub(crate) uniform_fields: Vec<(Ustr, GlslTy)>,
    /// Sampler param INDICES, dropped from the emitted signature and read as
    /// bare globals. Indices only: which samplers exist and what kind they are
    /// is the interface's answer, and a second copy here is what let the vertex
    /// stage reference a sampler it never declared.
    pub(crate) sampler_params: Vec<usize>,
    /// `UniformArray<T>` param indices. Dropped from the signature and referenced
    /// by name (via `at()` -> indexing); NOT tier-0-typed, so they are skipped
    /// when typing param cells.
    pub(crate) arrays: Vec<usize>,
    /// `void main()` mode: presence switches emission to `void main()` with a
    /// return-struct destructure (vertex varyings, or MRT fragment outputs).
    pub(crate) main_out: Option<MainOut>,
    /// Global names the entry's locals and value params must not take (every
    /// bare global the program declares, so hoisting cannot shadow one).
    pub(crate) reserved: Vec<Ustr>,
}

/// The default fullscreen vertex stage -- identical to the hand-written
/// vignette/heat_shimmer VERTEX blocks (the loader compiles the one file twice
/// with `#define VERTEX` / `#define PIXEL`).
const FULLSCREEN_VERTEX: &str = "layout (location = 0) in vec2 aPos;
layout (location = 1) in vec2 aTexCoord;

out vec2 TexCoord;

void main() {
    gl_Position = vec4(aPos, 0.0, 1.0);
    TexCoord = aTexCoord;
}
";

/// The nominal name behind a declared type, if any (`ShimmerU`, `Sampler2D`).
pub(crate) fn nominal_name(ws: &Workspace, ty: TyId) -> Option<Ustr> {
    match ty_expr(ws, ty)? {
        TypeExpr::App {
            ctor: TypeCtor::Nominal(n),
            ..
        } => Some(n),
        _ => None,
    }
}

/// A1's body-emission view, derived from its completed interface.
///
/// The uniform field types come from `iface`, which got them from
/// `uniform_group_members` -- the same classifier the std140 packer reads, so a
/// body's typing and a block's bytes cannot disagree about a field.
fn entry_shape(
    global_params: &[usize],
    sampler_params: &[usize],
    iface: &ShaderInterfacePlan,
) -> EntryShape {
    EntryShape {
        global_params: global_params.to_vec(),
        uniform_fields: stage_uniform_fields(iface, IrStage::Fragment),
        sampler_params: sampler_params.to_vec(),
        arrays: Vec::new(),
        main_out: None,
        reserved: entry_reserved_names(iface),
    }
}

pub struct ShaderEmission {
    pub source: String,
    pub dependencies: Vec<FuncRef>,
    /// What this program's interface MEANS, with no target in it. Published so
    /// a second backend can answer it without re-classifying the entry.
    pub interface: ShaderInterfacePlan,
    /// GLSL's physical answer: bindings, std140 layouts, fingerprints and
    /// texture units. The engine binds against THIS -- it never parses `source`
    /// and never recomputes a layout.
    pub abi: ShaderAbiPlan,
    /// The program's concrete bodies, per stage. Target-neutral: a second
    /// backend renders these under the same interface rather than re-walking
    /// the entry and re-deciding what the program packages.
    pub ir: ShaderIR,
}

/// Lowered entry and helpers, ready to render with the retained concrete plans.
pub(crate) struct StagePlan {
    /// The stage's concrete bodies -- target-neutral, and the only part of this
    /// plan a non-GLSL backend would want.
    pub(crate) ir: ShaderStageIr,
    /// The stage's helpers plus the stage entry itself -- the hot-reload
    /// dependency set. DECLARATIONS: a reload watches source files, and two
    /// instances of one generic helper share one.
    pub(crate) dependencies: Vec<FuncRef>,
    pub(crate) concrete_plans: Arc<ConcretePlans>,
    pub(crate) concrete_interner: Arc<MonoInterner>,
}

impl StagePlan {
    /// The helpers this stage emits: the int-div/mod prelude the reachable
    /// bodies need, then each helper definition in emission order.
    pub(crate) fn render_helpers(
        &self,
        ws: &Workspace,
        interner: &MonoInterner,
    ) -> Result<String, CompilerError> {
        let mut out = helper_prelude(self.ir.bodies.values().chain([&self.ir.body]));
        for href in &self.ir.order {
            let body_plan = self.concrete_plans.get(&href.inst).ok_or_else(|| {
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
        Ok(out)
    }

    /// The stage entry's definition, rendered under its interface shape.
    pub(crate) fn render_entry(
        &self,
        ws: &Workspace,
        interner: &MonoInterner,
        shape: &EntryShape,
    ) -> Result<String, CompilerError> {
        let entry = &self.ir.entry;
        let plan = self.concrete_plans.get(&entry.inst).ok_or_else(|| {
            let f = &ws.module(entry.fref().module).functions[entry.fref().func];
            err(f.name_span, "shader profile: entry has no concrete plan")
        })?;
        emit_fn_def_with(ws, interner, entry, &self.ir.body, Some(shape), plan)
    }
}

pub(crate) fn plan_stage_from_view(
    ws: &Workspace,
    scx: &ScalarCtx,
    view: &ShaderStageView,
    expected: ShaderStage,
    concrete_plans: &Arc<ConcretePlans>,
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
        concrete_plans: Arc::clone(concrete_plans),
        concrete_interner: scx.interner_handle(),
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

/// The fullscreen fragment entry and its resolved interface.
pub(crate) struct EntryPlan {
    fref: FuncRef,
    shape: EntryShape,
    stage: StagePlan,
    /// What the program's interface MEANS -- roles, names, order, neutral
    /// kinds. Target-neutral, and the sole input to `abi`.
    interface: ShaderInterfacePlan,
    /// GLSL's physical answer to `interface`.
    abi: ShaderAbiPlan,
}

/// The stage `view` holds for `stage`, or a rejection naming the program form.
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

/// The single-entry plan of a program view: the fragment stage planned, for
/// a caller that keeps the plan and renders it later.
pub(crate) fn plan_shader_entry_program(
    ws: &Workspace,
    scx: &ScalarCtx,
    view: &ShaderProgramView,
) -> Result<EntryPlan, CompilerError> {
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
    // The stage classification admits interface params such as uniform structs.
    let class = classify_entry(ws, f)?;
    let stage = plan_stage_from_view(ws, scx, stage_view, IrStage::Fragment, &view.concrete_plans)?;
    let interface = ShaderInterfacePlan {
        stages: vec![IrStage::Fragment],
        uniform_groups: class
            .uniform_group
            .map(|g| {
                logical_uniform_group(ws, UniformRole::Stage(IrStage::Fragment), g, f.name_span)
            })
            .transpose()?
            .into_iter()
            .collect(),
        textures: logical_textures(
            ws,
            class.texture_group,
            has_bare_sampler(&class.samplers, &class.global_params),
            vec![IrStage::Fragment],
            f.name_span,
        )?,
        bare_samplers: bare_sampler_list(&class.samplers, &class.global_params, IrStage::Fragment),
        // A1's entry returns one colour; MRT is an A2 feature.
        fragment_output: FragmentOutput::Single(WireKind::F32x4),
        // A1 has no vertex stage of its own (it rides the fullscreen quad), and
        // `classify_entry` refuses a `UniformArray` param outright -- arrays are
        // an A2 feature, so these are absences rather than omissions.
        ..Default::default()
    };
    check_entry_collisions(&interface, f.name_span)?;
    check_stage_sampler_budget(&interface, IrStage::Fragment, &f.name, f.name_span)?;
    let abi = ShaderAbiPlan::glsl(&interface, f.name_span)?;
    let shape = entry_shape(&class.global_params, &class.sampler_params, &interface);
    Ok(EntryPlan {
        fref,
        shape,
        stage,
        interface,
        abi,
    })
}

pub(crate) fn render_shader_entry(
    ws: &Workspace,
    interner: &MonoInterner,
    plan: &EntryPlan,
    profile: GlslProfile,
) -> Result<ShaderEmission, CompilerError> {
    let f = &ws.module(plan.fref.module).functions[plan.fref.func];
    let shape = &plan.shape;

    let resolved = stage_abi(&plan.interface, &plan.abi, IrStage::Fragment);

    let mut px = String::new();
    px.push_str("in vec2 TexCoord;\nout vec4 FragColor;\n\n");
    px.push_str(&resolved.declarations());
    px.push('\n');
    px.push_str(&plan.stage.render_helpers(ws, interner)?);
    px.push_str(&plan.stage.render_entry(ws, interner, shape)?);
    px.push('\n');
    px.push_str(&format!(
        "void main() {{\n  FragColor = {}(TexCoord);\n}}\n",
        symbol_of_key(f.name)
    ));

    let version = profile_header(profile);
    let source = format!(
        "{version}\n\n#ifdef PIXEL\n\n{px}\n#endif\n\n#ifdef VERTEX\n\n{FULLSCREEN_VERTEX}\n#endif\n"
    );
    Ok(ShaderEmission {
        source,
        dependencies: plan.stage.dependencies.clone(),
        interface: plan.interface.clone(),
        abi: plan.abi.clone(),
        ir: ShaderIR {
            interner: std::sync::Arc::clone(&plan.stage.concrete_interner),
            stages: vec![plan.stage.ir.clone()],
        },
    })
}
