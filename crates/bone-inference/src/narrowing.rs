//! Non-null refinements use a shared `(root, field path)` set for locals and
//! globals; whole-local types are narrowed in `binding_types`. Calls and yields
//! clear path refinements, and calls may revoke whole-local narrows assigned by
//! a closure.

use crate::expr::block_ends_in_exit;
use bone_core::hir;
use bone_core::hir::{BinOp, BindingInfo, Callee, Expr, Statement};
use bone_core::prelude::*;

use crate::{BodyCtx, Gamma, Use, fn_has_anno, infer_expr_used, peel_nullable};

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Root {
    Local(Idx<BindingInfo>),
    Global(Ustr),
}

impl Root {
    pub(crate) fn is_global(&self) -> bool {
        matches!(self, Root::Global(_))
    }
}

pub(crate) type Narrow = (Root, Vec<Ustr>);

pub(crate) fn path_key(c: BodyCtx, e: Idx<Expr>) -> Option<Narrow> {
    match c.body.expr_arena[e] {
        Expr::Var(b) => Some((Root::Local(b), vec![])),
        Expr::Global(name) => Some((Root::Global(name), vec![])),

        Expr::FieldAccess { base, field } | Expr::SafeNav { base, field } => {
            let (root, mut fields) = path_key(c, base)?;
            fields.push(field);
            Some((root, fields))
        }

        _ => None,
    }
}

/// Include ancestor paths because a field path requires each parent to be non-null.
fn path_prefixes((root, fields): Narrow) -> Vec<Narrow> {
    (0..=fields.len())
        .map(|i| (root, fields[..i].to_vec()))
        .collect()
}

/// Only the stdlib predicate promises that false means its argument is non-null.
/// An unresolved call or a user overload with the same name promises nothing.
fn is_stdlib_null_predicate(gamma: &Gamma, e: Idx<Expr>) -> bool {
    gamma
        .out
        .resolved_overload_calls
        .get(&e)
        .is_some_and(|&(mid, fid)| {
            let module = gamma.workspace.module(mid);
            bone_core::modules::is_stdlib_module_file(module.file.as_str())
                && module.functions[fid].name.as_str() == "is_null"
        })
}

/// Whether evaluating an already-inferred condition can invalidate a mutable
/// path after its null check. Visit all children, but not lambda bodies.
fn cond_has_effect(gamma: &Gamma, c: BodyCtx, e: Idx<Expr>) -> bool {
    let callee_pure = |e: Idx<Expr>| {
        gamma
            .out
            .resolved_overload_calls
            .get(&e)
            .is_some_and(|&(mid, fid)| {
                fn_has_anno(&gamma.workspace.module(mid).functions[fid], "pure")
            })
    };
    match c.body.expr_arena[e] {
        Expr::Call {
            callee: Callee::Unresolved { .. },
            ..
        } if !callee_pure(e) && !is_stdlib_null_predicate(gamma, e) => true,
        Expr::SafeNavCall { .. } if !callee_pure(e) => true,
        Expr::Call {
            callee: Callee::Local(_) | Callee::Value(_),
            ..
        } => true,
        Expr::Yield(_)
        | Expr::YieldDelegate(_)
        | Expr::Assign { .. }
        | Expr::CompoundAssign { .. }
        | Expr::IncDec { .. } => true,
        Expr::Lambda { .. } => false,
        _ => {
            let mut effect = false;
            c.body
                .for_each_child(bone_core::hir_walk::HirChild::Expr(e), |child| {
                    effect = effect || child_has_effect(gamma, c, child);
                });
            effect
        }
    }
}

fn child_has_effect(gamma: &Gamma, c: BodyCtx, child: bone_core::hir_walk::HirChild) -> bool {
    use bone_core::hir_walk::HirChild;
    match child {
        HirChild::Expr(e) => cond_has_effect(gamma, c, e),
        // Non-expression statements in a condition block may write.
        HirChild::Statement(s) => match c.body.statement_arena[s] {
            Statement::Expr(e) => cond_has_effect(gamma, c, e),
            Statement::Let { value, .. } => cond_has_effect(gamma, c, value),
            Statement::Comment(_) => false,
            _ => true,
        },
        HirChild::Pattern(_) => false,
    }
}

pub(crate) fn closure_assigned_locals(body: &hir::Body) -> FxHashSet<Idx<BindingInfo>> {
    use bone_core::hir_walk::HirChild;
    let mut out = FxHashSet::default();
    for (lambda, expr) in body.expr_arena.iter() {
        let Expr::Lambda { body: lbody, .. } = expr else {
            continue;
        };
        let (lo, hi) = body.expr_spans[lambda].offset_range();
        let declared_inside = |b: Idx<BindingInfo>| {
            let (blo, bhi) = body.binding_spans[b].offset_range();
            lo <= blo && bhi <= hi
        };
        let mut stack = vec![HirChild::Expr(*lbody)];
        while let Some(node) = stack.pop() {
            if let HirChild::Expr(e) = node {
                match body.expr_arena[e] {
                    Expr::Assign { target, .. }
                    | Expr::CompoundAssign { target, .. }
                    | Expr::IncDec { target, .. } => {
                        if let Expr::Var(b) = body.expr_arena[target]
                            && !declared_inside(b)
                        {
                            out.insert(b);
                        }
                    }
                    _ => {}
                }
            }
            body.for_each_child(node, |child| stack.push(child));
        }
    }
    out
}

