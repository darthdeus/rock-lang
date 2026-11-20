//! The worklist that discovers inline, standalone and spec bodies.

use super::*;

pub(super) struct JsInlineBody {
    pub(super) instance: Instance,
    pub(super) decl: FuncRef,
}

pub(super) struct DeliveredBody<'a> {
    pub(super) specialization: Option<Instance>,
    pub(super) plan: &'a skull_mir::concrete::ConcreteBodyPlan,
    pub(super) name: String,
    pub(super) mf: MirFunc,
}

impl DeliveredBody<'_> {
    pub(super) fn instance(&self) -> &Instance {
        self.specialization.as_ref().unwrap_or_else(|| {
            self.plan
                .function_instance()
                .expect("delivered function plan")
        })
    }
}

pub(super) struct WorklistState<'a> {
    pub(super) concrete: &'a skull_mir::concrete::ConcretePlans,
    pub(super) always: &'a skull_mir::concrete::ConcreteAlwaysPlans,
    pub(super) cache: skull_transform::InlineCache,
    pub(super) names: CalleeNames<'a>,
    pub(super) view: &'a ArtifactView,
    pub(super) queue: Vec<FuncRef>,
    pub(super) seen: HashSet<FuncRef>,
    pub(super) lowered: Vec<DeliveredBody<'a>>,
    pub(super) standalone_specs: Vec<Instance>,
    pub(super) standalone_seen: HashSet<Instance>,
    pub(super) graph: &'a InstanceGraph,
    pub(super) inline_js: Vec<JsInlineBody>,
    pub(super) state_index: HashMap<ustr::Ustr, GlobalSite>,
    pub(super) nobox_leaf_globals: HashMap<ustr::Ustr, ustr::Ustr>,
    pub(super) deco_globals: HashSet<ustr::Ustr>,
    pub(super) enum_defs: HashMap<ustr::Ustr, String>,
    pub(super) gctx: GlobalCtx,
    pub(super) ginit_queue: Vec<Ustr>,
    ginit_seen: HashSet<Ustr>,
    pub(super) ginits: Vec<GlobalInit>,
    pub(super) rejections: Vec<(String, CompilerError)>,
    pub(super) json_types: super::json_types::JsonTypes,
}

fn opaque_js_global_names(ws: &Workspace, body: &JsInlineBody) -> Vec<ustr::Ustr> {
    let Some((bone_core::hir::FnBackend::Js, source)) =
        ws.module(body.decl.module).functions[body.decl.func].inline_body
    else {
        return Vec::new();
    };
    let mut names = Vec::new();
    let mut rest = source.as_str();
    while let Some(offset) = rest.find("__G.") {
        let after = &rest[offset + "__G.".len()..];
        let end = after
            .char_indices()
            .find(|(_, ch)| !ch.is_ascii_alphanumeric() && *ch != '_')
            .map_or(after.len(), |(index, _)| index);
        if end > 0 {
            let symbol = &after[..end];
            if let Some(name) = ws
                .globals_by_name
                .keys()
                .find(|name| skull_mir::mangle::symbol_of_key(**name).as_str() == symbol)
            {
                names.push(*name);
            }
        }
        rest = &after[end..];
    }
    names
}

impl<'a> WorklistState<'a> {
    pub(super) fn new(
        ws: &Workspace,
        scx: &skull_mir::lower::ScalarCtx,
        roots: &[(FuncRef, String)],
        program_mode: bool,
        session: &'a JsSession,
    ) -> Result<Self, CompilerError> {
        let JsSession {
            graph,
            view,
            concrete,
        } = session;
        let mut names = CalleeNames {
            session: Some(session),
            ..Default::default()
        };
        for (fr, name) in roots {
            let key = names.base_instance(ws, *fr).ok_or_else(|| {
                err(format!(
                    "JS: root `{}` has no base InstanceGraph instance",
                    ws.module(fr.module).functions[fr.func].name
                ))
            })?;
            names.overrides.insert(key, name.clone());
        }
        let gctx = GlobalCtx {
            state: skull_mir::global_storage::global_storage_keys(ws, scx)
                .map_err(err)?
                .into_iter()
                .collect(),
            const_globals: build_const_global_names(ws, scx),
            ..GlobalCtx::default()
        };
        let mut st = WorklistState {
            concrete: &concrete.plans,
            always: &concrete.always_plans,
            standalone_specs: Vec::new(),
            standalone_seen: HashSet::new(),
            view,
            graph,
            cache: skull_transform::InlineCache::default(),
            names,
            queue: Vec::new(),
            seen: HashSet::new(),
            lowered: Vec::new(),
            inline_js: Vec::new(),
            state_index: build_state_index(ws),
            nobox_leaf_globals: build_nobox_leaf_globals(ws, scx)?,
            deco_globals: if program_mode {
                HashSet::new()
            } else {
                decorator_populated_globals(ws)
            },
            enum_defs: build_enum_global_defs(ws),
            gctx,
            ginit_queue: Vec::new(),
            ginit_seen: HashSet::new(),
            ginits: Vec::new(),
            rejections: Vec::new(),
            json_types: super::json_types::JsonTypes::new(scx),
        };
        for &(requested, _) in roots {
            st.queue_root(ws, requested)?;
        }
        Ok(st)
    }

