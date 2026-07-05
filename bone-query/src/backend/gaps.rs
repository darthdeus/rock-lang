//! THE "fill in what's missing on bone" surface.
//!
//! Every LSP feature bone can't answer yet is one function here. Each returns a
//! safe empty result today and documents the exact `crate::query::*` signature to
//! add and the one-line swap that lights the feature up. The facade
//! (`super`) already converts these bone-native results into LSP types, the
//! dispatch is already wired, and the Rust tests already assert these don't
//! panic -- so graduating a feature is: (1) add the query to `crate::query`
//! (this crate; if it needs new bone internals, propose that bone change as a
//! hint), (2) replace the stub body here, (3) flip the checkbox in
//! `docs/lsp-bone-migration-plan.md`.
//!
//! These functions take bone-native inputs (`&Workspace`, `ModuleId`,
//! `&InferenceResult`, byte `offset`) and return bone-native outputs (`Span`s +
//! the small neutral structs below). They deliberately know nothing about
//! `lsp_types` -- keep the bone/LSP boundary in `super`.

use bone_semantic::hir::{self, BindingKind, Function, NodeRef};
use bone_semantic::prelude::{Idx, InferenceResult, ModuleId, Span, TyId, Workspace};
use skull_parser::Ast;
use ustr::Ustr;

/// Symbol category, mapped to `CompletionItemKind` / `SymbolKind` by the facade.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SymCat {
    Function,
    Struct,
    Enum,
    Variant,
    Newtype,
    Global,
    Local,
    Param,
    Field,
    Keyword,
}

pub struct CompletionEntry {
    /// Interned candidate name (`Copy`, no heap clone during ranking). The
    /// caller materializes the `String` label only for the post-cap survivors.
    pub name: ustr::Ustr,
    pub kind: SymCat,
    pub detail: Option<String>,
    /// Whether a `SymCat::Function` candidate declares any parameters. Drives
    /// the completion snippet: a fn with params inserts `name($0)` (cursor
    /// inside), a param-less fn inserts `name()` (cursor after). Meaningless
    /// for non-function kinds (left `false`).
    pub takes_params: bool,
    /// For a `SymCat::Variant` candidate (qualified `Enum::Variant`), its
    /// construction shape -- drives the payload template (`($0)` / `{ $0 }`).
    /// `None` for every other kind.
    pub variant_shape: Option<crate::query::VariantShape>,
}

pub struct SymbolEntry {
    pub name: String,
    pub kind: SymCat,
    pub span: Span,
}

pub struct SignatureEntry {
    pub label: String,
    pub params: Vec<String>,
}

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
/// globals). (A callee in the warm stdlib resolves here but the facade drops it,
/// since `<bone-stdlib:...>` isn't a navigable path.)
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
/// params, and functions** (incl. UFCS call sites) with name-precise spans, so
/// rename is safe for them. Globals, structs, and enums aren't covered yet --
/// they need bone to record an identifier name-span at each reference site.
pub fn references(
    ws: &Workspace,
    mid: ModuleId,
    inf: &InferenceResult,
    offset: usize,
) -> Vec<Span> {
    crate::query::references(ws, mid, inf, offset)
}

// ---------------------------------------------------------------------------
// completion
// ---------------------------------------------------------------------------

