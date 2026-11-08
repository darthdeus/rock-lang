//! Macro expansion on skull-parser's arena AST.
//!
//! Runs as the FIRST cook stage, before desugar (bone's order), so the
//! desugars see fully-expanded code.
//!
//! Scope: expression-position macros (`macro f(a: expr, ...) { <rock body> }`),
//! selected by arity (defaults widen the range), params substituted by deep
//! copy, the call replaced by a `Block` of the substituted body. Macro bodies
//! live in their defining module's arena, so inlining a body into a caller is a
//! cross-arena graft (`graft.rs`). Macro defs are dropped after expansion.
//!
//! Not implemented: variadic `...rest` splat, body-local hygiene rename, span retag for
//! `__SITE__` (the literal rides along as an `Ident`; retag only affects the
//! later site-resolution pass, not the cooked s-expr), `ty`/`ident` fragment
//! substitution, and top-level item-macro splicing.

use crate::ast::*;
use crate::clone::clone_expr;
use crate::desugar::{BlockEdges, ExprEdges, child_blocks, child_exprs};
use crate::graft::{graft_block, graft_expr, graft_item, graft_type};
use rock_core::{CompilerError, Span};
use smallvec::smallvec;
use std::collections::{HashMap, HashSet};
use ustr::Ustr;

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

type Registry = HashMap<Ustr, Vec<MacroTemplate>>;

/// Opaque, reusable macro registry: every macro overload collected from a whole
/// workspace, keyed by name. Built once from the UN-expanded modules (expansion
/// drops the defs, so it can't be rebuilt from already-expanded ASTs), then
/// handed to `expand_one` / `cook_one` to expand a single module against it --
/// the building block for re-cooking just one edited file incrementally. Holds
/// its own grafted arenas, so it outlives the source modules.
#[derive(Clone, Debug, Default)]
pub struct MacroRegistry(Registry);

