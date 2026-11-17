//! Deep-copy of AST subtrees: from one `Ast` into another, or within one.
//!
//! Macro expansion inlines a macro body (defined in module A's arena) at a call
//! site in module B's arena, so the template's nodes are re-allocated into the
//! caller. Compound-assign's read copy of an lvalue (`a += b` -> `a = a + b`)
//! and repeated macro arguments copy within a single arena instead: `Idx` is a
//! node's identity, so duplication re-allocates every node with remapped child
//! indices.
//!
//! A copy can also rewrite what it copies ([`Rewrite`]): macro instantiation
//! freshens the body's binders, stamps the call span and substitutes params in
//! the same walk.

use crate::ast::*;
use rock_core::Span;
use ustr::Ustr;

/// How a copy changes the expressions, statements and blocks it copies. Every
/// hook defaults to leaving the node as it was, which is what [`Plain`] does.
///
/// Binders are reported as the walk meets them: `enter`/`exit` bracket a
/// scope, and a binder is bound after the expressions that cannot see it (a
/// `let`'s value, an `if let`'s scrutinee) were copied.
pub(crate) trait Rewrite {
    /// The span every copied node gets instead of its own; a stamped block
    /// has no braces.
    fn stamp(&self) -> Option<Span> {
        None
    }
    fn enter(&mut self) {}
    fn exit(&mut self) {}
    fn bind(&mut self, _name: &mut Ustr) {}
    fn bind_pattern(&mut self, _p: &mut Pattern) {}
    /// The name a reference to `name` takes.
    fn resolve(&self, name: Ustr) -> Ustr {
        name
    }
    /// The expression (in the destination arena) a reference to `name` is
    /// replaced by a copy of.
    fn arg(&self, _name: Ustr) -> Option<Idx<ExprData>> {
        None
    }
    /// The expressions a `...name` call argument forwards.
    fn splat(&self, _name: Ustr) -> Option<&[Idx<ExprData>]> {
        None
    }
    /// The type (in the destination arena) a type named `name` is replaced by;
    /// a declaration named `name` takes its name when it is a bare identifier.
    fn ty_arg(&self, _name: Ustr) -> Option<Idx<TypeExprData>> {
        None
    }
}

pub(crate) struct Plain;

impl Rewrite for Plain {}

pub(crate) struct Graft<'s, 'd, R> {
    src: Option<&'s Ast>,
    dst: &'d mut Ast,
    rw: R,
}

fn clone_expr(ast: &mut Ast, e: Idx<ExprData>) -> Idx<ExprData> {
    Graft {
        src: None,
        dst: ast,
        rw: Plain,
    }
    .expr(e)
}

impl<'s, 'd, R: Rewrite> Graft<'s, 'd, R> {
    pub(crate) fn new(src: &'s Ast, dst: &'d mut Ast, rw: R) -> Self {
        Graft {
            src: Some(src),
            dst,
            rw,
        }
    }

    fn source(&self) -> &Ast {
        self.src.unwrap_or(self.dst)
    }

    pub(crate) fn item(&mut self, item: &Item) -> Item {
        let mut item = item.clone();
        self.rewrite_item(&mut item);
        item
    }

    fn rewrite_item(&mut self, item: &mut Item) {
        match &mut item.kind {
            ItemKind::Function(f) => {
                self.decl(&mut f.name);
                self.generics(&mut f.generics);
                self.params(&mut f.params);
                f.return_ty = f.return_ty.map(|t| self.ty(t));
                if let FunctionBody::Rock(b) = &mut f.body {
                    *b = self.block(*b);
                }
                for item in &mut f.macro_items {
                    self.rewrite_item(item);
                }
            }
            ItemKind::Struct(s) => {
                self.decl(&mut s.name);
                self.generics(&mut s.generics);
                self.fields(&mut s.fields);
            }
            ItemKind::Enum(e) => {
                self.decl(&mut e.name);
                self.generics(&mut e.generics);
                for v in &mut e.variants {
                    match &mut v.kind {
                        EnumVariantKind::Unit => {}
                        EnumVariantKind::Tuple(ts) => self.types(ts),
                        EnumVariantKind::Record(fs) => self.fields(fs),
                    }
                }
            }
            ItemKind::Newtype(n) => {
                self.decl(&mut n.name);
                self.generics(&mut n.generics);
                n.wrapped = self.ty(n.wrapped);
            }
            ItemKind::Global(g) => {
                g.ty = g.ty.map(|t| self.ty(t));
                g.value = g.value.map(|e| self.expr(e));
            }
            _ => {}
        }
    }

