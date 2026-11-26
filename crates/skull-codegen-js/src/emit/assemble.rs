use super::*;

pub(super) fn assemble_program_core(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    roots: &[(FuncRef, String)],
    session: &JsSession,
    program_mode: bool,
) -> Result<ProgramBuild, CompilerError> {
    if program_mode
        && let Some((_, error)) = skull_mir::mangle::collision_errors_of(ws, [])
            .into_iter()
            .next()
    {
        return Err(error);
    }
    let mut st = WorklistState::new(ws, scx, roots, program_mode, session)?;
    let effects = if program_mode {
        let lifecycle = skull_mir::lifecycle::collect(ws)?;
        queue_lifecycle_dependency_initializers(ws, &mut st, &lifecycle)?;
        Some((DecoratorTable::collect(ws), lifecycle))
    } else {
        None
    };
    let mut deco_bodies = Vec::new();
    for (mid, did) in effects.iter().flat_map(|(decorators, _)| {
        decorators.applied_definitions(ws, bone_core::hir::FnBackend::Js)
    }) {
        let d = &ws.module(mid).decorators[did];
        let deco_name = decorator_symbol(d.def_key.as_str());
        let owner = skull_mir::concrete::ConcreteOwner::Decorator {
            module: mid,
            index: did.into_raw().into_u32(),
        };
        let concrete_plan = st.always.get(&owner);
        let lowered = concrete_plan
            .ok_or_else(|| err(format!("JS: no concrete plan for decorator `{}`", d.name)))
            .and_then(|plan| {
                skull_mir::lower::lower_decorator_with_plans(
                    ws,
                    scx,
                    plan,
                    st.concrete,
                    ustr::Ustr::from(deco_name.as_str()),
                )
            });
        let mut mf = match lowered {
            Ok(mf) => mf,
            Err(e) => {
                st.reject(&deco_name, e);
                continue;
            }
        };
        let self_ref = safe_self_ref(ws, &mf);
        if let Err(e) = prepare_body(
            ws,
            scx,
            self_ref,
            &mut mf,
            &deco_name,
            &mut st.cache,
            st.concrete,
        ) {
            st.reject(&deco_name, e);
            continue;
        }
        let findings = JsDelivery {
            ws,
            scx,
            st: &mut st,
        }
        .schedule_body(&mf)?;
        match merge_findings(findings) {
            Some(e) => st.reject(&deco_name, e),
            None => deco_bodies.push((deco_name, mf)),
        }
    }

    let deco_mfs: Vec<&MirFunc> = deco_bodies.iter().map(|(_, mf)| mf).collect();
    let mut rejections = st.finish(ws, scx, &deco_mfs)?;
    let bodies = st.top_bodies().chain(deco_mfs).collect::<Vec<_>>();
    let (hoisted, tables) = check_bodies(ws, scx, &st, &bodies, &mut rejections)?;

    let mut out = String::new();
    let mut code_artifact = CodeArtifact::default();
    for (name, mf) in &deco_bodies {
        let mut artifact = js_code_artifact(ws, scx.interner(), mf);
        artifact.set_body_names(mf, name);
        code_artifact.merge(artifact);
    }
    out.push_str(&tables);
    if program_mode {
        out.push_str(&st.json_types.render());
    }
    let ret_slots = ret_slot_decls(
        max_multi_wire(&bodies)?.max(max_published_lanes(&session.view, &st.inline_js)?),
    );
    out.push_str(&ret_slots);
    for (name, mf) in &deco_bodies {
        match emit_body_with_nested(ws, mf, name, &st.names, &hoisted, &st.gctx) {
            Ok(definition) => out.push_str(&definition),
            Err(e) => rejections.push((name.clone(), e)),
        }
    }
    for body in &st.lowered {
        match emit_lowered_fn(
            ws,
            scx.interner(),
            &body.mf,
            &body.name,
            body.plan,
            &st.names,
            &hoisted,
            &st.gctx,
        ) {
            Ok((js, artifact)) => {
                out.push_str(&js);
                if program_mode || body.specialization.is_none() {
                    code_artifact.merge(artifact);
                }
            }
            Err(error) => rejections.push((body.name.clone(), error)),
        }
    }
    for body in &st.inline_js {
        let name = st
            .names
            .get(&body.instance)
            .expect("emitted body has a name");
        match emit_inline_js_fn(ws, scx, st.concrete, &body.instance, body.decl, name) {
            Ok(definition) => out.push_str(&definition),
            Err(e) => rejections.push((name.clone(), e)),
        }
    }
    for g in &st.ginits {
        let definition = match emit_ginit_with_nested(ws, g, &st.names, &hoisted, &st.gctx) {
            Ok(js) => js,
            Err(e) => {
                rejections.push((g.init_name.clone(), e));
                continue;
            }
        };
        out.push_str(&definition);
        let mut artifact = js_code_artifact(ws, scx.interner(), &g.mf);
        artifact.set_body_names(&g.mf, &g.init_name);
        code_artifact.merge(artifact);
    }
    let init_lines = order_global_inits(&st.ginits, &st.lowered, &st.names, &st.gctx.state)?;
    // Report rejected bodies before staging could raise an incomplete-census error.
    combine_rejections(rejections)?;
    let lifecycle_plan = if let Some((decorators, lifecycle_skeleton)) = effects {
        out.push_str("function __rock_state_initialize() {\n");
        out.extend(init_lines.iter().map(String::as_str));
        // A global initialized from a lambda stores its CodeRef, so initializers run after the code
        // slots are staged.
        out.push_str("}\n");
        decorators.mark_generated_code_values(
            ws,
            bone_core::hir::FnBackend::Js,
            SimValues::All,
            &mut code_artifact,
        );
        let definition_plan = skull_mir::definition::collect(ws)?;
        let lifecycle_plan =
            skull_mir::lifecycle::attach_artifact(lifecycle_skeleton, &code_artifact)?;
        let has_definition_plan = !definition_plan.catalogs.is_empty();
        let code_slot_stage = code_slot_registration_js(ws, scx.interner(), &code_artifact)?;
        out.push_str(&code_slot_stage);
        if code_artifact
            .publication(has_definition_plan, lifecycle_plan.declarations > 0)
            .explicit_begin
        {
            out.push_str("__rock_code_slot_begin();\n");
        }
        out.push_str("__rock_state_initialize();\n");
        // Global initializers run once the slots they may reference are staged, and before the
        // definition plan materializes (a materializer may read them).
        let definition_stage = definition_plan_stage_js(ws, scx, st.concrete, &definition_plan)?;
        out.push_str(&definition_stage);
        out.push_str(&lifecycle_plan_stage_js(
            ws,
            scx.interner(),
            &lifecycle_plan,
        )?);
        // Decorator applications run after inits: a deco body pushes into a ginit-initialized
        // registry.
        let applications =
            decorator_application_lines_js(ws, &decorators, scx.interner(), &st.names)?;
        out.push_str(&applications);
        out.push_str(code_slot_publish_js(
            &code_artifact,
            has_definition_plan,
            lifecycle_plan.declarations > 0,
        ));
        lifecycle_plan
    } else {
        out.push_str(&code_slot_registration_js(
            ws,
            scx.interner(),
            &code_artifact,
        )?);
        out.extend(init_lines.iter().map(String::as_str));
        out.push_str(code_slot_publish_js(&code_artifact, false, false));
        Default::default()
    };
    let (program, global_layout) = if program_mode || st.gctx.uses_storage.get() {
        let layout = skull_mir::global_storage::global_layout_id(ws, scx).map_err(err)?;
        let program = format!(
            "{}{out}",
            globals_open_js(ws, scx, &st.gctx.state, &layout)?
        );
        (program, layout)
    } else {
        (out, String::new())
    };

    Ok(ProgramBuild {
        program,
        code_artifact,
        lifecycle_plan,
        global_layout,
    })
}

