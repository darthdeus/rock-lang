use super::*;

fn overlay_order_key(
    scx: &skull_mir::lower::ScalarCtx,
    body: &Instance,
) -> (usize, u32, Option<String>) {
    let fref = body.func;
    let key = (!body.args.is_empty()).then(|| {
        body.arg_types(scx.interner())
            .iter()
            .map(skull_mir::monoty::MonoTy::canonical)
            .collect::<Vec<_>>()
            .join(",")
    });
    (fref.module.index(), u32::from(fref.func.into_raw()), key)
}

fn overlay_callee_bindings(
    ws: &Workspace,
    cache: &SkullEmitCache,
    changed: &[Instance],
) -> (
    Vec<skull_codegen_lua::CalleeBindingPlan>,
    FxHashSet<Instance>,
) {
    let mut names: FxHashSet<String> = changed
        .iter()
        .map(|body| body.symbol(ws, cache.scx.interner()))
        .collect();
    for body in changed {
        if let Some(Ok(chunk)) = cache.chunks.get(body) {
            names.extend(chunk.binds.iter().map(|plan| plan.name.clone()));
        }
    }
    let mut candidates: Vec<_> = cache
        .chunks
        .iter()
        .filter(|(body, _)| cache.lua_view.instances.contains(*body))
        .filter_map(|(body, chunk)| chunk.as_ref().ok().map(|chunk| (body, chunk)))
        .flat_map(|(body, chunk)| chunk.binds.iter().map(move |plan| (body, plan)))
        .collect();
    candidates.sort_by(|(_, a), (_, b)| a.rank.cmp(&b.rank).then_with(|| a.name.cmp(&b.name)));
    let mut bindings = Vec::new();
    let mut bodies = FxHashSet::default();
    for (body, plan) in candidates {
        if names.contains(&plan.name) || plan.captures.iter().any(|name| names.contains(name)) {
            names.insert(plan.name.clone());
            bodies.insert(body.clone());
            bindings.push(plan.clone());
        }
    }
    (bindings, bodies)
}

fn transactional_overlay(lua: &str, mut globals: BTreeSet<String>) -> String {
    globals.extend(lua.lines().filter_map(definition_global).map(str::to_owned));
    let mut out = String::from("do\nlocal __rock_overlay_old = {}\n");
    let names: Vec<_> = globals.into_iter().collect();
    for names in names.chunks(4_096) {
        out.push_str("do\n(function()\n");
        for name in names {
            let name = skull_codegen_lua::lua_string(name);
            let _ = writeln!(out, "__rock_overlay_old[{name}] = {{ rawget(_G, {name}) }}");
        }
        out.push_str("end)()\nend\n");
    }
    out.push_str("local __rock_overlay_ok, __rock_overlay_error = xpcall(function()\n");
    out.push_str(lua);
    out.push_str(
        "end, debug.traceback)\n\
         if not __rock_overlay_ok then\n\
         if _G.__rock_code_slot_abort then __rock_code_slot_abort() end\n\
         for name, old in pairs(__rock_overlay_old) do rawset(_G, name, old[1]) end\n\
         error(__rock_overlay_error, 0)\n\
         end\nend\n",
    );
    out
}

fn assemble_overlay(
    ws: &Workspace,
    cache: &SkullEmitCache,
    shader_changed: bool,
    initialize_code_slots: bool,
    mut fingerprint_identities: FxHashSet<skull_mir::code_identity::CodeIdentity>,
) -> Result<
    (
        MirEmit,
        UnitComptimeReads,
        crate::artifact_fingerprint::CodeRows,
    ),
    CompilerError,