/// Completion candidates at `offset`.
///
/// IMPLEMENTED for the **global namespace** by mining the per-module name
/// indexes (`functions_by_name` / `struct_by_name` / `enums_by_name` /
/// `newtype_by_name` / `global_by_name`) across every module in the workspace
/// (user code + warm stdlib + builtin). Functions get a rendered signature as
/// `detail`. The facade filters these by the identifier prefix at the cursor.
///
/// In-scope **locals and params** (from [`crate::query::locals_in_scope`]) are
/// offered first and shadow same-named globals. NOT yet covered: member/`.`
/// completion (needs the receiver's type + its fields/methods).
///
/// `expr_position` (the cursor is in a value slot, not a type slot) governs enum
/// completion: in an expression slot a user enum is offered as its qualified
/// `Enum::Variant` values (the bare name isn't a usable value); in a type slot
/// the bare `Enum` name is offered instead. Mirrors rock-lsp.
pub fn completion(
    ws: &Workspace,
    mid: ModuleId,
    inf: &InferenceResult,
    offset: usize,
    prefix: &str,
    expr_position: bool,
) -> Vec<CompletionEntry> {
    let mut seen: std::collections::HashSet<ustr::Ustr> = std::collections::HashSet::new();
    let mut out = Vec::new();

    // Filter by the identifier query HERE, before rendering any per-candidate
    // detail (function signatures cost a render each). A post-filter in the
    // caller would render every signature in the workspace and then throw most
    // away. Match is a CLEAN fuzzy match (`super::clean_match`): the query is a
    // subsequence whose first char is at a word boundary and the rest land at
    // boundaries / in a run -- so `tilecen` finds `tile_center` across the `_`
    // and `log` finds `stone_log`, but a mid-word scatter (`nog` across
    // `__sto(n)e_l(o)(g)`) is dropped rather than shown ranked low. The fuzzy
    // score in the caller (`arg_sort_text`) then ranks tight runs first.
    // `prefix` is already lowercased.
    let pfx = prefix.as_bytes();
    let matches = |name: &str| -> bool { super::clean_match(name.as_bytes(), pfx) };

    // Locals/params in scope shadow the global namespace -- emit them first and
    // let the shared `seen` set suppress a same-named global.
    for (name, kind, ty) in crate::query::locals_in_scope(ws, mid, inf, offset) {
        if !matches(name.as_str()) {
            continue;
        }
        if seen.insert(name) {
            out.push(CompletionEntry {
                name,
                kind: match kind {
                    BindingKind::Param => SymCat::Param,
                    BindingKind::Let => SymCat::Local,
                },
                detail: (!ty.is_empty()).then_some(ty),
                takes_params: false,
                variant_shape: None,
            });
        }
    }

    for m in ws.modules.iter() {
        for (name, fns) in m.functions_by_name.iter() {
            if !matches(name.as_str()) {
                continue;
            }
            if !seen.insert(*name) {
                continue;
            }
            let first = fns.first().map(|(fmid, fid)| &ws.modules[fmid.index()].functions[*fid]);
            out.push(CompletionEntry {
                name: *name,
                kind: SymCat::Function,
                // Signature `detail` is deferred to the caller and materialized
                // (from `BoneProject::fn_sigs`) only for the post-cap survivors,
                // not rendered or cloned for every workspace function here.
                detail: None,
                takes_params: first.is_some_and(|f| !f.param_decls.is_empty()),
                variant_shape: None,
            });
        }
        for name in m.struct_by_name.keys() {
            if matches(name.as_str()) && seen.insert(*name) {
                out.push(CompletionEntry { name: *name, kind: SymCat::Struct, detail: None, takes_params: false, variant_shape: None });
            }
        }
        // A user enum in an expression slot is offered as its qualified
        // `Enum::Variant` values (matched by a fuzzy subsequence so typing the
        // variant name -- not the enum -- still surfaces it), and the bare enum
        // is suppressed. In a type slot (or for a stdlib/builtin enum, whose `<..>`
        // file id starts with `<`), the bare `Enum` name is offered as before.
        let user_module = !m.file.as_str().starts_with('<');
        for (name, idx) in m.enums_by_name.iter() {
            if expr_position && user_module {
                for v in m.enums[*idx].variants.iter() {
                    let qualified = format!("{name}::{}", v.name);
                    let qn = ustr::Ustr::from(qualified.as_str());
                    // Clean-match the VARIANT NAME (what the user types, `nog` ->
                    // NotGround -- camelCase-aware) or the qualified form (a
                    // `tarnog` Enum+variant query), dropping mid-word scatter.
                    let vname = v.name.as_str();
                    let hit = pfx.is_empty()
                        || super::clean_match(vname.as_bytes(), pfx)
                        || super::clean_match(qualified.as_bytes(), pfx);
                    if hit && seen.insert(qn) {
                        out.push(CompletionEntry {
                            name: qn,
                            kind: SymCat::Variant,
                            detail: Some(name.to_string()),
                            takes_params: false,
                            variant_shape: Some(crate::query::VariantShape::of(&v.payload)),
                        });
                    }
                }
            } else if matches(name.as_str()) && seen.insert(*name) {
                out.push(CompletionEntry { name: *name, kind: SymCat::Enum, detail: None, takes_params: false, variant_shape: None });
            }
        }
        for name in m.newtypes_by_name.keys() {
            if matches(name.as_str()) && seen.insert(*name) {
                out.push(CompletionEntry { name: *name, kind: SymCat::Newtype, detail: None, takes_params: false, variant_shape: None });
            }
        }
        for (name, gidx) in m.global_by_name.iter() {
            if matches(name.as_str()) && seen.insert(*name) {
                // Carry the rendered type in `detail` -- both for the inline
                // `: type` hint and so arg-position ranking can recognise a
                // global of the expected type (bucket 0).
                let ty = ws.render(None, m.globals[*gidx].ty);
                out.push(CompletionEntry {
                    name: *name,
                    kind: SymCat::Global,
                    detail: (!ty.is_empty()).then_some(ty),
                    takes_params: false,
                    variant_shape: None,
                });
            }
        }
    }

    // Builtin type names, in a TYPE slot only. `Coroutine`/`Map` are compiler
    // builtins with no entry in the workspace name-index, and the scalar
    // primitives aren't nominal types at all -- neither flows through the loops
    // above. (Stdlib structs like `Vec`/`Entity`/`Arena`/`NativeBuffer*`/`Ptr*`/
    // `Unit` already come through `struct_by_name`.)
    if !expr_position {
        for name in BUILTIN_TYPE_NAMES {
            let n = ustr::Ustr::from(*name);
            if matches(name) && seen.insert(n) {
                out.push(CompletionEntry {
                    name: n,
                    kind: SymCat::Struct,
                    detail: None,
                    takes_params: false,
                    variant_shape: None,
                });
            }
        }
    }

    // `yield` / `yield*` keywords, only inside a `co fn` body in an expression
    // slot. Outside a coroutine they're not valid, so don't offer them.
    if expr_position && enclosing_is_coroutine(ws, mid, offset) {
        for kw in ["yield", "yield*"] {
            let n = ustr::Ustr::from(kw);
            if matches(kw) && seen.insert(n) {
                out.push(CompletionEntry {
                    name: n,
                    kind: SymCat::Keyword,
                    detail: None,
                    takes_params: false,
                    variant_shape: None,
                });
            }
        }
    }

    out
}

