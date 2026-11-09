//! Shader libraries and program sessions over the instance graph.

use super::*;

/// Resolve a root fn name to a unique [`FuncRef`], preferring the user module.
pub fn resolve_root(
    ws: &Workspace,
    user_mid: ModuleId,
    name: &str,
) -> Result<FuncRef, CompilerError> {
    let key = Ustr::from(name);
    let Some(list): Option<&FnList> = ws.functions_by_name.get(&key) else {
        return Err(CompilerError::new_no_location(format!(
            "shader root `{name}` not found"
        )));
    };
    let in_user: Vec<_> = list.iter().filter(|(m, _)| *m == user_mid).collect();
    let pick = if in_user.len() == 1 {
        *in_user[0]
    } else if list.len() == 1 {
        list[0]
    } else {
        return Err(CompilerError::new_no_location(format!(
            "shader root `{name}` is overloaded/ambiguous ({} definitions); route B roots must be uniquely named",
            list.len()
        )));
    };
    Ok(FuncRef {
        module: pick.0,
        func: pick.1,
    })
}

/// The GLSL helper functions for tier-0 semantics GLSL's builtins get wrong:
/// floored int division/modulo (GLSL's are undefined on negative operands).
/// Emitted only when reachable code uses `IntDiv`/`IntMod`.
pub(crate) fn helper_prelude(uses_idiv: bool, uses_imod: bool) -> String {
    let mut s = String::new();
    if uses_idiv {
        s.push_str("int rock_idiv(int a, int b) {\n  return int(floor(float(a) / float(b)));\n}\n");
    }
    if uses_imod {
        s.push_str(
            "int rock_imod(int a, int b) {\n  return a - b * int(floor(float(a) / float(b)));\n}\n",
        );
    }
    s
}

fn library_root_instance(
    ws: &Workspace,
    graph: &InstanceGraph,
    root: FuncRef,
) -> Result<Instance, CompilerError> {
    let canonical = skull_mir::lower::canonical_variant(ws, root);
    graph
        .root_instances
        .iter()
        .find(|instance| instance.func == canonical && instance.args.is_empty())
        .cloned()
        .ok_or_else(|| {
            CompilerError::new_no_location(format!(
                "shader library graph has no explicit root instance for `{}`",
                ws.module(root.module).functions[root.func].name
            ))
        })
}

fn visit_shader_library(
    ws: &Workspace,
    graph: &InstanceGraph,
    instance: Instance,
    retained: &mut BTreeSet<Instance>,
    builtins: &mut BTreeSet<Instance>,
    active: &mut BTreeSet<Instance>,
    done: &mut BTreeSet<Instance>,
    order: &mut Vec<Instance>,
) -> Result<(), CompilerError> {
    if active.contains(&instance) {
        let f = &ws.module(instance.func.module).functions[instance.func.func];
        return Err(err(
            f.name_span,
            format!(
                "shader profile: recursion through `{}` is not allowed on GLSL",
                f.name
            ),
        ));
    }
    if !done.insert(instance.clone()) {
        return Ok(());
    }
    if graph.concreteness(&instance) == skull_graph::instance_graph::Concreteness::Missing {
        return Err(CompilerError::new_no_location(format!(
            "shader library graph has no substitution for `{}`",
            graph.symbol(ws, &instance)
        )));
    }
    active.insert(instance.clone());
    retained.insert(instance.clone());
    if let Some(sites) = graph.sites_of(&instance) {
        for record in sites.values() {
            let callee = match &record.outcome {
                SiteOutcome::Resolved(callee) => callee.clone(),
                SiteOutcome::Failed(reason) => {
                    return Err(CompilerError::new_with_label(
                        record.span,
                        format!("shader library graph could not resolve a call: {reason}"),
                    ));
                }
            };
            let f = &ws.module(callee.func.module).functions[callee.func.func];
            if graph.concreteness(&callee) == skull_graph::instance_graph::Concreteness::Missing {
                return Err(CompilerError::new_with_label(
                    record.span,
                    format!("shader library graph has no substitution for `{}`", f.name),
                ));
            }
            if renders_inline_instance(ws, f, &callee, graph.interner()) {
                builtins.insert(callee);
            } else if is_lowered_native_operator(ws, f, &callee, graph.interner()) {
                continue;
            } else if f.is_extern || f.inline_body.is_some() {
                // The supplied population stops at an opaque boundary. The
                // emitter still sees the call while profiling the caller and
                // reports the boundary after any earlier body-level rejection.
                continue;
            } else {
                visit_shader_library(ws, graph, callee, retained, builtins, active, done, order)?;
            }
        }
    }
    active.remove(&instance);
    order.push(instance);
    Ok(())
}

