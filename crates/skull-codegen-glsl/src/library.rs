//! Shader libraries and program sessions over the instance graph.

use super::*;

/// Resolve a root fn name to a unique [`FuncRef`], preferring the user module.
pub fn resolve_root(
    ws: &Workspace,
    user_mid: ModuleId,
    name: &str,
) -> Result<FuncRef, CompilerError> {
    let key = Ustr::from(name);
    let key = ws
        .module(user_mid)
        .functions
        .iter()
        .find(|(_, function)| declared_name(function.name.as_str()) == name)
        .map_or(key, |(_, function)| function.name);
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
/// Emitted only when one of `bodies` uses `IntDiv`/`IntMod`, and followed by a
/// blank line when not empty.
pub(crate) fn helper_prelude<'a>(bodies: impl IntoIterator<Item = &'a MirFunc>) -> String {
    let (mut uses_idiv, mut uses_imod) = (false, false);
    for body in bodies {
        for (_, inst) in body.insts.iter() {
            match inst {
                Inst::IntDiv(..) => uses_idiv = true,
                Inst::IntMod(..) => uses_imod = true,
                _ => {}
            }
        }
    }
    let mut s = String::new();
    if uses_idiv {
        s.push_str("int rock_idiv(int a, int b) {\n  return int(floor(float(a) / float(b)));\n}\n");
    }
    if uses_imod {
        s.push_str(
            "int rock_imod(int a, int b) {\n  return a - b * int(floor(float(a) / float(b)));\n}\n",
        );
    }
    if !s.is_empty() {
        s.push('\n');
    }
    s
}

fn declaration_dependencies<'a>(instances: impl Iterator<Item = &'a Instance>) -> Vec<FuncRef> {
    let mut seen = BTreeSet::new();
    instances
        .map(|instance| instance.func)
        .filter(|func| seen.insert((func.module.raw(), func.func.into_raw().into_u32())))
        .collect()
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

pub(crate) fn realize_shader_library_view(
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
    let dependencies = declaration_dependencies(order.iter().chain(&root_instances));
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

/// The lowering context and explicit-root population kept together so every
/// route that emits a function library reads one interner.
pub struct ShaderLibrarySession {
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
    Ok(ShaderLibrarySession { scx, view })
}

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
    for raw in &view.order {
        let instance = ShaderInstance::new(raw.clone());
        let (body, direct) = lower_profiled_body(ws, scx, raw, &view.concrete_plans, false, None)?;
        let retained = checked_body_calls(
            ws,
            &instance,
            direct,
            allowed,
            scx.interner(),
            &mut discovered_builtins,
            "shader library view",
        )?;
        calls.insert(instance.clone(), retained);
        bodies.insert(instance, body);
    }
    if discovered_builtins != view.builtins {
        return Err(CompilerError::new_no_location(
            "shader library view builtin population differs from lowered calls".to_string(),
        ));
    }
    reject_population_cycles(ws, &calls)?;
    if declaration_dependencies(view.order.iter().chain(&view.roots)) != view.dependencies {
        return Err(CompilerError::new_no_location(
            "shader library view dependencies differ from retained order".to_string(),
        ));
    }
    let mut out = helper_prelude(bodies.values());
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
            body_plan,
        )?);
    }
    Ok(out)
}

/// Emit the no-`#version` function library for `roots` and their reachable
/// helpers, realized from a fresh instance graph.
pub fn emit_shader_library(ws: &Workspace, roots: Vec<FuncRef>) -> Result<String, CompilerError> {
    let session = shader_library_session(ws, roots)?;
    emit_shader_library_with_view(ws, &session.scx, &session.view)
}
