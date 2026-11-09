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

/// Setup timings in run order, reported ahead of assembly's own phases.
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

/// The root sets one full build is taken over.
struct FullRoots {
    interface_roots: Vec<FuncRef>,
    sim_roots: Vec<FuncRef>,
    shader_roots: Vec<FuncRef>,
    emission_roots: Vec<FuncRef>,
}

/// Whole-workspace rejections that precede any graph or lowering work.
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
    Ok(())
}

/// Generated glue is part of the ROOT INPUT. Shader declarations are known
/// before the graph; their GLSL bodies are rendered only after the view owns
/// the retained instances.
fn collect_full_roots(
    ws: &Workspace,
    build: FullBuild<'_>,
    project_root: Option<&str>,
) -> Result<
    (
        skull_mir::lower::ShaderCollection,
        Vec<crate::shader_artifact::ShaderProgram>,
        FullRoots,
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
    #[cfg(test)]
    if matches!(build, FullBuild::Program | FullBuild::OneShotProgram) {
        assert_eq!(
            emission_roots,
            all_roots(ws, project_root, &shader_roots),
            "cached full-build roots diverged from the wholesale root oracle"
        );
    }
    Ok((
        shader_collection,
        shader_programs,
        FullRoots {
            interface_roots,
            sim_roots,
            shader_roots,
            emission_roots,
        },
    ))
}

/// The interface and sim-glue roots of one build kind.
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

/// A selected-sims build roots the sims a reached body names, until no new
/// one is reached, rebuilding the graph over each widened root set. Returns
/// the names of the sims the final graph reaches.
fn expand_selected_sim_roots(
    ws: &Workspace,
    roots: &mut FullRoots,
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
        *graph = crate::instance_graph::build(ws, roots.emission_roots.clone(), false);
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

/// Concrete lowering plans for every retained instance; a plan that cannot be
/// built rejects the compile.
#[cfg_attr(not(test), allow(unused_variables))]
fn build_concrete_plans(
    ws: &Workspace,
    graph: &crate::instance_graph::InstanceGraph,
    mutation: BodyMutation,
) -> Result<crate::concrete_body::ConcreteBodies, CompilerError> {
    #[allow(unused_mut)]
    let mut concrete = {
        #[cfg(test)]
        if let BodyMutation::ConcreteResultOrder(order) = mutation {
            crate::concrete_body::build_with_result_order(ws, graph, order).0
        } else {
            crate::concrete_body::build(ws, graph)
        }
        #[cfg(not(test))]
        {
            crate::concrete_body::build(ws, graph)
        }
    };
    #[cfg(test)]
    mutate_concrete_plans(&mut concrete, mutation);
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
    Ok(concrete)
}

#[cfg(test)]
fn mutate_concrete_plans(
    concrete: &mut crate::concrete_body::ConcreteBodies,
    mutation: BodyMutation,
) {
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
}

/// Test-only ordering of the setup lanes; inert in production.
struct LaneGate {
    #[cfg(all(test, not(feature = "sequential")))]
    scheduler: Option<std::sync::Arc<SetupLaneScheduler>>,
}

impl LaneGate {
    #[cfg_attr(any(not(test), feature = "sequential"), allow(unused_variables))]
    fn new(mutation: BodyMutation) -> Self {
        Self {
            #[cfg(all(test, not(feature = "sequential")))]
            scheduler: match mutation {
                BodyMutation::SetupLaneSchedule(schedule) => {
                    Some(std::sync::Arc::new(SetupLaneScheduler::new(schedule)))
                }
                _ => None,
            },
        }
    }

    /// Run one lane, held back or released per the test schedule. The lane
    /// times its own window inside `f`, so the gate's waits are not counted.
    #[cfg_attr(any(not(test), feature = "sequential"), allow(unused_variables))]
    fn run<T>(&self, lane: SetupLaneKind, f: impl FnOnce() -> T) -> T {
        #[cfg(all(test, not(feature = "sequential")))]
        if let Some(scheduler) = self.scheduler.as_deref() {
            scheduler.wait(lane);
        }
        let result = f();
        #[cfg(all(test, not(feature = "sequential")))]
        if let Some(scheduler) = self.scheduler.as_deref() {
            scheduler.finish(lane);
        }
        result
    }

    #[cfg(test)]
    fn handshake(&self) -> (usize, usize, bool) {
        #[cfg(not(feature = "sequential"))]
        {
            self.scheduler
                .as_deref()
                .map(|scheduler| scheduler.handshake())
                .unwrap_or((0, 0, false))
        }
        #[cfg(feature = "sequential")]
        {
            (0, 0, false)
        }
    }
}

/// Lane one: the CPU artifact view, realized with the scalar context lowering
/// will use, and the codegen context over that scalar context. The global
/// state slots and the call graph read the graph and the declarations only,
/// so they need no ABI plan and run here, off the plan build's path.
fn cpu_artifact_lane(
    ws: &Workspace,
    graph: &crate::instance_graph::InstanceGraph,
    emission_roots: &[FuncRef],
    shapes: ShapeOverrides,
) -> (
    Result<
        (
            crate::artifact::ArtifactView,
            skull_mir::lower::ScalarCtx,
            CodegenCtx,
        ),
        CompilerError,
    >,
    SetupLaneWindow,
) {
    let started = Instant::now();
    let scx = skull_mir::lower::ScalarCtx::new(ws).sharing(std::sync::Arc::clone(graph.interner()));
    let lua_view = crate::artifact::realize(
        ws,
        graph,
        &crate::artifact::ArtifactRequest::new(
            crate::target_projection::LUAJIT_DESKTOP,
            emission_roots,
        )
        .with_scalar_context(&scx),
    );
    let population = lua_view.instances.len() + lua_view.boundaries.len();
    let cg = full_codegen_ctx(ws, &scx, graph, shapes);
    let finished = Instant::now();
    (
        cg.map(|cg| (lua_view, scx, cg)),
        SetupLaneWindow {
            started,
            finished,
            population,
        },
    )
}

/// Lane two: the shader view and its registration lines.
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
    SetupLaneWindow,
) {
    let started = Instant::now();
    // The overload set lives in `cg` (computed once there);
    // borrow it rather than rescanning the workspace.
    let shader_scx = skull_mir::lower::ScalarCtx::new_values_first_class(ws)
        .sharing(std::sync::Arc::clone(graph.interner()));
    let result = (|| {
        let shader_view =
            realize_shader_view(ws, graph, shader_programs, &shader_scx, shader_plans)?;
        let population =
            shader_view.roots.len() + shader_view.helpers.len() + shader_view.builtins.len();
        let shader_registrations =
            shader_registration_lines(ws, &shader_scx, shader_collection, &shader_view)?;
        Ok::<_, CompilerError>((shader_view, shader_registrations, population))
    })();
    let population = result
        .as_ref()
        .map(|(_, _, population)| *population)
        .unwrap_or(0);
    let finished = Instant::now();
    (
        result.map(|(view, registrations, _)| (view, registrations)),
        SetupLaneWindow {
            started,
            finished,
            population,
        },
    )
}

