//! Lints over bone's typechecked HIR. A pure downstream consumer: reads
//! `Workspace`/`Body` produced by bone-semantic, never re-typechecks and never
//! touches the typechecker core. Findings publish through the LSP as warnings.
//!
//! Plan + catalog: `docs/lint-future-plan.md`, spec `docs/linter.md`.

use bone_core::hir::{BindingInfo, BodyOwner, Callee, Expr, Function, Module, Statement};
use bone_core::prelude::*;
use bone_core::workspace::{BodyInference, BoundNext, BoundTree};
use rock_core::errors::CompilerErrorLevel;

/// An `unused-var` finding: a local binding that is never read.
#[derive(Clone, Debug)]
pub struct UnusedVar {
    pub name: Ustr,
    pub span: Span,
}

impl UnusedVar {
    pub fn to_diagnostic(&self) -> CompilerError {
        CompilerError::new_with_label(
            self.span,
            format!("unused variable `{}` (prefix with `_` to silence)", self.name),
        )
        .with_level(CompilerErrorLevel::Warning)
    }
}

/// Unused local bindings in a single body. A binding is "used" iff some `Var`
/// read or local callee references it; resolution is already baked into the
/// HIR (`Expr::Var(Idx<BindingInfo>)`), so this is a flat arena walk with no
/// inference and no scope tracking. `_`-prefixed names are silenced.
pub fn unused_vars_in_body(body: &Body) -> Vec<UnusedVar> {
    let read = read_bindings(body);
    candidate_bindings(body)
        .into_iter()
        .filter(|idx| !read.contains(idx))
        .map(|idx| &body.binding_arena[idx])
        .filter(|b| !b.name.as_str().starts_with('_'))
        .map(|b| UnusedVar { name: b.name, span: b.span })
        .collect()
}

/// Bindings the lint reports on: `let`, destructuring `let`, and `for`-loop
/// vars -- the forms where "this local is never used" is unambiguous. Params
/// (lambda/fn/decorator) are `Param`-kind and excluded. Match-arm and if-let
/// pattern binds are excluded too: bone lowers a bare `Pattern::Ident` -- even
/// a payload-less enum variant like `Null` -- to a `Let` binding, so flagging
/// them would false-positive on variant arms.
fn candidate_bindings(body: &Body) -> Vec<Idx<BindingInfo>> {
    let mut out = Vec::new();
    for (_, stmt) in body.statement_arena.iter() {
        match stmt {
            Statement::Let { name, .. } => out.push(*name),
            Statement::LetTuple { vars, .. } | Statement::For { vars, .. } => {
                out.extend(vars.leaves())
            }
            _ => {}
        }
    }
    out
}

/// Every binding read anywhere in the body. The expr arena is flat, so one
/// pass catches reads at any nesting depth. A plain assignment target (`x =
/// v`) is a write, not a read, so a write-only local still counts as unused;
/// compound assign (`x += v`) and inc/dec read-then-write and DO count.
fn read_bindings(body: &Body) -> FxHashSet<Idx<BindingInfo>> {
    let mut write_targets: FxHashSet<Idx<Expr>> = FxHashSet::default();
    for (_, expr) in body.expr_arena.iter() {
        if let Expr::Assign { target, .. } = expr {
            collect_write_targets(body, *target, &mut write_targets);
        }
    }

    let mut read = FxHashSet::default();
    for (idx, expr) in body.expr_arena.iter() {
        match expr {
            Expr::Var(b) if !write_targets.contains(&idx) => {
                read.insert(*b);
            }
            Expr::Call { callee: Callee::Local(b), .. } => {
                read.insert(*b);
            }
            _ => {}
        }
    }
    read
}

/// Expr indices that are the LHS of a plain assignment. A bare `Var` target is
/// a write; a tuple target recurses; field/index targets read their receiver
/// so they are left as reads.
fn collect_write_targets(body: &Body, target: Idx<Expr>, out: &mut FxHashSet<Idx<Expr>>) {
    match &body.expr_arena[target] {
        Expr::Var(_) => {
            out.insert(target);
        }
        Expr::Tuple(elems) => {
            for e in *elems {
                collect_write_targets(body, e, out);
            }
        }
        _ => {}
    }
}

/// Canonical lint name for suppression: `// rock: allow(unused_var)`.
pub const UNUSED_VAR: &str = "unused_var";

/// A finding that carries its lint name and source line, so one suppression
/// pass serves every lint. `LINT` is the name matched inside
/// `// rock: allow(<LINT>)`.
pub trait LintFinding {
    const LINT: &'static str;
    fn line(&self) -> usize;
}

impl LintFinding for UnusedVar {
    const LINT: &'static str = UNUSED_VAR;
    fn line(&self) -> usize {
        self.span.line_range.0
    }
}

impl LintFinding for UnusedFn {
    const LINT: &'static str = UNUSED_FN;
    fn line(&self) -> usize {
        self.span.line_range.0
    }
}

