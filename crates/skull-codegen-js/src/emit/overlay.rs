//! Hot-reload overlays over a cached program.

use super::*;

pub fn emit_overlay(
    ws: &Workspace,
    user_mid: bone_core::prelude::ModuleId,
    cache: &mut JsEmitCache,
    reinferred: &[(bone_core::prelude::ModuleId, bone_core::hir::BodyOwner)],
) -> Result<JsReload, CompilerError> {
    emit_overlay_with_span_edits(ws, user_mid, cache, reinferred, &[])
}

pub fn emit_overlay_with_span_edits(
    ws: &Workspace,
    user_mid: bone_core::prelude::ModuleId,
    cache: &mut JsEmitCache,
    reinferred: &[(bone_core::prelude::ModuleId, bone_core::hir::BodyOwner)],
    edits: &[skull_graph::instance_graph::TextEditDelta],
) -> Result<JsReload, CompilerError> {
    if reinferred.iter().any(|(_, owner)| {
        !matches!(
            owner,
            bone_core::hir::BodyOwner::Function(_) | bone_core::hir::BodyOwner::Global(_)
        )
    }) {
        return Ok(JsReload::FullRequired);
    }

    // **The session's own arena, not a fresh one** -- see `JsEmitCache::scx`.
    // MAINTAINED first: the value-reference scan behind scalar-ABI eligibility
    // is per body, so a reused context that skipped this would plan the ABI the
    // source had at the full build. Same maintenance `skull::mir_emit`'s
    // `update_chunk_cache` performs for luajit.
    for &(mid, owner) in reinferred {
        cache.scx.update_body(ws, mid, owner);
    }

    for &(mid, owner) in reinferred {
        let key = (mid, owner);
        let previous = cache
            .shader_contributions
            .get(&key)
            .cloned()
            .unwrap_or_default();
        let (current, _) = skull_mir::lower::collect_shader_body(ws, mid, owner)?;
        if current != previous {
            return Ok(JsReload::FullRequired);
        }
    }

    let changed_funcs: Vec<FuncRef> = reinferred
        .iter()
        .filter_map(|(mid, owner)| match owner {
            bone_core::hir::BodyOwner::Function(func) => Some(FuncRef {
                module: *mid,
                func: *func,
            }),
            bone_core::hir::BodyOwner::Global(_) => None,
            _ => None,
        })
        .collect();
    let current_opaque = skull_graph::instance_graph::opaque_boundary_roots_for_decls(
        ws,
        changed_funcs.iter().copied(),
    );
    for function in changed_funcs {
        for member in skull_mir::lower::backend_variant_group(ws, function) {
            let decl = skull_graph::instance_graph::DeclKey(member);
            let previous = cache
                .opaque_root_contributions
                .get(&decl)
                .cloned()
                .unwrap_or_default();
            let current = current_opaque.get(&decl).cloned().unwrap_or_default();
            if current != previous {
                return Ok(JsReload::FullRequired);
            }
        }
    }

    if let Some(main) = nullary_main(ws, user_mid) {
        if !cache.root_plan.refs.contains(&main) {
            return Ok(JsReload::FullRequired);
        }
    }

    let root_plan = cache.root_plan.clone();
    let current_roots: HashSet<FuncRef> = root_plan.refs.iter().copied().collect();
    let session_roots: HashSet<FuncRef> = cache.session.roots.iter().copied().collect();
    if current_roots != session_roots {
        return Ok(JsReload::FullRequired);
    }
    update_js_session(ws, &mut cache.session, &root_plan.refs, reinferred, edits);
    let current = assemble_program_core(ws, &cache.scx, &root_plan.named, &cache.session)?;
    if current.global_layout != cache.build.global_layout {
        return Ok(JsReload::FullRequired);
    }
    if !cache
        .build
        .code_artifact
        .compatible_overlay(&current.code_artifact)
    {
        return Ok(JsReload::FullRequired);
    }
    let update = cache
        .session
        .last_update
        .as_ref()
        .expect("JS session update must publish its graph delta");
    if current.stable != cache.build.stable {
        return Ok(JsReload::FullRequired);
    }
    if current.const_chunks.keys().collect::<HashSet<_>>()
        != cache.build.const_chunks.keys().collect::<HashSet<_>>()
    {
        return Ok(JsReload::FullRequired);
    }

    let newly_reached_specs: BTreeSet<_> = current
        .spec_chunks
        .keys()
        .filter(|instance| !cache.build.spec_chunks.contains_key(*instance))
        .cloned()
        .collect();
    if !newly_reached_specs.is_subset(&update.added_instances) {
        return Err(CompilerError::new_no_location(
            "JS graph delta omitted a newly retained specialization".to_string(),
        ));
    }
    let stranded_specs: BTreeSet<_> = cache
        .build
        .spec_chunks
        .keys()
        .filter(|instance| !current.spec_chunks.contains_key(*instance))
        .cloned()
        .collect();
    if !stranded_specs.is_subset(&update.removed_instances) {
        return Err(CompilerError::new_no_location(
            "JS graph delta omitted a removed specialization".to_string(),
        ));
    }

    let mut changed_specs: Vec<&JsSpecChunk> = current
        .spec_chunks
        .iter()
        .filter_map(|(inst, chunk)| {
            if update.added_instances.contains(inst) {
                return Some(chunk);
            }
            cache
                .build
                .spec_chunks
                .get(inst)
                .and_then(|previous| (previous.js != chunk.js).then_some(chunk))
        })
        .collect();
    let mut removed_specs: Vec<&JsSpecChunk> = cache
        .build
        .spec_chunks
        .iter()
        .filter_map(|(inst, chunk)| update.removed_instances.contains(inst).then_some(chunk))
        .collect();
    changed_specs.sort_by(|left, right| left.name.cmp(&right.name));
    removed_specs.sort_by(|left, right| left.name.cmp(&right.name));

    let mut changed_now: Vec<FuncRef> = current
        .chunks
        .iter()
        .filter(|(fref, chunk)| {
            cache
                .build
                .chunks
                .get(fref)
                .is_none_or(|previous| previous.js != chunk.js)
        })
        .map(|(fref, _)| *fref)
        .collect();
    // The static resets below are emitted in this order.
    changed_now.sort_by_key(|fref| definition_order(*fref));
    let changed_consts: HashSet<ustr::Ustr> = current
        .const_chunks
        .iter()
        .filter_map(|(name, chunk)| {
            (cache.build.const_chunks.get(name) != Some(chunk)).then_some(*name)
        })
        .collect();
    if cache
        .build
        .lifecycle_plan
        .structure_changed(&current.lifecycle_plan)
    {
        return Ok(JsReload::FullRequired);
    }
    if changed_now.is_empty()
        && changed_consts.is_empty()
        && changed_specs.is_empty()
        && removed_specs.is_empty()
    {
        cache.build = current;
        return Ok(JsReload::NoChange);
    }
    cache.changed_since_full.extend(changed_now.iter().copied());

    let has_code_slots = current.code_artifact.has_escaping();
    let mut out = String::from("// skull JS incremental overlay\n");
    out.push_str("try {\n");
    let affected_consts = affected_const_names(ws, &changed_consts, &current.const_order);
    for name in &current.const_order {
        if !affected_consts.contains(name) {
            continue;
        }
        if changed_consts.contains(name) {
            out.push_str(&current.const_chunks[name]);
        } else {
            out.push_str(&current.const_init_lines[name]);
        }
    }
    let mut reset_cells = HashSet::new();
    for fref in &changed_now {
        let Some(chunk) = current.chunks.get(fref) else {
            continue;
        };
        for cell in &chunk.statics {
            let base = StaticBaseline::of(ws, cache.scx.interner(), cell).map_err(err)?;
            if cache
                .static_keys
                .get(&cell.cell)
                .is_none_or(|previous| previous.key != base.key)
                && reset_cells.insert(cell.cell)
            {
                out.push_str(&format!(
                    "delete __rock_globals[{}]; __rock_globals[{}] = false;\n",
                    js_string_lit(&base.value_slot)?,
                    js_string_lit(&base.done_slot)?,
                ));
            }
            cache.static_keys.insert(cell.cell, base);
        }
    }
    // A static an edit removed is held by no current chunk; its cells are
    // dropped so they stop holding their last value.
    let live: HashSet<ustr::Ustr> = current
        .chunks
        .values()
        .flat_map(|chunk| chunk.statics.iter().map(|cell| cell.cell))
        .collect();
    let mut dropped: Vec<StaticBaseline> = Vec::new();
    cache.static_keys.retain(|cell, base| {
        let keep = live.contains(cell);
        if !keep {
            dropped.push(base.clone());
        }
        keep
    });
    dropped.sort_by(|a, b| a.value_slot.cmp(&b.value_slot));
    for base in dropped {
        out.push_str(&format!(
            "delete __rock_globals[{}]; delete __rock_globals[{}];\n",
            js_string_lit(&base.value_slot)?,
            js_string_lit(&base.done_slot)?,
        ));
    }

    let mut order: Vec<FuncRef> = cache.changed_since_full.iter().copied().collect();
    order.sort_by_key(|fref| definition_order(*fref));
    for fref in &order {
        if let Some(chunk) = current.chunks.get(fref) {
            out.push_str(&chunk.js);
        }
    }

    for chunk in removed_specs {
        out.push_str(&format!("{} = undefined;\n", chunk.name));
    }
    for chunk in changed_specs {
        out.push_str(&chunk.js);
    }
    let code_slot_stage =
        code_slot_registration_js(ws, cache.scx.interner(), &current.code_artifact)?;
    out.push_str(&code_slot_stage);
    if !has_code_slots && current.lifecycle_plan.census.declarations > 0 {
        out.push_str("__rock_code_slot_begin();\n");
    }
    out.push_str(&lifecycle_plan_stage_js(
        ws,
        cache.scx.interner(),
        &current.lifecycle_plan,
    )?);
    out.push_str(code_slot_publish_js(
        &current.code_artifact,
        false,
        current.lifecycle_plan.census.declarations > 0,
    ));
    out.push_str("} catch (e) {\n");
    out.push_str("  __rock_code_slot_abort();\n");
    out.push_str("  throw e;\n");
    out.push_str("}\n");
    if let Some(wire) = find_plain_function(ws, "run_wire_systems") {
        let wire_name = callee_js_name(ws, wire);
        if current.chunks.contains_key(&wire) {
            out.push_str(&format!("{wire_name}();\n"));
        }
    }

    cache.build = current;
    Ok(JsReload::Overlay(out))
}

fn definition_order(fref: FuncRef) -> (usize, u32) {
    (fref.module.index(), u32::from(fref.func.into_raw()))
}

pub(super) fn collect_statics(function: &MirFunc) -> Vec<skull_mir::mir::StaticCell> {
    let mut cells = function.statics.clone();
    for nested in &function.nested_fns {
        cells.extend(collect_statics(nested));
    }
    cells
}

fn find_plain_function(ws: &Workspace, name: &str) -> Option<FuncRef> {
    ws.functions_by_name
        .get(&ustr::Ustr::from(name))?
        .iter()
        .find_map(|(module, function)| {
            let function_ref = &ws.module(*module).functions[*function];
            (function_ref.inline_body.is_none() && !function_ref.is_extern).then_some(FuncRef {
                module: *module,
                func: *function,
            })
        })
}
