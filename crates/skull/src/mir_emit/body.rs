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

/// Coroutines never inline, so a combinator chain leaves a residual call that
/// a specialized standalone body must serve. The constructor and `__step` are
/// not the declaration's own body, so they bind no callees.
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
    // No declaration edges: instantiation is the graph's fact. `deps` holds only
    // what codegen knows, the bodies this chunk's text contains.
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
        attribution: None,
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
    instance: &Instance,
    specialized_name: Option<&str>,
    inline_cache: &mut skull_transform::InlineCache,
) -> Result<Chunk, CompilerError> {
    let fref = instance.func;
    let f = &ws.module(fref.module).functions[fref.func];
    let specialized = specialized_name.is_some();
    let plan = concrete.plan_for(instance).ok_or_else(|| {
        let name = specialized_name.unwrap_or(f.name.as_str());
        CompilerError::new_with_label(f.name_span, format!("no concrete plan for `{name}`"))
    })?;
    if specialized {
        skull_mir::lower::validate_concrete_bound_plan(f, plan)?;
    }
    let started = Instant::now();
    let lowered = match inline_cache.canonical_body(instance) {
        Some(body) => Ok((*body).clone()),
        None => skull_mir::lower::lower_instance_with_plans(ws, scx, plan, &concrete.plans),
    };
    let mut mf = if specialized {
        lowered.map_err(|e| e.or_at(f.name_span))
    } else {
        lowered
    }?;
    let mut times = PhaseTimes {
        lower: started.elapsed(),
        lower_work: !mf.insts.is_empty(),
        ..PhaseTimes::default()
    };
    let mut attribution = UnitAttribution {
        owner: specialized_name.map_or_else(|| inline_owner_key(fref), str::to_owned),
        examined: true,
        hir_nodes: hir_nodes_for_owner(ws, fref.module, BodyOwner::Function(fref.func)),
        ..UnitAttribution::default()
    };
    if specialized {
        let started = Instant::now();
        skull_transform::bound::resolve_standalone_bound_calls(
            ws,
            scx,
            &mut mf,
            &plan.bound_tree,
            &concrete.plans,
        )
        .map_err(|e| CompilerError::new_with_label(f.name_span, e.message))?;
        times.validation += started.elapsed();
        times.validation_work = true;
    }
    let declared_name;
    let name: &str = match specialized_name {
        Some(name) => name,
        None => {
            declared_name = mangle_fn(ws, f);
            &declared_name
        }
    };
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
        // A hoisted shared body can need a splice to reach a specialized ABI.
        // After hoisting, that splice no longer adds LuaJIT upvalues.
        inline_hoisted: !specialized,
        ..skull_transform::body_chain::ChainOptions::PLAIN
    };
    let inliner = skull_transform::body_chain::Inliner {
        scx,
        self_ref: fref,
        cache: inline_cache,
        plans: &concrete.plans,
    };
    let deps = skull_transform::body_chain::prepare_body(
        ws,
        inliner,
        &mut mf,
        name,
        &opts,
        &mut ChainStats::new(&mut times, &mut attribution.inline),
    )
    .map_err(|e| {
        if specialized || e.pass == skull_transform::body_chain::BodyPass::HoistedInline {
            CompilerError::new_with_label(f.name_span, e.error.message)
        } else {
            e.error
        }
    })?;
    finish_body_chunk(
        ws,
        cg,
        scx,
        f,
        fref,
        name,
        !specialized,
        mf,
        deps,
        times,
        attribution,
    )
}

fn finish_body_chunk(
    ws: &Workspace,
    cg: &CodegenCtx,
    scx: &skull_mir::lower::ScalarCtx,
    f: &bone_semantic::hir::Function,
    self_ref: FuncRef,
    name: &str,
    ordinary: bool,
    mut mf: MirFunc,
    mut deps: FxHashSet<FuncRef>,
    mut times: PhaseTimes,
    mut attribution: UnitAttribution,
) -> Result<Chunk, CompilerError> {
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
    // A specialization's residual callees count as embedded, so any edit to one
    // rebuilds it.
    let embedded_deps = ordinary.then(|| deps.clone());
    collect_callee_frefs(&mf, &mut deps);
    let mut code_artifact =
        skull_mir::code_identity::CodeArtifact::from_mir_tree_with_volatile_vars(
            &mf,
            &ws.comptime_inputs.volatile_vars,
        );
    code_artifact.set_body_names(&mf, name);
    // Global-init ordering is stated over declarations, not specializations.
    let (direct_globals, direct_calls) = if ordinary {
        direct_refs(ws, cg, &mf)
    } else {
        (Vec::new(), Vec::new())
    };
    times.validation += tv.elapsed();
    times.validation_work = true;
    let bind = skull_codegen_lua::callee_binding_plan(ws, cg, scx.interner(), name, self_ref, &mf);
    let mut allocation_bodies = Vec::new();
    let mut emitter = BodyEmitter {
        ws,
        cg,
        scx,
        allocation_bodies: &mut allocation_bodies,
        times: &mut times,
    };
    let mut lua = emitter.emit(
        name,
        f.name.as_str(),
        &mut mf,
        &[],
        bind.as_ref(),
        noalloc::marked(f).then_some(f),
    )?;
    // Read before the nested fns are emitted, which detaches their children.
    let mut literal_globals = skull_mir::reach::global_ref_names(&mf);
    let mut abi_deps = FxHashMap::default();
    collect_abi_dependencies(&mf, &mut abi_deps);
    // Nested-fn statics fold into this chunk, or hot reload would not see them.
    let mut statics = mf.statics.clone();
    emitter.emit_nested(
        &mut mf.nested_fns,
        &mf.statics,
        Some(&mut NestedChunk {
            deps: &mut deps,
            statics: &mut statics,
        }),
        &mut lua,
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

pub(crate) struct BodyEmitter<'a> {
    pub(crate) ws: &'a Workspace,
    pub(crate) cg: &'a CodegenCtx,
    pub(crate) scx: &'a skull_mir::lower::ScalarCtx,
    pub(crate) allocation_bodies: &'a mut Vec<crate::allocation_report::AllocationBody>,
    pub(crate) times: &'a mut PhaseTimes,
}

pub(crate) struct NestedChunk<'a> {
    pub(crate) deps: &'a mut FxHashSet<FuncRef>,
    pub(crate) statics: &'a mut Vec<skull_mir::mir::StaticCell>,
}

