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

/// Emit a COMPLETE single-file shader for one `@shader(fragment)` root: the
/// generated PIXEL block (declarations + reachable helpers + the entry fn +
/// `main`) and the default fullscreen VERTEX block.
pub fn emit_shader_entry(
    ws: &Workspace,
    mid: ModuleId,
    root: &str,
    profile: GlslProfile,
) -> Result<String, CompilerError> {
    Ok(emit_shader_entry_with_dependencies(ws, mid, root, profile)?.source)
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

/// **One stage's validated plan**: its lowered body, the emittable helpers it
/// reaches in emission order, and the prelude flags those bodies imply.
///
/// Built from the supplied graph/view population, which performs every
/// rejection the stage's own code can produce -- lowering, the tier-0 body
/// profile, recursion, a foreign body with no GLSL mapping, a helper outside
/// tier 0 and the stage-specific builtin validation. Rendering reads it and
/// cannot reach a different verdict, because there is nothing left to decide.
pub(crate) struct StagePlan {
    /// The stage's concrete bodies -- target-neutral, and the only part of this
    /// plan a non-GLSL backend would want.
    pub(crate) ir: ShaderStageIr,
    /// GLSL-only: whether the reachable bodies need the int div/mod prelude.
    /// GLSL has no integer `/` and `%` with Rock's semantics, which is a
    /// property of this target and not of the bodies.
    uses_idiv: bool,
    uses_imod: bool,
    dependencies: Vec<FuncRef>,
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
        let mut out = String::new();
        let prelude = helper_prelude(self.uses_idiv, self.uses_imod);
        if !prelude.is_empty() {
            out.push_str(&prelude);
            out.push('\n');
        }
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
                Some(body_plan),
            )?);
            out.push('\n');
        }
        Ok(out)
    }

    /// The stage's helpers plus the stage entry itself -- the hot-reload
    /// dependency set. DECLARATIONS: a reload watches source files, and two
    /// instances of one generic helper share one.
    pub(crate) fn dependencies(&self) -> Vec<FuncRef> {
        self.dependencies.clone()
    }
}