    fn params(&mut self, params: &mut [Param]) {
        for p in params {
            self.decl(&mut p.name);
            p.ty = self.ty(p.ty);
            p.default = p.default.map(|e| self.expr(e));
        }
    }

    fn generics(&mut self, generics: &mut [Generic]) {
        for g in generics {
            g.bound = g.bound.map(|t| self.ty(t));
            g.default = g.default.map(|t| self.ty(t));
        }
    }

    fn fields(&mut self, fields: &mut [Field]) {
        for f in fields {
            self.decl(&mut f.name);
            f.ty = self.ty(f.ty);
            f.default = f.default.map(|e| self.expr(e));
        }
    }

    pub(crate) fn block(&mut self, b: Idx<BlockData>) -> Idx<BlockData> {
        let mut data = self.source().blocks[b].clone();
        self.rw.enter();
        for s in &mut data.statements {
            *s = self.stmt(*s);
        }
        self.rw.exit();
        if let Some(span) = self.rw.stamp() {
            data.span = span;
            data.braces = None;
        }
        self.dst.blocks.alloc(data)
    }

    pub(crate) fn expr(&mut self, e: Idx<ExprData>) -> Idx<ExprData> {
        let mut data = self.source().exprs[e].clone();
        if let ExprKind::Ident(id) = &data.kind {
            if let Some(arg) = self.rw.arg(id.name) {
                return clone_expr(self.dst, arg);
            }
        }
        self.expr_kind(&mut data.kind);
        data.span = self.rw.stamp().unwrap_or(data.span);
        self.dst.exprs.alloc(data)
    }

    pub(crate) fn ty(&mut self, t: Idx<TypeExprData>) -> Idx<TypeExprData> {
        let mut data = self.source().types[t].clone();
        if let TypeExprKind::Ident(id) = &data.kind {
            if let Some(r) = self.rw.ty_arg(id.name) {
                let replaced = &self.dst.types[r];
                data.kind = replaced.kind.clone();
                data.is_nullable |= replaced.is_nullable;
                return self.dst.types.alloc(data);
            }
        }
        self.ty_kind(&mut data.kind);
        self.dst.types.alloc(data)
    }

    fn stmt(&mut self, s: Idx<StatementData>) -> Idx<StatementData> {
        let mut data = self.source().stmts[s].clone();
        match &mut data.kind {
            StatementKind::Expression(e) | StatementKind::Defer(DeferKind::Expression(e)) => {
                *e = self.expr(*e);
            }
            StatementKind::Let(l) => {
                l.ty = l.ty.map(|t| self.ty(t));
                l.value = l.value.map(|e| self.expr(e));
                self.pattern(&mut l.pattern);
                // The else block cannot see the pattern's bindings.
                l.else_block = l.else_block.map(|b| self.block(b));
                self.rw.bind_pattern(&mut l.pattern);
            }
            StatementKind::Static(st) => {
                st.ty = st.ty.map(|t| self.ty(t));
                st.value = self.expr(st.value);
                self.rw.bind(&mut st.name.name);
            }
            StatementKind::NestedFn(f) => {
                self.params(&mut f.params);
                f.return_ty = f.return_ty.map(|t| self.ty(t));
                f.body = self.block(f.body);
            }
            StatementKind::Return { value, .. } => *value = value.map(|e| self.expr(e)),
            StatementKind::For(f) => {
                f.ty = f.ty.map(|t| self.ty(t));
                f.iter = self.expr(f.iter);
                self.pattern(&mut f.pattern);
                f.body = self.scoped(f.body, |rw| rw.bind_pattern(&mut f.pattern));
            }
            StatementKind::While(w) => {
                w.condition = self.expr(w.condition);
                w.body = self.block(w.body);
            }
            StatementKind::WhileLet(w) => {
                self.pattern(&mut w.pattern);
                w.value = self.expr(w.value);
                w.body = self.scoped(w.body, |rw| rw.bind_pattern(&mut w.pattern));
            }
            StatementKind::Defer(DeferKind::Block(b))
            | StatementKind::LifecycleHook { body: b, .. } => *b = self.block(*b),
            StatementKind::Use(_)
            | StatementKind::Break { .. }
            | StatementKind::Continue { .. }
            | StatementKind::Comment(_) => {}
        }
        data.span = self.rw.stamp().unwrap_or(data.span);
        self.dst.stmts.alloc(data)
    }

