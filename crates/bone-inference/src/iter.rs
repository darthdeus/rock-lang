use super::*;

pub(crate) fn for_elem_ty(gamma: &mut Gamma, iter_ty: TyId, span: Span) -> Result<TyId, CompilerError> {
    Ok(match *gamma.get(iter_ty) {
        // [T]
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
            ctor: TypeCtor::Nominal(name),
            params,
        } if matches!(name.as_str(), "NativeBuffer" | "FlatVec") => {
            let elem = or_fail!(
                params.into_iter().next(),
                span,
                "array type must have one parameter"
            )?;
            resolve(gamma, iter_ty.with_idx(elem), span)
        }

        TypeExpr::App {
            ctor: TypeCtor::Nominal(name),
            ..
        } if matches!(
            name.as_str(),
            "NativeBufferU8"
                | "NativeBufferI8"
                | "NativeBufferU16"
                | "NativeBufferI16"
                | "NativeBufferU32"
                | "NativeBufferI32"
        ) =>
        {
            gamma.workspace.builtins.int()
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
        } if name.as_str() == "Arena" => {
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

/// Whether `enumerate(coll)` in for-position keeps its builtin template
/// lowering (the types `for_elem_ty` knows). Everything else routes through
/// the stdlib `EnumIter` combinator.
pub(crate) fn builtin_enumerable(gamma: &Gamma, ty: TyId) -> bool {
    match *gamma.get(ty) {
        TypeExpr::App { ctor: TypeCtor::Array, .. } | TypeExpr::App { ctor: TypeCtor::Map, .. } => {
            true
        }
        TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } => matches!(
            n.as_str(),
            "Arena"
                | "NativeBuffer"
                | "NativeBufferU8"
                | "NativeBufferI8"
                | "NativeBufferU16"
                | "NativeBufferI16"
                | "NativeBufferU32"
                | "NativeBufferI32"
        ),
        _ => false,
    }
}

pub(crate) fn infer_for_elem(
    c: BodyCtx,
    gamma: &mut Gamma,
    iter: Idx<Expr>,
    span: Span,
) -> Result<TyId, CompilerError> {
    match c.body.expr_arena[iter] {
        Expr::Range { low, high } => {
            infer_range_bounds(c, gamma, low, high)
        }

        // Builtin-collection `enumerate(coll)` keeps its template lowering
        // (Map has no protocol iterator; the counted paths are co-fn safe).
        // `enumerate` over anything else -- an iterator chain -- falls through
        // to the default arm: the call resolves to the stdlib `EnumIter`
        // combinator and the result iterates via the protocol.
        Expr::Call {
            callee: Callee::Unresolved { name, .. },
            args,
            ..
        } if name.as_str() == "enumerate"
            && args.len() == 1
            && {
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

            // `[T]` routes through the stdlib iterator protocol (`VecIter`):
            // the desugar + @inline + SROA + FORL recovery emit the same
            // native numeric `for` the deleted skull template did. `Arena<T>`
            // likewise routes through `ArenaPairIter` (fetch-outside skip
            // shape; element = the `(Entity<T>, T)` tuple).
            // `for_elem_ty` keeps its Array/Arena arms -- `enumerate(..)`
            // above still calls it directly for the element type.
            let is_protocol_builtin = match *gamma.get(iter_ty) {
                TypeExpr::App { ctor: TypeCtor::Array, .. } => true,
                TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } => n.as_str() == "Arena",
                _ => false,
            };
            if is_protocol_builtin {
                return resolve_for_iter_protocol(c, gamma, iter, iter_ty, span);
            }

            match for_elem_ty(gamma, iter_ty, span) {
                Ok(elem) => Ok(elem),
                // Not a builtin collection: try the user iterator protocol
                // `iter(coll) -> It`, `next(It) -> (bool, E)`. The recorded
                // (iter_fn, next_fn) drive skull's for-loop desugar.
                Err(_not_builtin) => resolve_for_iter_protocol(c, gamma, iter, iter_ty, span),
            }
        }
    }
}

/// `for x in coll` where `coll` is not a builtin collection: resolve the user
/// iterator protocol and record it keyed by the For's `iter` expr (for skull's
/// desugar), returning the element type E. E is a single type for
/// `next -> (bool, E)`, or a tuple for `next -> (bool, A, B, ...)` so
/// `for (a, b) in coll` destructures.
///
/// Resolution order:
/// - a bounded generic (`coll: I`, `I: Iterator<E>`) iterates as itself with a
///   placeholder `next` (resolved per splice at concrete roots);
/// - `iter(coll) -> It` when it resolves;
/// - otherwise IDENTITY: a next-able type iterates as itself (bare combinator
///   chains -- `for y in v.iter().map(f)` -- need no blanket `iter<I>`).
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
            ForIterInfo { iter: None, next: BoundNext::CallerGeneric(idx) },
        );
        return Ok(elem);
    }

    let (iter_witness, it_ty) =
        match resolve_unary_by_recv(c, gamma, Ustr::from("iter"), coll_ty, span) {
            Ok((f, ret, subs)) => {
                let (fm, ff) = f;
                if gamma.workspace.module(fm).functions[ff]
                    .generic_bounds
                    .iter()
                    .any(Option::is_some)
                {
                    comp_error!(
                        span,
                        "`iter({})` resolves to a fn with `Iterator` bounds; bounded `iter` fns are not supported",
                        gamma.render(coll_ty)
                    );
                }
                (Some(IterWitness { func: f, generics: subs }), ret)
            }
            // Identity: the collection IS the iterator if `next` resolves on it.
            Err(_) => (None, coll_ty),
        };

    let (next_fn, next_ret, next_subs) =
        match resolve_unary_by_recv(c, gamma, Ustr::from("next"), it_ty, span) {
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

    // A bounded `next` (combinator chain) resolves its inner bounds HERE -- the
    // for-loop is the mono root; the tree drives skull's placeholder
    // substitution when the chain inlines.
    let next_callee = gamma.workspace.module(next_fn.0).functions[next_fn.1].clone();
    let mut sub = BoundTree::default();
    if next_callee.generic_bounds.iter().any(Option::is_some) {
        let mut done = FxHashSet::default();
        check_iterator_bounds(
            c, gamma, &next_callee, &next_subs, &mut sub, &mut done, true, 0, span,
        )?;
    }

    gamma.out.for_iter_protocol.insert(
        iter_expr,
        ForIterInfo {
            iter: iter_witness,
            next: BoundNext::Concrete { func: next_fn, sub, next_generics: next_subs },
        },
    );
    Ok(elem)
}

