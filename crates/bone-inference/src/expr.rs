use super::*;
use bone_core::hir_walk;

/// Contextual checking makes returned lambdas use the caller's fn-value ABI.
fn infer_return(
    c: BodyCtx,
    gamma: &mut Gamma,
    e: Option<Idx<Expr>>,
    span: Span,
) -> Result<(), CompilerError> {
    if gamma.in_defer {
        comp_error!(
            span,
            "`return` is not allowed inside a `defer` body: the deferred block \
             runs while the function is already returning"
        );
    }
    if let (Some(e), Some(returns)) = (e, gamma.lambda_returns.as_mut()) {
        returns.push(e);
    }
    match (e, gamma.declared_ret_ty) {
        (Some(e), Some(declared)) => {
            check_expr(c, gamma, e, declared, span)?;
            validate_frame_ref_return(c, gamma, e, declared)?;
        }
        (Some(e), None) => {
            infer_expr(c, gamma, e)?;
        }
        (None, Some(declared)) => {
            let unit = gamma.workspace.builtins.unit();
            coerce(gamma, unit, declared, span)?;
        }
        (None, None) => {}
    }
    Ok(())
}

fn infer_initialized_binding(
    c: BodyCtx,
    gamma: &mut Gamma,
    name: Idx<BindingInfo>,
    value: Idx<Expr>,
) -> Result<TyId, CompilerError> {
    match c.body.binding_arena[name].declared_ty {
        Some(ann) => {
            let span = c
                .body
                .expr_spans
                .get(value)
                .copied()
                .unwrap_or(Span::unknown());
            // Keep the annotated type available to editor queries after an initializer error.
            gamma.assign_named_type(name, ann)?;
            check_expr(c, gamma, value, ann, span)?;
            Ok(ann)
        }
        None => match infer_expr(c, gamma, value) {
            Ok(ty) => Ok(ty),
            Err(e) => {
                let poison = gamma.workspace.builtins.error();
                gamma.assign_named_type(name, poison)?;
                Err(e)
            }
        },
    }
}

fn check_write_target(
    c: BodyCtx,
    gamma: &mut Gamma,
    target: Idx<Expr>,
    span: Span,
) -> Result<(), CompilerError> {
    match c.body.expr_arena[target] {
        Expr::Global(name) if gamma.workspace.const_globals.contains(&name) => comp_error!(
            span,
            "cannot assign to const `{}`; consts are read-only",
            name
        ),
        Expr::FieldAccess { base, field } => {
            reject_swizzle_write(c, gamma, base, field, span)?;
            reject_value_struct_field_write(c, gamma, base, span)?;
            check_field_write_access(c, gamma, base, field, span)
        }
        _ => Ok(()),
    }
}

