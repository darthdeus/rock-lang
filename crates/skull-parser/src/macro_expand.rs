//! Macro expansion on skull-parser's arena AST.
//!
//! Runs as the FIRST cook stage, before desugar (bone's order), so the
//! desugars see fully-expanded code.
//!
//! Expression-position macros (`macro f(a: expr, ...) { <rock body> }`),
//! selected by arity (defaults widen the range), params substituted by deep
//! copy, the call replaced by a `Block` of the substituted body. Macro bodies
//! live in their defining module's arena, so inlining a body into a caller is a
//! cross-arena graft (`graft.rs`). Macro defs are dropped after expansion.
//!
//! Expansion grafts the body in one copy that freshens its binders (hygiene),
//! stamps the call site's span, splats `...rest` args and substitutes params.
//! Item macros splice their substituted items at top level.

use crate::ast::*;
use crate::desugar::{BlockEdges, ExprEdges, child_blocks, child_exprs};
use crate::graft::{
    Rewrite, graft_block, graft_block_with, graft_expr, graft_item, graft_items_with, graft_type,
};
use rock_core::{CompilerError, Span};
use smallvec::smallvec;
use ustr::{Ustr, UstrMap, UstrSet};

const MAX_MACRO_DEPTH: usize = 64;

/// A registered macro overload, self-contained in its own arena so expansion is
/// a clean cross-arena graft into the caller (no aliasing of the source module).
#[derive(Clone, Debug)]
struct MacroTemplate {
    params: Vec<MParam>,
    body: Idx<BlockData>,
    /// The macro's declared return type, if any. An expr-macro expansion is
    /// wrapped in `(:: <body> <return_ty>)` so the post-expansion type-checker
    /// sees the expected-type signal (rock parity — see `try_expand`).
    return_ty: Option<Idx<TypeExprData>>,
    /// Item declarations the macro emits when invoked at module scope.
    items: Vec<Item>,
    tmpl: Ast,
    /// The macro DEFINITION's name-ident span (carries its own file). Expansion
    /// erases the invocation name, so IDE goto-def on an invocation reads the
    /// target off the registry -- `MacroRegistry::def_name_span`.
    def_name_span: Span,
}

#[derive(Clone, Debug)]
struct MParam {
    name: Ustr,
    is_variadic: bool,
    default: Option<Idx<ExprData>>,
    /// `ident`/`ty`/`expr` fragment kind (from the param's declared type), or
    /// `None` for an ordinary runtime param.
    fragment: Option<Frag>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Frag {
    /// `name: ident` — a name that substitutes into both type and expr positions.
    Ident,
    /// `name: ty` — a type that substitutes into type positions.
    Ty,
    /// `name: expr`/`stmt`/... — a subtree that substitutes into expr positions.
    Expr,
}

type Registry = UstrMap<Vec<MacroTemplate>>;

/// Opaque, reusable macro registry: every macro overload collected from a whole
/// workspace, keyed by name. Built once from the UN-expanded modules (expansion
/// drops the defs, so it can't be rebuilt from already-expanded ASTs), then
/// handed to `expand_one` / `cook_one` to expand a single module against it --
/// the building block for re-cooking just one edited file incrementally. Holds
/// its own grafted arenas, so it outlives the source modules.
#[derive(Clone, Debug, Default)]
pub struct MacroRegistry(Registry);

impl MacroRegistry {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// This registry with the macros `modules` define added after its own:
    /// [`collect_registry`] over this registry's modules, then `modules`.
    pub fn extended<'a>(&self, modules: impl IntoIterator<Item = &'a Ast>) -> MacroRegistry {
        let mut reg = self.0.clone();
        for (name, overloads) in build_registry(modules) {
            reg.entry(name).or_default().extend(overloads);
        }
        MacroRegistry(reg)
    }

    /// The definition name-span of the macro overload that `name(<argc args>)`
    /// expands to, selected by the SAME arity rule as expansion
    /// (`select_overload`), or None if no macro overload matches this arity --
    /// so a same-named FN call (a different arity, e.g. the 3-arg `run_into`
    /// fns beside the 2-arg macro) returns None and falls through to normal
    /// definition resolution. Drives IDE goto-def on a macro invocation name.
    pub fn def_name_span(&self, name: Ustr, argc: usize) -> Option<Span> {
        select_overload(self.0.get(&name)?, argc).map(|t| t.def_name_span)
    }
}

/// Collect every macro overload defined across `modules` into a reusable
/// registry. Read-only over `modules` (templates graft into their own arenas),
/// so the caller keeps its un-expanded ASTs intact for later re-cooking.
pub fn collect_registry<'a>(modules: impl IntoIterator<Item = &'a Ast>) -> MacroRegistry {
    MacroRegistry(build_registry(modules))
}

/// Every name expanding `ast` against a registry can look up: each
/// identifier and member name in its expressions, each top-level invocation,
/// and each macro it defines.
///
/// Expansion looks a name up only for a call whose callee is an identifier
/// or a member, and for a top-level invocation; a same-named macro elsewhere
/// joins a module's own overloads. Every expression an expansion of these
/// modules builds is copied from their arenas (the call's arguments, the
/// grafted bodies, defaults and items of their macros), or renamed to a
/// hygiene local ([`is_hygiene_name`]). So expanding a set of modules
/// against a non-empty registry plus macros under none of the names the set
/// reaches, and no hygiene name, expands every module as the registry alone
/// does. (Against an empty registry a top-level invocation is not expanded
/// at all; a set with one needs a registry that holds a macro.)
pub fn macro_reach(ast: &Ast) -> impl Iterator<Item = Ustr> + '_ {
    let exprs = ast.exprs.values().filter_map(|data| match &data.kind {
        ExprKind::Ident(id) => Some(id.name),
        ExprKind::FieldAccess(_, member) => Some(member.name),
        _ => None,
    });
    let items = ast.items.iter().filter_map(|item| match &item.kind {
        ItemKind::MacroInvocation { name, .. } => Some(name.name),
        ItemKind::Function(f) if f.is_macro => Some(f.name.name),
        _ => None,
    });
    exprs.chain(items)
}

/// Whether `name` is one expansion gives a renamed body local
/// ([`Instantiation`]).
pub fn is_hygiene_name(name: &str) -> bool {
    name.starts_with(HYGIENE_PREFIX)
}

const HYGIENE_PREFIX: &str = "__inline_local_";

/// A macro name must be UNIQUE against top-level FUNCTION names across the whole
/// module set: `foo(...)` in call position is ambiguous when both a `macro foo`
/// and a `fn foo` exist (expansion would silently claim the call and shadow the
/// fn), so it is rejected -- a macro always has exactly one meaning. Macro
/// OVERLOADS (same name, different arity) stay legal; a same-named LOCAL binding
/// (param, nested fn, `let`) legally shadows a macro and is not a top-level fn,
/// so it never collides. Cross-module: all parsed modules share one namespace,
/// so this runs over the full parsed set before expansion erases the macro defs.
/// Run on the pre-expansion modules (expansion drops the `is_macro` items).
pub fn check_macro_name_uniqueness<'a>(
    modules: impl IntoIterator<Item = &'a Ast> + Clone,
) -> Vec<CompilerError> {
    let mut fn_names: UstrSet = UstrSet::default();
    for m in modules.clone() {
        for item in &m.items {
            if let ItemKind::Function(f) = &item.kind {
                if !f.is_macro {
                    fn_names.insert(f.name.name);
                }
            }
        }
    }
    let mut errs = Vec::new();
    let mut reported: UstrSet = UstrSet::default();
    for m in modules {
        for item in &m.items {
            if let ItemKind::Function(f) = &item.kind {
                if f.is_macro && fn_names.contains(&f.name.name) && reported.insert(f.name.name) {
                    errs.push(CompilerError::new_with_label(
                        f.name.span,
                        format!(
                            "macro `{0}` collides with a function named `{0}` -- a macro name must be unique in call position (`{0}(...)` would be ambiguous between the two). Rename the macro or the function.",
                            f.name.name
                        ),
                    ));
                }
            }
        }
    }
    errs
}

