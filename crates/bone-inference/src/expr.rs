use super::*;

pub(crate) fn infer_statement(c: BodyCtx, gamma: &mut Gamma, s: Idx<Statement>) -> Result<(), CompilerError> {
    let span = c.body.statement_spans[s];

    match &c.body.statement_arena[s] {
        Statement::Expr(e) => {
            infer_expr_used(c, gamma, *e, Use::Discard)?;
        }

        Statement::Defer(e) => {
            infer_expr_used(c, gamma, *e, Use::Discard)?;
        }

        Statement::Return(e) => {
            // CHECKED against the declared return, not inferred-then-coerced: a
            // returned lambda has to record the type of the hole it fills, or the
            // body and every indirect call site derive different fn-value ABIs
            // from it. Same rule the `let` arm below already follows.
            match (e, gamma.declared_ret_ty) {
                (Some(e), Some(declared)) => {
                    check_expr(c, gamma, *e, declared, c.body.statement_spans[s])?;
                    validate_frame_ref_return(c, gamma, *e, declared)?;
                }
                (Some(e), None) => {
                    infer_expr(c, gamma, *e)?;
                }
                (None, Some(declared)) => {
                    let unit = gamma.workspace.builtins.unit();
                    coerce(gamma, unit, declared, c.body.statement_spans[s])?;
                }
                (None, None) => {}
            }
        }

        Statement::Let { name, value } => {
            let declared = c.body.binding_arena[*name].declared_ty;

            let binding_ty = match declared {
                Some(ann) => {
                    let span = c
                        .body
                        .expr_spans
                        .get(*value)
                        .copied()
                        .unwrap_or(Span::unknown());

                    // Bind to the annotation in case RHS fails, helps LSP
                    gamma.assign_named_type(*name, ann)?;
                    check_expr(c, gamma, *value, ann, span)?;

                    ann
                }

                None => match infer_expr(c, gamma, *value) {
                    Ok(ty) => ty,
                    Err(e) => {
                        // RHS failed, position name with "error" so downstream can still do something
                        let poison = gamma.workspace.builtins.error();
                        gamma.assign_named_type(*name, poison)?;
                        return Err(e);
                    }
                },
            };

            gamma.assign_named_type(*name, binding_ty)?;

            // ctx.assign_named_type(*name, inf_ty);
            // ctx.assign_type(name, ty)?;
            // ctx.assign_type(e, ty)
        }

        Statement::Static { name, value } => {
            if gamma.coroutine_ret.is_some() {
                comp_error!(
                    c.body.statement_spans[s],
                    "`static` is not allowed in a `co fn`: a shared cell across yields corrupts"
                );
            }

            let declared = c.body.binding_arena[*name].declared_ty;
            let binding_ty = match declared {
                Some(ann) => {
                    let vspan = c.body.expr_spans.get(*value).copied().unwrap_or(Span::unknown());
                    gamma.assign_named_type(*name, ann)?;
                    check_expr(c, gamma, *value, ann, vspan)?;
                    ann
                }

                None => match infer_expr(c, gamma, *value) {
                    Ok(ty) => ty,
                    Err(e) => {
                        let poison = gamma.workspace.builtins.error();
                        gamma.assign_named_type(*name, poison)?;
                        return Err(e);
                    }
                },
            };

            if contains_frame_ref(gamma, binding_ty) {
                comp_error!(
                    c.body.statement_spans[s],
                    "a `static` local cannot store a frame reference; store an `Entity<T>` or a value instead"
                );
            }

            gamma.assign_named_type(*name, binding_ty)?;

            reject_static_init_captures(c, *value, c.body.statement_spans[s])?;
        }

        Statement::Fn { name, params, ret_ty, body } => {
            let (name, params, ret_ty, body) = (*name, *params, *ret_ty, *body);

            // Params always carry a declared type (the signature is mandatory).
            let mut param_tys = Vec::with_capacity(params.len());
            for pb in params {
                let ty = c.body.binding_arena[pb]
                    .declared_ty
                    .unwrap_or_else(|| gamma.workspace.builtins.error());
                gamma.assign_named_type(pb, ty)?;
                param_tys.push(ty);
            }

            // The nested body is hoisted and runs at call time, so no enclosing
            // GLOBAL refinement holds inside it (mirrors `infer_lambda`); the
            // enclosing return-context is saved and restored around it.
            let saved_ret = gamma.declared_ret_ty.take();
            let saved_coro = gamma.coroutine_ret.take();
            let saved_ref_source =
                std::mem::replace(&mut gamma.allow_frame_ref_return_place, false);
            let saved_globals: Vec<Narrow> = gamma
                .refined_nonnull
                .iter()
                .filter(|(root, _)| root.is_global())
                .cloned()
                .collect();
            for g in &saved_globals {
                gamma.refined_nonnull.remove(g);
            }

            let result = (|| -> Result<(), CompilerError> {
                match ret_ty {
                    Some(ret) => {
                        // Concrete signature -> assign the name its fn type BEFORE
                        // the body so a self-recursive call resolves.
                        let fn_ty = make_fn_ty(gamma, &param_tys, ret, span);
                        gamma.assign_named_type(name, fn_ty)?;
                        gamma.declared_ret_ty = Some(ret);

                        let body_ty = infer_expr(c, gamma, body)?;
                        let body_ty = resolve(gamma, body_ty, span);
                        let tail = match c.body.expr_arena[body] {
                            Expr::Block { tail, .. } => tail,
                            _ => Some(body),
                        };
                        let cspan = tail.map(|t| c.body.expr_spans[t]).unwrap_or(span);
                        if let Err(e) = coerce(gamma, body_ty, ret, cspan) {
                            gamma.body_errors.push(e);
                        }
                    }
                    None => {
                        // Inferred return: the body's tail is the return type (like
                        // a lambda). A self-recursive call needs an explicit `-> T`;
                        // an explicit `return X` in an inferred-return nested fn is
                        // not type-linked (v1 -- annotate the return type instead).
                        gamma.declared_ret_ty = None;
                        let body_ty = infer_expr(c, gamma, body)?;
                        let body_ty = resolve(gamma, body_ty, span);
                        let fn_ty = make_fn_ty(gamma, &param_tys, body_ty, span);
                        gamma.assign_named_type(name, fn_ty)?;
                    }
                }
                Ok(())
            })();

            gamma.refined_nonnull.extend(saved_globals);
            gamma.coroutine_ret = saved_coro;
            gamma.declared_ret_ty = saved_ret;
            gamma.allow_frame_ref_return_place = saved_ref_source;
            result?;

            reject_nested_fn_captures(c, body, span)?;
        }

        Statement::Comment(_) => (),

        Statement::For { vars, iter, body } => {
            let (iter, body) = (*iter, *body);

            let (elem_ty, deferred) = match infer_for_elem(c, gamma, iter, span) {
                Ok(t) => (t, None),
                Err(e) => (gamma.workspace.builtins.error(), Some(e)),
            };

            destructure_pattern(gamma, vars, elem_ty, span)?;
            gamma.loop_depth += 1;
            let body_res = infer_expr_used(c, gamma, body, Use::Discard);
            gamma.loop_depth -= 1;
            body_res?;

            if let Some(err) = deferred {
                return Err(err);
            }
        }

        Statement::While { cond, body } => {
            let (cond, body) = (*cond, *body);
            let cond_ty = infer_expr(c, gamma, cond)?;

            unify(gamma, cond_ty, gamma.workspace.builtins.bool(), span)?;

            let (narrow_then, _) = detect_null_narrow(gamma, c, cond);
            gamma.loop_depth += 1;
            let body_res = infer_branch(c, gamma, body, &narrow_then, span, Use::Discard);
            gamma.loop_depth -= 1;
            body_res?;
        }

        Statement::WhileLet {
            pattern,
            value,
            body,
        } => {
            let (pattern, value, body) = (*pattern, *value, *body);

            let (unwrapped, deferred) = match infer_expr(c, gamma, value) {
                Ok(t) => (peel_nullable(gamma, t, span), None),
                Err(e) => (gamma.workspace.builtins.error(), Some(e)),
            };

            check_pattern(c, gamma, pattern, unwrapped, span)?;
            gamma.loop_depth += 1;
            let body_res = infer_expr_used(c, gamma, body, Use::Discard);
            gamma.loop_depth -= 1;
            body_res?;

            if let Some(err) = deferred {
                return Err(err);
            }
        }

        Statement::Break | Statement::Continue => {
            if gamma.loop_depth == 0 {
                comp_error!(span, "`break`/`continue` is only valid inside a loop body");
            }
        }

        Statement::LetTuple { vars, value } => {
            reject_cf_in_destructure_rhs(c, *value)?;
            match infer_expr(c, gamma, *value) {
                Ok(t) => destructure_pattern(gamma, vars, t, span)?,
                Err(e) => {
                    destructure_pattern(gamma, vars, gamma.workspace.builtins.error(), span)?;
                    return Err(e);
                }
            }
        }

        Statement::LetNewtype {
            newtype_name,
            var,
            value,
        } => {
            reject_cf_in_destructure_rhs(c, *value)?;
            let value_ty = infer_expr(c, gamma, *value)?;
            let resolved = resolve(gamma, value_ty, span);

            match *gamma.get(resolved) {
                TypeExpr::App {
                    ctor: TypeCtor::Nominal(name),
                    ..
                } if name == *newtype_name => {}

                TypeExpr::Infer(..) | TypeExpr::Generic(..) => {}

                _ => {
                    comp_error!(span, "cannot destructure this value with newtype pattern `{}` - the value is not a newtype `{}`", newtype_name, newtype_name)
                }
            };

            let NewtypeId { module: ntm, def } =
                resolve_newtype(gamma, *newtype_name).ok_or_else(|| {
                    CompilerError::new_with_label(
                        span,
                        format!("unknown newtype `{}`", newtype_name),
                    )
                })?;

            let nt = gamma.workspace.module(ntm).newtypes[def].clone();

            let mut subs = FxHashMap::default();
            if let TypeExpr::App {
                ctor: TypeCtor::Nominal(_),
                params,
            } = *gamma.get(resolved)
            {
                for (g, arg) in nt.generics.iter().zip(params) {
                    if let TypeExpr::Generic(var_id, _) = gamma.get(*g) {
                        subs.insert(*var_id, resolved.with_idx(arg));
                    }
                }
            }

            let wt = substitute(gamma, nt.wrapped, &subs, span)?;
            gamma.assign_named_type(*var, wt)?;
        }
    };

    // Statement boundary: a sequence point, so any pinned lambda-param var is
    // committed -- resolve the constraints deferred on it (defer.rs).
    drain_ready_pendings(gamma)?;

    Ok(())
}

