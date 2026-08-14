//! Projection-only enforcement for `embed` fields
//! (docs/scalarization-and-boxing-design.md S 6).
//!
//! An embedded field's leaves live in the parent, so no aggregate object exists
//! to hand out: the field may be PROJECTED (leaf reads, place writes,
//! whole-field ctor writes, a scalar-ABI round trip, presence tests) but never
//! BOUND, RETURNED, CAPTURED, ERASED, or MATERIALIZED independently.
//!
//! The check runs as an allow-list over the flat expression arena rather than a
//! recursive walk with a parent map: one pass marks every position that CONFERS
//! legality on the expression sitting in it, a second pass rejects any embed
//! field access not so marked. That ordering is fail-closed by construction --
//! a new `Expr` variant confers nothing, so an unforeseen way to consume an
//! embedded field rejects instead of slipping through.

use super::*;

/// Positions that keep an embedded field leaf-wise, keyed by the expression
/// occupying them.
fn legal_positions(c: BodyCtx, gamma: &Gamma) -> FxHashSet<Idx<Expr>> {
    let mut ok: FxHashSet<Idx<Expr>> = FxHashSet::default();

    for (e, expr) in c.body.expr_arena.iter() {
        match *expr {
            // `h.p.x`, `h.p?.x` -- projecting a leaf out of the embedded value.
            Expr::FieldAccess { base, .. } | Expr::SafeNav { base, .. } => {
                ok.insert(base);
            }

            // `h.p = Foo { .. }` / `h.p = null` -- a whole-field ctor write is
            // leaf stores plus (for a nullable embed) a presence publish. The
            // VALUE side is not marked: `x = h.p` binds.
            Expr::Assign { target, .. } | Expr::CompoundAssign { target, .. } => {
                ok.insert(target);
            }
            Expr::IncDec { target, .. } => {
                ok.insert(target);
            }

            // `h.p == other`, `h.p != null` -- lowered leaf-wise / presence-gated.
            Expr::BinOp {
                op: hir::BinOp::Eq | hir::BinOp::NotEq,
                lhs,
                rhs,
            } => {
                ok.insert(lhs);
                ok.insert(rhs);
            }

            // `advance(h.p, dt)` -- legal only when the callee takes LEAVES.
            Expr::Call { callee, args, .. } => {
                if scalar_abi_callee(c, gamma, e, callee) {
                    ok.extend(args.into_iter());
                }
            }

            // `"{h.p}"` -- dump/serialize present the LOGICAL form.
            Expr::InterpolatedString(parts) => {
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

/// Does this call hand its arguments over as scalar leaves?
///
/// A direct call to a Rock-bodied fn rides the scalar ABI. A `@nomangle` /
/// `extern` / `inline(<backend>)` callee receives a BOX (its body is an opaque
/// string to the compiler), and a callee reached through a fn VALUE has no
/// known decl at all -- both materialize the aggregate.
fn scalar_abi_callee(c: BodyCtx, gamma: &Gamma, call: Idx<Expr>, callee: Callee) -> bool {
    let _ = c;
    match callee {
        Callee::Local(_) | Callee::Value(_) => false,
        Callee::Unresolved { .. } => match gamma.out.resolved_overload_calls.get(&call) {
            Some(&(mid, fid)) => {
                let f = &gamma.workspace.module(mid).functions[fid];
                !f.is_extern && f.inline_body.is_none() && !fn_has_anno(f, "nomangle")
            }
            // Unresolved at this point means inference already reported the
            // call; don't pile on, and don't grant legality either.
            None => false,
        },
    }
}

/// The struct field `<base>.<field>` resolves to, when it is an `embed` one.
fn embed_field_of(gamma: &mut Gamma, base_ty: TyId, field: Ustr, span: Span) -> bool {
    let base_r = resolve(gamma, base_ty, span);
    let mut ty = base_r;
    // A nullable owner still owns the field; the access itself is checked
    // elsewhere.
    if let TypeExpr::Nullable(inner) = *gamma.get(ty) {
        ty = ty.with_idx(inner);
        ty = resolve(gamma, ty, span);
    }
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

/// Reject every use of an `embed` field that is not a projection.
pub(crate) fn verify_embed_projection(c: BodyCtx, gamma: &mut Gamma) {
    // O(1) bail for a workspace with no `embed` anywhere -- the common case, and
    // this pass otherwise touches every field access in every body.
    if !gamma.workspace.has_embed_fields {
        return;
    }

    let mut errors: Vec<CompilerError> = Vec::new();
    let accesses: Vec<(Idx<Expr>, Idx<Expr>, Ustr)> = c
        .body
        .expr_arena
        .iter()
        .filter_map(|(e, expr)| match *expr {
            Expr::FieldAccess { base, field } | Expr::SafeNav { base, field } => {
                Some((e, base, field))
            }
            _ => None,
        })
        .collect();
    if accesses.is_empty() {
        return;
    }

    let mut embedded: Vec<(Idx<Expr>, Ustr)> = Vec::new();
    for (e, base, field) in accesses {
        let Some(base_ty) = gamma.out.expr_types.get(base).copied() else {
            continue;
        };
        if embed_field_of(gamma, base_ty, field, c.body.expr_spans[e]) {
            embedded.push((e, field));
        }
    }
    if embedded.is_empty() {
        return;
    }

    let ok = legal_positions(c, gamma);

    for (e, field) in embedded {
        // A `SafeNav` on an embedded field is itself a leaf projection
        // (`h.p?.x` parses as SafeNav{base: h.p}), so only the ACCESS of the
        // embedded field is judged here, never the leaf it produced.
        if ok.contains(&e) {
            continue;
        }
        errors.push(CompilerError::new_with_label(
            c.body.expr_spans[e],
            format!(
                "`{field}` is an `embed` field: it has no independently storable value, so it \
                 cannot be bound, returned, captured, erased, or passed to a callee that takes a \
                 whole struct -- project a leaf (`.{field}.<sub>`), write it whole (`.{field} = \
                 ...`), or pass it to a direct call that takes leaves"
            ),
        ));
    }

    gamma.body_errors.append(&mut errors);
}
