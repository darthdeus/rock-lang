//! The per-body pipeline: lower, inline, clean up, validate and print one body.

use super::*;

#[derive(Clone, Copy)]
pub(super) enum BodyMutation {
    None,
    #[cfg(test)]
    RemoveCalleeOwner {
        caller: FuncRef,
        callee: FuncRef,
    },
    #[cfg(test)]
    ClearCalleeOwnerKey {
        caller: FuncRef,
        callee: FuncRef,
    },
    #[cfg(test)]
    ClearBoundPlan {
        body: FuncRef,
    },
    #[cfg(test)]
    ClearCoroutineResultPlan {
        body: FuncRef,
    },
    #[cfg(test)]
    ClearIteratorWitnessInstance {
        body: FuncRef,
    },
    #[cfg(test)]
    SkipAbiPlanPublish,
    #[cfg(test)]
    ConcreteResultOrder(skull_graph::concrete_body::BuildResultOrder),
    #[cfg(all(test, not(feature = "sequential")))]
    SetupLaneSchedule(SetupLaneSchedule),
}

/// The statically-resolved callees referenced by a (post-inline) MIR function —
/// the callees still CALLED after inlining (their bodies could flip eligibility
/// on a future edit). Unioned with the driver's spliced set to form the chunk's
/// codegen deps.
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

fn collect_abi_dependencies(mf: &MirFunc, out: &mut FxHashSet<FuncRef>) {
    out.extend(
        mf.call_abi_plans
            .values()
            .filter(|plan| plan.body_dependent_parameters())
            .map(|plan| plan.subject.func),
    );
    for nested in &mf.nested_fns {
        collect_abi_dependencies(nested, out);
    }
}

fn emit_function_with_allocation_report(
    ws: &Workspace,
    cg: &CodegenCtx,
    interner: &skull_mir::monoty::MonoInterner,
    self_ref: FuncRef,
    mf: &MirFunc,
    bound: Option<&skull_codegen_lua::CalleeBindingPlan>,
) -> skull_codegen_lua::LuaEmission {
    if crate::allocation_report::enabled() {
        skull_codegen_lua::emit_function_with_allocation_sites(
            ws,
            cg,
            interner,
            self_ref,
            mf,
            bound,
            crate::allocation_report::activation_enabled(),
        )
    } else {
        skull_codegen_lua::LuaEmission {
            lua: emit_function(ws, cg, interner, self_ref, mf, bound),
            allocation_sites: Vec::new(),
        }
    }
}

pub(super) fn emit_named_with_allocation_report(
    ws: &Workspace,
    cg: &CodegenCtx,
    interner: &skull_mir::monoty::MonoInterner,
    name: &str,
    mf: &MirFunc,
    inherited_statics: &[skull_mir::mir::StaticCell],
    bound: Option<&skull_codegen_lua::CalleeBindingPlan>,
) -> skull_codegen_lua::LuaEmission {
    if crate::allocation_report::enabled() {
        skull_codegen_lua::emit_named_framed_with_allocation_sites(
            ws,
            cg,
            interner,
            name,
            mf,
            None,
            inherited_statics,
            bound,
            crate::allocation_report::activation_enabled(),
        )
    } else {
        skull_codegen_lua::LuaEmission {
            lua: skull_codegen_lua::emit_named_with_statics(
                ws,
                cg,
                interner,
                name,
                mf,
                inherited_statics,
                bound,
            ),
            allocation_sites: Vec::new(),
        }
    }
}

/// **The one Lua emission of a `co fn`**, shared by the declaration's own body
/// and by each specialized instantiation of a generic one.
///
/// Coroutines are not inlined into and do not inline: the call constructs a
/// coroutine object rather than running the body, which is why specialization
/// here is delivered by a standalone definition instead of a splice. That is the
/// whole difference from an ordinary specialization, and it is why this is not
/// [`prepare_instance_body_with_plans`] -- there is no inline pass, no `convert_closures`
/// (the step's lambdas convert framed, inside `emit_coroutine`) and no FORL
/// recovery (codegen's relooper does it).
///
/// The concrete body plan decides the instance symbol and yield channel.
fn emit_coroutine_chunk(
    ws: &Workspace,
    cg: &CodegenCtx,
    scx: &skull_mir::lower::ScalarCtx,
    ctx: &skull_mir::bound::BoundCtx,
    concrete_plan: &skull_mir::concrete::ConcreteBodyPlan,
    f: &bone_semantic::hir::Function,
    mf: &mut MirFunc,
    mut times: PhaseTimes,
) -> Result<Chunk, CompilerError> {
    let cleanup_before = mir_live_instructions(mf);
    let tc = Instant::now();
    skull_transform::cleanup_with_nobox(ws, mf)?;
    times.cleanup = tc.elapsed();
    times.cleanup_work = mir_live_instructions(mf) != cleanup_before;
    // Coroutines never inline, so a combinator chain here leaves a residual call
    // -- fine when the lowerer specialized it (a standalone body serves it), a
    // rejection when it did not.
    let tv = Instant::now();
    skull_transform::validate_no_residual_bound_calls(ws, f, mf)?;
    let mut specs = Vec::new();
    validate_emitted_body(ws, ctx, mf, &mut specs).map_err(|r| r.into_error(f.name_span, ""))?;
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
    // No declaration edge here: "this instance instantiates that declaration" is
    // the GRAPH's fact, and `update_chunk_cache` reads it through
    // `instances_of`. `deps` carries only what codegen alone knows -- which
    // bodies this chunk's TEXT contains.
    let mut abi_deps = FxHashSet::default();
    collect_abi_dependencies(mf, &mut abi_deps);
    deps.extend(abi_deps.iter().copied());
    Ok(Chunk {
        abi_deps,
        lua,
        literal_globals: skull_mir::reach::global_ref_names(mf),
        allocation_bodies,
        embedded_deps: deps.clone(),
        deps,
        direct_globals: Vec::new(),
        direct_calls: Vec::new(),
        times,
        attribution: UnitAttribution::default(),
        statics,
        comptime_reads: mf.comptime_reads.clone(),
        calls: skull_mir::reach::call_targets(mf),
        specs,
        code_artifact,
        // A coroutine's emitted text is a constructor plus a `__step`, neither
        // of which is the declaration's own body, so neither binds callees.
        binds: Vec::new(),
    })
}

