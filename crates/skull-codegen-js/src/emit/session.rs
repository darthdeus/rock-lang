//! Root plans and the cached instance-graph session behind a program emission.

use super::*;

/// The whole workspace as ONE JS program:
/// module-driven assembly mirroring skull's Lua `assemble`, but reachability-
/// driven (roots = `main` + every DECORATED fn + every `deco` body; the whole
/// stdlib is NOT emitted, only what those roots reach). Emission order:
/// singletons -> `__deco_<name>` def fns -> fn defs -> `__ginit_<name>` def fns
/// -> load-time init lines (dep-ordered) -> decorator APPLICATION lines (after
/// the inits so a deco body can push into a ginit-initialized registry). The
/// driver prepends the runtime preludes + `var __G = {}` + a direct
/// `print`/`put`, and appends `<main>();` when `main_js_name` is `Some`.
///
/// Unlike [`emit_with_closure`], a decorator-populated global read does NOT
/// reject here -- the applications ARE emitted, so the registry is populated at
/// module load exactly as on luajit.
/// **THE JS lowering context, and there is one construction of it per session.**
///
/// Non-cached emission creates a context and session through this module.
/// Cached full emission stores both in `JsEmitCache`, and overlay reuses them;
/// an `Instance` is an identity only relative to its arena, so every graph,
/// view, name, and delivery structure in one session reads the same interner.
///
/// The luajit driver already owns its own (`skull::mir_emit::FullEmit`);
/// this is that same rule for JS.
pub(super) fn js_scx(ws: &Workspace) -> skull_mir::lower::ScalarCtx {
    skull_mir::lower::ScalarCtx::new(ws).with_target(bone_core::hir::FnBackend::Js)
}

#[derive(Clone)]
pub(super) struct JsRootPlan {
    pub(super) named: Vec<(FuncRef, String)>,
    pub(super) refs: Vec<FuncRef>,
}

#[derive(Clone)]
pub(super) struct JsSession {
    pub(super) roots: Vec<FuncRef>,
    pub(super) graph: InstanceGraph,
    pub(super) view: ArtifactView,
    pub(super) concrete: skull_graph::concrete_body::ConcreteBodies,
    pub(super) last_update: Option<JsGraphUpdate>,
}

#[derive(Clone, Debug, Default)]
pub struct JsGraphUpdate {
    pub before: usize,
    pub after: usize,
    pub added: usize,
    pub removed: usize,
    pub added_instances: BTreeSet<Instance>,
    pub removed_instances: BTreeSet<Instance>,
}

fn js_root_plan(
    ws: &Workspace,
    extra_roots: &[(FuncRef, String)],
) -> Result<JsRootPlan, CompilerError> {
    let decorators = DecoratorTable::collect(ws);
    let mut named = extra_roots.to_vec();
    let mut seen: HashSet<FuncRef> = named.iter().map(|(fr, _)| *fr).collect();
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
    if let Some(fr) = shader_register_sibling(ws)? {
        if seen.insert(fr) {
            named.push((fr, "__rock_shader_register".to_string()));
        }
    }
    let mut refs: Vec<FuncRef> = named.iter().map(|(fr, _)| *fr).collect();
    for shader_root in shader_root_refs(ws)? {
        if seen.insert(shader_root) {
            refs.push(shader_root);
        }
    }
    Ok(JsRootPlan { named, refs })
}

pub(super) fn shader_root_refs(ws: &Workspace) -> Result<Vec<FuncRef>, CompilerError> {
    Ok(skull_mir::lower::collect_shader_pairs(ws)?
        .into_iter()
        .flat_map(|pair| pair.vert.into_iter().chain(std::iter::once(pair.frag)))
        .collect())
}

pub(super) fn extend_root_plan_from_inline_view(
    ws: &Workspace,
    plan: &mut JsRootPlan,
    view: &ArtifactView,
) -> bool {
    extend_root_plan_from_opaque_targets(
        ws,
        plan,
        view.opaque_boundary_roots.values().flatten().copied(),
    )
}

fn extend_root_plan_from_opaque_targets(
    ws: &Workspace,
    plan: &mut JsRootPlan,
    targets: impl IntoIterator<Item = FuncRef>,
) -> bool {
    let mut seen: HashSet<FuncRef> = plan.refs.iter().copied().collect();
    let mut added = false;
    for target in targets {
        if seen.insert(target) {
            let name = ws.module(target.module).functions[target.func]
                .name
                .to_string();
            plan.named.push((target, name));
            plan.refs.push(target);
            added = true;
        }
    }
    added
}

pub(super) fn new_js_session(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    roots: &[FuncRef],
) -> JsSession {
    let graph =
        skull_graph::instance_graph::build_in(ws, roots.to_vec(), false, scx.interner_handle());
    let mappings = artifact::ArtifactMappings::for_backend(bone_core::hir::FnBackend::Js)
        .with_supply(&crate::JsBackendRealization);
    let view = artifact::realize(
        ws,
        &graph,
        &artifact::ArtifactRequest::new(target_projection::JAVASCRIPT_WEB, roots)
            .with_mappings(mappings)
            .with_symbols(&artifact::REALIZED_SYMBOLS),
    );
    let concrete = skull_graph::concrete_body::build_for_lowering(ws, &graph);
    JsSession {
        roots: roots.to_vec(),
        graph,
        view,
        concrete,
        last_update: None,
    }
}

