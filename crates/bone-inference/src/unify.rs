use super::*;

pub(crate) fn contains_frame_ref(gamma: &Gamma, ty: TyId) -> bool {
    fn visit(gamma: &Gamma, ty: TyId, seen: &mut FxHashSet<TyId>) -> bool {
        if !seen.insert(ty) {
            return false;
        }
        match *gamma.get(ty) {
            TypeExpr::App {
                ctor: TypeCtor::Ref,
                ..
            } => true,
            TypeExpr::Infer(var, _) => gamma
                .out
                .substitutions
                .get(var)
                .is_some_and(|next| visit(gamma, next, seen)),
            TypeExpr::Nullable(inner) => visit(gamma, ty.with_idx(inner), seen),
            TypeExpr::App { params, .. } => {
                let mut params = params;
                params.any(|p| visit(gamma, ty.with_idx(p), seen))
            }
            TypeExpr::Function { params, ret } => {
                let mut params = params;
                params.any(|p| visit(gamma, ty.with_idx(p), seen))
                    || visit(gamma, ty.with_idx(ret), seen)
            }
            TypeExpr::Tuple { elements } => {
                let mut elements = elements;
                elements.any(|p| visit(gamma, ty.with_idx(p), seen))
            }
            TypeExpr::Builtin(_) | TypeExpr::Generic(..) => false,
        }
    }

    visit(gamma, ty, &mut FxHashSet::default())
}

pub(crate) fn is_identity_struct_type(gamma: &Gamma, ty: TyId) -> bool {
    fn visit(gamma: &Gamma, ty: TyId, seen: &mut FxHashSet<TyId>) -> bool {
        if !seen.insert(ty) {
            return false;
        }
        match *gamma.get(ty) {
            TypeExpr::Infer(var, _) => gamma
                .out
                .substitutions
                .get(var)
                .is_some_and(|next| visit(gamma, next, seen)),
            TypeExpr::Nullable(inner) => visit(gamma, ty.with_idx(inner), seen),
            TypeExpr::App {
                ctor: TypeCtor::Nominal(name),
                ..
            } => resolve_struct(gamma, name)
                .is_some_and(|sid| !gamma.workspace.module(sid.module).structs[sid.def].is_value),
            _ => false,
        }
    }

    visit(gamma, ty, &mut FxHashSet::default())
}

pub(crate) fn frame_ref_target(gamma: &Gamma, ty: TyId) -> Option<TyId> {
    match *gamma.get(ty) {
        TypeExpr::App {
            ctor: TypeCtor::Ref,
            params,
        } => params.into_iter().next().map(|inner| ty.with_idx(inner)),
        TypeExpr::Nullable(inner) => {
            let inner_ty = ty.with_idx(inner);
            match *gamma.get(inner_ty) {
                TypeExpr::App {
                    ctor: TypeCtor::Ref,
                    params,
                } => params
                    .into_iter()
                    .next()
                    .map(|target| inner_ty.with_idx(target)),
                _ => None,
            }
        }
        _ => None,
    }
}

pub(crate) fn persistent_type_contains_frame_ref(gamma: &Gamma, ty: TyId) -> bool {
    fn visit(gamma: &Gamma, ty: TyId, seen: &mut FxHashSet<TyId>) -> bool {
        if !seen.insert(ty) {
            return false;
        }
        match *gamma.get(ty) {
            TypeExpr::Infer(var, _) => gamma
                .out
                .substitutions
                .get(var)
                .is_some_and(|next| visit(gamma, next, seen)),
            TypeExpr::Nullable(inner) => visit(gamma, ty.with_idx(inner), seen),
            TypeExpr::Tuple { elements } => elements
                .into_iter()
                .any(|part| visit(gamma, ty.with_idx(part), seen)),
            TypeExpr::App {
                ctor: TypeCtor::Ref,
                ..
            } => false,
            TypeExpr::App { params, .. } => params
                .into_iter()
                .any(|part| contains_frame_ref(gamma, ty.with_idx(part))),
            TypeExpr::Function { params, ret } => {
                contains_frame_ref(gamma, ty.with_idx(ret))
                    || params
                        .into_iter()
                        .any(|part| contains_frame_ref(gamma, ty.with_idx(part)))
            }
            TypeExpr::Builtin(_) | TypeExpr::Generic(..) => false,
        }
    }

    visit(gamma, ty, &mut FxHashSet::default())
}

