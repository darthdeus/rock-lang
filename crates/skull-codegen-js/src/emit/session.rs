//! Root plans and the cached instance-graph session behind a program emission.

use super::*;

/// One lowering context supplies the interner for the graph, view and emitter.
/// Live reload keeps that interner so retained instances stay valid.
pub(super) fn js_scx(ws: &Workspace) -> skull_mir::lower::ScalarCtx {
    skull_mir::lower::ScalarCtx::new(ws).with_target(bone_core::hir::FnBackend::Js)
}

#[derive(Clone)]
struct JsRootPlan {
    named: Vec<(FuncRef, String)>,
    refs: Vec<FuncRef>,
}

#[derive(Clone)]
pub(super) struct JsSession {
    root_plan: JsRootPlan,
    pub(super) graph: InstanceGraph,
    pub(super) view: ArtifactView,
    pub(super) concrete: skull_graph::concrete_body::ConcreteBodies,
}

fn js_root_plan(
    ws: &Workspace,
    extra_roots: &[(FuncRef, String)],
    program_mode: bool,
    shader_pairs: &[skull_mir::lower::ShaderPair],
) -> Result<JsRootPlan, CompilerError> {
    let mut named = extra_roots.to_vec();
    let mut seen: HashSet<FuncRef> = named.iter().map(|(fr, _)| *fr).collect();
    if program_mode {
        let decorators = DecoratorTable::collect(ws);
        for module in ws.source_modules() {
            for (fid, f) in module.functions.iter() {
                if decorators.is_decorated(f) {
                    let fr = FuncRef {
                        module: module.id,
                        func: fid,
                    };
                    if seen.insert(fr) {
                        named.push((fr, callee_js_name(ws, fr)));
                    }
                }
            }
        }
        // Driver registration calls are outside MIR, so root their target under
        // the bare symbol the driver calls, before building the artifact graph.
        if !shader_pairs.is_empty() {
            let fr = shader_register_sibling(ws)?;
            if seen.insert(fr) {
                named.push((fr, "__rock_shader_register".to_string()));
            }
        }
    }
    let mut refs: Vec<FuncRef> = named.iter().map(|(fr, _)| *fr).collect();
    for shader_root in shader_pairs
        .iter()
        .flat_map(|pair| pair.vert.into_iter().chain([pair.frag]))
    {
        if seen.insert(shader_root) {
            refs.push(shader_root);
        }
    }
    push_synthesized_entry_roots(ws, &mut refs, &mut seen);
    push_json_factory_roots(ws, &mut refs, &mut seen);
    Ok(JsRootPlan { named, refs })
}

/// Graph roots only: a typed-JSON record walker calls these field factories
/// from opaque JS, and the worklist emits one once a walker reaches its field.
fn push_json_factory_roots(ws: &Workspace, refs: &mut Vec<FuncRef>, seen: &mut HashSet<FuncRef>) {
    for module in ws.modules.iter() {
        for (fid, f) in module.functions.iter() {
            let fr = FuncRef {
                module: module.id,
                func: fid,
            };
            if f.json_field_for.is_some() && seen.insert(fr) {
                refs.push(fr);
            }
        }
    }
}

/// Graph roots only: a synthesized call resolves to the entry's base instance,
/// and the worklist emits a definition only for a call that survives lowering.
fn push_synthesized_entry_roots(
    ws: &Workspace,
    refs: &mut Vec<FuncRef>,
    seen: &mut HashSet<FuncRef>,
) {
    for entry in skull_graph::instance_graph::synthesized_entry_roots(ws) {
        let name = ws.module(entry.module).functions[entry.func].name;
        let serves_js =
            skull_mir::lower::backend_variant(ws, name, entry, bone_core::hir::FnBackend::Js)
                .is_ok();
        if serves_js && seen.insert(entry) {
            refs.push(entry);
        }
    }
}

/// Graph roots only: the worklist emits an opaque body's `@nomangle` targets
/// once it emits the body that names them.
fn extend_root_plan_from_inline_view(plan: &mut JsRootPlan, view: &ArtifactView) -> bool {
    let mut seen: HashSet<FuncRef> = plan.refs.iter().copied().collect();
    let mut added = false;
    for &target in view.opaque_boundary_roots.values().flatten() {
        if seen.insert(target) {
            plan.refs.push(target);
            added = true;
        }
    }
    added
}

fn js_artifact_request(roots: &[FuncRef]) -> artifact::ArtifactRequest<'_> {
    let mappings = artifact::ArtifactMappings::for_backend(bone_core::hir::FnBackend::Js)
        .with_supply(&crate::JsBackendRealization);
    artifact::ArtifactRequest::new(target_projection::JAVASCRIPT_WEB, roots)
        .with_mappings(mappings)
        .with_symbols(&artifact::REALIZED_SYMBOLS)
}

impl JsSession {
    pub(super) fn contains_root(&self, root: FuncRef) -> bool {
        self.root_plan.refs.contains(&root)
    }

    pub(super) fn assemble(
        &self,
        ws: &Workspace,
        scx: &skull_mir::lower::ScalarCtx,
        program_mode: bool,
    ) -> Result<ProgramBuild, CompilerError> {
        assemble_program_core(ws, scx, &self.root_plan.named, self, program_mode)
    }

