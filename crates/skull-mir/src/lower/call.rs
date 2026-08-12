//! Call lowering: direct and indirect calls, the scalar-ABI arg paths, tuple
//! call trees, and the pack/leaf plumbing that decides whether a value crosses
//! a call boundary as leaves or as a box.
use super::*;

/// Everything a direct call to a statically-known callee carries besides its
/// operands: the bound-conformance tree, the concrete delivery instance, the
/// callee's concrete INSTANCE, and the ABI plan of the body the call TARGETS.
///
/// ONE bundle, built once by [`Builder::plan_direct_call`], because these four
/// facts are one decision and a path that computes a SUBSET is silently wrong
/// rather than merely incomplete. The UFCS path under `?.` computed exactly
/// that subset -- the instance, no tree, no key, and the DECLARATION's ABI plan
/// instead of the target's -- so `x?.m()` on a `value` receiver named an
/// instantiation it never delivered, and the callee was then reached at its
/// declaration with no body to serve it.
pub(crate) struct DirectCall {
    pub(crate) tree: Option<bone_semantic::prelude::BoundTree>,
    /// The concrete body delivered by this call, when it has one. An
    /// [`Instance`](crate::instance::Instance) is the delivery identity rather
    /// than a key plus a reconstructed substitution.
    pub(crate) delivery: Option<crate::instance::Instance>,
    pub(crate) callee_inst: Option<crate::instance::Instance>,
    pub(crate) plan: BackendFnAbiPlan,
}

