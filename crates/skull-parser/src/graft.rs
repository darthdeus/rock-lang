//! Cross-arena deep-copy: clone an AST subtree from one `Ast` into another.
//!
//! Macro expansion inlines a macro body (defined in module A's arena) at a call
//! site in module B's arena, so the template's nodes must be re-allocated into
//! the caller. This is the cross-`Ast` twin of `clone.rs` (which copies within a
//! single arena for compound-assign's read copy). Both exist because the borrow
//! checker can't alias one `Ast` as `&` source and `&mut` dest; the logic is
//! otherwise identical. Registry build (module -> template) and expansion
//! (template -> caller) both go through here.

use crate::ast::*;

/// Deep-copy an item (and all its arena children) from `src` into `dst`. Used
/// to inline a macro's `macro_items` template at an invocation site. Covers the
/// item kinds a macro body can emit (fn / struct / enum / newtype).
pub(crate) fn graft_item(src: &Ast, dst: &mut Ast, item: &Item) -> Item {
    let kind = match &item.kind {
        ItemKind::Function(f) => ItemKind::Function(graft_fn(src, dst, f)),
        ItemKind::Struct(s) => ItemKind::Struct(StructDef {
            span: s.span,
            keyword_span: s.keyword_span,
            name: s.name.clone(),
            generics: graft_generics(src, dst, &s.generics),
            fields: s
                .fields
                .iter()
                .map(|fl| graft_field(src, dst, fl))
                .collect(),
            is_value: s.is_value,
            is_nobox: s.is_nobox,
        }),
        ItemKind::Enum(e) => ItemKind::Enum(EnumDef {
            span: e.span,
            keyword_span: e.keyword_span,
            name: e.name.clone(),
            generics: graft_generics(src, dst, &e.generics),
            variants: e
                .variants
                .iter()
                .map(|v| graft_variant(src, dst, v))
                .collect(),
        }),
        ItemKind::Newtype(n) => ItemKind::Newtype(NewtypeDef {
            span: n.span,
            name: n.name.clone(),
            generics: graft_generics(src, dst, &n.generics),
            wrapped: graft_type(src, dst, n.wrapped),
        }),
        ItemKind::Global(g) => ItemKind::Global(GlobalDef {
            span: g.span,
            keyword_span: g.keyword_span,
            name: g.name.clone(),
            ty: g.ty.map(|t| graft_type(src, dst, t)),
            value: g.value.map(|e| graft_expr(src, dst, e)),
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

fn graft_fn(src: &Ast, dst: &mut Ast, f: &FunctionDef) -> FunctionDef {
    FunctionDef {
        span: f.span,
        keyword_span: f.keyword_span,
        name: f.name.clone(),
        generics: graft_generics(src, dst, &f.generics),
        params: f.params.iter().map(|p| graft_param(src, dst, p)).collect(),
        params_span: f.params_span,
        return_ty: f.return_ty.map(|t| graft_type(src, dst, t)),
        body: match &f.body {
            FunctionBody::Rock(b) => FunctionBody::Rock(graft_block(src, dst, *b)),
            FunctionBody::Inline(be, s) => FunctionBody::Inline(*be, *s),
            FunctionBody::ExternLua => FunctionBody::ExternLua,
            FunctionBody::ExternJs => FunctionBody::ExternJs,
            FunctionBody::Ffi(l) => FunctionBody::Ffi(*l),
        },
        is_macro: f.is_macro,
        is_coroutine: f.is_coroutine,
        is_synthetic: f.is_synthetic,
        json_field_for: f.json_field_for.clone(),
        macro_items: f
            .macro_items
            .iter()
            .map(|it| graft_item(src, dst, it))
            .collect(),
        receiver_surface: f.receiver_surface.clone(),
    }
}

fn graft_param(src: &Ast, dst: &mut Ast, p: &Param) -> Param {
    Param {
        span: p.span,
        name: p.name.clone(),
        ty: graft_type(src, dst, p.ty),
        default: p.default.map(|e| graft_expr(src, dst, e)),
        is_variadic: p.is_variadic,
        annotations: p.annotations.clone(),
    }
}

fn graft_generics(src: &Ast, dst: &mut Ast, gs: &[Generic]) -> Vec<Generic> {
    gs.iter()
        .map(|g| Generic {
            name: g.name.clone(),
            bound: g.bound.map(|t| graft_type(src, dst, t)),
            default: g.default.map(|t| graft_type(src, dst, t)),
        })
        .collect()
}

fn graft_field(src: &Ast, dst: &mut Ast, fl: &Field) -> Field {
    Field {
        span: fl.span,
        name: fl.name.clone(),
        ty: graft_type(src, dst, fl.ty),
        vis: fl.vis.clone(),
        set_private: fl.set_private,
        default: fl.default.map(|e| graft_expr(src, dst, e)),
        annotations: fl.annotations.clone(),
        is_embed: fl.is_embed,
    }
}

fn graft_variant(src: &Ast, dst: &mut Ast, v: &EnumVariant) -> EnumVariant {
    EnumVariant {
        span: v.span,
        name: v.name.clone(),
        kind: match &v.kind {
            EnumVariantKind::Unit => EnumVariantKind::Unit,
            EnumVariantKind::Tuple(ts) => {
                EnumVariantKind::Tuple(ts.iter().map(|&t| graft_type(src, dst, t)).collect())
            }
            EnumVariantKind::Record(fs) => {
                EnumVariantKind::Record(fs.iter().map(|fl| graft_field(src, dst, fl)).collect())
            }
        },
    }
}

pub(crate) fn graft_block(src: &Ast, dst: &mut Ast, b: Idx<BlockData>) -> Idx<BlockData> {
    let data = &src.blocks[b];
    let span = data.span;
    let braces = data.braces;
    let stmts: Vec<Idx<StatementData>> = data.statements.clone();
    let statements = stmts.into_iter().map(|s| graft_stmt(src, dst, s)).collect();
    dst.blocks.alloc(BlockData {
        span,
        braces,
        statements,
    })
}

pub(crate) fn graft_expr(src: &Ast, dst: &mut Ast, e: Idx<ExprData>) -> Idx<ExprData> {
    let span = src.exprs[e].span;
    let kind = graft_expr_kind(src, dst, &src.exprs[e].kind);
    dst.exprs.alloc(ExprData { span, kind })
}

pub(crate) fn graft_type(src: &Ast, dst: &mut Ast, t: Idx<TypeExprData>) -> Idx<TypeExprData> {
    let data = &src.types[t];
    let span = data.span;
    let is_nullable = data.is_nullable;
    let kind = graft_type_kind(src, dst, &data.kind);
    dst.types.alloc(TypeExprData {
        span,
        kind,
        is_nullable,
    })
}

fn graft_stmt(src: &Ast, dst: &mut Ast, s: Idx<StatementData>) -> Idx<StatementData> {
    let data = &src.stmts[s];
    let span = data.span;
    let kind = match &data.kind {
        StatementKind::Expression(e) => StatementKind::Expression(graft_expr(src, dst, *e)),
        StatementKind::Use(u) => StatementKind::Use(u.clone()),
        StatementKind::Let(l) => StatementKind::Let(Let {
            span: l.span,
            keyword_span: l.keyword_span,
            binding: l.binding.clone(),
            ty: l.ty.map(|t| graft_type(src, dst, t)),
            value: l.value.map(|e| graft_expr(src, dst, e)),
        }),
        StatementKind::Static(st) => StatementKind::Static(Static {
            span: st.span,
            keyword_span: st.keyword_span,
            name: st.name.clone(),
            ty: st.ty.map(|t| graft_type(src, dst, t)),
            value: graft_expr(src, dst, st.value),
        }),
        StatementKind::NestedFn(f) => StatementKind::NestedFn(NestedFn {
            span: f.span,
            keyword_span: f.keyword_span,
            name: f.name.clone(),
            params: f
                .params
                .iter()
                .map(|p| Param {
                    span: p.span,
                    name: p.name.clone(),
                    ty: graft_type(src, dst, p.ty),
                    default: p.default.map(|d| graft_expr(src, dst, d)),
                    is_variadic: p.is_variadic,
                    annotations: p.annotations.clone(),
                })
                .collect(),
            params_span: f.params_span,
            return_ty: f.return_ty.map(|t| graft_type(src, dst, t)),
            body: graft_block(src, dst, f.body),
        }),
        StatementKind::Return {
            keyword_span,
            value,
        } => StatementKind::Return {
            keyword_span: *keyword_span,
            value: value.map(|e| graft_expr(src, dst, e)),
        },
        StatementKind::Break { keyword_span } => StatementKind::Break {
            keyword_span: *keyword_span,
        },
        StatementKind::Continue { keyword_span } => StatementKind::Continue {
            keyword_span: *keyword_span,
        },
        StatementKind::For(f) => StatementKind::For(For {
            span: f.span,
            keyword_span: f.keyword_span,
            var: f.var.clone(),
            ty: f.ty.map(|t| graft_type(src, dst, t)),
            iter: graft_expr(src, dst, f.iter),
            body: graft_block(src, dst, f.body),
        }),
        StatementKind::While(w) => StatementKind::While(While {
            span: w.span,
            keyword_span: w.keyword_span,
            condition: graft_expr(src, dst, w.condition),
            body: graft_block(src, dst, w.body),
        }),
        StatementKind::WhileLet(w) => StatementKind::WhileLet(WhileLet {
            span: w.span,
            keyword_span: w.keyword_span,
            pattern: graft_pattern(src, dst, &w.pattern),
            value: graft_expr(src, dst, w.value),
            body: graft_block(src, dst, w.body),
        }),
        StatementKind::Defer(DeferKind::Block(b)) => {
            StatementKind::Defer(DeferKind::Block(graft_block(src, dst, *b)))
        }
        StatementKind::Defer(DeferKind::Expression(e)) => {
            StatementKind::Defer(DeferKind::Expression(graft_expr(src, dst, *e)))
        }
        StatementKind::LifecycleHook {
            keyword_span,
            name,
            body,
        } => StatementKind::LifecycleHook {
            keyword_span: *keyword_span,
            name: *name,
            body: graft_block(src, dst, *body),
        },
        StatementKind::Comment(c) => StatementKind::Comment(*c),
    };
    dst.stmts.alloc(StatementData { span, kind })
}

fn graft_expr_kind(src: &Ast, dst: &mut Ast, kind: &ExprKind) -> ExprKind {
    let ge = |dst: &mut Ast, e: Idx<ExprData>| graft_expr(src, dst, e);
    let gb = |dst: &mut Ast, b: Idx<BlockData>| graft_block(src, dst, b);
    let gt = |dst: &mut Ast, t: Idx<TypeExprData>| graft_type(src, dst, t);
    match kind {
        ExprKind::Null
        | ExprKind::Unit
        | ExprKind::Ident(_)
        | ExprKind::QualifiedIdent { .. }
        | ExprKind::Path { .. }
        | ExprKind::PathStructLiteral { .. }
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

        ExprKind::InterpolatedString(parts) => ExprKind::InterpolatedString(
            parts
                .iter()
                .map(|p| match p {
                    InterpolatedPart::Literal(l) => InterpolatedPart::Literal(*l),
                    InterpolatedPart::Expr(e, spec) => InterpolatedPart::Expr(
                        ge(dst, *e),
                        spec.as_ref().map(|s| graft_format_spec(src, dst, s)),
                    ),
                })
                .collect(),
        ),
        ExprKind::Paren(x) => ExprKind::Paren(ge(dst, *x)),
        ExprKind::As(x, t) => ExprKind::As(ge(dst, *x), gt(dst, *t)),
        ExprKind::Bitcast(x, t) => ExprKind::Bitcast(ge(dst, *x), gt(dst, *t)),
        ExprKind::TypeAscription(x, t) => ExprKind::TypeAscription(ge(dst, *x), gt(dst, *t)),
        ExprKind::ParenAscription(x, t) => ExprKind::ParenAscription(ge(dst, *x), gt(dst, *t)),
        ExprKind::Not(x) => ExprKind::Not(ge(dst, *x)),
        ExprKind::BitNot(x) => ExprKind::BitNot(ge(dst, *x)),
        ExprKind::UnaryMinus(x) => ExprKind::UnaryMinus(ge(dst, *x)),
        ExprKind::FieldAccess(x, id) => ExprKind::FieldAccess(ge(dst, *x), id.clone()),
        ExprKind::SafeNavigation(x, id) => ExprKind::SafeNavigation(ge(dst, *x), id.clone()),
        ExprKind::SafeNavigationLet {
            receiver,
            binding,
            body,
        } => ExprKind::SafeNavigationLet {
            receiver: ge(dst, *receiver),
            binding: binding.clone(),
            body: gb(dst, *body),
        },
        ExprKind::Index(a, b) => ExprKind::Index(ge(dst, *a), ge(dst, *b)),
        ExprKind::QuestionMark(x) => ExprKind::QuestionMark(ge(dst, *x)),
        ExprKind::PreIncrement(x) => ExprKind::PreIncrement(ge(dst, *x)),
        ExprKind::PostIncrement(x) => ExprKind::PostIncrement(ge(dst, *x)),
        ExprKind::PreDecrement(x) => ExprKind::PreDecrement(ge(dst, *x)),
        ExprKind::PostDecrement(x) => ExprKind::PostDecrement(ge(dst, *x)),
        ExprKind::Spread(x) => ExprKind::Spread(ge(dst, *x)),
        ExprKind::YieldDelegate(x) => ExprKind::YieldDelegate(ge(dst, *x)),
        ExprKind::ReturnExpr(o) => ExprKind::ReturnExpr(o.map(|e| ge(dst, e))),
        ExprKind::Yield(o) => ExprKind::Yield(o.map(|e| ge(dst, e))),

        ExprKind::FunctionCall {
            func,
            generic_args,
            args,
            syntax,
        } => ExprKind::FunctionCall {
            func: ge(dst, *func),
            generic_args: generic_args
                .iter()
                .map(|t| graft_type(src, dst, *t))
                .collect(),
            args: args
                .iter()
                .map(|a| FunctionCallArg {
                    name: a.name.clone(),
                    value: ge(dst, a.value),
                    is_splat: a.is_splat,
                    is_spread: a.is_spread,
                })
                .collect(),
            syntax: syntax.clone(),
        },
        ExprKind::BinOp { left, op, right } => ExprKind::BinOp {
            left: ge(dst, *left),
            op: *op,
            right: ge(dst, *right),
        },
        ExprKind::Assign {
            target,
            value,
            compound_op,
        } => ExprKind::Assign {
            target: ge(dst, *target),
            value: ge(dst, *value),
            compound_op: *compound_op,
        },
        ExprKind::ArrayLiteral(xs) => {
            ExprKind::ArrayLiteral(xs.iter().map(|x| ge(dst, *x)).collect())
        }
        ExprKind::Tuple(xs) => ExprKind::Tuple(xs.iter().map(|x| ge(dst, *x)).collect()),
        ExprKind::StructLiteral {
            name,
            bases,
            fields,
        } => ExprKind::StructLiteral {
            name: name.clone(),
            bases: bases.iter().map(|b| ge(dst, *b)).collect(),
            fields: fields
                .iter()
                .map(|(ns, v)| (ns.clone(), ge(dst, *v)))
                .collect(),
        },
        ExprKind::EnumVariantRecord {
            enum_name,
            variant_name,
            fields,
        } => ExprKind::EnumVariantRecord {
            enum_name: enum_name.clone(),
            variant_name: variant_name.clone(),
            fields: fields
                .iter()
                .map(|(ns, v)| (ns.clone(), ge(dst, *v)))
                .collect(),
        },
        ExprKind::Block(b) => ExprKind::Block(gb(dst, *b)),
        ExprKind::Lambda {
            params,
            return_ty,
            body,
        } => ExprKind::Lambda {
            params: params
                .iter()
                .map(|p| LambdaParam {
                    span: p.span,
                    name: p.name.clone(),
                    ty: p.ty.map(|t| gt(dst, t)),
                })
                .collect(),
            return_ty: return_ty.map(|t| gt(dst, t)),
            body: gb(dst, *body),
        },
        ExprKind::Match { scrutinee, arms } => ExprKind::Match {
            scrutinee: ge(dst, *scrutinee),
            arms: arms
                .iter()
                .map(|a| MatchArm {
                    pattern: graft_pattern(src, dst, &a.pattern),
                    guard: a.guard.map(|g| ge(dst, g)),
                    body: gb(dst, a.body),
                })
                .collect(),
        },
        ExprKind::If {
            branches,
            else_branch,
        } => ExprKind::If {
            branches: branches
                .iter()
                .map(|(c, b)| (ge(dst, *c), gb(dst, *b)))
                .collect(),
            else_branch: else_branch.map(|b| gb(dst, b)),
        },
        ExprKind::IfLet {
            pattern,
            value,
            then_block,
            else_block,
        } => ExprKind::IfLet {
            pattern: graft_pattern(src, dst, pattern),
            value: ge(dst, *value),
            then_block: gb(dst, *then_block),
            else_block: else_block.map(|b| gb(dst, b)),
        },
        ExprKind::IfLetChain {
            clauses,
            then_block,
            else_block,
        } => ExprKind::IfLetChain {
            clauses: clauses
                .iter()
                .map(|c| match c {
                    IfLetClause::Binding { pattern, value } => IfLetClause::Binding {
                        pattern: graft_pattern(src, dst, pattern),
                        value: ge(dst, *value),
                    },
                    IfLetClause::Condition(c) => IfLetClause::Condition(ge(dst, *c)),
                })
                .collect(),
            then_block: gb(dst, *then_block),
            else_block: else_block.map(|b| gb(dst, b)),
        },
    }
}

fn graft_pattern(src: &Ast, dst: &mut Ast, p: &Pattern) -> Pattern {
    match p {
        Pattern::Wildcard => Pattern::Wildcard,
        Pattern::Ident(id) => Pattern::Ident(id.clone()),
        Pattern::Literal(e) => Pattern::Literal(graft_expr(src, dst, *e)),
        Pattern::Tuple(ps) => {
            Pattern::Tuple(ps.iter().map(|p| graft_pattern(src, dst, p)).collect())
        }
        Pattern::EnumVariant {
            enum_name,
            variant_name,
            args,
        } => Pattern::EnumVariant {
            enum_name: enum_name.clone(),
            variant_name: variant_name.clone(),
            args: args
                .as_ref()
                .map(|ps| ps.iter().map(|p| graft_pattern(src, dst, p)).collect()),
        },
        Pattern::PathVariant { path, args } => Pattern::PathVariant {
            path: path.clone(),
            args: args
                .as_ref()
                .map(|ps| ps.iter().map(|p| graft_pattern(src, dst, p)).collect()),
        },
        Pattern::PathRecord { path, fields, rest } => Pattern::PathRecord {
            path: path.clone(),
            fields: fields
                .iter()
                .map(|(id, p)| (id.clone(), graft_pattern(src, dst, p)))
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
                .map(|(id, p)| (id.clone(), graft_pattern(src, dst, p)))
                .collect(),
            rest: *rest,
        },
        Pattern::Range {
            start,
            end,
            inclusive,
        } => Pattern::Range {
            start: start.map(|e| graft_expr(src, dst, e)),
            end: end.map(|e| graft_expr(src, dst, e)),
            inclusive: *inclusive,
        },
        Pattern::Or(ps) => Pattern::Or(ps.iter().map(|p| graft_pattern(src, dst, p)).collect()),
    }
}

/// Cross-arena copy of a format spec: re-allocate the dynamic width/precision
/// sub-expressions into `dst`, copy the scalar fields.
fn graft_format_spec(src: &Ast, dst: &mut Ast, s: &FormatSpec) -> FormatSpec {
    FormatSpec {
        precision: s.precision,
        width: s.width,
        width_expr: s.width_expr.map(|e| graft_expr(src, dst, e)),
        precision_expr: s.precision_expr.map(|e| graft_expr(src, dst, e)),
        alignment: s.alignment,
        fill: s.fill,
        sign: s.sign,
        alternate: s.alternate,
        zero_pad: s.zero_pad,
        grouping: s.grouping,
        format_type: s.format_type,
    }
}

fn graft_type_kind(src: &Ast, dst: &mut Ast, kind: &TypeExprKind) -> TypeExprKind {
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
        | TypeExprKind::Ident(_)
        | TypeExprKind::Path(_) => kind.clone(),
        TypeExprKind::Ref(t) => TypeExprKind::Ref(graft_type(src, dst, *t)),
        TypeExprKind::Array(t) => TypeExprKind::Array(graft_type(src, dst, *t)),
        TypeExprKind::AppPath { ctor, args } => TypeExprKind::AppPath {
            ctor: ctor.clone(),
            args: args.iter().map(|t| graft_type(src, dst, *t)).collect(),
        },
        TypeExprKind::App { ctor, args } => TypeExprKind::App {
            ctor: ctor.clone(),
            args: args.iter().map(|t| graft_type(src, dst, *t)).collect(),
        },
        TypeExprKind::Tuple(ts) => {
            TypeExprKind::Tuple(ts.iter().map(|t| graft_type(src, dst, *t)).collect())
        }
        TypeExprKind::Function { params, ret } => TypeExprKind::Function {
            params: params.iter().map(|t| graft_type(src, dst, *t)).collect(),
            ret: graft_type(src, dst, *ret),
        },
    }
}
