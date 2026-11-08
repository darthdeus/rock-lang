//! The full build: setup lanes, the per-body fanout and assembly.

use super::*;

#[cfg(all(test, not(feature = "sequential")))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SetupLaneSchedule {
    CpuFirst,
    AbiFirst,
    Handshake,
}

#[cfg(all(test, not(feature = "sequential")))]
#[derive(Clone, Copy, Debug, Default)]
struct SetupHandshakeState {
    entered: usize,
    released: usize,
    released_after_both: bool,
}

#[cfg(all(test, not(feature = "sequential")))]
struct SetupLaneScheduler {
    schedule: SetupLaneSchedule,
    finished: std::sync::Mutex<bool>,
    handshake: std::sync::Mutex<SetupHandshakeState>,
    wake: std::sync::Condvar,
}

#[cfg(all(test, not(feature = "sequential")))]
impl SetupLaneScheduler {
    fn new(schedule: SetupLaneSchedule) -> Self {
        Self {
            schedule,
            finished: std::sync::Mutex::new(false),
            handshake: std::sync::Mutex::new(SetupHandshakeState::default()),
            wake: std::sync::Condvar::new(),
        }
    }

    fn wait(&self, lane: SetupLaneKind) {
        if self.schedule == SetupLaneSchedule::Handshake {
            if !matches!(lane, SetupLaneKind::CpuArtifact | SetupLaneKind::Shader) {
                return;
            }
            let mut handshake = self.handshake.lock().expect("setup handshake lock");
            handshake.entered += 1;
            if handshake.entered == 2 {
                handshake.released_after_both = true;
                self.wake.notify_all();
            }
            while handshake.entered < 2 {
                handshake = self.wake.wait(handshake).expect("setup handshake wait");
            }
            handshake.released += 1;
            return;
        }
        let first = match self.schedule {
            SetupLaneSchedule::CpuFirst => SetupLaneKind::CpuArtifact,
            SetupLaneSchedule::AbiFirst => SetupLaneKind::AbiWarm,
            SetupLaneSchedule::Handshake => unreachable!(),
        };
        if lane != first {
            let mut finished = self.finished.lock().expect("setup scheduler lock");
            while !*finished {
                finished = self.wake.wait(finished).expect("setup scheduler wait");
            }
        }
    }

    fn finish(&self, lane: SetupLaneKind) {
        let first = match self.schedule {
            SetupLaneSchedule::CpuFirst => SetupLaneKind::CpuArtifact,
            SetupLaneSchedule::AbiFirst => SetupLaneKind::AbiWarm,
            SetupLaneSchedule::Handshake => return,
        };
        if lane != first {
            return;
        }
        *self.finished.lock().expect("setup scheduler lock") = true;
        self.wake.notify_all();
    }

    fn handshake(&self) -> (usize, usize, bool) {
        let handshake = *self.handshake.lock().expect("setup handshake lock");
        (
            handshake.entered,
            handshake.released,
            handshake.released_after_both,
        )
    }
}

/// A full build's kind with the roots it is taken over.
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
    pub(super) callsites: FxHashSet<(ModuleId, BodyOwner)>,
    pub(super) shader_contributions:
        HashMap<(ModuleId, BodyOwner), Vec<skull_mir::lower::ShaderPair>>,
    pub(super) shader_pair_users: BTreeMap<String, usize>,
    pub(super) shader_pairs: BTreeMap<String, skull_mir::lower::ShaderPair>,
    pub(super) shader_root_set: FxHashSet<FuncRef>,
    pub(super) roots: Vec<FuncRef>,
    pub(super) keys: BTreeSet<String>,
}

#[cfg(test)]
pub(super) fn emit_full_in_with_test_mutation(
    ws: &Workspace,
    user_mid: ModuleId,
    mutation: BodyMutation,
) -> Result<(MirEmit, SkullEmitCache), CompilerError> {
    emit_full_in_with_mutation(
        ws,
        user_mid,
        None,
        mutation,
        RendererTarget::Gl,
        FullBuild::Program,
        ShapeOverrides::default(),
    )
}