> {
    let t_clone_artifact = Instant::now();
    let mut out = String::from("-- skull incremental overlay (loaded after main.lua)\n");
    if initialize_code_slots {
        out.push_str(skull_codegen_lua::FN_MT_PRELUDE);
    }
    let mut candidate_code_artifact = cache.code_artifact.clone();
    let d_clone_artifact = t_clone_artifact.elapsed();
    out.push_str("do\nlocal __rock_state_cell = __rock_globals\n");
    let mut unit_reads = UnitComptimeReads::default();
    let mut overwritten_globals = BTreeSet::new();

    let t_definitions = Instant::now();
    let mut order: Vec<Instance> = cache.changed_since_full.iter().cloned().collect();
    order.sort_by_key(|b| overlay_order_key(&cache.scx, b));
    order.dedup();
    let value_ctors_at = out.len();
    let mut definitions = DefinitionOrder::default();
    for body in &order {
        match cache.chunks.get(body) {
            Some(Ok(chunk)) => definitions.push_chunk(chunk),
            Some(Err(err)) => return Err(err.clone()),
            None => {}
        }
    }
    let runtime_refs = definitions.runtime_refs();
    out.push_str(&definitions.definitions_lua());
    let (mut bindings, rebound_bodies) = overlay_callee_bindings(ws, cache, &order);
    overwritten_globals.extend(bindings.iter().map(|binding| binding.name.clone()));
    out.push_str(&callee_binding_pass_lua(&mut bindings));
    overwritten_globals.extend(
        runtime_refs
            .iter()
            .filter(|name| crate::is_enum_names_global(name))
            .map(ToString::to_string),
    );
    out.push_str(&crate::enum_names_defs(ws, &runtime_refs));
    let mut published_bodies = order.clone();
    published_bodies.extend(rebound_bodies);
    let overlay_code_artifact = overlay_code_artifact(ws, cache, &published_bodies);

    let d_definitions = t_definitions.elapsed();
    let t_code_slot_stage = Instant::now();
    let has_code_slots = cache.code_artifact.has_escaping();
    let code_slot_stage = code_slot_registration_lua_entries(
        ws,
        &cache.scx,
        overlay_code_artifact.entries.values(),
        has_code_slots,
    )?;
    out.push_str(&code_slot_stage);
    let d_code_slot_stage = t_code_slot_stage.elapsed();
    let t_static_resets = Instant::now();
    let mut static_resets = String::new();
    let mut seen_static: FxHashSet<Ustr> = FxHashSet::default();
    for body in &order {
        let Some(Ok(chunk)) = cache.chunks.get(body) else {
            continue;
        };
        for sc in &chunk.statics {
            if !seen_static.insert(sc.cell) {
                continue;
            }
            let (value_slot, done_slot) =
                skull_mir::global_storage::static_cell_storage_keys(ws, cache.scx.interner(), sc)
                    .map_err(CompilerError::new_no_location)?;
            let _ = writeln!(
                static_resets,
                "__rock_static_prepare({value_slot:?}, {done_slot:?}, {:?})",
                static_cell_key(sc),
            );
        }
    }
    if !cache.static_cells_since_full.is_empty() {
        let current: FxHashSet<Ustr> = cache
            .chunks
            .values()
            .flatten()
            .flat_map(|chunk| chunk.statics.iter().map(|sc| sc.cell))
            .collect();
        let mut dropped: Vec<&StaticBaseline> = cache
            .static_cells_since_full
            .iter()
            .filter(|(cell, _)| !current.contains(*cell))
            .map(|(_, base)| base)
            .collect();
        dropped.sort_by(|a, b| a.value_slot.cmp(&b.value_slot));
        for base in dropped {
            let _ = writeln!(
                static_resets,
                "__rock_static_drop({:?}, {:?})",
                base.value_slot, base.done_slot
            );
        }
    }
    let d_static_resets = t_static_resets.elapsed();

    let t_const_inits = Instant::now();
    if !cache.changed_consts_since_full.is_empty() {
        let deps = skull_mir::reach::symbol_references(ws);

        let changed_names: FxHashSet<Ustr> = cache
            .changed_consts_since_full
            .iter()
            .map(|&(mid, gid)| ws.module(mid).globals[gid].name)
            .collect();

        struct Affected {
            mid: ModuleId,
            gid: Idx<bone_semantic::hir::GlobalDef>,
            name: Ustr,
            reach: FxHashSet<Ustr>,
        }
        let mut const_items: Vec<(ModuleId, Idx<bone_semantic::hir::GlobalDef>)> = ws
            .source_modules()
            .flat_map(|m| {
                m.globals
                    .iter()
                    .filter(|(_, g)| g.is_const)
                    .map(move |(gid, _)| (m.id, gid))
            })
            .collect();
        const_items.sort_by_key(|(m, g)| (m.index(), u32::from(g.into_raw())));

        let mut affected: Vec<Affected> = Vec::new();
        for (mid, gid) in const_items {
            let name = ws.module(mid).globals[gid].name;
            let reach = skull_mir::reach::forward_reach(&deps, name);
            let changed = changed_names.contains(&name);
            if changed || reach.iter().any(|n| changed_names.contains(n)) {
                affected.push(Affected {
                    mid,
                    gid,
                    name,
                    reach,
                });
            }
        }

        let mut inits: Vec<PendingInit<String>> = Vec::new();
        let mut unit_refs = FxHashSet::default();
        let mut definitions = LuaDefinitionWriter::new(out);
        for a in affected
            .iter()
            .filter(|a| !cache.cg.is_unread_constant(a.name))
        {
            overwritten_globals.extend(
                global_init_target(ws, &cache.scx, &cache.cg, &ws.module(a.mid).globals[a.gid])?
                    .split(", ")
                    .map(str::to_owned),
            );
            let line = {
                let unit =
                    emit_global_unit(ws, &cache.scx, &cache.cg, &cache.concrete, a.mid, a.gid);
                fingerprint_identities.extend(
                    unit.code_artifact
                        .entries()
                        .iter()
                        .map(|entry| entry.identity.clone()),
                );
                candidate_code_artifact = std::sync::Arc::new(
                    candidate_code_artifact.with_replacements(unit.code_artifact.entries()),
                );
                if let Some(err) = unit.hard_error {
                    return Err(err);
                }
                unit_reads
                    .globals
                    .insert((a.mid, a.gid), unit.comptime_reads.clone());
                definitions.push_str(&unit.lua);
                unit_refs.extend(
                    unit.global_names
                        .iter()
                        .filter(|name| crate::is_enum_names_global(name)),
                );
                unit.pending_init.map(|(_, _, line)| line).ok_or_else(|| {
                    CompilerError::new_no_location(format!(
                        "changed global `{}` produced no initializer plan",
                        a.name
                    ))
                })?
            };
            inits.push(PendingInit {
                name: a.name,
                deps: a.reach.iter().copied().collect(),
                item: line,
            });
        }
        out = definitions.finish();
        overwritten_globals.extend(unit_refs.iter().map(ToString::to_string));
        out.push_str(&crate::enum_names_defs(ws, &unit_refs));

        out.push_str(&sort_global_init_lines(inits, &FxHashMap::default())?.concat());
    }
    let d_const_inits = t_const_inits.elapsed();

    if shader_changed {
        out.push_str(&cache.shader_registrations.lua);
    }
    let t_lifecycle = Instant::now();
    let lifecycle_plan = skull_mir::lifecycle::collect_with_artifact(ws, &candidate_code_artifact)?;
    let d_lifecycle = t_lifecycle.elapsed();
    if !has_code_slots && lifecycle_plan.declarations > 0 {
        out.push_str("__rock_code_slot_begin()\n");
    }
    let t_publication = Instant::now();
    out.push_str(&lifecycle_plan_stage_lua(ws, &cache.scx, &lifecycle_plan));
    out.push_str(&static_resets);
    out.push_str(code_slot_publish_lua(
        &candidate_code_artifact,
        false,
        lifecycle_plan.declarations > 0,
    ));
    let d_publication = t_publication.elapsed();
    let t_metadata = Instant::now();
    let definition_plan = cache.definition_plan.as_deref().cloned().ok_or_else(|| {
        CompilerError::new_no_location(
            "Artifact fingerprint generation requires a complete DefinitionPlan".to_string(),
        )
    })?;
    let global_layout = cache.global_layout.clone().ok_or_else(|| {
        CompilerError::new_no_location(
            "Artifact fingerprint generation requires a global layout".to_string(),
        )
    })?;
    let artifact_reads = artifact_comptime_reads(cache, &[], &unit_reads, false);
    let d_metadata = t_metadata.elapsed();
    let t_fingerprint = Instant::now();
    let mut fingerprint_code_rows = cache.fingerprint_code_rows.clone().ok_or_else(|| {
        CompilerError::new_no_location(
            "overlay requires a complete code fingerprint inventory".to_string(),
        )
    })?;
    let t_fingerprint_rows = Instant::now();
    fingerprint_code_rows
        .replace(
            ws,
            cache.scx.interner(),
            &candidate_code_artifact,
            &fingerprint_identities,
        )
        .map_err(|error| CompilerError::new_no_location(error.to_string()))?;
    let d_fingerprint_rows = t_fingerprint_rows.elapsed();
    let artifact_fingerprint = crate::artifact_fingerprint::compute_cached(
        ws,
        cache.scx.interner(),
        &candidate_code_artifact,
        &lifecycle_plan,
        &definition_plan,
        &global_layout,
        &artifact_reads,
        &ws.comptime_inputs.volatile_vars,
        renderer_fingerprint_tag(cache.renderer_target),
        &fingerprint_code_rows,
    )
    .map_err(|error| {
        CompilerError::new_no_location(format!("Artifact fingerprint generation failed: {error}"))
    })?;
    let d_fingerprint = t_fingerprint.elapsed();
    let mut ctor_calls = Vec::new();
    crate::push_value_ctor_calls(&out[value_ctors_at..], &mut ctor_calls);
    out.insert_str(
        value_ctors_at,
        &crate::value_ctor_defs(&cache.value_ctors, &ctor_calls.into_iter().collect()),
    );
    out.push_str("end\n");
    out = transactional_overlay(&out, overwritten_globals);
    if cache.cg.line_directives() {
        out = skull_codegen_lua::resolve_line_directive_resets(&out, "=[overlay]");
    }

    Ok((
        MirEmit {
            compile_counts: CompileCounts {
                concrete_plans: cache.concrete.plans.len() + cache.concrete.always_plans.len(),
                chunks: cache.chunks.len(),
                dependency_edges: None,
            },
            lua: out,
            globals_bridge_end: None,
            allocation_report: crate::allocation_report::AllocationReport::default(),
            code_artifact: candidate_code_artifact,
            lifecycle_plan,
            global_layout,
            definition_plan,
            artifact_fingerprint: Some(artifact_fingerprint),
            phases: vec![
                ("  overlay artifact clone", d_clone_artifact),
                ("  overlay definitions", d_definitions),
                ("  overlay code-slot stage", d_code_slot_stage),
                ("  overlay static resets", d_static_resets),
                ("  overlay const inits", d_const_inits),
                ("  overlay lifecycle collection", d_lifecycle),
                ("  overlay publication", d_publication),
                ("  overlay metadata", d_metadata),
                ("  overlay fingerprint", d_fingerprint),
                ("  overlay fingerprint code rows", d_fingerprint_rows),
            ],
            abi_plan_stats: cache.scx.abi_plan_stats(),
            attribution: EmitAttribution::default(),
            arena_traffic: ArenaTraffic::default(),
            setup_lanes: SetupLaneAttribution::default(),
            graph_census: crate::instance_graph::RunStats::default(),
            concrete_stats: crate::concrete_body::ConcreteBuildStats::default(),
            emits_main: false,
            shaders: std::sync::Arc::clone(&cache.shader_registrations.sources),
        },
        unit_reads,
        fingerprint_code_rows,
    ))
}