/// Type-name builtins surfaced in a type slot (see the call site). The generic
/// builtins (`Coroutine`, `Map`) and the scalar primitives have no workspace
/// name-index entry.
const BUILTIN_TYPE_NAMES: &[&str] =
    &["Coroutine", "Map", "int", "float", "string", "bool", "any"];

/// Whether the body enclosing `offset` is a `co fn`. Drives `yield`/`yield*`
/// keyword completion (and -- in the facade -- coroutine-fn ranking after a
/// `yield`).
fn enclosing_is_coroutine(ws: &Workspace, mid: ModuleId, offset: usize) -> bool {
    let Some(module) = ws.modules.get(mid.index()) else {
        return false;
    };
    matches!(
        module.body_at(offset),
        Some(hir::BodyOwner::Function(fid)) if module.functions[fid].is_coroutine
    )
}

/// Render `fn name(p: T, ...) -> R` from a HIR function (static types, no body
/// inference context needed).
pub(crate) fn render_fn_signature(ws: &Workspace, name: ustr::Ustr, f: &bone_semantic::hir::Function) -> String {
    let params: Vec<String> = f
        .param_decls
        .iter()
        .map(|p| format!("{}: {}", p.name, ws.render(None, p.ty)))
        .collect();
    let ret = f
        .return_ty
        .map(|t| format!(" -> {}", ws.render(None, t)))
        .unwrap_or_default();
    format!("fn {}({}){}", name, params.join(", "), ret)
}