/// Every macro call whose argument labels don't fit the macro's parameters,
/// across the whole module set. Runs BEFORE expansion (which erases the macro
/// defs) and alongside [`check_macro_name_uniqueness`], for the same reason:
/// `expand_macros` is driven from bone and returns no diagnostics, so a
/// rejection has to be raised by a pass the caller can collect from.
///
/// Both this and the expander bind through `bind_macro_args`, so what is
/// reported here is exactly what would have been bound.
///
/// Without this, a bad label leaves the call UNEXPANDED and the user sees
/// "missing function `foo`" -- a true rejection naming the wrong cause.
pub fn check_macro_call_labels<'a>(
    modules: impl IntoIterator<Item = &'a Ast> + Clone,
) -> Vec<CompilerError> {
    let reg = build_registry(modules.clone());
    if reg.is_empty() {
        return Vec::new();
    }
    // Every call of every module is visited; the modules are independent and
    // the errors keep module order.
    use rayon::prelude::*;
    let modules: Vec<&Ast> = modules.into_iter().collect();
    modules
        .par_iter()
        .map(|m| call_label_errors(m, &reg))
        .collect::<Vec<_>>()
        .into_iter()
        .flatten()
        .collect()
}

/// [`check_macro_call_labels`] for the calls of ONE module against a prebuilt
/// `registry`: what an incremental driver runs on an edited file when no
/// macro definition changed (every other file's calls were checked against
/// the same registry already).
pub fn check_macro_call_labels_in(m: &Ast, registry: &MacroRegistry) -> Vec<CompilerError> {
    call_label_errors(m, &registry.0)
}

fn call_label_errors(m: &Ast, reg: &Registry) -> Vec<CompilerError> {
    if reg.is_empty() {
        return Vec::new();
    }
    let mut errs = Vec::new();
    for (e, data) in m.exprs.iter() {
        let ExprKind::FunctionCall { func, args, .. } = &data.kind else {
            continue;
        };
        let (name, extra) = match &m.exprs[*func].kind {
            ExprKind::Ident(id) => (id.name, 0),
            ExprKind::FieldAccess(_, member) => (member.name, 1),
            _ => continue,
        };
        let Some(cands) = reg.get(&name) else {
            continue;
        };
        // Labels can only appear inside the parens, so a call with none
        // binds positionally and cannot fail this check.
        if args.iter().all(|a| a.name.is_none()) {
            continue;
        }
        let Some(tmpl) = select_overload(cands, args.len() + extra) else {
            continue;
        };
        let mut call_args: Vec<MacroArg> = Vec::with_capacity(args.len() + extra);
        if extra == 1 {
            call_args.push(MacroArg {
                label: None,
                value: e,
            });
        }
        call_args.extend(args.iter().map(|a| MacroArg {
            label: a.name.as_ref().map(|n| n.name),
            value: a.value,
        }));
        let Err(err) = bind_macro_args(&tmpl.params, &call_args) else {
            continue;
        };
        let names: Vec<String> = tmpl.params.iter().map(|p| p.name.to_string()).collect();
        let msg = match err {
            BindErr::UnknownLabel(l) => format!(
                "macro `{name}` has no parameter named `{l}` -- its parameters are ({}). An argument label must name a parameter; it is not a positional argument.",
                names.join(", ")
            ),
            BindErr::AlreadyBound(l) => {
                format!("macro `{name}` got two values for parameter `{l}`")
            }
            BindErr::LabelOnVariadic(l) => format!(
                "macro `{name}`'s parameter `{l}` is variadic -- it captures the remaining arguments, so it cannot be named"
            ),
        };
        errs.push(CompilerError::new_with_label(data.span, msg));
    }
    errs
}

/// Expand every macro call in a SINGLE module against a prebuilt `registry`,
/// then drop that module's macro defs. This is the per-module unit that
/// `expand_macros` applies to each module -- exposed so an incremental driver
/// can re-expand just one edited file. Independent across modules (the registry
/// is prebuilt and expansion only mutates `ast`), so `expand_one` over each
/// module yields exactly the batch `expand_macros` result. Idempotent on an
/// already-expanded module: defs are gone and no registered call remains.
pub(crate) fn expand_one(ast: &mut Ast, registry: &MacroRegistry) {
    let reg = &registry.0;
    if !reg.is_empty() {
        // Item-macro invocations splice their items in first, so the spliced
        // bodies also get expr-expanded by the walk below.
        expand_top_level_macros(ast, reg);
        // Skip the recursive expr walk when no call names a registered macro:
        // `try_expand` only mutates a `name(args)` whose `name` is a registry
        // key, so a flat scan that finds none proves `expand_in_ast` would be a
        // pure no-op; in a project with one macro definition nearly every module skips it.
        if module_calls_macro(ast, reg) {
            expand_in_ast(ast, reg);
        }
    }
    // Macro defs are not typed entities -- remove them (rock parity).
    ast.items
        .retain(|it| !matches!(&it.kind, ItemKind::Function(f) if f.is_macro));
}

/// Expand every macro call across the workspace, then drop the macro defs.
pub fn expand_macros(modules: &mut [Ast]) {
    let registry = collect_registry(modules.iter());
    // Each module expands independently against the immutable prebuilt registry
    // (see `expand_one`), so the per-module work parallelizes -- and on real
    // workspaces a macro like `log_once` is called from ~half the modules, so
    // the recursive walk is the dominant cost, not a rare case.
    use rayon::prelude::*;
    modules.par_iter_mut().for_each(|ast| {
        expand_one(ast, &registry);
    });
}

/// Allocation-free over-approximation of "does any expression in `ast` invoke a
/// registered macro?": a flat scan of the expr arena for a `FunctionCall` whose
/// callee is an `Ident` naming a registry key — exactly `try_expand`'s trigger.
/// A `false` is exact (the recursive walk would do nothing); a `true` may also
/// match a macro-def body's inner call (those items are skipped by
/// `expand_in_ast` anyway), so the worst case is a redundant walk, never a
/// missed expansion. Output stays byte-identical (gated by `skull-cook-diff`).
fn module_calls_macro(ast: &Ast, reg: &Registry) -> bool {
    ast.exprs.values().any(|ed| {
        if let ExprKind::FunctionCall { func, .. } = &ed.kind {
            return match &ast.exprs[*func].kind {
                // plain `name(args)`
                ExprKind::Ident(id) => reg.contains_key(&id.name),
                // UFCS `recv.name(args)` -- the member names the macro
                ExprKind::FieldAccess(_, member) => reg.contains_key(&member.name),
                _ => false,
            };
        }
        false
    })
}

/// Fragment kind a macro param's declared type names (`x: ty`, `n: ident`, ...).
fn fragment_kind(ast: &Ast, ty: Idx<TypeExprData>) -> Option<Frag> {
    let TypeExprKind::Ident(name) = &ast.types[ty].kind else {
        return None;
    };
    match name.name.as_str() {
        "ident" => Some(Frag::Ident),
        "ty" => Some(Frag::Ty),
        "expr" | "stmt" | "pat" | "item" | "tt" | "lit" => Some(Frag::Expr),
        _ => None,
    }
}

fn build_registry<'a>(modules: impl IntoIterator<Item = &'a Ast>) -> Registry {
    let mut reg = Registry::default();
    for m in modules {
        for item in &m.items {
            let ItemKind::Function(f) = &item.kind else {
                continue;
            };
            if !f.is_macro {
                continue;
            }
            let Some(body_block) = f.body.rock_block() else {
                continue;
            };
            let mut tmpl = Ast::new();
            let body = graft_block(m, &mut tmpl, body_block);
            let return_ty = f.return_ty.map(|t| graft_type(m, &mut tmpl, t));
            let items = f
                .macro_items
                .iter()
                .map(|it| graft_item(m, &mut tmpl, it))
                .collect();
            let params = f
                .params
                .iter()
                .map(|p| MParam {
                    name: p.name.name,
                    is_variadic: p.is_variadic,
                    default: p.default.map(|d| graft_expr(m, &mut tmpl, d)),
                    fragment: fragment_kind(m, p.ty),
                })
                .collect();
            reg.entry(f.name.name).or_default().push(MacroTemplate {
                params,
                body,
                return_ty,
                items,
                tmpl,
                def_name_span: f.name.span,
            });
        }
    }
    reg
}