impl BodyEmitter<'_> {
    pub(super) fn emit(
        &mut self,
        name: &str,
        source: &str,
        mf: &mut MirFunc,
        inherited_statics: &[skull_mir::mir::StaticCell],
        bound: Option<&skull_codegen_lua::CalleeBindingPlan>,
        contract: Option<&bone_semantic::hir::Function>,
    ) -> Result<String, CompilerError> {
        skull_codegen_lua::regalloc_time_reset();
        let te = Instant::now();
        let interner = self.scx.interner();
        let allocation = (contract.is_some() || crate::allocation_report::enabled())
            .then(crate::allocation_report::activation_enabled);
        let emission = skull_codegen_lua::with_constants_baked(self.cg, mf, |mf| {
            skull_codegen_lua::emit_lua_definition(
                self.ws,
                self.cg,
                interner,
                name,
                mf,
                None,
                inherited_statics,
                bound,
                allocation,
            )
        });
        if let Some(contract) = contract {
            noalloc::validate(self.ws, contract, mf, &emission)?;
        }
        if let Some(body) = crate::allocation_report::collect_emitted_body(
            self.ws,
            interner,
            name,
            source,
            mf,
            emission.allocation_sites,
            &emission.call_sites,
        ) {
            self.allocation_bodies.push(body);
        }
        let emit = te.elapsed();
        self.times.emit += emit;
        self.times.emit_work |= emit > Duration::ZERO;
        let regalloc = skull_codegen_lua::regalloc_time_elapsed().min(emit);
        self.times.regalloc += regalloc;
        self.times.regalloc_work |= regalloc > Duration::ZERO;
        Ok(emission.lua)
    }

    pub(crate) fn emit_nested(
        &mut self,
        nested: &mut [MirFunc],
        inherited_statics: &[skull_mir::mir::StaticCell],
        mut chunk: Option<&mut NestedChunk<'_>>,
        lua: &mut String,
    ) -> Result<(), CompilerError> {
        for nf in nested {
            let cleanup_before = mir_live_instructions(nf);
            let tc = Instant::now();
            skull_transform::cleanup_with_nobox(self.ws, nf)?;
            self.times.cleanup += tc.elapsed();
            self.times.cleanup_work |= mir_live_instructions(nf) != cleanup_before;
            if let Some(chunk) = chunk.as_deref_mut() {
                let counted_before = nf.blocks.len();
                let tl = Instant::now();
                skull_transform::recover_counted_loops(self.ws, nf);
                self.times.counted_loop += tl.elapsed();
                self.times.counted_loop_work |= nf.blocks.len() > counted_before;
                let tb = Instant::now();
                self.times.bounds_elided += skull_transform::elide_bounds_checks_for_target(
                    Some(self.ws),
                    nf,
                    bone_core::hir::FnBackend::Lua,
                );
                self.times.bounds += tb.elapsed();
                collect_callee_frefs(nf, chunk.deps);
                // A flat variant's text is another declaration's body.
                chunk.deps.extend(nf.flat_variant_source);
                chunk.statics.extend(nf.statics.iter().cloned());
            }
            let name = nf.name.as_str();
            lua.push_str(&self.emit(name, name, nf, inherited_statics, None, None)?);
            if chunk.is_some() {
                let mut children = std::mem::take(&mut nf.nested_fns);
                let mut next_inherited = inherited_statics.to_vec();
                next_inherited.extend(nf.statics.iter().cloned());
                self.emit_nested(&mut children, &next_inherited, chunk.as_deref_mut(), lua)?;
            } else {
                // The unit still needs its whole MIR tree to record code identities.
                self.emit_nested(&mut nf.nested_fns, inherited_statics, None, lua)?;
            }
        }
        Ok(())
    }
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
    let mut work = InlineAttribution::default();
    let mut stats = ChainStats::new(times, &mut work);
    skull_transform::body_chain::convert_body(ws, mf, name, &opts, &mut stats).map_err(|e| e.error)
}

pub fn is_cached_body(f: &bone_semantic::hir::Function) -> bool {
    !f.is_extern && f.inline_body.is_none() && !f.has_iterator_bounds() && !is_generic(f)
}

/// An `Iterator`-bounded declaration has no shared chunk: each instantiation
/// is emitted standalone with its bound calls resolved.
pub(crate) fn is_cached_emitted_body(f: &bone_semantic::hir::Function, body: &Instance) -> bool {
    if body.args.is_empty() {
        return is_cached_body(f);
    }
    !f.is_extern && f.inline_body.is_none()
}

pub fn is_generic(f: &bone_semantic::hir::Function) -> bool {
    !f.generics.is_empty()
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
    // LIFO demand order preserves the ordering of specialized definitions.
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
            attribution.add_unit(unit);
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