/// Per-function ctags signature pieces, keyed by declaration-span start:
/// `(params, returns)` where `params` is `"(a: int, b: int)"` and `returns` is
/// the rendered return type (None for a unit/no-return fn). Split form -- not
/// `render_fn_signature`'s combined string -- because ctags carries `signature:`
/// and `returns:` as separate extended fields. Keyed by span start (unique per
/// declaration, so overloads don't collide). Only the on-save ctags path calls
/// this; `document_symbols` stays signature-free so the hot `workspace_symbols`
/// query doesn't render types it won't show.
pub(crate) fn fn_ctags_signatures(
    ws: &Workspace,
    mid: ModuleId,
) -> std::collections::HashMap<usize, (String, Option<String>)> {
    let mut out = std::collections::HashMap::new();
    let Some(m) = ws.modules.get(mid.index()) else {
        return out;
    };
    for (_, f) in m.functions.iter() {
        let params: Vec<String> = f
            .param_decls
            .iter()
            .map(|p| format!("{}: {}", p.name, ws.render(None, p.ty)))
            .collect();
        let returns = f.return_ty.map(|t| ws.render(None, t));
        out.insert(f.span.offset_range.0, (format!("({})", params.join(", ")), returns));
    }
    out
}

// ---------------------------------------------------------------------------
// document symbols
// ---------------------------------------------------------------------------

/// Top-level items declared in `mid` (functions, types, globals) with spans.
///
/// Walks the module's item arenas directly -- no inference needed, every item
/// carries its name + declaration `Span`. The facade maps each `SymbolEntry`
/// to a `DocumentSymbol`.
pub fn document_symbols(ws: &Workspace, mid: ModuleId) -> Vec<SymbolEntry> {
    let Some(m) = ws.modules.get(mid.index()) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    let mut push = |name: ustr::Ustr, kind: SymCat, span: Span| {
        out.push(SymbolEntry { name: name.to_string(), kind, span });
    };

    for (_, f) in m.functions.iter() {
        push(f.name, SymCat::Function, f.span);
    }
    for (_, s) in m.structs.iter() {
        push(s.name, SymCat::Struct, s.span);
    }
    for (_, e) in m.enums.iter() {
        push(e.name, SymCat::Enum, e.span);
    }
    for (_, n) in m.newtypes.iter() {
        push(n.name, SymCat::Newtype, n.span);
    }
    for (_, g) in m.globals.iter() {
        push(g.name, SymCat::Global, g.span);
    }

    out
}

// ---------------------------------------------------------------------------
// signature help
// ---------------------------------------------------------------------------