pub(crate) fn detect_null_narrow(
    gamma: &Gamma,
    c: BodyCtx,
    cond: Idx<Expr>,
) -> (Vec<Narrow>, Vec<Narrow>) {
    let (mut then_set, mut else_set) = detect_null_narrow_inner(gamma, c, cond);
    if cond_has_effect(gamma, c, cond) {
        then_set.retain(|(root, fields)| !root.is_global() && fields.is_empty());
        else_set.retain(|(root, fields)| !root.is_global() && fields.is_empty());
    }
    (then_set, else_set)
}

fn detect_null_narrow_inner(
    gamma: &Gamma,
    c: BodyCtx,
    cond: Idx<Expr>,
) -> (Vec<Narrow>, Vec<Narrow>) {
    match c.body.expr_arena[cond] {
        Expr::Not(inner, _) => {
            let (t, e) = detect_null_narrow_inner(gamma, c, inner);
            (e, t)
        }

        Expr::BinOp {
            op: hir::BinOp::Or,
            lhs,
            rhs,
        } => {
            let (_, mut le) = detect_null_narrow_inner(gamma, c, lhs);
            let (_, re) = detect_null_narrow_inner(gamma, c, rhs);

            le.extend(re);

            (vec![], le)
        }

        Expr::Call { args, .. } if is_stdlib_null_predicate(gamma, cond) && args.len() == 1 => {
            match path_key(c, args.start()) {
                Some(p) => (vec![], path_prefixes(p)),
                None => (vec![], vec![]),
            }
        }

        Expr::BinOp {
            op: hir::BinOp::And,
            lhs,
            rhs,
        } => {
            let (mut lt, _) = detect_null_narrow_inner(gamma, c, lhs);
            let (rt, _) = detect_null_narrow_inner(gamma, c, rhs);

            lt.extend(rt);

            (lt, vec![])
        }

        Expr::BinOp { op, lhs, rhs } if matches!(op, BinOp::NotEq | BinOp::Eq) => {
            let null_side = match (c.body.expr_arena[lhs], c.body.expr_arena[rhs]) {
                (_, Expr::Null) => path_key(c, lhs),
                (Expr::Null, _) => path_key(c, rhs),
                _ => None,
            };

            match (null_side, op) {
                (Some(p), BinOp::NotEq) => (path_prefixes(p), vec![]),
                (Some(p), BinOp::Eq) => (vec![], path_prefixes(p)),
                _ => (vec![], vec![]),
            }
        }

        _ => (vec![], vec![]),
    }
}

/// Apply branch narrowings and restore only the facts this branch added, so an
/// intervening effect's invalidations remain in force.
pub(crate) fn infer_branch(
    c: BodyCtx,
    gamma: &mut Gamma,
    branch: Idx<Expr>,
    narrows: &[Narrow],
    span: Span,
    used: Use,
) -> Result<TyId, CompilerError> {
    let mut saved_bindings = vec![];
    let mut added_paths = vec![];
    let refinement_epoch = gamma.refinement_epoch;

    for (root, fields) in narrows {
        match root {
            Root::Local(b) if fields.is_empty() => {
                if let Some(cur) = gamma.out.binding_types.get(*b).copied() {
                    let narrowed = peel_nullable(gamma, cur, span);
                    gamma.out.binding_types.insert(*b, narrowed);
                    gamma.note_local_narrow(*b, cur);
                    saved_bindings.push((*b, cur));
                }
            }
            _ => {
                if gamma.refined_nonnull.insert((*root, fields.clone())) {
                    added_paths.push((*root, fields.clone()));
                }
            }
        }
    }

    let result = infer_expr_used(c, gamma, branch, used);

    // Restore the outermost type last when a path was narrowed more than once.
    for (b, old) in saved_bindings.into_iter().rev() {
        gamma.restore_local_narrow(b, old, refinement_epoch);
    }

    for p in added_paths {
        gamma.refined_nonnull.remove(&p);
    }

    result
}

pub(crate) fn stmt_guard_narrows(gamma: &Gamma, c: BodyCtx, stmt: Idx<Statement>) -> Vec<Narrow> {
    let Statement::Expr(e) = c.body.statement_arena[stmt] else {
        return vec![];
    };

    let Expr::If {
        cond,
        then_branch,
        else_branch,
    } = c.body.expr_arena[e]
    else {
        return vec![];
    };

    let (then_narrows, else_narrows) = detect_null_narrow(gamma, c, cond);

    match else_branch {
        None => {
            if expr_diverges(c, then_branch) {
                else_narrows
            } else {
                vec![]
            }
        }

        Some(eb) => {
            if expr_diverges(c, eb) && !expr_diverges(c, then_branch) {
                then_narrows
            } else {
                vec![]
            }
        }
    }
}

fn expr_diverges(c: BodyCtx, e: Idx<Expr>) -> bool {
    match c.body.expr_arena[e] {
        Expr::Block { statements, tail } => tail.is_none() && block_ends_in_exit(c, statements),
        _ => false,
    }
}
