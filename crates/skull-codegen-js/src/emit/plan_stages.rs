//! Code-slot registration and the definition and lifecycle plan stages.

use super::*;

/// Program-wide hoisted closure-body names -- the only globals a capture-free
/// lambda creation site may reference.
pub(super) fn collect_hoisted(bodies: &[&MirFunc]) -> HashSet<ustr::Ustr> {
    pub(super) fn visit(mf: &MirFunc, names: &mut HashSet<Ustr>) {
        for nested in &mf.nested_fns {
            names.insert(nested.name);
            visit(nested, names);
        }
    }
    let mut names = HashSet::new();
    for body in bodies {
        visit(body, &mut names);
    }
    names
}

pub(super) fn register_code_slots(
    ws: &Workspace,
    interner: &skull_mir::monoty::MonoInterner,
    mf: &MirFunc,
    gctx: &mut GlobalCtx,
) {
    if let Some(identity) = &mf.code_identity {
        gctx.code_slots
            .entry(identity.clone())
            .or_insert_with(|| identity.logical_key(ws, interner));
    }
    for identity in mf.code_value_identities.values() {
        gctx.code_slots
            .entry(identity.clone())
            .or_insert_with(|| identity.logical_key(ws, interner));
    }
    for nested in &mf.nested_fns {
        register_code_slots(ws, interner, nested, gctx);
    }
}

pub(super) fn code_slot_registration_js(
    ws: &Workspace,
    interner: &skull_mir::monoty::MonoInterner,
    artifact: &CodeArtifact,
) -> Result<String, CompilerError> {
    artifact.check_stageable().map_err(err)?;
    let slots =
        skull_mir::code_identity::staged_code_slots(ws, interner, artifact.entries.values())
            .map_err(err)?;
    if slots.is_empty() {
        return Ok(String::new());
    }
    let mut out = String::new();
    out.push_str("__rock_code_slot_begin();\n");
    for slot in slots {
        writeln!(
            out,
            "__rock_code_slot_stage_one({}, {}, {});",
            js_string_lit(&slot.key)?,
            slot.body,
            js_string_lit(slot.call_abi)?,
        )
        .expect("writing a String cannot fail");
    }
    Ok(out)
}

pub(super) fn code_slot_publish_js(
    artifact: &CodeArtifact,
    has_definition_plan: bool,
    has_lifecycle_plan: bool,
) -> &'static str {
    if artifact
        .publication(has_definition_plan, has_lifecycle_plan)
        .publish
    {
        "__rock_code_slot_publish();\n"
    } else {
        ""
    }
}

pub(super) fn definition_plan_stage_js(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    concrete: &skull_mir::concrete::ConcretePlans,
    plan: &skull_mir::definition::DefinitionPlan,
) -> Result<String, CompilerError> {
    if plan.catalogs.is_empty() {
        return Ok(String::new());
    }
    let mut out = String::new();
    let entries_by_catalog = plan.entries_by_catalog();
    let mut materializers = BTreeMap::new();
    for entry in &plan.entries {
        let skull_mir::code_identity::CodeIdentity::Named(factory) = &entry.identity else {
            return Err(err("definition factory has no named identity"));
        };
        let mut mf = skull_mir::lower::lower_definition_factory(ws, scx, concrete, factory)?;
        skull_transform::cleanup_with_nobox(ws, &mut mf)?;
        materializers.insert(entry.identity.clone(), emit_fn(ws, &mf, "")?);
    }
    for catalog_plan in &plan.catalogs {
        if let Some(accessor) = catalog_plan.accessor {
            let name = skull_mir::mangle::mangle_fn(
                ws,
                &ws.module(accessor.module).functions[accessor.func],
            );
            writeln!(
                out,
                "function {}() {{ return __rock_definition_catalog({}); }}",
                name,
                js_string_lit(catalog_plan.catalog.as_str())?,
            )
            .unwrap();
        }
    }
    out.push_str("__rock_definition_plan_stage({\n");
    for catalog_plan in &plan.catalogs {
        let entries = entries_by_catalog
            .get(&catalog_plan.catalog)
            .cloned()
            .unwrap_or_default();
        writeln!(
            out,
            "{}: {{catalog: {}, len: {}, _data: [",
            js_string_lit(catalog_plan.catalog.as_str())?,
            js_string_lit(catalog_plan.catalog.as_str())?,
            entries.len()
        )
        .unwrap();
        for entry in entries {
            let key = entry.identity.logical_key(ws, scx.interner());
            writeln!(
                out,
                "{{catalog: {}, key: {}, name: {}, ordinal: {}, factory: __rock_code_slot_pending_ref({})}},",
                js_string_lit(entry.catalog.as_str())?,
                js_string_lit(entry.key.as_str())?,
                js_string_lit(entry.diagnostic_name.as_str())?,
                entry.declaration_ordinal,
                js_string_lit(&key)?,
            )
            .unwrap();
        }
        out.push_str("]},\n");
    }
    out.push_str("}, {\n");
    for (identity, body) in materializers {
        writeln!(
            out,
            "{}: {},",
            js_string_lit(&identity.logical_key(ws, scx.interner()))?,
            body
        )
        .unwrap();
    }
    out.push_str("});\n");
    Ok(out)
}