/// Signature(s) of the call (or struct-literal constructor) enclosing `offset`,
/// plus that expr's source span so the facade can compute the active argument
/// index from cursor position.
///
/// Three resolution tiers, in order of precision:
/// 1. A function call bone resolved to a single overload (`resolved_overload_calls`,
///    same data as goto-definition) -> that one signature.
/// 2. A function call bone could NOT resolve (e.g. a generic call with a lambda
///    arg bone can't disambiguate -> "ambiguous overload") -> every function with
///    the callee's name, the standard LSP overload list. Inference isn't needed.
/// 3. A struct literal `Name { .. }` -> a `Name { f: T, .. }` constructor signature.
///
/// The active-parameter index is intentionally NOT computed here -- it's a
/// cursor/source concern, so the facade fills it from the returned span.
pub fn signature_help(
    ws: &Workspace,
    mid: ModuleId,
    inf: Option<&InferenceResult>,
    offset: usize,
) -> Option<(Vec<SignatureEntry>, Span)> {
    let module = ws.modules.get(mid.index())?;
    let owner = module.body_at(offset)?;
    let body = module.bodies.get(&owner)?;

    // Smallest enclosing call/struct-literal expr covering the cursor wins
    // (handles nested calls like `multiply(add(2, 3), 4)`).
    let mut best: Option<(usize, Idx<hir::Expr>)> = None;
    for (e, expr) in body.expr_arena.iter() {
        if !matches!(expr, hir::Expr::Call { .. } | hir::Expr::StructLiteral { .. }) {
            continue;
        }
        let Some(span) = body.expr_spans.get(e) else { continue };
        let (s, en) = span.offset_range;
        if s <= offset && offset < en {
            let len = en.saturating_sub(s);
            if best.map_or(true, |(bl, _)| len < bl) {
                best = Some((len, e));
            }
        }
    }
    let (_, e) = best?;
    let call_span = *body.expr_spans.get(e)?;

    match &body.expr_arena[e] {
        hir::Expr::Call { callee, .. } => {
            // Tier 1: the overload bone nailed down.
            if let Some((cmid, fid)) = inf
                .and_then(|inf| inf.per_fn.get(&owner))
                .and_then(|bi| bi.resolved_overload_calls.get(&e))
            {
                let f = &ws.modules.get(cmid.index())?.functions[*fid];
                return Some((vec![fn_sig_entry(ws, f.name, f)], call_span));
            }
            // Tier 2: by-name overload list (resolution was ambiguous/absent).
            let hir::Callee::Unresolved { name, .. } = callee else {
                return None;
            };
            let sigs = functions_named(ws, *name);
            (!sigs.is_empty()).then_some((sigs, call_span))
        }
        hir::Expr::StructLiteral { name, .. } => {
            Some((vec![struct_ctor_sig(ws, *name)?], call_span))
        }
        _ => None,
    }
}

/// Render one function as a `SignatureEntry` (label + per-param labels).
fn fn_sig_entry(ws: &Workspace, name: Ustr, f: &Function) -> SignatureEntry {
    let params = f
        .param_decls
        .iter()
        .map(|p| format!("{}: {}", p.name, ws.render(None, p.ty)))
        .collect();
    SignatureEntry { label: render_fn_signature(ws, name, f), params }
}

/// Every function in the workspace named `name` (user code + stdlib), as the
/// overload list a signature-help popup shows when resolution is ambiguous.
fn functions_named(ws: &Workspace, name: Ustr) -> Vec<SignatureEntry> {
    let mut out = Vec::new();
    for m in ws.modules.iter() {
        if let Some(fns) = m.functions_by_name.get(&name) {
            for (fmid, fid) in fns {
                let f = &ws.modules[fmid.index()].functions[*fid];
                out.push(fn_sig_entry(ws, name, f));
            }
        }
    }
    out
}

/// `Name { f: T, .. }` constructor signature for struct literal `name`, or None.
fn struct_ctor_sig(ws: &Workspace, name: Ustr) -> Option<SignatureEntry> {
    for m in ws.modules.iter() {
        if let Some(sid) = m.struct_by_name.get(&name) {
            let sdef = &m.structs[*sid];
            let params: Vec<String> = sdef
                .fields
                .iter()
                .map(|f| format!("{}: {}", f.name, ws.render(None, f.ty)))
                .collect();
            let label = format!("{} {{ {} }}", name, params.join(", "));
            return Some(SignatureEntry { label, params });
        }
    }
    None
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

// ---------------------------------------------------------------------------
// hole info
// ---------------------------------------------------------------------------

/// Which kind of `?` hole the cursor sits on.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum HoleKind {
    /// `?` in type position, e.g. `let x: ? = expr`.
    Type,
    /// `?` in value position, e.g. `let x = ?`.
    Expr,
}

/// The typed-hole report: kind, the inferred/expected type (when known), and the
/// bindings in scope at the hole. The type is the hole's solved type -- for a
/// type hole the inference var the `?` lowered to, for an expression hole the
/// fresh var coerced to its surrounding expected type (`int` in `let x: int = ?`,
/// an unsolved `?H` when nothing constrains it). `None` only if inference left
/// the node typeless.
pub struct HoleInfo {
    pub kind: HoleKind,
    pub ty: Option<String>,
    pub in_scope: Vec<(String, String)>,
    /// Source span of the `?` itself, so the hover handler can range its result.
    pub span: Span,
}

