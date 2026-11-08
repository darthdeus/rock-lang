//! The one enumeration of a HIR node's direct children.
//!
//! Every match here is exhaustive with no wildcard arm, so a new `Expr`,
//! `Statement` or `Pattern` variant fails to compile here instead of being
//! silently skipped by the analyses that walk a body. A walker that must not
//! descend into some child (a lambda body, a nested fn, a pattern) matches
//! that node itself before delegating the rest here.
//!
//! Children come in evaluation order: an `if let` yields its value before its
//! pattern, and a match arm yields its pattern, then its guard, then its body.

use std::convert::Infallible;

use crate::hir::{Body, Callee, Expr, InterpPart, Pattern, Statement, VariantArgs};
use crate::prelude::Idx;

/// A direct child of a HIR node.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum HirChild {
    Expr(Idx<Expr>),
    Statement(Idx<Statement>),
    Pattern(Idx<Pattern>),
}

impl Body {
    /// Calls `f` on each direct child of `node`.
    pub fn for_each_child(&self, node: HirChild, mut f: impl FnMut(HirChild)) {
        let _ = self.try_for_each_child(node, |c| -> Result<(), Infallible> {
            f(c);
            Ok(())
        });
    }

    /// Calls `f` on each direct child of `node`, stopping at the first error.
    pub fn try_for_each_child<E>(
        &self,
        node: HirChild,
        f: impl FnMut(HirChild) -> Result<(), E>,
    ) -> Result<(), E> {
        match node {
            HirChild::Expr(e) => self.try_for_each_expr_child(e, f),
            HirChild::Statement(s) => self.try_for_each_statement_child(s, f),
            HirChild::Pattern(p) => self.try_for_each_pattern_child(p, f),
        }
    }

    /// Calls `f` on every descendant of `node`, depth-first, parents before
    /// children. `f` returns whether to descend into the node it was given.
    pub fn walk_descendants(&self, node: HirChild, f: &mut impl FnMut(HirChild) -> bool) {
        self.for_each_child(node, |child| {
            if f(child) {
                self.walk_descendants(child, f);
            }
        });
    }

