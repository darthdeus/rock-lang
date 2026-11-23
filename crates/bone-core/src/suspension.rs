use crate::hir::{BinOp, BindingInfo, Callee, Expr, Pattern, Statement};
use crate::hir_walk::HirChild;
use crate::prelude::*;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum SuspensionPoint {
    Expression(Idx<Expr>),
    IteratorNext(Idx<Expr>),
}

pub trait SuspensionPolicy {
    fn binding_holds_frame_data(&self, binding: Idx<BindingInfo>, value: Option<Idx<Expr>>)
    -> bool;

    fn is_barrier(&self, body: &Body, at: SuspensionPoint) -> bool {
        matches!(at, SuspensionPoint::Expression(expr)
            if matches!(body.expr_arena[expr], Expr::Yield(_) | Expr::YieldDelegate(_)))
    }

    fn crosses_return(&self, _value: Idx<Expr>, _at: SuspensionPoint) -> bool {
        false
    }

    fn crosses_binding(&self, _binding: Idx<BindingInfo>, _at: SuspensionPoint) -> bool {
        true
    }
}

pub fn uses_after_suspension(body: &Body, policy: &impl SuspensionPolicy) -> Vec<Idx<Expr>> {
    let Some(root) = body.root else {
        return Vec::new();
    };
    if !body
        .expr_arena
        .iter()
        .any(|(at, _)| policy.is_barrier(body, SuspensionPoint::Expression(at)))
    {
        return Vec::new();
    }

    uses_after_suspension_in(body, root, body.param_bindings, policy)
}

/// Check one function region with fresh flow. Nested function expressions and
/// declarations stay execution boundaries; callers check their regions separately.
pub fn uses_after_suspension_in(
    body: &Body,
    root: Idx<Expr>,
    params: impl IntoIterator<Item = Idx<BindingInfo>>,
    policy: &impl SuspensionPolicy,
) -> Vec<Idx<Expr>> {
    let mut flow = Flow {
        reachable: true,
        ..Flow::default()
    };
    for binding in params {
        if policy.binding_holds_frame_data(binding, None) {
            flow.live.insert(binding);
        }
    }
    let mut walk = Walk {
        body,
        policy,
        reported: FxHashSet::default(),
        loops: Vec::new(),
        root,
        defers: Vec::new(),
        defer_floor: 0,
        returning: None,
    };
    walk.expr(root, &mut flow);
    let mut found: Vec<_> = walk.reported.into_iter().collect();
    found.sort_by_key(|at| body.expr_spans[*at].offset_range());
    found
}

#[derive(Clone, Default)]
struct Flow {
    reachable: bool,
    live: FxHashSet<Idx<BindingInfo>>,
    crossed: FxHashSet<Idx<BindingInfo>>,
}

impl Flow {
    fn suspend(&mut self, crosses: impl Fn(Idx<BindingInfo>) -> bool) {
        let crossed = &mut self.crossed;
        self.live.retain(|&binding| {
            if !crosses(binding) {
                return true;
            }
            crossed.insert(binding);
            false
        });
    }

    fn merge(&mut self, other: Flow) {
        self.reachable |= other.reachable;
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
    /// Flows leaving each enclosing loop by `break` and by `continue`.
    loops: Vec<LoopExits>,
    root: Idx<Expr>,
    defers: Vec<Vec<Idx<Expr>>>,
    defer_floor: usize,
    returning: Option<Idx<Expr>>,
}

#[derive(Default)]
struct LoopExits {
    defer_depth: usize,
    breaks: Flow,
    continues: Flow,
}

enum Exit {
    Break,
    Continue,
    Return(Option<Idx<Expr>>),
}

impl<P: SuspensionPolicy> Walk<'_, P> {
    fn report(&mut self, at: Idx<Expr>, binding: Idx<BindingInfo>, flow: &Flow) {
        if flow.crossed.contains(&binding) {
            self.reported.insert(at);
        }
    }

    fn bind(&self, binding: Idx<BindingInfo>, value: Option<Idx<Expr>>, flow: &mut Flow) {
        flow.rebind(
            binding,
            self.policy.binding_holds_frame_data(binding, value),
        );
    }