/// Lane three: a scalar context with every concrete plan's ABI warmed, adopted
/// by the CPU lane's context after the join.
fn abi_warm_lane(
    ws: &Workspace,
    graph: &crate::instance_graph::InstanceGraph,
    abi_plans: &skull_mir::concrete::ConcretePlans,
    abi_always_plans: &skull_mir::concrete::ConcreteAlwaysPlans,
    skip_abi_plan_publish: bool,
) -> (skull_mir::lower::ScalarCtx, SetupLaneWindow) {
    let started = Instant::now();
    let mut abi_scx =
        skull_mir::lower::ScalarCtx::new(ws).sharing(std::sync::Arc::clone(graph.interner()));
    if !skip_abi_plan_publish {
        abi_scx.warm_abi_plans(ws, abi_plans, abi_always_plans);
    }
    let population = abi_scx.abi_plan_stats().warm_keys as usize;
    let finished = Instant::now();
    (
        abi_scx,
        SetupLaneWindow {
            started,
            finished,
            population,
        },
    )
}

/// What the three concurrent setup lanes produce.
struct SetupLanes {
    lua_view: crate::artifact::ArtifactView,
    scx: skull_mir::lower::ScalarCtx,
    cg: CodegenCtx,
    shader_view: crate::shader_artifact::ShaderArtifactView,
    shader_registrations: ShaderRegistrations,
    attribution: SetupLaneAttribution,
    windows: [(&'static str, Duration); 4],
}

/// The graph is complete before these lanes start; shared interning remains
/// synchronized. The CPU artifact lane runs beside the concrete plan build,
/// which the shader and ABI lanes wait for. ABI plans carry their own
/// representation arena, so the warmed context is adopted after join.
fn run_setup_lanes(
    ws: &Workspace,
    graph: &crate::instance_graph::InstanceGraph,
    build_plans: impl FnOnce() -> Result<crate::concrete_body::ConcreteBodies, CompilerError> + Send,
    shader_programs: &[crate::shader_artifact::ShaderProgram],
    shader_collection: &skull_mir::lower::ShaderCollection,
    emission_roots: &[FuncRef],
    shapes: ShapeOverrides,
    mutation: BodyMutation,
) -> Result<(crate::concrete_body::ConcreteBodies, SetupLanes), CompilerError> {
    #[cfg(test)]
    let skip_abi_plan_publish = matches!(mutation, BodyMutation::SkipAbiPlanPublish);
    #[cfg(not(test))]
    let skip_abi_plan_publish = false;
    let gate = LaneGate::new(mutation);
    let mono_before_lanes = graph.interner().stats();
    let plans_branch = || {
        let started = Instant::now();
        let concrete = build_plans();
        let plans_window = started.elapsed();
        let concrete = match concrete {
            Ok(concrete) => concrete,
            Err(err) => return (Err(err), plans_window),
        };
        let shader_plans = std::sync::Arc::clone(&concrete.plans);
        let lanes = parallel_join(
            || {
                gate.run(SetupLaneKind::Shader, || {
                    shader_lane(ws, graph, shader_programs, shader_collection, shader_plans)
                })
            },
            || {
                gate.run(SetupLaneKind::AbiWarm, || {
                    abi_warm_lane(
                        ws,
                        graph,
                        &concrete.plans,
                        &concrete.always_plans,
                        skip_abi_plan_publish,
                    )
                })
            },
        );
        (Ok((concrete, lanes)), plans_window)
    };
    // The plan build stays on this thread: its build counter is a
    // thread-local witness the incremental repair statistics read back.
    let run_lanes = || {
        join_in_place(
            || {
                gate.run(SetupLaneKind::CpuArtifact, || {
                    cpu_artifact_lane(ws, graph, emission_roots, shapes)
                })
            },
            plans_branch,
        )
    };
    #[cfg(all(test, not(feature = "sequential")))]
    let (cpu_result, (plans_result, plans_window)) = if matches!(
        mutation,
        BodyMutation::SetupLaneSchedule(SetupLaneSchedule::Handshake)
    ) {
        rayon::ThreadPoolBuilder::new()
            .num_threads(3)
            .build()
            .expect("setup handshake test pool")
            .install(run_lanes)
    } else {
        run_lanes()
    };
    #[cfg(any(not(test), feature = "sequential"))]
    let (cpu_result, (plans_result, plans_window)) = run_lanes();
    let (concrete, (shader_result, (abi_scx, abi_lane))) = plans_result?;
    let mono_after_lanes = graph.interner().stats();
    #[cfg(test)]
    let setup_handshake = gate.handshake();
    let (cpu_result, cpu_lane) = cpu_result;
    let (shader_result, shader_lane) = shader_result;
    let (lua_view, mut scx, cg) = cpu_result?;
    let (shader_view, shader_registrations) = shader_result?;
    scx.adopt_warmed_abi(abi_scx);
    let attribution = SetupLaneAttribution {
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
    Ok((
        concrete,
        SetupLanes {
            lua_view,
            scx,
            cg,
            shader_view,
            shader_registrations,
            attribution,
            windows: [
                ("join: concrete plans branch", plans_window),
                (
                    "join: cpu artifact branch",
                    cpu_lane.finished - cpu_lane.started,
                ),
                (
                    "join: shader view branch",
                    shader_lane.finished - shader_lane.started,
                ),
                (
                    "join: ABI plan warm branch",
                    abi_lane.finished - abi_lane.started,
                ),
            ],
        },
    ))
}

/// The codegen context over the final global state slots and call graph.
fn full_codegen_ctx(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    graph: &crate::instance_graph::InstanceGraph,
    shapes: ShapeOverrides,
) -> Result<CodegenCtx, CompilerError> {
    let state_slots = lua_global_state_slots(ws, scx)?.into_iter().collect();
    let mut cg = CodegenCtx::new_with_state(ws, state_slots);
    cg.set_call_sccs(call_sccs(graph));
    if let Some(on) = shapes.frame_limits {
        cg.set_frame_limits(on);
    }
    if let Some(on) = shapes.line_directives {
        cg.set_line_directives(on);
    }
    Ok(cg)
}

/// The emitted param list must be injective. Checked per SIGNATURE, over every
/// function rather than just the emitted set: a duplicate Lua param is illegal
/// regardless of which filter would have reached the body.
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

/// Emit every retained ordinary/coroutine body into its chunk, in parallel
/// with one inline cache per worker over a shared candidate table.
fn emit_full_chunks(
    ws: &Workspace,
    cg: &CodegenCtx,
    scx: &skull_mir::lower::ScalarCtx,
    concrete: &crate::concrete_body::ConcreteBodies,
    lua_view: &crate::artifact::ArtifactView,
    mutation: BodyMutation,
    setup_phases: &mut SetupPhases,
) -> Result<
    (
        ChunkMap,
        std::sync::Arc<skull_transform::InlineCandidateTable>,
    ),
    CompilerError,
> {
    let fn_items: Vec<(FuncRef, &bone_semantic::hir::Function)> = ws
        .source_modules()
        .flat_map(|m| {
            m.functions.iter().filter_map(move |(fid, f)| {
                let self_ref = FuncRef {
                    module: m.id,
                    func: fid,
                };
                (is_cached_body(f) && should_emit_via_view(ws, lua_view, self_ref))
                    .then_some((self_ref, f))
            })
        })
        .collect();
    let inline_candidate_table = setup_phases.timed("setup: inline candidate table", || {
        build_inline_candidate_table(
            ws,
            scx,
            concrete,
            inline_candidate_keys(concrete),
            None,
            None,
            None,
            None,
            None,
        )
    })?;
    // The fan-out's wall window, beside the per-unit sums the chunks carry:
    // the budget reads idle time as threads * window - summed unit time.
    let chunks = setup_phases.timed("window: fan-out", || {
        par_map_with(
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
                        cg,
                        scx,
                        concrete,
                        self_ref,
                        f,
                        inline_cache,
                        mutation,
                    ),
                )
            },
        )
    });
    Ok((chunks.into_iter().collect(), inline_candidate_table))
}

