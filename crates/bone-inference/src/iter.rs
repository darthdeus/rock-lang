use super::*;

const INT_BUFFERS: &[&str] = &[
    "NativeBufferU8",
    "NativeBufferI8",
    "NativeBufferU16",
    "NativeBufferI16",
    "NativeBufferU32",
    "NativeBufferI32",
];

/// Buffer element type for indexing and `for` heads; typed buffers yield `int`.
pub(crate) fn buffer_elem(gamma: &Gamma, ty: TyId) -> Option<TyId> {
    let TypeExpr::App {
        ctor: TypeCtor::Nominal(name),
        params,
    } = *gamma.get(ty)
    else {
        return None;
    };
    if bone_core::modules::is_intrinsic_key_in(name, &["NativeBuffer", "FlatVec"]) {
        params.into_iter().next().map(|p| ty.with_idx(p))
    } else if bone_core::modules::is_intrinsic_key_in(name, INT_BUFFERS) {
        Some(gamma.workspace.builtins.int())
    } else {
        None
    }
}

pub(crate) fn for_elem_ty(
    gamma: &mut Gamma,
    iter_ty: TyId,
    span: Span,
) -> Result<TyId, CompilerError> {
    if let Some(elem) = buffer_elem(gamma, iter_ty) {
        return Ok(resolve(gamma, elem, span));
    }
    Ok(match *gamma.get(iter_ty) {
        TypeExpr::Builtin(BuiltinTy::Error) => gamma.workspace.builtins.error(),

        TypeExpr::App {
            ctor: TypeCtor::Array,
            params,
        } => {
            let elem = or_fail!(
                params.into_iter().next(),
                span,
                "array type must have one parameter"
            )?;
            resolve(gamma, iter_ty.with_idx(elem), span)
        }

        TypeExpr::App {
            ctor: TypeCtor::Map,
            params,
        } => {
            let mut it = params.into_iter();
            let k = or_fail!(it.next(), span, "map type must have two parameters")?;
            let v = or_fail!(it.next(), span, "map type must have two parameters")?;

            let kr = resolve(gamma, iter_ty.with_idx(k), span);
            let vr = resolve(gamma, iter_ty.with_idx(v), span);

            tuple_of(gamma, &[kr, vr], span)
        }

        TypeExpr::App {
            ctor: TypeCtor::Nominal(name),
            params,
        } if bone_core::modules::is_intrinsic_key(name, "Arena") => {
            // Arena<T> yields (Entity<T>, T)
            let elem = or_fail!(
                params.into_iter().next(),
                span,
                "Arena type must have one parameter"
            )?;

            let elem = resolve(gamma, iter_ty.with_idx(elem), span);
            let entity = gamma.nominal_app(*ENTITY, &[elem], span);

            tuple_of(gamma, &[entity, elem], span)
        }

        _ => comp_error!(span, "type is not iterable: {}", gamma.render(iter_ty)),
    })
}

/// Speculatively find the element type for a `for` head; protocol bindings roll back.
pub fn iterable_elem(gamma: &mut Gamma, ty: TyId, span: Span) -> Option<TyId> {
    gamma.trial(|g| {
        let ty = resolve(g, ty, span);
        if let TypeExpr::Generic(v, _) = *g.get(ty) {
            return g.fn_bound_for(v).map(|(_, elem)| elem);
        }
        if let Ok(elem) = for_elem_ty(g, ty, span) {
            return Some(elem);
        }
        let next_ret = protocol_next_ret(g, ty, false, span)?;
        for_iter_next_elem(g, next_ret, span).ok()
    })
}

pub fn iterator_elem(gamma: &mut Gamma, ty: TyId, span: Span) -> Option<TyId> {
    gamma.trial(|g| {
        let ty = resolve(g, ty, span);
        if let TypeExpr::Generic(v, _) = *g.get(ty) {
            return g.fn_bound_for(v).map(|(_, elem)| elem);
        }
        let next_ret = protocol_next_ret(g, ty, true, span)?;
        bound_next_elem(g, next_ret, ty, span).ok()
    })
}

