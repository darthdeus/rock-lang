use super::*;

fn collect_callee_frefs(mf: &MirFunc, out: &mut FxHashSet<FuncRef>) {
    for (_, inst) in mf.insts.iter() {
        match inst {
            Inst::Call { func, .. } | Inst::CallMulti { func, .. } => {
                out.insert(*func);
            }
            _ => {}
        }
    }
}

fn collect_abi_dependencies(mf: &MirFunc, out: &mut FxHashMap<FuncRef, u64>) {
    out.extend(mf.call_abi_plans.values().filter_map(|plan| {
        plan.parameter_demand_key()
            .map(|key| (plan.subject.func, key))
    }));
    for nested in &mf.nested_fns {
        collect_abi_dependencies(nested, out);
    }
}

pub(super) fn emit_named_with_allocation_report(
    ws: &Workspace,
    cg: &CodegenCtx,
    interner: &skull_mir::monoty::MonoInterner,
    name: &str,
    mf: &mut MirFunc,
    inherited_statics: &[skull_mir::mir::StaticCell],
    bound: Option<&skull_codegen_lua::CalleeBindingPlan>,
) -> skull_codegen_lua::LuaEmission {
    skull_codegen_lua::with_constants_baked(cg, mf, |mf| {
        skull_codegen_lua::emit_lua_definition(
            ws,
            cg,
            interner,
            name,
            mf,
            None,
            inherited_statics,
            bound,
            crate::allocation_report::enabled().then(crate::allocation_report::activation_enabled),
        )
    })
}

fn emit_coroutine_chunk(
    ws: &Workspace,
    cg: &CodegenCtx,
    scx: &skull_mir::lower::ScalarCtx,
    concrete_plan: &skull_mir::concrete::ConcreteBodyPlan,
    f: &bone_semantic::hir::Function,
    name: &str,
    mf: &mut MirFunc,
    mut times: PhaseTimes,
) -> Result<Chunk, CompilerError> {
    let cleanup_before = mir_live_instructions(mf);
    let tc = Instant::now();
    skull_transform::cleanup_with_nobox(ws, mf)?;
    times.cleanup = tc.elapsed();
    times.cleanup_work = mir_live_instructions(mf) != cleanup_before;
    let tv = Instant::now();
    skull_transform::validate_no_residual_bound_calls(ws, f, mf)?;
    let mut specs = Vec::new();
    validate_emitted_body(ws, mf, &mut specs).map_err(|r| r.into_error(f.name_span, ""))?;
    times.validation += tv.elapsed();
    times.validation_work = true;
    skull_codegen_lua::regalloc_time_reset();
    let te = Instant::now();
    let inst = concrete_plan
        .function_instance()
        .expect("a coroutine emission plan must own a function instance");
    let plan = skull_mir::lower::CoroutinePlan::from_concrete_plan(
        ws,
        scx.interner(),
        concrete_plan,
        inst.symbol(ws, scx.interner()),
    )
    .map_err(|reason| {
        CompilerError::new_with_label(
            f.name_span,
            format!(
                "cannot build concrete coroutine plan for `{}`: {}",
                f.name,
                reason.bucket()
            ),
        )
    })?;
    let (lua, code_artifact, allocation_bodies, mut deps, statics) =
        crate::coroutine_emit::emit_coroutine_with_artifact(ws, cg, scx, f, &plan, mf)?;
    times.emit = te.elapsed();
    times.emit_work = times.emit > Duration::ZERO;
    times.regalloc = skull_codegen_lua::regalloc_time_elapsed().min(times.emit);
    times.regalloc_work = times.regalloc > Duration::ZERO;
    let mut abi_deps = FxHashMap::default();
    collect_abi_dependencies(mf, &mut abi_deps);
    deps.extend(abi_deps.keys().copied());
    let mut literal_globals = skull_mir::reach::global_ref_names(mf);
    crate::push_value_ctor_calls(&lua, &mut literal_globals);
    Ok(Chunk {
        name: Ustr::from(name),
        abi_deps,
        definition_count: lua_definition_count(&lua),
        lua,
        literal_globals,
        allocation_bodies,
        embedded_deps: deps.clone(),
        deps,
        direct_globals: Vec::new(),
        direct_calls: Vec::new(),
        times,
        attribution: Some(Box::default()),
        statics,
        comptime_reads: mf.comptime_reads.clone(),
        calls: skull_mir::reach::call_targets(mf),
        specs,
        code_artifact: code_artifact.into(),
        binds: Vec::new(),
    })
}

