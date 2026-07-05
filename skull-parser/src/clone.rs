//! Deep-copy of AST subtrees WITHIN a single `Ast` arena.
//!
//! Used by compound-assign's read copy of the lvalue (`a += b` -> `a = a + b`
//! needs `a` in two distinct nodes). rock mutates `Box`-AST in place and
//! re-mints `AstNodeId`s; skull has no node id (the `Idx` IS identity), so
//! duplication = re-allocating every node with remapped child indices (the
//! `reify` pattern from `bone/CLAUDE.md`).
//!
//! `graft.rs` is the CROSS-arena twin (source `&Ast` -> dest `&mut Ast`), used
//! by macro expansion to inline a template body into a caller. The two can't be
//! one function: the borrow checker forbids aliasing one `Ast` as `&` and `&mut`
//! at once, so within-arena (clone-out-then-recurse) and cross-arena (read src /
//! alloc dst) are necessarily separate. Keep them in sync.

use crate::ast::*;

/// Deep-copy the expression subtree rooted at `e`, returning the new root.
/// Every descendant Expr/Block/Stmt/Type node is freshly allocated.
pub fn clone_expr(ast: &mut Ast, e: Idx<ExprData>) -> Idx<ExprData> {
    let data = ast.exprs[e].clone();
    let kind = clone_expr_kind(ast, data.kind);
    ast.exprs.alloc(ExprData { span: data.span, kind })
}

pub fn clone_block(ast: &mut Ast, b: Idx<BlockData>) -> Idx<BlockData> {
    let data = ast.blocks[b].clone();
    let statements = data.statements.iter().map(|&s| clone_stmt(ast, s)).collect();
    ast.blocks.alloc(BlockData { span: data.span, statements })
}

pub fn clone_type(ast: &mut Ast, t: Idx<TypeExprData>) -> Idx<TypeExprData> {
    let data = ast.types[t].clone();
    let kind = clone_type_kind(ast, data.kind);
    ast.types.alloc(TypeExprData { span: data.span, kind, is_nullable: data.is_nullable })
}

fn clone_stmt(ast: &mut Ast, s: Idx<StatementData>) -> Idx<StatementData> {
    let data = ast.stmts[s].clone();
    let kind = match data.kind {
        StatementKind::Expression(e) => StatementKind::Expression(clone_expr(ast, e)),
        StatementKind::Let(l) => StatementKind::Let(Let {
            span: l.span,
            keyword_span: l.keyword_span,
            binding: l.binding,
            ty: l.ty.map(|t| clone_type(ast, t)),
            value: l.value.map(|e| clone_expr(ast, e)),
        }),
        StatementKind::Return { keyword_span, value } => StatementKind::Return {
            keyword_span,
            value: value.map(|e| clone_expr(ast, e)),
        },
        StatementKind::Break { keyword_span } => StatementKind::Break { keyword_span },
        StatementKind::Continue { keyword_span } => StatementKind::Continue { keyword_span },
        StatementKind::For(f) => StatementKind::For(For {
            span: f.span,
            keyword_span: f.keyword_span,
            var: f.var,
            ty: f.ty.map(|t| clone_type(ast, t)),
            iter: clone_expr(ast, f.iter),
            body: clone_block(ast, f.body),
        }),
        StatementKind::While(w) => StatementKind::While(While {
            span: w.span,
            keyword_span: w.keyword_span,
            condition: clone_expr(ast, w.condition),
            body: clone_block(ast, w.body),
        }),
        StatementKind::WhileLet(w) => StatementKind::WhileLet(WhileLet {
            span: w.span,
            keyword_span: w.keyword_span,
            pattern: clone_pattern(ast, w.pattern),
            value: clone_expr(ast, w.value),
            body: clone_block(ast, w.body),
        }),
        StatementKind::Defer(DeferKind::Block(b)) => StatementKind::Defer(DeferKind::Block(clone_block(ast, b))),
        StatementKind::Defer(DeferKind::Expression(e)) => {
            StatementKind::Defer(DeferKind::Expression(clone_expr(ast, e)))
        }
        StatementKind::LifecycleHook { keyword_span, name, body } => StatementKind::LifecycleHook {
            keyword_span,
            name,
            body: clone_block(ast, body),
        },
        StatementKind::Comment(c) => StatementKind::Comment(c),
    };
    ast.stmts.alloc(StatementData { span: data.span, kind, is_global_write: data.is_global_write })
}

