use bone_core::hir::{
    AnnoValue, BinOp, BindingInfo, BindingKind, BodyOwner, Callee, Expr, Function, Module,
    Statement, TyId, TypeCtor, TypeExpr,
};
use bone_core::prelude::*;
use bone_core::workspace::{BodyInference, BoundNext, BoundTree};
use rock_core::errors::CompilerErrorLevel;

mod call_lints;
pub use call_lints::DEPRECATED;
use call_lints::deprecated_calls_in_body;

fn unused_vars_in_body_with_metadata(
    body: &Body,
    variants: &FxHashSet<Ustr>,
    metadata_used: &FxHashSet<Idx<BindingInfo>>,
) -> Vec<Finding> {
    let mut read = read_bindings(body);
    read.extend(metadata_used);
    candidate_bindings(body, variants)
        .filter(|idx| !read.contains(idx))
        .map(|idx| &body.binding_arena[idx])
        .filter(|b| !b.name.as_str().starts_with('_'))
        .map(|b| {
            Finding::warning(
                UNUSED_VAR,
                b.span,
                format!("unused variable `{}` (prefix with `_` to silence)", b.name),
            )
        })
        .collect()
}

fn decorator_metadata_used_bindings(
    module: &Module,
    owner: BodyOwner,
    body: &Body,
) -> FxHashSet<Idx<BindingInfo>> {
    let BodyOwner::Decorator(did) = owner else {
        return FxHashSet::default();
    };
    let decorator = &module.decorators[did];
    let mut names = FxHashSet::default();
    for annotation in &decorator.annotations {
        let metadata_arg = match annotation.name.name.as_str() {
            "artifact_definition" => "key_param",
            "lifecycle" => "stage_param",
            _ => continue,
        };
        if let Some(AnnoValue::String(name)) = annotation.args.iter().find_map(|arg| {
            (arg.name
                .as_ref()
                .is_some_and(|arg_name| arg_name.name.as_str() == metadata_arg))
            .then_some(&arg.value)
        }) {
            names.insert(*name);
        }
    }
    body.param_bindings
        .into_iter()
        .filter(|idx| names.contains(&body.binding_arena[*idx].name))
        .collect()
}

/// Let bindings and the enclosing function's parameters, excluding enum-variant
/// pattern names, lambda parameters, and parameters of opaque bodies.
fn candidate_bindings<'a>(
    body: &'a Body,
    variants: &'a FxHashSet<Ustr>,
) -> impl Iterator<Item = Idx<BindingInfo>> + 'a {
    let params = body.root.map(|_| body.param_bindings);
    body.binding_arena
        .iter()
        .filter(|(_, b)| b.kind == BindingKind::Let && !variants.contains(&b.name))
        .map(|(idx, _)| idx)
        .chain(params.into_iter().flatten())
}

pub(crate) fn enum_variant_names(ws: &Workspace, warm: &FxHashSet<ModuleId>) -> FxHashSet<Ustr> {
    ws.user_modules(warm)
        .flat_map(|m| m.enums.iter())
        .flat_map(|(_, e)| e.variants.iter().map(|v| v.name))
        .collect()
}

/// Bindings read in the body. Plain assignment targets are writes; compound
/// assignments and inc/dec read their targets.
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
            Expr::Call {
                callee: Callee::Local(b),
                ..
            } => {
                read.insert(*b);
            }
            _ => {}
        }
    }
    read
}

/// Expression indices that are plain assignment targets. Tuple targets recurse;
/// field and index targets still read their receivers.
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

pub const UNUSED_VAR: &str = "unused_var";

pub const KNOWN_LINTS: &[&str] = &[
    UNUSED_VAR,
    UNUSED_FN,
    UNUSED_PURE_EXPR,
    TRANSMUTE_OF_NULLABLE,
    TASK_NULL_ABANDON,
    DEPRECATED,
    RAW_INPUT_MIGRATION,
    RAW_INPUT_CLASSIFICATION,
    ACTION_CALLERS,
];

/// Lints excluded from the default selection. These often produce broad
/// migration or audit findings and must be selected explicitly.
pub const OPT_IN_LINTS: &[&str] = &[
    RAW_INPUT_MIGRATION,
    RAW_INPUT_CLASSIFICATION,
    ACTION_CALLERS,
];

pub use rock_host::stick::ActionLayerPolicy;

/// Selected lints and their options. The default excludes [`OPT_IN_LINTS`];
/// [`LintOptions::only`] runs exactly the named set. Disabled analyses do not run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LintOptions {
    action_policy: Option<std::sync::Arc<ActionLayerPolicy>>,
    only: Option<Vec<String>>,
}

impl LintOptions {
    /// Supply the caller's action policy, or explicitly select the plain build.
    pub fn with_action_policy(mut self, policy: Option<&ActionLayerPolicy>) -> Self {
        self.action_policy = policy.cloned().map(std::sync::Arc::new);
        self
    }

    /// Run exactly `names`, retaining unknown names for [`LintOptions::unknown`].
    pub fn only(names: impl IntoIterator<Item = String>) -> Self {
        LintOptions {
            only: Some(names.into_iter().collect()),
            ..Default::default()
        }
    }

    pub fn enabled(&self, lint: &str) -> bool {
        match &self.only {
            Some(names) => names.iter().any(|n| n == lint),
            None => !OPT_IN_LINTS.contains(&lint),
        }
    }