    pub(super) fn selected_decl(&self, instance: &Instance) -> Option<FuncRef> {
        self.view
            .realization
            .get(instance)
            .map(|realized| realized.decl)
    }

    fn queue_root(&mut self, ws: &Workspace, requested: FuncRef) -> Result<(), CompilerError> {
        let key = self.names.base_instance(ws, requested).ok_or_else(|| {
            err(format!(
                "JS: root `{}` has no base InstanceGraph instance",
                ws.module(requested.module).functions[requested.func].name
            ))
        })?;
        // A root named only from opaque JS has no published symbol; that JS calls it by its JS
        // name.
        if self.names.get(&key).is_none() {
            self.names
                .overrides
                .insert(key.clone(), callee_js_name(ws, requested));
        }
        let fr = self.selected_decl(&key).unwrap_or(requested);
        let f = &ws.module(fr.module).functions[fr.func];
        if self.seen.insert(fr) {
            if matches!(f.inline_body, Some((bone_core::hir::FnBackend::Js, _))) {
                self.add_inline_js_body(ws, key, fr)?;
            } else if !(f.is_extern && f.backend == bone_core::hir::FnBackend::Js) {
                self.queue.push(fr);
            }
        }
        Ok(())
    }

    pub(super) fn add_inline_js_body(
        &mut self,
        ws: &Workspace,
        instance: Instance,
        decl: FuncRef,
    ) -> Result<(), CompilerError> {
        if self.inline_js.iter().any(|body| body.instance == instance) {
            return Ok(());
        }
        let factories = self.json_types.add_body(ws, decl, &instance.args)?;
        self.inline_js.push(JsInlineBody { instance, decl });
        let mut targets: Vec<FuncRef> = self
            .view
            .opaque_boundary_roots
            .get(&skull_graph::instance_graph::DeclKey(decl))
            .map(|targets| targets.iter().copied().collect())
            .unwrap_or_default();
        targets.extend(factories);
        targets.sort_by_key(|f| (f.module.index(), f.func.into_raw()));
        for target in targets {
            self.queue_root(ws, target)?;
        }
        Ok(())
    }

    pub(super) fn reject(&mut self, name: &str, e: CompilerError) {
        self.rejections.push((name.to_string(), e));
    }

    pub(super) fn queue_ginit(&mut self, name: Ustr) {
        if self.ginit_seen.insert(name) {
            self.ginit_queue.push(name);
        }
    }

    /// An opaque `inline(js)` body has no MIR global-read facts: recover its explicit `__G.name`
    /// reads; dynamic `__G[...]` has no fallback.
    pub(super) fn queue_opaque_js_global_initializers(&mut self, ws: &Workspace) {
        let names: Vec<ustr::Ustr> = self
            .inline_js
            .iter()
            .flat_map(|body| opaque_js_global_names(ws, body))
            .collect();
        for name in names {
            let base = self.nobox_leaf_globals.get(&name).copied().unwrap_or(name);
            if self.state_index.contains_key(&base) {
                self.queue_ginit(base);
            }
        }
    }

    pub(super) fn finish(
        &mut self,
        ws: &Workspace,
        scx: &skull_mir::lower::ScalarCtx,
        extra: &[&MirFunc],
    ) -> Result<Vec<(String, CompilerError)>, CompilerError> {
        drain_worklists(ws, scx, self)?;
        self.canonicalize_emission_order();
        let interner = scx.interner();
        for mf in body_mirs(&self.lowered, &self.ginits).chain(extra.iter().copied()) {
            register_code_slots(ws, interner, mf, &mut self.gctx);
        }
        Ok(std::mem::take(&mut self.rejections))
    }