    fn decl(&self, id: &mut Ident) {
        if let Some(TypeExprKind::Ident(arg)) =
            self.rw.ty_arg(id.name).map(|r| &self.dst.types[r].kind)
        {
            *id = arg.clone();
        }
    }

    fn scoped(&mut self, body: Idx<BlockData>, bind: impl FnOnce(&mut R)) -> Idx<BlockData> {
        self.rw.enter();
        bind(&mut self.rw);
        let body = self.block(body);
        self.rw.exit();
        body
    }

    fn ctor(&self, id: &mut Ident) {
        if let Some(ExprKind::Ident(arg)) = self.rw.arg(id.name).map(|a| &self.dst.exprs[a].kind) {
            *id = arg.clone();
        }
    }

    fn path(&self, path: &mut Path) {
        for id in &mut path.segments {
            self.ctor(id);
        }
    }

    fn type_path(&self, path: &mut Path) {
        for id in &mut path.segments {
            self.decl(id);
        }
    }

    fn types(&mut self, types: &mut [Idx<TypeExprData>]) {
        for t in types {
            *t = self.ty(*t);
        }
    }

    fn call_args(&mut self, args: &mut Vec<FunctionCallArg>) {
        if !args.iter().any(|a| a.is_splat) {
            for a in args {
                a.value = self.expr(a.value);
            }
            return;
        }
        let count = args.len();
        let old = std::mem::replace(args, Vec::with_capacity(count));
        for mut a in old {
            if a.is_splat
                && let ExprKind::Ident(id) = &self.source().exprs[a.value].kind
                && let Some(values) = self.rw.splat(id.name).map(<[_]>::to_vec)
            {
                args.extend(values.into_iter().map(|v| FunctionCallArg {
                    name: None,
                    value: clone_expr(self.dst, v),
                    is_splat: false,
                    is_spread: false,
                }));
            } else {
                a.value = self.expr(a.value);
                args.push(a);
            }
        }
    }

