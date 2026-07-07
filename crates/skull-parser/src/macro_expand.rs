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
//! Not yet ported (extend when the oracle demands — none of the games need them
//! so far): variadic `...rest` splat, body-local hygiene rename, span retag for
//! `__SITE__` (the literal rides along as an `Ident`; retag only affects the
//! later site-resolution pass, not the cooked s-expr), `ty`/`ident` fragment
//! substitution, and top-level item-macro splicing.

use crate::ast::*;
use crate::clone::clone_expr;
use crate::desugar::{child_blocks, child_exprs, BlockEdges, ExprEdges};
use smallvec::smallvec;
use crate::graft::{graft_block, graft_expr, graft_item, graft_type};
use rock_core::Span;
use std::collections::HashMap;
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
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Collect every macro overload defined across `modules` into a reusable
/// registry. Read-only over `modules` (templates graft into their own arenas),
/// so the caller keeps its un-expanded ASTs intact for later re-cooking.
pub fn collect_registry(modules: &[Ast]) -> MacroRegistry {
    MacroRegistry(build_registry(modules))
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
        // pure no-op (mag: 1 def -> ~130/131 modules skip the walk entirely).
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
            if let ExprKind::Ident(id) = &ast.exprs[*func].kind {
                return reg.contains_key(&id.name);
            }
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
            let items = f.macro_items.iter().map(|it| graft_item(m, &mut tmpl, it)).collect();
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
            reg.entry(f.name.name)
                .or_default()
                .push(MacroTemplate { params, body, return_ty, items, tmpl });
        }
    }
    reg
}

/// Replace each top-level `name(args)` / `name!(args)` invocation with the
/// matching item-macro's spliced + substituted items. Non-item-macro
/// invocations are dropped (they're resolved as expr macros in bodies, not
/// here). Mirrors bone's `expand_top_level_macros`.
fn expand_top_level_macros(ast: &mut Ast, reg: &Registry) {
    if !ast.items.iter().any(|it| matches!(&it.kind, ItemKind::MacroInvocation { .. })) {
        return;
    }
    let mut out: Vec<Item> = Vec::with_capacity(ast.items.len());
    for item in std::mem::take(&mut ast.items) {
        let ItemKind::MacroInvocation { name, args, .. } = &item.kind else {
            out.push(item);
            continue;
        };
        let (name, args) = (name.clone(), args.clone());
        match expand_item_macro(ast, name, &args, reg) {
            Some(items) => out.extend(items),
            None => {} // not an item-macro at this arity — drop the invocation
        }
    }
    ast.items = out;
}

