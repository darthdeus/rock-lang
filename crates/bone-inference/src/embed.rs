//! Projection-only enforcement for `embed` fields.
//!
//! Embedded leaves live in the parent, so the field has no aggregate value to
//! bind, return, capture, erase, or materialize.
//!
//! An allow-list pass marks legal projections before another pass rejects
//! unmarked accesses. New expression forms are rejected until explicitly allowed.

use super::*;

/// Expressions in contexts that preserve embedded leaves.
fn legal_positions(c: BodyCtx, gamma: &Gamma) -> FxHashSet<Idx<Expr>> {
    let mut ok: FxHashSet<Idx<Expr>> = FxHashSet::default();

    for (e, expr) in c.body.expr_arena.iter() {
        match *expr {
            Expr::FieldAccess { base, .. } | Expr::SafeNav { base, .. } => {
                ok.insert(base);
            }

            // Whole-field writes lower to leaf stores and possibly a presence
            // publish; the value side still materializes the aggregate.
            Expr::Assign { target, .. } | Expr::CompoundAssign { target, .. } => {
                ok.insert(target);
            }
            Expr::IncDec { target, .. } => {
                ok.insert(target);
            }

            // Equality lowers leaf-wise; null checks are presence-gated.
            Expr::BinOp {
                op: hir::BinOp::Eq | hir::BinOp::NotEq,
                lhs,
                rhs,
            } => {
                ok.insert(lhs);
                ok.insert(rhs);
            }

            Expr::Call { callee, args, .. } => {
                if scalar_abi_callee(gamma, e, callee) {
                    ok.extend(args);
                }
            }

            Expr::InterpolatedString(parts) => {
                // Serialization presents the logical form.
                for p in parts {
                    if let InterpPart::Value { expr, .. } = c.body.interp_part_arena[p] {
                        ok.insert(expr);
                    }
                }
            }

            _ => {}
        }
    }

    ok
}

/// Whether a direct call passes arguments as scalar leaves.
///
/// Rock-bodied calls use the scalar ABI. Foreign, inline, `@nomangle`, and
/// function-value calls materialize the aggregate.
fn scalar_abi_callee(gamma: &Gamma, call: Idx<Expr>, callee: Callee) -> bool {
    match callee {
        Callee::Local(_) | Callee::Value(_) => false,
        Callee::Unresolved { .. } => match gamma.out.resolved_overload_calls.get(&call) {
            Some(&(mid, fid)) => {
                let f = &gamma.workspace.module(mid).functions[fid];
                !f.is_extern && f.inline_body.is_none() && !fn_has_anno(f, "nomangle")
            }
            // Inference already reported unresolved calls; do not grant legality.
            None => false,
        },
    }
}

/// Whether `<base>.<field>` is an embedded field.
pub(crate) fn embed_field_of(gamma: &mut Gamma, base_ty: TyId, field: Ustr, span: Span) -> bool {
    // A nullable owner still owns the field; access validity is checked elsewhere.
    let ty = peel_nullable(gamma, base_ty, span);
    let TypeExpr::App {
        ctor: TypeCtor::Nominal(name),
        ..
    } = *gamma.get(ty)
    else {
        return false;
    };
    let Some(sid) = resolve_struct(gamma, name) else {
        return false;
    };
    gamma.workspace.module(sid.module).structs[sid.def]
        .fields
        .iter()
        .any(|f| f.name == field && f.is_embed)
}

pub(crate) fn verify_embed_projection(c: BodyCtx, gamma: &mut Gamma) {
    // Skip the arena scan when the workspace has no embedded fields.
    if !gamma.workspace.has_embed_fields {
        return;
    }

    let embedded = c
        .body
        .expr_arena
        .iter()
        .filter_map(|(expr, node)| {
            let (Expr::FieldAccess { base, field } | Expr::SafeNav { base, field }) = *node else {
                return None;
            };
            let ty = gamma.out.expr_types.get(base).copied()?;
            embed_field_of(gamma, ty, field, c.body.expr_spans[expr]).then_some((expr, field))
        })
        .collect_vec();
    if embedded.is_empty() {
        return;
    }

    let ok = legal_positions(c, gamma);

    for (e, field) in embedded {
        // Safe navigation projects a leaf; judge the embedded-field access,
        // not the leaf it produced.
        if ok.contains(&e) {
            continue;
        }
        gamma.body_errors.push(CompilerError::new_with_label(
            c.body.expr_spans[e],
            format!(
                "`{field}` is an `embed` field: it has no independently storable value, so it \
                 cannot be bound, returned, captured, erased, or passed to a callee that takes a \
                 whole struct -- project a leaf (`.{field}.<sub>`), write it whole (`.{field} = \
                 ...`), or pass it to a direct call that takes leaves"
            ),
        ));
    }
}
