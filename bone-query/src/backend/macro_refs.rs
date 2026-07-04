//! Macro-parameter references: a lookup over the index the parser builds on the
//! raw (pre-expansion) `Ast` (`macro_param_refs`). Macros are erased before bone
//! sees a module, so the macro definition has no HIR -- but the session keeps
//! the raw `Ast`, and skull-parser's macro path records each param's decl + use
//! spans there (`index_macro_param_refs`, the read-side twin of `subst_in_item`).
//! The IDE just finds the group covering the cursor.

use skull_parser::{Ast, Span};

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
