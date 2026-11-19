//! Expression lowering: the main dispatch, place bases, assignment and
//! compound assignment, `++`/`--`, safe navigation, and array/spread literals.
use super::flat_ref::FlatRefVal;
use super::*;

enum Place {
    Cell(Idx<Cell>),
    Global(Ustr),
    Field(Idx<Inst>, Ustr),
    Index(Idx<Inst>, Idx<Inst>, ContainerKind),
    Flat(FlatRefVal),
    Native(Idx<Inst>, Idx<Inst>, LeafAddr),
}

enum Write<'a, 'f> {
    Assign(Idx<Expr>),
    Update {
        postfix: bool,
        apply: &'f dyn Fn(&mut Builder<'a>, Idx<Inst>) -> Result<Idx<Inst>, CompilerError>,
    },
}

impl<'a> Builder<'a> {
    /// Forward a terminal destination through the syntax that has no value
    /// consumer. `None` leaves materialization to the ordinary value funnel.
    pub(crate) fn lower_expr_to_return(
        &mut self,
        e: Idx<Expr>,
    ) -> Option<Result<(), CompilerError>> {
        if self.f32_round_exprs.contains(&e) || self.integer_field_checks.contains_key(&e) {
            return None;
        }
        match self.body.expr_arena[e] {
            Expr::Block {
                statements,
                tail: Some(tail),
            } if !statements
                .into_iter()
                .any(|s| matches!(self.body.statement_arena[s], Statement::Defer(_))) =>
            {
                Some({
                    self.defers.push(Vec::new());
                    let result = (|| {
                        if !self.lower_stmts(statements)? {
                            self.lower_return(Some(tail))?;
                        }
                        Ok(())
                    })();
                    self.defers.pop();
                    result
                })
            }
            Expr::If {
                cond,
                then_branch,
                else_branch: Some(else_branch),
            } => Some(self.lower_if_to_return(cond, then_branch, else_branch)),
            _ => None,
        }
    }

    pub(crate) fn lower_expr(&mut self, e: Idx<Expr>) -> Result<Idx<Inst>, CompilerError> {
        self.lower_expr_with_expected(e, None)
    }

    pub(crate) fn lower_condition(&mut self, e: Idx<Expr>) -> Result<Idx<Inst>, CompilerError> {
        self.lower_expr_for_use(e, None, true, true)
    }

    /// Lower a branch without materializing short-circuit results. `condition`
    /// preserves the caller's leaf use: match guards use value coercion.
    pub(crate) fn lower_branch(
        &mut self,
        e: Idx<Expr>,
        then_bb: Idx<Block>,
        else_bb: Idx<Block>,
        condition: bool,
    ) -> Result<(), CompilerError> {
        let outer = self.cur_span.replace(self.body.expr_spans[e]);
        let outer_expr = self.cur_expr.replace(e);
        // Structured targets consume the materialized logical diamonds: their
        // structurizer cannot yet recover every direct short-circuit CFG.
        let direct = self.scx.target == bone_core::hir::FnBackend::Lua;
        let result = (|| {
            match self.body.expr_arena[e] {
                // Logical left operands always use condition coercion; their
                // right operands inherit the enclosing use, as in value lowering.
                Expr::BinOp {
                    op: BinOp::And,
                    lhs,
                    rhs,
                } if direct => {
                    let rhs_bb = self.f.new_block();
                    self.lower_branch(lhs, rhs_bb, else_bb, true)?;
                    self.cur = rhs_bb;
                    self.lower_branch(rhs, then_bb, else_bb, condition)
                }
                Expr::BinOp {
                    op: BinOp::Or,
                    lhs,
                    rhs,
                } if direct => {
                    let rhs_bb = self.f.new_block();
                    self.lower_branch(lhs, then_bb, rhs_bb, true)?;
                    self.cur = rhs_bb;
                    self.lower_branch(rhs, then_bb, else_bb, condition)
                }
                Expr::Not(x, _) if direct && self.overload_target(e).is_none() => {
                    self.lower_branch(x, else_bb, then_bb, true)
                }
                _ => {
                    let cond = if condition {
                        self.lower_condition(e)?
                    } else {
                        self.lower_expr(e)?
                    };
                    self.branch_to(cond, then_bb, else_bb);
                    Ok(())
                }
            }
        })();
        let result = result.map_err(|e| self.located(e));
        self.cur_span = outer;
        self.cur_expr = outer_expr;
        result
    }

    pub(crate) fn lower_iterable_collection(
        &mut self,
        e: Idx<Expr>,
    ) -> Result<Idx<Inst>, CompilerError> {
        self.lower_expr_for_use(e, None, false, false)
    }

    pub(crate) fn lower_expr_with_expected(
        &mut self,
        e: Idx<Expr>,
        expected: Option<&crate::monoty::MonoTy>,
    ) -> Result<Idx<Inst>, CompilerError> {
        self.lower_expr_for_use(e, expected, false, true)
    }

    fn lower_expr_for_use(
        &mut self,
        e: Idx<Expr>,
        expected: Option<&crate::monoty::MonoTy>,
        condition: bool,
        coerce_iterator: bool,
    ) -> Result<Idx<Inst>, CompilerError> {
        let first_inst = self.f.insts.len();
        // Source cursor for instruction-level diagnostics: whatever is lowered
        // from here down came from `e` unless a nested `lower_expr` narrows it,
        // and the restore puts the enclosing expression back on the way out.
        let outer = self.cur_span.replace(self.body.expr_spans[e]);
        // Keep the span and expression together when switching call sites.
        let outer_expr = self.cur_expr.replace(e);
        let v = match self.body.expr_arena[e] {
            Expr::Var(binding) if let Some(value) = self.preflight_value(binding) => Ok(value),
            Expr::Array(elems)
                if expected
                    .is_some_and(|ty| ContainerPlan::matches_type(ContainerFamily::Vec, ty)) =>
            {
                self.lower_array_literal_with_expected(e, elems, expected)
            }
            _ => self.lower_expr_inner(e, condition),
        };
        let v = v.map(|value| self.check_field_value(e, value));
        let v = v.map_err(|e| self.located(e));
        self.cur_span = outer;
        self.cur_expr = outer_expr;
        let v = v?;
        self.mark_record_base(e, v);
        // Ascriptions and transparent newtype constructors reuse their operand.
        // Keep the operand constructor's logical type on that shared value.
        let reuses_operand = matches!(self.body.expr_arena[e], Expr::As { .. })
            || matches!(
                &self.body.expr_arena[e],
                Expr::Call {
                    callee: Callee::Unresolved { name, .. },
                    args,
                    ..
                } if args.len() == 1 && self.ws.newtypes_by_name.contains_key(name)
            );
        // Projections can forward an existing constructor from an inactive enum
        // payload lane. Its logical type belongs to the producer, not this use.
        if !reuses_operand
            && v.into_raw().into_u32() as usize >= first_inst
            && matches!(
                self.f.insts[v],
                Inst::MakeValueStruct { .. }
                    | Inst::MaterializeValueStruct { .. }
                    | Inst::MakeTuple(..)
            )
        {
            if let Some(ty) = self
                .concrete_plan
                .domain_types(&self.cur_domain)
                .and_then(|types| types.expr_type(e))
            {
                match self.f.inst_logical_types.get(&v) {
                    Some(existing) if *existing != ty => {
                        return Err(self.gap_here(
                            "one MIR value was assigned conflicting concrete logical types",
                        ));
                    }
                    Some(_) => {}
                    None => {
                        self.f.inst_logical_types.insert(v, ty);
                    }
                }
            }
        }
        // Implicit iterable->iterator coercion at a bounded call arg: bone
        // recorded the resolved `iter` fn; wrap the arg value in that call
        // (the call-arg analog of `lower_for_user_iter`'s `iter_fn`).
        if coerce_iterator && let Some(w) = self.inf.and_then(|i| i.implicit_iter_args.get(&e)) {
            let (m, f) = w.func;
            let iter_ref = FuncRef { module: m, func: f };
            // Attribute the coercion to this expression after restoring its caller.
            let outer_expr = self.cur_expr.replace(e);
            // The instance is resolved BEFORE the arguments are shaped: they are
            // splatted against the ABI of the body the call targets, not the
            // declaration's.
            let witness = self
                .with_site_op(crate::site::SiteOp::ImplicitIter, |b| {
                    b.concrete_callee_for_site(e)
                })?
                .ok_or_else(|| gap("implicit iterator has no concrete callee"))?;
            let ty = self.concrete_expr_ty(e).ok_or_else(|| {
                self.gap_here("implicit iterator expression has no concrete type")
            })?;
            let tuple_spelled = matches!(ty.peel_nullable().0, crate::monoty::MonoTy::Tuple(_));
            let representation = self.concrete_rep_id(
                ty,
                if tuple_spelled {
                    RepUse::Return
                } else {
                    RepUse::Value
                },
                tuple_spelled,
                self.nobox_value_name(e).is_some(),
            )?;
            let call = self.with_site_op(crate::site::SiteOp::ImplicitIter, |b| {
                let args = b.iter_witness_args(
                    &witness,
                    ValueBundle::new(representation, ValueRepr::Scalar(v)),
                )?;
                Ok::<_, CompilerError>(b.emit_cur(Inst::Call {
                    func: iter_ref,
                    args,
                }))
            })?;
            if let Some(delivery) = self.concrete_delivery_instance(&witness) {
                self.f.spec_bindings.insert(call, delivery);
            }
            // Record the concrete iterator owner alongside the delivery target.
            let iter_decl = &self.ws.module(m).functions[f];
            if iter_decl.generics.is_empty() || !witness.args.is_empty() {
                crate::site::record_call_owner(&mut self.f, call, &witness);
            }
            let abi = self.callee_abi_plan_for_instance(&witness)?;
            self.f.call_abi_plans.insert(call, abi);
            self.cur_expr = outer_expr;
            return Ok(call);
        }
        Ok(v)
    }

    pub(crate) fn lower_lambda_with_plan(
        &mut self,
        e: Idx<Expr>,
        params: IdxRange<BindingInfo>,
        body: Idx<Expr>,
        lam_plan: FunctionRepPlan,
    ) -> Result<Idx<Inst>, CompilerError> {
        let outer_span = self.cur_span.replace(self.body.expr_spans[e]);
        let outer_expr = self.cur_expr.replace(e);
        let result = (|| -> Result<Idx<Inst>, CompilerError> {
            let before = self.f.params.len();
            for (i, pb) in params.into_iter().enumerate() {
                let pname = self.body.binding_arena[pb].name;
                let &id = lam_plan.params().get(i).ok_or_else(|| {
                    self.gap_here(format!("lambda ABI has no parameter slot {i}"))
                })?;
                self.bind_param_slot(pb, pname, self.scx.rep_arena().layout(id))?;
            }
            let param_cells: Vec<Idx<Cell>> = self.f.params.split_off(before);
            let entry = self.f.new_block();
            let saved = self.cur;
            self.cur = entry;
            let lambda_params: Vec<Idx<BindingInfo>> = params.into_iter().collect();
            self.copy_ref_borrowed_params(&lambda_params)?;
            // Use the lambda's return representation while lowering its body.
            let saved_ret_rep = self.ret_rep.take();
            let saved_flat_ret = self.flat_ret.take();
            let lambda_ret_ty = self.fn_result_ty(self.concrete_expr_ty(e))?;
            let saved_ret_ty = self.ret_ty.replace(lambda_ret_ty);
            // A return from the lambda runs only the lambda's defers.
            let saved_defers = std::mem::take(&mut self.defers);
            let saved_in_defer = std::mem::replace(&mut self.in_defer_body, false);
            // A lambda can unwind even when created inside a coroutine.
            let saved_mode = std::mem::replace(
                &mut self.defer_mode,
                super::stmt::DeferMode::for_body(false, self.call_scx.target),
            );
            self.ret_rep = lam_plan.result();
            let leaves = if self.ret_rep.is_none() {
                self.lower_stmt_expr(body)?;
                Vec::new()
            } else if self.result_shape().is_some() {
                let r = self.lower_expr_repr(body)?;
                if self.is_open() {
                    self.repr_to_leaves_bundle(&r)?
                } else {
                    Vec::new()
                }
            } else {
                match self.result_tuple() {
                    // A splatting tuple return needs the body as a repr, so a
                    // `nobox` element reaches the wire as leaves.
                    Some(shape) if shape.splats(self.scx.rep_arena()) => {
                        let r = self.lower_expr_repr(body)?;
                        if self.is_open() {
                            self.tuple_repr_to_slots(r.into_value(), shape)?
                        } else {
                            Vec::new()
                        }
                    }
                    Some(shape) => {
                        let v = self.lower_expr(body)?;
                        if self.is_open() {
                            self.tuple_return_operands(v, shape.slots.len())
                        } else {
                            Vec::new()
                        }
                    }
                    None => vec![self.lower_expr(body)?],
                }
            };
            if self.is_open() {
                self.f.set_term(self.cur, Term::Return(leaves));
            }
            self.defers = saved_defers;
            self.in_defer_body = saved_in_defer;
            self.defer_mode = saved_mode;
            self.ret_rep = saved_ret_rep;
            self.flat_ret = saved_flat_ret;
            self.ret_ty = saved_ret_ty;
            self.cur = saved;
            let id = self.emit_cur(Inst::Lambda {
                params: param_cells,
                entry,
            });
            self.f
                .lambda_abi_fingerprints
                .insert(id, lam_plan.logical_fingerprint(self.scx.rep_arena()));
            self.f
                .lambda_abi_representations
                .insert(id, lam_plan.clone());
            Ok(id)
        })()
        .map_err(|error| self.located(error));
        self.cur_span = outer_span;
        self.cur_expr = outer_expr;
        result
    }