    fn branch(&mut self, expr: Idx<Expr>, entry: &Flow, out: &mut Flow) {
        let mut branch = entry.clone();
        self.expr(expr, &mut branch);
        out.merge(branch);
    }

    /// Walks a loop until the flow at its head stops growing. `cond` runs
    /// before every iteration and may leave the loop; `vars` are bound fresh
    /// for every iteration.
    fn loop_body(
        &mut self,
        vars: &[Idx<BindingInfo>],
        body: Idx<Expr>,
        flow: &mut Flow,
        mut advance: impl FnMut(&mut Self, &mut Flow),
    ) {
        let mut head = flow.clone();
        let mut exit = Flow::default();
        loop {
            let mut pass = head.clone();
            advance(self, &mut pass);
            exit.merge(pass.clone());
            for &binding in vars {
                self.bind(binding, None, &mut pass);
            }
            self.loops.push(LoopExits {
                defer_depth: self.defers.len(),
                ..LoopExits::default()
            });
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
        let depth = match exit {
            Exit::Return(_) => 0,
            Exit::Break | Exit::Continue => self.loops.last().map_or(0, |l| l.defer_depth),
        };
        let returning = match exit {
            Exit::Return(value) => value,
            _ => None,
        };
        self.run_defers(depth, returning, flow);
        let taken = std::mem::take(flow);
        if let Some(exits) = self.loops.last_mut() {
            match exit {
                Exit::Break => exits.breaks.merge(taken),
                Exit::Continue => exits.continues.merge(taken),
                Exit::Return(_) => {}
            }
        }
    }

    /// A pattern's bindings entering `flow`: a root `Bind` aliases the
    /// scrutinee (one name for the same value); any richer pattern's bindings
    /// are fresh projections with no single initializer. One rule for every
    /// pattern position -- let, if let, while let, match arms.
    fn bind_pattern_flow(&mut self, pattern: Idx<Pattern>, value: Idx<Expr>, flow: &mut Flow) {
        match self.body.pattern_single_bind(pattern) {
            Some(b) => self.bind(b, Some(value), flow),
            None => {
                for binding in self.body.pattern_bindings(pattern) {
                    self.bind(binding, None, flow);
                }
            }
        }
    }

    fn stmt(&mut self, stmt: Idx<Statement>, flow: &mut Flow) {
        if !flow.reachable {
            return;
        }
        match &self.body.statement_arena[stmt] {
            Statement::Let {
                pattern,
                value,
                else_block,
            } => {
                self.expr(*value, flow);
                if let Some(e) = else_block {
                    // The else diverges: it exits on its OWN path, which must
                    // not drain the fall-through flow the bindings join.
                    let mut miss = flow.clone();
                    self.expr(*e, &mut miss);
                }
                self.bind_pattern_flow(*pattern, *value, flow);
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
                self.exit(Exit::Return(*value), flow);
            }
            Statement::Expr(expr) => self.expr(*expr, flow),
            Statement::Defer(expr) => self.defers.last_mut().expect("block").push(*expr),
            Statement::For {
                pattern,
                iter,
                body,
            } => {
                self.expr(*iter, flow);
                let vars = self.body.pattern_bindings(*pattern);
                self.loop_body(&vars, *body, flow, |walk, pass| {
                    walk.cross_barrier(SuspensionPoint::IteratorNext(*iter), pass);
                });
            }
            Statement::While { cond, body } => {
                self.loop_body(&[], *body, flow, |walk, pass| walk.expr(*cond, pass));
            }
            Statement::Break => self.exit(Exit::Break, flow),
            Statement::Continue => self.exit(Exit::Continue, flow),
            Statement::Comment(_) => {}
        }
    }

    fn expr(&mut self, expr: Idx<Expr>, flow: &mut Flow) {
        if !flow.reachable {
            return;
        }
        match self.body.expr_arena[expr] {
            Expr::Var(binding) => self.report(expr, binding, flow),
            Expr::Lambda { .. } => {}
            Expr::Block { statements, tail } => {
                let depth = self.defers.len();
                self.defers.push(Vec::new());
                for stmt in statements {
                    self.stmt(stmt, flow);
                }
                if let Some(tail) = tail {
                    self.expr(tail, flow);
                }
                let returning = if expr == self.root { tail } else { None };
                self.run_defers(depth, returning, flow);
                self.defers.pop();
            }
            Expr::If {
                cond,
                then_branch,
                else_branch,
            } => {
                self.expr(cond, flow);
                let entry = flow.clone();
                let mut out = Flow::default();
                self.branch(then_branch, &entry, &mut out);
                match else_branch {
                    Some(branch) => self.branch(branch, &entry, &mut out),
                    None => out.merge(entry),
                }
                *flow = out;
            }
            Expr::IfLet {
                pattern,
                value,
                then_branch,
                else_branch,
            } => {
                self.expr(value, flow);
                let mut entry = flow.clone();
                self.bind_pattern_flow(pattern, value, &mut entry);
                let mut out = Flow::default();
                self.branch(then_branch, &entry, &mut out);
                match else_branch {
                    Some(branch) => self.branch(branch, flow, &mut out),
                    None => out.merge(flow.clone()),
                }
                *flow = out;
            }
            Expr::Match { scrutinee, arms } => {
                self.expr(scrutinee, flow);
                let mut entry = flow.clone();
                let mut out = Flow::default();
                for arm in arms {
                    let arm = self.body.match_arm_arena[arm];
                    let mut branch = entry.clone();
                    self.bind_pattern_flow(arm.pattern, scrutinee, &mut branch);
                    if let Some(guard) = arm.guard {
                        self.expr(guard, &mut branch);
                        entry.merge(branch.clone());
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
            Expr::SafeNavLet {
                receiver,
                binding,
                body,
            } => {
                self.expr(receiver, flow);
                self.bind(binding, Some(receiver), flow);
                self.expr(body, flow);
            }
            Expr::BinOp {
                op: BinOp::NullCoalesce | BinOp::And | BinOp::Or,
                lhs,
                rhs,
            } => {
                self.expr(lhs, flow);
                let entry = flow.clone();
                let mut out = Flow::default();
                self.branch(rhs, &entry, &mut out);
                out.merge(entry);
                *flow = out;
            }
            Expr::Return(value) => {
                if let Some(value) = value {
                    self.expr(value, flow);
                }
                self.exit(Exit::Return(value), flow);
            }
            Expr::Break => self.exit(Exit::Break, flow),
            Expr::Continue => self.exit(Exit::Continue, flow),
            other => {
                if let Expr::Call {
                    callee: Callee::Local(binding),
                    ..
                } = other
                {
                    self.report(expr, binding, flow);
                }
                self.body.for_each_child(HirChild::Expr(expr), |child| {
                    if let HirChild::Expr(child) = child {
                        self.expr(child, flow);
                    }
                });
            }
        }
        self.cross_barrier(SuspensionPoint::Expression(expr), flow);
    }

    fn cross_barrier(&mut self, at: SuspensionPoint, flow: &mut Flow) {
        if self.policy.is_barrier(self.body, at) {
            flow.suspend(|binding| self.policy.crosses_binding(binding, at));
            if let Some(value) = self.returning
                && self.policy.crosses_return(value, at)
            {
                self.reported.insert(value);
            }
        }
    }

    fn run_defers(&mut self, depth: usize, returning: Option<Idx<Expr>>, flow: &mut Flow) {
        if !flow.reachable {
            return;
        }
        let depth = depth.max(self.defer_floor);
        let floor = std::mem::replace(&mut self.defer_floor, self.defers.len());
        let saved = self.returning;
        self.returning = returning.or(saved);
        for frame in (depth..self.defers.len()).rev() {
            for index in (0..self.defers[frame].len()).rev() {
                self.expr(self.defers[frame][index], flow);
            }
        }
        self.returning = saved;
        self.defer_floor = floor;
    }
}
