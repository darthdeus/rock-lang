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
use std::borrow::Cow;
use ustr::Ustr;

/// Where a copy reads source nodes from. A separate `Ast` is borrowed for the
/// whole copy; a within-arena copy clones each node it reads, because `dst` is
/// mutated as the copy proceeds.
pub(crate) trait Source: Copy {
    fn read<'a, T: Clone + 'a>(
        self,
        dst: &Ast,
        arena: fn(&Ast) -> &Arena<T>,
        id: Idx<T>,
    ) -> Cow<'a, T>
    where
        Self: 'a;
}

impl Source for &Ast {
    fn read<'a, T: Clone + 'a>(
        self,
        _dst: &Ast,
        arena: fn(&Ast) -> &Arena<T>,
        id: Idx<T>,
    ) -> Cow<'a, T>
    where
        Self: 'a,
    {
        Cow::Borrowed(&arena(self)[id])
    }
}

#[derive(Clone, Copy)]
struct SameArena;

impl Source for SameArena {
    fn read<'a, T: Clone + 'a>(
        self,
        dst: &Ast,
        arena: fn(&Ast) -> &Arena<T>,
        id: Idx<T>,
    ) -> Cow<'a, T> {
        Cow::Owned(arena(dst)[id].clone())
    }
}

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
    fn bind_binding(&mut self, _b: &mut Binding) {}
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

pub(crate) struct Graft<'d, S, R> {
    src: S,
    dst: &'d mut Ast,
    rw: R,
}

/// Deep-copy the expression subtree rooted at `e` within `ast`, returning the
/// new root.
pub(crate) fn clone_expr(ast: &mut Ast, e: Idx<ExprData>) -> Idx<ExprData> {
    Graft::new(SameArena, ast, Plain).expr(e)
}

impl<'d, S: Source, R: Rewrite> Graft<'d, S, R> {
    /// A copy from `src` into `dst` with `rw` applied to every copied node.
    pub(crate) fn new(src: S, dst: &'d mut Ast, rw: R) -> Self {
        Graft { src, dst, rw }
    }
}