/// Realize an explicit-root function-library population from the shared graph.
/// This is deliberately separate from registered shader programs: the roots
/// have no stage or interface contract, only concrete definitions and helpers.
pub fn realize_shader_library_view(
    ws: &Workspace,
    graph: &InstanceGraph,
    roots: &[FuncRef],
) -> Result<ShaderLibraryView, CompilerError> {
    let mut root_instances = Vec::new();
    let mut seen_roots = BTreeSet::new();
    for &root in roots {
        let instance = library_root_instance(ws, graph, root)?;
        if seen_roots.insert(instance.clone()) {
            root_instances.push(instance);
        }
    }
    let mut retained = BTreeSet::new();
    let mut builtins = BTreeSet::new();
    let mut active = BTreeSet::new();
    let mut done = BTreeSet::new();
    let mut order = Vec::new();
    for root in &root_instances {
        visit_shader_library(
            ws,
            graph,
            root.clone(),
            &mut retained,
            &mut builtins,
            &mut active,
            &mut done,
            &mut order,
        )?;
    }
    let mut dependencies = Vec::new();
    let mut seen_dependencies = BTreeSet::new();
    for instance in order.iter().chain(root_instances.iter()) {
        if seen_dependencies.insert((
            instance.func.module.raw(),
            instance.func.func.into_raw().into_u32(),
        )) {
            dependencies.push(instance.func);
        }
    }
    let concrete_plans = skull_graph::concrete_body::build(ws, graph).plans;
    Ok(ShaderLibraryView {
        roots: root_instances,
        retained,
        builtins,
        order,
        dependencies,
        concrete_plans,
    })
}

/// The graph, lowering context and explicit-root population kept together so
/// every route that emits a function library reads one interner.
pub struct ShaderLibrarySession {
    pub graph: InstanceGraph,
    pub scx: ScalarCtx,
    pub view: ShaderLibraryView,
}

pub fn shader_library_session(
    ws: &Workspace,
    roots: Vec<FuncRef>,
) -> Result<ShaderLibrarySession, CompilerError> {
    let interner = Arc::new(MonoInterner::new());
    let graph =
        skull_graph::instance_graph::build_in(ws, roots.clone(), false, Arc::clone(&interner));
    let scx = ScalarCtx::new_values_first_class(ws).sharing(interner);
    let view = realize_shader_library_view(ws, &graph, &roots)?;
    Ok(ShaderLibrarySession { graph, scx, view })
}

/// The lowering context and registered-program population kept together
/// for routes that do not already own a `skull::shader_artifact` view.
pub struct ShaderProgramSession {
    pub scx: ScalarCtx,
    pub view: ShaderProgramView,
}

fn shader_program_root(
    ws: &Workspace,
    graph: &InstanceGraph,
    fref: FuncRef,
) -> Result<Instance, CompilerError> {
    let canonical = skull_mir::lower::canonical_variant(ws, fref);
    graph
        .instances
        .iter()
        .find(|instance| instance.func == canonical && instance.args.is_empty())
        .cloned()
        .ok_or_else(|| {
            CompilerError::new_no_location(format!(
                "shader graph has no concrete root instance for `{}`",
                ws.module(fref.module).functions[fref.func].name
            ))
        })
}

