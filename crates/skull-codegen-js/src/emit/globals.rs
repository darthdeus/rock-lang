//! Decorator applications and global initializer scheduling.

use super::*;

/// A JS literal for a `@deco` annotation arg; `None` for a non-literal.
pub(crate) fn annotation_value_js(v: &bone_core::hir::AnnoValue) -> Option<String> {
    use bone_core::hir::AnnoValue as A;
    Some(match v {
        A::Int(i) => i.to_string(),
        A::Float(f) => js_number(*f).ok()?,
        A::Bool(b) => b.to_string(),
        A::String(s) => js_string_lit(s.as_str()),
        A::Ident(id) => id.name.as_str().to_string(),
        A::IdentList(_) => return None,
        A::Other => return None,
    })
}

/// Render the shared decorator argument plan for JavaScript.
fn decorator_call_args_js(
    ws: &Workspace,
    module: bone_core::prelude::ModuleId,
    decorator: Idx<bone_core::hir::DecoratorDef>,
    ann: &bone_core::hir::Annotation,
) -> Result<Vec<String>, CompilerError> {
    use skull_mir::decorator::DecoArg;
    let d = &ws.module(module).decorators[decorator];
    let mut args = Vec::new();
    for choice in skull_mir::decorator::select_decorator_args(ws, module, decorator, ann)? {
        args.push(match choice {
            DecoArg::Value(v) => annotation_value_js(v).ok_or_else(|| {
                err(format!(
                    "JS subset: decorator `{}` arg is a non-literal expression (unsupported)",
                    d.name
                ))
            })?,
            DecoArg::Default => "null".to_string(),
            DecoArg::Omitted => OMITTED_ARG.to_string(),
        });
    }
    Ok(args)
}

/// Emit decorator applications in source order with published code references.
pub(super) fn decorator_application_lines_js(
    ws: &Workspace,
    decorators: &DecoratorTable,
    interner: &skull_mir::monoty::MonoInterner,
    names: &CalleeNames,
) -> Result<String, CompilerError> {
    let mut out = String::new();
    for app in decorators.applications(ws, bone_core::hir::FnBackend::Js) {
        let (dmid, did) = app.decorator;
        let instance = Instance::base(app.func);
        let _js_fn = names.get(&instance).ok_or_else(|| {
            err(format!(
                "JS subset: decorated fn `{}` was not emitted (not a program root)",
                app.function.name
            ))
        })?;
        let identity = skull_mir::code_identity::CodeIdentity::Named(instance);
        let key = identity.logical_key(ws, interner);
        let code_ref = format!("__rock_code_slot_ref({})", js_string_lit(&key));
        let descriptor = format!(
            "{{name: {}, params: [], func: {code_ref}}}",
            js_string_lit(bone_core::modules::display_name(app.function.name.as_str()))
        );
        let mut call_args = vec![descriptor];
        call_args.extend(decorator_call_args_js(ws, dmid, did, app.annotation)?);
        out.push_str(&format!(
            "{}({});\n",
            decorator_symbol(ws.module(dmid).decorators[did].def_key.as_str()),
            call_args.join(", ")
        ));
    }
    Ok(out)
}

#[derive(Clone, Copy)]
pub(super) struct GlobalSite {
    pub(super) module: bone_core::prelude::ModuleId,
    pub(super) gid: Idx<bone_core::hir::GlobalDef>,
}

pub(super) struct GlobalInit {
    pub(super) name: ustr::Ustr,
    pub(super) init_name: String,
    pub(super) mf: MirFunc,
    pub(super) is_const: bool,
    /// `Some` when the global's STORAGE is leaves (`nobox`): the init assigns
    /// these, never `name`, which no read ever looks at.
    pub(super) leaves: Option<Vec<ustr::Ustr>>,
    pub(super) order: (usize, u32),
}

pub(super) struct GlobalInitLine {
    pub(super) name: ustr::Ustr,
    pub(super) js: String,
}

/// Use the shared symbol spelling: a qualified logical name is not a JS
/// identifier, and flattening its separators would merge distinct namespaces.
pub(super) fn global_init_js_name(name: ustr::Ustr) -> String {
    format!("__ginit_{}", ident_of_key(name.as_str()))
}