pub fn infer_expr_used(
    c: BodyCtx,
    gamma: &mut Gamma,
    e: Idx<Expr>,
    used: Use,
) -> Result<TyId, CompilerError> {
    let expr_span = c.body.expr_spans[e];

    if let Some(&cached) = gamma.out.expr_types.get(e) {
        return Ok(resolve(gamma, cached, expr_span));
    }

    let ty = match &c.body.expr_arena[e] {
        Expr::Int(_) => gamma.workspace.builtins.int(),
        Expr::Float(_) => gamma.workspace.builtins.float(),
        Expr::Bool(_) => gamma.workspace.builtins.bool(),
        Expr::String(_) => gamma.workspace.builtins.string(),
        Expr::Unit => gamma.workspace.builtins.unit(),

        Expr::Hole(_) => gamma.fresh_callsite_var(*HOLE, expr_span),

        Expr::InterpolatedString(parts) => {
            for p in *parts {
                if let InterpPart::Value { expr, spec } = c.body.interp_part_arena[p] {
                    infer_expr(c, gamma, expr)?;

                    if let Some(s) = spec {
                        if let Some(we) = s.width_expr {
                            infer_expr(c, gamma, we)?;
                        }
                        if let Some(pe) = s.precision_expr {
                            infer_expr(c, gamma, pe)?;
                        }
                    }
                }
            }

            gamma.workspace.builtins.string()
        }
        Expr::Not(inner) => {
            let inner_ty = infer_expr(c, gamma, *inner)?;
            let resolved = resolve(gamma, inner_ty, c.body.expr_spans[*inner]);

            if matches!(gamma.get(resolved), TypeExpr::Builtin(BuiltinTy::Bool)) {
                gamma.workspace.builtins.bool()
            } else {
                resolve_named_call(
                    gamma,
                    c,
                    CallFamily::from_name(*NOT),
                    None,
                    IdxRange::new_inclusive(*inner..=*inner),
                    e,
                    expr_span,
                )?
            }
        }

        Expr::BitNot(inner) => {
            let inner_ty = infer_expr(c, gamma, *inner)?;
            let resolved = resolve(gamma, inner_ty, c.body.expr_spans[*inner]);
            match *gamma.get(resolved) {
                TypeExpr::Builtin(BuiltinTy::Int) | TypeExpr::Builtin(BuiltinTy::Uint) => resolved,
                _ => {
                    let int_ty = gamma.workspace.builtins.int();
                    unify(gamma, inner_ty, int_ty, c.body.expr_spans[*inner])?;
                    int_ty
                }
            }
        }

        Expr::Spread(..) => comp_error!(expr_span, "spread is only valid inside an array literal"),

        Expr::Yield(opt) => {
            let opt = *opt;
            let ret = gamma.coroutine_ret.ok_or_else(|| {
                CompilerError::new_with_label(
                    expr_span,
                    "yield is only allowed in a coroutine (co fn)".to_string(),
                )
            })?;

            // let ret = gamma.declared_ret_ty.ok_or_else(|| {
            //     CompilerError::new_with_label(
            //         expr_span,
            //         "yield is only allowed in functions with an annotated return type".to_string(),
            //     )
            // })?;

            let (y, _r, n) = coroutine_yrn(gamma, ret, expr_span)?;

            if let Some(e) = opt {
                let et = infer_expr(c, gamma, e)?;
                coerce(gamma, et, y, c.body.expr_spans[e])?;
            }

            // Suspension hands control to the resumer, who may reassign any
            // global; drop all global refinements (the yielded value above was
            // computed before suspending, so it still saw them).
            gamma.refined_nonnull.retain(|(root, _)| !root.is_global());

            n
        }

        Expr::YieldDelegate(inner) => {
            let inner = *inner;
            let ret = gamma.coroutine_ret.ok_or_else(|| {
                CompilerError::new_with_label(
                    expr_span,
                    "yield is only allowed in functions with an annotated return type".to_string(),
                )
            })?;

            let (outer_y, _, _) = coroutine_yrn(gamma, ret, expr_span)?;
            let inner_ty = infer_expr(c, gamma, inner)?;
            let (inner_y, inner_r, _) = coroutine_yrn(gamma, inner_ty, c.body.expr_spans[inner])?;

            coerce(gamma, inner_y, outer_y, c.body.expr_spans[e])?;

            // `yield*` suspends repeatedly into the delegate; the resumer may
            // reassign globals between resumptions.
            gamma.refined_nonnull.retain(|(root, _)| !root.is_global());

            inner_r
        }

        Expr::UnaryMinus(inner) => {
            let inner_ty = infer_expr(c, gamma, *inner)?;
            // TODO: do we actually want to call resolve after every infer? shouldn't that be part
            // of infer? figure that out later
            let resolved = resolve(gamma, inner_ty, c.body.expr_spans[*inner]);

            match *gamma.get(resolved) {
                TypeExpr::Builtin(BuiltinTy::Int) | TypeExpr::Builtin(BuiltinTy::Float) => resolved,
                TypeExpr::Builtin(BuiltinTy::Uint) => {
                    comp_error!(expr_span, "unary minus is not defined for `uint`")
                }

                TypeExpr::App {
                    ctor: TypeCtor::Nominal(name),
                    ..
                } if resolve_newtype(gamma, name).is_some() => resolved,

                _ => resolve_named_call(
                    gamma,
                    c,
                    CallFamily::from_name(*UNM),
                    None,
                    IdxRange::new_inclusive(*inner..=*inner),
                    e,
                    expr_span,
                )?,
            }
        }

        Expr::IncDec { target, .. } => {
            if let Expr::FieldAccess { base, field } = c.body.expr_arena[*target] {
                reject_swizzle_write(c, gamma, base, field, expr_span)?;
                reject_value_struct_field_write(c, gamma, base, expr_span)?;
                check_field_write_access(c, gamma, base, field, expr_span)?;
            }
            // Same const-rebind rejection as Expr::Assign/CompoundAssign;
            // `CONST++` is a write with extra steps.
            if let Expr::Global(name) = c.body.expr_arena[*target] {
                if gamma.workspace.const_globals.contains(&name) {
                    comp_error!(
                        expr_span,
                        "cannot assign to const `{}`; consts are read-only",
                        name
                    );
                }
            }
            let inner_ty = infer_expr(c, gamma, *target)?;
            resolve(gamma, inner_ty, c.body.expr_spans[*target])
        }

        Expr::As { value, target, kind } => {
            let (value, kind) = (*value, *kind);
            let target = resolve(gamma, *target, expr_span);
            let value_span = c.body.expr_spans[value];
            match kind {
                // `::` is the operand's expected type, so it reaches the operand
                // through the same route as a `let`/param/return hole -- which is
                // also what binds a bare `null` and an unannotated lambda param.
                AsKind::Ascribe => {
                    let target_str = gamma.render(target);
                    check_expr(c, gamma, value, target, value_span).map_err(|e| {
                        e.with_label(expr_span, format!("`::` ascribes `{target_str}` here"))
                    })?
                }
                AsKind::Cast => {
                    let value_ty = infer_expr(c, gamma, value)?;
                    check_numeric_literal_cast(c, gamma, value, target, expr_span)?;
                    check_cast(gamma, value_ty, target, expr_span)?;
                }
                AsKind::Bitcast => {
                    let value_ty = infer_expr(c, gamma, value)?;
                    check_bitcast(gamma, value_ty, target, expr_span)?;
                }
            }
            target
        }

        Expr::Lambda { params, body } => {
            infer_lambda(c, gamma, *params, *body, None, None, expr_span)?
        }

        Expr::Index { base, index } => {
            let (base, index) = (*base, *index);

            let base_ty = infer_expr(c, gamma, base)?;
            let base_ty = resolve(gamma, base_ty, expr_span);

            let index_ty = infer_expr(c, gamma, index)?;

            match *gamma.get(base_ty) {
                TypeExpr::App {
                    ctor: TypeCtor::Array,
                    params,
                } => {
                    let elem = params.into_iter().next().ok_or_else(|| {
                        CompilerError::new_with_label(
                            expr_span,
                            "array type must have one parameter".to_string(),
                        )
                    })?;

                    unify(
                        gamma,
                        index_ty,
                        gamma.workspace.builtins.int(),
                        c.body.expr_spans[index],
                    )?;
                    resolve(gamma, base_ty.with_idx(elem), expr_span)
                }

                TypeExpr::App {
                    ctor: TypeCtor::Map,
                    params,
                } => {
                    let mut it = params.into_iter();

                    let key = it.next().ok_or_else(|| {
                        CompilerError::new_with_label(
                            expr_span,
                            "map type must have two parameters".to_string(),
                        )
                    })?;

                    let val = it.next().ok_or_else(|| {
                        CompilerError::new_with_label(
                            expr_span,
                            "map type must have two parameters".to_string(),
                        )
                    })?;

                    let key_ty = resolve(gamma, base_ty.with_idx(key), expr_span);
                    unify(gamma, index_ty, key_ty, c.body.expr_spans[index])?;
                    resolve(gamma, base_ty.with_idx(val), expr_span)
                }

                TypeExpr::App {
                    ctor: TypeCtor::Nominal(name),
                    params,
                } => {
                    unify(
                        gamma,
                        index_ty,
                        gamma.workspace.builtins.int(),
                        c.body.expr_spans[index],
                    )?;

                    let elem = if bone_core::modules::is_intrinsic_key_in(
                        name,
                        &["NativeBuffer", "FlatVec"],
                    ) {
                            let p = params.into_iter().next().ok_or_else(|| {
                                CompilerError::new_with_label(
                                    expr_span,
                                    "NativeBuffer type must have one parameter".to_string(),
                                )
                            })?;

                            base_ty.with_idx(p)
                    } else if bone_core::modules::is_intrinsic_key_in(
                        name,
                        &[
                            "NativeBufferU8",
                            "NativeBufferI8",
                            "NativeBufferU16",
                            "NativeBufferU32",
                            "NativeBufferI16",
                            "NativeBufferI32",
                        ],
                    ) {
                        gamma.workspace.builtins.int()
                    } else {
                        comp_error!(
                            expr_span,
                            "cannot index into type {}",
                            gamma.workspace.render(Some(&gamma.out), base_ty)
                        )
                    };

                    resolve(gamma, elem, expr_span)
                }

                _ => comp_error!(
                    expr_span,
                    "cannot index into type {}",
                    gamma.workspace.render(Some(&gamma.out), base_ty)
                ),
            }
        }

        Expr::Range { low, high } => {
            let range_ty = infer_range_bounds(c, gamma, *low, *high)?;
            let range_texp = *gamma.get(range_ty);
            let params = alloc_contiguous(&mut gamma.out.ty_arena, [range_texp]);

            let arr = gamma.out.ty_arena.alloc(TypeExpr::App {
                ctor: TypeCtor::Array,
                params,
            });

            resolve(gamma, TyId::Local(arr), expr_span)
        }

        Expr::Var(binding) => gamma
            .out
            .binding_types
            .get(*binding)
            .cloned()
            .ok_or_else(|| {
                CompilerError::new_with_label(
                    c.body.expr_spans[e],
                    format!(
                        "Undefined variable `{}`",
                        c.body.binding_arena[*binding].name
                    ),
                )
            })?,

        Expr::Global(name) => match resolve_global(gamma, *name) {
            // A global guarded by `if G != null` (with no intervening write /
            // impure call / yield since) reads as its non-null inner type.
            Some(ty)
                if gamma
                    .refined_nonnull
                    .contains(&(Root::Global(*name), vec![])) =>
            {
                peel_nullable(gamma, ty, c.body.expr_spans[e])
            }

            Some(ty) => ty,

            // A backend-variant sibling pair (same name, same signature,
            // different `backend`) is ONE logical fn; collapse it to a single
            // representative before deciding value-ness, exactly as the call
            // path does. Genuine overloads (different signatures) survive and
            // still reject below.
            None => match collapse_backend_variants(gamma, &resolve_callee(gamma, &CallFamily::from_name(*name))).as_slice()
            {
                [(mid, fid)] => {
                    let function = gamma.workspace.module(*mid).functions[*fid].clone();

                    // A bounded fn only exists monomorphized-by-inlining at its
                    // call sites; as a value it would need a runtime identity
                    // its placeholder bound-calls can't have.
                    if function.generic_bounds.iter().any(Option::is_some) {
                        comp_error!(
                            expr_span,
                            "`{}` has `Iterator` bounds and cannot be used as a value; call it directly",
                            name
                        );
                    }

                    let param_tys = function.param_decls.iter().map(|p| p.ty).collect_vec();

                    let mut texps = Vec::with_capacity(param_tys.len());

                    for pty in param_tys {
                        let r = TyId::Local(reify(gamma, pty, expr_span));
                        texps.push(*gamma.get(r));
                    }

                    let params = alloc_contiguous(&mut gamma.out.ty_arena, texps);
                    let ret_ty = function
                        .return_ty
                        .unwrap_or_else(|| gamma.workspace.builtins.unit());
                    let ret = reify(gamma, ret_ty, expr_span);

                    TyId::Local(gamma.out.ty_arena.alloc(TypeExpr::Function { params, ret }))
                }

                [] => comp_error!(expr_span, "Undefined global `{}`", name),
                _ => comp_error!(
                    expr_span,
                    "ambiguous fn-value reference `{}` (overloaded)",
                    name
                ),
            },
        },

        Expr::Null => {
            let TyId::Local(idx) = gamma.fresh_callsite_var(*NULL, expr_span) else {
                comp_error!(expr_span, "fresh_callsite_var returned non-local idx");
            };

            TyId::Local(gamma.out.ty_arena.alloc(TypeExpr::Nullable(idx)))
        }

        Expr::BinOp { op, lhs, rhs } => match op {
            hir::BinOp::Eq | hir::BinOp::NotEq => {
                let lhs_ty = infer_expr(c, gamma, *lhs)?;
                let rhs_ty = infer_expr(c, gamma, *rhs)?;
                let mut lhs_ty = resolve(gamma, lhs_ty, c.body.expr_spans[*lhs]);
                let mut rhs_ty = resolve(gamma, rhs_ty, c.body.expr_spans[*rhs]);
                if matches!(gamma.get(lhs_ty), TypeExpr::Builtin(BuiltinTy::Uint))
                    && matches!(gamma.get(rhs_ty), TypeExpr::Builtin(BuiltinTy::Int))
                    && matches!(c.body.expr_arena[*rhs], Expr::Int(_))
                {
                    check_expr(c, gamma, *rhs, lhs_ty, c.body.expr_spans[*rhs])?;
                    rhs_ty = lhs_ty;
                } else if matches!(gamma.get(rhs_ty), TypeExpr::Builtin(BuiltinTy::Uint))
                    && matches!(gamma.get(lhs_ty), TypeExpr::Builtin(BuiltinTy::Int))
                    && matches!(c.body.expr_arena[*lhs], Expr::Int(_))
                {
                    check_expr(c, gamma, *lhs, rhs_ty, c.body.expr_spans[*lhs])?;
                    lhs_ty = rhs_ty;
                }

                // Comparing a value KNOWN to be non-null against `null` is a
                // category error: `null` can't inhabit it, so the check is dead.
                // "Known non-null" is flow-sensitive -- it covers a declared
                // non-nullable type AND a nullable refined to non-null by a guard
                // or assignment (`redundant_optional_on` reads the inferred type,
                // which reflects both). Exempts nullable / `any` / unresolved
                // generics. Consistent with bone's `??`/`?.` rejection on a
                // non-nullable; stricter than rock, which only narrows via guards.
                let non_null_ty = match (c.body.expr_arena[*lhs], c.body.expr_arena[*rhs]) {
                    (_, Expr::Null) => Some(lhs_ty),
                    (Expr::Null, _) => Some(rhs_ty),
                    _ => None,
                };
                if let Some(ty) = non_null_ty {
                    if redundant_optional_on(gamma, ty, expr_span) {
                        let rendered = gamma.workspace.render(Some(&gamma.out), ty);
                        comp_error!(
                            expr_span,
                            "Cannot compare non-nullable type '{}' with null",
                            rendered
                        );
                    }
                }

                // Fn values carry no meaningful identity: a capture-free lambda
                // is ONE shared hoisted fn, a capturing lambda a per-creation
                // env record (docs/closure-env-records-plan.md), so `==` would
                // compare representation artifacts. Null checks (`f != null`)
                // are refinement, not identity -- exempt via `non_null_ty`
                // being the literal-null case above.
                if non_null_ty.is_none() {
                    for ty in [lhs_ty, rhs_ty] {
                        let peeled = peel_nullable(gamma, ty, expr_span);
                        if matches!(*gamma.get(peeled), TypeExpr::Function { .. }) {
                            comp_error!(
                                expr_span,
                                "Cannot compare fn values: fn identity is not meaningful (a capture-free lambda is one shared fn; a capturing lambda is a fresh record per creation). Compare via `!= null` or restructure around explicit keys"
                            );
                        }
                    }
                }

                join(gamma, lhs_ty, rhs_ty, expr_span)?;
                gamma.workspace.builtins.bool()
            }

            hir::BinOp::BitAnd
            | hir::BinOp::BitOr
            | hir::BinOp::BitXor
            | hir::BinOp::Shl
            | hir::BinOp::Shr => {
                let int_ty = gamma.workspace.builtins.int();
                let lhs_ty = infer_expr(c, gamma, *lhs)?;
                let rhs_ty = infer_expr(c, gamma, *rhs)?;
                let mut lhs_ty = resolve(gamma, lhs_ty, c.body.expr_spans[*lhs]);
                let mut rhs_ty = resolve(gamma, rhs_ty, c.body.expr_spans[*rhs]);
                if matches!(gamma.get(lhs_ty), TypeExpr::Builtin(BuiltinTy::Uint))
                    && matches!(gamma.get(rhs_ty), TypeExpr::Builtin(BuiltinTy::Int))
                    && matches!(c.body.expr_arena[*rhs], Expr::Int(_))
                {
                    check_expr(c, gamma, *rhs, lhs_ty, c.body.expr_spans[*rhs])?;
                    rhs_ty = lhs_ty;
                } else if matches!(gamma.get(rhs_ty), TypeExpr::Builtin(BuiltinTy::Uint))
                    && matches!(gamma.get(lhs_ty), TypeExpr::Builtin(BuiltinTy::Int))
                    && matches!(c.body.expr_arena[*lhs], Expr::Int(_))
                {
                    check_expr(c, gamma, *lhs, rhs_ty, c.body.expr_spans[*lhs])?;
                    lhs_ty = rhs_ty;
                }
                if matches!(gamma.get(lhs_ty), TypeExpr::Builtin(BuiltinTy::Uint))
                    || matches!(gamma.get(rhs_ty), TypeExpr::Builtin(BuiltinTy::Uint))
                {
                    let uint_ty = gamma.workspace.builtins.uint();
                    unify(gamma, lhs_ty, uint_ty, c.body.expr_spans[*lhs])?;
                    unify(gamma, rhs_ty, uint_ty, c.body.expr_spans[*rhs])?;
                    if matches!(op, hir::BinOp::Shl | hir::BinOp::Shr)
                        && let Expr::Int(value) = c.body.expr_arena[*rhs]
                        && !(0..=31).contains(&value)
                    {
                        comp_error!(
                            c.body.expr_spans[*rhs],
                            "uint shift count must be in the range 0..=31, found {}",
                            value
                        );
                    }
                    uint_ty
                } else {
                    unify(gamma, lhs_ty, int_ty, c.body.expr_spans[*lhs])?;
                    unify(gamma, rhs_ty, int_ty, c.body.expr_spans[*rhs])?;
                    int_ty
                }
            }

            hir::BinOp::Lt | hir::BinOp::Leq | hir::BinOp::Gt | hir::BinOp::Geq => {
                let lhs_ty = infer_expr(c, gamma, *lhs)?;
                let rhs_ty = infer_expr(c, gamma, *rhs)?;
                let mut lhs_ty = resolve(gamma, lhs_ty, c.body.expr_spans[*lhs]);
                let mut rhs_ty = resolve(gamma, rhs_ty, c.body.expr_spans[*rhs]);
                if matches!(gamma.get(lhs_ty), TypeExpr::Builtin(BuiltinTy::Uint))
                    && matches!(gamma.get(rhs_ty), TypeExpr::Builtin(BuiltinTy::Int))
                    && matches!(c.body.expr_arena[*rhs], Expr::Int(_))
                {
                    check_expr(c, gamma, *rhs, lhs_ty, c.body.expr_spans[*rhs])?;
                    rhs_ty = lhs_ty;
                } else if matches!(gamma.get(rhs_ty), TypeExpr::Builtin(BuiltinTy::Uint))
                    && matches!(gamma.get(lhs_ty), TypeExpr::Builtin(BuiltinTy::Int))
                    && matches!(c.body.expr_arena[*lhs], Expr::Int(_))
                {
                    check_expr(c, gamma, *lhs, rhs_ty, c.body.expr_spans[*lhs])?;
                    lhs_ty = rhs_ty;
                }

                let joined = join(gamma, lhs_ty, rhs_ty, expr_span)?;
                // unify(gamma, lhs_ty, rhs_ty, expr_span)?;

                let resolved = resolve(gamma, joined, expr_span);

                let orderable = match *gamma.get(resolved) {
                    TypeExpr::Builtin(BuiltinTy::Int)
                    | TypeExpr::Builtin(BuiltinTy::Uint)
                    | TypeExpr::Builtin(BuiltinTy::Float)
                    | TypeExpr::Generic(..) => true,

                    TypeExpr::App {
                        ctor: TypeCtor::Nominal(name),
                        ..
                    } if resolve_newtype(gamma, name).is_some() => {
                        let inner = tuple_or_newtype_index_ty(gamma, resolved, 0, expr_span)?;
                        let inner = resolve(gamma, inner, expr_span);

                        matches!(
                            gamma.get(inner),
                            TypeExpr::Builtin(BuiltinTy::Int)
                                | TypeExpr::Builtin(BuiltinTy::Uint)
                                | TypeExpr::Builtin(BuiltinTy::Float)
                        )
                    }

                    _ => false,
                };

                if !orderable {
                    comp_error!(
                        expr_span,
                        "ordering operators require numeric operands, got {}",
                        gamma.workspace.render(Some(&gamma.out), resolved)
                    );
                }

                gamma.workspace.builtins.bool()
            }

            hir::BinOp::And => {
                let bool_ty = gamma.workspace.builtins.bool();
                let lhs_ty = infer_expr(c, gamma, *lhs)?;
                unify(gamma, lhs_ty, bool_ty, c.body.expr_spans[*lhs])?;

                let (then_narrows, _) = detect_null_narrow(gamma, c, *lhs);

                let rhs_ty = infer_branch(
                    c,
                    gamma,
                    *rhs,
                    &then_narrows,
                    c.body.expr_spans[*rhs],
                    Use::Value,
                )?;

                unify(gamma, rhs_ty, bool_ty, c.body.expr_spans[*rhs])?;

                bool_ty
            }

            hir::BinOp::Or => {
                let bool_ty = gamma.workspace.builtins.bool();
                let lhs_ty = infer_expr(c, gamma, *lhs)?;
                unify(gamma, lhs_ty, bool_ty, c.body.expr_spans[*lhs])?;

                // The RHS of `||` runs only when the LHS is false, so it sees the
                // LHS's ELSE narrowings (dual of `&&`, which uses THEN narrowings).
                let (_, else_narrows) = detect_null_narrow(gamma, c, *lhs);

                let rhs_ty = infer_branch(
                    c,
                    gamma,
                    *rhs,
                    &else_narrows,
                    c.body.expr_spans[*rhs],
                    Use::Value,
                )?;

                unify(gamma, rhs_ty, bool_ty, c.body.expr_spans[*rhs])?;

                bool_ty
            }

            hir::BinOp::NullCoalesce => {
                let lhs_ty = infer_expr(c, gamma, *lhs)?;
                if redundant_optional_on(gamma, lhs_ty, expr_span) {
                    comp_error!(
                        c.body.expr_spans[*lhs],
                        "left operand of `??` is not nullable -- the `?? default` is redundant"
                    );
                }
                let inner = peel_nullable(gamma, lhs_ty, expr_span);
                let rhs_ty = infer_expr(c, gamma, *rhs)?;
                join(gamma, inner, rhs_ty, expr_span)?
            }
        },

        Expr::Block { statements, tail } => {
            infer_block(c, gamma, statements, tail, used, expr_span, None)
        }

        Expr::If {
            cond,
            then_branch,
            else_branch,
        } => {
            let cond_ty = infer_expr(c, gamma, *cond)?;
            unify(gamma, cond_ty, gamma.workspace.builtins.bool(), expr_span)?;

            let (narrow_then, narrow_else) = detect_null_narrow(gamma, c, *cond);

            match used {
                Use::Discard => {
                    infer_branch(
                        c,
                        gamma,
                        *then_branch,
                        &narrow_then,
                        expr_span,
                        Use::Discard,
                    )?;

                    if let Some(else_branch) = else_branch {
                        infer_branch(
                            c,
                            gamma,
                            *else_branch,
                            &narrow_else,
                            expr_span,
                            Use::Discard,
                        )?;
                    }

                    gamma.workspace.builtins.unit()
                }

                Use::Value => {
                    let then_ty =
                        infer_branch(c, gamma, *then_branch, &narrow_then, expr_span, Use::Value)?;

                    let Some(else_branch) = else_branch else {
                        comp_error!(
                            expr_span,
                            "`if` without a final `else` cannot be used as a value"
                        );
                    };

                    let else_ty =
                        infer_branch(c, gamma, *else_branch, &narrow_else, expr_span, Use::Value)?;
                    join(gamma, then_ty, else_ty, expr_span)?
                }
            }
        }

        Expr::TupleIndex { tuple, index } => {
            let tuple_ty = infer_expr(c, gamma, *tuple)?;
            let tuple_ty = resolve(gamma, tuple_ty, expr_span);

            tuple_or_newtype_index_ty(gamma, tuple_ty, *index, expr_span)?
        }

        Expr::Array(elements) => {
            let mut elem_ty: Option<TyId> = None;

            for elem in *elements {
                let elem_span = c.body.expr_spans[elem];
                let tys: Vec<TyId> = if let Expr::Spread(operand) = c.body.expr_arena[elem] {
                    let op_ty = infer_expr(c, gamma, operand)?;
                    spread_constituents(gamma, op_ty, elem_span)?
                } else {
                    vec![infer_expr(c, gamma, elem)?]
                };

                for t in tys {
                    elem_ty = Some(match elem_ty {
                        None => t,
                        Some(acc) => join(gamma, acc, t, elem_span)?,
                    });
                }
            }

            let elem_ty = match elem_ty {
                Some(t) => resolve(gamma, t, expr_span),
                None => gamma.fresh_callsite_var(*Q_E, expr_span),
            };

            let elem_idx = reify(gamma, elem_ty, expr_span);
            let elem_texp = *gamma.get(TyId::Local(elem_idx));
            let params = alloc_contiguous(&mut gamma.out.ty_arena, [elem_texp]);

            let arr_ty = gamma
                .out
                .ty_arena
                .alloc(TypeExpr::App { ctor: TypeCtor::Array, params });

            let ret_ty = TyId::Local(arr_ty);

            resolve(gamma, ret_ty, expr_span)
        }

        Expr::Tuple(items) => {
            // TODO: can we get rid of the allocations here?
            let tys = items.map(|x| infer_expr(c, gamma, x)).try_vec()?;
            tuple_of(gamma, &tys, expr_span)
        }

        Expr::Call { callee, args, generic_args, family } => {
            let args = *args;

            match callee {
                hir::Callee::Unresolved { .. } => {
                    resolve_named_call_with_generics(
                        gamma,
                        c,
                        *family,
                        None,
                        args,
                        *generic_args,
                        e,
                        expr_span,
                    )?
                }

                hir::Callee::Local(binding) => {
                    let binding = *binding;

                    let fn_ty = gamma
                        .out
                        .binding_types
                        .get(binding)
                        .cloned()
                        .ok_or_else(|| {
                            CompilerError::new_with_label(
                                expr_span,
                                format!(
                                    "Undefined function `{}`",
                                    c.body.binding_arena[binding].name
                                ),
                            )
                        })?;

                    if !generic_args.is_empty() {
                        comp_error!(expr_span, "explicit type arguments require a named function call");
                    }
                    call_fn_value(c, gamma, fn_ty, args, expr_span)?
                }

                hir::Callee::Value(callee) => {
                    let fn_ty = infer_expr(c, gamma, *callee)?;
                    if !generic_args.is_empty() {
                        comp_error!(expr_span, "explicit type arguments require a named function call");
                    }
                    call_fn_value(c, gamma, fn_ty, args, expr_span)?
                }
            }
        }

        Expr::StructLiteral { name, fields, bases } => {
            let name = *name;
            let fields = *fields;
            let bases = *bases;

            let sid = resolve_struct(gamma, name).ok_or_else(|| {
                CompilerError::new_with_label(expr_span, format!("unknown struct `{}`", name))
            })?;

            let def = &gamma.workspace.module(sid.module).structs[sid.def];

            let (subs, arg_tys) = gamma.instantiate_generics(&def.generics, expr_span)?;
            let provided: Vec<(Ustr, Idx<Expr>)> = fields
                .into_iter()
                .map(|fi| {
                    let f = c.body.struct_lit_field_arena[fi];
                    (f.name, f.value)
                })
                .collect();

            let what = format!("struct `{}`", name);

            let ctor = if bone_core::modules::is_intrinsic_key(name, "Vec") {
                TypeCtor::Array
            } else {
                TypeCtor::Nominal(name)
            };
            let result_ty = gamma.app_with_ctor(ctor, &arg_tys, expr_span);

            // Functional update `Ty { ..base, .. }`: each base must have the same
            // nominal type as the literal (rule 6). A single base covers every
            // unwritten field. Multiple bases of the same type would all supply
            // every field (rule 5 collision), so only one is allowed.
            let has_base = !bases.is_empty();
            if bases.into_iter().count() > 1 {
                comp_error!(
                    expr_span,
                    "a functional-update literal takes at most one `..base`"
                );
            }
            for b in bases {
                check_expr(c, gamma, b, result_ty, c.body.expr_spans[b])?;
            }

            check_named_fields(
                c, gamma, Some(name), &def.fields, &provided, &subs, expr_span, &what, has_base,
            )?;

            result_ty
        }

        Expr::FieldAccess { base, field } => {
            let (base, field) = (*base, *field);

            let base_ty = infer_expr(c, gamma, base)?;
            let base_ty = resolve(gamma, base_ty, expr_span);

            let fty = field_ty_or_defer(gamma, base_ty, field, expr_span)?;

            match path_key(c, e) {
                Some(p) if gamma.refined_nonnull.contains(&p) => {
                    peel_nullable(gamma, fty, expr_span)
                }
                _ => fty,
            }
        }

        Expr::SiblingRef(name) => {
            let name = *name;

            *gamma
                .sibling_fields
                .last()
                .and_then(|m| m.get(&name))
                .ok_or_else(|| CompilerError::new_with_label(expr_span, format!("field reference `.{name}` is only valid against an expected `FieldRef<T, V>` / `ReadFieldRef<T, V>` / `WriteFieldRef<T, V>` or in a struct-field default")))?
        }

        Expr::SafeNav { base, field } => {
            let base = *base;
            let field = *field;

            let base_ty = infer_expr(c, gamma, base)?;
            if redundant_optional_on(gamma, base_ty, expr_span) {
                comp_error!(
                    c.body.expr_spans[base],
                    "receiver of `?.` is not nullable -- use `.` instead"
                );
            }
            let inner = peel_nullable(gamma, base_ty, expr_span);

            let field_ty = if let Ok(index) = field.as_str().parse::<usize>() {
                let inner = resolve(gamma, inner, expr_span);
                tuple_or_newtype_index_ty(gamma, inner, index, expr_span)?
            } else {
                struct_field_ty(gamma, inner, field, expr_span)?
            };

            nullable_of(gamma, field_ty, expr_span)
        }

        Expr::SafeNavLet {
            receiver,
            binding,
            body,
        } => {
            let (receiver, binding, body) = (*receiver, *binding, *body);

            let (inner, deferred) = match infer_expr(c, gamma, receiver) {
                Ok(t) => (peel_nullable(gamma, t, expr_span), None),
                Err(err) => (gamma.workspace.builtins.error(), Some(err)),
            };

            gamma.assign_named_type(binding, inner)?;

            let body_ty = infer_expr(c, gamma, body)?;

            if let Some(err) = deferred {
                return Err(err);
            }

            nullable_of(gamma, body_ty, expr_span)
        }

        Expr::SafeNavCall {
            receiver,
            args,
            family,
            ..
        } => {
            let (receiver, args, family) = (*receiver, *args, *family);
            let recv_ty = infer_expr(c, gamma, receiver)?;
            if redundant_optional_on(gamma, recv_ty, expr_span) {
                comp_error!(
                    c.body.expr_spans[receiver],
                    "receiver of `?.` is not nullable -- use `.` instead"
                );
            }
            let inner = peel_nullable(gamma, recv_ty, expr_span);

            let ret = resolve_named_call(gamma, c, family, Some(inner), args, e, expr_span)?;

            nullable_of(gamma, ret, expr_span)
        }

        Expr::EnumVariant {
            enum_name,
            variant,
            args,
            ..
        } => {
            let (enum_name, variant, args) = (*enum_name, *variant, *args);

            let eid = resolve_enum(gamma, enum_name).ok_or_else(|| {
                CompilerError::new_with_label(expr_span, format!("unknown enum `{}`", enum_name))
            })?;

            // TODO: maybe make this Copy and then no alloc here?
            let def = gamma.workspace.module(eid.module).enums[eid.def].clone();
            let (subs, arg_tys) = gamma.instantiate_generics(&def.generics, expr_span)?;

            let vdef = def
                .variants
                .iter()
                .find(|v| v.name == variant)
                .cloned()
                .ok_or_else(|| {
                    CompilerError::new_with_label(
                        expr_span,
                        format!("enum `{}` has no variant `{}`", enum_name, variant),
                    )
                })?;

            let what = format!("variant: `{}::{}`", enum_name, variant);

            match (&vdef.payload, args) {
                (VariantPayload::Unit, VariantArgs::Unit) => {}

                (VariantPayload::Tuple(tys), VariantArgs::Tuple(range)) => {
                    check_positional_args(
                        c,
                        gamma,
                        tys,
                        range,
                        tys.len(),
                        &subs,
                        expr_span,
                        &what,
                    )?;
                }

                (VariantPayload::Record(fields), VariantArgs::Record(range)) => {
                    let provided: Vec<(Ustr, Idx<Expr>)> = range
                        .into_iter()
                        .map(|fi| {
                            let f = c.body.struct_lit_field_arena[fi];
                            (f.name, f.value)
                        })
                        .collect();

                    check_named_fields(c, gamma, None, fields, &provided, &subs, expr_span, &what, false)?;
                }

                _ => comp_error!(
                    expr_span,
                    "mismatched enum variant payload and arguments for {}",
                    what
                ),
            }

            gamma.nominal_app(enum_name, &arg_tys, expr_span)
        }

        Expr::Assign { target, value } => {
            let (target, value) = (*target, *value);
            // A `const` global is read-only: reject a whole-global rebind. Field/
            // index writes through it (`Expr::Field`/`Expr::Index` target) are out
            // of scope and fall through.
            if let Expr::Global(name) = c.body.expr_arena[target] {
                if gamma.workspace.const_globals.contains(&name) {
                    comp_error!(
                        expr_span,
                        "cannot assign to const `{}`; consts are read-only",
                        name
                    );
                }
            }
            if let Expr::FieldAccess { base, field } = c.body.expr_arena[target] {
                reject_swizzle_write(c, gamma, base, field, expr_span)?;
                reject_value_struct_field_write(c, gamma, base, expr_span)?;
                check_field_write_access(c, gamma, base, field, expr_span)?;
            }
            // Writing any refined path (local field, global, or global field)
            // drops its refinement; re-established below if the stored value is
            // non-null. A whole local is handled via `binding_types` instead.
            let path = path_key(c, target);

            if let Some(p) = &path {
                gamma.refined_nonnull.remove(p);
            }

            let target_ty = match c.body.expr_arena[target] {
                Expr::Var(b) => match c.body.binding_arena[b].declared_ty {
                    Some(t) => resolve(gamma, t, expr_span),
                    None => infer_expr(c, gamma, target)?,
                },

                _ => infer_expr(c, gamma, target)?,
            };

            let value_ty = infer_expr(c, gamma, value)?;
            coerce(gamma, value_ty, target_ty, expr_span)?;

            let vr = resolve(gamma, value_ty, expr_span);
            let value_nonnull = !matches!(gamma.get(vr), TypeExpr::Nullable(_));

            if let Some(p) = path {
                match p.0 {
                    Root::Local(b) if p.1.is_empty() => {
                        if !value_nonnull {
                            gamma.out.binding_types.insert(b, target_ty);
                        }
                    }
                    _ if value_nonnull => {
                        gamma.refined_nonnull.insert(p);
                    }
                    _ => {}
                }
            };

            gamma.workspace.builtins.unit()
        }

        Expr::Match { scrutinee, arms } => {
            let (scrutinee, arms) = (*scrutinee, *arms);
            let scrut_ty = infer_expr(c, gamma, scrutinee)?;
            let scrut_ty = resolve(gamma, scrut_ty, expr_span);

            let mut result: Option<TyId> = None;
            let mut seen_unguarded_wildcard = false;
            // Exhaustiveness bookkeeping: only UNGUARDED arms contribute, since a
            // guarded arm may not fire. An unguarded `_`/binding arm catches the rest.
            let mut exhaustive_via_catch_all = false;
            let mut covered: FxHashSet<Ustr> = FxHashSet::default();

            for arm_idx in arms {
                let arm = c.body.match_arm_arena[arm_idx];

                if matches!(c.body.pattern_arena[arm.pattern], Pattern::Wildcard)
                    && arm.guard.is_none()
                {
                    if seen_unguarded_wildcard {
                        comp_error!(c.body.expr_spans[arm.body], "duplicate wildcard pattern in match expression - only one unguarded `_` arm is allowed");
                    }

                    seen_unguarded_wildcard = true;
                }

                let tag = unqualified_arm_variant(c, gamma, arm.pattern, scrut_ty, expr_span)?;
                check_pattern(c, gamma, arm.pattern, scrut_ty, expr_span)?;

                if let Some(guard) = arm.guard {
                    let guard_ty = infer_expr(c, gamma, guard)?;
                    unify(gamma, guard_ty, gamma.workspace.builtins.bool(), expr_span)?;
                } else if let Some(v) = tag {
                    covered.insert(v);
                } else {
                    if matches!(
                        c.body.pattern_arena[arm.pattern],
                        Pattern::Wildcard | Pattern::Bind(_)
                    ) {
                        exhaustive_via_catch_all = true;
                    }
                    collect_covered_enum_variants(c, arm.pattern, &mut covered);
                }

                let body_ty = infer_expr_used(c, gamma, arm.body, used)?;

                if used == Use::Value {
                    match result {
                        None => result = Some(body_ty),
                        Some(rt) => result = Some(join(gamma, body_ty, rt, expr_span)?),
                    }
                }
            }

            // A `match` over an enum must cover every variant unless an unguarded
            // `_`/binding arm catches the rest. Recovered (body_errors, not a hard
            // `?`) so partial inference survives for the LSP, like the return-type
            // mismatch. Message keeps the "Non-exhaustive match" prefix the LSP
            // fill-arms code action links against.
            if !exhaustive_via_catch_all {
                if let TypeExpr::App {
                    ctor: TypeCtor::Nominal(ename),
                    ..
                } = *gamma.get(scrut_ty)
                {
                    if let Some(eid) = resolve_enum(gamma, ename) {
                        let missing: Vec<Ustr> = gamma.workspace.module(eid.module).enums[eid.def]
                            .variants
                            .iter()
                            .map(|v| v.name)
                            .filter(|n| !covered.contains(n))
                            .collect();
                        if !missing.is_empty() {
                            let names = missing
                                .iter()
                                .map(|n| n.as_str())
                                .collect::<Vec<_>>()
                                .join(", ");
                            gamma.body_errors.push(CompilerError::new_with_label(
                                expr_span,
                                format!(
                                    "Non-exhaustive match expression. Missing patterns: {names}"
                                ),
                            ));
                        }
                    }
                }
            }

            match used {
                Use::Discard => gamma.workspace.builtins.unit(),
                Use::Value => result.unwrap_or_else(|| gamma.workspace.builtins.unit()),
            }
        }

        Expr::IfLet {
            pattern,
            value,
            then_branch,
            else_branch,
        } => {
            let (unwrapped, deferred) = match infer_expr(c, gamma, *value) {
                // if let x = 0 ... x: T
                Ok(t) => (peel_nullable(gamma, t, expr_span), None),
                Err(err) => (gamma.workspace.builtins.error(), Some(err)),
            };

            check_pattern(c, gamma, *pattern, unwrapped, expr_span)?;

            let result = match used {
                Use::Discard => {
                    infer_expr_used(c, gamma, *then_branch, Use::Discard)?;

                    if let Some(e) = else_branch {
                        infer_expr_used(c, gamma, *e, Use::Discard)?;
                    }

                    gamma.workspace.builtins.unit()
                }

                Use::Value => {
                    let then_ty = infer_expr(c, gamma, *then_branch)?;

                    let Some(e) = else_branch else {
                        comp_error!(
                            expr_span,
                            "`if let` without an `else` cannot be used as a value"
                        );
                    };

                    let else_ty = infer_expr(c, gamma, *e)?;
                    join(gamma, then_ty, else_ty, expr_span)?
                }
            };

            if let Some(err) = deferred {
                return Err(err);
            }

            result
        }

        Expr::CompoundAssign {
            target,
            op_fn,
            value,
        } => {
            let (target, op_fn, value) = (*target, *op_fn, *value);

            // `const` globals are read-only (mirrors the `Expr::Assign` check).
            if let Expr::Global(name) = c.body.expr_arena[target] {
                if gamma.workspace.const_globals.contains(&name) {
                    comp_error!(
                        expr_span,
                        "cannot assign to const `{}`; consts are read-only",
                        name
                    );
                }
            }

            if let Expr::FieldAccess { base, field } = c.body.expr_arena[target] {
                reject_swizzle_write(c, gamma, base, field, expr_span)?;
                reject_value_struct_field_write(c, gamma, base, expr_span)?;
                check_field_write_access(c, gamma, base, field, expr_span)?;
            }

            let target_ty = infer_expr(c, gamma, target)?;
            let unwrapped = peel_nullable(gamma, target_ty, expr_span);
            let value_ty = infer_expr(c, gamma, value)?;

            let candidates = resolve_callee(gamma, &CallFamily::from_name(op_fn));

            if candidates.is_empty() {
                comp_error!(expr_span, "Function `{}` doens't exist", op_fn);
            }

            let arg_tys = [
                resolve(gamma, unwrapped, expr_span),
                resolve(gamma, value_ty, expr_span),
            ];

            let (mid, fid) = if candidates.len() == 1 {
                candidates[0]
            } else {
                let mut arg_ty: FxHashMap<Idx<Expr>, TyId> = FxHashMap::default();
                arg_ty.insert(target, arg_tys[0]);
                arg_ty.insert(value, arg_tys[1]);

                select_overload(
                    c,
                    gamma,
                    &candidates,
                    None,
                    &[target, value],
                    &arg_ty,
                    &arg_tys,
                    op_fn,
                    expr_span,
                )?
            };

            gamma.out.resolved_overload_calls.insert(e, (mid, fid));

            let function = gamma.workspace.module(mid).functions[fid].clone();
            let (subs, _) = gamma.instantiate_generics(&function.generics, expr_span)?;

            let ret_ty = function
                .return_ty
                .unwrap_or_else(|| gamma.workspace.builtins.unit());

            let sub = substitute(gamma, ret_ty, &subs, expr_span)?;
            let ret_ty = resolve(gamma, sub, expr_span);

            coerce(gamma, ret_ty, unwrapped, expr_span)?;

            gamma.workspace.builtins.unit()
        }

        Expr::Return(e) => {
            // Checked, not inferred-then-coerced -- see `Statement::Return`.
            match (e, gamma.declared_ret_ty) {
                (Some(e), Some(declared)) => {
                    check_expr(c, gamma, *e, declared, expr_span)?;
                    validate_frame_ref_return(c, gamma, *e, declared)?;
                }
                (Some(e), None) => {
                    infer_expr(c, gamma, *e)?;
                }
                (None, Some(declared)) => {
                    let unit = gamma.workspace.builtins.unit();
                    coerce(gamma, unit, declared, expr_span)?;
                }
                (None, None) => {}
            }

            gamma.workspace.builtins.never()
        }

        Expr::Break | Expr::Continue => {
            if gamma.loop_depth == 0 {
                comp_error!(
                    expr_span,
                    "`break`/`continue` is only valid inside a loop body"
                );
            }
            gamma.workspace.builtins.never()
        }
    };

    gamma.assign_expr_type(c, e, ty)?;

    Ok(ty)
}