fn clone_expr_kind(ast: &mut Ast, kind: ExprKind) -> ExprKind {
    let ce = |ast: &mut Ast, e: Idx<ExprData>| clone_expr(ast, e);
    let cb = |ast: &mut Ast, b: Idx<BlockData>| clone_block(ast, b);
    let ct = |ast: &mut Ast, t: Idx<TypeExprData>| clone_type(ast, t);
    match kind {
        ExprKind::Null
        | ExprKind::Unit
        | ExprKind::Ident(_)
        | ExprKind::QualifiedIdent { .. }
        | ExprKind::Int(_, _)
        | ExprKind::Float(_)
        | ExprKind::String(_)
        | ExprKind::Bool(_)
        | ExprKind::Hole(_, _)
        | ExprKind::BreakExpr
        | ExprKind::ContinueExpr
        | ExprKind::SiblingFieldRef(_)
        | ExprKind::FunctionRef(_)
        | ExprKind::AnaphoricIdent(_) => kind,

        ExprKind::InterpolatedString(parts) => ExprKind::InterpolatedString(
            parts
                .into_iter()
                .map(|p| match p {
                    InterpolatedPart::Literal(l) => InterpolatedPart::Literal(l),
                    InterpolatedPart::Expr(e, spec) => {
                        InterpolatedPart::Expr(ce(ast, e), spec.map(|s| clone_format_spec(ast, s)))
                    }
                })
                .collect(),
        ),
        ExprKind::Paren(x) => ExprKind::Paren(ce(ast, x)),
        ExprKind::As(x, t) => ExprKind::As(ce(ast, x), ct(ast, t)),
        ExprKind::TypeAscription(x, t) => ExprKind::TypeAscription(ce(ast, x), ct(ast, t)),
        ExprKind::Not(x) => ExprKind::Not(ce(ast, x)),
        ExprKind::BitNot(x) => ExprKind::BitNot(ce(ast, x)),
        ExprKind::UnaryMinus(x) => ExprKind::UnaryMinus(ce(ast, x)),
        ExprKind::FieldAccess(x, id) => ExprKind::FieldAccess(ce(ast, x), id),
        ExprKind::SafeNavigation(x, id) => ExprKind::SafeNavigation(ce(ast, x), id),
        ExprKind::SafeNavigationLet { receiver, binding, body } => ExprKind::SafeNavigationLet {
            receiver: ce(ast, receiver),
            binding,
            body: cb(ast, body),
        },
        ExprKind::Index(a, b) => ExprKind::Index(ce(ast, a), ce(ast, b)),
        ExprKind::QuestionMark(x) => ExprKind::QuestionMark(ce(ast, x)),
        ExprKind::PreIncrement(x) => ExprKind::PreIncrement(ce(ast, x)),
        ExprKind::PostIncrement(x) => ExprKind::PostIncrement(ce(ast, x)),
        ExprKind::PreDecrement(x) => ExprKind::PreDecrement(ce(ast, x)),
        ExprKind::PostDecrement(x) => ExprKind::PostDecrement(ce(ast, x)),
        ExprKind::Spread(x) => ExprKind::Spread(ce(ast, x)),
        ExprKind::YieldDelegate(x) => ExprKind::YieldDelegate(ce(ast, x)),
        ExprKind::ReturnExpr(o) => ExprKind::ReturnExpr(o.map(|e| ce(ast, e))),
        ExprKind::Yield(o) => ExprKind::Yield(o.map(|e| ce(ast, e))),

        ExprKind::FunctionCall { func, args, syntax } => ExprKind::FunctionCall {
            func: ce(ast, func),
            args: args
                .into_iter()
                .map(|a| FunctionCallArg {
                    name: a.name,
                    value: ce(ast, a.value),
                    is_splat: a.is_splat,
                })
                .collect(),
            syntax,
        },
        ExprKind::BinOp { left, op, right } => ExprKind::BinOp {
            left: ce(ast, left),
            op,
            right: ce(ast, right),
        },
        ExprKind::Assign { target, value, compound_op } => ExprKind::Assign {
            target: ce(ast, target),
            value: ce(ast, value),
            compound_op,
        },
        ExprKind::ArrayLiteral(xs) => ExprKind::ArrayLiteral(xs.into_iter().map(|x| ce(ast, x)).collect()),
        ExprKind::NativeArrayLiteral(xs) => {
            ExprKind::NativeArrayLiteral(xs.into_iter().map(|x| ce(ast, x)).collect())
        }
        ExprKind::Tuple(xs) => ExprKind::Tuple(xs.into_iter().map(|x| ce(ast, x)).collect()),
        ExprKind::StructLiteral { name, fields } => ExprKind::StructLiteral {
            name,
            fields: fields.into_iter().map(|(ns, v)| (ns, ce(ast, v))).collect(),
        },
        ExprKind::EnumVariantRecord { enum_name, variant_name, fields } => ExprKind::EnumVariantRecord {
            enum_name,
            variant_name,
            fields: fields.into_iter().map(|(ns, v)| (ns, ce(ast, v))).collect(),
        },
        ExprKind::Block(b) => ExprKind::Block(cb(ast, b)),
        ExprKind::Lambda { params, return_ty, body } => ExprKind::Lambda {
            params: params
                .into_iter()
                .map(|p| LambdaParam {
                    span: p.span,
                    name: p.name,
                    ty: p.ty.map(|t| ct(ast, t)),
                })
                .collect(),
            return_ty: return_ty.map(|t| ct(ast, t)),
            body: cb(ast, body),
        },
        ExprKind::Match { scrutinee, arms } => ExprKind::Match {
            scrutinee: ce(ast, scrutinee),
            arms: arms
                .into_iter()
                .map(|a| MatchArm {
                    pattern: clone_pattern(ast, a.pattern),
                    guard: a.guard.map(|g| ce(ast, g)),
                    body: cb(ast, a.body),
                })
                .collect(),
        },
        ExprKind::If { branches, else_branch } => ExprKind::If {
            branches: branches.into_iter().map(|(c, b)| (ce(ast, c), cb(ast, b))).collect(),
            else_branch: else_branch.map(|b| cb(ast, b)),
        },
        ExprKind::IfLet { pattern, value, then_block, else_block } => ExprKind::IfLet {
            pattern: clone_pattern(ast, pattern),
            value: ce(ast, value),
            then_block: cb(ast, then_block),
            else_block: else_block.map(|b| cb(ast, b)),
        },
        ExprKind::IfLetChain { clauses, then_block, else_block } => ExprKind::IfLetChain {
            clauses: clauses
                .into_iter()
                .map(|c| match c {
                    IfLetClause::Binding { pattern, value } => IfLetClause::Binding {
                        pattern: clone_pattern(ast, pattern),
                        value: ce(ast, value),
                    },
                    IfLetClause::Condition(c) => IfLetClause::Condition(ce(ast, c)),
                })
                .collect(),
            then_block: cb(ast, then_block),
            else_block: else_block.map(|b| cb(ast, b)),
        },
    }
}

