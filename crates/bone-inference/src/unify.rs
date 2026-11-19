use super::*;
use std::convert::Infallible;

pub(crate) fn contains_frame_ref(gamma: &Gamma, ty: TyId) -> bool {
    gamma.workspace.type_contains_ref(Some(&gamma.out), ty)
}

pub(crate) fn is_identity_struct_type(gamma: &Gamma, ty: TyId) -> bool {
    match *gamma.get(ty) {
        TypeExpr::Infer(var, _) => gamma
            .out
            .substitutions
            .get(var)
            .is_some_and(|next| is_identity_struct_type(gamma, next)),
        TypeExpr::Nullable(inner) => is_identity_struct_type(gamma, ty.with_idx(inner)),
        TypeExpr::App {
            ctor: TypeCtor::Nominal(name),
            ..
        } => resolve_struct(gamma, name)
            .is_some_and(|sid| !gamma.workspace.module(sid.module).structs[sid.def].is_value),
        _ => false,
    }
}

pub(crate) fn frame_ref_target(gamma: &Gamma, ty: TyId) -> Option<TyId> {
    let ty = match *gamma.get(ty) {
        TypeExpr::Nullable(inner) => ty.with_idx(inner),
        _ => ty,
    };
    match *gamma.get(ty) {
        TypeExpr::App {
            ctor: TypeCtor::Ref,
            mut params,
        } => params.next().map(|inner| ty.with_idx(inner)),
        _ => None,
    }
}

pub(crate) fn persistent_type_contains_frame_ref(gamma: &Gamma, ty: TyId) -> bool {
    match *gamma.get(ty) {
        TypeExpr::Infer(var, _) => gamma
            .out
            .substitutions
            .get(var)
            .is_some_and(|next| persistent_type_contains_frame_ref(gamma, next)),
        TypeExpr::Nullable(inner) => persistent_type_contains_frame_ref(gamma, ty.with_idx(inner)),
        TypeExpr::Tuple { elements } => elements
            .into_iter()
            .any(|part| persistent_type_contains_frame_ref(gamma, ty.with_idx(part))),
        TypeExpr::App {
            ctor: TypeCtor::Ref,
            ..
        } => false,
        TypeExpr::App { .. } | TypeExpr::Function { .. } => contains_frame_ref(gamma, ty),
        TypeExpr::Builtin(_) | TypeExpr::Generic(..) => false,
    }
}

pub(crate) fn substitute(
    gamma: &mut Gamma,
    ty: TyId,
    subs: &FxHashMap<VarId, TyId>,
    span: Span,
) -> Result<TyId, CompilerError> {
    let texp = *gamma.get(ty);
    match texp {
        TypeExpr::Builtin(_) => Ok(ty),
        TypeExpr::Generic(var_id, ustr) => {
            if let Some(sub) = subs.get(&var_id) {
                Ok(*sub)
            } else {
                comp_error!(span, "No sub for generic {ustr} in {:?}", texp);
            }
        }
        TypeExpr::Infer(..) => {
            comp_error!(span, "Unsupported type expr {:?}", texp)
        }
        TypeExpr::Nullable(..)
        | TypeExpr::App { .. }
        | TypeExpr::Tuple { .. }
        | TypeExpr::Function { .. } => map_structural_type(gamma, ty, texp, span, |g, child| {
            substitute(g, child, subs, span)
        })
        .map(|mapped| mapped.unwrap_or(ty)),
    }
}