pub fn infer_expr(c: BodyCtx, gamma: &mut Gamma, e: Idx<Expr>) -> Result<TyId, CompilerError> {
    infer_expr_used(c, gamma, e, Use::Value)
}

pub(crate) fn validate_frame_ref_return(
    c: BodyCtx,
    gamma: &mut Gamma,
    e: Idx<Expr>,
    declared: TyId,
) -> Result<(), CompilerError> {
    let span = c.body.expr_spans[e];
    let declared = resolve(gamma, declared, span);
    if frame_ref_target(gamma, declared).is_none() || gamma.allow_frame_ref_return_place {
        return Ok(());
    }

    fn has_valid_origin(c: BodyCtx, gamma: &mut Gamma, e: Idx<Expr>) -> bool {
        let span = c.body.expr_spans[e];
        if let Some(ty) = gamma.out.expr_types.get(e).copied() {
            let ty = resolve(gamma, ty, span);
            if contains_frame_ref(gamma, ty) {
                return true;
            }
        }

        match c.body.expr_arena[e] {
            Expr::Null | Expr::Global(_) => true,
            Expr::FieldAccess { base, .. }
            | Expr::SafeNav { base, .. }
            | Expr::Index { base, .. }
            | Expr::TupleIndex { tuple: base, .. }
            | Expr::As { value: base, .. } => has_valid_origin(c, gamma, base),
            Expr::Block { tail, .. } => tail.is_some_and(|tail| has_valid_origin(c, gamma, tail)),
            Expr::If {
                then_branch,
                else_branch,
                ..
            }
            | Expr::IfLet {
                then_branch,
                else_branch,
                ..
            } => {
                has_valid_origin(c, gamma, then_branch)
                    && else_branch.is_some_and(|branch| has_valid_origin(c, gamma, branch))
            }
            Expr::Match { arms, .. } => {
                let mut arms = arms.into_iter();
                arms.next().is_some_and(|arm| {
                    has_valid_origin(c, gamma, c.body.match_arm_arena[arm].body)
                        && arms.all(|arm| {
                            has_valid_origin(c, gamma, c.body.match_arm_arena[arm].body)
                        })
                })
            }
            _ => false,
        }
    }

    if has_valid_origin(c, gamma, e) {
        return Ok(());
    }

    comp_error!(
        span,
        "cannot return a frame reference to a local place; return an input reference or a reference obtained from owner storage"
    );
}

