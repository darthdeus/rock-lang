//! Lookups over the raw (pre-expansion) `Ast`: expansion erases macros before bone sees a module.

use skull_parser::ast::{ExprKind, ItemKind};
use skull_parser::{Ast, MacroRegistry, Span};

pub(crate) fn macro_param_references(ast: &Ast, offset: usize) -> Vec<Span> {
    for group in &ast.macro_param_refs {
        if group.spans.iter().any(|s| {
            let (start, end) = s.offset_range();
            start <= offset && offset < end
        }) {
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
        let (start, end) = id.span.offset_range();
        if start <= offset && offset < end {
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
        let (start, end) = f.name.span.offset_range();
        f.is_macro && start <= offset && offset < end
    });
    on_def || macro_invocation_def(ast, registry, offset).is_some()
}