/// Replace each top-level `name(args)` / `name!(args)` invocation with the
/// matching item-macro's spliced + substituted items. An invocation that names
/// no item macro at its arity is rejected rather than dropped.
fn expand_top_level_macros(ast: &mut Ast, reg: &Registry) {
    if !ast
        .items
        .iter()
        .any(|it| matches!(&it.kind, ItemKind::MacroInvocation { .. }))
    {
        return;
    }
    let mut out: Vec<Item> = Vec::with_capacity(ast.items.len());
    for item in std::mem::take(&mut ast.items) {
        let ItemKind::MacroInvocation { name, args, .. } = &item.kind else {
            out.push(item);
            continue;
        };
        // `ItemKind::MacroInvocation` carries bare values -- the top-level
        // invocation form has no label syntax -- so every arg here is positional.
        let (name, args) = (name.clone(), args.clone());
        let args: Vec<MacroArg> = args
            .into_iter()
            .map(|value| MacroArg { label: None, value })
            .collect();
        // Dropping an unexpandable invocation would compile the module with
        // that part of the source silently missing.
        let span = item.span;
        let errors_before = ast.recovery_errors.len();
        match expand_item_macro(ast, name.clone(), &args, reg) {
            Some(items) => out.extend(items),
            None if ast.recovery_errors.len() > errors_before => {}
            None => {
                let message = match reg.get(&name.name) {
                    None => format!(
                        "`{}(...)` at the top level invokes a macro, but no macro named `{}` exists",
                        name.name, name.name
                    ),
                    Some(cands) if cands.iter().all(|d| d.items.is_empty()) => format!(
                        "macro `{}` declares no items, so it cannot be invoked at the top level",
                        name.name
                    ),
                    Some(cands) if !cands.iter().any(|d| item_arity_matches(d, args.len())) => {
                        format!(
                            "macro `{}` declares no items for {} argument{}",
                            name.name,
                            args.len(),
                            if args.len() == 1 { "" } else { "s" }
                        )
                    }
                    Some(_) => format!(
                        "an argument of item macro `{}` is not a valid fragment for its parameter",
                        name.name
                    ),
                };
                ast.recovery_errors
                    .push(CompilerError::new_with_label(span, message));
            }
        }
    }
    ast.items = out;
}

/// Is `d` an item macro that accepts `argc` positional arguments?
fn item_arity_matches(d: &MacroTemplate, argc: usize) -> bool {
    let total = d.params.len();
    let required = d.params.iter().filter(|p| p.default.is_none()).count();
    !d.items.is_empty() && required <= argc && argc <= total
}

/// Does a macro body hold a statement a top-level invocation would discard?
fn has_body_statements(tmpl: &MacroTemplate) -> bool {
    tmpl.tmpl.blocks[tmpl.body]
        .statements
        .iter()
        .any(|s| !matches!(tmpl.tmpl.stmts[*s].kind, StatementKind::Comment(_)))
}

/// Select the item-macro overload for `name(args)`, build the fragment/expr
/// substitutions, graft the macro_items into `ast`, substitute, and return them.
fn expand_item_macro(
    ast: &mut Ast,
    name: Ident,
    args: &[MacroArg],
    reg: &Registry,
) -> Option<Vec<Item>> {
    let argc = args.len();
    let cands = reg.get(&name.name)?;
    let tmpl = cands.iter().find(|d| item_arity_matches(d, argc))?;
    if has_body_statements(tmpl) {
        ast.recovery_errors.push(CompilerError::new_with_label(
            name.span,
            format!(
                "statements in macro `{}` cannot run at the top level; only its items are emitted there",
                name.name
            ),
        ));
        return None;
    }

    // Build substitution maps from the param fragment kinds. ty/ident args
    // recover a type via `expr_to_type_expr`; ident also substitutes into expr
    // positions (ctor names / refs). expr args substitute whole subtrees.
    let mut ty_subs: UstrMap<Idx<TypeExprData>> = UstrMap::default();
    let mut expr_subs: UstrMap<Idx<ExprData>> = UstrMap::default();
    // Same by-label-then-by-position binding as the expr path; item macros take
    // no variadic, so only the slots matter.
    let bound = bind_macro_args(&tmpl.params, args).ok()?;
    for (p, slot) in tmpl.params.iter().zip(bound.slots) {
        let arg = match slot {
            Some(arg) => arg,
            None => graft_expr(&tmpl.tmpl, ast, p.default?),
        };
        match p.fragment {
            Some(Frag::Ident) | Some(Frag::Ty) => {
                let te = expr_to_type_expr(ast, arg)?;
                ty_subs.insert(p.name, te);
                if p.fragment == Some(Frag::Ident) {
                    expr_subs.insert(p.name, arg);
                }
            }
            _ => {
                expr_subs.insert(p.name, arg);
            }
        }
    }

    // Graft the template items into the caller, substituting the args.
    let fragments = Fragments {
        exprs: expr_subs,
        types: ty_subs,
    };
    let items = graft_items_with(&tmpl.tmpl, ast, &tmpl.items, fragments);
    // Macro-generated module headers, `mod` declarations, and `use`
    // declarations are rejected: file/module/import selection is
    // fixed before expansion, so generated ones would mis-scope output.
    for it in &items {
        if let ItemKind::Use(_) | ItemKind::Module(_) = &it.kind {
            ast.recovery_errors.push(CompilerError::new_with_label(
                it.span,
                "macro-generated `mod`/`use` is not supported".to_string(),
            ));
            return None;
        }
    }
    Some(items)
}

fn expand_in_ast(ast: &mut Ast, reg: &Registry) {
    let mut blocks: Vec<(Idx<BlockData>, ShadowScope)> = Vec::new();
    let mut exprs: Vec<Idx<ExprData>> = Vec::new();
    for item in &ast.items {
        match &item.kind {
            ItemKind::Function(f) if !f.is_macro => {
                let params: ShadowScope = f.params.iter().map(|p| p.name.name).collect();
                blocks.extend(f.body.rock_block().map(|b| (b, params)));
            }
            ItemKind::Decorator(d) => {
                blocks.push((d.body, d.params.iter().map(|p| p.name.name).collect()))
            }
            ItemKind::Global(g) => {
                if let Some(v) = g.value {
                    exprs.push(v);
                }
            }
            ItemKind::Struct(s) => {
                for fl in &s.fields {
                    if let Some(d) = fl.default {
                        exprs.push(d);
                    }
                }
            }
            _ => {}
        }
    }
    for (b, params) in blocks {
        let mut stack = vec![params];
        expand_block(ast, b, reg, 0, &mut stack);
    }
    for e in exprs {
        expand_expr(ast, e, reg, 0, &mut Vec::new());
    }
}

/// Names bound by enclosing user scopes at the current point of the expansion
/// walk. A call whose callee is shadowed is a real call to that binding (a
/// fn-typed param, a local fn value, a nested `fn`), never a macro invocation —
/// without this, `macro key(..)` captures the stdlib's `min_by(it, key: ..)`
/// body call `key(x)` and splices itself into the stdlib AST.
type ShadowScope = UstrSet;

fn is_shadowed(stack: &[ShadowScope], name: Ustr) -> bool {
    stack.iter().any(|s| s.contains(&name))
}

/// Collect the names a `let`/`for` target binds.
fn shadow_binding(b: &Binding, scope: &mut ShadowScope) {
    match b {
        Binding::Ident(id) => {
            scope.insert(id.name);
        }
        Binding::Tuple(bs) => {
            for b in bs {
                shadow_binding(b, scope);
            }
        }
        Binding::Newtype { binding, .. } => {
            scope.insert(binding.name);
        }
    }
}

/// Collect the names a match/if-let/while-let pattern binds.
fn shadow_pattern(p: &Pattern, scope: &mut ShadowScope) {
    match p {
        Pattern::Wildcard | Pattern::Literal(_) | Pattern::Range { .. } => {}
        Pattern::Ident(id) => {
            scope.insert(id.name);
        }
        Pattern::Tuple(ps) | Pattern::Or(ps) => {
            for p in ps {
                shadow_pattern(p, scope);
            }
        }
        Pattern::EnumVariant { args, .. } => {
            if let Some(ps) = args {
                for p in ps {
                    shadow_pattern(p, scope);
                }
            }
        }
        Pattern::Record { fields, .. } => {
            for (_, sub) in fields {
                shadow_pattern(sub, scope);
            }
        }
        Pattern::PathVariant { args, .. } => {
            if let Some(ps) = args {
                for p in ps {
                    shadow_pattern(p, scope);
                }
            }
        }
        Pattern::PathRecord { fields, .. } => {
            for (_, sub) in fields {
                shadow_pattern(sub, scope);
            }
        }
    }
}

fn expand_block(
    ast: &mut Ast,
    b: Idx<BlockData>,
    reg: &Registry,
    depth: usize,
    stack: &mut Vec<ShadowScope>,
) {
    let stmts = ast.blocks[b].statements.clone();
    // Nested fns are hoisted: their names bind for the whole block.
    let mut scope = ShadowScope::default();
    for &s in &stmts {
        if let StatementKind::NestedFn(f) = &ast.stmts[s].kind {
            scope.insert(f.name.name);
        }
    }
    stack.push(scope);
    for s in stmts {
        expand_stmt(ast, s, reg, depth, stack);
    }
    stack.pop();
}

