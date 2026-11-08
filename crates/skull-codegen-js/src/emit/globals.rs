//! Decorator applications and global initializer scheduling.

use super::*;

/// A JS literal for a `@deco` annotation arg value (the JS analog of skull's
/// `annotation_value_lua`): the same value classes, rendered in JS syntax. A
/// non-literal (`AnnoValue::Other`) yields `None` (the caller rejects).
pub fn annotation_value_js(v: &bone_core::hir::AnnoValue) -> Option<String> {
    use bone_core::hir::AnnoValue as A;
    Some(match v {
        A::Int(i) => i.to_string(),
        A::Float(f) => js_number(*f).ok()?,
        A::Bool(b) => b.to_string(),
        A::String(s) => js_string_lit(s.as_str()).ok()?,
        A::Ident(id) => id.name.as_str().to_string(),
        A::IdentList(_) => return None,
        A::Other => return None,
    })
}

/// The JS argument list for one `@deco` application (the JS sibling of skull's
/// `decorator_call_args`): the SELECTION (named arg / defaulted `null` / missing-
/// required error) is the shared `skull_mir::decorator::select_decorator_args`;
/// only literal rendering (`annotation_value_js`) and the `null` default token
/// are JS-specific.
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

/// Decorator APPLICATION lines for every decorated fn workspace-wide (the JS
/// sibling of skull's Lua `decorator_application_lines`): one
/// `__deco_<name>({name: "..", params: [], func: <js name>}, <args>);` per
/// `@<deco>` annotation, in module then fn source order. The descriptor mirrors
/// luajit's bare `{ name, params, func }` table (no `__ty`: reads render as plain
/// `.name`/`.func`, and no deco body eq-compares `target`); `params` is an inert
/// `[]` placeholder (never read by a deco body, same as luajit's `{}`).
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
        let code_ref = format!("__rock_code_slot_ref({})", js_string_lit(&key)?);
        let descriptor = format!(
            "{{name: {}, params: [], func: {code_ref}}}",
            js_string_lit(app.function.name.as_str())?
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

/// The definition site of a module-level global (for lowering its initializer).
#[derive(Clone, Copy)]
pub(super) struct GlobalSite {
    pub(super) module: bone_core::prelude::ModuleId,
    pub(super) gid: Idx<bone_core::hir::GlobalDef>,
}

/// A lowered global initializer: `__G.<name> = <init_name>()` runs the `mf` body.
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

/// Global names referenced by any `deco` body. A decorator runs at MODULE LOAD
/// for each `@<deco>` application and typically mutates a registry global (e.g.
/// `@system` -> `UPDATE_SYSTEMS.push(...)`); the per-test JS emission does NOT
/// emit decorator applications, so a test reading such a global would see the
/// un-populated init value -- an emit-and-hope divergence. Reads of these reject
/// (SKIP) rather than silently diverge.
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

/// Map every module-level `let`/`global` name to its definition site.
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

/// `__nbg_<base>_<i>` -> `<base>`, for every global whose STORAGE is leaves.
/// A `nobox` global is not one name holding a box; lowering reads and writes the
/// leaves directly, and those names are synthesized, so they never appear in
/// `global_by_name`. They are state all the same.
///
/// Kept separate from [`build_state_index`] because the two answer different
/// questions: a leaf is state (a write to it is legal) but owns no INITIALIZER --
/// the base global's ginit assigns every leaf, so keying a ginit on a leaf would
/// emit that initializer once per leaf.
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
    fn direct_calls(mf: &MirFunc, out: &mut Vec<(Idx<Inst>, FuncRef)>) {
        for (_, block) in mf.blocks.iter() {
            for &iid in &block.insts {
                if mf.spec_bindings.contains_key(&iid) {
                    continue;
                }
                match &mf.insts[iid] {
                    Inst::Call { func, .. } | Inst::CallMulti { func, .. } => {
                        out.push((iid, *func))
                    }
                    _ => {}
                }
            }
        }
        for nested in &mf.nested_fns {
            direct_calls(nested, out);
        }
    }

    let mut findings = Vec::new();
    let mut calls = Vec::new();
    direct_calls(mf, &mut calls);
    for (iid, requested) in calls {
        let requested_f = &ws.module(requested.module).functions[requested.func];
        if js_supply(ws, requested_f).is_some() {
            continue;
        }
        let Some(instance) = st.names.instance_for_call(ws, mf, iid, requested) else {
            findings.push(err(format!(
                "JS: call to `{}` has no shared InstanceGraph CallSite resolution",
                requested_f.name
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
        let Some(name) = st.names.get(&instance).cloned() else {
            findings.push(err(format!(
                "JS: the artifact view has no symbol for the `{}` call instance",
                cf.name
            )));
            continue;
        };
        st.names.insert(instance.clone(), name);
        if js_supply(ws, cf).is_some() {
            continue;
        }
        if matches!(cf.inline_body, Some((bone_core::hir::FnBackend::Js, _))) {
            st.seen.insert(selected);
            add_inline_js_body(&mut st.inline_js, instance.clone(), selected);
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
            add_inline_js_body(&mut st.inline_js, instance.clone(), selected);
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
            if let Some(&site) = st.state_index.get(&base) {
                if st.ginit_seen.insert(base) {
                    st.ginit_queue.push((base, site));
                }
            }
        } else if let Some(&site) = st.state_index.get(&name) {
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
            if st.ginit_seen.insert(name) {
                st.ginit_queue.push((name, site));
            }
        } else if ws.functions_by_name.contains_key(&name) {
            return Err(err(format!(
                "JS: function global `{name}` has no MIR fn-value record"
            )));
        }
    }
    Ok(())
}

/// Static callee emit-names of `mf` (`Call`/`CallMulti`), resolved through
/// `names` (+ nested fns). Indirect (fn-value) and bound calls are not statically
/// resolvable and contribute no ordering edge -- parity with the luajit backend's
/// direct-call facts (`skull::mir_emit::direct_calls`).
fn direct_callees(mf: &MirFunc, names: &CalleeNames) -> Vec<ustr::Ustr> {
    pub(super) fn walk(mf: &MirFunc, names: &CalleeNames, out: &mut Vec<ustr::Ustr>) {
        for (_, block) in mf.blocks.iter() {
            for &iid in &block.insts {
                let fr = match &mf.insts[iid] {
                    Inst::Call { func, .. } | Inst::CallMulti { func, .. } => *func,
                    _ => continue,
                };
                if let Some(n) = names.get(&Instance::base(fr)) {
                    out.push(ustr::Ustr::from(n.as_str()));
                }
            }
        }
        for nf in &mf.nested_fns {
            walk(nf, names, out);
        }
    }
    let mut out = Vec::new();
    walk(mf, names, &mut out);
    out
}

/// The consts in `const_order` an overlay re-initializes: each changed const
/// and every const that transitively references one.
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

/// Order the global-init assignment lines so each global inits AFTER every global
/// it TRANSITIVELY reads through its call graph: `__G.<name> = <init_name>();`.
/// A shallow "reads in the init body" edge misses `ACT_UI_CONFIRM = action_new(..)`
/// where the `ACTIONS` read is inside `action_new`, not the init body -- so build
/// the transitive global-read closure over every emitted fn (ginits + lowered).
/// A genuine cycle rejects.
pub(super) fn order_global_inits(
    ginits: &[GlobalInit],
    lowered: &[(FuncRef, MirFunc)],
    names: &CalleeNames,
    state_slots: &HashMap<ustr::Ustr, String>,
) -> Result<Vec<GlobalInitLine>, CompilerError> {
    // Per-fn direct facts, keyed by emit name (ginit = init_name, lowered = js name).
    let mut direct_globals: rustc_hash::FxHashMap<Ustr, Vec<Ustr>> = Default::default();
    let mut direct_calls: rustc_hash::FxHashMap<Ustr, Vec<Ustr>> = Default::default();
    let mut record = |key: Ustr, mf: &MirFunc| {
        direct_globals.insert(key, global_refs(mf));
        direct_calls.insert(key, direct_callees(mf, names));
    };
    for g in ginits {
        record(Ustr::from(g.init_name.as_str()), &g.mf);
    }
    // Key by the EMIT name (`names[fr]`, mangled) -- that is what `direct_callees`
    // resolves a call to, NOT the unmangled `mf.name`.
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
    Ok(GlobalInitLine {
        name: g.name,
        js: match (g.is_const, &g.leaves) {
            (true, Some(leaves)) => {
                let mut js = format!(
                    "{} = {}();",
                    js_member("__G", leaves[0].as_str())?,
                    g.init_name
                );
                for (i, leaf) in leaves.iter().enumerate().skip(1) {
                    js.push_str(&format!(
                        " {} = __rock_ret{i};",
                        js_member("__G", leaf.as_str())?
                    ));
                }
                js.push('\n');
                js
            }
            (true, None) => format!(
                "{} = {}();\n",
                js_member("__G", g.name.as_str())?,
                g.init_name
            ),
            // Leaf storage: the ginit splats, so the extra leaves come
            // back through the scratch bank and must be read off it
            // before anything else can call and clobber it.
            (false, Some(leaves)) => {
                let keys: Vec<&str> = leaves
                    .iter()
                    .map(|leaf| {
                        state_slots.get(leaf).map(String::as_str).ok_or_else(|| {
                            err(format!(
                                "JS: global `{}` has no compiler-owned storage slot for `{leaf}`",
                                g.name
                            ))
                        })
                    })
                    .collect::<Result<_, _>>()?;
                let mut js = format!(
                    "if (__rock_globals_fresh) {{ __rock_globals[{:?}] = {}();",
                    keys[0], g.init_name
                );
                for (i, _) in leaves.iter().enumerate().skip(1) {
                    let key = keys[i];
                    js.push_str(&format!(" __rock_globals[{key:?}] = __rock_ret{i};"));
                }
                js.push_str(" }\n");
                js
            }
            (false, None) => {
                let key = state_slots.get(&symbol_of_key(g.name)).ok_or_else(|| {
                    err(format!(
                        "JS: global `{}` has no compiler-owned storage slot",
                        g.name
                    ))
                })?;
                format!(
                    "if (__rock_globals_fresh) __rock_globals[{key:?}] = {}();\n",
                    g.init_name
                )
            }
        },
    })
}

/// Every `Inst::Global` read + `Inst::StoreGlobal` write name in `mf` (main body
/// and hoisted nested fns) -- the module-global surface the emitter must resolve
/// (state, fn-value, or reject). Names are deduped in source order.
fn global_refs(mf: &MirFunc) -> Vec<ustr::Ustr> {
    pub(super) fn walk(mf: &MirFunc, out: &mut Vec<ustr::Ustr>, seen: &mut HashSet<ustr::Ustr>) {
        for (_, block) in mf.blocks.iter() {
            for &iid in &block.insts {
                let name = match &mf.insts[iid] {
                    Inst::Global(n) => *n,
                    Inst::StoreGlobal { name, .. } => *name,
                    _ => continue,
                };
                if seen.insert(name) {
                    out.push(name);
                }
            }
        }
        for nf in &mf.nested_fns {
            walk(nf, out, seen);
        }
    }
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    walk(mf, &mut out, &mut seen);
    out
}