fn map_structural_type<E>(
    gamma: &mut Gamma,
    ty: TyId,
    texp: TypeExpr,
    span: Span,
    mut map: impl FnMut(&mut Gamma, TyId) -> Result<TyId, E>,
) -> Result<Option<TyId>, E> {
    match texp {
        TypeExpr::Nullable(child) => {
            let child_ty = ty.with_idx(child);
            let mapped_child = map(gamma, child_ty)?;
            Ok((mapped_child != child_ty).then(|| nullable_of(gamma, mapped_child, span)))
        }
        TypeExpr::App { ctor, params } => {
            let Some(params) = map_children(gamma, ty, params, span, &mut map)? else {
                return Ok(None);
            };
            let idx = gamma.out.ty_arena.alloc(TypeExpr::App { ctor, params });
            gamma.out.ty_spans.insert(idx, span);
            Ok(Some(TyId::Local(idx)))
        }
        TypeExpr::Tuple { elements } => {
            let Some(elements) = map_children(gamma, ty, elements, span, &mut map)? else {
                return Ok(None);
            };
            let idx = gamma.out.ty_arena.alloc(TypeExpr::Tuple { elements });
            gamma.out.ty_spans.insert(idx, span);
            Ok(Some(TyId::Local(idx)))
        }
        TypeExpr::Function { params, ret } => {
            let new_params = map_children(gamma, ty, params, span, &mut map)?;
            let ret_ty = ty.with_idx(ret);
            let mapped_ret = map(gamma, ret_ty)?;

            if new_params.is_none() && mapped_ret == ret_ty {
                Ok(None)
            } else {
                let params = new_params.unwrap_or_else(|| reify_range(gamma, ty, params, span));
                let ret = reify(gamma, mapped_ret, span);
                let idx = gamma.out.ty_arena.alloc(TypeExpr::Function { params, ret });
                gamma.out.ty_spans.insert(idx, span);
                Ok(Some(TyId::Local(idx)))
            }
        }
        _ => unreachable!("map_structural_type called on a leaf type"),
    }
}

pub fn unify(gamma: &mut Gamma, a: TyId, b: TyId, span: Span) -> Result<(), CompilerError> {
    let a = resolve(gamma, a, span);
    let b = resolve(gamma, b, span);
    let (params, other_params, returns, arity_error) = match (*gamma.get(a), *gamma.get(b)) {
        (TypeExpr::Builtin(x), TypeExpr::Builtin(y)) if x == y => return Ok(()),
        (TypeExpr::Builtin(BuiltinTy::Never | BuiltinTy::Error), _)
        | (_, TypeExpr::Builtin(BuiltinTy::Never | BuiltinTy::Error)) => return Ok(()),
        (TypeExpr::Generic(x, _), TypeExpr::Generic(y, _)) if x == y => return Ok(()),
        (TypeExpr::Infer(x, _), TypeExpr::Infer(y, _)) if x == y => return Ok(()),
        (TypeExpr::Infer(var, _), _) => return bind_var_checked(gamma, var, b, span),
        (_, TypeExpr::Infer(var, _)) => return bind_var_checked(gamma, var, a, span),
        (TypeExpr::Nullable(inner), TypeExpr::Nullable(other)) => {
            return unify(gamma, a.with_idx(inner), b.with_idx(other), span);
        }
        (_, TypeExpr::Nullable(inner)) => return unify(gamma, a, b.with_idx(inner), span),
        (
            TypeExpr::App { ctor, params },
            TypeExpr::App {
                ctor: other,
                params: other_params,
            },
        ) => {
            if ctor != other {
                let detail = if params.is_empty() && other_params.is_empty() {
                    ""
                } else {
                    " (different type constructors)"
                };
                unify_fail!(
                    gamma,
                    span,
                    "type mismatch: expected `{}`, found `{}`{detail}",
                    gamma.render(b),
                    gamma.render(a)
                );
            }
            (params, other_params, None, "generics of different length")
        }
        (TypeExpr::Tuple { elements }, TypeExpr::Tuple { elements: other }) => {
            (elements, other, None, "tuples of different length")
        }
        (
            TypeExpr::Function { params, ret },
            TypeExpr::Function {
                params: other_params,
                ret: other_ret,
            },
        ) => (
            params,
            other_params,
            Some((ret, other_ret)),
            "functions with different arity",
        ),
        _ => unify_fail!(
            gamma,
            span,
            "type mismatch: expected `{}`, found `{}`",
            gamma.render(b),
            gamma.render(a)
        ),
    };
    if params.len() != other_params.len() {
        unify_fail!(
            gamma,
            span,
            "Failed to unify {} and {}, {arity_error}",
            gamma.render(a),
            gamma.render(b)
        );
    }
    for (param, other) in params.zip(other_params) {
        unify(gamma, a.with_idx(param), b.with_idx(other), span)?;
    }
    if let Some((ret, other)) = returns {
        unify(gamma, a.with_idx(ret), b.with_idx(other), span)?;
    }
    Ok(())
}

pub(crate) fn resolve(gamma: &mut Gamma, ty: TyId, span: Span) -> TyId {
    let texp = *gamma.get(ty);
    match texp {
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
        TypeExpr::Nullable(..)
        | TypeExpr::App { .. }
        | TypeExpr::Tuple { .. }
        | TypeExpr::Function { .. } => map_structural_type(gamma, ty, texp, span, |g, child| {
            Ok::<_, Infallible>(resolve(g, child, span))
        })
        .unwrap_or_else(|never| match never {})
        .unwrap_or(ty),
        _ => ty,
    }
}

