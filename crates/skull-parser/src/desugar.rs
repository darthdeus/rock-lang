//! "Desugaring" on skull-parser's arena AST. The only remaining transform is
//! item-level newtype-operator synthesis (`synth_newtype_operators`); there is no
//! per-expression walk. Every expression desugar now lives in bone's `lower.rs`,
//! consuming the raw AST directly:
//!   - placeholder lambda (`_.f`, `_ > 0`  ->  `p => p.f`)  [D1]
//!   - compound-assign (`a += b`)  [D2]
//!   - `^name` anaphoric (resolves as a caller-scope ident)
//! Never desugared at all: smart for-loop `.iter()` insertion [D3] (no game
//! triggers it) and `.type_info` -> `type_of(obj)` [D4] (`type_of` is no
//! fn/builtin). The `skull-cook-diff` oracle skips D1/D2 on rock's side
//! (`SyntaxDesugarPass::raw_compound_assign`) so it still byte-compares the rest.
//!
//! `child_exprs` / `child_blocks` remain here as the shared structural
//! child-edge enumeration used by `macro_expand`.

use crate::ast::*;
use rock_core::Span;
use smallvec::SmallVec;
use ustr::Ustr;

/// Direct child-edge lists are transient (collected, iterated, dropped) and
/// almost always 0-2 elements, so an inline `SmallVec` keeps the common case
/// off the heap. The collect-into-a-buffer step is load-bearing: it releases
/// the `&Ast` borrow so the caller can recurse with `&mut Ast`.
pub(crate) type ExprEdges = SmallVec<[Idx<ExprData>; 4]>;
pub(crate) type BlockEdges = SmallVec<[Idx<BlockData>; 2]>;

/// "Desugar" a parsed module in place. The only remaining transform is
/// item-level newtype-operator synthesis: every expression desugar (placeholder
/// `_.f` [D1], compound-assign [D2], `^name` anaphoric, `.type_info` [D4]) is now
/// handled directly in bone's `lower.rs` on the raw AST, so there is no
/// per-expression walk here. Idempotent on already-cooked input.
pub fn desugar(ast: &mut Ast) {
    synth_newtype_operators(ast);
}

