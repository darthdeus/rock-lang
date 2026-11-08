//! Delivering discovered bodies as JS chunks, and the scalar ABI check.

use super::*;

/// The backend-local delivery scheduler. Logical instances, selected
/// declarations and symbols come from `ArtifactView`; this type only lowers
/// retained declarations, follows post-inline residual calls, and emits the
/// global/opaque bodies those definitions still require.
pub(super) struct JsDelivery<'a, 'b> {
    pub(super) ws: &'a Workspace,
    pub(super) scx: &'a skull_mir::lower::ScalarCtx,
    pub(super) st: &'a mut WorklistState<'b>,
}

impl<'a, 'b> JsDelivery<'a, 'b> {
    /// Lower + transform one fn through the same per-body pipeline the luajit
    /// path runs (lower -> inline -> cleanup -> closures -> FORL -> residual-
    /// bound-call validation; coroutines skip inline/FORL), then record it.
    /// A rejected body is recorded and delivery continues; its residual facts
    /// are still scheduled so a sibling definition is not hidden by the error.
    fn visit_fn(&mut self, fr: FuncRef) -> Result<(), CompilerError> {
        let f = &self.ws.module(fr.module).functions[fr.func];
        let name = self
            .st
            .names
            .name_for_decl(self.ws, fr)
            .expect("queued fn has a name")
            .clone();
        let plan = match self.st.concrete.get(&Instance::base(fr)) {
            Some(plan) => plan,
            None => {
                self.st.reject(
                    &name,
                    err(format!(
                        "JS: no concrete plan for retained function `{}`",
                        f.name
                    )),
                );
                return Ok(());
            }
        };
        let mut mf = match skull_mir::lower::lower_function_with_plans(
            self.ws,
            self.scx,
            plan,
            self.st.concrete,
        ) {
            Ok(mf) => mf,
            Err(e) => {
                self.st.reject(&name, e);
                return Ok(());
            }
        };
        if mf.is_coroutine {
            if let Err(e) = skull_transform::cleanup_with_nobox(self.ws, &mut mf) {
                self.st.reject(&name, e);
                return Ok(());
            }
        } else {
            if let Err(e) = prepare_body(
                self.ws,
                self.scx,
                fr,
                &mut mf,
                &name,
                &mut self.st.cache,
                self.st.concrete,
            ) {
                self.st.reject(&name, e);
                return Ok(());
            }
        }
        let mut findings: Vec<CompilerError> = Vec::new();
        collect(
            &mut findings,
            skull_transform::validate_no_residual_bound_calls(self.ws, f, &mf),
        );
        collect(&mut findings, nobox_errs(self.ws, &mf));
        for nf in &mf.nested_fns {
            collect(&mut findings, nobox_errs(self.ws, nf));
        }
        findings.extend(self.schedule_body(&mf)?);
        match merge_findings(findings) {
            Some(e) => self.st.reject(&name, e),
            None => self.st.lowered.push((fr, mf)),
        }
        Ok(())
    }

