use super::*;

#[derive(Clone, Copy)]
pub(super) enum FullBuild<'a> {
    Program,
    OneShotProgram,
    PackageTest(&'a [FuncRef]),
    SelectedSims(&'a [String]),
}

impl FullBuild<'_> {
    fn lifetime(self) -> CacheLifetime {
        match self {
            Self::Program | Self::SelectedSims(_) => CacheLifetime::Retained,
            Self::OneShotProgram | Self::PackageTest(_) => CacheLifetime::OneShot,
        }
    }

    fn kind(self) -> BuildKind {
        match self {
            FullBuild::Program | FullBuild::OneShotProgram | FullBuild::SelectedSims(_) => {
                BuildKind::Program
            }
            FullBuild::PackageTest(_) => BuildKind::PackageTest,
        }
    }
}

#[derive(Default, Clone)]
pub(super) struct ShaderRegistrations {
    pub(super) lua: String,
    pub(super) sources: std::sync::Arc<[ShaderSource]>,
    pub(super) dependencies: FxHashSet<FuncRef>,
    pub(super) shader_contributions:
        HashMap<(ModuleId, BodyOwner), Vec<skull_mir::lower::ShaderPair>>,
    pub(super) roots: Vec<FuncRef>,
}

#[derive(Default)]
struct SetupPhases(Vec<(&'static str, Duration)>);

impl SetupPhases {
    fn timed<T>(&mut self, name: &'static str, f: impl FnOnce() -> T) -> T {
        let started = Instant::now();
        let result = f();
        self.0.push((name, started.elapsed()));
        result
    }
}

fn reject_workspace(ws: &Workspace, user_mid: ModuleId) -> Result<(), CompilerError> {
    if let Some(span) = skull_mir::lower::string_char_literal_out_of_range(ws, user_mid) {
        return Err(CompilerError::new_with_label(
            span,
            "string_char is portable only for ASCII 0-127; for code points use from_codepoint"
                .to_string(),
        ));
    }
    if let Some(err) = skull_mir::lower::expansion_rejection(ws, user_mid) {
        return Err(err);
    }
    if let Some(err) = skull_mir::lower::comptime_input_rejection(ws, user_mid) {
        return Err(err);
    }
    if let Some(err) = skull_mir::intrinsics::intrinsic_rejection(ws, user_mid) {
        return Err(err);
    }
    if let Some(err) = skull_mir::lower::gpu_only_call_rejection(ws, user_mid) {
        return Err(err);
    }
    if let Some((why, span)) = skull_mir::lower::map_key_rejection(ws, user_mid) {
        return Err(skull_mir::lower::map_key_error(&why, span));
    }
    if let Some((msg, span)) = skull_mir::lower::gc_value_c_boundary(ws, user_mid) {
        return Err(CompilerError::new_with_label(span, msg));
    }
    Ok(())
}

fn collect_full_roots(
    ws: &Workspace,
    build: FullBuild<'_>,
    project_root: Option<&str>,
) -> Result<
    (
        skull_mir::lower::ShaderCollection,
        Vec<crate::shader_artifact::ShaderProgram>,
        RootSet,
    ),
    CompilerError,
> {
    let (collections, roots) = parallel_join(
        || skull_mir::lower::collect_shader_collection(ws),
        || build_roots(ws, build, project_root),
    );
    let shader_collection = collections?;
    let shader_programs = shader_programs(&shader_collection);
    let shader_roots: Vec<FuncRef> = shader_programs
        .iter()
        .flat_map(|program| {
            program
                .vert
                .into_iter()
                .chain(std::iter::once(program.frag))
        })
        .collect();
    let (interface_roots, sim_roots) = roots?;
    let emission_roots = cached_roots(&interface_roots, &sim_roots, &shader_roots);
    Ok((
        shader_collection,
        shader_programs,
        RootSet {
            interface_roots,
            sim_roots,
            shader_roots,
            emission_roots,
        },
    ))
}

fn build_roots(
    ws: &Workspace,
    build: FullBuild<'_>,
    project_root: Option<&str>,
) -> Result<(Vec<FuncRef>, Vec<FuncRef>), CompilerError> {
    Ok(match build {
        FullBuild::Program | FullBuild::OneShotProgram => {
            (emission_roots(ws, project_root), sim_glue_roots(ws))
        }
        FullBuild::PackageTest(tests) => (explicit_and_global_roots(ws, tests), Vec::new()),
        FullBuild::SelectedSims(names) => {
            let sims = sim_refs_by_wire(ws);
            let mut roots = names
                .iter()
                .map(|name| {
                    sims.get(name.as_str()).copied().ok_or_else(|| {
                        CompilerError::new_no_location(format!("unknown sim root: {name}"))
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            roots.extend(ws.modules.iter().flat_map(|m| {
                m.functions.iter().filter_map(|(fid, f)| {
                    (skull_mir::mangle::declared_name(f.name.as_str()) == "main").then_some(
                        FuncRef {
                            module: m.id,
                            func: fid,
                        },
                    )
                })
            }));
            (explicit_and_global_roots(ws, &roots), sim_glue_roots(ws))
        }
    })
}

/// Widens the roots by every sim a reached body names, rebuilding the graph
/// until no new sim is reached.
fn expand_selected_sim_roots(
    ws: &Workspace,
    roots: &mut RootSet,
    graph: &mut crate::instance_graph::InstanceGraph,
) -> Result<FxHashSet<Ustr>, CompilerError> {
    let sims = sim_refs_by_wire(ws);
    let mut scanned = FxHashSet::default();
    loop {
        let reached = graph
            .instances
            .iter()
            .map(|i| i.func)
            .collect::<FxHashSet<_>>();
        let mut added = Vec::new();
        for module in &ws.modules {
            for (owner, body) in &module.bodies {
                if let BodyOwner::Function(fid) = owner {
                    let func = FuncRef {
                        module: module.id,
                        func: *fid,
                    };
                    if !reached.contains(&func) {
                        continue;
                    }
                }
                if !scanned.insert((module.id, *owner)) {
                    continue;
                }
                added.extend(named_sim_dependencies(ws, module.id, *owner, body, &sims)?);
            }
        }
        added.retain(|func| !roots.emission_roots.contains(func));
        if added.is_empty() {
            break;
        }
        roots.interface_roots.extend(added);
        roots
            .interface_roots
            .sort_by_key(|f| (f.module.index(), f.func.into_raw()));
        roots.interface_roots.dedup();
        roots.emission_roots = cached_roots(
            &roots.interface_roots,
            &roots.sim_roots,
            &roots.shader_roots,
        );
        let interner = std::sync::Arc::clone(graph.interner());
        *graph = crate::instance_graph::build_in(ws, roots.emission_roots.clone(), interner);
    }
    let reached = graph
        .instances
        .iter()
        .map(|i| i.func)
        .collect::<FxHashSet<_>>();
    Ok(sims
        .values()
        .filter(|func| reached.contains(func))
        .map(|func| ws.module(func.module).functions[func.func].name)
        .collect())
}

fn build_concrete_plans(
    ws: &Workspace,
    graph: &crate::instance_graph::InstanceGraph,
) -> Result<crate::concrete_body::ConcreteBodies, CompilerError> {
    let concrete = crate::concrete_body::build(ws, graph);
    if let Some((instance, reason)) = concrete.first_plan_failure() {
        let f = &ws.module(instance.func.module).functions[instance.func.func];
        return Err(CompilerError::new_with_label(
            f.name_span,
            format!(
                "cannot build concrete lowering plan for `{}`: {}",
                f.name,
                reason.describe()
            ),
        ));
    }
    if let Some((owner, reason)) = concrete.first_always_plan_failure() {
        let (span, what) = match owner {
            skull_mir::concrete::ConcreteOwner::Function(instance) => {
                let f = &ws.module(instance.func.module).functions[instance.func.func];
                (f.name_span, format!("`{}`", f.name))
            }
            skull_mir::concrete::ConcreteOwner::Global { module, index } => {
                let id = Idx::from_raw(comfy_la_arena::RawIdx::from_u32(*index));
                let g = &ws.module(*module).globals[id];
                (g.name_span, format!("global `{}`", g.name))
            }
            skull_mir::concrete::ConcreteOwner::Decorator { module, index } => {
                let id = Idx::from_raw(comfy_la_arena::RawIdx::from_u32(*index));
                let d = &ws.module(*module).decorators[id];
                (d.span, format!("decorator `{}`", d.name))
            }
        };
        return Err(CompilerError::new_with_label(
            span,
            format!(
                "cannot build concrete lowering plan for {what}: {}",
                reason.describe()
            ),
        ));
    }
    Ok(concrete)
}

type EarlyContexts = (
    skull_mir::lower::ScalarCtx,
    Result<CodegenCtx, CompilerError>,
);

fn early_contexts(
    ws: &Workspace,
    interner: std::sync::Arc<skull_mir::monoty::MonoInterner>,
    shapes: ShapeOverrides,
) -> EarlyContexts {
    let scx = skull_mir::lower::ScalarCtx::new(ws).sharing(interner);
    let cg = workspace_codegen_ctx(ws, &scx, shapes);
    (scx, cg)
}

fn cpu_artifact_lane(
    ws: &Workspace,
    graph: &crate::instance_graph::InstanceGraph,
    emission_roots: &[FuncRef],
    scx: skull_mir::lower::ScalarCtx,
    cg: Result<CodegenCtx, CompilerError>,
) -> (
    Result<
        (
            crate::artifact::ArtifactView,
            skull_mir::lower::ScalarCtx,
            CodegenCtx,
        ),
        CompilerError,
    >,
    Duration,
) {
    let started = Instant::now();
    let lua_view = crate::artifact::realize(
        ws,
        graph,
        &crate::artifact::ArtifactRequest {
            target: crate::target_projection::LUAJIT_DESKTOP.cpu,
            roots: emission_roots,
            scx: &scx,
            supply: None,
        },
    );
    let cg = cg.map(|mut cg| {
        cg.set_call_sccs(call_sccs(graph));
        cg
    });
    (cg.map(|cg| (lua_view, scx, cg)), started.elapsed())
}

fn shader_lane(
    ws: &Workspace,
    graph: &crate::instance_graph::InstanceGraph,
    shader_programs: &[crate::shader_artifact::ShaderProgram],
    shader_collection: &skull_mir::lower::ShaderCollection,
    shader_plans: std::sync::Arc<skull_mir::concrete::ConcretePlans>,
) -> (
    Result<
        (
            crate::shader_artifact::ShaderArtifactView,
            ShaderRegistrations,
        ),
        CompilerError,
    >,
    Duration,
) {
    let started = Instant::now();
    let shader_scx = skull_mir::lower::ScalarCtx::new_values_first_class(ws)
        .sharing(std::sync::Arc::clone(graph.interner()));
    let result = (|| {
        let shader_view =
            realize_shader_view(ws, graph, shader_programs, &shader_scx, shader_plans)?;
        let shader_registrations =
            shader_registration_lines(ws, &shader_scx, shader_collection, &shader_view)?;
        Ok::<_, CompilerError>((shader_view, shader_registrations))
    })();
    (result, started.elapsed())
}

struct SetupLanes {
    lua_view: crate::artifact::ArtifactView,
    scx: skull_mir::lower::ScalarCtx,
    cg: CodegenCtx,
    windows: [(&'static str, Duration); 2],
}

fn run_setup_lanes(
    ws: &Workspace,
    graph: &crate::instance_graph::InstanceGraph,
    build_plans: impl FnOnce() -> Result<crate::concrete_body::ConcreteBodies, CompilerError> + Send,
    emission_roots: &[FuncRef],
    early: EarlyContexts,
) -> Result<(crate::concrete_body::ConcreteBodies, SetupLanes), CompilerError> {
    let (scx, cg) = early;
    assert!(
        std::sync::Arc::ptr_eq(&scx.interner_handle(), graph.interner()),
        "the scalar context mints against the graph's arena"
    );
    // Stays on this thread: its build counter is a thread-local the
    // incremental repair statistics read back.
    let ((cpu_result, cpu_elapsed), (plans_result, plans_elapsed)) = join_in_place(
        || cpu_artifact_lane(ws, graph, emission_roots, scx, cg),
        || {
            let started = Instant::now();
            (build_plans(), started.elapsed())
        },
    );
    let concrete = plans_result?;
    let (lua_view, scx, cg) = cpu_result?;
    Ok((
        concrete,
        SetupLanes {
            lua_view,
            scx,
            cg,
            windows: [
                ("join: concrete plans branch", plans_elapsed),
                ("join: cpu artifact branch", cpu_elapsed),
            ],
        },
    ))
}

fn workspace_codegen_ctx(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    shapes: ShapeOverrides,
) -> Result<CodegenCtx, CompilerError> {
    let state_slots = lua_global_state_slots(ws, scx)?.into_iter().collect();
    let mut cg = CodegenCtx::new_with_state(ws, state_slots);
    if let Some(on) = shapes.frame_limits {
        cg.set_frame_limits(on);
    }
    if let Some(on) = shapes.line_directives {
        cg.set_line_directives(on);
    }
    Ok(cg)
}

/// Checked per signature over every function, not just the emitted set: a
/// duplicate Lua parameter is illegal whichever body is reached.
fn reject_abi_param_collisions(ws: &Workspace, scx: &skull_mir::lower::ScalarCtx, cg: &CodegenCtx) {
    for m in ws.source_modules() {
        for (fid, _) in m.functions.iter() {
            let fref = FuncRef {
                module: m.id,
                func: fid,
            };
            for (key, err) in skull_mir::lower::scalar_abi_param_collisions(ws, scx, fref) {
                cg.reject(key, err);
            }
        }
    }
}

type ChunkMap = FxHashMap<Instance, Result<Chunk, CompilerError>>;

type FanOutBodies = Vec<Option<(Instance, Result<Chunk, CompilerError>)>>;

enum FanOutJob {
    Body(usize, FuncRef),
    Spec(Instance),
}

fn emit_full_chunks<'a>(
    ws: &'a Workspace,
    cg: &CodegenCtx,
    scx: &'a skull_mir::lower::ScalarCtx,
    mut queue: SpecQueue<'a>,
    concrete: &crate::concrete_body::ConcreteBodies,
    lua_view: &crate::artifact::ArtifactView,
    attribution: bool,
    setup_phases: &mut SetupPhases,
) -> Result<
    (
        FanOutBodies,
        std::sync::Arc<skull_transform::InlineCandidateTable>,
        SpecQueue<'a>,
    ),
    CompilerError,
> {
    let mut jobs: Vec<FanOutJob> = ws
        .source_modules()
        .flat_map(|m| {
            m.functions.iter().filter_map(move |(fid, f)| {
                let self_ref = FuncRef {
                    module: m.id,
                    func: fid,
                };
                (is_cached_body(f) && should_emit_via_view(ws, lua_view, self_ref))
                    .then_some(self_ref)
            })
        })
        .enumerate()
        .map(|(at, self_ref)| FanOutJob::Body(at, self_ref))
        .collect();
    // Libraries come before their users; starting the users first starts their
    // specialization chains early, so no chain runs alone at the end.
    jobs.reverse();
    let inline_candidate_table = setup_phases.timed("setup: inline candidate table", || {
        build_inline_candidate_table(ws, scx, concrete)
    })?;
    queue.attribution = attribution;
    // One slot per body keeps the chunk map in declaration order without a sort.
    let bodies: std::sync::Mutex<FanOutBodies> =
        std::sync::Mutex::new(std::iter::repeat_with(|| None).take(jobs.len()).collect());
    setup_phases.timed("window: fan-out", || {
        par_drain(
            jobs,
            || worker_inline_cache(Some(&inline_candidate_table)),
            |inline_cache, job, follow_ups| {
                let mut demanded = Vec::new();
                match job {
                    FanOutJob::Body(at, self_ref) => {
                        let instance = shared(self_ref);
                        let mut chunk =
                            emit_body_chunk(ws, cg, scx, concrete, &instance, None, inline_cache);
                        if let Ok(chunk) = &mut chunk {
                            chunk.finish(attribution);
                        }
                        if let Ok(chunk) = &chunk {
                            queue.admit_all(&chunk.specs, &mut demanded);
                        }
                        bodies.lock().expect("fan-out chunks")[at] = Some((instance, chunk));
                    }
                    FanOutJob::Spec(inst) => {
                        queue.build(cg, concrete, inline_cache, inst, &mut demanded)
                    }
                }
                follow_ups.extend(demanded.into_iter().map(FanOutJob::Spec));
            },
        )
    });
    let bodies = bodies.into_inner().expect("fan-out chunks");
    Ok((bodies, inline_candidate_table, queue))
}

fn index_dependents(chunks: &ChunkMap) -> (FxHashMap<FuncRef, FxHashSet<Instance>>, usize) {
    let mut dependents = FxHashMap::default();
    let mut dependency_edges = 0;
    for (body, chunk) in chunks {
        if let Ok(chunk) = chunk {
            dependency_edges += chunk.index_dependents(body, &mut dependents);
        }
    }
    (dependents, dependency_edges)
}

fn check_unpublished_noalloc(
    ws: &Workspace,
    graph: &crate::instance_graph::InstanceGraph,
    concrete: &crate::concrete_body::ConcreteBodies,
    scx: &skull_mir::lower::ScalarCtx,
    cg: &CodegenCtx,
    lua_view: &crate::artifact::ArtifactView,
) -> Result<(), CompilerError> {
    let mut roots = Vec::new();
    let mut rejections = Rejections::default();
    for module in ws.source_modules() {
        for (func, f) in module.functions.iter() {
            if f.backend == bone_semantic::hir::FnBackend::Js || !noalloc::marked(f) {
                continue;
            }
            if f.is_extern || f.inline_body.is_some() {
                continue;
            }
            let fref = FuncRef {
                module: module.id,
                func,
            };
            if is_generic(f) {
                for instance in graph
                    .instances
                    .range(Instance::base(fref)..)
                    .take_while(|instance| instance.func == fref)
                {
                    if concrete.plan_for(instance).is_some()
                        && let Err(error) = check_noalloc_instance(ws, cg, scx, concrete, instance)
                    {
                        rejections.push(error);
                    }
                }
            } else if !should_emit_via_view(ws, lua_view, fref) {
                let instance = Instance::base(fref);
                if concrete.plan_for(&instance).is_none() {
                    roots.push(fref);
                } else if let Err(error) = check_noalloc_instance(ws, cg, scx, concrete, &instance)
                {
                    rejections.push(error);
                }
            }
        }
    }
    if !roots.is_empty() {
        // Check declarations pruned by this artifact without making them publication roots.
        let graph = crate::instance_graph::build_functions_in(
            ws,
            roots,
            std::sync::Arc::clone(graph.interner()),
        );
        if let Some(error) = graph_rejection_error(ws, &graph) {
            return Err(error);
        }
        let concrete = build_concrete_plans(ws, &graph)?;
        for instance in &graph.instances {
            let f = &ws.module(instance.func.module).functions[instance.func.func];
            if f.backend != bone_semantic::hir::FnBackend::Js
                && !f.is_extern
                && f.inline_body.is_none()
                && noalloc::marked(f)
                && (is_generic(f) || !should_emit_via_view(ws, lua_view, instance.func))
                && let Err(error) = check_noalloc_instance(ws, cg, scx, &concrete, instance)
            {
                rejections.push(error);
            }
        }
    }
    rejections.into_result()
}

fn check_noalloc_instance(
    ws: &Workspace,
    cg: &CodegenCtx,
    scx: &skull_mir::lower::ScalarCtx,
    concrete: &crate::concrete_body::ConcreteBodies,
    instance: &Instance,
) -> Result<(), CompilerError> {
    let name = (!instance.args.is_empty()).then(|| instance.symbol(ws, scx.interner()));
    emit_body_chunk(
        ws,
        cg,
        scx,
        concrete,
        instance,
        name.as_deref(),
        &mut worker_inline_cache(None),
    )
    .map(|_| ())
}

/// Check selected source bodies, including ones pruned from a runnable artifact.
pub fn inspect_allocations(
    ws: &Workspace,
    files: &[&str],
    hints: bool,
    published: Option<&crate::allocation_report::AllocationReport>,
) -> (
    Vec<crate::allocation_report::AllocationBody>,
    Vec<CompilerError>,
) {
    use bone_semantic::hir::FnBackend;
    use skull_mir::lower::backend_variant_instance;

    let mut roots = Vec::new();
    let mut errors = Vec::new();
    let mut bodies = Vec::new();
    let mut selected = FxHashSet::default();
    let mut has_generic = false;
    for &file in files {
        let Some(&mid) = ws.module_ids.get(&Ustr::from(file)) else {
            continue;
        };
        let m = ws.module(mid);
        for (func, f) in m.functions.iter() {
            if !(hints || noalloc::marked(f))
                || !matches!(f.backend, FnBackend::Any | FnBackend::Lua)
                || f.annotations.iter().any(|a| a.is_builtin("gpu_only"))
            {
                continue;
            }
            if !is_generic(f)
                && published.is_some_and(|report| report.bodies.contains_key(&mangle_fn(ws, f)))
            {
                continue;
            }
            let fref = FuncRef { module: m.id, func };
            if f.is_extern {
                if let Some(lib) = f.ffi_lib {
                    match ffi_extern_wrapper(ws, fref, f, lib, hints.then_some(false)) {
                        Ok((_, body)) => bodies.extend(body),
                        Err(error) if error.message.starts_with("`@noalloc`") => errors.push(error),
                        Err(_) => {}
                    }
                }
                continue;
            }
            if f.inline_body.is_some() {
                if noalloc::marked(f) {
                    errors.push(noalloc::reject_inline(f));
                }
                continue;
            }
            selected.insert(fref);
            if is_generic(f) {
                has_generic = true;
            } else {
                roots.push(fref);
            }
        }
    }
    if selected.is_empty() {
        return (bodies, errors);
    }

    if has_generic {
        // Concrete arguments can come from callers in other files.
        roots.extend(emission_roots(ws, None));
    }
    let interner = std::sync::Arc::new(skull_mir::monoty::MonoInterner::new());
    let graph = crate::instance_graph::build_functions_in(ws, roots, interner.clone());
    let concrete = crate::concrete_body::build(ws, &graph);
    let (scx, cg) = early_contexts(ws, interner, ShapeOverrides::default());
    let Ok(mut cg) = cg else {
        return (bodies, errors);
    };
    cg.set_call_sccs(call_sccs(&graph));
    let mut jobs = BTreeSet::new();
    for instance in &graph.instances {
        if let Ok(instance) = backend_variant_instance(ws, instance, FnBackend::Lua)
            && selected.contains(&instance.func)
        {
            jobs.insert(instance);
        }
    }
    jobs.extend(roots_for_inspection(ws, &selected));
    let _report = crate::allocation_report::scope(hints);
    let _activation = crate::allocation_report::activation_scope(false);
    let results = par_map_with(
        jobs.into_iter().collect(),
        || worker_inline_cache(None),
        |cache, instance| {
            let f = &ws.module(instance.func.module).functions[instance.func.func];
            if !is_cached_emitted_body(f, &instance) {
                return (Vec::new(), None);
            }
            let symbol = instance.symbol(ws, scx.interner());
            if published.is_some_and(|report| report.bodies.contains_key(&symbol)) {
                return (Vec::new(), None);
            }
            let spec = (!instance.args.is_empty()).then_some(symbol.as_str());
            match emit_body_chunk(ws, &cg, &scx, &concrete, &instance, spec, cache) {
                Ok(chunk) => (chunk.allocation_bodies, None),
                Err(error) if noalloc::marked(f) && error.message.starts_with("`@noalloc`") => {
                    (Vec::new(), Some(error))
                }
                Err(_) => (Vec::new(), None),
            }
        },
    );
    for (chunk, error) in results {
        bodies.extend(chunk);
        errors.extend(error);
    }
    (bodies, errors)
}

fn roots_for_inspection(
    ws: &Workspace,
    selected: &FxHashSet<FuncRef>,
) -> impl Iterator<Item = Instance> {
    selected.iter().copied().filter_map(|fref| {
        let f = &ws.module(fref.module).functions[fref.func];
        is_cached_body(f).then(|| Instance::base(fref))
    })
}

pub(super) fn emit_full_build(
    ws: &Workspace,
    user_mid: ModuleId,
    options: FullEmit<'_>,
) -> Result<(MirEmit, SkullEmitCache), CompilerError> {
    let FullEmit {
        build,
        project_root,
        renderer_target,
        shapes,
        validate_artifact,
        global_accessors,
        attribution,
    } = options;
    let mut setup_phases = SetupPhases::default();
    let project_root = setup_phases.timed("setup: validation+context", || {
        let project_root = canonical_project_root(ws, project_root)?;
        reject_workspace(ws, user_mid)?;
        if let Some(err) = SkullEmitCache::combine_errors(skull_mir::lua_interop::errors(ws)) {
            return Err(err);
        }
        Ok::<_, CompilerError>(project_root)
    })?;
    let project_root = project_root.as_deref();

    // `scx` shares the graph's arena so an `Instance` the graph mints means
    // the same thing to lowering.
    let (shader_collection, shader_programs, mut roots) = setup_phases
        .timed("setup: root collection", || {
            collect_full_roots(ws, build, project_root)
        })?;
    // Stays on this thread: its build counter is a thread-local the
    // incremental repair statistics read back.
    let (early, graph_result) = setup_phases.timed("setup: graph", || {
        let interner = std::sync::Arc::new(skull_mir::monoty::MonoInterner::new());
        let early_interner = std::sync::Arc::clone(&interner);
        join_in_place(
            || early_contexts(ws, early_interner, shapes),
            || {
                let mut graph =
                    crate::instance_graph::build_in(ws, roots.emission_roots.clone(), interner);
                let selected_sim_functions = if matches!(build, FullBuild::SelectedSims(_)) {
                    Some(expand_selected_sim_roots(ws, &mut roots, &mut graph)?)
                } else {
                    None
                };
                if let Some(err) = graph_rejection_error(ws, &graph) {
                    return Err(err);
                }
                Ok::<_, CompilerError>((graph, selected_sim_functions))
            },
        )
    });
    let (mut graph, selected_sim_functions) = graph_result?;
    let (concrete, lanes) = setup_phases.timed("setup: concrete plans+lanes", || {
        run_setup_lanes(
            ws,
            &graph,
            || build_concrete_plans(ws, &graph),
            &roots.emission_roots,
            early,
        )
    })?;
    let SetupLanes {
        lua_view,
        scx,
        mut cg,
        windows,
    } = lanes;
    cg.set_allocation_sources(
        crate::allocation_report::enabled() && !crate::allocation_report::activation_enabled(),
    );
    setup_phases.0.extend(windows);
    setup_phases.timed("setup: ABI validation", || {
        reject_abi_param_collisions(ws, &scx, &cg)
    });

    let noalloc_result = if noalloc::required(ws) {
        check_unpublished_noalloc(ws, &graph, &concrete, &scx, &cg, &lua_view)
    } else {
        Ok(())
    };

    let graph_records = (build.lifetime() == CacheLifetime::OneShot).then(|| graph.take_records());
    let spec_queue = SpecQueue::new(ws, &scx, graph_records.as_ref().unwrap_or(&graph), None);
    // Run the graph's last reader first when there is no worker to steal it.
    let ((shader_result, shader_window), (chunks, (early, early_window))) = parallel_join(
        || {
            let lane = shader_lane(
                ws,
                graph_records.as_ref().unwrap_or(&graph),
                &shader_programs,
                &shader_collection,
                std::sync::Arc::clone(&concrete.plans),
            );
            drop(graph_records);
            lane
        },
        || {
            parallel_join(
                || {
                    emit_full_chunks(
                        ws,
                        &cg,
                        &scx,
                        spec_queue,
                        &concrete,
                        &lua_view,
                        attribution,
                        &mut setup_phases,
                    )
                },
                || {
                    let started = Instant::now();
                    let early = prepare_early_assembly(
                        ws,
                        &scx,
                        &cg,
                        &concrete,
                        attribution,
                        global_accessors,
                    );
                    (early, started.elapsed())
                },
            )
        },
    );
    let (shader_view, shader_registrations) = shader_result?;
    let (bodies, inline_candidate_table, spec_queue) = chunks?;
    let (chunks, specs) = parallel_join(
        || {
            let filled = |slot: Option<_>| slot.expect("every fan-out body is emitted");
            bodies.into_iter().map(filled).collect::<ChunkMap>()
        },
        || {
            let mut spec_jobs = Vec::new();
            spec_queue.admit_all(early.demands(), &mut spec_jobs);
            drain_spec_jobs(
                &spec_queue,
                &cg,
                &concrete,
                Some(&inline_candidate_table),
                spec_jobs,
            );
            spec_queue.finish()
        },
    );
    setup_phases
        .0
        .push(("window: early assembly", early_window));
    setup_phases.0.push(("window: shader view", shader_window));
    let (dependents, dependency_edges) = if build.lifetime() == CacheLifetime::Retained {
        index_dependents(&chunks)
    } else {
        Default::default()
    };
    let mut cache = SkullEmitCache {
        chunks,
        value_ctors: Vec::new(),
        attribution,
        concrete,
        graph,
        lua_view,
        cg,
        scx,
        inline_cache: skull_transform::InlineCache::with_candidate_table(inline_candidate_table),
        dependents,
        roots,
        shader_registrations,
        shader_view,
        renderer_target,
        validate_artifact,
        global_accessors,
        build_kind: build.kind(),
        selected_sim_functions,
        edit: BodyEditState::default(),
        body_edit_rollback: None,
    };
    let mut built_specs: Vec<(Instance, Chunk)> = Vec::new();
    let assembly_started = Instant::now();
    let assembly = assemble(
        ws,
        &mut cache,
        &mut built_specs,
        build.lifetime(),
        Some(early),
        Some(specs),
    );
    if let Err(mut error) = noalloc_result {
        if let Err(other) = assembly {
            error = noalloc::merge_diagnostics(error, other);
        }
        return Err(error);
    }
    let (mut emit, emitted, unit_reads) = assembly?;
    let assembly_window = assembly_started.elapsed();
    let mut phases = setup_phases.0;
    phases.extend(emit.phases);
    phases.push(("window: assembly", assembly_window));
    emit.phases = phases;
    if build.lifetime() == CacheLifetime::Retained {
        let spec_edges = cache.retain_assembly(ws, &emit, emitted, unit_reads, built_specs);
        cache.retain_static_cells(ws)?;
        emit.compile_counts.chunks = cache.chunks.len();
        emit.compile_counts.dependency_edges = Some(dependency_edges + spec_edges);
    }
    Ok((emit, cache))
}
