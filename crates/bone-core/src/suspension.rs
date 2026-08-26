use crate::hir::{
    BinOp, BindingInfo, BindingKind, Callee, Expr, InterpPart, Pattern, Statement, VariantArgs,
};
use crate::prelude::*;

pub trait SuspensionPolicy {
    fn binding_holds_frame_data(&self, binding: Idx<BindingInfo>, value: Option<Idx<Expr>>) -> bool;
}

pub fn uses_after_suspension(
    body: &Body,
    policy: &impl SuspensionPolicy,
) -> Vec<Idx<Expr>> {
    let Some(root) = body.root else { return Vec::new() };
    if !body
        .expr_arena
        .iter()
        .any(|(_, e)| matches!(e, Expr::Yield(_) | Expr::YieldDelegate(_)))
    {
        return Vec::new();
    }

    let mut flow = Flow::default();
    for (binding, info) in body.binding_arena.iter() {
        if info.kind == BindingKind::Param && policy.binding_holds_frame_data(binding, None) {
            flow.live.insert(binding);
        }
    }
    let mut walk = Walk {
        body,
        policy,
        reported: FxHashSet::default(),
    };
    walk.expr(root, &mut flow);
    let mut found: Vec<_> = walk.reported.into_iter().collect();
    found.sort_by_key(|e| body.expr_spans[*e].offset_range);
    found
}

#[derive(Clone, Default)]
struct Flow {
    live: FxHashSet<Idx<BindingInfo>>,
    crossed: FxHashSet<Idx<BindingInfo>>,
}

impl Flow {
    fn suspend(&mut self) {
        self.crossed.extend(self.live.drain());
    }

    fn merge(&mut self, other: Flow) {
        self.live.extend(other.live);
        self.crossed.extend(other.crossed);
    }

    fn size(&self) -> usize {
        self.live.len() + self.crossed.len()
    }

    fn rebind(&mut self, binding: Idx<BindingInfo>, live: bool) {
        self.live.remove(&binding);
        self.crossed.remove(&binding);
        if live {
            self.live.insert(binding);
        }
    }
}

struct Walk<'a, P> {
    body: &'a Body,
    policy: &'a P,
    reported: FxHashSet<Idx<Expr>>,
}