/// Hole-info at `offset`, or None when the cursor isn't on a `?` hole. The hole
/// is detected on the raw (pre-lowering) AST (where both kinds survive with a
/// span); the type comes from the lowered node at the cursor -- a type
/// annotation for type holes, an `Expr` for expression holes -- rendered through
/// the body's substitution chain.
pub fn hole_info(
    ws: &Workspace,
    mid: ModuleId,
    inf: &InferenceResult,
    ast: &Ast,
    offset: usize,
) -> Option<HoleInfo> {
    let (kind, span) = hole_kind_at(ast, offset)?;
    let ty = type_at_offset(ws, mid, inf, offset);
    let in_scope = crate::query::locals_in_scope(ws, mid, inf, offset)
        .into_iter()
        .map(|(name, _kind, ty)| (name.to_string(), ty))
        .collect();
    Some(HoleInfo { kind, ty, in_scope, span })
}

/// Every `?` hole in `ast`, each with its kind, solved/expected type (when
/// inference could resolve it), and source span. Powers the unfilled-hole
/// diagnostics: expression holes warn ("fill me in"), resolved type holes hint
/// ("inferred as T"). Same detection as `hole_kind_at`, swept over the whole
/// module instead of one cursor offset.
pub fn all_holes(
    ws: &Workspace,
    mid: ModuleId,
    inf: &InferenceResult,
    ast: &Ast,
) -> Vec<(Span, HoleKind, Option<String>)> {
    let mut out = Vec::new();
    for (_, t) in ast.types.iter() {
        if matches!(t.kind, skull_parser::TypeExprKind::Hole(..)) {
            let ty = type_at_offset(ws, mid, inf, t.span.offset_range.0);
            out.push((t.span, HoleKind::Type, ty));
        }
    }
    for (_, e) in ast.exprs.iter() {
        if matches!(e.kind, skull_parser::ExprKind::Hole(..)) {
            let ty = type_at_offset(ws, mid, inf, e.span.offset_range.0);
            out.push((e.span, HoleKind::Expr, ty));
        }
    }
    out
}

/// The kind of `?` hole whose span covers `offset` plus that span, if any. Type
/// position wins (a `?` is only ever one or the other at a given span).
fn hole_kind_at(ast: &Ast, offset: usize) -> Option<(HoleKind, Span)> {
    let covers = |range: (usize, usize)| range.0 <= offset && offset < range.1;
    for (_, t) in ast.types.iter() {
        if matches!(t.kind, skull_parser::TypeExprKind::Hole(..)) && covers(t.span.offset_range) {
            return Some((HoleKind::Type, t.span));
        }
    }
    for (_, e) in ast.exprs.iter() {
        if matches!(e.kind, skull_parser::ExprKind::Hole(..)) && covers(e.span.offset_range) {
            return Some((HoleKind::Expr, e.span));
        }
    }
    None
}

/// Render the lowered node at `offset`, resolved through the body's substitution
/// chain. A type hole's `?` is a type annotation (`LocalTy`/`StaticTy` -> the
/// inference var); an expression hole's `?` is an `Expr` (its `expr_types`
/// entry, the fresh var coerced from context).
fn type_at_offset(ws: &Workspace, mid: ModuleId, inf: &InferenceResult, offset: usize) -> Option<String> {
    let module = ws.modules.get(mid.index())?;
    let owner = module.body_at(offset)?;
    let body = module.bodies.get(&owner)?;
    let bi = inf.per_fn.get(&owner)?;
    let iv = body
        .node_index()
        .find(offset, offset + 1)
        .min_by_key(|x| x.stop - x.start)?;
    let ty = match iv.val {
        NodeRef::LocalTy(idx) => TyId::Local(idx),
        NodeRef::StaticTy { mid, idx } => TyId::Static(mid, idx),
        NodeRef::Expr(e) => *bi.expr_types.get(e)?,
        _ => return None,
    };
    Some(ws.render(Some(bi), ty))
}