/// Check the `Iterator` bounds of `function` at an instantiation site and
/// build the resolution tree: for each `I: Iterator<E>` with instantiation
/// `C`, conformance is STRUCTURAL -- `next(C) -> (bool, E')` must resolve and
/// `E'` unifies with `E` (which is HOW `E` gets inferred from `C`). The
/// resolved `next` may itself be bounded (nested chains) -- recurse with its
/// own substitution. An instantiation that is the enclosing fn's bounded
/// generic records a `CallerGeneric` leaf (bound propagation).
///
/// `done` tracks generics already resolved: the call-site loop retries after
/// each argument unification so a later lambda argument sees `E` already
/// bound. `finalize` errors on bounds still unresolvable.
#[allow(clippy::too_many_arguments)]
pub(crate) fn check_iterator_bounds(
    c: BodyCtx,
    gamma: &mut Gamma,
    function: &Function,
    subs: &FxHashMap<VarId, TyId>,
    tree: &mut BoundTree,
    done: &mut FxHashSet<usize>,
    finalize: bool,
    depth: usize,
    span: Span,
) -> Result<(), CompilerError> {
    if depth > 64 {
        comp_error!(span, "`Iterator` bound resolution exceeded depth 64 (recursive iterator type?)");
    }

    for (idx, b) in function.generic_bounds.iter().enumerate() {
        let Some(elem_decl) = *b else { continue };
        if done.contains(&idx) {
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
                // Not yet determined -- the caller retries after more args unify.
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
                unify(gamma, caller_elem, elem_expected, span)?;
                tree.next.insert(idx, BoundNext::CallerGeneric(caller_idx));
                done.insert(idx);
            }

            _ => {
                let (next_fn, next_ret, next_subs) =
                    match resolve_unary_by_recv(c, gamma, Ustr::from("next"), inst, span) {
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

                let next_callee =
                    gamma.workspace.module(next_fn.0).functions[next_fn.1].clone();
                let mut sub = BoundTree::default();
                if next_callee.generic_bounds.iter().any(Option::is_some) {
                    let mut sub_done = FxHashSet::default();
                    check_iterator_bounds(
                        c, gamma, &next_callee, &next_subs, &mut sub, &mut sub_done,
                        true, depth + 1, span,
                    )?;
                }
                tree.next.insert(
                    idx,
                    BoundNext::Concrete { func: next_fn, sub, next_generics: next_subs },
                );
                done.insert(idx);
            }
        }
    }

    Ok(())
}

/// Element type for BOUND conformance: `next` must return exactly `(bool, E)`.
/// A multi-value `next` (`(bool, A, B)`) can be CONSUMED by a `for` loop but
/// not re-wrapped by a combinator: a generic body cannot destructure an
/// unknown-arity splat. No stdlib iterator is multi-value anymore (pairs/
/// enumerate/zip all yield tuple elements); only a user iterator hits this.
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