    fn lower_expr_inner(
        &mut self,
        e: Idx<Expr>,
        condition: bool,
    ) -> Result<Idx<Inst>, CompilerError> {
        if self.lending_pack && matches!(self.body.expr_arena[e], Expr::Lambda { .. }) {
            return self.with_value_lending(false, |s| s.lower_expr_inner(e, condition));
        }
        if let Some(read) = self.flat_ref_read(e)? {
            return self.pack(read.into_value());
        }
        // A splatting tuple used as one value is its unpacked form stored flat,
        // whatever produced it; the repr path asks back here for the box itself.
        if let Expr::Var(b) = self.body.expr_arena[e]
            && let Some(&boxed) = self.tuple_bind_boxes.get(&raw(b))
        {
            return Ok(boxed);
        }
        if !matches!(self.body.expr_arena[e], Expr::Tuple(_))
            && !self.scalar_tuple_reentry.contains(&e)
            && self.tuple_box_shape(e)?.is_some()
        {
            self.scalar_tuple_reentry.insert(e);
            let repr = self.lower_expr_repr(e);
            self.scalar_tuple_reentry.remove(&e);
            return self.pack_stored(repr?);
        }
        match self.body.expr_arena[e] {
            Expr::Unit | Expr::Null => Ok(self.emit_cur(Inst::Const(Lit::Nil))),
            Expr::Int(i) => {
                if self.arith_ty(e) == Some(ArithTy::Uint) {
                    Ok(self.emit_cur(Inst::Const(Lit::Uint(i as u32))))
                } else {
                    Ok(self.emit_cur(Inst::Const(Lit::Int(i))))
                }
            }
            Expr::Float(of) => Ok(self.emit_cur(Inst::Const(Lit::Float(of.0)))),
            Expr::Bool(x) => Ok(self.emit_cur(Inst::Const(Lit::Bool(x)))),
            Expr::String(s) => Ok(self.emit_cur(Inst::Const(Lit::Str(s)))),

            // A field-ref key path (`.field` resolved against `FieldRef<T, V>`)
            // lowers to its interned field name. skull emits named struct keys,
            // so `obj[name]` is identical to `obj.field`.
            Expr::SiblingRef(name) => Ok(self.emit_cur(Inst::Const(Lit::Str(name)))),

            // A scalarized value param read in a BOXED context reconstructs the
            // cdata (`pack` the rebuilt Group); a plain local is a cell load.
            Expr::Var(b) if self.group_binds.contains_key(&raw(b)) => {
                let r = self.lower_expr_repr(e)?.into_value();
                self.pack_maybe_absent(r)
            }
            // A static is a private global: a whole value read out of it copies.
            Expr::Var(b) if self.static_cells.contains_key(&raw(b)) => {
                if !self.scx.values_first_class
                    && (self.value_struct_of(e).is_some()
                        || self.nullable_place_value_of(e)?.is_some())
                {
                    let r = self.lower_expr_repr(e)?.into_value();
                    return self.pack_maybe_absent(r);
                }
                Ok(self.emit_cur(Inst::Global(self.static_cells[&raw(b)])))
            }
            // A nested fn used as a value: reference the hoisted top-level fn by
            // name (a plain global read -- zero closure alloc).
            Expr::Var(b) if self.nested_fn_globals.contains_key(&raw(b)) => {
                let hoisted = self.nested_fn_globals[&raw(b)].hoisted;
                self.f.hoisted_fn_globals.push(hoisted);
                let id = self.emit_cur(Inst::Global(hoisted));
                if let Some(identity) = self.f.nested_fn_identities.get(&hoisted).cloned() {
                    self.f.code_value_identities.insert(id, identity);
                }
                Ok(id)
            }
            Expr::Var(b) if self.tuple_binds.contains_key(&raw(b)) => {
                let r = self.lower_expr_repr(e)?;
                self.pack_stored(r)
            }
            // Snapshot a boxed value local on read.
            Expr::Var(b) if let Some(name) = self.boxed_value_local(b, e) => {
                let boxed = self.emit_cur(Inst::Load(self.binds[&raw(b)]));
                if self.lending_pack && self.binding_value_is_shareable(b) {
                    return Ok(boxed);
                }
                let copy = self.boxed_value_copy_repr(boxed, name, self.concrete_expr_ty(e))?;
                self.pack(copy)
            }
            Expr::Var(b) if let Some(name) = self.nullable_boxed_value_local(b, e) => {
                let boxed = self.emit_cur(Inst::Load(self.binds[&raw(b)]));
                if self.lending_pack && self.binding_value_is_shareable(b) {
                    return Ok(boxed);
                }
                self.nullable_boxed_value_copy(boxed, name, self.concrete_expr_ty(e))
            }
            Expr::Var(b) if let Some(&value) = self.value_binds.get(&raw(b)) => Ok(value),
            Expr::Var(b) => {
                let cell = *self
                    .binds
                    .get(&raw(b))
                    .ok_or_else(|| gap("read of an unbound variable"))?;
                Ok(self.emit_cur(Inst::Load(cell)))
            }
            Expr::Global(name) if self.ws.functions_by_name.contains_key(&name) => {
                let fref = self.resolve_funcref(e, name)?;
                self.reject_fn_value_abi_mismatch(e, fref)?;
                let sym = if self.scx.target == bone_core::hir::FnBackend::Js {
                    name.to_string()
                } else {
                    self.fn_value_symbol(e, name)?
                };
                let id = self.with_site_op(crate::site::SiteOp::FnValue, |b| {
                    b.emit_cur(Inst::Global(crate::names::ustr(sym.as_str())))
                });
                // Record identity before lowering erases the source name.
                self.f.fn_value_symbols.insert(id, fref);
                if let Some(instance) =
                    self.concrete_callee_for_site_op(e, crate::site::SiteOp::FnValue)?
                {
                    self.reject_flat_return_outside_direct_call(e, &instance)?;
                    // Foreign, inline and intrinsic values remain explicit
                    // backend boundaries; only a compiler-owned Rock body
                    // gets a logical CodeIdentity.
                    if crate::lower::requires_concrete_body_plan(self.ws, &instance) {
                        self.f
                            .code_value_identities
                            .insert(id, crate::code_identity::CodeIdentity::Named(instance));
                    }
                }
                Ok(id)
            }
            // A global holding a value struct routes through the repr layer so a
            // whole-value read copies the box (a global is a mutable place).
            Expr::Global(name) => {
                if self.value_struct_of(e).is_some()
                    || self.nullable_place_value_of(e)?.is_some()
                    || self.nobox_global_place(e)?.is_some()
                {
                    let r = self.lower_expr_repr(e)?.into_value();
                    return self.pack_maybe_absent(r);
                }
                Ok(self.emit_cur(Inst::Global(name)))
            }

            Expr::Hole(name) => {
                let label = match name {
                    Some(n) => format!("reached unfilled hole ?{n}"),
                    None => "reached unfilled hole ?".to_string(),
                };
                let msg = self.emit_cur(Inst::Const(Lit::Str(crate::names::ustr(label.as_str()))));
                let func = self
                    .ws
                    .functions_by_name
                    .get(&crate::names::ustr("error"))
                    .and_then(|l| l.first())
                    .map(|&(module, func)| FuncRef { module, func })
                    .ok_or_else(|| gap("stdlib `error` missing for hole lowering"))?;
                self.emit_cur(Inst::Call {
                    func,
                    args: vec![msg],
                });
                Ok(self.emit_cur(Inst::Const(Lit::Nil)))
            }
            Expr::Error => Err(gap("lowering error reached MIR")),

            // Unary `!`/`~`/`-`: a struct operand is an operator overload (bone
            // records the target on `e`); a scalar uses the native op.
            Expr::Not(x, _) => match self.overload_target(e) {
                Some(func) => {
                    let value = self.lower_direct_call_repr(e, func, &[x], false)?;
                    self.pack_maybe_absent(value.into_value())
                }
                None => {
                    let v = self.lower_condition(x)?;
                    Ok(self.emit_cur(Inst::Not(v)))
                }
            },
            Expr::BitNot(x) => match self.overload_target(e) {
                Some(func) => {
                    let value = self.lower_direct_call_repr(e, func, &[x], false)?;
                    self.pack_maybe_absent(value.into_value())
                }
                None => {
                    let v = self.lower_expr(x)?;
                    Ok(self.emit_cur(if self.arith_ty(x) == Some(ArithTy::Uint) {
                        Inst::UintBitNot(v)
                    } else {
                        Inst::BitNot(v)
                    }))
                }
            },
            Expr::UnaryMinus(x, _) => match self.overload_target(e) {
                Some(func) => {
                    let value = self.lower_direct_call_repr(e, func, &[x], false)?;
                    self.pack_maybe_absent(value.into_value())
                }
                None => {
                    let v = self.lower_expr(x)?;
                    Ok(self.emit_cur(Inst::Neg(v)))
                }
            },

            Expr::BinOp {
                op: BinOp::NullCoalesce,
                lhs,
                ..
            } if self.expr_is_flat_enum(e) || self.nullable_value_shape_of(lhs)?.is_some() => {
                let r = self.lower_expr_repr(e)?.into_value();
                self.pack_maybe_absent(r)
            }
            Expr::BinOp { op, lhs, rhs } => self.lower_binop(op, lhs, rhs, condition),

            Expr::If {
                cond,
                then_branch,
                else_branch,
            } => self.lower_if(cond, then_branch, else_branch),

            // `break`/`continue` in expression position: emit a placeholder value
            // (never read — the block diverges) then a goto to the loop target.
            Expr::Break => {
                let (_, brk, depth) = self.loop_target("break")?;
                self.used_break_targets.insert(brk);
                self.run_defers_above(depth)?;
                let v = self.emit_cur(Inst::Const(Lit::Nil));
                self.f.set_term(self.cur, Term::Goto(brk));
                Ok(v)
            }
            Expr::Continue => {
                let (cont, _, depth) = self.loop_target("continue")?;
                self.run_defers_above(depth)?;
                let v = self.emit_cur(Inst::Const(Lit::Nil));
                self.f.set_term(self.cur, Term::Goto(cont));
                Ok(v)
            }

            Expr::Block { statements, tail } => {
                let repr = self.lower_block_repr(statements, tail)?;
                self.pack_maybe_absent(repr)
            }

            Expr::Call { callee, args, .. } => self.lower_call(e, callee, args),

            // An absent tuple slot must stay nil so a presence test can see it.
            Expr::Tuple(elems) => {
                let vs = self.lower_tuple_operands(e, elems)?;
                Ok(self.emit_cur(Inst::MakeTuple(vs)))
            }
            Expr::Array(elems) => self.lower_array_literal(e, elems),
            Expr::Index { base, index } => {
                // Snapshot a whole value before later stores through the container.
                if self.vec_flat_elem_of(base)?.is_some()
                    || (!self.scx.values_first_class
                        && (self.value_struct_of(e).is_some()
                            || self.nullable_place_value_of(e)?.is_some()))
                {
                    let r = self.lower_expr_repr(e)?.into_value();
                    return self.pack_maybe_absent(r);
                }
                let (b, kind) = self.lower_index_base(base)?;
                let i = self.lower_expr(index)?;
                Ok(self.read_index(b, i, kind))
            }
            // Route through the repr layer: a field read on a scalarized value
            // projects a leaf directly (no intermediate cdata); a boxed base
            // reads the field off the value as before.
            Expr::FieldAccess { .. } => {
                let r = self.lower_expr_repr(e)?.into_value();
                self.pack_maybe_absent(r)
            }
            Expr::TupleIndex { tuple, .. } => {
                // `w.0` on a transparent newtype is identity -- its runtime value
                // IS the inner, not a positional table -- so `TupleIndex` (a
                // `[1]` read) would index a scalar and yield nil.
                if self.expr_is_newtype(tuple) {
                    return self.lower_expr(tuple);
                }
                // Otherwise through the repr layer, same as `FieldAccess` above:
                // a splatting base projects its part instead of packing a table
                // and reading a boxed element back out.
                let r = self.lower_expr_repr(e)?.into_value();
                self.pack_maybe_absent(r)
            }
            // A value-struct literal is built via the repr layer (so a callsite
            // can splat it without a cdata); a plain struct is a Lua table.
            Expr::StructLiteral {
                name,
                fields,
                bases,
            } if self.is_value_struct(name) => {
                let r = self.value_literal_repr(name, fields, bases)?;
                self.pack_maybe_absent(r)
            }
            Expr::StructLiteral {
                name,
                fields,
                bases,
            } => self.lower_struct_literal(e, name, fields, bases),
            Expr::EnumVariant { enum_name, .. }
                if EnumPlan::for_name(self.ws, enum_name)
                    .is_some_and(|plan| plan.flat().is_some()) =>
            {
                let r = self.lower_expr_repr(e)?.into_value();
                self.pack_maybe_absent(r)
            }
            Expr::EnumVariant {
                enum_name,
                variant,
                args,
                ..
            } => self.lower_enum_variant(enum_name, variant, args),

            Expr::Assign { target, value } => {
                self.lower_assign_effect(target, value)?;
                Ok(self.emit_cur(Inst::Const(Lit::Nil)))
            }
            Expr::CompoundAssign {
                target,
                op_fn,
                value,
                ..
            } => {
                self.lower_compound_assign_effect(e, target, op_fn, value)?;
                Ok(self.emit_cur(Inst::Const(Lit::Nil)))
            }

            Expr::InterpolatedString(parts) => self.lower_interp(parts),

            // `yield e` (or bare `yield`): end the block with `Term::Yield`, then
            // continue in the resume block; the expression evaluates to the
            // resume arg (the value of the next `resume(arg)`).
            Expr::Yield(opt) => {
                if self.in_defer_body {
                    return Err(gap("`yield` inside a `defer` body is not allowed"));
                }
                // A flat-enum yield value threads as leaves through the frame-slot
                // channel (the transform writes them to reused `s.__y*` slots, 0
                // alloc); a scalar/non-flat value stays a single packed value.
                let values: Vec<Idx<Inst>> = match opt {
                    Some(eb) => match self.flat_enum_value_shape_of(eb)? {
                        Some(shape) => {
                            let r = self.lower_expr_repr(eb)?;
                            self.repr_to_storage_leaves(&r, &shape, "flat enum yield")?
                        }
                        None => vec![self.lower_expr(eb)?],
                    },
                    None => Vec::new(),
                };
                let resume = self.f.new_block();
                self.f.set_term(self.cur, Term::Yield { values, resume });
                self.cur = resume;
                let rac = self
                    .resume_arg
                    .ok_or_else(|| gap("yield outside a coroutine"))?;
                Ok(self.emit_cur(Inst::Load(rac)))
            }
            Expr::YieldDelegate(sub) => {
                if self.in_defer_body {
                    return Err(gap("`await` inside a `defer` body is not allowed"));
                }
                self.lower_yield_delegate(sub)
            }

            // A closure: lower its body into a sub-CFG in the shared arenas.
            // Captured outer vars are the enclosing cells (Lua upvalues); the
            // closure's params are fresh cells (its Lua arg list).
            Expr::Lambda { params, body } => {
                let lam_ty = self
                    .inf
                    .and_then(|inf| inf.expr_types.get(e))
                    .copied()
                    .ok_or_else(|| gap("lambda has no inferred function type"))?;
                let lam_plan = self
                    .fn_value_rep_plan(lam_ty)?
                    .ok_or_else(|| gap("lambda does not have a function representation"))?;
                self.lower_lambda_with_plan(e, params, body, lam_plan)
            }

            Expr::As {
                value,
                target,
                kind,
            } => {
                let v = self.lower_expr(value)?;
                let to_int = self
                    .concrete_ty_id(target)
                    .is_some_and(|ty| matches!(ty.peel_nullable().0, crate::monoty::MonoTy::Int));
                let from_float = self
                    .concrete_expr_ty(value)
                    .is_some_and(|ty| matches!(ty.peel_nullable().0, crate::monoty::MonoTy::Float));
                let to_uint = self
                    .concrete_ty_id(target)
                    .is_some_and(|ty| matches!(ty.peel_nullable().0, crate::monoty::MonoTy::Uint));
                let from_uint = self
                    .concrete_expr_ty(value)
                    .is_some_and(|ty| matches!(ty.peel_nullable().0, crate::monoty::MonoTy::Uint));
                let from_int = self
                    .concrete_expr_ty(value)
                    .is_some_and(|ty| matches!(ty.peel_nullable().0, crate::monoty::MonoTy::Int));
                let to_float = self
                    .concrete_ty_id(target)
                    .is_some_and(|ty| matches!(ty.peel_nullable().0, crate::monoty::MonoTy::Float));
                if kind == bone_core::hir::AsKind::Bitcast && to_int && from_uint {
                    Ok(self.emit_cur(Inst::UintBitcastInt(v)))
                } else if kind == bone_core::hir::AsKind::Bitcast && to_uint && from_int {
                    Ok(self.emit_cur(Inst::IntBitcastUint(v)))
                } else if kind == bone_core::hir::AsKind::Bitcast {
                    Err(CompilerError::new_with_label(
                        self.cur_span.unwrap_or_else(Span::unknown),
                        "bitcast requires an `int` and a `uint` operand/target".to_string(),
                    ))
                } else if to_int && from_float {
                    Ok(self.emit_cur(Inst::FloatToInt(v)))
                } else if to_uint && from_int {
                    Ok(self.emit_cur(Inst::IntToUint(v)))
                } else if to_int && from_uint {
                    Ok(self.emit_cur(Inst::UintToInt(v)))
                } else if to_uint && from_float {
                    Ok(self.emit_cur(Inst::FloatToUint(v)))
                } else if to_float && from_uint {
                    Ok(self.emit_cur(Inst::UintToFloat(v)))
                } else if to_float && from_int {
                    Ok(self.emit_cur(Inst::IntToFloat(v)))
                } else {
                    Ok(v)
                }
            }

            // `x?.field` -> `if x == nil then nil else x.field`, joined through a
            // result cell. A numeric field is a tuple read
            // (`x?.0`), a name is a struct/enum-record field.
            Expr::SafeNav { base, field } => {
                // Keep the evaluated receiver across representation selection.
                // A boxed fallback must not lower the source expression again.
                let recv = self.lower_scalar_safe_nav_receiver(e, base, field)?;
                self.safe_nav_scalar(recv, true, |s, recv| match &recv {
                    SafeNavRecv::Unboxed {
                        repr: ValueRepr::Group { name, parts, .. },
                        ..
                    } if s.field_index(*name, field).is_some() => {
                        let index = s.field_index(*name, field).expect("known value field");
                        s.pack(parts[index].clone())
                    }
                    _ => {
                        let bv = s.safe_nav_present_value(recv)?;
                        s.safe_nav_field_value(e, base, bv, field)
                    }
                })
            }

            Expr::Match { scrutinee, arms } => self.lower_match(scrutinee, arms),
            Expr::IfLet {
                pattern,
                value,
                then_branch,
                else_branch,
            } => {
                let repr = self.lower_if_let(pattern, value, then_branch, else_branch, None)?;
                self.pack(repr)
            }

            Expr::IncDec {
                target,
                decrement,
                postfix,
            } => self.lower_incdec(target, decrement, postfix),

            // A range used as a VALUE (not a `for` iterable — `lower_for`
            // intercepts that) materializes to the stdlib Vec-shaped `__range`.
            Expr::Range { low, high } => {
                let is_uint = matches!(self.arith_ty(low), Some(ArithTy::Uint))
                    || matches!(self.arith_ty(high), Some(ArithTy::Uint));
                let lo = self.lower_expr(low)?;
                let hi = self.lower_expr(high)?;
                let func =
                    self.resolve_global_fn(if is_uint { "__range_uint" } else { "__range" })?;
                Ok(self.emit_cur(Inst::Call {
                    func,
                    args: vec![lo, hi],
                }))
            }

            Expr::Return(opt) => {
                self.lower_return(opt)?;
                Ok(self.emit_cur(Inst::Const(Lit::Nil)))
            }

            Expr::SafeNavCall {
                receiver,
                method,
                args,
                ..
            } => self.lower_safe_nav_call(e, receiver, method, args),
            Expr::SafeNavLet {
                receiver,
                binding,
                body,
            } => self.lower_safe_nav_let(receiver, binding, body),

            other => Err(gap(format!(
                "expression form not in Phase 1: {}",
                expr_tag(&other)
            ))),
        }
    }