fn expand_stmt(
    ast: &mut Ast,
    s: Idx<StatementData>,
    reg: &Registry,
    depth: usize,
    stack: &mut Vec<ShadowScope>,
) {
    match &ast.stmts[s].kind {
        StatementKind::Let(l) => {
            // RHS sees the outer scope; the binding shadows for the rest of the
            // current block (the top scope frame).
            let (value, binding) = (l.value, l.binding.clone());
            if let Some(v) = value {
                expand_expr(ast, v, reg, depth, stack);
            }
            let top = stack.last_mut().expect("expand_stmt: block scope present");
            shadow_binding(&binding, top);
        }
        StatementKind::Static(st) => {
            let (value, name) = (st.value, st.name.name);
            expand_expr(ast, value, reg, depth, stack);
            let top = stack.last_mut().expect("expand_stmt: block scope present");
            top.insert(name);
        }
        StatementKind::For(f) => {
            let (iter, body, var) = (f.iter, f.body, f.var.clone());
            expand_expr(ast, iter, reg, depth, stack);
            let mut scope = ShadowScope::default();
            shadow_binding(&var, &mut scope);
            stack.push(scope);
            expand_block(ast, body, reg, depth, stack);
            stack.pop();
        }
        StatementKind::WhileLet(w) => {
            let (value, body, pattern) = (w.value, w.body, w.pattern.clone());
            expand_expr(ast, value, reg, depth, stack);
            let mut scope = ShadowScope::default();
            shadow_pattern(&pattern, &mut scope);
            stack.push(scope);
            expand_block(ast, body, reg, depth, stack);
            stack.pop();
        }
        StatementKind::NestedFn(f) => {
            // The fn NAME is already bound at block entry (hoisted); its params
            // scope only the body. Param defaults see the outer scope.
            let params: ShadowScope = f.params.iter().map(|p| p.name.name).collect();
            let defaults: Vec<Idx<ExprData>> = f.params.iter().filter_map(|p| p.default).collect();
            let body = f.body;
            for d in defaults {
                expand_expr(ast, d, reg, depth, stack);
            }
            stack.push(params);
            expand_block(ast, body, reg, depth, stack);
            stack.pop();
        }
        _ => {
            for e in stmt_exprs(ast, s) {
                expand_expr(ast, e, reg, depth, stack);
            }
            for bl in stmt_blocks(ast, s) {
                expand_block(ast, bl, reg, depth, stack);
            }
        }
    }
}

/// Post-order (bone parity): expand inner calls before the enclosing one, so a
/// macro argument that is itself a macro call is expanded first.
/// A pattern or param list scopes only the body; the scrutinee/value is walked
/// in the outer scope.
fn expand_expr(
    ast: &mut Ast,
    e: Idx<ExprData>,
    reg: &Registry,
    depth: usize,
    stack: &mut Vec<ShadowScope>,
) {
    match &ast.exprs[e].kind {
        ExprKind::Lambda { .. } => {
            let (params, body) = match &ast.exprs[e].kind {
                ExprKind::Lambda { params, body, .. } => (
                    params.iter().map(|p| p.name.name).collect::<ShadowScope>(),
                    *body,
                ),
                _ => unreachable!(),
            };
            stack.push(params);
            expand_block(ast, body, reg, depth, stack);
            stack.pop();
            return;
        }
        ExprKind::IfLetChain { .. } => {
            let (n_clauses, then_b, else_b) = match &ast.exprs[e].kind {
                ExprKind::IfLetChain {
                    clauses,
                    then_block,
                    else_block,
                } => (clauses.len(), *then_block, *else_block),
                _ => unreachable!(),
            };
            // Bindings accumulate left-to-right: a later clause's value sees the
            // earlier clauses' bindings.
            stack.push(ShadowScope::default());
            for i in 0..n_clauses {
                let (value, pat_scope) = match &ast.exprs[e].kind {
                    ExprKind::IfLetChain { clauses, .. } => match &clauses[i] {
                        IfLetClause::Binding { value, pattern } => {
                            let mut scope = ShadowScope::default();
                            shadow_pattern(pattern, &mut scope);
                            (*value, Some(scope))
                        }
                        IfLetClause::Condition(c) => (*c, None),
                    },
                    _ => unreachable!(),
                };
                expand_expr(ast, value, reg, depth, stack);
                if let Some(scope) = pat_scope {
                    let top = stack.last_mut().expect("expand_expr: chain scope present");
                    top.extend(scope);
                }
            }
            expand_block(ast, then_b, reg, depth, stack);
            stack.pop();
            if let Some(eb) = else_b {
                expand_block(ast, eb, reg, depth, stack);
            }
            return;
        }
        ExprKind::Match { .. } => {
            let (scrutinee, n_arms) = match &ast.exprs[e].kind {
                ExprKind::Match { scrutinee, arms } => (*scrutinee, arms.len()),
                _ => unreachable!(),
            };
            expand_expr(ast, scrutinee, reg, depth, stack);
            for i in 0..n_arms {
                let (guard, body, mut scope) = match &ast.exprs[e].kind {
                    ExprKind::Match { arms, .. } => {
                        let mut scope = ShadowScope::default();
                        shadow_pattern(&arms[i].pattern, &mut scope);
                        (arms[i].guard, arms[i].body, scope)
                    }
                    _ => unreachable!(),
                };
                stack.push(std::mem::take(&mut scope));
                if let Some(g) = guard {
                    expand_expr(ast, g, reg, depth, stack);
                }
                expand_block(ast, body, reg, depth, stack);
                stack.pop();
            }
            return;
        }
        ExprKind::SafeNavigationLet { .. } => {
            let (receiver, body, name) = match &ast.exprs[e].kind {
                ExprKind::SafeNavigationLet {
                    receiver,
                    body,
                    binding,
                    ..
                } => (*receiver, *body, binding.name),
                _ => unreachable!(),
            };
            expand_expr(ast, receiver, reg, depth, stack);
            stack.push(ShadowScope::from_iter([name]));
            expand_block(ast, body, reg, depth, stack);
            stack.pop();
            return;
        }
        _ => {}
    }
    for child in child_exprs(ast, e) {
        expand_expr(ast, child, reg, depth, stack);
    }
    for blk in child_blocks(ast, e) {
        expand_block(ast, blk, reg, depth, stack);
    }
    try_expand(ast, e, reg, depth, stack);
}

