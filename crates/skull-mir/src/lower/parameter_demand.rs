//! Conservative field demand before parameter expansion. Unsupported operations
//! retain the declared transport; this is not an effect or alias analysis.
use std::collections::HashSet;
use bone_semantic::hir::{Body, Expr, Pattern, Statement, VariantArgs};
use comfy_la_arena::Idx;
use ustr::Ustr;

pub(super) fn fields(body: &Body) -> Option<Vec<Option<HashSet<Ustr>>>> {
    let params: Vec<_> = body.param_bindings.into_iter().collect();
    let mut demand = vec![Some(HashSet::new()); params.len()];
    fn pattern(body: &Body, p: Idx<Pattern>, params: &[Idx<bone_semantic::hir::BindingInfo>], demand: &mut [Option<HashSet<Ustr>>]) -> Option<()> {
        match body.pattern_arena[p] {
            Pattern::Wildcard | Pattern::Bind(_) => {},
            Pattern::Literal(e) => visit(body, e, params, demand)?,
            Pattern::Range { start, end, .. } => {
                if let Some(e) = start { visit(body, e, params, demand)?; }
                if let Some(e) = end { visit(body, e, params, demand)?; }
            }
            Pattern::EnumVariant { sub, .. } => {
                for p in sub { pattern(body, p, params, demand)?; }
            }
            Pattern::Record { fields, .. } => {
                for f in fields { pattern(body, body.record_pat_field_arena[f].pat, params, demand)?; }
            }
            Pattern::Tuple { elements } => {
                for p in elements { pattern(body, p, params, demand)?; }
            }
            Pattern::Or { alternatives } => {
                for p in alternatives { pattern(body, p, params, demand)?; }
            }
        }
        Some(())
    }
    fn visit(body: &Body, e: Idx<Expr>, params: &[Idx<bone_semantic::hir::BindingInfo>], demand: &mut [Option<HashSet<Ustr>>]) -> Option<()> {
        match body.expr_arena[e] {
            Expr::FieldAccess { base, field } => {
                if let Expr::Var(binding) = body.expr_arena[base]
                    && let Some(i) = params.iter().position(|p| *p == binding)
                {
                    if let Some(fields) = &mut demand[i] { fields.insert(field); }
                } else { visit(body, base, params, demand)?; }
            }
            Expr::Var(binding) => {
                if let Some(i) = params.iter().position(|p| *p == binding) { demand[i] = None; }
            }
            Expr::Return(value) => { if let Some(v) = value { visit(body, v, params, demand)?; } }
            Expr::Block { statements, tail } => {
                for s in statements {
                    match body.statement_arena[s] {
                        Statement::Let { value, .. } | Statement::Expr(value) => visit(body, value, params, demand)?,
                        Statement::Return(value) => { if let Some(v) = value { visit(body, v, params, demand)?; } }
                        Statement::Comment(_) => {},
                        _ => return None,
                    }
                }
                if let Some(v) = tail { visit(body, v, params, demand)?; }
            }
            Expr::If { cond, then_branch, else_branch } => {
                visit(body, cond, params, demand)?;
                visit(body, then_branch, params, demand)?;
                if let Some(v) = else_branch { visit(body, v, params, demand)?; }
            }
            Expr::Match { scrutinee, arms } => {
                // The selected top-level field retains its complete layout;
                // payload bindings therefore need no alias-to-lane analysis.
                visit(body, scrutinee, params, demand)?;
                for arm in arms {
                    let arm = body.match_arm_arena[arm];
                    pattern(body, arm.pattern, params, demand)?;
                    if let Some(guard) = arm.guard { visit(body, guard, params, demand)?; }
                    visit(body, arm.body, params, demand)?;
                }
            }
            Expr::BinOp { lhs, rhs, .. } => {
                visit(body, lhs, params, demand)?;
                visit(body, rhs, params, demand)?;
            }
            Expr::Not(v, _) | Expr::UnaryMinus(v, _) | Expr::BitNot(v) => visit(body, v, params, demand)?,
            Expr::EnumVariant { args: VariantArgs::Unit, .. } => {},
            Expr::Unit | Expr::Null | Expr::Int(_) | Expr::Float(_) | Expr::Bool(_) | Expr::String(_) | Expr::Global(_) => {},
            _ => return None,
        }
        Some(())
    }
    visit(body, body.root?, &params, &mut demand)?;
    Some(demand)
}