    /// The tuple's operand list is shared by materialization and a return
    /// destination. The tuple representation plan remains the layout oracle.
    pub(crate) fn lower_tuple_operands(
        &mut self,
        e: Idx<Expr>,
        elems: comfy_la_arena::IdxRange<Expr>,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        let outer = self.cur_span.replace(self.body.expr_spans[e]);
        let outer_expr = self.cur_expr.replace(e);
        let result = (|| {
            // A tuple box uses the same slot order as the multi-return ABI.
            let shape = self.tuple_box_shape(e)?;
            let mut vs = Vec::new();
            for (i, el) in elems.into_iter().enumerate() {
                let bundle = self.lower_expr_repr(el)?;
                match shape
                    .as_ref()
                    .and_then(|s| s.slots.get(i))
                    .map(|&id| self.scx.rep_arena().layout(id))
                {
                    Some(slot) if slot.splats() => {
                        let vals = self.repr_to_slot_values_bundle(&bundle, slot)?;
                        vs.extend(vals);
                    }
                    _ => {
                        let outer_expr = self.cur_expr.replace(el);
                        let packed = self.pack_maybe_absent(bundle.into_value());
                        self.cur_expr = outer_expr;
                        vs.push(packed?);
                    }
                }
            }
            Ok(vs)
        })();
        self.cur_span = outer;
        self.cur_expr = outer_expr;
        result
    }

    pub(crate) fn check_ref_holder(&self, e: Idx<Expr>) -> Result<(), CompilerError> {
        if let Expr::FieldAccess { base, field } = self.body.expr_arena[e] {
            if let Some(TypeExpr::App {
                ctor: TypeCtor::Nominal(owner),
                ..
            }) = self.field_base_ty(base)
            {
                if self
                    .struct_field_plan(owner, field, false)?
                    .inline_child()
                    .is_some()
                {
                    return Err(CompilerError::new_with_label(
                        self.body.expr_spans[e],
                        "cannot borrow an inline value field: its leaves have no holder to alias; \
                         borrow the containing value or copy the field"
                            .to_string(),
                    ));
                }
            }
        }
        Ok(())
    }