impl Builder<'_> {
    fn record_concrete_ablation(&self, e: Idx<Expr>, func: FuncRef) {
        if !crate::ablation::enabled() {
            return;
        }
        let callee = &self.ws.module(func.module).functions[func.func];
        if callee.generics.is_empty() {
            return;
        }
        crate::ablation::note_examined();
        let Some(plan) = self.concrete_plan else { return };
        let site = crate::site::Site {
            domain: self.cur_domain.clone(),
            expr: e,
            op: self.cur_op.clone(),
        };
        let span = self.body.expr_spans.get(e).copied().unwrap_or(callee.name_span);
        let (category, detail) = if callee.inline_body.is_some() || callee.is_extern {
            (
                crate::ablation::FatalCategory::TargetBoundary,
                "the selected body is supplied by a target boundary rather than a Rock instance",
            )
        } else if let Some(reason) = plan.failed_sites.get(&site) {
            let category = if reason.contains("recursion") || reason.contains("depth") {
                crate::ablation::FatalCategory::PolymorphicGrowth
            } else if reason.contains("unconstrained") {
                crate::ablation::FatalCategory::MissingGraphEdge
            } else {
                crate::ablation::FatalCategory::CompilerInvariant
            };
            (
                category,
                "the concrete graph recorded a failed generic call site before MIR lowering",
            )
        } else {
            return;
        };
        crate::ablation::record(category, callee.name.as_str(), self.f.name.as_str(), span, detail);
    }

    fn selected_abi_instance(
        &self,
        inst: &crate::instance::Instance,
    ) -> Result<crate::instance::Instance, CompilerError> {
        match crate::lower::backend_variant_instance(self.ws, inst, self.scx.target) {
            Ok(selected) => Ok(selected),
            Err(_) if self.scx.target == bone_semantic::hir::FnBackend::Js => {
                // JS host mappings deliberately defer body selection to the JS
                // emitter when no Rock declaration serves this target.
                Ok(inst.clone())
            }
            Err(_) => Err(gap(format!(
                "callee `{}` has no body serving the {:?} target",
                self.ws.module(inst.func.module).functions[inst.func.func].name,
                self.scx.target,
            ))),
        }
    }

    pub(crate) fn concrete_callee_for_site(
        &self,
        e: Idx<Expr>,
    ) -> Result<Option<crate::instance::Instance>, CompilerError> {
        self.concrete_callee_for_site_op(e, self.cur_op.clone())
    }

    pub(crate) fn concrete_callee_for_site_op(
        &self,
        e: Idx<Expr>,
        op: crate::site::SiteOp,
    ) -> Result<Option<crate::instance::Instance>, CompilerError> {
        let Some(plan) = self.concrete_plan else { return Ok(None) };
        let site = crate::site::Site {
            domain: self.cur_domain.clone(),
            expr: e,
            op,
        };
        if let Some(reason) = plan.failed_sites.get(&site) {
            return Err(self.gap_here(format!(
                "concrete plan records an unresolved callee site: {reason}"
            )));
        }
        plan.callees.get(&site).cloned().map(Some).ok_or_else(|| {
            gap(format!(
                "concrete plan is missing callee identity for site {:?}",
                site.op
            ))
        })
    }

    pub(crate) fn concrete_delivery_instance(
        &self,
        inst: &crate::instance::Instance,
    ) -> Option<crate::instance::Instance> {
        if inst.args.is_empty() {
            return None;
        }
        self.concrete_plans
            .and_then(|plans| plans.contains_key(inst).then(|| inst.clone()))
    }

    pub(crate) fn concrete_callee_param_ty(
        &self,
        e: Idx<Expr>,
        inst: &crate::instance::Instance,
        pidx: usize,
    ) -> Result<Option<crate::monoty::MonoTy>, CompilerError> {
        let callee = &self.ws.module(inst.func.module).functions[inst.func.func];
        if let Some(plan) = self.concrete_plan {
            let site = crate::site::Site {
                domain: self.cur_domain.clone(),
                expr: e,
                op: self.cur_op.clone(),
            };
            if plan.callees.get(&site) != Some(inst) {
                return Err(self.gap_here("concrete callee identity disagrees with its site plan"));
            }
            let signature = if let Some(plans) = self.concrete_plans {
                let needs_plan = crate::lower::requires_concrete_body_plan(self.ws, inst)
                    && crate::monoty::too_deep_to_instantiate(
                        inst.arg_types(self.scx.interner()).iter(),
                    )
                    .is_none();
                if needs_plan {
                    &plans.get(inst).ok_or_else(|| {
                        self.gap_here("concrete callee body plan is missing for its site")
                    })?
                    .signature
                } else {
                    plan.callee_signatures.get(&site).ok_or_else(|| {
                        self.gap_here("concrete callee signature is missing for its site")
                    })?
                }
            } else {
                plan.callee_signatures.get(&site).ok_or_else(|| {
                    self.gap_here("concrete callee signature is missing for its site")
                })?
            };
            let slot = signature.params.get(pidx).or_else(|| {
                callee
                    .is_variadic
                    .then(|| signature.params.last())
                    .flatten()
            });
            let id = slot.ok_or_else(|| {
                self.gap_here("concrete callee signature has no parameter for this argument")
            })?;
            return Ok(Some(self.concrete_mono(*id)));
        }
        let Some(plans) = self.concrete_plans else { return Ok(None) };
        let Some(plan) = plans.get(inst) else { return Ok(None) };
        let slot = plan.signature.params.get(pidx).or_else(|| {
            callee
                .is_variadic
                .then(|| plan.signature.params.last())
                .flatten()
        });
        let id = slot.ok_or_else(|| {
            gap(format!(
                "concrete ABI plan has no parameter {pidx} for callee instance"
            ))
        })?;
        Ok(Some(self.concrete_mono(*id)))
    }

    pub(crate) fn concrete_coroutine_channel(
        &self,
        receiver: Idx<Expr>,
    ) -> Result<Option<crate::lower::YieldChannel>, CompilerError> {
        let Some(body_plan) = self.concrete_plan else { return Ok(None) };
        let site = crate::site::Site {
            domain: self.cur_domain.clone(),
            expr: receiver,
            op: crate::site::SiteOp::DirectCall,
        };
        let result = if let Some(owner) = body_plan.callees.get(&site) {
            let signature = match self.concrete_plans {
                Some(plans)
                    if crate::lower::requires_concrete_body_plan(self.ws, owner)
                        && crate::monoty::too_deep_to_instantiate(
                            owner.arg_types(self.scx.interner()).iter(),
                        )
                        .is_none() => &plans.get(owner).ok_or_else(|| {
                        self.gap_here("concrete coroutine callee body plan is missing")
                    })?
                    .signature,
                Some(_) | None => body_plan.callee_signatures.get(&site).ok_or_else(|| {
                    self.gap_here("concrete coroutine callee signature is missing")
                })?,
            };
            signature.result.map(|id| self.concrete_mono(id))
        } else {
            Some(self.concrete_expr_ty(receiver).ok_or_else(|| {
                self.gap_here("concrete coroutine receiver has no type")
            })?)
        };
        Ok(result.as_ref().and_then(|ty| CoroutinePlan::channel_for_concrete_type(self.ws, ty)))
    }

    pub(crate) fn callee_abi_plan_for_instance(
        &self,
        inst: &crate::instance::Instance,
    ) -> Result<BackendFnAbiPlan, CompilerError> {
        let selected = self.selected_abi_instance(inst)?;
        if let Some(plans) = self.concrete_plans {
            if !crate::lower::requires_concrete_body_plan(self.ws, inst) {
                return Ok(BackendFnAbiPlan::for_instance(self.ws, self.scx, &selected));
            }
            if crate::monoty::too_deep_to_instantiate(
                inst.arg_types(self.scx.interner()).iter(),
            )
            .is_some()
            {
                return Ok(BackendFnAbiPlan::for_instance(self.ws, self.scx, &selected));
            }
            let plan = plans.get(inst).ok_or_else(|| {
                let name = self.ws.module(inst.func.module).functions[inst.func.func].name;
                gap(format!(
                    "concrete ABI plan is missing for callee `{name}` ({})",
                    inst.func.func.into_raw().into_u32()
                ))
            })?;
            if selected.func != inst.func {
                return Ok(BackendFnAbiPlan::for_instance(self.ws, self.scx, &selected));
            }
            return Ok(BackendFnAbiPlan::for_concrete_plan(self.ws, self.scx, plan));
        }
        Ok(BackendFnAbiPlan::for_instance(self.ws, self.scx, &selected))
    }

    /// Lower a call to a (boxed) single value — the scalar-ABI Group result is
    /// `pack`ed to a cdata. ABI-boundary callers use [`Self::lower_call_repr`].
    pub(crate) fn lower_call(
        &mut self,
        e: Idx<Expr>,
        callee: Callee,
        args: IdxRange<Expr>,
    ) -> Result<Idx<Inst>, CompilerError> {
        let r = self.lower_call_repr(e, callee, args)?;
        Ok(self.pack(r))
    }

    /// Lower a call to a [`ValueRepr`]. A scalar-ABI value-returning direct call
    /// yields an unboxed `Group` (so a consuming ABI boundary splats it without a
    /// round-trip through cdata); everything else is a `Scalar`.
    pub(crate) fn lower_call_repr(
        &mut self,
        e: Idx<Expr>,
        callee: Callee,
        args: IdxRange<Expr>,
    ) -> Result<ValueRepr, CompilerError> {
        let arg_idxs: Vec<Idx<Expr>> = args.into_iter().collect();
        if let Callee::Unresolved { name, .. } = callee {
            if matches!(name.as_str(), "inspect" | "inspect_formatted") {
                if let Some(&arg) = arg_idxs.first() {
                    if let Some(elem) = self.vec_column_elem_of(arg)? {
                        if self.is_nobox_struct(elem.name) {
                            return Err(CompilerError::new_with_label(
                                self.body.expr_spans[e],
                                format!(
                                    "cannot inspect a column-stored nobox value `{}`: \
                                     inspection requires implicit element materialization",
                                    elem.name
                                ),
                            ));
                        }
                    }
                    // A tuple whose BOX widened: its `nobox` part rides the box as
                    // leaves, so the table holds one slot per LEAF, not one per
                    // element. Typed readers derive that from the declared tuple
                    // type; `inspect` walks the table at RUNTIME and would report a
                    // wider tuple. Interpolation lowers to `inspect`, so this one
                    // check covers `"{t}"` too.
                    //
                    // Keyed by callee NAME beside its column sibling, and NOT on
                    // the param being `any`: `any` is also how the stdlib spells
                    // pure STORAGE (`__vec_data_set`, `push`'s primitives), which
                    // never look inside the value and must keep working -- they are
                    // the whole point of the widening.
                    //
                    // Rejecting matters more here than for a nominal nobox: this
                    // shape leaves no `<name>_ct` behind, so the residue check
                    // cannot catch it and an unrejected one is a SILENT wrong
                    // answer. Escape: `(materialize(v), n)` stops that slot
                    // splatting, so the box is one-slot-per-element again.
                    if let Some(shape) = self.tuple_box_shape(arg)? {
                        return Err(CompilerError::new_with_label(
                            self.body.expr_spans[e],
                            format!(
                                "cannot inspect a tuple carrying a nobox value: its {} \
                                 leaves ride the box, so it would be read as a wider \
                                 tuple (project the slots, or `materialize` the \
                                 element deliberately)",
                                shape.slot_count()
                            ),
                        ));
                    }
                }
            }
        }
        // `cast<T>(v)` is identity at runtime (`return value`), so a `nobox`
        // RESULT threads the argument's repr instead of packing it for the boxed
        // `any` param and field-reading the leaves back off that box. This is
        // what lets a whole COLUMN element read widen to `T?`: every stdlib
        // entry point that widens one spells it `cast(__vec_data_get(..))`, and
        // `__vec_data_get` already lowers to a Group of column reads -- the box
        // was minted only to cross `cast`. Same identity treatment the newtype
        // ctor and the bound `iter` call below already get.
        // Keyed on the RESOLVED callee, never the spelling: a user fn named
        // `cast` returning a nobox value is a different function, and threading
        // its argument through would delete the call and emit a leaf read of
        // whatever was passed in (pinned by tests/nobox_cast_identity.rock).
        if let Callee::Unresolved { name, .. } = callee {
            if name.as_str() == "cast" && self.nobox_value_name(e).is_some() {
                if let [inner] = arg_idxs[..] {
                    if self.resolves_to_inline_builtin(e, name) {
                        return self.lower_expr_repr(inner);
                    }
                }
            }
        }
        // Scalar arithmetic: rock desugars `+ - * / %` to `__op_*` calls; turn
        // them back into native typed ops when both operands are scalars.
        // (Non-scalar `__op_*` falls through to a real overload Call below.)
        if let Callee::Unresolved { name, .. } = callee {
            let n = name.as_str();
            if matches!(
                n,
                "__op_add" | "__op_sub" | "__op_mul" | "__op_div" | "__op_idiv" | "__op_mod"
            ) {
                if let [a, b] = arg_idxs[..] {
                    if let (Some(lt), Some(rt)) = (self.arith_ty(a), self.arith_ty(b)) {
                        return Ok(ValueRepr::Scalar(self.lower_native_arith(n, lt, rt, a, b)?));
                    }
                }
            }
        }
        // `native_buffer_alloc<T>(n)` intrinsic: allocate a flat ffi array of the
        // concrete element type and wrap it in a `NativeBuffer`. Resolving the
        // ctype here (not in a generic inline(lua) body) is the whole point -- it
        // gives value-struct-typed NativeBuffers a boilerplate-free ctor. Non-value
        // / GC / opaque-generic elements reject spanned.
        if let Callee::Unresolved { name, .. } = callee {
            if name.as_str() == "native_buffer_alloc" {
                let [n_arg] = arg_idxs[..] else {
                    return Err(gap("`native_buffer_alloc` takes exactly one argument (count)"));
                };
                let span = self.body.expr_spans[e];
                let ctype = self.native_buffer_alloc_ctype(e, span)?;
                let count = self.lower_expr(n_arg)?;
                let ptr = self.emit_cur(Inst::FfiNewArray { ctype, count });
                let mut fields =
                    vec![(Ustr::from("ptr"), ptr), (Ustr::from("len"), count)];
                fields.extend(self.leaf_view_fields(ctype, ptr));
                let nb = self.emit_cur(Inst::MakeStruct {
                    ty: Some(Ustr::from("NativeBuffer")),
                    fields,
                });
                return Ok(ValueRepr::Scalar(nb));
            }
        }
        // `xs.push(v)` / `vec_push(xs, v)` onto a COLUMN-stored Vec. The stdlib
        // push bodies write one element into a single `_data` backing, which
        // column storage does not have -- so the call is replaced here, where
        // the receiver's element type is concrete. (Inside `Vec<T>.push` itself
        // the element is an opaque `T`, so the kind is undecidable there.)
        if let Callee::Unresolved { name, .. } = callee {
            if matches!(name.as_str(), "push" | "vec_push") {
                if let [recv, item] = arg_idxs[..] {
                    if let Some(elem) = self.vec_column_elem_of(recv)? {
                        return self.column_push(recv, item, elem);
                    }
                }
            }
            // The three raw-backing primitives ARE the representation boundary.
            // Nearly every stdlib Vec helper -- `get`, `set`, `pop`, `clear`,
            // `insert_at`, `remove_at`, `swap_remove` -- reaches storage only
            // through these, so making them column-aware here converts that whole
            // surface at once instead of intercepting each helper by name.
            //
            // This works inside a GENERIC body because `vec_column_elem_of`
            // resolves through the active `SpecSubst`: in the shared snapshot of
            // `Vec<T>.clear` the element is an opaque `T` and the boxed `_data`
            // body stands, while the snapshot specialized at `T = Vec2` sees the
            // concrete element and lowers to column operations. Static, per
            // instantiation -- never a runtime test for whether `_data` exists.
            if let Some(r) = self.column_data_primitive(name.as_str(), &arg_idxs)? {
                return Ok(r);
            }
            // `vec_new()` is the fourth boundary primitive, and the odd one out:
            // its element type comes from the CALL's own type, not a receiver, so
            // it cannot be read off an argument. It is also `inline(lua)`, so it
            // never specializes (`compute_call_spec` bails on an inline body) --
            // the opaque body would hand back a `_data` table that every later
            // column access then indexes as nil.
            if name.as_str() == "__vec_data_alloc" && arg_idxs.len() == 1 {
                if let Some(elem) = self.vec_column_elem_of(e)? {
                    let capacity = self.lower_expr(arg_idxs[0])?;
                    return Ok(ValueRepr::Scalar(self.empty_column_vec(elem, capacity)?));
                }
            }
            if name.as_str() == "vec_new" && arg_idxs.is_empty() {
                if let Some(elem) = self.vec_column_elem_of(e)? {
                    let capacity = self.emit_cur(Inst::Const(Lit::Int(0)));
                    return Ok(ValueRepr::Scalar(self.empty_column_vec(elem, capacity)?));
                }
            }
            // The Map surface. `get`/`table_unset` are the storage boundary the
            // generic Rock bodies reach through -- `remove`, `contains`,
            // `get_or_insert`, `update` and `upsert` are all built on them, so
            // converting these converts that surface at once. `len`/`clear` are
            // intercepted despite having working generic bodies: the generic
            // `clear` unsets while iterating the very table it walks, and `len`
            // counts a spine it can read in O(1).
            if let Some(r) = self.map_column_primitive(name.as_str(), &arg_idxs)? {
                return Ok(r);
            }
            // `map_new()` is the Map's `vec_new`: its value type comes from the
            // CALL's own type, so it cannot be read off an argument.
            if name.as_str() == "map_new" && arg_idxs.is_empty() {
                if let Some(elem) = self.map_column_value_of(e)? {
                    return Ok(ValueRepr::Scalar(self.empty_column_map(elem)?));
                }
            }
            if name.as_str() == "__rock_vec_from_lua_array" {
                if let ([raw, len], Some(elem)) = (&arg_idxs[..], self.vec_column_elem_of(e)?) {
                    let raw = self.lower_expr(*raw)?;
                    let len = self.lower_expr(*len)?;
                    return Ok(ValueRepr::Scalar(
                        self.column_vec_from_raw_array(raw, len, elem)?,
                    ));
                }
            }
        }
        // `on_killed(f)` / `on_resume(f)` inside a `co fn` register a lifecycle
        // hook. bone desugars `on_killed { body }` to a call of an `on_killed`
        // stub (which errors at runtime) with a `() => body` lambda. We capture
        // it as `Inst::CoroHook` so the coroutine transform relocates the lambda
        // to the constructor (a kill before the first resume must still fire).
        // Outside a co fn (`resume_arg` unset) it falls through to the stub.
        if self.resume_arg.is_some() {
            if let Callee::Unresolved { name, .. } = callee {
                let kind = match name.as_str() {
                    "on_killed" => Some(CoroHookKind::OnKilled),
                    "on_resume" => Some(CoroHookKind::OnResume),
                    "on_hot_reload" => Some(CoroHookKind::OnHotReload),
                    _ => None,
                };
                if let Some(kind) = kind {
                    if let [arg] = arg_idxs[..] {
                        // The hook is peeled to the constructor, which can only
                        // re-render a LAMBDA LITERAL with the frame context -- any
                        // other fn-typed expression (a named fn, a fn in a
                        // variable) would silently install no hook. Reject.
                        if !matches!(self.body.expr_arena[arg], Expr::Lambda { .. }) {
                            return Err(gap(format!(
                                "`{name}` argument must be a lambda literal (`() => ...`) -- the hook is relocated to construction time"
                            )));
                        }
                        let lambda = self.lower_expr(arg)?;
                        self.emit_cur(Inst::CoroHook { kind, lambda });
                        return Ok(ValueRepr::Scalar(self.emit_cur(Inst::Const(Lit::Nil))));
                    }
                }
            }
        }
        // A concrete `c.resume()` / `c.resume(arg)` whose yield type is a flat enum:
        // thread the value through the frame-slot channel instead of the type-erased
        // generic `resume` return (which always boxes). `c.__advance(c, arg)` steps
        // the coroutine (writing the leaves to reused `__y*` slots on the object,
        // which IS the frame), then we read them back as an unboxed group -- no
        // per-yield allocation.
        if let Callee::Unresolved { name, .. } = callee {
            if name.as_str() == "resume" && !arg_idxs.is_empty() {
                if self.concrete_plan.is_some() {
                    if let Some(channel) = self.concrete_coroutine_channel(arg_idxs[0])? {
                        let recv = self.lower_expr(arg_idxs[0])?;
                        let advance = self.emit_cur(Inst::Field { base: recv, field: Ustr::from("__advance") });
                        let mut cargs: Vec<Idx<Inst>> = vec![recv];
                        if let Some(&a) = arg_idxs.get(1) {
                            cargs.push(self.lower_expr(a)?);
                        }
                        self.emit_cur(Inst::CallValue { callee: advance, args: cargs });
                        return Ok(self.resume_frame_read_channel(recv, &channel));
                    }
                } else if self.is_coroutine_recv(arg_idxs[0]) {
                    if let Some(shape) = self.flat_enum_shape_of_expr(e) {
                        let recv = self.lower_expr(arg_idxs[0])?;
                        let advance = self.emit_cur(Inst::Field { base: recv, field: Ustr::from("__advance") });
                        let mut cargs: Vec<Idx<Inst>> = vec![recv];
                        if let Some(&a) = arg_idxs.get(1) {
                            cargs.push(self.lower_expr(a)?);
                        }
                        self.emit_cur(Inst::CallValue { callee: advance, args: cargs });
                        return Ok(self.resume_frame_read(recv, &shape));
                    }
                }
            }
        }
        // Bound-calls inside an `Iterator`-bounded body (`next(x)` / `iter(x)`
        // where `x: I`): `iter` is IDENTITY (the arg is the iterator, no call);
        // `next` is a placeholder [`Inst::CallBoundMulti`] -- the concrete
        // callee is substituted per splice from the site's `BoundTree`.
        if let Some(inf) = self.inf {
            if inf.bound_identity_calls.contains(&e) {
                let [arg] = arg_idxs[..] else {
                    return Err(gap("bound `iter` call with non-unary args"));
                };
                return Ok(ValueRepr::Scalar(self.lower_expr(arg)?));
            }
            if let Some(&gidx) = inf.bound_calls.get(&e) {
                let [arg] = arg_idxs[..] else {
                    return Err(gap("bound `next` call with non-unary args"));
                };
                let arg_v = self.lower_expr(arg)?;
                // Bound conformance fixes the SLOTS at exactly `(bool, E)`; `E`'s
                // shape is this body's specialization to answer, so a value
                // element arrives as leaves instead of one opaque slot. Leaving
                // it opaque forces the splice to rebuild an aggregate, which a
                // presence-carrying element cannot be rebuilt into.
                let shape = self.bound_next_tuple_shape(gidx)?;
                let dests: Vec<Idx<Cell>> =
                    (0..shape.slot_count()).map(|_| self.f.new_cell(None)).collect();
                let call = self.emit_cur(Inst::CallBoundMulti {
                    dests: dests.clone(),
                    generic_idx: gidx as u32,
                    args: vec![arg_v],
                });
                if shape.splats() {
                    self.f.bound_next_split.insert(call);
                }
                let loads: Vec<Idx<Inst>> =
                    dests.iter().map(|c| self.emit_cur(Inst::Load(*c))).collect();
                return Ok(self.tuple_result(&shape, &loads));
            }
        }
        // A spread arg fills param slots by field name, so it takes the
        // slot-reordering path just like a named arg.
        let any_named = arg_idxs
            .iter()
            .any(|a| self.body.arg_names.contains_key(a) || self.body.spread_args.contains(a));
        if let Callee::Unresolved { name, .. } = callee {
            if !any_named {
                if let Some(&recv_expr) = arg_idxs.first() {
                    if self.struct_field_is_fn(recv_expr, name) {
                        let arg_vals: Vec<Idx<Inst>> =
                            arg_idxs.iter().map(|&a| self.lower_expr(a)).collect::<Result<_, _>>()?;
                        let fieldv = self.emit_cur(Inst::Field { base: arg_vals[0], field: name });
                        let v = self.emit_call_value(e, fieldv, arg_vals[1..].to_vec())?;
                        return Ok(ValueRepr::Scalar(v));
                    }
                }
            }
        }
        match callee {
            // A transparent newtype constructor (`UserId(5)`) is identity at
            // runtime -- the wrapped value IS the inner -- so it lowers to the
            // single argument value, no call.
            Callee::Unresolved { name, .. } if self.ws.newtypes_by_name.contains_key(&name) => {
                match arg_idxs.as_slice() {
                    [inner] => Ok(ValueRepr::Scalar(self.lower_expr(*inner)?)),
                    _ => Err(gap(format!("newtype ctor `{name}` with non-unary args"))),
                }
            }
            // A statically-named function -> a `FuncRef` (mangling is codegen's
            // job, not baked into the IR). On resolution failure, recover an
            // INDIRECT call: a fn-typed struct field via UFCS (`recv.f(a)` ==
            // `f(recv, a)`, no real fn `f`) or a fn-typed global (`g(a)`).
            Callee::Unresolved { name, .. } => match self.resolve_funcref(e, name) {
                // A `@struct_expand` fn (docs/shaders.md) is replaced
                // by its literal pairs list -- recognized through the intrinsic
                // registry off the resolved FuncRef, so no user fn is hijacked by
                // name and the common non-intrinsic call pays only one field read.
                Ok(func) => match self.fn_expand_kind(func) {
                    Some(ExpandKind::StructPairs) => self.expand_struct_pairs(e, func, &arg_idxs),
                    Some(ExpandKind::Materialize) => self.expand_materialize(&arg_idxs),
                    Some(ExpandKind::ShaderPipeline) => {
                        self.expand_shader_pipeline(e, &arg_idxs)
                    }
                    Some(ExpandKind::ComputeArtifact) => {
                        self.expand_compute_artifact(e, &arg_idxs)
                    }
                    Some(ExpandKind::VertexLayout) => {
                        self.expand_vertex_layout(e, func, &arg_idxs)
                    }
                    Some(ExpandKind::UniformBlock) => {
                        self.expand_uniform_block(e, func, &arg_idxs)
                    }
                    Some(ExpandKind::UniformValues) => {
                        self.expand_uniform_values(e, func, &arg_idxs)
                    }
                    None => self.lower_direct_call_repr(e, func, &arg_idxs, any_named),
                },
                Err(gap_err) => {
                    if !any_named {
                        let arg_vals: Vec<Idx<Inst>> =
                            arg_idxs.iter().map(|&a| self.lower_expr(a)).collect::<Result<_, _>>()?;
                        // `g(args)` where `g` is a fn-typed module global.
                        if self.is_fn_global(name) {
                            let g = self.emit_cur(Inst::Global(name));
                            let v = self.emit_call_value(e, g, arg_vals)?;
                            return Ok(ValueRepr::Scalar(v));
                        }
                    }
                    Err(gap_err)
                }
            },
            // A fn-typed local: load the cell and call through the value.
            Callee::Local(b) => {
                // A nested fn: call the hoisted top-level fn by name (no closure).
                // Under its scalar ABI (`nested_fn_abi`) value args splat to leaves
                // and a value return arrives as leaves via `CallValueMulti`.
                if let Some(abi) = self.nested_fn_globals.get(&raw(b)).cloned() {
                    self.f.hoisted_fn_globals.push(abi.hoisted);
                    let g = self.emit_cur(Inst::Global(abi.hoisted));
                    let arg_vals =
                        self.slotted_call_args(&abi.param_names, &abi.param_shapes, &arg_idxs)?;
                    let Some(shape) = abi.ret_shape else {
                        let v = self.emit_call_value(e, g, arg_vals)?;
                        return Ok(ValueRepr::Scalar(v));
                    };
                    let dests: Vec<Idx<Cell>> =
                        (0..shape.leaf_count()).map(|_| self.f.new_cell(None)).collect();
                    self.emit_cur(Inst::CallValueMulti { dests: dests.clone(), callee: g, args: arg_vals });
                    let leaves: Vec<Idx<Inst>> =
                        dests.iter().map(|c| self.emit_cur(Inst::Load(*c))).collect();
                    let mut idx = 0;
                    return Ok(self.rebuild_from_leaves(&shape, &leaves, &mut idx));
                }
                let cell = *self
                    .binds
                    .get(&raw(b))
                    .ok_or_else(|| gap("call to an unbound fn-typed local"))?;
                let callee_v = self.emit_cur(Inst::Load(cell));
                if let Some(abi) = self.fn_value_rep_plan(self.binding_ty(b).ok_or_else(|| {
                    gap("fn-typed local has no physical function signature")
                })?)? {
                    return self.indirect_call_by_plan(abi, callee_v, &arg_idxs);
                }
                // Nothing `nobox` in the signature -- args are boxed.
                let arg_vals: Vec<Idx<Inst>> =
                    arg_idxs.iter().map(|&a| self.lower_expr(a)).collect::<Result<_, _>>()?;
                let v = self.emit_call_value(e, callee_v, arg_vals)?;
                Ok(ValueRepr::Scalar(v))
            }
            // A fn-typed expression result: lower it, call through it.
            Callee::Value(ve) => {
                let abi = self.fn_value_rep_plan(*self.inf
                    .and_then(|inf| inf.expr_types.get(ve))
                    .ok_or_else(|| gap("fn-valued expression has no physical function signature"))?)?;
                let callee_v = self.lower_expr(ve)?;
                if let Some(abi) = abi {
                    return self.indirect_call_by_plan(abi, callee_v, &arg_idxs);
                }
                let arg_vals: Vec<Idx<Inst>> =
                    arg_idxs.iter().map(|&a| self.lower_expr(a)).collect::<Result<_, _>>()?;
                let v = self.emit_call_value(e, callee_v, arg_vals)?;
                Ok(ValueRepr::Scalar(v))
            }
        }
    }

    /// Lower a direct (statically-resolved) call. When the callee uses the scalar
    /// value ABI, value args splat to leaves and a value return arrives as N
    /// leaves via [`Inst::CallMulti`] (rebuilt into a `Group`); otherwise it is a
    /// plain boxed [`Inst::Call`].
    ///
    /// A call of an `Iterator`-bounded callee carries bone's resolution tree
    /// (keyed by the call expr `e`); it is re-recorded against the emitted call
    /// inst so the inliner can substitute the placeholders inside the callee.
    /// The specialization decision for a call `e` to `func`: `Some((key, subst))`
    /// iff `func` is a splice-able generic `@inline`/`@pure` callee whose per-site
    /// bindings (`call_generic_bindings[e]`) include at least one VALUE-struct
    /// leaf. A primitive-only / non-leaf instantiation, or a non-inline generic,
    /// keeps the shared generic snapshot (byte-identical). Eligibility is a subset
    /// of the inliner's `classify` (not extern / lua / coroutine) so a specialized
    /// splice always resolves; a residual case falls to the site-aware rejection.
    /// Build the [`DirectCall`] bundle for a call at `e` to `func`.
    ///
    /// The delivery decision comes first because the ABI plan depends on it: a
    /// served instantiation targets the specialized definition, whose parameter
    /// list is what the caller's argument splat has to match.
    pub(crate) fn plan_direct_call(
        &mut self,
        e: Idx<Expr>,
        func: FuncRef,
    ) -> Result<DirectCall, CompilerError> {
        let tree = self
            .inf
            .and_then(|i| i.bound_call_trees.get(&e))
            .filter(|t| !t.is_empty())
            .cloned();
        // IDENTITY, computed beside the delivery decision and independent of it.
        // A call the policy declines to specialize still names exactly one
        // concrete instance, and this is where it says which.
        let callee_inst = if self.concrete_plan.is_some() {
            self.record_concrete_ablation(e, func);
            self.concrete_callee_for_site(e)?
        } else {
            self.callee_instance_for(e, func)?
        };
        let delivery = if self.concrete_plan.is_some() {
            callee_inst.as_ref().and_then(|inst| self.concrete_delivery_instance(inst))
        } else {
            self.call_spec_for_target(e, func)?
        };
        let plan = if let Some(owner) = &callee_inst {
            self.callee_abi_plan_for_instance(owner)?
        } else {
            self.callee_abi_plan(func, delivery.as_ref())
        };
        Ok(DirectCall { tree, delivery, callee_inst, plan })
    }

    /// Attach the bundle's identity records to the emitted call inst.
    ///
    /// Takes the whole [`DirectCall`] rather than the three records loosely,
    /// because the loose form is what let a caller pass `None, None` for the
    /// two it had not computed and still typecheck.
    pub(crate) fn record_call(
        &mut self,
        call: Idx<Inst>,
        func: FuncRef,
        dc: &DirectCall,
    ) -> Result<(), CompilerError> {
        if let Some(t) = &dc.tree {
            self.f.bound_trees.insert(call, t.clone());
        }
        if let Some(inst) = &dc.delivery {
            self.f.spec_bindings.insert(call, inst.clone());
        }
        if let Some(owner) = &dc.callee_inst {
            debug_assert_eq!(
                crate::lower::canonical_variant(self.ws, owner.func),
                crate::lower::canonical_variant(self.ws, func)
            );
            self.record_callee_bound_tree(
                call,
                owner,
                dc.tree.as_ref().is_some_and(|tree| !tree.is_empty()),
            )?;
            self.f.callee_instances.insert(call, owner.clone());
        }
        self.f.call_abi_plans.insert(call, dc.plan.clone());
        Ok(())
    }

    pub(crate) fn record_callee_bound_tree(
        &mut self,
        call: Idx<Inst>,
        owner: &crate::instance::Instance,
        raw_tree_present: bool,
    ) -> Result<(), CompilerError> {
        let Some(plans) = self.concrete_plans else { return Ok(()) };
        let Some(plan) = plans.get(owner) else {
            if raw_tree_present {
                return Err(self.gap_here("concrete callee bound plan is missing for its site"));
            }
            return Ok(());
        };
        if raw_tree_present && plan.bound_tree.is_empty() {
            return Err(self.gap_here("concrete callee bound plan is empty for its site"));
        }
        if !plan.bound_tree.is_empty() {
            self.f.resolved_bound_trees.insert(call, plan.bound_tree.clone());
        }
        Ok(())
    }

    /// The concrete instance a call at `e` to `func` names -- the IDENTITY
    /// record, computed beside the delivery decision and independent of it.
    ///
    /// The ONE lowering-side entry to
    /// [`crate::lower::shape::callee_instance_subst`]. Every emission shape
    /// asks here and hands the answer to [`Self::record_call`]; a per-shape
    /// call to the oracle is exactly where one shape forgot it --
    /// `scalar_ufcs_call_repr` never recorded anything, so `x?.get()` on a
    /// scalar-ABI receiver had a graph instance and no claim.
    pub(crate) fn callee_instance_for(
        &self,
        e: Idx<Expr>,
        func: FuncRef,
    ) -> Result<Option<crate::instance::Instance>, CompilerError> {
        if self.ws.module(func.module).functions[func.func].generics.is_empty() {
            return Ok(Some(crate::instance::Instance::base(func)));
        }
        match self.inf {
            Some(inf) => {
                let Some(subst) = crate::lower::shape::callee_instance_subst(
                    self.ws,
                    inf,
                    e,
                    func,
                    self.subst(),
                )? else {
                    return Ok(None);
                };
                let key = crate::lower::spec_key_from_subst(self.ws, func, &subst).ok_or_else(|| {
                    CompilerError::new_with_label(
                        self.body.expr_spans[e],
                        format!(
                            "internal: concrete call to `{}` has an incomplete owner",
                            self.ws.module(func.module).functions[func.func].name
                        ),
                    )
                })?;
                Ok(Some(crate::instance::Instance::intern(
                    self.scx.interner(),
                    func,
                    key,
                )))
            }
            None => Ok(None),
        }
    }

    pub(crate) fn call_spec(
        &self,
        e: Idx<Expr>,
        func: FuncRef,
    ) -> Result<Option<crate::instance::Instance>, CompilerError> {
        let Some(inf) = self.inf else { return Ok(None) };
        // INTERNED HERE, at the one boundary where the build's arena is in
        // scope: `compute_call_spec` is arena-free shape analysis, and this is
        // where its answer stops being a list of types and becomes an identity.
        let spec = compute_call_spec(self.ws, inf, e, func, self.subst())?;
        // Stage-0e census. Recorded HERE rather than inside `compute_call_spec`
        // because the classification needs no context and the record needs the
        // caller's name and the call's span, which only the Builder has.
        if crate::ablation::enabled() {
            if !self.ws.module(func.module).functions[func.func].generics.is_empty() {
                crate::ablation::note_examined();
            }
        }
        // Polymorphic growth: a SUCCESSFUL specialization whose instantiation
        // type is deep. Recorded on the `Some` side, because growth shows up as
        // ever-deeper keys that each specialize fine, not as a decline.
        //
        // It also CAPS -- past the threshold `spec` becomes `None`, the call
        // takes the shared body, and the recursion terminates. Without the cap
        // the mode does not survive the shape it exists to measure: `depth<T>`
        // wrapping to `[T]` specializes one level deeper per recursive call and
        // lowering recurses until the stack overflows (measured: SIGABRT).
        // Plan S 7 predicts exactly this -- polymorphic recursion "does not
        // terminate under demand-driven instantiation" -- and the Stage-0d
        // shadow graph's `MAX_INSTANTIATION_DEPTH` caps for the same reason:
        // the cap turns divergence into a REPORT.
        let mut spec = spec;
        if crate::ablation::enabled() {
            if let Some((key, _)) = &spec {
                if let Some(d) = key
                    .iter()
                    .map(crate::lower::shape::growth_depth)
                    .max()
                    .filter(|d| *d > crate::lower::shape::ABLATION_GROWTH_DEPTH)
                {
                    let callee = &self.ws.module(func.module).functions[func.func];
                    crate::ablation::record(
                        crate::ablation::FatalCategory::PolymorphicGrowth,
                        callee.name.as_str(),
                        self.f.name.as_str(),
                        self.body.expr_spans.get(e).copied().unwrap_or(callee.name_span),
                        format!(
                            "instantiation type is {d} levels deep (threshold {}): the \
                             signature of polymorphic recursion, which does not terminate \
                             under demand-driven instantiation (plan S 7). Capped here so \
                             the mode terminates; uncapped this overflows the stack",
                            crate::lower::shape::ABLATION_GROWTH_DEPTH
                        ),
                    );
                    spec = None;
                }
            }
        }
        if crate::ablation::enabled() && spec.is_none() {
            if let Some((category, detail)) =
                crate::lower::shape::ablation_decline(self.ws, inf, e, func, self.subst())
            {
                let callee = &self.ws.module(func.module).functions[func.func];
                crate::ablation::record(
                    category,
                    callee.name.as_str(),
                    self.f.name.as_str(),
                    self.body.expr_spans.get(e).copied().unwrap_or(callee.name_span),
                    detail,
                );
            }
        }
        Ok(spec.map(|(key, _)| crate::instance::Instance::intern(self.scx.interner(), func, key)))
    }

    /// [`Self::call_spec`] narrowed by the target's storage filter -- the form
    /// every consumer of a call's INSTANTIATION must use, so none of them
    /// disagrees about whether this call specializes.
    pub(crate) fn call_spec_for_target(
        &self,
        e: Idx<Expr>,
        func: FuncRef,
    ) -> Result<Option<crate::instance::Instance>, CompilerError> {
        self.call_spec(e, func)
    }

    /// **The ABI plan of the body this call TARGETS.**
    ///
    /// The callee's emitted body is named by the delivery decision that reached
    /// this call: a served instantiation targets the specialized definition,
    /// and everything else targets the shared one. That is the same id the
    /// definition is lowered against, which is what keeps the caller's argument
    /// splat and the callee's parameter list from being two derivations.
    pub(crate) fn callee_abi_plan(
        &self,
        func: FuncRef,
        delivery: Option<&crate::instance::Instance>,
    ) -> BackendFnAbiPlan {
        let base;
        let inst = match delivery {
            Some(inst) => inst,
            None => {
                base = crate::instance::Instance::base(func);
                &base
            }
        };
        BackendFnAbiPlan::for_instance(self.ws, self.scx, inst)
    }

    /// THE tuple-return shape of a call, read off the CALLEE's declared return
    /// type under the CALLEE's instantiation.
    ///
    /// One oracle, because there were two and they disagreed. `lower_call`
    /// computed this under the call's own `SpecSubst` while the presence
    /// consumers (`nullable_tuple_call_shape`) computed it under the ENCLOSING
    /// body's -- empty in a non-generic caller, so the callee's generic stayed
    /// unbound, every slot came back `Opaque`, and `tuple_ret_shape`'s
    /// `nullable && !splats` guard answered `None`. The callee then faithfully
    /// returned its `false` presence slot and the call site threw it away:
    /// `!= null` fell through to packing a `MakeTuple`, which is structurally
    /// never nil, so it const-folded to `true` and `if let` emitted no branch at
    /// all. A shape disagreement here cannot fail loudly -- it just branches on
    /// a slot that is not there, or fails to.
    pub(crate) fn call_ret_tuple_shape(
        &self,
        e: Idx<Expr>,
        func: FuncRef,
    ) -> Result<Option<TupleRetShape>, CompilerError> {
        let owner = if self.concrete_plan.is_some() {
            self.concrete_callee_for_site(e)?
        } else {
            self.callee_instance_for(e, func)?
        };
        let delivery = if self.concrete_plan.is_some() {
            owner.as_ref().and_then(|inst| self.concrete_delivery_instance(inst))
        } else {
            self.call_spec_for_target(e, func)?
        };
        let abi = if let Some(owner) = owner {
            self.callee_abi_plan_for_instance(&owner)?
        } else {
            self.callee_abi_plan(func, delivery.as_ref())
        };
        Ok(abi.ret_tuple())
    }

    /// [`Self::call_spec`] for the for-protocol `next` callee, whose bindings
    /// come from bone's `next_generics` rather than a call expr. Ordered into
    /// the callee's generic declaration order, which is what [`Vec<MonoTy>`] is.
    pub(crate) fn next_call_spec(
        &self,
        func: FuncRef,
        next_generics: &rustc_hash::FxHashMap<bone_semantic::hir::VarId, TyId>,
        span: bone_semantic::prelude::Span,
    ) -> Result<Option<Vec<crate::monoty::MonoTy>>, CompilerError> {
        let Some(inf) = self.inf else { return Ok(None) };
        let key = spec_key_from_next_generics(self.ws, inf, func, next_generics, self.subst())?;
        let callee = &self.ws.module(func.module).functions[func.func];
        if crate::ablation::enabled() && !callee.generics.is_empty() {
            crate::ablation::note_examined();
            if key.is_none() {
                if let Some((category, detail)) = crate::lower::shape::ablation_next_decline(
                    self.ws,
                    inf,
                    func,
                    next_generics,
                    self.subst(),
                ) {
                    crate::ablation::record(
                        category,
                        callee.name.as_str(),
                        self.f.name.as_str(),
                        span,
                        detail,
                    );
                }
            }
        }
        // A key that names NO instantiation is not a delivery record, and the
        // producer now says so rather than handing back an empty vector for a
        // non-generic callee. This filter was the second half of that decision
        // and is kept as a FLOOR: filing an empty key as a `spec_bindings` entry
        // says "this call is served by a specialized snapshot" about a callee
        // that has none.
        //
        // It is not cosmetic. `reach::call_targets` SKIPS any site carrying a
        // `spec_bindings` entry, because `spec_bound_calls` owns that edge -- so
        // an empty-key entry removes the callee from ordinary discovery and
        // hands it to a demand path that resolves it right back to the shared
        // body. Lua survived that round trip; JS did not, and reported `no
        // specialized body emitted for the `next` instantiation` about
        // `CharsIter.next`, which is not generic at all.
        Ok(key.filter(|k| !k.is_empty()))
    }

    /// Lower a direct (statically-resolved) call. When the callee uses the scalar
    /// value ABI, value args splat to leaves and a value return arrives as N
    /// leaves via [`Inst::CallMulti`] (rebuilt into a `Group`); otherwise it is a
    /// plain boxed [`Inst::Call`].
    ///
    /// A call of an `Iterator`-bounded callee carries bone's resolution tree
    /// (keyed by the call expr `e`); it is re-recorded against the emitted call
    /// inst so the inliner can substitute the placeholders inside the callee.
    pub(crate) fn lower_direct_call_repr(
        &mut self,
        e: Idx<Expr>,
        func: FuncRef,
        arg_idxs: &[Idx<Expr>],
        any_named: bool,
    ) -> Result<ValueRepr, CompilerError> {
        // A value-instantiated `@inline` generic call takes the specialized scalar
        // ABI: value args splat, the return arrives as leaves, and the emitted call
        // is tagged so the inliner splices the matching specialized snapshot. Empty
        // `subst` = the shared (unspecialized) path, byte-identical. In an unspliced
        // context the splice never fires, so the splatted call would reach the
        // SHARED generic body and drop leaves -- keep the shared boxed ABI there.
        // In an unspliced context an ABI-only specialization is dropped: no splice
        // will fire, and the splatted call would reach the SHARED generic body and
        // drop every leaf but the first, so the boxed ABI is kept to match it.
        //
        // A column-eligible instantiation is NOT dropped, because the shared body
        // is wrong rather than merely unspecialized, and because a splice is no
        // longer the only thing that can serve it: the call targets a standalone
        // specialized copy, lowered under this same substitution, so the two agree
        // on the ABI without an inliner ever running.
        //
        // The instantiation key rides the emitted call inst whenever this is a
        // specialized call -- even on the boxed (non-scalar-ABI) path, because the
        // specialized body still differs (e.g. a `native_buffer_alloc<T>` ctype
        // resolves) though the param/return ABI is unchanged.
        let dc = self.plan_direct_call(e, func)?;
        let callee = &self.ws.module(func.module).functions[func.func];
        // Resolve phantom before tuple flattening can project from its nil placeholder.
        if arg_idxs.is_empty()
            && callee.inline_body.is_some_and(|(_, body)| body.trim() == "return nil")
        {
            return Ok(ValueRepr::Scalar(self.emit_cur(Inst::Const(Lit::Nil))));
        }
        let declared_ret_tuple = dc.plan.ret_tuple();
        if let Some(shape) = &declared_ret_tuple {
            self.reject_nobox_opaque_tuple_return(e, func, shape)?;
        } else {
            self.reject_nobox_opaque_value_return(e, func)?;
        }
        let lua_opaque = self.scx.target() == bone_semantic::hir::FnBackend::Lua
            && callee.inline_body.is_some()
            && !callee.generics.is_empty()
            && !callee.annotations.iter().any(|a| a.name.name.as_str() == "scalar_abi");
        let ret_tuple = (!lua_opaque).then_some(declared_ret_tuple).flatten();
        if !dc.plan.uses_scalar_abi() {
            self.reject_nobox_boxed_call_args(func, arg_idxs)?;
            self.reject_column_container_opaque_slots(
                e,
                func,
                self.concrete_plan.and_then(|_| dc.callee_inst.as_ref()),
            )?;
            let args = if any_named {
                self.reorder_named_args(e, arg_idxs, func, dc.callee_inst.as_ref())?
            } else {
                arg_idxs
                    .iter()
                    .enumerate()
                    .map(|(i, &a)| {
                        let v = self.lower_expr(a)?;
                        if self.concrete_plan.is_some() {
                            let inst = dc
                                .callee_inst
                                .as_ref()
                                .ok_or_else(|| self.gap_here("concrete call has no callee instance"))?;
                            let t = self
                                .concrete_callee_param_ty(e, inst, i)?
                                .ok_or_else(|| self.gap_here("concrete call has no callee parameter type"))?;
                            return Ok(self.coerced_mono(v, a, &t));
                        }
                        Ok(match self.param_ty(func, i) {
                            Some(t) => self.coerced(v, a, t),
                            None => v,
                        })
                    })
                    .collect::<Result<_, CompilerError>>()?
            };
            // Above the tuple early return: the exemption is a property of the
            // CALLEE, so making it arity-dependent would be a guarantee hole the
            // day a `@gpu_only` intrinsic returns a tuple.
            self.authorize_gpu_boundary_boxes(&func, &args);
            if let Some(shape) = &ret_tuple {
                return Ok(self.emit_tuple_call_tree(func, args, shape, &dc)?);
            }
            let call = self.emit_cur(Inst::Call { func, args });
            self.record_call(call, func, &dc)?;
            return Ok(ValueRepr::Scalar(call));
        }
        let param_shapes = dc.plan.param_shapes();
        let args = self.scalar_call_args(
            func,
            arg_idxs,
            any_named,
            &param_shapes,
        )?;
        let ret_shape = dc.plan.ret_shape();
        match ret_shape {
            Some(shape) => {
                let n = shape.leaf_count();
                let dests: Vec<Idx<Cell>> = (0..n).map(|_| self.f.new_cell(None)).collect();
                let call = self.emit_cur(Inst::CallMulti { dests: dests.clone(), func, args });
                self.record_call(call, func, &dc)?;
                let leaves: Vec<Idx<Inst>> =
                    dests.iter().map(|c| self.emit_cur(Inst::Load(*c))).collect();
                let mut idx = 0;
                Ok(self.rebuild_from_leaves(&shape, &leaves, &mut idx))
            }
            None => match &ret_tuple {
                Some(shape) => Ok(self.emit_tuple_call_tree(func, args, shape, &dc)?),
                None => {
                    let call = self.emit_cur(Inst::Call { func, args });
                    self.record_call(call, func, &dc)?;
                    Ok(ValueRepr::Scalar(call))
                }
            },
        }
    }

    /// A direct call to a tuple-returning fn under the multi-return ABI: the
    /// callee returns its slots, captured into that many dest cells via
    /// [`Inst::CallMulti`]. The result value is `MakeTuple(loads)` -- a boxed use
    /// materializes the table (same as the old ABI), a destructure elides it
    /// (`bind_for_pattern`). When the shape splats, the result is instead a
    /// `Tuple` repr whose splatting elements are Groups rebuilt from their
    /// leaves, so a `nobox` element never becomes a box on the way to a bind.
    ///
    /// `dc` carries the `Iterator`-bound resolution recorded against the emitted
    /// call inst (chain roots like a directly-called bounded `next`).
    ///
    /// A tuple return does not exempt the call from its instantiation key:
    /// under column storage the shared generic body reads a `_data` the
    /// instantiation does not have, so the specialized copy is the only correct
    /// target.
    pub(crate) fn emit_tuple_call_tree(
        &mut self,
        func: FuncRef,
        args: Vec<Idx<Inst>>,
        shape: &TupleRetShape,
        dc: &DirectCall,
    ) -> Result<ValueRepr, CompilerError> {
        let dests: Vec<Idx<Cell>> =
            (0..shape.slot_count()).map(|_| self.f.new_cell(None)).collect();
        let call = self.emit_cur(Inst::CallMulti { dests: dests.clone(), func, args });
        self.record_call(call, func, dc)?;
        let loads: Vec<Idx<Inst>> = dests.iter().map(|c| self.emit_cur(Inst::Load(*c))).collect();
        Ok(self.tuple_result(shape, &loads))
    }

    /// The call-site result for a captured tuple return: the plain
    /// `MakeTuple(slots)` when nothing splats, else the `Tuple` repr rebuilt
    /// from the slots.
    pub(crate) fn tuple_result(&mut self, shape: &TupleRetShape, slots: &[Idx<Inst>]) -> ValueRepr {
        if !shape.splats() {
            return ValueRepr::Scalar(self.emit_cur(Inst::MakeTuple(slots.to_vec())));
        }
        let mut idx = 0;
        self.tuple_from_slots(shape, slots, &mut idx)
    }

    /// An indirect call whose fn TYPE carries `nobox` values: value args splat to
    /// their leaves and a value return arrives as leaves through
    /// `CallValueMulti`, matching what the lambda/nested fn on the other end
    /// emitted from the SAME type. The callee is opaque here by construction --
    /// that is the whole point of keying on the type ([`Self::fn_value_abi`]).
    pub(crate) fn indirect_call_by_plan(
        &mut self,
        plan: FunctionRepPlan,
        callee: Idx<Inst>,
        arg_idxs: &[Idx<Expr>],
    ) -> Result<ValueRepr, CompilerError> {
        let arena = self.scx.rep_arena();
        let mut argv = Vec::new();
        for (i, &a) in arg_idxs.iter().enumerate() {
            let slot = plan
                .params()
                .get(i)
                .map(|id| arena.ret_slot(*id))
                .unwrap_or(RetSlot::Opaque);
            argv.extend(self.arg_values_for_slot(&slot, a)?);
        }
        let Some(result) = plan.result() else {
            return Ok(ValueRepr::Scalar(self.emit_cur(Inst::CallValue { callee, args: argv })));
        };
        if let Some(shape) = arena.tuple_shape(result) {
            let dests: Vec<Idx<Cell>> =
                (0..shape.slot_count()).map(|_| self.f.new_cell(None)).collect();
            self.emit_cur(Inst::CallValueMulti { dests: dests.clone(), callee, args: argv });
            let loads: Vec<Idx<Inst>> =
                dests.iter().map(|c| self.emit_cur(Inst::Load(*c))).collect();
            return Ok(self.tuple_result(&shape, &loads));
        }
        let Some(shape) = arena.value_shape(result) else {
            return Ok(ValueRepr::Scalar(self.emit_cur(Inst::CallValue { callee, args: argv })));
        };
        let dests: Vec<Idx<Cell>> =
            (0..shape.leaf_count()).map(|_| self.f.new_cell(None)).collect();
        self.emit_cur(Inst::CallValueMulti { dests: dests.clone(), callee, args: argv });
        let leaves: Vec<Idx<Inst>> =
            dests.iter().map(|c| self.emit_cur(Inst::Load(*c))).collect();
        let mut idx = 0;
        Ok(self.rebuild_from_leaves(&shape, &leaves, &mut idx))
    }

    /// An indirect call `callee(args)`. If the call's result type is a tuple, the
    /// fn-value (a lambda or named fn used as a value) returns N values under the
    /// multi-return ABI, captured via [`Inst::CallValueMulti`] -- a multi-assign
    /// `d0, d1 = callee(args)` (a plain `CallValue` in table position would
    /// truncate to the first return). The result is `MakeTuple(loads)`: boxed use
    /// materializes the table, a destructure elides it. The ABI is decided by the
    /// call's STATIC type, so direct and indirect callees of the same fn-type
    /// agree with no callee-identity analysis.
    pub(crate) fn emit_call_value(
        &mut self,
        e: Idx<Expr>,
        callee: Idx<Inst>,
        args: Vec<Idx<Inst>>,
    ) -> Result<Idx<Inst>, CompilerError> {
        Ok(self.emit_call_value_n(self.expr_tuple_shape(e)?, callee, args))
    }

    /// [`Self::emit_call_value`] with an explicit tuple shape (the safe-nav call
    /// paths pass the nil-peeled shape, since their expr type is `(..)?`).
    pub(crate) fn emit_call_value_n(
        &mut self,
        shape: Option<TupleRetShape>,
        callee: Idx<Inst>,
        args: Vec<Idx<Inst>>,
    ) -> Idx<Inst> {
        match shape {
            Some(shape) => {
                let dests: Vec<Idx<Cell>> =
                    (0..shape.slot_count()).map(|_| self.f.new_cell(None)).collect();
                self.emit_cur(Inst::CallValueMulti { dests: dests.clone(), callee, args });
                let loads: Vec<Idx<Inst>> =
                    dests.iter().map(|c| self.emit_cur(Inst::Load(*c))).collect();
                let r = self.tuple_result(&shape, &loads);
                self.pack(r)
            }
            None => self.emit_cur(Inst::CallValue { callee, args }),
        }
    }

    /// A UFCS call `method(recv, args)` where `recv` is an already-lowered value
    /// -- the safe-nav receiver. The receiver fills param 0 (splat to its leaves
    /// when that param is a value), args fill the rest.
    ///
    /// THE `?.` UFCS entry, scalar and boxed ABI alike, and it consumes the same
    /// [`DirectCall`] bundle [`Self::lower_direct_call_repr`] does. Splitting the
    /// two ABIs across two call sites is how the boxed one came to record a key
    /// the scalar one did not, and how both came to read the DECLARATION's ABI
    /// plan rather than the target's.
    ///
    /// A value callee hands back its leaves, so a safe-nav join that stores PER
    /// SLOT never has to take a box apart again.
    pub(crate) fn ufcs_call_repr(
        &mut self,
        e: Idx<Expr>,
        func: FuncRef,
        recv: Idx<Inst>,
        arg_idxs: &[Idx<Expr>],
    ) -> Result<ValueRepr, CompilerError> {
        let dc = self.plan_direct_call(e, func)?;
        if !dc.plan.uses_scalar_abi() {
            let mut argv = vec![recv];
            for &a in arg_idxs {
                argv.push(self.lower_expr(a)?);
            }
            // A tuple-returning method splats N values, handed on UNPACKED: the
            // per-slot join stores them slot-wise, so packing here would box a
            // `nobox` element the wire just kept unboxed. A method whose own
            // return is `(..)?` carries its presence tag in the repr, and the
            // join forwards that tag rather than minting `true` -- `a?.m()` is
            // absent when either link is.
            return Ok(match dc.plan.ret_tuple() {
                Some(shape) => self.emit_tuple_call_tree(func, argv, &shape, &dc)?,
                None => {
                    let call = self.emit_cur(Inst::Call { func, args: argv });
                    self.record_call(call, func, &dc)?;
                    ValueRepr::Scalar(call)
                }
            });
        };
        let param_shapes = dc.plan.param_shapes();
        let mut argv = Vec::new();
        // The RECEIVER arrives already lowered, so it cannot go through
        // `arg_values_for_slot`; a tuple receiver has no UFCS spelling, so
        // `Leaves` is the only splatting slot reachable here.
        match param_shapes.first() {
            Some(RetSlot::Leaves(shape)) => {
                let leaves = self.cdata_leaves(recv, &shape.clone());
                argv.extend(leaves);
            }
            _ => argv.push(recv),
        }
        for (i, &a) in arg_idxs.iter().enumerate() {
            let slot = param_shapes.get(i + 1).cloned().unwrap_or(RetSlot::Opaque);
            argv.extend(self.arg_values_for_slot(&slot, a)?);
        }
        match dc.plan.ret_shape() {
            Some(shape) => {
                let n = shape.leaf_count();
                let dests: Vec<Idx<Cell>> = (0..n).map(|_| self.f.new_cell(None)).collect();
                let call =
                    self.emit_cur(Inst::CallMulti { dests: dests.clone(), func, args: argv });
                self.record_call(call, func, &dc)?;
                let leaves: Vec<Idx<Inst>> =
                    dests.iter().map(|c| self.emit_cur(Inst::Load(*c))).collect();
                let mut idx = 0;
                Ok(self.rebuild_from_leaves(&shape, &leaves, &mut idx))
            }
            None => {
                let call = self.emit_cur(Inst::Call { func, args: argv });
                self.record_call(call, func, &dc)?;
                Ok(ValueRepr::Scalar(call))
            }
        }
    }

    /// Build the flat scalar-ABI argument list for a scalar-ABI callee: a value
    /// arg splats to its leaves, a non-value arg is boxed. Handles named-arg
    /// reordering (an omitted value param emits N nils so the callee default-fill
    /// fires; an omitted scalar param emits one nil).
    pub(crate) fn scalar_call_args(
        &mut self,
        func: FuncRef,
        arg_idxs: &[Idx<Expr>],
        any_named: bool,
        param_shapes: &[RetSlot],
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        if !any_named {
            let mut out = Vec::new();
            for (i, &a) in arg_idxs.iter().enumerate() {
                let slot = param_shapes.get(i).cloned().unwrap_or(RetSlot::Opaque);
                out.extend(self.arg_values_for_slot(&slot, a)?);
            }
            return Ok(out);
        }
        let param_names: Vec<Ustr> = self.ws.module(func.module).functions[func.func]
            .param_decls
            .iter()
            .map(|pd| pd.name)
            .collect();
        self.slotted_call_args(&param_names, param_shapes, arg_idxs)
    }

    /// Build a call's argument list by placing each argument into its PARAM SLOT
    /// (source order) and then splatting/nil-filling per slot: a named arg lands
    /// in the slot of that name, a struct spread `..op` by field name, everything
    /// else positionally. This is the one slot-placement rule, shared by every
    /// calling convention that has param names -- a convention with its own
    /// positional-only arg loop silently miscompiles `f(b: 2, a: 1)`.
    /// `param_shapes` shorter than `param_names` (or all-`Opaque`) is the boxed ABI.
    pub(crate) fn slotted_call_args(
        &mut self,
        param_names: &[Ustr],
        param_shapes: &[RetSlot],
        arg_idxs: &[Idx<Expr>],
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        let mut slots: Vec<Option<ValueRepr>> = vec![None; param_names.len()];
        let mut next_pos = 0usize;
        for &a in arg_idxs {
            // Struct spread `..op`: decompose into per-field reprs (a struct-
            // literal operand is elided) and place each into its param slot.
            if self.body.spread_args.contains(&a) {
                for (fname, r) in self.spread_field_reprs(a)? {
                    let pidx = param_names.iter().position(|p| *p == fname).ok_or_else(|| {
                        gap(format!("spread field `{fname}` matches no parameter"))
                    })?;
                    slots[pidx] = Some(r);
                }
                continue;
            }
            let r = self.lower_expr_repr(a)?;
            let pidx = match self.body.arg_names.get(&a) {
                Some(nm) => param_names
                    .iter()
                    .position(|p| p == nm)
                    .ok_or_else(|| gap(format!("named argument `{nm}` matches no parameter")))?,
                None => {
                    let i = next_pos;
                    next_pos += 1;
                    i
                }
            };
            if pidx >= slots.len() {
                return Err(gap(format!(
                    "argument lands in slot {pidx} of {} parameters",
                    slots.len()
                )));
            }
            slots[pidx] = Some(r);
        }
        let hi = slots.iter().rposition(|s| s.is_some()).map_or(0, |i| i + 1);
        let mut out = Vec::new();
        for i in 0..hi {
            let slot = param_shapes.get(i).cloned().unwrap_or(RetSlot::Opaque);
            match slots[i].clone() {
                Some(r) => out.extend(self.repr_to_slot_values(r, &slot)),
                // An omitted param arrives as one nil PER WIRE VALUE, so the
                // callee's default-fill sees a nil in every leaf it binds.
                None => {
                    for _ in 0..slot.slot_count() {
                        out.push(self.emit_cur(Inst::Const(Lit::Nil)));
                    }
                }
            }
        }
        Ok(out)
    }

    // --- ValueRepr <-> leaves <-> cdata (the scalar-ABI conversions) ---

    /// Materialize a `ValueRepr` as a single value — the one place a `Group`
    /// becomes a real `<name>_ct(...)` cdata (an escape: stored/compared/boxed).
    pub(crate) fn pack(&mut self, r: ValueRepr) -> Idx<Inst> {
        match r {
            ValueRepr::Scalar(v) => v,
            // A splatting tuple's box. Its `nobox` elements pack too, which is
            // exactly how a whole-tuple use reaches the guarantee's rejection.
            ValueRepr::Tuple { parts, .. } => {
                let vs = parts.into_iter().map(|p| self.pack(p)).collect();
                self.emit_cur(Inst::MakeTuple(vs))
            }
            // A flat-enum Group `[tag, _0.., <rec>..]` packs to the `{__variant}`
            // box (`MakeEnumDyn`), the enum escape -- not a `<name>_ct` cdata.
            ValueRepr::Group { name, parts, .. } if flat_enum_shape(self.ws, name).is_some() => {
                let shape = flat_enum_shape(self.ws, name).unwrap();
                let packed: Vec<Idx<Inst>> = parts.into_iter().map(|p| self.pack(p)).collect();
                let tag = packed[0];
                let positional = packed[1..1 + shape.tuple_width()].to_vec();
                let named: Vec<(Ustr, Idx<Inst>)> = shape
                    .record_fields
                    .iter()
                    .enumerate()
                    .map(|(i, rf)| (*rf, packed[1 + shape.tuple_width() + i]))
                    .collect();
                // A STATIC tag names a present variant: box directly. A payload-free
                // variant reuses its interned singleton (map-key / identity holds).
                if let Inst::Const(Lit::Str(variant)) = self.f.insts[tag] {
                    if self.variant_is_unit(name, variant) {
                        let g = Ustr::from(format!("__rock_enum_{name}_{variant}").as_str());
                        return self.emit_cur(Inst::Global(g));
                    }
                    return self.emit_cur(Inst::MakeEnumDyn { tag, ty: name, positional, named });
                }
                // A DYNAMIC tag may be `nil` (a null nullable enum): materialize to
                // `nil` in that case, else the box -- else a null threads to a
                // `{__variant = nil}` table that fails `== null`.
                let result = self.f.new_cell(None);
                let nil = self.emit_cur(Inst::Const(Lit::Nil));
                let is_nil = self.emit_cur(Inst::Eq(tag, nil));
                let nil_bb = self.f.new_block();
                let box_bb = self.f.new_block();
                let join = self.f.new_block();
                self.f.set_term(self.cur, Term::Branch { cond: is_nil, then_bb: nil_bb, else_bb: box_bb });
                self.cur = nil_bb;
                let z = self.emit_cur(Inst::Const(Lit::Nil));
                self.emit_cur(Inst::Store { cell: result, value: z });
                self.f.set_term(self.cur, Term::Goto(join));
                self.cur = box_bb;
                let boxed = self.emit_cur(Inst::MakeEnumDyn { tag, ty: name, positional, named });
                self.emit_cur(Inst::Store { cell: result, value: boxed });
                self.f.set_term(self.cur, Term::Goto(join));
                self.cur = join;
                self.emit_cur(Inst::Load(result))
            }
            // A leaf-table struct's physical keys ARE its leaves, so it packs
            // through the flat `<Name>_lct` ctor. Packing it field-wise would
            // box the nullable `nobox` child under its declared name -- the
            // guarantee's violation, and the only reason this struct is
            // GC-tier at all.
            ValueRepr::Group { name, .. } if value_struct_is_leaf_table(self.ws, name) => {
                let shape = value_shape(self.ws, name).expect("a leaf-table struct has a shape");
                let leaves = self.repr_to_leaves(&r, &shape);
                let fields = value_leaf_paths(&shape).into_iter().zip(leaves).collect();
                self.emit_cur(Inst::MakeValueStruct { name, fields })
            }
            // `present` is deliberately NOT consulted here. Packing an absent
            // Group to `nil` is what a `== nil` consumer needs, but `pack` cannot
            // see its consumer, and the consumers that DO need it already branch
            // on presence themselves (`??`, safe-nav). Branching here instead
            // puts the ctor behind a cell, which stops SROA forwarding it, so the
            // box survives to the residue check -- measured as
            // `nobox_coalesce_at_ctor_field` regressing CORRECT -> LOUD REJECT,
            // exactly the win `docs/parked/inline-lua.md` recorded this costing.
            // Narrowing to a static presence does not help: at a `??` the
            // presence is dynamic and the coalesce consumes it anyway.
            ValueRepr::Group { name, parts, present: _ } => {
                self.pack_group_fields(name, parts)
            }
        }
    }

    /// [`Self::pack`] for a consumer that can observe NIL-ness of the result --
    /// a boxed slot an absent value has to reach as `nil`, not as a statically
    /// non-nil ctor.
    ///
    /// `pack` itself deliberately ignores `present` (see its Group arm): it
    /// cannot see its consumer, and branching for every consumer puts the ctor
    /// behind a cell, which stops SROA forwarding it and costs
    /// `nobox_coalesce_at_ctor_field`. The branch is legal HERE because the slot
    /// is already an escape -- the ctor survives either way, so honouring
    /// presence buys the correct `nil` for free. Same split safe-nav uses.
    pub(crate) fn pack_maybe_absent(&mut self, r: ValueRepr) -> Idx<Inst> {
        let ValueRepr::Group { name, parts, present: Some(present) } = r else {
            return self.pack(r);
        };
        let result = self.f.new_cell(None);
        let absent_bb = self.f.new_block();
        let present_bb = self.f.new_block();
        let join = self.f.new_block();
        self.f.set_term(
            self.cur,
            Term::Branch { cond: present, then_bb: present_bb, else_bb: absent_bb },
        );

        self.cur = absent_bb;
        let nil = self.emit_cur(Inst::Const(Lit::Nil));
        self.emit_cur(Inst::Store { cell: result, value: nil });
        self.f.set_term(self.cur, Term::Goto(join));

        self.cur = present_bb;
        let boxed = self.pack_group_fields(name, parts);
        self.emit_cur(Inst::Store { cell: result, value: boxed });
        self.f.set_term(self.cur, Term::Goto(join));

        self.cur = join;
        self.emit_cur(Inst::Load(result))
    }

    /// The `<name>_ct(..)` cdata for a Group's parts, with no presence test.
    ///
    /// The parts are in declared field order (the positional ctor ABI requires
    /// it), so they line up with the declared names. The names let MIR passes
    /// forward a `Field` read to its ctor operand (SROA), eliminating the cdata
    /// when it never escapes. A payload-free enum field is just an `int` value
    /// (Prim), packed like any primitive.
    fn pack_group_fields(&mut self, name: Ustr, parts: Vec<ValueRepr>) -> Idx<Inst> {
        let names = struct_field_names(self.ws, name);
        let fields = parts
            .into_iter()
            .enumerate()
            .map(|(i, p)| {
                // A Group's parts are the nominal's declared fields, so the two
                // always line up. Naming a part after its STRUCT instead would
                // emit a ctor field the type does not have, and duplicate it for
                // every part -- fabricated identity, not a usable fallback.
                let fname = *names.get(i).expect("a Group's parts are its nominal's declared fields");
                (fname, self.pack(p))
            })
            .collect();
        self.emit_cur(Inst::MakeValueStruct { name, fields })
    }

    /// Flatten a `ValueRepr` to its primitive leaf values per `shape` (the wire
    /// ABI). A boxed `Scalar` (e.g. a global cdata / if-join) is field-read.
    /// Arguments for an `iter`/`pairs` WITNESS call -- the `for` protocol's and
    /// the implicit iterable->iterator coercion's. The collection obeys the same
    /// scalar ABI as any other direct-call parameter, so a `value`-struct
    /// collection (an `Arena<T>` handle) splats to its leaves; handing the
    /// materialized aggregate over instead gives a multi-parameter body one
    /// argument and nils the rest.
    ///
    /// No scalar ABI means the boxed convention, which takes the collection
    /// whole -- that is `Opaque`. A scalar ABI with an EMPTY parameter list is a
    /// different thing entirely: the witness would have no collection parameter
    /// to receive anything, so treating it as `Opaque` would silently pass an
    /// argument nothing binds. Reject instead.
    /// Takes the witness INSTANCE, not its declaration. The call this shapes
    /// arguments for targets the specialized definition, so asking the
    /// declaration would splat against one ABI and be received by another --
    /// the same mismatch the safe-navigation UFCS path had. Unification rather
    /// than a measured fix: `iter<T>`'s collection parameter is an array, which
    /// is `Opaque` at every instantiation, so the two answers agree today.
    pub(crate) fn iter_witness_args(
        &mut self,
        inst: &crate::instance::Instance,
        repr: ValueRepr,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        let iter_ref = inst.func;
        let plan = self.callee_abi_plan_for_instance(inst)?;
        let slot = if !plan.uses_scalar_abi() {
            RetSlot::Opaque
        } else {
            plan.param_shapes().first().cloned().ok_or_else(|| {
                gap(format!(
                    "`for`-protocol iter witness `{}` has a scalar ABI with no parameters, \
                     so it cannot receive the collection",
                    self.ws.module(iter_ref.module).functions[iter_ref.func].name
                ))
            })?
        };
        Ok(self.repr_to_slot_values(repr, &slot))
    }

    pub(crate) fn repr_to_leaves(&mut self, r: &ValueRepr, shape: &ValueShape) -> Vec<Idx<Inst>> {
        match r {
            // A tuple at a value-shaped sink is a boxed source: pack and
            // field-read, the `Scalar` treatment.
            ValueRepr::Tuple { .. } => {
                let v = self.pack(r.clone());
                self.repr_to_leaves(&ValueRepr::Scalar(v), shape)
            }
            ValueRepr::Group {
                parts, present, ..
            } => {
                let mut out = Vec::new();
                if shape.nullable {
                    out.push(match present {
                        Some(value) => *value,
                        None => self.emit_cur(Inst::Const(Lit::Bool(true))),
                    });
                }
                for ((_, fs), part) in shape.fields.iter().zip(parts) {
                    match fs {
                        FieldShape::Prim => out.push(self.pack(part.clone())),
                        FieldShape::Nested(sub) => out.extend(self.repr_to_leaves(part, sub)),
                    }
                }
                out
            }
            ValueRepr::Scalar(v) if shape.nullable => {
                let field_count = shape.leaf_count() - 1;
                if matches!(self.f.insts[*v], Inst::Const(Lit::Nil)) {
                    let mut out = Vec::with_capacity(1 + field_count);
                    out.push(self.emit_cur(Inst::Const(Lit::Bool(false))));
                    for _ in 0..field_count {
                        out.push(self.emit_cur(Inst::Const(Lit::Nil)));
                    }
                    return out;
                }

                let nil = self.emit_cur(Inst::Const(Lit::Nil));
                let present = self.emit_cur(Inst::NotEq(*v, nil));
                let cells: Vec<Idx<Cell>> =
                    (0..field_count).map(|_| self.f.new_cell(None)).collect();
                let nil_bb = self.f.new_block();
                let value_bb = self.f.new_block();
                let join = self.f.new_block();
                self.f.set_term(
                    self.cur,
                    Term::Branch {
                        cond: present,
                        then_bb: value_bb,
                        else_bb: nil_bb,
                    },
                );

                self.cur = nil_bb;
                for cell in &cells {
                    let value = self.emit_cur(Inst::Const(Lit::Nil));
                    self.emit_cur(Inst::Store {
                        cell: *cell,
                        value,
                    });
                }
                self.f.set_term(self.cur, Term::Goto(join));

                self.cur = value_bb;
                let mut inner = shape.clone();
                inner.nullable = false;
                let leaves = self.cdata_leaves(*v, &inner);
                for (cell, value) in cells.iter().zip(leaves) {
                    self.emit_cur(Inst::Store {
                        cell: *cell,
                        value,
                    });
                }
                self.f.set_term(self.cur, Term::Goto(join));

                self.cur = join;
                let mut out = Vec::with_capacity(1 + field_count);
                out.push(present);
                out.extend(cells.iter().map(|cell| self.emit_cur(Inst::Load(*cell))));
                out
            }
            ValueRepr::Scalar(v) => self.cdata_leaves(*v, shape),
        }
    }

    /// Flatten a boxed value to its primitive leaves via chained reads (the inverse
    /// of `pack`, used when a value arrives boxed). A flat ENUM box reads its box
    /// keys (`__variant`, positional `[i+1]`, `.recfield`) in shape order; a cdata
    /// value reads its `<name>_ct` fields.
    pub(crate) fn cdata_leaves(&mut self, v: Idx<Inst>, shape: &ValueShape) -> Vec<Idx<Inst>> {
        if let Some(es) = flat_enum_shape(self.ws, shape.name) {
            // NIL-SAFE spread: the box may be `nil` (a null nullable enum), and
            // `nil.__variant`/`nil[1]` would fault. Branch on nil -> all-nil leaves
            // (tag = nil, the null sentinel); else spread the box's keys.
            let n = es.leaf_count();
            let cells: Vec<Idx<Cell>> = (0..n).map(|_| self.f.new_cell(None)).collect();
            let nil = self.emit_cur(Inst::Const(Lit::Nil));
            let is_nil = self.emit_cur(Inst::Eq(v, nil));
            let nil_bb = self.f.new_block();
            let val_bb = self.f.new_block();
            let join = self.f.new_block();
            self.f.set_term(self.cur, Term::Branch { cond: is_nil, then_bb: nil_bb, else_bb: val_bb });
            self.cur = nil_bb;
            for c in &cells {
                let z = self.emit_cur(Inst::Const(Lit::Nil));
                self.emit_cur(Inst::Store { cell: *c, value: z });
            }
            self.f.set_term(self.cur, Term::Goto(join));
            self.cur = val_bb;
            let fc = self.spread_box_to_cells(v, &es);
            let leaves: Vec<Idx<Inst>> = std::iter::once(fc.tag)
                .chain(fc.positional)
                .chain(fc.named.into_iter().map(|(_, c)| c))
                .collect();
            for (c, lv) in cells.iter().zip(leaves) {
                self.emit_cur(Inst::Store { cell: *c, value: lv });
            }
            self.f.set_term(self.cur, Term::Goto(join));
            self.cur = join;
            return cells.iter().map(|c| self.emit_cur(Inst::Load(*c))).collect();
        }
        // A leaf-table box stores one key per leaf, so the split IS the key
        // reads -- the exact inverse of the `<Name>_lct` ctor, including the
        // presence key at a nullable child's own name.
        if value_struct_is_leaf_table(self.ws, shape.name) && !shape.nullable {
            return value_leaf_paths(shape)
                .into_iter()
                .map(|path| self.emit_cur(Inst::Field { base: v, field: path }))
                .collect();
        }
        let mut out = Vec::new();
        for (fname, fs) in &shape.fields {
            let fv = self.emit_cur(Inst::Field { base: v, field: *fname });
            match fs {
                FieldShape::Prim => out.push(fv),
                // A NULLABLE nested child: the stored box may be absent, so split
                // it presence-aware rather than field-reading straight off a
                // possible nil. Mirrors `repr_to_leaves`' `Scalar(v) if
                // shape.nullable` arm exactly -- presence is `v ~= nil` as leaf 0,
                // then the child's own leaves, nils on the absent arm -- because
                // the two must agree leaf-for-leaf or `rebuild_from_leaves`
                // indexes past the end. This function had NO nullable handling:
                // its one caller stripped `.nullable` first, which was safe only
                // while no shape carried it below the outermost level.
                FieldShape::Nested(sub) if sub.nullable => {
                    let field_count = sub.leaf_count() - 1;
                    let nil = self.emit_cur(Inst::Const(Lit::Nil));
                    let present = self.emit_cur(Inst::NotEq(fv, nil));
                    let cells: Vec<Idx<Cell>> =
                        (0..field_count).map(|_| self.f.new_cell(None)).collect();
                    let nil_bb = self.f.new_block();
                    let value_bb = self.f.new_block();
                    let join = self.f.new_block();
                    self.f.set_term(
                        self.cur,
                        Term::Branch { cond: present, then_bb: value_bb, else_bb: nil_bb },
                    );
                    self.cur = nil_bb;
                    for c in &cells {
                        let z = self.emit_cur(Inst::Const(Lit::Nil));
                        self.emit_cur(Inst::Store { cell: *c, value: z });
                    }
                    self.f.set_term(self.cur, Term::Goto(join));
                    self.cur = value_bb;
                    let mut inner = sub.clone();
                    inner.nullable = false;
                    let leaves = self.cdata_leaves(fv, &inner);
                    for (c, lv) in cells.iter().zip(leaves) {
                        self.emit_cur(Inst::Store { cell: *c, value: lv });
                    }
                    self.f.set_term(self.cur, Term::Goto(join));
                    self.cur = join;
                    out.push(present);
                    out.extend(
                        cells.iter().map(|c| self.emit_cur(Inst::Load(*c))).collect::<Vec<_>>(),
                    );
                }
                FieldShape::Nested(sub) => out.extend(self.cdata_leaves(fv, sub)),
            }
        }
        out
    }

    /// A whole-value read of value struct `name` out of a BOXED place (struct
    /// field, global, container element): split the box into leaves and rebuild a
    /// fresh `Group`, so `pack` materializes a COPY instead of aliasing the stored
    /// box. This is the same split-and-repack that already makes fn round-trips
    /// copy (params/returns ride the scalar ABI).
    ///
    /// Nested value fields recurse, so the snapshot survives a later place write
    /// through the owner -- a shallow expansion would snapshot the nested box's
    /// POINTER. A GC leaf is one slot, so its pointee stays shared by design. A
    /// leaf projection off the `Group` still reads its own slot at its own read
    /// point, and the unread leaves die in DCE.
    pub(crate) fn boxed_value_copy_repr(&mut self, boxed: Idx<Inst>, name: Ustr) -> ValueRepr {
        let Some(shape) = value_shape(self.ws, name).or_else(|| value_shape_gc(self.ws, name))
        else {
            return ValueRepr::Scalar(boxed);
        };
        let leaves = self.cdata_leaves(boxed, &shape);
        let mut idx = 0;
        self.rebuild_from_leaves(&shape, &leaves, &mut idx)
    }

    /// The `V?` twin of [`Self::boxed_value_copy_repr`]: an absent value has no
    /// leaves to split, so the copy runs on the PRESENT arm only and `nil` threads
    /// through untouched. The result is a single value (a fresh box or `nil`), not
    /// a `Group` -- a Group carries no presence for `pack`, which would materialize
    /// a box for an absent value and break `== null`.
    pub(crate) fn nullable_boxed_value_copy(&mut self, boxed: Idx<Inst>, name: Ustr) -> Idx<Inst> {
        let nil = self.emit_cur(Inst::Const(Lit::Nil));
        let is_nil = self.emit_cur(Inst::Eq(boxed, nil));
        let result = self.f.new_cell(None);
        let nil_bb = self.f.new_block();
        let val_bb = self.f.new_block();
        let join = self.f.new_block();
        self.f.set_term(self.cur, Term::Branch { cond: is_nil, then_bb: nil_bb, else_bb: val_bb });

        self.cur = nil_bb;
        let n = self.emit_cur(Inst::Const(Lit::Nil));
        self.store_and_join(result, n, join);

        self.cur = val_bb;
        let copy = self.boxed_value_copy_repr(boxed, name);
        let packed = self.pack(copy);
        self.store_and_join(result, packed, join);

        self.cur = join;
        self.emit_cur(Inst::Load(result))
    }

    /// One element of an indexable collection, for the loop lowerings that bind a
    /// whole element per iteration (`for x in coll`, `for (i, x) in enumerate`).
    ///
    /// Column storage has no element slot to index, so the read decomposes into K
    /// leaf reads and packs -- the same shape `let v = xs[i]` takes, which means
    /// SROA peels the pack wherever the binding is only projected from. Emitting a
    /// whole-element `Index` here instead is what made bitgun stop compiling: it
    /// reaches the emitter with no slot to name.
    pub(crate) fn loop_element_repr_at(
        &mut self,
        arr: Idx<Inst>,
        index: Idx<Inst>,
        kind: ContainerKind,
    ) -> Result<ValueRepr, CompilerError> {
        match kind {
            ContainerKind::VecValueColumns(elem) => self.column_element_repr(arr, index, elem),
            ContainerKind::NativeBufferValue(elem) if self.is_leaf_addressed(elem)? => {
                self.nb_leaf_element_repr(arr, index, elem)
            }
            _ => Ok(ValueRepr::Scalar(self.emit_cur(Inst::Index {
                base: arr,
                index,
                kind,
            }))),
        }
    }

    pub(crate) fn loop_element_at(
        &mut self,
        arr: Idx<Inst>,
        index: Idx<Inst>,
        kind: ContainerKind,
    ) -> Result<Idx<Inst>, CompilerError> {
        let repr = self.loop_element_repr_at(arr, index, kind)?;
        Ok(self.pack(repr))
    }

    /// Reconstruct a nested `Group` from a flat leaf list per `shape` (the inverse
    /// of `repr_to_leaves`), consuming leaves depth-first from `*idx`.
    pub(crate) fn rebuild_from_leaves(
        &mut self,
        shape: &ValueShape,
        leaves: &[Idx<Inst>],
        idx: &mut usize,
    ) -> ValueRepr {
        // `shape` is borrowed from a clone at the call sites, so collect the field
        // shapes first to avoid holding the borrow across `&mut self` calls.
        let fields: Vec<FieldShape> = shape.fields.iter().map(|(_, fs)| fs.clone()).collect();
        let name = shape.name;
        let present = shape.nullable.then(|| {
            let value = leaves[*idx];
            *idx += 1;
            value
        });
        let mut parts = Vec::with_capacity(fields.len());
        for fs in &fields {
            let part = match fs {
                FieldShape::Prim => {
                    let v = leaves[*idx];
                    *idx += 1;
                    ValueRepr::Scalar(v)
                }
                FieldShape::Nested(sub) => self.rebuild_from_leaves(sub, leaves, idx),
            };
            parts.push(part);
        }
        ValueRepr::Group {
            name,
            parts,
            present,
        }
    }

    /// Index of `field` in value struct `name`'s declared field order.
    pub(crate) fn field_index(&self, name: Ustr, field: Ustr) -> Option<usize> {
        self.struct_field_names(name)?.iter().position(|f| *f == field)
    }
}
