use crate::ast::*;
use crate::walk;
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
    fn resolve(&self, name: Ustr) -> Ustr {
        name
    }
    fn arg(&self, _name: Ustr) -> Option<Idx<ExprData>> {
        None
    }
    fn splat(&self, _name: Ustr) -> Option<&[Idx<ExprData>]> {
        None
    }
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
        if let StatementKind::NestedFn(f) = &mut data.kind {
            self.params(&mut f.params);
            f.return_ty = f.return_ty.map(|t| self.ty(t));
            f.body = self.block(f.body);
        } else {
            walk::statement_mut(self, &mut data.kind);
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
                args.extend(
                    values
                        .into_iter()
                        .map(|v| FunctionCallArg::positional(clone_expr(self.dst, v))),
                );
            } else {
                a.value = self.expr(a.value);
                args.push(a);
            }
        }
    }

    fn expr_kind(&mut self, kind: &mut ExprKind) {
        match kind {
            ExprKind::Ident(id) => id.name = self.rw.resolve(id.name),
            ExprKind::Path(path) => self.path(path),
            ExprKind::PathStructLiteral {
                path,
                bases,
                fields,
            } => {
                self.path(path);
                for e in bases {
                    *e = self.expr(*e);
                }
                for (_, e) in fields {
                    *e = self.expr(*e);
                }
            }
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
            kind => walk::expr_mut(self, kind),
        }
    }

    fn ty_kind(&mut self, kind: &mut TypeExprKind) {
        match kind {
            TypeExprKind::Path(path) => self.type_path(path),
            TypeExprKind::AppPath { ctor, args } => {
                self.type_path(ctor);
                self.types(args);
            }
            TypeExprKind::App { ctor, args } => {
                self.decl(ctor);
                self.types(args);
            }
            kind => walk::ty_mut(self, kind),
        }
    }
}

impl<R: Rewrite> walk::VisitMut for Graft<'_, '_, R> {
    fn enter(&mut self) {
        self.rw.enter();
    }
    fn exit(&mut self) {
        self.rw.exit();
    }
    fn bind(&mut self, id: &mut Ident) {
        self.rw.bind(&mut id.name);
    }
    fn bind_pattern(&mut self, p: &mut Pattern) {
        self.rw.bind_pattern(p);
    }
    fn expr(&mut self, e: &mut Idx<ExprData>) {
        *e = Graft::expr(self, *e);
    }
    fn block(&mut self, b: &mut Idx<BlockData>) {
        *b = Graft::block(self, *b);
    }
    fn ty(&mut self, t: &mut Idx<TypeExprData>) {
        *t = Graft::ty(self, *t);
    }
    fn path(&mut self, path: &mut Path) {
        if path.segments.len() != 1 {
            Graft::path(self, path);
        }
    }
}