    /// Mark the root cell so lowering preserves aliasing.
    pub(crate) fn lower_ref_borrow(&mut self, e: Idx<Expr>) -> Result<Idx<Inst>, CompilerError> {
        if let Some(vec) = self.capture_flat_ref_arg(e)? {
            return Ok(vec);
        }
        if let Some(key) = self.classify_flat_ref(e)? {
            return Err(CompilerError::new_with_label(
                self.body.expr_spans[e],
                format!(
                    "a `ref` to a flat-stored `{}` element can be passed to a `ref` parameter of \
                     a function, bound with `let` or returned; it has no table to alias here",
                    key.elem.name
                ),
            ));
        }
        self.check_ref_holder(e)?;
        let place = self.lower_place_base(e)?;
        let mut cur = place;
        while let Some((base, _)) = self.f.field_projection(cur) {
            cur = base;
        }
        if let Inst::Load(cell) = self.f.insts[cur] {
            self.f.mark_borrowed(cell);
        }
        Ok(place)
    }

    /// Find the leaf cells spanned by a projected field.
    fn group_leaf_place(&self, e: Idx<Expr>) -> Option<(Vec<GroupLeaf>, FieldShape)> {
        match self.group_leaf_walk(e)? {
            (guards, cells, selected) if guards.is_empty() => Some((cells, selected)),
            _ => None,
        }
    }

    /// [`Self::group_leaf_place`] through `?.` links too: also the presence
    /// cells of the nullable levels those links test, outermost first.
    fn group_leaf_walk(
        &self,
        e: Idx<Expr>,
    ) -> Option<(Vec<GroupLeaf>, Vec<GroupLeaf>, FieldShape)> {
        let mut links = Vec::new();
        let mut at = e;
        let b = loop {
            match self.body.expr_arena[at] {
                Expr::FieldAccess { base, field } if self.swizzle_of(base, field).is_none() => {
                    links.push((field, false));
                    at = base;
                }
                Expr::SafeNav { base, field } => {
                    links.push((field, true));
                    at = base;
                }
                Expr::Var(b) if self.group_binds.contains_key(&raw(b)) => break b,
                _ => return None,
            }
        };
        if links.is_empty() {
            return None;
        }
        links.reverse();
        let (cells, shape) = &self.group_binds[&raw(b)];
        let path: Vec<Ustr> = links.iter().map(|(field, _)| *field).collect();
        let mut guards = Vec::new();
        for (depth, &(_, safe)) in links.iter().enumerate() {
            if !safe {
                continue;
            }
            let nullable = if depth == 0 {
                shape.nullable
            } else {
                match self.value_projection_range(shape, &path[..depth])? {
                    (range, FieldShape::Nested(level)) => {
                        level.nullable && {
                            guards.push(cells[range.start]);
                            false
                        }
                    }
                    // A `?.` off an opaque child tests a box, not a leaf.
                    (_, FieldShape::Prim) => return None,
                }
            };
            if nullable {
                guards.push(cells[0]);
            }
        }
        let (range, selected) = self.value_projection_range(shape, &path)?;
        Some((guards, cells[range].to_vec(), selected))
    }

    /// Allow reference writes through a leaf, but reject aliasing a shared value box.
    fn group_leaf_place_base(
        &mut self,
        e: Idx<Expr>,
        leaf: GroupLeaf,
    ) -> Result<Idx<Inst>, CompilerError> {
        let GroupLeaf::Cell(cell) = leaf else {
            // Nothing writes through a binding held as values.
            return Ok(self.read_leaf(leaf));
        };
        let leaf = self.emit_cur(Inst::Load(cell));
        match self.concrete_expr_ty(e) {
            Some(ty)
                if ty
                    .peel_nullable()
                    .0
                    .nominal()
                    .is_some_and(|name| self.is_value_struct(name)) =>
            {
                let copy = self.copied_value_box(ty, leaf)?;
                self.store_cell(cell, copy);
                // Through a reload, so copy propagation sees the cell written
                // in place and does not fold reads to the copy's operands.
                Ok(self.emit_cur(Inst::Load(cell)))
            }
            _ => Ok(leaf),
        }
    }

    /// Branch to `absent` unless every presence cell in `guards` is set.
    fn group_presence_guard(&mut self, guards: &[GroupLeaf], absent: Idx<Block>) {
        for &leaf in guards {
            let flag = self.read_leaf(leaf);
            let present = self.f.new_block();
            self.branch_to(flag, present, absent);
            self.cur = present;
        }
    }

    /// Under first-class values (GLSL) a field write rooted at a value local
    /// or parameter, through value projections only: the root's cell and the
    /// path with each level's struct name.
    fn first_class_field_place(&self, e: Idx<Expr>) -> Option<(Idx<Cell>, Vec<(Ustr, Ustr)>)> {
        if !self.scx.values_first_class {
            return None;
        }
        let mut path = Vec::new();
        let mut at = e;
        let b = loop {
            match self.body.expr_arena[at] {
                Expr::FieldAccess { base, field } if self.swizzle_of(base, field).is_none() => {
                    path.push((self.value_struct_of(base)?, field));
                    at = base;
                }
                Expr::Var(b) if !self.static_cells.contains_key(&raw(b)) => break b,
                _ => return None,
            }
        };
        if path.is_empty() {
            return None;
        }
        path.reverse();
        Some((*self.binds.get(&raw(b))?, path))
    }

    /// Rebuild each first-class value along the field path.
    fn write_first_class_field(
        &mut self,
        target: Idx<Expr>,
        cell: Idx<Cell>,
        path: &[(Ustr, Ustr)],
        write: Write<'a, '_>,
    ) -> Result<Option<Idx<Inst>>, CompilerError> {
        let mut levels = vec![self.emit_cur(Inst::Load(cell))];
        for &(_, field) in &path[..path.len() - 1] {
            let base = *levels.last().expect("root level");
            levels.push(self.read_field(base, field));
        }
        let (_, last) = path[path.len() - 1];
        let base = *levels.last().expect("root level");
        let old = self.read_field(base, last);
        let (mut value, result) = match write {
            Write::Assign(rhs) => {
                let expected = self.concrete_expr_ty(target);
                (self.lower_expr_with_expected(rhs, expected)?, None)
            }
            Write::Update { postfix, apply } => {
                let new = apply(self, old)?;
                (new, Some(if postfix { old } else { new }))
            }
        };
        for (&(name, field), &base) in path.iter().zip(&levels).rev() {
            let parts = self
                .struct_field_names_required(name)?
                .into_iter()
                .map(|f| {
                    ValueRepr::Scalar(if f == field {
                        value
                    } else {
                        self.read_field(base, f)
                    })
                })
                .collect();
            value = self.pack(ValueRepr::Group {
                name,
                parts,
                present: None,
            })?;
        }
        // GLSL renders a value at its use, so a result read before the store
        // but used after it would see the new field. Snapshot it in a cell.
        let snapshot = result.map(|result| {
            let snap = self.f.new_cell(None);
            self.store_cell(snap, result);
            snap
        });
        self.store_cell(cell, value);
        Ok(snapshot.map(|snap| self.emit_cur(Inst::Load(snap))))
    }

    fn write_group_field(
        &mut self,
        target: Idx<Expr>,
        cells: &[GroupLeaf],
        shape: FieldShape,
        write: Write<'a, '_>,
    ) -> Result<Option<Idx<Inst>>, CompilerError> {
        match shape {
            FieldShape::Prim => self.write_scalar(
                Place::Cell(cells[0].cell()),
                self.concrete_expr_ty(target),
                write,
            ),
            FieldShape::Nested(shape) => {
                let Write::Assign(value) = write else {
                    unreachable!("scalar update of aggregate storage");
                };
                let repr = self.lower_expr_repr(value)?;
                let leaves =
                    self.repr_to_storage_leaves(&repr, &shape, "scalarized field assignment")?;
                for (leaf, value) in cells.iter().zip(leaves) {
                    self.store_cell(leaf.cell(), value);
                }
                Ok(None)
            }
        }
    }

    fn read_place(&mut self, place: &Place) -> Result<Idx<Inst>, CompilerError> {
        Ok(match *place {
            Place::Cell(cell) => self.emit_cur(Inst::Load(cell)),
            Place::Global(name) => self.emit_cur(Inst::Global(name)),
            Place::Field(base, key) => self.read_field(base, key),
            Place::Index(base, index, kind) => self.read_index(base, index, kind),
            Place::Flat(place) => self.flat_leaf_load(place)?,
            Place::Native(buf, slot, ref addr) => self.nb_leaf_load(buf, slot, addr),
        })
    }

    fn write_place(&mut self, place: &Place, value: Idx<Inst>) -> Result<(), CompilerError> {
        match *place {
            Place::Cell(cell) => {
                self.store_cell(cell, value);
            }
            Place::Global(name) => {
                self.emit_cur(Inst::StoreGlobal { name, value });
            }
            Place::Field(base, key) => {
                self.write_field(base, key, value);
            }
            Place::Index(base, index, kind) => {
                self.write_index(base, index, value, kind);
            }
            Place::Flat(place) => self.flat_leaf_store(place, value)?,
            Place::Native(buf, slot, ref addr) => self.nb_leaf_store(buf, slot, addr, value),
        }
        Ok(())
    }

    fn write_scalar(
        &mut self,
        place: Place,
        expected: Option<&MonoTy>,
        write: Write<'a, '_>,
    ) -> Result<Option<Idx<Inst>>, CompilerError> {
        let (value, result) = match write {
            Write::Assign(rhs) => (self.lower_expr_with_expected(rhs, expected)?, None),
            Write::Update { postfix, apply } => {
                let old = self.read_place(&place)?;
                let new = apply(self, old)?;
                (new, Some(if postfix { old } else { new }))
            }
        };
        self.write_place(&place, value)?;
        Ok(result)
    }

    fn index_place(
        &mut self,
        base: Idx<Expr>,
        index: Idx<Expr>,
    ) -> Result<(Idx<Inst>, Idx<Inst>, ContainerKind), CompilerError> {
        let (base, kind) = self.lower_index_base(base)?;
        let index = self.lower_expr(index)?;
        Ok((base, index, kind))
    }

    fn field_plan(
        &self,
        base: Idx<Expr>,
        field: Ustr,
        spliced: bool,
    ) -> Result<Option<StructFieldPlan>, CompilerError> {
        match self.field_base_ty(base) {
            Some(TypeExpr::App {
                ctor: TypeCtor::Nominal(name),
                ..
            }) if self.swizzle_of(base, field).is_none() => {
                self.struct_field_plan(name, field, spliced).map(Some)
            }
            _ => Ok(None),
        }
    }

    fn physical_field_place(
        &mut self,
        base: Idx<Expr>,
        field: Ustr,
        absent: Option<Idx<Block>>,
    ) -> Result<(Idx<Inst>, Ustr, Option<ValueShape>), CompilerError> {
        let (holder, key, shape, _) = self.physical_field_layout(base, field)?;
        let owner = self.lower_place_base_with_guard(holder, absent)?;
        Ok((owner, key, shape))
    }

    pub(crate) fn lower_place_base(&mut self, e: Idx<Expr>) -> Result<Idx<Inst>, CompilerError> {
        self.lower_place_base_with_guard(e, None)
    }

    fn lower_place_base_with_guard(
        &mut self,
        e: Idx<Expr>,
        absent: Option<Idx<Block>>,
    ) -> Result<Idx<Inst>, CompilerError> {
        let value = self.read_place_base(e, absent)?;
        self.mark_record_base(e, value);
        Ok(value)
    }

    fn guard_place_receiver(&mut self, recv: Idx<Inst>, absent: Idx<Block>) {
        let present = self.f.new_block();
        self.branch_on_nil(recv, absent, present);
        self.cur = present;
    }

