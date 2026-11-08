//! **Which expressions belong to one HIR expression's subtree.**
//!
//! A struct's `BodyOwner::StructDefaults` body holds every field's default
//! initializer in one arena, and a struct literal lowers only the defaults of
//! the fields it OMITS. Attributing that body's calls per field therefore needs
//! the subtree partition, and there is no child walker in bone to borrow.
//!
//! The walk is TOTAL over `Expr`, `Statement`, `Pattern` and `MatchArm` by
//! construction -- every `match` is exhaustive with no wildcard arm, so a new
//! HIR variant is a compile error here rather than a silently missing child.
//! [`covers_body`] is the runtime half: the union of a defaults body's field
//! subtrees must be its whole arena, and the census reports any residue.

use bone_core::hir::{
    BindingPattern, Body, Callee, Expr, MatchArm, Pattern, Statement, VariantArgs,
};
use bone_core::prelude::{FxHashSet, Idx};

/// Every expression in `root`'s subtree, `root` included.
pub fn expr_subtree(body: &Body, root: Idx<Expr>) -> FxHashSet<Idx<Expr>> {
    let mut out = FxHashSet::default();
    walk_expr(body, root, &mut out);
    out
}

fn walk_expr(body: &Body, e: Idx<Expr>, out: &mut FxHashSet<Idx<Expr>>) {
    if !out.insert(e) {
        return;
    }
    match body.expr_arena[e] {
        Expr::Unit
        | Expr::Null
        | Expr::Hole(_)
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::Bool(_)
        | Expr::String(_)
        | Expr::Var(_)
        | Expr::Break
        | Expr::Continue
        | Expr::Global(_)
        | Expr::SiblingRef(_) => {}

        Expr::Tuple(items) | Expr::Array(items) => {
            for i in items {
                walk_expr(body, i, out);
            }
        }
        Expr::InterpolatedString(parts) => {
            for p in parts {
                if let bone_core::hir::InterpPart::Value { expr, spec } = &body.interp_part_arena[p]
                {
                    walk_expr(body, *expr, out);
                    if let Some(spec) = spec {
                        for sub in [spec.width_expr, spec.precision_expr].into_iter().flatten() {
                            walk_expr(body, sub, out);
                        }
                    }
                }
            }
        }
        Expr::Not(v, _)
        | Expr::BitNot(v)
        | Expr::UnaryMinus(v, _)
        | Expr::YieldDelegate(v)
        | Expr::Spread(v)
        | Expr::IncDec { target: v, .. }
        | Expr::As { value: v, .. }
        | Expr::SafeNav { base: v, .. }
        | Expr::TupleIndex { tuple: v, .. }
        | Expr::FieldAccess { base: v, .. }
        | Expr::Lambda { body: v, .. } => walk_expr(body, v, out),

        Expr::Return(v) | Expr::Yield(v) => {
            if let Some(v) = v {
                walk_expr(body, v, out);
            }
        }
        Expr::Index { base: a, index: b }
        | Expr::BinOp { lhs: a, rhs: b, .. }
        | Expr::Range { low: a, high: b }
        | Expr::Assign {
            target: a,
            value: b,
        }
        | Expr::CompoundAssign {
            target: a,
            value: b,
            ..
        } => {
            walk_expr(body, a, out);
            walk_expr(body, b, out);
        }
        Expr::SafeNavLet {
            receiver, body: b, ..
        } => {
            walk_expr(body, receiver, out);
            walk_expr(body, b, out);
        }
        Expr::SafeNavCall { receiver, args, .. } => {
            walk_expr(body, receiver, out);
            for a in args {
                walk_expr(body, a, out);
            }
        }
        Expr::Block { statements, tail } => {
            for s in statements {
                walk_statement(body, s, out);
            }
            if let Some(t) = tail {
                walk_expr(body, t, out);
            }
        }
        Expr::If {
            cond,
            then_branch,
            else_branch,
        } => {
            walk_expr(body, cond, out);
            walk_expr(body, then_branch, out);
            if let Some(e) = else_branch {
                walk_expr(body, e, out);
            }
        }
        Expr::IfLet {
            pattern,
            value,
            then_branch,
            else_branch,
        } => {
            walk_pattern(body, pattern, out);
            walk_expr(body, value, out);
            walk_expr(body, then_branch, out);
            if let Some(e) = else_branch {
                walk_expr(body, e, out);
            }
        }
        Expr::Call { callee, args, .. } => {
            if let Callee::Value(v) = callee {
                walk_expr(body, v, out);
            }
            for a in args {
                walk_expr(body, a, out);
            }
        }
        Expr::StructLiteral { fields, bases, .. } => {
            for b in bases {
                walk_expr(body, b, out);
            }
            for f in fields {
                walk_expr(body, body.struct_lit_field_arena[f].value, out);
            }
        }
        Expr::EnumVariant { args, .. } => match args {
            VariantArgs::Unit => {}
            VariantArgs::Tuple(items) => {
                for i in items {
                    walk_expr(body, i, out);
                }
            }
            VariantArgs::Record(fields) => {
                for f in fields {
                    walk_expr(body, body.struct_lit_field_arena[f].value, out);
                }
            }
        },
        Expr::Match { scrutinee, arms } => {
            walk_expr(body, scrutinee, out);
            for a in arms {
                let MatchArm {
                    pattern,
                    body: b,
                    guard,
                } = body.match_arm_arena[a];
                walk_pattern(body, pattern, out);
                walk_expr(body, b, out);
                if let Some(g) = guard {
                    walk_expr(body, g, out);
                }
            }
        }
    }
}