pub(crate) fn substitute(
    gamma: &mut Gamma,
    ty: TyId,
    subs: &FxHashMap<VarId, TyId>,
    span: Span,
) -> Result<TyId, CompilerError> {
    let texp = gamma.get(ty);

    match *texp {
        TypeExpr::Builtin(_) => Ok(ty),
        TypeExpr::Generic(var_id, ustr) => {
            if let Some(sub) = subs.get(&var_id) {
                Ok(*sub)
            } else {
                comp_error!(span, "No sub for generic {ustr} in {:?}", texp);
            }
        }

        TypeExpr::Nullable(child) => {
            let child_ty = ty.with_idx(child);
            let sub_child = substitute(gamma, child_ty, subs, span)?;

            if sub_child == child_ty {
                Ok(ty)
            } else {
                Ok(nullable_of(gamma, sub_child, span))
            }
        }

        TypeExpr::App { ctor, params } => {
            if let Some(new_elements) = map_substitute(gamma, subs, span, ty, params)? {
                let idx = gamma.out.ty_arena.alloc(TypeExpr::App {
                    ctor,
                    params: new_elements,
                });
                gamma.out.ty_spans.insert(idx, span);

                Ok(TyId::Local(idx))
            } else {
                Ok(ty)
            }
        }

        TypeExpr::Tuple { elements } => {
            if let Some(new_elements) = map_substitute(gamma, subs, span, ty, elements)? {
                let idx = gamma.out.ty_arena.alloc(TypeExpr::Tuple {
                    elements: new_elements,
                });
                gamma.out.ty_spans.insert(idx, span);

                Ok(TyId::Local(idx))
            } else {
                Ok(ty)
            }

            // let mut subbed = Vec::with_capacity(elements.len());
            // let mut changed = false;
            //
            // for child in elements {
            //     let child_ty = ty.with_idx(child);
            //     let s = substitute(gamma, child_ty, subs, span)?;
            //     changed |= s != child_ty;
            //
            //     subbed.push(s);
            // }
            //
            // if !changed {
            //     return Ok(ty);
            // }
            //
            // let nodes = subbed
            //     .into_iter()
            //     .map(|x| gamma.get(x))
            //     .copied()
            //     .collect_vec();
            //
            // let new_elements = alloc_contiguous(&mut gamma.out.ty_arena, nodes);
        }

        TypeExpr::Function { params, ret } => {
            let new_params = map_substitute(gamma, subs, span, ty, params)?;
            let ret_ty = ty.with_idx(ret);
            let sub_ret = substitute(gamma, ret_ty, subs, span)?;

            if new_params.is_none() && sub_ret == ret_ty {
                Ok(ty)
            } else {
                let params = match new_params {
                    Some(p) => p,
                    None => reify_range(gamma, ty, params, span),
                };

                // let params = new_params.unwrap_or(params);
                let ret_idx = reify(gamma, sub_ret, span);

                let idx = gamma.out.ty_arena.alloc(TypeExpr::Function {
                    params,
                    ret: ret_idx,
                });
                gamma.out.ty_spans.insert(idx, span);

                Ok(TyId::Local(idx))
            }
        }

        TypeExpr::Infer(..) => {
            comp_error!(span, "Unsupported type expr {:?}", texp)
        }
    }
}

