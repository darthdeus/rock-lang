//! The worklist that discovers inline, standalone and spec bodies.

use super::*;

/// An opaque inline body selected by the artifact view.
pub(super) struct JsInlineBody {
    pub(super) instance: Instance,
    pub(super) decl: FuncRef,
}

pub(super) struct WorklistState<'a> {
    pub(super) concrete: &'a skull_mir::concrete::ConcretePlans,
    pub(super) always: &'a skull_mir::concrete::ConcreteAlwaysPlans,
    pub(super) cache: skull_transform::InlineCache,
    pub(super) names: CalleeNames,
    pub(super) queue: Vec<FuncRef>,
    pub(super) seen: HashSet<FuncRef>,
    pub(super) lowered: Vec<(FuncRef, MirFunc)>,
    /// Standalone specialized copies, one per distinct `(callee, instantiation)`
    /// a body's `spec_bindings` still asks for after inlining. This is DELIVERY
    /// work, not instance discovery: every entry must already be in the graph
    /// view's retained population.
    pub(super) spec_lowered: Vec<(Instance, MirFunc)>,
    pub(super) standalone_specs: Vec<Instance>,
    pub(super) standalone_seen: HashSet<Instance>,
    pub(super) view_instances: BTreeSet<Instance>,
    pub(super) bound_tree_disagreements: HashSet<Instance>,
    /// `inline(js)` callees, emitted verbatim (their raw JS body, not MIR-lowered).
    pub(super) inline_js: Vec<JsInlineBody>,
    pub(super) state_index: HashMap<ustr::Ustr, GlobalSite>,
    pub(super) nobox_leaf_globals: HashMap<ustr::Ustr, ustr::Ustr>,
    /// Empty in program mode (applications ARE emitted, so a registry read is
    /// legitimate); the per-test reject set otherwise.
    pub(super) deco_globals: HashSet<ustr::Ustr>,
    pub(super) enum_defs: HashMap<ustr::Ustr, String>,
    pub(super) gctx: GlobalCtx,
    pub(super) ginit_queue: Vec<(ustr::Ustr, GlobalSite)>,
    pub(super) ginit_seen: HashSet<ustr::Ustr>,
    pub(super) ginits: Vec<GlobalInit>,
    /// One entry per DEFINITION that could not be emitted, keyed by its emitted
    /// JS name (unique per definition, so the key sorts the report into an order
    /// that does not depend on the walk's post-order). See [`Rejections`].
    pub(super) rejections: Vec<(String, CompilerError)>,
    pub(super) artifact_decls: HashMap<Instance, FuncRef>,
    pub(super) fn_value_targets: HashSet<Instance>,
    /// The bare `@nomangle` declarations each `inline(js)` body names. They are
    /// emitted only once the body that names them is.
    opaque_targets: BTreeMap<skull_graph::instance_graph::DeclKey, rustc_hash::FxHashSet<FuncRef>>,
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
        graph: &InstanceGraph,
        view: &ArtifactView,
        concrete: &'a skull_mir::concrete::ConcretePlans,
        always: &'a skull_mir::concrete::ConcreteAlwaysPlans,
    ) -> Result<Self, CompilerError> {
        let mut names = CalleeNames::from_graph(graph);
        for (fr, name) in roots {
            let key = names.base_instance(ws, *fr).ok_or_else(|| {
                err(format!(
                    "JS: root `{}` has no base InstanceGraph instance",
                    ws.module(fr.module).functions[fr.func].name
                ))
            })?;
            names.insert(key, name.clone());
        }
        for inst in view.realization.keys() {
            let name = view
                .publications
                .get(inst)
                .or_else(|| view.symbols.get(inst))
                .expect("artifact view names every retained instance")
                .clone();
            names.names.entry(inst.clone()).or_insert(name);
        }
        let bound_tree_disagreements = graph
            .bound_tree_disagreements()
            .map(|finding| finding.instance)
            .collect();
        let artifact_decls = view
            .realization
            .iter()
            .map(|(inst, realized)| (inst.clone(), realized.decl))
            .collect();
        let gctx = GlobalCtx {
            state: skull_mir::global_storage::global_storage_keys(ws, scx)
                .map_err(err)?
                .into_iter()
                .collect(),
            const_globals: build_const_global_names(ws, scx),
            ..GlobalCtx::default()
        };
        let mut st = WorklistState {
            concrete,
            always,
            standalone_specs: Vec::new(),
            standalone_seen: HashSet::new(),
            view_instances: view.instances.clone(),
            bound_tree_disagreements,
            cache: skull_transform::InlineCache::default(),
            names,
            queue: Vec::new(),
            seen: HashSet::new(),
            lowered: Vec::new(),
            spec_lowered: Vec::new(),
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
            artifact_decls,
            fn_value_targets: view.fn_value_targets.iter().cloned().collect(),
            opaque_targets: view.opaque_boundary_roots.clone(),
            json_types: super::json_types::JsonTypes::new(scx),
        };
        for &(requested, _) in roots {
            st.queue_root(ws, requested)?;
        }
        Ok(st)
    }

    /// Queue `requested` for emission as an entry: a body with no MIR call
    /// site naming it, so the view's base instance is what it delivers.
    fn queue_root(&mut self, ws: &Workspace, requested: FuncRef) -> Result<(), CompilerError> {
        let key = self.names.base_instance(ws, requested).ok_or_else(|| {
            err(format!(
                "JS: root `{}` has no base InstanceGraph instance",
                ws.module(requested.module).functions[requested.func].name
            ))
        })?;
        // A root named only from opaque JS (an inline body or the JSON type
        // table) has no published symbol; that JS calls it by its JS name.
        if self.names.get(&key).is_none() {
            self.names
                .insert(key.clone(), callee_js_name(ws, requested));
        }
        let fr = self.artifact_decls.get(&key).copied().unwrap_or(requested);
        let f = &ws.module(fr.module).functions[fr.func];
        if matches!(f.inline_body, Some((bone_core::hir::FnBackend::Js, _))) {
            if self.seen.insert(fr) {
                self.add_inline_js_body(ws, key, fr)?;
            }
        } else if !(f.is_extern && f.backend == bone_core::hir::FnBackend::Js) {
            if self.seen.insert(fr) {
                self.queue.push(fr);
            }
        } else {
            self.seen.insert(fr);
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
            .opaque_targets
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

    /// Record a definition that cannot be emitted and keep going. The definition
    /// contributes no JS, and the caller fails the whole emission on the
    /// accumulated set -- so the program is never returned with a hole in it.
    pub(super) fn reject(&mut self, name: &str, e: CompilerError) {
        self.rejections.push((name.to_string(), e));
    }

    /// An opaque `inline(js)` body has no MIR global-read facts. Recover its
    /// explicit `__G.name` reads and feed the corresponding initializer through
    /// the same ginit worklist as lowered bodies. Dynamic `__G[...]` remains
    /// outside the compiler-known surface and has no silent fallback.
    pub(super) fn queue_opaque_js_global_initializers(&mut self, ws: &Workspace) {
        let names: Vec<ustr::Ustr> = self
            .inline_js
            .iter()
            .flat_map(|body| opaque_js_global_names(ws, body))
            .collect();
        for name in names {
            let base = self.nobox_leaf_globals.get(&name).copied().unwrap_or(name);
            let Some(&site) = self.state_index.get(&base) else {
                continue;
            };
            if self.ginit_seen.insert(base) {
                self.ginit_queue.push((base, site));
            }
        }
    }

    /// Drain the worklists into their canonical order and register the code
    /// slots of every body, `extra` included. Returns the rejections so far.
    pub(super) fn finish(
        &mut self,
        ws: &Workspace,
        scx: &skull_mir::lower::ScalarCtx,
        extra: &[&MirFunc],
    ) -> Result<Vec<(String, CompilerError)>, CompilerError> {
        drain_worklists(ws, scx, self)?;
        self.canonicalize_emission_order();
        let interner = scx.interner();
        for mf in self
            .lowered
            .iter()
            .map(|(_, mf)| mf)
            .chain(self.spec_lowered.iter().map(|(_, mf)| mf))
            .chain(self.ginits.iter().map(|g| &g.mf))
            .chain(extra.iter().copied())
        {
            register_code_slots(ws, interner, mf, &mut self.gctx);
        }
        Ok(std::mem::take(&mut self.rejections))
    }

    /// Every emitted top-level body (fn defs + ginit init fns) for the cross-
    /// cutting passes (arity guard, hoisted-name set, vt_eq). Excludes nested
    /// (lambda) fns.
    pub(super) fn top_bodies(&self) -> Vec<&MirFunc> {
        self.lowered
            .iter()
            .map(|(_, mf)| mf)
            .chain(self.spec_lowered.iter().map(|(_, mf)| mf))
            .chain(self.ginits.iter().map(|g| &g.mf))
            .collect()
    }

    pub(super) fn canonicalize_emission_order(&mut self) {
        self.lowered
            .sort_by_key(|(fr, _)| (fr.module.index(), u32::from(fr.func.into_raw())));
        // Specialized copies share a `FuncRef`, so the emitted NAME is what
        // orders them (one per instantiation, all distinct).
        self.spec_lowered.sort_by(|a, b| {
            (
                a.0.func.module.index(),
                u32::from(a.0.func.func.into_raw()),
                &a.1.name,
            )
                .cmp(&(
                    b.0.func.module.index(),
                    u32::from(b.0.func.func.into_raw()),
                    &b.1.name,
                ))
        });
        // Not by `instance.args`: interned type ids follow worker timing, so
        // the emitted name is the stable key within one declaration.
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
            if st.ginit_seen.insert(*name) {
                st.ginit_queue.push((
                    *name,
                    GlobalSite {
                        module: source_module.id,
                        gid,
                    },
                ));
            }
        }
    }
    Ok(())
}