/// If `e` is a `name(args)` call where `name` is a registered macro, splice the
/// substituted body in as a `Block`.
fn try_expand(
    ast: &mut Ast,
    e: Idx<ExprData>,
    reg: &Registry,
    depth: usize,
    stack: &mut Vec<ShadowScope>,
) {
    // Two invocation shapes: a plain `name(args)` and a UFCS `recv.name(args)`
    // (member-position macro -- the receiver becomes the first arg, so the slot
    // macros read naturally in `player.cast_co.poll_co_slot()` form). A macro
    // name is globally unique against fn/method names (`check_macro_name_
    // uniqueness`), so the member form is unambiguously the macro when `name` is
    // registered. Field/local shadowing only applies to the plain-ident form: a
    // local can shadow a bare callee name, never a member name.
    let (name, arg_values, is_ident) = {
        let ExprKind::FunctionCall { func, args, .. } = &ast.exprs[e].kind else {
            return;
        };
        let func = *func;
        // The caller's labels ride along: binding is by NAME when one is given,
        // by position otherwise (`bind_macro_args`).
        let arg_idxs: Vec<MacroArg> = args
            .iter()
            .map(|a| MacroArg {
                label: a.name.as_ref().map(|n| n.name),
                value: a.value,
            })
            .collect();
        match &ast.exprs[func].kind {
            ExprKind::Ident(id) => (id.name, arg_idxs, true),
            ExprKind::FieldAccess(recv, member) => {
                // UFCS receiver takes the first slot positionally -- it is
                // written before the parens, so it can carry no label.
                let mut vs = Vec::with_capacity(arg_idxs.len() + 1);
                vs.push(MacroArg {
                    label: None,
                    value: *recv,
                });
                vs.extend(arg_idxs);
                (member.name, vs, false)
            }
            _ => return,
        }
    };
    if !reg.contains_key(&name) {
        return;
    }
    // A locally-bound callee (fn-typed param, local fn value, nested fn, loop
    // binding) shadows any same-named macro — this is a real call.
    if is_ident && is_shadowed(stack, name) {
        return;
    }
    if depth >= MAX_MACRO_DEPTH {
        return;
    }

    let Some(tmpl) = select_overload(&reg[&name], arg_values.len()) else {
        return;
    };
    // Item declarations only reach a module through a top-level invocation;
    // expanding just the body here would drop them (a `use` included).
    if !tmpl.items.is_empty() {
        let span = ast.exprs[e].span;
        ast.recovery_errors.push(CompilerError::new_with_label(
            span,
            format!(
                "items declared by macro `{name}` cannot be emitted where it is used as an expression; invoke it at the top level"
            ),
        ));
        return;
    }

    // Bind params: a regular param takes the arg that names it or the next
    // positional one (or its default); a variadic param captures all trailing
    // args, spliced wherever the body forwards `...rest`.
    //
    // A binding error is reported by `check_macro_call_labels`, which runs the
    // same binder before the compile proceeds; leaving the call unexpanded here
    // is what makes that report the only thing the user sees.
    let Ok(bound) = bind_macro_args(&tmpl.params, &arg_values) else {
        return;
    };
    let mut inst = Instantiation {
        callsite: e.into_raw().into_u32(),
        counter: 0,
        call_span: ast.exprs[e].span,
        scopes: Vec::new(),
        subs: UstrMap::default(),
        variadic: UstrMap::default(),
    };
    for (i, p) in tmpl.params.iter().enumerate() {
        if p.is_variadic {
            let rest = bound.variadic.as_ref().filter(|(vi, _)| *vi == i);
            let rest = rest.map(|(_, v)| v.clone()).unwrap_or_default();
            inst.variadic.insert(p.name, rest);
        } else if let Some(a) = bound.slots[i] {
            inst.subs.insert(p.name, a);
        } else if let Some(d) = p.default {
            let g = graft_expr(&tmpl.tmpl, ast, d);
            inst.subs.insert(p.name, g);
        }
    }

    // Copy the body into the caller arena, rewriting it on the way
    // (`Instantiation`). Args and defaults are already in the caller arena and
    // are only ever copied plainly, so they keep their own names and spans.
    let body = graft_block_with(&tmpl.tmpl, ast, tmpl.body, inst);
    // Expand macros that appear inside the spliced body (macros calling macros).
    expand_block(ast, body, reg, depth + 1, stack);

    // Reduce the body to its yielded value (rock's `extract_body_expr`): a lone
    // statement collapses to its bare trailing expr, a multi-statement body stays
    // a block. A declared return type then ascribes the result — `(:: <body>
    // <ret>)` — giving bone the same expected-type signal rock's expansion does
    // (e.g. nullable-branch unification in a `-> int?` macro). Unconditional when
    // a return type is present (rock parity, `8bb1ea55e`).
    let value = extract_body_value(ast, body);
    ast.exprs[e].kind = match tmpl.return_ty {
        Some(rt) => {
            let rt = graft_type(&tmpl.tmpl, ast, rt);
            ExprKind::TypeAscription(value, rt)
        }
        None => ast.exprs[value].kind.clone(),
    };
}

/// Reduce a macro body block to the single value expression it yields, mirroring
/// rock's `extract_body_expr`. A trailing `return e` is rewritten to the bare
/// expression `e`; a lone meaningful statement then collapses to that expr (no
/// block wrapper), otherwise the block is kept. Returns the value's expr index.
fn extract_body_value(ast: &mut Ast, body: Idx<BlockData>) -> Idx<ExprData> {
    let meaningful: Vec<Idx<StatementData>> = ast.blocks[body]
        .statements
        .iter()
        .copied()
        .filter(|s| !matches!(ast.stmts[*s].kind, StatementKind::Comment(_)))
        .collect();

    if let Some(&last) = meaningful.last() {
        let trailing = match &ast.stmts[last].kind {
            StatementKind::Return { value: Some(e), .. } => Some(*e),
            _ => None,
        };
        if let Some(e) = trailing {
            ast.stmts[last].kind = StatementKind::Expression(e);
        }
    }

    if meaningful.len() == 1 {
        if let StatementKind::Expression(e) = &ast.stmts[meaningful[0]].kind {
            return *e;
        }
    }

    let span = ast.blocks[body].span;
    ast.exprs.alloc(ExprData {
        span,
        kind: ExprKind::Block(body),
    })
}

// ---------------------------------------------------------------------------
// Fragment recovery + item substitution (item-macros / sigil `ty:` args)
// ---------------------------------------------------------------------------

/// Recover a `Type` (allocated into `ast`) from a macro-arg expr — reverses the
/// parser's type->arg encoding. `Foo<T>` is a `FunctionCall`, `[T]` an array
/// literal, `(T,U)` a tuple, `T?` a `QuestionMark`. Mirror of bone's
/// `expr_to_type_expr`.
fn expr_to_type_expr(ast: &mut Ast, e: Idx<ExprData>) -> Option<Idx<TypeExprData>> {
    let span = ast.exprs[e].span;
    let kind = ast.exprs[e].kind.clone();
    match kind {
        ExprKind::QuestionMark(inner) => {
            let t = expr_to_type_expr(ast, inner)?;
            let inner_kind = ast.types[t].kind.clone();
            Some(ast.ty(span, inner_kind, true))
        }
        ExprKind::Paren(inner) => expr_to_type_expr(ast, inner),
        ExprKind::Ident(id) => {
            let k = match id.name.as_str() {
                "int" => TypeExprKind::Int,
                "uint" => TypeExprKind::Uint,
                "float" => TypeExprKind::Float,
                "f64" => TypeExprKind::F64,
                "f32" => TypeExprKind::F32,
                "string" => TypeExprKind::String,
                "bool" => TypeExprKind::Bool,
                "any" => TypeExprKind::Any,
                _ => TypeExprKind::Ident(id),
            };
            Some(ast.ty(span, k, false))
        }
        ExprKind::ArrayLiteral(elems) if elems.len() == 1 => {
            let inner = expr_to_type_expr(ast, elems[0])?;
            Some(ast.ty(span, TypeExprKind::Array(inner), false))
        }
        ExprKind::Tuple(elems) => {
            let mut ts = Vec::with_capacity(elems.len());
            for el in elems {
                ts.push(expr_to_type_expr(ast, el)?);
            }
            Some(ast.ty(span, TypeExprKind::Tuple(ts), false))
        }
        ExprKind::FunctionCall { func, args, .. } => {
            let ExprKind::Ident(ctor) = ast.exprs[func].kind.clone() else {
                return None;
            };
            let mut ts = Vec::with_capacity(args.len());
            for a in &args {
                ts.push(expr_to_type_expr(ast, a.value)?);
            }
            Some(ast.ty(span, TypeExprKind::App { ctor, args: ts }, false))
        }
        _ => None,
    }
}

/// An item macro's args, substituted where the spliced items name their
/// params: `expr` and `ident` args in expressions, `ty` and `ident` args in
/// types, and `ident` args in declared names.
struct Fragments {
    exprs: UstrMap<Idx<ExprData>>,
    types: UstrMap<Idx<TypeExprData>>,
}

impl Rewrite for Fragments {
    fn arg(&self, name: Ustr) -> Option<Idx<ExprData>> {
        self.exprs.get(&name).copied()
    }

    fn ty_arg(&self, name: Ustr) -> Option<Idx<TypeExprData>> {
        self.types.get(&name).copied()
    }
}

/// Exact-arity non-variadic overload preferred; else a variadic one whose fixed
/// params are satisfied. Mirror of bone's `select_macro_overload`.
/// One call argument as the expander sees it: the value, plus the label the
/// caller wrote before it (`f(x, speed: 2)`), if any.
#[derive(Clone, Copy)]
struct MacroArg {
    label: Option<Ustr>,
    value: Idx<ExprData>,
}

/// Why a call's args don't fit the macro's params. Carries the offending name
/// so the caller can build a spanned message.
enum BindErr {
    UnknownLabel(Ustr),
    AlreadyBound(Ustr),
    LabelOnVariadic(Ustr),
}

/// Where each param's argument comes from.
struct Bound {
    /// Per param, in declaration order. `None` = no arg reached it, so the
    /// param's own default applies (arity selection already guaranteed it has
    /// one).
    slots: Vec<Option<Idx<ExprData>>>,
    /// (param index, captured args) for a variadic param.
    variadic: Option<(usize, Vec<Idx<ExprData>>)>,
}

