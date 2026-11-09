//! Whole-program assembly and the program/cache types it produces.

use super::*;

pub(super) fn assemble_program_core(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    roots: &[(FuncRef, String)],
    session: &JsSession,
) -> Result<ProgramBuild, CompilerError> {
    let decorators = DecoratorTable::collect(ws);
    let mut st = WorklistState::new(
        ws,
        scx,
        roots,
        true,
        &session.graph,
        &session.view,
        session.concrete.plans.as_ref(),
        &session.concrete.always_plans,
    )?;
    let lifecycle_skeleton = skull_mir::lifecycle::collect(ws)?;
    queue_lifecycle_dependency_initializers(ws, &mut st, &lifecycle_skeleton)?;

    // Decorator DEFINITION bodies (`deco <kind> <name> { .. }` -> `__deco_<name>`),
    // for the APPLIED decorators only (D1 reachability: an unapplied `deco` -- e.g.
    // stdlib `on_reload` -- contributes no application line and must not force its
    // possibly-unsupported body into the program). Lowered via `BodyOwner::
    // Decorator`, then the SAME per-body pipeline a normal fn runs (incl. inlining,
    // so `@inline` no-ops like `transmute` splice away -- the luajit path leaves
    // them as top-level calls it emits whole-stdlib, which JS cannot). Their
    // callees/globals feed the shared worklists BEFORE the drain.
    let mut deco_bodies: Vec<(String, MirFunc)> = Vec::new();
    for (mid, did) in decorators.applied_definitions(ws, bone_core::hir::FnBackend::Js) {
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
        // THE shared discovery step, not a restatement of the parts of it a
        // decorator was thought to need. Enumerating callees and globals by
        // hand here left out the third thing `discover` does -- filing this
        // body's `spec_bindings` as demands -- so a specialization a deco
        // body was the only caller of was never queued, and the call site
        // spelled a name nothing emitted.
        let mut findings: Vec<CompilerError> = Vec::new();
        findings.extend(
            JsDelivery {
                ws,
                scx,
                st: &mut st,
            }
            .schedule_body(&mf)?,
        );
        match merge_findings(findings) {
            Some(e) => st.reject(&deco_name, e),
            None => deco_bodies.push((deco_name, mf)),
        }
    }

    drain_worklists(ws, scx, &mut st)?;
    st.canonicalize_emission_order();
    for (_, mf) in &st.lowered {
        register_code_slots(ws, scx.interner(), mf, &mut st.gctx);
    }
    for (_, mf) in &st.spec_lowered {
        register_code_slots(ws, scx.interner(), mf, &mut st.gctx);
    }
    for g in &st.ginits {
        register_code_slots(ws, scx.interner(), &g.mf, &mut st.gctx);
    }
    for (_, mf) in &deco_bodies {
        register_code_slots(ws, scx.interner(), mf, &mut st.gctx);
    }

    let mut rejections = std::mem::take(&mut st.rejections);
    let mut bodies = st.top_bodies();
    bodies.extend(deco_bodies.iter().map(|(_, mf)| mf));
    rejections.extend(check_scalar_abi(
        ws,
        scx,
        &st.lowered,
        &st.spec_lowered,
        &bodies,
        st.concrete,
        &st.artifact_decls,
    ));
    let hoisted = collect_hoisted(&bodies);
    let struct_registry = skull_mir::lower::StructRegistryPlan::for_bodies(
        ws,
        bone_core::hir::FnBackend::Js,
        &bodies,
    )
    .map_err(|error| err(error.message))?;

    let mut out = String::new();
    let mut stable = String::new();
    let mut code_artifact = CodeArtifact::default();
    let mut all_statics = Vec::new();
    for (name, mf) in &deco_bodies {
        let mut artifact = js_code_artifact(ws, mf);
        artifact.set_body_names(mf, name);
        code_artifact.merge(artifact);
        all_statics.extend(collect_statics(mf));
    }
    let enum_singletons = emit_enum_singletons(&st.gctx, &st.enum_defs);
    out.push_str(&enum_singletons);
    stable.push_str(&enum_singletons);
    let inspect_enum_payloads = emit_inspect_enum_payloads(ws)?;
    out.push_str(&inspect_enum_payloads);
    stable.push_str(&inspect_enum_payloads);
    let vt_eq = emit_vt_eq(ws, struct_registry.as_ref())?;
    out.push_str(&vt_eq);
    stable.push_str(&vt_eq);
    let inspect_ty = emit_inspect_ty(ws, struct_registry.as_ref())?;
    out.push_str(&inspect_ty);
    stable.push_str(&inspect_ty);
    let inspect_cdata_enums = emit_inspect_cdata_enums(ws)?;
    out.push_str(&inspect_cdata_enums);
    stable.push_str(&inspect_cdata_enums);
    let col_mat = emit_col_mat(ws, struct_registry.as_ref())?;
    out.push_str(&col_mat);
    stable.push_str(&col_mat);
    let ret_slots = ret_slot_decls(
        max_multi_wire(&bodies)?.max(max_published_lanes(&session.view, &st.inline_js)?),
    );
    out.push_str(&ret_slots);
    stable.push_str(&ret_slots);
    // 1. deco def fns.
    for (name, mf) in &deco_bodies {
        match emit_body_with_nested(ws, mf, name, &st.names, &hoisted, &st.gctx) {
            Ok(definition) => {
                out.push_str(&definition);
                stable.push_str(&definition);
            }
            Err(e) => rejections.push((name.clone(), e)),
        }
    }
    // 2. fn defs (main, decorated fns, transitive callees).
    let mut chunks = HashMap::new();
    for (fr, mf) in &st.lowered {
        let name = st
            .names
            .name_for_decl(ws, *fr)
            .expect("emitted body has a name");
        let plan = session
            .concrete
            .plans
            .get(&Instance::base(*fr))
            .ok_or_else(|| err(format!("JS: no concrete plan for `{name}`")))?;
        match emit_lowered_fn(
            ws,
            scx.interner(),
            mf,
            name,
            plan,
            &st.names,
            &hoisted,
            &st.gctx,
        ) {
            Ok((js, artifact)) => {
                out.push_str(&js);
                code_artifact.merge(artifact.clone());
                all_statics.extend(collect_statics(mf));
                chunks.insert(
                    *fr,
                    JsChunk {
                        js,
                        statics: collect_statics(mf),
                    },
                );
            }
            Err(e) => rejections.push((name.clone(), e)),
        }
    }
    // 2a. standalone specialized copies. They are keyed by Instance so an
    // overlay can add, remove, or rebuild one without confusing it with the
    // shared generic declaration.
    let spec_chunks = emit_spec_chunks(ws, scx.interner(), &st, &hoisted, &mut rejections);
    for chunk in ordered_spec_chunks(&spec_chunks, &st.spec_lowered) {
        out.push_str(&chunk.js);
        code_artifact.merge(chunk.code_artifact.clone());
    }
    for (_, mf) in &st.spec_lowered {
        all_statics.extend(collect_statics(mf));
    }
    // 2b. inline(js) fns, emitted verbatim.
    for body in &st.inline_js {
        let name = st
            .names
            .get(&body.instance)
            .expect("emitted body has a name");
        match emit_inline_js_fn(ws, scx, st.concrete, &body.instance, body.decl, name) {
            Ok(definition) => {
                out.push_str(&definition);
                stable.push_str(&definition);
            }
            Err(e) => rejections.push((name.clone(), e)),
        }
    }
    // 3. ginit def fns.
    let mut const_chunks = HashMap::new();
    for g in &st.ginits {
        let definition = match emit_ginit_with_nested(ws, g, &st.names, &hoisted, &st.gctx) {
            Ok(js) => js,
            Err(e) => {
                rejections.push((g.init_name.clone(), e));
                continue;
            }
        };
        out.push_str(&definition);
        let mut artifact = js_code_artifact(ws, &g.mf);
        artifact.set_body_names(&g.mf, &g.init_name);
        code_artifact.merge(artifact);
        all_statics.extend(collect_statics(&g.mf));
        if g.is_const {
            const_chunks
                .entry(g.name)
                .or_insert_with(String::new)
                .push_str(&definition);
        } else {
            stable.push_str(&definition);
        }
    }
    // 4. load-time init lines (dependency-ordered).
    let mut const_order = Vec::new();
    let mut const_init_lines = HashMap::new();
    let init_lines = order_global_inits(&st.ginits, &st.lowered, &st.names, &st.gctx.state)?;
    out.push_str("function __rock_state_initialize() {\n");
    stable.push_str("function __rock_state_initialize() {\n");
    for init in &init_lines {
        out.push_str(&init.js);
        if !const_chunks.contains_key(&init.name) {
            stable.push_str(&init.js);
        }
    }
    // A global initialized from a lambda stores that lambda's CodeRef, so the
    // initializers run at the CALL below, after the code slots are staged.
    out.push_str("}\n");
    stable.push_str("}\n");
    for init in init_lines {
        if let Some(chunk) = const_chunks.get_mut(&init.name) {
            chunk.push_str(&init.js);
            const_order.push(init.name);
            const_init_lines.insert(init.name, init.js);
        }
    }
    // Rejected callback bodies have no code entry; report their named errors
    // before staging would replace them with an incomplete-census error.
    combine_rejections(rejections)?;
    decorators.mark_generated_code_values(
        ws,
        bone_core::hir::FnBackend::Js,
        SimValues::All,
        &mut code_artifact,
    );
    let definition_plan = skull_mir::definition::collect_complete(ws)?;
    let lifecycle_plan = skull_mir::lifecycle::attach_artifact(lifecycle_skeleton, &code_artifact)?;
    let has_definition_plan = !definition_plan.catalogs.is_empty();
    let code_slot_stage = code_slot_registration_js(ws, scx.interner(), &code_artifact)?;
    out.push_str(&code_slot_stage);
    if code_artifact
        .publication(has_definition_plan, lifecycle_plan.census.declarations > 0)
        .explicit_begin
    {
        out.push_str("__rock_code_slot_begin();\n");
    }
    // Global initializers run once the slots they may reference are staged, and
    // before the definition plan materializes (a materializer may read them).
    out.push_str("__rock_state_initialize();\n");
    let definition_stage = definition_plan_stage_js(ws, scx, st.concrete, &definition_plan)?;
    out.push_str(&definition_stage);
    stable.push_str(&definition_stage);
    out.push_str(&lifecycle_plan_stage_js(
        ws,
        scx.interner(),
        &lifecycle_plan,
    )?);
    // 5. decorator applications (AFTER inits: a deco body pushes into a ginit-
    //    initialized registry).
    let applications = decorator_application_lines_js(ws, &decorators, scx.interner(), &st.names)?;
    out.push_str(&applications);
    stable.push_str(&applications);
    out.push_str(code_slot_publish_js(
        &code_artifact,
        has_definition_plan,
        lifecycle_plan.census.declarations > 0,
    ));
    let global_layout = skull_mir::global_storage::global_layout_id(ws, scx).map_err(err)?;
    let program = format!(
        "{}{out}",
        globals_open_js(ws, scx, &st.gctx.state, &global_layout)?
    );

    Ok(ProgramBuild {
        program,
        stable,
        chunks,
        spec_chunks,
        code_artifact,
        lifecycle_plan,
        global_layout,
        const_chunks,
        const_order,
        const_init_lines,
    })
}

