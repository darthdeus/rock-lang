use super::*;

fn reject_ref_captures(
    c: BodyCtx,
    gamma: &Gamma,
    lambda_span: Span,
) -> Result<(), CompilerError> {
    let contains = |outer: Span, inner: Span| {
        outer.file == inner.file
            && outer.offset_range.0 <= inner.offset_range.0
            && inner.offset_range.1 <= outer.offset_range.1
    };

    for (expr, node) in c.body.expr_arena.iter() {
        let Expr::Var(binding) = node else { continue };
        let use_span = c.body.expr_spans[expr];
        if !contains(lambda_span, use_span) || contains(lambda_span, c.body.binding_spans[*binding]) {
            continue;
        }
        let ty = gamma
            .out
            .binding_types
            .get(*binding)
            .copied()
            .or(c.body.binding_arena[*binding].declared_ty);
        if ty.is_some_and(|ty| contains_frame_ref(gamma, ty)) {
            comp_error!(
                use_span,
                "closure cannot capture frame reference `{}`; capture an `Entity<T>` and resolve it inside the closure",
                c.body.binding_arena[*binding].name
            );
        }
    }
    Ok(())
}

pub(crate) fn infer_lambda(
    c: BodyCtx,
    gamma: &mut Gamma,
    params: IdxRange<BindingInfo>,
    body: Idx<Expr>,
    expected_params: Option<&[TyId]>,
    expected_ret: Option<TyId>,
    expr_span: Span,
) -> Result<TyId, CompilerError> {
    let mut param_tys = Vec::with_capacity(params.len());

    for (i, pb) in params.into_iter().enumerate() {
        let binding = &c.body.binding_arena[pb];

        let ty = match binding.declared_ty {
            Some(ty) => ty,

            None => match expected_params.and_then(|ps| ps.get(i)) {
                Some(&et) => et,
                None => gamma.fresh_callsite_var(binding.name, expr_span),
            },
        };

        gamma.assign_named_type(pb, ty)?;
        if contains_frame_ref(gamma, ty) {
            gamma
                .ref_binding_provenance
                .insert(pb, RefProvenance::Stable);
        }

        // Defer-to-use: a param left as an unresolved var (fresh, or the
        // callee's instantiated generic) may be pinned by a later use in this
        // body -- sanction it so ambiguities on it queue instead of erroring.
        let name = binding.name;
        let r = resolve(gamma, ty, expr_span);
        if let TypeExpr::Infer(v, _) = *gamma.get(r) {
            let origin = DeferOrigin { name, span: expr_span };
            gamma.deferrable_vars.entry(v).or_insert(origin);
        }

        param_tys.push(ty)
    }

    // let body_ty = infer_expr(c, gamma, body)?;
    // let body_ty = resolve(gamma, body_ty, expr_span);

    let ret_var = gamma.fresh_callsite_var(*Q_R, expr_span);

    let expects_unit = expected_ret.is_some_and(|r| {
        let r = resolve(gamma, r, expr_span);
        matches!(*gamma.get(r), TypeExpr::Builtin(BuiltinTy::Unit))
    });

    let saved_ret = gamma.declared_ret_ty.replace(ret_var);
    let saved_coro = gamma.coroutine_ret.take();
    let saved_ref_source =
        std::mem::replace(&mut gamma.allow_frame_ref_return_place, false);
    // A lambda body runs at CALL time, after arbitrary intervening effects, and a
    // global is read fresh on each call -- so no enclosing GLOBAL refinement holds
    // inside it. Save + clear the global entries for the body, restore after.
    // Locals are captured by the closure, so they keep their refinement.
    let saved_globals: Vec<Narrow> = gamma
        .refined_nonnull
        .iter()
        .filter(|(root, _)| root.is_global())
        .cloned()
        .collect();
    for g in &saved_globals {
        gamma.refined_nonnull.remove(g);
    }
    let body_res = if expects_unit {
        infer_expr_used(c, gamma, body, Use::Discard)
    } else {
        infer_expr(c, gamma, body)
    };
    gamma.refined_nonnull.extend(saved_globals);
    gamma.coroutine_ret = saved_coro;
    gamma.declared_ret_ty = saved_ret;
    gamma.allow_frame_ref_return_place = saved_ref_source;
    let body_ty = body_res?;

    reject_ref_captures(c, gamma, expr_span)?;

    unify(gamma, ret_var, body_ty, expr_span)?;
    let ret_ty = resolve(gamma, ret_var, expr_span);

    let mut texps = Vec::with_capacity(param_tys.len());

    for pt in param_tys.iter() {
        let r = resolve(gamma, *pt, expr_span);
        let r = TyId::Local(reify(gamma, r, expr_span));
        let texp = *gamma.get(r);
        texps.push(texp);
    }

    let params_range = alloc_contiguous(&mut gamma.out.ty_arena, texps);

    let ret_idx = reify(gamma, ret_ty, expr_span);

    let idx = gamma.out.ty_arena.alloc(TypeExpr::Function {
        params: params_range,
        ret: ret_idx,
    });

    Ok(TyId::Local(idx))
}

/// Build a `fn(P..) -> R` TyId from resolved param/ret types, reifying each into
/// `gamma.out.ty_arena` (the same construction `infer_lambda` uses for a lambda's
/// type). Used to type a nested `fn` name binding.
pub(crate) fn make_fn_ty(gamma: &mut Gamma, param_tys: &[TyId], ret: TyId, span: Span) -> TyId {
    let mut texps = Vec::with_capacity(param_tys.len());
    for pt in param_tys {
        let r = resolve(gamma, *pt, span);
        let r = TyId::Local(reify(gamma, r, span));
        texps.push(*gamma.get(r));
    }
    let params_range = alloc_contiguous(&mut gamma.out.ty_arena, texps);
    let r = resolve(gamma, ret, span);
    let ret_idx = reify(gamma, r, span);
    let idx = gamma
        .out
        .ty_arena
        .alloc(TypeExpr::Function { params: params_range, ret: ret_idx });
    TyId::Local(idx)
}
