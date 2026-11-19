//! Direct AST children shared by readers and subtree remapping.

use crate::ast::*;
use smallvec::SmallVec;

macro_rules! walkers {
    ($visit:ident, $expr:ident, $statement:ident, $ty:ident, $pattern:ident, [$($mut:tt)*]) => {
        pub(crate) trait $visit {
            fn expr(&mut self, _expr: & $($mut)* Idx<ExprData>) {}
            fn block(&mut self, _block: & $($mut)* Idx<BlockData>) {}
            fn ty(&mut self, _ty: & $($mut)* Idx<TypeExprData>) {}
            fn ident(&mut self, _ident: & $($mut)* Ident) {}
            fn path(&mut self, _path: & $($mut)* Path) {}
            fn enter(&mut self) {}
            fn exit(&mut self) {}
            fn bind(&mut self, _ident: & $($mut)* Ident) {}
            fn bind_pattern(&mut self, _pattern: & $($mut)* Pattern) {}
            fn pattern(&mut self, pattern: & $($mut)* Pattern) {
                $pattern(self, pattern);
            }
        }

        pub(crate) fn $expr(v: &mut (impl $visit + ?Sized), kind: & $($mut)* ExprKind) {
            match kind {
                ExprKind::Ident(id) => v.ident(id),
                ExprKind::Path(path) => v.path(path),
                ExprKind::PathStructLiteral { path, bases, fields } => {
                    v.path(path);
                    for e in bases { v.expr(e); }
                    for (_, e) in fields { v.expr(e); }
                }
                ExprKind::InterpolatedString(parts) => {
                    for part in parts {
                        if let InterpolatedPart::Expr(e, spec) = part {
                            v.expr(e);
                            if let Some(spec) = spec {
                                for e in & $($mut)* spec.width_expr { v.expr(e); }
                                for e in & $($mut)* spec.precision_expr { v.expr(e); }
                            }
                        }
                    }
                }
                ExprKind::Paren(e) | ExprKind::Not(e) | ExprKind::BitNot(e)
                | ExprKind::UnaryMinus(e) | ExprKind::FieldAccess(e, _)
                | ExprKind::SafeNavigation(e, _) | ExprKind::QuestionMark(e)
                | ExprKind::PreIncrement(e) | ExprKind::PostIncrement(e)
                | ExprKind::PreDecrement(e) | ExprKind::PostDecrement(e)
                | ExprKind::Spread(e) | ExprKind::YieldDelegate(e) => v.expr(e),
                ExprKind::As(e, t) | ExprKind::Bitcast(e, t) | ExprKind::ParenAscription(e, t) => {
                    v.expr(e);
                    v.ty(t);
                }
                ExprKind::SafeNavigationLet { receiver, binding, body } => {
                    v.expr(receiver);
                    v.enter();
                    v.bind(binding);
                    v.block(body);
                    v.exit();
                }
                ExprKind::Index(left, right) | ExprKind::BinOp { left, right, .. }
                | ExprKind::Assign { target: left, value: right } => {
                    v.expr(left);
                    v.expr(right);
                }
                ExprKind::ReturnExpr(e) | ExprKind::Yield(e) => {
                    for e in e { v.expr(e); }
                }
                ExprKind::FunctionCall { func, generic_args, args, .. } => {
                    v.expr(func);
                    for t in generic_args { v.ty(t); }
                    for a in args { v.expr(& $($mut)* a.value); }
                }
                ExprKind::ArrayLiteral(es) | ExprKind::Tuple(es) => {
                    for e in es { v.expr(e); }
                }
                ExprKind::Block(b) => v.block(b),
                ExprKind::Lambda { params, return_ty, body } => {
                    for p in & $($mut)* *params {
                        for t in & $($mut)* p.ty { v.ty(t); }
                    }
                    for t in return_ty { v.ty(t); }
                    v.enter();
                    for p in params { v.bind(& $($mut)* p.name); }
                    v.block(body);
                    v.exit();
                }
                ExprKind::Match { scrutinee, arms } => {
                    v.expr(scrutinee);
                    for arm in arms {
                        v.pattern(& $($mut)* arm.pattern);
                        v.enter();
                        v.bind_pattern(& $($mut)* arm.pattern);
                        for g in & $($mut)* arm.guard { v.expr(g); }
                        v.block(& $($mut)* arm.body);
                        v.exit();
                    }
                }
                ExprKind::If { branches, else_branch } => {
                    for (c, b) in branches {
                        v.expr(c);
                        v.block(b);
                    }
                    for b in else_branch { v.block(b); }
                }
                ExprKind::IfLetChain { clauses, then_block, else_block } => {
                    v.enter();
                    for clause in clauses {
                        match clause {
                            IfLetClause::Binding { pattern, value } => {
                                v.pattern(pattern);
                                v.expr(value);
                                v.bind_pattern(pattern);
                            }
                            IfLetClause::Condition(e) => v.expr(e),
                        }
                    }
                    v.block(then_block);
                    v.exit();
                    for b in else_block { v.block(b); }
                }
                ExprKind::Null | ExprKind::Unit | ExprKind::Int(..) | ExprKind::Float(_)
                | ExprKind::String(_) | ExprKind::Bool(_) | ExprKind::Hole(..)
                | ExprKind::BreakExpr | ExprKind::ContinueExpr | ExprKind::SiblingFieldRef(_)
                | ExprKind::FunctionRef(_) | ExprKind::AnaphoricIdent(_) => {}
            }
        }

        pub(crate) fn $statement(v: &mut (impl $visit + ?Sized), kind: & $($mut)* StatementKind) {
            match kind {
                StatementKind::Expression(e) | StatementKind::Defer(DeferKind::Expression(e)) => v.expr(e),
                StatementKind::Let(l) => {
                    for t in & $($mut)* l.ty { v.ty(t); }
                    for e in & $($mut)* l.value { v.expr(e); }
                    v.pattern(& $($mut)* l.pattern);
                    for b in & $($mut)* l.else_block { v.block(b); }
                    v.bind_pattern(& $($mut)* l.pattern);
                }
                StatementKind::Static(s) => {
                    for t in & $($mut)* s.ty { v.ty(t); }
                    v.expr(& $($mut)* s.value);
                    v.bind(& $($mut)* s.name);
                }
                StatementKind::NestedFn(f) => {
                    for p in & $($mut)* f.params {
                        v.ty(& $($mut)* p.ty);
                        for e in & $($mut)* p.default { v.expr(e); }
                    }
                    for t in & $($mut)* f.return_ty { v.ty(t); }
                    v.block(& $($mut)* f.body);
                }
                StatementKind::Return { value, .. } => {
                    for e in value { v.expr(e); }
                }
                StatementKind::For(f) => {
                    for t in & $($mut)* f.ty { v.ty(t); }
                    v.expr(& $($mut)* f.iter);
                    v.pattern(& $($mut)* f.pattern);
                    v.enter();
                    v.bind_pattern(& $($mut)* f.pattern);
                    v.block(& $($mut)* f.body);
                    v.exit();
                }
                StatementKind::While(w) => {
                    v.expr(& $($mut)* w.condition);
                    v.block(& $($mut)* w.body);
                }
                StatementKind::WhileLet(w) => {
                    v.pattern(& $($mut)* w.pattern);
                    v.expr(& $($mut)* w.value);
                    v.enter();
                    v.bind_pattern(& $($mut)* w.pattern);
                    v.block(& $($mut)* w.body);
                    v.exit();
                }
                StatementKind::Defer(DeferKind::Block(b))
                | StatementKind::LifecycleHook { body: b, .. } => v.block(b),
                StatementKind::Use(_) | StatementKind::Break { .. }
                | StatementKind::Continue { .. } | StatementKind::Comment(_) => {}
            }
        }

        pub(crate) fn $ty(v: &mut (impl $visit + ?Sized), kind: & $($mut)* TypeExprKind) {
            match kind {
                TypeExprKind::Ident(id) => v.ident(id),
                TypeExprKind::Path(p) => v.path(p),
                TypeExprKind::Ref(t) | TypeExprKind::Array(t) => v.ty(t),
                TypeExprKind::AppPath { ctor, args } => {
                    v.path(ctor);
                    for t in args { v.ty(t); }
                }
                TypeExprKind::App { ctor, args } => {
                    v.ident(ctor);
                    for t in args { v.ty(t); }
                }
                TypeExprKind::Tuple(ts) => { for t in ts { v.ty(t); } }
                TypeExprKind::Function { params, ret } => {
                    for t in params { v.ty(t); }
                    v.ty(ret);
                }
                TypeExprKind::Any | TypeExprKind::Unit | TypeExprKind::String
                | TypeExprKind::Int | TypeExprKind::Uint | TypeExprKind::Float
                | TypeExprKind::F64 | TypeExprKind::F32 | TypeExprKind::Bool
                | TypeExprKind::Hole(..) => {}
            }
        }

        pub(crate) fn $pattern(v: &mut (impl $visit + ?Sized), pattern: & $($mut)* Pattern) {
            match pattern {
                Pattern::Wildcard => {},
                Pattern::Ident(id) => v.ident(id),
                Pattern::Literal(e) => v.expr(e),
                Pattern::Tuple(ps) | Pattern::Or(ps) => {
                    for p in ps { v.pattern(p); }
                }
                Pattern::PathVariant { path, args } => {
                    v.path(path);
                    if let Some(ps) = args {
                        for p in ps { v.pattern(p); }
                    }
                }
                Pattern::PathRecord { path, fields, .. } => {
                    v.path(path);
                    for (_, p) in fields { v.pattern(p); }
                }
                Pattern::Range { start, end, .. } => {
                    for e in start { v.expr(e); }
                    for e in end { v.expr(e); }
                }
            }
        }
    };
}