/// Map a call's args onto a macro's params the way a fn call maps them:
/// unlabelled args fill params left to right, a labelled arg fills the param it
/// NAMES wherever that sits, and a param no arg reaches falls back to its
/// default.
///
/// This is the single binding implementation -- the expander uses it to bind and
/// `check_macro_call_labels` uses it to report. A second copy of the rule in the
/// checker is how the checker and the expander come to disagree about what a
/// call means.
///
/// Binding by position alone (what this replaced) silently mis-binds every call
/// that labels an argument out of its declared slot, and silently accepts a
/// label naming no parameter at all.
fn bind_macro_args(params: &[MParam], args: &[MacroArg]) -> Result<Bound, BindErr> {
    let mut slots: Vec<Option<Idx<ExprData>>> = vec![None; params.len()];
    let mut variadic: Option<(usize, Vec<Idx<ExprData>>)> = None;
    // Next param a positional arg would fill. Labels never move it: `f(1, c: 3)`
    // leaves 3's slot for the next positional, matching fn-call binding.
    let mut next = 0usize;

    for a in args {
        match a.label {
            Some(l) => {
                let Some(i) = params.iter().position(|p| p.name == l) else {
                    return Err(BindErr::UnknownLabel(l));
                };
                // A variadic captures a RUN of args; naming it binds one, which
                // has no meaning. Reject rather than pick an interpretation.
                if params[i].is_variadic {
                    return Err(BindErr::LabelOnVariadic(l));
                }
                if slots[i].is_some() {
                    return Err(BindErr::AlreadyBound(l));
                }
                slots[i] = Some(a.value);
            }
            None => {
                while next < params.len() && slots[next].is_some() {
                    next += 1;
                }
                if next >= params.len() {
                    // Arity selection admitted this call, so the only way to run
                    // past the last param is a trailing variadic; without one
                    // there is no slot and the overload was mis-selected.
                    match variadic.as_mut() {
                        Some((_, rest)) => rest.push(a.value),
                        None => {
                            return Err(BindErr::UnknownLabel(
                                params.last().map(|p| p.name).unwrap_or_default(),
                            ));
                        }
                    }
                    continue;
                }
                if params[next].is_variadic {
                    variadic.get_or_insert((next, Vec::new())).1.push(a.value);
                } else {
                    slots[next] = Some(a.value);
                    next += 1;
                }
            }
        }
    }

    // A variadic that no arg reached still binds -- to nothing.
    if variadic.is_none() {
        if let Some(i) = params.iter().position(|p| p.is_variadic) {
            variadic = Some((i, Vec::new()));
        }
    }
    Ok(Bound { slots, variadic })
}

fn select_overload(cands: &[MacroTemplate], argc: usize) -> Option<&MacroTemplate> {
    let mut exact: Option<&MacroTemplate> = None;
    let mut variadic: Option<&MacroTemplate> = None;
    for d in cands {
        match d.params.iter().position(|p| p.is_variadic) {
            Some(v_idx) => {
                if argc >= v_idx && variadic.is_none() {
                    variadic = Some(d);
                }
            }
            None => {
                let total = d.params.len();
                let required = d.params.iter().filter(|p| p.default.is_none()).count();
                if argc >= required && argc <= total {
                    exact = Some(d);
                }
            }
        }
    }
    exact.or(variadic)
}

// ---------------------------------------------------------------------------
// Instantiation: the rewrite a macro body gets as it is copied to a call site
//
// Hygiene (rock's `rename_body_locals_for_macro`): every binder the body
// introduces is renamed to a fresh `__inline_local_<callsite>_<n>_<orig>`, and
// references resolve to the nearest enclosing rename, so a body local never
// captures a substituted arg that shares its surface name. A reference that
// resolves to a body local is therefore never a param, nor a `...rest` splat.
// `^name` (AnaphoricIdent) is an explicit caller-scope reference and keeps its
// name.
//
// Span stamp: every body node gets the call's span, so `__SITE__` resolves to
// the caller's position and body-originated errors report at the call. Nested
// macro calls in the body carry it on, so the outermost call's span wins.
// ---------------------------------------------------------------------------

struct Instantiation {
    /// The call's arena index: the per-expansion part of fresh names.
    callsite: u32,
    counter: u32,
    call_span: Span,
    scopes: Vec<UstrMap<Ustr>>,
    subs: UstrMap<Idx<ExprData>>,
    variadic: UstrMap<Vec<Idx<ExprData>>>,
}

impl Instantiation {
    fn renamed(&self, name: Ustr) -> Option<Ustr> {
        self.scopes.iter().rev().find_map(|s| s.get(&name).copied())
    }
}

impl Rewrite for Instantiation {
    fn stamp(&self) -> Option<Span> {
        Some(self.call_span)
    }

    fn enter(&mut self) {
        self.scopes.push(UstrMap::default());
    }

    fn exit(&mut self) {
        self.scopes.pop();
    }

    fn bind(&mut self, name: &mut Ustr) {
        let fresh = Ustr::from(&format!(
            "{HYGIENE_PREFIX}{}_{}_{}",
            self.callsite, self.counter, name
        ));
        self.counter += 1;
        let scope = self.scopes.last_mut().expect("binder outside a scope");
        scope.insert(*name, fresh);
        *name = fresh;
    }

    fn bind_binding(&mut self, b: &mut Binding) {
        match b {
            Binding::Ident(id) | Binding::Newtype { binding: id, .. } => self.bind(&mut id.name),
            Binding::Tuple(bs) => bs.iter_mut().for_each(|b| self.bind_binding(b)),
        }
    }

    fn bind_pattern(&mut self, p: &mut Pattern) {
        match p {
            Pattern::Wildcard | Pattern::Literal(_) | Pattern::Range { .. } => {}
            Pattern::Ident(id) => self.bind(&mut id.name),
            Pattern::Tuple(ps)
            | Pattern::Or(ps)
            | Pattern::EnumVariant { args: Some(ps), .. }
            | Pattern::PathVariant { args: Some(ps), .. } => {
                ps.iter_mut().for_each(|p| self.bind_pattern(p))
            }
            Pattern::EnumVariant { args: None, .. } | Pattern::PathVariant { args: None, .. } => {}
            Pattern::Record { fields, .. } | Pattern::PathRecord { fields, .. } => {
                fields.iter_mut().for_each(|(_, p)| self.bind_pattern(p))
            }
        }
    }

    fn resolve(&self, name: Ustr) -> Ustr {
        self.renamed(name).unwrap_or(name)
    }

    fn arg(&self, name: Ustr) -> Option<Idx<ExprData>> {
        match self.renamed(name) {
            Some(_) => None,
            None => self.subs.get(&name).copied(),
        }
    }

    fn splat(&self, name: Ustr) -> Option<&[Idx<ExprData>]> {
        match self.renamed(name) {
            Some(_) => None,
            None => self.variadic.get(&name).map(Vec::as_slice),
        }
    }
}

// ---------------------------------------------------------------------------
// Statement child enumeration (full coverage, for the expand + substitute walks)
// ---------------------------------------------------------------------------

fn stmt_exprs(ast: &Ast, s: Idx<StatementData>) -> ExprEdges {
    match &ast.stmts[s].kind {
        StatementKind::Expression(e)
        | StatementKind::Return { value: Some(e), .. }
        | StatementKind::Defer(DeferKind::Expression(e)) => smallvec![*e],
        StatementKind::Let(l) => l.value.into_iter().collect(),
        StatementKind::Static(st) => smallvec![st.value],
        StatementKind::NestedFn(f) => f.params.iter().filter_map(|p| p.default).collect(),
        StatementKind::For(f) => smallvec![f.iter],
        StatementKind::While(w) => smallvec![w.condition],
        StatementKind::WhileLet(w) => smallvec![w.value],
        _ => ExprEdges::new(),
    }
}

fn stmt_blocks(ast: &Ast, s: Idx<StatementData>) -> BlockEdges {
    match &ast.stmts[s].kind {
        StatementKind::For(f) => smallvec![f.body],
        StatementKind::While(w) => smallvec![w.body],
        StatementKind::WhileLet(w) => smallvec![w.body],
        StatementKind::Defer(DeferKind::Block(b)) => smallvec![*b],
        StatementKind::LifecycleHook { body, .. } => smallvec![*body],
        StatementKind::NestedFn(f) => smallvec![f.body],
        _ => BlockEdges::new(),
    }
}

/// Per parameter, its declaration span + every use span in the macro template.
pub(crate) fn index_macro_param_refs(
    ast: &Ast,
    params: &[Param],
    body: Option<Idx<BlockData>>,
    macro_items: &[Item],
) -> Vec<MacroParamRef> {
    let names: UstrSet = params.iter().map(|p| p.name.name).collect();
    if names.is_empty() {
        return Vec::new();
    }

    let mut occ: Vec<(Ustr, Span)> = params.iter().map(|p| (p.name.name, p.name.span)).collect();
    if let Some(body) = body {
        refs_in_block(ast, body, &names, &mut occ);
    }
    for it in macro_items {
        refs_in_item(ast, it, &names, &mut occ);
    }

    params
        .iter()
        .map(|p| MacroParamRef {
            name: p.name.name,
            spans: occ
                .iter()
                .filter(|(n, _)| *n == p.name.name)
                .map(|(_, s)| *s)
                .collect(),
        })
        .collect()
}

