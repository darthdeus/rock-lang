//! Expand macro templates before desugaring. Expression calls graft a body
//! into the caller; top-level invocations splice items into the module.

use crate::ast::*;
use crate::graft::{Graft, Plain, Rewrite};
use crate::walk::{self, expr_edges, statement_edges};
use rock_core::{CompilerError, Span};
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

/// Pre-expansion templates owned independently of the source modules.
#[derive(Clone, Debug, Default)]
pub struct MacroRegistry(Registry);

impl MacroRegistry {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Source name span used for navigation after an invocation is expanded.
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

/// Names whose registry entries could affect expansion of this AST.
/// Include member calls and top-level invocations so a retained module
/// can be recooked when one of their macro definitions changes.
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

/// Reject conflicting function names and overlapping overloads of equal priority.
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
    let mut arities: UstrMap<Vec<(MacroArity, Span)>> = UstrMap::default();
    for m in modules {
        for item in &m.items {
            if let ItemKind::Function(f) = &item.kind {
                if f.is_macro {
                    let arity = MacroArity::of(f);
                    let earlier = arities.entry(f.name.name).or_default();
                    if let Some((argc, at)) = earlier
                        .iter()
                        .find_map(|(other, at)| Some((arity.overlap(other)?, at.start())))
                    {
                        errs.push(CompilerError::new_with_label(
                            f.name.span,
                            format!(
                                "duplicate definition of macro `{}` taking {argc} argument{}: already defined at {}:{}",
                                f.name.name,
                                if argc == 1 { "" } else { "s" },
                                at.file,
                                at.line + 1
                            ),
                        ));
                    }
                    earlier.push((arity, f.name.span));
                }
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

/// The argument counts a macro overload accepts, as [`select_overload`]
/// matches them: a fixed overload takes `lo..=hi`, a variadic one `lo` or more
/// and loses to any fixed overload that also fits.
struct MacroArity {
    lo: usize,
    hi: usize,
    variadic: bool,
}

impl MacroArity {
    fn of(f: &FunctionDef) -> Self {
        match f.params.iter().position(|p| p.is_variadic) {
            Some(lo) => MacroArity {
                lo,
                hi: usize::MAX,
                variadic: true,
            },
            None => MacroArity {
                lo: f.params.iter().filter(|p| p.default.is_none()).count(),
                hi: f.params.len(),
                variadic: false,
            },
        }
    }

    /// The smallest argument count both overloads would claim.
    fn overlap(&self, other: &Self) -> Option<usize> {
        let lo = self.lo.max(other.lo);
        (self.variadic == other.variadic && lo <= self.hi.min(other.hi)).then_some(lo)
    }
}

fn call_label_errors(m: &Ast, reg: &Registry) -> Vec<CompilerError> {
    if reg.is_empty() {
        return Vec::new();
    }
    let mut errs = Vec::new();
    for data in m.exprs.values() {
        let ExprKind::FunctionCall { func, args, .. } = &data.kind else {
            continue;
        };
        let Some((name, receiver)) = macro_callee(m, *func) else {
            continue;
        };
        let Some(cands) = reg.get(&name) else {
            continue;
        };
        if args.iter().all(|a| a.name.is_none()) {
            continue;
        }
        let Some(tmpl) = select_overload(cands, args.len() + usize::from(receiver.is_some()))
        else {
            continue;
        };
        let Err(err) = bind_macro_args(&tmpl.params, &macro_args(receiver, args)) else {
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

/// Expand a module using the workspace registry, then remove its macro definitions.
pub(crate) fn expand_one(ast: &mut Ast, registry: &MacroRegistry) {
    let reg = &registry.0;
    let errors = call_label_errors(ast, reg);
    if !errors.is_empty() {
        ast.recovery_errors.extend(errors);
        return;
    }
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

/// Skip the recursive walk only if no expression can name a registered macro.
fn module_calls_macro(ast: &Ast, reg: &Registry) -> bool {
    ast.exprs.values().any(|data| {
        if let ExprKind::FunctionCall { func, .. } = &data.kind {
            return macro_callee(ast, *func).is_some_and(|(name, _)| reg.contains_key(&name));
        }
        false
    })
}

fn macro_callee(ast: &Ast, func: Idx<ExprData>) -> Option<(Ustr, Option<Idx<ExprData>>)> {
    match &ast.exprs[func].kind {
        ExprKind::Ident(id) => Some((id.name, None)),
        ExprKind::FieldAccess(receiver, member) => Some((member.name, Some(*receiver))),
        _ => None,
    }
}

fn macro_args(receiver: Option<Idx<ExprData>>, args: &[FunctionCallArg]) -> Vec<MacroArg> {
    let mut values = Vec::with_capacity(args.len() + usize::from(receiver.is_some()));
    if let Some(value) = receiver {
        values.push(MacroArg { label: None, value });
    }
    values.extend(args.iter().map(|a| MacroArg {
        label: a.name.as_ref().map(|n| n.name),
        value: a.value,
    }));
    values
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
            let mut g = Graft::new(m, &mut tmpl, Plain);
            let body = g.block(body_block);
            let return_ty = f.return_ty.map(|t| g.ty(t));
            let items = f.macro_items.iter().map(|it| g.item(it)).collect();
            let params = f
                .params
                .iter()
                .map(|p| MParam {
                    name: p.name.name,
                    is_variadic: p.is_variadic,
                    default: p.default.map(|d| g.expr(d)),
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
            None => Graft::new(&tmpl.tmpl, ast, Plain).expr(p.default?),
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
    let mut g = Graft::new(&tmpl.tmpl, ast, fragments);
    let items: Vec<Item> = tmpl.items.iter().map(|it| g.item(it)).collect();
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

/// A local binding shadows a bare macro callee of the same name.
type ShadowScope = UstrSet;

fn is_shadowed(stack: &[ShadowScope], name: Ustr) -> bool {
    stack.iter().any(|s| s.contains(&name))
}

enum Step {
    Expr(Idx<ExprData>),
    Block(Idx<BlockData>),
    Enter,
    Exit,
    Bind(Ustr),
}

#[derive(Default)]
struct Steps(smallvec::SmallVec<[Step; 8]>);

impl walk::Visit for Steps {
    fn expr(&mut self, e: &Idx<ExprData>) {
        self.0.push(Step::Expr(*e));
    }
    fn block(&mut self, b: &Idx<BlockData>) {
        self.0.push(Step::Block(*b));
    }
    fn enter(&mut self) {
        self.0.push(Step::Enter);
    }
    fn exit(&mut self) {
        self.0.push(Step::Exit);
    }
    fn bind(&mut self, id: &Ident) {
        self.0.push(Step::Bind(id.name));
    }
    fn pattern(&mut self, _: &Pattern) {}
    fn bind_pattern(&mut self, p: &Pattern) {
        walk::bindings(p, &mut |id| self.0.push(Step::Bind(id.name)));
    }
}

fn expand_steps(
    ast: &mut Ast,
    steps: Steps,
    reg: &Registry,
    depth: usize,
    stack: &mut Vec<ShadowScope>,
) {
    for step in steps.0 {
        match step {
            Step::Expr(e) => expand_expr(ast, e, reg, depth, stack),
            Step::Block(b) => expand_block(ast, b, reg, depth, stack),
            Step::Enter => stack.push(ShadowScope::default()),
            Step::Exit => {
                stack.pop();
            }
            Step::Bind(name) => {
                stack.last_mut().expect("expansion scope").insert(name);
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
    let mut steps = Steps::default();
    if let StatementKind::NestedFn(f) = &ast.stmts[s].kind {
        for p in &f.params {
            if let Some(e) = p.default {
                steps.0.push(Step::Expr(e));
            }
        }
        steps.0.push(Step::Enter);
        steps
            .0
            .extend(f.params.iter().map(|p| Step::Bind(p.name.name)));
        steps.0.push(Step::Block(f.body));
        steps.0.push(Step::Exit);
    } else {
        walk::statement(&mut steps, &ast.stmts[s].kind);
    }
    expand_steps(ast, steps, reg, depth, stack);
}

fn expand_expr(
    ast: &mut Ast,
    e: Idx<ExprData>,
    reg: &Registry,
    depth: usize,
    stack: &mut Vec<ShadowScope>,
) {
    let mut steps = Steps::default();
    walk::expr(&mut steps, &ast.exprs[e].kind);
    // Ordinary expressions visit their expression children before blocks.
    // Scoped forms retain the binding order in their step sequence.
    if !steps.0.iter().any(|s| matches!(s, Step::Enter)) {
        steps.0.sort_by_key(|s| matches!(s, Step::Block(_)));
    }
    expand_steps(ast, steps, reg, depth, stack);
    try_expand(ast, e, reg, depth, stack);
}

fn try_expand(
    ast: &mut Ast,
    e: Idx<ExprData>,
    reg: &Registry,
    depth: usize,
    stack: &mut Vec<ShadowScope>,
) {
    let ExprKind::FunctionCall { func, args, .. } = &ast.exprs[e].kind else {
        return;
    };
    let Some((name, receiver)) = macro_callee(ast, *func) else {
        return;
    };
    let Some(cands) = reg.get(&name) else {
        return;
    };
    if (receiver.is_none() && is_shadowed(stack, name)) || depth >= MAX_MACRO_DEPTH {
        return;
    }
    let Some(tmpl) = select_overload(cands, args.len() + usize::from(receiver.is_some())) else {
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

    // The preflight already reports invalid labels. Leave the call in place.
    let Ok(bound) = bind_macro_args(&tmpl.params, &macro_args(receiver, args)) else {
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
            let g = Graft::new(&tmpl.tmpl, ast, Plain).expr(d);
            inst.subs.insert(p.name, g);
        }
    }

    // Copy the body into the caller arena, rewriting it on the way
    // (`Instantiation`). Args and defaults are already in the caller arena and
    // are only ever copied plainly, so they keep their own names and spans.
    let body = Graft::new(&tmpl.tmpl, ast, inst).block(tmpl.body);
    // Expand macros that appear inside the spliced body (macros calling macros).
    expand_block(ast, body, reg, depth + 1, stack);

    // Preserve the expected type supplied by a macro return annotation.
    let value = extract_body_value(ast, body);
    ast.exprs[e].kind = match tmpl.return_ty {
        Some(rt) => {
            let rt = Graft::new(&tmpl.tmpl, ast, Plain).ty(rt);
            ExprKind::ParenAscription(value, rt)
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
        ExprKind::Path { path, generic_args } => {
            let kind = if generic_args.is_empty() {
                TypeExprKind::Path(path)
            } else {
                TypeExprKind::AppPath {
                    ctor: path,
                    args: generic_args,
                }
            };
            Some(ast.ty(span, kind, false))
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

/// Bind labelled arguments by name and positional arguments to the next free slot.
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

    fn bind_pattern(&mut self, p: &mut Pattern) {
        match p {
            Pattern::Wildcard | Pattern::Literal(_) | Pattern::Range { .. } => {}
            Pattern::Ident(id) => self.bind(&mut id.name),
            Pattern::Tuple(ps) | Pattern::Or(ps) | Pattern::PathVariant { args: Some(ps), .. } => {
                ps.iter_mut().for_each(|p| self.bind_pattern(p))
            }
            Pattern::PathVariant { args: None, .. } => {}
            Pattern::PathRecord { fields, .. } => {
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
    struct Refs<'a> {
        ast: &'a Ast,
        names: &'a UstrSet,
        occ: &'a mut Vec<(Ustr, Span)>,
    }
    impl walk::Visit for Refs<'_> {
        fn ident(&mut self, id: &Ident) {
            note(id, self.names, self.occ);
        }
        fn path(&mut self, path: &Path) {
            refs_in_path(path, self.names, self.occ);
        }
        fn ty(&mut self, t: &Idx<TypeExprData>) {
            walk::ty(self, &self.ast.types[*t].kind);
        }
    }
    walk::ty(&mut Refs { ast, names, occ }, &ast.types[t].kind);
}

fn refs_in_path(path: &Path, names: &UstrSet, occ: &mut Vec<(Ustr, Span)>) {
    for segment in &path.segments {
        note(segment, names, occ);
    }
}

fn refs_in_block(ast: &Ast, b: Idx<BlockData>, names: &UstrSet, occ: &mut Vec<(Ustr, Span)>) {
    for &st in &ast.blocks[b].statements {
        if let StatementKind::Let(l) = &ast.stmts[st].kind {
            if let Some(t) = l.ty {
                refs_in_type(ast, t, names, occ);
            }
        }
        let edges = statement_edges(ast, st);
        for e in edges.exprs {
            refs_in_expr(ast, e, names, occ);
        }
        for b in edges.blocks {
            refs_in_block(ast, b, names, occ);
        }
    }
}

fn refs_in_expr(ast: &Ast, e: Idx<ExprData>, names: &UstrSet, occ: &mut Vec<(Ustr, Span)>) {
    match &ast.exprs[e].kind {
        ExprKind::Ident(id) => note(id, names, occ),
        ExprKind::Path { path, .. } | ExprKind::PathStructLiteral { path, .. } => {
            refs_in_path(path, names, occ)
        }
        ExprKind::Match { arms, .. } => {
            for arm in arms {
                refs_in_pattern(&arm.pattern, names, occ);
            }
        }
        ExprKind::IfLetChain { clauses, .. } => {
            for clause in clauses {
                if let IfLetClause::Binding { pattern, .. } = clause {
                    refs_in_pattern(pattern, names, occ);
                }
            }
        }
        _ => {}
    }
    let edges = expr_edges(ast, e);
    for child in edges.exprs {
        refs_in_expr(ast, child, names, occ);
    }
    for b in edges.blocks {
        refs_in_block(ast, b, names, occ);
    }
}

fn refs_in_pattern(p: &Pattern, names: &UstrSet, occ: &mut Vec<(Ustr, Span)>) {
    match p {
        Pattern::PathVariant { path, args } => {
            if path.segments.len() > 1 || path.from_root {
                refs_in_path(path, names, occ);
            }
            if let Some(args) = args {
                args.iter().for_each(|p| refs_in_pattern(p, names, occ));
            }
        }
        Pattern::PathRecord { path, fields, .. } => {
            refs_in_path(path, names, occ);
            fields
                .iter()
                .for_each(|(_, p)| refs_in_pattern(p, names, occ));
        }
        Pattern::Tuple(ps) | Pattern::Or(ps) => {
            ps.iter().for_each(|p| refs_in_pattern(p, names, occ));
        }
        Pattern::Literal(_) | Pattern::Range { .. } | Pattern::Wildcard | Pattern::Ident(_) => {}
    }
}