fn protocol_next_ret(gamma: &mut Gamma, ty: TyId, next_first: bool, span: Span) -> Option<TyId> {
    // An unresolved or failed type would bind to whatever `next` takes.
    if matches!(
        *gamma.get(ty),
        TypeExpr::Infer(..) | TypeExpr::Builtin(BuiltinTy::Error)
    ) {
        return None;
    }
    let c = gamma.body_ctx();
    let next = Ustr::from("next");
    let cp = gamma.out.checkpoint();
    if next_first {
        if let Ok((_, ret, _)) = resolve_unary_by_recv(c, gamma, next, ty, span) {
            return Some(ret);
        }
        gamma.out.rollback_to(cp);
    }
    let it = match resolve_unary_by_recv(c, gamma, Ustr::from("iter"), ty, span) {
        Ok(((m, f), _, _)) if gamma.workspace.module(m).functions[f].has_iterator_bounds() => {
            return None;
        }
        Ok((_, ret, _)) => ret,
        Err(_) => {
            gamma.out.rollback_to(cp);
            ty
        }
    };
    resolve_unary_by_recv(c, gamma, next, it, span)
        .ok()
        .map(|(_, ret, _)| ret)
}

pub(crate) fn builtin_enumerable(gamma: &Gamma, ty: TyId) -> bool {
    match *gamma.get(ty) {
        TypeExpr::App {
            ctor: TypeCtor::Array,
            ..
        }
        | TypeExpr::App {
            ctor: TypeCtor::Map,
            ..
        } => true,
        _ => buffer_elem(gamma, ty).is_some(),
    }
}

pub(crate) fn infer_for_elem(
    c: BodyCtx,
    gamma: &mut Gamma,
    iter: Idx<Expr>,
    span: Span,
) -> Result<TyId, CompilerError> {
    match c.body.expr_arena[iter] {
        Expr::Range { low, high } => infer_range_bounds(c, gamma, low, high),

        // Builtin collections keep template lowering (Map has no iterator
        // protocol); Arena and iterator chains use `EnumIter`.
        Expr::Call {
            callee: Callee::Unresolved { name, .. },
            args,
            ..
        } if name.as_str() == "enumerate" && args.len() == 1 && {
            let coll_ty = infer_expr(c, gamma, args.start())?;
            let coll_ty = resolve(gamma, coll_ty, span);
            builtin_enumerable(gamma, coll_ty)
        } =>
        {
            let coll_ty = infer_expr(c, gamma, args.start())?;
            let coll_ty = resolve(gamma, coll_ty, span);
            let elem = for_elem_ty(gamma, coll_ty, span)?;

            let int_ty = gamma.workspace.builtins.int();

            Ok(tuple_of(gamma, &[int_ty, elem], span))
        }

        _ => {
            let iter_ty = infer_expr(c, gamma, iter)?;
            let iter_ty = resolve(gamma, iter_ty, span);

            // Arrays and arenas use protocol lowering here. Array's direct
            // element rule remains for builtin `enumerate`; Arena uses EnumIter.
            let is_protocol_builtin = match *gamma.get(iter_ty) {
                TypeExpr::App {
                    ctor: TypeCtor::Array,
                    ..
                } => true,
                TypeExpr::App {
                    ctor: TypeCtor::Nominal(n),
                    ..
                } => bone_core::modules::is_intrinsic_key(n, "Arena"),
                _ => false,
            };
            if is_protocol_builtin {
                return resolve_for_iter_protocol(c, gamma, iter, iter_ty, span);
            }

            match for_elem_ty(gamma, iter_ty, span) {
                Ok(elem) => Ok(elem),
                Err(_not_builtin) => resolve_for_iter_protocol(c, gamma, iter, iter_ty, span),
            }
        }
    }
}