    fn read_place_base(
        &mut self,
        e: Idx<Expr>,
        absent: Option<Idx<Block>>,
    ) -> Result<Idx<Inst>, CompilerError> {
        let syntax = &self.body.expr_arena[e];
        if let (Some(join), &Expr::SafeNav { base, field }) = (absent, syntax) {
            if let Some((guards, cells, FieldShape::Prim)) = self.group_leaf_walk(e) {
                self.group_presence_guard(&guards, join);
                return self.group_leaf_place_base(e, cells[0]);
            }
            let recv = self.lower_place_base_with_guard(base, absent)?;
            self.guard_place_receiver(recv, join);
            return self.read_place(&Place::Field(recv, field));
        }
        if let Expr::FieldAccess { base, field } = *syntax {
            let flat = self
                .field_plan(base, field, false)?
                .and_then(|plan| plan.flat_shape());
            let direct = self.swizzle_of(base, field).is_none() && flat.is_none();
            if absent.is_none() || !direct {
                if let Some((cells, FieldShape::Prim)) = self.group_leaf_place(e) {
                    return self.group_leaf_place_base(e, cells[0]);
                }
                if self.flat_leaf_path(base, field)?.is_some() {
                    return self.lower_expr(e);
                }
            }
            if direct {
                let (owner, key, _) = self.physical_field_place(base, field, absent)?;
                return self.read_place(&Place::Field(owner, key));
            }
        }
        match *syntax {
            // Element stores, leaf places and `ref` borrows of flat storage are
            // decomposed before this ([`flat_ref`]); a flat element has no
            // table to name as a place.
            Expr::Index { base, .. } if self.flat_stored_container(base)? => {
                Err(CompilerError::new_with_label(
                    self.body.expr_spans[e],
                    "this container element is stored as flat columns or rows and has no table \
                     to use as a place; read or write its fields, or pass it to a `ref` parameter"
                        .to_string(),
                ))
            }
            Expr::Index { base, index } => {
                let (base, index, kind) = self.index_place(base, index)?;
                self.read_place(&Place::Index(base, index, kind))
            }
            Expr::Global(name) if self.nobox_global_place(e)?.is_some() => {
                Err(CompilerError::new_with_label(
                    self.body.expr_spans[e],
                    format!(
                        "`{name}` is a `nobox` global, which is stored as leaves and has no \
                         slot this assignment target can write through"
                    ),
                ))
            }
            Expr::Global(name) => self.read_place(&Place::Global(name)),
            Expr::Var(b) if self.static_cells.contains_key(&raw(b)) => {
                self.read_place(&Place::Global(self.static_cells[&raw(b)]))
            }
            Expr::Var(b)
                if self.boxed_value_local(b, e).is_some()
                    || self.nullable_boxed_value_local(b, e).is_some() =>
            {
                self.read_place(&Place::Cell(self.binds[&raw(b)]))
            }
            // Reject a value temporary as an assignment base.
            Expr::Call { .. } | Expr::SafeNavCall { .. } if self.expr_is_value_struct(e) => {
                Err(CompilerError::new_with_label(
                    self.body.expr_spans[e],
                    "assignment target's base is a `value` call result, which is not storage: \
                     the write would land on a temporary and be discarded"
                        .to_string(),
                ))
            }
            _ => self.lower_expr(e),
        }
    }

    fn write_guarded_field(
        &mut self,
        target: Idx<Expr>,
        base: Idx<Expr>,
        field: Ustr,
        write: Write<'a, '_>,
    ) -> Result<(), CompilerError> {
        let join = self.f.new_block();
        if let Some((guards, cells, shape)) = self.group_leaf_walk(target)
            && (matches!(write, Write::Assign(_)) || matches!(shape, FieldShape::Prim))
        {
            self.group_presence_guard(&guards, join);
            self.write_group_field(target, &cells, shape, write)?;
        } else {
            let recv = self.lower_place_base_with_guard(base, Some(join))?;
            self.guard_place_receiver(recv, join);
            self.write_scalar(Place::Field(recv, field), None, write)?;
        }
        self.f.set_term(self.cur, Term::Goto(join));
        self.cur = join;
        Ok(())
    }

    /// `place.f = v` on a stdlib descriptor. Copies share its one Lua table, so
    /// the write builds a fresh descriptor and stores it back into `place`.
    fn descriptor_field_store(
        &mut self,
        target: Idx<Expr>,
        place: Idx<Expr>,
        field: Ustr,
        value: Idx<Expr>,
    ) -> Result<(), CompilerError> {
        let owner = match self.body.expr_arena[place] {
            Expr::FieldAccess { base, field: slot } => {
                let (base, key, _) = self.physical_field_place(base, slot, None)?;
                Place::Field(base, key)
            }
            Expr::Global(name) => Place::Global(name),
            Expr::Var(b) if self.static_cells.contains_key(&raw(b)) => {
                Place::Global(self.static_cells[&raw(b)])
            }
            _ => {
                return Err(self.gap_here(
                    "a descriptor field write must name the field or global holding the \
                     descriptor; replace it wholesale instead",
                ));
            }
        };
        let old = self.read_place(&owner)?;
        let key = self.emit_cur(Inst::Const(Lit::Str(field)));
        let v = self.lower_expr(value)?;
        // The write's target names the site, which a statement-position
        // assignment does not otherwise set.
        let outer_expr = self.cur_expr.replace(target);
        let fresh = self.call_spine("__descriptor_with", vec![old, key, v]);
        self.cur_expr = outer_expr;
        let fresh = fresh?;
        self.write_place(&owner, fresh)
    }

    fn write_field_target(
        &mut self,
        target: Idx<Expr>,
        base: Idx<Expr>,
        field: Ustr,
        globals: Option<(Vec<Ustr>, Option<ValueShape>)>,
        write: Write<'a, '_>,
    ) -> Result<Option<Idx<Inst>>, CompilerError> {
        if let Some((cell, path)) = self.first_class_field_place(target) {
            return self.write_first_class_field(target, cell, &path, write);
        }
        if let Some((cells, shape)) = self.group_leaf_place(target)
            && (matches!(write, Write::Assign(_)) || matches!(shape, FieldShape::Prim))
        {
            return self.write_group_field(target, &cells, shape, write);
        }
        match write {
            Write::Assign(value) => {
                if let Some((leaves, shape)) = globals {
                    self.nobox_global_store(target, &leaves, shape, value)?;
                    return Ok(None);
                }
                if !self.scx.values_first_class
                    && matches!(
                        self.field_base_ty(base),
                        Some(TypeExpr::App { ctor: TypeCtor::Nominal(name), .. })
                            if bone_core::modules::is_descriptor_key(name)
                    )
                {
                    self.descriptor_field_store(target, base, field, value)?;
                    return Ok(None);
                }
            }
            // JS updates the typed-array leaf. Lua can update the FFI struct slot.
            Write::Update { .. } if self.scx.target == bone_core::hir::FnBackend::Js => {
                if let Some((buf, slot, elem, path)) = self.nb_leaf_field_place(base, field)?
                    && let Some(addr) = self.nb_leaf_address(elem, &path)?
                {
                    return self.write_scalar(Place::Native(buf, slot, addr), None, write);
                }
            }
            _ => {}
        }
        let flat = self.flat_field_path(base, field)?;
        let flat = match write {
            Write::Assign(_) => flat.or(Some((base, field, None))),
            Write::Update { .. } => flat.filter(|(_, _, shape)| shape.is_none()),
        };
        if let Some((root, path, shape)) = flat
            && let Some(place) = self.flat_root_place(root, path)?
        {
            return match write {
                Write::Assign(value) => {
                    self.flat_field_store(place, shape.as_ref(), value)?;
                    Ok(None)
                }
                Write::Update { .. } => self.write_scalar(Place::Flat(place), None, write),
            };
        }
        if let Write::Assign(value) = write {
            if let Some((buf, slot, elem, path)) = self.nb_leaf_field_place(base, field)? {
                self.nb_leaf_field_store(buf, slot, elem, &path, value)?;
            } else {
                self.store_physical_field(target, base, field, value)?;
            }
            Ok(None)
        } else {
            let (base, key, _) = self.physical_field_place(base, field, None)?;
            self.write_scalar(Place::Field(base, key), None, write)
        }
    }

    pub(crate) fn lower_assign_effect(
        &mut self,
        target: Idx<Expr>,
        value: Idx<Expr>,
    ) -> Result<(), CompilerError> {
        self.with_value_lending(false, |s| s.lower_assign_effect_owned(target, value))
    }

    fn lower_assign_effect_owned(
        &mut self,
        target: Idx<Expr>,
        value: Idx<Expr>,
    ) -> Result<(), CompilerError> {
        if self.assign_target_is_f32(target) {
            self.f32_round_exprs.insert(value);
        }
        if let Some(unsigned) = self.assign_integer_field_unsigned(target) {
            self.integer_field_checks.insert(value, unsigned);
        }
        let nobox_global_place = self.nobox_global_place(target)?;
        match self.body.expr_arena[target] {
            // A threaded (group_binds) var -- a scalarized flat-enum local or value
            // param -- updates its leaf cells, not one boxed cell.
            Expr::Var(b) if self.group_binds.contains_key(&raw(b)) => {
                let (cells, shape) = self.group_binds.get(&raw(b)).cloned().unwrap();
                let r = self.lower_expr_repr(value)?;
                let leaves =
                    self.repr_to_storage_leaves(&r, &shape, "scalarized local assignment")?;
                for (c, lv) in cells.iter().zip(leaves) {
                    self.store_cell(c.cell(), lv);
                }
            }
            // The tuple twin: a name bound to a splatting tuple's WIRE updates its
            // slot cells (presence included), never one boxed cell.
            Expr::Var(b) if self.tuple_binds.contains_key(&raw(b)) => {
                let (cells, shape) = self.tuple_binds.get(&raw(b)).cloned().unwrap();
                let r = self.lower_expr_repr(value)?;
                let owned = self.owned_value_children(r.into_value())?;
                let slots = self.tuple_repr_to_slots(owned, &shape)?;
                for (c, sv) in cells.iter().zip(slots) {
                    self.store_cell(*c, sv);
                }
            }
            Expr::Var(b) if self.static_cells.contains_key(&raw(b)) => {
                let name = self.static_cells[&raw(b)];
                let v = self.lower_expr(value)?;
                self.write_place(&Place::Global(name), v)?;
            }
            // All uses of a flat reference need the same storage layout.
            Expr::Var(b) if self.flat_ref_binds.contains_key(&raw(b)) => {
                let key = self.flat_ref_binds[&raw(b)];
                if self.classify_flat_ref(value)? != Some(key) {
                    return Err(CompilerError::new_with_label(
                        self.body.expr_spans[target],
                        "cannot reassign a `ref` to a flat-stored container element on the Lua \
                         backend except to another element stored the same way; assign its \
                         fields, or copy it into a local first"
                            .to_string(),
                    ));
                }
                let val = self.lower_flat_ref(value)?;
                let (vec, slot) = *self
                    .flat_ref_cells
                    .get(&raw(b))
                    .ok_or_else(|| self.gap_here("flat `ref` binding has no cells"))?;
                self.store_cell(vec, val.vec);
                self.store_cell(slot, val.slot);
            }
            Expr::Var(b) => {
                let cell = *self
                    .binds
                    .get(&raw(b))
                    .ok_or_else(|| gap("assignment to an unbound variable"))?;
                // The boxed binding's cell can only hold a copy of a flat
                // element, which would silently detach later writes from it.
                if self.binding_is_ref(b) && self.classify_flat_ref(value)?.is_some() {
                    return Err(CompilerError::new_with_label(
                        self.body.expr_spans[target],
                        "cannot reassign this `ref` to a flat-stored container element on the \
                         Lua backend: it was bound to an element stored differently; assign its \
                         fields, or copy it into a local first"
                            .to_string(),
                    ));
                }
                let expected = self.binding_ty(b).and_then(|ty| self.concrete_ty_id(ty));
                let v = if self.binding_is_ref(b) {
                    self.lower_ref_borrow(value)?
                } else {
                    self.lower_expr_with_expected(value, expected)?
                };
                let v = self.coerced_to_binding(v, value, b);
                // An owned binding stays owned: a box it did not build is copied.
                let v = match self.value_struct_of(value) {
                    Some(name)
                        if self.owned_value_binds.contains(&raw(b))
                            && !matches!(self.f.insts[v], Inst::MakeValueStruct { .. }) =>
                    {
                        let copy =
                            self.boxed_value_copy_repr(v, name, self.concrete_expr_ty(value))?;
                        self.pack(copy)?
                    }
                    _ => v,
                };
                self.write_place(&Place::Cell(cell), v)?;
            }
            Expr::FieldAccess { base, field } => {
                self.write_field_target(
                    target,
                    base,
                    field,
                    nobox_global_place,
                    Write::Assign(value),
                )?;
            }
            // The synthesized store call belongs to the target expression.
            Expr::Index { base, index } => self.with_site_expr(target, |s| {
                let (b, i, kind) = s.index_place(base, index)?;
                if let Some((elem, storage)) = FlatStorage::of_vec_kind(kind) {
                    return s.flat_element_store(b, i, value, elem, storage);
                }
                // A column-stored Map has no slot at the key either: the write
                // goes through the spine to a slot, then leaf-wise into `data`.
                if let Some(elem) = s.map_column_value_of(base)? {
                    return s.map_column_store(b, i, value, elem);
                }
                if let ContainerKind::NativeBufferValue(elem) = kind {
                    if s.is_leaf_addressed(elem)? {
                        return s.nb_leaf_element_store(b, i, value, elem);
                    }
                }
                let v = s.lower_expr(value)?;
                s.write_place(&Place::Index(b, i, kind), v)
            })?,
            Expr::Global(_) if let Some((leaves, shape)) = nobox_global_place => {
                self.nobox_global_store(target, &leaves, shape, value)?;
            }
            Expr::Global(name) => {
                let v = self.lower_expr(value)?;
                self.write_place(&Place::Global(name), v)?;
            }
            Expr::SafeNav { base, field } => {
                self.write_guarded_field(target, base, field, Write::Assign(value))?;
            }
            _ => return Err(gap("assignment to this target form (later)")),
        }
        Ok(())
    }