    /// Calls `f` on each direct child of `e`, stopping at the first error.
    fn try_for_each_expr_child<E>(
        &self,
        e: Idx<Expr>,
        mut f: impl FnMut(HirChild) -> Result<(), E>,
    ) -> Result<(), E> {
        let mut ex = |x: Idx<Expr>| f(HirChild::Expr(x));
        match self.expr_arena[e] {
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
            | Expr::SiblingRef(_)
            | Expr::Return(None)
            | Expr::Yield(None) => {}

            Expr::Tuple(items) | Expr::Array(items) => {
                for i in items {
                    ex(i)?;
                }
            }
            Expr::InterpolatedString(parts) => {
                for p in parts {
                    if let InterpPart::Value { expr, spec } = self.interp_part_arena[p] {
                        ex(expr)?;
                        if let Some(spec) = spec {
                            for sub in [spec.width_expr, spec.precision_expr].into_iter().flatten()
                            {
                                ex(sub)?;
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
            | Expr::Return(Some(v))
            | Expr::Yield(Some(v))
            | Expr::IncDec { target: v, .. }
            | Expr::As { value: v, .. }
            | Expr::SafeNav { base: v, .. }
            | Expr::TupleIndex { tuple: v, .. }
            | Expr::FieldAccess { base: v, .. }
            | Expr::Lambda { body: v, .. } => ex(v)?,

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
            }
            | Expr::SafeNavLet {
                receiver: a,
                body: b,
                ..
            } => {
                ex(a)?;
                ex(b)?;
            }
            Expr::SafeNavCall { receiver, args, .. } => {
                ex(receiver)?;
                for a in args {
                    ex(a)?;
                }
            }
            Expr::Block { statements, tail } => {
                for s in statements {
                    f(HirChild::Statement(s))?;
                }
                if let Some(t) = tail {
                    f(HirChild::Expr(t))?;
                }
            }
            Expr::If {
                cond,
                then_branch,
                else_branch,
            } => {
                ex(cond)?;
                ex(then_branch)?;
                if let Some(x) = else_branch {
                    ex(x)?;
                }
            }
            Expr::IfLet {
                pattern,
                value,
                then_branch,
                else_branch,
            } => {
                f(HirChild::Expr(value))?;
                f(HirChild::Pattern(pattern))?;
                f(HirChild::Expr(then_branch))?;
                if let Some(x) = else_branch {
                    f(HirChild::Expr(x))?;
                }
            }
            Expr::Call { callee, args, .. } => {
                match callee {
                    Callee::Value(v) => ex(v)?,
                    Callee::Unresolved { .. } | Callee::Local(_) => {}
                }
                for a in args {
                    ex(a)?;
                }
            }
            Expr::StructLiteral { fields, bases, .. } => {
                for b in bases {
                    ex(b)?;
                }
                for fld in fields {
                    ex(self.struct_lit_field_arena[fld].value)?;
                }
            }
            Expr::EnumVariant { args, .. } => match args {
                VariantArgs::Unit => {}
                VariantArgs::Tuple(items) => {
                    for i in items {
                        ex(i)?;
                    }
                }
                VariantArgs::Record(fields) => {
                    for fld in fields {
                        ex(self.struct_lit_field_arena[fld].value)?;
                    }
                }
            },
            Expr::Match { scrutinee, arms } => {
                f(HirChild::Expr(scrutinee))?;
                for a in arms {
                    let arm = self.match_arm_arena[a];
                    f(HirChild::Pattern(arm.pattern))?;
                    if let Some(g) = arm.guard {
                        f(HirChild::Expr(g))?;
                    }
                    f(HirChild::Expr(arm.body))?;
                }
            }
        }
        Ok(())
    }

    /// Calls `f` on each direct child of `s`, stopping at the first error.
    fn try_for_each_statement_child<E>(
        &self,
        s: Idx<Statement>,
        mut f: impl FnMut(HirChild) -> Result<(), E>,
    ) -> Result<(), E> {
        let mut ex = |x: Idx<Expr>| f(HirChild::Expr(x));
        match self.statement_arena[s] {
            Statement::Let { value, .. }
            | Statement::LetTuple { value, .. }
            | Statement::LetNewtype { value, .. }
            | Statement::Static { value, .. }
            | Statement::Expr(value)
            | Statement::Defer(value)
            | Statement::Return(Some(value))
            | Statement::Fn { body: value, .. } => ex(value)?,
            Statement::For {
                iter: a, body: b, ..
            }
            | Statement::While { cond: a, body: b } => {
                ex(a)?;
                ex(b)?;
            }
            Statement::WhileLet {
                pattern,
                value,
                body,
            } => {
                f(HirChild::Expr(value))?;
                f(HirChild::Pattern(pattern))?;
                f(HirChild::Expr(body))?;
            }
            Statement::Return(None)
            | Statement::Comment(_)
            | Statement::Break
            | Statement::Continue => {}
        }
        Ok(())
    }

    /// Calls `f` on each direct child of `p`, stopping at the first error.
    fn try_for_each_pattern_child<E>(
        &self,
        p: Idx<Pattern>,
        mut f: impl FnMut(HirChild) -> Result<(), E>,
    ) -> Result<(), E> {
        match self.pattern_arena[p] {
            Pattern::Wildcard | Pattern::Bind(_) => {}
            Pattern::Literal(e) => f(HirChild::Expr(e))?,
            Pattern::Range { start, end, .. } => {
                for e in [start, end].into_iter().flatten() {
                    f(HirChild::Expr(e))?;
                }
            }
            Pattern::EnumVariant { sub: pats, .. }
            | Pattern::Tuple { elements: pats }
            | Pattern::Or { alternatives: pats } => {
                for s in pats {
                    f(HirChild::Pattern(s))?;
                }
            }
            Pattern::Record { fields, .. } => {
                for fld in fields {
                    f(HirChild::Pattern(self.record_pat_field_arena[fld].pat))?;
                }
            }
        }
        Ok(())
    }
}