pub(super) fn emit_body_chunk(
    ws: &Workspace,
    cg: &CodegenCtx,
    scx: &skull_mir::lower::ScalarCtx,
    concrete: &crate::concrete_body::ConcreteBodies,
    self_ref: FuncRef,
    f: &bone_semantic::hir::Function,
    inline_cache: &mut skull_transform::InlineCache,
) -> Result<Chunk, CompilerError> {
    let plan = concrete.plan_for(&shared(self_ref)).ok_or_else(|| {
        CompilerError::new_with_label(f.name_span, format!("no concrete plan for `{}`", f.name))
    })?;
    let ordinary_instance = plan.function_instance().cloned();
    let (mut mf, mut times, mut attribution) = lower_body(
        ws,
        scx,
        concrete,
        plan,
        ordinary_instance.as_ref(),
        LoweredInstanceKind::Ordinary,
        inline_owner_key(self_ref),
        self_ref,
        inline_cache,
    )?;
    if f.is_coroutine {
        let name = mangle_fn(ws, f);
        let mut chunk = emit_coroutine_chunk(ws, cg, scx, plan, f, &name, &mut mf, times)?;
        attribution.mir_instructions = mf.insts.len();
        attribution.times = chunk.times;
        chunk.attribution = Some(Box::new(attribution));
        return Ok(chunk);
    }
    // Hoisted bodies inline only after closure conversion has split them
    // out: inside a closure a cell is an upvalue, and splicing there piles
    // onto LuaJIT's 60-upvalue cap.
    let name = mangle_fn(ws, f);
    let mut stats = ChainStats::new(&mut times, &mut attribution.inline);
    stats.ordinary = true;
    stats.ordinary_key = ordinary_instance;
    let inliner = skull_transform::body_chain::Inliner {
        scx,
        self_ref,
        cache: inline_cache,
        plans: &concrete.plans,
    };
    let opts = skull_transform::body_chain::ChainOptions {
        target: bone_core::hir::FnBackend::Lua,
        reserve: &|name| {
            cg.mint_synthetic(name.to_string());
        },
        counted_loops: skull_transform::body_chain::CountedLoops::Root,
        inline_hoisted: true,
    };
    let deps =
        skull_transform::body_chain::prepare_body(ws, inliner, &mut mf, &name, &opts, &mut stats)
            .map_err(|e| match e.pass {
            skull_transform::body_chain::BodyPass::HoistedInline => {
                CompilerError::new_with_label(f.name_span, e.error.message)
            }
            _ => e.error,
        })?;
    finish_body_chunk(
        ws,
        cg,
        scx,
        f,
        self_ref,
        &name,
        LoweredInstanceKind::Ordinary,
        mf,
        deps,
        times,
        attribution,
    )
}

fn lower_body(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    concrete: &crate::concrete_body::ConcreteBodies,
    plan: &skull_mir::concrete::ConcreteBodyPlan,
    instance: Option<&Instance>,
    kind: LoweredInstanceKind,
    owner: String,
    fref: FuncRef,
    inline_cache: &mut skull_transform::InlineCache,
) -> Result<(MirFunc, PhaseTimes, UnitAttribution), CompilerError> {
    let tl = Instant::now();
    let canonical_body = instance.and_then(|instance| inline_cache.canonical_body(instance));
    let canonical_reused = canonical_body.is_some();
    let mf = match canonical_body {
        Some(body) => (*body).clone(),
        None => skull_mir::lower::lower_instance_with_plans(ws, scx, plan, &concrete.plans)?,
    };
    let times = PhaseTimes {
        lower: tl.elapsed(),
        lower_work: !mf.insts.is_empty(),
        ..PhaseTimes::default()
    };
    let attribution = UnitAttribution {
        owner,
        examined: true,
        hir_nodes: hir_nodes_for_owner(ws, fref.module, BodyOwner::Function(fref.func)),
        mir_instructions: mf.insts.len(),
        mir_lowered: mf.insts.len(),
        lower_facts: mf.lower_attribution.clone(),
        abi_plan: mf.abi_plan.clone(),
        consumer_instance: instance.map(|instance| (instance.clone(), kind)),
        lowered_instance: instance
            .filter(|_| !canonical_reused)
            .map(|instance| (instance.clone(), kind)),
        canonical_reused,
        ..UnitAttribution::default()
    };
    Ok((mf, times, attribution))
}