/// Infer the common scalar type of a half-open range's bounds. Integer
/// literals are context-sensitive here, just as they are in an assignment or
/// call argument; non-literal `int`/`uint` mixtures remain rejected.
pub(crate) fn infer_range_bounds(
    c: BodyCtx,
    gamma: &mut Gamma,
    low: Idx<Expr>,
    high: Idx<Expr>,
) -> Result<TyId, CompilerError> {
    let low_ty = infer_expr(c, gamma, low)?;
    let high_ty = infer_expr(c, gamma, high)?;
    let low_r = resolve(gamma, low_ty, c.body.expr_spans[low]);
    let high_r = resolve(gamma, high_ty, c.body.expr_spans[high]);
    let uint = matches!(gamma.get(low_r), TypeExpr::Builtin(BuiltinTy::Uint))
        || matches!(gamma.get(high_r), TypeExpr::Builtin(BuiltinTy::Uint));

    if uint {
        let uint_ty = gamma.workspace.builtins.uint();
        if matches!(gamma.get(low_r), TypeExpr::Builtin(BuiltinTy::Int))
            && matches!(c.body.expr_arena[low], Expr::Int(_))
        {
            check_expr(c, gamma, low, uint_ty, c.body.expr_spans[low])?;
        }
        if matches!(gamma.get(high_r), TypeExpr::Builtin(BuiltinTy::Int))
            && matches!(c.body.expr_arena[high], Expr::Int(_))
        {
            check_expr(c, gamma, high, uint_ty, c.body.expr_spans[high])?;
        }
        unify(gamma, low_ty, uint_ty, c.body.expr_spans[low])?;
        unify(gamma, high_ty, uint_ty, c.body.expr_spans[high])?;
        Ok(uint_ty)
    } else {
        let int_ty = gamma.workspace.builtins.int();
        unify(gamma, low_ty, int_ty, c.body.expr_spans[low])?;
        unify(gamma, high_ty, int_ty, c.body.expr_spans[high])?;
        Ok(int_ty)
    }
}

