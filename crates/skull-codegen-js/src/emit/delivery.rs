use super::*;

/// The backend-local delivery scheduler: lowers retained declarations, follows post-inline residual
/// calls and emits the global/opaque bodies they need.
pub(super) struct JsDelivery<'a, 'b> {
    pub(super) ws: &'a Workspace,
    pub(super) scx: &'a skull_mir::lower::ScalarCtx,
    pub(super) st: &'a mut WorklistState<'b>,
}

impl<'a, 'b> JsDelivery<'a, 'b> {
    fn visit_fn(
        &mut self,
        fr: FuncRef,
        specialization: Option<Instance>,
    ) -> Result<(), CompilerError> {
        let f = &self.ws.module(fr.module).functions[fr.func];
        let name = match &specialization {
            Some(inst) => match self.st.names.get(inst) {
                Some(name) => name.clone(),
                None => {
                    self.st.reject(
                        &format!("instance:{fr:?}"),
                        err("JS: the artifact view has no symbol for a retained instance"),
                    );
                    return Ok(());
                }
            },
            None => self
                .st
                .names
                .name_for_decl(self.ws, fr)
                .expect("queued fn has a name")
                .clone(),
        };
        let selected = Instance {
            func: fr,
            args: specialization
                .as_ref()
                .map(|inst| inst.args.clone())
                .unwrap_or_default(),
        };
        let concrete = self.st.concrete;
        let prepared = (|| {
            let plan = concrete.get(&selected).ok_or_else(|| {
                err(match specialization {
                    Some(_) => "JS: no concrete plan for the selected retained body".to_string(),
                    None => format!("JS: no concrete plan for retained function `{}`", f.name),
                })
            })?;
            let mut mf = skull_mir::lower::lower_instance_with_plans(
                self.ws,
                self.scx,
                plan,
                self.st.concrete,
            )?;
            // Resolve bounded placeholders before any transform consumes the body.
            if specialization.is_some() {
                if f.has_iterator_bounds() && plan.bound_tree.is_empty() {
                    return Err(CompilerError::new_with_label(
                        f.name_span,
                        "JS: the concrete plan has no resolved bound tree for a bounded specialization".to_string(),
                    ));
                }
                skull_transform::bound::resolve_standalone_bound_calls(
                    self.ws,
                    self.scx,
                    &mut mf,
                    &plan.bound_tree,
                    self.st.concrete,
                )?;
            }
            self.prepare_fn(fr, &mut mf, &name)?;
            Ok((mf, plan.as_ref()))
        })();
        let (mut mf, plan) = match prepared {
            Ok(body) => body,
            Err(error) => {
                self.st.reject(&name, error);
                return Ok(());
            }
        };
        let mut findings = Vec::new();
        if specialization.is_none() {
            collect(
                &mut findings,
                skull_transform::validate_no_residual_bound_calls(self.ws, f, &mf),
            );
        }
        nobox_tree_errs(self.ws, &mf, &mut findings);
        if specialization.is_some() {
            mf.name = Ustr::from(name.as_str());
        }
        findings.extend(self.schedule_body(&mf)?);
        if let Some(error) = merge_findings(findings) {
            self.st.reject(&name, error);
        } else {
            self.st.lowered.push(DeliveredBody {
                specialization,
                plan,
                name,
                mf,
            });
        }
        Ok(())
    }

    fn lower_standalone(&mut self, inst: Instance) -> Result<(), CompilerError> {
        if !self.st.view.retains_rock_body(&inst) {
            self.st.reject(
                &format!("instance:{:?}", inst.func),
                err("JS: a residual specialization is absent from the artifact view"),
            );
            return Ok(());
        }
        let interner = self.scx.interner();
        let rejection = inst.too_deep_error(self.ws, interner).or_else(|| {
            self.st
                .graph
                .has_bound_tree_disagreement(&inst)
                .then(|| inst.bound_clash_error(self.ws, interner))
        });
        if let Some(error) = rejection {
            self.st.reject(&inst.symbol(self.ws, interner), error);
            return Ok(());
        }
        let Some(fr) = self.st.selected_decl(&inst) else {
            self.st.reject(
                &format!("instance:{:?}", inst.func),
                err("JS: a lowered specialization is absent from the artifact view"),
            );
            return Ok(());
        };
        self.visit_fn(fr, Some(inst))
    }

