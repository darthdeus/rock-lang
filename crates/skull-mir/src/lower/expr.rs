//! Expression lowering: the main dispatch, place bases, assignment and
//! compound assignment, `++`/`--`, safe navigation, and array/spread literals.
use super::*;

impl Builder<'_> {
    pub(crate) fn lower_expr(&mut self, e: Idx<Expr>) -> Result<Idx<Inst>, CompilerError> {
        // Source cursor for instruction-level diagnostics: whatever is lowered
        // from here down came from `e` unless a nested `lower_expr` narrows it,
        // and the restore puts the enclosing expression back on the way out.
        let outer = self.cur_span.replace(self.body.expr_spans[e]);
        // The site's EXPRESSION half, tracked exactly beside the span half. A
        // nested `lower_expr` narrows it and the restore puts the enclosing
        // expression back, so a call always carries the innermost expression
        // that produced it -- which is the key `ConcreteBody` is built on.
        let outer_expr = self.cur_expr.replace(e);
        let v = self.lower_expr_inner(e);
        self.cur_span = outer;
        self.cur_expr = outer_expr;
        let v = v?;
        // Implicit iterable->iterator coercion at a bounded call arg: bone
        // recorded the resolved `iter` fn; wrap the arg value in that call
        // (the call-arg analog of `lower_for_user_iter`'s `iter_fn`).
        if let Some(w) = self.inf.and_then(|i| i.implicit_iter_args.get(&e)) {
            let (m, f) = w.func;
            let iter_ref = FuncRef { module: m, func: f };
            // This coercion belongs to `e`, but the cursor was already restored
            // to the ENCLOSING expression above -- so set both halves of the
            // site explicitly rather than inheriting a cursor that names the
            // wrong expression.
            let outer_expr = self.cur_expr.replace(e);
            // The instance is resolved BEFORE the arguments are shaped: they are
            // splatted against the ABI of the body the call targets, not the
            // declaration's.
            let witness = self
                .with_site_op(crate::site::SiteOp::ImplicitIter, |b| b.concrete_callee_for_site(e))?
                .ok_or_else(|| gap("implicit iterator has no concrete callee"))?;
            let ty = self
                .concrete_expr_ty(e)
                .ok_or_else(|| self.gap_here("implicit iterator expression has no concrete type"))?;
            let tuple_spelled = matches!(ty.peel_nullable().0, crate::monoty::MonoTy::Tuple(_));
            let representation = self.concrete_rep_id(
                &ty,
                if tuple_spelled { RepUse::Return } else { RepUse::Value },
                tuple_spelled,
                self.nobox_value_name(e).is_some(),
            )?;
            let call = self.with_site_op(crate::site::SiteOp::ImplicitIter, |b| {
                let args = b.iter_witness_args(
                    &witness,
                    ValueBundle::new(representation, ValueRepr::Scalar(v)),
                )?;
                Ok::<_, CompilerError>(b.emit_cur(Inst::Call { func: iter_ref, args }))
            })?;
            // DELIVERY, exactly as the `for` head's witness does it. Both forms
            // are ordinary calls to a generic `iter`, so under instance-owned
            // emission both must name an instance; recording identity without
            // delivering it spells a symbol nothing defines.
            if let Some(delivery) = self.concrete_delivery_instance(&witness) {
                self.f.spec_bindings.insert(call, delivery);
            }
            // IDENTITY beside delivery, the plan's split, and the same pair the
            // `for` witness records. Delivery alone leaves the site's own claim
            // unstated, which the census reports as `callee-no-instance` -- it
            // was invisible only because the census runs POST-TRANSFORM and a
            // splice removed the site before it could be examined.
            let iter_decl = &self.ws.module(m).functions[f];
            if iter_decl.generics.is_empty() || !witness.args.is_empty() {
                crate::site::record_call_owner(
                    &mut self.f,
                    call,
                    &witness,
                );
            }
            let abi = self.callee_abi_plan_for_instance(&witness)?;
            self.f.call_abi_plans.insert(call, abi);
            self.cur_expr = outer_expr;
            return Ok(call);
        }
        Ok(v)
    }

    pub(crate) fn lower_expr_inner(&mut self, e: Idx<Expr>) -> Result<Idx<Inst>, CompilerError> {
        match self.body.expr_arena[e].clone() {
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
                Ok(self.pack(r)?)
            }
            Expr::Var(b) if self.static_cells.contains_key(&raw(b)) => {
                Ok(self.emit_cur(Inst::Global(self.static_cells[&raw(b)])))
            }
            // A nested fn used as a value: reference the hoisted top-level fn by
            // name (a plain global read -- zero closure alloc).
            Expr::Var(b) if self.nested_fn_globals.contains_key(&raw(b)) => {
                let hoisted = self.nested_fn_globals[&raw(b)].hoisted;
                self.f.hoisted_fn_globals.push(hoisted);
                Ok(self.emit_cur(Inst::Global(hoisted)))
            }
            // A whole-tuple use of a name bound to slot cells: rebuild and pack.
            // Packing a `nobox` element here is exactly how a whole-tuple escape
            // still reaches the guarantee's rejection.
            Expr::Var(b) if self.tuple_binds.contains_key(&raw(b)) => {
                let r = self.lower_expr_repr(e)?.into_value();
                Ok(self.pack(r)?)
            }
            Expr::Var(b) => {
                let cell = *self
                    .binds
                    .get(&raw(b))
                    .ok_or_else(|| gap("read of an unbound variable"))?;
                Ok(self.emit_cur(Inst::Load(cell)))
            }
            // A bare name resolving to a FUNCTION taken by value (`let f = adder`,
            // passing `adder` as an arg, a `func: adder` field) is stamped as a
            // FnValue site for every backend. The backend owns the emitted symbol,
            // but the graph needs the target-neutral obligation before that choice.
            Expr::Global(name) if self.ws.functions_by_name.contains_key(&name) => {
                let fref = self.resolve_funcref(e, name)?;
                self.reject_fn_value_abi_mismatch(e, fref)?;
                let sym = if self.scx.target == bone_semantic::hir::FnBackend::Js {
                    name.to_string()
                } else {
                    self.fn_value_symbol(e, name)?
                };
                let id = self.with_site_op(crate::site::SiteOp::FnValue, |b| {
                    b.emit_cur(Inst::Global(Ustr::from(sym.as_str())))
                });
                // The one place a callee DECLARATION becomes a spelled value.
                // Recorded here rather than recovered downstream from the name:
                // `Inst::Global` is also a module global and a `static` cell,
                // and a name comparison could not tell them apart.
                self.f.fn_value_symbols.insert(id, fref);
                Ok(id)
            }
            // A global holding a value struct routes through the repr layer so a
            // whole-value read copies the box (a global is a mutable place).
            Expr::Global(name) => {
                if self.value_struct_of(e).is_some() || self.nullable_place_value_of(e)?.is_some() {
                    let r = self.lower_expr_repr(e)?.into_value();
                    return Ok(self.pack(r)?);
                }
                Ok(self.emit_cur(Inst::Global(name)))
            }

            // A reached `?` hole is a not-yet-filled marker: raise a readable
            // runtime error (naming the hole) rather than silently evaluating to
            // nil. `error` diverges, so the trailing nil is never read -- it only
            // satisfies the expression's value slot.
            Expr::Hole(name) => {
                let label = match name {
                    Some(n) => format!("reached unfilled hole ?{n}"),
                    None => "reached unfilled hole ?".to_string(),
                };
                let msg = self.emit_cur(Inst::Const(Lit::Str(Ustr::from(label.as_str()))));
                let func = self
                    .ws
                    .functions_by_name
                    .get(&Ustr::from("error"))
                    .and_then(|l| l.first())
                    .map(|&(module, func)| FuncRef { module, func })
                    .ok_or_else(|| gap("stdlib `error` missing for hole lowering"))?;
                self.emit_cur(Inst::Call { func, args: vec![msg] });
                Ok(self.emit_cur(Inst::Const(Lit::Nil)))
            }

            // Unary `!`/`~`/`-`: a struct operand is an operator overload (bone
            // records the target on `e`); a scalar uses the native op.
            Expr::Not(x) => {
                let v = self.lower_expr(x)?;
                match self.overload_target(e) {
                    Some(func) => Ok(self.emit_cur(Inst::Call { func, args: vec![v] })),
                    None => Ok(self.emit_cur(Inst::Not(v))),
                }
            }
            Expr::BitNot(x) => {
                let v = self.lower_expr(x)?;
                match self.overload_target(e) {
                    Some(func) => Ok(self.emit_cur(Inst::Call { func, args: vec![v] })),
                    None if self.arith_ty(x) == Some(ArithTy::Uint) => {
                        Ok(self.emit_cur(Inst::UintBitNot(v)))
                    }
                    None => Ok(self.emit_cur(Inst::BitNot(v))),
                }
            }
            Expr::UnaryMinus(x) => {
                let v = self.lower_expr(x)?;
                match self.overload_target(e) {
                    Some(func) => Ok(self.emit_cur(Inst::Call { func, args: vec![v] })),
                    None => Ok(self.emit_cur(Inst::Neg(v))),
                }
            }

            Expr::BinOp { op, lhs, rhs } => self.lower_binop(op, lhs, rhs),

            Expr::If {
                cond,
                then_branch,
                else_branch,
            } => self.lower_if(cond, then_branch, else_branch),

            // `break`/`continue` in expression position: emit a placeholder value
            // (never read — the block diverges) then a goto to the loop target.
            Expr::Break => {
                let (_, brk) = self.loop_target("break")?;
                self.used_break_targets.insert(brk);
                let v = self.emit_cur(Inst::Const(Lit::Nil));
                self.f.set_term(self.cur, Term::Goto(brk));
                Ok(v)
            }
            Expr::Continue => {
                let (cont, _) = self.loop_target("continue")?;
                let v = self.emit_cur(Inst::Const(Lit::Nil));
                self.f.set_term(self.cur, Term::Goto(cont));
                Ok(v)
            }

            Expr::Block { statements, tail } => {
                let repr = self.lower_block_repr(statements, tail)?;
                Ok(self.pack(repr)?)
            }

            Expr::Call { callee, args, .. } => self.lower_call(e, callee, args),

            // --- aggregates + projections + container reads ---
            // A BOXED tuple slot can be observed for nil-ness (`t.0 == null`), so
            // an absent nullable value element has to land there as `nil` rather
            // than as the ctor `pack` builds regardless of presence. The
            // splatting form keeps its own repr path in `lower_expr_repr`, where
            // the presence tag stays a leaf and is never at risk.
            Expr::Tuple(elems) => {
                // A `nobox` element rides the BOX as its leaves, on the same slot
                // list the multi-return wire uses -- so the box is the wire put in
                // a table, and `tuple_box_shape` is the one oracle both this
                // producer and every reader derive the layout from. Without it a
                // `nobox` part packs to a `<name>_ct` and the guarantee is gone the
                // moment the tuple escapes into a container (matrix row 13).
                //
                // `None` when nothing splats, which is every tuple that compiled
                // before: those keep today's one-value-per-element box exactly.
                let shape = self.tuple_box_shape(e)?;
                let mut vs = Vec::new();
                for (i, el) in elems.into_iter().enumerate() {
                    let bundle = self.lower_expr_repr(el)?;
                    match shape.as_ref().and_then(|s| s.slots.get(i)).cloned() {
                        Some(slot) if slot.splats() => {
                            let vals = self.repr_to_slot_values_bundle(&bundle, &slot)?;
                            vs.extend(vals);
                        }
                        _ => vs.push(self.pack_maybe_absent(bundle.into_value())?),
                    }
                }
                Ok(self.emit_cur(Inst::MakeTuple(vs)))
            }
            Expr::Array(elems) => self.lower_array_literal(e, elems),
            Expr::Index { base, index } => {
                // A whole-value read of a value-struct element COPIES (via the repr
                // layer's per-field Group -> `pack` rebuilds a fresh cdata) rather
                // than binding the interior array reference.
                //
                // Column storage routes on the CONTAINER, not on the element
                // type: inside a body specialized at `T = E?` the element type is
                // an unsubstituted generic (a nullable binding has no
                // synthesizable node), so the type predicates below see nothing
                // while the storage still has no element slot to name.
                if self.value_struct_of(e).is_some()
                    || self.nullable_place_value_of(e)?.is_some()
                    || self.vec_column_elem_of(base)?.is_some()
                {
                    let r = self.lower_expr_repr(e)?.into_value();
                    return Ok(self.pack(r)?);
                }
                let (b, kind) = self.lower_index_base(base)?;
                let i = self.lower_expr(index)?;
                Ok(self.emit_cur(Inst::Index {
                    base: b,
                    index: i,
                    kind,
                }))
            }
            // Route through the repr layer: a field read on a scalarized value
            // projects a leaf directly (no intermediate cdata); a boxed base
            // reads the field off the value as before.
            Expr::FieldAccess { .. } => {
                let r = self.lower_expr_repr(e)?.into_value();
                Ok(self.pack(r)?)
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
                Ok(self.pack(r)?)
            }
            // A value-struct literal is built via the repr layer (so a callsite
            // can splat it without a cdata); a plain struct is a Lua table.
            Expr::StructLiteral { name, fields, bases } if self.is_value_struct(name) => {
                let r = self.value_literal_repr(name, fields, bases)?;
                Ok(self.pack(r)?)
            }
            Expr::StructLiteral { name, fields, bases } => {
                self.lower_struct_literal(e, name, fields, bases)
            }
            // A FLAT enum's payload threads as leaves, so a boxed context must
            // build the Group and let `pack` box it -- the same repr-then-pack
            // route the cdata-eligible enum takes inside `lower_enum_variant`.
            // Lowering the payload args directly instead packs each one on its
            // own, which boxes a `nobox` payload the enum would have stored
            // splat (`tests/nobox_enum_payload_container.rock`).
            Expr::EnumVariant { enum_name, .. }
                if EnumPlan::for_name(self.ws, enum_name).is_some_and(|plan| plan.flat().is_some()) =>
            {
                let r = self.lower_expr_repr(e)?.into_value();
                Ok(self.pack(r)?)
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
                        Some(_shape) => {
                            let r = self.lower_expr_repr(eb)?;
                            self.repr_to_leaves_bundle(&r)?
                        }
                        None => vec![self.lower_expr(eb)?],
                    },
                    None => Vec::new(),
                };
                let resume = self.f.new_block();
                self.f.set_term(self.cur, Term::Yield { values, resume });
                self.cur = resume;
                let rac = self.resume_arg.ok_or_else(|| gap("yield outside a coroutine"))?;
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
                // A `nobox` param arrives as leaves, so the lambda binds a Group
                // over N cells instead of one -- the same shape a scalar-ABI fn
                // gives its params. The lambda and call site read the same
                // function-value representation plan.
                let lam_ty = self
                    .inf
                    .and_then(|inf| inf.expr_types.get(e))
                    .copied()
                    .ok_or_else(|| gap("lambda has no inferred function type"))?;
                let lam_plan = self
                    .fn_value_rep_plan(lam_ty)?
                    .ok_or_else(|| gap("lambda does not have a function representation"))?;
                // Param shapes come from the param BINDINGS, which is where a
                // lambda's param types actually land -- its own `fn(..) -> ..`
                // node can still hold unresolved vars at this point, while the
                // bindings carry what inference unified them WITH, i.e. the same
                // types the call site reads off the fn type.
                let param_slots: Vec<RetSlot> = params
                    .clone()
                    .into_iter()
                    .enumerate()
                    .map(|(i, _)| {
                        let id = lam_plan.params().get(i).copied().ok_or_else(|| {
                            gap(format!("lambda parameter {i} has no representation plan"))
                        })?;
                        Ok(self.scx.rep_arena().ret_slot(id))
                    })
                    .collect::<Result<_, CompilerError>>()?;
                // A lambda's params land in the SAME cell namespace as the
                // enclosing body's, so it collects its own list off `f.params`
                // rather than owning it -- but the binding rule itself is
                // `bind_param_slot`, shared with the other two definition forms.
                let before = self.f.params.len();
                for (i, pb) in params.into_iter().enumerate() {
                    let pname = self.body.binding_arena[pb].name;
                    let slot = param_slots.get(i).cloned().ok_or_else(|| {
                        gap(format!("lambda parameter {i} has no representation plan"))
                    })?;
                    self.bind_param_slot(pb, pname, &slot);
                }
                let param_cells: Vec<Idx<Cell>> = self.f.params.split_off(before);
                let entry = self.f.new_block();
                let saved = self.cur;
                self.cur = entry;
                // The lambda body returns via the LAMBDA's ABI, not the enclosing
                // fn's: a bone-desugared `() => e` body is a block with a `return e`
                // statement, so the return machinery (`return_leaves`) must see the
                // lambda's own return shape. A tuple-returning lambda splats N values
                // so an indirect call site can capture them (multi-return ABI); the
                // enclosing value-struct result never applies (a lambda is
                // called through a boxed fn-value). Save/restore around the body.
                let saved_ret_rep = self.ret_rep.take();
                // Isolate the defer state, like `lower_nested_fn`. A lambda body
                // is its OWN fn scope: bone desugars `() => e` to a block with
                // `return e`, so the body's return runs `run_all_defers` -- which
                // must see only the lambda's own pending defers, never the
                // enclosing fn's. Leaving the stack shared spliced every pending
                // enclosing defer into the lambda body (it then ran on every call)
                // and, when the defer body was what created the lambda, recursed
                // forever (run_all_defers re-splices the still-pending defer ->
                // re-lowers the lambda). `in_defer_body` resets to false: the
                // lambda body is not the defer body's code (a `yield`/`await`
                // there is already rejected by bone as yield-in-plain-lambda, so
                // the reset unmasks no diagnostic -- it just makes the flag honest).
                let saved_defers = std::mem::take(&mut self.defers);
                let saved_in_defer = std::mem::replace(&mut self.in_defer_body, false);
                // Box value args (see field docs). A lambda body IS spliced into
                // now -- `convert_closures` splits it out and the inliner runs
                // over the standalone body -- but that happens after lowering, so
                // this decision cannot see the specialized callee it would need.
                self.ret_rep = lam_plan.result();
                if let Some(_shape) = self.result_shape() {
                    let r = self.lower_expr_repr(body)?;
                    if self.is_open() {
                        let leaves = self.repr_to_leaves_bundle(&r)?;
                        self.f.set_term(self.cur, Term::Return(leaves));
                    }
                    self.defers = saved_defers;
                    self.in_defer_body = saved_in_defer;
                    self.ret_rep = saved_ret_rep;
                    self.cur = saved;
                    return Ok(self.emit_cur(Inst::Lambda { params: param_cells, entry }));
                }
                match self.result_tuple() {
                    // A splatting tuple return needs the body as a repr, so a
                    // `nobox` element reaches the wire as leaves.
                    Some(shape) if shape.splats() => {
                        let r = self.lower_expr_repr(body)?;
                        if self.is_open() {
                            let leaves = self.tuple_repr_to_slots(r.into_value(), &shape)?;
                            self.f.set_term(self.cur, Term::Return(leaves));
                        }
                    }
                    other => {
                        let v = self.lower_expr(body)?;
                        if self.is_open() {
                            let leaves = match other {
                                Some(shape) => self.tuple_return_operands(v, shape.slots.len()),
                                None => vec![v],
                            };
                            self.f.set_term(self.cur, Term::Return(leaves));
                        }
                    }
                }
                self.defers = saved_defers;
                self.in_defer_body = saved_in_defer;
                self.ret_rep = saved_ret_rep;
                self.cur = saved;
                Ok(self.emit_cur(Inst::Lambda { params: param_cells, entry }))
            }

            // `as` is identity at runtime EXCEPT `float as int`, which floors
            // (Lua numbers are all doubles) — mirrors rock-codegen.
            Expr::As { value, target, kind } => {
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
                if kind == bone_semantic::hir::AsKind::Bitcast && to_int && from_uint {
                    Ok(self.emit_cur(Inst::UintBitcastInt(v)))
                } else if kind == bone_semantic::hir::AsKind::Bitcast && to_uint && from_int {
                    Ok(self.emit_cur(Inst::IntBitcastUint(v)))
                } else if kind == bone_semantic::hir::AsKind::Bitcast {
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
                } else {
                    Ok(v)
                }
            }

            // `x?.field` -> `if x == nil then nil else x.field`, joined through a
            // result cell (mirrors rock-codegen). A numeric field is a tuple read
            // (`x?.0`), a name is a struct/enum-record field.
            Expr::SafeNav { base, field } => {
                // A chained `?.` whose BASE is a `nobox` value: this node's own
                // result is an ordinary scalar, so it correctly declines the
                // per-slot path -- but that decision is about the RESULT, and
                // lowering the base with `lower_expr` packs it (`safe_nav_field_
                // value` -> `materialize_embed`). Take the base as a repr instead
                // and read the field off its leaves. The presence tag replaces
                // the `== nil` test: there is no box to compare.
                if let Some(r) = self.safe_nav_group_base(base, field)? {
                    return Ok(r);
                }
                let bv = self.lower_expr(base)?;
                let nil = self.emit_cur(Inst::Const(Lit::Nil));
                let is_nil = self.emit_cur(Inst::Eq(bv, nil));
                let result = self.f.new_cell(None);
                let nil_bb = self.f.new_block();
                let val_bb = self.f.new_block();
                let join = self.f.new_block();
                self.f.set_term(self.cur, Term::Branch { cond: is_nil, then_bb: nil_bb, else_bb: val_bb });

                self.cur = nil_bb;
                let n = self.emit_cur(Inst::Const(Lit::Nil));
                self.store_and_join(result, n, join);

                self.cur = val_bb;
                let fv = self.safe_nav_field_value(e, base, bv, field)?;
                self.store_and_join(result, fv, join);

                self.cur = join;
                Ok(self.emit_cur(Inst::Load(result)))
            }

            Expr::Match { scrutinee, arms } => self.lower_match(scrutinee, arms),
            Expr::IfLet {
                pattern,
                value,
                then_branch,
                else_branch,
            } => self.lower_if_let(pattern, value, then_branch, else_branch),

            Expr::IncDec { target, decrement, postfix } => {
                self.lower_incdec(target, decrement, postfix)
            }

            // A range used as a VALUE (not a `for` iterable — `lower_for`
            // intercepts that) materializes to the stdlib Vec-shaped `__range`.
            Expr::Range { low, high } => {
                let is_uint = matches!(self.arith_ty(low), Some(ArithTy::Uint))
                    || matches!(self.arith_ty(high), Some(ArithTy::Uint));
                let lo = self.lower_expr(low)?;
                let hi = self.lower_expr(high)?;
                let func = self.resolve_global_fn(if is_uint { "__range_uint" } else { "__range" })?;
                Ok(self.emit_cur(Inst::Call { func, args: vec![lo, hi] }))
            }

            // `return e` in expression position (`x ?? return`, `cond ? a : return`).
            // Diverges: run defers, set the Return terminator. The block is now
            // closed, so callers skip any fall-through (via `is_open()`); the
            // placeholder value is never used.
            Expr::Return(opt) => {
                let leaves = self.return_leaves(opt)?;
                self.run_all_defers()?;
                self.f.set_term(self.cur, Term::Return(leaves));
                Ok(self.emit_cur(Inst::Const(Lit::Nil)))
            }

            Expr::SafeNavCall { receiver, method, args } => {
                self.lower_safe_nav_call(e, receiver, method, args)
            }
            Expr::SafeNavLet { receiver, binding, body } => {
                self.lower_safe_nav_let(receiver, binding, body)
            }

            other => Err(gap(format!("expression form not in Phase 1: {}", expr_tag(&other)))),
        }
    }

    /// `target = value` -> a `Store`/`StoreField`/`StoreIndex`/`StoreGlobal`
    /// (the effect; no value). The expression form wraps this with a unit value.
    /// The BASE of an assignment target, lowered as a PLACE: the slot itself,
    /// never a packed copy of it. `lower_expr` on a value-struct element/field is
    /// an RVALUE read and rebuilds a fresh box (the whole-value copy rule), so
    /// using it here would land the store on a temporary. The syntactic position
    /// ("I am an assignment target's base") is known here, so nothing needs to be
    /// plumbed down from bone. Recurses the whole chain: `a.b[i].c.x = 5` must not
    /// pack at any level.
    pub(crate) fn lower_place_base(&mut self, e: Idx<Expr>) -> Result<Idx<Inst>, CompilerError> {
        let nobox_global_place = self.nobox_global_place(e)?;
        let flat_field = match self.body.expr_arena[e].clone() {
            Expr::FieldAccess { base, field } if self.swizzle_of(base, field).is_none() => match self.expr_ty(base) {
                Some(TypeExpr::App { ctor: TypeCtor::Nominal(sname), .. }) =>
                    Some(self.struct_field_plan(sname, field, false)?),
                _ => None,
            },
            _ => None,
        };
        match self.body.expr_arena[e].clone() {
            Expr::Index { base, index } => {
                let (b, kind) = self.lower_index_base(base)?;
                let i = self.lower_expr(index)?;
                Ok(self.emit_cur(Inst::Index { base: b, index: i, kind }))
            }
            // A `nobox` global has no single slot -- `Inst::Global(name)` would
            // read a nil global and the write would land on nothing. Every
            // reachable place form (whole group, leaf field) is handled before
            // this; reject rather than emit that silent nil.
            Expr::Global(name) if nobox_global_place.is_some() => {
                Err(CompilerError::new_with_label(
                    self.body.expr_spans[e],
                    format!(
                        "`{name}` is a `nobox` global, which is stored as leaves and has no \
                         slot this assignment target can write through"
                    ),
                ))
            }
            Expr::Global(name) => Ok(self.emit_cur(Inst::Global(name))),
            // A swizzle and a flattened-enum field are not places (both
            // materialize a fresh value); leave them to the rvalue path, which
            // rejects or copies as it does today.
            Expr::FieldAccess { base, field }
                if self.swizzle_of(base, field).is_none()
                    && !flat_field.as_ref().is_some_and(|p| p.flat_shape().is_some()) =>
            {
                let (pbase, key) = self.physical_field(base, field)?;
                let b = self.lower_place_base(pbase)?;
                Ok(self.emit_cur(Inst::Field { base: b, field: key }))
            }
            // A call returning a VALUE is not storage: the rvalue lowering packs a
            // fresh box, so the store lands on a temporary and emits
            // `(Pt_ct(a, b)).x = v` -- not valid Lua, so the whole chunk fails to
            // parse at load. bone rejects the reachable form (`1dd209b65`), so
            // this is unreachable by construction; it rejects rather than falls
            // back so a future admission change surfaces here instead of as an
            // unparseable chunk.
            //
            // A call returning a REFERENCE struct IS storage -- it hands back the
            // shared table, so the write is real. `o.next.unwrap().value = v` is
            // the live shape (`tests/lambda_stress_tests.rock`).
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

    pub(crate) fn lower_assign_effect(
        &mut self,
        target: Idx<Expr>,
        value: Idx<Expr>,
    ) -> Result<(), CompilerError> {
        let nobox_global_place = self.nobox_global_place(target)?;
        match self.body.expr_arena[target].clone() {
            // A threaded (group_binds) var -- a scalarized flat-enum local or value
            // param -- updates its leaf cells, not one boxed cell.
            Expr::Var(b) if self.group_binds.contains_key(&raw(b)) => {
                let (cells, shape) = self.group_binds.get(&raw(b)).cloned().unwrap();
                let r = self.lower_expr_repr(value)?;
                let leaves = self.repr_to_leaves_bundle_for_shape(&r, &shape)?;
                for (c, lv) in cells.iter().zip(leaves) {
                    self.emit_cur(Inst::Store { cell: *c, value: lv });
                }
            }
            // The tuple twin: a name bound to a splatting tuple's WIRE updates its
            // slot cells (presence included), never one boxed cell.
            Expr::Var(b) if self.tuple_binds.contains_key(&raw(b)) => {
                let (cells, shape) = self.tuple_binds.get(&raw(b)).cloned().unwrap();
                let r = self.lower_expr_repr(value)?;
                let slots = self.tuple_repr_to_slots(r.into_value(), &shape)?;
                for (c, sv) in cells.iter().zip(slots) {
                    self.emit_cur(Inst::Store { cell: *c, value: sv });
                }
            }
            Expr::Var(b) if self.static_cells.contains_key(&raw(b)) => {
                let name = self.static_cells[&raw(b)];
                let v = self.lower_expr(value)?;
                self.emit_cur(Inst::StoreGlobal { name, value: v });
            }
            Expr::Var(b) => {
                let cell = *self
                    .binds
                    .get(&raw(b))
                    .ok_or_else(|| gap("assignment to an unbound variable"))?;
                let v = self.lower_expr(value)?;
                let v = self.coerced_to_binding(v, value, b);
                self.emit_cur(Inst::Store { cell, value: v });
            }
            // A leaf of a `nobox` global (`SPOT.x = v`, `SPOT.a.b = v`): the
            // group has no slot to take a place base of, so the write lands in
            // the leaf global itself.
            Expr::FieldAccess { .. } if nobox_global_place.is_some() => {
                let (leaves, shape) = nobox_global_place.clone().expect("guard matched");
                self.nobox_global_store(&leaves, shape, value)?;
            }
            Expr::FieldAccess { base, field } => {
                // `xs[i].x = v` on a column-stored Vec: there is no element slot
                // to take a place base of, so the write lands directly in the
                // leaf's own column.
                if let Some((vec, idx, elem)) = self.column_index_place(base)? {
                    return self.column_leaf_store(vec, idx, elem, field, value);
                }
                // `buf[i].pos = v` / `buf[i].id = v` / `buf[i].lo.n = v` on a
                // leaf-addressed NativeBuffer: there is no element slot to take a
                // place base of, so the write lands in the leaves under this
                // field's path. A whole nested field spans several leaves; a
                // primitive field is exactly one.
                if let Some((buf, idx, elem, path)) = self.nb_leaf_field_place(base, field)? {
                    return self.nb_leaf_field_store(buf, idx, elem, &path, value);
                }
                // A whole embedded field (`p.f = Foo { .. }`): the child has no
                // slot of its own, so write its leaves. Composition happens
                // BEFORE this, so `p.f` here is the child, not a physical key.
                let embed_plan = match self.expr_ty(base) {
                    Some(TypeExpr::App { ctor: TypeCtor::Nominal(sname), .. })
                        if self.swizzle_of(base, field).is_none() =>
                        Some(self.struct_field_plan(sname, field, false)?),
                    _ => None,
                };
                if let Some((cname, _)) = embed_plan.as_ref().and_then(|p| p.embed()) {
                    let b = self.lower_place_base(base)?;
                    self.store_embed(b, base, field, cname, value)?;
                    return Ok(());
                }
                // An embed LEAF write composes to one dotted key on the parent
                // that holds it (`h.p.x = 9` -> `h["p.x"] = 9`).
                let (pbase, key) = self.physical_field(base, field)?;
                let b = self.lower_place_base(pbase)?;
                // A flattened enum field: write the discriminant + payload slots
                // directly on the parent table (no nested box).
                let flat_plan = match self.expr_ty(base) {
                    Some(TypeExpr::App { ctor: TypeCtor::Nominal(sname), .. })
                        if self.swizzle_of(base, field).is_none() =>
                        Some(self.struct_field_plan(sname, field, false)?),
                    _ => None,
                };
                if let Some(shape) = flat_plan.and_then(|p| p.flat_shape()) {
                    let cells = self.lower_flat_value(&shape, value)?;
                    self.store_flat_cells(b, key, cells);
                    return Ok(());
                }
                let v = self.lower_expr(value)?;
                self.emit_cur(Inst::StoreField {
                    base: b,
                    field: key,
                    value: v,
                });
            }
            // `target` IS the `Expr::Index` node, and that is the expression
            // the graph walk keys this arm's synthesized obligations on
            // (`INDEX_TRIGGERED`). The place expression is never lowered as a
            // value, so nothing else installs it: in statement position the
            // cursor is absent entirely and in value position it names the
            // enclosing assignment.
            Expr::Index { base, index } => self.with_site_expr(target, |s| {
                let (b, kind) = s.lower_index_base(base)?;
                let i = s.lower_expr(index)?;
                // Column storage has no element slot: the write goes leaf-wise
                // into K columns, so the value is consumed as a repr and never
                // boxed. Mirrors `column_element_repr` on the read side.
                if let ContainerKind::VecValueColumns(elem) = kind {
                    return s.column_element_store(b, i, value, elem);
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
                s.emit_cur(Inst::StoreIndex {
                    base: b,
                    index: i,
                    value: v,
                    kind,
                });
                Ok(())
            })?,
            Expr::Global(_) if nobox_global_place.is_some() => {
                let (leaves, shape) = nobox_global_place.clone().expect("guard matched");
                self.nobox_global_store(&leaves, shape, value)?;
            }
            Expr::Global(name) => {
                let v = self.lower_expr(value)?;
                self.emit_cur(Inst::StoreGlobal { name, value: v });
            }
            // `base?.field = value` — silently skip if any `?.` link is null (the
            // receiver `base` lowers to nil then). `value` evaluates ONLY on the
            // non-null path. Deep `a?.b?.c = v` works: `base` (`a?.b`) is itself
            // a SafeNav that lowers to nil if any inner link is null.
            Expr::SafeNav { base, field } => {
                let recv = self.lower_place_base(base)?;
                let nil = self.emit_cur(Inst::Const(Lit::Nil));
                let is_nil = self.emit_cur(Inst::Eq(recv, nil));
                let store_bb = self.f.new_block();
                let join = self.f.new_block();
                self.f.set_term(self.cur, Term::Branch { cond: is_nil, then_bb: join, else_bb: store_bb });
                self.cur = store_bb;
                let v = self.lower_expr(value)?;
                self.emit_cur(Inst::StoreField { base: recv, field, value: v });
                self.f.set_term(self.cur, Term::Goto(join));
                self.cur = join;
            }
            _ => return Err(gap("assignment to this target form (later)")),
        }
        Ok(())
    }

    /// `target <op>= value` == `target = target <op> value` (the effect), over
    /// var / global / field / index targets. The place is read and written
    /// through a SINGLE base/index evaluation (no double side effects). A scalar
    /// target uses native arith; a non-scalar target (struct/value with an
    /// operator overload) routes `op_fn(target, value)` through the resolved
    /// overload (bone records it on the CompoundAssign expr).
    pub(crate) fn lower_compound_assign_effect(
        &mut self,
        e: Idx<Expr>,
        target: Idx<Expr>,
        op_fn: Ustr,
        value: Idx<Expr>,
    ) -> Result<(), CompilerError> {
        let nobox_global_place = self.nobox_global_place(target)?;
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
        match self.body.expr_arena[target].clone() {
            Expr::Var(b) if self.static_cells.contains_key(&raw(b)) => {
                let name = self.static_cells[&raw(b)];
                let cur = self.emit_cur(Inst::Global(name));
                let rv = self.lower_expr(value)?;
                let new_v = self.compound_combine(scalar, op_fn, overload, cur, rv)?;
                self.emit_cur(Inst::StoreGlobal { name, value: new_v });
            }
            Expr::Var(b) => {
                let cell = *self
                    .binds
                    .get(&raw(b))
                    .ok_or_else(|| gap("compound-assign to an unbound variable"))?;
                let cur = self.emit_cur(Inst::Load(cell));
                let rv = self.lower_expr(value)?;
                let new_v = self.compound_combine(scalar, op_fn, overload, cur, rv)?;
                self.emit_cur(Inst::Store { cell, value: new_v });
            }
            // A `nobox` global is stored as leaves, so neither the whole-group
            // nor the leaf form has a slot the arms below can read-modify-write
            // through. bone desugars `t <op>= v` to `t = t <op> v` before skull
            // sees it, so this is unreachable by construction (proved by
            // `error`-ing the arm over the full surface: zero fires). It rejects
            // rather than falling through so a future routing change surfaces
            // here instead of as a silent read of a nil global.
            _ if nobox_global_place.is_some() => {
                return Err(gap("compound assignment reached a `nobox` global place"))
            }
            Expr::Global(name) => {
                let cur = self.emit_cur(Inst::Global(name));
                let rv = self.lower_expr(value)?;
                let new_v = self.compound_combine(scalar, op_fn, overload, cur, rv)?;
                self.emit_cur(Inst::StoreGlobal { name, value: new_v });
            }
            Expr::FieldAccess { base, field } => {
                if let Some((buf, checked, _, place)) = self.nb_leaf_rmw_place(target)? {
                    let cur = self.nb_leaf_load(buf, checked, &place);
                    let rv = self.lower_expr(value)?;
                    let new_v = self.compound_combine(scalar, op_fn, overload, cur, rv)?;
                    self.nb_leaf_store(buf, checked, &place, new_v);
                    return Ok(());
                }
                let (pbase, key) = self.physical_field(base, field)?;
                let b = self.lower_place_base(pbase)?;
                let cur = self.emit_cur(Inst::Field { base: b, field: key });
                let rv = self.lower_expr(value)?;
                let new_v = self.compound_combine(scalar, op_fn, overload, cur, rv)?;
                self.emit_cur(Inst::StoreField { base: b, field: key, value: new_v });
            }
            Expr::Index { base, index } => {
                let (b, kind) = self.lower_index_base(base)?;
                let i = self.lower_expr(index)?;
                let cur = self.emit_cur(Inst::Index { base: b, index: i, kind });
                let rv = self.lower_expr(value)?;
                let new_v = self.compound_combine(scalar, op_fn, overload, cur, rv)?;
                self.emit_cur(Inst::StoreIndex { base: b, index: i, value: new_v, kind });
            }
            // `base?.field <op>= value` — skip silently if any `?.` link is null.
            // The read-modify-write and `value` run only on the non-null path.
            Expr::SafeNav { base, field } => {
                let recv = self.lower_place_base(base)?;
                let nil = self.emit_cur(Inst::Const(Lit::Nil));
                let is_nil = self.emit_cur(Inst::Eq(recv, nil));
                let store_bb = self.f.new_block();
                let join = self.f.new_block();
                self.f.set_term(self.cur, Term::Branch { cond: is_nil, then_bb: join, else_bb: store_bb });
                self.cur = store_bb;
                let cur = self.emit_cur(Inst::Field { base: recv, field });
                let rv = self.lower_expr(value)?;
                let new_v = self.compound_combine(scalar, op_fn, overload, cur, rv)?;
                self.emit_cur(Inst::StoreField { base: recv, field, value: new_v });
                self.f.set_term(self.cur, Term::Goto(join));
                self.cur = join;
            }
            _ => return Err(gap("compound-assign to this target form (later)")),
        }
        Ok(())
    }

    /// `cur <op> rv`: native arith for a scalar target, else the resolved
    /// operator-overload call `op_fn(cur, rv)`.
    pub(crate) fn compound_combine(
        &mut self,
        scalar: Option<(ArithTy, ArithTy)>,
        op_fn: Ustr,
        overload: Option<FuncRef>,
        cur: Idx<Inst>,
        rv: Idx<Inst>,
    ) -> Result<Idx<Inst>, CompilerError> {
        match scalar {
            Some((lt, rt)) => {
                let inst = arith_inst(op_fn.as_str(), lt, rt, cur, rv)?;
                Ok(self.emit_cur(inst))
            }
            None => Ok(self.emit_cur(Inst::Call {
                func: overload.expect("non-scalar compound-assign resolved an overload"),
                args: vec![cur, rv],
            })),
        }
    }

    /// `target++` / `target--` (prefix or postfix). Reads the target, adds or
    /// subtracts one with the target's scalar instruction family, writes it back,
    /// and yields the OLD value (postfix) or NEW value (prefix). Var / global /
    /// field / index targets, mirroring assignment.
    pub(crate) fn lower_incdec(
        &mut self,
        target: Idx<Expr>,
        decrement: bool,
        postfix: bool,
    ) -> Result<Idx<Inst>, CompilerError> {
        let nobox_global_place = self.nobox_global_place(target)?;
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
            s.emit_cur(inst)
        };
        match self.body.expr_arena[target].clone() {
            Expr::Var(b) if self.static_cells.contains_key(&raw(b)) => {
                let name = self.static_cells[&raw(b)];
                let old = self.emit_cur(Inst::Global(name));
                let new = bump(self, old);
                self.emit_cur(Inst::StoreGlobal { name, value: new });
                Ok(if postfix { old } else { new })
            }
            Expr::Var(b) => {
                let cell = *self
                    .binds
                    .get(&raw(b))
                    .ok_or_else(|| gap("inc/dec of an unbound variable"))?;
                let old = self.emit_cur(Inst::Load(cell));
                let new = bump(self, old);
                self.emit_cur(Inst::Store { cell, value: new });
                Ok(if postfix { old } else { new })
            }
            // A primitive leaf of a `nobox` global (`SPOT.x++`) is its own global.
            // `arith_ty` already restricted the target to a scalar, so a whole
            // group can never reach here.
            _ if matches!(nobox_global_place.as_ref(), Some((_, None))) => {
                let (leaves, _) = nobox_global_place.expect("guard matched");
                let name = leaves[0];
                let old = self.emit_cur(Inst::Global(name));
                let new = bump(self, old);
                self.emit_cur(Inst::StoreGlobal { name, value: new });
                Ok(if postfix { old } else { new })
            }
            Expr::Global(name) => {
                let old = self.emit_cur(Inst::Global(name));
                let new = bump(self, old);
                self.emit_cur(Inst::StoreGlobal { name, value: new });
                Ok(if postfix { old } else { new })
            }
            Expr::FieldAccess { base, field } => {
                if let Some((buf, checked, _, place)) = self.nb_leaf_rmw_place(target)? {
                    let old = self.nb_leaf_load(buf, checked, &place);
                    let new = bump(self, old);
                    self.nb_leaf_store(buf, checked, &place, new);
                    return Ok(if postfix { old } else { new });
                }
                let (pbase, key) = self.physical_field(base, field)?;
                let b = self.lower_place_base(pbase)?;
                let old = self.emit_cur(Inst::Field { base: b, field: key });
                let new = bump(self, old);
                self.emit_cur(Inst::StoreField { base: b, field: key, value: new });
                Ok(if postfix { old } else { new })
            }
            Expr::Index { base, index } => {
                let (b, kind) = self.lower_index_base(base)?;
                let i = self.lower_expr(index)?;
                let old = self.emit_cur(Inst::Index { base: b, index: i, kind });
                let new = bump(self, old);
                self.emit_cur(Inst::StoreIndex { base: b, index: i, value: new, kind });
                Ok(if postfix { old } else { new })
            }
            _ => Err(gap("inc/dec of a non-assignable target")),
        }
    }

    /// The non-nil arm of `recv?.method(args)`, with the receiver already
    /// lowered. Returns a plan-owned bundle: a scalar-ABI value callee yields its leaves, so a
    /// per-slot join never has to take a box apart again. Shared by the scalar
    /// lowering and the repr twin -- one value arm, two joins.
    pub(crate) fn safe_nav_call_value_repr(
        &mut self,
        e: Idx<Expr>,
        receiver: Idx<Expr>,
        method: Ustr,
        args: IdxRange<Expr>,
        recv: Idx<Inst>,
    ) -> Result<ValueBundle, CompilerError> {
        let value = if self.struct_field_is_fn(receiver, method) {
            // `recv?.field(args)` on a fn-typed FIELD: nil-guarded indirect call
            // `(recv.field)(args)` — the field IS the callee, no receiver arg.
            let fieldv = self.emit_cur(Inst::Field { base: recv, field: method });
            let mut argv = Vec::new();
            for a in args {
                argv.push(self.lower_expr(a)?);
            }
            let shape = self.peeled_tuple_shape(e)?;
            ValueRepr::Scalar(self.emit_call_value_n(shape, fieldv, argv)?)
        } else {
            // `recv?.method(args)` UFCS: `method(recv, args)`, recv as arg 0.
            // The whole call -- both ABIs, the delivery key, the bound tree, the
            // callee instance and the target ABI plan -- is the ordinary direct
            // call's business, so it happens there rather than being reassembled
            // here from the DECLARATION's plan.
            let func = self.resolve_funcref(e, method)?;
            let arg_idxs: Vec<Idx<Expr>> = args.into_iter().collect();
            self.ufcs_call_repr(e, func, recv, &arg_idxs)?
        };
        self.value_bundle_for_expr(e, value)
    }

    /// [`Self::lower_safe_nav_call`] joining PER SLOT.
    pub(crate) fn safe_nav_call_repr(
        &mut self,
        e: Idx<Expr>,
        receiver: Idx<Expr>,
        method: Ustr,
        args: IdxRange<Expr>,
        shape: RetSlot,
    ) -> Result<ValueRepr, CompilerError> {
        self.safe_nav_repr(receiver, shape, move |s, recv| {
            s.safe_nav_call_value_repr(e, receiver, method, args, recv)
        })
    }

    fn pack_safe_nav_call_value(
        &mut self,
        e: Idx<Expr>,
        receiver: Idx<Expr>,
        method: Ustr,
        args: IdxRange<Expr>,
        recv: Idx<Inst>,
    ) -> Result<Idx<Inst>, CompilerError> {
        let r = self.safe_nav_call_value_repr(e, receiver, method, args, recv)?;
        Ok(self.pack(r.into_value())?)
    }

    /// `recv?.method(args)` — `nil` if `recv` is nil, else the UFCS call
    /// `method(recv, args)` (cdata-everywhere: no scalar splat). Args evaluate
    /// only on the non-null path. Result flows through a cell.
    /// Lower a safe-nav RECEIVER once, keeping whatever tells it apart from nil.
    ///
    /// An unboxed nullable value carries its own presence tag, and that tag is
    /// the only honest absence test: packing the Group to compare it against nil
    /// builds a ctor, which is statically non-nil, so the absent arm gets
    /// const-folded away and the miss path runs the hit code on nil leaves. The
    /// box cannot be moved into `pack` instead -- `pack` cannot see whether its
    /// consumer needs nil-ness, and putting the ctor behind a branch there costs
    /// SROA the forwarding that keeps `??`-at-a-ctor-field boxless.
    pub(crate) fn lower_safe_nav_receiver(
        &mut self,
        receiver: Idx<Expr>,
    ) -> Result<SafeNavRecv, CompilerError> {
        // Asked of the REPR, not of a `nobox` shape query: this is not a nobox
        // rule. A plain unmarked `value` read out of a `Map`/`Vec`/embed field
        // arrives as a Group with a presence tag too, and it is the unmarked case
        // that crashed. `lower_expr_repr` is a superset of `lower_expr` -- every
        // form it does not special-case falls through to `Scalar(lower_expr(e))`
        // -- so nothing outside the Group case changes shape.
        let r = self.lower_expr_repr(receiver)?.into_value();
        if let ValueRepr::Group { present: Some(p), .. } = &r {
            return Ok(SafeNavRecv::Unboxed { repr: r.clone(), present: *p });
        }
        Ok(SafeNavRecv::Boxed(self.pack(r)?))
    }

    /// The "receiver is absent" branch condition for [`SafeNavRecv`].
    pub(crate) fn safe_nav_absent_cond(&mut self, r: &SafeNavRecv) -> Idx<Inst> {
        match r {
            SafeNavRecv::Boxed(v) => {
                let nil = self.emit_cur(Inst::Const(Lit::Nil));
                self.emit_cur(Inst::Eq(*v, nil))
            }
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
        let is_nil = self.safe_nav_absent_cond(&recv);
        let result = self.f.new_cell(None);
        let nil_bb = self.f.new_block();
        let call_bb = self.f.new_block();
        let join = self.f.new_block();
        self.f.set_term(self.cur, Term::Branch { cond: is_nil, then_bb: nil_bb, else_bb: call_bb });

        self.cur = nil_bb;
        let n = self.emit_cur(Inst::Const(Lit::Nil));
        self.store_and_join(result, n, join);

        self.cur = call_bb;
        let recv_v = self.safe_nav_present_value(recv)?;
        let cv = self.pack_safe_nav_call_value(e, receiver, method, args, recv_v)?;
        self.store_and_join(result, cv, join);

        self.cur = join;
        Ok(self.emit_cur(Inst::Load(result)))
    }

    /// The non-nil arm of `base?.field`, with the receiver already lowered to
    /// `bv`. Shared by the scalar lowering and the per-slot repr twin so the two
    /// cannot drift about what a field read IS -- and so the receiver is lowered
    /// exactly once either way.
    /// `Some` when `base` denotes a NULLABLE `nobox` value, so `base?.field` can
    /// read the field off the base's leaves instead of a box.
    ///
    /// This is the chained `?.`: `h?.v?.x` where `h?.v` is `NV?`. The outer node
    /// asks `repr_join_slot` about ITS OWN type (`int?`, no leaves) and rightly
    /// declines the per-slot join -- but the base still has to get there, and
    /// `lower_expr` on it takes the scalar arm, whose embed read rebuilds the box
    /// the mark forbids. Asking the BASE the same question is what routes it
    /// through `safe_nav_field_repr`.
    ///
    /// The base's presence tag replaces the `== nil` test: once the value is
    /// leaves there is no box to compare against nil, and its leaves are nil on
    /// the absent row whether or not the tag is.
    fn safe_nav_group_base(
        &mut self,
        base: Idx<Expr>,
        field: Ustr,
    ) -> Result<Option<Idx<Inst>>, CompilerError> {
        // Only a NULLABLE base: a non-nullable one has no presence tag, and `?.`
        // on it is already dead (bone rejects the redundant form).
        if !self.repr_join_slot(base)?.is_some_and(
            |s| matches!(s, RetSlot::Leaves(sh) if sh.nullable),
        ) {
            return Ok(None);
        }
        let ValueRepr::Group { name, parts, present: Some(present) } =
            self.lower_expr_repr(base)?.into_value()
        else {
            return Ok(None);
        };
        let Some(i) = self.field_index(name, field) else {
            return Ok(None);
        };
        let part = parts[i].clone();
        let result = self.f.new_cell(None);
        let absent_bb = self.f.new_block();
        let val_bb = self.f.new_block();
        let join = self.f.new_block();
        self.f.set_term(
            self.cur,
            Term::Branch { cond: present, then_bb: val_bb, else_bb: absent_bb },
        );

        self.cur = absent_bb;
        let n = self.emit_cur(Inst::Const(Lit::Nil));
        self.store_and_join(result, n, join);

        self.cur = val_bb;
        let fv = self.pack(part)?;
        self.store_and_join(result, fv, join);

        self.cur = join;
        Ok(Some(self.emit_cur(Inst::Load(result))))
    }

    pub(crate) fn safe_nav_field_value(
        &mut self,
        e: Idx<Expr>,
        base: Idx<Expr>,
        bv: Idx<Inst>,
        field: Ustr,
    ) -> Result<Idx<Inst>, CompilerError> {
        let embed_plan = if field.parse::<usize>().is_ok() || self.expr_is_newtype(base) {
            None
        } else {
            match self.expr_ty(base) {
                Some(TypeExpr::App { ctor: TypeCtor::Nominal(sname), .. })
                    if self.swizzle_of(base, field).is_none() =>
                    Some(self.struct_field_plan(sname, field, false)?),
                _ => None,
            }
        };
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
                    let parent = if pbase == base { bv } else { self.lower_expr(pbase)? };
                    if self.embed_read_is_nullable(base, field)? {
                        self.materialize_embed_nullable(parent, key, cname)?
                    } else {
                        self.materialize_embed(parent, key, cname)?
                    }
                }
                // `o?.p` reads a value field whole out of a mutable place, so
                // it copies exactly as `o.p` does.
                None => {
                    let fv = self.emit_cur(Inst::Field { base: bv, field });
                    let copy_name = match self.value_struct_name_of(e) {
                        Some(name) if self.is_mutable_place(base)? => Some(name),
                        _ => None,
                    };
                    match copy_name {
                        Some(name) if self.embed_read_is_nullable(base, field)? => {
                            self.nullable_boxed_value_copy(fv, name)?
                        }
                        Some(name) => {
                            let copy = self.boxed_value_copy_repr(fv, name)?;
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
        binding: Idx<bone_semantic::hir::BindingInfo>,
        body: Idx<Expr>,
        shape: RetSlot,
    ) -> Result<ValueRepr, CompilerError> {
        self.safe_nav_repr(receiver, shape, |s, recv| {
            let cell = s.f.new_cell(Some(s.body.binding_arena[binding].name));
            s.emit_cur(Inst::Store { cell, value: recv });
            s.binds.insert(raw(binding), cell);
            s.lower_expr_repr(body)
        })
    }

    /// [`Expr::SafeNav`] joining PER SLOT. Only the `nobox`/embed field read
    /// differs from the scalar arm: it builds the child's Group off the parent's
    /// dotted leaves instead of materializing a box from them, which is the whole
    /// point -- `materialize_embed` IS the packing this path had to do to fit one
    /// cell. Every other field flavour packs as before and rides one slot.
    pub(crate) fn safe_nav_field_repr(
        &mut self,
        e: Idx<Expr>,
        base: Idx<Expr>,
        field: Ustr,
        shape: RetSlot,
    ) -> Result<ValueRepr, CompilerError> {
        let plan = if field.parse::<usize>().is_ok() || self.expr_is_newtype(base) {
            None
        } else {
            match self.expr_ty(base) {
                Some(TypeExpr::App { ctor: TypeCtor::Nominal(sname), .. })
                    if self.swizzle_of(base, field).is_none() =>
                    Some(self.struct_field_plan(sname, field, false)?),
                _ => None,
            }
        };
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
            let parent = if pbase == base { bv } else { s.lower_expr(pbase)? };
            let mut g = s.embed_group(parent, key, &vs);
            if nullable_child {
                // A nullable child carries presence in the field's own undotted
                // key; the Group takes it rather than gating the read, so an
                // absent child's leaves are nil and consumers branch on presence.
                let present = s.emit_cur(Inst::Field { base: parent, field: key });
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
        binding: Idx<bone_semantic::hir::BindingInfo>,
        body: Idx<Expr>,
    ) -> Result<Idx<Inst>, CompilerError> {
        let recv = self.lower_safe_nav_receiver(receiver)?;
        let is_nil = self.safe_nav_absent_cond(&recv);
        let result = self.f.new_cell(None);
        let nil_bb = self.f.new_block();
        let let_bb = self.f.new_block();
        let join = self.f.new_block();
        self.f.set_term(self.cur, Term::Branch { cond: is_nil, then_bb: nil_bb, else_bb: let_bb });

        self.cur = nil_bb;
        let n = self.emit_cur(Inst::Const(Lit::Nil));
        self.store_and_join(result, n, join);

        self.cur = let_bb;
        let recv_v = self.safe_nav_present_value(recv)?;
        let cell = self.f.new_cell(Some(self.body.binding_arena[binding].name));
        self.emit_cur(Inst::Store { cell, value: recv_v });
        self.binds.insert(raw(binding), cell);
        let bv = self.lower_expr(body)?;
        self.store_and_join(result, bv, join);

        self.cur = join;
        Ok(self.emit_cur(Inst::Load(result)))
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
        let mut columns: Vec<(Ustr, Vec<Idx<Inst>>)> =
            paths.iter().map(|p| (*p, Vec::with_capacity(elems.len()))).collect();
        for &e in elems {
            let repr = self.lower_expr_repr(e)?;
            let leaves = self.repr_to_storage_leaves(&repr, &shape, "column literal")?;
            if leaves.len() != paths.len() {
                return Err(gap(format!(
                    "column literal arity mismatch for [{}]: {} leaves vs {} columns",
                    elem.name,
                    leaves.len(),
                    paths.len()
                )));
            }
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

    pub(crate) fn lower_array_literal(
        &mut self,
        lit: Idx<Expr>,
        elems: IdxRange<Expr>,
    ) -> Result<Idx<Inst>, CompilerError> {
        let idxs: Vec<Idx<Expr>> = elems.into_iter().collect();
        let has_spread = idxs
            .iter()
            .any(|&e| matches!(self.body.expr_arena[e], Expr::Spread(_)));
        if !has_spread {
            // Column storage: transpose the literal's elements into one array
            // per leaf. Each element lowers as a repr, so no element box is
            // built even for a literal with entries.
            if let Some(elem) = self.vec_column_elem_of(lit)? {
                return self.column_vec_literal(&idxs, elem);
            }
            let vs: Vec<Idx<Inst>> = idxs.iter().map(|&e| self.lower_expr(e)).collect::<Result<_, _>>()?;
            return Ok(self.emit_cur(Inst::MakeVec(vs)));
        }
        if let Some(elem) = self.vec_column_elem_of(lit)? {
            let capacity = self.emit_cur(Inst::Const(Lit::Int(0)));
            let arr = self.empty_column_vec(elem, capacity)?;
            for &e in &idxs {
                if let Expr::Spread(inner) = self.body.expr_arena[e] {
                    self.spread_into(arr, inner, SpreadDest::Columns(elem))?;
                } else {
                    let bundle = self.lower_expr_repr(e)?;
                    self.column_append_bundle(arr, &bundle, elem)?;
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
                self.emit_cur(Inst::ArrayPush { arr, value: v, kind: ContainerKind::Vec });
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
            SpreadDest::Columns(elem) => self.column_append_bundle(arr, &bundle, elem),
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
            SpreadDest::Columns(elem) => self.append_spread_bundle(
                arr,
                ValueBundle::new(elem.representation(), value),
                dest,
            ),
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
        if let (SpreadDest::Columns(elem), Expr::Tuple(elements)) =
            (dest, self.body.expr_arena[inner])
        {
            for element in elements {
                let bundle = self.lower_expr_repr(element)?;
                self.column_append_bundle(arr, &bundle, elem)?;
            }
            return Ok(());
        }
        let v = self.lower_expr(inner)?;
        if let Some(ty) = self.concrete_expr_ty(inner) {
            return match ty.peel_nullable().0 {
                crate::monoty::MonoTy::Tuple(elements) => {
                    for (index, _) in elements.into_iter().enumerate() {
                        let value = self.emit_cur(Inst::TupleIndex { base: v, index });
                        self.append_spread_value(arr, ValueRepr::Scalar(value), dest)?;
                    }
                    Ok(())
                }
                crate::monoty::MonoTy::App {
                    ctor: crate::monoty::MonoCtor::Array | crate::monoty::MonoCtor::Map,
                    ..
                } => self.spread_array_loop(arr, inner, v, dest),
                crate::monoty::MonoTy::App {
                    ctor: crate::monoty::MonoCtor::Nominal(name), ..
                } => {
                    let fields = self
                        .struct_field_names(*name)
                        .ok_or_else(|| gap("spread of a non-struct nominal type"))?;
                    for field in fields {
                        let value = self.emit_cur(Inst::Field { base: v, field });
                        self.append_spread_value(arr, ValueRepr::Scalar(value), dest)?;
                    }
                    Ok(())
                }
                _ => Err(gap("spread of an unsupported type")),
            };
        }
        Err(gap("spread expression is missing from the concrete type plan"))
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
        let n = self.emit_cur(Inst::Len { base: src, kind: src_kind });
        let i_cell = self.f.new_cell(None);
        let zero = self.emit_cur(Inst::Const(Lit::Int(0)));
        self.emit_cur(Inst::Store { cell: i_cell, value: zero });

        let header = self.f.new_block();
        let body = self.f.new_block();
        let exit = self.f.new_block();
        self.f.set_term(self.cur, Term::Goto(header));

        self.cur = header;
        let i = self.emit_cur(Inst::Load(i_cell));
        let cmp = self.emit_cur(Inst::IntLt(i, n));
        self.f.set_term(self.cur, Term::Branch { cond: cmp, then_bb: body, else_bb: exit });

        self.cur = body;
        let i_now = self.emit_cur(Inst::Load(i_cell));
        let repr = self.loop_element_repr_at(src, i_now, src_kind)?;
        self.append_spread_value(arr, repr, dest)?;
        let i2 = self.emit_cur(Inst::Load(i_cell));
        let one = self.emit_cur(Inst::Const(Lit::Int(1)));
        let inc = self.emit_cur(Inst::IntAdd(i2, one));
        self.emit_cur(Inst::Store { cell: i_cell, value: inc });
        self.f.set_term(self.cur, Term::Goto(header));

        self.cur = exit;
        Ok(())
    }
}