/// Drop findings silenced by a `// rock: allow(<lint>)` comment on the
/// finding's own line or the line directly above it. `source` is the full text
/// of the file the findings belong to.
pub fn filter_suppressed<F: LintFinding>(findings: Vec<F>, source: &str) -> Vec<F> {
    let allow: FxHashSet<usize> = source
        .lines()
        .enumerate()
        .filter(|(_, line)| parse_allow_comment(line, F::LINT))
        .map(|(n, _)| n)
        .collect();
    if allow.is_empty() {
        return findings;
    }
    findings
        .into_iter()
        .filter(|f| {
            let line = f.line();
            !allow.contains(&line) && !(line > 0 && allow.contains(&(line - 1)))
        })
        .collect()
}

/// True iff `line` bears a `// rock: allow(<lint_name>)` magic comment
/// (standalone or trailing). Tolerates `//rock:allow`, extra whitespace, and a
/// trailing justification arg: `allow(unused_var, "WIP")`. No typo-checking --
/// a misspelled name simply won't match (spec: `docs/linter.md`).
fn parse_allow_comment(line: &str, lint_name: &str) -> bool {
    let mut rest = line;
    while let Some(pos) = rest.find("//") {
        let after = &rest[pos + 2..];
        if allow_body_matches(after, lint_name) {
            return true;
        }
        rest = after;
    }
    false
}

fn allow_body_matches(s: &str, lint_name: &str) -> bool {
    let Some(s) = s.trim_start().strip_prefix("rock") else { return false };
    let Some(s) = s.trim_start().strip_prefix(':') else { return false };
    let Some(s) = s.trim_start().strip_prefix("allow") else { return false };
    let Some(s) = s.trim_start().strip_prefix('(') else { return false };
    let Some(rest) = s.trim_start().strip_prefix(lint_name) else { return false };
    matches!(rest.chars().next(), None | Some(' ') | Some('\t') | Some(',') | Some(')'))
}

/// An `unused-fn` finding: a top-level user function unreachable from any root.
#[derive(Clone, Debug)]
pub struct UnusedFn {
    pub name: Ustr,
    pub span: Span,
}

impl UnusedFn {
    pub fn to_diagnostic(&self) -> CompilerError {
        CompilerError::new_with_label(self.span, format!("unused function `{}`", self.name))
            .with_level(CompilerErrorLevel::Warning)
    }
}

/// Canonical lint name for suppression: `// rock: allow(unused_fn)`.
pub const UNUSED_FN: &str = "unused_fn";

/// Top-level user functions unreachable from any root. A whole-workspace
/// reachability closure: seed the roots, follow call/reference edges, report
/// what's left. Roots are functions that are reachable from outside the HIR
/// call graph: `extern` / `@nomangle` (called by original name from FFI or
/// `inline(lua)`), `main`, and anything bearing a decorator (engine
/// entry-points `@init`/`@update`/`@draw`/..., `@test`/`@sim`, and custom
/// decorators whose machinery invokes the target). Bias is deliberately
/// conservative -- over-marking reachable is a missed warning; under-marking
/// false-flags live code.
///
/// The lint is only meaningful closed-world: a library's public API has no
/// in-project caller yet isn't dead. Callers gate it to `bin` packages.
pub fn unused_fns(ws: &Workspace) -> Vec<UnusedFn> {
    unused_fns_by_file(ws).into_values().flatten().collect()
}

/// Same as [`unused_fns`], but grouped by the source file each dead function
/// lives in (`Module.file`). The LSP publishes per file, so it distributes the
/// whole-project result this way; a file with no dead functions is absent.
pub fn unused_fns_by_file(ws: &Workspace) -> FxHashMap<Ustr, Vec<UnusedFn>> {
    let name_map = user_fn_names(ws);

    let mut reachable: FxHashSet<(ModuleId, Idx<Function>)> = FxHashSet::default();
    let mut stack: Vec<(ModuleId, Idx<Function>)> = Vec::new();
    for module in user_modules(ws) {
        for (fid, f) in module.functions.iter() {
            if is_root(f) && reachable.insert((module.id, fid)) {
                stack.push((module.id, fid));
            }
        }
        // Non-function bodies always run -- a global initializer, a struct's
        // field defaults, a decorator body -- so any function they reference
        // (incl. through a lambda, which is inline in the same expr arena) is a
        // root. Missing these false-flags callbacks stored in globals.
        for (owner, body) in module.bodies.iter() {
            if matches!(owner, BodyOwner::Function(_)) {
                continue;
            }
            for callee in body_edges(ws, module.id, *owner, body, &name_map) {
                if reachable.insert(callee) {
                    stack.push(callee);
                }
            }
        }
    }

    while let Some((mid, fid)) = stack.pop() {
        let owner = BodyOwner::Function(fid);
        if let Some(body) = ws.module(mid).bodies.get(&owner) {
            for callee in body_edges(ws, mid, owner, body, &name_map) {
                if reachable.insert(callee) {
                    stack.push(callee);
                }
            }
        }
    }

    let mut out: FxHashMap<Ustr, Vec<UnusedFn>> = FxHashMap::default();
    for module in user_modules(ws) {
        for (fid, f) in module.functions.iter() {
            if !reachable.contains(&(module.id, fid)) {
                out.entry(module.file)
                    .or_default()
                    .push(UnusedFn { name: f.name, span: f.name_span });
            }
        }
    }
    out
}