    /// The selected names that name no lint in the catalog.
    pub fn unknown(&self) -> Vec<&str> {
        let Some(names) = &self.only else {
            return Vec::new();
        };
        names
            .iter()
            .map(String::as_str)
            .filter(|n| !KNOWN_LINTS.contains(n))
            .collect()
    }
}

#[derive(Clone, Debug)]
pub struct Finding {
    pub lint: &'static str,
    pub error: CompilerError,
    pub line: usize,
}

impl Finding {
    fn new(lint: &'static str, level: CompilerErrorLevel, span: Span, message: String) -> Self {
        Self {
            lint,
            error: CompilerError::new_with_label(span, message).with_level(level),
            line: span.line_range().0,
        }
    }

    fn warning(lint: &'static str, span: Span, message: String) -> Self {
        Self::new(lint, CompilerErrorLevel::Warning, span, message)
    }

    fn advice(lint: &'static str, span: Span, message: String) -> Self {
        Self::new(lint, CompilerErrorLevel::Advice, span, message)
    }

    pub fn level(&self) -> CompilerErrorLevel {
        self.error.level
    }

    pub fn file(&self) -> Ustr {
        self.error.location.file
    }
}

/// What changed since the previous [`IncrementalLintCache::update`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LintCacheUpdate {
    pub full: bool,
    /// Changed files were lowered again, so their function indices may differ.
    pub relowered: bool,
    pub changed_files: FxHashSet<Ustr>,
    pub reinferred: Vec<(ModuleId, BodyOwner)>,
}

impl LintCacheUpdate {
    pub fn body_only(
        changed_files: impl IntoIterator<Item = Ustr>,
        reinferred: impl IntoIterator<Item = (ModuleId, BodyOwner)>,
    ) -> Self {
        Self {
            changed_files: changed_files.into_iter().collect(),
            reinferred: reinferred.into_iter().collect(),
            ..Default::default()
        }
    }

    pub fn full() -> Self {
        Self {
            full: true,
            ..Default::default()
        }
    }

    /// An interface or scope edit that re-lowered `changed_files` and
    /// re-inferred `reinferred`. Module ids must be unchanged; otherwise rebuild.
    pub fn relowered(
        changed_files: impl IntoIterator<Item = Ustr>,
        reinferred: impl IntoIterator<Item = (ModuleId, BodyOwner)>,
    ) -> Self {
        Self {
            relowered: true,
            ..Self::body_only(changed_files, reinferred)
        }
    }
}

type FunctionKey = (ModuleId, Idx<Function>);

/// One body's unsuppressed findings and, for `unused_fn`, what it references.
#[derive(Clone, Debug, Default)]
struct BodyLints {
    findings: Vec<Finding>,
    calls: Vec<FunctionKey>,
    /// Function names referenced as values; resolved per update because a
    /// declaration elsewhere can change what they name.
    names: Vec<Ustr>,
}

#[derive(Clone, Debug)]
struct FileLints {
    module: ModuleId,
    bodies: FxHashMap<BodyOwner, BodyLints>,
    /// Findings no body owns.
    other: Vec<Finding>,
    suppressions: FxHashSet<(usize, &'static str)>,
}

/// Inputs every cached entry depends on; any change rebuilds the cache.
#[derive(Clone, Debug, PartialEq, Eq)]
struct CacheKey {
    is_bin: bool,
    warm: FxHashSet<ModuleId>,
    variants: FxHashSet<Ustr>,
    workspace_files: FxHashSet<Ustr>,
    report_files: FxHashSet<Ustr>,
    selection: LintOptions,
}

impl CacheKey {
    fn tracks_calls(&self) -> bool {
        self.is_bin && self.selection.enabled(UNUSED_FN)
    }
}

/// Per-body lint memo. Changed files and reinferred bodies are derived again;
/// workspace lints are recomputed when what bodies reference changes.
/// Reporting policy stays in the CLI/LSP adapters.
#[derive(Clone, Debug, Default)]
pub struct IncrementalLintCache {
    key: Option<CacheKey>,
    files: FxHashMap<Ustr, FileLints>,
    reachable: Option<FxHashSet<FunctionKey>>,
    /// Workspace-lint findings by file.
    workspace: FxHashMap<Ustr, Vec<Finding>>,
    /// Files reported as empty until they change again.
    withheld: FxHashSet<Ustr>,
    output: FxHashMap<Ustr, Vec<Finding>>,
}

impl IncrementalLintCache {
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Report nothing for `file` until an update changes it.
    pub fn invalidate_file(&mut self, file: Ustr) {
        self.withheld.insert(file);
        self.output.remove(&file);
    }

    pub fn findings_for_file(&self, file: Ustr) -> Vec<Finding> {
        self.output.get(&file).cloned().unwrap_or_default()
    }