pub fn emit_overlay(
    ws: &Workspace,
    cache: &mut SkullEmitCache,
    reinferred: &[(ModuleId, BodyOwner)],
) -> Result<MirEmit, CompilerError> {
    emit_overlay_with_span_edits(ws, cache, reinferred, &[])
}

pub fn emit_overlay_with_span_edits(
    ws: &Workspace,
    cache: &mut SkullEmitCache,
    reinferred: &[(ModuleId, BodyOwner)],
    edits: &[crate::instance_graph::TextEditDelta],
) -> Result<MirEmit, CompilerError> {
    cache.concrete.detach_plan_store();
    let t0 = Instant::now();
    let t_cg = t0.elapsed();
    let t1 = Instant::now();
    let t_scx = t1.elapsed();
    let t2 = Instant::now();
    let t_shader = Instant::now();
    let shader_changed = cache.update_shader_registrations(ws, reinferred)?;
    let d_shader = t_shader.elapsed();
    let t_chunk = Instant::now();
    let (affected, update, added_errors, fingerprint_identities) =
        cache.update_chunk_cache(ws, reinferred, edits)?;
    let d_chunk = t_chunk.elapsed();
    if let Some(err) = SkullEmitCache::combine_errors(added_errors) {
        return Err(err);
    }
    let shader_changed = shader_changed || cache.shader_delta_affects_view(&update);
    if shader_changed {
        cache.refresh_shader_registrations(ws)?;
    }
    cache.changed_since_full.extend(affected);
    for &(mid, owner) in reinferred {
        if let BodyOwner::Global(gid) = owner {
            if ws.module(mid).globals[gid].is_const {
                cache.changed_consts_since_full.insert((mid, gid));
            }
        }
    }
    let t_replacements = Instant::now();
    let replacements = cache
        .changed_since_full
        .iter()
        .filter_map(|body| cache.chunks.get(body).and_then(|chunk| chunk.as_ref().ok()))
        .flat_map(|chunk| chunk.code_artifact.entries());
    let next_code_artifact = cache.code_artifact.with_replacements(replacements);
    let initialize_code_slots =
        !cache.code_artifact.has_escaping() && next_code_artifact.has_escaping();
    let d_replacements = t_replacements.elapsed();
    if !cache.code_artifact.compatible_overlay(&next_code_artifact) {
        return Err(CompilerError::new_no_location(
            "Rock CodeRef compatibility requires a safe restart".to_string(),
        ));
    }
    let t_layout = Instant::now();
    let body_only = reinferred
        .iter()
        .all(|(_, owner)| matches!(owner, BodyOwner::Function(_)));
    if !body_only {
        let layout = skull_mir::global_storage::global_layout_id(ws, &cache.scx)
            .map_err(CompilerError::new_no_location)?;
        if cache.global_layout.as_deref() != Some(layout.as_str()) {
            return Err(CompilerError::new_no_location(
                "Rock global layout changed; restart required".to_string(),
            ));
        }
    }
    let d_layout = t_layout.elapsed();
    cache.code_artifact = std::sync::Arc::new(next_code_artifact);
    let t_stale = Instant::now();
    {
        let mut file_hashes: FxHashMap<Ustr, Option<u64>> = FxHashMap::default();
        let stale_consts: Vec<(ModuleId, Idx<bone_semantic::hir::GlobalDef>)> = cache
            .unit_comptime_reads
            .globals
            .iter()
            .filter(|((mid, gid), reads)| {
                ws.module(*mid).globals[*gid].is_const
                    && comptime_reads_stale(ws, reads, &mut file_hashes)
            })
            .map(|(key, _)| *key)
            .collect();
        cache.changed_consts_since_full.extend(stale_consts);
    }
    let d_stale = t_stale.elapsed();
    let t_relower = t2.elapsed();
    let t3 = Instant::now();
    let (mut emit, overlay_unit_reads, fingerprint_code_rows) = assemble_overlay(
        ws,
        cache,
        shader_changed,
        initialize_code_slots,
        fingerprint_identities,
    )?;
    cache.fingerprint_code_rows = Some(fingerprint_code_rows);
    cache.code_artifact = emit.code_artifact.clone();
    cache.lifecycle_plan = Some(std::sync::Arc::new(emit.lifecycle_plan.clone()));
    cache.definition_plan = Some(std::sync::Arc::new(emit.definition_plan.clone()));
    cache.artifact_fingerprint = emit.artifact_fingerprint.clone();
    for (key, reads) in overlay_unit_reads.globals {
        if reads.is_empty() {
            cache.unit_comptime_reads.globals.remove(&key);
        } else {
            cache.unit_comptime_reads.globals.insert(key, reads);
        }
    }
    if concrete_stats_requested() {
        emit.concrete_stats = cache.concrete.snapshot_build_stats();
    }
    cache.retain_static_cells(ws)?;
    let t_asm = t3.elapsed();
    if let Some(err) = cache.cg.take_rejection() {
        return Err(err);
    }
    emit.phases.extend([
        ("CodegenCtx::new", t_cg),
        ("ScalarCtx::new", t_scx),
        ("update_chunk_cache (re-lower changed)", t_relower),
        ("  shader registrations", d_shader),
        ("  owner-local chunk cache", d_chunk),
        ("  CodeArtifact replacement", d_replacements),
        ("  global layout check", d_layout),
        ("  stale comptime scan", d_stale),
        (
            "  graph update",
            Duration::from_nanos(cache.repair_stats.graph_update_ns),
        ),
        (
            "  concrete update",
            Duration::from_nanos(cache.repair_stats.concrete_update_ns),
        ),
        (
            "  ArtifactView update",
            Duration::from_nanos(cache.repair_stats.artifact_update_ns),
        ),
        (
            "  candidate update",
            Duration::from_nanos(cache.repair_stats.candidate_update_ns),
        ),
        (
            "  affected selection",
            Duration::from_nanos(cache.repair_stats.affected_selection_ns),
        ),
        (
            "  relowering",
            Duration::from_nanos(cache.repair_stats.relowering_ns),
        ),
        ("assemble_overlay (+build_deco_map)", t_asm),
    ]);
    Ok(emit)
}