    /// Lower one standalone specialized copy already retained by the graph and
    /// required by a post-inline residual call. The view supplies identity,
    /// declaration and symbol; this method only performs delivery lowering.
    fn lower_standalone(&mut self, inst: Instance) -> Result<(), CompilerError> {
        if !self.st.view_instances.contains(&inst) {
            self.st.reject(
                &format!("instance:{:?}", inst.func),
                err("JS: a residual specialization is absent from the artifact view"),
            );
            return Ok(());
        }
        if self.st.bound_tree_disagreements.contains(&inst) {
            let f = &self.ws.module(inst.func.module).functions[inst.func.func];
            let name = inst.symbol(self.ws, self.scx.interner());
            self.st.reject(
                &name,
                CompilerError::new_with_label(
                    f.name_span,
                    format!(
                        "internal: two call sites demand `{name}` with different `Iterator` \
                         bound resolutions, so one emitted definition cannot serve both"
                    ),
                ),
            );
            return Ok(());
        }
        if let Some(at) =
            skull_mir::monoty::too_deep_to_instantiate(inst.arg_types(self.scx.interner()).iter())
        {
            let f = &self.ws.module(inst.func.module).functions[inst.func.func];
            let at = at.render();
            self.st.reject(
                f.name.as_ref(),
                CompilerError::new_with_label(
                    f.name_span,
                    format!(
                        "polymorphic recursion: instantiating `{}` at `{at}` exceeds the \
                         instantiation depth limit of {}. A recursive call whose type \
                         arguments GROW never terminates -- each call needs a new body -- so it \
                         is rejected rather than expanded. Same-instance recursion (type \
                         arguments unchanged) is unaffected.",
                        f.name,
                        skull_mir::monoty::MAX_INSTANTIATION_DEPTH,
                    ),
                ),
            );
            return Ok(());
        }
        let Some(&fr) = self.st.artifact_decls.get(&inst) else {
            self.st.reject(
                &format!("instance:{:?}", inst.func),
                err("JS: a lowered specialization is absent from the artifact view"),
            );
            return Ok(());
        };
        let Some(name) = self.st.names.get(&inst).cloned() else {
            self.st.reject(
                &format!("instance:{:?}", fr),
                err("JS: the artifact view has no symbol for a retained instance"),
            );
            return Ok(());
        };
        let lowered_inst = Instance {
            func: fr,
            args: inst.args.clone(),
        };
        let plan = match self.st.concrete.get(&lowered_inst) {
            Some(plan) => plan,
            None => {
                self.st.reject(
                    &name,
                    err("JS: no concrete plan for the selected retained body"),
                );
                return Ok(());
            }
        };
        let mut mf = match skull_mir::lower::lower_instance_with_plans(
            self.ws,
            self.scx,
            plan,
            self.st.concrete,
        ) {
            Ok(mf) => mf,
            Err(e) => {
                self.st.reject(&name, e);
                return Ok(());
            }
        };
        let f = &self.ws.module(fr.module).functions[fr.func];
        // BEFORE every transform, and for the same reason as the luajit path:
        // an `Iterator`-bounded body's placeholders are what make it unemittable
        // on its own, and the demand that reached here is what resolves them.
        let tree = plan.bound_tree.clone();
        if f.generic_bounds.iter().any(Option::is_some) && tree.is_empty() {
            self.st.reject(
                &name,
                CompilerError::new_with_label(
                    f.name_span,
                    "JS: the concrete plan has no resolved bound tree for a bounded specialization"
                        .to_string(),
                ),
            );
            return Ok(());
        }
        if let Err(e) = skull_transform::bound::resolve_standalone_bound_calls(
            self.ws,
            self.scx,
            self.scx.interner(),
            &mut mf,
            &tree,
            self.st.concrete,
        ) {
            self.st.reject(&name, e);
            return Ok(());
        }
        // A specialized `co fn` takes cleanup only, like an unspecialized one:
        // closure conversion here would erase the lambdas of its detached
        // defer copies (unreachable from the entry) before the coroutine
        // preparation splits them out.
        let prepared = if mf.is_coroutine {
            skull_transform::cleanup_with_nobox(self.ws, &mut mf)
        } else {
            prepare_body(
                self.ws,
                self.scx,
                fr,
                &mut mf,
                &name,
                &mut self.st.cache,
                self.st.concrete,
            )
        };
        if let Err(e) = prepared {
            self.st.reject(&name, e);
            return Ok(());
        }
        let mut findings: Vec<CompilerError> = Vec::new();
        collect(&mut findings, nobox_errs(self.ws, &mf));
        for nf in &mf.nested_fns {
            collect(&mut findings, nobox_errs(self.ws, nf));
        }
        mf.name = ustr::Ustr::from(name.as_str());
        self.st.names.insert(inst.clone(), name.clone());
        findings.extend(self.schedule_body(&mf)?);
        match merge_findings(findings) {
            Some(e) => self.st.reject(&name, e),
            None => self.st.spec_lowered.push((inst, mf)),
        }
        Ok(())
    }