/// Re-allocate a format spec's dynamic width/precision sub-expressions into the
/// same arena (the scalar fields copy as-is).
fn clone_format_spec(ast: &mut Ast, s: FormatSpec) -> FormatSpec {
    FormatSpec {
        width_expr: s.width_expr.map(|e| clone_expr(ast, e)),
        precision_expr: s.precision_expr.map(|e| clone_expr(ast, e)),
        ..s
    }
}

fn clone_pattern(ast: &mut Ast, p: Pattern) -> Pattern {
    match p {
        Pattern::Wildcard => Pattern::Wildcard,
        Pattern::Ident(id) => Pattern::Ident(id),
        Pattern::Literal(e) => Pattern::Literal(clone_expr(ast, e)),
        Pattern::Tuple(ps) => Pattern::Tuple(ps.into_iter().map(|p| clone_pattern(ast, p)).collect()),
        Pattern::EnumVariant { enum_name, variant_name, args } => Pattern::EnumVariant {
            enum_name,
            variant_name,
            args: args.map(|ps| ps.into_iter().map(|p| clone_pattern(ast, p)).collect()),
        },
        Pattern::Record { enum_name, name, fields, rest } => Pattern::Record {
            enum_name,
            name,
            fields: fields.into_iter().map(|(id, p)| (id, clone_pattern(ast, p))).collect(),
            rest,
        },
        Pattern::Range { start, end, inclusive } => Pattern::Range {
            start: start.map(|e| clone_expr(ast, e)),
            end: end.map(|e| clone_expr(ast, e)),
            inclusive,
        },
        Pattern::Or(ps) => Pattern::Or(ps.into_iter().map(|p| clone_pattern(ast, p)).collect()),
    }
}

fn clone_type_kind(ast: &mut Ast, kind: TypeExprKind) -> TypeExprKind {
    match kind {
        TypeExprKind::Any
        | TypeExprKind::Unit
        | TypeExprKind::String
        | TypeExprKind::Int
        | TypeExprKind::Float
        | TypeExprKind::Bool
        | TypeExprKind::Hole(_, _)
        | TypeExprKind::Ident(_) => kind,
        TypeExprKind::Array(t) => TypeExprKind::Array(clone_type(ast, t)),
        TypeExprKind::App { ctor, args } => TypeExprKind::App {
            ctor,
            args: args.into_iter().map(|t| clone_type(ast, t)).collect(),
        },
        TypeExprKind::Tuple(ts) => TypeExprKind::Tuple(ts.into_iter().map(|t| clone_type(ast, t)).collect()),
        TypeExprKind::Function { params, ret } => TypeExprKind::Function {
            params: params.into_iter().map(|t| clone_type(ast, t)).collect(),
            ret: clone_type(ast, ret),
        },
    }
}
