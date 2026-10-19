//! Thin delegation layer between the workspace and the LSP facade.
//!
//! Started as the "fill in what's missing on bone" stub surface; every feature
//! has since graduated to a real implementation in `crate::query` (completion,
//! symbols, signature help, typed holes live there directly). What remains
//! here are the delegations whose doc comments carry the coverage caveats
//! (references: globals/generic-params pending) plus the
//! inlay-hints profiling wrapper. A future not-yet-answerable feature gets its
//! stub here again: safe empty result + the `crate::query::*` API to add +
//! the checkbox in query facade contract.
//!
//! Bone-native inputs (`&Workspace`, `ModuleId`, `&InferenceResult`, byte
//! `offset`) and outputs (`Span`s + neutral structs) only -- these know
//! nothing about `lsp_types`; the boundary stays in `super`.

use bone_semantic::prelude::{InferenceResult, ModuleId, Span, Workspace};

pub struct HintEntry {
    /// Span the hint annotates. A `leading` hint renders before the span (a
    /// parameter name, `name:` before an argument); otherwise it renders after
    /// the span (a type hint, `: T` after a binding name).
    pub span: Span,
    pub label: String,
    pub leading: bool,
}

// ---------------------------------------------------------------------------
// goto-definition
// ---------------------------------------------------------------------------

/// Declaration span of the name under `offset`, or None.
///
/// Delegates to `crate::query::definition_at`, which resolves all identifier
/// kinds (local/param uses, function calls, struct literals, enum variants,
/// globals). A callee in the warm stdlib resolves to its stdlib id, which the
/// backend maps to the file's real path (`editor_path`).
pub fn definition(
    ws: &Workspace,
    mid: ModuleId,
    inf: &InferenceResult,
    offset: usize,
) -> Option<Span> {
    crate::query::definition_at(ws, mid, inf, offset)
}

/// Declaration of the *type* of the value under `offset` (LSP
/// `textDocument/typeDefinition`), or None.
///
/// Delegates to `crate::query::type_definition_at`: reads the inferred type of
/// the expression/binding at the cursor and resolves its named type's
/// declaration (struct/enum/newtype, or a stdlib landmark for `Vec`/`Map`/
/// `Coroutine`). Returns None for primitives and non-nominal types.
pub fn type_definition(
    ws: &Workspace,
    mid: ModuleId,
    inf: &InferenceResult,
    offset: usize,
) -> Option<Span> {
    crate::query::type_definition_at(ws, mid, inf, offset)
}

// ---------------------------------------------------------------------------
// find-references / rename
// ---------------------------------------------------------------------------

/// Every use site (including the declaration) of the symbol under `offset`.
/// Drives both find-references and rename.
///
/// Delegates to `crate::query::references`, which resolves **local bindings,
/// params** (incl. callers' named-arg labels), **functions** (incl. UFCS and
/// `?.` call sites), **globals**, **structs + fields** (incl. `?.`/`.field`
/// sites and nullable annotations), **enums + variants**, and **fn generic
/// type params** with name-precise spans, so rename is safe for them.
/// Not covered (refused, never partial): newtypes, record-variant payload
/// fields, decorators, macro names, struct/enum-level generics --
/// rename behavior contract. Shorthand sites (`S { x }`) carry a
/// [`crate::query::Shorthand`] tag so rename expands them instead of
/// replacing the shared token.
pub fn references<'t>(
    ws: &Workspace,
    mid: ModuleId,
    inf: &InferenceResult,
    offset: usize,
    text_of: crate::query::TextOf<'_, 't>,
    fn_scope: crate::query::FnRefScope,
) -> Vec<crate::query::RefSite> {
    crate::query::references(ws, mid, inf, offset, text_of, fn_scope)
}

// ---------------------------------------------------------------------------
// inlay hints
// ---------------------------------------------------------------------------

/// Type hints for unannotated `let` bindings whose name-span falls in
/// `[start, end)`. Delegates to [`crate::query::inferred_let_bindings`]; the
/// facade places each hint at the binding span's end (`let n`|`: int`).
pub fn inlay_hints(
    ws: &Workspace,
    mid: ModuleId,
    inf: &InferenceResult,
    start: usize,
    end: usize,
) -> Vec<HintEntry> {
    let prof = crate::backend::lsp_profile_enabled();
    let t = std::time::Instant::now();
    let lets = crate::query::inferred_let_bindings(ws, mid, inf, start, end);
    let let_ms = t.elapsed().as_secs_f64() * 1000.0;
    let t = std::time::Instant::now();
    let params = crate::query::call_param_hints(ws, mid, inf, start, end);
    let param_ms = t.elapsed().as_secs_f64() * 1000.0;
    if prof {
        eprintln!(
            "[bone inlay] let-bindings {let_ms:.2}ms ({} hints)  call-params {param_ms:.2}ms ({} hints)",
            lets.len(),
            params.len()
        );
    }
    let type_hints = lets.into_iter().map(|(span, ty)| HintEntry {
        span,
        label: format!(": {ty}"),
        leading: false,
    });
    let param_hints = params.into_iter().map(|(span, label)| HintEntry {
        span,
        label,
        leading: true,
    });
    type_hints.chain(param_hints).collect()
}