pub fn unify(gamma: &mut Gamma, a: TyId, b: TyId, span: Span) -> Result<(), CompilerError> {
    let a = resolve(gamma, a, span);
    let b = resolve(gamma, b, span);

    match (gamma.get(a), gamma.get(b)) {
        (TypeExpr::Builtin(x), TypeExpr::Builtin(y)) if x == y => {}

        (TypeExpr::Builtin(BuiltinTy::Never), _) => {}
        (_, TypeExpr::Builtin(BuiltinTy::Never)) => {}

        (TypeExpr::Builtin(BuiltinTy::Error), _) => {}
        (_, TypeExpr::Builtin(BuiltinTy::Error)) => {}

        (TypeExpr::Generic(x, _), TypeExpr::Generic(y, _)) if x == y => {}
        // (TypeExpr::Named(x), TypeExpr::Named(y)) if x == y => {}
        (TypeExpr::Infer(va, _), TypeExpr::Infer(vb, _)) if va == vb => {}

        (TypeExpr::Infer(va, _), _) => {
            let va = *va;
            bind_var_checked(gamma, va, b, span)?;
        }

        (_, TypeExpr::Infer(vb, _)) => {
            let vb = *vb;
            bind_var_checked(gamma, vb, a, span)?;
        }

        (
            TypeExpr::App {
                ctor: ctor_a,
                params: params_a,
            },
            TypeExpr::App {
                ctor: ctor_b,
                params: params_b,
            },
        ) => {
            if ctor_a != ctor_b {
                // Both sides are plain nominals (structs/enums with no type
                // args) far more often than generics; "different generic
                // constructors" is nonsense for those. Only mention generics
                // when a side actually has type parameters.
                let detail = if params_a.is_empty() && params_b.is_empty() {
                    ""
                } else {
                    " (different type constructors)"
                };
                unify_fail!(
                    gamma,
                    span,
                    "type mismatch: expected `{}`, found `{}`{detail}",
                    gamma.workspace.render(Some(&gamma.out), b),
                    gamma.workspace.render(Some(&gamma.out), a)
                )
            }

            if params_a.len() != params_b.len() {
                unify_fail!(
                    gamma,
                    span,
                    "Failed to unify {} and {}, generics of different length",
                    gamma.workspace.render(Some(&gamma.out), a),
                    gamma.workspace.render(Some(&gamma.out), b)
                )
            }

            for (ea, eb) in (*params_a).zip(*params_b) {
                unify(gamma, a.with_idx(ea), b.with_idx(eb), span)?;
            }
        }

        (
            TypeExpr::Tuple {
                elements: elements_a,
            },
            TypeExpr::Tuple {
                elements: elements_b,
            },
        ) => {
            if elements_a.len() != elements_b.len() {
                unify_fail!(
                    gamma,
                    span,
                    "Failed to unify {} and {}, tuples of different length",
                    gamma.workspace.render(Some(&gamma.out), a),
                    gamma.workspace.render(Some(&gamma.out), b)
                )
            }

            for (ea, eb) in (*elements_a).zip(*elements_b) {
                unify(gamma, a.with_idx(ea), b.with_idx(eb), span)?;
            }
        }

        (TypeExpr::Nullable(ia), TypeExpr::Nullable(ib)) => {
            let (ia, ib) = (*ia, *ib);

            unify(gamma, a.with_idx(ia), b.with_idx(ib), span)?;
        }

        (_, TypeExpr::Nullable(ib)) => {
            let ib = *ib;

            unify(gamma, a, b.with_idx(ib), span)?;
        }

        (
            TypeExpr::Function {
                params: pa,
                ret: ra,
            },
            TypeExpr::Function {
                params: pb,
                ret: rb,
            },
        ) => {
            if pa.len() != pb.len() {
                unify_fail!(
                    gamma,
                    span,
                    "Failed to unify {} and {}, functions with different arity",
                    gamma.workspace.render(Some(&gamma.out), a),
                    gamma.workspace.render(Some(&gamma.out), b)
                )
            }

            let (ra, rb) = (*ra, *rb);

            for (ea, eb) in (*pa).zip(*pb) {
                unify(gamma, a.with_idx(ea), b.with_idx(eb), span)?;
            }

            unify(gamma, a.with_idx(ra), b.with_idx(rb), span)?;
        }

        // `unify(actual, expected)` by convention (coerce passes
        // `from`/`to` in that order), so `b` is the expected type and `a` the
        // found one -- matching the nominal-mismatch arm above.
        _ => {
            unify_fail!(
                gamma,
                span,
                "type mismatch: expected `{}`, found `{}`",
                gamma.workspace.render(Some(&gamma.out), b),
                gamma.workspace.render(Some(&gamma.out), a)
            )
        }
    }

    Ok(())
}

pub(crate) fn resolve(gamma: &mut Gamma, ty: TyId, span: Span) -> TyId {
    match *gamma.get(ty) {
        TypeExpr::Infer(var_id, _) => match gamma.out.substitutions.get(var_id) {
            Some(next) => {
                let root = resolve(gamma, next, span);

                if root != next {
                    gamma.out.bind_var(var_id, root);
                }

                root
            }
            None => ty,
        },

        TypeExpr::Nullable(inner) => {
            let inner_ty = ty.with_idx(inner);
            let resolved = resolve(gamma, inner_ty, span);

            if resolved == inner_ty {
                ty
            } else {
                nullable_of(gamma, resolved, span)
            }
        }

        TypeExpr::App { ctor, params } => {
            if let Some(new_params) = map_resolve(gamma, span, ty, params) {
                let idx = gamma.out.ty_arena.alloc(TypeExpr::App {
                    ctor,
                    params: new_params,
                });

                gamma.out.ty_spans.insert(idx, span);

                TyId::Local(idx)
            } else {
                ty
            }
        }

        TypeExpr::Tuple { elements } => {
            if let Some(new_elements) = map_resolve(gamma, span, ty, elements) {
                let idx = gamma.out.ty_arena.alloc(TypeExpr::Tuple {
                    elements: new_elements,
                });

                gamma.out.ty_spans.insert(idx, span);

                TyId::Local(idx)
            } else {
                ty
            }
        }

        TypeExpr::Function { params, ret } => {
            let new_params = map_resolve(gamma, span, ty, params);
            let ret_ty = ty.with_idx(ret);
            let resolved_ret = resolve(gamma, ret_ty, span);

            if new_params.is_none() && resolved_ret == ret_ty {
                ty
            } else {
                let params = new_params.unwrap_or(params);
                let ret_idx = reify(gamma, resolved_ret, span);

                let idx = gamma.out.ty_arena.alloc(TypeExpr::Function {
                    params,
                    ret: ret_idx,
                });

                gamma.out.ty_spans.insert(idx, span);

                TyId::Local(idx)
            }
        }

        _ => ty,
    }
}