    pub fn update(
        &mut self,
        ws: &Workspace,
        warm: &FxHashSet<ModuleId>,
        is_bin: bool,
        sources: &FxHashMap<Ustr, &str>,
        sel: &LintOptions,
        change: &LintCacheUpdate,
    ) -> Vec<Finding> {
        let key = CacheKey {
            is_bin,
            warm: warm.clone(),
            variants: enum_variant_names(ws, warm),
            workspace_files: ws.source_modules().map(|m| m.file).collect(),
            report_files: sources.keys().copied().collect(),
            selection: sel.clone(),
        };
        let module_of = |file: &Ustr| ws.modules.iter().find(|m| m.file == *file).map(|m| m.id);
        let renumbered = change.relowered
            && change.changed_files.iter().any(|file| {
                self.files
                    .get(file)
                    .is_some_and(|cached| module_of(file) != Some(cached.module))
            });
        let mut stale_bodies: FxHashSet<(ModuleId, BodyOwner)> = FxHashSet::default();
        let mut refresh_all = change.full || renumbered || self.key.as_ref() != Some(&key);
        if refresh_all {
            self.clear();
        } else {
            for file in &change.changed_files {
                self.withheld.remove(file);
            }
            stale_bodies.extend(change.reinferred.iter().copied());
            if change.relowered {
                refresh_all = true;
                let changed: FxHashSet<ModuleId> = change
                    .changed_files
                    .iter()
                    .filter_map(|file| self.files.get(file).map(|cached| cached.module))
                    .collect();
                for cached in self.files.values() {
                    for (&owner, body) in &cached.bodies {
                        if body.calls.iter().any(|(mid, _)| changed.contains(mid)) {
                            stale_bodies.insert((cached.module, owner));
                        }
                    }
                }
            }
        }

        let stale_modules: FxHashSet<ModuleId> = stale_bodies.iter().map(|&(m, _)| m).collect();
        let todo: Vec<&Module> = ws
            .user_modules(warm)
            .filter(|m| {
                !self.files.contains_key(&m.file)
                    || change.changed_files.contains(&m.file)
                    || stale_modules.contains(&m.id)
            })
            .collect();
        let todo: Vec<_> = todo
            .into_iter()
            .map(|m| (m, self.files.remove(&m.file)))
            .collect();
        let derived: Vec<_> = todo
            .into_par_iter()
            .map(|(module, old)| {
                let source = sources.get(&module.file).copied();
                let reuse = !change.changed_files.contains(&module.file);
                file_lints(ws, module, source, old, reuse, &stale_bodies, &key)
            })
            .collect();
        let mut rederived = Vec::with_capacity(derived.len());
        for (file, refs_changed) in derived {
            refresh_all |= refs_changed;
            rederived.push(file.module);
            self.files.insert(ws.module(file.module).file, file);
        }

        refresh_all |= sel.enabled(ACTION_CALLERS) && sel.action_policy.is_some();
        if refresh_all {
            self.reachable = key
                .tracks_calls()
                .then(|| reachable_functions(ws, warm, &self.files));
            self.workspace = workspace_findings_by_file(ws, warm, sel, self.reachable.as_ref());
        } else if let Some(reachable) = &self.reachable {
            for &mid in &rederived {
                let module = ws.module(mid);
                match unused_fns_in(module, reachable) {
                    found if found.is_empty() => self.workspace.remove(&module.file),
                    found => self.workspace.insert(module.file, found),
                };
            }
        }

        let reported = || {
            ws.user_modules(warm)
                .filter(|module| sources.contains_key(&module.file))
        };
        let advice = reported()
            .next()
            .and_then(|module| missing_policy_advice(module, sel));
        let rederived: FxHashSet<ModuleId> = rederived.into_iter().collect();
        for module in reported().filter(|m| refresh_all || rederived.contains(&m.id)) {
            let cached = &self.files[&module.file];
            let mut found: Vec<Finding> = cached
                .bodies
                .values()
                .flat_map(|body| &body.findings)
                .chain(&cached.other)
                .chain(self.workspace.get(&module.file).into_iter().flatten())
                .filter(|finding| !is_suppressed(finding, &cached.suppressions))
                .chain(advice.iter().filter(|a| a.file() == module.file))
                .cloned()
                .collect();
            found.sort_by(cmp_by_offset);
            if found.is_empty() || self.withheld.contains(&module.file) {
                self.output.remove(&module.file);
            } else {
                self.output.insert(module.file, found);
            }
        }
        self.key = Some(key);
        reported()
            .filter_map(|module| self.output.get(&module.file))
            .flatten()
            .cloned()
            .collect()
    }
}

/// One file's entries, reusing `old` bodies unless the file changed or the
/// body is stale. Also reports whether what the file references changed.
fn file_lints(
    ws: &Workspace,
    module: &Module,
    source: Option<&str>,
    old: Option<FileLints>,
    reuse: bool,
    stale: &FxHashSet<(ModuleId, BodyOwner)>,
    key: &CacheKey,
) -> (FileLints, bool) {
    let sel = &key.selection;
    let (mut old_bodies, kept) = match old {
        Some(file) if reuse => (file.bodies, Some((file.other, file.suppressions))),
        Some(file) => (file.bodies, None),
        None => (FxHashMap::default(), None),
    };
    let (other, suppressions) = kept.unwrap_or_else(|| {
        let mut other = Vec::new();
        let migration = sel.enabled(RAW_INPUT_MIGRATION);
        let classification = sel.enabled(RAW_INPUT_CLASSIFICATION);
        if let Some(src) = source
            && (migration || classification)
            && let Some(policy) = sel.action_policy.as_deref()
        {
            raw_input_findings(module, src, policy, migration, classification, &mut other);
        }
        (other, source.map(suppression_entries).unwrap_or_default())
    });
    let mut refs_changed = false;
    let bodies = module
        .bodies
        .iter()
        .map(|(&owner, body)| {
            let previous = old_bodies.remove(&owner);
            if reuse
                && !stale.contains(&(module.id, owner))
                && let Some(previous) = previous
            {
                return (owner, previous);
            }
            let mut lints = BodyLints::default();
            if source.is_some() {
                lints.findings = flat_body_findings(module, owner, body, &key.variants, sel);
                lints
                    .findings
                    .extend(inference_body_findings(ws, module, owner, body, sel));
            }
            if key.tracks_calls() {
                body_references(ws, module.id, owner, body, &mut lints);
                refs_changed |=
                    previous.is_none_or(|p| p.calls != lints.calls || p.names != lints.names);
            }
            (owner, lints)
        })
        .collect();
    refs_changed |= key.tracks_calls() && !old_bodies.is_empty();
    let file = FileLints {
        module: module.id,
        bodies,
        other,
        suppressions,
    };
    (file, refs_changed)
}

fn finding_offset(finding: &Finding) -> usize {
    finding
        .error
        .labels
        .first()
        .map(|label| label.span.offset_range().0)
        .unwrap_or(finding.error.location.offset)
}

fn cmp_by_offset(a: &Finding, b: &Finding) -> std::cmp::Ordering {
    finding_offset(a)
        .cmp(&finding_offset(b))
        .then_with(|| a.lint.cmp(b.lint))
        .then_with(|| a.error.message.cmp(&b.error.message))
        .then_with(|| {
            fn label_key(label: &rock_core::errors::CompilerLabel) -> impl Ord + '_ {
                (
                    label.span.file.as_str(),
                    label.span.offset_range(),
                    label.span.line_range(),
                    label.span.col_range(),
                    label.message.as_str(),
                )
            }
            a.error
                .labels
                .iter()
                .map(label_key)
                .cmp(b.error.labels.iter().map(label_key))
        })
}