/// True when `ty` is a concrete non-nullable type, so `??` / `?.` on it is
/// redundant (dead). Skips `Nullable` (the meaningful case), unresolved `Infer`,
/// and `Error`/`Never`/`Any` -- don't reject on uncertainty.
pub(crate) fn redundant_optional_on(gamma: &mut Gamma, ty: TyId, span: Span) -> bool {
    let r = resolve(gamma, ty, span);
    !matches!(
        gamma.get(r),
        TypeExpr::Nullable(_)
            | TypeExpr::Infer(..)
            | TypeExpr::Builtin(BuiltinTy::Error)
            | TypeExpr::Builtin(BuiltinTy::Never)
            | TypeExpr::Builtin(BuiltinTy::Any)
    )
}


/// A block's statements, scoping and tail -- the shared body of the `Expr::Block`
/// arm above and [`check_expr`]'s. `expected` is `Some` only on the checked
/// path, where it is pushed to the TAIL; everything else is identical, which is
/// the point: the two must not drift.
#[allow(clippy::too_many_arguments)]
fn infer_block(
    c: BodyCtx,
    gamma: &mut Gamma,
    statements: &IdxRange<Statement>,
    tail: &Option<Idx<Expr>>,
    used: Use,
    expr_span: Span,
    expected: Option<TyId>,
) -> TyId {
            let mut g_saved: Vec<(Idx<BindingInfo>, TyId)> = vec![];
            // Snapshot the path-refinement set on entry; restore on exit. This
            // scopes to the block BOTH escaping guard narrows AND
            // assignment-refinements (`x.f = nonNull` marks `x.f` non-null):
            // flow-sensitively, a refinement established inside a block must not
            // survive past it (the other control path didn't run it). Whole-local
            // narrows peel `binding_types` and are restored via `g_saved`. The set
            // is usually empty/small, so the clone is cheap (an empty FxHashSet
            // clone doesn't allocate).
            let refined_snapshot = gamma.refined_nonnull.clone();

            for statement in *statements {
                if let Err(e) = infer_statement(c, gamma, statement) {
                    gamma.body_errors.push(e);
                    continue;
                };

                for (root, fields) in stmt_guard_narrows(gamma, c, statement) {
                    match root {
                        Root::Local(b) if fields.is_empty() => {
                            if let Some(cur) = gamma.out.binding_types.get(b).copied() {
                                let narrowed = peel_nullable(gamma, cur, expr_span);
                                gamma.out.binding_types.insert(b, narrowed);
                                g_saved.push((b, cur));
                            }
                        }
                        _ => {
                            gamma.refined_nonnull.insert((root, fields));
                        }
                    }
                }
            }

            let result = match tail {
                // The ONE difference between inferring a block and checking one:
                // a checked block pushes the hole down to its TAIL. Without it a
                // lambda in tail position records its own type, which is narrower
                // than the hole it fills, and the fn-value ABI disagrees at the
                // two ends (`nobox_fn_value_nullable_return_sentinel`).
                Some(tail) => match expected {
                    Some(exp) => match check_expr(c, gamma, *tail, exp, c.body.expr_spans[*tail]) {
                        Ok(()) => exp,
                        Err(err) => {
                            gamma.body_errors.push(err);
                            gamma.workspace.builtins.error()
                        }
                    },
                    None => match infer_expr_used(c, gamma, *tail, used) {
                        Ok(t) => t,
                        Err(err) => {
                            gamma.body_errors.push(err);
                            gamma.workspace.builtins.error()
                        }
                    },
                },
                None => {
                    let diverges = (*statements)
                        .into_iter()
                        .rfind(|&s| !matches!(c.body.statement_arena[s], Statement::Comment(_)))
                        .is_some_and(|s| matches!(c.body.statement_arena[s], Statement::Return(_)));

                    if diverges {
                        gamma.workspace.builtins.never()
                    } else {
                        gamma.workspace.builtins.unit()
                    }
                }
            };

            for (b, old) in g_saved {
                gamma.out.binding_types.insert(b, old);
            }

            gamma.refined_nonnull = refined_snapshot;

            result
}