    fn prepare_fn(
        &mut self,
        fr: FuncRef,
        mf: &mut MirFunc,
        name: &str,
    ) -> Result<(), CompilerError> {
        if mf.is_coroutine {
            skull_transform::cleanup_with_nobox(self.ws, mf)
        } else {
            prepare_body(
                self.ws,
                self.scx,
                fr,
                mf,
                name,
                &mut self.st.cache,
                self.st.concrete,
            )
        }
    }

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
        nobox_tree_errs(self.ws, &mf, &mut findings);
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
        for instance in demands {
            if st.graph.has_bound_tree_disagreement(&instance) {
                findings.push(instance.bound_clash_error(ws, self.scx.interner()));
            } else {
                queue_standalone(st, ws, instance, &mut findings);
            }
        }
        collect(&mut findings, schedule_residual_calls(self.ws, mf, st));
        schedule_global_reads(self.ws, mf, st)?;
        Ok(findings)
    }
}

/// Emission order is canonicalized afterwards, so the walk order does not reach the output.
pub(super) fn drain_worklists(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    st: &mut WorklistState,
) -> Result<(), CompilerError> {
    let mut delivery = JsDelivery { ws, scx, st };
    let mut spec_cursor = 0;
    loop {
        while let Some(fr) = delivery.st.queue.pop() {
            delivery.visit_fn(fr, None)?;
        }
        delivery.st.queue_opaque_js_global_initializers(ws);
        while let Some(gname) = delivery.st.ginit_queue.pop() {
            delivery.visit_ginit(gname)?;
        }
        if !delivery.st.queue.is_empty() {
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

pub(super) fn emit_lowered_fn(
    ws: &Workspace,
    interner: &skull_mir::monoty::MonoInterner,
    mf: &MirFunc,
    js_name: &str,
    concrete: &skull_mir::concrete::ConcreteBodyPlan,
    names: &CalleeNames<'_>,
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
    let mut code_artifact = js_code_artifact(ws, interner, mf);
    code_artifact.set_body_names(mf, js_name);
    Ok((out, code_artifact))
}

/// Reject calls whose scalarized argument lanes a callee retaining a whole-value parameter would
/// drop; one mismatch per body.
pub(super) fn check_scalar_abi(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    st: &WorklistState,
    bodies: &[&MirFunc],
) -> Vec<(String, CompilerError)> {
    let concrete = st.concrete;
    let arities: HashMap<(&Instance, bool), usize> = st
        .lowered
        .iter()
        .map(|body| {
            (
                (body.instance(), body.specialization.is_some()),
                body.mf.params.len(),
            )
        })
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
                // One admission rule over the emitted body the call targets, so the shared and
                // specialized arms agree on the omitted-default window.
                let shared = Instance::base(*callee);
                let specialization = mf.spec_bindings.get(&iid);
                let body = specialization.unwrap_or(&shared);
                let Some(&arity) = arities.get(&(body, specialization.is_some())) else {
                    continue;
                };
                let what = if specialization.is_some() {
                    "specialized "
                } else {
                    ""
                };
                let selected = st.selected_decl(body).map(|func| Instance {
                    func,
                    args: body.args.clone(),
                });
                let abi = match selected.as_ref().filter(|inst| inst.func != body.func) {
                    Some(selected) => match concrete.get(body).or_else(|| concrete.get(selected)) {
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
                    },
                    None => skull_mir::lower::owner_abi_plan(ws, scx, concrete, body),
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
                // A variadic surplus is passed past the declared params; only the lower bound is an
                // ABI fact.
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