    fn store_physical_field(
        &mut self,
        target: Idx<Expr>,
        base: Idx<Expr>,
        field: Ustr,
        value: Idx<Expr>,
    ) -> Result<(), CompilerError> {
        // A whole embedded field (`p.f = Foo { .. }`): the child has no
        // slot of its own, so write its leaves. Composition happens
        // BEFORE this, so `p.f` here is the child, not a physical key.
        let embed_plan = self.field_plan(base, field, false)?;
        if let Some((cname, nullable)) = embed_plan.as_ref().and_then(|p| p.embed()) {
            let (b, key, _) = self.physical_field_place(base, field, None)?;
            self.store_embed(b, key, cname, nullable, value)?;
            return Ok(());
        }
        // An embed LEAF write composes to one dotted key on the parent
        // that holds it (`h.p.x = 9` -> `h["p.x"] = 9`).
        let (b, key, projected_shape) = self.physical_field_place(base, field, None)?;
        if let Some(sname) = self.leaf_table_base(base)?
            && self.leaf_table_value_child(sname, field)?.is_some()
        {
            return self.store_leaf_table_child(b, sname, field, key, value);
        }
        if let Some(shape) = projected_shape {
            let repr = self.lower_expr_repr(value)?;
            let leaves = self.repr_to_leaves_bundle_for_shape(&repr, &shape)?;
            let keys = super::shape::leaf_names(&shape, key.as_str(), ".");
            for (slot, value) in Self::pair_embed_leaves(
                keys.into_iter()
                    .map(|key| crate::names::ustr(&key))
                    .collect(),
                leaves,
            )? {
                self.write_field(b, slot, value);
            }
            return Ok(());
        }
        if key != field
            && let Some((cname, nullable)) = self
                .field_plan(base, field, true)?
                .and_then(|plan| plan.embed())
        {
            let leaves = self.embed_value_leaves(nullable, cname, value)?;
            for (slot, v) in Self::embed_field_slots(key, leaves) {
                self.write_field(b, slot, v);
            }
            return Ok(());
        }
        // A flattened enum field: write the discriminant + payload slots
        // directly on the parent table (no nested box).
        if let Some(shape) = embed_plan.and_then(|p| p.flat_shape()) {
            let cells = self.lower_flat_value(&shape, value)?;
            self.store_flat_cells(b, key, cells);
            return Ok(());
        }
        // A whole `nobox` child that is not inline lives nested in its
        // cdata parent: write its leaves through the stored child, the
        // inverse of `cdata_leaves`, rather than box a copy to store.
        if !self.scx.values_first_class
            && let Some(cname) = self.value_struct_of(target)
            && self.is_nobox_struct(cname)
        {
            let shape = self.nominal_rep_shape_required(cname, false)?;
            if self.shape_is_plain_keys(&shape) {
                let r = self.lower_expr_repr(value)?;
                let leaves = self.repr_to_leaves_bundle_for_shape(&r, &shape)?;
                let paths = self.boxed_leaf_paths(&shape, true)?;
                if paths.len() != leaves.len() {
                    return Err(self.gap_here(format!(
                        "`{cname}` has {} leaf paths but the value produced {} leaves",
                        paths.len(),
                        leaves.len()
                    )));
                }
                let child = self.read_field(b, key);
                for (path, leaf) in paths.into_iter().zip(leaves) {
                    let (last, walk) = path.split_last().ok_or_else(|| {
                        self.gap_here(format!("`{cname}` has an empty leaf path"))
                    })?;
                    let mut at = child;
                    for seg in walk {
                        at = self.read_field(at, *seg);
                    }
                    self.write_field(at, *last, leaf);
                }
                return Ok(());
            }
        }
        let expected = self.concrete_expr_ty(target);
        let v = self.lower_expr_with_expected(value, expected)?;
        self.write_place(&Place::Field(b, key), v)
    }

    pub(crate) fn assign_target_is_f32(&self, target: Idx<Expr>) -> bool {
        let (Expr::FieldAccess { base, field } | Expr::SafeNav { base, field }) =
            self.body.expr_arena[target]
        else {
            return false;
        };
        let Some(TypeExpr::App {
            ctor: TypeCtor::Nominal(sname),
            ..
        }) = self.field_base_ty(base)
        else {
            return false;
        };
        self.field_is_f32(sname, field)
    }

    fn assign_integer_field_unsigned(&self, target: Idx<Expr>) -> Option<bool> {
        if !crate::verify::checks_enabled() {
            return None;
        }
        let (Expr::FieldAccess { base, .. } | Expr::SafeNav { base, .. }) =
            self.body.expr_arena[target]
        else {
            return None;
        };
        let mut ty = self.concrete_expr_ty(base)?.peel_nullable().0;
        if let MonoTy::App {
            ctor: crate::monoty::MonoCtor::Ref,
            params,
        } = ty
        {
            ty = params.first()?.peel_nullable().0;
        }
        if !self.is_value_struct(ty.nominal()?) {
            return None;
        }
        Self::integer_field_unsigned(self.concrete_expr_ty(target)?)
    }

    pub(crate) fn integer_field_unsigned(ty: &MonoTy) -> Option<bool> {
        match ty.peel_nullable().0 {
            MonoTy::Int => Some(false),
            MonoTy::Uint => Some(true),
            _ => None,
        }
    }

    pub(super) fn check_field_value(&mut self, e: Idx<Expr>, mut value: Idx<Inst>) -> Idx<Inst> {
        if self.f32_round_exprs.contains(&e) {
            value = self.emit_cur(Inst::FloatToF32(value));
        }
        self.check_integer_field(value, self.integer_field_checks.get(&e).copied())
    }

    pub(crate) fn check_integer_field(
        &mut self,
        value: Idx<Inst>,
        unsigned: Option<bool>,
    ) -> Idx<Inst> {
        // Shader lowering uses Any and already has native integer storage.
        let Some(unsigned) = unsigned.filter(|_| {
            crate::verify::checks_enabled() && self.scx.target != bone_core::hir::FnBackend::Any
        }) else {
            return value;
        };
        let callee = self.emit_cur(Inst::Global(crate::names::ustr(
            crate::mir::VALUE_INTEGER_CHECK,
        )));
        let unsigned = self.emit_cur(Inst::Const(Lit::Bool(unsigned)));
        self.emit_cur(Inst::CallValue {
            callee,
            args: vec![value, unsigned],
        })
    }

    pub(crate) fn lower_compound_assign_effect(
        &mut self,
        e: Idx<Expr>,
        target: Idx<Expr>,
        op_fn: Ustr,
        value: Idx<Expr>,
    ) -> Result<(), CompilerError> {
        self.with_value_lending(false, |s| {
            s.lower_compound_assign_owned(e, target, op_fn, value)
        })
    }

    fn lower_compound_assign_owned(
        &mut self,
        e: Idx<Expr>,
        target: Idx<Expr>,
        op_fn: Ustr,
        value: Idx<Expr>,
    ) -> Result<(), CompilerError> {
        let f32_target = self.assign_target_is_f32(target);
        let scalar = self.arith_ty(target).zip(self.arith_ty(value));
        // Non-scalar: resolve the operator overload once (overload target on the
        // expr, else the named op fn).
        let overload = if scalar.is_none() {
            Some(match self.overload_target(e) {
                Some(f) => f,
                None => self.resolve_funcref(e, op_fn)?,
            })
        } else {
            None
        };
        let combine = |s: &mut Self, cur| -> Result<Idx<Inst>, CompilerError> {
            let mut new = s.compound_combine(e, scalar, op_fn, overload, cur, value)?;
            if f32_target {
                new = s.emit_cur(Inst::FloatToF32(new));
            }
            Ok(s.check_integer_field(new, s.assign_integer_field_unsigned(target)))
        };
        let Expr::SafeNav { base, field } = self.body.expr_arena[target] else {
            unreachable!("compound assignment without a top-level safe navigation");
        };
        self.write_guarded_field(
            target,
            base,
            field,
            Write::Update {
                postfix: false,
                apply: &combine,
            },
        )
    }

    pub(crate) fn compound_combine(
        &mut self,
        e: Idx<Expr>,
        scalar: Option<(ArithTy, ArithTy)>,
        op_fn: Ustr,
        overload: Option<FuncRef>,
        cur: Idx<Inst>,
        rhs: Idx<Expr>,
    ) -> Result<Idx<Inst>, CompilerError> {
        match scalar {
            Some((lt, rt)) => {
                let rv = self.lower_expr(rhs)?;
                let inst = arith_inst(op_fn.as_str(), lt, rt, cur, rv)?;
                Ok(self.emit_cur(inst))
            }
            None => {
                let func = overload.expect("non-scalar compound-assign resolved an overload");
                let value = self.ufcs_call_repr(e, func, cur, &[rhs])?;
                self.pack_maybe_absent(value.into_value())
            }
        }
    }

    pub(crate) fn lower_incdec(
        &mut self,
        target: Idx<Expr>,
        decrement: bool,
        postfix: bool,
    ) -> Result<Idx<Inst>, CompilerError> {
        let nobox_global_place = self.nobox_global_place(target)?;
        let f32_target = self.assign_target_is_f32(target);
        let ty = self
            .arith_ty(target)
            .filter(|t| matches!(t, ArithTy::Int | ArithTy::Uint | ArithTy::Float))
            .ok_or_else(|| gap("inc/dec of a non-numeric target"))?;
        let bump = |s: &mut Self, old: Idx<Inst>| -> Idx<Inst> {
            let delta = match (ty, decrement) {
                (ArithTy::Float, false) => Inst::Const(Lit::Float(1.0)),
                (ArithTy::Float, true) => Inst::Const(Lit::Float(-1.0)),
                (ArithTy::Uint, _) => Inst::Const(Lit::Uint(1)),
                (_, false) => Inst::Const(Lit::Int(1)),
                (_, true) => Inst::Const(Lit::Int(-1)),
            };
            let d = s.emit_cur(delta);
            let inst = match (ty, decrement) {
                (ArithTy::Float, _) => Inst::FloatAdd(old, d),
                (ArithTy::Uint, false) => Inst::UintAdd(old, d),
                (ArithTy::Uint, true) => Inst::UintSub(old, d),
                (ArithTy::Int, _) => Inst::IntAdd(old, d),
                (ArithTy::Str, _) => unreachable!("inc/dec scalar filter admitted a string"),
            };
            let bumped = s.emit_cur(inst);
            if f32_target {
                s.emit_cur(Inst::FloatToF32(bumped))
            } else {
                bumped
            }
        };
        let update = |s: &mut Self, old| {
            let new = bump(s, old);
            Ok(s.check_integer_field(new, s.assign_integer_field_unsigned(target)))
        };
        let write = Write::Update {
            postfix,
            apply: &update,
        };
        let place = match self.body.expr_arena[target] {
            Expr::Var(b) if self.static_cells.contains_key(&raw(b)) => {
                Place::Global(self.static_cells[&raw(b)])
            }
            Expr::Var(b) => Place::Cell(
                *self
                    .binds
                    .get(&raw(b))
                    .ok_or_else(|| gap("read-modify-write of an unbound variable"))?,
            ),
            // The target's numeric type rules out a whole nobox group.
            _ if let Some((leaves, None)) = nobox_global_place => Place::Global(leaves[0]),
            Expr::Global(name) => Place::Global(name),
            Expr::FieldAccess { base, field } => {
                return self
                    .write_field_target(target, base, field, None, write)
                    .map(|v| v.expect("increment/decrement requests a result"));
            }
            Expr::Index { base, index } => {
                let (base, index, kind) = self.index_place(base, index)?;
                Place::Index(base, index, kind)
            }
            _ => return Err(gap("read-modify-write of a non-assignable target")),
        };
        self.write_scalar(place, None, write)
            .map(|v| v.expect("increment/decrement requests a result"))
    }

