//! Lints over bone's typechecked HIR. A pure downstream consumer: reads
//! `Workspace`/`Body` produced by bone-semantic, never re-typechecks and never
//! touches the typechecker core. Findings publish through the LSP as warnings.
//!
//! Plan + catalog: `docs/lint-future-plan.md`, spec `docs/linter.md`.

use bone_core::hir::{
    BinOp, BindingInfo, BindingKind, BodyOwner, Callee, Expr, FnBackend, Function, Module,
    Statement, TyId, TypeExpr,
};
use bone_core::prelude::*;
use bone_core::workspace::{BodyInference, BoundNext, BoundTree};
use rock_core::errors::CompilerErrorLevel;

/// An `unused-var` finding: a local binding that is never read.
#[derive(Clone, Debug)]
pub struct UnusedVar {
    pub name: Ustr,
    pub span: Span,
}

/// Unused local bindings in a single body. A binding is "used" iff some `Var`
/// read or local callee references it; resolution is already baked into the
/// HIR (`Expr::Var(Idx<BindingInfo>)`), so this is a flat arena walk with no
/// inference and no scope tracking. `_`-prefixed names are silenced. `variants`
/// is the workspace's enum-variant names (see [`enum_variant_names`]), used to
/// drop payload-less match arms.
pub fn unused_vars_in_body(body: &Body, variants: &FxHashSet<Ustr>) -> Vec<UnusedVar> {
    let read = read_bindings(body);
    candidate_bindings(body, variants)
        .into_iter()
        .filter(|idx| !read.contains(idx))
        .map(|idx| &body.binding_arena[idx])
        .filter(|b| !b.name.as_str().starts_with('_'))
        .map(|b| UnusedVar { name: b.name, span: b.span })
        .collect()
}

/// Bindings the lint reports on:
/// - every `Let`-kind binding -- `let`, destructuring `let`, `for`-loop vars,
///   and match-arm / if-let pattern binds. A payload-less enum-variant arm
///   (`None =>`) lowers to a `Let` binding named after the variant, not a user
///   variable, so variant names are dropped.
/// - the enclosing function's OWN params (`param_bindings`). Lambda params are
///   `Param`-kind too but sit outside `param_bindings`, so they're excluded --
///   a lambda that ignores an arg is idiomatic. Params are skipped entirely for
///   an opaque body (`root` is `None`: `inline(lua)` / `extern`), whose Lua/C
///   consumes them invisibly.
fn candidate_bindings(body: &Body, variants: &FxHashSet<Ustr>) -> Vec<Idx<BindingInfo>> {
    let mut out: Vec<Idx<BindingInfo>> = body
        .binding_arena
        .iter()
        .filter(|(_, b)| b.kind == BindingKind::Let && !variants.contains(&b.name))
        .map(|(idx, _)| idx)
        .collect();
    if body.root.is_some() {
        out.extend(body.param_bindings);
    }
    out
}