    pub(super) fn update(
        &mut self,
        ws: &Workspace,
        changed: &[(bone_core::prelude::ModuleId, bone_core::hir::BodyOwner)],
        edits: &[skull_graph::instance_graph::TextEditDelta],
    ) -> skull_graph::instance_graph::UpdateStats {
        // The view retracts against the graph it was realized from.
        self.view.prepare_update(&self.graph);
        let update = self
            .graph
            .update_bodies_with_root_delta(ws, &[], &[], changed, edits);
        self.concrete.update_local(
            ws,
            &self.graph,
            &update.revisited,
            &update.removed,
            &update.revisited_owners,
        );
        self.view.update_local_with_root_delta(
            ws,
            &self.graph,
            &js_artifact_request(&self.root_plan.refs),
            &update.revisited,
            &update.added,
            &update.removed,
            &[],
            &[],
        );
        update
    }
}

fn entry_root(ws: &Workspace, user_mid: bone_core::prelude::ModuleId) -> Option<(FuncRef, String)> {
    nullary_main(ws, user_mid).map(|fr| (fr, callee_js_name(ws, fr)))
}

pub fn emit_program(
    ws: &Workspace,
    user_mid: bone_core::prelude::ModuleId,
) -> Result<JsProgram, CompilerError> {
    let scx = js_scx(ws);
    let entry = entry_root(ws, user_mid);
    let (build, _, _) = build_program_session(ws, &scx, entry.as_slice(), true)?;
    Ok(JsProgram {
        program: build.program,
        main_js_name: entry.map(|(_, name)| name),
        code_artifact: build.code_artifact,
        lifecycle_plan: build.lifecycle_plan,
        global_layout: build.global_layout,
    })
}

pub fn emit_program_cached(
    ws: &Workspace,
    user_mid: bone_core::prelude::ModuleId,
) -> Result<(JsProgram, JsEmitCache), CompilerError> {
    let scx = js_scx(ws);
    let entry = entry_root(ws, user_mid);
    let (build, session, shaders) = build_program_session(ws, &scx, entry.as_slice(), true)?;
    let static_keys = build
        .chunks
        .values()
        .flat_map(|chunk| chunk.statics.iter())
        .map(|cell| {
            Ok((
                cell.cell,
                StaticBaseline::of(ws, scx.interner(), cell).map_err(err)?,
            ))
        })
        .collect::<Result<_, CompilerError>>()?;
    let program = JsProgram {
        program: build.program.clone(),
        main_js_name: entry.map(|(_, name)| name),
        code_artifact: build.code_artifact.clone(),
        lifecycle_plan: build.lifecycle_plan.clone(),
        global_layout: build.global_layout.clone(),
    };
    let cache = JsEmitCache {
        build,
        changed_since_full: HashSet::new(),
        static_keys,
        shader_contributions: shaders.contributions,
        opaque_root_contributions: session.view.opaque_boundary_roots.clone(),
        scx,
        session,
    };
    Ok((program, cache))
}

pub fn emit_program_with_roots_artifact(
    ws: &Workspace,
    roots: &[(FuncRef, String)],
) -> Result<(String, CodeArtifact), CompilerError> {
    let scx = js_scx(ws);
    let build = build_program_session(ws, &scx, roots, true)?.0;
    Ok((build.program, build.code_artifact))
}

/// Emit only the requested roots and reachable definitions. Module-load
/// callbacks can themselves require unsupported features in JS test units.
pub fn emit_with_closure(
    ws: &Workspace,
    roots: &[(FuncRef, String)],
) -> Result<String, CompilerError> {
    let scx = js_scx(ws);
    Ok(build_program_session(ws, &scx, roots, false)?.0.program)
}

fn build_program_session(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    extra_roots: &[(FuncRef, String)],
    program_mode: bool,
) -> Result<(ProgramBuild, JsSession, skull_mir::lower::ShaderCollection), CompilerError> {
    if program_mode {
        for module in ws.source_modules() {
            if let Some((why, span)) = skull_mir::lower::map_key_rejection(ws, module.id) {
                return Err(CompilerError::new_with_label(
                    span,
                    format!("this `Map` key is not value-keyed and would silently miss: {why}"),
                ));
            }
        }
    }
    let shaders = skull_mir::lower::collect_shader_collection(ws)?;
    let mut root_plan = js_root_plan(ws, extra_roots, program_mode, &shaders.pairs)?;
    loop {
        let graph = skull_graph::instance_graph::build_in(
            ws,
            root_plan.refs.clone(),
            scx.interner_handle(),
        );
        let view = artifact::realize(ws, &graph, &js_artifact_request(&root_plan.refs));
        let concrete = skull_graph::concrete_body::build(ws, &graph);
        if extend_root_plan_from_inline_view(&mut root_plan, &view) {
            continue;
        }
        let session = JsSession {
            root_plan,
            graph,
            view,
            concrete,
        };
        let build = session.assemble(ws, scx, program_mode)?;
        return Ok((build, session, shaders));
    }
}