fn allow_lines(source: &str) -> impl Iterator<Item = (usize, &str)> {
    let mut line_no = 0;
    let mut counted_to = 0;
    let mut next_from = 0;
    source.match_indices("allow").filter_map(move |(at, _)| {
        if at < next_from {
            return None;
        }
        let start = source[..at].rfind('\n').map_or(0, |i| i + 1);
        let newline = source[at..].find('\n').map(|i| at + i);
        let end = newline.unwrap_or(source.len());
        line_no += source.as_bytes()[counted_to..start]
            .iter()
            .filter(|&&b| b == b'\n')
            .count();
        counted_to = start;
        next_from = end;
        let line = &source[start..end];
        // `str::lines` strips a carriage return only before a line feed.
        let line = match newline {
            Some(_) => line.strip_suffix('\r').unwrap_or(line),
            None => line,
        };
        Some((line_no, line))
    })
}

fn missing_policy_advice(module: &Module, sel: &LintOptions) -> Option<Finding> {
    if sel.action_policy.is_some() {
        return None;
    }
    [
        ACTION_CALLERS,
        RAW_INPUT_MIGRATION,
        RAW_INPUT_CLASSIFICATION,
    ]
    .into_iter()
    .find(|lint| sel.enabled(lint))
    .map(|lint| {
        Finding::advice(
            lint,
            Span::new(module.file, (0, 0), (0, 0), (0, 0)),
            "no action-layer policy configured".to_owned(),
        )
    })
}

/// Findings from lints that read one body's HIR without inference. Results are
/// unsuppressed; callers apply the file's allow comments.
fn flat_body_findings(
    module: &Module,
    owner: BodyOwner,
    body: &Body,
    variants: &FxHashSet<Ustr>,
    sel: &LintOptions,
) -> Vec<Finding> {
    let mut out = Vec::new();
    if sel.enabled(UNUSED_VAR) {
        let metadata_used = decorator_metadata_used_bindings(module, owner, body);
        let found = unused_vars_in_body_with_metadata(body, variants, &metadata_used);
        out.extend(found);
    }
    if sel.enabled(UNUSED_PURE_EXPR) {
        out.extend(unused_pure_exprs_in_body(body));
    }
    out
}

fn inference_body_findings(
    ws: &Workspace,
    module: &Module,
    owner: BodyOwner,
    body: &Body,
    sel: &LintOptions,
) -> Vec<Finding> {
    let bi = ws.inference(module.id).and_then(|i| i.per_fn.get(&owner));
    let mut out = Vec::new();
    if sel.enabled(TRANSMUTE_OF_NULLABLE) {
        let found = transmute_of_nullable_in_body(ws, bi, body);
        out.extend(found);
    }
    if sel.enabled(TASK_NULL_ABANDON) {
        let found = task_null_abandon_in_body(ws, bi, body);
        out.extend(found);
    }
    if sel.enabled(DEPRECATED) {
        out.extend(deprecated_calls_in_body(ws, bi, body));
    }
    out
}

fn workspace_findings_by_file(
    ws: &Workspace,
    warm: &FxHashSet<ModuleId>,
    sel: &LintOptions,
    reachable: Option<&FxHashSet<FunctionKey>>,
) -> FxHashMap<Ustr, Vec<Finding>> {
    let mut out = reachable
        .map(|reachable| unused_fns_by_file(ws, warm, reachable))
        .unwrap_or_default();
    if sel.enabled(ACTION_CALLERS)
        && let Some(policy) = sel.action_policy.as_deref()
    {
        for (file, finding) in action_caller_gaps(ws, warm, policy) {
            out.entry(file).or_default().push(finding);
        }
    }
    out
}