fn finish_body_chunk(
    ws: &Workspace,
    cg: &CodegenCtx,
    scx: &skull_mir::lower::ScalarCtx,
    f: &bone_semantic::hir::Function,
    self_ref: FuncRef,
    name: &str,
    kind: LoweredInstanceKind,
    mut mf: MirFunc,
    mut deps: FxHashSet<FuncRef>,
    mut times: PhaseTimes,
    mut attribution: UnitAttribution,
) -> Result<Chunk, CompilerError> {
    let ordinary = matches!(kind, LoweredInstanceKind::Ordinary);
    let tv = Instant::now();
    skull_transform::validate_no_residual_bound_calls(ws, f, &mf)?;
    skull_transform::body_chain::debug_verify(
        &mf,
        skull_mir::verify::Stage::EMIT_READY,
        &"the residual-bound-call gate",
    );
    let mut specs = Vec::new();
    let prefix = if ordinary {
        String::new()
    } else {
        format!("cannot emit specialized `{name}`: ")
    };
    validate_emitted_body(ws, &mf, &mut specs).map_err(|r| r.into_error(f.name_span, &prefix))?;
    let embedded_deps = ordinary.then(|| deps.clone());
    collect_callee_frefs(&mf, &mut deps);
    let mut code_artifact =
        skull_mir::code_identity::CodeArtifact::from_mir_tree_with_volatile_vars(
            &mf,
            &ws.comptime_inputs.volatile_vars,
        );
    code_artifact.set_body_names(&mf, name);
    let (direct_globals, direct_calls) = if ordinary {
        direct_refs(ws, cg, &mf)
    } else {
        (Vec::new(), Vec::new())
    };
    times.validation += tv.elapsed();
    times.validation_work = true;
    skull_codegen_lua::regalloc_time_reset();
    let te = Instant::now();
    let bind = skull_codegen_lua::callee_binding_plan(ws, cg, scx.interner(), name, self_ref, &mf);
    let emission = emit_named_with_allocation_report(
        ws,
        cg,
        scx.interner(),
        name,
        &mut mf,
        &[],
        bind.as_ref(),
    );
    let mut lua = emission.lua;
    let mut allocation_bodies = Vec::new();
    if let Some(body) = crate::allocation_report::collect_emitted_body(
        ws,
        scx.interner(),
        name,
        f.name.as_str(),
        &mf,
        emission.allocation_sites,
    ) {
        allocation_bodies.push(body);
    }
    let root_emit = te.elapsed();
    times.emit += root_emit;
    times.emit_work |= root_emit > Duration::ZERO;
    let regalloc = skull_codegen_lua::regalloc_time_elapsed().min(root_emit);
    times.regalloc += regalloc;
    times.regalloc_work |= regalloc > Duration::ZERO;
    let mut literal_globals = skull_mir::reach::global_ref_names(&mf);
    let mut abi_deps = FxHashMap::default();
    collect_abi_dependencies(&mf, &mut abi_deps);
    let mut statics = mf.statics.clone();
    emit_nested_fns(
        ws,
        cg,
        scx,
        &mut mf.nested_fns,
        &mf.statics,
        &mut deps,
        &mut statics,
        &mut allocation_bodies,
        &mut lua,
        &mut times,
    )?;
    attribution.mir_instructions = mf.insts.len();
    attribution.times = times;
    deps.extend(abi_deps.keys().copied());
    crate::push_value_ctor_calls(&lua, &mut literal_globals);
    Ok(Chunk {
        name: Ustr::from(name),
        abi_deps,
        definition_count: lua_definition_count(&lua),
        lua,
        literal_globals,
        allocation_bodies,
        embedded_deps: embedded_deps.unwrap_or_else(|| deps.clone()),
        deps,
        direct_globals,
        direct_calls,
        times,
        statics,
        calls: skull_mir::reach::call_targets(&mf),
        specs,
        code_artifact: code_artifact.into(),
        attribution: Some(Box::new(attribution)),
        comptime_reads: mf.comptime_reads.clone(),
        binds: bind.into_iter().collect(),
    })
}