fn walk_statement(body: &Body, s: Idx<Statement>, out: &mut FxHashSet<Idx<Expr>>) {
    match body.statement_arena[s] {
        Statement::Let { value, .. }
        | Statement::LetNewtype { value, .. }
        | Statement::Static { value, .. }
        | Statement::Expr(value)
        | Statement::Defer(value) => walk_expr(body, value, out),
        Statement::LetTuple { ref vars, value } => {
            let _: &BindingPattern = vars;
            walk_expr(body, value, out);
        }
        Statement::Fn { body: b, .. } => walk_expr(body, b, out),
        Statement::Return(v) => {
            if let Some(v) = v {
                walk_expr(body, v, out);
            }
        }
        Statement::For { iter, body: b, .. } => {
            walk_expr(body, iter, out);
            walk_expr(body, b, out);
        }
        Statement::While { cond, body: b } => {
            walk_expr(body, cond, out);
            walk_expr(body, b, out);
        }
        Statement::WhileLet {
            pattern,
            value,
            body: b,
        } => {
            walk_pattern(body, pattern, out);
            walk_expr(body, value, out);
            walk_expr(body, b, out);
        }
        Statement::Comment(_) | Statement::Break | Statement::Continue => {}
    }
}

fn walk_pattern(body: &Body, p: Idx<Pattern>, out: &mut FxHashSet<Idx<Expr>>) {
    match body.pattern_arena[p] {
        Pattern::Wildcard | Pattern::Bind(_) => {}
        Pattern::Literal(e) => walk_expr(body, e, out),
        Pattern::Range { start, end, .. } => {
            for e in [start, end].into_iter().flatten() {
                walk_expr(body, e, out);
            }
        }
        Pattern::EnumVariant { sub, .. } => {
            for s in sub {
                walk_pattern(body, s, out);
            }
        }
        Pattern::Record { fields, .. } => {
            for f in fields {
                walk_pattern(body, body.record_pat_field_arena[f].pat, out);
            }
        }
        Pattern::Tuple { elements } => {
            for e in elements {
                walk_pattern(body, e, out);
            }
        }
        Pattern::Or { alternatives } => {
            for a in alternatives {
                walk_pattern(body, a, out);
            }
        }
    }
}

/// Expressions of a struct-defaults `body` that NO field's default subtree
/// reaches.
///
/// A defaults body holds nothing but field initializers, so this is 0 by
/// construction -- and it is measured rather than assumed, because a missed
/// child link would show up here and nowhere else: the walk would simply
/// attribute fewer calls and every count would still balance.
pub fn defaults_body_residue(body: &Body) -> usize {
    let mut covered = FxHashSet::default();
    for root in body.field_defaults.values() {
        walk_expr(body, *root, &mut covered);
    }
    body.expr_arena
        .iter()
        .filter(|(e, _)| !covered.contains(e))
        .count()
}