pub(crate) fn plan_stage_from_view(
    ws: &Workspace,
    scx: &ScalarCtx,
    view: &ShaderStageView,
    expected: Stage,
    concrete_plans: &Arc<ConcretePlans>,
) -> Result<StagePlan, CompilerError> {
    let expected_ir = match expected {
        Stage::Vertex => ShaderStage::Vertex,
        Stage::Fragment => ShaderStage::Fragment,
    };
    if view.stage != expected_ir {
        return Err(CompilerError::new_no_location(format!(
            "shader graph/view stage mismatch: expected {expected_ir:?}, got {:?}",
            view.stage
        )));
    }
    let input = skull_codegen_shader::ConcreteStageInput {
        stage: expected_ir,
        entry: view.entry.clone(),
        retained: view.retained.iter().cloned().collect(),
        helpers: view.helpers.iter().cloned().collect(),
        builtins: view.builtins.iter().cloned().collect(),
        order: view.order.clone(),
        dependencies: view.dependencies.clone(),
        concrete_plans: Arc::clone(concrete_plans),
    };
    let ir = skull_codegen_shader::plan_stage_from_concrete(ws, scx, &input, &GlslShaderPolicy)?;
    let (mut uses_idiv, mut uses_imod) = (false, false);
    for body in ir.bodies.values().chain(std::iter::once(&ir.body)) {
        for (_, inst) in body.insts.iter() {
            match inst {
                Inst::IntDiv(..) => uses_idiv = true,
                Inst::IntMod(..) => uses_imod = true,
                _ => {}
            }
        }
    }
    Ok(StagePlan {
        ir,
        uses_idiv,
        uses_imod,
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

/// **The one validated plan for a single-entry (A1) shader program.**
///
/// Everything emission decides before it writes GLSL, in emission's own order:
/// the stage classification of the entry's signature, its lowering and tier-0
/// body pass over the graph/view population, the stage-builtin validation, the
/// `@uniform` block's std140 layout and the program's `@textures` slot
/// assignment.
///
/// [`check_entry_profile`] builds this and DROPS it; [`emit_shader_entry`]
/// builds it and renders it. There is no second sequence, so the mono shader
/// artifact view structurally cannot admit a program emission refuses -- which
/// is not what a summary of these checks could promise, and did not deliver: it
/// accepted a standalone `@shader(vertex)` entry and never reached the
/// layout/slot planning at all.
struct EntryPlan {
    fref: FuncRef,
    shape: EntryShape,
    stage: StagePlan,
    /// What the program's interface MEANS -- roles, names, order, neutral
    /// kinds. Target-neutral, and the sole input to `abi`.
    interface: ShaderInterfacePlan,
    /// GLSL's physical answer to `interface`.
    abi: ShaderAbiPlan,
}

fn plan_shader_entry_from_view(
    ws: &Workspace,
    scx: &ScalarCtx,
    view: &ShaderStageView,
    concrete_plans: &Arc<ConcretePlans>,
) -> Result<EntryPlan, CompilerError> {
    plan_shader_entry_inner(ws, scx, view.entry.func, view, concrete_plans)
}

fn plan_shader_entry_inner(
    ws: &Workspace,
    scx: &ScalarCtx,
    fref: FuncRef,
    stage_view: &ShaderStageView,
    concrete_plans: &Arc<ConcretePlans>,
) -> Result<EntryPlan, CompilerError> {
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
    // The entry's signature is validated by its STAGE classification, never by
    // the fn-library tier-0 leaf check: an `@uniform` block param is legal here
    // and is not a tier-0 leaf (asking the library check of an entry rejected
    // 1,539 real shaders).
    let class = skull_codegen_shader::classify_entry(ws, f)?;
    let stage = plan_stage_from_view(ws, scx, stage_view, Stage::Fragment, concrete_plans)?;
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
    skull_codegen_shader::check_entry_collisions(&interface, f.name_span)?;
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

/// Validate one shader ENTRY exactly as emission does, without emitting.
///
/// Published for the mono SHADER ARTIFACT VIEW, which must not be able to admit
/// what emission refuses. It is the view-fed entry plan used by emission,
/// not a summary of it: the only thing left out below this point is the GLSL
/// text itself.
///
/// Declaration-based, like the rest of GLSL lowering today --
/// `lower_function_source`
/// takes no substitution -- so a caller holding concrete instances must not
/// hand this a generic declaration and read the answer as concrete.
pub fn check_entry_profile(ws: &Workspace, fref: FuncRef) -> Result<(), CompilerError> {
    let session = shader_program_session(
        ws,
        ShaderProgram {
            vert: None,
            frag: fref,
        },
    )?;
    check_entry_profile_with_view(ws, &session.scx, &session.view)
}

/// Validate one shader entry against the graph-owned concrete population.
pub fn check_entry_profile_with_view(
    ws: &Workspace,
    scx: &ScalarCtx,
    view: &ShaderProgramView,
) -> Result<(), CompilerError> {
    let stage = view
        .stages
        .iter()
        .find(|stage| stage.stage == ShaderStage::Fragment)
        .ok_or_else(|| {
            CompilerError::new_no_location(
                "shader graph/view has no fragment stage for a single-entry shader".to_string(),
            )
        })?;
    plan_shader_entry_from_view(ws, scx, stage, &view.concrete_plans).map(|_| ())
}

pub fn emit_shader_entry_with_dependencies(
    ws: &Workspace,
    mid: ModuleId,
    root: &str,
    profile: GlslProfile,
) -> Result<ShaderEmission, CompilerError> {
    let fref = resolve_root(ws, mid, root)?;
    let session = shader_program_session(
        ws,
        ShaderProgram {
            vert: None,
            frag: fref,
        },
    )?;
    emit_shader_entry_with_view(ws, &session.scx, &session.view, profile)
}

/// Emit a single-entry program from the graph-owned population. The caller's
/// `ScalarCtx` must share the graph interner; the view supplies the concrete
/// entry and helper order, while this backend supplies only MIR delivery and
/// GLSL rendering.
pub fn emit_shader_entry_with_view(
    ws: &Workspace,
    scx: &ScalarCtx,
    view: &ShaderProgramView,
    profile: GlslProfile,
) -> Result<ShaderEmission, CompilerError> {
    if view.program.vert.is_some() {
        return Err(CompilerError::new_no_location(
            "shader graph/view pipeline passed to single-entry GLSL emission".to_string(),
        ));
    }
    let stage = view
        .stages
        .iter()
        .find(|stage| stage.stage == ShaderStage::Fragment)
        .ok_or_else(|| {
            CompilerError::new_no_location(
                "shader graph/view has no fragment stage for a single-entry shader".to_string(),
            )
        })?;
    let plan = plan_shader_entry_from_view(ws, scx, stage, &view.concrete_plans)?;
    render_shader_entry(ws, scx.interner(), &plan, profile)
}

fn render_shader_entry(
    ws: &Workspace,
    interner: &MonoInterner,
    plan: &EntryPlan,
    profile: GlslProfile,
) -> Result<ShaderEmission, CompilerError> {
    let f = &ws.module(plan.fref.module).functions[plan.fref.func];
    let shape = &plan.shape;

    // A1 goes through the SAME resolver A2 does. Rendering every ABI block
    // directly would emit one the interface never claimed, which is exactly
    // what the program-wide validation exists to refuse.
    let program = ProgramAbi::of(
        &plan.interface,
        &plan.abi,
        &[IrStage::Fragment],
        f.name_span,
    )?;
    let resolved = program.stage(IrStage::Fragment)?;

    let mut px = String::new();
    px.push_str("in vec2 TexCoord;\nout vec4 FragColor;\n\n");
    if let Some(block) = resolved.engine_block {
        px.push_str(&uniform_block_decl(block));
    }
    if let Some(block) = resolved.uniform_block {
        px.push_str(&uniform_block_decl(block));
    }
    // Samplers are opaque: GLSL forbids them inside a uniform block, so they
    // stay loose beside it and bind by unit. Declared from the INTERFACE -- the
    // grouped ones in the ABI's unit order, then the bare ones.
    for (n, k) in &resolved.samplers {
        px.push_str(&format!(
            "uniform {} {};\n",
            sampler_glsl_ty(*k).uniform_decl(),
            n
        ));
    }
    px.push('\n');
    px.push_str(&plan.stage.render_helpers(ws, interner)?);
    let entry_plan = plan
        .stage
        .concrete_plans
        .get(&plan.stage.ir.entry.inst)
        .ok_or_else(|| err(f.name_span, "shader profile: entry has no concrete plan"))?;
    px.push_str(&emit_fn_def_with(
        ws,
        interner,
        &plan.stage.ir.entry,
        &plan.stage.ir.body,
        Some(shape),
        Some(entry_plan),
    )?);
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
        dependencies: plan.stage.dependencies(),
        interface: plan.interface.clone(),
        abi: plan.abi.clone(),
        ir: ShaderIR {
            interner: std::sync::Arc::clone(&plan.stage.concrete_interner),
            stages: vec![plan.stage.ir.clone()],
        },
    })
}

// --- A2: vertex+fragment pipeline ---------------------