    pub(super) fn top_bodies(&self) -> impl Iterator<Item = &MirFunc> {
        body_mirs(&self.lowered, &self.ginits)
    }

    pub(super) fn canonicalize_emission_order(&mut self) {
        self.lowered.sort_by_key(|body| {
            let f = body.instance().func;
            (
                body.specialization.is_some(),
                f.module.index(),
                u32::from(f.func.into_raw()),
                body.mf.name,
            )
        });
        // Not by `instance.args`: interned type ids follow worker timing.
        let names = &self.names;
        self.inline_js.sort_by_cached_key(|body| {
            (
                body.decl.module.index(),
                u32::from(body.decl.func.into_raw()),
                names.get(&body.instance).cloned(),
            )
        });
        self.ginits.sort_by_key(|g| g.order);
    }
}

fn body_mirs<'a>(
    lowered: &'a [DeliveredBody<'_>],
    ginits: &'a [GlobalInit],
) -> impl Iterator<Item = &'a MirFunc> {
    lowered
        .iter()
        .map(|body| &body.mf)
        .chain(ginits.iter().map(|global| &global.mf))
}

pub(super) fn queue_lifecycle_dependency_initializers(
    ws: &Workspace,
    st: &mut WorklistState<'_>,
    plan: &skull_mir::lifecycle::LifecyclePlan,
) -> Result<(), CompilerError> {
    for entry in &plan.entries {
        for dependency in entry.qualification.dependencies() {
            let CodeIdentity::Artifact(skull_mir::code_identity::CodeOwner::Artifact {
                kind: skull_mir::code_identity::CodeOwnerKind::Global,
                module,
                name,
            }) = &dependency.identity
            else {
                return Err(err("JS: lifecycle dependency is not a top-level global"));
            };
            let Some(source_module) = ws
                .modules
                .iter()
                .find(|candidate| candidate.file == *module)
            else {
                return Err(err(format!(
                    "JS: lifecycle dependency `{name}` has no source module"
                )));
            };
            let Some(&gid) = source_module.global_by_name.get(name) else {
                return Err(err(format!(
                    "JS: lifecycle dependency `{name}` has no source declaration"
                )));
            };
            if !source_module.globals[gid].is_const {
                return Err(err(format!(
                    "JS: lifecycle dependency `{name}` is not immutable"
                )));
            }
            st.queue_ginit(*name);
        }
    }
    Ok(())
}

pub(super) fn emit_inline_js_fn(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    plans: &skull_mir::concrete::ConcretePlans,
    instance: &Instance,
    fr: FuncRef,
    js_name: &str,
) -> Result<String, CompilerError> {
    let f = &ws.module(fr.module).functions[fr.func];
    let selected = Instance {
        func: fr,
        args: instance.args.clone(),
    };
    let abi = skull_mir::lower::owner_abi_plan(ws, scx, plans, &selected)?;
    let lua_param_names = abi.lua_param_names(ws);
    let params = match &lua_param_names {
        Some(names) => names.join(", "),
        None => f
            .param_decls
            .iter()
            .map(|p| p.name.to_string())
            .collect::<Vec<_>>()
            .join(", "),
    };
    let src = match f.inline_body {
        Some((bone_core::hir::FnBackend::Js, s)) => s.to_string(),
        _ => String::new(),
    };
    let src = lower_publisher(&src, abi.result_lanes(), f.name.as_str())?;
    Ok(format!("function {js_name}({params}) {{\n{src}\n}}\n"))
}

/// Register a body's `static` locals as `__G` state; no initializer is needed since an undefined
/// done-flag is falsy.
pub(super) fn register_statics(
    ws: &Workspace,
    interner: &skull_mir::monoty::MonoInterner,
    mf: &MirFunc,
    gctx: &mut GlobalCtx,
) -> Result<(), CompilerError> {
    let slots =
        skull_mir::global_storage::static_storage_slots(ws, interner, &mf.statics).map_err(err)?;
    gctx.state.extend(slots);
    for nf in &mf.nested_fns {
        register_statics(ws, interner, nf, gctx)?;
    }
    Ok(())
}

pub(super) fn collect_spec_bound_calls(
    ws: &Workspace,
    mf: &MirFunc,
    out: &mut Vec<skull_mir::bound::SpecDemand>,
) -> Result<(), CompilerError> {
    skull_mir::reach::spec_bound_calls(ws, mf, out)?;
    for nf in &mf.nested_fns {
        collect_spec_bound_calls(ws, nf, out)?;
    }
    Ok(())
}