/// Resolve a unary call `name(recv)` by the receiver's TYPE alone (no arg expr) --
/// the iterator protocol calls `iter(coll)` / `next(it)` on synthesized values.
/// Reuses `select_overload` via the `recv_ty` slot, then instantiates generics
/// and returns the resolved `(callee, return_ty, generic subs)`. Does NOT
/// record into `resolved_overload_calls` (the caller stores it in
/// `for_iter_protocol` / the bound tree).
/// Implicit iterable->iterator coercion target for an argument at an
/// `Iterator`-bounded param: `Some((witness, iter_ret))` iff the arg type is
/// not itself an iterator (no `next(aty)`) but has a plain `iter(aty)`. The
/// same rule the `for` head applies, extended to bounded call positions --
/// `v.any(p)` means `v.iter().any(p)`. A caller bounded generic returns
/// `None` (bound propagation handles it), as does an unresolved infer var.
/// On success the `iter` resolution's unifications persist (the caller
/// either commits them or rolls back a surrounding checkpoint).
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
            // A bounded `iter` fn can't be a coercion target (same restriction
            // as the for head).
            if gamma.workspace.module(f.0).functions[f.1]
                .generic_bounds
                .iter()
                .any(Option::is_some)
            {
                gamma.out.rollback_to(snap);
                return None;
            }
            Some((IterWitness { func: f, generics: subs }, ret))
        }
        Err(_) => {
            gamma.out.rollback_to(snap);
            None
        }
    }
}

/// The `VarId`s of a fn's `Iterator`-bounded generics -- the params declared
/// as exactly one of these are the implicit-iter coercion positions.
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

/// Whether a declared (pre-substitution) param type is one of the fn's
/// bounded generics.
pub(crate) fn declared_is_bounded_generic(gamma: &Gamma, declared: TyId, bounded: &[VarId]) -> bool {
    matches!(*gamma.get(declared), TypeExpr::Generic(v, _) if bounded.contains(&v))
}

/// `fit_cost`, trying the implicit-iter coercion first when the param is a
/// bounded generic: a coerced fit costs +1 so an exact match (a concrete
/// overload, or a real iterator arg) always outranks it.
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

pub(crate) fn resolve_unary_by_recv(
    c: BodyCtx,
    gamma: &mut Gamma,
    name: Ustr,
    recv_ty: TyId,
    span: Span,
) -> Result<((ModuleId, Idx<Function>), TyId, FxHashMap<VarId, TyId>), CompilerError> {
    let candidates = resolve_callee(gamma, name);
    if candidates.is_empty() {
        comp_error!(span, "no `{}` function in scope", name);
    }
    let recv_ty = resolve(gamma, recv_ty, span);

    let (mid, fid) = if candidates.len() == 1 {
        candidates[0]
    } else {
        select_overload(
            c,
            gamma,
            &candidates,
            Some(recv_ty),
            &[],
            &FxHashMap::default(),
            &[recv_ty],
            name,
            span,
        )?
    };

    let function = &gamma.workspace.module(mid).functions[fid];
    let generics = function.generics.clone();
    let n_params = function.param_decls.len();
    let num_required = function.num_required;
    let param0 = function.param_decls.first().map(|p| p.ty);
    let ret = function.return_ty;

    // The protocol is unary: exactly one required receiver parameter.
    if n_params != 1 || num_required != 1 {
        comp_error!(span, "`{}` must take exactly one argument to drive a `for` loop", name);
    }
    let Some(p0) = param0 else {
        comp_error!(span, "`{}` has no receiver parameter", name);
    };

    let (subs, _) = gamma.instantiate_generics(&generics, span)?;
    let p0_sub = substitute(gamma, p0, &subs, span)?;
    coerce(gamma, recv_ty, p0_sub, span)?;

    let ret = ret.unwrap_or_else(|| gamma.workspace.builtins.unit());
    let ret_sub = substitute(gamma, ret, &subs, span)?;
    let ret_res = resolve(gamma, ret_sub, span);
    Ok(((mid, fid), ret_res, subs))
}

/// Element type of a `next() -> (bool, E)` return: the tuple tail. `(bool, E)`
/// yields `E`; `(bool, A, B, ...)` yields the tuple `(A, B, ...)`. The first
/// element must be `bool` (the done flag) and there must be at least one element.
pub(crate) fn for_iter_next_elem(gamma: &mut Gamma, next_ret: TyId, span: Span) -> Result<TyId, CompilerError> {
    let next_ret = resolve(gamma, next_ret, span);
    let TypeExpr::Tuple { elements } = *gamma.get(next_ret) else {
        comp_error!(span, "`next` must return `(bool, E)`, got `{}`", gamma.render(next_ret));
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