    fn expr_kind(&mut self, kind: &mut ExprKind) {
        match kind {
            ExprKind::Null
            | ExprKind::Unit
            | ExprKind::Int(_, _)
            | ExprKind::Float(_)
            | ExprKind::String(_)
            | ExprKind::Bool(_)
            | ExprKind::Hole(_, _)
            | ExprKind::BreakExpr
            | ExprKind::ContinueExpr
            | ExprKind::SiblingFieldRef(_)
            | ExprKind::FunctionRef(_)
            | ExprKind::AnaphoricIdent(_) => {}
            ExprKind::Ident(id) => id.name = self.rw.resolve(id.name),
            ExprKind::Path { path, generic_args } => {
                self.path(path);
                self.types(generic_args);
            }
            ExprKind::PathStructLiteral {
                path,
                bases,
                fields,
            } => {
                self.path(path);
                for b in bases {
                    *b = self.expr(*b);
                }
                for (_, v) in fields {
                    *v = self.expr(*v);
                }
            }
            ExprKind::InterpolatedString(parts) => {
                for p in parts {
                    if let InterpolatedPart::Expr(e, spec) = p {
                        *e = self.expr(*e);
                        if let Some(s) = spec {
                            s.width_expr = s.width_expr.map(|e| self.expr(e));
                            s.precision_expr = s.precision_expr.map(|e| self.expr(e));
                        }
                    }
                }
            }
            ExprKind::Paren(x)
            | ExprKind::Not(x)
            | ExprKind::BitNot(x)
            | ExprKind::UnaryMinus(x)
            | ExprKind::FieldAccess(x, _)
            | ExprKind::SafeNavigation(x, _)
            | ExprKind::QuestionMark(x)
            | ExprKind::PreIncrement(x)
            | ExprKind::PostIncrement(x)
            | ExprKind::PreDecrement(x)
            | ExprKind::PostDecrement(x)
            | ExprKind::Spread(x)
            | ExprKind::YieldDelegate(x) => *x = self.expr(*x),
            ExprKind::As(x, t) | ExprKind::Bitcast(x, t) | ExprKind::ParenAscription(x, t) => {
                *x = self.expr(*x);
                *t = self.ty(*t);
            }
            ExprKind::SafeNavigationLet {
                receiver,
                binding,
                body,
            } => {
                *receiver = self.expr(*receiver);
                *body = self.scoped(*body, |rw| rw.bind(&mut binding.name));
            }
            ExprKind::Index(left, right)
            | ExprKind::BinOp { left, right, .. }
            | ExprKind::Assign {
                target: left,
                value: right,
            } => {
                *left = self.expr(*left);
                *right = self.expr(*right);
            }
            ExprKind::ReturnExpr(o) | ExprKind::Yield(o) => *o = o.map(|e| self.expr(e)),
            ExprKind::FunctionCall {
                func,
                generic_args,
                args,
                ..
            } => {
                *func = self.expr(*func);
                self.types(generic_args);
                self.call_args(args);
            }
            ExprKind::ArrayLiteral(xs) | ExprKind::Tuple(xs) => {
                for x in xs {
                    *x = self.expr(*x);
                }
            }
            ExprKind::Block(b) => *b = self.block(*b),
            ExprKind::Lambda {
                params,
                return_ty,
                body,
            } => {
                for p in &mut *params {
                    p.ty = p.ty.map(|t| self.ty(t));
                }
                *return_ty = return_ty.map(|t| self.ty(t));
                *body = self.scoped(*body, |rw| {
                    for p in params {
                        rw.bind(&mut p.name.name);
                    }
                });
            }
            ExprKind::Match { scrutinee, arms } => {
                *scrutinee = self.expr(*scrutinee);
                for a in arms {
                    self.pattern(&mut a.pattern);
                    self.rw.enter();
                    self.rw.bind_pattern(&mut a.pattern);
                    a.guard = a.guard.map(|g| self.expr(g));
                    a.body = self.block(a.body);
                    self.rw.exit();
                }
            }
            ExprKind::If {
                branches,
                else_branch,
            } => {
                for (c, b) in branches {
                    *c = self.expr(*c);
                    *b = self.block(*b);
                }
                *else_branch = else_branch.map(|b| self.block(b));
            }
            ExprKind::IfLetChain {
                clauses,
                then_block,
                else_block,
            } => {
                // Each clause sees the bindings of the preceding clauses.
                self.rw.enter();
                for c in clauses {
                    match c {
                        IfLetClause::Binding { pattern, value } => {
                            self.pattern(pattern);
                            *value = self.expr(*value);
                            self.rw.bind_pattern(pattern);
                        }
                        IfLetClause::Condition(c) => *c = self.expr(*c),
                    }
                }
                *then_block = self.block(*then_block);
                self.rw.exit();
                *else_block = else_block.map(|b| self.block(b));
            }
        }
    }

    fn pattern(&mut self, p: &mut Pattern) {
        match p {
            Pattern::Wildcard | Pattern::Ident(_) => {}
            Pattern::Literal(e) => *e = self.expr(*e),
            Pattern::Tuple(ps) | Pattern::Or(ps) => {
                for p in ps {
                    self.pattern(p);
                }
            }
            Pattern::PathVariant { path, args } => {
                if path.segments.len() != 1 {
                    self.path(path);
                }
                for p in args.iter_mut().flatten() {
                    self.pattern(p);
                }
            }
            Pattern::PathRecord { path, fields, .. } => {
                if path.segments.len() != 1 {
                    self.path(path);
                }
                for (_, p) in fields {
                    self.pattern(p);
                }
            }
            Pattern::Range { start, end, .. } => {
                *start = start.map(|e| self.expr(e));
                *end = end.map(|e| self.expr(e));
            }
        }
    }

    fn ty_kind(&mut self, kind: &mut TypeExprKind) {
        match kind {
            TypeExprKind::Any
            | TypeExprKind::Unit
            | TypeExprKind::String
            | TypeExprKind::Int
            | TypeExprKind::Uint
            | TypeExprKind::Float
            | TypeExprKind::F64
            | TypeExprKind::F32
            | TypeExprKind::Bool
            | TypeExprKind::Hole(_, _)
            | TypeExprKind::Ident(_) => {}
            TypeExprKind::Path(path) => self.type_path(path),
            TypeExprKind::Ref(t) | TypeExprKind::Array(t) => *t = self.ty(*t),
            TypeExprKind::AppPath { ctor, args } => {
                self.type_path(ctor);
                self.types(args);
            }
            TypeExprKind::App { ctor, args } => {
                self.decl(ctor);
                self.types(args);
            }
            TypeExprKind::Tuple(ts) => self.types(ts),
            TypeExprKind::Function { params, ret } => {
                self.types(params);
                *ret = self.ty(*ret);
            }
        }
    }
}