/// Emit one ordinary or coroutine function body to a cacheable [`Chunk`]. Extern
/// and `inline(lua)` functions are NOT handled here — they are cheap and
/// regenerated in [`assemble`]. Captures per-stage emit time into the chunk so the
/// parallel chunk fan-out's per-unit elapsed time still rolls up into
/// `backend-stats`.
pub(super) fn emit_body_chunk(
    ws: &Workspace,
    cg: &CodegenCtx,
    scx: &skull_mir::lower::ScalarCtx,
    concrete: &crate::concrete_body::ConcreteBodies,
    self_ref: FuncRef,
    f: &bone_semantic::hir::Function,
    inline_cache: &mut skull_transform::InlineCache,
) -> Result<Chunk, CompilerError> {
    emit_body_chunk_with_mutation(
        ws,
        cg,
        scx,
        concrete,
        self_ref,
        f,
        inline_cache,
        BodyMutation::None,
    )
}

pub(super) fn emit_body_chunk_with_mutation(
    ws: &Workspace,
    cg: &CodegenCtx,
    scx: &skull_mir::lower::ScalarCtx,
    concrete: &crate::concrete_body::ConcreteBodies,
    self_ref: FuncRef,
    f: &bone_semantic::hir::Function,
    inline_cache: &mut skull_transform::InlineCache,
    mutation: BodyMutation,
) -> Result<Chunk, CompilerError> {
    let mut times = PhaseTimes::default();
    let mut attribution = UnitAttribution {
        owner: inline_owner_key(self_ref),
        ..UnitAttribution::default()
    };
    let tl = Instant::now();
    let plan = concrete.plan_for(&shared(self_ref)).ok_or_else(|| {
        CompilerError::new_with_label(f.name_span, format!("no concrete plan for `{}`", f.name))
    })?;
    let ordinary_instance = plan.function_instance().cloned();
    let canonical_body = ordinary_instance
        .as_ref()
        .and_then(|instance| inline_cache.canonical_body(instance));
    let canonical_reused = canonical_body.is_some();
    let mut mf = match canonical_body {
        Some(body) => (*body).clone(),
        None => skull_mir::lower::lower_function_with_plans(ws, scx, plan, &concrete.plans)?,
    };
    times.lower = tl.elapsed();
    times.lower_work = mf.insts.len() > 0;
    attribution.examined = true;
    attribution.hir_nodes =
        hir_nodes_for_owner(ws, self_ref.module, BodyOwner::Function(self_ref.func));
    attribution.mir_instructions = mf.insts.len();
    attribution.lower_facts = mf.lower_attribution.clone();
    attribution.consumer_instance = ordinary_instance
        .clone()
        .map(|instance| (instance, LoweredInstanceKind::Ordinary));
    attribution.canonical_reused = canonical_reused;
    if !canonical_reused {
        attribution.lowered_instance = ordinary_instance
            .clone()
            .map(|instance| (instance, LoweredInstanceKind::Ordinary));
    }
    match mutation {
        BodyMutation::None => {}
        #[cfg(test)]
        BodyMutation::RemoveCalleeOwner { caller, callee }
        | BodyMutation::ClearCalleeOwnerKey { caller, callee }
            if caller == self_ref =>
        {
            let (call, owner) = mf
                .callee_instances
                .iter()
                .find(|(_, owner)| owner.func == callee)
                .map(|(call, owner)| (*call, owner.clone()))
                .expect("the mutation fixture must lower a concrete generic call");
            assert!(
                !owner.args.is_empty(),
                "the mutation fixture must have a complete concrete owner"
            );
            assert!(
                mf.spec_bindings.contains_key(&call),
                "the mutation must retain the specialized delivery record"
            );
            assert!(
                mf.call_abi_plans.contains_key(&call),
                "the mutation must retain the lowered ABI record"
            );
            match mutation {
                BodyMutation::RemoveCalleeOwner { .. } => {
                    mf.callee_instances.remove(&call);
                }
                BodyMutation::ClearCalleeOwnerKey { .. } => {
                    mf.callee_instances
                        .get_mut(&call)
                        .expect("owner disappeared before key mutation")
                        .args
                        .clear();
                }
                BodyMutation::None => unreachable!(),
                BodyMutation::ClearBoundPlan { .. } => unreachable!(),
                BodyMutation::ClearCoroutineResultPlan { .. } => unreachable!(),
                BodyMutation::ClearIteratorWitnessInstance { .. }
                | BodyMutation::SkipAbiPlanPublish => {
                    unreachable!()
                }
                BodyMutation::ConcreteResultOrder(_) => unreachable!(),
                #[cfg(all(test, not(feature = "sequential")))]
                BodyMutation::SetupLaneSchedule(_) => unreachable!(),
            }
        }
        #[cfg(test)]
        _ => {}
    }
    if f.is_coroutine {
        let owner = BodyOwner::Function(self_ref.func);
        let ctx = skull_mir::bound::concrete_body_ctx(
            ws,
            self_ref.module,
            owner,
            &plan.bound_tree,
            scx.interner(),
        );
        let mut chunk = emit_coroutine_chunk(ws, cg, scx, &ctx, plan, f, &mut mf, times)?;
        attribution.mir_instructions = mf.insts.len();
        attribution.times = chunk.times;
        chunk.attribution = attribution;
        return Ok(chunk);
    }
    // Inline eligible direct calls before cleanup so a hot loop traces with no
    // per-call frame.
    let before_inline = (mf.insts.len(), mf.blocks.len(), mf.cells.len());
    let ti = Instant::now();
    let (spliced, splices, scan_stats) = skull_transform::inline_functions_with_plans(
        ws,
        scx,
        self_ref,
        &mut mf,
        inline_cache,
        &concrete.plans,
    )?;
    let mut scan_stats = scan_stats;
    if let Some(instance) = ordinary_instance {
        scan_stats.ordinary_keys.push(instance);
    }
    times.inline = ti.elapsed();
    attribution.inline.add_records(&splices);
    attribution.inline.ordinary_splices += splices.len();
    attribution.inline.add_scan(scan_stats);
    times.inline_work = !splices.is_empty();
    attribution.inline.add_allocations(
        before_inline,
        (mf.insts.len(), mf.blocks.len(), mf.cells.len()),
    );
    let mut deps: FxHashSet<FuncRef> = spliced.into_iter().collect();
    let cleanup_before = mir_live_instructions(&mf);
    let tc = Instant::now();
    // drop dead values + dead-cell stores
    skull_transform::cleanup_with_nobox_census(
        ws,
        &mut mf,
        before_inline.0,
        inline_cache
            .census_enabled()
            .then_some(&mut attribution.inline.census.load_store),
    )?;
    times.cleanup = tc.elapsed();
    let cleanup_after = mir_live_instructions(&mf);
    attribution.inline.cleanup_live_before += cleanup_before;
    attribution.inline.cleanup_live_after += cleanup_after;
    times.cleanup_work = cleanup_after != cleanup_before;
    // Shared closure conversion:
    // every `Inst::Lambda` in this ORDINARY fn becomes a `MakeEnv` record / bare
    // `Global` + a hoisted sibling in `nested_fns`. Runs AFTER inline+cleanup,
    // BEFORE FORL recovery (matching the JS pipeline) so a loop lifted into a
    // lambda body is FORL-recovered in the split fn. Coroutines skip this (their
    // `Inst::Lambda`s convert at codegen via `FrameCtx`/`HoistPlan` -- S3).
    let name = mangle_fn(ws, f);
    let pre_nested = mf.nested_fns.len();
    let tclosure = Instant::now();
    skull_transform::convert_closures(&mut mf, &name);
    times.closure = tclosure.elapsed();
    times.closure_work = mf.nested_fns.len() > pre_nested;
    // Route each pass-hoisted `<name>__lambda<k>` through the synthetic-name
    // collision guard now that they enter the Lua global namespace (a user fn
    // named `foo__lambda0` must reject, not silently clobber). The pass appends
    // its lambda bodies after any named nested `fn`s, so guard only the new tail.
    for nf in &mf.nested_fns[pre_nested..] {
        cg.mint_synthetic(nf.name.to_string());
    }
    convert_named_nested_closures(cg, &mut mf.nested_fns[..pre_nested]);
    let counted_before = mf.blocks.len();
    let tcounted = Instant::now();
    skull_transform::recover_counted_loops(ws, &mut mf); // desugared iter loops -> FORL shape
    times.counted_loop = tcounted.elapsed();
    times.counted_loop_work = mf.blocks.len() > counted_before;
    // Hoisted bodies inline in their own right, and only HERE. The inliner scans a
    // caller's top-level blocks and never a closure sub-CFG, because a cell used in
    // a lambda body is an upvalue and splicing there piles onto LuaJIT's 60-upvalue
    // cap. Once `convert_closures` has split those bodies out they are ordinary
    // standalone functions with their own top-level CFG, so that reason is gone --
    // running the pass before the split would hit it, running it after does not.
    //
    // Correctness, not just an optimization: a call the lowerer gave the
    // specialized scalar ABI has no serviceable shared body, and for a
    // column-stored element the shared `next` reads a `_data` that does not exist.
    let mut hoisted_work = InlineWork::default();
    inline_hoisted_bodies(
        ws,
        scx,
        self_ref,
        &mut mf.nested_fns,
        inline_cache,
        &concrete.plans,
        &mut deps,
        &mut times,
        &mut hoisted_work,
    )
    .map_err(|msg| CompilerError::new_with_label(f.name_span, msg))?;
    times.hoisted_inline_work = hoisted_work.accepted_splices > 0;
    attribution.inline.add(hoisted_work);
    let tv = Instant::now();
    skull_transform::validate_no_residual_bound_calls(ws, f, &mf)?;
    let mut specs = Vec::new();
    let owner = BodyOwner::Function(self_ref.func);
    let ctx = skull_mir::bound::concrete_body_ctx(
        ws,
        self_ref.module,
        owner,
        &plan.bound_tree,
        scx.interner(),
    );
    validate_emitted_body(ws, &ctx, &mf, &mut specs).map_err(|r| r.into_error(f.name_span, ""))?;
    let embedded_deps = deps.clone();
    collect_callee_frefs(&mf, &mut deps);
    let mut code_artifact =
        skull_mir::code_identity::CodeArtifact::from_mir_tree_with_volatile_vars(
            &mf,
            &ws.comptime_inputs.volatile_vars,
        );
    code_artifact.set_body_names(&mf, &mangle_fn(ws, f));
    let (direct_globals, direct_calls) = direct_refs(ws, &mf);
    times.validation = tv.elapsed();
    times.validation_work = true;
    skull_codegen_lua::regalloc_time_reset();
    let te = Instant::now();
    let bind = skull_codegen_lua::callee_binding_plan(ws, cg, scx.interner(), &name, self_ref, &mf);
    let emission =
        emit_function_with_allocation_report(ws, cg, scx.interner(), self_ref, &mf, bind.as_ref());
    let mut lua = emission.lua;
    let mut allocation_bodies = Vec::new();
    if let Some(body) = crate::allocation_report::collect_emitted_body(
        ws,
        scx.interner(),
        &name,
        f.name.as_str(),
        &mf,
        emission.allocation_sites,
    ) {
        allocation_bodies.push(body);
    }
    times.emit = te.elapsed();
    times.emit_work = times.emit > Duration::ZERO;
    times.regalloc = skull_codegen_lua::regalloc_time_elapsed().min(times.emit);
    times.regalloc_work = times.regalloc > Duration::ZERO;
    // Nested `fn`s are hoisted to sibling top-level Lua fns in the same chunk
    // (their `__local_*` names are already final -- emit_named, no mangling). Their
    // `static` cells fold into THIS chunk's `statics` so the hot-reload preamble +
    // overlay reset see them -- a nested-fn static is otherwise invisible to both.
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
    let calls = skull_mir::reach::call_targets(&mf);
    let mut abi_deps = FxHashSet::default();
    collect_abi_dependencies(&mf, &mut abi_deps);
    deps.extend(abi_deps.iter().copied());
    Ok(Chunk {
        abi_deps,
        lua,
        literal_globals: skull_mir::reach::global_ref_names(&mf),
        allocation_bodies,
        deps,
        embedded_deps,
        direct_globals,
        direct_calls,
        times,
        statics,
        calls,
        specs,
        code_artifact,
        attribution,
        comptime_reads: mf.comptime_reads.clone(),
        binds: bind.into_iter().collect(),
    })
}