/// Per-test emission lacks decorator applications, so reject reads that
/// depend on their module-load effects.
pub(super) fn decorator_populated_globals(ws: &Workspace) -> HashSet<ustr::Ustr> {
    use bone_core::hir::{BodyOwner, Expr};
    let mut set = HashSet::new();
    for module in &ws.modules {
        for (did, _) in module.decorators.iter() {
            let Some(body) = module.bodies.get(&BodyOwner::Decorator(did)) else {
                continue;
            };
            for (_, expr) in body.expr_arena.iter() {
                if let Expr::Global(name) = expr {
                    set.insert(*name);
                }
            }
        }
    }
    set
}

pub(super) fn build_state_index(ws: &Workspace) -> HashMap<ustr::Ustr, GlobalSite> {
    let mut m = HashMap::new();
    for module in &ws.modules {
        for (&name, &gid) in &module.global_by_name {
            m.insert(
                name,
                GlobalSite {
                    module: module.id,
                    gid,
                },
            );
        }
    }
    m
}

pub(super) fn build_const_global_names(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
) -> HashSet<ustr::Ustr> {
    let mut names = HashSet::new();
    for module in ws.source_modules() {
        for (_, global) in module.globals.iter().filter(|(_, g)| g.is_const) {
            names.insert(global.name);
            if let Ok(plan) = skull_mir::lower::GlobalPlan::for_declared_type(ws, scx, global.ty) {
                if let Some(leaves) = plan.leaf_names(symbol_of_key(global.name)) {
                    names.extend(leaves);
                }
            }
        }
    }
    names
}

pub(super) fn build_nobox_leaf_globals(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
) -> Result<HashMap<ustr::Ustr, ustr::Ustr>, CompilerError> {
    let mut m = HashMap::new();
    for module in &ws.modules {
        for (_gid, g) in module.globals.iter() {
            let plan = skull_mir::lower::GlobalPlan::for_declared_type(ws, scx, g.ty)
                .map_err(|e| CompilerError::new_no_location(e.message))?;
            if let Some(leaves) = plan.leaf_names(symbol_of_key(g.name)) {
                for leaf in leaves {
                    m.insert(leaf, g.name);
                }
            }
        }
    }
    Ok(m)
}

pub(super) fn queue_standalone(
    st: &mut WorklistState,
    ws: &Workspace,
    inst: Instance,
    findings: &mut Vec<CompilerError>,
) {
    let Some(&selected) = st.artifact_decls.get(&inst) else {
        findings.push(err(
            "JS: a retained specialization has no artifact declaration",
        ));
        return;
    };
    if js_supply(ws, &ws.module(selected.module).functions[selected.func]).is_some() {
        return;
    }
    if !st.view_instances.contains(&inst) {
        let f = &ws.module(inst.func.module).functions[inst.func.func];
        findings.push(CompilerError::new_with_label(
            f.name_span,
            format!(
                "JS: residual specialization of `{}` is absent from the artifact view",
                f.name
            ),
        ));
        return;
    }
    if st.standalone_seen.insert(inst.clone()) {
        st.standalone_specs.push(inst);
    }
}