/// Opens (or reattaches to) the global storage and publishes the directory.
/// Every program runs this before any initializer.
pub(super) fn globals_open_js(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    state: &HashMap<ustr::Ustr, String>,
    global_layout: &str,
) -> Result<String, CompilerError> {
    Ok(format!(
        "__rock_globals_open({});\n{}",
        js_string_lit(global_layout)?,
        globals_directory_js(ws, scx, state)?
    ))
}

/// The global directory reflection and the `__G` bridge read.
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
            .collect::<Result<Vec<_>, _>>()?
            .join(",");
        let slots = entry
            .slots
            .iter()
            .map(|slot| js_string_lit(slot))
            .collect::<Result<Vec<_>, _>>()?
            .join(",");
        out.push_str(&format!(
            "{{name:{},identity:{},physical:[{physical}],slots:[{slots}]}},",
            js_string_lit(&entry.name)?,
            js_string_lit(&entry.identity)?,
        ));
    }
    out.push_str("]);\n");
    Ok(out)
}

/// Select the JS-served registration declaration for shader driver lines.
pub(super) fn shader_register_sibling(ws: &Workspace) -> Result<Option<FuncRef>, CompilerError> {
    if skull_mir::lower::collect_shader_pairs(ws)?.is_empty() {
        return Ok(None);
    }
    // The registry uses DefKeys for v1 modules. @nomangle changes the emitted
    // symbol, not that key, so match the declared segment on both dialects.
    let cands: Vec<_> = ws
        .functions_by_name
        .iter()
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
            return Ok(Some(FuncRef { module, func }));
        }
    }
    Err(err(
        "JS: `rock_shader` pipelines emit `__rock_shader_register` calls, but no \
         JS-servable `__rock_shader_register` is reachable (inline(js)/extern(js) \
         sibling or a plain-Rock body); refusing to drop the shader registration",
    ))
}

