//! Null-refinement / flow narrowing for inference.
//!
//! One home for the `if x != null { .. }` machinery: detect the null-check in a
//! condition ([`detect_null_narrow`]), apply the resulting narrowings while
//! inferring a branch ([`infer_branch`]), and the supporting helpers -- path
//! keys/prefixes, the purity-aware condition effect check that guards mutable
//! refinement ([`cond_has_effect`]), and the divergence test that lets a guard's
//! narrowing escape into the rest of a block ([`stmt_guard_narrows`]).
//!
//! Locals and globals share one refinement set (`Gamma.refined_nonnull`), keyed
//! on a [`Root`] + field path -- so `x.a` and `G.a` narrow through the same
//! mechanism. A WHOLE local (`Root::Local`, empty path) is special-cased to peel
//! its `binding_types` entry instead. Calls and yields invalidate the path set:
//! a callee or resumer can reassign globals or mutate local fields through an
//! alias. Whole-local bindings survive because callees cannot reassign them.

use bone_core::hir;
use bone_core::hir::{BinOp, BindingInfo, CallFamily, Callee, Expr, Statement};
use bone_core::prelude::*;

use crate::{fn_has_anno, infer_expr_used, peel_nullable, resolve_callee, BodyCtx, Gamma, Use};

/// The root of a refinement path: a local binding or a global (by name).
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

/// A refined non-null path: a root plus a field chain. `x` -> `(Local x, [])`,
/// `G.inner` -> `(Global G, [inner])`.
pub(crate) type Narrow = (Root, Vec<Ustr>);

/// The `(root, field-path)` a path expression refers to (`x` -> `(Local x, [])`,
/// `x.a.b` -> `(Local x, [a, b])`, `G.inner` -> `(Global G, [inner])`), or `None`
/// if it isn't rooted in a local binding or a global.
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

/// Every prefix of a path (`(r, [a, b])` -> `(r, [])`, `(r, [a])`, `(r, [a, b])`),
/// so refining `r.a.b` also marks `r` and `r.a` non-null.
fn path_prefixes((root, fields): Narrow) -> Vec<Narrow> {
    (0..=fields.len())
        .map(|i| (root, fields[..i].to_vec()))
        .collect()
}

/// `@pure`-aware effect check over a CONDITION expression: true if evaluating it
/// could mutate a path -- a non-`@pure` call, a yield, or an assignment.
/// Used to suppress mutable-path narrows from a condition whose own
/// evaluation has an effect AFTER the null-check (`g != null && impure()`):
/// `impure()` may null `g` before the body runs. Whole-local bindings are immune;
/// their fields can change through aliases.
///
/// A named call is effect-free iff every candidate it resolves to is `@pure`
/// (a `@pure` body provably writes no global -- `verify_pure_expr`); the `all`
/// is conservative for overload sets. A method / fn-value / unresolved callee is
/// opaque -> effect. A `@pure` call's ARGUMENTS are still walked (`pure_fn(
/// impure())` is an effect through the argument).
fn cond_has_effect(gamma: &Gamma, c: BodyCtx, e: Idx<Expr>) -> bool {
    match c.body.expr_arena[e] {
        Expr::Call {
            callee: Callee::Unresolved { name, .. },
            args,
            ..
        } => {
            let candidates = resolve_callee(gamma, &CallFamily::from_name(name));
            let callee_pure = !candidates.is_empty()
                && candidates.iter().all(|(mid, fid)| {
                    fn_has_anno(&gamma.workspace.module(*mid).functions[*fid], "pure")
                });
            // `is_null` is a known-pure builtin with no candidate function.
            let callee_pure = callee_pure || name.as_str() == "is_null";
            if !callee_pure {
                return true;
            }
            // Pure callee: an effect can still hide in an argument.
            args.into_iter().any(|a| cond_has_effect(gamma, c, a))
        }
        // Method / fn-value callee: opaque, can't prove purity.
        Expr::Call { .. } => true,
        Expr::Yield(_) | Expr::YieldDelegate(_) | Expr::Assign { .. } => true,
        Expr::Not(i, _) => cond_has_effect(gamma, c, i),
        Expr::BinOp { lhs, rhs, .. } => {
            cond_has_effect(gamma, c, lhs) || cond_has_effect(gamma, c, rhs)
        }
        Expr::FieldAccess { base, .. } | Expr::SafeNav { base, .. } => {
            cond_has_effect(gamma, c, base)
        }
        _ => false,
    }
}

