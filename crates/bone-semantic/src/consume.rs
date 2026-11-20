use bone_core::hir_walk::HirChild;

use crate::hir::{
    AsKind, BinOp, BindingInfo, Callee, Expr, Function, Pattern, Statement, TypeCtor, TypeExpr,
};
use crate::prelude::*;

pub(crate) fn check(
    ws: &Workspace,
    body: &Body,
    inference: &BodyInference,
    function: Option<&Function>,
) -> Vec<CompilerError> {
    let tracked = inference
        .binding_types
        .iter()
        .filter_map(|(binding, &ty)| is_consumable(ws, inference, function, ty).then_some(binding))
        .collect::<FxHashSet<_>>();
    if tracked.is_empty() {
        return Vec::new();
    }
    let mut walk = Walk {
        ws,
        body,
        inference,
        tracked: &tracked,
        depths: FxHashMap::default(),
        loops: Vec::new(),
        defers: Vec::new(),
        errors: Vec::new(),
    };
    let mut flow = Flow::new();
    for binding in body.param_bindings {
        walk.bind(binding, &mut flow);
    }
    if let Some(root) = body.root {
        walk.expr(root, false, &mut flow);
    }
    for &value in body.field_defaults.values() {
        walk.expr(value, false, &mut Flow::new());
    }
    walk.errors
        .sort_by_key(|error| error.labels[0].span.offset_range());
    walk.errors
        .dedup_by(|a, b| a.labels[0].span == b.labels[0].span);
    walk.errors
}

fn is_consumable(
    ws: &Workspace,
    inference: &BodyInference,
    function: Option<&Function>,
    ty: TyId,
) -> bool {
    match ws.resolve_ty(Some(inference), ty) {
        Some((
            _,
            TypeExpr::App {
                ctor: TypeCtor::Nominal(name),
                ..
            },
        )) => ws
            .nominals
            .struct_id(name)
            .is_some_and(|id| ws.module(id.module).structs[id.def].consume),
        Some((base, TypeExpr::Nullable(inner))) => {
            is_consumable(ws, inference, function, base.with_idx(inner))
        }
        Some((_, TypeExpr::Generic(var, _))) => function.is_some_and(|f| {
            f.generics
                .iter()
                .zip(&f.generic_bounds)
                .any(|(&ty, bound)| {
                    bound.is_some()
                        && matches!(ws.resolve_ty(Some(inference), ty),
                        Some((_, TypeExpr::Generic(generic, _))) if generic == var)
                })
        }),
        _ => false,
    }
}

#[derive(Clone)]
struct Flow {
    spent: FxHashMap<Idx<BindingInfo>, Idx<Expr>>,
    reachable: bool,
}

impl Flow {
    fn new() -> Self {
        Self {
            spent: FxHashMap::default(),
            reachable: true,
        }
    }

    fn merge(&mut self, other: Self) {
        if !other.reachable {
            return;
        }
        if !self.reachable {
            *self = other;
            return;
        }
        for (binding, at) in other.spent {
            self.spent.entry(binding).or_insert(at);
        }
    }
}

struct Loop {
    defer_depth: usize,
    exits: Flow,
}

struct Walk<'a> {
    ws: &'a Workspace,
    body: &'a Body,
    inference: &'a BodyInference,
    tracked: &'a FxHashSet<Idx<BindingInfo>>,
    depths: FxHashMap<Idx<BindingInfo>, usize>,
    loops: Vec<Loop>,
    defers: Vec<Vec<Idx<Expr>>>,
    errors: Vec<CompilerError>,
}