/// Emit each hoisted nested `fn` (and its own nested fns, depth-first) as a
/// top-level Lua function appended to the enclosing chunk. Runs the standard
/// cleanup pipeline per fn, folds their callee refs into the chunk's deps (so an
/// edit to a callee still invalidates the enclosing chunk) and their `static`
/// cells into the chunk's `statics` (so hot-reload can reset them).
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
        collect_callee_frefs(nf, deps);
        // A flat variant's text is another declaration's body.
        if let Some(source) = nf.flat_variant_source {
            deps.insert(source);
        }
        statics.extend(nf.statics.iter().cloned());
        skull_codegen_lua::regalloc_time_reset();
        let te = Instant::now();
        // A hoisted nested fn has no declaration of its own, so no call-graph
        // component and no callee binding.
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

pub(crate) fn convert_named_nested_closures(cg: &CodegenCtx, nested: &mut [MirFunc]) {
    for nf in nested {
        let pre_nested = nf.nested_fns.len();
        let name = nf.name;
        skull_transform::convert_closures(nf, name.as_str());
        for child in &nf.nested_fns[pre_nested..] {
            cg.mint_synthetic(child.name.to_string());
        }
        convert_named_nested_closures(cg, &mut nf.nested_fns[..pre_nested]);
    }
}