pub(crate) fn emit_nested_fns(
    ws: &Workspace,
    cg: &CodegenCtx,
    scx: &skull_mir::lower::ScalarCtx,
    nested: &mut [MirFunc],
    inherited_statics: &[skull_mir::mir::StaticCell],
    deps: &mut FxHashSet<FuncRef>,
    statics: &mut Vec<skull_mir::mir::StaticCell>,
    allocation_bodies: &mut Vec<crate::allocation_report::AllocationBody>,
    out: &mut String,
    times: &mut PhaseTimes,
) -> Result<(), CompilerError> {
    for nf in nested.iter_mut() {
        let cleanup_before = mir_live_instructions(nf);
        let tc = Instant::now();
        skull_transform::cleanup_with_nobox(ws, nf)?;
        times.cleanup += tc.elapsed();
        times.cleanup_work |= mir_live_instructions(nf) != cleanup_before;
        let counted_before = nf.blocks.len();
        let tl = Instant::now();
        skull_transform::recover_counted_loops(ws, nf);
        times.counted_loop += tl.elapsed();
        times.counted_loop_work |= nf.blocks.len() > counted_before;
        let tb = Instant::now();
        times.bounds_elided += skull_transform::elide_bounds_checks_for_target(
            Some(ws),
            nf,
            bone_core::hir::FnBackend::Lua,
        );
        times.bounds += tb.elapsed();
        collect_callee_frefs(nf, deps);
        if let Some(source) = nf.flat_variant_source {
            deps.insert(source);
        }
        statics.extend(nf.statics.iter().cloned());
        skull_codegen_lua::regalloc_time_reset();
        let te = Instant::now();
        let emission = emit_named_with_allocation_report(
            ws,
            cg,
            scx.interner(),
            nf.name.as_str(),
            nf,
            inherited_statics,
            None,
        );
        out.push_str(&emission.lua);
        if let Some(body) = crate::allocation_report::collect_emitted_body(
            ws,
            scx.interner(),
            nf.name.as_str(),
            nf.name.as_str(),
            nf,
            emission.allocation_sites,
        ) {
            allocation_bodies.push(body);
        }
        let emit = te.elapsed();
        times.emit += emit;
        times.emit_work |= emit > Duration::ZERO;
        let regalloc = skull_codegen_lua::regalloc_time_elapsed().min(emit);
        times.regalloc += regalloc;
        times.regalloc_work |= regalloc > Duration::ZERO;
        let mut children = std::mem::take(&mut nf.nested_fns);
        let mut next_inherited = inherited_statics.to_vec();
        next_inherited.extend(nf.statics.iter().cloned());
        emit_nested_fns(
            ws,
            cg,
            scx,
            &mut children,
            &next_inherited,
            deps,
            statics,
            allocation_bodies,
            out,
            times,
        )?;
    }
    Ok(())
}

pub(super) fn convert_unit_body(
    ws: &Workspace,
    cg: &CodegenCtx,
    mf: &mut MirFunc,
    name: &str,
    times: &mut PhaseTimes,
) -> Result<(), CompilerError> {
    let opts = skull_transform::body_chain::ChainOptions {
        target: bone_core::hir::FnBackend::Lua,
        reserve: &|name| {
            cg.mint_synthetic(name.to_string());
        },
        counted_loops: skull_transform::body_chain::CountedLoops::Skip,
        inline_hoisted: false,
    };
    let mut work = InlineWork::default();
    let mut stats = ChainStats::new(times, &mut work);
    skull_transform::body_chain::convert_body(ws, mf, name, &opts, &mut stats).map_err(|e| e.error)
}