/// Narrowings a condition implies for its then / else branches. Detects the
/// null-check shape ([`detect_null_narrow_inner`]), then drops mutable paths if
/// the condition's own evaluation could invalidate them ([`cond_has_effect`]).
pub(crate) fn detect_null_narrow(
    gamma: &Gamma,
    c: BodyCtx,
    cond: Idx<Expr>,
) -> (Vec<Narrow>, Vec<Narrow>) {
    let (mut then_set, mut else_set) = detect_null_narrow_inner(c, cond);
    if cond_has_effect(gamma, c, cond) {
        then_set.retain(|(root, fields)| !root.is_global() && fields.is_empty());
        else_set.retain(|(root, fields)| !root.is_global() && fields.is_empty());
    }
    (then_set, else_set)
}

fn detect_null_narrow_inner(c: BodyCtx, cond: Idx<Expr>) -> (Vec<Narrow>, Vec<Narrow>) {
    match c.body.expr_arena[cond] {
        Expr::Not(inner, _) => {
            let (t, e) = detect_null_narrow_inner(c, inner);
            (e, t)
        }

        Expr::BinOp {
            op: hir::BinOp::Or,
            lhs,
            rhs,
        } => {
            let (_, mut le) = detect_null_narrow_inner(c, lhs);
            let (_, re) = detect_null_narrow_inner(c, rhs);

            le.extend(re);

            (vec![], le)
        }

        // TODO: disgusting is_null check
        Expr::Call {
            callee: Callee::Unresolved { name, .. },
            args,
            ..
        } if name.as_str() == "is_null" && args.len() == 1 => match path_key(c, args.start()) {
            Some(p) => (vec![], path_prefixes(p)),
            None => (vec![], vec![]),
        },

        Expr::BinOp {
            op: hir::BinOp::And,
            lhs,
            rhs,
        } => {
            let (mut lt, _) = detect_null_narrow_inner(c, lhs);
            let (rt, _) = detect_null_narrow_inner(c, rhs);

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

/// Infer `branch` with `narrows` applied, restoring the prior state on exit. A
/// whole local peels its `binding_types` entry; every other path (local field,
/// whole global, global field) goes into `refined_nonnull`. A path cleared
/// mid-branch by an intervening effect stays cleared -- we only undo what this
/// branch itself added.
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

    for (root, fields) in narrows {
        match root {
            Root::Local(b) if fields.is_empty() => {
                if let Some(cur) = gamma.out.binding_types.get(*b).copied() {
                    let narrowed = peel_nullable(gamma, cur, span);
                    gamma.out.binding_types.insert(*b, narrowed);
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

    for (b, old) in saved_bindings {
        gamma.out.binding_types.insert(b, old);
    }

    for p in added_paths {
        gamma.refined_nonnull.remove(&p);
    }

    result
}

/// Narrowings a guard STATEMENT leaks into the rest of its block: an `if` whose
/// other path always diverges (`if x == null { return }`) narrows `x` for the
/// statements that follow, since control only reaches them on the narrowed path.
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

/// Whether a branch always exits its enclosing block (its last non-comment
/// statement is `return` / `break` / `continue` and it has no tail value).
fn expr_diverges(c: BodyCtx, e: Idx<Expr>) -> bool {
    match c.body.expr_arena[e] {
        Expr::Block { statements, tail } => {
            if tail.is_some() {
                return false;
            }

            statements
                .into_iter()
                .rfind(|&s| !matches!(c.body.statement_arena[s], Statement::Comment(_)))
                .is_some_and(|s| {
                    matches!(
                        c.body.statement_arena[s],
                        Statement::Return(_) | Statement::Break | Statement::Continue
                    )
                })
        }

        _ => false,
    }
}