walkers!(Visit, expr, statement, ty, pattern, []);
walkers!(VisitMut, expr_mut, statement_mut, ty_mut, pattern_mut, [mut]);

pub(crate) type ExprEdges = SmallVec<[Idx<ExprData>; 4]>;
pub(crate) type BlockEdges = SmallVec<[Idx<BlockData>; 2]>;

#[derive(Default)]
pub(crate) struct Edges {
    pub exprs: ExprEdges,
    pub blocks: BlockEdges,
}

impl Visit for Edges {
    fn expr(&mut self, e: &Idx<ExprData>) {
        self.exprs.push(*e);
    }
    fn block(&mut self, b: &Idx<BlockData>) {
        self.blocks.push(*b);
    }
    fn pattern(&mut self, _: &Pattern) {}
}

pub(crate) fn expr_edges(ast: &Ast, e: Idx<ExprData>) -> Edges {
    let mut edges = Edges::default();
    expr(&mut edges, &ast.exprs[e].kind);
    edges
}

pub(crate) fn statement_edges(ast: &Ast, s: Idx<StatementData>) -> Edges {
    let mut edges = Edges::default();
    statement(&mut edges, &ast.stmts[s].kind);
    edges
}

pub(crate) fn bindings(p: &Pattern, f: &mut impl FnMut(&Ident)) {
    struct Bindings<'a, F>(&'a mut F);
    impl<F: FnMut(&Ident)> Visit for Bindings<'_, F> {
        fn ident(&mut self, id: &Ident) {
            self.0(id);
        }
    }
    pattern(&mut Bindings(f), p);
}