/// Schedule residual delivery edges. The graph/view owns the logical instance,
/// declaration, boundary, and symbol; post-inline MIR only decides whether a
/// retained body still needs a definition.
pub(super) fn schedule_residual_calls(
    ws: &Workspace,
    mf: &MirFunc,
    st: &mut WorklistState,
) -> Result<(), CompilerError> {
    let mut findings = Vec::new();
    let mut calls = Vec::new();
    visit_placed(mf, &mut |body, iid| {
        if !body.spec_bindings.contains_key(&iid)
            && let Inst::Call { func, .. } | Inst::CallMulti { func, .. } = body.insts[iid]
        {
            calls.push((body, iid, func));
        }
    });
    for (body, iid, requested) in calls {
        let requested_f = &ws.module(requested.module).functions[requested.func];
        if js_supply(ws, requested_f).is_some() {
            continue;
        }
        let Some(instance) = st.names.instance_for_call(ws, body, iid, requested) else {
            findings.push(err(format!(
                "JS: call to `{}` has no shared InstanceGraph CallSite resolution",
                requested_f.name,
            )));
            continue;
        };
        let Some(&selected) = st.artifact_decls.get(&instance) else {
            if requested_f.inline_body.is_some() || requested_f.is_extern {
                findings.push(err(format!(
                    "JS subset: call to `{}` is not a mapped JS intrinsic",
                    requested_f.name
                )));
            } else {
                findings.push(err(format!(
                    "JS: call to `{}` resolves to an instance absent from the artifact view",
                    requested_f.name
                )));
            }
            continue;
        };
        let cf = &ws.module(selected.module).functions[selected.func];
        if st.names.get(&instance).is_none() {
            findings.push(err(format!(
                "JS: the artifact view has no symbol for the `{}` call instance",
                cf.name
            )));
            continue;
        }
        if js_supply(ws, cf).is_some() {
            continue;
        }
        if matches!(cf.inline_body, Some((bone_core::hir::FnBackend::Js, _))) {
            st.seen.insert(selected);
            st.add_inline_js_body(ws, instance.clone(), selected)?;
            continue;
        }
        if cf.is_extern && cf.backend == bone_core::hir::FnBackend::Js {
            continue;
        }
        if cf.inline_body.is_some() || cf.is_extern {
            findings.push(err(format!(
                "JS subset: call to `{}` is not a mapped JS intrinsic",
                cf.name
            )));
            continue;
        }
        if !cf.generics.is_empty() {
            if instance.args.is_empty() {
                findings.push(err(format!(
                    "JS: generic call to `{}` has no concrete graph arguments",
                    cf.name
                )));
            } else {
                queue_standalone(st, ws, instance, &mut findings);
            }
        } else if st.seen.insert(selected) {
            st.queue.push(selected);
        }
    }
    match merge_findings(findings) {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

fn register_fn_values_from_view(
    ws: &Workspace,
    mf: &MirFunc,
    st: &mut WorklistState,
    surfaces: &mut HashSet<ustr::Ustr>,
) -> Result<(), CompilerError> {
    let mut fn_value_ids: Vec<Idx<Inst>> = mf
        .call_sites
        .iter()
        .filter_map(|(&iid, call_site)| {
            (call_site.site.op == skull_graph::instance_graph::SiteOp::FnValue).then_some(iid)
        })
        .collect();
    // A global initializer or decorator body has no owner instance, so its
    // mints carry no call site. A fn value is never generic, so its base
    // instance is the graph's resolution.
    let mut ownerless: Vec<(Idx<Inst>, FuncRef)> = mf
        .fn_value_symbols
        .iter()
        .filter(|(iid, _)| !mf.call_sites.contains_key(iid))
        .map(|(&iid, &fr)| (iid, fr))
        .collect();
    ownerless.sort_by_key(|(iid, _)| iid.into_raw().into_u32());
    fn_value_ids.extend(ownerless.iter().map(|(iid, _)| *iid));
    for iid in fn_value_ids {
        let surface = match &mf.insts[iid] {
            Inst::Global(surface) => *surface,
            _ => {
                return Err(err(
                    "JS: a function-value record does not name a Global instruction",
                ));
            }
        };
        surfaces.insert(surface);
        let instance = match mf.call_sites.contains_key(&iid) {
            true => st.names.instance_for_site(ws, mf, iid),
            false => st.names.base_instance(ws, mf.fn_value_symbols[&iid]),
        };
        let Some(instance) = instance else {
            return Err(err(format!(
                "JS: function value `{surface}` has no shared InstanceGraph resolution"
            )));
        };
        if !st.fn_value_targets.contains(&instance) {
            return Err(err(format!(
                "JS: function value `{surface}` is absent from ArtifactView::fn_value_targets"
            )));
        }
        let Some(&selected) = st.artifact_decls.get(&instance) else {
            return Err(err(format!(
                "JS: function value `{surface}` resolves to an instance absent from the artifact view"
            )));
        };
        let Some(js_name) = st.names.get(&instance).cloned() else {
            return Err(err(format!(
                "JS: function value `{surface}` has no artifact symbol"
            )));
        };
        let f = &ws.module(selected.module).functions[selected.func];
        match js_supply(ws, f) {
            Some(JsSupply::Global(global)) => {
                st.gctx.fns.insert(surface, global.to_string());
                continue;
            }
            Some(JsSupply::Template(_)) => {
                return Err(err(format!(
                    "JS subset: `{}` is a host-bodied declaration with no JS binding, \
                     so it cannot be used as a function value",
                    f.name
                )));
            }
            None => {}
        }
        st.gctx.fns.entry(surface).or_insert(js_name);
        if f.is_extern && f.backend == bone_core::hir::FnBackend::Js {
            continue;
        }
        if matches!(f.inline_body, Some((bone_core::hir::FnBackend::Js, _))) {
            st.seen.insert(selected);
            st.add_inline_js_body(ws, instance.clone(), selected)?;
        } else if f.inline_body.is_some() || f.is_extern {
            return Err(err(format!(
                "JS subset: function value `{}` is not a mapped JS intrinsic",
                f.name
            )));
        } else if st.seen.insert(selected) {
            st.queue.push(selected);
        }
    }
    for nested in &mf.nested_fns {
        register_fn_values_from_view(ws, nested, st, surfaces)?;
    }
    Ok(())
}

pub(super) fn schedule_global_reads(
    ws: &Workspace,
    mf: &MirFunc,
    st: &mut WorklistState,
) -> Result<(), CompilerError> {
    let mut fn_value_names = HashSet::new();
    register_fn_values_from_view(ws, mf, st, &mut fn_value_names)?;
    for name in global_refs(mf) {
        if fn_value_names.contains(&name) {
            continue;
        }
        if name.as_str() == "error" || is_runtime_global(&name) {
            continue;
        } else if st.enum_defs.contains_key(&name) {
            st.gctx.enum_globals.insert(name);
        } else if let Some(&base) = st.nobox_leaf_globals.get(&name) {
            if !st.gctx.state.contains_key(&symbol_of_key(name))
                && !st.gctx.const_globals.contains(&name)
            {
                return Err(err(format!(
                    "JS: nobox leaf `{name}` has no compiler-owned storage slot"
                )));
            }
            if st.state_index.contains_key(&base) {
                st.queue_ginit(base);
            }
        } else if st.state_index.contains_key(&name) {
            if st.deco_globals.contains(&name) {
                return Err(err(format!(
                    "JS subset: global `{name}` is populated by decorator application \
                     (module-load side effect not emitted)"
                )));
            }
            if !st.gctx.state.contains_key(&symbol_of_key(name))
                && !st.gctx.const_globals.contains(&name)
            {
                return Err(err(format!(
                    "JS: global `{name}` has no compiler-owned storage slot"
                )));
            }
            st.queue_ginit(name);
        } else if ws.functions_by_name.contains_key(&name) {
            return Err(err(format!(
                "JS: function global `{name}` has no MIR fn-value record"
            )));
        }
    }
    Ok(())
}

/// Static callee emit-names of `mf`; indirect and bound calls add no ordering edge.
fn direct_callees(mf: &MirFunc, names: &CalleeNames) -> Vec<ustr::Ustr> {
    let mut out = Vec::new();
    visit_placed(mf, &mut |body, iid| {
        if let Inst::Call { func, .. } | Inst::CallMulti { func, .. } = body.insts[iid]
            && let Some(name) = names.get(&Instance::base(func))
        {
            out.push(Ustr::from(name.as_str()));
        }
    });
    out
}

pub(super) fn affected_const_names(
    ws: &Workspace,
    changed: &HashSet<ustr::Ustr>,
    const_order: &[ustr::Ustr],
) -> HashSet<ustr::Ustr> {
    if changed.is_empty() {
        return HashSet::default();
    }
    let deps = skull_mir::reach::symbol_references(ws);
    const_order
        .iter()
        .copied()
        .filter(|name| {
            changed.contains(name)
                || skull_mir::reach::forward_reach(&deps, *name)
                    .iter()
                    .any(|dependency| changed.contains(dependency))
        })
        .collect()
}

/// Order global-init assignments so each global inits after every global it transitively reads
/// through its call graph; a cycle rejects.
pub(super) fn order_global_inits(
    ginits: &[GlobalInit],
    lowered: &[(FuncRef, MirFunc)],
    names: &CalleeNames,
    state_slots: &HashMap<ustr::Ustr, String>,
) -> Result<Vec<GlobalInitLine>, CompilerError> {
    let mut direct_globals: rustc_hash::FxHashMap<Ustr, Vec<Ustr>> = Default::default();
    let mut direct_calls: rustc_hash::FxHashMap<Ustr, Vec<Ustr>> = Default::default();
    let mut record = |key: Ustr, mf: &MirFunc| {
        direct_globals.insert(key, global_refs(mf));
        direct_calls.insert(key, direct_callees(mf, names));
    };
    for g in ginits {
        record(Ustr::from(g.init_name.as_str()), &g.mf);
    }
    // Key by the mangled emit name, which is what `direct_callees` resolves to.
    for (fr, mf) in lowered {
        if let Some(emit_name) = names.get(&Instance::base(*fr)) {
            record(Ustr::from(emit_name.as_str()), mf);
        }
    }
    let reach = skull_mir::reach::transitive_global_reads(&direct_globals, &direct_calls);
    // A `nobox` global is READ by its leaf names, which are synthesized and never
    // appear in `global_by_name`; the leaves are the storage, so an edge to one
    // is an edge to the global that owns them.
    let leaf_owner: rustc_hash::FxHashMap<Ustr, Ustr> = ginits
        .iter()
        .filter_map(|g| g.leaves.as_ref().map(|ls| (g.name, ls)))
        .flat_map(|(base, ls)| ls.iter().map(move |&leaf| (leaf, base)))
        .collect();
    let pending = ginits
        .iter()
        .map(|g| skull_mir::reach::PendingInit {
            name: g.name,
            deps: reach.deps_of(Ustr::from(g.init_name.as_str())),
            item: g,
        })
        .collect();
    let ordered = skull_mir::reach::order_global_inits(pending, &leaf_owner).map_err(|stuck| {
        let names: Vec<&str> = stuck.iter().map(|name| name.as_str()).collect();
        err(format!(
            "JS subset: module globals are mutually recursive: {}",
            names.join(", ")
        ))
    })?;
    ordered
        .into_iter()
        .map(|g| global_init_line(g, state_slots))
        .collect()
}

fn global_init_line(
    g: &GlobalInit,
    state_slots: &HashMap<ustr::Ustr, String>,
) -> Result<GlobalInitLine, CompilerError> {
    let slot = |name: Ustr, scalar: bool| {
        if g.is_const {
            return Ok(js_member("__G", name.as_str()));
        }
        let key = if scalar { symbol_of_key(name) } else { name };
        let key = state_slots.get(&key).ok_or_else(|| {
            let mut message = format!("JS: global `{}` has no compiler-owned storage slot", g.name);
            if !scalar {
                message.push_str(&format!(" for `{name}`"));
            }
            err(message)
        })?;
        Ok(format!("__rock_globals[{key:?}]"))
    };
    let mut js = String::new();
    if !g.is_const {
        js.push_str("if (__rock_globals_fresh) ");
    }
    if let Some(leaves) = &g.leaves {
        if !g.is_const {
            js.push_str("{ ");
        }
        js.push_str(&format!("{} = {}();", slot(leaves[0], false)?, g.init_name));
        // Read the scratch return bank before another initializer can call.
        for (i, &leaf) in leaves.iter().enumerate().skip(1) {
            js.push_str(&format!(" {} = __rock_ret{i};", slot(leaf, false)?));
        }
        if !g.is_const {
            js.push_str(" }");
        }
    } else {
        js.push_str(&format!("{} = {}();", slot(g.name, true)?, g.init_name));
    }
    js.push('\n');
    Ok(GlobalInitLine { name: g.name, js })
}

fn global_refs(mf: &MirFunc) -> Vec<ustr::Ustr> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    visit_placed(mf, &mut |body, iid| {
        let name = match body.insts[iid] {
            Inst::Global(name) | Inst::StoreGlobal { name, .. } => name,
            _ => return,
        };
        if seen.insert(name) {
            out.push(name);
        }
    });
    out
}

fn visit_placed<'a>(body: &'a MirFunc, visit: &mut impl FnMut(&'a MirFunc, Idx<Inst>)) {
    for (_, block) in body.blocks.iter() {
        for &iid in &block.insts {
            visit(body, iid);
        }
    }
    for nested in &body.nested_fns {
        visit_placed(nested, visit);
    }
}