/// A whole-program JS emission plus the entry fn's JS name (`None` when the entry
/// module has no nullary `main`; the driver then appends nothing and the program
/// is module-load effects only, matching luajit).
pub struct JsProgram {
    pub program: String,
    pub main_js_name: Option<String>,
    /// Compiler-owned logical callable identities for the emitted JS artifact.
    pub code_artifact: CodeArtifact,
    /// Compiler-owned lifecycle metadata shared with the Lua artifact.
    pub lifecycle_plan: skull_mir::lifecycle::LifecyclePlan,
    /// The mutable-global layout fingerprint the program opens its storage with.
    pub global_layout: String,
}

impl JsProgram {
    /// True when a callable escapes as a code ref, so the prelude needs the
    /// full code-slot call helper (see [`crate::assemble_prelude`]).
    pub fn has_code_refs(&self) -> bool {
        self.code_artifact.has_escaping()
    }
}

#[derive(Clone)]
pub(super) struct JsChunk {
    pub(super) js: String,
    pub(super) statics: Vec<skull_mir::mir::StaticCell>,
}

#[derive(Clone)]
pub(super) struct JsSpecChunk {
    pub(super) name: String,
    pub(super) js: String,
    pub(super) code_artifact: CodeArtifact,
}

#[derive(Clone)]
pub(super) struct ProgramBuild {
    pub(super) program: String,
    pub(super) stable: String,
    pub(super) chunks: HashMap<FuncRef, JsChunk>,
    pub(super) spec_chunks: HashMap<Instance, JsSpecChunk>,
    pub(super) code_artifact: CodeArtifact,
    pub(super) lifecycle_plan: skull_mir::lifecycle::LifecyclePlan,
    pub(super) global_layout: String,
    pub(super) const_chunks: HashMap<ustr::Ustr, String>,
    pub(super) const_order: Vec<ustr::Ustr>,
    pub(super) const_init_lines: HashMap<ustr::Ustr, String>,
}