pub(super) fn lifecycle_plan_stage_js(
    ws: &Workspace,
    interner: &skull_mir::monoty::MonoInterner,
    plan: &skull_mir::lifecycle::LifecyclePlan,
) -> Result<String, CompilerError> {
    if plan.census.declarations == 0 {
        return Ok(String::new());
    }
    let mut out = String::from("__rock_lifecycle_plan_stage({\n");
    for (phase, entries) in plan.entries_by_phase() {
        let scene_owned = entries
            .iter()
            .filter(|entry| entry.owner.as_str() == "scene")
            .count();
        writeln!(
            out,
            "[{}]: {{len: {}, scene_owned: {}, _data: [",
            js_string_lit(phase.as_str())?,
            entries.len(),
            scene_owned
        )
        .unwrap();
        for entry in entries {
            let key = entry.identity.logical_key(ws, interner);
            let dependencies = entry.qualification.dependencies();
            let root_type = match &entry.root_type {
                Some(root) => js_string_lit(
                    &skull_mir::lifecycle::scene_root_type_key(root).unwrap_or_else(|| {
                        panic!("validated scene lifecycle root reached emission as {root:?}")
                    }),
                )?,
                None => "null".to_string(),
            };
            write!(out, "{{phase: {}, owner: {}, root_type: {}, key: {}, name: {}, stage: {}, ordinal: {}, qualification: {}, body_fingerprint: {}, dependencies: {{len: {}, _data: [",
                js_string_lit(entry.phase.as_str())?, js_string_lit(entry.owner.as_str())?,
                root_type,
                js_string_lit(&key)?, js_string_lit(entry.diagnostic_name.as_str())?,
                entry.stage, entry.declaration_ordinal,
                js_string_lit(entry.qualification.tag())?,
                js_string_lit(&format!("{:016x}", entry.body_fingerprint))?,
                dependencies.len()).unwrap();
            for dependency in dependencies {
                let dependency_key = dependency.identity.logical_key(ws, interner);
                write!(
                    out,
                    "{{key: {}, fingerprint: {}}},",
                    js_string_lit(&dependency_key)?,
                    js_string_lit(&format!("{:016x}", dependency.fingerprint))?
                )
                .unwrap();
            }
            // A compiler-synthesized thunk of one uniform signature
            // (`scene_id -> ()`) per entry -- see the Lua sibling in
            // `rock-lang/crates/skull/src/mir_emit.rs::lifecycle_plan_stage_lua` for
            // the full rationale. A scene thunk resolves the live root for
            // `scene_id` and calls through; it never asks the value its
            // runtime type.
            let key_lit = js_string_lit(&key)?;
            let sim_guard = (!entry.sim_wires.is_empty())
                .then(|| {
                    entry
                        .sim_wires
                        .iter()
                        .map(|wire| {
                            Ok(format!(
                                "__G.__ROCK_ACTIVE_SIM === {}",
                                js_string_lit(wire.as_str())?
                            ))
                        })
                        .collect::<Result<Vec<_>, CompilerError>>()
                        .map(|parts| parts.join(" || "))
                })
                .transpose()?;
            if entry.owner.as_str() == "scene" {
                out.push_str("]}, func: (function(){var __s = __rock_code_slot_ref(");
                out.push_str(&key_lit);
                out.push_str("); return function(scene_id){");
                if let Some(guard) = &sim_guard {
                    write!(out, "if ({guard}) {{").unwrap();
                }
                out.push_str("var __root = __scene_root_any(scene_id); if (__root !== null) { __rock_call(__s, __root); }");
                if sim_guard.is_some() {
                    out.push('}');
                }
                out.push_str("};})()},\n");
            } else {
                out.push_str("]}, func: (function(){var __s = __rock_code_slot_ref(");
                out.push_str(&key_lit);
                out.push_str("); return function(__scene_id){");
                if let Some(guard) = &sim_guard {
                    write!(out, "if ({guard}) {{").unwrap();
                }
                out.push_str("__rock_call(__s);");
                if sim_guard.is_some() {
                    out.push('}');
                }
                out.push_str("};})()},\n");
            }
        }
        out.push_str("]},\n");
    }
    out.push_str("});\n");
    Ok(out)
}
