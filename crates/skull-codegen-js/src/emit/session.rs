use super::*;
use skull_mir::instance::DeclKey;

/// One lowering context supplies the interner for the graph, view and emitter.
pub(super) fn js_scx(ws: &Workspace) -> skull_mir::lower::ScalarCtx {
    skull_mir::lower::ScalarCtx::new(ws).with_target(bone_core::hir::FnBackend::Js)
}

struct JsRootPlan {
    named: Vec<(FuncRef, String)>,
    refs: Vec<FuncRef>,
}

pub(super) type OpaqueRoots = BTreeMap<DeclKey, rustc_hash::FxHashSet<FuncRef>>;

pub(super) struct JsSession {
    pub(super) graph: InstanceGraph,
    pub(super) view: ArtifactView,
    pub(super) symbols: rustc_hash::FxHashMap<Instance, String>,
    pub(super) opaque_roots: OpaqueRoots,
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

/// An inline JS body can call a published name without a MIR call site.
fn opaque_roots(ws: &Workspace, view: &ArtifactView) -> OpaqueRoots {
    let bodies: BTreeMap<_, _> = view
        .realization
        .values()
        .filter_map(|row| {
            let f = &ws.module(row.decl.module).functions[row.decl.func];
            match f.inline_body {
                Some((bone_core::hir::FnBackend::Js, body)) => Some((DeclKey(row.decl), body)),
                _ => None,
            }
        })
        .collect();
    if bodies.is_empty() {
        return BTreeMap::new();
    }
    let mut publications: rustc_hash::FxHashMap<&str, Vec<FuncRef>> = Default::default();
    for module in &ws.modules {
        for (func, function) in module.functions.iter() {
            if function
                .annotations
                .iter()
                .any(|annotation| annotation.is_builtin("nomangle"))
            {
                let name = skull_mir::mangle::declared_name(function.name.as_str());
                publications.entry(name).or_default().push(FuncRef {
                    module: module.id,
                    func,
                });
            }
        }
    }
    bodies
        .into_iter()
        .map(|(decl, body)| {
            let targets = super::inline_js::bare_names(body.as_str())
                .filter_map(|name| publications.get(name))
                .flatten()
                .copied()
                .collect();
            (decl, targets)
        })
        .collect()
}

fn js_symbols(
    ws: &Workspace,
    graph: &InstanceGraph,
    view: &ArtifactView,
) -> rustc_hash::FxHashMap<Instance, String> {
    view.realization
        .iter()
        .map(|(instance, row)| {
            let name = view.publications.get(instance).cloned().unwrap_or_else(|| {
                Instance {
                    func: row.decl,
                    args: instance.args.clone(),
                }
                .symbol(ws, graph.interner())
            });
            (instance.clone(), name)
        })
        .collect()
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
    let build = build_program_session(ws, &scx, entry.as_slice(), true)?;
    Ok(JsProgram {
        program: build.program,
        main_js_name: entry.map(|(_, name)| name),
        code_artifact: build.code_artifact,
        lifecycle_plan: build.lifecycle_plan,
        global_layout: build.global_layout,
    })
}

pub fn emit_program_with_roots_artifact(
    ws: &Workspace,
    roots: &[(FuncRef, String)],
) -> Result<(String, CodeArtifact), CompilerError> {
    let scx = js_scx(ws);
    let build = build_program_session(ws, &scx, roots, true)?;
    Ok((build.program, build.code_artifact))
}

/// Emit only the requested roots and reachable definitions. Module-load
/// callbacks can themselves require unsupported features in JS test units.
pub fn emit_with_closure(
    ws: &Workspace,
    roots: &[(FuncRef, String)],
) -> Result<String, CompilerError> {
    let scx = js_scx(ws);
    Ok(build_program_session(ws, &scx, roots, false)?.program)
}

fn build_program_session(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    extra_roots: &[(FuncRef, String)],
    program_mode: bool,
) -> Result<ProgramBuild, CompilerError> {
    for module in ws.source_modules() {
        if let Some((why, span)) = skull_mir::lower::map_key_rejection(ws, module.id) {
            return Err(CompilerError::new_with_label(
                span,
                format!("this `Map` key is not value-keyed and would silently miss: {why}"),
            ));
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
        let view = artifact::realize(
            ws,
            &graph,
            &artifact::ArtifactRequest {
                target: target_projection::JAVASCRIPT_WEB.cpu,
                roots: &root_plan.refs,
                scx,
                supply: Some(&crate::JsBackendRealization),
            },
        );
        let opaque_roots = opaque_roots(ws, &view);
        let mut seen: HashSet<FuncRef> = root_plan.refs.iter().copied().collect();
        let old_count = root_plan.refs.len();
        root_plan.refs.extend(
            opaque_roots
                .values()
                .flatten()
                .copied()
                .filter(|target| seen.insert(*target)),
        );
        if root_plan.refs.len() != old_count {
            continue;
        }
        let session = JsSession {
            symbols: js_symbols(ws, &graph, &view),
            concrete: skull_graph::concrete_body::build(ws, &graph),
            graph,
            view,
            opaque_roots,
        };
        return assemble_program_core(ws, scx, &root_plan.named, &session, program_mode);
    }
}
