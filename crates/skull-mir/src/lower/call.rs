//! Call lowering: direct and indirect calls, the scalar-ABI arg paths, tuple
//! call trees, and the pack/leaf plumbing that decides whether a value crosses
//! a call boundary as leaves or as a box.
use super::*;

impl Builder<'_> {
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
                    if let Some(elem) = self.vec_column_elem_of(arg) {
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
                "__op_add" | "__op_sub" | "__op_mul" | "__op_div" | "__op_mod"
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
                    if let Some(elem) = self.vec_column_elem_of(recv) {
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
                if let Some(elem) = self.vec_column_elem_of(e) {
                    let capacity = self.lower_expr(arg_idxs[0])?;
                    return Ok(ValueRepr::Scalar(self.empty_column_vec(elem, capacity)?));
                }
            }
            if name.as_str() == "vec_new" && arg_idxs.is_empty() {
                if let Some(elem) = self.vec_column_elem_of(e) {
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
                if let Some(elem) = self.map_column_value_of(e) {
                    return Ok(ValueRepr::Scalar(self.empty_column_map(elem)?));
                }
            }
            if name.as_str() == "__rock_vec_from_lua_array" {
                if let ([raw, len], Some(elem)) = (&arg_idxs[..], self.vec_column_elem_of(e)) {
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
            if name.as_str() == "resume"
                && !arg_idxs.is_empty()
                && self.is_coroutine_recv(arg_idxs[0])
            {
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
                let shape = self.bound_next_tuple_shape(gidx);
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
                Ok(func) if matches!(self.fn_expand_kind(func), Some(ExpandKind::StructPairs)) => {
                    self.expand_struct_pairs(e, func, &arg_idxs)
                }
                Ok(func) if matches!(self.fn_expand_kind(func), Some(ExpandKind::Materialize)) => {
                    self.expand_materialize(&arg_idxs)
                }
                Ok(func)
                    if matches!(self.fn_expand_kind(func), Some(ExpandKind::ShaderPipeline)) =>
                {
                    self.expand_shader_pipeline(e, &arg_idxs)
                }
                Ok(func) => self.lower_direct_call_repr(e, func, &arg_idxs, any_named),
                Err(gap_err) => {
                    if !any_named {
                        let arg_vals: Vec<Idx<Inst>> =
                            arg_idxs.iter().map(|&a| self.lower_expr(a)).collect::<Result<_, _>>()?;
                        // `recv.field(args)` where `field` is a fn-typed field:
                        // call through `(recv.field)(args)` (recv is UFCS arg 0).
                        if let Some(&recv_expr) = arg_idxs.first() {
                            if self.struct_field_is_fn(recv_expr, name) {
                                let fieldv = self.emit_cur(Inst::Field { base: arg_vals[0], field: name });
                                let v = self.emit_call_value(e, fieldv, arg_vals[1..].to_vec());
                                return Ok(ValueRepr::Scalar(v));
                            }
                        }
                        // `g(args)` where `g` is a fn-typed module global.
                        if self.is_fn_global(name) {
                            let g = self.emit_cur(Inst::Global(name));
                            let v = self.emit_call_value(e, g, arg_vals);
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
                    let g = self.emit_cur(Inst::Global(abi.hoisted));
                    let arg_vals =
                        self.slotted_call_args(&abi.param_names, &abi.param_shapes, &arg_idxs)?;
                    let Some(shape) = abi.ret_shape else {
                        let v = self.emit_call_value(e, g, arg_vals);
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
                if let Some(abi) = self.binding_ty(b).and_then(|t| self.fn_value_abi(t)) {
                    return self.indirect_call_by_abi(abi, callee_v, &arg_idxs);
                }
                // Nothing `nobox` in the signature -- args are boxed.
                let arg_vals: Vec<Idx<Inst>> =
                    arg_idxs.iter().map(|&a| self.lower_expr(a)).collect::<Result<_, _>>()?;
                let v = self.emit_call_value(e, callee_v, arg_vals);
                Ok(ValueRepr::Scalar(v))
            }
            // A fn-typed expression result: lower it, call through it.
            Callee::Value(ve) => {
                let abi = self.expr_fn_value_abi(ve);
                let callee_v = self.lower_expr(ve)?;
                if let Some(abi) = abi {
                    return self.indirect_call_by_abi(abi, callee_v, &arg_idxs);
                }
                let arg_vals: Vec<Idx<Inst>> =
                    arg_idxs.iter().map(|&a| self.lower_expr(a)).collect::<Result<_, _>>()?;
                let v = self.emit_call_value(e, callee_v, arg_vals);
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
    pub(crate) fn call_spec(&self, e: Idx<Expr>, func: FuncRef) -> Option<(SpecKey, SpecSubst)> {
        compute_call_spec(self.ws, self.inf?, e, func, self.spec_subst, self.scx.target)
    }

    /// [`Self::call_spec`] narrowed by the target's storage filter -- the form
    /// every consumer of a call's INSTANTIATION must use, so none of them
    /// disagrees about whether this call specializes.
    pub(crate) fn call_spec_for_target(
        &self,
        e: Idx<Expr>,
        func: FuncRef,
    ) -> Option<(SpecKey, SpecSubst)> {
        self.call_spec(e, func).filter(|(k, _)| {
            !self.in_unspliced_ctx || spec_key_has_target_storage(self.ws, k, self.scx.target)
        })
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
    ) -> Option<TupleRetShape> {
        let spec = self.call_spec_for_target(e, func);
        let empty = SpecSubst::new();
        let subst = spec.as_ref().map_or(&empty, |(_, s)| s);
        let ret = self.ws.module(func.module).functions[func.func].return_ty?;
        tuple_ret_shape(self.ws, subst, ret)
    }

    /// [`Self::call_spec`] for the for-protocol `next` callee, whose bindings
    /// come from bone's `next_generics` rather than a call expr. Ordered into
    /// the callee's generic declaration order, which is what [`SpecKey`] is.
    pub(crate) fn next_call_spec(
        &self,
        func: FuncRef,
        next_generics: &rustc_hash::FxHashMap<bone_semantic::hir::VarId, TyId>,
    ) -> Option<SpecKey> {
        let inf = self.inf?;
        spec_key_from_next_generics(self.ws, inf, func, next_generics, self.spec_subst)
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
        let tree = self
            .inf
            .and_then(|i| i.bound_call_trees.get(&e))
            .filter(|t| !t.is_empty())
            .cloned();
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
        let spec = self.call_spec_for_target(e, func);
        // The instantiation key is recorded on the emitted call inst whenever this
        // is a specialized call -- even on the boxed (non-scalar-ABI) path, because
        // the specialized body still differs (e.g. a `native_buffer_alloc<T>` ctype
        // resolves) though the param/return ABI is unchanged.
        let key = spec.as_ref().map(|(k, _)| k.clone());
        let ret_tuple = self.call_ret_tuple_shape(e, func);
        if let Some(shape) = &ret_tuple {
            self.reject_nobox_opaque_tuple_return(e, func, shape)?;
        } else {
            self.reject_nobox_opaque_value_return(e, func)?;
        }
        let callee = &self.ws.module(func.module).functions[func.func];
        // Resolve phantom before tuple flattening can project from its nil placeholder.
        if arg_idxs.is_empty()
            && callee.inline_body.is_some_and(|(_, body)| body.trim() == "return nil")
        {
            return Ok(ValueRepr::Scalar(self.emit_cur(Inst::Const(Lit::Nil))));
        }
        let empty_subst = SpecSubst::new();
        let subst = spec.as_ref().map_or(&empty_subst, |(_, s)| s);
        let scalar_abi = fn_scalar_abi_spec(self.ws, self.scx, func, subst);
        let Some(scalar_abi) = scalar_abi else {
            self.reject_nobox_boxed_call_args(func, arg_idxs)?;
            let args = if any_named {
                self.reorder_named_args(arg_idxs, func)?
            } else {
                arg_idxs
                    .iter()
                    .enumerate()
                    .map(|(i, &a)| {
                        let v = self.lower_expr(a)?;
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
                return Ok(self.emit_tuple_call_tree(func, args, shape, tree, key));
            }
            let call = self.emit_cur(Inst::Call { func, args });
            if let Some(t) = tree {
                self.f.bound_trees.insert(call, t);
            }
            if let Some(k) = key {
                self.f.spec_bindings.insert(call, k);
            }
            return Ok(ValueRepr::Scalar(call));
        };
        let args = self.scalar_call_args(
            func,
            arg_idxs,
            any_named,
            &scalar_abi.param_shapes,
        )?;
        let ret_shape = scalar_abi.ret_shape;
        let key = spec.map(|(k, _)| k);
        match ret_shape {
            Some(shape) => {
                let n = shape.leaf_count();
                let dests: Vec<Idx<Cell>> = (0..n).map(|_| self.f.new_cell(None)).collect();
                let call = self.emit_cur(Inst::CallMulti { dests: dests.clone(), func, args });
                if let Some(t) = tree {
                    self.f.bound_trees.insert(call, t);
                }
                if let Some(k) = key {
                    self.f.spec_bindings.insert(call, k);
                }
                let leaves: Vec<Idx<Inst>> =
                    dests.iter().map(|c| self.emit_cur(Inst::Load(*c))).collect();
                let mut idx = 0;
                Ok(self.rebuild_from_leaves(&shape, &leaves, &mut idx))
            }
            None => match &ret_tuple {
                Some(shape) => Ok(self.emit_tuple_call_tree(func, args, shape, tree, key)),
                None => {
                    let call = self.emit_cur(Inst::Call { func, args });
                    if let Some(t) = tree {
                        self.f.bound_trees.insert(call, t);
                    }
                    if let Some(k) = key {
                        self.f.spec_bindings.insert(call, k);
                    }
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
    /// `tree` records an `Iterator`-bound resolution against the emitted call
    /// inst (chain roots like a directly-called bounded `next`).
    ///
    /// A tuple return does not exempt the call from its instantiation `key`:
    /// under column storage the shared generic body reads a `_data` the
    /// instantiation does not have, so the specialized copy is the only correct
    /// target.
    pub(crate) fn emit_tuple_call_tree(
        &mut self,
        func: FuncRef,
        args: Vec<Idx<Inst>>,
        shape: &TupleRetShape,
        tree: Option<bone_semantic::prelude::BoundTree>,
        key: Option<SpecKey>,
    ) -> ValueRepr {
        let dests: Vec<Idx<Cell>> =
            (0..shape.slot_count()).map(|_| self.f.new_cell(None)).collect();
        let call = self.emit_cur(Inst::CallMulti { dests: dests.clone(), func, args });
        if let Some(t) = tree {
            self.f.bound_trees.insert(call, t);
        }
        if let Some(k) = key {
            self.f.spec_bindings.insert(call, k);
        }
        let loads: Vec<Idx<Inst>> = dests.iter().map(|c| self.emit_cur(Inst::Load(*c))).collect();
        self.tuple_result(shape, &loads)
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
    pub(crate) fn indirect_call_by_abi(
        &mut self,
        (param_shapes, ret_shape): (Vec<RetSlot>, Option<ValueShape>),
        callee: Idx<Inst>,
        arg_idxs: &[Idx<Expr>],
    ) -> Result<ValueRepr, CompilerError> {
        let mut argv = Vec::new();
        for (i, &a) in arg_idxs.iter().enumerate() {
            let slot = param_shapes.get(i).cloned().unwrap_or(RetSlot::Opaque);
            argv.extend(self.arg_values_for_slot(&slot, a)?);
        }
        let Some(shape) = ret_shape else {
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
    pub(crate) fn emit_call_value(&mut self, e: Idx<Expr>, callee: Idx<Inst>, args: Vec<Idx<Inst>>) -> Idx<Inst> {
        self.emit_call_value_n(self.expr_tuple_shape(e), callee, args)
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

    /// A scalar-ABI UFCS call `method(recv, args)` where `recv` is an
    /// already-lowered value (the safe-nav receiver). The receiver fills param 0
    /// (splat its leaves if that param is a value), args fill the rest; a value
    /// return packs to a single value (the safe-nav result is one nullable value).
    /// A scalar-ABI UFCS call with the receiver already lowered, yielding a REPR:
    /// a value callee hands back its leaves, so a safe-nav join that stores PER
    /// SLOT never has to take a box apart again. The single packing caller went
    /// away when `?.` stopped packing.
    pub(crate) fn scalar_ufcs_call_repr(
        &mut self,
        func: FuncRef,
        recv: Idx<Inst>,
        arg_idxs: &[Idx<Expr>],
        abi: &FnScalarAbi,
    ) -> Result<ValueRepr, CompilerError> {
        let mut argv = Vec::new();
        // The RECEIVER arrives already lowered, so it cannot go through
        // `arg_values_for_slot`; a tuple receiver has no UFCS spelling, so
        // `Leaves` is the only splatting slot reachable here.
        match abi.param_shapes.first() {
            Some(RetSlot::Leaves(shape)) => {
                let leaves = self.cdata_leaves(recv, &shape.clone());
                argv.extend(leaves);
            }
            _ => argv.push(recv),
        }
        for (i, &a) in arg_idxs.iter().enumerate() {
            let slot = abi.param_shapes.get(i + 1).cloned().unwrap_or(RetSlot::Opaque);
            argv.extend(self.arg_values_for_slot(&slot, a)?);
        }
        match abi.ret_shape.clone() {
            Some(shape) => {
                let n = shape.leaf_count();
                let dests: Vec<Idx<Cell>> = (0..n).map(|_| self.f.new_cell(None)).collect();
                self.emit_cur(Inst::CallMulti { dests: dests.clone(), func, args: argv });
                let leaves: Vec<Idx<Inst>> =
                    dests.iter().map(|c| self.emit_cur(Inst::Load(*c))).collect();
                let mut idx = 0;
                Ok(self.rebuild_from_leaves(&shape, &leaves, &mut idx))
            }
            None => Ok(ValueRepr::Scalar(self.emit_cur(Inst::Call { func, args: argv }))),
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
                    if let Some(pidx) = param_names.iter().position(|p| *p == fname) {
                        slots[pidx] = Some(r);
                    }
                }
                continue;
            }
            let r = self.lower_expr_repr(a)?;
            let pidx = match self.body.arg_names.get(&a) {
                Some(nm) => param_names.iter().position(|p| p == nm).unwrap_or(next_pos),
                None => {
                    let i = next_pos;
                    next_pos += 1;
                    i
                }
            };
            if pidx < slots.len() {
                slots[pidx] = Some(r);
            }
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
                let positional = packed[1..1 + shape.tuple_width].to_vec();
                let named: Vec<(Ustr, Idx<Inst>)> = shape
                    .record_fields
                    .iter()
                    .enumerate()
                    .map(|(i, rf)| (*rf, packed[1 + shape.tuple_width + i]))
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
                let fname = names.get(i).copied().unwrap_or(name);
                (fname, self.pack(p))
            })
            .collect();
        self.emit_cur(Inst::MakeValueStruct { name, fields })
    }

    /// Flatten a `ValueRepr` to its primitive leaf values per `shape` (the wire
    /// ABI). A boxed `Scalar` (e.g. a global cdata / if-join) is field-read.
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
            ContainerKind::NativeBufferValue(elem) if self.is_leaf_addressed(elem) => {
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