/// Every enum-variant name across user modules. A bare variant in pattern
/// position (`None =>`) lowers to a `Let` binding of that name; the lint must
/// not report those as unused variables.
pub fn enum_variant_names(ws: &Workspace, warm: &FxHashSet<ModuleId>) -> FxHashSet<Ustr> {
    user_modules(ws, warm)
        .flat_map(|m| m.enums.iter())
        .flat_map(|(_, e)| e.variants.iter().map(|v| v.name))
        .collect()
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

/// Every lint name the suppression/completion/code-action machinery knows. The
/// single source: the LSP code action and the magic-comment completion both
/// read this so they never drift a hardcoded name apart.
pub const KNOWN_LINTS: &[&str] = &[
    UNUSED_VAR,
    UNUSED_FN,
    UNUSED_PURE_EXPR,
    TRANSMUTE_OF_NULLABLE,
    BACKEND_SIBLING_MISMATCH,
];

/// A name-tagged, rendered finding of any lint -- the type every SURFACE
/// consumes (LSP diagnostics, `rock lint`, the build pass), so a surface never
/// needs to know which lints exist or which cost class each belongs to.
#[derive(Clone, Debug)]
pub struct Finding {
    pub lint: &'static str,
    pub error: CompilerError,
    pub line: usize,
}

impl Finding {
    fn of<F: LintFinding>(f: &F) -> Self {
        Finding { lint: F::LINT, error: f.to_diagnostic(), line: f.line() }
    }

    pub fn level(&self) -> CompilerErrorLevel {
        self.error.level
    }

    pub fn file(&self) -> Ustr {
        self.error.location.file
    }
}

/// The lints that read only a body's HIR -- no inference, no other file. A
/// file's findings change only when that file's own text changes, which is what
/// lets the LSP cache them per file across unrelated edits.
///
/// `src` is the file's text, for `// rock: allow(...)` suppression. A warm
/// (stdlib) module lints to nothing: it is a read-only reference to the author,
/// and every stdlib helper would false-flag.
pub fn flat_findings(
    ws: &Workspace,
    warm: &FxHashSet<ModuleId>,
    mid: ModuleId,
    src: &str,
) -> Vec<Finding> {
    flat_findings_with(ws, &enum_variant_names(ws, warm), warm, mid, src)
}

/// [`flat_findings`] with the workspace's enum-variant names supplied. The
/// variant scan is whole-workspace, so a caller linting EVERY module computes it
/// once here instead of once per file.
fn flat_findings_with(
    ws: &Workspace,
    variants: &FxHashSet<Ustr>,
    warm: &FxHashSet<ModuleId>,
    mid: ModuleId,
    src: &str,
) -> Vec<Finding> {
    if warm.contains(&mid) {
        return Vec::new();
    }
    let module = ws.module(mid);
    let unused_vars: Vec<_> =
        module.bodies.values().flat_map(|b| unused_vars_in_body(b, variants)).collect();
    let unused_pure: Vec<_> =
        module.bodies.values().flat_map(unused_pure_exprs_in_body).collect();
    let mut out: Vec<Finding> =
        filter_suppressed(unused_vars, src).iter().map(Finding::of).collect();
    out.extend(filter_suppressed(unused_pure, src).iter().map(Finding::of));
    out
}

/// The lints that read the module's INFERENCE. A dependency edit can re-infer
/// this module and change its findings without its own text changing, so a
/// caching surface must invalidate these on re-inference, not on text.
pub fn inference_findings(
    ws: &Workspace,
    warm: &FxHashSet<ModuleId>,
    mid: ModuleId,
    src: &str,
) -> Vec<Finding> {
    if warm.contains(&mid) {
        return Vec::new();
    }
    let module = ws.module(mid);
    let inf = ws.inference(mid);
    let findings: Vec<_> = module
        .bodies
        .iter()
        .flat_map(|(owner, body)| {
            transmute_of_nullable_in_body(ws, inf.and_then(|i| i.per_fn.get(owner)), body)
        })
        .collect();
    filter_suppressed(findings, src).iter().map(Finding::of).collect()
}

/// The whole-workspace lints, grouped by the file each finding sits in. A
/// finding anywhere can flip from an edit anywhere, so these are recomputed as
/// a unit; a file with none is absent from the map. Unsuppressed -- the caller
/// holds the per-file text and applies [`filter_suppressed_findings`].
///
/// `unused_fn` is closed-world and runs only for a `bin` package;
/// `backend_sibling_mismatch` is not gated (a lib's siblings must agree too).
pub fn workspace_findings_by_file(
    ws: &Workspace,
    warm: &FxHashSet<ModuleId>,
    is_bin: bool,
) -> FxHashMap<Ustr, Vec<Finding>> {
    let mut out: FxHashMap<Ustr, Vec<Finding>> = FxHashMap::default();
    if is_bin {
        for (file, findings) in unused_fns_by_file(ws, warm) {
            out.entry(file).or_default().extend(findings.iter().map(Finding::of));
        }
    }
    // Warm modules are SCANNED (half of every stone sibling set is stdlib) but
    // not reported on, like every other lint's treatment of stdlib files.
    let warm_files: FxHashSet<Ustr> = warm.iter().map(|&mid| ws.module(mid).file).collect();
    for (file, findings) in backend_sibling_mismatches_by_file(ws) {
        if warm_files.contains(&file) {
            continue;
        }
        out.entry(file).or_default().extend(findings.iter().map(Finding::of));
    }
    out
}

/// Every lint in the catalog, reported for the files in `sources`. The
/// whole-workspace entry point behind `rock lint` and the build pass.
///
/// `sources` is the REPORT set: it names the files to report on and supplies
/// each one's text for suppression. Every other non-warm module is still
/// SCANNED -- the workspace lints need the whole call graph and every sibling
/// decl -- but produces no findings, which is how a project's `stick.toml`
/// dependencies stay silent without being invisible. Restricting the report set
/// is also what keeps the pass off the compile hot path: a dependency's bodies
/// are never walked for the per-body lints.
pub fn lint_workspace(
    ws: &Workspace,
    warm: &FxHashSet<ModuleId>,
    is_bin: bool,
    sources: &FxHashMap<Ustr, &str>,
) -> Vec<Finding> {
    let mut by_file = workspace_findings_by_file(ws, warm, is_bin);
    let variants = enum_variant_names(ws, warm);
    let mut out = Vec::new();
    for module in user_modules(ws, warm) {
        let Some(&src) = sources.get(&module.file) else { continue };
        out.extend(flat_findings_with(ws, &variants, warm, module.id, src));
        out.extend(inference_findings(ws, warm, module.id, src));
        if let Some(findings) = by_file.remove(&module.file) {
            out.extend(filter_suppressed_findings(findings, src));
        }
    }
    out
}

/// [`filter_suppressed`] for already-rendered [`Finding`]s, matching each one's
/// own lint name. Same rule: the finding's line or the line above it.
pub fn filter_suppressed_findings(findings: Vec<Finding>, source: &str) -> Vec<Finding> {
    let mut allow: FxHashSet<(usize, &'static str)> = FxHashSet::default();
    for (n, line) in source.lines().enumerate() {
        for &lint in KNOWN_LINTS {
            if parse_allow_comment(line, lint) {
                allow.insert((n, lint));
            }
        }
    }
    if allow.is_empty() {
        return findings;
    }
    findings
        .into_iter()
        .filter(|f| {
            !allow.contains(&(f.line, f.lint))
                && !(f.line > 0 && allow.contains(&(f.line - 1, f.lint)))
        })
        .collect()
}

/// A finding that carries its lint name and source line, so one suppression
/// pass serves every lint. `LINT` is the name matched inside
/// `// rock: allow(<LINT>)`.
pub trait LintFinding {
    const LINT: &'static str;
    fn line(&self) -> usize;
    fn to_diagnostic(&self) -> CompilerError;
}

impl LintFinding for UnusedVar {
    const LINT: &'static str = UNUSED_VAR;
    fn line(&self) -> usize {
        self.span.line_range.0
    }
    fn to_diagnostic(&self) -> CompilerError {
        CompilerError::new_with_label(
            self.span,
            format!("unused variable `{}` (prefix with `_` to silence)", self.name),
        )
        .with_level(CompilerErrorLevel::Warning)
    }
}

impl LintFinding for UnusedFn {
    const LINT: &'static str = UNUSED_FN;
    fn line(&self) -> usize {
        self.span.line_range.0
    }
    fn to_diagnostic(&self) -> CompilerError {
        CompilerError::new_with_label(self.span, format!("unused function `{}`", self.name))
            .with_level(CompilerErrorLevel::Warning)
    }
}

impl LintFinding for BackendSiblingMismatch {
    const LINT: &'static str = BACKEND_SIBLING_MISMATCH;
    fn line(&self) -> usize {
        self.span.line_range.0
    }
    fn to_diagnostic(&self) -> CompilerError {
        let detail = match &self.issue {
            BackendSiblingIssue::MissingOn { backend } => format!(
                "declared for `{}` but no `{backend}` declaration has this signature -- \
                 a `{backend}` build cannot emit a call to it",
                self.backend
            ),
            BackendSiblingIssue::ReturnDiffers { other_backend, other_ret } => format!(
                "returns a different type than its `{other_backend}` sibling (`{other_ret}`) -- \
                 overloads key on params, so these are one overload with two answers"
            ),
        };
        CompilerError::new_with_label(
            self.span,
            format!("backend sibling mismatch: `{}{}` {detail}", self.name, self.signature),
        )
        .with_level(CompilerErrorLevel::Warning)
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

/// What is wrong with one decl of a backend-sibling set.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum BackendSiblingIssue {
    /// No decl of this signature targets `backend` -- neither a native decl
    /// there nor a portable (`Any`) body that every backend emits.
    MissingOn { backend: &'static str },
    /// Same signature, disagreeing return types. Overload resolution keys on
    /// params only, so these two decls are ONE overload with two answers: the
    /// typechecker records the collapsed representative's return type and the
    /// other backend emits a body returning something else.
    ReturnDiffers { other_backend: &'static str, other_ret: String },
}

/// A `backend-sibling-mismatch` finding: one decl of a name that is declared on
/// more than one concrete backend, where the set of declarations does not line
/// up across those backends.
#[derive(Clone, Debug)]
pub struct BackendSiblingMismatch {
    pub name: Ustr,
    /// Rendered param list, `(int, bool)` -- the overload key.
    pub signature: String,
    pub backend: &'static str,
    pub issue: BackendSiblingIssue,
    pub span: Span,
}

/// Canonical lint name for suppression: `// rock: allow(backend_sibling_mismatch)`.
pub const BACKEND_SIBLING_MISMATCH: &str = "backend_sibling_mismatch";

/// One decl, reduced to what the sibling comparison needs.
struct SiblingDecl {
    name: Ustr,
    signature: String,
    ret: String,
    backend: FnBackend,
    span: Span,
    file: Ustr,
}

/// The lint's name for a backend, and `None` for the portable `Any` rung (which
/// is not a backend you can be missing from -- it serves all of them).
fn backend_name(b: FnBackend) -> Option<&'static str> {
    match b {
        FnBackend::Lua => Some("lua"),
        FnBackend::Js => Some("js"),
        FnBackend::Any => None,
    }
}

/// Declarations of one name that don't line up across the backends they target.
///
/// A name declared on two concrete backends is a *sibling set*: shared code
/// calls it and each backend emits its own body, so the decls have to agree.
/// They drift silently -- the split of `field_get` onto `ReadFieldRef<T, V>` left the
/// `inline(js)` sibling on `FieldRef`, which nothing caught until a luajit build
/// resolved to the js-only decl and emit rejected it. skull's emit check sees
/// only the backend being built and only reached calls, so the OTHER backend's
/// half stays broken until someone builds for it.
///
/// The set is keyed on the rendered param list (the overload key -- param
/// names and return type are not part of it) and a portable `Any` body counts
/// as serving every backend, so a wrapper-over-raw-extern pair is clean. A name
/// on ONE backend is never reported: that is a platform-exclusive fn, not a
/// drifted sibling, which is why this needs no opt-out annotation.
///
/// Whole-workspace and decl-level: no inference, no bodies, no call graph.
/// Stdlib decls participate (half of every stone sibling set lives there);
/// findings are attributed to the file of the offending decl.
pub fn backend_sibling_mismatches(ws: &Workspace) -> Vec<BackendSiblingMismatch> {
    backend_sibling_mismatches_by_file(ws).into_values().flatten().collect()
}

/// Same as [`backend_sibling_mismatches`], grouped by the source file of the
/// decl each finding sits on. The LSP publishes per file; a file with no
/// findings is absent.
pub fn backend_sibling_mismatches_by_file(
    ws: &Workspace,
) -> FxHashMap<Ustr, Vec<BackendSiblingMismatch>> {
    let mut out: FxHashMap<Ustr, Vec<BackendSiblingMismatch>> = FxHashMap::default();
    for decls in ws.functions_by_name.values() {
        for finding in sibling_set_mismatches(ws, decls) {
            let file = finding.1;
            out.entry(file).or_default().push(finding.0);
        }
    }
    out
}

/// Whether `decls` carries declarations on two DIFFERENT concrete backends --
/// the precondition for a sibling set. Reads only `Function.backend`, no types.
fn spans_two_backends(ws: &Workspace, decls: &[(ModuleId, Idx<Function>)]) -> bool {
    let mut seen: Option<&'static str> = None;
    for &(mid, fid) in decls {
        if mid == ModuleId::builtin() {
            continue;
        }
        let Some(b) = backend_name(ws.module(mid).functions[fid].backend) else { continue };
        match seen {
            None => seen = Some(b),
            Some(prev) if prev != b => return true,
            Some(_) => {}
        }
    }
    false
}

/// The findings for ONE name's decl set, each paired with the file it belongs
/// to. Empty for the overwhelmingly common case: fewer than two decls, or every
/// decl on a single backend.
fn sibling_set_mismatches(
    ws: &Workspace,
    decls: &[(ModuleId, Idx<Function>)],
) -> Vec<(BackendSiblingMismatch, Ustr)> {
    if decls.len() < 2 {
        return Vec::new();
    }
    // Cheap gate before the expensive part: a set is only interesting if its
    // decls span two concrete backends. Rendering every param list first would
    // pay a full type-render for every overload set in the workspace (stdlib
    // included) to discard almost all of them -- ~1ms per build on a game.
    if !spans_two_backends(ws, decls) {
        return Vec::new();
    }
    let infos: Vec<SiblingDecl> = decls
        .iter()
        .filter(|(mid, _)| *mid != ModuleId::builtin())
        .map(|&(mid, fid)| {
            let module = ws.module(mid);
            let f = &module.functions[fid];
            SiblingDecl {
                name: f.name,
                signature: format!(
                    "({})",
                    f.param_decls.iter().map(|p| ws.render(None, p.ty)).join(", ")
                ),
                ret: f.return_ty.map(|t| ws.render(None, t)).unwrap_or_else(|| "()".to_string()),
                backend: f.backend,
                span: f.name_span,
                file: module.file,
            }
        })
        .collect();

    let concrete: Vec<&'static str> =
        infos.iter().filter_map(|d| backend_name(d.backend)).unique().collect();
    if concrete.len() < 2 {
        return Vec::new();
    }

    let mut out = Vec::new();
    for d in &infos {
        let Some(here) = backend_name(d.backend) else { continue };
        for &other in &concrete {
            if other == here {
                continue;
            }
            let served = infos.iter().any(|o| {
                o.signature == d.signature && backend_name(o.backend).is_none_or(|b| b == other)
            });
            if !served {
                out.push((
                    BackendSiblingMismatch {
                        name: d.name,
                        signature: d.signature.clone(),
                        backend: here,
                        issue: BackendSiblingIssue::MissingOn { backend: other },
                        span: d.span,
                    },
                    d.file,
                ));
            }
        }
        if let Some(o) = infos
            .iter()
            .find(|o| o.signature == d.signature && o.ret != d.ret && o.backend != d.backend)
        {
            out.push((
                BackendSiblingMismatch {
                    name: d.name,
                    signature: d.signature.clone(),
                    backend: here,
                    issue: BackendSiblingIssue::ReturnDiffers {
                        other_backend: backend_name(o.backend).unwrap_or("portable"),
                        other_ret: o.ret.clone(),
                    },
                    span: d.span,
                },
                d.file,
            ));
        }
    }
    out
}

/// An `unused-fn` finding: a top-level user function unreachable from any root.
#[derive(Clone, Debug)]
pub struct UnusedFn {
    pub name: Ustr,
    pub span: Span,
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
    unused_fns_by_file(ws, &FxHashSet::default()).into_values().flatten().collect()
}

/// Same as [`unused_fns`], but grouped by the source file each dead function
/// lives in (`Module.file`). The LSP publishes per file, so it distributes the
/// whole-project result this way; a file with no dead functions is absent.
pub fn unused_fns_by_file(
    ws: &Workspace,
    warm: &FxHashSet<ModuleId>,
) -> FxHashMap<Ustr, Vec<UnusedFn>> {
    let name_map = user_fn_names(ws, warm);

    let mut reachable: FxHashSet<(ModuleId, Idx<Function>)> = FxHashSet::default();
    let mut stack: Vec<(ModuleId, Idx<Function>)> = Vec::new();
    for module in user_modules(ws, warm) {
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
    for module in user_modules(ws, warm) {
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
        if let Some(w) = &info.iter {
            out.push(w.func);
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
    if let BoundNext::Concrete { func, sub, .. } = bn {
        out.push(*func);
        collect_bound_tree(sub, out);
    }
}

/// Every user function keyed by name (single namespace, so a name can map to
/// several overloads across modules -- all are marked reachable together).
fn user_fn_names(
    ws: &Workspace,
    warm: &FxHashSet<ModuleId>,
) -> FxHashMap<Ustr, Vec<(ModuleId, Idx<Function>)>> {
    let mut map: FxHashMap<Ustr, Vec<(ModuleId, Idx<Function>)>> = FxHashMap::default();
    for module in user_modules(ws, warm) {
        for (fid, f) in module.functions.iter() {
            map.entry(f.name).or_default().push((module.id, fid));
        }
    }
    map
}

/// The workspace's user modules -- everything except the builtin module and the
/// warm stdlib (`warm`, the session's `warm_mids`). Warm modules carry real
/// paths now, so they can't be told apart by file id; `warm` is the authoritative
/// signal. Passing an empty set (tests / whole-workspace CLI) lints every module.
fn user_modules<'a>(
    ws: &'a Workspace,
    warm: &'a FxHashSet<ModuleId>,
) -> impl Iterator<Item = &'a Module> {
    ws.modules
        .iter()
        .filter(move |m| m.id != ModuleId::builtin() && !warm.contains(&m.id))
}

/// Unused-var findings across every non-stdlib body in the workspace. A
/// convenience for tests and whole-workspace CLI runs; the LSP path lints a
/// single file's bodies instead.
pub fn unused_vars(ws: &Workspace) -> Vec<UnusedVar> {
    let warm = FxHashSet::default();
    let variants = enum_variant_names(ws, &warm);
    user_modules(ws, &warm)
        .flat_map(|m| m.bodies.values())
        .flat_map(|b| unused_vars_in_body(b, &variants))
        .collect()
}

/// An `unused-pure-expr` finding: a statement-position expression whose result
/// is discarded and whose evaluation is side-effect-free. `is_comparison`
/// tailors the message toward the canonical `==`-for-`=` typo.
#[derive(Clone, Debug)]
pub struct UnusedPureExpr {
    pub span: Span,
    pub is_comparison: bool,
}

/// Canonical lint name for suppression: `// rock: allow(unused_pure_expr)`.
pub const UNUSED_PURE_EXPR: &str = "unused_pure_expr";

impl LintFinding for UnusedPureExpr {
    const LINT: &'static str = UNUSED_PURE_EXPR;
    fn line(&self) -> usize {
        self.span.line_range.0
    }
    fn to_diagnostic(&self) -> CompilerError {
        let msg = if self.is_comparison {
            "result of this comparison is unused -- did you mean `=`?".to_string()
        } else {
            "result of pure expression is unused".to_string()
        };
        CompilerError::new_with_label(self.span, msg).with_level(CompilerErrorLevel::Warning)
    }
}

/// Statement-position expressions whose result is discarded but whose
/// evaluation is side-effect-free. A flat HIR walk with no inference: only
/// `Statement::Expr` is a true discard position -- a block's trailing
/// expression lowers to `Body::root`/`Expr::Block.tail` (a VALUE position),
/// never a statement, so tails and expression-position blocks are never
/// touched. `is_pure` gates on the whole subtree, so a comparison with a call
/// operand (`g() == x`) is not flagged.
pub fn unused_pure_exprs_in_body(body: &Body) -> Vec<UnusedPureExpr> {
    let mut out = Vec::new();
    for (_, stmt) in body.statement_arena.iter() {
        let Statement::Expr(e) = stmt else { continue };
        let Some(is_comparison) = reportable_root(body, *e) else { continue };
        if is_pure(body, *e) {
            out.push(UnusedPureExpr { span: body.expr_spans[*e], is_comparison });
        }
    }
    out
}

/// `Some(is_comparison)` iff `e`'s ROOT is a shape worth reporting when
/// discarded. Comparisons carry the `==`-vs-`=` hint. The set is deliberately
/// narrow: constructors (struct/array/enum literals), `if`/`match`/`block`, and
/// anything with its own statement positions are excluded so we never
/// double-report or flag an intentional expression-statement block.
fn reportable_root(body: &Body, e: Idx<Expr>) -> Option<bool> {
    match &body.expr_arena[e] {
        Expr::BinOp { op, .. } => Some(is_comparison_op(*op)),
        Expr::Not(_) | Expr::BitNot(_) | Expr::UnaryMinus(_) => Some(false),
        Expr::Var(_) => Some(false),
        Expr::Int(_) | Expr::Float(_) | Expr::Bool(_) | Expr::String(_) => Some(false),
        Expr::Unit | Expr::Null => Some(false),
        Expr::FieldAccess { .. } | Expr::SafeNav { .. } | Expr::TupleIndex { .. } => Some(false),
        Expr::Tuple(_) => Some(false),
        _ => None,
    }
}

fn is_comparison_op(op: BinOp) -> bool {
    matches!(op, BinOp::Eq | BinOp::NotEq | BinOp::Lt | BinOp::Leq | BinOp::Gt | BinOp::Geq)
}

/// Whole-subtree side-effect freedom. Conservative: any construct that can run
/// user code (calls, UFCS/method calls, safe-nav calls), mutate (assign,
/// compound assign, inc/dec), error (index -- OOB), or divert control
/// (return/break/yield) makes the expression impure. Literals, reads, field
/// projections, operators, and casts are pure iff their operands are.
fn is_pure(body: &Body, e: Idx<Expr>) -> bool {
    match &body.expr_arena[e] {
        Expr::Unit
        | Expr::Null
        | Expr::Hole(_)
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::Bool(_)
        | Expr::String(_)
        | Expr::Var(_)
        | Expr::Global(_)
        | Expr::SiblingRef(_) => true,

        Expr::Not(x) | Expr::BitNot(x) | Expr::UnaryMinus(x) => is_pure(body, *x),
        Expr::As { value, .. } => is_pure(body, *value),
        Expr::FieldAccess { base, .. } | Expr::SafeNav { base, .. } => is_pure(body, *base),
        Expr::TupleIndex { tuple, .. } => is_pure(body, *tuple),
        Expr::BinOp { lhs, rhs, .. } => is_pure(body, *lhs) && is_pure(body, *rhs),
        Expr::Tuple(elems) | Expr::Array(elems) => (*elems).into_iter().all(|x| is_pure(body, x)),

        // Constructors: side-effect-free (allocation is not an observable
        // effect), pure iff every operand is. Covers the canonical bug's RHS:
        // `x == MobAiState::Wander` where `MobAiState::Wander` is an EnumVariant.
        Expr::EnumVariant { args, .. } => variant_args_pure(body, args),
        Expr::StructLiteral { fields, bases, .. } => {
            (*bases).into_iter().all(|b| is_pure(body, b))
                && (*fields).into_iter().all(|f| is_pure(body, body.struct_lit_field_arena[f].value))
        }

        _ => false,
    }
}

fn variant_args_pure(body: &Body, args: &bone_core::hir::VariantArgs) -> bool {
    use bone_core::hir::VariantArgs;
    match args {
        VariantArgs::Unit => true,
        VariantArgs::Tuple(elems) => (*elems).into_iter().all(|x| is_pure(body, x)),
        VariantArgs::Record(fields) => {
            (*fields).into_iter().all(|f| is_pure(body, body.struct_lit_field_arena[f].value))
        }
    }
}

/// Unused-pure-expr findings across every non-stdlib body in the workspace. A
/// convenience for tests and whole-workspace CLI runs; the LSP path lints a
/// single file's bodies instead.
pub fn unused_pure_exprs(ws: &Workspace) -> Vec<UnusedPureExpr> {
    let warm = FxHashSet::default();
    user_modules(ws, &warm)
        .flat_map(|m| m.bodies.values())
        .flat_map(unused_pure_exprs_in_body)
        .collect()
}

/// A `transmute_of_nullable` finding: a `cast`/`transmute` call that strips a
/// nullable argument to a non-nullable result -- laundering a real
/// null-on-failure (e.g. `stone_load_image` -> `int?`) into an apparently-valid
/// handle. `inner_type` is the argument's rendered type (`int?`, `Mob?`); the
/// diagnostic names it so the fix (`?? default` / a null check) is obvious.
#[derive(Clone, Debug)]
pub struct TransmuteOfNullable {
    pub fn_name: Ustr,
    pub inner_type: String,
    pub span: Span,
}

/// Canonical lint name for suppression: `// rock: allow(transmute_of_nullable)`.
pub const TRANSMUTE_OF_NULLABLE: &str = "transmute_of_nullable";

impl LintFinding for TransmuteOfNullable {
    const LINT: &'static str = TRANSMUTE_OF_NULLABLE;
    fn line(&self) -> usize {
        self.span.line_range.0
    }
    fn to_diagnostic(&self) -> CompilerError {
        CompilerError::new_with_label(
            self.span,
            format!(
                "`{}` strips nullability from a `{}` -- a null-on-failure silently \
                 becomes a valid value; use `?? default` or an explicit null check instead",
                self.fn_name, self.inner_type
            ),
        )
        // The catalog's only `Error`: a laundered null-on-failure is a
        // correctness hole, not a smell. Irreducible instances do exist (a
        // generic `R` has no `?? default` to fall back to -- see
        // `take_result` in stone), so unlike `entity_across_yield` the allow
        // comment stays legal.
        .with_level(CompilerErrorLevel::Error)
    }
}

/// `cast`/`transmute` calls in one body whose argument is nullable and whose
/// result is non-nullable. Both are the stdlib nullability-erasers; a call
/// whose result KEEPS the `?` is preserving (adding/round-tripping a `?` is not
/// laundering) and stays clean. Nesting needs no special-casing: each call is
/// judged on its own arg/result pair, so an inner cast that preserves
/// nullability inside an outer strip is clean at the inner site and flagged only
/// at the outer one. Reads the body's inference (`bi`) for both types; without
/// it nothing is judgeable. Matched by callee NAME -- user overloads named
/// `cast`/`transmute` that strip a `?` are the same bug shape, so flagging them
/// too is intended.
pub fn transmute_of_nullable_in_body(
    ws: &Workspace,
    bi: Option<&BodyInference>,
    body: &Body,
) -> Vec<TransmuteOfNullable> {
    let Some(bi) = bi else { return Vec::new() };
    let cast = Ustr::from("cast");
    let transmute = Ustr::from("transmute");

    let mut out = Vec::new();
    for (idx, expr) in body.expr_arena.iter() {
        let Expr::Call { callee: Callee::Unresolved { name, .. }, args } = expr else { continue };
        if (*name != cast && *name != transmute) || args.len() != 1 {
            continue;
        }
        let arg = (*args).into_iter().next().expect("len == 1");
        let (Some(arg_ty), Some(result_ty)) =
            (bi.expr_types.get(arg).copied(), bi.expr_types.get(idx).copied())
        else {
            continue;
        };
        if !is_nullable(ws, bi, arg_ty) || is_nullable(ws, bi, result_ty) {
            continue;
        }
        out.push(TransmuteOfNullable {
            fn_name: *name,
            inner_type: ws.render(Some(bi), arg_ty),
            span: body.expr_spans[idx],
        });
    }
    out
}

fn is_nullable(ws: &Workspace, bi: &BodyInference, ty: TyId) -> bool {
    matches!(head_type(ws, bi, ty), TypeExpr::Nullable(_))
}

/// Follow inference-var substitutions to a type's head constructor (mirrors
/// `Workspace::render`'s `Infer` arm). An unbound var resolves to itself, so a
/// genuinely-unknown type reads as non-nullable -- the conservative side (a
/// missed warning, never a false strip).
fn head_type(ws: &Workspace, bi: &BodyInference, ty: TyId) -> TypeExpr {
    let mut cur = ty;
    loop {
        let te = match cur {
            TyId::Static(mid, i) => ws.module(mid).ty_arena[i],
            TyId::Local(i) => bi.ty_arena[i],
        };
        match te {
            TypeExpr::Infer(var, _) => match bi.substitutions.get(var) {
                Some(sub) => cur = sub,
                None => return te,
            },
            _ => return te,
        }
    }
}

/// `transmute_of_nullable` findings across every non-stdlib body in the
/// workspace. Convenience for tests and whole-workspace CLI runs; the LSP path
/// lints a single file's bodies (reading that module's inference) instead.
pub fn transmute_of_nullable(ws: &Workspace) -> Vec<TransmuteOfNullable> {
    let warm = FxHashSet::default();
    let mut out = Vec::new();
    for module in user_modules(ws, &warm) {
        let inf = ws.inference(module.id);
        for (owner, body) in module.bodies.iter() {
            let bi = inf.and_then(|i| i.per_fn.get(owner));
            out.extend(transmute_of_nullable_in_body(ws, bi, body));
        }
    }
    out
}