    /// Lower one module global's initializer (never inlined, mirroring the luajit
    /// global path). A global with no initializer contributes no node body.
    fn visit_ginit(&mut self, gname: ustr::Ustr) -> Result<(), CompilerError> {
        let site = self.st.state_index[&gname];
        let init_name = global_init_js_name(gname);
        let owner = skull_mir::concrete::ConcreteOwner::Global {
            module: site.module,
            index: site.gid.into_raw().into_u32(),
        };
        let concrete_plan = self.st.always.get(&owner);
        let lowered = match concrete_plan {
            Some(plan) => skull_mir::lower::lower_global_with_plans(
                self.ws,
                self.scx,
                plan,
                self.st.concrete,
                ustr::Ustr::from(init_name.as_str()),
            ),
            None if self
                .ws
                .module(site.module)
                .bodies
                .get(&bone_core::hir::BodyOwner::Global(site.gid))
                .and_then(|body| body.root)
                .is_none() =>
            {
                Ok(None)
            }
            None => Err(err(format!("JS: no concrete plan for global `{gname}`"))),
        };
        let lowered = match lowered {
            Ok(l) => l,
            Err(e) => {
                self.st.reject(&init_name, e);
                return Ok(());
            }
        };
        let Some(mut mf) = lowered else { return Ok(()) };
        if let Err(e) = convert_body(self.ws, &mut mf, &init_name) {
            self.st.reject(&init_name, e);
            return Ok(());
        }
        let mut findings: Vec<CompilerError> = Vec::new();
        collect(&mut findings, nobox_errs(self.ws, &mf));
        for nf in &mf.nested_fns {
            collect(&mut findings, nobox_errs(self.ws, nf));
        }
        findings.extend(self.schedule_body(&mf)?);
        if let Some(e) = merge_findings(findings) {
            self.st.reject(&init_name, e);
            return Ok(());
        }
        let gdecl = &self.ws.module(site.module).globals[site.gid];
        let is_const = gdecl.is_const;
        let leaves =
            match skull_mir::lower::GlobalPlan::for_declared_type(self.ws, self.scx, gdecl.ty) {
                Ok(plan) => plan.leaf_names(symbol_of_key(gdecl.name)),
                Err(error) => {
                    self.st.reject(
                        &init_name,
                        CompilerError::new_no_location(format!(
                            "JS: global `{}` representation planning failed: {}",
                            gdecl.name, error.message
                        )),
                    );
                    return Ok(());
                }
            };
        self.st.ginits.push(GlobalInit {
            name: gname,
            init_name,
            mf,
            is_const,
            leaves,
            order: (site.module.index(), u32::from(site.gid.into_raw())),
        });
        Ok(())
    }

    /// Queue everything this body still needs after inlining, returning the
    /// body's delivery findings (unmapped callees) for the caller to accumulate.
    ///
    /// An `Err` is the ONE scheduling outcome that is not a census finding:
    /// global scheduling rejects exactly once, on a read of a
    /// decorator-populated global, and the per-test driver matches that message
    /// to RETRY the file in program mode (where the applications are emitted).
    /// Demoting it to a sibling would turn a test that lifts into a test that
    /// skips, so it aborts the emission immediately as before.
    pub(super) fn schedule_body(
        &mut self,
        mf: &MirFunc,
    ) -> Result<Vec<CompilerError>, CompilerError> {
        let ws = self.ws;
        let st = &mut *self.st;
        register_statics(ws, self.scx.interner(), mf, &mut st.gctx)?;
        let mut findings = Vec::new();
        let mut demands = Vec::new();
        collect(
            &mut findings,
            collect_spec_bound_calls(ws, mf, &mut demands),
        );
        for demand in demands {
            if st.bound_tree_disagreements.contains(&demand.inst) {
                let f = &ws.module(demand.inst.func.module).functions[demand.inst.func.func];
                collect::<()>(
                    &mut findings,
                    Err(CompilerError::new_with_label(
                        f.name_span,
                        format!(
                            "internal: two call sites demand a specialization of `{}` with \
                             different `Iterator` bound resolutions, so one emitted \
                             definition cannot serve both",
                            f.name
                        ),
                    )),
                );
            } else {
                queue_standalone(st, ws, demand.inst, &mut findings);
            }
        }
        collect(&mut findings, schedule_residual_calls(self.ws, mf, st));
        schedule_global_reads(self.ws, mf, st)?;
        Ok(findings)
    }
}

/// Expand the reachable program from whatever the root seeding left on the
/// worklists. Emission order is canonicalized afterwards, so the driver's
/// post-order does not reach the output.
pub(super) fn drain_worklists(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    st: &mut WorklistState,
) -> Result<(), CompilerError> {
    let mut delivery = JsDelivery { ws, scx, st };
    let mut spec_cursor = 0;
    loop {
        while let Some(fr) = delivery.st.queue.pop() {
            delivery.visit_fn(fr)?;
        }
        delivery.st.queue_opaque_js_global_initializers(ws);
        while let Some((gname, _)) = delivery.st.ginit_queue.pop() {
            delivery.visit_ginit(gname)?;
        }
        if !delivery.st.queue.is_empty() || !delivery.st.ginit_queue.is_empty() {
            continue;
        }
        if let Some(inst) = delivery.st.standalone_specs.get(spec_cursor).cloned() {
            spec_cursor += 1;
            delivery.lower_standalone(inst)?;
            continue;
        }
        break;
    }
    Ok(())
}

