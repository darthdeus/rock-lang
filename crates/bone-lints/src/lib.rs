//! Lints over bone's typechecked HIR. A pure downstream consumer: reads
//! `Workspace`/`Body` produced by bone-semantic, never re-typechecks and never
//! touches the typechecker core. Findings publish through the LSP as warnings.
//!
//! Plan + catalog: `docs/lint-future-plan.md`, spec `docs/linter.md`.

use bone_core::hir::{BindingInfo, Callee, Expr, Statement};
use bone_core::prelude::*;
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

/// Drop findings silenced by a `// rock: allow(unused_var)` comment on the
/// finding's own line or the line directly above it. `source` is the full text
/// of the file the findings belong to.
pub fn filter_suppressed(findings: Vec<UnusedVar>, source: &str) -> Vec<UnusedVar> {
    let allow: FxHashSet<usize> = source
        .lines()
        .enumerate()
        .filter(|(_, line)| parse_allow_comment(line, UNUSED_VAR))
        .map(|(n, _)| n)
        .collect();
    if allow.is_empty() {
        return findings;
    }
    findings
        .into_iter()
        .filter(|f| {
            let line = f.span.line_range.0;
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
