use super::*;

// Defer tied overloads and ambiguous field projections until later constraints
// pin the lambda parameter; unique cheapest overloads still resolve eagerly.

#[derive(Copy, Clone, Debug)]
pub(crate) struct DeferOrigin {
    pub(crate) name: Ustr,
    pub(crate) span: Span,
}

#[derive(Debug)]
pub(crate) struct Pending {
    pub(crate) kind: PendingKind,
    pub(crate) origin: DeferOrigin,
    pub(crate) span: Span,
    pub(crate) result: TyId,
}

#[derive(Debug)]
pub(crate) enum PendingKind {
    Call {
        family: CallFamily,
        call_expr: Idx<Expr>,
        recv_override: Option<TyId>,
        args: Vec<Idx<Expr>>,
        arg_ty: FxHashMap<Idx<Expr>, TyId>,
        arg_tys: Vec<TyId>,
        preinferred: FxHashMap<Idx<Expr>, TyId>,
    },
    Field {
        base: TyId,
        field: Ustr,
    },
}

/// First operand that resolves to a deferrable inference variable.
pub(crate) fn deferrable_operand(
    gamma: &mut Gamma,
    tys: &[TyId],
    span: Span,
) -> Option<(VarId, DeferOrigin)> {
    for &t in tys {
        let r = resolve(gamma, t, span);
        if let TypeExpr::Infer(v, _) = *gamma.get(r) {
            if let Some(&origin) = gamma.deferrable_vars.get(&v) {
                return Some((v, origin));
            }
        }
    }
    None
}

/// First operand still unresolved at retry time. Variable unification may have
/// moved a queued constraint to a var that was not originally marked.
fn unresolved_operand(gamma: &mut Gamma, tys: &[TyId], span: Span) -> Option<VarId> {
    for &t in tys {
        let r = resolve(gamma, t, span);
        if let TypeExpr::Infer(v, _) = *gamma.get(r) {
            return Some(v);
        }
    }
    None
}

/// Make a result var deferrable so later operations can chain from it.
pub(crate) fn defer_result_var(gamma: &mut Gamma, origin: DeferOrigin, span: Span) -> TyId {
    let ty = gamma.fresh_callsite_var(origin.name, span);
    let r = resolve(gamma, ty, span);
    if let TypeExpr::Infer(v, _) = *gamma.get(r) {
        gamma.deferrable_vars.insert(v, origin);
    }
    ty
}

pub(crate) fn queue_pending(gamma: &mut Gamma, v: VarId, p: Pending) {
    gamma.deferrable_vars.entry(v).or_insert(p.origin);
    let id = gamma.pending.len();
    gamma.pending.push(Some(p));
    gamma.pending_by_var.entry(v).or_default().push(id);
}

/// Retry ready constraints to a fixpoint at statement and body-end sequence
/// points. Do not retry inside `unify`: type work can be speculative there,
/// while retries commit call records that substitution rollback cannot undo.
pub(crate) fn drain_ready_pendings(gamma: &mut Gamma) -> Result<(), CompilerError> {
    if gamma.quiet || gamma.pending_by_var.is_empty() {
        return Ok(());
    }
    loop {
        let ready: SmallVec<[VarId; 4]> = gamma
            .pending_by_var
            .keys()
            .filter(|v| gamma.out.substitutions.get(**v).is_some())
            .copied()
            .collect();
        if ready.is_empty() {
            return Ok(());
        }
        for v in ready {
            let Some(ids) = gamma.pending_by_var.remove(&v) else {
                continue;
            };
            for id in ids {
                let Some(p) = gamma.pending[id].take() else {
                    continue;
                };
                retry_pending(gamma, p)?;
            }
        }
    }
}

fn retry_pending(gamma: &mut Gamma, pending: Pending) -> Result<(), CompilerError> {
    let c = gamma.body_ctx();
    let span = pending.span;
    let ret = match &pending.kind {
        PendingKind::Call {
            family,
            call_expr,
            recv_override,
            args,
            arg_ty,
            arg_tys,
            preinferred,
        } => {
            if gamma.out.resolved_overload_calls.contains_key(call_expr) {
                return Ok(());
            }
            let name = family.key();
            let candidates = resolve_callee(gamma, family);
            match select_overload(
                c,
                gamma,
                *family,
                &candidates,
                *recv_override,
                args,
                arg_ty,
                arg_tys,
                name,
                span,
            ) {
                Ok((mid, fid)) => commit_selected_call(
                    c,
                    gamma,
                    name,
                    mid,
                    fid,
                    *recv_override,
                    args,
                    preinferred,
                    IdxRange::new_empty(),
                    *call_expr,
                    span,
                )?,
                Err(SelectError::Tied(error)) => {
                    if let Some(var) = unresolved_operand(gamma, arg_tys, span) {
                        queue_pending(gamma, var, pending);
                        return Ok(());
                    }
                    return Err(error);
                }
                Err(SelectError::NoFit(error)) => return Err(error),
            }
        }
        PendingKind::Field { base, field } => {
            let base = resolve(gamma, *base, span);
            if let TypeExpr::Infer(var, _) = *gamma.get(base) {
                queue_pending(gamma, var, pending);
                return Ok(());
            }
            struct_field_ty(gamma, base, *field, span)?
        }
    };
    coerce(gamma, ret, pending.result, span)
}

/// Defer an ambiguous field projection; `struct_field_ty` handles unique
/// matches and all error cases.
pub(crate) fn field_ty_or_defer(
    gamma: &mut Gamma,
    base_ty: TyId,
    field: Ustr,
    span: Span,
) -> Result<TyId, CompilerError> {
    if !gamma.quiet {
        if let TypeExpr::Infer(v, _) = *gamma.get(base_ty) {
            if let Some(&origin) = gamma.deferrable_vars.get(&v) {
                if structs_with_field(gamma, field).len() > 1 {
                    let result = defer_result_var(gamma, origin, span);
                    queue_pending(
                        gamma,
                        v,
                        Pending {
                            kind: PendingKind::Field {
                                base: base_ty,
                                field,
                            },
                            origin,
                            span,
                            result,
                        },
                    );
                    return Ok(result);
                }
            }
        }
    }
    Ok(struct_field_ty(gamma, base_ty, field, span)?)
}

pub(crate) fn report_unpinned_lambda_params(gamma: &mut Gamma) {
    if let Err(e) = drain_ready_pendings(gamma) {
        gamma.body_errors.push(e);
    }
    if gamma.pending.iter().all(Option::is_none) {
        gamma.pending.clear();
        gamma.pending_by_var.clear();
        return;
    }
    let mut seen: FxHashSet<(Ustr, Span)> = FxHashSet::default();
    for p in gamma.pending.iter().flatten() {
        let origin = p.origin;
        if seen.insert((origin.name, origin.span)) {
            gamma.body_errors.push(CompilerError::new_with_label(
                origin.span,
                format!(
                    "cannot infer type of lambda parameter `{}`: annotate it or use the lambda",
                    origin.name
                ),
            ));
        }
    }
    gamma.pending.clear();
    gamma.pending_by_var.clear();
}