pub(super) fn check_bodies(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    st: &WorklistState,
    bodies: &[&MirFunc],
    rejections: &mut Vec<(String, CompilerError)>,
) -> Result<(HashSet<ustr::Ustr>, String), CompilerError> {
    rejections.extend(check_scalar_abi(ws, scx, st, bodies));
    let hoisted = collect_hoisted(bodies);
    let registry =
        skull_mir::lower::StructRegistryPlan::for_bodies(ws, bone_core::hir::FnBackend::Js, bodies)
            .map_err(|error| err(error.message))?;
    let registry = registry.as_ref();
    let mut tables = emit_enum_singletons(&st.gctx, &st.enum_defs);
    tables.push_str(&emit_inspect_enum_payloads(ws, scx.rep_arena()));
    tables.push_str(&emit_vt_eq(ws, registry)?);
    tables.push_str(&emit_tostring_ty(ws, registry)?);
    tables.push_str(&emit_inspect_ty(ws, registry)?);
    tables.push_str(&emit_inspect_cdata_enums(ws)?);
    tables.push_str(&emit_col_mat(ws, registry)?);
    Ok((hoisted, tables))
}

pub(super) fn globals_open_js(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    state: &HashMap<ustr::Ustr, String>,
    global_layout: &str,
) -> Result<String, CompilerError> {
    Ok(format!(
        "__rock_globals_open({});\n{}",
        js_string_lit(global_layout),
        globals_directory_js(ws, scx, state)?
    ))
}