fn map_children<E>(
    gamma: &mut Gamma,
    root_ty: TyId,
    elements: IdxRange<TypeExpr>,
    span: Span,
    mut f: impl FnMut(&mut Gamma, TyId) -> Result<TyId, E>,
) -> Result<Option<IdxRange<TypeExpr>>, E> {
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

pub(crate) fn nullable_of(gamma: &mut Gamma, inner: TyId, span: Span) -> TyId {
    if matches!(gamma.get(inner), TypeExpr::Nullable(_)) {
        return inner;
    }

    let inner_idx = reify(gamma, inner, span);
    let idx = gamma.out.ty_arena.alloc(TypeExpr::Nullable(inner_idx));
    gamma.out.ty_spans.insert(idx, span);

    TyId::Local(idx)
}

pub(crate) fn peel_ref(gamma: &Gamma, ty: TyId) -> TyId {
    match *gamma.get(ty) {
        TypeExpr::App {
            ctor: TypeCtor::Ref,
            mut params,
        } => params.next().map_or(ty, |inner| ty.with_idx(inner)),
        _ => ty,
    }
}

pub(crate) fn peel_nullable(gamma: &mut Gamma, ty: TyId, span: Span) -> TyId {
    let ty = resolve(gamma, ty, span);

    match *gamma.get(ty) {
        TypeExpr::Nullable(inner) => resolve(gamma, ty.with_idx(inner), span),
        _ => ty,
    }
}

pub(crate) fn types_equal(gamma: &Gamma, a: TyId, b: TyId) -> bool {
    if a == b {
        return true;
    }

    match (gamma.get(a), gamma.get(b)) {
        (TypeExpr::Builtin(x), TypeExpr::Builtin(y)) => x == y,
        (TypeExpr::Generic(x, _), TypeExpr::Generic(y, _)) => x == y,
        (TypeExpr::Infer(x, _), TypeExpr::Infer(y, _)) => x == y,
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

fn occurs_in(gamma: &Gamma, var: VarId, ty: TyId) -> bool {
    gamma.workspace.type_any(
        Some(&gamma.out),
        ty,
        &mut |node| matches!(node, TypeExpr::Infer(v, _) if v == var),
    )
}

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
            gamma.render(ty)
        );
    }
    gamma.out.bind_var(var, ty);
    Ok(())
}

/// Whether `ty` contains an unbound inference variable, without path compression.
pub(crate) fn has_unbound_infer(gamma: &Gamma, ty: TyId) -> bool {
    gamma.workspace.type_any(Some(&gamma.out), ty, &mut |node| {
        matches!(node, TypeExpr::Infer(v, _) if gamma.out.substitutions.get(v).is_none())
    })
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

    // Stored functions keep their return ABI; only contextual lambda literals
    // can adopt the target's nullable return shape.
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

    // Bare `any` excludes the null literal, but accepts nullable values with a
    // concrete inner type.
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
        gamma.out.any_erasures.push((span, from_r));
        return Ok(());
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

    // FieldRef carries read and write capabilities and coerces to either single-
    // capability form. ReadFieldRef is covariant in V; WriteFieldRef is invariant.
    // The single-capability forms stay incomparable to prevent whole-value reads
    // through embedded fields.
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

// Reify into the local arena so copied child indices cannot point into a static arena.
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

    let ifields = &gamma.workspace.module(isid.module).structs[isid.def].fields;

    let from_key = match *gamma.get(peel_ref(gamma, from)) {
        TypeExpr::App {
            ctor: TypeCtor::Nominal(name),
            ..
        } => Some(name),
        _ => None,
    };

    for field in ifields {
        let fname = field.name;
        let actual = struct_field_ty(gamma, from, fname, span).map_err(|e| match e {
            StructFieldError::MissingField { .. }
            | StructFieldError::AccessDenied {
                denied: AccessDenied::NoSuchField { .. },
                ..
            } => CompilerError::new_with_label(
                span,
                format!(
                    "type {} does not have required field `{}` of interface `{}`",
                    gamma.render(from),
                    fname,
                    iname
                ),
            ),
            other => other.into(),
        })?;

        // A public getter cannot satisfy a publicly writable interface field.
        if field.write_vis == NsVisibility::Pub {
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

        coerce(gamma, actual, field.ty, span)?;
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