    pub(crate) fn safe_nav_call_value_repr(
        &mut self,
        e: Idx<Expr>,
        method: Ustr,
        args: IdxRange<Expr>,
        recv: Idx<Inst>,
    ) -> Result<ValueBundle, CompilerError> {
        if let Some(field) = self.fn_field_of_call(e) {
            // `recv?.field(args)` on a fn-typed FIELD: nil-guarded indirect call
            // `(recv.field)(args)` — the field IS the callee, no receiver arg.
            let fieldv = self.read_field(recv, field);
            let call_args: Vec<Idx<Expr>> = args.into_iter().collect();
            let field_ty = self
                .inf
                .and_then(|inf| inf.fn_field_call_types.get(&e))
                .copied()
                .ok_or_else(|| {
                    self.gap_here("fn-field call is missing its resolved function type")
                })?;
            let abi = self
                .fn_value_rep_plan(field_ty)?
                .ok_or_else(|| self.gap_here("fn-field call has no concrete function ABI plan"))?;
            self.indirect_call_by_plan(abi, fieldv, &call_args)
        } else {
            let func = self.resolve_funcref(e, method)?;
            let arg_idxs: Vec<Idx<Expr>> = args.into_iter().collect();
            self.ufcs_call_repr(e, func, recv, &arg_idxs)
        }
    }

    fn pack_safe_nav_call_value(
        &mut self,
        e: Idx<Expr>,
        method: Ustr,
        args: IdxRange<Expr>,
        recv: Idx<Inst>,
    ) -> Result<Idx<Inst>, CompilerError> {
        let r = self.safe_nav_call_value_repr(e, method, args, recv)?;
        self.pack_maybe_absent(r.into_value())
    }

    fn lower_scalar_safe_nav_receiver(
        &mut self,
        e: Idx<Expr>,
        base: Idx<Expr>,
        field: Ustr,
    ) -> Result<SafeNavRecv, CompilerError> {
        // A scalar projection cannot expose the nullable value box. Borrow its
        // slot for this expression, then use the shared present-field reader.
        // All eligibility checks precede lowering: a fallback evaluates once.
        if !self.scx.values_first_class
            && let Some(ty) = self.concrete_expr_ty(e)
            && (matches!(
                ty.peel_nullable().0,
                MonoTy::Int | MonoTy::Uint | MonoTy::Float | MonoTy::Bool | MonoTy::String
            ) || ty.peel_nullable().0.nominal().is_some_and(|name| {
                EnumPlan::for_name(self.ws, name).is_some_and(|plan| plan.is_payload_free())
            }))
            && let Expr::FieldAccess {
                base: owner,
                field: slot,
            } = self.body.expr_arena[base]
            && let Some(TypeExpr::App {
                ctor: TypeCtor::Nominal(owner_name),
                ..
            }) = self.field_base_ty(owner)
            && self.ws.struct_by_name.contains_key(&owner_name)
            && !self.is_value_struct(owner_name)
            && let plan = self.struct_field_plan(owner_name, slot, false)?
            && plan.is_direct()
            && plan.nullable()
            && let Some(child) = plan.child()
            && self.is_value_struct(child)
            && self.swizzle_of(base, field).is_none()
            && self.struct_field_plan(child, field, false)?.is_direct()
        {
            let (owner, slot) = self.physical_field(owner, slot)?;
            let owner = self.lower_expr(owner)?;
            let value = self.read_field(owner, slot);
            return Ok(SafeNavRecv::Boxed(value));
        }
        self.lower_safe_nav_receiver(base)
    }

    /// Evaluate call arguments only when the receiver is present.
    pub(crate) fn lower_safe_nav_receiver(
        &mut self,
        receiver: Idx<Expr>,
    ) -> Result<SafeNavRecv, CompilerError> {
        // A boxed read can already carry an unboxed nullable representation.
        let r = self.lower_expr_repr(receiver)?.into_value();
        if let ValueRepr::Group {
            present: Some(p), ..
        } = &r
        {
            return Ok(SafeNavRecv::Unboxed {
                repr: r.clone(),
                present: *p,
            });
        }
        Ok(SafeNavRecv::Boxed(self.pack(r)?))
    }

    /// The "receiver is absent" branch condition for [`SafeNavRecv`].
    pub(crate) fn safe_nav_absent_cond(&mut self, r: &SafeNavRecv) -> Idx<Inst> {
        match r {
            SafeNavRecv::Boxed(v) => self.is_nil(*v),
            SafeNavRecv::Unboxed { present, .. } => self.emit_cur(Inst::Not(*present)),
        }
    }

    /// The receiver VALUE, for the PRESENT arm only. Packing an unboxed Group is
    /// sound here and nowhere else: this arm runs only when the presence tag
    /// said so, so the ctor stands for a value that really is there.
    pub(crate) fn safe_nav_present_value(
        &mut self,
        r: SafeNavRecv,
    ) -> Result<Idx<Inst>, CompilerError> {
        match r {
            SafeNavRecv::Boxed(v) => Ok(v),
            SafeNavRecv::Unboxed { repr, .. } => self.pack(repr),
        }
    }

    pub(crate) fn lower_safe_nav_call(
        &mut self,
        e: Idx<Expr>,
        receiver: Idx<Expr>,
        method: Ustr,
        args: IdxRange<Expr>,
    ) -> Result<Idx<Inst>, CompilerError> {
        let recv = self.lower_safe_nav_receiver(receiver)?;
        self.safe_nav_scalar(recv, false, |s, recv| {
            let recv_v = s.safe_nav_present_value(recv)?;
            s.pack_safe_nav_call_value(e, method, args, recv_v)
        })
    }

    /// The one-cell join of a `?.` form whose present arm yields one value.
    /// `test_presence` branches on an unboxed receiver's presence tag itself
    /// rather than on its negation.
    fn safe_nav_scalar(
        &mut self,
        recv: SafeNavRecv,
        test_presence: bool,
        value_arm: impl FnOnce(&mut Self, SafeNavRecv) -> Result<Idx<Inst>, CompilerError>,
    ) -> Result<Idx<Inst>, CompilerError> {
        let presence = match &recv {
            SafeNavRecv::Unboxed { present, .. } if test_presence => Some(*present),
            _ => None,
        };
        let cond = match presence {
            Some(present) => present,
            None => self.safe_nav_absent_cond(&recv),
        };
        let result = self
            .f
            .new_cell_typed(None, self.current_expr_storage_ty_id());
        self.nil_or_join(result, cond, presence.is_some(), |s| value_arm(s, recv))
    }

    /// The struct field plan behind `base?.field`, when the field is a named
    /// struct field rather than a tuple index, newtype unwrap or swizzle.
    fn safe_nav_field_plan(
        &self,
        base: Idx<Expr>,
        field: Ustr,
    ) -> Result<Option<StructFieldPlan>, CompilerError> {
        if field.parse::<usize>().is_ok() || self.expr_is_newtype(base) {
            return Ok(None);
        }
        match self.field_base_ty(base) {
            Some(TypeExpr::App {
                ctor: TypeCtor::Nominal(sname),
                ..
            }) if self.swizzle_of(base, field).is_none() => {
                Ok(Some(self.struct_field_plan(sname, field, false)?))
            }
            _ => Ok(None),
        }
    }

    /// Read a field from an already evaluated, present boxed receiver.
    /// Shared by scalar and per-slot safe navigation.
    pub(crate) fn safe_nav_field_value(
        &mut self,
        e: Idx<Expr>,
        base: Idx<Expr>,
        bv: Idx<Inst>,
        field: Ustr,
    ) -> Result<Idx<Inst>, CompilerError> {
        let embed_plan = self.safe_nav_field_plan(base, field)?;
        let embed = embed_plan.as_ref().and_then(|p| p.embed());
        let fv = match field.as_str().parse::<usize>() {
            // `x?.0` on a transparent newtype is identity (see TupleIndex).
            Ok(_) if self.expr_is_newtype(base) => bv,
            Ok(index) => self.emit_cur(Inst::TupleIndex { base: bv, index }),
            Err(_) => match self.swizzle_of(base, field) {
                // `v?.yx`: expand on the non-null arm, packed for the join cell.
                Some(idxs) => {
                    let g = self.swizzle_from_packed(bv, &idxs);
                    self.pack(g)?
                }
                // An embed/nobox field has no slot under its own name: its leaves
                // live on the parent under dotted keys.
                None if embed.is_some() => {
                    let cname = embed
                        .map(|(child, _)| child)
                        .ok_or_else(|| gap("embed field plan disappeared"))?;
                    let (pbase, key) = self.physical_field(base, field)?;
                    let parent = if pbase == base {
                        bv
                    } else {
                        self.lower_expr(pbase)?
                    };
                    if self.embed_read_is_nullable(base, field)? {
                        self.materialize_embed_nullable(parent, key, cname)?
                    } else {
                        self.materialize_embed(parent, key, cname)?
                    }
                }
                // `o?.p` reads a value field whole out of a mutable place, so
                // it copies exactly as `o.p` does.
                None => {
                    let fv = self.read_field(bv, field);
                    let copy_name = match self.value_struct_name_of(e) {
                        Some(name) if self.is_mutable_place(base)? => Some(name),
                        _ => None,
                    };
                    match copy_name {
                        Some(name) if self.embed_read_is_nullable(base, field)? => {
                            self.nullable_boxed_value_copy(fv, name, self.concrete_expr_ty(e))?
                        }
                        Some(name) => {
                            let copy =
                                self.boxed_value_copy_repr(fv, name, self.concrete_expr_ty(e))?;
                            self.pack(copy)?
                        }
                        None => fv,
                    }
                }
            },
        };
        Ok(fv)
    }

    /// [`Self::lower_safe_nav_let`] joining PER SLOT, so a `nobox` body result
    /// crosses as leaves. The body is the whole value arm, so it just recurses
    /// into the repr lowering.
    pub(crate) fn safe_nav_let_repr(
        &mut self,
        receiver: Idx<Expr>,
        binding: Idx<bone_core::hir::BindingInfo>,
        body: Idx<Expr>,
        shape: RepLayout,
    ) -> Result<ValueRepr, CompilerError> {
        self.safe_nav_repr(receiver, shape, |s, recv| {
            let cell = s.new_binding_cell(
                binding,
                s.body.binding_arena[binding].name,
                s.binding_ty(binding),
                crate::names::ustr(""),
            );
            s.store_cell(cell, recv);
            s.binds.insert(raw(binding), cell);
            s.lower_expr_repr(body)
        })
    }