/// Emit the flat list of pass-produced hoisted lambda bodies (`convert_closures`
/// appends one `MirFunc` per live lambda, nested ones included) as sibling
/// top-level fns appended to `out`. Used by the decorator/global-init units, which
/// -- unlike [`emit_nested_fns`] -- fold no deps/statics (their `UnitOut` tracks
/// neither) and run no `recover_counted_loops` (their loops recover at codegen,
/// same as the co-fn step).
/// The `static` cells of a hoisted-lambda tree, for the callers that emit it
/// through [`emit_pass_nested_fns`] and so do not fold them along the way.
fn collect_nested_statics(nested: &[MirFunc], out: &mut Vec<skull_mir::mir::StaticCell>) {
    for nf in nested {
        out.extend(nf.statics.iter().cloned());
        collect_nested_statics(&nf.nested_fns, out);
    }
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
            out.push_str(&skull_codegen_lua::emit_named_with_statics(
                ws,
                cg,
                scx.interner(),
                nf.name.as_str(),
                nf,
                inherited_statics,
                None,
            ));
        }
        let emit = te.elapsed();
        times.emit += emit;
        times.emit_work |= emit > Duration::ZERO;
        let regalloc = skull_codegen_lua::regalloc_time_elapsed().min(emit);
        times.regalloc += regalloc;
        times.regalloc_work |= regalloc > Duration::ZERO;
    }
    Ok(())
}

