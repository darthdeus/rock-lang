//! Overlay builds that patch a cached artifact with edited bodies.

use super::*;

/// Deterministic overlay order over emitted bodies. Reduces to the declaration
/// order the overlay used before specializations could appear in it: a
/// declaration's own body sorts ahead of its specializations, and those sort by
/// their instantiation's canonical spelling, which is injective.
fn overlay_order_key(
    scx: &skull_mir::lower::ScalarCtx,
    body: &Instance,
) -> (usize, u32, Option<String>) {
    let fref = body.func;
    // The instantiation's canonical spelling, which is injective -- a
    // declaration's own body sorts ahead of its specializations because it has
    // no arguments to spell.
    let key = (!body.args.is_empty()).then(|| {
        body.arg_types(scx.interner())
            .iter()
            .map(skull_mir::monoty::MonoTy::canonical)
            .collect::<Vec<_>>()
            .join(",")
    });
    (fref.module.index(), u32::from(fref.func.into_raw()), key)
}

/// Recreate only factories whose captures changed, in the same order as a full
/// build. Rebound callers must also publish their escaping CodeRef slots.
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

/// Assemble the incremental OVERLAY (`incremental.lua`): the cached chunks of the
/// functions changed since the last full build, then slot publication for their
/// current CodeRefs. Registries retain those logical values across the overlay.
/// Loaded AFTER `main.lua`; redefined globals override their stale defs. Emits NO
/// scaffold/globals -- those are unchanged on a body edit and live in main.lua.
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
        usize,
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

    // Redefine the changed functions, deterministic order for a stable diff.
    let t_definitions = Instant::now();
    let mut order: Vec<Instance> = cache.changed_since_full.iter().cloned().collect();
    order.sort_by_key(|b| overlay_order_key(&cache.scx, b));
    order.dedup();
    let mut definitions = LuaDefinitionWriter::new(out);
    for body in &order {
        match cache.chunks.get(body) {
            Some(Ok(chunk)) => definitions.push_str(&chunk.lua),
            Some(Err(err)) => return Err(err.clone()),
            // extern / inline(lua) edits are interface-tier -> full rebuild path.
            None => {}
        }
    }
    out = definitions.finish();
    let (mut bindings, rebound_bodies) = overlay_callee_bindings(ws, cache, &order);
    out.push_str(&callee_binding_pass_lua(&mut bindings));
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
    // Reset `static` cells whose initializer MEANING changed since the last full
    // build (`type@init_hash` key differs from the baseline). A static is
    // name-keyed and lazy, so redefining its fn above leaves the warmed cell +
    // set done-flag intact -- the redefined fn would re-read the stale value.
    // Re-run the initializer transition so the next reach re-inits with the
    // edited initializer; an unchanged key is skipped (an unrelated body edit
    // preserves the live cell). Only the changed fns' statics are considered --
    // a static's key can't change without its enclosing fn being re-lowered into
    // `changed_since_full`. The resets run in the publication step, so an
    // overlay that fails to load leaves every static as it was.
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
            let changed = cache
                .static_key_baseline
                .get(&sc.cell)
                .is_none_or(|base| base.key != static_cell_key(sc));
            if changed {
                let (value_slot, done_slot) = skull_mir::global_storage::static_cell_storage_keys(
                    ws,
                    cache.scx.interner(),
                    sc,
                )
                .map_err(CompilerError::new_no_location)?;
                let _ = writeln!(
                    static_resets,
                    "__rock_static_prepare({value_slot:?}, {done_slot:?}, {:?})",
                    static_cell_key(sc),
                );
            }
        }
    }
    // A static an edit removed is in the baseline but in no current chunk (a
    // generic's static can sit in several callers' chunks, so absence from the
    // edited body alone does not remove it). Its cells are dropped so they stop
    // holding their last value.
    if !cache.static_key_baseline.is_empty() {
        let current: FxHashSet<Ustr> = cache
            .chunks
            .values()
            .flatten()
            .flat_map(|chunk| chunk.statics.iter().map(|sc| sc.cell))
            .collect();
        let mut dropped: Vec<&StaticBaseline> = cache
            .static_key_baseline
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

    // Re-emit only the AFFECTED `const` inits when a const changed since the last
    // full build: the edited const(s) PLUS their transitive DEPENDENTS (consts
    // whose init reads a changed const, directly or through a function). An
    // unrelated const is left untouched in main.lua. The dependency graph is the
    // SAME symbol-reference info goto-def / references / the re-inference cascade
    // use -- `BodyInference.deps` (the names each body references). Globals are
    // ALWAYS re-inferred on a body edit, so these deps are fresh (no caching, no
    // staleness). Each affected const emits an UNGUARDED
    // `<NAME> = __skull_ginit_<NAME>()` line in dependency order. A dependent's
    // ginit can embed an upstream scalar literal, so its definition is refreshed
    // along with the edited const. Mutable globals are never re-emitted:
    // their guard preserves the live value across reload.
    let t_const_inits = Instant::now();
    if !cache.changed_consts_since_full.is_empty() {
        // Forward symbol-reference graph (name -> names it references) from the
        // CURRENT inference. Overloads merge under one name (conservative: at worst
        // re-emits an extra const, never misses one).
        let deps = skull_mir::reach::symbol_references(ws);

        let changed_names: FxHashSet<Ustr> = cache
            .changed_consts_since_full
            .iter()
            .map(|&(mid, gid)| ws.module(mid).globals[gid].name)
            .collect();

        // Every const; affected = itself changed OR it transitively references a
        // changed const. `reach` (its transitive refs) is reused for the sort.
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

        // Emit each affected const's ginit fn now; build its init
        // line + dependency edges (restricted to affected consts) for the sort.
        let mut inits: Vec<PendingInit<String>> = Vec::new();
        let mut definitions = LuaDefinitionWriter::new(out);
        for a in &affected {
            let line = {
                let unit =
                    emit_global_unit(ws, &cache.scx, &cache.cg, &cache.concrete, a.mid, a.gid);
                fingerprint_identities.extend(unit.code_artifact.entries.keys().cloned());
                candidate_code_artifact = std::sync::Arc::new(
                    candidate_code_artifact.with_replacements(&[&unit.code_artifact]),
                );
                if let Some(err) = unit.hard_error {
                    return Err(err);
                }
                unit_reads
                    .globals
                    .insert((a.mid, a.gid), unit.comptime_reads.clone());
                definitions.push_str(&unit.lua);
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

        out.push_str(&sort_global_init_lines(inits, &FxHashMap::default())?.concat());
    }
    let d_const_inits = t_const_inits.elapsed();

    if shader_changed {
        out.push_str(&cache.shader_registrations.lua);
    }
    let t_lifecycle = Instant::now();
    let lifecycle_plan = skull_mir::lifecycle::collect_with_artifact(ws, &candidate_code_artifact)?;
    let d_lifecycle = t_lifecycle.elapsed();
    if !has_code_slots && lifecycle_plan.census.declarations > 0 {
        out.push_str("__rock_code_slot_begin()\n");
    }
    let t_publication = Instant::now();
    out.push_str(&lifecycle_plan_stage_lua(ws, &cache.scx, &lifecycle_plan));
    out.push_str(&static_resets);
    out.push_str(code_slot_publish_lua(
        &candidate_code_artifact,
        false,
        lifecycle_plan.census.declarations > 0,
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
    let fingerprint_rows_derived = fingerprint_code_rows
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
    if cache.full_scans {
        let fresh = self::artifact_fingerprint(
            ws,
            &cache.scx,
            &candidate_code_artifact,
            &lifecycle_plan,
            &definition_plan,
            &global_layout,
            &artifact_reads,
            cache.renderer_target,
        )?;
        if artifact_fingerprint != fresh {
            return Err(CompilerError::new_no_location(
                "cached code fingerprint differs from fresh inventory".to_string(),
            ));
        }
    }
    let d_fingerprint = t_fingerprint.elapsed();
    out.push_str("end\n");
    if cache.cg.line_directives() {
        out = skull_codegen_lua::resolve_line_directive_resets(&out, "=[overlay]");
    }

    Ok((
        MirEmit {
            release_stats: CompileReleaseStats::default(),
            compile_counts: CompileCounts {
                concrete_plans: cache.concrete.plans.len() + cache.concrete.always_plans.len(),
                chunks: cache.chunks.len(),
                dependency_edges: None,
            },
            lua: out,
            allocation_report: crate::allocation_report::AllocationReport::default(),
            code_artifact: candidate_code_artifact,
            lifecycle_plan,
            global_layout,
            definition_plan,
            artifact_fingerprint,
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
            // An overlay redefines changed bodies only; the entry point is
            // already installed by the `main.lua` it loads over.
            emits_main: false,
            shaders: std::sync::Arc::clone(&cache.shader_registrations.sources),
        },
        unit_reads,
        fingerprint_code_rows,
        fingerprint_rows_derived,
    ))
}

/// Incremental OVERLAY emit for a body edit: update the chunk cache for the
/// re-inferred bodies, accumulate them into `changed_since_full`, and assemble
/// `incremental.lua` from the WHOLE set (so an earlier edit isn't reverted to
/// main.lua's stale copy. The caller writes the result to
/// `tmp/incremental.lua` (loaded after `tmp/main.lua`); on an interface/full
/// rebuild the caller uses `emit_full` and clears `changed_since_full`.
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
    // The retained context refreshes scalar values inside update_chunk_cache.
    let t_cg = t0.elapsed();
    let t1 = Instant::now();
    // `scx` is maintained incrementally inside `update_chunk_cache` (per-body diff)
    // -- no whole-workspace rebuild.
    let t_scx = t1.elapsed();
    let t2 = Instant::now();
    // Shader registrations FIRST -- their dependencies are graph/view roots, so
    // `update_chunk_cache`'s re-walk must see this edit's set.
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
    // Accumulate changed `const` globals so the overlay re-emits their inits
    // (unguarded). A mutable `global` value edit is interface-tier (full rebuild),
    // so it never reaches here; even so, gate on `is_const` defensively.
    for &(mid, owner) in reinferred {
        if let BodyOwner::Global(gid) = owner {
            if ws.module(mid).globals[gid].is_const {
                cache.changed_consts_since_full.insert((mid, gid));
            }
        }
    }
    let t_replacements = Instant::now();
    let replacements: Vec<&skull_mir::code_identity::CodeArtifact> = cache
        .changed_since_full
        .iter()
        .filter_map(|body| cache.chunks.get(body).and_then(|chunk| chunk.as_ref().ok()))
        .map(|chunk| &chunk.code_artifact)
        .collect();
    let next_code_artifact = cache.code_artifact.with_replacements(&replacements);
    let initialize_code_slots =
        !cache.code_artifact.has_escaping() && next_code_artifact.has_escaping();
    let d_replacements = t_replacements.elapsed();
    if !cache.code_artifact.compatible_overlay(&next_code_artifact) {
        return Err(CompilerError::new_no_location(
            "Rock CodeRef compatibility requires a safe restart".to_string(),
        ));
    }
    // A body edit cannot change the mutable-global layout; any other reinferred
    // owner is checked, since the overlay runs over the live storage.
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
    // A comptime input read by a GLOBAL initializer can go stale with no
    // reinferred body at all (bone hashes only source text). Route a stale
    // const through the same changed-consts machinery -- its ginit re-lowers
    // fresh, re-reading the file / new var value. A stale MUTABLE global is
    // deliberately left alone: its guarded init preserves the live value on
    // reload, exactly as a source edit to its initializer does.
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
    let published_code_artifact = cache.code_artifact.clone();
    let (mut emit, overlay_unit_reads, fingerprint_code_rows, fingerprint_rows_derived) =
        assemble_overlay(
            ws,
            cache,
            shader_changed,
            initialize_code_slots,
            fingerprint_identities,
        )?;
    cache.fingerprint_code_rows = Some(fingerprint_code_rows);
    cache.repair_stats.fingerprint_code_rows_derived = fingerprint_rows_derived;
    cache.repair_stats.code_artifact_assembly_reused =
        std::sync::Arc::ptr_eq(&published_code_artifact, &emit.code_artifact);
    cache.code_artifact = emit.code_artifact.clone();
    cache.lifecycle_plan = Some(std::sync::Arc::new(emit.lifecycle_plan.clone()));
    cache.definition_plan = Some(std::sync::Arc::new(emit.definition_plan.clone()));
    cache.artifact_fingerprint = Some(emit.artifact_fingerprint.clone());
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
    // Advance the static baseline to the keys now in effect. A reset line fires on
    // the reload that dofiles the overlay carrying it, so once THIS overlay resets a
    // cell (or leaves it unchanged), a LATER overlay must not reset it again -- else
    // an unrelated follow-up edit would re-run the initializer and wipe accumulated
    // state. Bring each changed fn's cells up to their current key so the next
    // overlay only resets cells whose key changes AGAIN. A cell this overlay
    // dropped leaves the baseline, so a later overlay does not drop it again.
    // A specialization first built by this overlay joins the baseline here.
    cache.advance_static_baseline(ws)?;
    let t_asm = t3.elapsed();
    // A body edit that mints a newly-colliding synthetic name is not safe to
    // deliver as an overlay.
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