/// Run the selected lints and report findings for `sources`, whose text drives
/// suppression. Other non-warm modules are inputs to workspace lints only.
pub fn lint_workspace(
    ws: &Workspace,
    warm: &FxHashSet<ModuleId>,
    is_bin: bool,
    sources: &FxHashMap<Ustr, &str>,
    sel: &LintOptions,
) -> Vec<Finding> {
    IncrementalLintCache::default().update(ws, warm, is_bin, sources, sel, &LintCacheUpdate::full())
}

/// Unknown lint names do not match.
fn suppression_entries(source: &str) -> FxHashSet<(usize, &'static str)> {
    let mut allow = FxHashSet::default();
    for (n, line) in allow_lines(source) {
        let mut rest = line;
        while let Some(pos) = find_comment_start(rest) {
            let after = &rest[pos + 2..];
            if let Some(args) = allow_comment_args(after) {
                for &lint in KNOWN_LINTS {
                    if allow_args_name(args, lint) {
                        allow.insert((n, lint));
                    }
                }
            }
            rest = after;
        }
    }
    allow
}

fn is_suppressed(finding: &Finding, allow: &FxHashSet<(usize, &'static str)>) -> bool {
    allow.contains(&(finding.line, finding.lint))
        || (finding.line > 0 && allow.contains(&(finding.line - 1, finding.lint)))
}

fn find_comment_start(s: &str) -> Option<usize> {
    s.as_bytes().windows(2).position(|pair| pair == b"//")
}

fn allow_comment_args(s: &str) -> Option<&str> {
    let s = s.trim_start().strip_prefix("rock")?;
    let s = s.trim_start().strip_prefix(':')?;
    let s = s.trim_start().strip_prefix("allow")?;
    let s = s.trim_start().strip_prefix('(')?;
    Some(s.trim_start())
}

fn allow_args_name(args: &str, lint_name: &str) -> bool {
    let Some(rest) = args.strip_prefix(lint_name) else {
        return false;
    };
    matches!(
        rest.chars().next(),
        None | Some(' ') | Some('\t') | Some(',') | Some(')')
    )
}

pub const UNUSED_FN: &str = "unused_fn";

fn unused_fn(function: &Function) -> Finding {
    Finding::warning(
        UNUSED_FN,
        function.name_span,
        format!(
            "unused function `{}`",
            declared_name(function.name.as_str())
        ),
    )
}

/// Report unreachable user functions by source file. Roots include `main`,
/// JSON callbacks, and functions with non-implementation annotations. This
/// closed-world lint is for bins and favors missed warnings over false flags.
fn unused_fns_by_file(
    ws: &Workspace,
    warm: &FxHashSet<ModuleId>,
    reachable: &FxHashSet<FunctionKey>,
) -> FxHashMap<Ustr, Vec<Finding>> {
    ws.user_modules(warm)
        .map(|module| (module.file, unused_fns_in(module, reachable)))
        .filter(|(_, found)| !found.is_empty())
        .collect()
}

fn unused_fns_in(module: &Module, reachable: &FxHashSet<FunctionKey>) -> Vec<Finding> {
    module
        .functions
        .iter()
        .filter(|&(fid, function)| !reachable.contains(&(module.id, fid)) && !function.is_synthetic)
        .map(|(_, function)| unused_fn(function))
        .collect()
}

fn reachable_functions(
    ws: &Workspace,
    warm: &FxHashSet<ModuleId>,
    files: &FxHashMap<Ustr, FileLints>,
) -> FxHashSet<FunctionKey> {
    let names = user_fn_names(ws, warm);
    let callees = |body: &BodyLints| {
        let named = body.names.iter().filter_map(|name| names.get(name));
        body.calls
            .iter()
            .copied()
            .chain(named.flatten().copied())
            .collect::<Vec<_>>()
    };
    let mut reachable = FxHashSet::default();
    let mut stack = Vec::new();
    for module in ws.user_modules(warm) {
        for (fid, f) in module.functions.iter() {
            if is_root(f) {
                stack.push((module.id, fid));
            }
        }
        // Non-function bodies run at initialization, so their references are roots.
        for (owner, body) in &files[&module.file].bodies {
            if !matches!(owner, BodyOwner::Function(_)) {
                stack.extend(callees(body));
            }
        }
    }
    while let Some((mid, fid)) = stack.pop() {
        if !reachable.insert((mid, fid)) {
            continue;
        }
        let body = files
            .get(&ws.module(mid).file)
            .and_then(|file| file.bodies.get(&BodyOwner::Function(fid)));
        if let Some(body) = body {
            stack.extend(callees(body));
        }
    }
    reachable
}

fn is_root(f: &Function) -> bool {
    f.json_field_for.is_some()
        || declared_name(f.name.as_str()) == "main"
        || f.annotations
            .iter()
            .any(|a| !is_implementation_anno(a.name.name.as_str()))
}

/// Builtin annotations that describe compilation, not entry. Unknown and user
/// annotations remain roots to avoid flagging externally invoked functions.
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
            | "shader_expand"
            | "build_var_expand"
            | "embed_expand"
            | "scene_type_key"
            | "consume"
    )
}