/// Whether a function body is cached as a [`Chunk`] (ordinary or coroutine) vs
/// regenerated inline (extern / `inline(lua)`). An `Iterator`-bounded fn is
/// NEITHER: it is emitted STANDALONE per instantiation, resolving its
/// placeholder bound-calls from the tree that specialization demand carries, so
/// there is no declaration-keyed chunk to cache.
pub fn is_cached_body(f: &bone_semantic::hir::Function) -> bool {
    !f.is_extern && f.inline_body.is_none() && !fn_has_bounds(f) && !is_generic(f)
}

/// Whether THIS EMITTED BODY is a cached [`Chunk`] -- the same question
/// [`is_cached_body`] answers for a declaration, asked of the body actually
/// being emitted.
///
/// A SPECIALIZATION always is. It is a concrete instance with a body of its own,
/// so the two reasons a declaration is not cached under E1 -- it is generic, or
/// it is `Iterator`-bounded -- are precisely the reasons the specialization
/// EXISTS. Only opacity survives the distinction, and an opaque declaration has
/// no specializations to begin with.
///
/// Asking the declaration's predicate of an emitted body is what deleted a
/// generic's specialized chunk on every edit: `is_cached_body` said false, the
/// caller took its "regenerated, drop the chunk" branch, and the overlay then
/// found nothing to redefine.
pub fn is_cached_emitted_body(f: &bone_semantic::hir::Function, body: &Instance) -> bool {
    if body.args.is_empty() {
        return is_cached_body(f);
    }
    !f.is_extern && f.inline_body.is_none()
}

/// Whether the fn declares generics, and so has NO declaration-owned body.
///
/// This is the E1 cut stated as a predicate. A generic declaration is not a
/// definition that happens to be shared across its instantiations -- it is not a
/// definition at all, the way an `extern` or an `inline(lua)` body is not one.
/// Every emitted body comes from an instance, and a declaration nothing
/// instantiates emits nothing.
pub fn is_generic(f: &bone_semantic::hir::Function) -> bool {
    !f.generics.is_empty()
}

/// Whether the fn declares any `Iterator` bound on its generics.
pub(super) fn fn_has_bounds(f: &bone_semantic::hir::Function) -> bool {
    f.generic_bounds.iter().any(Option::is_some)
}