pub(super) fn update_js_session(
    ws: &Workspace,
    session: &mut JsSession,
    roots: &[FuncRef],
    changed: &[(bone_core::prelude::ModuleId, bone_core::hir::BodyOwner)],
    edits: &[skull_graph::instance_graph::TextEditDelta],
) {
    let before = session.graph.instances.len();
    let previous_roots = session.roots.clone();
    let root_added: Vec<FuncRef> = roots
        .iter()
        .filter(|root| !previous_roots.contains(root))
        .copied()
        .collect();
    let root_removed: Vec<FuncRef> = previous_roots
        .iter()
        .filter(|root| !roots.contains(root))
        .copied()
        .collect();
    let update =
        session
            .graph
            .update_bodies_with_root_delta(ws, &root_added, &root_removed, changed, edits);
    session.concrete.update_local_with_span_edits(
        ws,
        &session.graph,
        &update.revisited,
        &update.removed,
        &update.revisited_owners,
        edits,
    );
    let mappings = artifact::ArtifactMappings::for_backend(bone_core::hir::FnBackend::Js)
        .with_supply(&crate::JsBackendRealization);
    let request = artifact::ArtifactRequest::new(target_projection::JAVASCRIPT_WEB, roots)
        .with_mappings(mappings)
        .with_symbols(&artifact::REALIZED_SYMBOLS);
    session.view.update_local_with_root_delta(
        ws,
        &session.graph,
        &request,
        &update.revisited,
        &update.added,
        &update.removed,
        &root_added,
        &root_removed,
    );
    session.roots = roots.to_vec();
    session.last_update = Some(JsGraphUpdate {
        before,
        after: session.graph.instances.len(),
        added: update.added.len(),
        removed: update.removed.len(),
        added_instances: update.added,
        removed_instances: update.removed,
    });
}

pub fn emit_program(
    ws: &Workspace,
    user_mid: bone_core::prelude::ModuleId,
) -> Result<JsProgram, CompilerError> {
    let scx = js_scx(ws);
    // The program entry: a nullary `main` in the entry module. Passed as the
    // one extra root; the module-load effects (deco applications, ginits) run
    // regardless of whether a `main` exists.
    let mut extra: Vec<(FuncRef, String)> = Vec::new();
    let main_js_name = nullary_main(ws, user_mid).map(|fr| {
        let nm = callee_js_name(ws, fr);
        extra.push((fr, nm.clone()));
        nm
    });
    let (build, _, _) = build_program_session(ws, &scx, &extra)?;
    Ok(JsProgram {
        program: build.program,
        main_js_name,
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
    let mut extra = Vec::new();
    let main_js_name = nullary_main(ws, user_mid).map(|fr| {
        let name = callee_js_name(ws, fr);
        extra.push((fr, name.clone()));
        name
    });
    let (build, session, root_plan) = build_program_session(ws, &scx, &extra)?;
    let shader_collection = skull_mir::lower::collect_shader_collection(ws)?;
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
        main_js_name,
        code_artifact: build.code_artifact.clone(),
        lifecycle_plan: build.lifecycle_plan.clone(),
        global_layout: build.global_layout.clone(),
    };
    let cache = JsEmitCache {
        build,
        changed_since_full: HashSet::new(),
        static_keys,
        shader_contributions: shader_collection.contributions,
        root_plan,
        opaque_root_contributions: session.view.opaque_boundary_roots.clone(),
        scx,
        session,
    };
    Ok((program, cache))
}

/// The whole-file test emission plus its compiler-owned callable artifact. The
/// runtime wrapper uses the artifact to decide whether CodeRef machinery is
/// needed; the string-only API above remains for callers that only execute it.
pub fn emit_program_with_roots_artifact(
    ws: &Workspace,
    roots: &[(FuncRef, String)],
) -> Result<(String, CodeArtifact), CompilerError> {
    let scx = js_scx(ws);
    let build = build_program_session(ws, &scx, roots)?.0;
    Ok((build.program, build.code_artifact))
}

fn build_program_session(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    extra_roots: &[(FuncRef, String)],
) -> Result<(ProgramBuild, JsSession, JsRootPlan), CompilerError> {
    for module in ws.source_modules() {
        if let Some((why, span)) = skull_mir::lower::map_key_rejection(ws, module.id) {
            return Err(CompilerError::new_with_label(
                span,
                format!("this `Map` key is not value-keyed and would silently miss: {why}"),
            ));
        }
    }
    let mut root_plan = js_root_plan(ws, extra_roots)?;
    loop {
        let session = new_js_session(ws, scx, &root_plan.refs);
        if extend_root_plan_from_inline_view(ws, &mut root_plan, &session.view) {
            continue;
        }
        let build = assemble_program_core(ws, scx, &root_plan.named, &session)?;
        return Ok((build, session, root_plan));
    }
}