/// Try `next(coll)` as identity after a caller bound and `iter(coll)`.
pub(crate) fn resolve_for_iter_protocol(
    c: BodyCtx,
    gamma: &mut Gamma,
    iter_expr: Idx<Expr>,
    coll_ty: TyId,
    span: Span,
) -> Result<TyId, CompilerError> {
    if let TypeExpr::Generic(var_id, gname) = *gamma.get(coll_ty) {
        let Some((idx, elem)) = gamma.fn_bound_for(var_id) else {
            comp_error!(
                span,
                "`{}` is a generic type with no `Iterator` bound, so it cannot be iterated; declare it `{}: Iterator<T>`",
                gname,
                gname
            );
        };
        gamma.out.for_iter_protocol.insert(
            iter_expr,
            ForIterInfo {
                iter: None,
                next: BoundNext::CallerGeneric(idx),
            },
        );
        return Ok(elem);
    }

    let (iter_witness, it_ty) = match resolve_unary_by_recv(
        c,
        gamma,
        Ustr::from("iter"),
        coll_ty,
        span,
    ) {
        Ok((f, ret, subs)) => {
            let (fm, ff) = f;
            if gamma.workspace.module(fm).functions[ff].has_iterator_bounds() {
                comp_error!(
                    span,
                    "`iter({})` resolves to a fn with `Iterator` bounds; bounded `iter` fns are not supported",
                    gamma.render(coll_ty)
                );
            }
            (
                Some(IterWitness {
                    func: f,
                    generics: subs,
                }),
                ret,
            )
        }
        Err(_) => (None, coll_ty),
    };

    let (next_fn, next_ret, next_subs) = match resolve_unary_by_recv(
        c,
        gamma,
        Ustr::from("next"),
        it_ty,
        span,
    ) {
        Ok(x) => x,
        Err(_) if iter_witness.is_some() => comp_error!(
            span,
            "`iter` yields `{}`, but there is no `next({}) -> (bool, E)` function to drive the loop",
            gamma.render(it_ty),
            gamma.render(it_ty)
        ),
        Err(_) => comp_error!(
            span,
            "type `{}` is not iterable: not a builtin collection, and neither `iter({})` nor `next({}) -> (bool, E)` is in scope",
            gamma.render(coll_ty),
            gamma.render(coll_ty),
            gamma.render(coll_ty)
        ),
    };

    let elem = for_iter_next_elem(gamma, next_ret, span)?;
    let next = concrete_next(c, gamma, next_fn, next_subs, 0, span)?;
    gamma.out.for_iter_protocol.insert(
        iter_expr,
        ForIterInfo {
            iter: iter_witness,
            next,
        },
    );
    Ok(elem)
}

/// A resolved `next` witness, with the bounds of a bounded `next` resolved
/// beneath it; lowering walks this tree.
fn concrete_next(
    c: BodyCtx,
    gamma: &mut Gamma,
    func: (ModuleId, Idx<Function>),
    next_generics: FxHashMap<VarId, TyId>,
    depth: usize,
    span: Span,
) -> Result<BoundNext, CompilerError> {
    let next_callee = &gamma.workspace.module(func.0).functions[func.1];
    let mut sub = BoundTree::default();
    if next_callee.has_iterator_bounds() {
        check_iterator_bounds(
            c,
            gamma,
            next_callee,
            &next_generics,
            &mut sub,
            true,
            depth,
            span,
        )?;
    }
    Ok(BoundNext::Concrete {
        func,
        sub,
        next_generics,
    })
}

/// Retry after each argument so later lambdas see inferred element types.
#[allow(clippy::too_many_arguments)]
pub(crate) fn check_iterator_bounds(
    c: BodyCtx,
    gamma: &mut Gamma,
    function: &Function,
    subs: &FxHashMap<VarId, TyId>,
    tree: &mut BoundTree,
    finalize: bool,
    depth: usize,
    span: Span,
) -> Result<(), CompilerError> {
    if depth > 64 {
        comp_error!(
            span,
            "`Iterator` bound resolution exceeded depth 64 (recursive iterator type?)"
        );
    }

    for (idx, b) in function.generic_bounds.iter().enumerate() {
        let Some(elem_decl) = *b else { continue };
        if tree.next.contains_key(&idx) {
            continue;
        }

        let inst = substitute(gamma, function.generics[idx], subs, span)?;
        let inst = resolve(gamma, inst, span);

        match *gamma.get(inst) {
            TypeExpr::Infer(..) => {
                if finalize {
                    comp_error!(
                        span,
                        "cannot resolve the `Iterator` bound on `{}`: the iterator type is not determined by the arguments",
                        function.name
                    );
                }
                // The caller retries after more arguments unify.
            }

            TypeExpr::Generic(var_id, gname) => {
                let Some((caller_idx, caller_elem)) = gamma.fn_bound_for(var_id) else {
                    comp_error!(
                        span,
                        "`{}` does not satisfy `Iterator<{}>`: it is a generic type with no `Iterator` bound",
                        gname,
                        gamma.render(elem_decl)
                    );
                };
                let elem_expected = substitute(gamma, elem_decl, subs, span)?;
                if let Err(e) = unify(gamma, caller_elem, elem_expected, span) {
                    // Quiet overload scoring reports this candidate's rejection.
                    if !gamma.quiet {
                        return Err(e);
                    }
                    comp_error!(
                        span,
                        "`{}` satisfies `Iterator<{}>`, but the bound requires `Iterator<{}>`",
                        gname,
                        gamma.render(caller_elem),
                        gamma.render(elem_expected)
                    );
                }
                tree.next.insert(idx, BoundNext::CallerGeneric(caller_idx));
            }

            _ => {
                let (next_fn, next_ret, next_subs) = match resolve_unary_by_recv(
                    c,
                    gamma,
                    Ustr::from("next"),
                    inst,
                    span,
                ) {
                    Ok(x) => x,
                    Err(_) => comp_error!(
                        span,
                        "`{}` does not satisfy `Iterator<{}>`: no `next({}) -> (bool, E)` function in scope",
                        gamma.render(inst),
                        gamma.render(elem_decl),
                        gamma.render(inst)
                    ),
                };
                let elem_actual = bound_next_elem(gamma, next_ret, inst, span)?;
                let elem_expected = substitute(gamma, elem_decl, subs, span)?;
                if unify(gamma, elem_actual, elem_expected, span).is_err() {
                    comp_error!(
                        span,
                        "`{}` satisfies `Iterator<{}>`, but the bound requires `Iterator<{}>`",
                        gamma.render(inst),
                        gamma.render(elem_actual),
                        gamma.render(elem_expected)
                    );
                }

                let next = concrete_next(c, gamma, next_fn, next_subs, depth + 1, span)?;
                tree.next.insert(idx, next);
            }
        }
    }

    Ok(())
}