pub(super) fn emit_full_in_with_mutation(
    ws: &Workspace,
    user_mid: ModuleId,
    project_root: Option<&str>,
    mutation: BodyMutation,
    renderer_target: RendererTarget,
    build: FullBuild<'_>,
    shapes: ShapeOverrides,
) -> Result<(MirEmit, SkullEmitCache), CompilerError> {
    let mut setup_phases = Vec::new();
    let t_setup = Instant::now();
    let project_root = canonical_project_root(ws, project_root)?;
    let project_root = project_root.as_deref();
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
        return Err(CompilerError::new_with_label(
            span,
            format!(
                "this `Map` key is not value-keyed and would silently miss: {why}. A runtime-built \
                 key does not find the entry stored under an equal-by-value one, so an insert at \
                 an equal key grows the map instead of replacing. Use an int, string, bool or \
                 float key -- or a newtype over one of those -- or derive a string key from the \
                 value's fields."
            ),
        ));
    }
    if let Some((msg, span)) = skull_mir::lower::gc_value_c_boundary(ws, user_mid) {
        return Err(CompilerError::new_with_label(span, msg));
    }
    setup_phases.push(("setup: validation+context", t_setup.elapsed()));

    let t_setup = Instant::now();
    // **The graph is built FIRST, into a fresh arena, and `scx` shares that
    // arena rather than minting its own.** An `Instance` the graph mints has to
    // mean something to lowering, and a second arena would be a second identity
    // for the same instance. See `SkullEmitCache::graph`.
    // Generated glue is part of the ROOT INPUT. Shader declarations are known
    // before the graph; their GLSL bodies are rendered only after the view owns
    // the retained instances.
    let (collections, roots) = parallel_join(
        || skull_mir::lower::collect_shader_collection(ws),
        || {
            Ok::<_, CompilerError>(match build {
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
        },
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
    let (mut interface_roots, sim_roots) = roots?;
    let mut emission_roots = cached_roots(&interface_roots, &sim_roots, &shader_roots);
    #[cfg(test)]
    if matches!(build, FullBuild::Program | FullBuild::OneShotProgram) {
        assert_eq!(
            emission_roots,
            all_roots(ws, project_root, &shader_roots),
            "cached full-build roots diverged from the wholesale root oracle"
        );
    }
    setup_phases.push(("setup: root collection", t_setup.elapsed()));

    let t_setup = Instant::now();
    let mut graph = crate::instance_graph::build(ws, emission_roots.clone(), false);
    let selected_sim_functions = if matches!(build, FullBuild::SelectedSims(_)) {
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
            added.retain(|func| !emission_roots.contains(func));
            if added.is_empty() {
                break;
            }
            interface_roots.extend(added);
            interface_roots.sort_by_key(|f| (f.module.index(), f.func.into_raw()));
            interface_roots.dedup();
            emission_roots = cached_roots(&interface_roots, &sim_roots, &shader_roots);
            graph = crate::instance_graph::build(ws, emission_roots.clone(), false);
        }
        let reached = graph
            .instances
            .iter()
            .map(|i| i.func)
            .collect::<FxHashSet<_>>();
        Some(
            sims.values()
                .filter(|func| reached.contains(func))
                .map(|func| ws.module(func.module).functions[func.func].name)
                .collect(),
        )
    } else {
        None
    };
    if let Some(err) = graph_rejection_error(ws, &graph) {
        return Err(err);
    }
    setup_phases.push(("setup: graph", t_setup.elapsed()));

    let t_setup = Instant::now();
    #[allow(unused_mut)]
    let mut concrete = {
        #[cfg(test)]
        if let BodyMutation::ConcreteResultOrder(order) = mutation {
            crate::concrete_body::build_with_result_order(ws, &graph, order).0
        } else {
            crate::concrete_body::build(ws, &graph)
        }
        #[cfg(not(test))]
        {
            crate::concrete_body::build(ws, &graph)
        }
    };
    #[cfg(test)]
    if let BodyMutation::ClearBoundPlan { body } = mutation {
        let mut matched = 0;
        for (instance, plan) in std::sync::Arc::get_mut(&mut concrete.plans)
            .expect("concrete plans are unshared before the setup join")
            .iter_mut()
        {
            if instance.func != body || instance.args.is_empty() {
                continue;
            }
            assert!(
                !plan.bound_tree.is_empty(),
                "bound-plan mutation found a specialized body with no witness tree"
            );
            let plan = std::sync::Arc::make_mut(plan);
            plan.bound_tree = Default::default();
            matched += 1;
        }
        assert!(
            matched > 0,
            "bound-plan mutation found no specialized instance for {body:?}; plans={:?}",
            concrete
                .plans
                .keys()
                .map(|instance| (instance.func, instance.args.len()))
                .collect::<Vec<_>>()
        );
    }
    #[cfg(test)]
    if let BodyMutation::ClearCoroutineResultPlan { body } = mutation {
        let mut matched = 0;
        for (instance, plan) in std::sync::Arc::get_mut(&mut concrete.plans)
            .expect("concrete plans are unshared before the setup join")
            .iter_mut()
        {
            if instance.func == body {
                assert!(
                    plan.signature.result.is_some(),
                    "coroutine-plan mutation found a body with no concrete result witness"
                );
                let plan = std::sync::Arc::make_mut(plan);
                std::sync::Arc::make_mut(&mut plan.signature).result = None;
                matched += 1;
            }
        }
        assert!(
            matched > 0,
            "coroutine-plan mutation found no concrete instance for {body:?}"
        );
    }
    #[cfg(test)]
    if let BodyMutation::ClearIteratorWitnessInstance { body } = mutation {
        let mut matched = 0;
        for (instance, plan) in std::sync::Arc::get_mut(&mut concrete.plans)
            .expect("concrete plans are unshared before the setup join")
            .iter_mut()
        {
            if instance.func != body {
                continue;
            }
            let witness = std::sync::Arc::make_mut(plan)
                .bound_tree
                .next
                .values_mut()
                .find_map(|next| match next {
                    skull_mir::bound::ResolvedBoundNext::Concrete { instance, .. }
                        if !instance.args.is_empty() =>
                    {
                        Some(instance)
                    }
                    _ => None,
                })
                .expect("iterator-plan mutation found no concrete witness instance");
            witness.args.clear();
            matched += 1;
        }
        assert!(
            matched > 0,
            "iterator-plan mutation found no concrete instance for {body:?}"
        );
    }
    if let Some((instance, reason)) = concrete.first_plan_failure() {
        let f = &ws.module(instance.func.module).functions[instance.func.func];
        return Err(CompilerError::new_with_label(
            f.name_span,
            format!(
                "cannot build concrete lowering plan for `{}`: {:?}",
                f.name, reason
            ),
        ));
    }
    if let Some((owner, reason)) = concrete.first_always_plan_failure() {
        return Err(CompilerError::new_no_location(format!(
            "cannot build concrete always-emitted lowering plan for {owner:?}: {reason:?}"
        )));
    }
    setup_phases.push(("setup: concrete plans", t_setup.elapsed()));

    #[cfg(test)]
    let skip_abi_plan_publish = matches!(mutation, BodyMutation::SkipAbiPlanPublish);
    #[cfg(not(test))]
    let skip_abi_plan_publish = false;

    #[cfg(all(test, not(feature = "sequential")))]
    let setup_scheduler = match mutation {
        BodyMutation::SetupLaneSchedule(schedule) => {
            Some(std::sync::Arc::new(SetupLaneScheduler::new(schedule)))
        }
        _ => None,
    };

    let t_setup = Instant::now();
    // The graph is complete before these lanes start; shared interning remains
    // synchronized. ABI plans carry their own representation arena, so the
    // warmed context is adopted after join.
    let shader_plans = std::sync::Arc::clone(&concrete.plans);
    let abi_plans = std::sync::Arc::clone(&concrete.plans);
    let abi_always_plans = &concrete.always_plans;
    let mono_before_lanes = graph.interner().stats();
    let run_setup_lanes = || {
        parallel_join(
            || {
                parallel_join(
                    || {
                        #[cfg(all(test, not(feature = "sequential")))]
                        if let Some(scheduler) = setup_scheduler.as_deref() {
                            scheduler.wait(SetupLaneKind::CpuArtifact);
                        }
                        let started = Instant::now();
                        let scx = skull_mir::lower::ScalarCtx::new(ws)
                            .sharing(std::sync::Arc::clone(graph.interner()));
                        let lua_view = crate::artifact::realize(
                            ws,
                            &graph,
                            &crate::artifact::ArtifactRequest::new(
                                crate::target_projection::LUAJIT_DESKTOP,
                                &emission_roots,
                            )
                            .with_scalar_context(&scx),
                        );
                        let population = lua_view.instances.len() + lua_view.boundaries.len();
                        let finished = Instant::now();
                        #[cfg(all(test, not(feature = "sequential")))]
                        if let Some(scheduler) = setup_scheduler.as_deref() {
                            scheduler.finish(SetupLaneKind::CpuArtifact);
                        }
                        (
                            lua_view,
                            scx,
                            SetupLaneWindow {
                                started,
                                finished,
                                population,
                            },
                        )
                    },
                    || {
                        #[cfg(all(test, not(feature = "sequential")))]
                        if let Some(scheduler) = setup_scheduler.as_deref() {
                            scheduler.wait(SetupLaneKind::Shader);
                        }
                        let started = Instant::now();
                        // The overload set lives in `cg` (computed once there);
                        // borrow it rather than rescanning the workspace.
                        let shader_scx = skull_mir::lower::ScalarCtx::new_values_first_class(ws)
                            .sharing(std::sync::Arc::clone(graph.interner()));
                        let result = (|| {
                            let shader_view = realize_shader_view(
                                ws,
                                &graph,
                                &shader_programs,
                                &shader_scx,
                                shader_plans,
                            )?;
                            let population = shader_view.roots.len()
                                + shader_view.helpers.len()
                                + shader_view.builtins.len();
                            let shader_registrations = shader_registration_lines(
                                ws,
                                &shader_scx,
                                &shader_collection,
                                &shader_view,
                            )?;
                            Ok::<_, CompilerError>((shader_view, shader_registrations, population))
                        })();
                        let population = result
                            .as_ref()
                            .map(|(_, _, population)| *population)
                            .unwrap_or(0);
                        let finished = Instant::now();
                        #[cfg(all(test, not(feature = "sequential")))]
                        if let Some(scheduler) = setup_scheduler.as_deref() {
                            scheduler.finish(SetupLaneKind::Shader);
                        }
                        (
                            result.map(|(view, registrations, _)| (view, registrations)),
                            SetupLaneWindow {
                                started,
                                finished,
                                population,
                            },
                        )
                    },
                )
            },
            || {
                #[cfg(all(test, not(feature = "sequential")))]
                if let Some(scheduler) = setup_scheduler.as_deref() {
                    scheduler.wait(SetupLaneKind::AbiWarm);
                }
                let started = Instant::now();
                let mut abi_scx = skull_mir::lower::ScalarCtx::new(ws)
                    .sharing(std::sync::Arc::clone(graph.interner()));
                if !skip_abi_plan_publish {
                    abi_scx.warm_abi_plans(ws, &abi_plans, abi_always_plans);
                }
                let population = abi_scx.abi_plan_stats().warm_keys as usize;
                let finished = Instant::now();
                #[cfg(all(test, not(feature = "sequential")))]
                if let Some(scheduler) = setup_scheduler.as_deref() {
                    scheduler.finish(SetupLaneKind::AbiWarm);
                }
                (
                    abi_scx,
                    SetupLaneWindow {
                        started,
                        finished,
                        population,
                    },
                )
            },
        )
    };
    #[cfg(all(test, not(feature = "sequential")))]
    let ((cpu_result, shader_result), (abi_scx, abi_lane)) = if matches!(
        mutation,
        BodyMutation::SetupLaneSchedule(SetupLaneSchedule::Handshake)
    ) {
        rayon::ThreadPoolBuilder::new()
            .num_threads(3)
            .build()
            .expect("setup handshake test pool")
            .install(run_setup_lanes)
    } else {
        run_setup_lanes()
    };
    #[cfg(any(not(test), feature = "sequential"))]
    let ((cpu_result, shader_result), (abi_scx, abi_lane)) = run_setup_lanes();
    let mono_after_lanes = graph.interner().stats();
    #[cfg(test)]
    let setup_handshake = {
        #[cfg(not(feature = "sequential"))]
        {
            setup_scheduler
                .as_deref()
                .map(|scheduler| scheduler.handshake())
                .unwrap_or((0, 0, false))
        }
        #[cfg(feature = "sequential")]
        {
            (0, 0, false)
        }
    };
    let (lua_view, mut scx, cpu_lane) = cpu_result;
    let (shader_result, shader_lane) = shader_result;
    let (shader_view, shader_registrations) = shader_result?;
    scx.adopt_warmed_abi(abi_scx);
    let state_slots = lua_global_state_slots(ws, &scx)?.into_iter().collect();
    let mut cg = CodegenCtx::new_with_state(ws, state_slots);
    cg.set_call_sccs(call_sccs(&graph));
    if let Some(on) = shapes.frame_limits {
        cg.set_frame_limits(on);
    }
    if let Some(on) = shapes.line_directives {
        cg.set_line_directives(on);
    }
    let setup_lanes = SetupLaneAttribution {
        cpu_population: cpu_lane.population,
        shader_population: shader_lane.population,
        abi_population: abi_lane.population,
        cpu_shader_overlap: setup_lanes_overlap(cpu_lane, shader_lane),
        abi_cpu_overlap: setup_lanes_overlap(abi_lane, cpu_lane),
        abi_shader_overlap: setup_lanes_overlap(abi_lane, shader_lane),
        first_started_lane: Some(first_setup_lane(cpu_lane, shader_lane, abi_lane)),
        mono_write_acqs_during_join: mono_after_lanes
            .write_acqs
            .saturating_sub(mono_before_lanes.write_acqs),
        mono_intern_attempts_during_join: mono_after_lanes
            .intern_attempts
            .saturating_sub(mono_before_lanes.intern_attempts),
        mono_intern_new_during_join: mono_after_lanes
            .intern_new
            .saturating_sub(mono_before_lanes.intern_new),
        #[cfg(test)]
        setup_handshake_entered: setup_handshake.0,
        #[cfg(test)]
        setup_handshake_released: setup_handshake.1,
        #[cfg(test)]
        setup_handshake_released_after_both: setup_handshake.2,
    };
    setup_phases.push(("setup: artifact+shader+ABI lanes", t_setup.elapsed()));
    setup_phases.push((
        "join: cpu artifact branch",
        cpu_lane.finished - cpu_lane.started,
    ));
    setup_phases.push((
        "join: shader view branch",
        shader_lane.finished - shader_lane.started,
    ));
    setup_phases.push((
        "join: ABI plan warm branch",
        abi_lane.finished - abi_lane.started,
    ));

    // The emitted param list must be injective. Checked per SIGNATURE, over every
    // function rather than just the emitted set: a duplicate Lua param is illegal
    // regardless of which filter would have reached the body.
    let t_setup = Instant::now();
    for m in ws.modules.iter().filter(|m| m.id != ModuleId::builtin()) {
        for (fid, _) in m.functions.iter() {
            let fref = FuncRef {
                module: m.id,
                func: fid,
            };
            for (key, err) in skull_mir::lower::scalar_abi_param_collisions(ws, &scx, fref) {
                cg.reject(key, err);
            }
        }
    }
    setup_phases.push(("setup: ABI validation", t_setup.elapsed()));

    let setup_traffic = (scx.rep_arena_stats(), scx.interner().stats());

    let fn_items: Vec<(FuncRef, &bone_semantic::hir::Function)> = ws
        .modules
        .iter()
        .filter(|m| m.id != ModuleId::builtin())
        .flat_map(|m| {
            let view = &lua_view;
            m.functions.iter().filter_map(move |(fid, f)| {
                let self_ref = FuncRef {
                    module: m.id,
                    func: fid,
                };
                (is_cached_body(f) && should_emit_via_view(ws, view, self_ref))
                    .then_some((self_ref, f))
            })
        })
        .collect();
    let t_setup = Instant::now();
    let inline_candidate_table = build_inline_candidate_table(
        ws,
        &scx,
        &concrete,
        inline_candidate_keys(&concrete),
        None,
        None,
        None,
        None,
        None,
    )?;
    setup_phases.push(("setup: inline candidate table", t_setup.elapsed()));
    let chunks = par_map_with(
        fn_items,
        || {
            skull_transform::InlineCache::with_candidate_table(std::sync::Arc::clone(
                &inline_candidate_table,
            ))
        },
        |inline_cache, (self_ref, f)| {
            (
                shared(self_ref),
                emit_body_chunk_with_mutation(
                    ws,
                    &cg,
                    &scx,
                    &concrete,
                    self_ref,
                    f,
                    inline_cache,
                    mutation,
                ),
            )
        },
    );
    let chunks: FxHashMap<Instance, Result<Chunk, CompilerError>> = chunks.into_iter().collect();
    // Seed the reverse dependency index from every chunk's codegen deps.
    let mut dependents: FxHashMap<FuncRef, FxHashSet<Instance>> = FxHashMap::default();
    let mut dependency_edges = 0;
    for (body, chunk) in &chunks {
        if let Ok(c) = chunk {
            dependency_edges += c.deps.len();
            if build.lifetime() == CacheLifetime::Retained {
                for d in &c.deps {
                    dependents.entry(*d).or_default().insert(body.clone());
                }
            }
        }
    }
    let concrete_stats = if concrete_stats_requested() {
        concrete.snapshot_build_stats()
    } else {
        crate::concrete_body::ConcreteBuildStats::default()
    };
    let mut cache = SkullEmitCache {
        chunks,
        concrete,
        graph,
        lua_view,
        changed_since_full: FxHashSet::default(),
        cg,
        scx,
        inline_cache: skull_transform::InlineCache::with_candidate_table(inline_candidate_table),
        dependents,
        changed_consts_since_full: FxHashSet::default(),
        static_key_baseline: FxHashMap::default(),
        roots: RootCache {
            project_root: project_root.map(str::to_string),
            interface_roots,
            sim_roots,
            shader_roots,
            roots: emission_roots.clone(),
        },
        shader_registrations,
        shader_view,
        emitted: FxHashSet::default(),
        code_artifact: std::sync::Arc::new(skull_mir::code_identity::CodeArtifact::default()),
        lifecycle_plan: None,
        global_layout: None,
        definition_plan: None,
        artifact_fingerprint: None,
        fingerprint_code_rows: None,
        unit_comptime_reads: UnitComptimeReads::default(),
        renderer_target,
        build_kind: build.kind(),
        selected_sim_functions,
        repair_stats: IncrementalRepairStats::default(),
        body_edit_rollback: None,
        full_scans: false,
    };
    let mut built_specs: Vec<(Instance, Chunk)> = Vec::new();
    let graph_census = cache.graph.run_stats;
    let (mut emit, emitted, unit_reads) =
        assemble(ws, &mut cache, &mut built_specs, build.lifetime())?;
    emit.concrete_stats = concrete_stats;
    emit.setup_lanes = setup_lanes;
    let mut phases = setup_phases;
    phases.extend(emit.phases);
    emit.phases = phases;
    emit.arena_traffic = ArenaTraffic {
        rep_setup: setup_traffic.0,
        rep_final: cache.scx.rep_arena_stats(),
        mono_setup: setup_traffic.1,
        mono_final: cache.scx.interner().stats(),
    };
    if build.lifetime() == CacheLifetime::OneShot {
        emit.graph_census = graph_census;
        return Ok((emit, cache));
    }
    cache.emitted = emitted;
    cache.code_artifact = emit.code_artifact.clone();
    cache.lifecycle_plan = Some(std::sync::Arc::new(emit.lifecycle_plan.clone()));
    cache.global_layout = Some(emit.global_layout.clone());
    cache.definition_plan = Some(std::sync::Arc::new(emit.definition_plan.clone()));
    cache.artifact_fingerprint = Some(emit.artifact_fingerprint.clone());
    cache.fingerprint_code_rows = Some(
        crate::artifact_fingerprint::CodeRows::collect(
            ws,
            cache.scx.interner(),
            &emit.code_artifact,
        )
        .map_err(|error| CompilerError::new_no_location(error.to_string()))?,
    );
    cache.unit_comptime_reads = unit_reads;
    dependency_edges += cache.absorb_spec_chunks(built_specs);
    // Baseline every `static` cell's reload key so the overlay can detect an
    // initializer edit (changed key) vs an unrelated body edit (unchanged key),
    // and its storage keys so the overlay can drop a static an edit removed.
    // After the specializations are absorbed: a generic's static lives in its
    // specialization's chunk.
    cache.advance_static_baseline(ws)?;
    emit.compile_counts.chunks = cache.chunks.len();
    emit.compile_counts.dependency_edges = Some(dependency_edges);
    emit.graph_census = graph_census;
    emit.release_stats.retained_dependency_edges =
        cache.dependents.values().map(|keys| keys.len()).sum();
    emit.release_stats.retained_static_keys = cache.static_key_baseline.len();
    Ok((emit, cache))
}