/// Emit one lowered TOP body: a coroutine through the trampoline pipeline, else
/// the ordinary fn path plus its hoisted (`convert_closures`) sibling bodies.
pub(super) fn emit_lowered_fn(
    ws: &Workspace,
    interner: &skull_mir::monoty::MonoInterner,
    mf: &MirFunc,
    js_name: &str,
    concrete: &skull_mir::concrete::ConcreteBodyPlan,
    names: &CalleeNames,
    hoisted: &HashSet<ustr::Ustr>,
    gctx: &GlobalCtx,
) -> Result<(String, CodeArtifact), CompilerError> {
    if mf.is_coroutine {
        let plan = skull_mir::lower::CoroutinePlan::from_concrete_plan(
            ws,
            interner,
            concrete,
            js_name.to_string(),
        )
        .map_err(|reason| {
            err(format!(
                "JS: cannot build concrete coroutine plan: {}",
                reason.bucket()
            ))
        })?;
        return emit_coroutine_js(ws, interner, &plan, concrete, mf, names, hoisted, gctx);
    }
    let out = emit_body_with_nested(ws, mf, js_name, names, hoisted, gctx)?;
    let mut code_artifact = js_code_artifact(ws, mf);
    code_artifact.set_body_names(mf, js_name);
    Ok((out, code_artifact))
}

/// The standalone specialized copies, each under its `spec_fn_name`.
pub(super) fn emit_spec_chunks(
    ws: &Workspace,
    interner: &skull_mir::monoty::MonoInterner,
    st: &WorklistState,
    hoisted: &HashSet<ustr::Ustr>,
    rejections: &mut Vec<(String, CompilerError)>,
) -> HashMap<Instance, JsSpecChunk> {
    let mut chunks = HashMap::new();
    for (inst, mf) in &st.spec_lowered {
        let name = st
            .names
            .get(inst)
            .expect("a lowered specialization has a name");
        let selected_decl = match st.artifact_decls.get(inst).copied() {
            Some(decl) => decl,
            None => {
                rejections.push((
                    name.clone(),
                    err("JS: no selected declaration for specialization"),
                ));
                continue;
            }
        };
        let selected = Instance {
            func: selected_decl,
            args: inst.args.clone(),
        };
        let plan = match st.concrete.get(&selected) {
            Some(plan) => plan,
            None => {
                rejections.push((
                    name.clone(),
                    err("JS: no concrete plan for selected specialization"),
                ));
                continue;
            }
        };
        match emit_lowered_fn(ws, interner, mf, name, plan, &st.names, hoisted, &st.gctx) {
            Ok((js, code_artifact)) => {
                chunks.insert(
                    inst.clone(),
                    JsSpecChunk {
                        name: name.clone(),
                        js,
                        code_artifact,
                    },
                );
            }
            Err(e) => rejections.push((name.clone(), e)),
        }
    }
    chunks
}

pub(super) fn ordered_spec_chunks<'a>(
    chunks: &'a HashMap<Instance, JsSpecChunk>,
    order: &[(Instance, MirFunc)],
) -> Vec<&'a JsSpecChunk> {
    let ordered: Vec<_> = order
        .iter()
        .filter_map(|(inst, _)| chunks.get(inst))
        .collect();
    assert_eq!(
        ordered.len(),
        chunks.len(),
        "every emitted specialization must retain its delivery order"
    );
    ordered
}

pub(super) fn emit_spec_bodies(
    ws: &Workspace,
    interner: &skull_mir::monoty::MonoInterner,
    st: &WorklistState,
    hoisted: &HashSet<ustr::Ustr>,
    rejections: &mut Vec<(String, CompilerError)>,
) -> String {
    let chunks = emit_spec_chunks(ws, interner, st, hoisted, rejections);
    ordered_spec_chunks(&chunks, &st.spec_lowered)
        .into_iter()
        .map(|chunk| chunk.js.as_str())
        .collect()
}