pub(super) fn emit_pass_nested_fns(
    ws: &Workspace,
    cg: &CodegenCtx,
    scx: &skull_mir::lower::ScalarCtx,
    nested: &mut [MirFunc],
    inherited_statics: &[skull_mir::mir::StaticCell],
    mut allocation_bodies: Option<&mut Vec<crate::allocation_report::AllocationBody>>,
    out: &mut String,
    times: &mut PhaseTimes,
) -> Result<(), CompilerError> {
    for nf in nested.iter_mut() {
        let cleanup_before = mir_live_instructions(nf);
        let tc = Instant::now();
        skull_transform::cleanup_with_nobox(ws, nf)?;
        times.cleanup += tc.elapsed();
        times.cleanup_work |= mir_live_instructions(nf) != cleanup_before;
        skull_codegen_lua::regalloc_time_reset();
        let te = Instant::now();
        if let Some(bodies) = allocation_bodies.as_deref_mut() {
            let emission = emit_named_with_allocation_report(
                ws,
                cg,
                scx.interner(),
                nf.name.as_str(),
                nf,
                inherited_statics,
                None,
            );
            out.push_str(&emission.lua);
            if let Some(body) = crate::allocation_report::collect_emitted_body(
                ws,
                scx.interner(),
                nf.name.as_str(),
                nf.name.as_str(),
                nf,
                emission.allocation_sites,
            ) {
                bodies.push(body);
            }
        } else {
            let name = nf.name;
            out.push_str(&skull_codegen_lua::with_constants_baked(cg, nf, |nf| {
                skull_codegen_lua::emit_lua_definition(
                    ws,
                    cg,
                    scx.interner(),
                    name.as_str(),
                    nf,
                    None,
                    inherited_statics,
                    None,
                    None,
                )
                .lua
            }));
        }
        let emit = te.elapsed();
        times.emit += emit;
        times.emit_work |= emit > Duration::ZERO;
        let regalloc = skull_codegen_lua::regalloc_time_elapsed().min(emit);
        times.regalloc += regalloc;
        times.regalloc_work |= regalloc > Duration::ZERO;
        let mut children = std::mem::take(&mut nf.nested_fns);
        emit_pass_nested_fns(
            ws,
            cg,
            scx,
            &mut children,
            inherited_statics,
            allocation_bodies.as_deref_mut(),
            out,
            times,
        )?;
        nf.nested_fns = children;
    }
    Ok(())
}

/// Whether a function body is cached as a [`Chunk`].
pub fn is_cached_body(f: &bone_semantic::hir::Function) -> bool {
    !f.is_extern && f.inline_body.is_none() && !f.has_iterator_bounds() && !is_generic(f)
}

pub(crate) fn is_cached_emitted_body(f: &bone_semantic::hir::Function, body: &Instance) -> bool {
    if body.args.is_empty() {
        return is_cached_body(f);
    }
    !f.is_extern && f.inline_body.is_none()
}

pub fn is_generic(f: &bone_semantic::hir::Function) -> bool {
    !f.generics.is_empty()
}

pub(super) fn emit_spec_chunk(
    ws: &Workspace,
    cg: &CodegenCtx,
    scx: &skull_mir::lower::ScalarCtx,
    concrete: &crate::concrete_body::ConcreteBodies,
    body: &Instance,
    name: &str,
    inline_cache: &mut skull_transform::InlineCache,
) -> Result<Chunk, CompilerError> {
    let fref = body.func;
    let f = &ws.module(fref.module).functions[fref.func];
    let plan = concrete.plan_for(body).ok_or_else(|| {
        CompilerError::new_with_label(f.name_span, format!("no concrete plan for `{name}`"))
    })?;
    let tree = &plan.bound_tree;
    skull_mir::lower::validate_concrete_bound_plan(f, plan)?;
    let (mut mf, mut times, mut attribution) = lower_body(
        ws,
        scx,
        concrete,
        plan,
        Some(body),
        LoweredInstanceKind::Specialized,
        name.to_string(),
        fref,
        inline_cache,
    )
    .map_err(|e| e.or_at(f.name_span))?;
    let tresolve = Instant::now();
    skull_transform::bound::resolve_standalone_bound_calls(
        ws,
        scx,
        scx.interner(),
        &mut mf,
        tree,
        &concrete.plans,
    )
    .map_err(|e| CompilerError::new_with_label(f.name_span, e.message))?;
    times.validation += tresolve.elapsed();
    times.validation_work = true;
    if f.is_coroutine {
        let mut chunk = emit_coroutine_chunk(ws, cg, scx, plan, f, name, &mut mf, times)?;
        attribution.mir_instructions = mf.insts.len();
        attribution.times = chunk.times;
        chunk.attribution = Some(Box::new(attribution));
        return Ok(chunk);
    }
    let opts = skull_transform::body_chain::ChainOptions {
        reserve: &|name| {
            cg.mint_synthetic(name.to_string());
        },
        ..skull_transform::body_chain::ChainOptions::PLAIN
    };
    let (deps, prepared_times, prepared_inline) = prepare_instance_body_with_attribution(
        ws,
        scx,
        fref,
        name,
        &mut mf,
        inline_cache,
        &concrete.plans,
        &opts,
    )
    .map_err(|e| CompilerError::new_with_label(f.name_span, e.message))?;
    times.add(&prepared_times);
    let mut prepared_inline = prepared_inline;
    prepared_inline.specialized_splices = prepared_inline.accepted_splices;
    prepared_inline.census.ordinary_keys.push(body.clone());
    attribution.inline.add(prepared_inline);
    finish_body_chunk(
        ws,
        cg,
        scx,
        f,
        fref,
        name,
        LoweredInstanceKind::Specialized,
        mf,
        deps,
        times,
        attribution,
    )
}

