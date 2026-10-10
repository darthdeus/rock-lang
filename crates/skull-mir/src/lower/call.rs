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
    pub(crate) callee_inst: crate::instance::Instance,
    pub(crate) plan: BackendFnAbiPlanHandle,
}

impl Builder<'_> {
    fn lower_nullable_field_get(
        &mut self,
        e: Idx<Expr>,
        name: Ustr,
        args: &[Idx<Expr>],
    ) -> Result<Option<ValueRepr>, CompilerError> {
        let func = self.resolve_funcref(e, name)?;
        let module = self.ws.module(func.module);
        if !bone_semantic::prelude::is_stdlib_module_file(module.file.as_str())
            || module.functions[func.func].inline_body.is_none()
        {
            return Ok(None);
        }
        let inst = self.concrete_callee_for_site(e)?
            .ok_or_else(|| self.gap_here("field_get has no concrete callee"))?;
        let Some(ret) = self.concrete_callee_return_ty(e, &inst)? else { return Ok(None) };
        let (payload, nullable) = ret.peel_nullable();
        let Some(child) = payload.nominal() else { return Ok(None) };
        if !nullable || !self.is_nobox_struct(child) {
            return Ok(None);
        }
        let owner = self.concrete_callee_param_ty(e, &inst, 0)?
            .ok_or_else(|| self.gap_here("field_get has no concrete owner type"))?;
        let Some(parent) = owner.nominal() else { return Ok(None) };
        let fields = self.struct_field_names_required(parent)?;
        let mut flattened = false;
        for field in fields {
            if self.struct_field_plan(parent, field, false)?.embed() == Some((child, true)) {
                flattened = true;
                break;
            }
        }
        if !flattened {
            return Ok(None);
        }
        let id = self.concrete_rep_id(&ret, RepUse::Return, false, false)?;
        let shape = self.scx.rep_arena().value_shape(id)
            .ok_or_else(|| self.gap_here("flattened field_get result has no leaf shape"))?;
        let args = self.reorder_named_args(e, args, func, Some(&inst))?;
        let [owner, selector] = args.as_slice() else {
            return Err(self.gap_here("field_get requires an owner and selector"));
        };
        // The field's own key is presence; its dotted keys hold the payload.
        let mut leaves = vec![self.emit_cur(Inst::Index {
            base: *owner, index: *selector, kind: ContainerKind::Map,
        })];
        for path in value_leaf_paths(&shape) {
            let suffix = self.emit_cur(Inst::Const(Lit::Str(Ustr::from(&format!(".{path}")))));
            let index = self.emit_cur(Inst::StringConcat(*selector, suffix));
            leaves.push(self.emit_cur(Inst::Index {
                base: *owner, index, kind: ContainerKind::Map,
            }));
        }
        Ok(Some(self.rebuild_from_leaves(&shape, &leaves, &mut 0)))
    }

    fn record_concrete_ablation(&self, e: Idx<Expr>, func: FuncRef) {
        if !crate::ablation::enabled() {
            return;
        }
        let callee = &self.ws.module(func.module).functions[func.func];
        if callee.generics.is_empty() {
            return;
        }
        crate::ablation::note_examined();
        let plan = self.concrete_plan;
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
        let plan = self.concrete_plan;
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
        self.concrete_plans.contains_key(inst).then(|| inst.clone())
    }

    pub(crate) fn concrete_callee_param_ty(
        &self,
        e: Idx<Expr>,
        inst: &crate::instance::Instance,
        pidx: usize,
    ) -> Result<Option<std::rc::Rc<crate::monoty::MonoTy>>, CompilerError> {
        let callee = &self.ws.module(inst.func.module).functions[inst.func.func];
        let plan = self.concrete_plan;
        let site = crate::site::Site {
            domain: self.cur_domain.clone(),
            expr: e,
            op: self.cur_op.clone(),
        };
        if plan.callees.get(&site) != Some(inst) {
            return Err(self.gap_here("concrete callee identity disagrees with its site plan"));
        }
        let signature = if crate::lower::requires_concrete_body_plan(self.ws, inst)
            && crate::monoty::too_deep_to_instantiate(inst.arg_types(self.scx.interner()).iter()).is_none()
        {
            &self
                .concrete_plans
                .get(inst)
                .ok_or_else(|| self.gap_here("concrete callee body plan is missing for its site"))?
                .signature
        } else {
            plan.callee_signatures
                .get(&site)
                .ok_or_else(|| self.gap_here("concrete callee signature is missing for its site"))?
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
        Ok(Some(self.concrete_mono_view(*id, ConcreteMonoConsumer::CallParameter)))
    }

    pub(crate) fn concrete_callee_return_ty(
        &self,
        e: Idx<Expr>,
        inst: &crate::instance::Instance,
    ) -> Result<Option<crate::monoty::MonoTy>, CompilerError> {
        let plan = self.concrete_plan;
        let site = crate::site::Site {
            domain: self.cur_domain.clone(),
            expr: e,
            op: self.cur_op.clone(),
        };
        if plan.callees.get(&site) != Some(inst) {
            return Err(self.gap_here("concrete callee identity disagrees with its site plan"));
        }
        let signature = if crate::lower::requires_concrete_body_plan(self.ws, inst)
            && crate::monoty::too_deep_to_instantiate(inst.arg_types(self.scx.interner()).iter()).is_none()
        {
            &self
                .concrete_plans
                .get(inst)
                .ok_or_else(|| self.gap_here("concrete callee body plan is missing for its return"))?
                .signature
        } else {
            self.concrete_plan
                .callee_signatures
                .get(&site)
                .ok_or_else(|| self.gap_here("concrete callee signature is missing for its return"))?
        };
        Ok(signature
            .result
            .map(|id| self.concrete_mono_owned(id, ConcreteMonoConsumer::ReturnLowering)))
    }

    pub(crate) fn concrete_coroutine_channel(
        &self,
        receiver: Idx<Expr>,
    ) -> Result<Option<crate::lower::YieldChannel>, CompilerError> {
        let body_plan = self.concrete_plan;
        let site = crate::site::Site {
            domain: self.cur_domain.clone(),
            expr: receiver,
            op: crate::site::SiteOp::DirectCall,
        };
        let result = if let Some(owner) = body_plan.callees.get(&site) {
            let signature = if crate::lower::requires_concrete_body_plan(self.ws, owner)
                && crate::monoty::too_deep_to_instantiate(owner.arg_types(self.scx.interner()).iter()).is_none()
            {
                &self.concrete_plans.get(owner).ok_or_else(|| {
                    self.gap_here("concrete coroutine callee body plan is missing")
                })?
                .signature
            } else {
                body_plan.callee_signatures.get(&site).ok_or_else(|| {
                    self.gap_here("concrete coroutine callee signature is missing")
                })?
            };
            signature
                .result
                .map(|id| self.concrete_mono_view(id, ConcreteMonoConsumer::CoroutineChannel))
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
    ) -> Result<BackendFnAbiPlanHandle, CompilerError> {
        crate::lower::abi::callee_abi_plan_for_instance(
            self.ws,
            self.call_scx,
            self.concrete_plans,
            inst,
        )
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
        self.pack_maybe_absent(r)
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
        if self.scx.target() == bone_semantic::hir::FnBackend::Lua {
            if let Callee::Unresolved { name, .. } = callee {
                if name.as_str() == "field_get" {
                    if let Some(repr) = self.lower_nullable_field_get(e, name, &arg_idxs)? {
                        return Ok(repr);
                    }
                }
            }
        }
        if let Callee::Unresolved { name, .. } = callee {
            if matches!(name.as_str(), "inspect" | "inspect_formatted") {
                if let Some(&arg) = arg_idxs.first() {
                    if let Some(name) = self
                        .concrete_expr_ty(arg)
                        .and_then(|ty| ty.peel_nullable().0.nominal())
                        .filter(|name| self.ws.struct_by_name.contains_key(name))
                    {
                        self.f.struct_registry_roots.insert(name);
                    }
                    let flat_elem = match self.vec_column_elem_of(arg)? {
                        Some(elem) => Some(elem),
                        None => self.vec_strided_elem_of(arg)?,
                    };
                    if let Some(elem) = flat_elem {
                        if elem.is_nobox() {
                            return Err(CompilerError::new_with_label(
                                self.body.expr_spans[e],
                                format!(
                                    "cannot inspect a flat-stored nobox value `{}`: \
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
        // `tostring` of an enum, value record or handle is the structural
        // rendering, never the backend's own rendering of its representation.
        if let Some((arg, _)) = self.dispatch().tostring_route(e) {
            return self
                .with_site_expr(arg, |s| s.stringify_structural(arg))
                .map(ValueRepr::Scalar);
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
                        return self.lower_expr_repr(inner).map(ValueBundle::into_value);
                    }
                }
            }
        }
        // On Lua a value with no mutable heap storage is already the owner's
        // copy, so `clone_into` is its argument; the inline body allocated a
        // `seen` table per call even for a string. C keeps the call: its
        // strings are bytes the destination owner must own.
        if let Callee::Unresolved { name, .. } = callee {
            if name.as_str() == "clone_into"
                && self.scx.target() == bone_semantic::hir::FnBackend::Lua
            {
                if let [value, storage] = arg_idxs[..] {
                    let positional = arg_idxs.iter().all(|a| {
                        !self.body.arg_names.contains_key(a) && !self.body.spread_args.contains(a)
                    });
                    if positional && self.resolves_to_inline_builtin(e, name) {
                        if let Some(elem) = self.vec_column_elem_of(value)? {
                            let ty = MonoTy::App {
                                ctor: MonoCtor::Nominal(elem.name),
                                params: Vec::new(),
                            };
                            // Immutable elements can share leaves, but the vector needs
                            // independent columns. Mutable leaves still require deep cloning.
                            if super::shape::lua_value_is_immutable(self.ws, &ty) {
                                let vec = self.lower_expr(value)?;
                                self.lower_expr(storage)?;
                                return self.clone_column_data(vec, elem);
                            }
                        }
                    }
                    if positional
                        && self.resolves_to_inline_builtin(e, name)
                        && self
                            .concrete_expr_ty(value)
                            .is_some_and(|ty| super::shape::lua_value_is_immutable(self.ws, &ty))
                    {
                        let copy = self.lower_expr_repr(value)?;
                        self.lower_expr(storage)?;
                        return Ok(copy.into_value());
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
                let (ctype, representation) = self.native_buffer_alloc_ctype(e, span)?;
                let count = self.lower_expr(n_arg)?;
                let ptr = self.emit_cur(Inst::FfiNewArray { ctype, count, representation });
                let mut fields =
                    vec![(Ustr::from("ptr"), ptr), (Ustr::from("len"), count)];
                if representation.is_some() && self.is_leaf_addressed(ctype)? {
                    fields.extend(self.leaf_view_fields(ctype, ptr));
                }
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
                    if let Some(elem) = self.vec_strided_elem_of(recv)? {
                        return self.strided_push(recv, item, elem);
                    }
                    if let Some(elem) = self.vec_column_elem_of(recv)? {
                        return self.column_push(recv, item, elem);
                    }
                }
            }
            if let Some(r) = self.strided_data_primitive(name.as_str(), &arg_idxs)? {
                return Ok(r);
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
            // it cannot be read off an argument. It is also an opaque Lua
            // boundary; the concrete plan must select the column representation
            // before the body would otherwise expose a table-backed `_data`.
            if name.as_str() == "__vec_data_alloc" && arg_idxs.len() == 1 {
                if let Some(elem) = self.vec_strided_elem_of(e)? {
                    let capacity = self.lower_expr(arg_idxs[0])?;
                    return Ok(ValueRepr::Scalar(self.empty_strided_vec(elem, capacity)?));
                }
                if let Some(elem) = self.vec_column_elem_of(e)? {
                    let capacity = self.lower_expr(arg_idxs[0])?;
                    return Ok(ValueRepr::Scalar(self.empty_column_vec(elem, capacity)?));
                }
            }
            if name.as_str() == "vec_new" && arg_idxs.is_empty() {
                if let Some(elem) = self.vec_strided_elem_of(e)? {
                    let capacity = self.emit_cur(Inst::Const(Lit::Int(0)));
                    return Ok(ValueRepr::Scalar(self.empty_strided_vec(elem, capacity)?));
                }
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
                if let ([raw, len], Some(elem)) = (&arg_idxs[..], self.vec_strided_elem_of(e)?) {
                    let raw = self.lower_expr(*raw)?;
                    let len = self.lower_expr(*len)?;
                    return Ok(ValueRepr::Scalar(
                        self.strided_vec_from_raw_array(raw, len, elem)?,
                    ));
                }
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
        if let Some(field) = self.fn_field_of_call(e) {
            let Some((&receiver_arg, call_args)) = arg_idxs.split_first() else {
                return Err(self.gap_here("fn-field call has no receiver"));
            };
            let receiver = self.lower_expr(receiver_arg)?;
            let fieldv = self.emit_cur(Inst::Field { base: receiver, field });
            let field_ty = self
                .inf
                .and_then(|inf| inf.fn_field_call_types.get(&e))
                .copied()
                .ok_or_else(|| {
                    self.gap_here("fn-field call is missing its resolved function type")
                })?;
            let abi = self.fn_value_rep_plan(field_ty)?.ok_or_else(|| {
                self.gap_here("fn-field call has no concrete function ABI plan")
            })?;
            return self.indirect_call_by_plan(abi, fieldv, call_args);
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
            // INDIRECT call through a fn-typed global (`g(a)`).
            Callee::Unresolved { name, .. } => match self.resolve_funcref(e, name) {
                // A `@struct_expand` fn (docs/shaders.md) is replaced
                // by its literal pairs list -- recognized through the intrinsic
                // registry off the resolved FuncRef, so no user fn is hijacked by
                // name and the common non-intrinsic call pays only one field read.
                Ok(func) => match self.fn_expand_kind(func) {
                    Some(ExpandKind::SceneTypeKey) => {
                        self.expand_scene_type_key(e, func, &arg_idxs)
                    }
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
                    Some(ExpandKind::BuildVar | ExpandKind::EmbedString) => {
                        let kind = self.fn_expand_kind(func).unwrap();
                        self.expand_comptime_input(e, &arg_idxs, kind)
                    }
                    None => self.lower_direct_call_repr(e, func, &arg_idxs, any_named),
                },
                Err(gap_err) => {
                    // `g(args)` where `g` is a fn-typed module global.
                    if !any_named && self.is_fn_global(name) {
                        let abi = match self.ws.globals_by_name.get(&name) {
                            Some(&ty) => {
                                let mono = crate::monoty::convert_decl(self.ws, ty, &SpecSubst::default())
                                    .map_err(|reason| {
                                        self.gap_here(format!(
                                            "fn-typed global `{name}` has no concrete function type ({reason:?})"
                                        ))
                                    })?;
                                self.fn_value_rep_plan_for_mono(&mono)?
                            }
                            None => None,
                        };
                        if let Some(abi) = abi {
                            let g = self.emit_cur(Inst::Global(name));
                            return self.indirect_call_by_plan(abi, g, &arg_idxs);
                        }
                        let arg_vals: Vec<Idx<Inst>> =
                            arg_idxs.iter().map(|&a| self.lower_expr(a)).collect::<Result<_, _>>()?;
                        let g = self.emit_cur(Inst::Global(name));
                        let v = self.emit_call_value(e, g, arg_vals)?;
                        return Ok(ValueRepr::Scalar(v));
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
                    if let Some(identity) = self.f.nested_fn_identities.get(&abi.hoisted).cloned() {
                        self.f.code_value_identities.insert(g, identity);
                    }
                    let param_shapes = abi.param_slots(&self.scx.rep_arena());
                    let arg_vals = self.slotted_call_args(
                        e,
                        &abi.param_names,
                        &param_shapes,
                        &arg_idxs,
                        &[],
                    )?;
                    let Some(shape) = abi.result_shape(&self.scx.rep_arena()) else {
                        let v = self.emit_call_value(e, g, arg_vals)?;
                        self.f.call_rep_plans.insert(v, abi.representation.clone());
                        return Ok(ValueRepr::Scalar(v));
                    };
                    let dests: Vec<Idx<Cell>> =
                        (0..shape.leaf_count()).map(|_| self.f.new_cell(None)).collect();
                    let call = self.emit_cur(Inst::CallValueMulti { dests: dests.clone(), callee: g, args: arg_vals });
                    self.f.call_rep_plans.insert(call, abi.representation.clone());
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
        // A dispatched helper shares the hole's expression, not its user call's bound tree.
        let tree = if matches!(self.cur_op, crate::site::SiteOp::DirectCall) {
            self.inf
                .and_then(|i| i.bound_call_trees.get(&e))
                .filter(|t| !t.is_empty())
                .cloned()
        } else {
            None
        };
        // IDENTITY, computed beside the delivery decision and independent of it.
        // A call the policy declines to specialize still names exactly one
        // concrete instance, and this is where it says which.
        self.record_concrete_ablation(e, func);
        let callee_inst = self
            .concrete_callee_for_site(e)?
            .ok_or_else(|| self.gap_here("direct call has no concrete callee identity"))?;
        let delivery = self.concrete_delivery_instance(&callee_inst);
        let plan = self.callee_abi_plan_for_instance(&callee_inst)?;
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
        let owner = &dc.callee_inst;
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
        self.f.call_abi_plans.insert(call, dc.plan.clone());
        Ok(())
    }

    pub(crate) fn record_callee_bound_tree(
        &mut self,
        call: Idx<Inst>,
        owner: &crate::instance::Instance,
        raw_tree_present: bool,
    ) -> Result<(), CompilerError> {
        let Some(plan) = self.concrete_plans.get(owner) else {
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

    /// THE tuple-return shape of a call, read off the CALLEE's declared return
    /// type under the CALLEE's instantiation.
    ///
    /// One oracle, because there were two and they disagreed. `lower_call`
    /// computed this under the call's own `SpecSubst` while the presence
    /// consumers (`nullable_tuple_call_shape`) computed it under the ENCLOSING
    /// body's -- empty in a non-generic caller, so the callee's generic stayed
    /// unbound, every slot came back `Opaque`, and the tuple-plan guard answered
    /// `None`. The callee then faithfully
    /// returned its `false` presence slot and the call site threw it away:
    /// `!= null` fell through to packing a `MakeTuple`, which is structurally
    /// never nil, so it const-folded to `true` and `if let` emitted no branch at
    /// all. A shape disagreement here cannot fail loudly -- it just branches on
    /// a slot that is not there, or fails to.
    pub(crate) fn call_ret_tuple_shape(
        &self,
        e: Idx<Expr>,
    ) -> Result<Option<TupleRetShape>, CompilerError> {
        let owner = self
            .concrete_callee_for_site(e)?
            .ok_or_else(|| self.gap_here("tuple-return call has no concrete callee identity"))?;
        let abi = self.callee_abi_plan_for_instance(&owner)?;
        Ok(abi.ret_tuple())
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
            self.reject_nobox_opaque_value_return(e, func, &dc.callee_inst)?;
        }
        let lua_opaque = self.call_scx.target() == bone_semantic::hir::FnBackend::Lua
            && callee.inline_body.is_some()
            && !callee.generics.is_empty()
            && !callee.annotations.iter().any(|a| a.is_builtin("scalar_abi"));
        let ret_tuple = (!lua_opaque).then_some(declared_ret_tuple).flatten();
        // A flat-stored element passed to a `ref` parameter, or a `ref` result
        // addressing one, takes the flat-variant ABI (`flat_ref`). Only a
        // direct call's own site has a flat plan.
        if matches!(self.cur_op, crate::site::SiteOp::DirectCall)
            && let Some(plan) = self.flat_call_plan(e)?
        {
            return match self.lower_flat_call(e, func, arg_idxs, any_named, &dc, &plan, ret_tuple)? {
                FlatCallOut::Value(value) => Ok(value),
                FlatCallOut::Flat(_) => Err(CompilerError::new_with_label(
                    self.body.expr_spans[e],
                    "this call returns a `ref` to a flat-stored container element; pass it to a \
                     `ref` parameter, bind it with `let`, read its fields or compare it with null"
                        .to_string(),
                )),
            };
        }
        if !dc.plan.uses_scalar_abi() {
            let args = self.direct_call_args(e, func, arg_idxs, any_named, &dc)?;
            // Above the tuple early return: the exemption is a property of the
            // CALLEE, so making it arity-dependent would be a guarantee hole the
            // day a `@gpu_only` intrinsic returns a tuple.
            self.authorize_gpu_boundary_boxes(&func, &args);
            if let Some(shape) = &ret_tuple {
                return Ok(self.emit_tuple_call_tree(e, func, args, shape, &dc)?);
            }
            let call = self.emit_cur(Inst::Call { func, args });
            self.record_call(call, func, &dc)?;
            return Ok(ValueRepr::Scalar(call));
        }
        let args = self.direct_call_args(e, func, arg_idxs, any_named, &dc)?;
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
                Some(shape) => Ok(self.emit_tuple_call_tree(e, func, args, shape, &dc)?),
                None => {
                    let call = self.emit_cur(Inst::Call { func, args });
                    self.record_call(call, func, &dc)?;
                    Ok(ValueRepr::Scalar(call))
                }
            },
        }
    }

    /// The argument list of a direct call under `dc`'s ABI: boxed, or splatted
    /// to leaves for a scalar-ABI callee. A `ref` argument is its borrow.
    pub(crate) fn direct_call_args(
        &mut self,
        e: Idx<Expr>,
        func: FuncRef,
        arg_idxs: &[Idx<Expr>],
        any_named: bool,
        dc: &DirectCall,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        if dc.plan.uses_scalar_abi() {
            let param_shapes = dc.plan.param_shapes();
            return self.scalar_call_args(e, func, arg_idxs, any_named, &param_shapes);
        }
        let callee = &self.ws.module(func.module).functions[func.func];
        self.reject_nobox_boxed_call_args(func, arg_idxs)?;
        self.reject_column_container_opaque_slots(
            e,
            &dc.callee_inst,
        )?;
        let has_tuple_storage = dc
            .plan
            .representation()
            .params()
            .iter()
            .any(|&id| matches!(dc.plan.rep_plan(id).layout(), RepLayout::Tuple { .. }));
        Ok(if has_tuple_storage {
            let opaque = vec![RetSlot::Opaque; callee.param_decls.len()];
            self.scalar_call_args(e, func, arg_idxs, any_named, &opaque)?
        } else if any_named {
            self.reorder_named_args(e, arg_idxs, func, Some(&dc.callee_inst))?
        } else {
            let mut args: Vec<Idx<Inst>> = arg_idxs
                .iter()
                .enumerate()
                .map(|(i, &a)| {
                    let t = self
                        .concrete_callee_param_ty(e, &dc.callee_inst, i)?
                        .ok_or_else(|| self.gap_here("concrete call has no callee parameter type"))?;
                    if self.is_ref_call_arg(e, a) {
                        self.lower_ref_borrow(a)
                    } else {
                        let v = self.lower_expr_with_expected(a, Some(t.as_ref()))?;
                        Ok(self.coerced_mono(v, a, t.as_ref()))
                    }
                })
                .collect::<Result<_, CompilerError>>()?;
            args.extend(self.trailing_default_args(func, arg_idxs.len(), None)?);
            args
        })
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
    /// A tuple return does not exempt the call from its concrete owner:
    /// under column storage an erased body could read a `_data` the
    /// instantiation does not have, so the concrete plan is the only correct
    /// target.
    pub(crate) fn emit_tuple_call_tree(
        &mut self,
        e: Idx<Expr>,
        func: FuncRef,
        args: Vec<Idx<Inst>>,
        shape: &TupleRetShape,
        dc: &DirectCall,
    ) -> Result<ValueRepr, CompilerError> {
        let slot_types = self.tuple_opaque_slot_types(e, shape);
        let dests: Vec<Idx<Cell>> = slot_types
            .into_iter()
            .map(|ty| self.f.new_cell_typed(None, ty))
            .collect();
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
    /// that is the whole point of keying on the function-value representation
    /// plan.
    pub(crate) fn indirect_call_by_plan(
        &mut self,
        plan: FunctionRepPlan,
        callee: Idx<Inst>,
        arg_idxs: &[Idx<Expr>],
    ) -> Result<ValueRepr, CompilerError> {
        let arena = self.scx.rep_arena();
        let mut argv = Vec::new();
        for (i, &a) in arg_idxs.iter().enumerate() {
            let slot = required_wire_slot(
                plan.params(),
                i,
                false,
                "indirect function-value call",
                |id| arena.ret_slot(*id),
                |message| self.gap_here(message),
            )?;
            argv.extend(self.arg_values_for_slot(&slot, a)?);
        }
        let Some(result) = plan.result() else {
            let call = self.emit_cur(Inst::CallValue { callee, args: argv });
            self.f.call_rep_plans.insert(call, plan.clone());
            return Ok(ValueRepr::Scalar(call));
        };
        if let Some(shape) = arena.tuple_shape(result) {
            let dests: Vec<Idx<Cell>> =
                (0..shape.slot_count()).map(|_| self.f.new_cell(None)).collect();
            let call = self.emit_cur(Inst::CallValueMulti { dests: dests.clone(), callee, args: argv });
            self.f.call_rep_plans.insert(call, plan.clone());
            let loads: Vec<Idx<Inst>> =
                dests.iter().map(|c| self.emit_cur(Inst::Load(*c))).collect();
            return Ok(self.tuple_result(&shape, &loads));
        }
        let Some(shape) = arena.value_shape(result) else {
            let call = self.emit_cur(Inst::CallValue { callee, args: argv });
            self.f.call_rep_plans.insert(call, plan.clone());
            return Ok(ValueRepr::Scalar(call));
        };
        let dests: Vec<Idx<Cell>> =
            (0..shape.leaf_count()).map(|_| self.f.new_cell(None)).collect();
        let call = self.emit_cur(Inst::CallValueMulti { dests: dests.clone(), callee, args: argv });
        self.f.call_rep_plans.insert(call, plan);
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
        self.emit_call_value_n(self.expr_tuple_shape(e)?, callee, args)
    }

    /// [`Self::emit_call_value`] with an explicit tuple shape (the safe-nav call
    /// paths pass the nil-peeled shape, since their expr type is `(..)?`).
    pub(crate) fn emit_call_value_n(
        &mut self,
        shape: Option<TupleRetShape>,
        callee: Idx<Inst>,
        args: Vec<Idx<Inst>>,
    ) -> Result<Idx<Inst>, CompilerError> {
        match shape {
            Some(shape) => {
                let dests: Vec<Idx<Cell>> =
                    (0..shape.slot_count()).map(|_| self.f.new_cell(None)).collect();
                self.emit_cur(Inst::CallValueMulti { dests: dests.clone(), callee, args });
                let loads: Vec<Idx<Inst>> =
                    dests.iter().map(|c| self.emit_cur(Inst::Load(*c))).collect();
                let r = self.tuple_result(&shape, &loads);
                Ok(self.pack(r)?)
            }
            None => Ok(self.emit_cur(Inst::CallValue { callee, args })),
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
        self.reject_flat_return_outside_direct_call(e, &dc.callee_inst)?;
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
                Some(shape) => self.emit_tuple_call_tree(e, func, argv, &shape, &dc)?,
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
        let receiver_slot = required_wire_slot(
            &param_shapes,
            0,
            false,
            "UFCS receiver",
            |slot| slot.clone(),
            |message| self.gap_here(message),
        )?;
        match receiver_slot {
            RetSlot::Leaves(shape) => {
                let leaves = self.cdata_leaves(recv, &shape)?;
                argv.extend(leaves);
            }
            _ => argv.push(recv),
        }
        for (i, &a) in arg_idxs.iter().enumerate() {
            let slot = required_wire_slot(
                &param_shapes,
                i + 1,
                self.ws.module(func.module).functions[func.func].is_variadic,
                "UFCS call",
                |slot| slot.clone(),
                |message| self.gap_here(message),
            )?;
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
        e: Idx<Expr>,
        func: FuncRef,
        arg_idxs: &[Idx<Expr>],
        any_named: bool,
        param_shapes: &[RetSlot],
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        if !any_named {
            let mut out = Vec::new();
            for (i, &a) in arg_idxs.iter().enumerate() {
                let slot = required_wire_slot(
                    param_shapes,
                    i,
                    self.ws.module(func.module).functions[func.func].is_variadic,
                    "direct scalar-ABI call",
                    |slot| slot.clone(),
                    |message| self.gap_here(message),
                )?;
                out.extend(self.arg_values_for_call_slot(e, &slot, a)?);
            }
            out.extend(self.trailing_default_args(func, arg_idxs.len(), Some(param_shapes))?);
            return Ok(out);
        }
        let param_names: Vec<Ustr> = self.ws.module(func.module).functions[func.func]
            .param_decls
            .iter()
            .map(|pd| pd.name)
            .collect();
        let defaults = self.literal_param_defaults(func);
        self.slotted_call_args(e, &param_names, param_shapes, arg_idxs, &defaults)
    }

    /// Build a call's argument list by placing each argument into its PARAM SLOT
    /// (source order) and then splatting/nil-filling per slot: a named arg lands
    /// in the slot of that name, a struct spread `..op` by field name, everything
    /// else positionally. This is the one slot-placement rule, shared by every
    /// calling convention that has param names -- a convention with its own
    /// positional-only arg loop silently miscompiles `f(b: 2, a: 1)`.
    /// `param_shapes` shorter than `param_names` (or all-`Opaque`) is the boxed ABI.
    /// `defaults` is [`literal_param_defaults`] of the callee, or empty when the
    /// callee is not a declaration the emitter can read (a nested fn value).
    pub(crate) fn slotted_call_args(
        &mut self,
        e: Idx<Expr>,
        param_names: &[Ustr],
        param_shapes: &[RetSlot],
        arg_idxs: &[Idx<Expr>],
        defaults: &[Option<Lit>],
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        let mut slots: Vec<Option<ValueBundle>> = vec![None; param_names.len()];
        let mut next_pos = 0usize;
        for &a in arg_idxs {
            // Struct spread `..op`: decompose into per-field reprs (a struct-
            // literal operand is elided) and place each into its param slot.
            if self.body.spread_args.contains(&a) {
                for (fname, r) in self.spread_field_bundles(a)? {
                    let pidx = param_names.iter().position(|p| *p == fname).ok_or_else(|| {
                        gap(format!("spread field `{fname}` matches no parameter"))
                    })?;
                    slots[pidx] = Some(r);
                }
                continue;
            }
            let r = if self.is_ref_call_arg(e, a) {
                let place = self.lower_ref_borrow(a)?;
                self.value_bundle_for_expr(a, ValueRepr::Scalar(place))?
            } else {
                self.lower_expr_repr(a)?
            };
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
        // A trailing param with a literal default is passed explicitly too.
        let hi = defaults
            .iter()
            .take(slots.len())
            .rposition(|d| d.is_some())
            .map_or(hi, |i| hi.max(i + 1));
        let mut out = Vec::new();
        for i in 0..hi {
            let slot = required_wire_slot(
                param_shapes,
                i,
                false,
                "slotted call",
                |slot| slot.clone(),
                |message| self.gap_here(message),
            )?;
            match slots[i].clone() {
                Some(r) => out.extend(self.repr_to_slot_values_bundle(&r, &slot)?),
                // A one-slot param with a literal default is passed that literal.
                None if slot.slot_count() == 1 && defaults.get(i).is_some_and(Option::is_some) => {
                    let lit = defaults[i].clone().expect("checked above");
                    out.push(self.emit_cur(Inst::Const(lit)));
                }
                // Otherwise an omitted param arrives as one nil PER WIRE VALUE,
                // so the callee's default-fill sees a nil in every leaf it binds.
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
    pub(crate) fn pack(&mut self, r: ValueRepr) -> Result<Idx<Inst>, CompilerError> {
        self.pack_boxed(r, BoxKind::Implicit)
    }

    /// [`Self::pack`] with the box's AUTHORIZATION carried through the whole
    /// tree. This is the one packer: `materialize(v)` differs from an ordinary
    /// escape only in which ctor instruction each `value`-struct box mints, so
    /// it takes this path rather than a parallel one. A second recursion went
    /// wrong at every tier the first one had grown an arm for -- a flat-enum
    /// field reached it as if it were a struct, and a leaf-table struct emitted
    /// a declared-form ctor against a leaf-table plan.
    pub(crate) fn pack_boxed(
        &mut self,
        r: ValueRepr,
        kind: BoxKind,
    ) -> Result<Idx<Inst>, CompilerError> {
        let r = match r {
            r @ ValueRepr::Group { .. } => self.owned_value_children(r)?,
            r => r,
        };
        match r {
            ValueRepr::Scalar(v) => Ok(v),
            // A splatting tuple's box. Its `nobox` elements pack too, which is
            // exactly how a whole-tuple use reaches the guarantee's rejection.
            ValueRepr::Tuple { parts, .. } => {
                let vs = parts
                    .into_iter()
                    .map(|p| self.pack_maybe_absent_boxed(p, kind))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(self.emit_cur(Inst::MakeTuple(vs)))
            }
            // A flat-enum Group `[tag, _0.., <rec>..]` packs to the `{__variant}`
            // box (`MakeEnumDyn`), the enum escape -- not a `<name>_ct` cdata.
            ValueRepr::Group { name, parts, .. }
                if EnumPlan::for_name(self.ws, name).is_some_and(|plan| plan.flat().is_some()) =>
            {
                let shape = EnumPlan::for_name(self.ws, name)
                    .and_then(|plan| plan.flat().cloned())
                    .ok_or_else(|| {
                        self.gap_here(format!("flat enum `{name}` lost its representation plan"))
                    })?;
                let expected_len = 1 + shape.tuple_width() + shape.record_fields.len();
                if parts.len() != expected_len {
                    return Err(self.gap_here(format!(
                        "flat enum `{name}` produced {} fields for a plan requiring {expected_len}",
                        parts.len()
                    )));
                }
                let packed: Vec<Idx<Inst>> = parts
                    .into_iter()
                    .map(|p| self.pack_maybe_absent_boxed(p, kind))
                    .collect::<Result<Vec<_>, _>>()?;
                let cells = FlatCells {
                    tag: packed[0],
                    positional: packed[1..1 + shape.tuple_width()].to_vec(),
                    named: shape
                    .record_fields
                    .iter()
                    .enumerate()
                    .map(|(i, rf)| (*rf, packed[1 + shape.tuple_width() + i]))
                    .collect(),
                };
                Ok(self.materialize_flat_cells(&shape, cells))
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
            // An ORDINARY value has no residue check to catch a lost presence,
            // and its consumers (`let`, `== null`, a boxed argument) pack
            // without branching, so its box must be `nil` when absent.
            ValueRepr::Group { name, parts, present: Some(present) }
                if !self.is_nobox_struct(name) =>
            {
                self.pack_maybe_absent_boxed(
                    ValueRepr::Group { name, parts, present: Some(present) },
                    kind,
                )
            }
            ValueRepr::Group { name, parts, present } => {
                // A leaf-table struct's physical keys ARE its leaves, so it
                // packs through the flat `<Name>_lct` ctor. Packing it
                // field-wise would box a flattened child under its declared
                // name and violate the selected plan. Non-value Groups are
                // enum/ordinary aggregate carriers and keep their own packer.
                let storage = if self.is_value_struct(name) {
                    Some(
                        self.scx
                            .rep_arena()
                            .nominal_value_storage_required(self.ws, self.scx.target, name)
                            .map_err(|e| self.gap_here(e.message))?,
                    )
                } else {
                    None
                };
                if storage == Some(ValueStorage::LeafTable) {
                    let representation = self.nominal_value_representation(name)?;
                    let shape = self
                        .scx
                        .rep_arena()
                        .value_shape(representation)
                        .ok_or_else(|| self.gap_here(format!("`{name}` has no planned value shape")))?;
                    let bundle = ValueBundle::new(
                        representation,
                        ValueRepr::Group { name, parts, present },
                    );
                    let leaves = self.repr_to_leaves_bundle(&bundle)?;
                    let paths = value_leaf_paths(&shape);
                    if paths.len() != leaves.len() {
                        return Err(self.gap_here(format!(
                            "`{name}` leaf plan has {} paths but the value produced {} leaves",
                            paths.len(),
                            leaves.len()
                        )));
                    }
                    let fields = paths.into_iter().zip(leaves).collect();
                    let ctor = kind.value_ctor(
                        name,
                        representation,
                        ValueCtorForm::Leaves,
                        fields,
                    );
                    Ok(self.emit_cur(ctor))
                } else {
                    self.pack_group_fields(name, parts, kind)
                }
            }
        }
    }

    pub(crate) fn pack_bundle(
        &mut self,
        bundle: &ValueBundle,
    ) -> Result<Idx<Inst>, CompilerError> {
        if let ValueRepr::Group { name, parts, present: Some(present) } = bundle.value() {
            return self.pack_typed_nullable(*name, parts.clone(), *present, BoxKind::Implicit);
        }
        let Some(shape) = self
            .scx
            .rep_arena()
            .tuple_shape(bundle.representation())
        else {
            return self.pack(bundle.value().clone());
        };
        let mut slots = self.tuple_repr_to_slots(bundle.value().clone(), &shape)?;
        if !shape.nullable {
            return Ok(self.emit_cur(Inst::MakeTuple(slots)));
        }
        let present = slots.remove(0);
        let result = self.f.new_cell(None);
        let absent_bb = self.f.new_block();
        let present_bb = self.f.new_block();
        let join = self.f.new_block();
        self.f.set_term(
            self.cur,
            Term::Branch {
                cond: present,
                then_bb: present_bb,
                else_bb: absent_bb,
            },
        );

        self.cur = absent_bb;
        let nil = self.emit_cur(Inst::Const(Lit::Nil));
        self.emit_cur(Inst::Store {
            cell: result,
            value: nil,
        });
        self.f.set_term(self.cur, Term::Goto(join));

        self.cur = present_bb;
        let tuple = self.emit_cur(Inst::MakeTuple(slots));
        self.emit_cur(Inst::Store {
            cell: result,
            value: tuple,
        });
        self.f.set_term(self.cur, Term::Goto(join));

        self.cur = join;
        Ok(self.emit_cur(Inst::Load(result)))
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
    pub(crate) fn pack_maybe_absent(&mut self, r: ValueRepr) -> Result<Idx<Inst>, CompilerError> {
        self.pack_maybe_absent_boxed(r, BoxKind::Implicit)
    }

    /// A presence-carrying group as one typed nullable value. Straight-line,
    /// unlike [`Self::pack_maybe_absent_boxed`]'s nil-or-box join, which has no
    /// single C representation inside a parent constructor.
    fn pack_typed_nullable(
        &mut self,
        name: Ustr,
        parts: Vec<ValueRepr>,
        present: Idx<Inst>,
        kind: BoxKind,
    ) -> Result<Idx<Inst>, CompilerError> {
        let value = self.pack_boxed(ValueRepr::Group { name, parts, present: None }, kind)?;
        Ok(self.emit_cur(Inst::MakeNullableValue { present, value }))
    }

    /// [`Self::pack_maybe_absent`] carrying the box's authorization, the shape
    /// `materialize(v)` of a nullable value takes.
    pub(crate) fn pack_maybe_absent_boxed(
        &mut self,
        r: ValueRepr,
        kind: BoxKind,
    ) -> Result<Idx<Inst>, CompilerError> {
        let (present, value) = match r {
            ValueRepr::Group { name, parts, present: Some(present) } => {
                (present, ValueRepr::Group { name, parts, present: None })
            }
            ValueRepr::Tuple { parts, present: Some(present) } => {
                (present, ValueRepr::Tuple { parts, present: None })
            }
            other => return self.pack_boxed(other, kind),
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
        // The present arm is an ordinary whole-value box, so it goes through the
        // same tier dispatch: a leaf-table or flat-enum payload has no
        // declared-field ctor to take.
        let boxed = self.pack_boxed(value, kind)?;
        self.emit_cur(Inst::Store { cell: result, value: boxed });
        self.f.set_term(self.cur, Term::Goto(join));

        self.cur = join;
        Ok(self.emit_cur(Inst::Load(result)))
    }

    /// The physical layout of GC-table value `name` when some field is stored
    /// inline (a `nobox` child keyed by dotted leaf paths); `None` for every
    /// other box, which keys each declared field whole. First-class targets
    /// nest children as declared fields, so they never take this layout.
    pub(crate) fn gc_table_inline_layout(
        &self,
        name: Ustr,
    ) -> Result<Option<Vec<GcTableField>>, CompilerError> {
        if self.scx.values_first_class || !self.is_value_struct(name) {
            return Ok(None);
        }
        let storage = self
            .scx
            .rep_arena()
            .nominal_value_storage_required(self.ws, self.scx.target, name)
            .map_err(|e| self.gap_here(e.message))?;
        if storage != ValueStorage::GcTable {
            return Ok(None);
        }
        let fields = crate::lower::gc_table_fields(self.ws, self.scx.target, name)
            .map_err(|e| self.gap_here(e.message))?;
        Ok(fields.iter().any(GcTableField::is_inline).then_some(fields))
    }

    /// A GC-table Group's `<Name>_vt` operands, keyed as `layout` stores them:
    /// a whole field packs, an inline child contributes its leaves. A presence
    /// tag canonicalizes to `true`/`nil`, the form a `= null` write stores; the
    /// scalar ABI spells absence `false`, and `__eq` compares tags per key.
    fn gc_table_box_fields(
        &mut self,
        name: Ustr,
        layout: &[GcTableField],
        parts: Vec<ValueRepr>,
        kind: BoxKind,
    ) -> Result<Vec<(Ustr, Idx<Inst>)>, CompilerError> {
        if parts.len() != layout.len() {
            return Err(self.gap_here(format!(
                "GC-table value `{name}` has {} parts for {} fields",
                parts.len(),
                layout.len()
            )));
        }
        let mut out = Vec::new();
        for (field, part) in layout.iter().zip(parts) {
            let Some(sub) = &field.inline else {
                out.push((field.name, self.pack_maybe_absent_boxed(part, kind)?));
                continue;
            };
            let leaves = self.repr_to_leaves_shape(&part, sub)?;
            let keys = field.keys();
            if keys.len() != leaves.len() {
                return Err(self.gap_here(format!(
                    "`{name}.{}` has {} keys but the value produced {} leaves",
                    field.name,
                    keys.len(),
                    leaves.len()
                )));
            }
            for ((key, presence), leaf) in keys.into_iter().zip(leaves) {
                let value = if presence {
                    let tag = self.emit_cur(Inst::Const(Lit::Bool(true)));
                    self.emit_cur(Inst::MakeNullableValue { present: leaf, value: tag })
                } else {
                    leaf
                };
                out.push((key, value));
            }
        }
        Ok(out)
    }

    /// The `<name>_ct(..)` cdata for a Group's parts, with no presence test.
    ///
    /// The parts are in declared field order (the positional ctor ABI requires
    /// it), so they line up with the declared names. The names let MIR passes
    /// forward a `Field` read to its ctor operand (SROA), eliminating the cdata
    /// when it never escapes. A payload-free enum field is just an `int` value
    /// (Prim), packed like any primitive.
    fn pack_group_fields(
        &mut self,
        name: Ustr,
        parts: Vec<ValueRepr>,
        kind: BoxKind,
    ) -> Result<Idx<Inst>, CompilerError> {
        let representation = self.nominal_value_representation(name)?;
        if let Some(layout) = self.gc_table_inline_layout(name)? {
            let fields = self.gc_table_box_fields(name, &layout, parts, kind)?;
            let ctor = kind.value_ctor(name, representation, ValueCtorForm::Declared, fields);
            return Ok(self.emit_cur(ctor));
        }
        let names = struct_field_names(self.ws, name);
        let fields = parts
            .into_iter()
            .enumerate()
            .map(|(i, p)| -> Result<(Ustr, Idx<Inst>), CompilerError> {
                // A Group's parts are the nominal's declared fields, so the two
                // always line up. Naming a part after its STRUCT instead would
                // emit a ctor field the type does not have, and duplicate it for
                // every part -- fabricated identity, not a usable fallback.
                let fname = *names
                    .get(i)
                    .ok_or_else(|| self.gap_here("a planned value group has too many fields"))?;
                // A nullable nobox field is observable as null through its
                // parent, so its presence must reach the field.
                if let ValueRepr::Group { name, parts, present: Some(present) } = p {
                    return Ok((fname, self.pack_typed_nullable(name, parts, present, kind)?));
                }
                Ok((fname, self.pack_boxed(p, kind)?))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let ctor = kind.value_ctor(name, representation, ValueCtorForm::Declared, fields);
        Ok(self.emit_cur(ctor))
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
        bundle: ValueBundle,
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
        self.repr_to_slot_values_bundle(&bundle, &slot)
    }

    pub(crate) fn repr_to_leaves_shape(
        &mut self,
        r: &ValueRepr,
        shape: &ValueShape,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        match r {
            // A tuple at a value-shaped sink is a boxed source: pack and
            // field-read, the `Scalar` treatment.
            ValueRepr::Tuple { .. } => {
                let v = self.pack(r.clone())?;
                self.repr_to_leaves_shape(&ValueRepr::Scalar(v), shape)
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
                        FieldShape::Prim => out.push(self.pack_maybe_absent(part.clone())?),
                        FieldShape::Nested(sub) => out.extend(self.repr_to_leaves_shape(part, sub)?),
                    }
                }
                Ok(out)
            }
            ValueRepr::Scalar(v) if shape.nullable => {
                let field_count = shape.leaf_count() - 1;
                if matches!(self.f.insts[*v], Inst::Const(Lit::Nil)) {
                    let mut out = Vec::with_capacity(1 + field_count);
                    out.push(self.emit_cur(Inst::Const(Lit::Bool(false))));
                    for _ in 0..field_count {
                        out.push(self.emit_cur(Inst::Const(Lit::Nil)));
                    }
                    return Ok(out);
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
                let leaves = self.cdata_leaves(*v, &inner)?;
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
                Ok(out)
            }
            ValueRepr::Scalar(v) => self.cdata_leaves(*v, shape),
        }
    }

    pub(crate) fn repr_to_leaves_bundle(
        &mut self,
        bundle: &ValueBundle,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        let shape = self
            .scx
            .rep_arena()
            .value_shape(bundle.representation())
            .ok_or_else(|| {
                CompilerError::new_no_location(format!(
                    "representation {:?} has no value leaf shape",
                    bundle.representation()
                ))
            })?;
        self.repr_to_leaves_shape(bundle.value(), &shape)
    }

    pub(crate) fn repr_to_leaves_bundle_for_shape(
        &mut self,
        bundle: &ValueBundle,
        expected: &ValueShape,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        let actual = self
            .scx
            .rep_arena()
            .value_shape(bundle.representation())
            .ok_or_else(|| {
                self.gap_here(format!(
                    "representation {:?} has no value leaf plan while lowering `{}`",
                    bundle.representation(),
                    self.f.name
                ))
            })?;
        if actual.name != expected.name || actual.fields != expected.fields {
            return Err(self.gap_here(format!(
                "representation {:?} shape {:?} does not match destination value shape {:?}",
                bundle.representation(),
                actual,
                expected
            )));
        }
        if actual.nullable && !expected.nullable {
            return self.repr_to_leaves_bundle_after_presence(bundle, expected);
        }
        if actual == *expected {
            self.repr_to_leaves_bundle(bundle)
        } else {
            self.repr_to_leaves_shape(bundle.value(), expected)
        }
    }

    pub(crate) fn repr_to_storage_leaves(
        &mut self,
        bundle: &ValueBundle,
        expected: &ValueShape,
        reason: &str,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        let plan = self
            .scx
            .rep_arena()
            .try_get(bundle.representation())
            .ok_or_else(|| {
                self.gap_here(format!(
                    "representation {:?} does not belong to this build arena at {reason}",
                    bundle.representation()
                ))
            })?;
        match plan.layout() {
            RepLayout::Opaque | RepLayout::Foreign { .. } => {
                self.opaque_boundary_leaves(bundle, expected, reason)
            }
            RepLayout::Leaves(_) => self.repr_to_leaves_bundle_for_shape(bundle, expected),
            RepLayout::Tuple { .. } => Err(self.gap_here(format!(
                "tuple representation {:?} cannot fill a value storage shape at {reason}",
                bundle.representation()
            ))),
        }
    }

    pub(crate) fn repr_to_leaves_bundle_after_presence(
        &mut self,
        bundle: &ValueBundle,
        expected: &ValueShape,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        let actual = self
            .scx
            .rep_arena()
            .value_shape(bundle.representation())
            .ok_or_else(|| {
                self.gap_here(format!(
                    "representation {:?} has no value leaf plan while lowering `{}`",
                    bundle.representation(),
                    self.f.name
                ))
            })?;
        if actual.name != expected.name || actual.fields != expected.fields {
            return Err(self.gap_here(format!(
                "representation {:?} does not match narrowed return shape",
                bundle.representation()
            )));
        }
        if !actual.nullable || expected.nullable {
            return self.repr_to_leaves_bundle_for_shape(bundle, expected);
        }
        if !matches!(bundle.value(), ValueRepr::Group { present: Some(_), .. }) {
            return Err(self.gap_here(format!(
                "representation {:?} lost its presence witness before narrowing",
                bundle.representation()
            )));
        }
        self.repr_to_leaves_shape(bundle.value(), expected)
    }

    pub(crate) fn opaque_boundary_leaves(
        &mut self,
        bundle: &ValueBundle,
        expected: &ValueShape,
        reason: &str,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        let plan = self
            .scx
            .rep_arena()
            .try_get(bundle.representation())
            .ok_or_else(|| {
                self.gap_here(format!(
                    "representation {:?} does not belong to this build arena at {reason}",
                    bundle.representation()
                ))
            })?;
        if !matches!(plan.layout(), RepLayout::Opaque | RepLayout::Foreign { .. }) {
            return Err(self.gap_here(format!(
                "representation {:?} is not an opaque boundary at {reason}",
                bundle.representation()
            )));
        }
        self.repr_to_leaves_shape(bundle.value(), expected)
    }

    pub(crate) fn repr_to_slot_values_bundle(
        &mut self,
        bundle: &ValueBundle,
        slot: &RetSlot,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        match slot {
            RetSlot::Opaque => self.pack_bundle(bundle).map(|value| vec![value]),
            RetSlot::Leaves(expected) => {
                let plan = self
                    .scx
                    .rep_arena()
                    .try_get(bundle.representation())
                    .ok_or_else(|| {
                        self.gap_here(format!(
                            "representation {:?} is missing at a leaves slot while lowering `{}`",
                            bundle.representation(),
                            self.f.name
                        ))
                    })?;
                match plan.layout() {
                    RepLayout::Leaves(actual) => {
                        if actual.name != expected.name || actual.fields != expected.fields {
                            return Err(self.gap_here(format!(
                                "representation {:?} shape does not match its leaves slot shape",
                                bundle.representation()
                            )));
                        }
                        if actual.nullable && !expected.nullable {
                            return self.repr_to_leaves_bundle_after_presence(bundle, expected);
                        }
                        self.repr_to_leaves_bundle_for_shape(bundle, expected)
                    }
                    RepLayout::Opaque | RepLayout::Foreign { .. } => self
                        .opaque_boundary_leaves(bundle, expected, "scalar ABI argument"),
                    RepLayout::Tuple { .. } => Err(self.gap_here(format!(
                        "tuple representation {:?} cannot fill a leaves slot",
                        bundle.representation()
                    ))),
                }
            }
            RetSlot::Nested(expected) => {
                let actual = self
                    .scx
                    .rep_arena()
                    .tuple_shape(bundle.representation())
                    .ok_or_else(|| {
                        self.gap_here(format!(
                            "representation {:?} has no tuple plan for a nested slot",
                            bundle.representation()
                        ))
                    })?;
                if !ret_slot_can_fill(
                    &RetSlot::Nested(actual.clone()),
                    &RetSlot::Nested(expected.clone()),
                ) {
                    return Err(self.gap_here(format!(
                        "representation {:?} tuple shape {:?} does not match its nested tuple slot {:?}",
                        bundle.representation(), actual, expected
                    )));
                }
                self.tuple_repr_to_slots(bundle.value().clone(), expected)
            }
        }
    }

    /// Flatten a boxed value to its primitive leaves via chained reads (the inverse
    /// of `pack`, used when a value arrives boxed). A flat ENUM box reads its box
    /// keys (`__variant`, positional `[i+1]`, `.recfield`) in shape order; a cdata
    /// value reads its `<name>_ct` fields.
    pub(crate) fn cdata_leaves(
        &mut self,
        v: Idx<Inst>,
        shape: &ValueShape,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        if let Some(es) = EnumPlan::for_name(self.ws, shape.name).and_then(|plan| plan.flat().cloned()) {
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
            return Ok(cells.iter().map(|c| self.emit_cur(Inst::Load(*c))).collect());
        }
        // A shape with no nullable and no enum child anywhere is a plain key
        // walk: the paths come from the one rule the uniform packer reads a
        // global with too (`boxed_leaf_paths`), so the two readers cannot drift.
        // Each intermediate box is loaded ONCE and its children read off it
        // (a whole-value read wants every leaf anyway); the packer walks the
        // same paths but re-reads the parent per leaf, keeping one leaf live.
        if self.shape_is_plain_keys(shape) {
            let paths = self.boxed_leaf_paths(shape)?;
            let mut loaded: Vec<(Vec<Ustr>, Idx<Inst>)> = Vec::new();
            let mut out = Vec::with_capacity(paths.len());
            for path in paths {
                let mut cur = v;
                for depth in 1..=path.len() {
                    let prefix = &path[..depth];
                    if let Some((_, inst)) = loaded.iter().find(|(p, _)| p.as_slice() == prefix) {
                        cur = *inst;
                        continue;
                    }
                    cur = self.emit_cur(Inst::Field { base: cur, field: path[depth - 1] });
                    if depth < path.len() {
                        loaded.push((prefix.to_vec(), cur));
                    }
                }
                out.push(cur);
            }
            return Ok(out);
        }
        // A leaf-table box stores one key per leaf, so the split IS the key
        // reads -- the exact inverse of the `<Name>_lct` ctor, including the
        // presence key at a nullable child's own name.
        let storage = self
            .scx
            .rep_arena()
            .nominal_value_storage_required(self.ws, self.scx.target, shape.name)
            .map_err(|e| self.gap_here(e.message))?;
        if storage == ValueStorage::LeafTable && !shape.nullable
        {
            return Ok(value_leaf_paths(shape)
                .into_iter()
                .map(|path| self.emit_cur(Inst::Field { base: v, field: path }))
                .collect());
        }
        // A GC-table box keys an inline child's leaves by dotted path: the
        // exact inverse of `gc_table_box_fields`, presence tag included.
        let inline = self.gc_table_inline_layout(shape.name)?;
        let mut out = Vec::new();
        for (i, (fname, fs)) in shape.fields.iter().enumerate() {
            if let Some(field) = inline.as_ref().and_then(|layout| layout.get(i)).filter(|f| f.is_inline()) {
                for (key, _) in field.keys() {
                    out.push(self.emit_cur(Inst::Field { base: v, field: key }));
                }
                continue;
            }
            let fv = self.emit_cur(Inst::Field { base: v, field: *fname });
            match fs {
                FieldShape::Prim => out.push(fv),
                FieldShape::Nested(sub) if sub.nullable => out.extend(self.nullable_child_leaves(fv, sub)?),
                FieldShape::Nested(sub) => out.extend(self.cdata_leaves(fv, sub)?),
            }
        }
        Ok(out)
    }

    /// The leaves of a NULLABLE nested child `fv`: the stored box may be
    /// absent, so split it presence-aware rather than field-reading straight
    /// off a possible nil. Mirrors `repr_to_leaves`' `Scalar(v) if
    /// shape.nullable` arm exactly -- presence is `v ~= nil` as leaf 0, then
    /// the child's own leaves, nils on the absent arm -- because the two must
    /// agree leaf-for-leaf or `rebuild_from_leaves` indexes past the end.
    fn nullable_child_leaves(
        &mut self,
        fv: Idx<Inst>,
        sub: &ValueShape,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        let field_count = sub.leaf_count() - 1;
        let nil = self.emit_cur(Inst::Const(Lit::Nil));
        let present = self.emit_cur(Inst::NotEq(fv, nil));
        let cells: Vec<Idx<Cell>> = (0..field_count).map(|_| self.f.new_cell(None)).collect();
        let nil_bb = self.f.new_block();
        let value_bb = self.f.new_block();
        let join = self.f.new_block();
        self.f.set_term(self.cur, Term::Branch { cond: present, then_bb: value_bb, else_bb: nil_bb });
        self.cur = nil_bb;
        for c in &cells {
            let z = self.emit_cur(Inst::Const(Lit::Nil));
            self.emit_cur(Inst::Store { cell: *c, value: z });
        }
        self.f.set_term(self.cur, Term::Goto(join));
        self.cur = value_bb;
        let mut inner = sub.clone();
        inner.nullable = false;
        let leaves = self.cdata_leaves(fv, &inner)?;
        for (c, lv) in cells.iter().zip(leaves) {
            self.emit_cur(Inst::Store { cell: *c, value: lv });
        }
        self.f.set_term(self.cur, Term::Goto(join));
        self.cur = join;
        let mut out = vec![present];
        out.extend(cells.iter().map(|c| self.emit_cur(Inst::Load(*c))));
        Ok(out)
    }

    /// Whether every leaf of `shape` is reached by a plain key walk: no
    /// nullable child (presence branch) and no flat enum child (tag spread)
    /// anywhere.
    pub(crate) fn shape_is_plain_keys(&self, shape: &ValueShape) -> bool {
        if shape.nullable || EnumPlan::for_name(self.ws, shape.name).is_some() {
            return false;
        }
        shape.fields.iter().all(|(_, fs)| match fs {
            FieldShape::Prim => true,
            FieldShape::Nested(sub) => self.shape_is_plain_keys(sub),
        })
    }

    /// The key path from a box of `shape` (no nullable child anywhere) to each
    /// of its leaves, in `repr_to_leaves` order. A leaf-table box keys every
    /// leaf by its dotted path at the top level; cdata and GC boxes nest, and
    /// a nested child consults its own storage, so a leaf-table child inside a
    /// GC parent keys by its dotted path under the parent's field.
    pub(crate) fn boxed_leaf_paths(
        &self,
        shape: &ValueShape,
    ) -> Result<Vec<Vec<Ustr>>, CompilerError> {
        let mut out = Vec::new();
        self.boxed_leaf_paths_into(shape, &mut Vec::new(), &mut out)?;
        Ok(out)
    }

    fn boxed_leaf_paths_into(
        &self,
        shape: &ValueShape,
        prefix: &mut Vec<Ustr>,
        out: &mut Vec<Vec<Ustr>>,
    ) -> Result<(), CompilerError> {
        let storage = self
            .scx
            .rep_arena()
            .nominal_value_storage_required(self.ws, self.scx.target, shape.name)
            .map_err(|e| self.gap_here(e.message))?;
        if storage == ValueStorage::LeafTable {
            for path in value_leaf_paths(shape) {
                let mut full = prefix.clone();
                full.push(path);
                out.push(full);
            }
            return Ok(());
        }
        let inline = self.gc_table_inline_layout(shape.name)?;
        for (i, (fname, fs)) in shape.fields.iter().enumerate() {
            if let Some(field) = inline.as_ref().and_then(|layout| layout.get(i)).filter(|f| f.is_inline()) {
                for (key, _) in field.keys() {
                    let mut full = prefix.clone();
                    full.push(key);
                    out.push(full);
                }
                continue;
            }
            prefix.push(*fname);
            match fs {
                FieldShape::Prim => out.push(prefix.clone()),
                FieldShape::Nested(sub) => self.boxed_leaf_paths_into(sub, prefix, out)?,
            }
            prefix.pop();
        }
        Ok(())
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
    pub(crate) fn boxed_value_copy_repr(
        &mut self,
        boxed: Idx<Inst>,
        name: Ustr,
        owner_ty: Option<&MonoTy>,
    ) -> Result<ValueRepr, CompilerError> {
        let shape = self.nominal_rep_shape_required(name, false)?;
        self.enter_value_copy(name)?;
        let copy = self.boxed_copy_repr_of(boxed, &shape, owner_ty);
        self.value_copy_stack.pop();
        copy
    }

    /// Copies are unrolled at compile time, so a copy of `name` inside its own
    /// copy (`next: Node?` inside `Node`) would need a runtime copier. There
    /// is none, so it is rejected rather than left sharing the child.
    fn enter_value_copy(&mut self, name: Ustr) -> Result<(), CompilerError> {
        if self.value_copy_stack.contains(&name) {
            return Err(self.gap_here(format!(
                "recursive value type `{name}` cannot be copied: its `{name}?` descendant would need a runtime copier"
            )));
        }
        self.value_copy_stack.push(name);
        Ok(())
    }

    /// The concrete type of `owner.field`. `owner_ty` substitutes a generic
    /// owner's arguments; a generic owner without one has no field types.
    fn value_field_ty(
        &self,
        owner: Ustr,
        owner_ty: Option<&MonoTy>,
        field: Ustr,
    ) -> Result<Rc<MonoTy>, CompilerError> {
        if !self.struct_is_generic(owner) {
            let ty = self
                .struct_field_ty(owner, field)
                .ok_or_else(|| self.gap_here(format!("field `{owner}.{field}` is not declared")))?;
            return crate::monoty::convert_decl(self.ws, ty, &crate::monoty::MonoSubst::default())
                .map(Rc::new)
                .map_err(|reason| self.gap_here(format!("field `{owner}.{field}` has no concrete type: {reason:?}")));
        }
        let owner_ty = owner_ty.ok_or_else(|| {
            self.gap_here(format!("a copy of generic value `{owner}` has no concrete type for `{field}`"))
        })?;
        self.struct_field_ty_in(owner, field, owner_ty)
    }

    fn struct_is_generic(&self, name: Ustr) -> bool {
        self.ws
            .struct_by_name
            .get(&name)
            .is_some_and(|sid| !self.ws.modules[sid.module.index()].structs[sid.def].generics.is_empty())
    }

    /// The type of `owner.field` when its plan holds an ordinary value as one
    /// opaque slot: a declared `V?`, or a generic `T`/`T?` instantiated at a
    /// value.
    fn opaque_value_child(
        &self,
        owner: Ustr,
        owner_ty: Option<&MonoTy>,
        field: Ustr,
    ) -> Result<Option<Rc<MonoTy>>, CompilerError> {
        if !self.struct_is_generic(owner) {
            let Some(ty) = self.struct_field_ty(owner, field) else { return Ok(None) };
            let Some((child, true)) = super::shape::nominal_name_from_ty(self.ws, ty) else {
                return Ok(None);
            };
            if !self.is_value_struct(child) || self.is_nobox_struct(child) {
                return Ok(None);
            }
        }
        let ty = self.value_field_ty(owner, owner_ty, field)?;
        let ordinary = ty
            .peel_nullable()
            .0
            .nominal()
            .is_some_and(|child| self.is_value_struct(child) && !self.is_nobox_struct(child));
        Ok(ordinary.then_some(ty))
    }

    /// The owner type a nested field's copy needs: only a generic child
    /// substitutes anything.
    fn nested_owner_ty(
        &self,
        owner: Ustr,
        owner_ty: Option<&MonoTy>,
        field: Ustr,
        child: Ustr,
    ) -> Result<Option<Rc<MonoTy>>, CompilerError> {
        if !self.struct_is_generic(child) {
            return Ok(None);
        }
        self.value_field_ty(owner, owner_ty, field).map(Some)
    }

    /// A repr built from a source's leaves keeps each opaque value child as
    /// the source's box: a read through it needs nothing more, and the copy
    /// happens where the value escapes ([`Self::owned_value_children`]).
    pub(crate) fn mark_borrowed_value_children(
        &mut self,
        shape: &ValueShape,
        repr: &ValueRepr,
        owner_ty: Option<&MonoTy>,
    ) -> Result<(), CompilerError> {
        let ValueRepr::Group { parts, .. } = repr else { return Ok(()) };
        if EnumPlan::for_name(self.ws, shape.name).is_some() {
            return Ok(());
        }
        for ((fname, fs), part) in shape.fields.iter().zip(parts) {
            match (fs, part) {
                (FieldShape::Prim, ValueRepr::Scalar(v)) => {
                    if let Some(ty) = self.opaque_value_child(shape.name, owner_ty, *fname)? {
                        self.borrowed_value_children.insert(*v, ty);
                    }
                }
                (FieldShape::Nested(sub), part) => {
                    let sub_ty = self.nested_owner_ty(shape.name, owner_ty, *fname, sub.name)?;
                    self.mark_borrowed_value_children(sub, part, sub_ty.as_deref())?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// `r` with each borrowed opaque value child copied, at an escape: a box
    /// ([`Self::pack_boxed`]), a return, or an enum payload. A child is
    /// borrowed when a copy, a scalarized parameter or a binding read it from
    /// a source that still holds it.
    pub(crate) fn owned_value_children(&mut self, r: ValueRepr) -> Result<ValueRepr, CompilerError> {
        if self.scx.values_first_class || self.lending_pack {
            return Ok(r);
        }
        match r {
            ValueRepr::Group { name, parts, present } => {
                let parts = parts
                    .into_iter()
                    .map(|part| match part {
                        ValueRepr::Scalar(v) => match self.borrowed_value_children.get(&v).cloned() {
                            Some(ty) => self.copied_value_box(&ty, v).map(ValueRepr::Scalar),
                            None => Ok(ValueRepr::Scalar(v)),
                        },
                        part => self.owned_value_children(part),
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(ValueRepr::Group { name, parts, present })
            }
            ValueRepr::Tuple { parts, present } => {
                let parts = parts
                    .into_iter()
                    .map(|p| self.owned_value_children(p))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(ValueRepr::Tuple { parts, present })
            }
            r => Ok(r),
        }
    }

    /// [`Self::pack`] for a `let` binding's own cell: its value children stay
    /// borrowed, since only the binding holds the box and each whole use of
    /// it copies ([`Self::boxed_value_local`]).
    pub(crate) fn pack_lent(&mut self, r: ValueRepr) -> Result<Idx<Inst>, CompilerError> {
        let outer = std::mem::replace(&mut self.lending_pack, true);
        let packed = self.pack(r);
        self.lending_pack = outer;
        packed
    }

    /// The copy of a non-null box of `shape`. Enums are immutable, so a GC-table
    /// box's whole enum child is shared rather than split and rebuilt: only the
    /// value's own storage has to be fresh. Every other box splits to leaves.
    fn boxed_copy_repr_of(
        &mut self,
        v: Idx<Inst>,
        shape: &ValueShape,
        owner_ty: Option<&MonoTy>,
    ) -> Result<ValueRepr, CompilerError> {
        let gc_table = !self.scx.values_first_class
            && !shape.nullable
            && EnumPlan::for_name(self.ws, shape.name).is_none()
            && !self.shape_is_plain_keys(shape)
            && self
                .scx
                .rep_arena()
                .nominal_value_storage_required(self.ws, self.scx.target, shape.name)
                .map_err(|e| self.gap_here(e.message))?
                == ValueStorage::GcTable;
        if !gc_table {
            let leaves = self.cdata_leaves(v, shape)?;
            let copy = self.rebuild_from_leaves(shape, &leaves, &mut 0);
            self.mark_borrowed_value_children(shape, &copy, owner_ty)?;
            return Ok(copy);
        }
        let inline = self.gc_table_inline_layout(shape.name)?;
        let mut parts = Vec::with_capacity(shape.fields.len());
        for (i, (fname, fs)) in shape.fields.iter().enumerate() {
            if let Some(field) = inline.as_ref().and_then(|layout| layout.get(i)).filter(|f| f.is_inline()) {
                let leaves: Vec<Idx<Inst>> = field
                    .keys()
                    .into_iter()
                    .map(|(key, _)| self.emit_cur(Inst::Field { base: v, field: key }))
                    .collect();
                parts.push(match fs {
                    FieldShape::Prim => ValueRepr::Scalar(leaves[0]),
                    FieldShape::Nested(sub) => self.rebuild_from_leaves(sub, &leaves, &mut 0),
                });
                continue;
            }
            let fv = self.emit_cur(Inst::Field { base: v, field: *fname });
            parts.push(match fs {
                FieldShape::Prim => {
                    if let Some(ty) = self.opaque_value_child(shape.name, owner_ty, *fname)? {
                        self.borrowed_value_children.insert(fv, ty);
                    }
                    ValueRepr::Scalar(fv)
                }
                FieldShape::Nested(sub)
                    if EnumPlan::for_name(self.ws, sub.name).is_some_and(|plan| plan.flat().is_some()) =>
                {
                    ValueRepr::Scalar(fv)
                }
                FieldShape::Nested(sub) if sub.nullable => {
                    let leaves = self.nullable_child_leaves(fv, sub)?;
                    let copy = self.rebuild_from_leaves(sub, &leaves, &mut 0);
                    let sub_ty = self.nested_owner_ty(shape.name, owner_ty, *fname, sub.name)?;
                    self.mark_borrowed_value_children(sub, &copy, sub_ty.as_deref())?;
                    copy
                }
                FieldShape::Nested(sub) => {
                    let sub_ty = self.nested_owner_ty(shape.name, owner_ty, *fname, sub.name)?;
                    self.boxed_copy_repr_of(fv, sub, sub_ty.as_deref())?
                }
            });
        }
        Ok(ValueRepr::Group { name: shape.name, parts, present: None })
    }

    /// The `V?` twin of [`Self::boxed_value_copy_repr`]: an absent value has no
    /// leaves to split, so the copy runs on the PRESENT arm only and `nil` threads
    /// through untouched. The result is a single value (a fresh box or `nil`), not
    /// a `Group` -- a Group carries no presence for `pack`, which would materialize
    /// a box for an absent value and break `== null`.
    pub(crate) fn nullable_boxed_value_copy(
        &mut self,
        boxed: Idx<Inst>,
        name: Ustr,
        owner_ty: Option<&MonoTy>,
    ) -> Result<Idx<Inst>, CompilerError> {
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

        // The child copies made while packing are this copy's descendants.
        self.cur = val_bb;
        let shape = self.nominal_rep_shape_required(name, false)?;
        self.enter_value_copy(name)?;
        let packed = self.boxed_copy_repr_of(boxed, &shape, owner_ty).and_then(|copy| self.pack(copy));
        self.value_copy_stack.pop();
        self.store_and_join(result, packed?, join);

        self.cur = join;
        Ok(self.emit_cur(Inst::Load(result)))
    }

    /// A fresh box for a whole value of type `ty` that is already held by one
    /// owner and is about to be stored into another. Anything other than an
    /// ordinary (possibly nullable) value struct passes through unchanged.
    pub(crate) fn copied_value_box(
        &mut self,
        ty: &MonoTy,
        v: Idx<Inst>,
    ) -> Result<Idx<Inst>, CompilerError> {
        match self.copied_value_repr(ty, v)? {
            ValueRepr::Scalar(v) => Ok(v),
            copy => self.pack(copy),
        }
    }

    /// [`Self::copied_value_box`] before packing: a non-null copy stays a
    /// `Group`, so a leaf consumer reads its parts and nothing is built.
    pub(crate) fn copied_value_repr(
        &mut self,
        ty: &MonoTy,
        v: Idx<Inst>,
    ) -> Result<ValueRepr, CompilerError> {
        let (payload, nullable) = ty.peel_nullable();
        let Some(name) = payload
            .nominal()
            .filter(|name| self.is_value_struct(*name) && !self.is_nobox_struct(*name))
        else {
            return Ok(ValueRepr::Scalar(v));
        };
        if self.scx.values_first_class {
            return Ok(ValueRepr::Scalar(v));
        }
        if nullable {
            return Ok(ValueRepr::Scalar(self.nullable_boxed_value_copy(v, name, Some(payload))?));
        }
        self.boxed_value_copy_repr(v, name, Some(payload))
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
            ContainerKind::VecValueColumns(elem) => {
                self.column_element_repr(arr, index, elem).map(ValueBundle::into_value)
            }
            ContainerKind::VecValueStrided(elem) => {
                self.strided_element_repr(arr, index, elem).map(ValueBundle::into_value)
            }
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
        let fields = self.struct_field_names(name);
        fields?.iter().position(|f| *f == field)
    }
}
