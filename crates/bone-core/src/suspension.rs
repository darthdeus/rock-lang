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
    suspension_crossings(body, policy).into_iter().map(|crossing| crossing.at).collect()
}

/// A use of frame data after a suspension, with the last suspension before
/// it on each path that reaches it (`rock fix --across-yield` rebinds after
/// each of them).
pub struct Crossing {
    pub at: Idx<Expr>,
    pub suspensions: Vec<Idx<Expr>>,
}

pub fn suspension_crossings(body: &Body, policy: &impl SuspensionPolicy) -> Vec<Crossing> {
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
        reported: FxHashMap::default(),
        loops: Vec::new(),
        unwound: Flow::default(),
    };
    walk.expr(root, &mut flow);
    let mut found: Vec<_> = walk
        .reported
        .into_iter()
        .map(|(at, suspensions)| {
            let mut suspensions: Vec<_> = suspensions.into_iter().collect();
            suspensions.sort_by_key(|e| body.expr_spans[*e].offset_range);
            Crossing { at, suspensions }
        })
        .collect();
    found.sort_by_key(|c| body.expr_spans[c.at].offset_range);
    found
}

#[derive(Clone, Default)]
struct Flow {
    live: FxHashSet<Idx<BindingInfo>>,
    /// Each crossed binding with the last suspension it was held across on
    /// each path merged here.
    crossed: FxHashMap<Idx<BindingInfo>, FxHashSet<Idx<Expr>>>,
}

impl Flow {
    /// A suspension becomes the last one on this path for every binding it
    /// holds, live or already crossed.
    fn suspend(&mut self, at: Idx<Expr>) {
        for suspensions in self.crossed.values_mut() {
            suspensions.clear();
            suspensions.insert(at);
        }
        for binding in self.live.drain() {
            self.crossed.insert(binding, FxHashSet::from_iter([at]));
        }
    }

    fn merge(&mut self, other: Flow) {
        self.live.extend(other.live);
        for (binding, suspensions) in other.crossed {
            self.crossed.entry(binding).or_default().extend(suspensions);
        }
    }

    fn size(&self) -> usize {
        self.live.len() + self.crossed.values().map(|s| s.len()).sum::<usize>()
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
    reported: FxHashMap<Idx<Expr>, FxHashSet<Idx<Expr>>>,
    /// Flows leaving each enclosing loop by `break` and by `continue`.
    loops: Vec<LoopExits>,
    /// Flows that left a block early; its defers still run on them.
    unwound: Flow,
}

#[derive(Default)]
struct LoopExits {
    breaks: Flow,
    continues: Flow,
}

enum Exit {
    Break,
    Continue,
    Return,
}

impl<P: SuspensionPolicy> Walk<'_, P> {
    fn report(&mut self, at: Idx<Expr>, binding: Idx<BindingInfo>, flow: &Flow) {
        if let Some(suspensions) = flow.crossed.get(&binding) {
            self.reported.entry(at).or_default().extend(suspensions.iter().copied());
        }
    }

    fn bind(&self, binding: Idx<BindingInfo>, value: Option<Idx<Expr>>, flow: &mut Flow) {
        flow.rebind(binding, self.policy.binding_holds_frame_data(binding, value));
    }

    fn branch(&mut self, expr: Option<Idx<Expr>>, entry: &Flow, out: &mut Flow) {
        let Some(expr) = expr else { return };
        let mut branch = entry.clone();
        self.expr(expr, &mut branch);
        out.merge(branch);
    }