/// References found by inference (including protocol dispatch) plus names of
/// function values, which inference does not record as calls.
fn body_references(
    ws: &Workspace,
    mid: ModuleId,
    owner: BodyOwner,
    body: &Body,
    out: &mut BodyLints,
) {
    if let Some(bi) = ws.inference(mid).and_then(|inf| inf.per_fn.get(&owner)) {
        inference_edges(bi, &mut out.calls);
    }
    for (_, expr) in body.expr_arena.iter() {
        match expr {
            Expr::Global(n) | Expr::SiblingRef(n) => out.names.push(*n),
            Expr::Call {
                callee: Callee::Unresolved { name, .. },
                ..
            } => out.names.push(*name),
            _ => {}
        }
    }
}

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

fn user_fn_names(
    ws: &Workspace,
    warm: &FxHashSet<ModuleId>,
) -> FxHashMap<Ustr, Vec<(ModuleId, Idx<Function>)>> {
    let mut map: FxHashMap<Ustr, Vec<(ModuleId, Idx<Function>)>> = FxHashMap::default();
    for module in ws.user_modules(warm) {
        for (fid, f) in module.functions.iter() {
            map.entry(f.name).or_default().push((module.id, fid));
        }
    }
    map
}

pub(crate) const UNUSED_PURE_EXPR: &str = "unused_pure_expr";

/// Flag discarded, side-effect-free statement expressions. Block tails are
/// value positions, and the whole expression must be pure.
pub(crate) fn unused_pure_exprs_in_body(body: &Body) -> Vec<Finding> {
    let mut out = Vec::new();
    for (_, stmt) in body.statement_arena.iter() {
        let Statement::Expr(e) = stmt else { continue };
        let Some(is_comparison) = reportable_root(body, *e) else {
            continue;
        };
        if is_pure(body, *e) {
            let message = if is_comparison {
                "result of this comparison is unused -- did you mean `=`?"
            } else {
                "result of pure expression is unused"
            };
            out.push(Finding::warning(
                UNUSED_PURE_EXPR,
                body.expr_spans[*e],
                message.into(),
            ));
        }
    }
    out
}

/// Whether the discarded root is reportable and whether it is a comparison.
/// Exclude constructors and expressions with their own statement positions.
fn reportable_root(body: &Body, e: Idx<Expr>) -> Option<bool> {
    match &body.expr_arena[e] {
        Expr::BinOp { op, .. } => Some(is_comparison_op(*op)),
        Expr::Not(..) | Expr::BitNot(_) | Expr::UnaryMinus(..) => Some(false),
        Expr::Var(_) => Some(false),
        Expr::Int(_) | Expr::Float(_) | Expr::Bool(_) | Expr::String(_) => Some(false),
        Expr::Unit | Expr::Null => Some(false),
        Expr::FieldAccess { .. } | Expr::SafeNav { .. } | Expr::TupleIndex { .. } => Some(false),
        Expr::Tuple(_) => Some(false),
        _ => None,
    }
}

fn is_comparison_op(op: BinOp) -> bool {
    matches!(
        op,
        BinOp::Eq | BinOp::NotEq | BinOp::Lt | BinOp::Leq | BinOp::Gt | BinOp::Geq
    )
}

/// Conservative whole-subtree purity check; calls, mutation, indexing, and
/// control flow are impure.
fn is_pure(body: &Body, e: Idx<Expr>) -> bool {
    match &body.expr_arena[e] {
        Expr::Unit
        | Expr::Null
        | Expr::Error
        | Expr::Hole(_)
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::Bool(_)
        | Expr::String(_)
        | Expr::Var(_)
        | Expr::Global(_)
        | Expr::SiblingRef(_) => true,

        Expr::Not(x, _) | Expr::BitNot(x) | Expr::UnaryMinus(x, _) => is_pure(body, *x),
        Expr::As { value, .. } => is_pure(body, *value),
        Expr::FieldAccess { base, .. } | Expr::SafeNav { base, .. } => is_pure(body, *base),
        Expr::TupleIndex { tuple, .. } => is_pure(body, *tuple),
        Expr::BinOp { lhs, rhs, .. } => is_pure(body, *lhs) && is_pure(body, *rhs),
        Expr::Tuple(elems) | Expr::Array(elems) => (*elems).into_iter().all(|x| is_pure(body, x)),

        // Allocation is not an observable effect; constructor operands must be pure.
        Expr::EnumVariant { args, .. } => variant_args_pure(body, args),
        Expr::StructLiteral { fields, bases, .. } => {
            (*bases).into_iter().all(|b| is_pure(body, b))
                && (*fields)
                    .into_iter()
                    .all(|f| is_pure(body, body.struct_lit_field_arena[f].value))
        }

        _ => false,
    }
}

fn variant_args_pure(body: &Body, args: &bone_core::hir::VariantArgs) -> bool {
    use bone_core::hir::VariantArgs;
    match args {
        VariantArgs::Unit => true,
        VariantArgs::Tuple(elems) => (*elems).into_iter().all(|x| is_pure(body, x)),
        VariantArgs::Record(fields) => (*fields)
            .into_iter()
            .all(|f| is_pure(body, body.struct_lit_field_arena[f].value)),
    }
}

pub const TRANSMUTE_OF_NULLABLE: &str = "transmute_of_nullable";