/// Bound conformance requires exactly `(bool, E)`: generic combinators cannot
/// destructure an unknown-arity multi-value return, though `for` can.
pub(crate) fn bound_next_elem(
    gamma: &mut Gamma,
    next_ret: TyId,
    inst: TyId,
    span: Span,
) -> Result<TyId, CompilerError> {
    let next_ret = resolve(gamma, next_ret, span);
    let TypeExpr::Tuple { elements } = *gamma.get(next_ret) else {
        comp_error!(
            span,
            "`{}` does not satisfy `Iterator<..>`: its `next` returns `{}`, not `(bool, E)`",
            gamma.render(inst),
            gamma.render(next_ret)
        );
    };
    let elems: Vec<_> = elements.into_iter().collect();
    if elems.len() != 2 {
        comp_error!(
            span,
            "`{}` does not satisfy `Iterator<..>`: its `next` returns `{}`; a bound requires exactly `(bool, E)` -- multi-value iterators can drive a `for` loop but cannot be wrapped by combinators",
            gamma.render(inst),
            gamma.render(next_ret)
        );
    }
    let first = resolve(gamma, next_ret.with_idx(elems[0]), span);
    if !matches!(gamma.get(first), TypeExpr::Builtin(BuiltinTy::Bool)) {
        comp_error!(
            span,
            "`{}` does not satisfy `Iterator<..>`: `next`'s first return must be `bool` (the done flag), got `{}`",
            gamma.render(inst),
            gamma.render(first)
        );
    }
    Ok(resolve(gamma, next_ret.with_idx(elems[1]), span))
}

/// Try an implicit `iter(aty)` coercion when `aty` has no applicable `next`.
/// Successful bindings remain for the caller to commit or roll back.
pub(crate) fn implicit_iter_target(
    c: BodyCtx,
    gamma: &mut Gamma,
    aty: TyId,
    span: Span,
) -> Option<(IterWitness, TyId)> {
    let a = resolve(gamma, aty, span);
    if matches!(*gamma.get(a), TypeExpr::Generic(..) | TypeExpr::Infer(..)) {
        return None;
    }
    let snap = gamma.out.checkpoint();
    let has_next = resolve_unary_by_recv(c, gamma, Ustr::from("next"), a, span).is_ok();
    gamma.out.rollback_to(snap);
    if has_next {
        return None;
    }
    let snap = gamma.out.checkpoint();
    match resolve_unary_by_recv(c, gamma, Ustr::from("iter"), a, span) {
        Ok((f, ret, subs)) => {
            // Keep the for-head restriction: bounded `iter` functions cannot coerce.
            if gamma.workspace.module(f.0).functions[f.1].has_iterator_bounds() {
                gamma.out.rollback_to(snap);
                return None;
            }
            Some((
                IterWitness {
                    func: f,
                    generics: subs,
                },
                ret,
            ))
        }
        Err(_) => {
            gamma.out.rollback_to(snap);
            None
        }
    }
}

pub(crate) fn bounded_generic_vars(gamma: &Gamma, f: &Function) -> SmallVec<[VarId; 2]> {
    f.generics
        .iter()
        .zip(f.generic_bounds.iter())
        .filter(|(_, b)| b.is_some())
        .filter_map(|(g, _)| match *gamma.get(*g) {
            TypeExpr::Generic(v, _) => Some(v),
            _ => None,
        })
        .collect()
}