    /// Walks a loop until the flow at its head stops growing. `cond` runs
    /// before every iteration and may leave the loop; `vars` are bound fresh
    /// for every iteration.
    fn loop_body(
        &mut self,
        cond: Option<Idx<Expr>>,
        vars: &[(Idx<BindingInfo>, Option<Idx<Expr>>)],
        body: Idx<Expr>,
        flow: &mut Flow,
    ) {
        let mut head = flow.clone();
        let mut exit = Flow::default();
        loop {
            let mut pass = head.clone();
            if let Some(cond) = cond {
                self.expr(cond, &mut pass);
            }
            exit.merge(pass.clone());
            for &(binding, value) in vars {
                self.bind(binding, value, &mut pass);
            }
            self.loops.push(LoopExits::default());
            self.expr(body, &mut pass);
            let exits = self.loops.pop().expect("pushed above");
            pass.merge(exits.continues);
            exit.merge(exits.breaks);
            let before = head.size();
            head.merge(pass);
            if head.size() == before {
                break;
            }
        }
        *flow = exit;
    }

    /// Ends the current path: nothing after a `break`, `continue` or
    /// `return` runs on it.
    fn exit(&mut self, exit: Exit, flow: &mut Flow) {
        let taken = std::mem::take(flow);
        self.unwound.merge(taken.clone());
        let Some(exits) = self.loops.last_mut() else { return };
        match exit {
            Exit::Break => exits.breaks.merge(taken),
            Exit::Continue => exits.continues.merge(taken),
            Exit::Return => {}
        }
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
                self.exit(Exit::Return, flow);
            }
            Statement::Expr(expr) => self.expr(*expr, flow),
            Statement::Defer(_) => {}
            Statement::For { vars, iter, body } => {
                self.expr(*iter, flow);
                let vars: Vec<_> = vars.leaves().into_iter().map(|binding| (binding, None)).collect();
                self.loop_body(None, &vars, *body, flow);
            }
            Statement::While { cond, body } => self.loop_body(Some(*cond), &[], *body, flow),
            Statement::WhileLet { pattern, value, body } => {
                let vars: &[_] = match self.body.pattern_arena[*pattern] {
                    Pattern::Bind(binding) => &[(binding, Some(*value))],
                    _ => &[],
                };
                self.loop_body(Some(*value), vars, *body, flow);
            }
            Statement::Break => self.exit(Exit::Break, flow),
            Statement::Continue => self.exit(Exit::Continue, flow),
            Statement::Comment(_) => {}
        }
    }

    fn expr(&mut self, expr: Idx<Expr>, flow: &mut Flow) {
        match self.body.expr_arena[expr] {
            Expr::Var(binding) => self.report(expr, binding, flow),
            Expr::Yield(value) => {
                if let Some(value) = value {
                    self.expr(value, flow);
                }
                flow.suspend(expr);
            }
            Expr::YieldDelegate(value) => {
                self.expr(value, flow);
                flow.suspend(expr);
            }
            Expr::Lambda { .. } => {}
            Expr::Block { statements, tail } => {
                let outer = std::mem::take(&mut self.unwound);
                for stmt in statements {
                    self.stmt(stmt, flow);
                }
                if let Some(tail) = tail {
                    self.expr(tail, flow);
                }
                self.run_defers(statements, flow);
                self.unwound.merge(outer);
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
                    self.report(expr, binding, flow);
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
            Expr::Not(value, _)
            | Expr::BitNot(value)
            | Expr::UnaryMinus(value, _)
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
                self.exit(Exit::Return, flow);
            }
            Expr::Break => self.exit(Exit::Break, flow),
            Expr::Continue => self.exit(Exit::Continue, flow),
            Expr::Unit
            | Expr::Null
            | Expr::Hole(_)
            | Expr::Int(_)
            | Expr::Float(_)
            | Expr::Bool(_)
            | Expr::String(_)
            | Expr::Global(_)
            | Expr::SiblingRef(_) => {}
        }
    }

    fn run_defers(&mut self, statements: IdxRange<Statement>, flow: &Flow) {
        for stmt in statements {
            if let Statement::Defer(expr) = self.body.statement_arena[stmt] {
                let mut deferred = flow.clone();
                deferred.merge(self.unwound.clone());
                self.expr(expr, &mut deferred);
            }
        }
    }
}