/// Select the item-macro overload for `name(args)`, build the fragment/expr
/// substitutions, graft the macro_items into `ast`, substitute, and return them.
fn expand_item_macro(ast: &mut Ast, name: Ident, args: &[Idx<ExprData>], reg: &Registry) -> Option<Vec<Item>> {
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
    // Snapshot the per-param (fragment, name) + the arg/default source so the
    // graft (needs &tmpl + &mut ast) doesn't overlap the &reg borrow.
    let plan: Vec<(Ustr, Option<Frag>, bool)> = cands[idx]
        .params
        .iter()
        .enumerate()
        .map(|(i, p)| (p.name, p.fragment, i < argc))
        .collect();
    for (i, (pname, frag, has_arg)) in plan.iter().enumerate() {
        let arg = if *has_arg {
            args[i]
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
    Some(items)
}

fn expand_in_ast(ast: &mut Ast, reg: &Registry) {
    let mut blocks: Vec<Idx<BlockData>> = Vec::new();
    let mut exprs: Vec<Idx<ExprData>> = Vec::new();
    for item in &ast.items {
        match &item.kind {
            ItemKind::Function(f) if !f.is_macro => {
                blocks.extend(f.body.rock_block())
            }
            ItemKind::Decorator(d) => blocks.push(d.body),
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
    for b in blocks {
        expand_block(ast, b, reg, 0);
    }
    for e in exprs {
        expand_expr(ast, e, reg, 0);
    }
}

fn expand_block(ast: &mut Ast, b: Idx<BlockData>, reg: &Registry, depth: usize) {
    let stmts = ast.blocks[b].statements.clone();
    for s in stmts {
        for e in stmt_exprs(ast, s) {
            expand_expr(ast, e, reg, depth);
        }
        for bl in stmt_blocks(ast, s) {
            expand_block(ast, bl, reg, depth);
        }
    }
}

/// Post-order (bone parity): expand inner calls before the enclosing one, so a
/// macro argument that is itself a macro call is expanded first.
fn expand_expr(ast: &mut Ast, e: Idx<ExprData>, reg: &Registry, depth: usize) {
    for child in child_exprs(ast, e) {
        expand_expr(ast, child, reg, depth);
    }
    for blk in child_blocks(ast, e) {
        expand_block(ast, blk, reg, depth);
    }
    try_expand(ast, e, reg, depth);
}

/// If `e` is a `name(args)` call where `name` is a registered macro, splice the
/// substituted body in as a `Block`.
fn try_expand(ast: &mut Ast, e: Idx<ExprData>, reg: &Registry, depth: usize) {
    let ExprKind::FunctionCall { func, args, .. } = &ast.exprs[e].kind else {
        return;
    };
    let ExprKind::Ident(id) = &ast.exprs[*func].kind else {
        return;
    };
    let name = id.name;
    if !reg.contains_key(&name) {
        return;
    }
    let arg_values: Vec<Idx<ExprData>> = args.iter().map(|a| a.value).collect();
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

    // Span retag (M2): stamp the macro invocation's span on every expanded body
    // node BEFORE substitution, so `__SITE__` resolves to the caller's source
    // position (not the macro body's) and body-originated type errors report at
    // the call site. Args, substituted in below, keep their own caller spans.
    // Nested macros propagate the outermost caller's span (the inner call was
    // itself retagged before it expands).
    retag_block(ast, body, ast.exprs[e].span);

    // Bind params: a regular param takes one arg (or its default); a variadic
    // param captures all trailing args, spliced wherever the body forwards
    // `...rest`.
    let mut subs: HashMap<Ustr, Idx<ExprData>> = HashMap::new();
    let mut variadic: HashMap<Ustr, Vec<Idx<ExprData>>> = HashMap::new();
    for (i, p) in tmpl.params.iter().enumerate() {
        if p.is_variadic {
            variadic.insert(p.name, arg_values.get(i..).map(|s| s.to_vec()).unwrap_or_default());
        } else if let Some(a) = arg_values.get(i) {
            subs.insert(p.name, *a);
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
    expand_block(ast, body, reg, depth + 1);

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
    ast.exprs.alloc(ExprData { span, kind: ExprKind::Block(body) })
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
                "float" => TypeExprKind::Float,
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
                    ast.types[t].kind = TypeExprKind::App { ctor, args: args.clone() };
                }
            }
            for a in args {
                subst_in_type(ast, a, ty_subs);
            }
        }
        TypeExprKind::Array(inner) => subst_in_type(ast, inner, ty_subs),
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
fn subst_in_type_exprs_block(ast: &mut Ast, b: Idx<BlockData>, ty_subs: &HashMap<Ustr, Idx<TypeExprData>>) {
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
        for e in stmt_exprs(ast, s) {
            subst_in_type_exprs(ast, e, ty_subs);
        }
        for bl in stmt_blocks(ast, s) {
            subst_in_type_exprs_block(ast, bl, ty_subs);
        }
    }
}

fn subst_in_type_exprs(ast: &mut Ast, e: Idx<ExprData>, ty_subs: &HashMap<Ustr, Idx<TypeExprData>>) {
    match ast.exprs[e].kind.clone() {
        ExprKind::As(inner, t) | ExprKind::TypeAscription(inner, t) => {
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
fn splat_expand_block(ast: &mut Ast, b: Idx<BlockData>, variadic: &HashMap<Ustr, Vec<Idx<ExprData>>>) {
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
    }
}

fn rename_block(ast: &mut Ast, b: Idx<BlockData>, callsite: u32, counter: &mut u32, stack: &mut Vec<RenameScope>) {
    // Each block is its own scope: its `let` bindings unbind at block exit.
    stack.push(RenameScope::new());
    let stmts = ast.blocks[b].statements.clone();
    for s in stmts {
        rename_stmt(ast, s, callsite, counter, stack);
    }
    stack.pop();
}

fn rename_stmt(ast: &mut Ast, s: Idx<StatementData>, callsite: u32, counter: &mut u32, stack: &mut Vec<RenameScope>) {
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

fn rename_expr(ast: &mut Ast, e: Idx<ExprData>, callsite: u32, counter: &mut u32, stack: &mut Vec<RenameScope>) {
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
                ExprKind::IfLet { value, then_block, else_block, .. } => (*value, *then_block, *else_block),
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
                ExprKind::IfLetChain { clauses, then_block, else_block } => (clauses.len(), *then_block, *else_block),
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

use std::collections::HashSet;

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

    let mut occ: Vec<(Ustr, Span)> =
        params.iter().map(|p| (p.name.name, p.name.span)).collect();
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

fn refs_in_type(ast: &Ast, t: Idx<TypeExprData>, names: &HashSet<Ustr>, occ: &mut Vec<(Ustr, Span)>) {
    match &ast.types[t].kind {
        TypeExprKind::Ident(id) => note(id, names, occ),
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
        ExprKind::StructLiteral { name, bases, fields } => {
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