impl<P: SuspensionPolicy> Walk<'_, P> {
    fn bind(&self, binding: Idx<BindingInfo>, value: Option<Idx<Expr>>, flow: &mut Flow) {
        flow.rebind(binding, self.policy.binding_holds_frame_data(binding, value));
    }

    fn branch(&mut self, expr: Option<Idx<Expr>>, entry: &Flow, out: &mut Flow) {
        let Some(expr) = expr else { return };
        let mut branch = entry.clone();
        self.expr(expr, &mut branch);
        out.merge(branch);
    }

    fn loop_body(&mut self, body: Idx<Expr>, flow: &mut Flow) {
        let before = flow.size();
        let mut pass = flow.clone();
        self.expr(body, &mut pass);
        flow.merge(pass);
        if flow.size() == before {
            return;
        }
        let mut second = flow.clone();
        self.expr(body, &mut second);
        flow.merge(second);
    }

    fn stmt(&mut self, stmt: Idx<Statement>, flow: &mut Flow) {
        match &self.body.statement_arena[stmt] {
            Statement::Let { name, value } => {
                self.expr(*value, flow);
                self.bind(*name, Some(*value), flow);
            }
            Statement::LetNewtype { var, value, .. } => {
                self.expr(*value, flow);
                self.bind(*var, Some(*value), flow);
            }
            Statement::LetTuple { vars, value } => {
                self.expr(*value, flow);
                for binding in vars.leaves() {
                    self.bind(binding, None, flow);
                }
            }
            Statement::Static { name, value } => {
                self.expr(*value, flow);
                flow.rebind(*name, false);
            }
            Statement::Fn { .. } => {}
            Statement::Return(value) => {
                if let Some(value) = value {
                    self.expr(*value, flow);
                }
            }
            Statement::Expr(expr) => self.expr(*expr, flow),
            Statement::Defer(_) => {}
            Statement::For { vars, iter, body } => {
                self.expr(*iter, flow);
                for binding in vars.leaves() {
                    self.bind(binding, None, flow);
                }
                self.loop_body(*body, flow);
            }
            Statement::While { cond, body } => {
                self.expr(*cond, flow);
                self.loop_body(*body, flow);
                self.expr(*cond, flow);
            }
            Statement::WhileLet { pattern, value, body } => {
                self.expr(*value, flow);
                if let Pattern::Bind(binding) = self.body.pattern_arena[*pattern] {
                    self.bind(binding, Some(*value), flow);
                }
                self.loop_body(*body, flow);
            }
            Statement::Comment(_) | Statement::Break | Statement::Continue => {}
        }
    }

    fn expr(&mut self, expr: Idx<Expr>, flow: &mut Flow) {
        match self.body.expr_arena[expr] {
            Expr::Var(binding) => {
                if flow.crossed.contains(&binding) {
                    self.reported.insert(expr);
                }
            }
            Expr::Yield(value) => {
                if let Some(value) = value {
                    self.expr(value, flow);
                }
                flow.suspend();
            }
            Expr::YieldDelegate(value) => {
                self.expr(value, flow);
                flow.suspend();
            }
            Expr::Lambda { .. } => {}
            Expr::Block { statements, tail } => {
                for stmt in statements {
                    self.stmt(stmt, flow);
                }
                if let Some(tail) = tail {
                    self.expr(tail, flow);
                }
                self.run_defers(statements, flow);
            }
            Expr::If { cond, then_branch, else_branch } => {
                self.expr(cond, flow);
                let entry = flow.clone();
                let mut out = Flow::default();
                self.branch(Some(then_branch), &entry, &mut out);
                match else_branch {
                    Some(_) => self.branch(else_branch, &entry, &mut out),
                    None => out.merge(entry),
                }
                *flow = out;
            }
            Expr::IfLet { pattern, value, then_branch, else_branch } => {
                self.expr(value, flow);
                let mut entry = flow.clone();
                if let Pattern::Bind(binding) = self.body.pattern_arena[pattern] {
                    self.bind(binding, Some(value), &mut entry);
                }
                let mut out = Flow::default();
                self.branch(Some(then_branch), &entry, &mut out);
                match else_branch {
                    Some(_) => self.branch(else_branch, flow, &mut out),
                    None => out.merge(flow.clone()),
                }
                *flow = out;
            }
            Expr::Match { scrutinee, arms } => {
                self.expr(scrutinee, flow);
                let entry = flow.clone();
                let mut out = Flow::default();
                for arm in arms {
                    let arm = self.body.match_arm_arena[arm];
                    let mut branch = entry.clone();
                    if let Some(guard) = arm.guard {
                        self.expr(guard, &mut branch);
                    }
                    if let Pattern::Bind(binding) = self.body.pattern_arena[arm.pattern] {
                        self.bind(binding, Some(scrutinee), &mut branch);
                    }
                    self.expr(arm.body, &mut branch);
                    out.merge(branch);
                }
                *flow = out;
            }
            Expr::Assign { target, value } => {
                self.expr(value, flow);
                match self.body.expr_arena[target] {
                    Expr::Var(binding) => self.bind(binding, Some(value), flow),
                    _ => self.expr(target, flow),
                }
            }
            Expr::CompoundAssign { target, value, .. } => {
                self.expr(value, flow);
                self.expr(target, flow);
            }
            Expr::Call { callee, args, .. } => {
                if let Callee::Value(value) = callee {
                    self.expr(value, flow);
                }
                if let Callee::Local(binding) = callee {
                    if flow.crossed.contains(&binding) {
                        self.reported.insert(expr);
                    }
                }
                for arg in args {
                    self.expr(arg, flow);
                }
            }
            Expr::SafeNavCall { receiver, args, .. } => {
                self.expr(receiver, flow);
                for arg in args {
                    self.expr(arg, flow);
                }
            }
            Expr::SafeNavLet { receiver, binding, body } => {
                self.expr(receiver, flow);
                self.bind(binding, Some(receiver), flow);
                self.expr(body, flow);
            }
            Expr::BinOp { op: BinOp::NullCoalesce, lhs, rhs } => {
                self.expr(lhs, flow);
                let entry = flow.clone();
                let mut out = Flow::default();
                self.branch(Some(rhs), &entry, &mut out);
                out.merge(entry);
                *flow = out;
            }
            Expr::BinOp { lhs, rhs, .. } | Expr::Range { low: lhs, high: rhs } => {
                self.expr(lhs, flow);
                self.expr(rhs, flow);
            }
            Expr::Not(value)
            | Expr::BitNot(value)
            | Expr::UnaryMinus(value)
            | Expr::Spread(value)
            | Expr::As { value, .. }
            | Expr::FieldAccess { base: value, .. }
            | Expr::SafeNav { base: value, .. }
            | Expr::TupleIndex { tuple: value, .. }
            | Expr::IncDec { target: value, .. } => self.expr(value, flow),
            Expr::Index { base, index } => {
                self.expr(base, flow);
                self.expr(index, flow);
            }
            Expr::Tuple(values) | Expr::Array(values) => {
                for value in values {
                    self.expr(value, flow);
                }
            }
            Expr::StructLiteral { fields, bases, .. } => {
                for base in bases {
                    self.expr(base, flow);
                }
                for field in fields {
                    self.expr(self.body.struct_lit_field_arena[field].value, flow);
                }
            }
            Expr::EnumVariant { args, .. } => match args {
                VariantArgs::Unit => {}
                VariantArgs::Tuple(values) => {
                    for value in values {
                        self.expr(value, flow);
                    }
                }
                VariantArgs::Record(fields) => {
                    for field in fields {
                        self.expr(self.body.struct_lit_field_arena[field].value, flow);
                    }
                }
            },
            Expr::InterpolatedString(parts) => {
                for part in parts {
                    if let InterpPart::Value { expr, spec } = self.body.interp_part_arena[part] {
                        self.expr(expr, flow);
                        if let Some(value) = spec.and_then(|s| s.precision_expr) {
                            self.expr(value, flow);
                        }
                    }
                }
            }
            Expr::Return(value) => {
                if let Some(value) = value {
                    self.expr(value, flow);
                }
            }
            Expr::Unit
            | Expr::Null
            | Expr::Hole(_)
            | Expr::Int(_)
            | Expr::Float(_)
            | Expr::Bool(_)
            | Expr::String(_)
            | Expr::Global(_)
            | Expr::SiblingRef(_)
            | Expr::Break
            | Expr::Continue => {}
        }
    }

    fn run_defers(&mut self, statements: IdxRange<Statement>, flow: &mut Flow) {
        for stmt in statements {
            if let Statement::Defer(expr) = self.body.statement_arena[stmt] {
                let mut deferred = flow.clone();
                self.expr(expr, &mut deferred);
            }
        }
    }
}
