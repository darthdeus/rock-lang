//! Macro-parameter references: a lookup over the index the parser builds on the
//! raw (pre-expansion) `Ast` (`macro_param_refs`). Macros are erased before bone
//! sees a module, so the macro definition has no HIR -- but the session keeps
//! the raw `Ast`, and skull-parser's macro path records each param's decl + use
//! spans there (`index_macro_param_refs`, the read-side twin of `subst_in_item`).
//! The IDE just finds the group covering the cursor.

use skull_parser::ast::{ExprKind, ItemKind};
use skull_parser::{Ast, MacroRegistry, Span};

/// All occurrences (decl + template uses) of the macro parameter the cursor is
/// on, or empty if `offset` isn't on a macro parameter.
pub fn macro_param_references(ast: &Ast, offset: usize) -> Vec<Span> {
    for group in &ast.macro_param_refs {
        if group.spans.iter().any(|s| {
            let (start, end) = s.offset_range;
            start <= offset && offset < end
        }) {
            return group.spans.clone();
        }
    }
    Vec::new()
}

/// If `offset` is on a macro INVOCATION name in `ast` (the pre-expansion tree),
/// the definition name-span of the macro overload it expands to. Expansion
/// erases the invocation name and retags the body onto the call site, so
/// without this goto-def resolves whatever expanded token now sits at the
/// cursor (e.g. `enum Wait` inside the spliced body). Arity is checked against
/// the registry, so a same-named FN call at a different arity (the 3-arg
/// `run_into` fns beside the 2-arg macro) returns None and falls through to
/// normal definition resolution.
pub fn macro_invocation_def(ast: &Ast, registry: &MacroRegistry, offset: usize) -> Option<Span> {
    for ed in ast.exprs.values() {
        let ExprKind::FunctionCall { func, args, .. } = &ed.kind else {
            continue;
        };
        let ExprKind::Ident(id) = &ast.exprs[*func].kind else {
            continue;
        };
        let (start, end) = id.span.offset_range;
        if start <= offset && offset < end {
            return registry.def_name_span(id.name, args.len());
        }
    }
    None
}

/// Whether `offset` is on a macro's name in `ast`: a macro definition's name
/// or an invocation that expands to a macro. Expansion erases both before
/// bone sees the module, so no reference walk can find every use of one.
pub fn on_macro_name(ast: &Ast, registry: &MacroRegistry, offset: usize) -> bool {
    let on_def = ast.items.iter().any(|item| {
        let ItemKind::Function(f) = &item.kind else {
            return false;
        };
        let (start, end) = f.name.span.offset_range;
        f.is_macro && start <= offset && offset < end
    });
    on_def || macro_invocation_def(ast, registry, offset).is_some()
}
