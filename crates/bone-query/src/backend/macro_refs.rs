//! Lookups over the raw (pre-expansion) `Ast`: expansion erases macros before bone sees a module.

use skull_parser::ast::{ExprKind, ItemKind};
use skull_parser::{Ast, MacroRegistry, Span};

use crate::query::span_contains;

pub(crate) fn macro_param_references(ast: &Ast, offset: usize) -> Vec<Span> {
    for group in &ast.macro_param_refs {
        if group.spans.iter().any(|s| span_contains(s, offset)) {
            return group.spans.clone();
        }
    }
    Vec::new()
}

/// Arity is checked against the registry, so a same-named fn of another arity
/// returns None.
pub(crate) fn macro_invocation_def(
    ast: &Ast,
    registry: &MacroRegistry,
    offset: usize,
) -> Option<Span> {
    for ed in ast.exprs.values() {
        let ExprKind::FunctionCall { func, args, .. } = &ed.kind else {
            continue;
        };
        let ExprKind::Ident(id) = &ast.exprs[*func].kind else {
            continue;
        };
        if span_contains(&id.span, offset) {
            return registry.def_name_span(id.name, args.len());
        }
    }
    None
}

pub(crate) fn on_macro_name(ast: &Ast, registry: &MacroRegistry, offset: usize) -> bool {
    let on_def = ast.items.iter().any(|item| {
        let ItemKind::Function(f) = &item.kind else {
            return false;
        };
        f.is_macro && span_contains(&f.name.span, offset)
    });
    on_def || macro_invocation_def(ast, registry, offset).is_some()
}