pub(crate) fn map_resolve(
    gamma: &mut Gamma,
    span: Span,
    root_ty: TyId,
    elements: IdxRange<TypeExpr>,
) -> Option<IdxRange<TypeExpr>> {
    // This case is actually infallible
    map_children(gamma, root_ty, elements, span, |g, c| {
        Ok(resolve(g, c, span))
    })
    .unwrap()
}

pub(crate) fn map_substitute(
    gamma: &mut Gamma,
    subs: &FxHashMap<VarId, TyId>,
    span: Span,
    root_ty: TyId,
    elements: IdxRange<TypeExpr>,
) -> Result<Option<IdxRange<TypeExpr>>, CompilerError> {
    map_children(gamma, root_ty, elements, span, |g, c| {
        substitute(g, c, subs, span)
    })
}

pub(crate) fn map_children(
    gamma: &mut Gamma,
    root_ty: TyId,
    elements: IdxRange<TypeExpr>,
    span: Span,
    mut f: impl FnMut(&mut Gamma, TyId) -> Result<TyId, CompilerError>,
) -> Result<Option<IdxRange<TypeExpr>>, CompilerError> {
    let mut out: SmallVec<[TyId; 4]> = SmallVec::with_capacity(elements.len());
    let mut changed = false;

    for child in elements {
        let child_ty = root_ty.with_idx(child);
        let s = f(gamma, child_ty)?;

        changed |= s != child_ty;
        out.push(s);
    }

    if !changed {
        return Ok(None);
    }

    let mut nodes: SmallVec<[TypeExpr; 4]> = SmallVec::with_capacity(out.len());

    for x in out {
        let tyid = TyId::Local(reify(gamma, x, span));
        let t = *gamma.get(tyid);
        nodes.push(t);
    }

    Ok(Some(alloc_contiguous(&mut gamma.out.ty_arena, nodes)))
}

// Allocates a fresh local nullable wrapper
pub(crate) fn nullable_of(gamma: &mut Gamma, inner: TyId, span: Span) -> TyId {
    // collapse T?? -> T?
    if matches!(gamma.get(inner), TypeExpr::Nullable(_)) {
        return inner;
    }

    let inner_idx = reify(gamma, inner, span);
    let idx = gamma.out.ty_arena.alloc(TypeExpr::Nullable(inner_idx));
    gamma.out.ty_spans.insert(idx, span);

    TyId::Local(idx)
}

// If `ty` resolves to Nullable, return the resolved inner
pub(crate) fn peel_nullable(gamma: &mut Gamma, ty: TyId, span: Span) -> TyId {
    let ty = resolve(gamma, ty, span);

    match *gamma.get(ty) {
        TypeExpr::Nullable(inner) => resolve(gamma, ty.with_idx(inner), span),
        _ => ty,
    }
}

// If `cond` is a null check on a variable, return the binding and whether we narrow on
// `then` or `else` (x != null -> then, x == null -> else)
// fn detect_null_narrow(module: &Module, cond: Idx<Expr>) -> Option<(Idx<BindingInfo>, bool)> {
//     let Expr::BinOp { op, lhs, rhs } = c.body.expr_arena[cond] else {
//         return None;
//     };
//
//     let narrow_in_then = match op {
//         hir::BinOp::NotEq => true,
//         hir::BinOp::Eq => false,
//         _ => return None,
//     };
//
//     let binding = match (c.body.expr_arena[lhs], c.body.expr_arena[rhs]) {
//         (Expr::Var(b), Expr::Null) => b,
//         (Expr::Null, Expr::Var(b)) => b,
//         _ => return None,
//     };
//
//     Some((binding, narrow_in_then))
// }

pub(crate) fn types_equal(gamma: &Gamma, a: TyId, b: TyId) -> bool {
    if a == b {
        return true;
    }

    match (gamma.get(a), gamma.get(b)) {
        (TypeExpr::Builtin(x), TypeExpr::Builtin(y)) => x == y,
        (TypeExpr::Generic(x, _), TypeExpr::Generic(y, _)) => x == y,
        (TypeExpr::Infer(x, _), TypeExpr::Infer(y, _)) => x == y,
        // (TypeExpr::Named(x), TypeExpr::Named(y)) => x == y,
        (TypeExpr::Nullable(ia), TypeExpr::Nullable(ib)) => {
            types_equal(gamma, a.with_idx(*ia), b.with_idx(*ib))
        }

        (
            TypeExpr::App {
                ctor: ca,
                params: pa,
            },
            TypeExpr::App {
                ctor: cb,
                params: pb,
            },
        ) => {
            ca == cb
                && pa.len() == pb.len()
                && (*pa)
                    .zip(*pb)
                    .all(|(ea, eb)| types_equal(gamma, a.with_idx(ea), b.with_idx(eb)))
        }

        (TypeExpr::Tuple { elements: ea }, TypeExpr::Tuple { elements: eb }) => {
            ea.len() == eb.len()
                && (*ea)
                    .zip(*eb)
                    .all(|(ta, tb)| types_equal(gamma, a.with_idx(ta), b.with_idx(tb)))
        }

        (
            TypeExpr::Function {
                params: pa,
                ret: ra,
            },
            TypeExpr::Function {
                params: pb,
                ret: rb,
            },
        ) => {
            pa.len() == pb.len()
                && types_equal(gamma, a.with_idx(*ra), b.with_idx(*rb))
                && (*pa)
                    .zip(*pb)
                    .all(|(ea, eb)| types_equal(gamma, a.with_idx(ea), b.with_idx(eb)))
        }

        _ => false,
    }
}