fn note(id: &Ident, names: &UstrSet, occ: &mut Vec<(Ustr, Span)>) {
    if names.contains(&id.name) {
        occ.push((id.name, id.span));
    }
}

fn refs_in_item(ast: &Ast, item: &Item, names: &UstrSet, occ: &mut Vec<(Ustr, Span)>) {
    match &item.kind {
        ItemKind::Struct(s) => {
            note(&s.name, names, occ);
            for f in &s.fields {
                note(&f.name, names, occ);
                refs_in_type(ast, f.ty, names, occ);
                if let Some(d) = f.default {
                    refs_in_expr(ast, d, names, occ);
                }
            }
        }
        ItemKind::Function(f) => {
            note(&f.name, names, occ);
            for p in &f.params {
                note(&p.name, names, occ);
                refs_in_type(ast, p.ty, names, occ);
            }
            if let Some(rt) = f.return_ty {
                refs_in_type(ast, rt, names, occ);
            }
            if let Some(body) = f.body.rock_block() {
                refs_in_block(ast, body, names, occ);
            }
        }
        ItemKind::Enum(e) => {
            note(&e.name, names, occ);
            for v in &e.variants {
                match &v.kind {
                    EnumVariantKind::Unit => {}
                    EnumVariantKind::Tuple(tys) => {
                        for t in tys {
                            refs_in_type(ast, *t, names, occ);
                        }
                    }
                    EnumVariantKind::Record(fields) => {
                        for f in fields {
                            note(&f.name, names, occ);
                            refs_in_type(ast, f.ty, names, occ);
                        }
                    }
                }
            }
        }
        ItemKind::Global(g) => {
            if let Some(t) = g.ty {
                refs_in_type(ast, t, names, occ);
            }
            if let Some(v) = g.value {
                refs_in_expr(ast, v, names, occ);
            }
        }
        _ => {}
    }
}

fn refs_in_type(ast: &Ast, t: Idx<TypeExprData>, names: &UstrSet, occ: &mut Vec<(Ustr, Span)>) {
    match &ast.types[t].kind {
        TypeExprKind::Ident(id) => note(id, names, occ),
        TypeExprKind::Ref(inner) => refs_in_type(ast, *inner, names, occ),
        TypeExprKind::Array(inner) => refs_in_type(ast, *inner, names, occ),
        TypeExprKind::App { ctor, args } => {
            note(ctor, names, occ);
            for a in args {
                refs_in_type(ast, *a, names, occ);
            }
        }
        TypeExprKind::Tuple(ts) => {
            for a in ts {
                refs_in_type(ast, *a, names, occ);
            }
        }
        TypeExprKind::Function { params, ret } => {
            for a in params {
                refs_in_type(ast, *a, names, occ);
            }
            refs_in_type(ast, *ret, names, occ);
        }
        _ => {}
    }
}

fn refs_in_block(ast: &Ast, b: Idx<BlockData>, names: &UstrSet, occ: &mut Vec<(Ustr, Span)>) {
    for &st in &ast.blocks[b].statements {
        if let StatementKind::Let(l) = &ast.stmts[st].kind {
            if let Some(t) = l.ty {
                refs_in_type(ast, t, names, occ);
            }
        }
        for e in stmt_exprs(ast, st) {
            refs_in_expr(ast, e, names, occ);
        }
        for b in stmt_blocks(ast, st) {
            refs_in_block(ast, b, names, occ);
        }
    }
}

