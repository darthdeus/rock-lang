use super::*;

// Defer-to-use inference for unconstrained lambda params.
// An overload ambiguity (cost tie) or
// field-projection ambiguity whose operand is an unresolved lambda-param var
// queues a pending constraint instead of erroring; when later unification pins
// the var, the constraint re-resolves with full information. Strictly
// error-path: a unique cheapest fit still pins eagerly, so programs that
// resolve today keep their picks. All state is body-local (Gamma), so
// bone-incremental's per-body model is untouched.

/// The lambda param a deferrable var types (or is chained from, for result
/// vars of deferred ops). Diagnostics only.
#[derive(Copy, Clone, Debug)]
pub(crate) struct DeferOrigin {
    pub(crate) name: Ustr,
    pub(crate) span: Span,
}

#[derive(Debug)]
pub(crate) enum Pending {
    Call {
        family: CallFamily,
        call_expr: Idx<Expr>,
        recv_override: Option<TyId>,
        args: Vec<Idx<Expr>>,
        arg_ty: FxHashMap<Idx<Expr>, TyId>,
        arg_tys: Vec<TyId>,
        preinferred: FxHashMap<Idx<Expr>, TyId>,
        origin: DeferOrigin,
        span: Span,
        result: TyId,
    },
    Field {
        base: TyId,
        field: Ustr,
        origin: DeferOrigin,
        span: Span,
        result: TyId,
    },
}

impl Pending {
    fn origin(&self) -> DeferOrigin {
        match self {
            Pending::Call { origin, .. } | Pending::Field { origin, .. } => *origin,
        }
    }
}

/// First operand that resolves to a var sanctioned for deferral.
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

/// First operand still unresolved at retry time. Any var qualifies: the entry
/// was sanctioned for deferral when queued, and a var->var unification may
/// have moved the wait onto an unmarked var.
fn unresolved_operand(gamma: &mut Gamma, tys: &[TyId], span: Span) -> Option<VarId> {
    for &t in tys {
        let r = resolve(gamma, t, span);
        if let TypeExpr::Infer(v, _) = *gamma.get(r) {
            return Some(v);
        }
    }
    None
}

/// Result var for a deferred op: named after the originating param and itself
/// deferrable, so downstream ops on it chain (`x * 2 + 1`).
pub(crate) fn defer_result_var(gamma: &mut Gamma, origin: DeferOrigin, span: Span) -> TyId {
    let ty = gamma.fresh_callsite_var(origin.name, span);
    let r = resolve(gamma, ty, span);
    if let TypeExpr::Infer(v, _) = *gamma.get(r) {
        gamma.deferrable_vars.insert(v, origin);
    }
    ty
}

pub(crate) fn queue_pending(gamma: &mut Gamma, v: VarId, p: Pending) {
    gamma.deferrable_vars.entry(v).or_insert(p.origin());
    let id = gamma.pending.len();
    gamma.pending.push(Some(p));
    gamma.pending_by_var.entry(v).or_default().push(id);
}

/// Retry every pending whose key var got pinned, to fixpoint (a retry can pin
/// further vars and make more entries ready). Runs at SEQUENCE POINTS only --
/// after each statement and at end of body -- never from inside `unify`:
/// unification also runs speculatively (`coerce`/`join`/`fit_cost` trial-keep
/// bindings, `score_candidate` trial-rollbacks them), and a retry commits
/// side effects (`record_resolved_call`) that no checkpoint can roll back.
/// At a sequence point nothing speculative is in flight, so every binding in
/// the substitution store is committed.
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
            let Some(ids) = gamma.pending_by_var.remove(&v) else { continue };
            for id in ids {
                let Some(p) = gamma.pending[id].take() else { continue };
                retry_pending(gamma, p)?;
            }
        }
    }
}

fn retry_pending(gamma: &mut Gamma, p: Pending) -> Result<(), CompilerError> {
    let c = gamma.body_ctx();
    match p {
        Pending::Call {
            family,
            call_expr,
            recv_override,
            args,
            arg_ty,
            arg_tys,
            preinferred,
            origin,
            span,
            result,
        } => {
            let name = family.key();
            // A queue entry can be a speculative duplicate (scoring infers
            // spread operands and rolls back only the substitution trail); if
            // an equivalent entry already committed this call, drop it.
            if gamma.out.resolved_overload_calls.contains_key(&call_expr) {
                return Ok(());
            }
            let candidates = resolve_callee(gamma, &family);
            match select_overload(
                c, gamma, family, &candidates, recv_override, &args, &arg_ty, &arg_tys, name, span,
            ) {
                Ok((mid, fid)) => {
                    let ret = commit_selected_call(
                        c,
                        gamma,
                        name,
                        mid,
                        fid,
                        recv_override,
                        &args,
                        &preinferred,
                        IdxRange::new_empty(),
                        call_expr,
                        span,
                    )?;
                    // `result` is the hole the deferred expression left; a use
                    // may have pinned it already (`print(..)` pins `any`, a
                    // `float` slot widens an int ret) -- coerce, don't unify.
                    coerce(gamma, ret, result, span)
                }
                Err(SelectError::Tied(e)) => match unresolved_operand(gamma, &arg_tys, span) {
                    Some(v) => {
                        queue_pending(
                            gamma,
                            v,
                            Pending::Call {
                                family,
                                call_expr,
                                recv_override,
                                args,
                                arg_ty,
                                arg_tys,
                                preinferred,
                                origin,
                                span,
                                result,
                            },
                        );
                        Ok(())
                    }
                    None => Err(e),
                },
                Err(SelectError::NoFit(e)) => Err(e),
            }
        }

        Pending::Field {
            base,
            field,
            origin,
            span,
            result,
        } => {
            let base_r = resolve(gamma, base, span);
            if let TypeExpr::Infer(v, _) = *gamma.get(base_r) {
                queue_pending(gamma, v, Pending::Field { base, field, origin, span, result });
                return Ok(());
            }
            let fty = struct_field_ty(gamma, base_r, field, span)?;
            coerce(gamma, fty, result, span)
        }
    }
}

/// Field projection with deferral: `_.x` on an unpinned lambda param where
/// several structs define the field waits for the pin. The single-match
/// reverse inference and every error case stay in `struct_field_ty`.
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
                        Pending::Field { base: base_ty, field, origin, span, result },
                    );
                    return Ok(result);
                }
            }
        }
    }
    Ok(struct_field_ty(gamma, base_ty, field, span)?)
}

/// End of body: drain what the body tail pinned, then report anything still
/// queued -- a never-used unconstrained lambda has no type; defer-to-use does
/// not invent one.
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
        let origin = p.origin();
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