fn shader_reachable(
    roots: &[Instance],
    edges: &std::collections::BTreeMap<Instance, Vec<Instance>>,
) -> BTreeSet<Instance> {
    let mut seen: BTreeSet<Instance> = roots.iter().cloned().collect();
    let mut queue: std::collections::VecDeque<Instance> = roots.iter().cloned().collect();
    while let Some(instance) = queue.pop_front() {
        for callee in edges.get(&instance).into_iter().flatten() {
            if seen.insert(callee.clone()) {
                queue.push_back(callee.clone());
            }
        }
    }
    seen
}

fn shader_postorder(
    instance: &Instance,
    edges: &std::collections::BTreeMap<Instance, Vec<Instance>>,
    retained: &BTreeSet<Instance>,
    active: &mut BTreeSet<Instance>,
    done: &mut BTreeSet<Instance>,
    order: &mut Vec<Instance>,
) {
    if done.contains(instance) || !active.insert(instance.clone()) {
        return;
    }
    for callee in edges.get(instance).into_iter().flatten() {
        if retained.contains(callee) {
            shader_postorder(callee, edges, retained, active, done, order);
        }
    }
    active.remove(instance);
    done.insert(instance.clone());
    if !order.contains(instance) {
        order.push(instance.clone());
    }
}

fn shader_stage_view(
    ws: &Workspace,
    graph: &InstanceGraph,
    edges: &std::collections::BTreeMap<Instance, Vec<Instance>>,
    entry: Instance,
    stage: ShaderStage,
) -> ShaderStageView {
    let retained = shader_reachable(std::slice::from_ref(&entry), edges);
    let helpers: BTreeSet<Instance> = retained
        .iter()
        .filter(|instance| *instance != &entry)
        .cloned()
        .collect();
    let mut builtins = BTreeSet::new();
    for caller in &retained {
        let Some(sites) = graph.sites_of(caller) else {
            continue;
        };
        for record in sites.values() {
            let SiteOutcome::Resolved(callee) = &record.outcome else {
                continue;
            };
            let f = &ws.module(callee.func.module).functions[callee.func.func];
            if renders_inline_instance(ws, f, callee, graph.interner()) {
                builtins.insert(callee.clone());
            }
        }
    }
    let mut order = Vec::new();
    shader_postorder(
        &entry,
        edges,
        &retained,
        &mut BTreeSet::new(),
        &mut BTreeSet::new(),
        &mut order,
    );
    order.retain(|instance| instance != &entry);
    let mut dependencies = Vec::new();
    let mut seen = BTreeSet::new();
    for instance in order.iter().chain(std::iter::once(&entry)) {
        let key = (
            instance.func.module.raw(),
            instance.func.func.into_raw().into_u32(),
        );
        if seen.insert(key) {
            dependencies.push(instance.func);
        }
    }
    ShaderStageView {
        stage,
        entry,
        retained,
        helpers,
        builtins,
        order,
        dependencies,
    }
}