pub(crate) fn infer_statement(
    c: BodyCtx,
    gamma: &mut Gamma,
    s: Idx<Statement>,
) -> Result<(), CompilerError> {
    let span = c.body.statement_spans[s];

    match &c.body.statement_arena[s] {
        Statement::Expr(e) => {
            infer_expr_used(c, gamma, *e, Use::Discard)?;
        }

        Statement::Defer(e) => {
            // A deferred block runs at scope exit, not here: it is outside any
            // enclosing loop, and it cannot return from the function.
            let saved_loop = std::mem::replace(&mut gamma.loop_depth, 0);
            let saved_defer = std::mem::replace(&mut gamma.in_defer, true);
            let res = infer_expr_used(c, gamma, *e, Use::Discard);
            gamma.loop_depth = saved_loop;
            gamma.in_defer = saved_defer;
            res?;
        }

        Statement::Return(e) => infer_return(c, gamma, *e, span)?,

        Statement::Let {
            pattern,
            value,
            else_block,
        } => {
            let (pattern, value, else_block) = (*pattern, *value, *else_block);

            if let Some(name) = c.body.pattern_single_bind(pattern)
                && else_block.is_none()
            {
                let binding_ty = infer_initialized_binding(c, gamma, name, value)?;
                gamma.assign_named_type(name, binding_ty)?;

                // A bare enum variant handles only its tag and can make a pattern refutable.
                if let Some(tag) = unqualified_arm_variant(c, gamma, pattern, binding_ty, span)? {
                    require_irrefutable(
                        c,
                        gamma,
                        pattern,
                        binding_ty,
                        Some(tag),
                        span,
                        "a pattern that can fail needs `match`, `if let`, or `let ... else`",
                    )?;
                }

                if contains_frame_ref(gamma, binding_ty) {
                    let provenance = frame_ref_provenance(c, gamma, value);
                    if !provenance.retainable() {
                        reject_unstable_frame_ref(c.body.expr_spans[value], provenance)?;
                    }
                    gamma.ref_binding_provenance.insert(name, provenance);
                }
            } else {
                reject_cf_in_destructure_rhs(c, value)?;
                let (value_ty, deferred) = match infer_expr(c, gamma, value) {
                    Ok(t) => (t, None),
                    Err(e) => (gamma.workspace.builtins.error(), Some(e)),
                };

                let scrut_ty = if else_block.is_some() {
                    peel_nullable(gamma, value_ty, span)
                } else {
                    value_ty
                };

                let expected = annotated_pattern_type(c, gamma, pattern, scrut_ty, span)?;

                let tag = unqualified_arm_variant(c, gamma, pattern, scrut_ty, span)?;
                check_pattern(c, gamma, pattern, expected, span)?;

                match else_block {
                    None => require_irrefutable(
                        c,
                        gamma,
                        pattern,
                        scrut_ty,
                        tag,
                        span,
                        "a pattern that can fail needs `match`, `if let`, or `let ... else`",
                    )?,
                    Some(else_e) => {
                        let vt = resolve(gamma, value_ty, span);
                        let peeled = resolve(gamma, scrut_ty, span);
                        let nullable = !types_equal(gamma, vt, peeled);
                        let poisoned = matches!(gamma.get(vt), TypeExpr::Builtin(BuiltinTy::Error));
                        if !nullable
                            && !poisoned
                            && pattern_is_irrefutable(c, gamma, pattern, scrut_ty, tag, span)
                        {
                            comp_error!(
                                span,
                                "this pattern always matches a non-null value, so the miss path \
                                 (the `else`, or the `while let` exit) never runs"
                            );
                        }
                        let ety = infer_expr(c, gamma, else_e)?;
                        let ety = resolve(gamma, ety, span);
                        if !matches!(
                            gamma.get(ety),
                            TypeExpr::Builtin(BuiltinTy::Never | BuiltinTy::Error)
                        ) {
                            comp_error!(
                                c.body.expr_spans[else_e],
                                "the `else` block of `let ... else` must diverge: end it with \
                                 `return`, `break`, `continue`, or a loop that never breaks"
                            );
                        }
                    }
                }

                if let Some(err) = deferred {
                    return Err(err);
                }
            }
        }

        Statement::Static { name, value } => {
            if gamma.coroutine_ret.is_some() {
                comp_error!(
                    c.body.statement_spans[s],
                    "`static` is not allowed in a `co fn`: a shared cell across yields corrupts"
                );
            }

            let binding_ty = infer_initialized_binding(c, gamma, *name, *value)?;

            if contains_frame_ref(gamma, binding_ty) {
                comp_error!(
                    c.body.statement_spans[s],
                    "a `static` local cannot store a frame reference; store an `Entity<T>` or a value instead"
                );
            }

            gamma.assign_named_type(*name, binding_ty)?;

            reject_captures(c, *value, c.body.expr_spans[*value], span, true)?;
        }

        Statement::Fn {
            name,
            params,
            ret_ty,
            body,
        } => {
            let (name, params, ret_ty, body) = (*name, *params, *ret_ty, *body);

            let mut param_tys = Vec::with_capacity(params.len());
            for pb in params {
                let ty = c.body.binding_arena[pb]
                    .declared_ty
                    .unwrap_or_else(|| gamma.workspace.builtins.error());
                gamma.assign_named_type(pb, ty)?;
                if contains_frame_ref(gamma, ty) {
                    gamma
                        .ref_binding_provenance
                        .insert(pb, RefProvenance::Stable);
                }
                param_tys.push(ty);
            }

            // A nested function reads globals at call time, after the enclosing checks.
            let saved_ret = gamma.declared_ret_ty.take();
            let saved_coro = gamma.coroutine_ret.take();
            let saved_returns = gamma.lambda_returns.take();
            let saved_ref_source =
                std::mem::replace(&mut gamma.allow_frame_ref_return_place, false);
            let saved_globals = gamma.take_global_refinements();

            let result = (|| -> Result<(), CompilerError> {
                match ret_ty {
                    Some(ret) => {
                        // Seed the signature before inferring self-recursive calls.
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
            gamma.lambda_returns = saved_returns;
            gamma.allow_frame_ref_return_place = saved_ref_source;
            result?;

            reject_captures(c, body, span, span, false)?;
        }

        Statement::Comment(_) => (),

        Statement::For {
            pattern,
            iter,
            body,
        } => {
            let (pattern, iter, body) = (*pattern, *iter, *body);

            let (elem_ty, deferred) = match infer_for_elem(c, gamma, iter, span) {
                Ok(t) => (t, None),
                Err(e) => (gamma.workspace.builtins.error(), Some(e)),
            };

            let expected = annotated_pattern_type(c, gamma, pattern, elem_ty, span)?;
            let tag = unqualified_arm_variant(c, gamma, pattern, elem_ty, span)?;
            check_pattern(c, gamma, pattern, expected, span)?;
            require_irrefutable(
                c,
                gamma,
                pattern,
                elem_ty,
                tag,
                span,
                "a `for` pattern must match every element; use `match` or `if let` in the body",
            )?;
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

        Statement::Break | Statement::Continue => {
            if gamma.loop_depth == 0 {
                comp_error!(span, "`break`/`continue` is only valid inside a loop body");
            }
        }
    };

    drain_ready_pendings(gamma)?;

    Ok(())
}

fn annotated_pattern_type(
    c: BodyCtx,
    gamma: &mut Gamma,
    pattern: Idx<Pattern>,
    ty: TyId,
    span: Span,
) -> Result<TyId, CompilerError> {
    if let Some(binding) = c.body.pattern_single_bind(pattern)
        && let Some(annotation) = c.body.binding_arena[binding].declared_ty
    {
        coerce(gamma, ty, annotation, span)?;
        return Ok(annotation);
    }
    Ok(ty)
}

/// Refinements from one arrival don't hold at a shared else branch.
fn branch_narrows<'n>(c: BodyCtx, branch: Idx<Expr>, narrows: &'n [Narrow]) -> &'n [Narrow] {
    if c.body.shared_branch_targets.contains(&branch) {
        &[]
    } else {
        narrows
    }
}

/// Type a shared else branch before clause refinements reach the inference cache.
fn pin_shared_else(
    c: BodyCtx,
    gamma: &mut Gamma,
    else_branch: Option<Idx<Expr>>,
    used: Use,
) -> Result<(), CompilerError> {
    if let Some(e) = else_branch
        && c.body.shared_branch_targets.contains(&e)
        && gamma.out.expr_types.get(e).is_none()
    {
        infer_expr_used(c, gamma, e, used)?;
    }
    Ok(())
}

pub(crate) fn block_ends_in_exit(c: BodyCtx, statements: IdxRange<Statement>) -> bool {
    statements
        .into_iter()
        .rfind(|&s| !matches!(c.body.statement_arena[s], Statement::Comment(_)))
        .is_some_and(|s| {
            matches!(
                c.body.statement_arena[s],
                Statement::Return(_) | Statement::Break | Statement::Continue
            ) || loop_never_exits(c, s)
        })
}

fn loop_never_exits(c: BodyCtx, s: Idx<Statement>) -> bool {
    let Statement::While { cond, body } = c.body.statement_arena[s] else {
        return false;
    };
    matches!(c.body.expr_arena[cond], Expr::Bool(true))
        && !reaches_break(c.body, hir_walk::HirChild::Expr(body))
}

fn reaches_break(body: &bone_core::hir::Body, node: hir_walk::HirChild) -> bool {
    use hir_walk::HirChild;
    match node {
        HirChild::Expr(e) => match body.expr_arena[e] {
            Expr::Break => return true,
            Expr::Lambda { .. } => return false,
            _ => {}
        },
        HirChild::Statement(s) => match body.statement_arena[s] {
            Statement::Break => return true,
            // A break in a nested loop's header can still exit this loop.
            Statement::While { cond, .. } => {
                return reaches_break(body, HirChild::Expr(cond));
            }
            Statement::For { iter, .. } => {
                return reaches_break(body, HirChild::Expr(iter));
            }
            Statement::Fn { .. } => return false,
            _ => {}
        },
        HirChild::Pattern(_) => return false,
    }
    let mut found = false;
    body.for_each_child(node, |child| {
        found = found || reaches_break(body, child);
    });
    found
}

pub(crate) fn infer_expr_used(
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

        Expr::Error => gamma.workspace.builtins.error(),
        Expr::Hole(_) => gamma.fresh_callsite_var(*HOLE, expr_span),

        Expr::InterpolatedString(parts) => {
            for p in *parts {
                if let InterpPart::Value { expr, spec } = c.body.interp_part_arena[p] {
                    let hole_ty = infer_expr(c, gamma, expr)?;
                    check_interpolated_value(gamma, hole_ty, c.body.expr_spans[expr])?;

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
        Expr::Not(inner, _) => {
            let inner_ty = infer_expr(c, gamma, *inner)?;
            let resolved = resolve(gamma, inner_ty, c.body.expr_spans[*inner]);

            if matches!(gamma.get(resolved), TypeExpr::Builtin(BuiltinTy::Bool)) {
                gamma.workspace.builtins.bool()
            } else {
                resolve_named_call(gamma, c, e, None)?
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

            let (y, _r, n) = coroutine_yrn(gamma, ret, expr_span)?;

            if let Some(e) = opt {
                let et = infer_expr(c, gamma, e)?;
                coerce(gamma, et, y, c.body.expr_spans[e])?;
            }

            // Apply the resumer's possible mutations after checking the yielded value.
            gamma.invalidate_mutable_refinements();

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

            gamma.invalidate_mutable_refinements();

            inner_r
        }

        Expr::UnaryMinus(inner, _) => {
            let inner_ty = infer_expr(c, gamma, *inner)?;
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

                _ => resolve_named_call(gamma, c, e, None)?,
            }
        }

        Expr::IncDec {
            target, decrement, ..
        } => {
            let op = if *decrement { "--" } else { "++" };
            match c.body.expr_arena[*target] {
                Expr::Var(_) | Expr::Global(_) | Expr::FieldAccess { .. } | Expr::Index { .. } => {}
                Expr::SafeNav { .. } => comp_error!(
                    expr_span,
                    "`{}` cannot be applied through `?.`; write the update as `{} 1` on the \
                     same path",
                    op,
                    if *decrement { "-=" } else { "+=" },
                ),
                _ => comp_error!(expr_span, "`{}` needs a variable, field, or element", op),
            }
            check_write_target(c, gamma, *target, expr_span)?;
            let inner_ty = infer_expr(c, gamma, *target)?;
            let inner_ty = resolve(gamma, inner_ty, c.body.expr_spans[*target]);
            if !may_be_numeric(gamma, inner_ty, expr_span) {
                comp_error!(
                    expr_span,
                    "`{}` needs a numeric target, but this has type `{}`",
                    op,
                    gamma.render(inner_ty),
                );
            }
            inner_ty
        }

        Expr::As {
            value,
            target,
            kind,
        } => {
            let (value, kind) = (*value, *kind);
            let target = resolve(gamma, *target, expr_span);
            let value_span = c.body.expr_spans[value];
            match kind {
                // `::` supplies context to a bare null or an unannotated lambda parameter.
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
                    ctor: TypeCtor::Nominal(_),
                    ..
                } => {
                    unify(
                        gamma,
                        index_ty,
                        gamma.workspace.builtins.int(),
                        c.body.expr_spans[index],
                    )?;

                    let Some(elem) = buffer_elem(gamma, base_ty) else {
                        comp_error!(
                            expr_span,
                            "cannot index into type {}",
                            gamma.render(base_ty)
                        )
                    };

                    resolve(gamma, elem, expr_span)
                }

                _ => comp_error!(
                    expr_span,
                    "cannot index into type {}",
                    gamma.render(base_ty)
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
            Some(ty)
                if gamma
                    .refined_nonnull
                    .contains(&(Root::Global(*name), vec![])) =>
            {
                peel_nullable(gamma, ty, c.body.expr_spans[e])
            }

            Some(ty) => ty,

            // Backend siblings are a single logical function value.
            None => match &*collapse_backend_variants(
                gamma,
                &resolve_callee(gamma, &CallFamily::Resolved(*name)).visible,
            ) {
                [(mid, fid)] => {
                    let function = &gamma.workspace.module(*mid).functions[*fid];
                    gamma.out.resolved_overload_calls.insert(e, (*mid, *fid));

                    // Bounded functions are specialized at calls and have no runtime identity to store.
                    if function.has_iterator_bounds() {
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
                let (lhs_ty, rhs_ty) = infer_uint_aware_operands(c, gamma, *lhs, *rhs)?;

                let non_null_ty = match (c.body.expr_arena[*lhs], c.body.expr_arena[*rhs]) {
                    (_, Expr::Null) => Some(lhs_ty),
                    (Expr::Null, _) => Some(rhs_ty),
                    _ => None,
                };
                if let Some(ty) = non_null_ty {
                    if redundant_optional_on(gamma, ty, expr_span) {
                        let rendered = gamma.render(ty);
                        comp_error!(
                            expr_span,
                            "Cannot compare non-nullable type '{}' with null",
                            rendered
                        );
                    }
                }

                // Equality would compare whether lambda representations are shared or freshly allocated.
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
                let (lhs_ty, rhs_ty) = infer_uint_aware_operands(c, gamma, *lhs, *rhs)?;
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
                let (lhs_ty, rhs_ty) = infer_uint_aware_operands(c, gamma, *lhs, *rhs)?;

                let joined = join(gamma, lhs_ty, rhs_ty, expr_span)?;

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
                        gamma.render(resolved)
                    );
                }

                gamma.workspace.builtins.bool()
            }

            hir::BinOp::And | hir::BinOp::Or => {
                let bool_ty = gamma.workspace.builtins.bool();
                let lhs_ty = infer_expr(c, gamma, *lhs)?;
                unify(gamma, lhs_ty, bool_ty, c.body.expr_spans[*lhs])?;

                let (then_narrows, else_narrows) = detect_null_narrow(gamma, c, *lhs);
                let narrows = if matches!(op, hir::BinOp::And) {
                    &then_narrows
                } else {
                    &else_narrows
                };
                let rhs_span = c.body.expr_spans[*rhs];
                let rhs_ty = infer_branch(c, gamma, *rhs, narrows, rhs_span, Use::Value)?;
                unify(gamma, rhs_ty, bool_ty, rhs_span)?;

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
            infer_if_arms(
                c,
                gamma,
                *then_branch,
                *else_branch,
                &narrow_then,
                &narrow_else,
                used,
                expr_span,
                "`if` without a final `else` cannot be used as a value",
            )?
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

            let arr_ty = gamma.out.ty_arena.alloc(TypeExpr::App {
                ctor: TypeCtor::Array,
                params,
            });

            let ret_ty = TyId::Local(arr_ty);

            resolve(gamma, ret_ty, expr_span)
        }

        Expr::Tuple(items) => {
            let tys = items.map(|x| infer_expr(c, gamma, x)).try_vec()?;
            tuple_of(gamma, &tys, expr_span)
        }

        Expr::Call {
            callee,
            args,
            generic_args,
            ..
        } => {
            let args = *args;

            match callee {
                hir::Callee::Unresolved { .. } => resolve_named_call(gamma, c, e, None)?,

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
                        comp_error!(
                            expr_span,
                            "explicit type arguments require a named function call"
                        );
                    }
                    call_fn_value(c, gamma, fn_ty, args, expr_span)?
                }

                hir::Callee::Value(callee) => {
                    let fn_ty = infer_expr(c, gamma, *callee)?;
                    if !generic_args.is_empty() {
                        comp_error!(
                            expr_span,
                            "explicit type arguments require a named function call"
                        );
                    }
                    call_fn_value(c, gamma, fn_ty, args, expr_span)?
                }
            }
        }

        Expr::StructLiteral {
            name,
            fields,
            bases,
        } => {
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

            // Each base supplies all unwritten fields, so two bases would collide.
            let has_base = !bases.is_empty();
            if bases.len() > 1 {
                comp_error!(
                    expr_span,
                    "a functional-update literal takes at most one `..base`"
                );
            }
            for b in bases {
                check_expr(c, gamma, b, result_ty, c.body.expr_spans[b])?;
            }

            check_named_fields(
                c,
                gamma,
                Some(name),
                &def.fields,
                &provided,
                &subs,
                expr_span,
                &what,
                has_base,
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

        Expr::SafeNavCall { receiver, .. } => {
            let receiver = *receiver;
            let recv_ty = infer_expr(c, gamma, receiver)?;
            if redundant_optional_on(gamma, recv_ty, expr_span) {
                comp_error!(
                    c.body.expr_spans[receiver],
                    "receiver of `?.` is not nullable -- use `.` instead"
                );
            }
            let inner = peel_nullable(gamma, recv_ty, expr_span);

            let ret = resolve_named_call(gamma, c, e, Some(inner))?;

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

            let def = &gamma.workspace.module(eid.module).enums[eid.def];
            let (subs, arg_tys) = gamma.instantiate_generics(&def.generics, expr_span)?;

            let vdef = def
                .variants
                .iter()
                .find(|v| v.name == variant)
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

                    check_named_fields(
                        c, gamma, None, fields, &provided, &subs, expr_span, &what, false,
                    )?;
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
            if !matches!(
                c.body.expr_arena[target],
                Expr::Var(_)
                    | Expr::Global(_)
                    | Expr::FieldAccess { .. }
                    | Expr::Index { .. }
                    | Expr::SafeNav { .. }
            ) {
                comp_error!(
                    c.body.expr_spans[target],
                    "cannot assign to this expression: it is not a variable, field, or element",
                );
            }
            check_write_target(c, gamma, target, expr_span)?;
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

            check_expr(c, gamma, value, target_ty, c.body.expr_spans[value])?;
            if value_int_field_literal(c, value).is_some()
                && let Expr::FieldAccess { base, .. } | Expr::SafeNav { base, .. } =
                    c.body.expr_arena[target]
            {
                let base_ty = infer_expr(c, gamma, base)?;
                let base_ty = resolve(gamma, base_ty, c.body.expr_spans[base]);
                if struct_fields_of(gamma, base_ty, expr_span).is_some_and(|(name, _)| {
                    resolve_struct(gamma, name).is_some_and(|sid| {
                        gamma.workspace.module(sid.module).structs[sid.def].is_value
                    })
                }) {
                    check_value_int_field_literal(c, gamma, value, target_ty)?;
                }
            }
            let value_ty = infer_expr(c, gamma, value)?;

            let vr = resolve(gamma, value_ty, expr_span);
            // Contextual checking records a nullable callback's declared ABI,
            // but a lambda literal still stores a non-null function value.
            let value_nonnull = matches!(c.body.expr_arena[value], Expr::Lambda { .. })
                || !matches!(gamma.get(vr), TypeExpr::Nullable(_));

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
            let mut unguarded: Vec<exhaustive::ArmPattern> = Vec::new();

            for arm_idx in arms {
                let arm = c.body.match_arm_arena[arm_idx];

                if matches!(c.body.pattern_arena[arm.pattern], Pattern::Wildcard)
                    && arm.guard.is_none()
                {
                    if seen_unguarded_wildcard {
                        comp_error!(
                            c.body.expr_spans[arm.body],
                            "duplicate wildcard pattern in match expression - only one unguarded `_` arm is allowed"
                        );
                    }

                    seen_unguarded_wildcard = true;
                }

                let tag = unqualified_arm_variant(c, gamma, arm.pattern, scrut_ty, expr_span)?;
                check_pattern(c, gamma, arm.pattern, scrut_ty, expr_span)?;

                if let Some(guard) = arm.guard {
                    let guard_ty = infer_expr(c, gamma, guard)?;
                    unify(gamma, guard_ty, gamma.workspace.builtins.bool(), expr_span)?;
                } else {
                    unguarded.push(exhaustive::ArmPattern {
                        pattern: arm.pattern,
                        tag,
                    });
                }

                let body_ty = infer_expr_used(c, gamma, arm.body, used)?;

                if used == Use::Value {
                    match result {
                        None => result = Some(body_ty),
                        Some(rt) => result = Some(join(gamma, body_ty, rt, expr_span)?),
                    }
                }
            }

            // Keep partial inference and the diagnostic prefix used by the LSP fill-arms action.
            let missing =
                exhaustive::missing_match_patterns(c, gamma, scrut_ty, &unguarded, expr_span);
            if !missing.is_empty() {
                let names = missing.join(", ");
                gamma.body_errors.push(CompilerError::new_with_label(
                    expr_span,
                    format!("Non-exhaustive match expression. Missing patterns: {names}"),
                ));
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
                Ok(t) => (peel_nullable(gamma, t, expr_span), None),
                Err(err) => (gamma.workspace.builtins.error(), Some(err)),
            };

            unqualified_arm_variant(c, gamma, *pattern, unwrapped, expr_span)?;
            check_pattern(c, gamma, *pattern, unwrapped, expr_span)?;
            let result = infer_if_arms(
                c,
                gamma,
                *then_branch,
                *else_branch,
                &[],
                &[],
                used,
                expr_span,
                "`if let` without an `else` cannot be used as a value",
            )?;

            if let Some(err) = deferred {
                return Err(err);
            }

            result
        }

        Expr::CompoundAssign {
            target,
            op_fn,
            value,
            family,
        } => {
            let (target, op_fn, value, family) = (*target, *op_fn, *value, *family);

            check_write_target(c, gamma, target, expr_span)?;

            let target_ty = infer_expr(c, gamma, target)?;
            let unwrapped = peel_nullable(gamma, target_ty, expr_span);
            let value_ty = infer_expr(c, gamma, value)?;

            let candidates = resolve_callee(gamma, &family);

            if candidates.visible.is_empty() {
                comp_error!(expr_span, "Function `{}` doens't exist", op_fn);
            }

            let arg_tys = [
                resolve(gamma, unwrapped, expr_span),
                resolve(gamma, value_ty, expr_span),
            ];

            let (mid, fid) = if candidates.visible.len() == 1 {
                candidates.visible[0]
            } else {
                let mut arg_ty: FxHashMap<Idx<Expr>, TyId> = FxHashMap::default();
                arg_ty.insert(target, arg_tys[0]);
                arg_ty.insert(value, arg_tys[1]);

                select_overload(
                    c,
                    gamma,
                    family,
                    &candidates,
                    None,
                    &[target, value],
                    &arg_ty,
                    &arg_tys,
                    IdxRange::new_empty(),
                    op_fn,
                    expr_span,
                )?
            };

            gamma.out.resolved_overload_calls.insert(e, (mid, fid));

            let function = &gamma.workspace.module(mid).functions[fid];
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
            infer_return(c, gamma, *e, expr_span)?;
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

/// An integer literal facing a `uint` operand is checked as `uint` rather than
/// left an `int`.
fn infer_if_arms(
    c: BodyCtx,
    gamma: &mut Gamma,
    then_branch: Idx<Expr>,
    else_branch: Option<Idx<Expr>>,
    then_narrows: &[Narrow],
    else_narrows: &[Narrow],
    used: Use,
    span: Span,
    missing_else: &str,
) -> Result<TyId, CompilerError> {
    pin_shared_else(c, gamma, else_branch, used)?;
    let then_ty = infer_branch(c, gamma, then_branch, then_narrows, span, used)?;
    let else_ty = match else_branch {
        Some(branch) => Some(infer_branch(
            c,
            gamma,
            branch,
            branch_narrows(c, branch, else_narrows),
            span,
            used,
        )?),
        None => None,
    };
    match (used, else_ty) {
        (Use::Discard, _) => Ok(gamma.workspace.builtins.unit()),
        (Use::Value, Some(else_ty)) => join(gamma, then_ty, else_ty, span),
        (Use::Value, None) => comp_error!(span, "{missing_else}"),
    }
}

fn infer_uint_aware_operands(
    c: BodyCtx,
    gamma: &mut Gamma,
    lhs: Idx<Expr>,
    rhs: Idx<Expr>,
) -> Result<(TyId, TyId), CompilerError> {
    let lhs_ty = infer_expr(c, gamma, lhs)?;
    let rhs_ty = infer_expr(c, gamma, rhs)?;
    let mut lhs_ty = resolve(gamma, lhs_ty, c.body.expr_spans[lhs]);
    let mut rhs_ty = resolve(gamma, rhs_ty, c.body.expr_spans[rhs]);
    if matches!(gamma.get(lhs_ty), TypeExpr::Builtin(BuiltinTy::Uint))
        && matches!(gamma.get(rhs_ty), TypeExpr::Builtin(BuiltinTy::Int))
        && matches!(c.body.expr_arena[rhs], Expr::Int(_))
    {
        check_expr(c, gamma, rhs, lhs_ty, c.body.expr_spans[rhs])?;
        rhs_ty = lhs_ty;
    } else if matches!(gamma.get(rhs_ty), TypeExpr::Builtin(BuiltinTy::Uint))
        && matches!(gamma.get(lhs_ty), TypeExpr::Builtin(BuiltinTy::Int))
        && matches!(c.body.expr_arena[lhs], Expr::Int(_))
    {
        check_expr(c, gamma, lhs, rhs_ty, c.body.expr_spans[lhs])?;
        lhs_ty = rhs_ty;
    }
    Ok((lhs_ty, rhs_ty))
}

pub(crate) fn infer_expr(
    c: BodyCtx,
    gamma: &mut Gamma,
    e: Idx<Expr>,
) -> Result<TyId, CompilerError> {
    infer_expr_used(c, gamma, e, Use::Value)
}

/// A Vec or Map: growth, removal or a rehash can move its elements.
fn type_is_relocatable_collection(gamma: &mut Gamma, mut ty: TyId, span: Span) -> bool {
    loop {
        ty = resolve(gamma, ty, span);
        match *gamma.get(ty) {
            TypeExpr::Nullable(inner) => ty = ty.with_idx(inner),
            TypeExpr::App {
                ctor: TypeCtor::Ref,
                params,
            } => {
                let Some(inner) = params.into_iter().next() else {
                    return false;
                };
                ty = ty.with_idx(inner);
            }
            TypeExpr::App {
                ctor: TypeCtor::Array | TypeCtor::Map,
                ..
            } => return true,
            _ => return false,
        }
    }
}

fn resolved_call_is_frame_source(gamma: &Gamma, e: Idx<Expr>) -> bool {
    let Some(&(mid, fid)) = gamma.out.resolved_overload_calls.get(&e) else {
        return false;
    };
    gamma.workspace.module(mid).functions[fid]
        .annotations
        .iter()
        .any(|annotation| annotation.is_builtin("frame_ref_source"))
}

fn call_ref_provenance(c: BodyCtx, gamma: &mut Gamma, e: Idx<Expr>) -> RefProvenance {
    if resolved_call_is_frame_source(gamma, e) {
        return RefProvenance::Stable;
    }
    let args = gamma.out.ref_call_args.get(&e).cloned().unwrap_or_default();
    if args.is_empty() {
        let Some(&(mid, fid)) = gamma.out.resolved_overload_calls.get(&e) else {
            return RefProvenance::Unknown;
        };
        let has_ref_input = gamma.workspace.module(mid).functions[fid]
            .param_decls
            .iter()
            .any(|param| gamma.workspace.type_contains_ref(None, param.ty));
        return if has_ref_input {
            RefProvenance::Unknown
        } else {
            RefProvenance::Stable
        };
    }
    args.into_iter().fold(RefProvenance::Stable, |origin, arg| {
        origin.merge(frame_ref_provenance(c, gamma, arg))
    })
}

fn frame_ref_provenance(c: BodyCtx, gamma: &mut Gamma, e: Idx<Expr>) -> RefProvenance {
    let span = c.body.expr_spans[e];
    match c.body.expr_arena[e] {
        Expr::Null | Expr::Global(_) => RefProvenance::Stable,
        Expr::Var(binding) => gamma
            .ref_binding_provenance
            .get(&binding)
            .copied()
            .unwrap_or(RefProvenance::Local),
        Expr::FieldAccess { base, .. }
        | Expr::SafeNav { base, .. }
        | Expr::TupleIndex { tuple: base, .. }
        | Expr::As { value: base, .. } => frame_ref_provenance(c, gamma, base),
        Expr::Index { base, .. } => {
            let base_ty = gamma
                .out
                .expr_types
                .get(base)
                .copied()
                .or_else(|| infer_expr(c, gamma, base).ok());
            if base_ty.is_some_and(|ty| type_is_relocatable_collection(gamma, ty, span)) {
                RefProvenance::Relocatable
            } else {
                frame_ref_provenance(c, gamma, base)
            }
        }
        Expr::Call { .. } | Expr::SafeNavCall { .. } => call_ref_provenance(c, gamma, e),
        Expr::BinOp {
            op: hir::BinOp::NullCoalesce,
            lhs,
            ..
        } => frame_ref_provenance(c, gamma, lhs),
        Expr::Block { tail, .. } => tail
            .map(|tail| frame_ref_provenance(c, gamma, tail))
            .unwrap_or(RefProvenance::Unknown),
        Expr::If {
            then_branch,
            else_branch,
            ..
        }
        | Expr::IfLet {
            then_branch,
            else_branch,
            ..
        } => else_branch.map_or(RefProvenance::Unknown, |else_branch| {
            frame_ref_provenance(c, gamma, then_branch).merge(frame_ref_provenance(
                c,
                gamma,
                else_branch,
            ))
        }),
        Expr::Match { arms, .. } => arms.into_iter().fold(RefProvenance::Stable, |origin, arm| {
            origin.merge(frame_ref_provenance(
                c,
                gamma,
                c.body.match_arm_arena[arm].body,
            ))
        }),
        _ => RefProvenance::Unknown,
    }
}

fn reject_unstable_frame_ref(span: Span, provenance: RefProvenance) -> Result<(), CompilerError> {
    match provenance {
        RefProvenance::Relocatable => comp_error!(
            span,
            "cannot retain a reference into relocatable Vec or Map storage; pass the element directly to a `ref T` parameter"
        ),
        RefProvenance::Unknown => comp_error!(
            span,
            "cannot retain a frame reference whose storage origin is unknown"
        ),
        RefProvenance::Stable | RefProvenance::Local => Ok(()),
    }
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

    let provenance = frame_ref_provenance(c, gamma, e);
    if provenance.returnable() {
        return Ok(());
    }

    if provenance == RefProvenance::Relocatable {
        return reject_unstable_frame_ref(span, provenance);
    }

    comp_error!(
        span,
        "cannot return a frame reference to a local place; return an input reference or a reference obtained from owner storage"
    );
}

fn may_be_numeric(gamma: &mut Gamma, ty: TyId, span: Span) -> bool {
    let mut ty = peel_nullable(gamma, ty, span);
    // A newtype cycle is rejected at its own declaration; bound the walk.
    for _ in 0..64 {
        match *gamma.get(ty) {
            TypeExpr::Builtin(
                BuiltinTy::Int
                | BuiltinTy::Uint
                | BuiltinTy::Float
                | BuiltinTy::F32
                | BuiltinTy::Never
                | BuiltinTy::Error,
            )
            | TypeExpr::Generic(..)
            | TypeExpr::Infer(..) => return true,
            TypeExpr::App {
                ctor: TypeCtor::Nominal(name),
                ..
            } if gamma.workspace.newtypes_by_name.contains_key(&name) => {
                let NewtypeId { module, def } = resolve_newtype(gamma, name).unwrap();
                let wrapped = gamma.workspace.module(module).newtypes[def].wrapped;
                ty = peel_nullable(gamma, wrapped, span);
            }
            _ => return false,
        }
    }
    true
}

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
    let mut saved_bindings = vec![];
    // Block-local guards and assignments must not refine paths outside the block.
    let refined_snapshot = gamma.refined_nonnull.clone();

    for statement in *statements {
        if let Err(e) = infer_statement(c, gamma, statement) {
            gamma.body_errors.push(e);
            continue;
        };

        for (root, fields) in stmt_guard_narrows(gamma, c, statement) {
            match root {
                Root::Local(b) if fields.is_empty() => {
                    if let Some(old) = gamma.narrow_local(b, expr_span) {
                        saved_bindings.push((b, old, gamma.refinement_epoch));
                    }
                }
                _ => {
                    gamma.refined_nonnull.insert((root, fields));
                }
            }
        }
    }

    let result = match tail {
        Some(tail) => {
            let ty = match expected {
                Some(exp) => {
                    check_expr(c, gamma, *tail, exp, c.body.expr_spans[*tail]).map(|()| exp)
                }
                None => infer_expr_used(c, gamma, *tail, used),
            };
            ty.unwrap_or_else(|err| {
                gamma.body_errors.push(err);
                gamma.workspace.builtins.error()
            })
        }
        None => {
            if block_ends_in_exit(c, *statements) {
                gamma.workspace.builtins.never()
            } else {
                gamma.workspace.builtins.unit()
            }
        }
    };

    for (b, old, epoch) in saved_bindings.into_iter().rev() {
        gamma.restore_local_narrow(b, old, epoch);
    }

    // Remove block-local facts without resurrecting paths invalidated
    // by an effect inside the block.
    gamma
        .refined_nonnull
        .retain(|p| refined_snapshot.contains(p));

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
    if frame_ref_target(gamma, expected_r).is_some() && !matches!(c.body.expr_arena[e], Expr::Null)
    {
        let actual = infer_expr(c, gamma, e)?;
        let actual_r = resolve(gamma, actual, span);
        if !contains_frame_ref(gamma, actual_r)
            && !matches!(
                c.body.expr_arena[e],
                Expr::Var(_) | Expr::Global(_) | Expr::FieldAccess { .. } | Expr::Index { .. }
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

    // Contextual uint checking needs the original literal for the range check.
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

    if let Expr::Block { statements, tail } = &c.body.expr_arena[e] {
        let ty = infer_block(c, gamma, statements, tail, Use::Value, span, Some(expected));
        if tail.is_none() {
            coerce(gamma, ty, expected, span)?;
        }
        gamma.assign_expr_type(c, e, ty)?;
        return Ok(());
    }

    if let Expr::Lambda { params, body } = c.body.expr_arena[e] {
        let exp_r = resolve(gamma, expected, span);

        let fn_ty = match *gamma.get(exp_r) {
            TypeExpr::Nullable(inner) => exp_r.with_idx(inner),
            _ => exp_r,
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
            coerce_contextual_lambda(gamma, lam_ty, expected, span)?;
            // Use the expected type so indirect calls and returned closures agree on nullable return slots.
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
            if bone_core::modules::is_field_ref_former(former) {
                let writable = !bone_core::modules::is_intrinsic_key(former, "ReadFieldRef");
                let readable = !bone_core::modules::is_intrinsic_key(former, "WriteFieldRef");
                let mut it = params.into_iter();
                let t = exp_r.with_idx(it.next().unwrap());
                let v = exp_r.with_idx(it.next().unwrap());
                let t_r = resolve(gamma, t, span);
                if let TypeExpr::App {
                    ctor: TypeCtor::Nominal(name),
                    ..
                } = *gamma.get(t_r)
                {
                    if let Some(sid) = resolve_struct(gamma, name) {
                        let def = &gamma.workspace.module(sid.module).structs[sid.def];
                        if writable && def.is_value {
                            comp_error!(
                                span,
                                "cannot form a writable field reference `.{field}` into `value` struct `{name}`: a value argument is a copy, so a write through the reference (`field_set`) would be lost; assign the field directly"
                            );
                        }
                        if readable && def.fields.iter().any(|f| f.name == field && f.is_embed) {
                            comp_error!(
                                span,
                                "`{field}` is an `embed` field of `{name}`, so it admits only the write-only `WriteFieldRef<{name}, _>`; a readable selector would hand out the whole value"
                            );
                        }
                    }
                    if writable {
                        field_access(
                            gamma,
                            name,
                            field,
                            gamma.access_site(),
                            FieldAccessNeed::Write,
                        )
                        .map_err(|e| e.to_compiler_error(span))?;
                    }
                }

                let fty = struct_field_ty(gamma, t_r, field, span)?;

                // A writable selector cannot widen its value type to nullable: field_set could store null.
                if writable {
                    let (fty_r, v_r) = (resolve(gamma, fty, span), resolve(gamma, v, span));
                    if matches!(gamma.get(v_r), TypeExpr::Nullable(_))
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
                if div_span != span {
                    err = err.with_label(
                        span,
                        "this `float` is used where `int` is required".to_string(),
                    );
                }
                return Err(err);
            }
        }
        if let Some(origin) = gamma.span_of(expected).or_else(|| gamma.span_of(exp_r)) {
            if origin != span && !origin.is_empty() {
                let exp_str = gamma.render(exp_r);
                return Err(
                    coerce_err.with_label(origin, format!("expected `{exp_str}` because of this"))
                );
            }
        }
        return Err(coerce_err);
    }

    Ok(())
}

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

    // A target hole would let the enclosing expected type bypass cast validation.
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

    if let (TypeExpr::Builtin(from), TypeExpr::Builtin(to)) = (gamma.get(from_r), gamma.get(to_r))
        && from != to
        && matches!(from, BuiltinTy::Int | BuiltinTy::Uint | BuiltinTy::Float)
        && matches!(to, BuiltinTy::Int | BuiltinTy::Uint | BuiltinTy::Float)
    {
        return Ok(());
    }

    let snap = gamma.out.checkpoint();
    let compatible = gamma.trial(|g| coerce(g, from_r, to_r, span).is_ok());
    let erased = gamma.out.any_erasures_since(snap);
    gamma.out.rollback_to(snap);
    if compatible {
        gamma.out.any_erasures.extend(erased);
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

/// Functions and unit values have no text, and dynamic `any` needs an explicit
/// render path; reject them here so no unsupported hole reaches lowering.
fn check_interpolated_value(gamma: &mut Gamma, ty: TyId, span: Span) -> Result<(), CompilerError> {
    let resolved = resolve(gamma, ty, span);
    let mut base = resolved;
    while let TypeExpr::Nullable(inner) = *gamma.get(base) {
        base = base.with_idx(inner);
    }
    if matches!(gamma.get(base), TypeExpr::Builtin(BuiltinTy::Any)) {
        let rendered = gamma.render(resolved);
        return Err(CompilerError::new_with_label(
            span,
            format!(
                "cannot interpolate a value of type `{rendered}` into a string; \
                 convert it explicitly with `inspect(value)`"
            ),
        ));
    }
    let what = match gamma.get(resolved) {
        TypeExpr::Function { .. } => "a function",
        TypeExpr::Builtin(BuiltinTy::Unit) => "a value of type `()`",
        _ => return Ok(()),
    };
    let rendered = gamma.render(resolved);
    Err(CompilerError::new_with_label(
        span,
        format!("cannot interpolate {what} (`{rendered}`) into a string"),
    ))
}

fn check_bitcast(gamma: &mut Gamma, from: TyId, to: TyId, span: Span) -> Result<(), CompilerError> {
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

pub(crate) fn check_value_int_field_literal(
    c: BodyCtx,
    gamma: &mut Gamma,
    value: Idx<Expr>,
    ty: TyId,
) -> Result<(), CompilerError> {
    let span = c.body.expr_spans[value];
    let ty = peel_nullable(gamma, ty, span);
    if matches!(gamma.get(ty), TypeExpr::Builtin(BuiltinTy::Int))
        && let Some(number) = value_int_field_literal(c, value)
        && !(i32::MIN as f64..=i32::MAX as f64).contains(&number)
    {
        comp_error!(
            span,
            "value struct `int` field must be in the range -2147483648..=2147483647, found {}",
            number
        );
    }
    Ok(())
}

fn value_int_field_literal(c: BodyCtx, value: Idx<Expr>) -> Option<f64> {
    match c.body.expr_arena[value] {
        Expr::As {
            value,
            kind: hir::AsKind::Cast | hir::AsKind::Ascribe,
            ..
        } => integer_literal_value(c, value)
            .map(|n| n as f64)
            .or_else(|| float_literal_value(c, value).map(f64::floor)),
        _ => integer_literal_value(c, value).map(|n| n as f64),
    }
}

fn integer_literal_value(c: BodyCtx, e: Idx<Expr>) -> Option<i64> {
    match c.body.expr_arena[e] {
        Expr::Int(value) => Some(value),
        Expr::UnaryMinus(inner, _) => integer_literal_value(c, inner)?.checked_neg(),
        _ => None,
    }
}

fn float_literal_value(c: BodyCtx, e: Idx<Expr>) -> Option<f64> {
    match c.body.expr_arena[e] {
        Expr::Float(value) => Some(value.0),
        Expr::UnaryMinus(inner, _) => Some(-float_literal_value(c, inner)?),
        _ => None,
    }
}

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
        Expr::UnaryMinus(x, _) => find_float_div_span_rec(c, *x, depth + 1),
        Expr::Var(b) => {
            let init = binding_initializer(c, *b)?;
            find_float_div_span_rec(c, init, depth + 1)
        }
        _ => None,
    }
}

pub(crate) fn binding_initializer(c: BodyCtx, b: Idx<BindingInfo>) -> Option<Idx<Expr>> {
    c.body
        .statement_arena
        .iter()
        .find_map(|(_, stmt)| match stmt {
            Statement::Let { pattern, value, .. }
                if c.body.pattern_single_bind(*pattern) == Some(b) =>
            {
                Some(*value)
            }
            Statement::Static { name, value } if *name == b => Some(*value),
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

        _ => comp_error!(span, "Expected a Coroutine type, got {}", gamma.render(r)),
    }
}