    pub(crate) fn safe_nav_field_repr(
        &mut self,
        e: Idx<Expr>,
        base: Idx<Expr>,
        field: Ustr,
        shape: RepLayout,
    ) -> Result<ValueRepr, CompilerError> {
        let plan = self.safe_nav_field_plan(base, field)?;
        let embed = plan.and_then(|p| p.inline_child());
        let group_shape = match embed {
            Some(cname) => self.nobox_embed_group_shape(cname)?,
            None => None,
        };
        let nullable_child = embed.is_some() && self.embed_read_is_nullable(base, field)?;
        self.safe_nav_repr(base, shape, move |s, bv| {
            let Some(vs) = group_shape else {
                // No embed group to project: fall back to the scalar field read
                // for this one arm, then ride the single slot.
                let value = ValueRepr::Scalar(s.safe_nav_field_value(e, base, bv, field)?);
                return s.value_bundle_for_expr(e, value);
            };
            // Compose through `physical_field` exactly as the scalar arm does:
            // when the base is itself an embed read, `bv` is a re-materialized
            // box whose keys are undotted, so the dotted leaves are not on it.
            let (pbase, key) = s.physical_field(base, field)?;
            let parent = if pbase == base {
                bv
            } else {
                s.lower_expr(pbase)?
            };
            let mut g = s.embed_group(parent, key, &vs);
            if nullable_child {
                // A nullable child carries presence in the field's own undotted
                // key; the Group takes it rather than gating the read, so an
                // absent child's leaves are nil and consumers branch on presence.
                let present = s.read_field(parent, key);
                if let ValueRepr::Group { present: p, .. } = &mut g {
                    *p = Some(present);
                }
            }
            s.value_bundle_for_expr(e, g)
        })
    }

    /// `recv?.let { b -> body }` — `nil` if `recv` is nil, else `body` with `b`
    /// bound to the (non-nil) receiver. Result flows through a cell.
    pub(crate) fn lower_safe_nav_let(
        &mut self,
        receiver: Idx<Expr>,
        binding: Idx<bone_core::hir::BindingInfo>,
        body: Idx<Expr>,
    ) -> Result<Idx<Inst>, CompilerError> {
        let recv = self.lower_safe_nav_receiver(receiver)?;
        self.safe_nav_scalar(recv, false, |s, recv| {
            let recv_v = s.safe_nav_present_value(recv)?;
            let cell = s.new_binding_cell(
                binding,
                s.body.binding_arena[binding].name,
                s.binding_ty(binding),
                crate::names::ustr(""),
            );
            s.store_cell(cell, recv_v);
            s.binds.insert(raw(binding), cell);
            s.lower_expr(body)
        })
    }

    /// A `[T]` literal whose element stores as columns lowers every element to
    /// its leaves, including elements supplied through a spread.
    pub(crate) fn column_vec_literal(
        &mut self,
        elems: &[Idx<Expr>],
        elem: ColumnElem,
    ) -> Result<Idx<Inst>, CompilerError> {
        let shape = self.column_shape(elem)?;
        let paths = column_leaf_paths(&shape);
        let mut columns: Vec<(Ustr, Vec<Idx<Inst>>)> = paths
            .iter()
            .map(|p| (*p, Vec::with_capacity(elems.len())))
            .collect();
        for &e in elems {
            let repr = self.lower_expr_repr(e)?;
            let leaves =
                self.element_storage_leaves(&repr, &shape, paths.len(), elem, "column literal")?;
            for (col, leaf) in columns.iter_mut().zip(leaves) {
                col.1.push(leaf);
            }
        }
        let capacity = self.emit_cur(Inst::Const(Lit::Int(elems.len() as i64)));
        Ok(self.emit_cur(Inst::MakeVecColumns {
            elem: elem.name,
            representation: elem.representation(),
            nobox: elem.is_nobox(),
            columns,
            len: elems.len(),
            capacity,
        }))
    }

    pub(crate) fn strided_vec_literal(
        &mut self,
        elems: &[Idx<Expr>],
        elem: ColumnElem,
    ) -> Result<Idx<Inst>, CompilerError> {
        let shape = self.column_shape(elem)?;
        let stride = shape.leaf_count();
        let mut values = Vec::with_capacity(elems.len() * stride);
        for &expr in elems {
            let bundle = self.lower_expr_repr(expr)?;
            let leaves =
                self.element_storage_leaves(&bundle, &shape, stride, elem, "strided literal")?;
            if self.strided_stores_raw(elem) {
                values.extend(leaves);
            } else {
                values.extend(
                    leaves
                        .into_iter()
                        .map(|leaf| self.emit_cur(Inst::VecStrideNilToFalse(leaf))),
                );
            }
        }
        let capacity = self.emit_cur(Inst::Const(Lit::Int(elems.len() as i64)));
        Ok(self.emit_cur(Inst::MakeVecStrided {
            elem: elem.name,
            representation: elem.representation(),
            nobox: elem.is_nobox(),
            stride,
            values,
            len: elems.len(),
            capacity,
        }))
    }

    pub(crate) fn lower_array_literal(
        &mut self,
        lit: Idx<Expr>,
        elems: IdxRange<Expr>,
    ) -> Result<Idx<Inst>, CompilerError> {
        self.lower_array_literal_with_expected(lit, elems, None)
    }

    fn lower_array_literal_with_expected(
        &mut self,
        lit: Idx<Expr>,
        elems: IdxRange<Expr>,
        expected: Option<&crate::monoty::MonoTy>,
    ) -> Result<Idx<Inst>, CompilerError> {
        self.with_value_lending(false, |s| s.lower_array_literal_owned(lit, elems, expected))
    }

    fn lower_array_literal_owned(
        &mut self,
        lit: Idx<Expr>,
        elems: IdxRange<Expr>,
        expected: Option<&crate::monoty::MonoTy>,
    ) -> Result<Idx<Inst>, CompilerError> {
        let idxs: Vec<Idx<Expr>> = elems.into_iter().collect();
        let has_spread = idxs
            .iter()
            .any(|&e| matches!(self.body.expr_arena[e], Expr::Spread(_)));
        let flat_elem = match expected {
            Some(ty) => self.vec_flat_elem_for_type(ty)?,
            None => self.vec_flat_elem_of(lit)?,
        };
        if !has_spread {
            match flat_elem {
                Some((elem, FlatStorage::Columns)) => return self.column_vec_literal(&idxs, elem),
                Some((elem, FlatStorage::Strided)) => return self.strided_vec_literal(&idxs, elem),
                None => {}
            }
            let vs: Vec<Idx<Inst>> = idxs
                .iter()
                .map(|&e| self.lower_expr(e))
                .collect::<Result<_, _>>()?;
            return Ok(self.emit_cur(Inst::MakeVec(vs)));
        }
        if let Some((elem, storage)) = flat_elem {
            let capacity = self.emit_cur(Inst::Const(Lit::Int(0)));
            let arr = self.empty_flat_vec(elem, storage, capacity)?;
            for &e in &idxs {
                if let Expr::Spread(inner) = self.body.expr_arena[e] {
                    self.spread_into(arr, inner, SpreadDest::Flat(elem, storage))?;
                } else {
                    let bundle = self.lower_expr_repr(e)?;
                    self.flat_append_bundle(arr, &bundle, elem, storage)?;
                }
            }
            return Ok(arr);
        }
        let arr = self.emit_cur(Inst::MakeVec(vec![]));
        for &e in &idxs {
            if let Expr::Spread(inner) = self.body.expr_arena[e] {
                self.spread_into(arr, inner, SpreadDest::Container(ContainerKind::Vec))?;
            } else {
                let v = self.lower_expr(e)?;
                self.emit_cur(Inst::ArrayPush {
                    arr,
                    value: v,
                    kind: ContainerKind::Vec,
                });
            }
        }
        Ok(arr)
    }

    pub(crate) fn append_spread_bundle(
        &mut self,
        arr: Idx<Inst>,
        bundle: ValueBundle,
        dest: SpreadDest,
    ) -> Result<(), CompilerError> {
        match dest {
            SpreadDest::Container(kind) => {
                let value = self.pack(bundle.into_value())?;
                self.emit_cur(Inst::ArrayPush { arr, value, kind });
                Ok(())
            }
            SpreadDest::Flat(elem, storage) => self.flat_append_bundle(arr, &bundle, elem, storage),
        }
    }

    fn append_spread_value(
        &mut self,
        arr: Idx<Inst>,
        value: ValueRepr,
        dest: SpreadDest,
    ) -> Result<(), CompilerError> {
        match dest {
            SpreadDest::Container(kind) => {
                let value = self.pack(value)?;
                self.emit_cur(Inst::ArrayPush { arr, value, kind });
                Ok(())
            }
            SpreadDest::Flat(elem, _) => {
                self.append_spread_bundle(arr, ValueBundle::new(elem.representation(), value), dest)
            }
        }
    }

    /// Splice `inner` into `arr`: a tuple by static arity, a struct by declared
    /// fields, or an array/Vec through one runtime loop. Only the destination
    /// append differs between ordinary and column storage.
    pub(crate) fn spread_into(
        &mut self,
        arr: Idx<Inst>,
        inner: Idx<Expr>,
        dest: SpreadDest,
    ) -> Result<(), CompilerError> {
        if let Expr::Tuple(elements) = self.body.expr_arena[inner] {
            match dest {
                SpreadDest::Flat(..) => {
                    for element in elements {
                        let bundle = self.lower_expr_repr(element)?;
                        self.append_spread_bundle(arr, bundle, dest)?;
                    }
                    return Ok(());
                }
                SpreadDest::Container(_) => {}
            }
        }
        let v = self.lower_expr(inner)?;
        if let Some(ty) = self.concrete_expr_ty(inner) {
            return match ty.peel_nullable().0 {
                // Each spliced value element is the destination's own copy.
                crate::monoty::MonoTy::Tuple(elements) => {
                    for (index, ety) in elements.iter().enumerate() {
                        let value = self.emit_cur(Inst::TupleIndex { base: v, index });
                        let value = self.copied_value_repr(ety, value)?;
                        self.append_spread_value(arr, value, dest)?;
                    }
                    Ok(())
                }
                crate::monoty::MonoTy::App {
                    ctor: crate::monoty::MonoCtor::Array | crate::monoty::MonoCtor::Map,
                    ..
                } => self.spread_array_loop(arr, inner, v, dest),
                crate::monoty::MonoTy::App {
                    ctor: crate::monoty::MonoCtor::Nominal(name),
                    ..
                } => {
                    let fields = self
                        .struct_field_names(*name)
                        .ok_or_else(|| gap("spread of a non-struct nominal type"))?;
                    for field in fields {
                        let value = self.read_field(v, field);
                        let fty = self.struct_field_mono_ty(*name, field, Some(inner))?;
                        let value = self.copied_value_repr(&fty, value)?;
                        self.append_spread_value(arr, value, dest)?;
                    }
                    Ok(())
                }
                _ => Err(gap("spread of an unsupported type")),
            };
        }
        Err(gap(
            "spread expression is missing from the concrete type plan",
        ))
    }

    /// Runtime copy of every element of `src` (read per its `coll_kind`) into
    /// `arr` through the destination's append operation.
    pub(crate) fn spread_array_loop(
        &mut self,
        arr: Idx<Inst>,
        src_expr: Idx<Expr>,
        src: Idx<Inst>,
        dest: SpreadDest,
    ) -> Result<(), CompilerError> {
        let src_kind = self.coll_kind(src_expr)?;
        let n = self.emit_cur(Inst::Len {
            base: src,
            kind: src_kind,
        });
        let i_cell = self.f.new_cell(None);
        let zero = self.emit_cur(Inst::Const(Lit::Int(0)));
        self.store_cell(i_cell, zero);

        let header = self.f.new_block();
        let body = self.f.new_block();
        let exit = self.f.new_block();
        self.f.set_term(self.cur, Term::Goto(header));

        self.cur = header;
        let i = self.emit_cur(Inst::Load(i_cell));
        let cmp = self.emit_cur(Inst::IntLt(i, n));
        self.branch_to(cmp, body, exit);

        self.cur = body;
        let i_now = self.emit_cur(Inst::Load(i_cell));
        let repr = self.loop_element_repr_at(src, i_now, src_kind)?;
        self.append_spread_value(arr, repr, dest)?;
        self.increment_cell(i_cell);
        self.f.set_term(self.cur, Term::Goto(header));

        self.cur = exit;
        Ok(())
    }
}