impl Walk<'_> {
    fn bind(&mut self, binding: Idx<BindingInfo>, flow: &mut Flow) {
        if self.tracked.contains(&binding) {
            self.depths.insert(binding, self.loops.len());
            flow.spent.remove(&binding);
        }
    }

    fn read(&mut self, binding: Idx<BindingInfo>, at: Idx<Expr>, borrow: bool, flow: &mut Flow) {
        if !self.tracked.contains(&binding) {
            return;
        }
        let repeated_in_loop = !borrow
            && self
                .depths
                .get(&binding)
                .is_some_and(|&depth| depth < self.loops.len());
        if let Some(&first) = flow.spent.get(&binding).or(repeated_in_loop.then_some(&at)) {
            let name = self.body.binding_arena[binding].name;
            let message = if repeated_in_loop && first == at {
                format!("cannot consume `{name}` declared outside this loop")
            } else {
                format!("use of consumed binding `{name}`")
            };
            self.errors.push(
                CompilerError::new_with_label(self.body.expr_spans[at], message).with_label(
                    self.body.expr_spans[first],
                    if repeated_in_loop && first == at {
                        "consumed here on each iteration".into()
                    } else {
                        "first consumed here".into()
                    },
                ),
            );
        }
        if !borrow {
            flow.spent.entry(binding).or_insert(at);
        }
    }

    fn children(&mut self, node: HirChild, flow: &mut Flow) {
        self.body
            .for_each_child(node, |child| self.node(child, flow));
    }

    fn node(&mut self, node: HirChild, flow: &mut Flow) {
        if let HirChild::Statement(stmt) = node
            && let Statement::Fn { params, body, .. } = self.body.statement_arena[stmt]
        {
            self.region(body, params, &[]);
            return;
        }
        if !flow.reachable {
            return;
        }
        match node {
            HirChild::Expr(expr) => self.expr(expr, false, flow),
            HirChild::Statement(stmt) => self.stmt(stmt, flow),
            HirChild::Pattern(pattern) => {
                if let Pattern::Bind(binding) = self.body.pattern_arena[pattern] {
                    self.bind(binding, flow);
                }
                self.children(node, flow);
            }
        }
    }

    fn region(
        &mut self,
        root: Idx<Expr>,
        params: IdxRange<BindingInfo>,
        captures: &[Idx<BindingInfo>],
    ) {
        let mut nested = Walk {
            ws: self.ws,
            body: self.body,
            inference: self.inference,
            tracked: self.tracked,
            depths: FxHashMap::default(),
            loops: Vec::new(),
            defers: Vec::new(),
            errors: Vec::new(),
        };
        let mut flow = Flow::new();
        for binding in params.into_iter().chain(captures.iter().copied()) {
            nested.bind(binding, &mut flow);
        }
        nested.expr(root, false, &mut flow);
        self.errors.extend(nested.errors);
    }

    fn run_defers(&mut self, from: usize, flow: &mut Flow) {
        let deferred: Vec<_> = self.defers[from..]
            .iter()
            .rev()
            .flat_map(|block| block.iter().rev().copied())
            .collect();
        for expr in deferred {
            self.expr(expr, false, flow);
        }
    }

    fn exit(&mut self, is_loop: bool, flow: &mut Flow) {
        let depth = if is_loop {
            self.loops.last().map_or(0, |l| l.defer_depth)
        } else {
            0
        };
        self.run_defers(depth, flow);
        if is_loop && let Some(enclosing) = self.loops.last_mut() {
            enclosing.exits.merge(flow.clone());
        }
        flow.reachable = false;
    }

    fn loop_body(
        &mut self,
        cond: Option<Idx<Expr>>,
        pattern: Option<Idx<Pattern>>,
        body: Idx<Expr>,
        flow: &mut Flow,
    ) {
        self.loops.push(Loop {
            defer_depth: self.defers.len(),
            exits: Flow {
                reachable: false,
                ..Flow::new()
            },
        });
        if let Some(cond) = cond {
            self.expr(cond, false, flow);
        }
        let mut pass = flow.clone();
        if let Some(pattern) = pattern {
            self.node(HirChild::Pattern(pattern), &mut pass);
        }
        self.expr(body, false, &mut pass);
        let exits = self.loops.pop().unwrap().exits;
        flow.merge(pass);
        flow.merge(exits);
    }

    fn stmt(&mut self, at: Idx<Statement>, flow: &mut Flow) {
        match self.body.statement_arena[at] {
            Statement::Let {
                pattern,
                value,
                else_block,
            } => {
                self.expr(value, false, flow);
                if let Some(else_block) = else_block {
                    let mut miss = flow.clone();
                    self.expr(else_block, false, &mut miss);
                    flow.merge(miss);
                }
                self.node(HirChild::Pattern(pattern), flow);
            }
            Statement::Static { name, value } => {
                self.expr(value, false, flow);
                self.bind(name, flow);
            }
            Statement::For {
                pattern,
                iter,
                body,
            } => {
                self.expr(iter, false, flow);
                self.loop_body(None, Some(pattern), body, flow);
            }
            Statement::While { cond, body } => self.loop_body(Some(cond), None, body, flow),
            Statement::Fn { params, body, .. } => self.region(body, params, &[]),
            Statement::Defer(expr) => self.defers.last_mut().unwrap().push(expr),
            Statement::Return(value) => {
                if let Some(value) = value {
                    self.expr(value, false, flow);
                }
                self.exit(false, flow);
            }
            Statement::Break | Statement::Continue => self.exit(true, flow),
            _ => self.children(HirChild::Statement(at), flow),
        }
    }

    fn is_next(&self, at: Idx<Expr>) -> bool {
        !self.inference.fn_field_calls.contains_key(&at)
            && (self.inference.bound_calls.contains_key(&at)
                || self
                    .inference
                    .resolved_overload_calls
                    .get(&at)
                    .is_some_and(|&(mid, fid)| {
                        self.ws.module(mid).functions[fid]
                            .name
                            .as_str()
                            .rsplit("::")
                            .next()
                            == Some("next")
                    }))
    }

    fn expr(&mut self, at: Idx<Expr>, borrow: bool, flow: &mut Flow) {
        if !flow.reachable {
            return;
        }
        match self.body.expr_arena[at] {
            Expr::Var(binding) => self.read(binding, at, borrow, flow),
            Expr::As {
                value,
                kind: AsKind::Ascribe,
                ..
            } => self.expr(value, borrow, flow),
            Expr::FieldAccess { base, .. } | Expr::SafeNav { base, .. } => {
                self.expr(base, true, flow)
            }
            Expr::Assign { target, value } => {
                self.expr(value, false, flow);
                if let Expr::Var(binding) = self.body.expr_arena[target] {
                    flow.spent.remove(&binding);
                } else {
                    self.expr(target, true, flow);
                }
            }
            Expr::Lambda { params, body } => {
                let span = self.body.expr_spans[at];
                let mut captures = FxHashMap::default();
                self.body
                    .walk_descendants(HirChild::Expr(at), &mut |child| {
                        if let HirChild::Expr(expr) = child
                            && let Expr::Var(binding) = self.body.expr_arena[expr]
                            && self.tracked.contains(&binding)
                            && !span.contains(self.body.binding_spans[binding])
                        {
                            captures.entry(binding).or_insert(expr);
                        }
                        true
                    });
                let mut captures: Vec<_> = captures.into_iter().collect();
                captures.sort_by_key(|(_, expr)| self.body.expr_spans[*expr].offset_range());
                for &(binding, expr) in &captures {
                    self.read(binding, expr, false, flow);
                }
                self.region(
                    body,
                    params,
                    &captures.iter().map(|&(binding, _)| binding).collect_vec(),
                );
            }
            Expr::Block { statements, tail } => {
                self.defers.push(Vec::new());
                for stmt in statements {
                    self.node(HirChild::Statement(stmt), flow);
                }
                if let Some(tail) = tail {
                    self.expr(tail, borrow, flow);
                }
                if flow.reachable {
                    self.run_defers(self.defers.len() - 1, flow);
                }
                self.defers.pop();
            }
            Expr::If {
                cond,
                then_branch,
                else_branch,
            } => {
                self.expr(cond, false, flow);
                self.branches(None, then_branch, else_branch, borrow, flow);
            }
            Expr::IfLet {
                pattern,
                value,
                then_branch,
                else_branch,
            } => {
                self.expr(value, false, flow);
                self.branches(Some(pattern), then_branch, else_branch, borrow, flow);
            }
            Expr::Match { scrutinee, arms } => {
                self.expr(scrutinee, false, flow);
                let mut entry = flow.clone();
                flow.reachable = false;
                for arm in arms {
                    let arm = self.body.match_arm_arena[arm];
                    let mut branch = entry.clone();
                    self.node(HirChild::Pattern(arm.pattern), &mut branch);
                    if let Some(guard) = arm.guard {
                        self.expr(guard, false, &mut branch);
                        entry.merge(branch.clone());
                    }
                    self.expr(arm.body, borrow, &mut branch);
                    flow.merge(branch);
                }
            }
            Expr::BinOp {
                op: BinOp::And | BinOp::Or | BinOp::NullCoalesce,
                lhs,
                rhs,
            } => {
                self.expr(lhs, borrow, flow);
                let mut branch = flow.clone();
                self.expr(rhs, borrow, &mut branch);
                flow.merge(branch);
            }
            Expr::Call { callee, args, .. } => {
                if let Callee::Value(value) = callee {
                    self.expr(value, false, flow);
                }
                let next = self.is_next(at);
                let field = self.inference.fn_field_calls.contains_key(&at);
                for (i, arg) in args.into_iter().enumerate() {
                    self.expr(arg, next || (field && i == 0), flow);
                }
            }
            Expr::SafeNavCall { receiver, args, .. } => {
                let next = self.is_next(at);
                self.expr(
                    receiver,
                    next || self.inference.fn_field_calls.contains_key(&at),
                    flow,
                );
                let mut branch = flow.clone();
                for arg in args {
                    self.expr(arg, next, &mut branch);
                }
                flow.merge(branch);
            }
            Expr::SafeNavLet {
                receiver,
                binding,
                body,
            } => {
                self.expr(receiver, false, flow);
                let mut branch = flow.clone();
                self.bind(binding, &mut branch);
                self.expr(body, borrow, &mut branch);
                flow.merge(branch);
            }
            Expr::Return(value) => {
                if let Some(value) = value {
                    self.expr(value, false, flow);
                }
                self.exit(false, flow);
            }
            Expr::Break | Expr::Continue => self.exit(true, flow),
            _ => self.children(HirChild::Expr(at), flow),
        }
    }

    fn branches(
        &mut self,
        pattern: Option<Idx<Pattern>>,
        then_branch: Idx<Expr>,
        else_branch: Option<Idx<Expr>>,
        borrow: bool,
        flow: &mut Flow,
    ) {
        let mut then_flow = flow.clone();
        if let Some(pattern) = pattern {
            self.node(HirChild::Pattern(pattern), &mut then_flow);
        }
        self.expr(then_branch, borrow, &mut then_flow);
        if let Some(other) = else_branch {
            self.expr(other, borrow, flow);
        }
        flow.merge(then_flow);
    }
}