fn realize_shader_program_view(
    ws: &Workspace,
    graph: &InstanceGraph,
    program: ShaderProgram,
) -> Result<ShaderProgramView, CompilerError> {
    let mut roots = Vec::new();
    if let Some(vertex) = program.vert {
        roots.push(shader_program_root(ws, graph, vertex)?);
    }
    roots.push(shader_program_root(ws, graph, program.frag)?);

    let mut seen = roots.iter().cloned().collect::<BTreeSet<_>>();
    let mut queue = roots.clone();
    let mut edges: std::collections::BTreeMap<Instance, Vec<Instance>> =
        std::collections::BTreeMap::new();
    while let Some(caller) = queue.pop() {
        let Some(sites) = graph.sites_of(&caller) else {
            continue;
        };
        for record in sites.values() {
            let SiteOutcome::Resolved(callee) = &record.outcome else {
                continue;
            };
            if graph.concreteness(callee) == skull_graph::instance_graph::Concreteness::Missing {
                return Err(CompilerError::new_no_location(format!(
                    "shader graph has no substitution for `{}`",
                    ws.module(callee.func.module).functions[callee.func.func].name
                )));
            }
            let f = &ws.module(callee.func.module).functions[callee.func.func];
            if renders_inline_instance(ws, f, callee, graph.interner())
                || is_lowered_native_operator(ws, f, callee, graph.interner())
            {
                continue;
            }
            edges
                .entry(caller.clone())
                .or_default()
                .push(callee.clone());
            if seen.insert(callee.clone()) {
                queue.push(callee.clone());
            }
        }
    }
    let stages = match program.vert {
        Some(vertex) => vec![
            shader_stage_view(
                ws,
                graph,
                &edges,
                shader_program_root(ws, graph, vertex)?,
                ShaderStage::Vertex,
            ),
            shader_stage_view(
                ws,
                graph,
                &edges,
                shader_program_root(ws, graph, program.frag)?,
                ShaderStage::Fragment,
            ),
        ],
        None => vec![shader_stage_view(
            ws,
            graph,
            &edges,
            shader_program_root(ws, graph, program.frag)?,
            ShaderStage::Fragment,
        )],
    };
    let concrete_plans = skull_graph::concrete_body::build(ws, graph).plans;
    Ok(ShaderProgramView {
        program,
        stages,
        concrete_plans,
    })
}

/// Build a registered-program view from the shared graph. The returned session
/// is the compatibility seam for callers that have roots but no retained
/// `ShaderArtifactView`; emission itself still consumes the supplied view.
pub fn shader_program_session(
    ws: &Workspace,
    program: ShaderProgram,
) -> Result<ShaderProgramSession, CompilerError> {
    let roots: Vec<_> = program
        .vert
        .into_iter()
        .chain(std::iter::once(program.frag))
        .collect();
    let interner = Arc::new(MonoInterner::new());
    let graph = skull_graph::instance_graph::build_in(ws, roots, false, Arc::clone(&interner));
    let scx = ScalarCtx::new_values_first_class(ws).sharing(interner);
    let view = realize_shader_program_view(ws, &graph, program)?;
    Ok(ShaderProgramSession { scx, view })
}

fn checked_library_callees(
    ws: &Workspace,
    caller: &ShaderInstance,
    callees: Vec<ShaderInstance>,
    allowed: &BTreeSet<Instance>,
    interner: &MonoInterner,
    builtins: &mut BTreeSet<Instance>,
) -> Result<Vec<ShaderInstance>, CompilerError> {
    let caller_f = &ws.module(caller.fref().module).functions[caller.fref().func];
    let mut retained = Vec::new();
    for callee in callees {
        let f = &ws.module(callee.fref().module).functions[callee.fref().func];
        if renders_inline_instance(ws, f, &callee.inst, interner) {
            builtins.insert(callee.inst.clone());
            continue;
        }
        if is_lowered_native_operator(ws, f, &callee.inst, interner) {
            continue;
        }
        if f.is_extern || f.inline_body.is_some() {
            return Err(err(
                f.name_span,
                format!(
                    "shader profile: `{}` has no emittable body and no GLSL intrinsic mapping",
                    f.name
                ),
            ));
        }
        if !allowed.contains(&callee.inst) {
            return Err(err(
                caller_f.name_span,
                format!(
                    "shader library view omitted the concrete callee `{}` reached by `{}`",
                    f.name, caller_f.name
                ),
            ));
        }
        retained.push(callee);
    }
    Ok(retained)
}