/// Whether inference variable `var` appears anywhere in `ty`'s tree, following
/// bound variables. Binding `var` to such a type would make it infinite
/// (`a := a?`), and every later walk of it would recurse forever.
fn occurs_in(gamma: &Gamma, var: VarId, ty: TyId) -> bool {
    match *gamma.get(ty) {
        TypeExpr::Infer(v, _) => {
            v == var
                || gamma
                    .out
                    .substitutions
                    .get(v)
                    .is_some_and(|next| occurs_in(gamma, var, next))
        }
        TypeExpr::Builtin(_) | TypeExpr::Generic(..) => false,
        TypeExpr::Nullable(inner) => occurs_in(gamma, var, ty.with_idx(inner)),
        TypeExpr::App { params, .. } => params
            .into_iter()
            .any(|p| occurs_in(gamma, var, ty.with_idx(p))),
        TypeExpr::Tuple { elements } => elements
            .into_iter()
            .any(|e| occurs_in(gamma, var, ty.with_idx(e))),
        TypeExpr::Function { params, ret } => {
            occurs_in(gamma, var, ty.with_idx(ret))
                || params
                    .into_iter()
                    .any(|p| occurs_in(gamma, var, ty.with_idx(p)))
        }
    }
}

/// Bind `var := ty`, rejecting a binding that would make the type infinite.
fn bind_var_checked(
    gamma: &mut Gamma,
    var: VarId,
    ty: TyId,
    span: Span,
) -> Result<(), CompilerError> {
    if occurs_in(gamma, var, ty) {
        unify_fail!(
            gamma,
            span,
            "cannot infer a type: it would contain itself (`{}`)",
            gamma.workspace.render(Some(&gamma.out), ty)
        );
    }
    gamma.out.bind_var(var, ty);
    Ok(())
}

/// Whether `ty` still holds an unbound inference variable anywhere in its tree.
/// Pure: reads the substitution store without writing to it, so a caller can ask
/// whether a type stands on its own before deciding to admit it.
pub(crate) fn has_unbound_infer(gamma: &Gamma, ty: TyId) -> bool {
    match *gamma.get(ty) {
        TypeExpr::Infer(v, _) => match gamma.out.substitutions.get(v) {
            Some(next) => has_unbound_infer(gamma, next),
            None => true,
        },

        TypeExpr::Builtin(_) | TypeExpr::Generic(..) => false,

        TypeExpr::Nullable(inner) => has_unbound_infer(gamma, ty.with_idx(inner)),

        TypeExpr::App { params, .. } => params
            .into_iter()
            .any(|p| has_unbound_infer(gamma, ty.with_idx(p))),

        TypeExpr::Tuple { elements } => elements
            .into_iter()
            .any(|e| has_unbound_infer(gamma, ty.with_idx(e))),

        TypeExpr::Function { params, ret } => {
            has_unbound_infer(gamma, ty.with_idx(ret))
                || params
                    .into_iter()
                    .any(|p| has_unbound_infer(gamma, ty.with_idx(p)))
        }
    }
}

pub fn coerce(gamma: &mut Gamma, from: TyId, to: TyId, span: Span) -> Result<(), CompilerError> {
    coerce_impl(gamma, from, to, span, false)
}

pub(crate) fn coerce_contextual_lambda(
    gamma: &mut Gamma,
    from: TyId,
    to: TyId,
    span: Span,
) -> Result<(), CompilerError> {
    coerce_impl(gamma, from, to, span, true)
}

