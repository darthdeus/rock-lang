// RED driver for the enum/variant HIR-span work (docs/lsp-rename-coverage.md).
//
// `hir::Expr::EnumVariant` and `hir::Pattern::EnumVariant` today record only the
// WHOLE expression/pattern span -- not the enum-qualifier token (`Color`) or the
// variant token (`Red`). LSP rename must rewrite the exact name token at every
// use, and recovering those tokens by reparsing the parser AST (what hover does)
// is not acceptable precision. So lowering must carry the spans, populated from
// the parser-AST `Ident`s it already holds.
//
// This file WILL NOT COMPILE until the author adds the new fields:
//   Expr::EnumVariant     { .., enum_name_span: Span,        variant_span: Span }
//   Pattern::EnumVariant  { .., enum_name_span: Option<Span>, variant_span: Span }
//   Pattern::Record       { .., enum_name_span: Option<Span> }   // name_span already exists
// and populates them at the five bone-lower construction sites (see the plan
// doc). Once it compiles, the assertions pin that each span covers exactly its
// own name token. It is in its own test binary so the missing fields don't block
// the rest of the structural suite from building.

use bone_semantic::prelude::*;

fn parse_rock(src: &str) -> Result<skull_parser::Ast, CompilerError> {
    skull_parser::parse_module_clean(src, Ustr::from("<enum_variant_spans>"))
}

fn check(src: &str) -> bone_semantic::inference::Workspace {
    let mut module = [parse_rock(src).expect("parse")];
    semantic_pass(&mut module).expect("semantic_pass")
}

/// The source slice a byte-offset span covers -- the token text to assert on.
fn text<'a>(src: &'a str, span: &Span) -> &'a str {
    let (s, e) = span.offset_range;
    &src[s..e]
}

#[test]
fn enum_variant_uses_carry_name_token_spans() {
    use bone_semantic::hir::{Expr, Pattern};

    let src = "\
enum Color { Red, Green }
enum Shape { Rect { w: int }, Circle(int) }
fn f(c: Color, s: Shape) -> int {
    let x = Color::Red
    let a = match c {
        Color::Red => 1,
        Color::Green => 2,
    }
    return match s {
        Shape::Rect { w } => w,
        Shape::Circle(r) => r,
    }
}";
    let ws = check(src);
    let mid = ws
        .modules
        .iter()
        .find(|m| m.id != ModuleId::builtin())
        .expect("one user module")
        .id;
    let hir = ws.module(mid);
    let (f_idx, _) = hir
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == "f")
        .expect("fn f");
    let body = &hir.bodies[&BodyOwner::Function(f_idx)];

    // --- Expression use: `Color::Red` (the `let x = ...` value). ---
    let (_, ev) = body
        .expr_arena
        .iter()
        .find(|(_, e)| matches!(e, Expr::EnumVariant { variant, .. } if variant.as_str() == "Red"))
        .expect("an Expr::EnumVariant for Color::Red");
    let Expr::EnumVariant {
        enum_name_span,
        variant_span,
        ..
    } = ev
    else {
        unreachable!()
    };
    assert_eq!(text(src, enum_name_span), "Color", "expr enum-qualifier token");
    assert_eq!(text(src, variant_span), "Red", "expr variant token");

    // --- Unit-variant pattern: `Color::Green => ..`. ---
    let green = body
        .pattern_arena
        .values()
        .find_map(|p| match p {
            Pattern::EnumVariant {
                variant,
                enum_name_span,
                variant_span,
                ..
            } if variant.as_str() == "Green" => Some((*enum_name_span, *variant_span)),
            _ => None,
        })
        .expect("a Pattern::EnumVariant for Color::Green");
    assert_eq!(text(src, &green.0.expect("enum qualifier present")), "Color");
    assert_eq!(text(src, &green.1), "Green");

    // --- Tuple-variant pattern: `Shape::Circle(r) => ..`. ---
    let circle = body
        .pattern_arena
        .values()
        .find_map(|p| match p {
            Pattern::EnumVariant {
                variant,
                enum_name_span,
                variant_span,
                ..
            } if variant.as_str() == "Circle" => Some((*enum_name_span, *variant_span)),
            _ => None,
        })
        .expect("a Pattern::EnumVariant for Shape::Circle");
    assert_eq!(text(src, &circle.0.expect("enum qualifier present")), "Shape");
    assert_eq!(text(src, &circle.1), "Circle");

    // --- Record-variant pattern: `Shape::Rect { w } => ..`. ---
    let (rect_enum_span, rect_name_span) = body
        .pattern_arena
        .values()
        .find_map(|p| match p {
            Pattern::Record {
                name: Some(n),
                enum_name_span,
                name_span,
                ..
            } if n.as_str() == "Rect" => Some((*enum_name_span, *name_span)),
            _ => None,
        })
        .expect("a Pattern::Record for Shape::Rect");
    assert_eq!(text(src, &rect_enum_span.expect("enum qualifier present")), "Shape");
    assert_eq!(text(src, &rect_name_span.expect("variant head present")), "Rect");
}