#[derive(Clone)]
pub struct JsEmitCache {
    pub(super) build: ProgramBuild,
    pub(super) changed_since_full: HashSet<FuncRef>,
    pub(super) static_keys: HashMap<ustr::Ustr, StaticBaseline>,
    pub(super) shader_contributions: HashMap<
        (bone_core::prelude::ModuleId, bone_core::hir::BodyOwner),
        Vec<skull_mir::lower::ShaderPair>,
    >,
    pub(super) root_plan: JsRootPlan,
    pub(super) opaque_root_contributions:
        BTreeMap<skull_graph::instance_graph::DeclKey, rustc_hash::FxHashSet<FuncRef>>,
    /// The session's lowering context, graph, and artifact view, kept across edits.
    pub(super) scx: skull_mir::lower::ScalarCtx,
    pub(super) session: JsSession,
}

impl JsEmitCache {
    /// The session arena. Handed out so a caller can prove two runs share one;
    /// `Arc::ptr_eq` on it is the identity, `len()` the population.
    pub fn interner_handle(&self) -> std::sync::Arc<skull_mir::monoty::MonoInterner> {
        self.scx.interner_handle()
    }

    pub fn graph_population(&self) -> usize {
        self.session.graph.instances.len()
    }

    pub fn graph(&self) -> &InstanceGraph {
        &self.session.graph
    }

    pub fn graph_update(&self) -> Option<JsGraphUpdate> {
        self.session.last_update.clone()
    }

    pub fn global_layout(&self) -> &str {
        &self.build.global_layout
    }

    pub fn lifecycle_plan(&self) -> &skull_mir::lifecycle::LifecyclePlan {
        &self.build.lifecycle_plan
    }

    /// The session context's ABI-plan memo counters (parity with the Lua
    /// path's `MirEmit::abi_plan_stats`).
    pub fn abi_plan_stats(&self) -> skull_mir::lower::AbiPlanMemoStats {
        self.scx.abi_plan_stats()
    }
}

pub enum JsReload {
    Overlay(String),
    FullRequired,
    NoChange,
}

impl JsReload {
    pub fn expect_overlay(self) -> String {
        match self {
            Self::Overlay(js) => js,
            Self::FullRequired => panic!("JS overlay required a full reload"),
            Self::NoChange => panic!("JS overlay reported no emitted change"),
        }
    }
}