fn refs_in_expr(ast: &Ast, e: Idx<ExprData>, names: &UstrSet, occ: &mut Vec<(Ustr, Span)>) {
    match &ast.exprs[e].kind {
        ExprKind::Ident(id) | ExprKind::StructLiteral { name: id, .. } => note(id, names, occ),
        _ => {}
    }
    for child in child_exprs(ast, e) {
        refs_in_expr(ast, child, names, occ);
    }
    for b in child_blocks(ast, e) {
        refs_in_block(ast, b, names, occ);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_module_clean;
    use ustr::Ustr;

    fn parse(src: &str, file: &str) -> Ast {
        parse_module_clean(src, Ustr::from(file)).expect("parse failed")
    }

    fn assert_same_ast(left: &Ast, right: &Ast) {
        assert_eq!(left.items, right.items);
        assert_eq!(
            left.exprs.values().collect::<Vec<_>>(),
            right.exprs.values().collect::<Vec<_>>()
        );
        assert_eq!(
            left.stmts.values().collect::<Vec<_>>(),
            right.stmts.values().collect::<Vec<_>>()
        );
        assert_eq!(
            left.blocks.values().collect::<Vec<_>>(),
            right.blocks.values().collect::<Vec<_>>()
        );
        assert_eq!(
            left.types.values().collect::<Vec<_>>(),
            right.types.values().collect::<Vec<_>>()
        );
    }

    fn count_calls(ast: &Ast, name: &str) -> usize {
        body_nodes(ast)
            .0
            .iter()
            .filter(|&&e| match &ast.exprs[e].kind {
                ExprKind::FunctionCall { func, .. } => matches!(&ast.exprs[*func].kind,
                ExprKind::Ident(id) if id.name == name),
                _ => false,
            })
            .count()
    }

    #[test]
    fn macro_parameter_references_follow_expression_and_statement_children() {
        let src = r#"macro m(arg: expr) {
            if arg { arg } else { !arg }
            for value in arg { consume(arg) }
            while arg { consume(arg) }
            defer { consume(arg) }
            let callback = () => arg
            match arg { _ => arg }
            consume(arg[0], "{arg}")
        }"#;
        let ast = parse(src, "refs.rock");
        let refs = &ast.macro_param_refs[0];
        let mut actual: Vec<_> = refs.spans.iter().map(|s| s.offset_range()).collect();
        actual.sort_unstable();
        let expected: Vec<_> = src
            .match_indices("arg")
            .map(|(start, name)| (start, start + name.len()))
            .collect();
        assert_eq!(actual, expected);
    }

    /// The incremental invariant: cooking ONE module against a registry built
    /// from the whole (un-expanded) workspace yields the same module that the
    /// batch `expand_macros` produces -- including a macro DEFINED in another
    /// file. This is what lets a body edit re-cook just the edited file.
    #[test]
    fn expand_one_against_registry_matches_batch() {
        let defs = "macro twice(x: expr) { x + x }";
        let user = "fn f() -> int { twice(21) }";

        // Batch: expand both together, read the user module's result.
        let mut batch = vec![parse(defs, "defs.rock"), parse(user, "user.rock")];
        expand_macros(&mut batch);

        // Incremental: registry from the un-expanded pair, then expand ONLY the
        // user module against it.
        let unexpanded = vec![parse(defs, "defs.rock"), parse(user, "user.rock")];
        let reg = collect_registry(&unexpanded);
        let mut user_only = parse(user, "user.rock");
        expand_one(&mut user_only, &reg);

        assert_same_ast(&user_only, &batch[1]);
        assert_eq!(count_calls(&user_only, "twice"), 0);
        let nodes = body_nodes(&user_only).0;
        assert!(nodes.iter().any(|&e| matches!(&user_only.exprs[e].kind,
            ExprKind::BinOp { left, right, .. }
                if matches!(user_only.exprs[*left].kind, ExprKind::Int(21, _))
                && matches!(user_only.exprs[*right].kind, ExprKind::Int(21, _)))));
    }

    fn expand_with_key_macro(user: &str) -> Ast {
        let mut mods = vec![
            parse("macro key(x: expr) { x }", "defs.rock"),
            parse(user, "user.rock"),
        ];
        expand_macros(&mut mods);
        mods.pop().unwrap()
    }

    #[track_caller]
    fn assert_call_survives(user: &str) {
        let ast = expand_with_key_macro(user);
        assert!(
            count_calls(&ast, "key") > 0,
            "shadowed call must survive: {user}"
        );
    }

    /// A callee bound by an enclosing scope is a real call, never a macro
    /// invocation — one case per binding form.
    #[test]
    fn shadowed_callee_is_not_expanded() {
        // fn param (the stdlib `min_by(it, key: fn(T) -> float)` capture).
        assert_call_survives("fn f(x: int, key: fn(int) -> int) -> int { return key(x) }");
        // let-bound local.
        assert_call_survives("fn f() -> int { let key = (v: int) => v\n return key(1) }");
        // lambda param.
        assert_call_survives(
            "fn f() -> int { let g = (key: fn(int) -> int) => key(1)\n return g((v: int) => v) }",
        );
        // nested fn (hoisted: the call BEFORE the def is also shadowed).
        assert_call_survives(
            "fn f() -> int { let a = key(1)\n fn key(v: int) -> int { return v }\n return a }",
        );
        // for-loop binding.
        assert_call_survives(
            "fn f(fs: [fn(int) -> int]) -> int { for key in fs { return key(1) }\n return 0 }",
        );
        // match-arm pattern binding.
        assert_call_survives(
            "enum E { A(fn(int) -> int) }\nfn f(e: E) -> int { match e { E::A(key) => { return key(1) } }\n return 0 }",
        );
        // if-let pattern binding.
        assert_call_survives(
            "fn f(k: fn(int) -> int?) -> int { if let key = k { return key(1) }\n return 0 }",
        );
    }

    /// The shadow ends with its scope; sibling and outer positions still expand.
    #[test]
    fn unshadowed_callee_still_expands() {
        // No binding anywhere: the identity macro erases the call.
        let ast = expand_with_key_macro("fn f() -> int { return key(1) }");
        assert_eq!(count_calls(&ast, "key"), 0);
        // Call before the `let` refers to the macro; only the later call is the local.
        let ast = expand_with_key_macro(
            "fn f() -> int { let a = key(1)\n let key = (v: int) => v + v\n return a + key(2) }",
        );
        assert_eq!(
            count_calls(&ast, "key"),
            1,
            "pre-let call expands, post-let call survives"
        );
    }

    /// Re-cooking an already-expanded module is a no-op (defs gone, no call to a
    /// registered macro remains) -- the idempotency the incremental cache relies
    /// on when a body edit re-cooks a file.
    #[test]
    fn macro_reach_names_every_lookup_expansion_can_make() {
        let ast = parse(
            "macro m(f: ident, x: expr) -> int { f(x) }\n\
             fn g(v: int) -> int { v.h() + m(k, v) }\n",
            "a.rock",
        );
        let reach: UstrSet = super::macro_reach(&ast).collect();
        for name in ["m", "f", "h", "k", "v"] {
            assert!(reach.contains(&Ustr::from(name)), "missing {name}");
        }
        assert!(!reach.contains(&Ustr::from("g")));
    }

    #[test]
    fn an_extended_registry_matches_one_collected_over_both() {
        let lib = parse("macro twice(x: expr) -> int { x + x }", "lib.rock");
        let user = parse("macro thrice(x: expr) -> int { x + x + x }", "user.rock");
        let caller = "fn f() -> int { twice(1) + thrice(2) }";
        let expand = |registry: &MacroRegistry| {
            let mut ast = parse(caller, "c.rock");
            expand_one(&mut ast, registry);
            ast
        };
        let both = collect_registry([&lib, &user]);
        let extended = collect_registry([&lib]).extended([&user]);
        assert_same_ast(&expand(&both), &expand(&extended));
    }

    fn body_nodes(ast: &Ast) -> (Vec<Idx<ExprData>>, Vec<Idx<BlockData>>) {
        fn walk_block(
            ast: &Ast,
            b: Idx<BlockData>,
            es: &mut Vec<Idx<ExprData>>,
            bs: &mut Vec<Idx<BlockData>>,
        ) {
            bs.push(b);
            for &s in &ast.blocks[b].statements {
                for e in stmt_exprs(ast, s) {
                    walk_expr(ast, e, es, bs);
                }
                for bl in stmt_blocks(ast, s) {
                    walk_block(ast, bl, es, bs);
                }
            }
        }
        fn walk_expr(
            ast: &Ast,
            e: Idx<ExprData>,
            es: &mut Vec<Idx<ExprData>>,
            bs: &mut Vec<Idx<BlockData>>,
        ) {
            es.push(e);
            for c in child_exprs(ast, e) {
                walk_expr(ast, c, es, bs);
            }
            for bl in child_blocks(ast, e) {
                walk_block(ast, bl, es, bs);
            }
        }
        let body = ast
            .items
            .iter()
            .find_map(|it| match &it.kind {
                ItemKind::Function(f) => f.body.rock_block(),
                _ => None,
            })
            .expect("user fn");
        let (mut es, mut bs) = (Vec::new(), Vec::new());
        walk_block(ast, body, &mut es, &mut bs);
        (es, bs)
    }

    fn expanded_nodes(defs: &str, user: &str) -> (Ast, Vec<Idx<ExprData>>, Vec<Idx<BlockData>>) {
        let mut mods = vec![parse(defs, "defs.rock"), parse(user, "user.rock")];
        expand_macros(&mut mods);
        let ast = mods.pop().unwrap();
        let (es, bs) = body_nodes(&ast);
        (ast, es, bs)
    }

    fn find_ident(ast: &Ast, es: &[Idx<ExprData>], name: &str) -> Idx<ExprData> {
        *es.iter()
            .find(|&&e| matches!(&ast.exprs[e].kind, ExprKind::Ident(id) if id.name == name))
            .unwrap_or_else(|| panic!("no ident {name}"))
    }

    /// Expanded body nodes report at the call (blocks without braces); the
    /// caller's args keep their own spans, so `__SITE__` passed as an arg names
    /// the arg, and a braced block passed as an arg keeps its braces.
    #[test]
    fn expansion_stamps_the_call_span_on_body_nodes_only() {
        let defs = "macro m(a: expr) -> int {\n    let t = 7\n    if t > 0 { t + a } else { a }\n}";
        let user = "fn f() -> int {\n    m(g(__SITE__, { 1 }))\n}";
        let call_span = {
            let ast = parse(user, "user.rock");
            let outermost = ast
                .exprs
                .iter()
                .filter(|(_, d)| matches!(d.kind, ExprKind::FunctionCall { .. }))
                .last();
            outermost.unwrap().1.span
        };
        let (ast, es, bs) = expanded_nodes(defs, user);
        let site = find_ident(&ast, &es, "__SITE__");
        assert_ne!(ast.exprs[site].span, call_span);
        assert_eq!(ast.exprs[site].span.file, Ustr::from("user.rock"));
        let g = find_ident(&ast, &es, "g");
        assert_ne!(ast.exprs[g].span, call_span);

        let seven = es
            .iter()
            .find(|&&e| matches!(ast.exprs[e].kind, ExprKind::Int(7, _)))
            .unwrap();
        assert_eq!(ast.exprs[*seven].span, call_span);
        let one = es
            .iter()
            .find(|&&e| matches!(ast.exprs[e].kind, ExprKind::Int(1, _)))
            .unwrap();
        assert_ne!(ast.exprs[*one].span, call_span);

        for &e in &es {
            assert_ne!(ast.exprs[e].span.file, Ustr::from("defs.rock"));
        }
        let braced: Vec<_> = bs
            .iter()
            .filter(|&&b| ast.blocks[b].braces.is_some())
            .collect();
        // The user fn body and a `{ 1 }` per branch; none of the template's.
        assert_eq!(braced.len(), 3, "{braced:?}");
        for &b in &bs {
            assert_ne!(ast.blocks[b].span.file, Ustr::from("defs.rock"));
            for &s in &ast.blocks[b].statements {
                assert_ne!(ast.stmts[s].span.file, Ustr::from("defs.rock"));
            }
        }
    }

    /// A default arg is copied from the template after the body is renamed and
    /// stamped: it keeps the template's span and its names resolve at the call
    /// site, even one a body local shares.
    #[test]
    fn default_args_keep_template_spans_and_caller_names() {
        let defs = "macro m(a: expr, b: expr = x + 5) -> int {\n    let x = 1\n    a + b + x\n}";
        let user = "fn f(x: int) -> int {\n    m(2)\n}";
        let (ast, es, _) = expanded_nodes(defs, user);
        let five = es
            .iter()
            .find(|&&e| matches!(ast.exprs[e].kind, ExprKind::Int(5, _)))
            .unwrap();
        assert_eq!(ast.exprs[*five].span.file, Ustr::from("defs.rock"));
        let x = find_ident(&ast, &es, "x");
        assert_eq!(ast.exprs[x].span.file, Ustr::from("defs.rock"));
        let renamed = es
            .iter()
            .filter(|&&e| matches!(&ast.exprs[e].kind, ExprKind::Ident(id) if is_hygiene_name(&id.name)))
            .count();
        assert_eq!(renamed, 1, "only the body's `x` reference is renamed");
    }

    #[test]
    fn expand_one_is_idempotent() {
        let user = "fn f() -> int { twice(21) }";
        let unexpanded = vec![
            parse("macro twice(x: expr) { x + x }", "defs.rock"),
            parse(user, "user.rock"),
        ];
        let reg = collect_registry(&unexpanded);

        let mut m = parse(user, "user.rock");
        expand_one(&mut m, &reg);
        let once = m.clone();
        expand_one(&mut m, &reg);
        assert_same_ast(&m, &once);
    }
}