impl MacroRegistry {
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
pub fn collect_registry(modules: &[Ast]) -> MacroRegistry {
    MacroRegistry(build_registry(modules))
}

/// A macro name must be UNIQUE against top-level FUNCTION names across the whole
/// module set: `foo(...)` in call position is ambiguous when both a `macro foo`
/// and a `fn foo` exist (expansion would silently claim the call and shadow the
/// fn), so it is rejected -- a macro always has exactly one meaning. Macro
/// OVERLOADS (same name, different arity) stay legal; a same-named LOCAL binding
/// (param, nested fn, `let`) legally shadows a macro and is not a top-level fn,
/// so it never collides. Cross-module: all parsed modules share one namespace,
/// so this runs over the full parsed set before expansion erases the macro defs.
/// Run on the pre-expansion modules (expansion drops the `is_macro` items).
pub fn check_macro_name_uniqueness(modules: &[Ast]) -> Vec<CompilerError> {
    let mut fn_names: HashSet<Ustr> = HashSet::new();
    for m in modules {
        for item in &m.items {
            if let ItemKind::Function(f) = &item.kind {
                if !f.is_macro {
                    fn_names.insert(f.name.name);
                }
            }
        }
    }
    let mut errs = Vec::new();
    let mut reported: HashSet<Ustr> = HashSet::new();
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
pub fn check_macro_call_labels(modules: &[Ast]) -> Vec<CompilerError> {
    let reg = build_registry(modules);
    modules
        .iter()
        .flat_map(|m| call_label_errors(m, &reg))
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
        // Labels can only appear inside the parens, so a call with none
        // binds positionally and cannot fail this check.
        if args.iter().all(|a| a.name.is_none()) {
            continue;
        }
        let (name, extra) = match &m.exprs[*func].kind {
            ExprKind::Ident(id) => (id.name, 0),
            ExprKind::FieldAccess(_, member) => (member.name, 1),
            _ => continue,
        };
        let Some(cands) = reg.get(&name) else {
            continue;
        };
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
pub fn expand_one(ast: &mut Ast, registry: &MacroRegistry) {
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
    let registry = collect_registry(modules);
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

fn build_registry(modules: &[Ast]) -> Registry {
    let mut reg: Registry = HashMap::new();
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
/// matching item-macro's spliced + substituted items. Non-item-macro
/// invocations are dropped (they're resolved as expr macros in bodies, not
/// here). Mirrors bone's `expand_top_level_macros`.
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
        match expand_item_macro(ast, name, &args, reg) {
            Some(items) => out.extend(items),
            None => {} // not an item-macro at this arity — drop the invocation
        }
    }
    ast.items = out;
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
    // Index of the matching overload (one with item bodies, satisfiable arity).
    let cands = reg.get(&name.name)?;
    let idx = cands.iter().position(|d| {
        let total = d.params.len();
        let required = d.params.iter().filter(|p| p.default.is_none()).count();
        !d.items.is_empty() && required <= argc && argc <= total
    })?;

    // Build substitution maps from the param fragment kinds. ty/ident args
    // recover a type via `expr_to_type_expr`; ident also substitutes into expr
    // positions (ctor names / refs). expr args substitute whole subtrees.
    let mut ty_subs: HashMap<Ustr, Idx<TypeExprData>> = HashMap::new();
    let mut expr_subs: HashMap<Ustr, Idx<ExprData>> = HashMap::new();
    // Same by-label-then-by-position binding as the expr path; item macros take
    // no variadic, so only the slots matter.
    let bound = bind_macro_args(&cands[idx].params, args).ok()?;
    // Snapshot the per-param (fragment, name) + the arg/default source so the
    // graft (needs &tmpl + &mut ast) doesn't overlap the &reg borrow.
    let plan: Vec<(Ustr, Option<Frag>, Option<Idx<ExprData>>)> = cands[idx]
        .params
        .iter()
        .enumerate()
        .map(|(i, p)| (p.name, p.fragment, bound.slots[i]))
        .collect();
    for (i, (pname, frag, arg_slot)) in plan.iter().enumerate() {
        let arg = if let Some(a) = *arg_slot {
            a
        } else {
            // Fill from the template default (grafted into the caller arena).
            let d = cands[idx].params[i].default?;
            graft_expr(&cands[idx].tmpl, ast, d)
        };
        match frag {
            Some(Frag::Ident) | Some(Frag::Ty) => {
                let te = expr_to_type_expr(ast, arg)?;
                ty_subs.insert(*pname, te);
                if *frag == Some(Frag::Ident) {
                    expr_subs.insert(*pname, arg);
                }
            }
            _ => {
                expr_subs.insert(*pname, arg);
            }
        }
    }

    // Graft the template items into the caller, then substitute.
    let mut items: Vec<Item> = (0..cands[idx].items.len())
        .map(|k| graft_item(&cands[idx].tmpl, ast, &cands[idx].items[k]))
        .collect();
    for it in &mut items {
        subst_in_item(ast, it, &ty_subs, &expr_subs);
    }
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
type ShadowScope = std::collections::HashSet<Ustr>;

fn is_shadowed(stack: &[ShadowScope], name: Ustr) -> bool {
    stack.iter().any(|s| s.contains(&name))
}

/// Collect the names a `let`/`for` target binds (shadow-side twin of
/// `rename_binding`).
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

/// Collect the names a match/if-let/while-let pattern binds (shadow-side twin
/// of `rename_pattern`).
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
    let mut scope = ShadowScope::new();
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
            let mut scope = ShadowScope::new();
            shadow_binding(&var, &mut scope);
            stack.push(scope);
            expand_block(ast, body, reg, depth, stack);
            stack.pop();
        }
        StatementKind::WhileLet(w) => {
            let (value, body, pattern) = (w.value, w.body, w.pattern.clone());
            expand_expr(ast, value, reg, depth, stack);
            let mut scope = ShadowScope::new();
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
/// Binding-introducing exprs mirror `rename_expr`: the pattern/params scope only
/// the body, with the scrutinee/value walked in the outer scope.
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
        ExprKind::IfLet { .. } => {
            let (value, then_b, else_b, mut scope) = match &ast.exprs[e].kind {
                ExprKind::IfLet {
                    value,
                    then_block,
                    else_block,
                    pattern,
                    ..
                } => {
                    let mut scope = ShadowScope::new();
                    shadow_pattern(pattern, &mut scope);
                    (*value, *then_block, *else_block, scope)
                }
                _ => unreachable!(),
            };
            expand_expr(ast, value, reg, depth, stack);
            stack.push(std::mem::take(&mut scope));
            expand_block(ast, then_b, reg, depth, stack);
            stack.pop();
            if let Some(eb) = else_b {
                expand_block(ast, eb, reg, depth, stack);
            }
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
            stack.push(ShadowScope::new());
            for i in 0..n_clauses {
                let (value, pat_scope) = match &ast.exprs[e].kind {
                    ExprKind::IfLetChain { clauses, .. } => match &clauses[i] {
                        IfLetClause::Binding { value, pattern } => {
                            let mut scope = ShadowScope::new();
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
                        let mut scope = ShadowScope::new();
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

    // Graft the template body into the caller arena.
    let body = graft_block(&tmpl.tmpl, ast, tmpl.body);

    // Hygiene: rename body-local bindings to fresh names BEFORE param
    // substitution, so a body-local can't capture a substituted arg that shares
    // its surface name (rock's `rename_body_locals_for_macro`). The call-site
    // arena index is the per-expansion id; the `__inline_local_*` names collapse
    // back to the original under the cook oracle's hygiene normalization.
    {
        let callsite = e.into_raw().into_u32();
        let mut counter = 0u32;
        let mut stack: Vec<RenameScope> = Vec::new();
        rename_block(ast, body, callsite, &mut counter, &mut stack);
    }

    // Span retag: stamp the macro invocation's span on every expanded body
    // node BEFORE substitution, so `__SITE__` resolves to the caller's source
    // position (not the macro body's) and body-originated type errors report at
    // the call site. Args, substituted in below, keep their own caller spans.
    // Nested macros propagate the outermost caller's span (the inner call was
    // itself retagged before it expands).
    retag_block(ast, body, ast.exprs[e].span);

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
    let mut subs: HashMap<Ustr, Idx<ExprData>> = HashMap::new();
    let mut variadic: HashMap<Ustr, Vec<Idx<ExprData>>> = HashMap::new();
    for (i, p) in tmpl.params.iter().enumerate() {
        if p.is_variadic {
            let rest = bound.variadic.as_ref().filter(|(vi, _)| *vi == i);
            variadic.insert(p.name, rest.map(|(_, v)| v.clone()).unwrap_or_default());
        } else if let Some(a) = bound.slots[i] {
            subs.insert(p.name, a);
        } else if let Some(d) = p.default {
            let g = graft_expr(&tmpl.tmpl, ast, d);
            subs.insert(p.name, g);
        }
    }

    // Splat BEFORE substitution: `...rest` forwards expand to the captured args
    // (e.g. `sum_of(...rest)` -> `sum_of(2, 3, 4)`), which the recursive expand
    // below then re-dispatches on the reduced arity until the base case.
    if !variadic.is_empty() {
        splat_expand_block(ast, body, &variadic);
    }
    substitute_block(ast, body, &subs);
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

/// Rewrite a binder name if it matches an `ident`-fragment whose replacement is
/// itself an `Ident` type (struct/fn/field/enum names).
fn replace_ident_name(ast: &Ast, id: &mut Ident, ty_subs: &HashMap<Ustr, Idx<TypeExprData>>) {
    if let Some(&t) = ty_subs.get(&id.name) {
        if let TypeExprKind::Ident(new) = &ast.types[t].kind {
            id.name = new.name;
            id.span = new.span;
        }
    }
}

/// Substitute `ty`/`ident` fragments into a type subtree in place.
fn subst_in_type(ast: &mut Ast, t: Idx<TypeExprData>, ty_subs: &HashMap<Ustr, Idx<TypeExprData>>) {
    if ty_subs.is_empty() {
        return;
    }
    match ast.types[t].kind.clone() {
        TypeExprKind::Ident(id) => {
            if let Some(&repl) = ty_subs.get(&id.name) {
                let was_nullable = ast.types[t].is_nullable;
                let repl_data = ast.types[repl].clone();
                ast.types[t].kind = repl_data.kind;
                ast.types[t].is_nullable = was_nullable || repl_data.is_nullable;
            }
        }
        TypeExprKind::App { mut ctor, args } => {
            if let Some(&repl) = ty_subs.get(&ctor.name) {
                if let TypeExprKind::Ident(new) = &ast.types[repl].kind {
                    ctor = new.clone();
                    ast.types[t].kind = TypeExprKind::App {
                        ctor,
                        args: args.clone(),
                    };
                }
            }
            for a in args {
                subst_in_type(ast, a, ty_subs);
            }
        }
        TypeExprKind::Array(inner) => subst_in_type(ast, inner, ty_subs),
        TypeExprKind::Ref(inner) => subst_in_type(ast, inner, ty_subs),
        TypeExprKind::Tuple(items) => {
            for it in items {
                subst_in_type(ast, it, ty_subs);
            }
        }
        TypeExprKind::Function { params, ret } => {
            for p in params {
                subst_in_type(ast, p, ty_subs);
            }
            subst_in_type(ast, ret, ty_subs);
        }
        _ => {}
    }
}

/// Substitute fragments into a spliced item: `ident` into names, `ty` into type
/// positions, `expr` into the body. Covers fn / struct / newtype / enum.
fn subst_in_item(
    ast: &mut Ast,
    item: &mut Item,
    ty_subs: &HashMap<Ustr, Idx<TypeExprData>>,
    expr_subs: &HashMap<Ustr, Idx<ExprData>>,
) {
    match &mut item.kind {
        ItemKind::Function(f) => {
            replace_ident_name(ast, &mut f.name, ty_subs);
            let params = std::mem::take(&mut f.params);
            let mut params = params;
            for p in &mut params {
                replace_ident_name(ast, &mut p.name, ty_subs);
                subst_in_type(ast, p.ty, ty_subs);
                if let Some(d) = p.default {
                    substitute_expr(ast, d, expr_subs);
                    subst_in_type_exprs(ast, d, ty_subs);
                }
            }
            if let ItemKind::Function(f) = &mut item.kind {
                f.params = params;
                if let Some(rt) = f.return_ty {
                    subst_in_type(ast, rt, ty_subs);
                }
                if let Some(body) = f.body.rock_block() {
                    substitute_block(ast, body, expr_subs);
                    subst_in_type_exprs_block(ast, body, ty_subs);
                }
            }
        }
        ItemKind::Struct(s) => {
            replace_ident_name(ast, &mut s.name, ty_subs);
            let mut fields = std::mem::take(&mut s.fields);
            for fl in &mut fields {
                replace_ident_name(ast, &mut fl.name, ty_subs);
                subst_in_type(ast, fl.ty, ty_subs);
                if let Some(d) = fl.default {
                    substitute_expr(ast, d, expr_subs);
                    subst_in_type_exprs(ast, d, ty_subs);
                }
            }
            if let ItemKind::Struct(s) = &mut item.kind {
                s.fields = fields;
            }
        }
        ItemKind::Newtype(n) => {
            replace_ident_name(ast, &mut n.name, ty_subs);
            subst_in_type(ast, n.wrapped, ty_subs);
        }
        ItemKind::Enum(e) => {
            replace_ident_name(ast, &mut e.name, ty_subs);
            let mut variants = std::mem::take(&mut e.variants);
            for v in &mut variants {
                match &mut v.kind {
                    EnumVariantKind::Unit => {}
                    EnumVariantKind::Tuple(tys) => {
                        for &t in tys.iter() {
                            subst_in_type(ast, t, ty_subs);
                        }
                    }
                    EnumVariantKind::Record(fs) => {
                        for fl in fs.iter_mut() {
                            replace_ident_name(ast, &mut fl.name, ty_subs);
                            subst_in_type(ast, fl.ty, ty_subs);
                        }
                    }
                }
            }
            if let ItemKind::Enum(e) = &mut item.kind {
                e.variants = variants;
            }
        }
        _ => {}
    }
}

/// Walk a block and substitute `ty` fragments into embedded type annotations
/// (`as`, ascription, lambda params).
fn subst_in_type_exprs_block(
    ast: &mut Ast,
    b: Idx<BlockData>,
    ty_subs: &HashMap<Ustr, Idx<TypeExprData>>,
) {
    if ty_subs.is_empty() {
        return;
    }
    let stmts = ast.blocks[b].statements.clone();
    for s in stmts {
        if let StatementKind::Let(l) = &ast.stmts[s].kind {
            if let Some(t) = l.ty {
                subst_in_type(ast, t, ty_subs);
            }
        }
        if let StatementKind::Static(st) = &ast.stmts[s].kind {
            if let Some(t) = st.ty {
                subst_in_type(ast, t, ty_subs);
            }
        }
        let nested_fn_tys: Vec<Idx<TypeExprData>> =
            if let StatementKind::NestedFn(f) = &ast.stmts[s].kind {
                f.params.iter().map(|p| p.ty).chain(f.return_ty).collect()
            } else {
                Vec::new()
            };
        for t in nested_fn_tys {
            subst_in_type(ast, t, ty_subs);
        }
        for e in stmt_exprs(ast, s) {
            subst_in_type_exprs(ast, e, ty_subs);
        }
        for bl in stmt_blocks(ast, s) {
            subst_in_type_exprs_block(ast, bl, ty_subs);
        }
    }
}

fn subst_in_type_exprs(
    ast: &mut Ast,
    e: Idx<ExprData>,
    ty_subs: &HashMap<Ustr, Idx<TypeExprData>>,
) {
    match ast.exprs[e].kind.clone() {
        ExprKind::As(inner, t)
        | ExprKind::Bitcast(inner, t)
        | ExprKind::TypeAscription(inner, t) => {
            subst_in_type(ast, t, ty_subs);
            subst_in_type_exprs(ast, inner, ty_subs);
        }
        ExprKind::Lambda { params, body, .. } => {
            for p in params {
                if let Some(t) = p.ty {
                    subst_in_type(ast, t, ty_subs);
                }
            }
            subst_in_type_exprs_block(ast, body, ty_subs);
        }
        _ => {
            for c in child_exprs(ast, e) {
                subst_in_type_exprs(ast, c, ty_subs);
            }
            for bl in child_blocks(ast, e) {
                subst_in_type_exprs_block(ast, bl, ty_subs);
            }
        }
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

/// Replace every `...rest` call argument (a splat of a captured variadic param)
/// with the captured args, in place, across the body.
fn splat_expand_block(
    ast: &mut Ast,
    b: Idx<BlockData>,
    variadic: &HashMap<Ustr, Vec<Idx<ExprData>>>,
) {
    for call in collect_calls(ast, b) {
        let ExprKind::FunctionCall { args, .. } = &ast.exprs[call].kind else {
            continue;
        };
        if !args.iter().any(|a| a.is_splat) {
            continue;
        }
        let old = args.clone();
        let mut new_args: Vec<FunctionCallArg> = Vec::with_capacity(old.len());
        for a in old {
            let captured = if a.is_splat {
                match &ast.exprs[a.value].kind {
                    ExprKind::Ident(id) => variadic.get(&id.name).cloned(),
                    _ => None,
                }
            } else {
                None
            };
            match captured {
                Some(items) => {
                    for c in items {
                        let v = clone_expr(ast, c);
                        new_args.push(FunctionCallArg {
                            name: None,
                            value: v,
                            is_splat: false,
                            is_spread: false,
                        });
                    }
                }
                None => new_args.push(a),
            }
        }
        if let ExprKind::FunctionCall { args, .. } = &mut ast.exprs[call].kind {
            *args = new_args;
        }
    }
}

/// Every `FunctionCall` expr reachable from a block (for splat expansion).
fn collect_calls(ast: &Ast, b: Idx<BlockData>) -> Vec<Idx<ExprData>> {
    let mut out = Vec::new();
    let stmts = ast.blocks[b].statements.clone();
    for s in stmts {
        for e in stmt_exprs(ast, s) {
            collect_calls_expr(ast, e, &mut out);
        }
        for bl in stmt_blocks(ast, s) {
            out.extend(collect_calls(ast, bl));
        }
    }
    out
}

fn collect_calls_expr(ast: &Ast, e: Idx<ExprData>, out: &mut Vec<Idx<ExprData>>) {
    if matches!(ast.exprs[e].kind, ExprKind::FunctionCall { .. }) {
        out.push(e);
    }
    for c in child_exprs(ast, e) {
        collect_calls_expr(ast, c, out);
    }
    for bl in child_blocks(ast, e) {
        out.extend(collect_calls(ast, bl));
    }
}

// ---------------------------------------------------------------------------
// Param substitution: replace each `Ident(param)` use with a fresh deep copy of
// the bound argument. A lambda param that shadows a macro param stops the
// substitution under that lambda (bone parity).
// ---------------------------------------------------------------------------

fn substitute_block(ast: &mut Ast, b: Idx<BlockData>, subs: &HashMap<Ustr, Idx<ExprData>>) {
    let stmts = ast.blocks[b].statements.clone();
    for s in stmts {
        for e in stmt_exprs(ast, s) {
            substitute_expr(ast, e, subs);
        }
        for bl in stmt_blocks(ast, s) {
            substitute_block(ast, bl, subs);
        }
    }
}

fn substitute_expr(ast: &mut Ast, e: Idx<ExprData>, subs: &HashMap<Ustr, Idx<ExprData>>) {
    // Replace this node if it is a bare param ident.
    if let ExprKind::Ident(id) = &ast.exprs[e].kind {
        if let Some(&arg) = subs.get(&id.name) {
            let copy = clone_expr(ast, arg);
            ast.exprs[e] = ast.exprs[copy].clone();
            return;
        }
    }
    // A lambda shadows any macro param with the same name as one of its params.
    if let ExprKind::Lambda { params, body, .. } = &ast.exprs[e].kind {
        let shadowed: Vec<Ustr> = params
            .iter()
            .map(|p| p.name.name)
            .filter(|n| subs.contains_key(n))
            .collect();
        let body = *body;
        if shadowed.is_empty() {
            substitute_block(ast, body, subs);
        } else {
            let mut inner = subs.clone();
            for n in shadowed {
                inner.remove(&n);
            }
            substitute_block(ast, body, &inner);
        }
        return;
    }
    // An `ident` fragment in constructor position: `Name { .. }` / `Name::V`.
    // The bound arg is an `Ident` expr; lift its name into the ctor.
    let ctor_name = match &ast.exprs[e].kind {
        ExprKind::StructLiteral { name, .. } => Some(name.name),
        ExprKind::EnumVariantRecord { enum_name, .. } => Some(enum_name.name),
        ExprKind::QualifiedIdent { base, .. } => Some(base.name),
        _ => None,
    };
    if let Some(cn) = ctor_name {
        if let Some(&arg) = subs.get(&cn) {
            if let ExprKind::Ident(id) = ast.exprs[arg].kind.clone() {
                match &mut ast.exprs[e].kind {
                    ExprKind::StructLiteral { name, .. } => *name = id,
                    ExprKind::EnumVariantRecord { enum_name, .. } => *enum_name = id,
                    ExprKind::QualifiedIdent { base, .. } => *base = id,
                    _ => {}
                }
            }
        }
    }
    for child in child_exprs(ast, e) {
        substitute_expr(ast, child, subs);
    }
    for blk in child_blocks(ast, e) {
        substitute_block(ast, blk, subs);
    }
}

// ---------------------------------------------------------------------------
// Body-local hygiene rename (rock's `rename_body_locals_for_macro`)
//
// Before param substitution, every binding the macro body introduces is renamed
// to a fresh `__inline_local_<callsite>_<n>_<orig>` so a body-local can never
// capture a substituted `expr` arg that shares its surface name. Scope-aware:
// inner shadows freshen independently; references resolve to the nearest
// enclosing rename. `^name` (AnaphoricIdent) escapes hygiene — it is an explicit
// caller-scope reference and must keep its surface name.
// ---------------------------------------------------------------------------

type RenameScope = HashMap<Ustr, Ustr>;

fn fresh_local(orig: Ustr, callsite: u32, counter: &mut u32) -> Ustr {
    let n = *counter;
    *counter += 1;
    Ustr::from(&format!("__inline_local_{}_{}_{}", callsite, n, orig))
}

fn lookup_rename(stack: &[RenameScope], name: Ustr) -> Option<Ustr> {
    stack.iter().rev().find_map(|s| s.get(&name).copied())
}

/// Rename the bindings introduced by a `let`/`for` target into `scope`.
fn rename_binding(b: &mut Binding, callsite: u32, counter: &mut u32, scope: &mut RenameScope) {
    match b {
        Binding::Ident(id) => {
            let fresh = fresh_local(id.name, callsite, counter);
            scope.insert(id.name, fresh);
            id.name = fresh;
        }
        Binding::Tuple(bs) => {
            for b in bs.iter_mut() {
                rename_binding(b, callsite, counter, scope);
            }
        }
        Binding::Newtype { binding, .. } => {
            let fresh = fresh_local(binding.name, callsite, counter);
            scope.insert(binding.name, fresh);
            binding.name = fresh;
        }
    }
}

/// Rename the bindings introduced by a match/if-let/while-let pattern into
/// `scope`. Literal/range sub-exprs bind nothing (rock parity — they are not
/// walked here).
fn rename_pattern(p: &mut Pattern, callsite: u32, counter: &mut u32, scope: &mut RenameScope) {
    match p {
        Pattern::Wildcard | Pattern::Literal(_) | Pattern::Range { .. } => {}
        Pattern::Ident(id) => {
            let fresh = fresh_local(id.name, callsite, counter);
            scope.insert(id.name, fresh);
            id.name = fresh;
        }
        Pattern::Tuple(ps) | Pattern::Or(ps) => {
            for p in ps.iter_mut() {
                rename_pattern(p, callsite, counter, scope);
            }
        }
        Pattern::EnumVariant { args, .. } => {
            if let Some(ps) = args {
                for p in ps.iter_mut() {
                    rename_pattern(p, callsite, counter, scope);
                }
            }
        }
        Pattern::Record { fields, .. } => {
            for (_, sub) in fields.iter_mut() {
                rename_pattern(sub, callsite, counter, scope);
            }
        }
        Pattern::PathVariant { args, .. } => {
            if let Some(ps) = args {
                for p in ps.iter_mut() {
                    rename_pattern(p, callsite, counter, scope);
                }
            }
        }
        Pattern::PathRecord { fields, .. } => {
            for (_, sub) in fields.iter_mut() {
                rename_pattern(sub, callsite, counter, scope);
            }
        }
    }
}

fn rename_block(
    ast: &mut Ast,
    b: Idx<BlockData>,
    callsite: u32,
    counter: &mut u32,
    stack: &mut Vec<RenameScope>,
) {
    // Each block is its own scope: its `let` bindings unbind at block exit.
    stack.push(RenameScope::new());
    let stmts = ast.blocks[b].statements.clone();
    for s in stmts {
        rename_stmt(ast, s, callsite, counter, stack);
    }
    stack.pop();
}

fn rename_stmt(
    ast: &mut Ast,
    s: Idx<StatementData>,
    callsite: u32,
    counter: &mut u32,
    stack: &mut Vec<RenameScope>,
) {
    match &ast.stmts[s].kind {
        StatementKind::Let(l) => {
            // RHS sees the outer scope; the binding freshens for the rest of the
            // current block (the top scope frame).
            let value = l.value;
            if let Some(v) = value {
                rename_expr(ast, v, callsite, counter, stack);
            }
            let top = stack.last_mut().expect("rename_stmt: block scope present");
            if let StatementKind::Let(l) = &mut ast.stmts[s].kind {
                rename_binding(&mut l.binding, callsite, counter, top);
            }
        }
        StatementKind::For(f) => {
            let iter = f.iter;
            let body = f.body;
            rename_expr(ast, iter, callsite, counter, stack);
            let mut scope = RenameScope::new();
            if let StatementKind::For(f) = &mut ast.stmts[s].kind {
                rename_binding(&mut f.var, callsite, counter, &mut scope);
            }
            stack.push(scope);
            rename_block(ast, body, callsite, counter, stack);
            stack.pop();
        }
        StatementKind::WhileLet(w) => {
            let value = w.value;
            let body = w.body;
            rename_expr(ast, value, callsite, counter, stack);
            let mut scope = RenameScope::new();
            if let StatementKind::WhileLet(w) = &mut ast.stmts[s].kind {
                rename_pattern(&mut w.pattern, callsite, counter, &mut scope);
            }
            stack.push(scope);
            rename_block(ast, body, callsite, counter, stack);
            stack.pop();
        }
        _ => {
            for e in stmt_exprs(ast, s) {
                rename_expr(ast, e, callsite, counter, stack);
            }
            for bl in stmt_blocks(ast, s) {
                rename_block(ast, bl, callsite, counter, stack);
            }
        }
    }
}

fn rename_expr(
    ast: &mut Ast,
    e: Idx<ExprData>,
    callsite: u32,
    counter: &mut u32,
    stack: &mut Vec<RenameScope>,
) {
    // Leaves: rename an in-scope reference; `^name` escapes hygiene untouched.
    match &mut ast.exprs[e].kind {
        ExprKind::Ident(id) => {
            if let Some(fresh) = lookup_rename(stack, id.name) {
                id.name = fresh;
            }
            return;
        }
        ExprKind::AnaphoricIdent(_) => return,
        _ => {}
    }

    // Binding-introducing exprs: the pattern/binding scopes only the body, with
    // the scrutinee/value evaluated in the outer scope. Handled explicitly so
    // the generic child walk below never re-visits them.
    match &ast.exprs[e].kind {
        ExprKind::IfLet { .. } => {
            let (value, then_b, else_b) = match &ast.exprs[e].kind {
                ExprKind::IfLet {
                    value,
                    then_block,
                    else_block,
                    ..
                } => (*value, *then_block, *else_block),
                _ => unreachable!(),
            };
            rename_expr(ast, value, callsite, counter, stack);
            let mut scope = RenameScope::new();
            if let ExprKind::IfLet { pattern, .. } = &mut ast.exprs[e].kind {
                rename_pattern(pattern, callsite, counter, &mut scope);
            }
            stack.push(scope);
            rename_block(ast, then_b, callsite, counter, stack);
            stack.pop();
            if let Some(eb) = else_b {
                rename_block(ast, eb, callsite, counter, stack);
            }
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
            // earlier clauses' renames.
            stack.push(RenameScope::new());
            for i in 0..n_clauses {
                let value = match &ast.exprs[e].kind {
                    ExprKind::IfLetChain { clauses, .. } => match &clauses[i] {
                        IfLetClause::Binding { value, .. } => Some(*value),
                        IfLetClause::Condition(c) => Some(*c),
                    },
                    _ => unreachable!(),
                };
                if let Some(v) = value {
                    rename_expr(ast, v, callsite, counter, stack);
                }
                let top = stack.last_mut().expect("rename_stmt: chain scope present");
                if let ExprKind::IfLetChain { clauses, .. } = &mut ast.exprs[e].kind {
                    if let IfLetClause::Binding { pattern, .. } = &mut clauses[i] {
                        rename_pattern(pattern, callsite, counter, top);
                    }
                }
            }
            rename_block(ast, then_b, callsite, counter, stack);
            stack.pop();
            if let Some(eb) = else_b {
                rename_block(ast, eb, callsite, counter, stack);
            }
            return;
        }
        ExprKind::Match { .. } => {
            let (scrutinee, n_arms) = match &ast.exprs[e].kind {
                ExprKind::Match { scrutinee, arms } => (*scrutinee, arms.len()),
                _ => unreachable!(),
            };
            rename_expr(ast, scrutinee, callsite, counter, stack);
            for i in 0..n_arms {
                let (guard, body) = match &ast.exprs[e].kind {
                    ExprKind::Match { arms, .. } => (arms[i].guard, arms[i].body),
                    _ => unreachable!(),
                };
                let mut scope = RenameScope::new();
                if let ExprKind::Match { arms, .. } = &mut ast.exprs[e].kind {
                    rename_pattern(&mut arms[i].pattern, callsite, counter, &mut scope);
                }
                stack.push(scope);
                if let Some(g) = guard {
                    rename_expr(ast, g, callsite, counter, stack);
                }
                rename_block(ast, body, callsite, counter, stack);
                stack.pop();
            }
            return;
        }
        ExprKind::SafeNavigationLet { .. } => {
            let (receiver, body) = match &ast.exprs[e].kind {
                ExprKind::SafeNavigationLet { receiver, body, .. } => (*receiver, *body),
                _ => unreachable!(),
            };
            rename_expr(ast, receiver, callsite, counter, stack);
            let mut scope = RenameScope::new();
            if let ExprKind::SafeNavigationLet { binding, .. } = &mut ast.exprs[e].kind {
                let fresh = fresh_local(binding.name, callsite, counter);
                scope.insert(binding.name, fresh);
                binding.name = fresh;
            }
            stack.push(scope);
            rename_block(ast, body, callsite, counter, stack);
            stack.pop();
            return;
        }
        _ => {}
    }

    // Plain exprs: recurse over direct children. Child blocks (If branches,
    // `Block`, lambda body) each open their own scope via `rename_block`.
    for child in child_exprs(ast, e) {
        rename_expr(ast, child, callsite, counter, stack);
    }
    for blk in child_blocks(ast, e) {
        rename_block(ast, blk, callsite, counter, stack);
    }
}

// ---------------------------------------------------------------------------
// Span retag for macro expansion (rock's `retag_block_spans_for_macro`)
// ---------------------------------------------------------------------------

fn retag_block(ast: &mut Ast, b: Idx<BlockData>, call_span: Span) {
    ast.blocks[b].span = call_span;
    // The braces of the macro TEMPLATE's blocks don't exist at the call site;
    // a retagged block is effectively synthetic, so it gets no brace spans
    // (real spans here would plant brace markers at template offsets).
    ast.blocks[b].braces = None;
    let stmts = ast.blocks[b].statements.clone();
    for s in stmts {
        ast.stmts[s].span = call_span;
        for e in stmt_exprs(ast, s) {
            retag_expr(ast, e, call_span);
        }
        for bl in stmt_blocks(ast, s) {
            retag_block(ast, bl, call_span);
        }
    }
}

fn retag_expr(ast: &mut Ast, e: Idx<ExprData>, call_span: Span) {
    ast.exprs[e].span = call_span;
    for c in child_exprs(ast, e) {
        retag_expr(ast, c, call_span);
    }
    for bl in child_blocks(ast, e) {
        retag_block(ast, bl, call_span);
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

// ---------------------------------------------------------------------------
// Macro-parameter reference index (read-side twin of subst_in_item)
// ---------------------------------------------------------------------------
//
// Substitution REWRITES param-name occurrences in a grafted clone at
// invocation (and `retag_block` stamps the call-site span first); this COLLECTS
// them in the *definition*, with their source spans, at parse time -- the data
// IDE references/rename for a macro parameter needs. The two walks visit the
// same node shapes but one is `&mut Ast` (rewrite) and one is `&Ast` (read), so
// the borrow checker keeps them separate, exactly like `clone.rs`/`graft.rs`.
// Keep them in sync.

/// Per parameter, its declaration span + every use span in the macro template.
pub fn index_macro_param_refs(
    ast: &Ast,
    params: &[Param],
    body: Option<Idx<BlockData>>,
    macro_items: &[Item],
) -> Vec<MacroParamRef> {
    let names: HashSet<Ustr> = params.iter().map(|p| p.name.name).collect();
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

fn note(id: &Ident, names: &HashSet<Ustr>, occ: &mut Vec<(Ustr, Span)>) {
    if names.contains(&id.name) {
        occ.push((id.name, id.span));
    }
}

fn refs_in_item(ast: &Ast, item: &Item, names: &HashSet<Ustr>, occ: &mut Vec<(Ustr, Span)>) {
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

fn refs_in_type(
    ast: &Ast,
    t: Idx<TypeExprData>,
    names: &HashSet<Ustr>,
    occ: &mut Vec<(Ustr, Span)>,
) {
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

fn refs_in_block(ast: &Ast, b: Idx<BlockData>, names: &HashSet<Ustr>, occ: &mut Vec<(Ustr, Span)>) {
    for &st in &ast.blocks[b].statements {
        match &ast.stmts[st].kind {
            StatementKind::Let(l) => {
                if let Some(t) = l.ty {
                    refs_in_type(ast, t, names, occ);
                }
                if let Some(v) = l.value {
                    refs_in_expr(ast, v, names, occ);
                }
            }
            StatementKind::Expression(e) => refs_in_expr(ast, *e, names, occ),
            StatementKind::Return { value: Some(e), .. } => refs_in_expr(ast, *e, names, occ),
            _ => {}
        }
    }
}

fn refs_in_expr(ast: &Ast, e: Idx<ExprData>, names: &HashSet<Ustr>, occ: &mut Vec<(Ustr, Span)>) {
    match &ast.exprs[e].kind {
        ExprKind::Ident(id) => note(id, names, occ),
        ExprKind::FieldAccess(base, _) => refs_in_expr(ast, *base, names, occ),
        ExprKind::BinOp { left, right, .. } => {
            refs_in_expr(ast, *left, names, occ);
            refs_in_expr(ast, *right, names, occ);
        }
        ExprKind::FunctionCall { func, args, .. } => {
            refs_in_expr(ast, *func, names, occ);
            for a in args {
                refs_in_expr(ast, a.value, names, occ);
            }
        }
        ExprKind::StructLiteral {
            name,
            bases,
            fields,
        } => {
            note(name, names, occ);
            for b in bases {
                refs_in_expr(ast, *b, names, occ);
            }
            for (_, v) in fields {
                refs_in_expr(ast, *v, names, occ);
            }
        }
        ExprKind::ArrayLiteral(es) | ExprKind::Tuple(es) => {
            for x in es {
                refs_in_expr(ast, *x, names, occ);
            }
        }
        ExprKind::Paren(x) | ExprKind::UnaryMinus(x) | ExprKind::Not(x) => {
            refs_in_expr(ast, *x, names, occ)
        }
        ExprKind::Block(b) => refs_in_block(ast, *b, names, occ),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_module;
    use ustr::Ustr;

    fn parse(src: &str, file: &str) -> Ast {
        parse_module(src, Ustr::from(file)).expect("parse failed")
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
        let batch_user = batch[1].module_sexp();

        // Incremental: registry from the un-expanded pair, then expand ONLY the
        // user module against it.
        let unexpanded = vec![parse(defs, "defs.rock"), parse(user, "user.rock")];
        let reg = collect_registry(&unexpanded);
        let mut user_only = parse(user, "user.rock");
        expand_one(&mut user_only, &reg);

        assert_eq!(
            user_only.module_sexp(),
            batch_user,
            "expand_one against the shared registry must match the batch result"
        );
        // The macro actually expanded (not left as a call).
        assert!(
            user_only.module_sexp().contains("(+ 21 21)"),
            "macro should have expanded: {}",
            user_only.module_sexp()
        );
    }

    /// Expand `user` against a workspace that also defines `macro key(x: expr) { x }`,
    /// returning the user module's s-expr. The macro body is the identity, so a
    /// CAPTURED call `key(a)` collapses to `a` (the call vanishes), while a
    /// correctly-shadowed call keeps its `(call key ...)` shape.
    fn expand_with_key_macro(user: &str) -> String {
        let mut mods = vec![
            parse("macro key(x: expr) { x }", "defs.rock"),
            parse(user, "user.rock"),
        ];
        expand_macros(&mut mods);
        mods[1].module_sexp()
    }

    #[track_caller]
    fn assert_call_survives(user: &str) {
        let sexp = expand_with_key_macro(user);
        assert!(
            sexp.contains("key"),
            "shadowed callee `key` must not expand as a macro: {sexp}"
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
        let sexp = expand_with_key_macro("fn f() -> int { return key(1) }");
        assert!(!sexp.contains("key"), "unshadowed call must expand: {sexp}");
        // Call before the `let` refers to the macro; only the later call is the local.
        let sexp = expand_with_key_macro(
            "fn f() -> int { let a = key(1)\n let key = (v: int) => v + v\n return a + key(2) }",
        );
        assert_eq!(
            sexp.matches("(call key").count(),
            1,
            "pre-let call expands, post-let call survives: {sexp}"
        );
    }

    /// Re-cooking an already-expanded module is a no-op (defs gone, no call to a
    /// registered macro remains) -- the idempotency the incremental cache relies
    /// on when a body edit re-cooks a file.
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
        let once = m.module_sexp();
        expand_one(&mut m, &reg);
        assert_eq!(m.module_sexp(), once, "second expand_one must be a no-op");
    }
}