pub(super) fn order_specialized_callees<'c>(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    pending: &mut Vec<&'c skull_mir::bound::SpecDemand>,
    cached: &'c FxHashMap<Instance, Result<Chunk, CompilerError>>,
    live: &'c FxHashMap<Instance, Chunk>,
    rejected: &[(String, CompilerError)],
    order: &mut DefinitionOrder<'c>,
    attribution: &mut AttributionAccumulator,
    times: &mut PhaseTimes,
) -> (Vec<CompilerError>, Vec<Instance>) {
    let mut done: FxHashSet<&Instance> = FxHashSet::default();
    let mut moved = Vec::new();
    let mut errs: Vec<(String, CompilerError)> = rejected.to_vec();
    let rejected_names: FxHashSet<&str> = rejected.iter().map(|(n, _)| n.as_str()).collect();
    while let Some(demand) = pending.pop() {
        let body = &demand.inst;
        if !done.insert(body) {
            continue;
        }
        let chunk = live
            .get(body)
            .or_else(|| cached.get(body).and_then(|c| c.as_ref().ok()));
        let Some(chunk) = chunk else {
            let name = body.symbol(ws, scx.interner());
            if !rejected_names.contains(name.as_str()) {
                let f = &ws.module(body.func.module).functions[body.func.func];
                errs.push((
                    name.clone(),
                    CompilerError::new_with_label(
                        f.name_span,
                        format!("internal: specialized `{name}` is emitted but no liveness walk reached it"),
                    ),
                ));
            }
            continue;
        };
        order.push_chunk(chunk);
        if let Some(unit) = &chunk.attribution {
            attribution.add_body(unit, Some(&chunk.lua));
        }
        times.add(&chunk.times);
        pending.extend(chunk.specs.iter());
        if !cached.contains_key(body) && live.contains_key(body) {
            moved.push(body.clone());
        }
    }
    errs.sort_by(|(a, _), (b, _)| a.cmp(b));
    (errs.into_iter().map(|(_, e)| e).collect(), moved)
}

/// The transform sequence an instance body is emitted through.
pub fn prepare_instance_body_with_plans(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    fref: FuncRef,
    name: &str,
    mf: &mut skull_mir::mir::MirFunc,
    cache: &mut skull_transform::InlineCache,
    concrete_plans: &skull_mir::concrete::ConcretePlans,
) -> Result<FxHashSet<FuncRef>, CompilerError> {
    let opts = skull_transform::body_chain::ChainOptions::PLAIN;
    prepare_instance_body_with_attribution(ws, scx, fref, name, mf, cache, concrete_plans, &opts)
        .map(|(spliced, _, _)| spliced)
}

fn prepare_instance_body_with_attribution(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    fref: FuncRef,
    name: &str,
    mf: &mut skull_mir::mir::MirFunc,
    cache: &mut skull_transform::InlineCache,
    concrete_plans: &skull_mir::concrete::ConcretePlans,
    opts: &skull_transform::body_chain::ChainOptions<'_>,
) -> Result<(FxHashSet<FuncRef>, PhaseTimes, InlineWork), CompilerError> {
    let mut times = PhaseTimes::default();
    let mut work = InlineWork::default();
    let inliner = skull_transform::body_chain::Inliner {
        scx,
        self_ref: fref,
        cache,
        plans: concrete_plans,
    };
    let spliced = skull_transform::body_chain::prepare_body(
        ws,
        inliner,
        mf,
        name,
        opts,
        &mut ChainStats::new(&mut times, &mut work),
    )
    .map_err(|e| e.error)?;
    Ok((spliced, times, work))
}