/// Seed the reverse dependency index from every chunk's codegen deps (only a
/// retained cache keeps it). Also returns the dependency-edge total.
fn index_dependents(
    chunks: &ChunkMap,
    lifetime: CacheLifetime,
) -> (FxHashMap<FuncRef, FxHashSet<Instance>>, usize) {
    let mut dependents: FxHashMap<FuncRef, FxHashSet<Instance>> = FxHashMap::default();
    let mut dependency_edges = 0;
    for (body, chunk) in chunks {
        if let Ok(c) = chunk {
            dependency_edges += c.deps.len();
            if lifetime == CacheLifetime::Retained {
                for d in &c.deps {
                    dependents.entry(*d).or_default().insert(body.clone());
                }
            }
        }
    }
    (dependents, dependency_edges)
}

impl SkullEmitCache {
    /// Keep what a retained build's later incremental and overlay builds start
    /// from: the emitted set, the artifact identity, the absorbed
    /// specializations and the static baseline.
    fn retain_full_build(
        &mut self,
        ws: &Workspace,
        emit: &mut MirEmit,
        emitted: FxHashSet<Instance>,
        unit_reads: UnitComptimeReads,
        built_specs: Vec<(Instance, Chunk)>,
        mut dependency_edges: usize,
    ) -> Result<(), CompilerError> {
        self.emitted = emitted;
        self.code_artifact = emit.code_artifact.clone();
        self.lifecycle_plan = Some(std::sync::Arc::new(emit.lifecycle_plan.clone()));
        self.global_layout = Some(emit.global_layout.clone());
        self.definition_plan = Some(std::sync::Arc::new(emit.definition_plan.clone()));
        self.artifact_fingerprint = Some(emit.artifact_fingerprint.clone());
        self.fingerprint_code_rows = Some(
            crate::artifact_fingerprint::CodeRows::collect(
                ws,
                self.scx.interner(),
                &emit.code_artifact,
            )
            .map_err(|error| CompilerError::new_no_location(error.to_string()))?,
        );
        self.unit_comptime_reads = unit_reads;
        dependency_edges += self.absorb_spec_chunks(built_specs);
        // Baseline every `static` cell's reload key so the overlay can detect an
        // initializer edit (changed key) vs an unrelated body edit (unchanged key),
        // and its storage keys so the overlay can drop a static an edit removed.
        // After the specializations are absorbed: a generic's static lives in its
        // specialization's chunk.
        self.advance_static_baseline(ws)?;
        emit.compile_counts.chunks = self.chunks.len();
        emit.compile_counts.dependency_edges = Some(dependency_edges);
        emit.release_stats.retained_dependency_edges =
            self.dependents.values().map(|keys| keys.len()).sum();
        emit.release_stats.retained_static_keys = self.static_key_baseline.len();
        Ok(())
    }
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
        mutation,
    } = options;
    let mut setup_phases = SetupPhases::default();
    let project_root = setup_phases.timed("setup: validation+context", || {
        let project_root = canonical_project_root(ws, project_root)?;
        reject_workspace(ws, user_mid)?;
        Ok::<_, CompilerError>(project_root)
    })?;
    let project_root = project_root.as_deref();

    // **The graph is built FIRST, into a fresh arena, and `scx` shares that
    // arena rather than minting its own.** An `Instance` the graph mints has to
    // mean something to lowering, and a second arena would be a second identity
    // for the same instance. See `SkullEmitCache::graph`.
    let (shader_collection, shader_programs, mut roots) = setup_phases
        .timed("setup: root collection", || {
            collect_full_roots(ws, build, project_root)
        })?;
    let (graph, selected_sim_functions) = setup_phases.timed("setup: graph", || {
        let mut graph = crate::instance_graph::build(ws, roots.emission_roots.clone(), false);
        let selected_sim_functions = if matches!(build, FullBuild::SelectedSims(_)) {
            Some(expand_selected_sim_roots(ws, &mut roots, &mut graph)?)
        } else {
            None
        };
        if let Some(err) = graph_rejection_error(ws, &graph) {
            return Err(err);
        }
        Ok((graph, selected_sim_functions))
    })?;
    // The CPU artifact view and the codegen context read only the graph, so
    // they are built while the concrete plans are; the shader and ABI lanes
    // read the plans and start once they are published.
    let (mut concrete, lanes) = setup_phases.timed("setup: concrete plans+lanes", || {
        run_setup_lanes(
            ws,
            &graph,
            || build_concrete_plans(ws, &graph, mutation),
            &shader_programs,
            &shader_collection,
            &roots.emission_roots,
            shapes,
            mutation,
        )
    })?;
    let SetupLanes {
        lua_view,
        scx,
        cg,
        shader_view,
        shader_registrations,
        attribution: setup_lanes,
        windows,
    } = lanes;
    setup_phases.0.extend(windows);
    setup_phases.timed("setup: ABI validation", || {
        reject_abi_param_collisions(ws, &scx, &cg)
    });

    let setup_traffic = (scx.rep_arena_stats(), scx.interner().stats());
    let (chunks, inline_candidate_table) = emit_full_chunks(
        ws,
        &cg,
        &scx,
        &concrete,
        &lua_view,
        mutation,
        &mut setup_phases,
    )?;
    let (dependents, dependency_edges) = index_dependents(&chunks, build.lifetime());
    let concrete_stats = if concrete_stats_requested() {
        concrete.snapshot_build_stats()
    } else {
        crate::concrete_body::ConcreteBuildStats::default()
    };
    let FullRoots {
        interface_roots,
        sim_roots,
        shader_roots,
        emission_roots,
    } = roots;
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
            roots: emission_roots,
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
    let assembly_started = Instant::now();
    let (mut emit, emitted, unit_reads) =
        assemble(ws, &mut cache, &mut built_specs, build.lifetime())?;
    let assembly_window = assembly_started.elapsed();
    emit.concrete_stats = concrete_stats;
    emit.setup_lanes = setup_lanes;
    let mut phases = setup_phases.0;
    phases.extend(emit.phases);
    phases.push(("window: assembly", assembly_window));
    emit.phases = phases;
    emit.arena_traffic = ArenaTraffic {
        rep_setup: setup_traffic.0,
        rep_final: cache.scx.rep_arena_stats(),
        mono_setup: setup_traffic.1,
        mono_final: cache.scx.interner().stats(),
    };
    emit.graph_census = graph_census;
    if build.lifetime() == CacheLifetime::Retained {
        cache.retain_full_build(
            ws,
            &mut emit,
            emitted,
            unit_reads,
            built_specs,
            dependency_edges,
        )?;
    }
    Ok((emit, cache))
}