pub(crate) fn check_expr(
    c: BodyCtx,
    gamma: &mut Gamma,
    e: Idx<Expr>,
    expected: TyId,
    span: Span,
) -> Result<(), CompilerError> {
    let expected_r = resolve(gamma, expected, span);
    if frame_ref_target(gamma, expected_r).is_some()
        && !matches!(c.body.expr_arena[e], Expr::Null)
    {
        let actual = infer_expr(c, gamma, e)?;
        let actual_r = resolve(gamma, actual, span);
        if !contains_frame_ref(gamma, actual_r)
            && !matches!(
                c.body.expr_arena[e],
                Expr::Var(_) | Expr::FieldAccess { .. } | Expr::Index { .. }
            )
        {
            comp_error!(
                span,
                "a `ref` can only borrow a stable place (a local, field, or indexed slot)"
            );
        }
        coerce(gamma, actual_r, expected_r, span)?;
        return Ok(());
    }

    // Integer literals are int by default, but a non-negative literal in a
    // uint context is admitted without widening arbitrary int values. The
    // range check stays here, before inference can erase the literal's
    // provenance or a backend can narrow it.
    if let Expr::Int(value) = c.body.expr_arena[e] {
        let expected_r = resolve(gamma, expected, span);
        if matches!(gamma.get(expected_r), TypeExpr::Builtin(BuiltinTy::Uint)) {
            if !(0..=u32::MAX as i64).contains(&value) {
                comp_error!(
                    span,
                    "uint literal must be in the range 0..=4294967295, found {}",
                    value
                );
            }
            if gamma.out.expr_types.contains_idx(e) {
                gamma.out.expr_types.insert(e, expected_r);
            } else {
                gamma.assign_expr_type(c, e, expected_r)?;
            }
            return Ok(());
        }
    }

    // A block fills the hole with its TAIL, so push `expected` through rather
    // than inferring the block and coercing the result -- otherwise the arms
    // below never see a tail expression, and a fn body whose tail is a lambda
    // records the lambda's own narrower type.
    if let Expr::Block { statements, tail } = &c.body.expr_arena[e] {
        let ty = infer_block(c, gamma, statements, tail, Use::Value, span, Some(expected));
        // No re-coerce: a tail was checked against `expected` already, and a
        // tail-less block still has to answer for its unit/never type.
        if tail.is_none() {
            coerce(gamma, ty, expected, span)?;
        }
        gamma.assign_expr_type(c, e, ty)?;
        return Ok(());
    }

    if let Expr::Lambda { params, body } = c.body.expr_arena[e] {
        let exp_r = resolve(gamma, expected, span);

        // Lambda literal is a valid value for a NULLABLe function param `(() ->())?`,
        // so look through a nullable wrapper to reach the function type.
        let fn_ty = match *gamma.get(exp_r) {
            TypeExpr::Nullable(inner) => exp_r.with_idx(inner),
            _ => exp_r
        };

        if let TypeExpr::Function { params: ep, ret } = *gamma.get(fn_ty) {
            let expected_params: Vec<TyId> = ep.into_iter().map(|p| fn_ty.with_idx(p)).collect();
            let lam_ty = infer_lambda(
                c,
                gamma,
                params,
                body,
                Some(&expected_params),
                Some(fn_ty.with_idx(ret)),
                c.body.expr_spans[e],
            )?;
            coerce(gamma, lam_ty, expected, span)?;
            // The EXPECTED type, not the lambda's own -- same as the `Array` and
            // `SiblingRef` arms below. A lambda's inferred return is narrower than
            // the hole it fills (`if c { P {..} } else { null }` infers `P`, the
            // `null` arm does not widen it), and codegen reads this type to decide
            // the fn-value ABI at BOTH ends. Recording the narrow one gives the
            // body a return shape with no presence slot while the indirect call
            // site, reading the declared fn type, binds one. Every RETURN position
            // routes here for exactly that reason -- see the `Block` arm above.
            gamma.assign_expr_type(c, e, exp_r)?;
            return Ok(());
        }
    }

    if let Expr::Array(elements) = c.body.expr_arena[e] {
        let exp_r = resolve(gamma, expected, span);
        let has_spread = elements
            .into_iter()
            .any(|el| matches!(c.body.expr_arena[el], Expr::Spread(_)));

        if !has_spread {
            if let TypeExpr::App {
                ctor: TypeCtor::Array,
                params,
            } = *gamma.get(exp_r)
            {
                let elem_ty = exp_r.with_idx(params.into_iter().next().unwrap());

                for el in elements {
                    check_expr(c, gamma, el, elem_ty, c.body.expr_spans[el])?;
                }

                gamma.assign_expr_type(c, e, exp_r)?;
                return Ok(());
            }
        }
    }

    if let Expr::SiblingRef(field) = c.body.expr_arena[e] {
        let exp_r = resolve(gamma, expected, span);

        if let TypeExpr::App {
            ctor: TypeCtor::Nominal(former),
            params,
        } = *gamma.get(exp_r)
        {
            if bone_core::modules::is_intrinsic_key_in(
                former,
                &["FieldRef", "ReadFieldRef", "WriteFieldRef"],
            ) {
                let is_field_ref = bone_core::modules::is_intrinsic_key(former, "FieldRef");
                let is_read_field_ref = bone_core::modules::is_intrinsic_key(former, "ReadFieldRef");
                let is_write_field_ref = bone_core::modules::is_intrinsic_key(former, "WriteFieldRef");
                let mut it = params.into_iter();
                let t = exp_r.with_idx(it.next().unwrap());
                let v = exp_r.with_idx(it.next().unwrap());

                let t_r = resolve(gamma, t, span);
                // `FieldRef` and `WriteFieldRef` carry the WRITE capability
                // (`field_set`, `poll_co_field`), so neither can resolve against a
                // `value` struct, whose fields are immutable. `ReadFieldRef`
                // (read-only, `field_get`) allows it -- reading a value-struct
                // field is fine. Lattice per docs/field-refs-plan.md.
                if is_field_ref || is_write_field_ref {
                    if let TypeExpr::App { ctor: TypeCtor::Nominal(tn), .. } = *gamma.get(t_r) {
                        let is_value = resolve_struct(gamma, tn)
                            .is_some_and(|sid| gamma.workspace.module(sid.module).structs[sid.def].is_value);
                        if is_value {
                            comp_error!(
                                span,
                                "cannot form a writable field reference `.{}` into `value` struct `{}`; value structs are immutable",
                                field,
                                tn
                            );
                        }
                    }
                }
                // ADMISSION: every field ref satisfies `WriteFieldRef`; only a
                // NON-embed field also satisfies the readable formers. An
                // embedded field has no whole value to hand out, and `FieldRef`
                // coerces onward to `ReadFieldRef`, so it re-opens the same
                // extraction (docs/scalarization-and-boxing-design.md S 6).
                if is_field_ref || is_read_field_ref {
                    if let TypeExpr::App { ctor: TypeCtor::Nominal(tn), .. } = *gamma.get(t_r) {
                        let is_embed = resolve_struct(gamma, tn).is_some_and(|sid| {
                            gamma.workspace.module(sid.module).structs[sid.def]
                                .fields
                                .iter()
                                .any(|f| f.name == field && f.is_embed)
                        });
                        if is_embed {
                            comp_error!(
                                span,
                                "`{}` is an `embed` field of `{}`, so it admits only the write-only `WriteFieldRef<{}, _>`; a readable selector would hand out the whole value",
                                field,
                                tn,
                                tn
                            );
                        }
                    }
                }
                if is_field_ref || is_write_field_ref {
                    if let TypeExpr::App { ctor: TypeCtor::Nominal(tn), .. } = *gamma.get(t_r) {
                        let site = gamma.access_site();
                        field_access(gamma, tn, field, site, FieldAccessNeed::Write)
                            .map_err(|e| e.to_compiler_error(span))?;
                    }
                }

                let fty = struct_field_ty(gamma, t_r, field, span)?;

                // The write capability is INVARIANT in V, and `unify` alone does
                // not deliver that: its `(_, Nullable)` arm widens on the right,
                // so `unify(int, int?)` succeeds and a non-null field would hand
                // out a `FieldRef<T, int?>` that `field_set(o, f, null)` writes
                // nil through. A read-only `ReadFieldRef` keeps the widening --
                // reading an `int` as `int?` is sound.
                if is_field_ref || is_write_field_ref {
                    let (fty_r, v_r) = (resolve(gamma, fty, span), resolve(gamma, v, span));
                    let unbound_v = matches!(
                        gamma.get(v_r),
                        TypeExpr::Infer(..) | TypeExpr::Generic(..)
                    );
                    if !unbound_v
                        && matches!(gamma.get(v_r), TypeExpr::Nullable(_))
                        && !matches!(gamma.get(fty_r), TypeExpr::Nullable(_))
                    {
                        comp_error!(
                            span,
                            "type mismatch: expected `{}`, found `{}` -- a writable field reference is invariant, so `.{}` cannot widen to a nullable selector",
                            gamma.render(v_r),
                            gamma.render(fty_r),
                            field
                        );
                    }
                }

                unify(gamma, fty, v, span)?;

                gamma.assign_expr_type(c, e, exp_r)?;

                return Ok(());
            }
        }
    }

    let aty = infer_expr(c, gamma, e)?;

    // `/` is float division; its `float` result cannot fill an int-shaped hole.
    // When coercion actually rejects a `float` against an `int` (NOT a
    // re-bindable numeric var that coerce would widen, e.g. `min(1, x)`), and
    // the expression is (or contains, through arithmetic operators / unary
    // minus) a `/`, replace the bare unify error with a pointer at that `/`
    // suggesting `./` (integer floor division) (docs/float-division-plan.md).
    if let Err(coerce_err) = coerce(gamma, aty, expected, span) {
        let exp_r = resolve(gamma, expected, span);
        let aty_r = resolve(gamma, aty, span);
        if matches!(gamma.get(exp_r), TypeExpr::Builtin(BuiltinTy::Int))
            && matches!(gamma.get(aty_r), TypeExpr::Builtin(BuiltinTy::Float))
        {
            if let Some(div_span) = find_float_div_span(c, e) {
                let mut err = CompilerError::new_with_label(
                    div_span,
                    "`/` is float division; its `float` result does not fit `int` -- use `./` for integer floor division"
                        .to_string(),
                );
                // When the `/` reached the hole through a local binding, keep a
                // breadcrumb at the actual mismatch site.
                if div_span != span {
                    err = err.with_label(span, "this `float` is used where `int` is required".to_string());
                }
                return Err(err);
            }
        }
        // Point a secondary label at where the expected type came from (the
        // `: T` annotation, `-> T` return, or param decl) so the reader sees
        // WHY this slot is `T`. Adds a label; keeps coerce's own message (its
        // specific ones, e.g. the null->`any` note, survive). Skip when the
        // origin is unknown or coincides with the mismatch site itself
        // (docs/error-quality-plan.md P1b).
        if let Some(origin) = gamma.span_of(expected).or_else(|| gamma.span_of(exp_r)) {
            if origin != span && !origin.is_empty() {
                let exp_str = gamma.workspace.render(Some(&gamma.out), exp_r);
                return Err(coerce_err
                    .with_label(origin, format!("expected `{exp_str}` because of this")));
            }
        }
        return Err(coerce_err);
    }

    Ok(())
}