impl<S: Source, R: Rewrite> Graft<'_, S, R> {
    pub(crate) fn item(&mut self, item: &Item) -> Item {
        let kind = match &item.kind {
            ItemKind::Function(f) => ItemKind::Function(self.func(f)),
            ItemKind::Struct(s) => ItemKind::Struct(StructDef {
                span: s.span,
                keyword_span: s.keyword_span,
                name: self.decl(&s.name),
                generics: self.generics(&s.generics),
                fields: s.fields.iter().map(|fl| self.field(fl)).collect(),
                is_value: s.is_value,
                is_nobox: s.is_nobox,
            }),
            ItemKind::Enum(e) => ItemKind::Enum(EnumDef {
                span: e.span,
                keyword_span: e.keyword_span,
                name: self.decl(&e.name),
                generics: self.generics(&e.generics),
                variants: e.variants.iter().map(|v| self.variant(v)).collect(),
            }),
            ItemKind::Newtype(n) => ItemKind::Newtype(NewtypeDef {
                span: n.span,
                name: self.decl(&n.name),
                generics: self.generics(&n.generics),
                wrapped: self.ty(n.wrapped),
            }),
            ItemKind::Global(g) => ItemKind::Global(GlobalDef {
                span: g.span,
                keyword_span: g.keyword_span,
                name: g.name.clone(),
                ty: g.ty.map(|t| self.ty(t)),
                value: g.value.map(|e| self.expr(e)),
                is_const: g.is_const,
            }),
            // Comment / directive / nested invocation don't appear in macro_items.
            other => other.clone(),
        };
        Item {
            span: item.span,
            annotations: item.annotations.clone(),
            vis: item.vis.clone(),
            kind,
        }
    }

    fn func(&mut self, f: &FunctionDef) -> FunctionDef {
        FunctionDef {
            span: f.span,
            keyword_span: f.keyword_span,
            name: self.decl(&f.name),
            generics: self.generics(&f.generics),
            params: f.params.iter().map(|p| self.param(p)).collect(),
            params_span: f.params_span,
            return_ty: f.return_ty.map(|t| self.ty(t)),
            body: match &f.body {
                FunctionBody::Rock(b) => FunctionBody::Rock(self.block(*b)),
                FunctionBody::Inline(be, s) => FunctionBody::Inline(*be, *s),
                FunctionBody::ExternLua => FunctionBody::ExternLua,
                FunctionBody::ExternJs => FunctionBody::ExternJs,
                FunctionBody::Ffi(l) => FunctionBody::Ffi(*l),
            },
            is_macro: f.is_macro,
            is_coroutine: f.is_coroutine,
            is_synthetic: f.is_synthetic,
            json_field_for: f.json_field_for.clone(),
            macro_items: f.macro_items.iter().map(|it| self.item(it)).collect(),
            receiver_surface: f.receiver_surface.clone(),
        }
    }

    fn param(&mut self, p: &Param) -> Param {
        Param {
            span: p.span,
            name: self.decl(&p.name),
            ty: self.ty(p.ty),
            default: p.default.map(|e| self.expr(e)),
            is_variadic: p.is_variadic,
            annotations: p.annotations.clone(),
        }
    }

    fn generics(&mut self, gs: &[Generic]) -> Vec<Generic> {
        gs.iter()
            .map(|g| Generic {
                name: g.name.clone(),
                bound: g.bound.map(|t| self.ty(t)),
                default: g.default.map(|t| self.ty(t)),
            })
            .collect()
    }

    fn field(&mut self, fl: &Field) -> Field {
        Field {
            span: fl.span,
            name: self.decl(&fl.name),
            ty: self.ty(fl.ty),
            vis: fl.vis.clone(),
            set_private: fl.set_private,
            default: fl.default.map(|e| self.expr(e)),
            annotations: fl.annotations.clone(),
            is_embed: fl.is_embed,
        }
    }

    fn variant(&mut self, v: &EnumVariant) -> EnumVariant {
        EnumVariant {
            span: v.span,
            name: v.name.clone(),
            kind: match &v.kind {
                EnumVariantKind::Unit => EnumVariantKind::Unit,
                EnumVariantKind::Tuple(ts) => {
                    EnumVariantKind::Tuple(ts.iter().map(|&t| self.ty(t)).collect())
                }
                EnumVariantKind::Record(fs) => {
                    EnumVariantKind::Record(fs.iter().map(|fl| self.field(fl)).collect())
                }
            },
        }
    }

    pub(crate) fn block(&mut self, b: Idx<BlockData>) -> Idx<BlockData> {
        let src = self.src;
        let data = src.read(self.dst, |ast| &ast.blocks, b);
        self.rw.enter();
        let statements = data.statements.iter().map(|&s| self.stmt(s)).collect();
        self.rw.exit();
        let (span, braces) = match self.rw.stamp() {
            Some(span) => (span, None),
            None => (data.span, data.braces),
        };
        self.dst.blocks.alloc(BlockData {
            span,
            braces,
            statements,
        })
    }

    pub(crate) fn expr(&mut self, e: Idx<ExprData>) -> Idx<ExprData> {
        let src = self.src;
        let data = src.read(self.dst, |ast| &ast.exprs, e);
        if let ExprKind::Ident(id) = &data.kind {
            if let Some(arg) = self.rw.arg(id.name) {
                return clone_expr(self.dst, arg);
            }
        }
        let kind = self.expr_kind(&data.kind);
        self.dst.exprs.alloc(ExprData {
            span: self.rw.stamp().unwrap_or(data.span),
            kind,
        })
    }

    pub(crate) fn ty(&mut self, t: Idx<TypeExprData>) -> Idx<TypeExprData> {
        let src = self.src;
        let data = src.read(self.dst, |ast| &ast.types, t);
        let replaced = match &data.kind {
            TypeExprKind::Ident(id) => self.rw.ty_arg(id.name).map(|r| &self.dst.types[r]),
            _ => None,
        };
        let (kind, is_nullable) = match replaced {
            Some(r) => (r.kind.clone(), data.is_nullable || r.is_nullable),
            None => (self.ty_kind(&data.kind), data.is_nullable),
        };
        self.dst.types.alloc(TypeExprData {
            span: data.span,
            kind,
            is_nullable,
        })
    }

    fn stmt(&mut self, s: Idx<StatementData>) -> Idx<StatementData> {
        let src = self.src;
        let data = src.read(self.dst, |ast| &ast.stmts, s);
        let kind = match &data.kind {
            StatementKind::Expression(e) => StatementKind::Expression(self.expr(*e)),
            StatementKind::Use(u) => StatementKind::Use(u.clone()),
            StatementKind::Let(l) => {
                let ty = l.ty.map(|t| self.ty(t));
                let value = l.value.map(|e| self.expr(e));
                let mut binding = l.binding.clone();
                self.rw.bind_binding(&mut binding);
                StatementKind::Let(Let {
                    span: l.span,
                    keyword_span: l.keyword_span,
                    binding,
                    ty,
                    value,
                })
            }
            StatementKind::Static(st) => {
                let ty = st.ty.map(|t| self.ty(t));
                let value = self.expr(st.value);
                let mut name = st.name.clone();
                self.rw.bind(&mut name.name);
                StatementKind::Static(Static {
                    span: st.span,
                    keyword_span: st.keyword_span,
                    name,
                    ty,
                    value,
                })
            }
            StatementKind::NestedFn(f) => StatementKind::NestedFn(NestedFn {
                span: f.span,
                keyword_span: f.keyword_span,
                name: f.name.clone(),
                params: f.params.iter().map(|p| self.param(p)).collect(),
                params_span: f.params_span,
                return_ty: f.return_ty.map(|t| self.ty(t)),
                body: self.block(f.body),
            }),
            StatementKind::Return {
                keyword_span,
                value,
            } => StatementKind::Return {
                keyword_span: *keyword_span,
                value: value.map(|e| self.expr(e)),
            },
            StatementKind::Break { keyword_span } => StatementKind::Break {
                keyword_span: *keyword_span,
            },
            StatementKind::Continue { keyword_span } => StatementKind::Continue {
                keyword_span: *keyword_span,
            },
            StatementKind::For(f) => {
                let ty = f.ty.map(|t| self.ty(t));
                let iter = self.expr(f.iter);
                let mut var = f.var.clone();
                StatementKind::For(For {
                    span: f.span,
                    keyword_span: f.keyword_span,
                    body: self.scoped(f.body, |rw| rw.bind_binding(&mut var)),
                    var,
                    ty,
                    iter,
                })
            }
            StatementKind::While(w) => StatementKind::While(While {
                span: w.span,
                keyword_span: w.keyword_span,
                condition: self.expr(w.condition),
                body: self.block(w.body),
            }),
            StatementKind::WhileLet(w) => {
                let mut pattern = self.pattern(&w.pattern);
                let value = self.expr(w.value);
                StatementKind::WhileLet(WhileLet {
                    span: w.span,
                    keyword_span: w.keyword_span,
                    body: self.scoped(w.body, |rw| rw.bind_pattern(&mut pattern)),
                    pattern,
                    value,
                })
            }
            StatementKind::Defer(DeferKind::Block(b)) => {
                StatementKind::Defer(DeferKind::Block(self.block(*b)))
            }
            StatementKind::Defer(DeferKind::Expression(e)) => {
                StatementKind::Defer(DeferKind::Expression(self.expr(*e)))
            }
            StatementKind::LifecycleHook {
                keyword_span,
                name,
                body,
            } => StatementKind::LifecycleHook {
                keyword_span: *keyword_span,
                name: *name,
                body: self.block(*body),
            },
            StatementKind::Comment(c) => StatementKind::Comment(*c),
        };
        self.dst.stmts.alloc(StatementData {
            span: self.rw.stamp().unwrap_or(data.span),
            kind,
        })
    }

    fn decl(&self, id: &Ident) -> Ident {
        match self.rw.ty_arg(id.name).map(|r| &self.dst.types[r].kind) {
            Some(TypeExprKind::Ident(new)) => new.clone(),
            _ => id.clone(),
        }
    }

    /// Copy `body` in a scope that `bind` first adds binders to.
    fn scoped(&mut self, body: Idx<BlockData>, bind: impl FnOnce(&mut R)) -> Idx<BlockData> {
        self.rw.enter();
        bind(&mut self.rw);
        let body = self.block(body);
        self.rw.exit();
        body
    }

    /// An `ident`-fragment param in constructor position (`Name { .. }`,
    /// `Name::V`) takes the name of the identifier it is bound to.
    fn ctor(&self, id: &Ident) -> Ident {
        match self.rw.arg(id.name).map(|a| &self.dst.exprs[a].kind) {
            Some(ExprKind::Ident(arg)) => arg.clone(),
            _ => id.clone(),
        }
    }

    fn path(&self, path: &Path) -> Path {
        Path {
            from_root: path.from_root,
            segments: path.segments.iter().map(|id| self.ctor(id)).collect(),
        }
    }

    fn type_path(&self, path: &Path) -> Path {
        Path {
            from_root: path.from_root,
            segments: path.segments.iter().map(|id| self.decl(id)).collect(),
        }
    }

    fn call_args(&mut self, args: &[FunctionCallArg]) -> Vec<FunctionCallArg> {
        let src = self.src;
        let mut out = Vec::with_capacity(args.len());
        for a in args {
            let forwarded = match &src.read(self.dst, |ast| &ast.exprs, a.value).kind {
                ExprKind::Ident(id) if a.is_splat => self.rw.splat(id.name).map(<[_]>::to_vec),
                _ => None,
            };
            match forwarded {
                Some(values) => out.extend(values.into_iter().map(|v| FunctionCallArg {
                    name: None,
                    value: clone_expr(self.dst, v),
                    is_splat: false,
                    is_spread: false,
                })),
                None => out.push(FunctionCallArg {
                    name: a.name.clone(),
                    value: self.expr(a.value),
                    is_splat: a.is_splat,
                    is_spread: a.is_spread,
                }),
            }
        }
        out
    }

    fn expr_kind(&mut self, kind: &ExprKind) -> ExprKind {
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
            | ExprKind::AnaphoricIdent(_) => kind.clone(),

            ExprKind::Ident(id) => ExprKind::Ident(Ident {
                span: id.span,
                name: self.rw.resolve(id.name),
            }),
            ExprKind::QualifiedIdent { base, member } => ExprKind::QualifiedIdent {
                base: self.ctor(base),
                member: member.clone(),
            },
            ExprKind::Path { path, generic_args } => ExprKind::Path {
                path: self.path(path),
                generic_args: generic_args.iter().map(|t| self.ty(*t)).collect(),
            },
            ExprKind::PathStructLiteral {
                path,
                bases,
                fields,
            } => ExprKind::PathStructLiteral {
                path: self.path(path),
                bases: bases.iter().map(|b| self.expr(*b)).collect(),
                fields: fields
                    .iter()
                    .map(|(ns, v)| (ns.clone(), self.expr(*v)))
                    .collect(),
            },
            ExprKind::InterpolatedString(parts) => ExprKind::InterpolatedString(
                parts
                    .iter()
                    .map(|p| match p {
                        InterpolatedPart::Literal(l) => InterpolatedPart::Literal(*l),
                        InterpolatedPart::Expr(e, spec) => InterpolatedPart::Expr(
                            self.expr(*e),
                            spec.as_ref().map(|s| self.format_spec(s)),
                        ),
                    })
                    .collect(),
            ),
            ExprKind::Paren(x) => ExprKind::Paren(self.expr(*x)),
            ExprKind::As(x, t) => ExprKind::As(self.expr(*x), self.ty(*t)),
            ExprKind::Bitcast(x, t) => ExprKind::Bitcast(self.expr(*x), self.ty(*t)),
            ExprKind::ParenAscription(x, t) => {
                ExprKind::ParenAscription(self.expr(*x), self.ty(*t))
            }
            ExprKind::Not(x) => ExprKind::Not(self.expr(*x)),
            ExprKind::BitNot(x) => ExprKind::BitNot(self.expr(*x)),
            ExprKind::UnaryMinus(x) => ExprKind::UnaryMinus(self.expr(*x)),
            ExprKind::FieldAccess(x, id) => ExprKind::FieldAccess(self.expr(*x), id.clone()),
            ExprKind::SafeNavigation(x, id) => ExprKind::SafeNavigation(self.expr(*x), id.clone()),
            ExprKind::SafeNavigationLet {
                receiver,
                binding,
                body,
            } => {
                let receiver = self.expr(*receiver);
                let mut binding = binding.clone();
                ExprKind::SafeNavigationLet {
                    receiver,
                    body: self.scoped(*body, |rw| rw.bind(&mut binding.name)),
                    binding,
                }
            }
            ExprKind::Index(a, b) => ExprKind::Index(self.expr(*a), self.expr(*b)),
            ExprKind::QuestionMark(x) => ExprKind::QuestionMark(self.expr(*x)),
            ExprKind::PreIncrement(x) => ExprKind::PreIncrement(self.expr(*x)),
            ExprKind::PostIncrement(x) => ExprKind::PostIncrement(self.expr(*x)),
            ExprKind::PreDecrement(x) => ExprKind::PreDecrement(self.expr(*x)),
            ExprKind::PostDecrement(x) => ExprKind::PostDecrement(self.expr(*x)),
            ExprKind::Spread(x) => ExprKind::Spread(self.expr(*x)),
            ExprKind::YieldDelegate(x) => ExprKind::YieldDelegate(self.expr(*x)),
            ExprKind::ReturnExpr(o) => ExprKind::ReturnExpr(o.map(|e| self.expr(e))),
            ExprKind::Yield(o) => ExprKind::Yield(o.map(|e| self.expr(e))),

            ExprKind::FunctionCall {
                func,
                generic_args,
                args,
                syntax,
            } => ExprKind::FunctionCall {
                func: self.expr(*func),
                generic_args: generic_args.iter().map(|t| self.ty(*t)).collect(),
                args: self.call_args(args),
                syntax: syntax.clone(),
            },
            ExprKind::BinOp {
                left,
                op,
                op_span,
                right,
            } => ExprKind::BinOp {
                left: self.expr(*left),
                op: *op,
                op_span: *op_span,
                right: self.expr(*right),
            },
            ExprKind::Assign { target, value } => ExprKind::Assign {
                target: self.expr(*target),
                value: self.expr(*value),
            },
            ExprKind::ArrayLiteral(xs) => {
                ExprKind::ArrayLiteral(xs.iter().map(|x| self.expr(*x)).collect())
            }
            ExprKind::Tuple(xs) => ExprKind::Tuple(xs.iter().map(|x| self.expr(*x)).collect()),
            ExprKind::StructLiteral {
                name,
                bases,
                fields,
            } => ExprKind::StructLiteral {
                name: self.ctor(name),
                bases: bases.iter().map(|b| self.expr(*b)).collect(),
                fields: fields
                    .iter()
                    .map(|(ns, v)| (ns.clone(), self.expr(*v)))
                    .collect(),
            },
            ExprKind::EnumVariantRecord {
                enum_name,
                variant_name,
                fields,
            } => ExprKind::EnumVariantRecord {
                enum_name: self.ctor(enum_name),
                variant_name: variant_name.clone(),
                fields: fields
                    .iter()
                    .map(|(ns, v)| (ns.clone(), self.expr(*v)))
                    .collect(),
            },
            ExprKind::Block(b) => ExprKind::Block(self.block(*b)),
            ExprKind::Lambda {
                params,
                return_ty,
                body,
            } => {
                let mut params: Vec<LambdaParam> = params
                    .iter()
                    .map(|p| LambdaParam {
                        span: p.span,
                        name: p.name.clone(),
                        ty: p.ty.map(|t| self.ty(t)),
                    })
                    .collect();
                let return_ty = return_ty.map(|t| self.ty(t));
                let body = self.scoped(*body, |rw| {
                    params.iter_mut().for_each(|p| rw.bind(&mut p.name.name))
                });
                ExprKind::Lambda {
                    params,
                    return_ty,
                    body,
                }
            }
            ExprKind::Match { scrutinee, arms } => ExprKind::Match {
                scrutinee: self.expr(*scrutinee),
                arms: arms
                    .iter()
                    .map(|a| {
                        let mut pattern = self.pattern(&a.pattern);
                        self.rw.enter();
                        self.rw.bind_pattern(&mut pattern);
                        let guard = a.guard.map(|g| self.expr(g));
                        let body = self.block(a.body);
                        self.rw.exit();
                        MatchArm {
                            pattern,
                            guard,
                            body,
                        }
                    })
                    .collect(),
            },
            ExprKind::If {
                branches,
                else_branch,
            } => ExprKind::If {
                branches: branches
                    .iter()
                    .map(|(c, b)| (self.expr(*c), self.block(*b)))
                    .collect(),
                else_branch: else_branch.map(|b| self.block(b)),
            },
            ExprKind::IfLetChain {
                clauses,
                then_block,
                else_block,
            } => {
                // A clause sees the bindings of the clauses before it.
                self.rw.enter();
                let clauses = clauses
                    .iter()
                    .map(|c| match c {
                        IfLetClause::Binding { pattern, value } => {
                            let mut pattern = self.pattern(pattern);
                            let value = self.expr(*value);
                            self.rw.bind_pattern(&mut pattern);
                            IfLetClause::Binding { pattern, value }
                        }
                        IfLetClause::Condition(c) => IfLetClause::Condition(self.expr(*c)),
                    })
                    .collect();
                let then_block = self.block(*then_block);
                self.rw.exit();
                ExprKind::IfLetChain {
                    clauses,
                    then_block,
                    else_block: else_block.map(|b| self.block(b)),
                }
            }
        }
    }

    fn pattern(&mut self, p: &Pattern) -> Pattern {
        match p {
            Pattern::Wildcard => Pattern::Wildcard,
            Pattern::Ident(id) => Pattern::Ident(id.clone()),
            Pattern::Literal(e) => Pattern::Literal(self.expr(*e)),
            Pattern::Tuple(ps) => Pattern::Tuple(ps.iter().map(|p| self.pattern(p)).collect()),
            Pattern::EnumVariant {
                enum_name,
                variant_name,
                args,
            } => Pattern::EnumVariant {
                enum_name: enum_name.clone(),
                variant_name: variant_name.clone(),
                args: args
                    .as_ref()
                    .map(|ps| ps.iter().map(|p| self.pattern(p)).collect()),
            },
            Pattern::PathVariant { path, args } => Pattern::PathVariant {
                path: self.path(path),
                args: args
                    .as_ref()
                    .map(|ps| ps.iter().map(|p| self.pattern(p)).collect()),
            },
            Pattern::PathRecord { path, fields, rest } => Pattern::PathRecord {
                path: self.path(path),
                fields: fields
                    .iter()
                    .map(|(id, p)| (id.clone(), self.pattern(p)))
                    .collect(),
                rest: *rest,
            },
            Pattern::Record {
                enum_name,
                name,
                fields,
                rest,
            } => Pattern::Record {
                enum_name: enum_name.clone(),
                name: name.clone(),
                fields: fields
                    .iter()
                    .map(|(id, p)| (id.clone(), self.pattern(p)))
                    .collect(),
                rest: *rest,
            },
            Pattern::Range {
                start,
                end,
                inclusive,
            } => Pattern::Range {
                start: start.map(|e| self.expr(e)),
                end: end.map(|e| self.expr(e)),
                inclusive: *inclusive,
            },
            Pattern::Or(ps) => Pattern::Or(ps.iter().map(|p| self.pattern(p)).collect()),
        }
    }

    fn format_spec(&mut self, s: &FormatSpec) -> FormatSpec {
        FormatSpec {
            precision: s.precision,
            width: s.width,
            width_expr: s.width_expr.map(|e| self.expr(e)),
            precision_expr: s.precision_expr.map(|e| self.expr(e)),
            alignment: s.alignment,
            fill: s.fill,
            sign: s.sign,
            alternate: s.alternate,
            zero_pad: s.zero_pad,
            grouping: s.grouping,
            format_type: s.format_type,
        }
    }

    fn ty_kind(&mut self, kind: &TypeExprKind) -> TypeExprKind {
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
            | TypeExprKind::Ident(_) => kind.clone(),
            TypeExprKind::Path(path) => TypeExprKind::Path(self.type_path(path)),
            TypeExprKind::Ref(t) => TypeExprKind::Ref(self.ty(*t)),
            TypeExprKind::Array(t) => TypeExprKind::Array(self.ty(*t)),
            TypeExprKind::AppPath { ctor, args } => TypeExprKind::AppPath {
                ctor: self.type_path(ctor),
                args: args.iter().map(|t| self.ty(*t)).collect(),
            },
            TypeExprKind::App { ctor, args } => TypeExprKind::App {
                ctor: self.decl(ctor),
                args: args.iter().map(|t| self.ty(*t)).collect(),
            },
            TypeExprKind::Tuple(ts) => {
                TypeExprKind::Tuple(ts.iter().map(|t| self.ty(*t)).collect())
            }
            TypeExprKind::Function { params, ret } => TypeExprKind::Function {
                params: params.iter().map(|t| self.ty(*t)).collect(),
                ret: self.ty(*ret),
            },
        }
    }
}
