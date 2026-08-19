//! Lints over bone's typechecked HIR. A pure downstream consumer: reads
//! `Workspace`/`Body` produced by bone-semantic, never re-typechecks and never
//! touches the typechecker core. Findings publish through the LSP as warnings.
//!
//! Plan + catalog: `docs/lint-future-plan.md`, spec `docs/linter.md`.

use bone_core::hir::{
    BinOp, BindingInfo, BindingKind, BodyOwner, Callee, Expr, FnBackend, Function, Module,
    Statement, TyId, TypeCtor, TypeExpr,
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
    TASK_NULL_ABANDON,
    BACKEND_SIBLING_MISMATCH,
    METHOD_SYNTAX,
    ROW_ESCAPE,
    RAW_INPUT_MIGRATION,
    RAW_INPUT_CLASSIFICATION,
    ACTION_CALLERS,
];

/// Lints that are OFF unless the invocation names them. A MIGRATION lint has a
/// finding on nearly every declaration written before it existed, so a
/// default-on tier would bury the rest of the catalog and make every build's
/// output useless until the migration finished. Naming it
/// (`rock lint <path> --lints=method_syntax`) is what a file-at-a-time
/// conversion needs: the sweep is readable, and the build/LSP surfaces stay
/// quiet the whole time.
pub const OPT_IN_LINTS: &[&str] = &[
    METHOD_SYNTAX,
    ROW_ESCAPE,
    RAW_INPUT_MIGRATION,
    RAW_INPUT_CLASSIFICATION,
    ACTION_CALLERS,
];

/// What an invocation asks of the lint engine: which lints run, and the knobs
/// the lints themselves take. The default tier is the catalog minus
/// [`OPT_IN_LINTS`]; [`LintOptions::only`] runs exactly the named set, opt-in
/// ones included, and silences everything else.
///
/// This gates COMPUTATION, not just output: a silenced lint's analysis never
/// runs, so an opt-in lint costs a build nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LintOptions {
    only: Option<Vec<String>>,
    only_types: Option<FxHashSet<Ustr>>,
    ignore_types: FxHashSet<Ustr>,
}

impl LintOptions {
    /// Run exactly `names`. Names that aren't in [`KNOWN_LINTS`] are kept so a
    /// caller can reject a typo through [`LintOptions::unknown`] -- silently
    /// dropping one would run nothing and report success.
    pub fn only(names: impl IntoIterator<Item = String>) -> Self {
        LintOptions { only: Some(names.into_iter().collect()), ..Default::default() }
    }

    /// Drop these types from `method_syntax`'s candidate receivers. A project
    /// converts one type at a time, so the sweep has to be narrowable from the
    /// invocation -- the alternative is an `allow` comment on every call site of
    /// a type the author is not converting today.
    pub fn ignoring_types(mut self, names: impl IntoIterator<Item = Ustr>) -> Self {
        self.ignore_types = names.into_iter().collect();
        self
    }

    /// Restrict `method_syntax` to these receiver types -- the whitelist form
    /// of the same knob, for working one type at a time rather than subtracting
    /// the rest. Composes with [`LintOptions::ignoring_types`], which subtracts
    /// from whatever this leaves.
    pub fn over_types(mut self, names: impl IntoIterator<Item = Ustr>) -> Self {
        self.only_types = Some(names.into_iter().collect());
        self
    }

    pub fn ignored_types(&self) -> &FxHashSet<Ustr> {
        &self.ignore_types
    }

    /// Whether `method_syntax` may report over receiver type `name`.
    pub fn covers_type(&self, name: Ustr) -> bool {
        self.only_types.as_ref().is_none_or(|only| only.contains(&name))
            && !self.ignore_types.contains(&name)
    }

    pub fn enabled(&self, lint: &str) -> bool {
        match &self.only {
            Some(names) => names.iter().any(|n| n == lint),
            None => !OPT_IN_LINTS.contains(&lint),
        }
    }

    /// The selected names that name no lint in the catalog.
    pub fn unknown(&self) -> Vec<&str> {
        let Some(names) = &self.only else { return Vec::new() };
        names
            .iter()
            .map(String::as_str)
            .filter(|n| !KNOWN_LINTS.contains(n))
            .collect()
    }
}

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

#[derive(Clone, Debug, Default)]
pub struct LintFamilyCensus {
    pub requests: usize,
    pub files_examined: usize,
    pub modules_examined: usize,
    pub bodies_examined: usize,
    pub findings_derived: usize,
    pub findings_reused: usize,
    pub findings_suppressed: usize,
    pub elapsed_ns: u128,
}

#[derive(Clone, Debug, Default)]
pub struct LintCensus {
    pub full_workspace_passes: usize,
    pub flat: LintFamilyCensus,
    pub inference: LintFamilyCensus,
    pub suppression: LintFamilyCensus,
    pub unused_fn: LintFamilyCensus,
    pub backend_sibling: LintFamilyCensus,
    pub action_caller: LintFamilyCensus,
}

impl LintCensus {
    pub fn merge(&mut self, other: &Self) {
        self.full_workspace_passes += other.full_workspace_passes;
        for (dst, src) in [
            (&mut self.flat, &other.flat),
            (&mut self.inference, &other.inference),
            (&mut self.suppression, &other.suppression),
            (&mut self.unused_fn, &other.unused_fn),
            (&mut self.backend_sibling, &other.backend_sibling),
            (&mut self.action_caller, &other.action_caller),
        ] {
            dst.requests += src.requests;
            dst.files_examined += src.files_examined;
            dst.modules_examined += src.modules_examined;
            dst.bodies_examined += src.bodies_examined;
            dst.findings_derived += src.findings_derived;
            dst.findings_reused += src.findings_reused;
            dst.findings_suppressed += src.findings_suppressed;
            dst.elapsed_ns += src.elapsed_ns;
        }
    }
}

/// The coordinate delta produced by the same body edit that updates semantic
/// spans. A cache never guesses from the new text: it consumes this metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextEditDelta {
    pub file: Ustr,
    pub at: usize,
    pub byte_delta: isize,
    pub line_delta: isize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LintCacheUpdate {
    pub full: bool,
    pub changed_files: FxHashSet<Ustr>,
    pub reinferred: Vec<(ModuleId, BodyOwner)>,
    pub edits: Vec<TextEditDelta>,
}

impl LintCacheUpdate {
    pub fn body_only(
        changed_files: impl IntoIterator<Item = Ustr>,
        reinferred: impl IntoIterator<Item = (ModuleId, BodyOwner)>,
        edits: impl IntoIterator<Item = TextEditDelta>,
    ) -> Self {
        Self {
            changed_files: changed_files.into_iter().collect(),
            reinferred: reinferred.into_iter().collect(),
            edits: edits.into_iter().collect(),
            ..Default::default()
        }
    }