// The closed `as` conversion relation. A pair is admitted only if it is an
// ordinary assignment-compatible one or one of the two explicit numeric
// conversions; everything else is a spanned rejection, never a silent identity.
//
// `as` is not contextual: the target validates an already-inferred source, so
// nothing here may bind an operand variable or discharge a constraint waiting on
// one. That is `::`'s job.
pub(crate) fn check_cast(
    gamma: &mut Gamma,
    from: TyId,
    to: TyId,
    span: Span,
) -> Result<(), CompilerError> {
    let from_r = resolve(gamma, from, span);
    let to_r = resolve(gamma, to, span);

    if has_unbound_infer(gamma, from_r) {
        let (from_str, to_str) = (gamma.render(from_r), gamma.render(to_r));
        return Err(CompilerError::new_with_label(
            span,
            format!(
                "cannot convert `{from_str}` to `{to_str}` with `as`: the operand \
                 is not fully inferred on its own, and `as` does not take types \
                 from its target -- annotate the operand, or ascribe it with \
                 `:: {to_str}`"
            ),
        ));
    }

    // The same rule on the other side. A `_` in the target is an inference hole,
    // so there is no written type to validate against: the relation would pass
    // vacuously and the surrounding expectation would then bind the hole, which
    // is how `let s: string = 42 as _` laundered a rejected pair.
    if has_unbound_infer(gamma, to_r) {
        let (from_str, to_str) = (gamma.render(from_r), gamma.render(to_r));
        return Err(CompilerError::new_with_label(
            span,
            format!(
                "cannot convert `{from_str}` to `{to_str}` with `as`: the target \
                 is not a written type, and `as` validates against one -- name \
                 the target type, or ascribe with `::` to infer it from context"
            ),
        ));
    }

    // Explicit narrowing/widening between the two numeric builtins. `int -> float`
    // is also assignment-compatible; `float -> int` is admitted ONLY here, which
    // is what keeps it out of `::`.
    if matches!(
        (gamma.get(from_r), gamma.get(to_r)),
        (
            TypeExpr::Builtin(BuiltinTy::Int | BuiltinTy::Uint),
            TypeExpr::Builtin(BuiltinTy::Float)
        ) | (
            TypeExpr::Builtin(BuiltinTy::Float),
            TypeExpr::Builtin(BuiltinTy::Int | BuiltinTy::Uint)
        ) | (
            TypeExpr::Builtin(BuiltinTy::Int),
            TypeExpr::Builtin(BuiltinTy::Uint)
        ) | (
            TypeExpr::Builtin(BuiltinTy::Uint),
            TypeExpr::Builtin(BuiltinTy::Int)
        )
    ) {
        return Ok(());
    }

    // `coerce` decides assignment compatibility by unifying, which records
    // bindings. Ask it against a checkpoint and roll back whatever it wrote, so
    // the answer is a classification and not inference evidence.
    let snap = gamma.out.checkpoint();
    let compatible = gamma.trial(|g| coerce(g, from_r, to_r, span).is_ok());
    gamma.out.rollback_to(snap);
    if compatible {
        return Ok(());
    }

    let (from_str, to_str) = (gamma.render(from_r), gamma.render(to_r));
    Err(CompilerError::new_with_label(
        span,
        format!(
            "cannot convert `{from_str}` to `{to_str}` with `as`; \
             `as` converts only between numeric types, or where `{from_str}` \
             is already assignable to `{to_str}`"
        ),
    ))
}