/// Flag `cast`/`transmute` calls that erase a nullable argument's `?`. Each call
/// is checked independently using inference; matching by name includes user
/// overloads with the same nullability-erasing behavior.
pub(crate) fn transmute_of_nullable_in_body(
    ws: &Workspace,
    bi: Option<&BodyInference>,
    body: &Body,
) -> Vec<Finding> {
    let Some(bi) = bi else { return Vec::new() };

    let mut out = Vec::new();
    for (idx, expr) in body.expr_arena.iter() {
        let Expr::Call {
            callee: Callee::Unresolved { name, .. },
            args,
            ..
        } = expr
        else {
            continue;
        };
        if !matches!(declared_name(name.as_str()), "cast" | "transmute") || args.len() != 1 {
            continue;
        }
        let arg = (*args).into_iter().next().expect("len == 1");
        let (Some(arg_ty), Some(result_ty)) = (
            bi.expr_types.get(arg).copied(),
            bi.expr_types.get(idx).copied(),
        ) else {
            continue;
        };
        if !is_nullable(ws, bi, arg_ty) || is_nullable(ws, bi, result_ty) {
            continue;
        }
        out.push(Finding::new(
            TRANSMUTE_OF_NULLABLE,
            CompilerErrorLevel::Error,
            body.expr_spans[idx],
            format!(
                "`{name}` strips nullability from a `{}` -- a null-on-failure silently \
                 becomes a valid value; use `?? default` or an explicit null check instead",
                ws.render(Some(bi), arg_ty)
            ),
        ));
    }
    out
}

fn is_nullable(ws: &Workspace, bi: &BodyInference, ty: TyId) -> bool {
    matches!(head_type(ws, bi, ty), TypeExpr::Nullable(_))
}

/// Resolve inference variables to the head constructor. An unbound variable is
/// treated as non-nullable to avoid a false positive.
pub(crate) fn head_type(ws: &Workspace, bi: &BodyInference, ty: TyId) -> TypeExpr {
    head_ty(ws, bi, ty).1
}

/// [`head_type`] with the resolved id, needed to descend into a nested type in
/// the arena that owns it after substitution.
fn head_ty(ws: &Workspace, bi: &BodyInference, ty: TyId) -> (TyId, TypeExpr) {
    ws.resolve_ty(Some(bi), ty).expect("body supplied")
}

pub const RAW_INPUT_MIGRATION: &str = "action_layer_raw_input";

/// Require each raw-input site to carry an accepted non-action class marker.
pub const RAW_INPUT_CLASSIFICATION: &str = "action_layer_raw_input_classification";

fn raw_input_findings(
    module: &Module,
    source: &str,
    policy: &ActionLayerPolicy,
    migration: bool,
    classification: bool,
    out: &mut Vec<Finding>,
) {
    let mut sites = Vec::new();
    let mut sites_per_line = FxHashMap::default();
    for body in module.bodies.values() {
        for (idx, expr) in body.expr_arena.iter() {
            let Expr::Call {
                callee: Callee::Unresolved { name, .. },
                ..
            } = expr
            else {
                continue;
            };
            if !policy
                .raw_input_names
                .iter()
                .any(|item| item == declared_name(name.as_str()))
            {
                continue;
            }
            let span = body.expr_spans[idx];
            if migration {
                out.push(Finding::advice(
                    RAW_INPUT_MIGRATION,
                    span,
                    format!(
                        "raw input query `{name}` needs an action, editor/tool, text-input, or deliberate-remainder classification"
                    ),
                ));
            }
            if classification {
                sites.push((*name, span));
                *sites_per_line.entry(span.line_range().0).or_insert(0usize) += 1;
            }
        }
    }

    for (name, span) in sites {
        let line = span.line_range().0;
        let count = sites_per_line[&line];
        let message = if count != 1 {
            format!(
                "raw input classification requires exactly one query per source line (found {count})"
            )
        } else if !has_raw_input_class(source, line, policy) {
            format!(
                "raw input query `{name}` needs an adjacent `// action-layer:` marker: {}",
                policy.raw_input_classes.join(", ")
            )
        } else {
            continue;
        };
        out.push(Finding::advice(RAW_INPUT_CLASSIFICATION, span, message));
    }
}

fn has_raw_input_class(source: &str, line: usize, policy: &ActionLayerPolicy) -> bool {
    source
        .lines()
        .nth(line.saturating_sub(1))
        .and_then(|previous| previous.trim().strip_prefix("// action-layer:"))
        .map(str::trim)
        .is_some_and(|class| {
            class.split_whitespace().count() == 1
                && policy.raw_input_classes.iter().any(|item| item == class)
        })
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
    path.starts_with('<') || path_uses_component(file, "tests") || path_uses_component(file, "test")
}

fn is_action_name(name: Ustr, policy: &ActionLayerPolicy) -> bool {
    declared_name(name.as_str()).starts_with(&policy.action_prefix)
}

fn has_annotation(f: &Function, name: &str) -> bool {
    f.annotations
        .iter()
        .any(|annotation| annotation.name.name.as_str() == name)
}