    pub fn full() -> Self {
        Self { full: true, ..Default::default() }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LintCacheStats {
    pub full_rebuilds: usize,
    pub body_updates: usize,
    pub flat_body_requests: usize,
    pub inference_body_requests: usize,
    pub flat_bodies_reused: usize,
    pub inference_bodies_reused: usize,
    pub flat_findings_derived: usize,
    pub inference_findings_derived: usize,
    pub flat_findings_reused: usize,
    pub inference_findings_reused: usize,
    pub span_shifts: usize,
    pub suppression_invalidations: usize,
    pub global_passes: usize,
    pub global_findings_derived: usize,
}

impl LintCacheStats {
    pub fn merge(&mut self, other: &Self) {
        self.full_rebuilds += other.full_rebuilds;
        self.body_updates += other.body_updates;
        self.flat_body_requests += other.flat_body_requests;
        self.inference_body_requests += other.inference_body_requests;
        self.flat_bodies_reused += other.flat_bodies_reused;
        self.inference_bodies_reused += other.inference_bodies_reused;
        self.flat_findings_derived += other.flat_findings_derived;
        self.inference_findings_derived += other.inference_findings_derived;
        self.flat_findings_reused += other.flat_findings_reused;
        self.inference_findings_reused += other.inference_findings_reused;
        self.span_shifts += other.span_shifts;
        self.suppression_invalidations += other.suppression_invalidations;
        self.global_passes += other.global_passes;
        self.global_findings_derived += other.global_findings_derived;
    }
}

#[derive(Clone, Debug, Default)]
struct CachedFileFindings {
    flat: FxHashMap<BodyOwner, Vec<Finding>>,
    inference: FxHashMap<BodyOwner, Vec<Finding>>,
    other: Vec<Finding>,
    suppression_fingerprint: u64,
}

/// The single reusable incremental lint derivation cache. It owns the local
/// body entries and the last workspace-global result; reporting policy stays in
/// the CLI/LSP adapters.
#[derive(Clone, Debug, Default)]
pub struct IncrementalLintCache {
    initialized: bool,
    is_bin: bool,
    warm: FxHashSet<ModuleId>,
    workspace_files: FxHashSet<Ustr>,
    report_files: FxHashSet<Ustr>,
    selection: Option<LintOptions>,
    files: FxHashMap<Ustr, CachedFileFindings>,
    global: FxHashMap<Ustr, Vec<Finding>>,
    last_by_file: FxHashMap<Ustr, Vec<Finding>>,
}

impl IncrementalLintCache {
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn invalidate_file(&mut self, file: Ustr) {
        self.files.remove(&file);
        self.global.remove(&file);
        self.last_by_file.remove(&file);
    }

    pub fn findings_for_file(&self, file: Ustr) -> Vec<Finding> {
        self.last_by_file.get(&file).cloned().unwrap_or_default()
    }

    pub fn update(
        &mut self,
        ws: &Workspace,
        warm: &FxHashSet<ModuleId>,
        is_bin: bool,
        sources: &FxHashMap<Ustr, &str>,
        sel: &LintOptions,
        change: &LintCacheUpdate,
    ) -> (Vec<Finding>, LintCacheStats) {
        let mut stats = LintCacheStats::default();
        let workspace_files: FxHashSet<Ustr> = ws
            .modules
            .iter()
            .filter(|m| m.id != ModuleId::builtin())
            .map(|m| m.file)
            .collect();
        let report_files: FxHashSet<Ustr> = sources.keys().copied().collect();
        let metadata_changed = self.initialized
            && (self.is_bin != is_bin
                || self.warm != *warm
                || self.workspace_files != workspace_files
                || self.report_files != report_files
                || self.selection.as_ref() != Some(sel));
        let body_cacheable = *sel == LintOptions::default();
        if !self.initialized || change.full || metadata_changed || !body_cacheable {
            let findings = self.rebuild(
                ws,
                warm,
                is_bin,
                sources,
                sel,
                workspace_files,
                report_files,
                &mut stats,
            );
            return (findings, stats);
        }

        let suppression_changed = change.changed_files.iter().any(|file| {
            let Some(source) = sources.get(file) else { return false };
            let old = self.files.get(file).map(|f| f.suppression_fingerprint);
            old.is_some_and(|old| old != suppression_fingerprint(source))
        });
        if suppression_changed {
            stats.suppression_invalidations = change
                .changed_files
                .iter()
                .filter(|file| sources.contains_key(file))
                .count();
            let findings = self.rebuild(
                ws,
                warm,
                is_bin,
                sources,
                sel,
                workspace_files,
                report_files,
                &mut stats,
            );
            return (findings, stats);
        }

        if change.changed_files.is_empty()
            && change.reinferred.is_empty()
            && change.edits.is_empty()
        {
            let findings = self.rebuild_output(ws, warm, sources);
            self.last_by_file = by_file_from_findings(&findings);
            return (findings, stats);
        }

        stats.body_updates = 1;
        self.warm = warm.clone();
        let variants = enum_variant_names(ws, warm);
        let mut flat_targets: FxHashSet<(ModuleId, BodyOwner)> = FxHashSet::default();
        let mut inference_targets: FxHashSet<(ModuleId, BodyOwner)> =
            change.reinferred.iter().copied().collect();
        for file in &change.changed_files {
            let Some(module) = ws.modules.iter().find(|m| m.file == *file) else { continue };
            let owners: Vec<BodyOwner> = module.bodies.keys().copied().collect();
            if !change.reinferred.iter().any(|(mid, _)| *mid == module.id) {
                flat_targets.extend(owners.iter().copied().map(|owner| (module.id, owner)));
                inference_targets.extend(owners.into_iter().map(|owner| (module.id, owner)));
            }
        }
        flat_targets.extend(
            change
                .reinferred
                .iter()
                .copied()
                .filter(|(mid, _)| change.changed_files.contains(&ws.module(*mid).file)),
        );
        let flat_target_owners = flat_targets.clone();
        let inference_target_owners = inference_targets.clone();

        let mut target_flat_by_file: FxHashSet<Ustr> = FxHashSet::default();
        let mut target_inference_by_file: FxHashSet<Ustr> = FxHashSet::default();
        for (mid, owner) in flat_targets {
            let file = ws.module(mid).file;
            if !self.report_files.contains(&file) {
                continue;
            }
            target_flat_by_file.insert(file);
            let cached = self.files.entry(file).or_default();
            let source = sources.get(&file).copied().unwrap_or("");
            let Some(body) = ws.module(mid).bodies.get(&owner) else { continue };
            let raw = unused_vars_in_body(body, &variants)
                .into_iter()
                .map(|f| Finding::of(&f))
                .chain(unused_pure_exprs_in_body(body).into_iter().map(|f| Finding::of(&f)))
                .collect::<Vec<_>>();
            stats.flat_body_requests += 1;
            stats.flat_findings_derived += raw.len();
            cached.flat.insert(owner, filter_suppressed_findings(raw, source));
        }
        for (mid, owner) in inference_targets {
            let file = ws.module(mid).file;
            if !self.report_files.contains(&file) {
                continue;
            }
            target_inference_by_file.insert(file);
            let cached = self.files.entry(file).or_default();
            let source = sources.get(&file).copied().unwrap_or("");
            let module = ws.module(mid);
            let Some(body) = module.bodies.get(&owner) else { continue };
            let inf = ws.inference(mid);
            let bi = inf.and_then(|i| i.per_fn.get(&owner));
            let raw = transmute_of_nullable_in_body(ws, bi, body)
                .into_iter()
                .map(|f| Finding::of(&f))
                .chain(
                    task_null_abandon_in_body(ws, bi, body)
                        .into_iter()
                        .map(|f| Finding::of(&f)),
                )
                .chain(
                    row_escapes_in_body(ws, bi, body, tail_sink(module, &owner))
                        .into_iter()
                        .map(|f| Finding::of(&f)),
                )
                .collect::<Vec<_>>();
            stats.inference_body_requests += 1;
            stats.inference_findings_derived += raw.len();
            cached.inference.insert(owner, filter_suppressed_findings(raw, source));
        }

        for module in user_modules(ws, warm) {
            let Some(source) = sources.get(&module.file).copied() else { continue };
            let cached = self.files.entry(module.file).or_default();
            cached.suppression_fingerprint = suppression_fingerprint(source);
            for owner in module.bodies.keys().copied() {
                if !cached.flat.contains_key(&owner) {
                    cached.flat.insert(owner, Vec::new());
                }
                if !cached.inference.contains_key(&owner) {
                    cached.inference.insert(owner, Vec::new());
                }
            }
            let flat_targeted = target_flat_by_file.contains(&module.file);
            let inference_targeted = target_inference_by_file.contains(&module.file);
            if flat_targeted {
                stats.flat_bodies_reused += cached
                    .flat
                    .keys()
                    .filter(|owner| !flat_target_owners.contains(&(module.id, **owner)))
                    .count();
                stats.flat_findings_reused += cached
                    .flat
                    .iter()
                    .filter(|(owner, _)| !flat_target_owners.contains(&(module.id, **owner)))
                    .map(|(_, findings)| findings.len())
                    .sum::<usize>();
            } else {
                stats.flat_bodies_reused += cached.flat.len();
                stats.flat_findings_reused += cached.flat.values().map(Vec::len).sum::<usize>();
            }
            if inference_targeted {
                stats.inference_bodies_reused += cached
                    .inference
                    .keys()
                    .filter(|owner| !inference_target_owners.contains(&(module.id, **owner)))
                    .count();
                stats.inference_findings_reused += cached
                    .inference
                    .iter()
                    .filter(|(owner, _)| !inference_target_owners.contains(&(module.id, **owner)))
                    .map(|(_, findings)| findings.len())
                    .sum::<usize>();
            } else {
                stats.inference_bodies_reused += cached.inference.len();
                stats.inference_findings_reused +=
                    cached.inference.values().map(Vec::len).sum::<usize>();
            }
        }

        for edit in &change.edits {
            let Some(cached) = self.files.get_mut(&edit.file) else { continue };
            for (owner, findings) in &mut cached.flat {
                if !flat_target_owners.contains(&(
                    ws.modules.iter().find(|m| m.file == edit.file).map_or(ModuleId::builtin(), |m| m.id),
                    *owner,
                )) {
                    stats.span_shifts += shift_findings(findings, edit);
                }
            }
            for (owner, findings) in &mut cached.inference {
                if !inference_target_owners.contains(&(
                    ws.modules.iter().find(|m| m.file == edit.file).map_or(ModuleId::builtin(), |m| m.id),
                    *owner,
                )) {
                    stats.span_shifts += shift_findings(findings, edit);
                }
            }
            stats.span_shifts += shift_findings(&mut cached.other, edit);
        }

        self.global.clear();
        let by_file = workspace_findings_by_file_with(ws, warm, is_bin, sel);
        stats.global_passes = 1;
        for (file, findings) in by_file {
            let Some(source) = sources.get(&file).copied() else { continue };
            let kept = filter_suppressed_findings(findings, source);
            stats.global_findings_derived += kept.len();
            self.global.insert(file, kept);
        }
        let findings = self.rebuild_output(ws, warm, sources);
        self.last_by_file = by_file_from_findings(&findings);
        (findings, stats)
    }

    fn rebuild(
        &mut self,
        ws: &Workspace,
        warm: &FxHashSet<ModuleId>,
        is_bin: bool,
        sources: &FxHashMap<Ustr, &str>,
        sel: &LintOptions,
        workspace_files: FxHashSet<Ustr>,
        report_files: FxHashSet<Ustr>,
        stats: &mut LintCacheStats,
    ) -> Vec<Finding> {
        let findings = lint_workspace(ws, warm, is_bin, sources, sel);
        self.initialized = true;
        self.is_bin = is_bin;
        self.warm = warm.clone();
        self.workspace_files = workspace_files;
        self.report_files = report_files;
        self.selection = Some(sel.clone());
        self.files.clear();
        self.global.clear();
        for (&file, source) in sources {
            self.files.insert(
                file,
                CachedFileFindings {
                    suppression_fingerprint: suppression_fingerprint(source),
                    ..Default::default()
                },
            );
        }
        for finding in &findings {
            if is_global_lint(finding.lint) {
                self.global.entry(finding.file()).or_default().push(finding.clone());
                continue;
            }
            let file = finding.file();
            let Some(module) = ws.modules.iter().find(|m| m.file == file) else { continue };
            let owner = finding_owner(module, finding).map(|owner| {
                if is_inference_lint(finding.lint) { &mut self.files.get_mut(&file).unwrap().inference }
                else { &mut self.files.get_mut(&file).unwrap().flat }
                    .entry(owner)
                    .or_default()
                    .push(finding.clone());
            });
            if owner.is_none() {
                self.files.entry(file).or_default().other.push(finding.clone());
            }
        }
        for module in user_modules(ws, warm) {
            let Some(file) = sources.get(&module.file) else { continue };
            let cached = self.files.entry(module.file).or_default();
            cached.suppression_fingerprint = suppression_fingerprint(file);
            for owner in module.bodies.keys().copied() {
                cached.flat.entry(owner).or_default();
                cached.inference.entry(owner).or_default();
            }
        }
        self.last_by_file = by_file_from_findings(&findings);
        stats.full_rebuilds = 1;
        findings
    }

    fn rebuild_output(
        &self,
        ws: &Workspace,
        warm: &FxHashSet<ModuleId>,
        sources: &FxHashMap<Ustr, &str>,
    ) -> Vec<Finding> {
        let mut out = Vec::new();
        for module in user_modules(ws, warm) {
            if !sources.contains_key(&module.file) {
                continue;
            }
            let Some(cached) = self.files.get(&module.file) else { continue };
            for owner in module.bodies.keys() {
                out.extend(cached.flat.get(owner).cloned().unwrap_or_default());
            }
            for owner in module.bodies.keys() {
                out.extend(cached.inference.get(owner).cloned().unwrap_or_default());
            }
            out.extend(cached.other.clone());
            out.extend(self.global.get(&module.file).cloned().unwrap_or_default());
        }
        out
    }
}

fn is_global_lint(lint: &str) -> bool {
    matches!(lint, UNUSED_FN | BACKEND_SIBLING_MISMATCH | ACTION_CALLERS)
}

fn is_inference_lint(lint: &str) -> bool {
    matches!(lint, TRANSMUTE_OF_NULLABLE | TASK_NULL_ABANDON | ROW_ESCAPE)
}

fn finding_offset(finding: &Finding) -> usize {
    finding
        .error
        .labels
        .first()
        .map(|label| label.span.offset_range.0)
        .unwrap_or(finding.error.location.offset)
}

fn finding_owner(module: &Module, finding: &Finding) -> Option<BodyOwner> {
    module
        .item_index()
        .find(finding_offset(finding), finding_offset(finding).saturating_add(1))
        .min_by_key(|interval| interval.stop - interval.start)
        .map(|interval| interval.val)
}

fn by_file_from_findings(findings: &[Finding]) -> FxHashMap<Ustr, Vec<Finding>> {
    let mut by_file = FxHashMap::default();
    for finding in findings {
        by_file.entry(finding.file()).or_insert_with(Vec::new).push(finding.clone());
    }
    by_file
}

fn suppression_fingerprint(source: &str) -> u64 {
    let mut hash = 1469598103934665603u64;
    for line in source.lines() {
        if line.contains("rock") && line.contains("allow") {
            for byte in line.as_bytes() {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(1099511628211);
            }
            hash ^= b'\n' as u64;
            hash = hash.wrapping_mul(1099511628211);
        }
    }
    hash
}

fn shift_findings(findings: &mut [Finding], edit: &TextEditDelta) -> usize {
    let mut shifted = 0;
    for finding in findings {
        let mut moved = false;
        if finding.error.location.file == edit.file
            && finding.error.location.offset >= edit.at
        {
            finding.error.location.offset = shift_value(finding.error.location.offset, edit.byte_delta);
            finding.error.location.line = shift_value(finding.error.location.line, edit.line_delta);
            moved = true;
        }
        for label in &mut finding.error.labels {
            if label.span.file != edit.file || label.span.offset_range.0 < edit.at {
                continue;
            }
            label.span = shift_span(label.span, edit);
            moved = true;
        }
        if moved {
            if let Some(label) = finding.error.labels.first() {
                finding.line = label.span.line_range.0;
            } else {
                finding.line = finding.error.location.line;
            }
            shifted += 1;
        }
    }
    shifted
}

fn shift_span(mut span: Span, edit: &TextEditDelta) -> Span {
    span.offset_range.0 = shift_value(span.offset_range.0, edit.byte_delta);
    span.offset_range.1 = shift_value(span.offset_range.1, edit.byte_delta);
    span.line_range.0 = shift_value(span.line_range.0, edit.line_delta);
    span.line_range.1 = shift_value(span.line_range.1, edit.line_delta);
    if edit.line_delta == 0 {
        span.col_range.0 = shift_value(span.col_range.0, edit.byte_delta);
        span.col_range.1 = shift_value(span.col_range.1, edit.byte_delta);
    }
    span
}

fn shift_value(value: usize, delta: isize) -> usize {
    if delta.is_negative() {
        value.saturating_sub(delta.unsigned_abs())
    } else {
        value.saturating_add(delta as usize)
    }
}

#[derive(Copy, Clone)]
enum CensusFamily {
    Flat,
    Inference,
    UnusedFn,
    BackendSibling,
    ActionCaller,
}

fn family_mut(census: &mut LintCensus, family: CensusFamily) -> &mut LintFamilyCensus {
    match family {
        CensusFamily::Flat => &mut census.flat,
        CensusFamily::Inference => &mut census.inference,
        CensusFamily::UnusedFn => &mut census.unused_fn,
        CensusFamily::BackendSibling => &mut census.backend_sibling,
        CensusFamily::ActionCaller => &mut census.action_caller,
    }
}

fn record_family(
    census: Option<&mut LintCensus>,
    family: CensusFamily,
    elapsed: std::time::Duration,
    files: usize,
    modules: usize,
    bodies: usize,
    findings: usize,
) {
    let Some(census) = census else { return };
    let family = family_mut(census, family);
    family.requests += 1;
    family.files_examined += files;
    family.modules_examined += modules;
    family.bodies_examined += bodies;
    family.findings_derived += findings;
    family.elapsed_ns += elapsed.as_nanos();
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
    let census = None;
    flat_findings_with(
        ws,
        &enum_variant_names(ws, warm),
        warm,
        mid,
        src,
        &LintOptions::default(),
        &FxHashSet::default(),
        census,
    )
}

/// [`flat_findings`] with the workspace-wide scans supplied. The variant scan
/// and the nominal-type scan are whole-workspace, so a caller linting EVERY
/// module computes them once instead of once per file. `types` is only read by
/// the opt-in `method_syntax` lint, so the default tier passes an empty set.
fn flat_findings_with(
    ws: &Workspace,
    variants: &FxHashSet<Ustr>,
    warm: &FxHashSet<ModuleId>,
    mid: ModuleId,
    src: &str,
    sel: &LintOptions,
    types: &FxHashSet<Ustr>,
    mut census: Option<&mut LintCensus>,
) -> Vec<Finding> {
    if warm.contains(&mid) {
        return Vec::new();
    }
    let module = ws.module(mid);
    let mut out: Vec<Finding> = Vec::new();
    if sel.enabled(UNUSED_VAR) {
        let found: Vec<_> =
            module.bodies.values().flat_map(|b| unused_vars_in_body(b, variants)).collect();
        out.extend(
            filter_suppressed_census(found, src, census.as_deref_mut())
                .iter()
                .map(Finding::of),
        );
    }
    if sel.enabled(UNUSED_PURE_EXPR) {
        let found: Vec<_> =
            module.bodies.values().flat_map(unused_pure_exprs_in_body).collect();
        out.extend(
            filter_suppressed_census(found, src, census.as_deref_mut())
                .iter()
                .map(Finding::of),
        );
    }
    if sel.enabled(METHOD_SYNTAX) {
        let found = method_syntax_in_module(ws, module, types, src);
        out.extend(
            filter_suppressed_census(found, src, census.as_deref_mut())
                .iter()
                .map(Finding::of),
        );
    }
    if sel.enabled(RAW_INPUT_MIGRATION) {
        let found = raw_input_sites_in_module(module);
        out.extend(
            filter_suppressed_census(found, src, census.as_deref_mut())
                .iter()
                .map(Finding::of),
        );
    }
    if sel.enabled(RAW_INPUT_CLASSIFICATION) {
        let found = unclassified_raw_input_sites_in_module(module, src);
        out.extend(
            filter_suppressed_census(found, src, census.as_deref_mut())
                .iter()
                .map(Finding::of),
        );
    }
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
    inference_findings_with(ws, warm, mid, src, &LintOptions::default(), None)
}

fn inference_findings_with(
    ws: &Workspace,
    warm: &FxHashSet<ModuleId>,
    mid: ModuleId,
    src: &str,
    sel: &LintOptions,
    mut census: Option<&mut LintCensus>,
) -> Vec<Finding> {
    if warm.contains(&mid) {
        return Vec::new();
    }
    let module = ws.module(mid);
    let inf = ws.inference(mid);
    let bi = |owner: &BodyOwner| inf.and_then(|i| i.per_fn.get(owner));
    let mut out: Vec<Finding> = Vec::new();
    if sel.enabled(TRANSMUTE_OF_NULLABLE) {
        let found: Vec<_> = module
            .bodies
            .iter()
            .flat_map(|(owner, body)| transmute_of_nullable_in_body(ws, bi(owner), body))
            .collect();
        out.extend(
            filter_suppressed_census(found, src, census.as_deref_mut())
                .iter()
                .map(Finding::of),
        );
    }
    if sel.enabled(TASK_NULL_ABANDON) {
        let found: Vec<_> = module
            .bodies
            .iter()
            .flat_map(|(owner, body)| task_null_abandon_in_body(ws, bi(owner), body))
            .collect();
        out.extend(
            filter_suppressed_census(found, src, census.as_deref_mut())
                .iter()
                .map(Finding::of),
        );
    }
    if sel.enabled(ROW_ESCAPE) {
        let found: Vec<_> = module
            .bodies
            .iter()
            .flat_map(|(owner, body)| {
                row_escapes_in_body(ws, bi(owner), body, tail_sink(module, owner))
            })
            .collect();
        out.extend(
            filter_suppressed_census(found, src, census.as_deref_mut())
                .iter()
                .map(Finding::of),
        );
    }
    out
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
    workspace_findings_by_file_with(ws, warm, is_bin, &LintOptions::default())
}

fn workspace_findings_by_file_with(
    ws: &Workspace,
    warm: &FxHashSet<ModuleId>,
    is_bin: bool,
    sel: &LintOptions,
) -> FxHashMap<Ustr, Vec<Finding>> {
    workspace_findings_by_file_census(ws, warm, is_bin, sel, None)
}

fn workspace_findings_by_file_census(
    ws: &Workspace,
    warm: &FxHashSet<ModuleId>,
    is_bin: bool,
    sel: &LintOptions,
    mut census: Option<&mut LintCensus>,
) -> FxHashMap<Ustr, Vec<Finding>> {
    let mut out: FxHashMap<Ustr, Vec<Finding>> = FxHashMap::default();
    if is_bin && sel.enabled(UNUSED_FN) {
        let t = std::time::Instant::now();
        let findings = unused_fns_by_file(ws, warm);
        let derived = findings.values().map(Vec::len).sum();
        record_family(
            census.as_deref_mut(),
            CensusFamily::UnusedFn,
            t.elapsed(),
            findings.len(),
            user_modules(ws, warm).count(),
            ws.modules
                .iter()
                .filter(|m| m.id != ModuleId::builtin() && !warm.contains(&m.id))
                .flat_map(|m| m.bodies.values())
                .count(),
            derived,
        );
        for (file, findings) in findings {
            out.entry(file).or_default().extend(findings.iter().map(Finding::of));
        }
    }
    if !sel.enabled(BACKEND_SIBLING_MISMATCH) {
        if sel.enabled(ACTION_CALLERS) {
            let t = std::time::Instant::now();
            let findings = action_caller_gaps(ws, warm);
            record_family(
                census.as_deref_mut(),
                CensusFamily::ActionCaller,
                t.elapsed(),
                0,
                user_modules(ws, warm).count(),
                ws.modules
                    .iter()
                    .filter(|m| m.id != ModuleId::builtin() && !warm.contains(&m.id))
                    .flat_map(|m| m.bodies.values())
                    .count(),
                findings.len(),
            );
            for finding in findings {
                out.entry(finding.file).or_default().push(Finding::of(&finding));
            }
        }
        return out;
    }
    // Warm modules are SCANNED (half of every stone sibling set is stdlib) but
    // not reported on, like every other lint's treatment of stdlib files.
    let warm_files: FxHashSet<Ustr> = warm.iter().map(|&mid| ws.module(mid).file).collect();
    let t = std::time::Instant::now();
    let sibling_findings = backend_sibling_mismatches_by_file(ws);
    let derived = sibling_findings.values().map(Vec::len).sum();
    record_family(
        census.as_deref_mut(),
        CensusFamily::BackendSibling,
        t.elapsed(),
        sibling_findings.len(),
        user_modules(ws, warm).count(),
        0,
        derived,
    );
    for (file, findings) in sibling_findings {
        if warm_files.contains(&file) {
            continue;
        }
        out.entry(file).or_default().extend(findings.iter().map(Finding::of));
    }
    if sel.enabled(ACTION_CALLERS) {
        let t = std::time::Instant::now();
        let findings = action_caller_gaps(ws, warm);
        record_family(
            census.as_deref_mut(),
            CensusFamily::ActionCaller,
            t.elapsed(),
            0,
            user_modules(ws, warm).count(),
            ws.modules
                .iter()
                .filter(|m| m.id != ModuleId::builtin() && !warm.contains(&m.id))
                .flat_map(|m| m.bodies.values())
                .count(),
            findings.len(),
        );
        for finding in findings {
            out.entry(finding.file).or_default().push(Finding::of(&finding));
        }
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
///
/// `sel` selects the tier: [`LintOptions::default`] is what a build and the
/// LSP run (the catalog minus [`OPT_IN_LINTS`]); `rock lint --lints=<names>`
/// runs exactly what it names.
pub fn lint_workspace(
    ws: &Workspace,
    warm: &FxHashSet<ModuleId>,
    is_bin: bool,
    sources: &FxHashMap<Ustr, &str>,
    sel: &LintOptions,
) -> Vec<Finding> {
    lint_workspace_inner(ws, warm, is_bin, sources, sel, None)
}

pub fn lint_workspace_with_census(
    ws: &Workspace,
    warm: &FxHashSet<ModuleId>,
    is_bin: bool,
    sources: &FxHashMap<Ustr, &str>,
    sel: &LintOptions,
) -> (Vec<Finding>, LintCensus) {
    let mut census = LintCensus::default();
    let findings = lint_workspace_inner(ws, warm, is_bin, sources, sel, Some(&mut census));
    (findings, census)
}

fn lint_workspace_inner(
    ws: &Workspace,
    warm: &FxHashSet<ModuleId>,
    is_bin: bool,
    sources: &FxHashMap<Ustr, &str>,
    sel: &LintOptions,
    mut census: Option<&mut LintCensus>,
) -> Vec<Finding> {
    if let Some(census) = census.as_deref_mut() {
        census.full_workspace_passes += 1;
    }
    let mut by_file = workspace_findings_by_file_census(
        ws,
        warm,
        is_bin,
        sel,
        census.as_deref_mut(),
    );
    let variants = enum_variant_names(ws, warm);
    let types = if sel.enabled(METHOD_SYNTAX) {
        let mut names = nominal_type_names(ws, warm);
        names.retain(|&n| sel.covers_type(n));
        names
    } else {
        FxHashSet::default()
    };
    let mut out = Vec::new();
    for module in user_modules(ws, warm) {
        let Some(&src) = sources.get(&module.file) else { continue };
        let t_flat = std::time::Instant::now();
        let flat = flat_findings_with(
            ws,
            &variants,
            warm,
            module.id,
            src,
            sel,
            &types,
            census.as_deref_mut(),
        );
        record_family(
            census.as_deref_mut(),
            CensusFamily::Flat,
            t_flat.elapsed(),
            1,
            1,
            module.bodies.len(),
            flat.len(),
        );
        out.extend(flat);

        let t_inference = std::time::Instant::now();
        let inference = inference_findings_with(
            ws,
            warm,
            module.id,
            src,
            sel,
            census.as_deref_mut(),
        );
        record_family(
            census.as_deref_mut(),
            CensusFamily::Inference,
            t_inference.elapsed(),
            1,
            1,
            module.bodies.len(),
            inference.len(),
        );
        out.extend(inference);
        if let Some(findings) = by_file.remove(&module.file) {
            let findings = filter_suppressed_findings_census(
                findings,
                src,
                census.as_deref_mut(),
            );
            out.extend(findings);
        }
    }
    out
}

/// [`filter_suppressed`] for already-rendered [`Finding`]s, matching each one's
/// own lint name. Same rule: the finding's line or the line above it.
pub fn filter_suppressed_findings(findings: Vec<Finding>, source: &str) -> Vec<Finding> {
    filter_suppressed_findings_census(findings, source, None)
}

fn filter_suppressed_findings_census(
    findings: Vec<Finding>,
    source: &str,
    census: Option<&mut LintCensus>,
) -> Vec<Finding> {
    let raw = findings.len();
    let started = std::time::Instant::now();
    let mut allow: FxHashSet<(usize, &'static str)> = FxHashSet::default();
    for (n, line) in source.lines().enumerate() {
        for &lint in KNOWN_LINTS {
            if parse_allow_comment(line, lint) {
                allow.insert((n, lint));
            }
        }
    }
    let filtered = if allow.is_empty() {
        findings
    } else {
        findings
        .into_iter()
        .filter(|f| {
            !allow.contains(&(f.line, f.lint))
                && !(f.line > 0 && allow.contains(&(f.line - 1, f.lint)))
        })
        .collect()
    };
    if let Some(census) = census {
        let family = &mut census.suppression;
        family.requests += 1;
        family.files_examined += 1;
        family.findings_derived += raw;
        family.findings_suppressed += raw - filtered.len();
        family.elapsed_ns += started.elapsed().as_nanos();
    }
    filtered
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
    filter_suppressed_census(findings, source, None)
}

fn filter_suppressed_census<F: LintFinding>(
    findings: Vec<F>,
    source: &str,
    census: Option<&mut LintCensus>,
) -> Vec<F> {
    let raw = findings.len();
    let started = std::time::Instant::now();
    let allow: FxHashSet<usize> = source
        .lines()
        .enumerate()
        .filter(|(_, line)| parse_allow_comment(line, F::LINT))
        .map(|(n, _)| n)
        .collect();
    let filtered = if allow.is_empty() {
        findings
    } else {
        findings
        .into_iter()
        .filter(|f| {
            let line = f.line();
            !allow.contains(&line) && !(line > 0 && allow.contains(&(line - 1)))
        })
        .collect()
    };
    if let Some(census) = census {
        let family = &mut census.suppression;
        family.requests += 1;
        family.files_examined += 1;
        family.findings_derived += raw;
        family.findings_suppressed += raw - filtered.len();
        family.elapsed_ns += started.elapsed().as_nanos();
    }
    filtered
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
            // A derived newtype operator is not user surface: there is no
            // declaration to delete and its span points at the `newtype` line.
            if !reachable.contains(&(module.id, fid)) && !f.is_synthetic {
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
    f.is_extern
        || f.name.as_str() == "main"
        || f.annotations.iter().any(|a| !is_implementation_anno(a.name.name.as_str()))
}

/// Builtin fn annotations that describe how a function is COMPILED, not how it
/// is entered. Everything else -- `@test`/`@sim`, `@shader`, `@nomangle`, and
/// every user `deco` -- is a root, so an annotation this list doesn't know
/// keeps the conservative bias (root, no warning). Without the split, `@inline`
/// or `@pure` alone silently exempts a fn from `unused_fn`.
fn is_implementation_anno(name: &str) -> bool {
    matches!(
        name,
        "pure"
            | "debug_impure"
            | "inline"
            | "intrinsic"
            | "native_required"
            | "gpu_only"
            | "scalar_abi"
            | "struct_expand"
            | "shader_expand"
            | "build_var_expand"
            | "embed_expand"
            | "consume"
    )
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
/// overload-resolved calls (incl. operators and UFCS), implicit iterable
/// coercions, the `iter`/`next` of a user `for`-loop, and bounded-generic `next`
/// chains.
fn inference_edges(bi: &BodyInference, out: &mut Vec<(ModuleId, Idx<Function>)>) {
    out.extend(bi.resolved_overload_calls.values().copied());
    out.extend(bi.implicit_iter_args.values().map(|w| w.func));
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
        let Expr::Call { callee: Callee::Unresolved { name, .. }, args, .. } = expr else { continue };
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
pub(crate) fn head_type(ws: &Workspace, bi: &BodyInference, ty: TyId) -> TypeExpr {
    head_ty(ws, bi, ty).1
}

/// [`head_type`] keeping the resolved id. A nested `Idx<TypeExpr>` (a
/// `Nullable`'s inner, an `App`'s params) indexes the arena of the type that
/// CONTAINS it -- which substitution-following may have moved to another module
/// -- so descending needs `resolved.with_idx(inner)`, never a bare
/// `TyId::Local`.
fn head_ty(ws: &Workspace, bi: &BodyInference, ty: TyId) -> (TyId, TypeExpr) {
    let mut cur = ty;
    loop {
        let te = match cur {
            TyId::Static(mid, i) => ws.module(mid).ty_arena[i],
            TyId::Local(i) => bi.ty_arena[i],
        };
        match te {
            TypeExpr::Infer(var, _) => match bi.substitutions.get(var) {
                Some(sub) => cur = sub,
                None => return (cur, te),
            },
            _ => return (cur, te),
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

/// A `method_syntax` finding: a free function whose first parameter is a user
/// type, declared in prefix form (`fn damage(m: Mob, n: int)`) where the
/// receiver form (`fn Mob.damage(m, n: int)`) says the same thing.
#[derive(Clone, Debug)]
pub struct MethodSyntax {
    pub name: Ustr,
    /// The nominal type of param 0 -- the receiver path of the rewrite.
    pub receiver: Ustr,
    /// The rewritten parameter list, param 0 stripped of its type annotation.
    pub params: String,
    pub span: Span,
}

/// Canonical lint name for suppression: `// rock: allow(method_syntax)`.
pub const METHOD_SYNTAX: &str = "method_syntax";

impl LintFinding for MethodSyntax {
    const LINT: &'static str = METHOD_SYNTAX;
    fn line(&self) -> usize {
        self.span.line_range.0
    }
    fn to_diagnostic(&self) -> CompilerError {
        CompilerError::new_with_label(
            self.span,
            format!(
                "`{}` takes `{}` as its first parameter -- declare it as `fn {}.{}({})`",
                self.name, self.receiver, self.receiver, self.name, self.params
            ),
        )
        .with_level(CompilerErrorLevel::Advice)
    }
}

/// One direct call to an input observation primitive in a cold module. This
/// is an opt-in migration census: the checked HIR has already resolved scopes
/// and rejected malformed calls, while the source span identifies the caller
/// that still needs an action/editor/tool/text classification.
#[derive(Clone, Debug)]
pub struct RawInputSite {
    pub name: Ustr,
    pub span: Span,
}

/// Canonical lint name for the Phase 6 raw-input migration census.
pub const RAW_INPUT_MIGRATION: &str = "action_layer_raw_input";

/// The live census remains unsuppressed; this second opt-in lint makes every
/// remaining site carry one of the accepted non-action classes in source.
pub const RAW_INPUT_CLASSIFICATION: &str = "action_layer_raw_input_classification";

pub const RAW_INPUT_CLASSES: &[&str] = &[
    "editor_tool_debug",
    "deliberate_pointer",
    "splash_skip",
    "test_only",
];

const RAW_INPUT_NAMES: &[&str] = &[
    "is_key_down",
    "is_key_pressed",
    "is_key_released",
    "is_scancode_down",
    "is_scancode_pressed",
    "is_scancode_released",
    "is_mouse_down",
    "is_mouse_pressed",
    "is_mouse_released",
    "mouse_screen",
    "stone_get_mouse_position",
    "stone_get_global_mouse_position",
    "is_pad_down",
    "is_pad_pressed",
    "is_pad_released",
    "pad_axis_value",
];

/// Direct input observations are the census population. Action calls, input
/// event injection, and tool-input helpers are intentionally separate APIs and
/// therefore do not appear here.
pub fn raw_input_sites_in_module(module: &Module) -> Vec<RawInputSite> {
    let mut out = Vec::new();
    for body in module.bodies.values() {
        for (idx, expr) in body.expr_arena.iter() {
            let Expr::Call { callee: Callee::Unresolved { name, .. }, .. } = expr else {
                continue;
            };
            if RAW_INPUT_NAMES.contains(&name.as_str()) {
                out.push(RawInputSite { name: *name, span: body.expr_spans[idx] });
            }
        }
    }
    out
}

impl LintFinding for RawInputSite {
    const LINT: &'static str = RAW_INPUT_MIGRATION;
    fn line(&self) -> usize {
        self.span.line_range.0
    }
    fn to_diagnostic(&self) -> CompilerError {
        CompilerError::new_with_label(
            self.span,
            format!(
                "raw input query `{}` needs an action, editor/tool, text-input, or deliberate-remainder classification",
                self.name
            ),
        )
        .with_level(CompilerErrorLevel::Advice)
    }
}

fn raw_input_class_marker(source: &str, line: usize) -> Option<&str> {
    let marker_line = source.lines().nth(line.saturating_sub(1))?.trim();
    let class = marker_line.strip_prefix("// action-layer:")?.trim();
    if class.is_empty() || class.split_whitespace().count() != 1 {
        return None;
    }
    RAW_INPUT_CLASSES.iter().copied().find(|&known| known == class)
}

#[derive(Clone, Debug)]
pub struct RawInputClassificationGap {
    pub name: Ustr,
    pub span: Span,
    pub line_population: usize,
}

pub fn unclassified_raw_input_sites_in_module(
    module: &Module,
    source: &str,
) -> Vec<RawInputClassificationGap> {
    let sites = raw_input_sites_in_module(module);
    let mut sites_per_line: FxHashMap<usize, usize> = FxHashMap::default();
    for site in &sites {
        *sites_per_line.entry(site.span.line_range.0).or_default() += 1;
    }
    sites
        .into_iter()
        .filter_map(|site| {
            let line_population = sites_per_line[&site.span.line_range.0];
            (line_population != 1
                || raw_input_class_marker(source, site.span.line_range.0).is_none())
            .then_some(RawInputClassificationGap {
                name: site.name,
                span: site.span,
                line_population,
            })
        })
        .collect()
}

impl LintFinding for RawInputClassificationGap {
    const LINT: &'static str = RAW_INPUT_CLASSIFICATION;
    fn line(&self) -> usize {
        self.span.line_range.0
    }
    fn to_diagnostic(&self) -> CompilerError {
        if self.line_population != 1 {
            return CompilerError::new_with_label(
                self.span,
                format!(
                    "raw input classification requires exactly one query per source line (found {})",
                    self.line_population
                ),
            )
            .with_level(CompilerErrorLevel::Advice);
        }
        CompilerError::new_with_label(
            self.span,
            format!(
                "raw input query `{}` needs an adjacent `// action-layer:` marker: {}",
                self.name,
                RAW_INPUT_CLASSES.join(", ")
            ),
        )
        .with_level(CompilerErrorLevel::Advice)
    }
}

/// A production Mag action declaration with no production caller. The source
/// span is the declaration itself so a missing consumer is named exactly.
#[derive(Clone, Debug)]
pub struct ActionCallerGap {
    pub action: Ustr,
    pub file: Ustr,
    pub span: Span,
    pub population_empty: bool,
}

pub const ACTION_CALLERS: &str = "action_layer_action_callers";

fn path_uses_component(file: Ustr, component: &str) -> bool {
    let path = file.as_str().replace('\\', "/");
    path == component
        || path.starts_with(&format!("{component}/"))
        || path.contains(&format!("/{component}/"))
}

fn is_test_module(file: Ustr) -> bool {
    let path = file.as_str().replace('\\', "/");
    path.starts_with('<')
        || path_uses_component(file, "tests")
        || path_uses_component(file, "test")
}

fn is_production_mag_module(file: Ustr) -> bool {
    path_uses_component(file, "mag") && !is_test_module(file)
}

fn is_action_name(name: Ustr) -> bool {
    name.as_str().starts_with("ACT_")
}

fn has_annotation(f: &Function, name: &str) -> bool {
    f.annotations.iter().any(|annotation| annotation.name.name.as_str() == name)
}

impl LintFinding for ActionCallerGap {
    const LINT: &'static str = ACTION_CALLERS;
    fn line(&self) -> usize {
        self.span.line_range.0
    }
    fn to_diagnostic(&self) -> CompilerError {
        let message = if self.population_empty {
            "no production Mag ACT_* declarations found".to_owned()
        } else {
            format!("production Mag action `{}` has no production caller", self.action)
        };
        CompilerError::new_with_label(self.span, message).with_level(CompilerErrorLevel::Advice)
    }
}

/// HIR-backed caller census for production Mag actions. Global declaration
/// initializers and `@wire` refresh bodies are registration, not callers.
pub fn action_caller_gaps(ws: &Workspace, warm: &FxHashSet<ModuleId>) -> Vec<ActionCallerGap> {
    let declarations: Vec<(Ustr, Ustr, Span)> = user_modules(ws, warm)
        .filter(|module| is_production_mag_module(module.file))
        .flat_map(|module| {
            module
                .globals
                .iter()
                .filter(|(_, global)| is_action_name(global.name))
                .map(move |(_, global)| (global.name, module.file, global.name_span))
        })
        .collect();

    if declarations.is_empty() {
        let Some(module) = user_modules(ws, warm).find(|module| is_production_mag_module(module.file))
        else {
            return Vec::new();
        };
        let Some((_, function)) = module.functions.iter().next() else {
            return Vec::new();
        };
        return vec![ActionCallerGap {
            action: Ustr::from("<none>"),
            file: module.file,
            span: function.span,
            population_empty: true,
        }];
    }

    let declared: FxHashSet<Ustr> = declarations.iter().map(|(name, _, _)| *name).collect();
    let mut callers: FxHashSet<Ustr> = FxHashSet::default();
    for module in user_modules(ws, warm).filter(|module| !is_test_module(module.file)) {
        for (owner, body) in &module.bodies {
            let production_read = match owner {
                BodyOwner::Function(fid) => {
                    let function = &module.functions[*fid];
                    !has_annotation(function, "wire")
                        && function.name != "refresh_action_defaults"
                        && function.name != "action_new"
                }
                BodyOwner::Global(gid) => !is_action_name(module.globals[*gid].name),
                _ => false,
            };
            if !production_read {
                continue;
            }
            for expr in body.expr_arena.values() {
                if let Expr::Global(name) = expr {
                    if declared.contains(name) {
                        callers.insert(*name);
                    }
                }
            }
        }
    }

    declarations
        .into_iter()
        .filter(|(name, _, _)| !callers.contains(name))
        .map(|(action, file, span)| ActionCallerGap {
            action,
            file,
            span,
            population_empty: false,
        })
        .collect()
}

/// Prefix-form declarations in one module that the receiver form
/// (`fn Type.name(recv, ..)`) expresses directly. The receiver form is pure
/// surface sugar -- it desugars to exactly this free fn with param 0 typed by
/// the path -- so every finding is a mechanical, signature-preserving rewrite,
/// which is what lets a project convert a file at a time.
///
/// `types` is the workspace's nominal type names (see [`nominal_type_names`]).
/// A candidate needs a param 0 whose type is one of them with NO type
/// arguments: that is the only shape the receiver path can spell, so a
/// container, nullable, tuple, builtin, or generic param 0 is never a
/// candidate. `extern` decls are excluded because the parser REJECTS a receiver
/// path on one.
pub fn method_syntax_in_module(
    ws: &Workspace,
    module: &Module,
    types: &FxHashSet<Ustr>,
    src: &str,
) -> Vec<MethodSyntax> {
    let mut out = Vec::new();
    for (_, f) in module.functions.iter() {
        if f.is_extern || f.name.as_str() == "main" || f.name_span.file != module.file {
            continue;
        }
        let Some(recv) = f.param_decls.first() else { continue };
        let Some(receiver) = nominal_name(ws, recv.ty) else { continue };
        if !types.contains(&receiver)
            || !spelled_at(src, f.name, f.name_span)
            || written_as_method(src, f.span, f.name_span)
        {
            continue;
        }
        out.push(MethodSyntax {
            name: f.name,
            receiver,
            params: rewritten_params(ws, f),
            span: f.name_span,
        });
    }
    out
}

/// `m, n: int` -- the receiver form's parameter list: param 0 loses its type
/// annotation (the receiver path carries it), the rest are unchanged.
///
/// A default's EXPRESSION is not in the signature (it lives in the body arena),
/// so a decl carrying one renders as `m, ..` instead: a suggestion that silently
/// dropped `= 3` reads as a complete new signature and is one paste away from
/// changing behavior.
fn rewritten_params(ws: &Workspace, f: &Function) -> String {
    if f.param_decls.iter().any(|p| p.has_default) {
        return format!("{}, ..", f.param_decls[0].name);
    }
    f.param_decls
        .iter()
        .enumerate()
        .map(|(i, p)| {
            if i == 0 {
                p.name.to_string()
            } else {
                format!("{}: {}", p.name, ws.render(None, p.ty))
            }
        })
        .join(", ")
}

/// The name of a bare nominal type (`Mob`), and `None` for anything a receiver
/// path cannot spell: a builtin, a generic param, a nullable, a tuple, a fn
/// type, or an APPLIED nominal (`Pair<int>` -- the path form takes the decl's
/// own generics, not concrete arguments).
fn nominal_name(ws: &Workspace, ty: TyId) -> Option<Ustr> {
    let te = match ty {
        TyId::Static(m, i) => ws.module(m).ty_arena[i],
        // A decl's param types are lowered into the module arena; a body-local
        // type here would be a different question than the one being asked.
        TyId::Local(_) => return None,
    };
    match te {
        TypeExpr::App { ctor: TypeCtor::Nominal(name), params } if params.len() == 0 => Some(name),
        _ => None,
    }
}

/// Whether the source at `name_span` spells this function's name -- i.e. the
/// author wrote this declaration, here. A SYNTHESIZED decl fails it: a
/// `newtype Hp(int)` mints ten operator fns (`__op_add(a: Hp, b: Hp)`) whose
/// name span covers the newtype's name, and every one of them is a
/// receiver-shaped signature nobody can rewrite. Same net catches a
/// macro-expanded decl, whose span is retagged to the call site.
fn spelled_at(src: &str, name: Ustr, name_span: Span) -> bool {
    let (a, b) = name_span.offset_range;
    if b > src.len() || a > b || !src.is_char_boundary(a) || !src.is_char_boundary(b) {
        return false;
    }
    &src[a..b] == name.as_str()
}

/// Whether the decl was WRITTEN in receiver form. Both forms lower to the same
/// HIR -- that is the point of the sugar -- so the surface is the only
/// discriminant, and without it the lint reports every already-converted fn.
/// `fn Mob.damage` puts a `.` immediately before the name; the prefix form
/// cannot, since the only thing between `fn` and the name is whitespace.
fn written_as_method(src: &str, fn_span: Span, name_span: Span) -> bool {
    let (start, end) = (fn_span.offset_range.0, name_span.offset_range.0);
    if end > src.len() || start > end {
        return false;
    }
    if !src.is_char_boundary(start) || !src.is_char_boundary(end) {
        return false;
    }
    src[start..end].trim_end().ends_with('.')
}

/// Every nominal type name declared in a user module: structs, enums, and
/// newtypes. `Vec`/`Map` are not here -- they lower to their own type
/// constructors, never `Nominal` -- so a container param can't reach the lint
/// through this set either.
/// The names in `names` that no user module declares as a type. An ignore list
/// fails OPEN -- a misspelled entry filters nothing and the type keeps showing
/// up, which reads as the flag not working -- so the caller names them.
pub fn unknown_type_names(
    ws: &Workspace,
    warm: &FxHashSet<ModuleId>,
    names: &[Ustr],
) -> Vec<Ustr> {
    let known = nominal_type_names(ws, warm);
    names.iter().copied().filter(|n| !known.contains(n)).collect()
}

/// How a row place left the scope its arena's lifetime governs. One kind per
/// storage class the scoped-place rules in `docs/scene-system-plan.md` list, so
/// a census can report the shape of the escape population and not just its size.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EscapeKind {
    StoreGlobal,
    StoreStatic,
    StoreField,
    StoreCollection,
    Return,
    ClosureCapture,
    AnyErasure,
    AcrossYield,
    FfiArg,
}

/// How much a kind actually claims. A row's validity runs from the `get()`
/// until something invalidates the slot -- `remove`, `clear`, Scene unload --
/// so what matters is whether the row is HELD across such a point, not which
/// direction the reference travelled.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EscapeClass {
    /// The row outlives an invalidation point unconditionally. A defect.
    Hazard,
    /// A hazard only if the consumer RETAINS the row. Nothing here computes a
    /// retain summary, so these are reported without the claim: a lambda
    /// consumed synchronously by `any(..)` and one stashed in a Scene closer
    /// are the same shape to this lint.
    Unprovable,
    /// Not a hazard. The row moved somewhere still governed by the program's
    /// own rules -- the escape question simply moves with it. Reported because
    /// it is where an intraprocedural check STOPS, which is what bounds the
    /// census, not because the code is wrong.
    Propagation,
}

impl EscapeKind {
    pub fn class(self) -> EscapeClass {
        match self {
            EscapeKind::StoreGlobal
            | EscapeKind::StoreStatic
            | EscapeKind::StoreField
            | EscapeKind::StoreCollection
            | EscapeKind::AcrossYield => EscapeClass::Hazard,
            EscapeKind::ClosureCapture | EscapeKind::FfiArg | EscapeKind::AnyErasure => {
                EscapeClass::Unprovable
            }
            // A `return` runs nothing, so it cannot invalidate the slot: what
            // crosses it is the reference, never the validity window. The
            // accessor shape (`wall_at`, `Sound.data`) is CORRECT code.
            EscapeKind::Return => EscapeClass::Propagation,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            EscapeKind::StoreGlobal => "store_global",
            EscapeKind::StoreStatic => "store_static",
            EscapeKind::StoreField => "store_field",
            EscapeKind::StoreCollection => "store_collection",
            EscapeKind::Return => "return",
            EscapeKind::ClosureCapture => "closure_capture",
            EscapeKind::AnyErasure => "any_erasure",
            EscapeKind::AcrossYield => "across_yield",
            EscapeKind::FfiArg => "ffi_arg",
        }
    }
}

/// A `row_escape` finding: a row place -- a `T` read out of an `Arena<T>` or a
/// `Scene<T>` root -- reaching storage that outlives the borrow.
#[derive(Clone, Debug)]
pub struct RowEscape {
    pub kind: EscapeKind,
    /// Rendered type of the escaping row (`Mob`, `TrActor`).
    pub row_type: String,
    pub span: Span,
}

/// Canonical lint name for suppression: `// rock: allow(row_escape)`.
pub const ROW_ESCAPE: &str = "row_escape";

impl LintFinding for RowEscape {
    const LINT: &'static str = ROW_ESCAPE;
    fn line(&self) -> usize {
        self.span.line_range.0
    }
    fn to_diagnostic(&self) -> CompilerError {
        let (ty, kind) = (&self.row_type, self.kind.as_str());
        let msg = match self.kind.class() {
            EscapeClass::Hazard => format!(
                "row of `{ty}` is held past an invalidation point here ({kind}) -- \
                 the slot can be removed or the Scene unloaded before the next read; \
                 hold the `Entity` and re-`get()` instead"
            ),
            EscapeClass::Unprovable => format!(
                "row of `{ty}` leaves the checked region here ({kind}) -- safe only if \
                 the consumer does not retain it, which nothing here proves"
            ),
            EscapeClass::Propagation => format!(
                "row of `{ty}` is returned here -- not a defect: a `return` invalidates \
                 nothing. The row becomes the CALLER's to account for, and this check \
                 does not cross the boundary, so audit the callers"
            ),
        };
        CompilerError::new_with_label(self.span, msg).with_level(CompilerErrorLevel::Advice)
    }
}

mod row_escape;
pub use row_escape::{row_escapes_in_body, row_population, RowPopulation};

/// Where a body's TRAILING expression ends up, so `row_escape` can tell a
/// returned row from a discarded one. A unit function's tail goes nowhere --
/// `fn unload_sound(s: Sound) { s.handle.remove() }` drops the `T?` it produced.
fn tail_sink(module: &Module, owner: &BodyOwner) -> Option<EscapeKind> {
    match owner {
        BodyOwner::Function(fid) => {
            module.functions[*fid].return_ty.map(|_| EscapeKind::Return)
        }
        BodyOwner::Global(_) => Some(EscapeKind::StoreGlobal),
        // A struct default / enum / newtype body's value is stored in the
        // aggregate it belongs to.
        _ => Some(EscapeKind::StoreField),
    }
}

/// `row_escape` findings across every non-stdlib body in the workspace.
pub fn row_escapes(ws: &Workspace) -> Vec<RowEscape> {
    let warm = FxHashSet::default();
    let mut out = Vec::new();
    for module in user_modules(ws, &warm) {
        let inf = ws.inference(module.id);
        for (owner, body) in module.bodies.iter() {
            let bi = inf.and_then(|i| i.per_fn.get(owner));
            out.extend(row_escapes_in_body(ws, bi, body, tail_sink(module, owner)));
        }
    }
    out
}

pub fn nominal_type_names(ws: &Workspace, warm: &FxHashSet<ModuleId>) -> FxHashSet<Ustr> {
    let mut out = FxHashSet::default();
    for m in user_modules(ws, warm) {
        out.extend(m.struct_by_name.keys().copied());
        out.extend(m.enums_by_name.keys().copied());
        out.extend(m.newtypes_by_name.keys().copied());
    }
    out
}

/// A `task_null_abandon` finding: a plain `= null` over a `Task?` place.
/// `place` is the rendered target (`m.co`), which the diagnostic names so the
/// fix (`stop(m, .co)`) is spellable from the message alone.
#[derive(Clone, Debug)]
pub struct TaskNullAbandon {
    pub place: Option<String>,
    pub span: Span,
}

/// Canonical lint name for suppression: `// rock: allow(task_null_abandon)`.
pub const TASK_NULL_ABANDON: &str = "task_null_abandon";

impl LintFinding for TaskNullAbandon {
    const LINT: &'static str = TASK_NULL_ABANDON;
    fn line(&self) -> usize {
        self.span.line_range.0
    }
    fn to_diagnostic(&self) -> CompilerError {
        let what = match &self.place {
            Some(p) => format!("`{p} = null`"),
            None => "assigning `null` to a `Task?` slot".to_string(),
        };
        CompilerError::new_with_label(
            self.span,
            format!(
                "{what} abandons the coroutine mid-stack -- it is dropped where it stands, \
                 so its terminal `defer`s never run; use `stop` to cancel it (or `replace` \
                 to swap one in), which kill the coroutine first"
            ),
        )
        // `Warning`, not `Error`: nulling a slot just observed terminal, and
        // cleanup inside the task's own terminal defer, are both legal and
        // indistinguishable without liveness -- so the allow comment must stay
        // reachable.
        .with_level(CompilerErrorLevel::Warning)
    }
}

/// `task_null_abandon` for one body. A flat scan, not a walk: v1 asks only
/// "does a `= null` land on a `Task?` place", and every such assignment is
/// judged the same wherever it sits.
///
/// Deliberately NOT covered: overwriting a live slot with a *new* task
/// (`m.co = start(...)`, which `replace` exists for). Distinguishing that from
/// the first store into an empty slot needs liveness-at-assignment, and a false
/// positive on every initial store would be worse than the miss. The two LEGAL
/// null-assign shapes -- nulling a task just observed terminal, and completion
/// cleanup inside the task's own terminal defer -- are equally invisible here,
/// which is why the lint reports at `Warning` with the allow comment legal.
pub fn task_null_abandon_in_body(
    ws: &Workspace,
    bi: Option<&BodyInference>,
    body: &Body,
) -> Vec<TaskNullAbandon> {
    let Some(bi) = bi else { return Vec::new() };
    let mut out = Vec::new();
    for (_, expr) in body.expr_arena.iter() {
        let Expr::Assign { target, value } = *expr else { continue };
        if !matches!(body.expr_arena[value], Expr::Null) {
            continue;
        }
        let Some(ty) = place_type(bi, body, target) else { continue };
        if !is_task_slot(ws, bi, ty) {
            continue;
        }
        out.push(TaskNullAbandon {
            place: render_place(body, target),
            span: body.expr_spans[target],
        });
    }
    out
}

/// The type of an assignment TARGET. A bare local target carries no
/// `expr_types` entry -- a `Var` is a reference to a binding, and the binding
/// arena is where its type lives -- so the two shapes read different tables.
fn place_type(bi: &BodyInference, body: &Body, target: Idx<Expr>) -> Option<TyId> {
    match body.expr_arena[target] {
        Expr::Var(b) => bi.binding_types.get(b).copied(),
        _ => bi.expr_types.get(target).copied(),
    }
}

/// A `Task<R>?` place. The `?` is required: a non-nullable `Task` field cannot
/// take a `null` at all, so bone has already rejected that program.
fn is_task_slot(ws: &Workspace, bi: &BodyInference, ty: TyId) -> bool {
    let (id, TypeExpr::Nullable(inner)) = head_ty(ws, bi, ty) else { return false };
    matches!(
        head_type(ws, bi, id.with_idx(inner)),
        TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } if n.as_str() == "Task"
    )
}

/// Re-spell an assignment target for the diagnostic (`m.co`, `e.get()?.co`,
/// `SLOT`). `None` for a shape not covered here, which the diagnostic phrases
/// around rather than naming wrongly -- the span already points at the place.
fn render_place(body: &Body, e: Idx<Expr>) -> Option<String> {
    Some(match body.expr_arena[e] {
        Expr::Var(b) => body.binding_arena[b].name.to_string(),
        Expr::Global(n) => n.to_string(),
        Expr::FieldAccess { base, field } => {
            format!("{}.{}", render_place(body, base)?, field)
        }
        Expr::SafeNav { base, field } => format!("{}?.{}", render_place(body, base)?, field),
        Expr::SafeNavCall { receiver, method, .. } => {
            format!("{}?.{}()", render_place(body, receiver)?, method)
        }
        // UFCS: `e.get()` lowers to a call whose first arg is the receiver.
        Expr::Call { callee: Callee::Unresolved { name, .. }, args, .. } => {
            let recv = args.into_iter().next()?;
            format!("{}.{}()", render_place(body, recv)?, name)
        }
        _ => return None,
    })
}

/// `task_null_abandon` findings across every non-stdlib body in the workspace.
/// Convenience for tests and whole-workspace CLI runs; the surfaces go through
/// [`inference_findings`].
pub fn task_null_abandon(ws: &Workspace) -> Vec<TaskNullAbandon> {
    let warm = FxHashSet::default();
    let mut out = Vec::new();
    for module in user_modules(ws, &warm) {
        let inf = ws.inference(module.id);
        for (owner, body) in module.bodies.iter() {
            out.extend(task_null_abandon_in_body(ws, inf.and_then(|i| i.per_fn.get(owner)), body));
        }
    }
    out
}