fn check_bitcast(
    gamma: &mut Gamma,
    from: TyId,
    to: TyId,
    span: Span,
) -> Result<(), CompilerError> {
    let from_r = resolve(gamma, from, span);
    let to_r = resolve(gamma, to, span);
    if matches!(
        (gamma.get(from_r), gamma.get(to_r)),
        (
            TypeExpr::Builtin(BuiltinTy::Int),
            TypeExpr::Builtin(BuiltinTy::Uint)
        ) | (
            TypeExpr::Builtin(BuiltinTy::Uint),
            TypeExpr::Builtin(BuiltinTy::Int)
        )
    ) {
        return Ok(());
    }
    let (from_str, to_str) = (gamma.render(from_r), gamma.render(to_r));
    Err(CompilerError::new_with_label(
        span,
        format!(
            "cannot bitcast `{from_str}` to `{to_str}`: bitcast is only defined between `int` and `uint`"
        ),
    ))
}

fn check_numeric_literal_cast(
    c: BodyCtx,
    gamma: &mut Gamma,
    value: Idx<Expr>,
    target: TyId,
    span: Span,
) -> Result<(), CompilerError> {
    let target = resolve(gamma, target, span);
    let TypeExpr::Builtin(target) = *gamma.get(target) else {
        return Ok(());
    };
    let message = |value: String| {
        CompilerError::new_with_label(
            span,
            format!("numeric literal {value} is out of range for `{target:?}`"),
        )
    };
    if let Some(value) = integer_literal_value(c, value) {
        if matches!(target, BuiltinTy::Uint) && !(0..=u32::MAX as i64).contains(&value) {
            return Err(message(value.to_string()));
        }
    }
    if let Some(value) = float_literal_value(c, value) {
        if matches!(target, BuiltinTy::Uint)
            && (!value.is_finite() || value < 0.0 || value > u32::MAX as f64)
        {
            return Err(message(value.to_string()));
        }
        if matches!(target, BuiltinTy::Int)
            && (!value.is_finite() || value < i64::MIN as f64 || value >= 9223372036854775808.0)
        {
            return Err(message(value.to_string()));
        }
    }
    Ok(())
}

fn integer_literal_value(c: BodyCtx, e: Idx<Expr>) -> Option<i64> {
    match c.body.expr_arena[e] {
        Expr::Int(value) => Some(value),
        Expr::UnaryMinus(inner) => integer_literal_value(c, inner)?.checked_neg(),
        _ => None,
    }
}

fn float_literal_value(c: BodyCtx, e: Idx<Expr>) -> Option<f64> {
    match c.body.expr_arena[e] {
        Expr::Float(value) => Some(value.0),
        Expr::UnaryMinus(inner) => Some(-float_literal_value(c, inner)?),
        _ => None,
    }
}

// Find the `/` (float division) responsible for a `float` value flowing into an
// `int` hole. Float-ness reaches that hole through arithmetic operators
// (`__op_*`), unary minus, and local `let` bindings, so the walk descends those
// and follows a `Var` back to its initializer; a `float` from anywhere else (a
// call's return type, a field, a reassignment) is not a syntactically-traceable
// `/` and yields None. Returns the offending `/`'s span for the diagnostic.
pub(crate) fn find_float_div_span(c: BodyCtx, e: Idx<Expr>) -> Option<Span> {
    find_float_div_span_rec(c, e, 0)
}

pub(crate) fn find_float_div_span_rec(c: BodyCtx, e: Idx<Expr>, depth: usize) -> Option<Span> {
    // Guard against a pathological binding chain; well-formed HIR terminates
    // (an initializer only references earlier bindings) but recovery may not.
    if depth > 32 {
        return None;
    }
    match &c.body.expr_arena[e] {
        Expr::Call {
            callee: Callee::Unresolved { name, .. },
            args,
            ..
        } => {
            let name = *name;
            let args = *args;
            if name == "__op_div" {
                return Some(c.body.expr_spans[e]);
            }
            if name.as_str().starts_with("__op_") {
                for a in args {
                    if let Some(s) = find_float_div_span_rec(c, a, depth + 1) {
                        return Some(s);
                    }
                }
            }
            None
        }
        Expr::UnaryMinus(x) => find_float_div_span_rec(c, *x, depth + 1),
        Expr::Var(b) => {
            let init = binding_initializer(c, *b)?;
            find_float_div_span_rec(c, init, depth + 1)
        }
        _ => None,
    }
}

// The `let`/`static` initializer expression for a simple single-name binding,
// so the float-div walk can follow a local variable back to the `/` that made
// it float. Params, tuple/loop/pattern bindings have no single initializer here
// and yield None.
pub(crate) fn binding_initializer(c: BodyCtx, b: Idx<BindingInfo>) -> Option<Idx<Expr>> {
    c.body.statement_arena.iter().find_map(|(_, stmt)| match stmt {
        Statement::Let { name, value }
        | Statement::Static { name, value }
        | Statement::LetNewtype { var: name, value, .. }
            if *name == b =>
        {
            Some(*value)
        }
        _ => None,
    })
}

pub(crate) fn coroutine_yrn(
    gamma: &mut Gamma,
    ret: TyId,
    span: Span,
) -> Result<(TyId, TyId, TyId), CompilerError> {
    let r = resolve(gamma, ret, span);

    match *gamma.get(r) {
        TypeExpr::App {
            ctor: TypeCtor::Nominal(n),
            params,
        } if bone_core::modules::is_intrinsic_key(n, "Coroutine") => {
            let mut it = params.into_iter();
            let (y, rr, nn) = (it.next(), it.next(), it.next());

            match (y, rr, nn) {
                (Some(y), Some(rr), Some(nn)) => {
                    Ok((r.with_idx(y), r.with_idx(rr), r.with_idx(nn)))
                }
                _ => comp_error!(
                    span,
                    "Coroutine type must have 3 parameters (yield, return, next), got {}",
                    params.len()
                ),
            }
        }

        _ => comp_error!(
            span,
            "Expected a Coroutine type, got {}",
            gamma.workspace.render(Some(&gamma.out), r)
        ),
    }
}