fn coerce_impl(
    gamma: &mut Gamma,
    from: TyId,
    to: TyId,
    span: Span,
    contextual_lambda: bool,
) -> Result<(), CompilerError> {
    let from_r = resolve(gamma, from, span);
    let to_r = resolve(gamma, to, span);

    // A stored function value keeps its declared return ABI. Lifting a `nobox`
    // return into a nullable one adds a presence slot at indirect call sites.
    // Lambda literals checked against the target type get that ABI directly.
    let function_return = |ty: TyId| {
        let ty = match *gamma.get(ty) {
            TypeExpr::Nullable(inner) => ty.with_idx(inner),
            _ => ty,
        };
        match *gamma.get(ty) {
            TypeExpr::Function { ret, .. } => Some(ty.with_idx(ret)),
            _ => None,
        }
    };
    if !contextual_lambda
        && let (Some(from_ret), Some(to_ret)) = (function_return(from_r), function_return(to_r))
    {
        let from_ret = resolve(gamma, from_ret, span);
        let to_ret = resolve(gamma, to_ret, span);
        if let TypeExpr::Nullable(inner) = *gamma.get(to_ret) {
            let to_inner = resolve(gamma, to_ret.with_idx(inner), span);
            if !matches!(gamma.get(from_ret), TypeExpr::Nullable(_))
                && let TypeExpr::App {
                    ctor: TypeCtor::Nominal(name),
                    ..
                } = *gamma.get(to_inner)
                && resolve_struct(gamma, name)
                    .is_some_and(|sid| gamma.workspace.module(sid.module).structs[sid.def].is_nobox)
                && gamma.trial(|g| unify(g, from_ret, to_inner, span).is_ok())
            {
                return Err(CompilerError::new_with_label(
                    span,
                    "a function returning a `nobox` value cannot be used as one returning a nullable value; wrap the call in a lambda with the target return type".to_string(),
                ));
            }
        }
    }

    if matches!(gamma.get(from_r), TypeExpr::Builtin(BuiltinTy::Never)) {
        return Ok(());
    }

    if matches!(gamma.get(from_r), TypeExpr::Builtin(BuiltinTy::Error))
        || matches!(gamma.get(to_r), TypeExpr::Builtin(BuiltinTy::Error))
    {
        return Ok(());
    }

    if let TypeExpr::Infer(_, _) = *gamma.get(from_r) {
        return unify(gamma, from_r, to_r, span);
    }

    // `null` (a `Nullable` whose inner is still an unconstrained `Infer`) may
    // not be assigned to bare `any` -- `any` excludes null; use `any?`. A
    // nullable VALUE with a concrete inner (e.g. `int?`) IS allowed (rock
    // parity: only the null literal is rejected here).
    if matches!(gamma.get(to_r), TypeExpr::Builtin(BuiltinTy::Any)) {
        if contains_frame_ref(gamma, from_r) {
            return Err(CompilerError::new_with_label(
                span,
                "a frame reference cannot be erased to `any`".to_string(),
            ));
        }
        if let TypeExpr::Nullable(inner) = *gamma.get(from_r) {
            let inner_r = resolve(gamma, from_r.with_idx(inner), span);
            if matches!(gamma.get(inner_r), TypeExpr::Infer(..)) {
                return Err(CompilerError::new_with_label(
                    span,
                    "`null` cannot be assigned to bare `any` (which excludes null) -- use `any?`"
                        .to_string(),
                ));
            }
        }
    }

    let from_ty = gamma.get(from_r);
    let to_ty = gamma.get(to_r);

    if let Some(target) = frame_ref_target(gamma, to_r) {
        if !contains_frame_ref(gamma, from_r) && !matches!(from_ty, TypeExpr::Nullable(_)) {
            return unify(gamma, from_r, target, span);
        }
    }

    if is_identity_struct_type(gamma, to_r)
        && let Some(target) = frame_ref_target(gamma, from_r)
    {
        return unify(gamma, target, to_r, span);
    }

    if matches!(to_ty, TypeExpr::Builtin(BuiltinTy::Any)) {
        gamma.out.any_erasures.push((span, from_r));
        return Ok(());
    }

    if matches!(from_ty, TypeExpr::Builtin(BuiltinTy::Int | BuiltinTy::Uint))
        && matches!(to_ty, TypeExpr::Builtin(BuiltinTy::Float))
    {
        return Ok(());
    }

    if matches!(from_ty, TypeExpr::Builtin(BuiltinTy::Float))
        && matches!(to_ty, TypeExpr::Builtin(BuiltinTy::Int))
    {
        if let Some(v) = bound_var(gamma, to) {
            gamma.out.bind_var(v, from_r);
            return Ok(());
        }
    }

    if let TypeExpr::App {
        ctor: TypeCtor::Nominal(iname),
        ..
    } = *gamma.get(to_r)
    {
        if gamma.workspace.interface_names.contains(&iname) {
            return coerce_to_interface(gamma, from_r, iname, span);
        }
    }

    // `FieldRef<T, V>` carries both capabilities, so it is the TOP of the lattice
    // and coerces DOWN to either single-capability former: to `ReadFieldRef<T, V>`
    // (invariant in T, covariant in V) and to `WriteFieldRef<T, V>` (invariant in
    // both -- write-only contravariance in V would be sound but has no consumer).
    // Nothing coerces the other way, and the two single-capability formers are
    // INCOMPARABLE: `WriteFieldRef -> ReadFieldRef` is the whole-value extraction
    // an embedded field must not offer.
    if let (
        TypeExpr::App {
            ctor: TypeCtor::Nominal(fname),
            params: fp,
        },
        TypeExpr::App {
            ctor: TypeCtor::Nominal(tname),
            params: tp,
        },
    ) = (*gamma.get(from_r), *gamma.get(to_r))
    {
        if bone_core::modules::is_intrinsic_key(fname, "FieldRef")
            && bone_core::modules::is_intrinsic_key_in(tname, &["ReadFieldRef", "WriteFieldRef"])
        {
            let mut fi = fp.into_iter();
            let mut ti = tp.into_iter();
            let (ft, fv) = (
                from_r.with_idx(fi.next().unwrap()),
                from_r.with_idx(fi.next().unwrap()),
            );
            let (tt, tv) = (
                to_r.with_idx(ti.next().unwrap()),
                to_r.with_idx(ti.next().unwrap()),
            );
            unify(gamma, ft, tt, span)?;
            return match bone_core::modules::is_intrinsic_key(tname, "WriteFieldRef") {
                true => unify(gamma, fv, tv, span),
                _ => coerce(gamma, fv, tv, span),
            };
        }
    }

    let snap = gamma.out.checkpoint();
    if gamma.trial(|g| unify(g, from_r, to_r, span).is_ok()) {
        return Ok(());
    }

    gamma.out.rollback_to(snap);

    if let TypeExpr::Nullable(ti) = *gamma.get(to_r) {
        let to_inner = to_r.with_idx(ti);
        let from_inner = match *gamma.get(from_r) {
            TypeExpr::Nullable(fi) => from_r.with_idx(fi),
            _ => from_r,
        };

        return coerce_impl(gamma, from_inner, to_inner, span, contextual_lambda);
    }

    unify(gamma, from_r, to_r, span)
}