/// HIR-backed caller census for production actions. The missing-population
/// finding is attached to the first function of the first production module.
fn action_caller_gaps(
    ws: &Workspace,
    warm: &FxHashSet<ModuleId>,
    policy: &ActionLayerPolicy,
) -> Vec<(Ustr, Finding)> {
    let modules = || {
        ws.user_modules(warm)
            .filter(|module| !is_test_module(module.file))
    };
    let mut production = modules()
        .filter(|module| path_uses_component(module.file, &policy.production_root))
        .peekable();
    let first_production = production.peek().copied();
    let declarations: Vec<_> = production
        .flat_map(|module| {
            module
                .globals
                .values()
                .filter(|global| is_action_name(global.name, policy))
                .map(move |global| (module.file, global))
        })
        .collect();
    if declarations.is_empty() {
        return first_production
            .into_iter()
            .filter_map(|module| {
                let function = module.functions.values().next()?;
                Some((
                    module.file,
                    Finding::advice(
                        ACTION_CALLERS,
                        function.span,
                        format!(
                            "no production {} {}* declarations found",
                            policy.production_label, policy.action_prefix
                        ),
                    ),
                ))
            })
            .collect();
    }

    let mut unread: FxHashSet<Ustr> = declarations.iter().map(|(_, g)| g.name).collect();
    for module in modules() {
        for (&owner, body) in &module.bodies {
            let counts_as_caller = match owner {
                BodyOwner::Function(fid) => {
                    let function = &module.functions[fid];
                    !has_annotation(function, "wire")
                        && !policy
                            .registration_functions
                            .iter()
                            .any(|name| name == declared_name(function.name.as_str()))
                }
                BodyOwner::Global(gid) => !is_action_name(module.globals[gid].name, policy),
                _ => false,
            };
            if counts_as_caller {
                for expr in body.expr_arena.values() {
                    if let Expr::Global(name) = expr {
                        unread.remove(name);
                    }
                }
            }
        }
    }

    declarations
        .into_iter()
        .filter(|(_, global)| unread.contains(&global.name))
        .map(|(file, global)| {
            let label = &policy.production_label;
            let action = global.name;
            (
                file,
                Finding::advice(
                    ACTION_CALLERS,
                    global.name_span,
                    format!("production {label} action `{action}` has no production caller"),
                ),
            )
        })
        .collect()
}

pub(crate) const TASK_NULL_ABANDON: &str = "task_null_abandon";

/// Warn when assigning `null` to a `Task?` slot. This flat scan does not track
/// liveness, so it cannot distinguish abandonment from valid cleanup.
pub(crate) fn task_null_abandon_in_body(
    ws: &Workspace,
    bi: Option<&BodyInference>,
    body: &Body,
) -> Vec<Finding> {
    let Some(bi) = bi else { return Vec::new() };
    let mut out = Vec::new();
    for (_, expr) in body.expr_arena.iter() {
        let Expr::Assign { target, value } = *expr else {
            continue;
        };
        if !matches!(body.expr_arena[value], Expr::Null) {
            continue;
        }
        let Some(ty) = place_type(bi, body, target) else {
            continue;
        };
        if !is_task_slot(ws, bi, ty) {
            continue;
        }
        let what = match render_place(body, target) {
            Some(p) => format!("`{p} = null`"),
            None => "assigning `null` to a `Task?` slot".to_string(),
        };
        out.push(Finding::warning(
            TASK_NULL_ABANDON,
            body.expr_spans[target],
            format!(
                "{what} abandons the coroutine mid-stack -- it is dropped where it stands, \
                 so its terminal `defer`s never run; use `stop` to cancel it (or `replace` \
                 to swap one in), which kill the coroutine first"
            ),
        ));
    }
    out
}

fn place_type(bi: &BodyInference, body: &Body, target: Idx<Expr>) -> Option<TyId> {
    match body.expr_arena[target] {
        Expr::Var(b) => bi.binding_types.get(b).copied(),
        _ => bi.expr_types.get(target).copied(),
    }
}

fn is_task_slot(ws: &Workspace, bi: &BodyInference, ty: TyId) -> bool {
    let (id, TypeExpr::Nullable(inner)) = head_ty(ws, bi, ty) else {
        return false;
    };
    matches!(
        head_type(ws, bi, id.with_idx(inner)),
        TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } if declared_name(n.as_str()) == "Task"
    )
}

fn render_place(body: &Body, e: Idx<Expr>) -> Option<String> {
    Some(match body.expr_arena[e] {
        Expr::Var(b) => body.binding_arena[b].name.to_string(),
        Expr::Global(n) => n.to_string(),
        Expr::FieldAccess { base, field } => {
            format!("{}.{}", render_place(body, base)?, field)
        }
        Expr::SafeNav { base, field } => format!("{}?.{}", render_place(body, base)?, field),
        Expr::SafeNavCall {
            receiver, method, ..
        } => {
            format!("{}?.{}()", render_place(body, receiver)?, method)
        }
        // UFCS lowers the receiver to the first call argument.
        Expr::Call {
            callee: Callee::Unresolved { name, .. },
            args,
            ..
        } => {
            let recv = args.into_iter().next()?;
            format!("{}.{}()", render_place(body, recv)?, name)
        }
        _ => return None,
    })
}

fn declared_name(name: &str) -> &str {
    name.rsplit("::").next().unwrap_or(name)
}

#[cfg(test)]
mod allow_lines_tests {
    use super::allow_lines;

    #[test]
    fn matches_str_lines_on_allow_lines() {
        for source in [
            "",
            "allow",
            "a\nallow allow\r\nb\nx // rock: allow(unused_var)\r",
            "\r\nallow\r\n\nallow\rmid\nend allow\n",
            "one\ntwo\n// rock: allow(a)\n// rock: allow(b) allow\nallow",
        ] {
            let expected: Vec<(usize, &str)> = source
                .lines()
                .enumerate()
                .filter(|(_, line)| line.contains("allow"))
                .collect();
            assert_eq!(
                allow_lines(source).collect::<Vec<_>>(),
                expected,
                "{source:?}"
            );
        }
    }
}