pub(crate) fn declared_is_bounded_generic(
    gamma: &Gamma,
    declared: TyId,
    bounded: &[VarId],
) -> bool {
    matches!(*gamma.get(declared), TypeExpr::Generic(v, _) if bounded.contains(&v))
}

pub(crate) fn fit_cost_with_iter_coercion(
    c: BodyCtx,
    gamma: &mut Gamma,
    aty: TyId,
    pty: TyId,
    declared_bounded: bool,
    span: Span,
) -> Option<usize> {
    if declared_bounded {
        if let Some((_, it_ty)) = implicit_iter_target(c, gamma, aty, span) {
            return fit_cost(gamma, it_ty, pty, span).map(|k| k + 1);
        }
    }
    fit_cost(gamma, aty, pty, span)
}

/// Resolve a synthetic protocol call using its receiver type, without recording
/// a source call expression.
pub(crate) fn resolve_unary_by_recv(
    c: BodyCtx,
    gamma: &mut Gamma,
    name: Ustr,
    recv_ty: TyId,
    span: Span,
) -> Result<((ModuleId, Idx<Function>), TyId, FxHashMap<VarId, TyId>), CompilerError> {
    let scoped = c
        .module
        .iterator_families
        .as_ref()
        .and_then(|families| match name.as_str() {
            "iter" => Some(&families[0]),
            "next" => Some(&families[1]),
            _ => None,
        });
    let family = match scoped {
        Some(Ok(family)) => *family,
        Some(Err(message)) => return Err(CompilerError::new_with_label(span, message.clone())),
        None => CallFamily::Resolved(name),
    };
    let candidates = resolve_callee(gamma, &family);
    if candidates.visible.is_empty() {
        comp_error!(span, "no `{}` function in scope", name);
    }
    let recv_ty = resolve(gamma, recv_ty, span);

    let (mid, fid) = if candidates.visible.len() == 1 {
        candidates.visible[0]
    } else {
        select_overload(
            c,
            gamma,
            family,
            &candidates,
            Some(recv_ty),
            &[],
            &FxHashMap::default(),
            &[recv_ty],
            IdxRange::new_empty(),
            name,
            span,
        )?
    };

    let function = &gamma.workspace.module(mid).functions[fid];
    let n_params = function.param_decls.len();
    let num_required = function.num_required;
    let param0 = function.param_decls.first().map(|p| p.ty);
    let ret = function.return_ty;

    if n_params != 1 || num_required != 1 {
        comp_error!(
            span,
            "`{}` must take exactly one argument to drive a `for` loop",
            name
        );
    }
    let Some(p0) = param0 else {
        comp_error!(span, "`{}` has no receiver parameter", name);
    };

    let (subs, _) = gamma.instantiate_generics(&function.generics, span)?;
    let p0_sub = substitute(gamma, p0, &subs, span)?;
    coerce(gamma, recv_ty, p0_sub, span)?;

    let ret = ret.unwrap_or_else(|| gamma.workspace.builtins.unit());
    let ret_sub = substitute(gamma, ret, &subs, span)?;
    let ret_res = resolve(gamma, ret_sub, span);
    Ok(((mid, fid), ret_res, subs))
}

pub(crate) fn for_iter_next_elem(
    gamma: &mut Gamma,
    next_ret: TyId,
    span: Span,
) -> Result<TyId, CompilerError> {
    let next_ret = resolve(gamma, next_ret, span);
    let TypeExpr::Tuple { elements } = *gamma.get(next_ret) else {
        comp_error!(
            span,
            "`next` must return `(bool, E)`, got `{}`",
            gamma.render(next_ret)
        );
    };
    let elems: Vec<_> = elements.into_iter().collect();
    if elems.len() < 2 {
        comp_error!(
            span,
            "`next` must return `(bool, E)` (a done-flag plus at least one element), got `{}`",
            gamma.render(next_ret)
        );
    }

    let first = resolve(gamma, next_ret.with_idx(elems[0]), span);
    if !matches!(gamma.get(first), TypeExpr::Builtin(BuiltinTy::Bool)) {
        comp_error!(
            span,
            "`next`'s first return must be `bool` (the done flag), got `{}`",
            gamma.render(first)
        );
    }

    if elems.len() == 2 {
        Ok(resolve(gamma, next_ret.with_idx(elems[1]), span))
    } else {
        let tail: Vec<TyId> = elems[1..]
            .iter()
            .map(|&e| resolve(gamma, next_ret.with_idx(e), span))
            .collect();
        Ok(tuple_of(gamma, &tail, span))
    }
}