pub fn join(gamma: &mut Gamma, a: TyId, b: TyId, span: Span) -> Result<TyId, CompilerError> {
    let ar = resolve(gamma, a, span);
    let br = resolve(gamma, b, span);

    if matches!(
        gamma.get(ar),
        TypeExpr::Builtin(BuiltinTy::Never) | TypeExpr::Builtin(BuiltinTy::Error)
    ) {
        return Ok(br);
    }

    if matches!(
        gamma.get(br),
        TypeExpr::Builtin(BuiltinTy::Never) | TypeExpr::Builtin(BuiltinTy::Error)
    ) {
        return Ok(ar);
    }

    let a_inner = match *gamma.get(ar) {
        TypeExpr::Nullable(i) => Some(ar.with_idx(i)),
        _ => None,
    };

    let b_inner = match *gamma.get(br) {
        TypeExpr::Nullable(i) => Some(br.with_idx(i)),
        _ => None,
    };

    if a_inner.is_some() || b_inner.is_some() {
        let inner = join(gamma, a_inner.unwrap_or(ar), b_inner.unwrap_or(br), span)?;
        return Ok(nullable_of(gamma, inner, span));
    }

    match (gamma.get(ar), gamma.get(br)) {
        (
            TypeExpr::Builtin(BuiltinTy::Int | BuiltinTy::Uint),
            TypeExpr::Builtin(BuiltinTy::Float),
        )
        | (
            TypeExpr::Builtin(BuiltinTy::Float),
            TypeExpr::Builtin(BuiltinTy::Int | BuiltinTy::Uint),
        ) => Ok(gamma.workspace.builtins.float()),

        _ => {
            let snap = gamma.out.checkpoint();
            if gamma.trial(|g| coerce(g, ar, br, span).is_ok()) {
                return Ok(br);
            }

            gamma.out.rollback_to(snap);

            if gamma.trial(|g| coerce(g, br, ar, span).is_ok()) {
                return Ok(ar);
            }

            gamma.out.rollback_to(snap);

            unify(gamma, ar, br, span)?;
            Ok(resolve(gamma, ar, span))
        }
    }
}

// Deep-copy `ty` (any arena) into gamma.out, remapping child indices. Only safe way to
// move possibly Static into a local arena. Shallow copy leaves dangling indices.
pub(crate) fn reify(gamma: &mut Gamma, ty: TyId, span: Span) -> Idx<TypeExpr> {
    let node = match *gamma.get(ty) {
        TypeExpr::App { ctor, params } => TypeExpr::App {
            ctor,
            params: reify_range(gamma, ty, params, span),
        },

        TypeExpr::Tuple { elements } => TypeExpr::Tuple {
            elements: reify_range(gamma, ty, elements, span),
        },

        TypeExpr::Function { params, ret } => {
            let params = reify_range(gamma, ty, params, span);
            let ret = reify(gamma, ty.with_idx(ret), span);

            TypeExpr::Function { params, ret }
        }

        TypeExpr::Nullable(inner) => TypeExpr::Nullable(reify(gamma, ty.with_idx(inner), span)),

        leaf => leaf,
    };

    let idx = gamma.out.ty_arena.alloc(node);
    gamma.out.ty_spans.insert(idx, span);
    idx
}