/// Emit a no-`#version` GLSL function library from a supplied graph population.
/// The view owns identity, retention, builtins, order and dependencies; this
/// function only lowers those instances, validates their bodies and renders them.
pub fn emit_shader_library_with_view(
    ws: &Workspace,
    scx: &ScalarCtx,
    view: &ShaderLibraryView,
) -> Result<String, CompilerError> {
    if view.order.iter().cloned().collect::<BTreeSet<_>>() != view.retained
        || view.order.len() != view.retained.len()
        || view.roots.iter().any(|root| !view.retained.contains(root))
    {
        return Err(CompilerError::new_no_location(
            "shader library view order is not the retained population".to_string(),
        ));
    }
    let allowed = &view.retained;
    let mut bodies: HashMap<ShaderInstance, MirFunc> = HashMap::default();
    let mut calls: HashMap<ShaderInstance, Vec<ShaderInstance>> = HashMap::default();
    let mut discovered_builtins = BTreeSet::new();
    let mut uses_idiv = false;
    let mut uses_imod = false;
    for raw in &view.order {
        let instance = ShaderInstance::new(raw.clone());
        let f = &ws.module(instance.fref().module).functions[instance.fref().func];
        let plan = view.concrete_plans.get(raw).ok_or_else(|| {
            err(
                f.name_span,
                format!("shader profile: no concrete plan for `{}`", f.name),
            )
        })?;
        check_tier0_signature_concrete(scx.interner(), &plan.signature, f.name_span)?;
        let body = skull_mir::lower::lower_instance_with_plans(ws, scx, plan, &view.concrete_plans)
            .map_err(|e| {
                err(
                    f.name_span,
                    format!(
                        "shader profile: `{}` cannot lower for GLSL: {}",
                        f.name, e.message
                    ),
                )
            })?;
        let mut direct = Vec::new();
        profile_body(ws, f, &body, &mut direct, true)?;
        let retained = checked_library_callees(
            ws,
            &instance,
            direct,
            allowed,
            scx.interner(),
            &mut discovered_builtins,
        )?;
        for (_, inst) in body.insts.iter() {
            match inst {
                Inst::IntDiv(..) => uses_idiv = true,
                Inst::IntMod(..) => uses_imod = true,
                _ => {}
            }
        }
        calls.insert(instance.clone(), retained);
        bodies.insert(instance, body);
    }
    if discovered_builtins != view.builtins {
        return Err(CompilerError::new_no_location(
            "shader library view builtin population differs from lowered calls".to_string(),
        ));
    }
    reject_population_cycles(ws, &calls)?;
    let mut dependencies = Vec::new();
    let mut seen_dependencies = BTreeSet::new();
    for instance in view.order.iter().chain(view.roots.iter()) {
        if seen_dependencies.insert((
            instance.func.module.raw(),
            instance.func.func.into_raw().into_u32(),
        )) {
            dependencies.push(instance.func);
        }
    }
    if dependencies != view.dependencies {
        return Err(CompilerError::new_no_location(
            "shader library view dependencies differ from retained order".to_string(),
        ));
    }
    let mut out = helper_prelude(uses_idiv, uses_imod);
    if !out.is_empty() {
        out.push('\n');
    }
    for (index, raw) in view.order.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        let instance = ShaderInstance::new(raw.clone());
        let body = bodies.get(&instance).expect("view order body was lowered");
        let body_plan = view.concrete_plans.get(raw).ok_or_else(|| {
            CompilerError::new_no_location(
                "shader profile: helper has no concrete plan".to_string(),
            )
        })?;
        out.push_str(&emit_fn_def_with(
            ws,
            scx.interner(),
            &instance,
            body,
            None,
            Some(body_plan),
        )?);
    }
    Ok(out)
}

/// Compatibility wrapper for callers that have not yet retained a graph. The
/// route itself is still graph-backed; production and tests that own a session
/// should call [`emit_shader_library_with_view`] directly.
pub fn emit_shader_library(
    ws: &Workspace,
    mid: ModuleId,
    roots: &[String],
) -> Result<String, CompilerError> {
    let root_refs: Vec<_> = roots
        .iter()
        .map(|root| resolve_root(ws, mid, root))
        .collect::<Result<_, _>>()?;
    let session = shader_library_session(ws, root_refs)?;
    emit_shader_library_with_view(ws, &session.scx, &session.view)
}

// --- A1: fragment-entry compilation --------------------