/// Emit one top-level Lua function per distinct `(callee, instantiation)` a body
/// asked for, under the name its call sites already target (`spec_fn_name`).
///
/// This is what lets specialization reach contexts the inliner cannot enter. A
/// splice needs a caller whose CFG the inliner will walk; a standalone definition
/// needs nothing, so a coroutine's `__step` and a global initializer -- neither of
/// which runs an inline pass -- call the specialized copy exactly as any other
/// body does.
///
/// A worklist, not a loop over the initial set: a specialized body is lowered
/// under its own substitution and can therefore require further specializations
/// that no shared body ever mentions (`Vec<Vec2>.remove_at` calling
/// `__vec_data_set` at `T = Vec2`). `done` keys on the full `Instance`
/// so each is defined exactly once no matter how many bodies requested it.
///
/// A failing specialization does NOT stop the worklist: each instantiation is an
/// independent definition, and bailing at the first one made every census of
/// remaining nobox rejections a floor -- the three members of the column-read
/// class were each found one behind another. The rejections are returned (sorted
/// by specialized name, since the worklist order is a pop order and two
/// instantiations of one generic share its span) and the caller fails the build
/// on all of them; a rejected specialization contributes NO Lua.
/// Build ONE specialized body's chunk -- the same transform sequence and the
/// same rendering the worklist ran inline before it was a chunk.
///
/// `deps` carries the DECLARATION this specializes as well as its splices and
/// residual callees. That edge is what makes editing a generic's body invalidate
/// every specialization of it: nothing else names the connection, since a
/// specialization is not reached through the declaration's own chunk.
///
/// Two fields are deliberately empty, matching what the inline worklist
/// contributed:
/// - `direct_globals` / `direct_calls`: global-init ordering is stated over
///   declarations, and a specialization's ordering facts are its declaration's.
///
/// `statics` is NOT one of them. A specialized body's `static` cells are cells
/// the emitted program uses, so leaving them out would leave them without a
/// reload reset. With emission owned by the
/// instance, every generic body with a `static` is one.
pub(super) fn emit_spec_chunk(
    ws: &Workspace,
    cg: &CodegenCtx,
    scx: &skull_mir::lower::ScalarCtx,
    concrete: &crate::concrete_body::ConcreteBodies,
    body: &Instance,
    name: &str,
    inline_cache: &mut skull_transform::InlineCache,
    specs: &mut Vec<skull_mir::bound::SpecDemand>,
) -> Result<Chunk, CompilerError> {
    let fref = body.func;
    let f = &ws.module(fref.module).functions[fref.func];
    let plan = concrete.plan_for(body).ok_or_else(|| {
        CompilerError::new_with_label(f.name_span, format!("no concrete plan for `{name}`"))
    })?;
    let tree = &plan.bound_tree;
    skull_mir::lower::validate_concrete_bound_plan(f, plan)?;
    let tl = Instant::now();
    let canonical_body = inline_cache.canonical_body(body);
    let canonical_reused = canonical_body.is_some();
    let mut mf = match canonical_body {
        Some(body) => (*body).clone(),
        None => skull_mir::lower::lower_instance_with_plans(ws, scx, plan, &concrete.plans)
            .map_err(|e| CompilerError::new_with_label(f.name_span, e.message))?,
    };
    let mut times = PhaseTimes {
        lower: tl.elapsed(),
        lower_work: !mf.insts.is_empty(),
        ..PhaseTimes::default()
    };
    let mut attribution = UnitAttribution {
        owner: name.to_string(),
        examined: true,
        hir_nodes: hir_nodes_for_owner(ws, fref.module, BodyOwner::Function(fref.func)),
        mir_instructions: mf.insts.len(),
        lower_facts: mf.lower_attribution.clone(),
        consumer_instance: Some((body.clone(), LoweredInstanceKind::Specialized)),
        lowered_instance: (!canonical_reused)
            .then(|| (body.clone(), LoweredInstanceKind::Specialized)),
        canonical_reused,
        ..UnitAttribution::default()
    };
    // BEFORE every transform: an `Iterator`-bounded body's placeholders are what
    // make it unemittable on its own, and the demand that reached here is what
    // resolves them. The splice path does the same substitution at the moment it
    // clones the body into a caller.
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
    let owner = BodyOwner::Function(fref.func);
    let ctx = skull_mir::bound::concrete_body_ctx(ws, fref.module, owner, tree, scx.interner());
    if f.is_coroutine {
        let mut chunk = emit_coroutine_chunk(ws, cg, scx, &ctx, plan, f, &mut mf, times)?;
        attribution.mir_instructions = mf.insts.len();
        attribution.times = chunk.times;
        chunk.attribution = attribution;
        return Ok(chunk);
    }
    let (mut deps, prepared_times, prepared_inline) = prepare_instance_body_with_attribution(
        ws,
        scx,
        fref,
        name,
        &mut mf,
        inline_cache,
        &concrete.plans,
    )
    .map_err(|e| CompilerError::new_with_label(f.name_span, e.message))?;
    times.add(&prepared_times);
    let mut prepared_inline = prepared_inline;
    prepared_inline.specialized_splices = prepared_inline.accepted_splices;
    prepared_inline.census.ordinary_keys.push(body.clone());
    attribution.inline.add(prepared_inline);
    let before = specs.len();
    // Discovered BEFORE the verdict, and handed back through `specs` even when
    // the verdict rejects: the inline worklist pushed them into the live queue
    // first, so a failing build reports over that same closure.
    let tv = Instant::now();
    validate_emitted_body(ws, &ctx, &mf, specs)
        .map_err(|r| r.into_error(f.name_span, &format!("cannot emit specialized `{name}`: ")))?;
    times.validation += tv.elapsed();
    times.validation_work = true;
    let specs = specs[before..].to_vec();
    let mut code_artifact =
        skull_mir::code_identity::CodeArtifact::from_mir_tree_with_volatile_vars(
            &mf,
            &ws.comptime_inputs.volatile_vars,
        );
    code_artifact.set_body_names(&mf, name);
    skull_codegen_lua::regalloc_time_reset();
    let te = Instant::now();
    let bind = skull_codegen_lua::callee_binding_plan(ws, cg, scx.interner(), name, fref, &mf);
    let emission =
        emit_named_with_allocation_report(ws, cg, scx.interner(), name, &mf, &[], bind.as_ref());
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
    let mut statics = mf.statics.clone();
    let literal_globals = skull_mir::reach::global_ref_names(&mf);
    let mut nested_abi_deps = FxHashSet::default();
    collect_abi_dependencies(&mf, &mut nested_abi_deps);
    let mut nested = std::mem::take(&mut mf.nested_fns);
    collect_nested_statics(&nested, &mut statics);
    emit_pass_nested_fns(
        ws,
        cg,
        scx,
        &mut nested,
        &statics,
        Some(&mut allocation_bodies),
        &mut lua,
        &mut times,
    )?;
    collect_callee_frefs(&mf, &mut deps);
    attribution.mir_instructions = mf.insts.len();
    attribution.times = times;
    let mut abi_deps = nested_abi_deps;
    collect_abi_dependencies(&mf, &mut abi_deps);
    deps.extend(abi_deps.iter().copied());
    Ok(Chunk {
        abi_deps,
        lua,
        allocation_bodies,
        embedded_deps: deps.clone(),
        deps,
        literal_globals,
        direct_globals: Vec::new(),
        direct_calls: Vec::new(),
        times,
        attribution,
        statics,
        comptime_reads: mf.comptime_reads.clone(),
        calls: skull_mir::reach::call_targets(&mf),
        specs,
        code_artifact,
        binds: bind.into_iter().collect(),
    })
}