pub(crate) fn reify_range(
    gamma: &mut Gamma,
    parent: TyId,
    range: IdxRange<TypeExpr>,
    span: Span,
) -> IdxRange<TypeExpr> {
    let mut children = Vec::with_capacity(range.len());

    for c in range {
        let tyid = TyId::Local(reify(gamma, parent.with_idx(c), span));
        let texp = *gamma.get(tyid);
        children.push(texp);
    }

    alloc_contiguous(&mut gamma.out.ty_arena, children)
}

pub(crate) fn tuple_of(gamma: &mut Gamma, elems: &[TyId], span: Span) -> TyId {
    let idxs: Vec<TyId> = elems
        .iter()
        .map(|&t| TyId::Local(reify(gamma, t, span)))
        .collect();
    let nodes: Vec<TypeExpr> = idxs.iter().map(|&id| *gamma.get(id)).collect();
    let range = alloc_contiguous(&mut gamma.out.ty_arena, nodes);

    let idx = gamma
        .out
        .ty_arena
        .alloc(TypeExpr::Tuple { elements: range });
    gamma.out.ty_spans.insert(idx, span);

    TyId::Local(idx)
}

pub fn coerce_to_interface(
    gamma: &mut Gamma,
    from: TyId,
    iname: Ustr,
    span: Span,
) -> Result<(), CompilerError> {
    let isid = resolve_struct(gamma, iname).ok_or_else(|| {
        CompilerError::new_with_label(span, format!("unknown interface `{iname}`"))
    })?;

    let ifields: Vec<(Ustr, TyId, NsVisibility)> = gamma.workspace.module(isid.module).structs
        [isid.def]
        .fields
        .iter()
        .map(|f| (f.name, f.ty, f.write_vis.clone()))
        .collect();

    let from_peeled = match *gamma.get(from) {
        TypeExpr::App {
            ctor: TypeCtor::Ref,
            params,
        } => {
            let mut params = params;
            if let Some(inner) = params.next() {
                from.with_idx(inner)
            } else {
                from
            }
        }
        _ => from,
    };
    let from_key = match *gamma.get(from_peeled) {
        TypeExpr::App {
            ctor: TypeCtor::Nominal(name),
            ..
        } => Some(name),
        _ => None,
    };

    for (fname, fty, fwrite_vis) in ifields {
        let actual = struct_field_ty(gamma, from, fname, span).map_err(|e| match e {
            StructFieldError::MissingField { .. }
            | StructFieldError::AccessDenied {
                denied: AccessDenied::NoSuchField { .. },
                ..
            } => CompilerError::new_with_label(
                span,
                format!(
                    "type {} does not have required field `{}` of interface `{}`",
                    gamma.workspace.render(Some(&gamma.out), from),
                    fname,
                    iname
                ),
            ),
            other => other.into(),
        })?;

        // Structural interface conformance capability check:
        // An external conversion cannot use a public getter to satisfy a publicly writable interface field.
        if fwrite_vis == NsVisibility::Pub {
            if let Some(skey) = from_key {
                if gamma.workspace.interface_names.contains(&skey) {
                    let from_sid = resolve_struct(gamma, skey).ok_or_else(|| {
                        CompilerError::new_with_label(span, format!("unknown interface `{skey}`"))
                    })?;
                    let from_def = &gamma.workspace.module(from_sid.module).structs[from_sid.def];
                    if let Some(ff) = from_def.fields.iter().find(|f| f.name == fname) {
                        if ff.write_vis != NsVisibility::Pub {
                            comp_error!(
                                span,
                                "field `{}` on interface `{}` is not writable",
                                fname,
                                skey
                            );
                        }
                    }
                } else {
                    let site = gamma.access_site();
                    field_access(gamma, skey, fname, site, FieldAccessNeed::Write)
                        .map_err(|e| e.to_compiler_error(span))?;
                }
            }
        }

        coerce(gamma, actual, fty, span)?;
    }

    Ok(())
}

pub(crate) fn bound_var(gamma: &Gamma, ty: TyId) -> Option<VarId> {
    match *gamma.get(ty) {
        TypeExpr::Infer(var_id, _) => match gamma.out.substitutions.get(var_id) {
            Some(next) => bound_var(gamma, next).or(Some(var_id)),
            None => None,
        },

        _ => None,
    }
}

pub fn instantiate_params(
    gamma: &mut Gamma,
    generics: &[TyId],
    params: &[TyId],
    span: Span,
) -> Result<Vec<TyId>, CompilerError> {
    let (subs, _fresh) = gamma.instantiate_generics(generics, span)?;
    params
        .iter()
        .map(|&p| substitute(gamma, p, &subs, span))
        .collect()
}