/// Scalar-ABI mismatch guard: a value-struct arg scalarizes to N leaf values at
/// the call, but a co-emitted callee that uses the param as a WHOLE value keeps
/// a single MIR param -- so `argc > arity` would silently drop leaves. Residual
/// co-emitted callees were NOT inlined (whole-value use blocks eligibility), so
/// reject rather than emit-and-hope. `argc < arity` down to the count of params
/// WITHOUT a default is the legitimate omitted-defaulted-arg shape.
///
/// Every offending BODY, keyed by its name: one mismatch per body used to abort
/// the emission, hiding the others.
pub(super) fn check_scalar_abi(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    lowered: &[(FuncRef, MirFunc)],
    spec_lowered: &[(Instance, MirFunc)],
    bodies: &[&MirFunc],
    concrete: &skull_mir::concrete::ConcretePlans,
    artifact_decls: &HashMap<Instance, FuncRef>,
) -> Vec<(String, CompilerError)> {
    let arities: HashMap<FuncRef, usize> = lowered
        .iter()
        .map(|(fr, mf)| (*fr, mf.params.len()))
        .collect();
    // A SPECIALIZED callee takes the scalar ABI its instantiation implies, which
    // is exactly where a value-struct arg legitimately becomes several leaves --
    // so a spec-bound call must be measured against its own copy, not the shared
    // body it shares a `FuncRef` with. Checking the shared arity reads every
    // correct specialized call as a dropped-leaf bug.
    let spec_arities: HashMap<Instance, usize> = spec_lowered
        .iter()
        .map(|(inst, mf)| (inst.clone(), mf.params.len()))
        .collect();
    let mut out: Vec<(String, CompilerError)> = Vec::new();
    for mf in bodies {
        let mut findings: Vec<CompilerError> = Vec::new();
        for (_, block) in mf.blocks.iter() {
            for &iid in &block.insts {
                let (callee, argc) = match &mf.insts[iid] {
                    Inst::Call { func, args } | Inst::CallMulti { func, args, .. } => {
                        (func, args.len())
                    }
                    _ => continue,
                };
                // ONE admission rule, over the EMITTED BODY the call targets.
                // The two arms decide only WHICH body that is; the arity window
                // is then the same question of both, asked through the same ABI
                // plan. Stating the rule twice is how the specialized arm came
                // to demand `argc == arity`: it lost the omitted-defaulted-
                // argument window the shared arm allowed, so the one shape that
                // exists only at a specialization -- a defaulted param behind a
                // scalarized value-struct one -- rejected while luajit compiled
                // and ran it.
                let (body, arity, what) = match mf.spec_bindings.get(&iid) {
                    Some(inst) => match spec_arities.get(inst) {
                        // An unemitted specialization is `callee_name`'s
                        // rejection to make -- it knows which name went missing.
                        None => continue,
                        Some(&arity) => (inst.clone(), arity, "specialized "),
                    },
                    None => match arities.get(callee) {
                        None => continue,
                        Some(&arity) => (Instance::base(*callee), arity, ""),
                    },
                };
                let selected = artifact_decls.get(&body).map(|decl| Instance {
                    func: *decl,
                    args: body.args.clone(),
                });
                // The ABI is the shared oracle's answer about that emitted
                // body; the arms only decide which entry point matches the
                // selection: `owner_abi_plan` for a body serving its own
                // canonical instance, the sibling entry point when the
                // artifact selected a different declaration.
                let abi = match selected.as_ref().filter(|inst| inst.func != body.func) {
                    Some(selected) => {
                        match concrete.get(&body).or_else(|| concrete.get(selected)) {
                            Some(plan) => Ok(skull_mir::lower::selected_sibling_abi_plan(
                                ws, scx, concrete, selected, plan,
                            )),
                            None if skull_mir::lower::requires_concrete_body_plan(ws, selected) => {
                                findings.push(err(format!(
                                    "JS subset: no concrete ABI signature for selected {what}`{}`",
                                    ws.module(callee.module).functions[callee.func].name,
                                )));
                                continue;
                            }
                            None => skull_mir::lower::owner_abi_plan(ws, scx, concrete, selected),
                        }
                    }
                    None => skull_mir::lower::owner_abi_plan(ws, scx, concrete, &body),
                };
                let required = match abi {
                    Ok(abi) => required_arg_count(ws, &abi, arity),
                    Err(_) => {
                        findings.push(err(format!(
                            "JS subset: no concrete ABI plan for {what}`{}`",
                            ws.module(callee.module).functions[callee.func].name,
                        )));
                        continue;
                    }
                };
                let cf = &ws.module(callee.module).functions[callee.func];
                // A variadic surplus is passed past the declared params, as the
                // luajit emitter passes it; only the lower bound is an ABI fact.
                if argc < required || (argc > arity && !cf.is_variadic) {
                    findings.push(err(format!(
                        "JS subset: value-struct scalar ABI mismatch calling {what}`{}` \
                         ({argc} args vs {required}..={arity} params)",
                        cf.name
                    )));
                }
            }
        }
        if let Some(e) = merge_findings(findings) {
            out.push((mf.name.to_string(), e));
        }
    }
    out
}