/// Render every specialized body the liveness walk found, in the order the
/// assembled text has always had.
///
/// This ORDERS text; it does not decide or discover. The worklist is still LIFO
/// and still dedupes, because that is what fixes the order, but every body it
/// pops was already reached and built by [`live_bodies`]. A body it cannot find
/// is an invariant failure rather than something to quietly build: two places
/// that can decide a definition into the program is exactly the pair this step
/// removes.
pub(super) fn emit_specialized_callees(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    pending: &mut Vec<skull_mir::bound::SpecDemand>,
    cached: &FxHashMap<Instance, Result<Chunk, CompilerError>>,
    live: &FxHashMap<Instance, Chunk>,
    rejected: &[(String, CompilerError)],
    built: &mut Vec<(Instance, Chunk)>,
    allocation_bodies: &mut Vec<crate::allocation_report::AllocationBody>,
    statics: &mut Vec<skull_mir::mir::StaticCell>,
    out: &mut LuaDefinitionBuffer,
    binds: &mut Vec<skull_codegen_lua::CalleeBindingPlan>,
    attribution: &mut AttributionAccumulator,
    times: &mut PhaseTimes,
) -> Vec<CompilerError> {
    let mut done: FxHashSet<Instance> = FxHashSet::default();
    let mut errs: Vec<(String, CompilerError)> = rejected.to_vec();
    let rejected_names: FxHashSet<&str> = rejected.iter().map(|(n, _)| n.as_str()).collect();
    while let Some(demand) = pending.pop() {
        let body = demand.inst;
        let fref = body.func;
        if !done.insert(body.clone()) {
            continue;
        }
        let f = &ws.module(fref.module).functions[fref.func];
        let name = body.symbol(ws, scx.interner());
        let Some(chunk) = live
            .get(&body)
            .or_else(|| cached.get(&body).and_then(|c| c.as_ref().ok()))
        else {
            // Not a fallback: the walk either built this body or recorded why it
            // could not, and a rejected one is reported through `rejected`.
            if !rejected_names.contains(name.as_str()) {
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
        out.push_str(&chunk.lua);
        binds.extend(chunk.binds.iter().cloned());
        allocation_bodies.extend(chunk.allocation_bodies.iter().cloned());
        attribution.add_unit(&chunk.attribution);
        times.add(&chunk.times);
        // A specialized definition's `static` cells are cells the program uses,
        // so they register for hot-reload preservation on the same terms as a
        // declaration's. Folded HERE because this is where a specialized body
        // enters the output -- the declaration loop never sees one.
        statics.extend(chunk.statics.iter().cloned());
        pending.extend(chunk.specs.iter().cloned());
        if !cached.contains_key(&body) {
            built.push((body.clone(), chunk.clone()));
        }
    }
    errs.sort_by(|(a, _), (b, _)| a.cmp(b));
    errs.into_iter().map(|(_, e)| e).collect()
}

/// **THE transform sequence an instance body is emitted through.**
///
/// Extracted so `emit_specialized_callees` and `skull::site_join`'s census
/// consume ONE sequence rather than two that agree today. The census's whole
/// claim is about the body EMISSION sees, so a census that re-listed these
/// passes would be free to drift into measuring a body no emitter renders --
/// and the drift would look like a clean measurement.
///
/// Everything after this point is rendering: `validate_emitted_body`,
/// `emit_named` and the nested-fn emission. Nothing here mints a name or
/// touches a `CodegenCtx`, which is what lets a read-only consumer run it.
pub fn prepare_instance_body_with_plans(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    fref: FuncRef,
    name: &str,
    mf: &mut skull_mir::mir::MirFunc,
    cache: &mut skull_transform::InlineCache,
    concrete_plans: &skull_mir::concrete::ConcretePlans,
) -> Result<FxHashSet<FuncRef>, CompilerError> {
    prepare_instance_body_with_attribution(ws, scx, fref, name, mf, cache, concrete_plans)
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
) -> Result<(FxHashSet<FuncRef>, PhaseTimes, InlineWork), CompilerError> {
    let mut times = PhaseTimes::default();
    let mut work = InlineWork::default();
    let before = (mf.insts.len(), mf.blocks.len(), mf.cells.len());
    let ti = Instant::now();
    let (spliced, records, scan_stats) =
        skull_transform::inline_functions_with_plans(ws, scx, fref, mf, cache, concrete_plans)?;
    times.inline = ti.elapsed();
    times.inline_work = !records.is_empty();
    work.add_records(&records);
    work.add_scan(scan_stats);
    work.add_allocations(before, (mf.insts.len(), mf.blocks.len(), mf.cells.len()));
    let cleanup_before = mir_live_instructions(mf);
    let tc = Instant::now();
    skull_transform::cleanup_with_nobox_census(
        ws,
        mf,
        before.0,
        cache
            .census_enabled()
            .then_some(&mut work.census.load_store),
    )?;
    times.cleanup = tc.elapsed();
    let cleanup_after = mir_live_instructions(mf);
    work.cleanup_live_before += cleanup_before;
    work.cleanup_live_after += cleanup_after;
    times.cleanup_work = cleanup_after != cleanup_before;
    let tclosure = Instant::now();
    let nested_before = mf.nested_fns.len();
    skull_transform::convert_closures(mf, name);
    times.closure = tclosure.elapsed();
    times.closure_work = mf.nested_fns.len() > nested_before;
    let counted_before = mf.blocks.len();
    let tcounted = Instant::now();
    skull_transform::recover_counted_loops(ws, mf);
    times.counted_loop = tcounted.elapsed();
    times.counted_loop_work = mf.blocks.len() > counted_before;
    Ok((spliced.into_iter().collect(), times, work))
}

/// Run the inliner over a hoisted-body tree (named nested `fn`s and the lambda
/// bodies `convert_closures` appends), depth-first so a lambda nested in a lambda
/// is reached too. Splices fold into the enclosing chunk's `deps`: the chunk now
/// contains a copy of each spliced callee's body, so an edit to that callee has to
/// invalidate this chunk.
fn inline_hoisted_bodies(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    self_ref: FuncRef,
    nested: &mut [MirFunc],
    inline_cache: &mut skull_transform::InlineCache,
    concrete_plans: &skull_mir::concrete::ConcretePlans,
    deps: &mut FxHashSet<FuncRef>,
    times: &mut PhaseTimes,
    work: &mut InlineWork,
) -> Result<(), String> {
    for nf in nested.iter_mut() {
        // Flat variants embed source without going through an inline splice.
        if let Some(source) = nf.flat_variant_source {
            deps.insert(source);
        }
        let before = (nf.insts.len(), nf.blocks.len(), nf.cells.len());
        let ti = Instant::now();
        let (spliced, records, scan_stats) = skull_transform::inline_functions_with_plans(
            ws,
            scx,
            self_ref,
            nf,
            inline_cache,
            concrete_plans,
        )
        .map_err(|e| e.message)?;
        times.hoisted_inline += ti.elapsed();
        work.add_records(&records);
        work.hoisted_splices += records.len();
        work.add_scan(scan_stats);
        times.hoisted_inline_work |= !records.is_empty();
        work.add_allocations(before, (nf.insts.len(), nf.blocks.len(), nf.cells.len()));
        deps.extend(spliced);
        let cleanup_before = mir_live_instructions(nf);
        let tc = Instant::now();
        skull_transform::cleanup_with_nobox_census(
            ws,
            nf,
            before.0,
            inline_cache
                .census_enabled()
                .then_some(&mut work.census.load_store),
        )
        .map_err(|e| e.message)?;
        times.cleanup += tc.elapsed();
        let cleanup_after = mir_live_instructions(nf);
        work.cleanup_live_before += cleanup_before;
        work.cleanup_live_after += cleanup_after;
        times.cleanup_work |= cleanup_after != cleanup_before;
        let mut children = std::mem::take(&mut nf.nested_fns);
        inline_hoisted_bodies(
            ws,
            scx,
            self_ref,
            &mut children,
            inline_cache,
            concrete_plans,
            deps,
            times,
            work,
        )?;
        nf.nested_fns = children;
    }
    Ok(())
}