/// A function is a reachability root iff it can be entered from outside the
/// in-project call graph. See `unused_fns` for the rationale on each arm.
fn is_root(f: &Function) -> bool {
    f.is_extern || f.name.as_str() == "main" || !f.annotations.is_empty()
}

/// Every function one body references: the resolved-call edges in its inference
/// (calls, `for`-loop iterator protocol, and bounded-generic `next` chains --
/// each a dispatch whose target name is not reliably textual at the call site)
/// unioned with name-resolution of function-valued references. A function
/// passed as a value (`take(cb)`) lowers to `Expr::Global(cb)`, never a call
/// and never in the inference maps, so scanning `Global`/`SiblingRef` names is
/// what keeps callbacks alive. The two nets are complementary: inference sees
/// protocol dispatch the name-scan can't, the name-scan sees values inference
/// can't. Works for any body owner (function, global initializer, defaults).
fn body_edges(
    ws: &Workspace,
    mid: ModuleId,
    owner: BodyOwner,
    body: &Body,
    name_map: &FxHashMap<Ustr, Vec<(ModuleId, Idx<Function>)>>,
) -> Vec<(ModuleId, Idx<Function>)> {
    let mut out = Vec::new();
    if let Some(bi) = ws.inference(mid).and_then(|inf| inf.per_fn.get(&owner)) {
        inference_edges(bi, &mut out);
    }
    let resolve = |name: &Ustr, out: &mut Vec<_>| {
        if let Some(fns) = name_map.get(name) {
            out.extend(fns.iter().copied());
        }
    };
    for (_, expr) in body.expr_arena.iter() {
        match expr {
            Expr::Global(n) | Expr::SiblingRef(n) => resolve(n, &mut out),
            Expr::Call { callee: Callee::Unresolved { name, .. }, .. } => resolve(name, &mut out),
            _ => {}
        }
    }
    out
}

/// Every function a body's inference resolved a call/protocol edge to:
/// overload-resolved calls (incl. operators and UFCS), the `iter`/`next` of a
/// user `for`-loop, and bounded-generic `next` chains.
fn inference_edges(bi: &BodyInference, out: &mut Vec<(ModuleId, Idx<Function>)>) {
    out.extend(bi.resolved_overload_calls.values().copied());
    for info in bi.for_iter_protocol.values() {
        if let Some(f) = info.iter_fn {
            out.push(f);
        }
        collect_bound_next(&info.next, out);
    }
    for tree in bi.bound_call_trees.values() {
        collect_bound_tree(tree, out);
    }
}

fn collect_bound_tree(tree: &BoundTree, out: &mut Vec<(ModuleId, Idx<Function>)>) {
    for bn in tree.next.values() {
        collect_bound_next(bn, out);
    }
}

fn collect_bound_next(bn: &BoundNext, out: &mut Vec<(ModuleId, Idx<Function>)>) {
    if let BoundNext::Concrete { func, sub } = bn {
        out.push(*func);
        collect_bound_tree(sub, out);
    }
}

/// Every user function keyed by name (single namespace, so a name can map to
/// several overloads across modules -- all are marked reachable together).
fn user_fn_names(ws: &Workspace) -> FxHashMap<Ustr, Vec<(ModuleId, Idx<Function>)>> {
    let mut map: FxHashMap<Ustr, Vec<(ModuleId, Idx<Function>)>> = FxHashMap::default();
    for module in user_modules(ws) {
        for (fid, f) in module.functions.iter() {
            map.entry(f.name).or_default().push((module.id, fid));
        }
    }
    map
}

fn user_modules(ws: &Workspace) -> impl Iterator<Item = &Module> {
    ws.modules
        .iter()
        .filter(|m| m.id != ModuleId::builtin() && !m.file.as_str().starts_with("<bone-stdlib"))
}

/// Unused-var findings across every non-stdlib body in the workspace. A
/// convenience for tests and whole-workspace CLI runs; the LSP path lints a
/// single file's bodies instead.
pub fn unused_vars(ws: &Workspace) -> Vec<UnusedVar> {
    ws.modules
        .iter()
        .filter(|m| m.id != ModuleId::builtin() && !m.file.as_str().starts_with("<bone-stdlib"))
        .flat_map(|m| m.bodies.values())
        .flat_map(unused_vars_in_body)
        .collect()
}