/// Emit an `inline(js)` fn verbatim: `function <js_name>(p1, p2) { <raw js> }`.
/// The body is the author's raw JS, spliced unchanged (mirrors the luajit path's
/// verbatim `inline(lua)` emission in `skull::mir_emit`).
pub(super) fn emit_inline_js_fn(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    plans: &skull_mir::concrete::ConcretePlans,
    instance: &Instance,
    fr: FuncRef,
    js_name: &str,
) -> Result<String, CompilerError> {
    let f = &ws.module(fr.module).functions[fr.func];
    // A @scalar_abi fn's value-struct params explode into their
    // `<name>__<leaf>` scalars, same as the lua path, so the exploded call
    // sites and the raw body's leaf names agree.
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
    // The publisher contract is checked against the selected body's ABI. The
    // canonical graph instance supplies identity; this sibling supplies the
    // opaque body and its parameter/result convention.
    let src = lower_publisher(&src, abi.result_lanes(), f.name.as_str())?;
    Ok(format!("function {js_name}({params}) {{\n{src}\n}}\n"))
}

/// Register a body's `static` locals as `__G` state: each lowers to a private
/// `__static_*` cell + done-flag global (`lower_static`). No initializer line
/// is needed -- an undefined done-flag is falsy, so the first-reach guard runs
/// the init exactly as Lua's nil does.
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

/// The `(callee, instantiation)` pairs `mf` and its hoisted bodies still ask for
/// after inlining. Per-BODY in `skull_mir`, so the nested walk is here: a hoisted
/// lambda is its own emitted callable and its residual bindings are its own.
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