fn globals_directory_js(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    state_slots: &HashMap<ustr::Ustr, String>,
) -> Result<String, CompilerError> {
    let entries = skull_mir::global_storage::global_directory(ws, scx, |leaf| {
        state_slots.get(&leaf).cloned()
    })
    .map_err(err)?;
    let mut out = String::from("__rock_globals_publish([");
    for entry in entries {
        let physical = entry
            .physical
            .iter()
            .map(|leaf| js_string_lit(leaf))
            .collect::<Vec<_>>()
            .join(",");
        let slots = entry
            .slots
            .iter()
            .map(|slot| js_string_lit(slot))
            .collect::<Vec<_>>()
            .join(",");
        out.push_str(&format!(
            "{{name:{},identity:{},physical:[{physical}],slots:[{slots}]}},",
            js_string_lit(&entry.name),
            js_string_lit(&entry.identity),
        ));
    }
    out.push_str("]);\n");
    Ok(out)
}

pub(super) fn shader_register_sibling(ws: &Workspace) -> Result<FuncRef, CompilerError> {
    let cands: Vec<_> = ws
        .functions_by_name
        .iter()
        // Scoped packages key the registry by DefKey; @nomangle changes the emitted symbol, not
        // that key, so match the declared segment.
        .filter(|(key, _)| skull_mir::mangle::declared_name(key) == "__rock_shader_register")
        .flat_map(|(_, functions)| functions.iter().copied())
        .collect();
    for js_sibling in [true, false] {
        let mut matches = cands.iter().filter(|(m, f)| {
            let cf = &ws.module(*m).functions[*f];
            if js_sibling {
                cf.backend == bone_core::hir::FnBackend::Js
                    && (matches!(cf.inline_body, Some((bone_core::hir::FnBackend::Js, _)))
                        || cf.is_extern)
            } else {
                cf.inline_body.is_none() && !cf.is_extern
            }
        });
        if let Some(&(module, func)) = matches.next() {
            if let Some(&(other_module, other_func)) = matches.next() {
                return Err(err(format!(
                    "JS: ambiguous `__rock_shader_register`: `{}` and `{}` would own the same driver symbol",
                    ws.module(module).functions[func].name,
                    ws.module(other_module).functions[other_func].name,
                )));
            }
            return Ok(FuncRef { module, func });
        }
    }
    Err(err(
        "JS: `rock_shader` pipelines emit `__rock_shader_register` calls, but no \
         JS-servable `__rock_shader_register` is reachable (inline(js)/extern(js) \
         sibling or a plain-Rock body); refusing to drop the shader registration",
    ))
}

pub struct JsProgram {
    pub program: String,
    pub main_js_name: Option<String>,
    pub code_artifact: CodeArtifact,
    pub lifecycle_plan: skull_mir::lifecycle::LifecyclePlan,
    pub global_layout: String,
}

impl JsProgram {
    pub fn has_code_refs(&self) -> bool {
        self.code_artifact.has_escaping()
    }
}

pub(super) struct ProgramBuild {
    pub(super) program: String,
    pub(super) code_artifact: CodeArtifact,
    pub(super) lifecycle_plan: skull_mir::lifecycle::LifecyclePlan,
    pub(super) global_layout: String,
}