/// Append `@pure @inline fn __op_*(a: NT, b: NT)` operators for every newtype
/// over a primitive (int/float/string), mirroring rock's parser-side
/// `synth_newtype_operator_items`. Newtypes are runtime-transparent, so each body
/// is the raw Lua op over the unwrapped `a`/`b`. Self-returning arithmetic ops
/// declare the newtype as their return; comparisons declare `bool`. Generic or
/// non-primitive-wrapped newtypes derive nothing. Marked `is_synthetic` so the
/// cook oracle compares user surface only.
fn synth_newtype_operators(ast: &mut Ast) {
    // (op stem, lua body over raw a/b, returns Self else bool)
    let int_specs: &[(&str, &str, bool)] = &[
        ("__op_add", "a + b", true),
        ("__op_sub", "a - b", true),
        ("__op_mul", "a * b", true),
        ("__op_div", "math.floor(a / b)", true),
        ("__op_mod", "a % b", true),
        ("__op_lt", "a < b", false),
        ("__op_le", "a <= b", false),
        ("__op_gt", "a > b", false),
        ("__op_ge", "a >= b", false),
    ];
    let float_specs: &[(&str, &str, bool)] = &[
        ("__op_add", "a + b", true),
        ("__op_sub", "a - b", true),
        ("__op_mul", "a * b", true),
        ("__op_div", "a / b", true),
        ("__op_mod", "a % b", true),
        ("__op_lt", "a < b", false),
        ("__op_le", "a <= b", false),
        ("__op_gt", "a > b", false),
        ("__op_ge", "a >= b", false),
    ];
    let string_specs: &[(&str, &str, bool)] = &[("__op_add", "a .. b", true)];

    // Collect targets first — the synthesis allocs into the type arena, so it
    // can't hold an `items` borrow while building.
    let mut targets: Vec<(Ustr, Span, &[(&str, &str, bool)])> = Vec::new();
    for item in &ast.items {
        let ItemKind::Newtype(nt) = &item.kind else { continue };
        if !nt.generics.is_empty() {
            continue;
        }
        let specs = match ast.types[nt.wrapped].kind {
            TypeExprKind::Int => int_specs,
            TypeExprKind::Float => float_specs,
            TypeExprKind::String => string_specs,
            _ => continue,
        };
        targets.push((nt.name.name, nt.span, specs));
    }

    for (nt_name, sp, specs) in targets {
        for &(stem, inner, ret_self) in specs {
            let pa_ty = ast.ty(sp, TypeExprKind::Ident(Ident { span: sp, name: nt_name }), false);
            let pb_ty = ast.ty(sp, TypeExprKind::Ident(Ident { span: sp, name: nt_name }), false);
            let return_ty = if ret_self {
                ast.ty(sp, TypeExprKind::Ident(Ident { span: sp, name: nt_name }), false)
            } else {
                ast.ty(sp, TypeExprKind::Bool, false)
            };
            let mk_param = |ty, nm: &str| Param {
                span: sp,
                name: Ident { span: sp, name: Ustr::from(nm) },
                ty,
                default: None,
                is_variadic: false,
            };
            let mk_anno = |nm: &str| Annotation {
                span: sp,
                name: Ident { span: sp, name: Ustr::from(nm) },
                args: Vec::new(),
            };
            ast.items.push(Item {
                annotations: vec![mk_anno("pure"), mk_anno("inline")],
                span: sp,
                kind: ItemKind::Function(FunctionDef {
                    span: sp,
                    keyword_span: None,
                    name: Ident { span: sp, name: Ustr::from(stem) },
                    generics: Vec::new(),
                    params: vec![mk_param(pa_ty, "a"), mk_param(pb_ty, "b")],
                    return_ty: Some(return_ty),
                    body: FunctionBody::Lua(Ustr::from(format!("return {inner}").as_str())),
                    is_macro: false,
                    is_coroutine: false,
                    is_synthetic: true,
                    macro_items: Vec::new(),
                }),
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Structural child enumeration (shared by cook driver + rename)
// ---------------------------------------------------------------------------

/// Direct child `Expr` edges of `e` (not descending through blocks — those are
/// returned by `child_blocks`).
pub(crate) fn child_exprs(ast: &Ast, e: Idx<ExprData>) -> ExprEdges {
    let mut out = ExprEdges::new();
    match &ast.exprs[e].kind {
        ExprKind::Null
        | ExprKind::Unit
        | ExprKind::Hole(_, _)
        | ExprKind::Ident(_)
        | ExprKind::QualifiedIdent { .. }
        | ExprKind::Int(_, _)
        | ExprKind::Float(_)
        | ExprKind::String(_)
        | ExprKind::Bool(_)
        | ExprKind::BreakExpr
        | ExprKind::ContinueExpr
        | ExprKind::SiblingFieldRef(_)
        | ExprKind::FunctionRef(_)
        | ExprKind::AnaphoricIdent(_) => {}

        ExprKind::Paren(x)
        | ExprKind::As(x, _)
        | ExprKind::TypeAscription(x, _)
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
        | ExprKind::YieldDelegate(x) => out.push(*x),

        ExprKind::Index(a, b) => {
            out.push(*a);
            out.push(*b);
        }
        ExprKind::BinOp { left, right, .. } => {
            out.push(*left);
            out.push(*right);
        }
        ExprKind::Assign { target, value, .. } => {
            out.push(*target);
            out.push(*value);
        }
        ExprKind::FunctionCall { func, args, .. } => {
            out.push(*func);
            out.extend(args.iter().map(|a| a.value));
        }
        ExprKind::ArrayLiteral(xs) | ExprKind::NativeArrayLiteral(xs) | ExprKind::Tuple(xs) => {
            out.extend(xs.iter().copied());
        }
        ExprKind::StructLiteral { fields, .. } | ExprKind::EnumVariantRecord { fields, .. } => {
            out.extend(fields.iter().map(|(_, v)| *v));
        }
        ExprKind::ReturnExpr(o) | ExprKind::Yield(o) => out.extend(o.iter().copied()),
        ExprKind::SafeNavigationLet { receiver, .. } => out.push(*receiver),
        ExprKind::IfLet { value, .. } => out.push(*value),
        ExprKind::Match { scrutinee, arms } => {
            out.push(*scrutinee);
            out.extend(arms.iter().filter_map(|a| a.guard));
        }
        ExprKind::If { branches, .. } => out.extend(branches.iter().map(|(c, _)| *c)),
        ExprKind::IfLetChain { clauses, .. } => {
            for c in clauses {
                match c {
                    IfLetClause::Binding { value, .. } => out.push(*value),
                    IfLetClause::Condition(c) => out.push(*c),
                }
            }
        }
        // Block-bearing nodes: their inner exprs are reached via `child_blocks`.
        ExprKind::Block(_) | ExprKind::Lambda { .. } => {}
        ExprKind::InterpolatedString(parts) => {
            for p in parts {
                if let InterpolatedPart::Expr(x, spec) = p {
                    out.push(*x);
                    // Dynamic `{w}`/`{p}` sub-exprs are real children — they must
                    // be cooked / renamed / substituted like any other.
                    if let Some(s) = spec {
                        out.extend(s.width_expr);
                        out.extend(s.precision_expr);
                    }
                }
            }
        }
    }
    out
}

/// Direct child `Block` edges of `e`.
pub(crate) fn child_blocks(ast: &Ast, e: Idx<ExprData>) -> BlockEdges {
    let mut out = BlockEdges::new();
    match &ast.exprs[e].kind {
        ExprKind::Block(b) => out.push(*b),
        ExprKind::Lambda { body, .. } => out.push(*body),
        ExprKind::If { branches, else_branch } => {
            out.extend(branches.iter().map(|(_, b)| *b));
            out.extend(else_branch.iter().copied());
        }
        ExprKind::IfLet { then_block, else_block, .. }
        | ExprKind::IfLetChain { then_block, else_block, .. } => {
            out.push(*then_block);
            out.extend(else_block.iter().copied());
        }
        ExprKind::Match { arms, .. } => out.extend(arms.iter().map(|a| a.body)),
        ExprKind::SafeNavigationLet { body, .. } => out.push(*body),
        _ => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use crate::parse_module;
    use ustr::Ustr;

    /// Parse `src`, cook it, and return the first function body's s-expr.
    fn cooked_body(src: &str) -> String {
        let mut ast = parse_module(src, Ustr::from("test.rock")).expect("parse failed");
        crate::cook_single(&mut ast);
        let crate::ast::ItemKind::Function(f) = &ast.items[0].kind else {
            panic!("expected fn");
        };
        ast.block_sexp(f.body.rock_block().expect("rock body"))
    }

    fn check(body: &str, expected: &str) {
        let src = format!("fn f() {{ {} }}", body);
        assert_eq!(cooked_body(&src), expected, "source: {src:?}");
    }

    #[test]
    fn type_hole_and_range_and_dollar() {
        // `?` type hole, `..=` inclusive range pattern, `$` low-prec application
        // — corpus features the games don't hit (caught by the tests/ oracle).
        check("let x: ? = 42", "(block (let x ? 42))");
        check(
            "match x { 1..=10 => \"s\", _ => \"o\" }",
            "(block (match x (arm (range 1 ..= 10) (block \"s\")) (arm _ (block \"o\"))))",
        );
        // `f $ a == b` -> `f(a == b)` ($ is the lowest-binding operator).
        check("assert $ a == b", "(block (call assert (== a b)))");
        // `recv?.let { body }` — binding is the receiver ident; body excluded
        // from the s-expr (rock parity).
        check("x?.let { x * 2 }", "(block (?.let x x))");
        // `^name` anaphoric ident renders bare; `&name` function ref keeps `&`.
        check("^x + 1", "(block (+ x 1))");
        check("let f = &helper", "(block (let f () &helper))");
    }

    #[test]
    fn non_placeholder_untouched() {
        // skull parses `=> expr` lambda bodies as a `return` block (same shape
        // the placeholder path builds), so an explicit lambda is byte-identical
        // before and after cook — proof the driver doesn't touch it.
        check("xs.map(y => y.health)", "(block (call (. xs map) (lambda (None) None (block (return (. y health))))))");
    }
}
