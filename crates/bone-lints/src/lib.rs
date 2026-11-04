//! Lints over bone's typechecked HIR. A pure downstream consumer: reads
//! `Workspace`/`Body` produced by bone-semantic, never re-typechecks and never
//! touches the typechecker core. Findings publish through the LSP as warnings.

use bone_core::hir::{
    AnnoValue, BinOp, BindingInfo, BindingKind, BodyOwner, Callee, Expr, FnBackend, Function,
    Module, Statement, TyId, TypeCtor, TypeExpr,
};
use bone_core::prelude::Instant;
use bone_core::prelude::*;
use bone_core::workspace::{BodyInference, BoundNext, BoundTree};
use rock_core::errors::CompilerErrorLevel;
use std::collections::BTreeSet;

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
    unused_vars_in_body_with_metadata(body, variants, &FxHashSet::default())
}

fn unused_vars_in_body_with_metadata(
    body: &Body,
    variants: &FxHashSet<Ustr>,
    metadata_used: &FxHashSet<Idx<BindingInfo>>,
) -> Vec<UnusedVar> {
    let mut read = read_bindings(body);
    read.extend(metadata_used);
    candidate_bindings(body, variants)
        .into_iter()
        .filter(|idx| !read.contains(idx))
        .map(|idx| &body.binding_arena[idx])
        .filter(|b| !b.name.as_str().starts_with('_'))
        .map(|b| UnusedVar {
            name: b.name,
            span: b.span,
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

fn unused_vars_in_module(module: &Module, variants: &FxHashSet<Ustr>) -> Vec<UnusedVar> {
    module
        .bodies
        .iter()
        .flat_map(|(owner, body)| {
            let metadata_used = decorator_metadata_used_bindings(module, *owner, body);
            unused_vars_in_body_with_metadata(body, variants, &metadata_used)
        })
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
    BACKEND_SIBLING_MISMATCH,
    METHOD_SYNTAX,
    ROW_ESCAPE,
    RAW_INPUT_MIGRATION,
    RAW_INPUT_CLASSIFICATION,
    ACTION_CALLERS,
];

/// Host-owned action-layer conventions. The public engine has no project policy.
/// Paths match a complete component; test modules never declare production actions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionLayerPolicy {
    pub production_root: &'static str,
    pub production_label: &'static str,
    pub action_prefix: &'static str,
    pub raw_input_names: &'static [&'static str],
    pub raw_input_classes: &'static [&'static str],
    /// Registration helpers whose reads do not count as action consumers.
    pub registration_functions: &'static [&'static str],
}

/// What an invocation asks of the lint engine: which lints run, and the knobs
/// the lints themselves take. The default tier is the catalog minus
/// [`OPT_IN_LINTS`]; [`LintOptions::only`] runs exactly the named set, opt-in
/// ones included, and silences everything else.
///
/// This gates COMPUTATION, not just output: a silenced lint's analysis never
/// runs, so an opt-in lint costs a build nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LintOptions {
    action_policy: Option<&'static ActionLayerPolicy>,
    only: Option<Vec<String>>,
    only_types: Option<FxHashSet<Ustr>>,
    ignore_types: FxHashSet<Ustr>,
}

impl LintOptions {
    /// Supply the caller's action policy, or explicitly select the plain build.
    pub fn with_action_policy(mut self, policy: Option<&'static ActionLayerPolicy>) -> Self {
        self.action_policy = policy;
        self
    }

    /// Run exactly `names`. Names that aren't in [`KNOWN_LINTS`] are kept so a
    /// caller can reject a typo through [`LintOptions::unknown`] -- silently
    /// dropping one would run nothing and report success.
    pub fn only(names: impl IntoIterator<Item = String>) -> Self {
        LintOptions {
            only: Some(names.into_iter().collect()),
            ..Default::default()
        }
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
        self.only_types
            .as_ref()
            .is_none_or(|only| only.contains(&name))
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
        Finding {
            lint: F::LINT,
            error: f.to_diagnostic(),
            line: f.line(),
        }
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
        Self {
            full: true,
            ..Default::default()
        }
    }

    fn inverse(&self) -> Self {
        let mut edits = self.edits.clone();
        edits.reverse();
        for edit in &mut edits {
            edit.byte_delta = edit.byte_delta.saturating_neg();
            edit.line_delta = edit.line_delta.saturating_neg();
        }
        Self {
            full: false,
            changed_files: self.changed_files.clone(),
            reinferred: self.reinferred.clone(),
            edits,
        }
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
    pub changed_declarations: usize,
    pub call_edges_retracted: usize,
    pub call_edges_installed: usize,
    pub reachability_components_visited: usize,
    pub declarations_revisited: usize,
    pub declarations_reused: usize,
    pub sibling_groups_requested: usize,
    pub sibling_groups_revisited: usize,
    pub action_declarations_requested: usize,
    pub action_declarations_revisited: usize,
    pub action_callers_requested: usize,
    pub action_callers_revisited: usize,
    pub suppression_files_requested: usize,
    pub suppression_files_refiltered: usize,
    pub full_lint_workspace_passes: usize,
    pub full_cache_reconstructions: usize,
    pub fresh_cached_finding_comparisons: usize,
    pub reuse_totals_reads: usize,
    pub output_refresh_requests: usize,
    pub output_files_refreshed: usize,
    pub output_owner_entries_visited: usize,
    pub output_file_rows_read: usize,
    pub local_lint_derivation_ns: u128,
    pub global_lint_dependency_ns: u128,
    pub global_lint_derivation_ns: u128,
    pub suppression_filtering_ns: u128,
    pub suppression_filter_calls: usize,
    pub suppression_source_scans: usize,
    pub suppression_entry_parses: usize,
    timed: bool,
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
        self.changed_declarations += other.changed_declarations;
        self.call_edges_retracted += other.call_edges_retracted;
        self.call_edges_installed += other.call_edges_installed;
        self.reachability_components_visited += other.reachability_components_visited;
        self.declarations_revisited += other.declarations_revisited;
        self.declarations_reused += other.declarations_reused;
        self.sibling_groups_requested += other.sibling_groups_requested;
        self.sibling_groups_revisited += other.sibling_groups_revisited;
        self.action_declarations_requested += other.action_declarations_requested;
        self.action_declarations_revisited += other.action_declarations_revisited;
        self.action_callers_requested += other.action_callers_requested;
        self.action_callers_revisited += other.action_callers_revisited;
        self.suppression_files_requested += other.suppression_files_requested;
        self.suppression_files_refiltered += other.suppression_files_refiltered;
        self.full_lint_workspace_passes += other.full_lint_workspace_passes;
        self.full_cache_reconstructions += other.full_cache_reconstructions;
        self.fresh_cached_finding_comparisons += other.fresh_cached_finding_comparisons;
        self.reuse_totals_reads += other.reuse_totals_reads;
        self.output_refresh_requests += other.output_refresh_requests;
        self.output_files_refreshed += other.output_files_refreshed;
        self.output_owner_entries_visited += other.output_owner_entries_visited;
        self.output_file_rows_read += other.output_file_rows_read;
        self.local_lint_derivation_ns += other.local_lint_derivation_ns;
        self.global_lint_dependency_ns += other.global_lint_dependency_ns;
        self.global_lint_derivation_ns += other.global_lint_derivation_ns;
        self.suppression_filtering_ns += other.suppression_filtering_ns;
        self.suppression_filter_calls += other.suppression_filter_calls;
        self.suppression_source_scans += other.suppression_source_scans;
        self.suppression_entry_parses += other.suppression_entry_parses;
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LintCacheSnapshotStats {
    pub snapshots: usize,
    pub cached_entries: usize,
    pub cached_findings: usize,
    pub file_entries: usize,
    pub flat_body_entries: usize,
    pub inference_body_entries: usize,
    pub other_entries: usize,
    pub global_entries: usize,
    pub last_entries: usize,
    pub count_ns: u128,
    pub clone_ns: u128,
}

impl LintCacheSnapshotStats {
    pub fn merge(&mut self, other: &Self) {
        self.snapshots += other.snapshots;
        self.cached_entries += other.cached_entries;
        self.cached_findings += other.cached_findings;
        self.file_entries += other.file_entries;
        self.flat_body_entries += other.flat_body_entries;
        self.inference_body_entries += other.inference_body_entries;
        self.other_entries += other.other_entries;
        self.global_entries += other.global_entries;
        self.last_entries += other.last_entries;
        self.count_ns += other.count_ns;
        self.clone_ns += other.clone_ns;
    }
}

#[derive(Clone, Debug, Default)]
struct CachedFileFindings {
    flat_raw: FxHashMap<BodyOwner, Vec<Finding>>,
    flat: FxHashMap<BodyOwner, Vec<Finding>>,
    inference_raw: FxHashMap<BodyOwner, Vec<Finding>>,
    inference: FxHashMap<BodyOwner, Vec<Finding>>,
    other_raw: Vec<Finding>,
    other: Vec<Finding>,
    suppression_fingerprint: u64,
    suppressions: FxHashSet<(usize, &'static str)>,
    owner_order: Vec<BodyOwner>,
    owner_rank: FxHashMap<BodyOwner, usize>,
    active_flat_ranks: BTreeSet<usize>,
    active_inference_ranks: BTreeSet<usize>,
}

#[derive(Clone, Debug)]
struct LintCacheTransaction {
    change: LintCacheUpdate,
    applied: bool,
}

type FunctionKey = (ModuleId, Idx<Function>);
type BodyKey = (ModuleId, BodyOwner);
type SiblingGroupKey = (Ustr, String);

#[derive(Clone, Debug, Default)]
struct SiblingInvalidation {
    names: FxHashSet<Ustr>,
    locations: FxHashSet<(Ustr, usize)>,
}

#[derive(Clone, Debug, Default)]
struct ActionInvalidation {
    names: FxHashSet<Ustr>,
    locations: FxHashSet<(Ustr, usize)>,
}

#[derive(Clone, Debug, Default)]
struct UnusedInvalidation {
    functions: FxHashSet<FunctionKey>,
    locations: FxHashSet<(Ustr, usize)>,
    seeds: FxHashSet<BodyKey>,
}

#[derive(Clone, Debug, Default)]
struct LintDependencyIndex {
    function_names: FxHashMap<Ustr, Vec<FunctionKey>>,
    function_nodes: FxHashSet<FunctionKey>,
    body_edges: FxHashMap<BodyKey, FxHashSet<FunctionKey>>,
    reverse_callers: FxHashMap<FunctionKey, FxHashSet<BodyKey>>,
    root_functions: FxHashSet<FunctionKey>,
    always_live_bodies: FxHashSet<BodyKey>,
    reachable: FxHashSet<FunctionKey>,
    declaration_files: FxHashMap<FunctionKey, Ustr>,
    declaration_names: FxHashMap<FunctionKey, Ustr>,
    declaration_spans: FxHashMap<FunctionKey, Span>,
    declaration_fingerprints: FxHashMap<Ustr, String>,
    action_policy: Option<&'static ActionLayerPolicy>,
    action_declarations: FxHashMap<Ustr, (Ustr, Span)>,
    action_body_edges: FxHashMap<BodyKey, FxHashSet<Ustr>>,
    action_callers: FxHashMap<Ustr, FxHashSet<BodyKey>>,
    sibling_groups: FxHashMap<SiblingGroupKey, FxHashSet<FunctionKey>>,
    sibling_names: FxHashMap<Ustr, FxHashSet<FunctionKey>>,
    sibling_declarations: FxHashMap<FunctionKey, SiblingDecl>,
    action_declaration_fingerprints: FxHashMap<Ustr, String>,
}

impl LintDependencyIndex {
    fn build(
        ws: &Workspace,
        warm: &FxHashSet<ModuleId>,
        action_enabled: bool,
        policy: Option<&'static ActionLayerPolicy>,
    ) -> Self {
        let mut index = Self {
            action_policy: policy,
            ..Self::default()
        };
        for module in user_modules(ws, warm) {
            index
                .declaration_fingerprints
                .insert(module.file, declaration_fingerprint(ws, module));
            index
                .action_declaration_fingerprints
                .insert(module.file, action_declaration_fingerprint(module, policy));
            for (fid, function) in module.functions.iter() {
                let key = (module.id, fid);
                index
                    .function_names
                    .entry(function.name)
                    .or_default()
                    .push(key);
                index.function_nodes.insert(key);
                index.declaration_files.insert(key, module.file);
                index.declaration_names.insert(key, function.name);
                index.declaration_spans.insert(key, function.name_span);
                if is_root(function) {
                    index.root_functions.insert(key);
                }
                index
                    .body_edges
                    .entry((module.id, BodyOwner::Function(fid)))
                    .or_default();
            }
            for owner in module.bodies.keys() {
                let body_key = (module.id, *owner);
                index.body_edges.entry(body_key).or_default();
                if !matches!(owner, BodyOwner::Function(_)) {
                    index.always_live_bodies.insert(body_key);
                }
            }
        }
        for module in user_modules(ws, warm) {
            for (owner, body) in module.bodies.iter() {
                let body_key = (module.id, *owner);
                let edges = body_edges(ws, module.id, *owner, body, &index.function_names)
                    .into_iter()
                    .collect();
                index.install_edges(body_key, edges);
            }
        }
        if action_enabled {
            index.build_action_rows(ws, warm);
        }
        index.build_sibling_rows(ws);
        let nodes = index.all_nodes();
        index.recompute_component(&nodes);
        index
    }

    fn build_action_rows(&mut self, ws: &Workspace, warm: &FxHashSet<ModuleId>) {
        for module in user_modules(ws, warm)
            .filter(|module| is_production_action_module(module.file, self.action_policy))
        {
            for (_, global) in module
                .globals
                .iter()
                .filter(|(_, global)| is_action_name(global.name, self.action_policy))
            {
                self.action_declarations
                    .insert(global.name, (module.file, global.name_span));
            }
        }
        let declared: FxHashSet<Ustr> = self.action_declarations.keys().copied().collect();
        for module in user_modules(ws, warm).filter(|module| !is_test_module(module.file)) {
            for (owner, body) in &module.bodies {
                let edges =
                    action_edges_in_body(module, *owner, body, &declared, self.action_policy);
                self.install_action_edges((module.id, *owner), edges);
            }
        }
    }

    fn install_action_edges(&mut self, body: BodyKey, edges: FxHashSet<Ustr>) {
        for action in &edges {
            self.action_callers.entry(*action).or_default().insert(body);
        }
        self.action_body_edges.insert(body, edges);
    }

    fn install_edges(&mut self, body: BodyKey, edges: FxHashSet<FunctionKey>) {
        for target in &edges {
            self.reverse_callers
                .entry(*target)
                .or_default()
                .insert(body);
        }
        self.body_edges.insert(body, edges);
    }

    fn build_sibling_rows(&mut self, ws: &Workspace) {
        for decls in ws.functions_by_name.values() {
            for &(mid, fid) in decls {
                let declaration = sibling_decl(ws, mid, fid);
                let key = (declaration.name, declaration.signature.clone());
                let function = (mid, fid);
                self.sibling_groups.entry(key).or_default().insert(function);
                self.sibling_names
                    .entry(declaration.name)
                    .or_default()
                    .insert(function);
                self.sibling_declarations.insert(function, declaration);
            }
        }
    }

    fn update_sibling_declarations(
        &mut self,
        ws: &Workspace,
        changed_files: &FxHashSet<Ustr>,
        stats: &mut LintCacheStats,
    ) -> SiblingInvalidation {
        let mut invalidation = SiblingInvalidation::default();
        for declaration in self.sibling_declarations.values() {
            if changed_files.contains(&declaration.file) {
                invalidation.names.insert(declaration.name);
            }
        }
        for module in user_modules(ws, &FxHashSet::default()) {
            if changed_files.contains(&module.file) {
                invalidation
                    .names
                    .extend(module.functions.iter().map(|(_, f)| f.name));
            }
        }

        for declaration in self.sibling_declarations.values() {
            if invalidation.names.contains(&declaration.name) {
                invalidation
                    .locations
                    .insert((declaration.file, declaration.span.offset_range.0));
            }
        }
        for module in user_modules(ws, &FxHashSet::default()) {
            if !invalidation
                .names
                .iter()
                .any(|name| module.functions.iter().any(|(_, f)| f.name == *name))
            {
                continue;
            }
            for (_, function) in module.functions.iter() {
                if invalidation.names.contains(&function.name) {
                    invalidation
                        .locations
                        .insert((module.file, function.name_span.offset_range.0));
                }
            }
        }

        let old_groups: FxHashSet<SiblingGroupKey> = self
            .sibling_groups
            .keys()
            .filter(|(name, _)| invalidation.names.contains(name))
            .cloned()
            .collect();
        stats.sibling_groups_requested += old_groups.len();

        let old_functions: Vec<FunctionKey> = self
            .sibling_declarations
            .iter()
            .filter(|(_, declaration)| changed_files.contains(&declaration.file))
            .map(|(&function, _)| function)
            .collect();
        for function in old_functions {
            if let Some(declaration) = self.sibling_declarations.remove(&function) {
                let group = (declaration.name, declaration.signature.clone());
                if let Some(functions) = self.sibling_groups.get_mut(&group) {
                    functions.remove(&function);
                    if functions.is_empty() {
                        self.sibling_groups.remove(&group);
                    }
                }
                if let Some(functions) = self.sibling_names.get_mut(&declaration.name) {
                    functions.remove(&function);
                    if functions.is_empty() {
                        self.sibling_names.remove(&declaration.name);
                    }
                }
            }
        }
        for module in user_modules(ws, &FxHashSet::default()) {
            if !changed_files.contains(&module.file) {
                continue;
            }
            for (fid, _) in module.functions.iter() {
                let declaration = sibling_decl(ws, module.id, fid);
                let group = (declaration.name, declaration.signature.clone());
                let function = (module.id, fid);
                self.sibling_groups
                    .entry(group)
                    .or_default()
                    .insert(function);
                self.sibling_names
                    .entry(declaration.name)
                    .or_default()
                    .insert(function);
                self.sibling_declarations.insert(function, declaration);
            }
        }
        let new_groups: FxHashSet<SiblingGroupKey> = self
            .sibling_groups
            .keys()
            .filter(|(name, _)| invalidation.names.contains(name))
            .cloned()
            .collect();
        stats.sibling_groups_requested += new_groups.difference(&old_groups).count();
        stats.sibling_groups_revisited += old_groups.union(&new_groups).count();
        for &file in changed_files {
            if let Some(module) = ws.modules.iter().find(|module| module.file == file) {
                self.declaration_fingerprints
                    .insert(file, declaration_fingerprint(ws, module));
            } else {
                self.declaration_fingerprints.remove(&file);
            }
        }
        invalidation
    }

    fn all_nodes(&self) -> FxHashSet<BodyKey> {
        let mut nodes = self.body_edges.keys().copied().collect::<FxHashSet<_>>();
        nodes.extend(
            self.function_nodes
                .iter()
                .map(|&(mid, fid)| (mid, BodyOwner::Function(fid))),
        );
        nodes
    }

    fn collect_component(&self, seed: BodyKey) -> FxHashSet<BodyKey> {
        let mut component = FxHashSet::default();
        let mut stack = vec![seed];
        while let Some(node) = stack.pop() {
            if !component.insert(node) {
                continue;
            }
            if let Some(edges) = self.body_edges.get(&node) {
                stack.extend(
                    edges
                        .iter()
                        .map(|&(mid, fid)| (mid, BodyOwner::Function(fid))),
                );
            }
            if let BodyOwner::Function(fid) = node.1 {
                if let Some(callers) = self.reverse_callers.get(&(node.0, fid)) {
                    stack.extend(callers.iter().copied());
                }
            }
        }
        component
    }

    fn recompute_component(&mut self, component: &FxHashSet<BodyKey>) {
        for &(mid, owner) in component {
            if let BodyOwner::Function(fid) = owner {
                self.reachable.remove(&(mid, fid));
            }
        }
        let mut stack = Vec::new();
        for &node in component {
            let root = match node.1 {
                BodyOwner::Function(fid) => self.root_functions.contains(&(node.0, fid)),
                _ => self.always_live_bodies.contains(&node),
            };
            if root {
                if let BodyOwner::Function(fid) = node.1 {
                    self.reachable.insert((node.0, fid));
                }
                stack.push(node);
            }
        }
        while let Some(node) = stack.pop() {
            let Some(edges) = self.body_edges.get(&node) else {
                continue;
            };
            for &target in edges {
                let target_node = (target.0, BodyOwner::Function(target.1));
                if component.contains(&target_node) && self.reachable.insert(target) {
                    stack.push(target_node);
                }
            }
        }
    }

    fn update_bodies(
        &mut self,
        ws: &Workspace,
        bodies: &[(ModuleId, BodyOwner)],
        stats: &mut LintCacheStats,
    ) -> FxHashSet<FunctionKey> {
        let mut seeds = FxHashSet::default();
        for &(mid, owner) in bodies {
            let body_key = (mid, owner);
            let old = self.body_edges.get(&body_key).cloned().unwrap_or_default();
            let new = ws
                .modules
                .iter()
                .find(|module| module.id == mid)
                .and_then(|module| module.bodies.get(&owner))
                .map(|body| {
                    body_edges(ws, mid, owner, body, &self.function_names)
                        .into_iter()
                        .collect()
                })
                .unwrap_or_default();
            if old == new {
                continue;
            }
            for target in &old {
                if let Some(callers) = self.reverse_callers.get_mut(target) {
                    callers.remove(&body_key);
                    if callers.is_empty() {
                        self.reverse_callers.remove(target);
                    }
                }
            }
            stats.call_edges_retracted += old.len();
            stats.call_edges_installed += new.len();
            seeds.insert(body_key);
            seeds
                .extend(old.iter().map(|&(target_mid, target_fid)| {
                    (target_mid, BodyOwner::Function(target_fid))
                }));
            seeds
                .extend(new.iter().map(|&(target_mid, target_fid)| {
                    (target_mid, BodyOwner::Function(target_fid))
                }));
            self.install_edges(body_key, new);
        }

        let mut covered = FxHashSet::default();
        let mut affected = FxHashSet::default();
        for seed in seeds {
            if covered.contains(&seed) {
                continue;
            }
            let component = self.collect_component(seed);
            covered.extend(component.iter().copied());
            stats.reachability_components_visited += 1;
            self.recompute_component(&component);
            affected.extend(
                component
                    .into_iter()
                    .filter_map(|(mid, owner)| match owner {
                        BodyOwner::Function(fid) => Some((mid, fid)),
                        _ => None,
                    }),
            );
        }
        affected
    }

    fn update_declarations(
        &mut self,
        ws: &Workspace,
        warm: &FxHashSet<ModuleId>,
        changed_files: &FxHashSet<Ustr>,
        stats: &mut LintCacheStats,
    ) -> UnusedInvalidation {
        let mut invalidation = UnusedInvalidation::default();
        let old_functions: Vec<FunctionKey> = self
            .declaration_files
            .iter()
            .filter(|(_, file)| changed_files.contains(file))
            .map(|(&function, _)| function)
            .collect();
        for &function in &old_functions {
            invalidation.functions.insert(function);
            invalidation
                .seeds
                .insert((function.0, BodyOwner::Function(function.1)));
            if let Some(&span) = self.declaration_spans.get(&function) {
                let file = self.declaration_files[&function];
                invalidation.locations.insert((file, span.offset_range.0));
            }
            if let Some(callers) = self.reverse_callers.get(&function) {
                invalidation.seeds.extend(callers.iter().copied());
            }
            if let Some(edges) = self
                .body_edges
                .get(&(function.0, BodyOwner::Function(function.1)))
            {
                invalidation.seeds.extend(
                    edges
                        .iter()
                        .map(|&(mid, fid)| (mid, BodyOwner::Function(fid))),
                );
            }
        }

        let current_functions: Vec<FunctionKey> = user_modules(ws, warm)
            .filter(|module| changed_files.contains(&module.file))
            .flat_map(|module| {
                module
                    .functions
                    .iter()
                    .map(move |(fid, _)| (module.id, fid))
            })
            .collect();
        for &function in &current_functions {
            invalidation.functions.insert(function);
            invalidation
                .seeds
                .insert((function.0, BodyOwner::Function(function.1)));
            let module = ws.module(function.0);
            let declaration = &module.functions[function.1];
            invalidation
                .locations
                .insert((module.file, declaration.name_span.offset_range.0));
        }

        for function in old_functions {
            let old_edges = self
                .body_edges
                .remove(&(function.0, BodyOwner::Function(function.1)))
                .unwrap_or_default();
            stats.call_edges_retracted += old_edges.len();
            for target in old_edges {
                if let Some(callers) = self.reverse_callers.get_mut(&target) {
                    callers.remove(&(function.0, BodyOwner::Function(function.1)));
                    if callers.is_empty() {
                        self.reverse_callers.remove(&target);
                    }
                }
            }
            self.function_nodes.remove(&function);
            self.declaration_files.remove(&function);
            self.declaration_names.remove(&function);
            self.declaration_spans.remove(&function);
            self.root_functions.remove(&function);
            self.reachable.remove(&function);
            self.reverse_callers.remove(&function);
            for functions in self.function_names.values_mut() {
                functions.retain(|candidate| *candidate != function);
            }
        }
        self.function_names
            .retain(|_, functions| !functions.is_empty());

        for &function in &current_functions {
            let module = ws.module(function.0);
            let declaration = &module.functions[function.1];
            self.function_names
                .entry(declaration.name)
                .or_default()
                .push(function);
            self.function_nodes.insert(function);
            self.declaration_files.insert(function, module.file);
            self.declaration_names.insert(function, declaration.name);
            self.declaration_spans
                .insert(function, declaration.name_span);
            if is_root(declaration) {
                self.root_functions.insert(function);
            }
            self.body_edges
                .entry((function.0, BodyOwner::Function(function.1)))
                .or_default();
        }
        for &function in &current_functions {
            let module = ws.module(function.0);
            let owner = BodyOwner::Function(function.1);
            let Some(body) = module.bodies.get(&owner) else {
                continue;
            };
            let edges = body_edges(ws, function.0, owner, body, &self.function_names)
                .into_iter()
                .collect::<FxHashSet<_>>();
            let old = self
                .body_edges
                .get(&(function.0, owner))
                .cloned()
                .unwrap_or_default();
            if old != edges {
                stats.call_edges_retracted += old.len();
                stats.call_edges_installed += edges.len();
                for target in old {
                    if let Some(callers) = self.reverse_callers.get_mut(&target) {
                        callers.remove(&(function.0, owner));
                        if callers.is_empty() {
                            self.reverse_callers.remove(&target);
                        }
                    }
                }
                self.install_edges((function.0, owner), edges);
            }
        }
        for &file in changed_files {
            if let Some(module) = ws.modules.iter().find(|module| module.file == file) {
                self.declaration_fingerprints
                    .insert(file, declaration_fingerprint(ws, module));
            } else {
                self.declaration_fingerprints.remove(&file);
            }
        }
        stats.changed_declarations += invalidation.functions.len();
        invalidation
    }

    fn recompute_seed_components(
        &mut self,
        seeds: &FxHashSet<BodyKey>,
        stats: &mut LintCacheStats,
    ) -> FxHashSet<FunctionKey> {
        let mut covered = FxHashSet::default();
        let mut affected = FxHashSet::default();
        for &seed in seeds {
            if covered.contains(&seed)
                || (!self.body_edges.contains_key(&seed)
                    && !matches!(seed.1, BodyOwner::Function(fid) if self.function_nodes.contains(&(seed.0, fid))))
            {
                continue;
            }
            let component = self.collect_component(seed);
            covered.extend(component.iter().copied());
            stats.reachability_components_visited += 1;
            self.recompute_component(&component);
            affected.extend(
                component
                    .into_iter()
                    .filter_map(|(mid, owner)| match owner {
                        BodyOwner::Function(fid) if self.function_nodes.contains(&(mid, fid)) => {
                            Some((mid, fid))
                        }
                        _ => None,
                    }),
            );
        }
        affected
    }

    fn update_action_bodies(
        &mut self,
        ws: &Workspace,
        bodies: &[(ModuleId, BodyOwner)],
        stats: &mut LintCacheStats,
    ) -> FxHashSet<Ustr> {
        let declared: FxHashSet<Ustr> = self.action_declarations.keys().copied().collect();
        let mut affected = FxHashSet::default();
        for &(mid, owner) in bodies {
            stats.action_callers_requested += 1;
            let body_key = (mid, owner);
            let old = self
                .action_body_edges
                .get(&body_key)
                .cloned()
                .unwrap_or_default();
            let new = ws
                .modules
                .iter()
                .find(|module| module.id == mid)
                .and_then(|module| module.bodies.get(&owner).map(|body| (module, body)))
                .map(|(module, body)| {
                    action_edges_in_body(module, owner, body, &declared, self.action_policy)
                })
                .unwrap_or_default();
            if old == new {
                continue;
            }
            for action in &old {
                if let Some(callers) = self.action_callers.get_mut(action) {
                    callers.remove(&body_key);
                    if callers.is_empty() {
                        self.action_callers.remove(action);
                    }
                }
            }
            affected.extend(old.iter().copied());
            affected.extend(new.iter().copied());
            self.install_action_edges(body_key, new);
        }
        stats.action_callers_revisited += affected.len();
        affected
    }

    fn action_declarations_changed(&self, ws: &Workspace, changed_files: &FxHashSet<Ustr>) -> bool {
        changed_files.iter().any(|file| {
            let current = ws.modules.iter().find(|module| module.file == *file);
            match current {
                Some(module) => {
                    self.action_declaration_fingerprints.get(file)
                        != Some(&action_declaration_fingerprint(module, self.action_policy))
                }
                None => self.action_declaration_fingerprints.contains_key(file),
            }
        })
    }

    fn update_action_declarations(
        &mut self,
        ws: &Workspace,
        changed_files: &FxHashSet<Ustr>,
        stats: &mut LintCacheStats,
    ) -> ActionInvalidation {
        let mut invalidation = ActionInvalidation::default();
        for (&action, &(file, span)) in &self.action_declarations {
            if changed_files.contains(&file) {
                invalidation.names.insert(action);
                invalidation.locations.insert((file, span.offset_range.0));
            }
        }
        for module in user_modules(ws, &FxHashSet::default()) {
            if !changed_files.contains(&module.file)
                || !is_production_action_module(module.file, self.action_policy)
            {
                continue;
            }
            for (_, global) in module
                .globals
                .iter()
                .filter(|(_, global)| is_action_name(global.name, self.action_policy))
            {
                invalidation.names.insert(global.name);
                invalidation
                    .locations
                    .insert((module.file, global.name_span.offset_range.0));
            }
        }
        if invalidation.names.is_empty() {
            for &file in changed_files {
                if let Some(module) = ws.modules.iter().find(|module| module.file == file) {
                    self.action_declaration_fingerprints.insert(
                        file,
                        action_declaration_fingerprint(module, self.action_policy),
                    );
                } else {
                    self.action_declaration_fingerprints.remove(&file);
                }
            }
            return invalidation;
        }

        stats.action_declarations_requested += invalidation.names.len();
        stats.changed_declarations += invalidation.names.len();
        let removed: Vec<Ustr> = self
            .action_declarations
            .iter()
            .filter(|(_, (file, _))| changed_files.contains(file))
            .map(|(&action, _)| action)
            .collect();
        for action in removed {
            self.action_declarations.remove(&action);
        }
        for module in user_modules(ws, &FxHashSet::default()) {
            if !changed_files.contains(&module.file)
                || !is_production_action_module(module.file, self.action_policy)
            {
                continue;
            }
            for (_, global) in module
                .globals
                .iter()
                .filter(|(_, global)| is_action_name(global.name, self.action_policy))
            {
                self.action_declarations
                    .insert(global.name, (module.file, global.name_span));
            }
        }
        let declared: FxHashSet<Ustr> = self.action_declarations.keys().copied().collect();
        let old_edges: Vec<(BodyKey, FxHashSet<Ustr>)> = self
            .action_body_edges
            .iter()
            .map(|(&body, edges)| (body, edges.clone()))
            .collect();
        for (body, mut edges) in old_edges {
            let removed_edges: Vec<Ustr> = edges
                .iter()
                .copied()
                .filter(|action| !declared.contains(action))
                .collect();
            for action in removed_edges {
                edges.remove(&action);
                if let Some(callers) = self.action_callers.get_mut(&action) {
                    callers.remove(&body);
                    if callers.is_empty() {
                        self.action_callers.remove(&action);
                    }
                }
            }
            self.action_body_edges.insert(body, edges);
        }
        stats.action_declarations_revisited += self.action_declarations.len();
        for &file in changed_files {
            if let Some(module) = ws.modules.iter().find(|module| module.file == file) {
                self.action_declaration_fingerprints.insert(
                    file,
                    action_declaration_fingerprint(module, self.action_policy),
                );
            } else {
                self.action_declaration_fingerprints.remove(&file);
            }
        }
        invalidation
    }

    fn declarations_changed(&self, ws: &Workspace, changed_files: &FxHashSet<Ustr>) -> bool {
        changed_files.iter().any(|file| {
            let current = ws.modules.iter().find(|module| module.file == *file);
            match current {
                Some(module) => {
                    self.declaration_fingerprints.get(file)
                        != Some(&declaration_fingerprint(ws, module))
                }
                None => self.declaration_fingerprints.contains_key(file),
            }
        })
    }
}

fn declaration_fingerprint(ws: &Workspace, module: &Module) -> String {
    let mut rows = module
        .functions
        .iter()
        .map(|(_, function)| {
            let params = function
                .param_decls
                .iter()
                .map(|param| ws.render(None, param.ty))
                .join(",");
            let ret = function
                .return_ty
                .map(|ty| ws.render(None, ty))
                .unwrap_or_default();
            let annotations = function
                .annotations
                .iter()
                .map(|annotation| annotation.name.name.as_str())
                .join(",");
            format!(
                "{}|{:?}|({})|{}|{}|{}|{}",
                function.name,
                function.backend,
                params,
                ret,
                function.is_extern,
                function.is_synthetic,
                annotations
            )
        })
        .collect::<Vec<_>>();
    rows.sort();
    rows.join("\n")
}

fn action_declaration_fingerprint(module: &Module, policy: Option<&ActionLayerPolicy>) -> String {
    let mut rows = module
        .globals
        .iter()
        .filter(|(_, global)| is_action_name(global.name, policy))
        .map(|(_, global)| global.name.as_str().to_owned())
        .collect::<Vec<_>>();
    rows.sort();
    rows.join("\n")
}

fn action_edges_in_body(
    module: &Module,
    owner: BodyOwner,
    body: &Body,
    declared: &FxHashSet<Ustr>,
    policy: Option<&ActionLayerPolicy>,
) -> FxHashSet<Ustr> {
    let production_read = match owner {
        BodyOwner::Function(fid) => {
            let function = &module.functions[fid];
            !has_annotation(function, "wire")
                && !policy.is_some_and(|policy| {
                    policy
                        .registration_functions
                        .contains(&declared_name(function.name.as_str()))
                })
        }
        BodyOwner::Global(gid) => !is_action_name(module.globals[gid].name, policy),
        _ => false,
    };
    if !production_read {
        return FxHashSet::default();
    }
    body.expr_arena
        .values()
        .filter_map(|expr| match expr {
            Expr::Global(name) if declared.contains(name) => Some(*name),
            _ => None,
        })
        .collect()
}

/// The single reusable incremental lint derivation cache. It owns the local
/// body entries and the last workspace-global result; reporting policy stays in
/// the CLI/LSP adapters.
#[derive(Clone, Debug, Default)]
pub struct IncrementalLintCache {
    initialized: bool,
    is_bin: bool,
    warm: FxHashSet<ModuleId>,
    variants: FxHashSet<Ustr>,
    workspace_files: FxHashSet<Ustr>,
    report_files: FxHashSet<Ustr>,
    report_order: Vec<Ustr>,
    report_rank: FxHashMap<Ustr, usize>,
    active_output_ranks: BTreeSet<usize>,
    selection: Option<LintOptions>,
    files: FxHashMap<Ustr, CachedFileFindings>,
    global_raw: FxHashMap<Ustr, Vec<Finding>>,
    global: FxHashMap<Ustr, Vec<Finding>>,
    last_by_file: FxHashMap<Ustr, Vec<Finding>>,
    flat_body_entries: usize,
    inference_body_entries: usize,
    flat_finding_entries: usize,
    inference_finding_entries: usize,
    action_population_file: Option<Ustr>,
    global_span_owners: FxHashMap<Ustr, FxHashSet<Ustr>>,
    global_referenced_files: FxHashMap<Ustr, FxHashSet<Ustr>>,
    dependency: LintDependencyIndex,
    timing_enabled: bool,
    body_transaction: Option<LintCacheTransaction>,
}

impl IncrementalLintCache {
    pub fn compare_fresh_findings(
        &self,
        ws: &Workspace,
        warm: &FxHashSet<ModuleId>,
        is_bin: bool,
        sources: &FxHashMap<Ustr, &str>,
        sel: &LintOptions,
        cached: &[Finding],
        stats: &mut LintCacheStats,
    ) -> bool {
        let fresh = lint_workspace(ws, warm, is_bin, sources, sel);
        stats.fresh_cached_finding_comparisons += 1;
        let mut fresh_keys = fresh
            .iter()
            .map(|finding| format!("{finding:?}"))
            .collect::<Vec<_>>();
        let mut cached_keys = cached
            .iter()
            .map(|finding| format!("{finding:?}"))
            .collect::<Vec<_>>();
        fresh_keys.sort();
        cached_keys.sort();
        fresh_keys == cached_keys
    }

    pub fn snapshot_stats(&self, measure: bool) -> Option<LintCacheSnapshotStats> {
        let mut stats = measure.then(LintCacheSnapshotStats::default);
        let count_started = measure.then(Instant::now);
        if let (Some(stats), Some(count_started)) = (&mut stats, count_started) {
            stats.snapshots = 1;
            stats.file_entries = self.files.len();
            stats.flat_body_entries = self.files.values().map(|file| file.flat.len()).sum();
            stats.inference_body_entries =
                self.files.values().map(|file| file.inference.len()).sum();
            stats.other_entries = self.files.values().map(|file| file.other.len()).sum();
            stats.global_entries = self.global.len();
            stats.last_entries = self.last_by_file.len();
            stats.cached_entries = stats.file_entries
                + stats.flat_body_entries
                + stats.inference_body_entries
                + stats.other_entries
                + stats.global_entries
                + stats.last_entries;
            stats.cached_findings = self
                .files
                .values()
                .map(|file| {
                    file.flat.values().map(Vec::len).sum::<usize>()
                        + file.inference.values().map(Vec::len).sum::<usize>()
                        + file.other.len()
                })
                .sum::<usize>()
                + self.global.values().map(Vec::len).sum::<usize>()
                + self.last_by_file.values().map(Vec::len).sum::<usize>();
            stats.count_ns = count_started.elapsed().as_nanos();
        }
        stats
    }

    pub fn begin_body_transaction(&mut self, change: &LintCacheUpdate) {
        debug_assert!(
            !change.full,
            "body transaction cannot wrap a full lint update"
        );
        debug_assert!(
            self.body_transaction.is_none(),
            "nested lint-cache transaction"
        );
        self.body_transaction = Some(LintCacheTransaction {
            change: change.clone(),
            applied: false,
        });
    }

    pub fn commit_body_transaction(&mut self) {
        self.body_transaction = None;
    }

    pub fn rollback_body_transaction(
        &mut self,
        ws: &Workspace,
        warm: &FxHashSet<ModuleId>,
        is_bin: bool,
        sources: &FxHashMap<Ustr, &str>,
        sel: &LintOptions,
    ) -> Result<LintCacheStats, String> {
        let Some(transaction) = self.body_transaction.take() else {
            return Ok(LintCacheStats::default());
        };
        if !transaction.applied {
            return Ok(LintCacheStats::default());
        }
        let inverse = transaction.change.inverse();
        let (_, stats) = self.update(ws, warm, is_bin, sources, sel, &inverse);
        if stats.full_rebuilds != 0 {
            return Err(format!(
                "body lint rollback unexpectedly rebuilt the full cache ({} rebuilds)",
                stats.full_rebuilds
            ));
        }
        Ok(stats)
    }

    fn mark_body_transaction_applied(&mut self) {
        if let Some(transaction) = &mut self.body_transaction {
            transaction.applied = true;
        }
    }

    pub fn update_timed(
        &mut self,
        ws: &Workspace,
        warm: &FxHashSet<ModuleId>,
        is_bin: bool,
        sources: &FxHashMap<Ustr, &str>,
        sel: &LintOptions,
        change: &LintCacheUpdate,
    ) -> (Vec<Finding>, LintCacheStats) {
        self.timing_enabled = true;
        let result = self.update(ws, warm, is_bin, sources, sel, change);
        self.timing_enabled = false;
        result
    }

    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn invalidate_file(&mut self, file: Ustr) {
        if let Some(cached) = self.files.remove(&file) {
            self.flat_body_entries = self.flat_body_entries.saturating_sub(cached.flat.len());
            self.inference_body_entries = self
                .inference_body_entries
                .saturating_sub(cached.inference.len());
            self.flat_finding_entries = self
                .flat_finding_entries
                .saturating_sub(cached.flat.values().map(Vec::len).sum());
            self.inference_finding_entries = self
                .inference_finding_entries
                .saturating_sub(cached.inference.values().map(Vec::len).sum());
        }
        self.global.remove(&file);
        self.last_by_file.remove(&file);
        if let Some(rank) = self.report_rank.get(&file) {
            self.active_output_ranks.remove(rank);
        }
    }

    pub fn findings_for_file(&self, file: Ustr) -> Vec<Finding> {
        self.last_by_file.get(&file).cloned().unwrap_or_default()
    }

    fn filter_cached_suppressions(
        &self,
        file: Ustr,
        findings: Vec<Finding>,
        stats: &mut LintCacheStats,
    ) -> Vec<Finding> {
        let cached = self
            .files
            .get(&file)
            .expect("lint derivation requires a populated suppression cache");
        filter_suppressed_findings_with_entries_timed(findings, &cached.suppressions, stats)
    }

    fn refresh_global_span_index(&mut self, output_files: &FxHashSet<Ustr>) {
        for &output_file in output_files {
            if let Some(old) = self.global_referenced_files.remove(&output_file) {
                for referenced in old {
                    if let Some(owners) = self.global_span_owners.get_mut(&referenced) {
                        owners.remove(&output_file);
                        if owners.is_empty() {
                            self.global_span_owners.remove(&referenced);
                        }
                    }
                }
            }
            let mut referenced = FxHashSet::default();
            if let Some(findings) = self.global_raw.get(&output_file) {
                for finding in findings {
                    referenced.insert(finding.error.location.file);
                    referenced.extend(finding.error.labels.iter().map(|label| label.span.file));
                }
            }
            for file in &referenced {
                self.global_span_owners
                    .entry(*file)
                    .or_default()
                    .insert(output_file);
            }
            if !referenced.is_empty() {
                self.global_referenced_files.insert(output_file, referenced);
            }
        }
    }

    fn refresh_active_owner_ranks(cached: &mut CachedFileFindings) {
        cached.active_flat_ranks = cached
            .flat
            .iter()
            .filter(|(_, findings)| !findings.is_empty())
            .filter_map(|(owner, _)| cached.owner_rank.get(owner).copied())
            .collect();
        cached.active_inference_ranks = cached
            .inference
            .iter()
            .filter(|(_, findings)| !findings.is_empty())
            .filter_map(|(owner, _)| cached.owner_rank.get(owner).copied())
            .collect();
    }

    fn replace_unused_findings(
        &mut self,
        ws: &Workspace,
        warm: &FxHashSet<ModuleId>,
        sources: &FxHashMap<Ustr, &str>,
        affected: &FxHashSet<FunctionKey>,
        extra_locations: &FxHashSet<(Ustr, usize)>,
        stats: &mut LintCacheStats,
    ) {
        let mut locations = extra_locations.clone();
        let mut affected_files: FxHashSet<Ustr> = affected
            .iter()
            .filter_map(|&(mid, _)| ws.modules.iter().find(|module| module.id == mid))
            .map(|module| module.file)
            .filter(|file| sources.contains_key(file))
            .collect();
        for &(mid, fid) in affected {
            let Some(module) = ws.modules.iter().find(|module| module.id == mid) else {
                continue;
            };
            let Some((_, function)) = module
                .functions
                .iter()
                .find(|(candidate, _)| *candidate == fid)
            else {
                continue;
            };
            locations.insert((module.file, function.name_span.offset_range.0));
            if sources.contains_key(&module.file) {
                affected_files.insert(module.file);
            }
        }
        affected_files.extend(
            locations
                .iter()
                .map(|(file, _)| *file)
                .filter(|file| sources.contains_key(file)),
        );
        for file in &affected_files {
            if let Some(findings) = self.global.get_mut(file) {
                findings.retain(|finding| {
                    finding.lint != UNUSED_FN
                        || !locations.contains(&(finding.file(), finding_offset(finding)))
                });
            }
            if let Some(findings) = self.global_raw.get_mut(file) {
                findings.retain(|finding| {
                    finding.lint != UNUSED_FN
                        || !locations.contains(&(finding.file(), finding_offset(finding)))
                });
            }
        }

        let mut derived_raw: FxHashMap<Ustr, Vec<Finding>> = FxHashMap::default();
        let mut derived: FxHashMap<Ustr, Vec<Finding>> = FxHashMap::default();
        for &(mid, fid) in affected {
            if warm.contains(&mid) {
                continue;
            }
            let Some(module) = ws.modules.iter().find(|module| module.id == mid) else {
                continue;
            };
            let Some((_, function)) = module
                .functions
                .iter()
                .find(|(candidate, _)| *candidate == fid)
            else {
                continue;
            };
            if self.dependency.reachable.contains(&(mid, fid)) || function.is_synthetic {
                continue;
            }
            if !sources.contains_key(&module.file) {
                continue;
            }
            let raw = Finding::of(&UnusedFn {
                name: function.name,
                span: function.name_span,
            });
            let kept = self.filter_cached_suppressions(module.file, vec![raw.clone()], stats);
            derived_raw
                .entry(module.file)
                .or_default()
                .push(raw.clone());
            stats.global_findings_derived += kept.len();
            derived.entry(module.file).or_default().extend(kept);
        }
        for findings in derived.values_mut() {
            findings.sort_by(|a, b| {
                finding_offset(a)
                    .cmp(&finding_offset(b))
                    .then_with(|| a.lint.cmp(b.lint))
                    .then_with(|| a.error.message.cmp(&b.error.message))
            });
        }
        for file in affected_files {
            let old_raw = self.global_raw.remove(&file).unwrap_or_default();
            let mut raw = derived_raw.remove(&file).unwrap_or_default();
            raw.extend(old_raw);
            if !raw.is_empty() {
                self.global_raw.insert(file, raw);
            }
            let old = self.global.remove(&file).unwrap_or_default();
            let mut findings = derived.remove(&file).unwrap_or_default();
            findings.extend(old);
            if !findings.is_empty() {
                self.global.insert(file, findings);
            }
        }
    }

    fn refilter_suppression_files(
        &mut self,
        files: &FxHashSet<Ustr>,
        sources: &FxHashMap<Ustr, &str>,
        stats: &mut LintCacheStats,
    ) {
        for &file in files {
            let Some(source) = sources.get(&file).copied() else {
                continue;
            };
            let Some(cached) = self.files.get_mut(&file) else {
                continue;
            };
            cached.suppressions = suppression_entries_for_cache(source, stats);
            let flat_raw = cached.flat_raw.clone();
            for (owner, raw) in flat_raw {
                cached.flat.insert(
                    owner,
                    filter_suppressed_findings_with_entries_timed(
                        raw.clone(),
                        &cached.suppressions,
                        stats,
                    ),
                );
            }
            let inference_raw = cached.inference_raw.clone();
            for (owner, raw) in inference_raw {
                cached.inference.insert(
                    owner,
                    filter_suppressed_findings_with_entries_timed(
                        raw.clone(),
                        &cached.suppressions,
                        stats,
                    ),
                );
            }
            cached.other = filter_suppressed_findings_with_entries_timed(
                cached.other_raw.clone(),
                &cached.suppressions,
                stats,
            );
            Self::refresh_active_owner_ranks(cached);
            cached.suppression_fingerprint = suppression_fingerprint(source);
            if let Some(raw) = self.global_raw.get(&file) {
                self.global.insert(
                    file,
                    filter_suppressed_findings_timed(raw.clone(), source, stats),
                );
            } else {
                self.global.remove(&file);
            }
            stats.suppression_files_refiltered += 1;
        }
    }

    fn replace_action_findings(
        &mut self,
        ws: &Workspace,
        warm: &FxHashSet<ModuleId>,
        sources: &FxHashMap<Ustr, &str>,
        affected: &FxHashSet<Ustr>,
        invalidation: Option<&ActionInvalidation>,
        stats: &mut LintCacheStats,
    ) {
        if invalidation.is_none() {
            stats.action_declarations_requested += affected.len();
            stats.action_declarations_revisited += affected.len();
        }
        let mut locations: FxHashSet<(Ustr, usize)> = affected
            .iter()
            .filter_map(|action| self.dependency.action_declarations.get(action))
            .map(|(file, span)| (*file, span.offset_range.0))
            .collect();
        if let Some(invalidation) = invalidation {
            locations.extend(invalidation.locations.iter().copied());
        }
        let population_file = invalidation
            .is_some()
            .then(|| self.action_population_file.take())
            .flatten();
        let mut removal_files = locations
            .iter()
            .map(|(file, _)| *file)
            .collect::<FxHashSet<_>>();
        removal_files.extend(population_file);
        for file in removal_files {
            if let Some(findings) = self.global.get_mut(&file) {
                findings.retain(|finding| {
                    finding.lint != ACTION_CALLERS
                        || (population_file != Some(file)
                            && !locations.contains(&(file, finding_offset(finding))))
                });
            }
            if let Some(findings) = self.global_raw.get_mut(&file) {
                findings.retain(|finding| {
                    finding.lint != ACTION_CALLERS
                        || (population_file != Some(file)
                            && !locations.contains(&(file, finding_offset(finding))))
                });
            }
        }

        let mut files = FxHashSet::default();
        if invalidation.is_some() && self.dependency.action_declarations.is_empty() {
            if let Some(module) = user_modules(ws, warm).find(|module| {
                is_production_action_module(module.file, self.dependency.action_policy)
            }) {
                if let Some((_, function)) = module.functions.iter().next() {
                    let file = module.file;
                    if sources.contains_key(&file) {
                        let raw = Finding::of(&ActionCallerGap {
                            action: Ustr::from("<none>"),
                            file,
                            span: function.span,
                            population_empty: true,
                            policy: self
                                .dependency
                                .action_policy
                                .expect("configured action policy"),
                        });
                        let kept = self.filter_cached_suppressions(file, vec![raw.clone()], stats);
                        self.global_raw.entry(file).or_default().push(raw.clone());
                        stats.global_findings_derived += kept.len();
                        self.global.entry(file).or_default().extend(kept);
                        self.action_population_file = Some(file);
                        files.insert(file);
                    }
                }
            }
        }

        let mut actions: Vec<Ustr> = affected.iter().copied().collect();
        actions.sort();
        for action in actions {
            if self
                .dependency
                .action_callers
                .get(&action)
                .is_some_and(|callers| !callers.is_empty())
            {
                continue;
            }
            let Some(&(file, span)) = self.dependency.action_declarations.get(&action) else {
                continue;
            };
            if !sources.contains_key(&file) {
                continue;
            }
            let raw = Finding::of(&ActionCallerGap {
                action,
                file,
                span,
                population_empty: false,
                policy: self
                    .dependency
                    .action_policy
                    .expect("configured action policy"),
            });
            let kept = self.filter_cached_suppressions(file, vec![raw.clone()], stats);
            self.global_raw.entry(file).or_default().push(raw.clone());
            stats.global_findings_derived += kept.len();
            self.global.entry(file).or_default().extend(kept);
            files.insert(file);
        }
        for file in files {
            if let Some(findings) = self.global.get_mut(&file) {
                findings.sort_by_key(|a| finding_offset(a));
            }
            if let Some(findings) = self.global_raw.get_mut(&file) {
                findings.sort_by_key(|a| finding_offset(a));
            }
        }
    }

    fn replace_sibling_findings(
        &mut self,
        ws: &Workspace,
        sources: &FxHashMap<Ustr, &str>,
        invalidation: &SiblingInvalidation,
        stats: &mut LintCacheStats,
    ) {
        for file in invalidation
            .locations
            .iter()
            .map(|(file, _)| *file)
            .collect::<FxHashSet<_>>()
        {
            if let Some(findings) = self.global.get_mut(&file) {
                findings.retain(|finding| {
                    finding.lint != BACKEND_SIBLING_MISMATCH
                        || !invalidation
                            .locations
                            .contains(&(file, finding_offset(finding)))
                });
            }
            if let Some(findings) = self.global_raw.get_mut(&file) {
                findings.retain(|finding| {
                    finding.lint != BACKEND_SIBLING_MISMATCH
                        || !invalidation
                            .locations
                            .contains(&(file, finding_offset(finding)))
                });
            }
        }

        let mut files = FxHashSet::default();
        for &name in &invalidation.names {
            let Some(decls) = ws.functions_by_name.get(&name) else {
                continue;
            };
            for (finding, file) in sibling_set_mismatches(ws, decls) {
                let raw = Finding::of(&finding);
                if !sources.contains_key(&file) {
                    continue;
                }
                let kept = self.filter_cached_suppressions(file, vec![raw.clone()], stats);
                files.insert(file);
                self.global_raw.entry(file).or_default().push(raw.clone());
                stats.global_findings_derived += kept.len();
                self.global.entry(file).or_default().extend(kept);
            }
        }
        for file in files {
            if let Some(findings) = self.global.get_mut(&file) {
                findings.sort_by(|a, b| {
                    finding_offset(a)
                        .cmp(&finding_offset(b))
                        .then_with(|| a.lint.cmp(b.lint))
                        .then_with(|| a.error.message.cmp(&b.error.message))
                });
            }
            if let Some(findings) = self.global_raw.get_mut(&file) {
                findings.sort_by(|a, b| {
                    finding_offset(a)
                        .cmp(&finding_offset(b))
                        .then_with(|| a.lint.cmp(b.lint))
                        .then_with(|| a.error.message.cmp(&b.error.message))
                });
            }
        }
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
        let mut stats = LintCacheStats {
            timed: self.timing_enabled,
            ..LintCacheStats::default()
        };
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
        let body_cacheable = cacheable_selection(sel);
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
            self.mark_body_transaction_applied();
            return (findings, stats);
        }

        let missing_cache_files: FxHashSet<Ustr> = change
            .changed_files
            .iter()
            .copied()
            .filter(|file| !self.files.contains_key(file))
            .collect();
        for &file in &missing_cache_files {
            let Some(source) = sources.get(&file).copied() else {
                continue;
            };
            let mut cached = CachedFileFindings {
                suppression_fingerprint: suppression_fingerprint(source),
                suppressions: suppression_entries_for_cache(source, &mut stats),
                ..Default::default()
            };
            if let Some(module) = ws.modules.iter().find(|module| module.file == file) {
                cached.owner_order = module.bodies.keys().copied().collect();
                cached.owner_rank = cached
                    .owner_order
                    .iter()
                    .enumerate()
                    .map(|(rank, &owner)| (owner, rank))
                    .collect();
            }
            self.files.insert(file, cached);
        }

        let suppression_changed_files: FxHashSet<Ustr> = change
            .changed_files
            .iter()
            .copied()
            .filter(|file| {
                let Some(source) = sources.get(file) else {
                    return false;
                };
                let old = self.files.get(file).map(|f| f.suppression_fingerprint);
                old.is_some_and(|old| old != suppression_fingerprint(source))
            })
            .collect();
        stats.suppression_invalidations = suppression_changed_files.len();
        stats.suppression_files_requested = suppression_changed_files.len();

        if change.changed_files.is_empty()
            && change.reinferred.is_empty()
            && change.edits.is_empty()
        {
            let findings = self.current_output(sources, &mut stats);
            self.mark_body_transaction_applied();
            return (findings, stats);
        }

        stats.body_updates = 1;
        let global_started = self.timing_enabled.then(Instant::now);
        self.warm = warm.clone();
        let declarations_changed = self
            .dependency
            .declarations_changed(ws, &change.changed_files);
        let sibling_invalidation = if declarations_changed && sel.enabled(BACKEND_SIBLING_MISMATCH)
        {
            Some(
                self.dependency
                    .update_sibling_declarations(ws, &change.changed_files, &mut stats),
            )
        } else {
            None
        };
        let action_invalidation = if sel.enabled(ACTION_CALLERS)
            && self
                .dependency
                .action_declarations_changed(ws, &change.changed_files)
        {
            Some(
                self.dependency
                    .update_action_declarations(ws, &change.changed_files, &mut stats),
            )
        } else {
            None
        };
        let unused_invalidation = if declarations_changed && sel.enabled(UNUSED_FN) {
            Some(
                self.dependency
                    .update_declarations(ws, warm, &change.changed_files, &mut stats),
            )
        } else {
            None
        };
        let mut affected_unused = if let Some(invalidation) = &unused_invalidation {
            invalidation.functions.clone()
        } else {
            FxHashSet::default()
        };
        if sel.enabled(UNUSED_FN) {
            affected_unused.extend(self.dependency.update_bodies(
                ws,
                &change.reinferred,
                &mut stats,
            ));
        } else if declarations_changed {
            stats.changed_declarations += change.changed_files.len();
        }
        if let Some(invalidation) = &unused_invalidation {
            affected_unused.extend(
                self.dependency
                    .recompute_seed_components(&invalidation.seeds, &mut stats),
            );
        }
        let declaration_count = self.dependency.function_nodes.len();
        if let Some(invalidation) = &unused_invalidation {
            stats.declarations_revisited += invalidation.functions.len();
            stats.declarations_reused +=
                declaration_count.saturating_sub(invalidation.functions.len());
        } else if declarations_changed {
            stats.declarations_revisited += change.changed_files.len();
            stats.declarations_reused +=
                declaration_count.saturating_sub(change.changed_files.len());
        } else {
            stats.declarations_reused += declaration_count;
        }
        let mut affected_actions = if !declarations_changed && sel.enabled(ACTION_CALLERS) {
            self.dependency
                .update_action_bodies(ws, &change.reinferred, &mut stats)
        } else {
            FxHashSet::default()
        };
        if let Some(invalidation) = &action_invalidation {
            affected_actions.extend(invalidation.names.iter().copied());
        }
        if let Some(started) = global_started {
            stats.global_lint_dependency_ns += started.elapsed().as_nanos();
        }
        let local_started = self.timing_enabled.then(Instant::now);
        let suppression_before_local = stats.suppression_filtering_ns;
        let variants = self.variants.clone();
        let suppression_only =
            !suppression_changed_files.is_empty() && change.reinferred.is_empty();
        let flat_enabled = sel.enabled(UNUSED_VAR) || sel.enabled(UNUSED_PURE_EXPR);
        let inference_enabled = sel.enabled(TRANSMUTE_OF_NULLABLE)
            || sel.enabled(TASK_NULL_ABANDON)
            || sel.enabled(ROW_ESCAPE);
        let mut flat_targets: FxHashSet<(ModuleId, BodyOwner)> = FxHashSet::default();
        let mut inference_targets: FxHashSet<(ModuleId, BodyOwner)> = if inference_enabled {
            change.reinferred.iter().copied().collect()
        } else {
            FxHashSet::default()
        };
        for file in &change.changed_files {
            let Some(module) = ws.modules.iter().find(|m| m.file == *file) else {
                continue;
            };
            let owners: Vec<BodyOwner> = module.bodies.keys().copied().collect();
            if flat_enabled
                && !suppression_only
                && (missing_cache_files.contains(file)
                    || !change.reinferred.iter().any(|(mid, _)| *mid == module.id))
            {
                flat_targets.extend(owners.iter().copied().map(|owner| (module.id, owner)));
            }
            if inference_enabled
                && !suppression_only
                && (missing_cache_files.contains(file)
                    || !change.reinferred.iter().any(|(mid, _)| *mid == module.id))
            {
                inference_targets.extend(owners.into_iter().map(|owner| (module.id, owner)));
            }
        }
        if flat_enabled {
            flat_targets.extend(
                change
                    .reinferred
                    .iter()
                    .copied()
                    .filter(|(mid, _)| change.changed_files.contains(&ws.module(*mid).file)),
            );
        }
        let flat_target_owners = flat_targets.clone();
        let inference_target_owners = inference_targets.clone();

        let mut dirty_output_files = change.changed_files.clone();
        dirty_output_files.extend(
            flat_target_owners
                .iter()
                .chain(inference_target_owners.iter())
                .map(|(mid, _)| ws.module(*mid).file),
        );
        dirty_output_files.extend(affected_unused.iter().map(|(mid, _)| ws.module(*mid).file));
        if let Some(invalidation) = &unused_invalidation {
            dirty_output_files.extend(invalidation.locations.iter().map(|(file, _)| *file));
        }
        dirty_output_files.extend(affected_actions.iter().filter_map(|action| {
            self.dependency
                .action_declarations
                .get(action)
                .map(|(file, _)| *file)
        }));
        if let Some(invalidation) = &action_invalidation {
            dirty_output_files.extend(invalidation.locations.iter().map(|(file, _)| *file));
            dirty_output_files.extend(self.action_population_file);
        }
        if let Some(invalidation) = &sibling_invalidation {
            dirty_output_files.extend(invalidation.locations.iter().map(|(file, _)| *file));
        }
        dirty_output_files.extend(suppression_changed_files.iter().copied());

        let total_flat_bodies = self.flat_body_entries;
        let total_inference_bodies = self.inference_body_entries;
        let total_flat_findings = self.flat_finding_entries;
        let total_inference_findings = self.inference_finding_entries;
        stats.reuse_totals_reads += 4;
        let targeted_flat_findings = flat_targets
            .iter()
            .filter_map(|(mid, owner)| self.files.get(&ws.module(*mid).file)?.flat.get(owner))
            .map(Vec::len)
            .sum::<usize>();
        let targeted_inference_findings = inference_targets
            .iter()
            .filter_map(|(mid, owner)| self.files.get(&ws.module(*mid).file)?.inference.get(owner))
            .map(Vec::len)
            .sum::<usize>();
        let mut flat_bodies_added = 0;
        let mut flat_findings_removed = 0;
        let mut flat_findings_added = 0;
        for (mid, owner) in flat_targets {
            let file = ws.module(mid).file;
            if !self.report_files.contains(&file) {
                continue;
            }
            let Some(body) = ws.module(mid).bodies.get(&owner) else {
                continue;
            };
            let mut raw = Vec::new();
            if sel.enabled(UNUSED_VAR) {
                raw.extend(
                    unused_vars_in_body(body, &variants)
                        .into_iter()
                        .map(|f| Finding::of(&f)),
                );
            }
            if sel.enabled(UNUSED_PURE_EXPR) {
                raw.extend(
                    unused_pure_exprs_in_body(body)
                        .into_iter()
                        .map(|f| Finding::of(&f)),
                );
            }
            stats.flat_body_requests += 1;
            stats.flat_findings_derived += raw.len();
            let filtered = {
                let suppressions = &self.files.entry(file).or_default().suppressions;
                filter_suppressed_findings_with_entries_timed(raw.clone(), suppressions, &mut stats)
            };
            let cached = self.files.entry(file).or_default();
            if !cached.owner_order.contains(&owner) {
                cached.owner_rank.insert(owner, cached.owner_order.len());
                cached.owner_order.push(owner);
            }
            cached.flat_raw.insert(owner, raw);
            if !cached.flat.contains_key(&owner) {
                flat_bodies_added += 1;
            }
            flat_findings_removed += cached.flat.get(&owner).map_or(0, Vec::len);
            flat_findings_added += filtered.len();
            cached.flat.insert(owner, filtered);
            let rank = cached.owner_rank[&owner];
            if cached.flat[&owner].is_empty() {
                cached.active_flat_ranks.remove(&rank);
            } else {
                cached.active_flat_ranks.insert(rank);
            }
        }
        self.flat_body_entries += flat_bodies_added;
        self.flat_finding_entries = self
            .flat_finding_entries
            .saturating_sub(flat_findings_removed)
            + flat_findings_added;
        let mut inference_bodies_added = 0;
        let mut inference_findings_removed = 0;
        let mut inference_findings_added = 0;
        for (mid, owner) in inference_targets {
            let file = ws.module(mid).file;
            if !self.report_files.contains(&file) {
                continue;
            }
            let module = ws.module(mid);
            let Some(body) = module.bodies.get(&owner) else {
                continue;
            };
            let inf = ws.inference(mid);
            let bi = inf.and_then(|i| i.per_fn.get(&owner));
            let mut raw = Vec::new();
            if sel.enabled(TRANSMUTE_OF_NULLABLE) {
                raw.extend(
                    transmute_of_nullable_in_body(ws, bi, body)
                        .into_iter()
                        .map(|f| Finding::of(&f)),
                );
            }
            if sel.enabled(TASK_NULL_ABANDON) {
                raw.extend(
                    task_null_abandon_in_body(ws, bi, body)
                        .into_iter()
                        .map(|f| Finding::of(&f)),
                );
            }
            if sel.enabled(ROW_ESCAPE) {
                raw.extend(
                    row_escapes_in_body(ws, bi, body, tail_sink(module, &owner))
                        .into_iter()
                        .map(|f| Finding::of(&f)),
                );
            }
            stats.inference_body_requests += 1;
            stats.inference_findings_derived += raw.len();
            let filtered = {
                let suppressions = &self.files.entry(file).or_default().suppressions;
                filter_suppressed_findings_with_entries_timed(raw.clone(), suppressions, &mut stats)
            };
            let cached = self.files.entry(file).or_default();
            if !cached.owner_order.contains(&owner) {
                cached.owner_rank.insert(owner, cached.owner_order.len());
                cached.owner_order.push(owner);
            }
            cached.inference_raw.insert(owner, raw);
            if !cached.inference.contains_key(&owner) {
                inference_bodies_added += 1;
            }
            inference_findings_removed += cached.inference.get(&owner).map_or(0, Vec::len);
            inference_findings_added += filtered.len();
            cached.inference.insert(owner, filtered);
            let rank = cached.owner_rank[&owner];
            if cached.inference[&owner].is_empty() {
                cached.active_inference_ranks.remove(&rank);
            } else {
                cached.active_inference_ranks.insert(rank);
            }
        }
        self.inference_body_entries += inference_bodies_added;
        self.inference_finding_entries = self
            .inference_finding_entries
            .saturating_sub(inference_findings_removed)
            + inference_findings_added;
        if let Some(started) = local_started {
            let suppression = stats
                .suppression_filtering_ns
                .saturating_sub(suppression_before_local);
            stats.local_lint_derivation_ns +=
                started.elapsed().as_nanos().saturating_sub(suppression);
        }
        for file in &change.changed_files {
            if let Some(source) = sources.get(file).copied() {
                if let Some(cached) = self.files.get_mut(file) {
                    cached.suppression_fingerprint = suppression_fingerprint(source);
                }
            }
        }
        stats.flat_bodies_reused += total_flat_bodies.saturating_sub(flat_target_owners.len());
        stats.flat_findings_reused += total_flat_findings.saturating_sub(targeted_flat_findings);
        stats.inference_bodies_reused +=
            total_inference_bodies.saturating_sub(inference_target_owners.len());
        stats.inference_findings_reused +=
            total_inference_findings.saturating_sub(targeted_inference_findings);

        for edit in &change.edits {
            let Some(cached) = self.files.get_mut(&edit.file) else {
                continue;
            };
            for (owner, findings) in &mut cached.flat {
                if !flat_target_owners.contains(&(
                    ws.modules
                        .iter()
                        .find(|m| m.file == edit.file)
                        .map_or(ModuleId::builtin(), |m| m.id),
                    *owner,
                )) {
                    stats.span_shifts += shift_findings(findings, edit);
                }
            }
            for (owner, findings) in &mut cached.flat_raw {
                if !flat_target_owners.contains(&(
                    ws.modules
                        .iter()
                        .find(|m| m.file == edit.file)
                        .map_or(ModuleId::builtin(), |m| m.id),
                    *owner,
                )) {
                    shift_findings(findings, edit);
                }
            }
            for (owner, findings) in &mut cached.inference {
                if !inference_target_owners.contains(&(
                    ws.modules
                        .iter()
                        .find(|m| m.file == edit.file)
                        .map_or(ModuleId::builtin(), |m| m.id),
                    *owner,
                )) {
                    stats.span_shifts += shift_findings(findings, edit);
                }
            }
            for (owner, findings) in &mut cached.inference_raw {
                if !inference_target_owners.contains(&(
                    ws.modules
                        .iter()
                        .find(|m| m.file == edit.file)
                        .map_or(ModuleId::builtin(), |m| m.id),
                    *owner,
                )) {
                    shift_findings(findings, edit);
                }
            }
            shift_findings(&mut cached.other_raw, edit);
            stats.span_shifts += shift_findings(&mut cached.other, edit);
            let global_owners = self
                .global_span_owners
                .get(&edit.file)
                .cloned()
                .unwrap_or_default();
            for file in global_owners {
                let shifted = self
                    .global
                    .get_mut(&file)
                    .map_or(0, |findings| shift_findings(findings, edit));
                stats.span_shifts += shifted;
                if shifted > 0 {
                    dirty_output_files.insert(file);
                }
                if let Some(findings) = self.global_raw.get_mut(&file) {
                    shift_findings(findings, edit);
                }
            }
        }

        let global_findings_started = self.timing_enabled.then(Instant::now);
        let suppression_before_global = stats.suppression_filtering_ns;
        if is_bin
            && sel.enabled(UNUSED_FN)
            && (!affected_unused.is_empty() || unused_invalidation.is_some())
        {
            let extra_locations = unused_invalidation
                .as_ref()
                .map(|invalidation| invalidation.locations.clone())
                .unwrap_or_default();
            self.replace_unused_findings(
                ws,
                warm,
                sources,
                &affected_unused,
                &extra_locations,
                &mut stats,
            );
        }
        if stats.global_passes == 0
            && sel.enabled(ACTION_CALLERS)
            && (!affected_actions.is_empty() || action_invalidation.is_some())
        {
            self.replace_action_findings(
                ws,
                warm,
                sources,
                &affected_actions,
                action_invalidation.as_ref(),
                &mut stats,
            );
            dirty_output_files.extend(self.action_population_file);
        }
        if stats.global_passes == 0 {
            if let Some(invalidation) = sibling_invalidation {
                self.replace_sibling_findings(ws, sources, &invalidation, &mut stats);
            }
        }
        if let Some(started) = global_findings_started {
            let suppression = stats
                .suppression_filtering_ns
                .saturating_sub(suppression_before_global);
            stats.global_lint_derivation_ns +=
                started.elapsed().as_nanos().saturating_sub(suppression);
        }
        if !suppression_changed_files.is_empty() {
            let before_flat = suppression_changed_files
                .iter()
                .filter_map(|file| self.files.get(file))
                .map(|file| file.flat.values().map(Vec::len).sum::<usize>())
                .sum::<usize>();
            let before_inference = suppression_changed_files
                .iter()
                .filter_map(|file| self.files.get(file))
                .map(|file| file.inference.values().map(Vec::len).sum::<usize>())
                .sum::<usize>();
            self.refilter_suppression_files(&suppression_changed_files, sources, &mut stats);
            let after_flat = suppression_changed_files
                .iter()
                .filter_map(|file| self.files.get(file))
                .map(|file| file.flat.values().map(Vec::len).sum::<usize>())
                .sum::<usize>();
            let after_inference = suppression_changed_files
                .iter()
                .filter_map(|file| self.files.get(file))
                .map(|file| file.inference.values().map(Vec::len).sum::<usize>())
                .sum::<usize>();
            self.flat_finding_entries =
                self.flat_finding_entries.saturating_sub(before_flat) + after_flat;
            self.inference_finding_entries = self
                .inference_finding_entries
                .saturating_sub(before_inference)
                + after_inference;
        }
        for file in &dirty_output_files {
            if let Some(findings) = self.global.get_mut(file) {
                sort_global_findings(findings);
            }
            if let Some(findings) = self.global_raw.get_mut(file) {
                sort_global_findings(findings);
            }
        }
        self.refresh_global_span_index(&dirty_output_files);
        self.refresh_output_files(&dirty_output_files, sources, &mut stats);
        let findings = self.current_output(sources, &mut stats);
        self.mark_body_transaction_applied();
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
        let raw_findings = lint_workspace_unsuppressed(ws, warm, is_bin, sources, sel);
        let findings = filter_workspace_findings(raw_findings.clone(), sources);
        self.initialized = true;
        self.is_bin = is_bin;
        self.warm = warm.clone();
        self.variants = enum_variant_names(ws, warm);
        self.workspace_files = workspace_files;
        self.report_files = report_files;
        self.report_order = user_modules(ws, warm)
            .filter(|module| sources.contains_key(&module.file))
            .map(|module| module.file)
            .collect();
        self.report_rank = self
            .report_order
            .iter()
            .enumerate()
            .map(|(rank, &file)| (file, rank))
            .collect();
        self.selection = Some(sel.clone());
        self.files.clear();
        self.global_raw.clear();
        self.global.clear();
        self.global_span_owners.clear();
        self.global_referenced_files.clear();
        self.dependency =
            LintDependencyIndex::build(ws, warm, sel.enabled(ACTION_CALLERS), sel.action_policy);
        stats.declarations_revisited += self.dependency.function_nodes.len();
        stats.sibling_groups_requested += self.dependency.sibling_groups.len();
        stats.sibling_groups_revisited += self.dependency.sibling_groups.len();
        stats.action_declarations_requested += self.dependency.action_declarations.len();
        stats.action_declarations_revisited += self.dependency.action_declarations.len();
        stats.action_callers_requested += self.dependency.action_body_edges.len();
        stats.action_callers_revisited += self.dependency.action_body_edges.len();
        for (&file, source) in sources {
            self.files.insert(
                file,
                CachedFileFindings {
                    suppression_fingerprint: suppression_fingerprint(source),
                    suppressions: suppression_entries_for_cache(source, stats),
                    ..Default::default()
                },
            );
        }
        for finding in &raw_findings {
            if is_global_lint(finding.lint) {
                self.global_raw
                    .entry(finding.file())
                    .or_default()
                    .push(finding.clone());
                continue;
            }
            let file = finding.file();
            let Some(module) = ws.modules.iter().find(|m| m.file == file) else {
                continue;
            };
            let owner = finding_owner(module, finding).map(|owner| {
                let cached = self.files.entry(file).or_default();
                if is_inference_lint(finding.lint) {
                    &mut cached.inference_raw
                } else {
                    &mut cached.flat_raw
                }
                .entry(owner)
                .or_default()
                .push(finding.clone());
            });
            if owner.is_none() {
                self.files
                    .entry(file)
                    .or_default()
                    .other_raw
                    .push(finding.clone());
            }
        }
        for module in user_modules(ws, warm) {
            if !sources.contains_key(&module.file) {
                continue;
            }
            let cached = self.files.entry(module.file).or_default();
            cached.owner_order = module.bodies.keys().copied().collect();
            cached.owner_rank = cached
                .owner_order
                .iter()
                .enumerate()
                .map(|(rank, &owner)| (owner, rank))
                .collect();
            for owner in module.bodies.keys().copied() {
                cached.flat_raw.entry(owner).or_default();
                cached.inference_raw.entry(owner).or_default();
                let flat = cached.flat_raw.get(&owner).cloned().unwrap_or_default();
                let inference = cached
                    .inference_raw
                    .get(&owner)
                    .cloned()
                    .unwrap_or_default();
                cached.flat.insert(
                    owner,
                    filter_suppressed_findings_with_entries_timed(
                        flat,
                        &cached.suppressions,
                        stats,
                    ),
                );
                cached.inference.insert(
                    owner,
                    filter_suppressed_findings_with_entries_timed(
                        inference,
                        &cached.suppressions,
                        stats,
                    ),
                );
            }
            cached.other = filter_suppressed_findings_with_entries_timed(
                cached.other_raw.clone(),
                &cached.suppressions,
                stats,
            );
            Self::refresh_active_owner_ranks(cached);
        }
        for (&file, raw) in &self.global_raw {
            let Some(source) = sources.get(&file).copied() else {
                continue;
            };
            self.global.insert(
                file,
                filter_suppressed_findings_timed(raw.clone(), source, stats),
            );
        }
        self.action_population_file = self
            .dependency
            .action_declarations
            .is_empty()
            .then(|| {
                self.global_raw.iter().find_map(|(&file, findings)| {
                    findings
                        .iter()
                        .any(|finding| finding.lint == ACTION_CALLERS)
                        .then_some(file)
                })
            })
            .flatten();
        let global_files = self.global_raw.keys().copied().collect();
        self.refresh_global_span_index(&global_files);
        self.flat_body_entries = self.files.values().map(|file| file.flat.len()).sum();
        self.inference_body_entries = self.files.values().map(|file| file.inference.len()).sum();
        self.flat_finding_entries = self
            .files
            .values()
            .map(|file| file.flat.values().map(Vec::len).sum::<usize>())
            .sum();
        self.inference_finding_entries = self
            .files
            .values()
            .map(|file| file.inference.values().map(Vec::len).sum::<usize>())
            .sum();
        self.last_by_file = by_file_from_findings(&findings);
        self.active_output_ranks = self
            .last_by_file
            .keys()
            .filter_map(|file| self.report_rank.get(file).copied())
            .collect();
        stats.full_rebuilds = 1;
        stats.full_lint_workspace_passes = 1;
        stats.full_cache_reconstructions = 1;
        findings
    }

    fn refresh_output_files(
        &mut self,
        files: &FxHashSet<Ustr>,
        sources: &FxHashMap<Ustr, &str>,
        stats: &mut LintCacheStats,
    ) {
        stats.output_refresh_requests += 1;
        for &file in files {
            let rank = self.report_rank.get(&file).copied();
            if !sources.contains_key(&file) {
                self.last_by_file.remove(&file);
                if let Some(rank) = rank {
                    self.active_output_ranks.remove(&rank);
                }
                continue;
            }
            let Some(cached) = self.files.get(&file) else {
                self.last_by_file.remove(&file);
                if let Some(rank) = rank {
                    self.active_output_ranks.remove(&rank);
                }
                continue;
            };
            stats.output_files_refreshed += 1;
            let mut findings = Vec::new();
            for rank in &cached.active_flat_ranks {
                stats.output_owner_entries_visited += 1;
                let owner = &cached.owner_order[*rank];
                findings.extend(cached.flat.get(owner).cloned().unwrap_or_default());
            }
            for rank in &cached.active_inference_ranks {
                stats.output_owner_entries_visited += 1;
                let owner = &cached.owner_order[*rank];
                findings.extend(cached.inference.get(owner).cloned().unwrap_or_default());
            }
            findings.extend(cached.other.clone());
            findings.extend(self.global.get(&file).cloned().unwrap_or_default());
            if findings.is_empty() {
                self.last_by_file.remove(&file);
                if let Some(rank) = rank {
                    self.active_output_ranks.remove(&rank);
                }
            } else {
                self.last_by_file.insert(file, findings);
                if let Some(rank) = rank {
                    self.active_output_ranks.insert(rank);
                }
            }
        }
    }

    fn current_output(
        &self,
        sources: &FxHashMap<Ustr, &str>,
        stats: &mut LintCacheStats,
    ) -> Vec<Finding> {
        let mut out = Vec::new();
        for rank in &self.active_output_ranks {
            stats.output_file_rows_read += 1;
            let file = &self.report_order[*rank];
            if !sources.contains_key(file) {
                continue;
            }
            out.extend(self.last_by_file.get(file).cloned().unwrap_or_default());
        }
        out
    }
}

fn is_global_lint(lint: &str) -> bool {
    matches!(lint, UNUSED_FN | BACKEND_SIBLING_MISMATCH | ACTION_CALLERS)
}

fn cacheable_selection(sel: &LintOptions) -> bool {
    (sel.only.is_none() && sel.only_types.is_none() && sel.ignore_types.is_empty())
        || sel.only.as_ref().is_some_and(|names| {
            names.len() == 1
                && ((names[0] == ACTION_CALLERS && sel.action_policy.is_some())
                    || names[0] == BACKEND_SIBLING_MISMATCH)
        })
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

fn sort_global_findings(findings: &mut Vec<Finding>) {
    findings.sort_by(|a, b| {
        global_family_rank(a.lint)
            .cmp(&global_family_rank(b.lint))
            .then_with(|| finding_offset(a).cmp(&finding_offset(b)))
            .then_with(|| a.error.message.cmp(&b.error.message))
    });
}

fn global_family_rank(lint: &str) -> usize {
    match lint {
        UNUSED_FN => 0,
        BACKEND_SIBLING_MISMATCH => 1,
        ACTION_CALLERS => 2,
        _ => 3,
    }
}

fn finding_owner(module: &Module, finding: &Finding) -> Option<BodyOwner> {
    module
        .item_index()
        .find(
            finding_offset(finding),
            finding_offset(finding).saturating_add(1),
        )
        .min_by_key(|interval| interval.stop - interval.start)
        .map(|interval| interval.val)
}

fn by_file_from_findings(findings: &[Finding]) -> FxHashMap<Ustr, Vec<Finding>> {
    let mut by_file = FxHashMap::default();
    for finding in findings {
        by_file
            .entry(finding.file())
            .or_insert_with(Vec::new)
            .push(finding.clone());
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
        if finding.error.location.file == edit.file && finding.error.location.offset >= edit.at {
            finding.error.location.offset =
                shift_value(finding.error.location.offset, edit.byte_delta);
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
    if sel.action_policy.is_none() {
        for lint in [
            ACTION_CALLERS,
            RAW_INPUT_MIGRATION,
            RAW_INPUT_CLASSIFICATION,
        ] {
            if sel.enabled(lint) {
                out.push(Finding {
                    lint,
                    error: CompilerError::new_with_label(
                        Span {
                            file: module.file,
                            ..Span::unknown()
                        },
                        "no action-layer policy configured".to_owned(),
                    )
                    .with_level(CompilerErrorLevel::Advice),
                    line: 0,
                });
            }
        }
    }

    if sel.enabled(UNUSED_VAR) {
        let found = unused_vars_in_module(module, variants);
        out.extend(
            filter_suppressed_census(found, src, census.as_deref_mut())
                .iter()
                .map(Finding::of),
        );
    }
    if sel.enabled(UNUSED_PURE_EXPR) {
        let found: Vec<_> = module
            .bodies
            .values()
            .flat_map(unused_pure_exprs_in_body)
            .collect();
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
    if sel.enabled(RAW_INPUT_MIGRATION)
        && let Some(policy) = sel.action_policy
    {
        let found = raw_input_sites_in_module(module, policy);
        out.extend(
            filter_suppressed_census(found, src, census.as_deref_mut())
                .iter()
                .map(Finding::of),
        );
    }
    if sel.enabled(RAW_INPUT_CLASSIFICATION)
        && let Some(policy) = sel.action_policy
    {
        let found = unclassified_raw_input_sites_in_module(module, src, policy);
        out.extend(
            filter_suppressed_census(found, src, census)
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
            filter_suppressed_census(found, src, census)
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
/// `backend_sibling_mismatch` is opt-in because JS is an optional backend; use
/// `LintOptions::only` or `rock lint --lints=backend_sibling_mismatch` when
/// auditing parity explicitly.
pub fn workspace_findings_by_file(
    ws: &Workspace,
    warm: &FxHashSet<ModuleId>,
    is_bin: bool,
) -> FxHashMap<Ustr, Vec<Finding>> {
    workspace_findings_by_file_with(ws, warm, is_bin, &LintOptions::default())
}

pub fn workspace_findings_by_file_with(
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
        let t = bone_core::prelude::Instant::now();
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
            out.entry(file)
                .or_default()
                .extend(findings.iter().map(Finding::of));
        }
    }
    if !sel.enabled(BACKEND_SIBLING_MISMATCH) {
        if sel.enabled(ACTION_CALLERS)
            && let Some(policy) = sel.action_policy
        {
            let t = bone_core::prelude::Instant::now();
            let findings = action_caller_gaps(ws, warm, policy);
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
                out.entry(finding.file)
                    .or_default()
                    .push(Finding::of(&finding));
            }
        }
        return out;
    }
    // Warm modules are SCANNED (half of every backend sibling set is stdlib) but
    // not reported on, like every other lint's treatment of stdlib files.
    let warm_files: FxHashSet<Ustr> = warm.iter().map(|&mid| ws.module(mid).file).collect();
    let t = bone_core::prelude::Instant::now();
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
        out.entry(file)
            .or_default()
            .extend(findings.iter().map(Finding::of));
    }
    if sel.enabled(ACTION_CALLERS)
        && let Some(policy) = sel.action_policy
    {
        let t = bone_core::prelude::Instant::now();
        let findings = action_caller_gaps(ws, warm, policy);
        record_family(
            census,
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
            out.entry(finding.file)
                .or_default()
                .push(Finding::of(&finding));
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
    lint_workspace_inner(ws, warm, is_bin, sources, sel, true, None)
}

fn lint_workspace_unsuppressed(
    ws: &Workspace,
    warm: &FxHashSet<ModuleId>,
    is_bin: bool,
    sources: &FxHashMap<Ustr, &str>,
    sel: &LintOptions,
) -> Vec<Finding> {
    lint_workspace_inner(ws, warm, is_bin, sources, sel, false, None)
}

pub fn lint_workspace_with_census(
    ws: &Workspace,
    warm: &FxHashSet<ModuleId>,
    is_bin: bool,
    sources: &FxHashMap<Ustr, &str>,
    sel: &LintOptions,
) -> (Vec<Finding>, LintCensus) {
    let mut census = LintCensus::default();
    let findings = lint_workspace_inner(ws, warm, is_bin, sources, sel, true, Some(&mut census));
    (findings, census)
}

fn lint_workspace_inner(
    ws: &Workspace,
    warm: &FxHashSet<ModuleId>,
    is_bin: bool,
    sources: &FxHashMap<Ustr, &str>,
    sel: &LintOptions,
    suppress: bool,
    mut census: Option<&mut LintCensus>,
) -> Vec<Finding> {
    if let Some(census) = census.as_deref_mut() {
        census.full_workspace_passes += 1;
    }
    let mut by_file =
        workspace_findings_by_file_census(ws, warm, is_bin, sel, census.as_deref_mut());
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
        let Some(&src) = sources.get(&module.file) else {
            continue;
        };
        let t_flat = bone_core::prelude::Instant::now();
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

        let t_inference = bone_core::prelude::Instant::now();
        let inference =
            inference_findings_with(ws, warm, module.id, src, sel, census.as_deref_mut());
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
            if suppress {
                out.extend(filter_suppressed_findings_census(
                    findings,
                    src,
                    census.as_deref_mut(),
                ));
            } else {
                out.extend(findings);
            }
        }
    }
    out
}

/// [`filter_suppressed`] for already-rendered [`Finding`]s, matching each one's
/// own lint name. Same rule: the finding's line or the line above it.
pub fn filter_suppressed_findings(findings: Vec<Finding>, source: &str) -> Vec<Finding> {
    filter_suppressed_findings_census(findings, source, None)
}

fn filter_suppressed_findings_timed(
    findings: Vec<Finding>,
    source: &str,
    stats: &mut LintCacheStats,
) -> Vec<Finding> {
    stats.suppression_source_scans += 1;
    if !stats.timed {
        return filter_suppressed_findings(findings, source);
    }
    let started = Instant::now();
    let filtered = filter_suppressed_findings(findings, source);
    stats.suppression_filtering_ns += started.elapsed().as_nanos();
    stats.suppression_filter_calls += 1;
    filtered
}

fn filter_suppressed_findings_with_entries_timed(
    findings: Vec<Finding>,
    allow: &FxHashSet<(usize, &'static str)>,
    stats: &mut LintCacheStats,
) -> Vec<Finding> {
    let started = stats.timed.then(Instant::now);
    let filtered = filter_suppressed_findings_with_entries(findings, allow);
    if let Some(started) = started {
        stats.suppression_filtering_ns += started.elapsed().as_nanos();
        stats.suppression_filter_calls += 1;
    }
    filtered
}

fn suppression_entries(source: &str) -> FxHashSet<(usize, &'static str)> {
    let mut allow = FxHashSet::default();
    for (n, line) in source.lines().enumerate() {
        for &lint in KNOWN_LINTS {
            if parse_allow_comment(line, lint) {
                allow.insert((n, lint));
            }
        }
    }
    allow
}

fn suppression_entries_for_cache(
    source: &str,
    stats: &mut LintCacheStats,
) -> FxHashSet<(usize, &'static str)> {
    stats.suppression_entry_parses += 1;
    suppression_entries(source)
}

fn filter_suppressed_findings_with_entries(
    findings: Vec<Finding>,
    allow: &FxHashSet<(usize, &'static str)>,
) -> Vec<Finding> {
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

fn filter_suppressed_findings_census(
    findings: Vec<Finding>,
    source: &str,
    census: Option<&mut LintCensus>,
) -> Vec<Finding> {
    let raw = findings.len();
    let started = bone_core::prelude::Instant::now();
    let allow = suppression_entries(source);
    let filtered = filter_suppressed_findings_with_entries(findings, &allow);
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

fn filter_workspace_findings(
    findings: Vec<Finding>,
    sources: &FxHashMap<Ustr, &str>,
) -> Vec<Finding> {
    let mut out = Vec::with_capacity(findings.len());
    for finding in findings {
        let Some(source) = sources.get(&finding.file()).copied() else {
            continue;
        };
        out.extend(filter_suppressed_findings(vec![finding], source));
    }
    out
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
            format!(
                "unused variable `{}` (prefix with `_` to silence)",
                self.name
            ),
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
            BackendSiblingIssue::ReturnDiffers {
                other_backend,
                other_ret,
            } => format!(
                "returns a different type than its `{other_backend}` sibling (`{other_ret}`) -- \
                 overloads key on params, so these are one overload with two answers"
            ),
        };
        CompilerError::new_with_label(
            self.span,
            format!(
                "backend sibling mismatch: `{}{}` {detail}",
                self.name, self.signature
            ),
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
    let started = bone_core::prelude::Instant::now();
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
/// a misspelled name simply won't match.
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
    let Some(s) = s.trim_start().strip_prefix("rock") else {
        return false;
    };
    let Some(s) = s.trim_start().strip_prefix(':') else {
        return false;
    };
    let Some(s) = s.trim_start().strip_prefix("allow") else {
        return false;
    };
    let Some(s) = s.trim_start().strip_prefix('(') else {
        return false;
    };
    let Some(rest) = s.trim_start().strip_prefix(lint_name) else {
        return false;
    };
    matches!(
        rest.chars().next(),
        None | Some(' ') | Some('\t') | Some(',') | Some(')')
    )
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
    ReturnDiffers {
        other_backend: &'static str,
        other_ret: String,
    },
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
#[derive(Clone, Debug)]
struct SiblingDecl {
    name: Ustr,
    signature: String,
    ret: String,
    backend: FnBackend,
    span: Span,
    file: Ustr,
}

fn sibling_decl(ws: &Workspace, mid: ModuleId, fid: Idx<Function>) -> SiblingDecl {
    let module = ws.module(mid);
    let function = &module.functions[fid];
    SiblingDecl {
        name: function.name,
        signature: format!(
            "({})",
            function
                .param_decls
                .iter()
                .map(|p| ws.render(None, p.ty))
                .join(", ")
        ),
        ret: function
            .return_ty
            .map(|ty| ws.render(None, ty))
            .unwrap_or_else(|| "()".to_string()),
        backend: function.backend,
        span: function.name_span,
        file: module.file,
    }
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
/// Stdlib decls participate (half of every backend sibling set lives there);
/// findings are attributed to the file of the offending decl.
pub fn backend_sibling_mismatches(ws: &Workspace) -> Vec<BackendSiblingMismatch> {
    backend_sibling_mismatches_by_file(ws)
        .into_values()
        .flatten()
        .collect()
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
        let Some(b) = backend_name(ws.module(mid).functions[fid].backend) else {
            continue;
        };
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
        .map(|&(mid, fid)| sibling_decl(ws, mid, fid))
        .collect();

    let concrete: Vec<&'static str> = infos
        .iter()
        .filter_map(|d| backend_name(d.backend))
        .unique()
        .collect();
    if concrete.len() < 2 {
        return Vec::new();
    }

    let mut out = Vec::new();
    for d in &infos {
        let Some(here) = backend_name(d.backend) else {
            continue;
        };
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
    unused_fns_by_file(ws, &FxHashSet::default())
        .into_values()
        .flatten()
        .collect()
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
                out.entry(module.file).or_default().push(UnusedFn {
                    name: f.name,
                    span: f.name_span,
                });
            }
        }
    }
    out
}

/// A function is a reachability root iff it can be entered from outside the
/// in-project call graph. See `unused_fns` for the rationale on each arm.
fn is_root(f: &Function) -> bool {
    f.is_extern
        || declared_name(f.name.as_str()) == "main"
        || f.annotations
            .iter()
            .any(|a| !is_implementation_anno(a.name.name.as_str()))
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
            | "scene_type_key"
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
            Expr::Call {
                callee: Callee::Unresolved { name, .. },
                ..
            } => resolve(name, &mut out),
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
        .flat_map(|module| unused_vars_in_module(module, &variants))
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
        let Some(is_comparison) = reportable_root(body, *e) else {
            continue;
        };
        if is_pure(body, *e) {
            out.push(UnusedPureExpr {
                span: body.expr_spans[*e],
                is_comparison,
            });
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

        Expr::Not(x, _) | Expr::BitNot(x) | Expr::UnaryMinus(x, _) => is_pure(body, *x),
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
/// null-on-failure (e.g. `host_load_image` -> `int?`) into an apparently-valid
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
        // `take_result` in a host library), so unlike `entity_across_yield` the allow
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
        let Expr::Call {
            callee: Callee::Unresolved { name, .. },
            args,
            ..
        } = expr
        else {
            continue;
        };
        if (*name != cast && *name != transmute) || args.len() != 1 {
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

/// Direct input observations are the census population. Action calls, input
/// event injection, and tool-input helpers are intentionally separate APIs and
/// therefore do not appear here.
pub fn raw_input_sites_in_module(module: &Module, policy: &ActionLayerPolicy) -> Vec<RawInputSite> {
    let mut out = Vec::new();
    for body in module.bodies.values() {
        for (idx, expr) in body.expr_arena.iter() {
            let Expr::Call {
                callee: Callee::Unresolved { name, .. },
                ..
            } = expr
            else {
                continue;
            };
            if policy.raw_input_names.contains(&name.as_str()) {
                out.push(RawInputSite {
                    name: *name,
                    span: body.expr_spans[idx],
                });
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

fn raw_input_class_marker<'a>(
    source: &'a str,
    line: usize,
    policy: &ActionLayerPolicy,
) -> Option<&'a str> {
    let marker_line = source.lines().nth(line.saturating_sub(1))?.trim();
    let class = marker_line.strip_prefix("// action-layer:")?.trim();
    if class.is_empty() || class.split_whitespace().count() != 1 {
        return None;
    }
    policy.raw_input_classes.contains(&class).then_some(class)
}

#[derive(Clone, Debug)]
pub struct RawInputClassificationGap {
    pub name: Ustr,
    pub span: Span,
    pub line_population: usize,
    pub classes: &'static [&'static str],
}

pub fn unclassified_raw_input_sites_in_module(
    module: &Module,
    source: &str,
    policy: &'static ActionLayerPolicy,
) -> Vec<RawInputClassificationGap> {
    let sites = raw_input_sites_in_module(module, policy);
    let mut sites_per_line: FxHashMap<usize, usize> = FxHashMap::default();
    for site in &sites {
        *sites_per_line.entry(site.span.line_range.0).or_default() += 1;
    }
    sites
        .into_iter()
        .filter_map(|site| {
            let line_population = sites_per_line[&site.span.line_range.0];
            (line_population != 1
                || raw_input_class_marker(source, site.span.line_range.0, policy).is_none())
            .then_some(RawInputClassificationGap {
                name: site.name,
                span: site.span,
                line_population,
                classes: policy.raw_input_classes,
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
                self.classes.join(", ")
            ),
        )
        .with_level(CompilerErrorLevel::Advice)
    }
}

/// A production project action declaration with no production caller. The source
/// span is the declaration itself so a missing consumer is named exactly.
#[derive(Clone, Debug)]
pub struct ActionCallerGap {
    pub action: Ustr,
    pub file: Ustr,
    pub span: Span,
    pub population_empty: bool,
    pub policy: &'static ActionLayerPolicy,
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

fn is_production_action_module(file: Ustr, policy: Option<&ActionLayerPolicy>) -> bool {
    policy.is_some_and(|policy| path_uses_component(file, policy.production_root))
        && !is_test_module(file)
}

fn is_action_name(name: Ustr, policy: Option<&ActionLayerPolicy>) -> bool {
    policy.is_some_and(|policy| declared_name(name.as_str()).starts_with(policy.action_prefix))
}

fn has_annotation(f: &Function, name: &str) -> bool {
    f.annotations
        .iter()
        .any(|annotation| annotation.name.name.as_str() == name)
}

impl LintFinding for ActionCallerGap {
    const LINT: &'static str = ACTION_CALLERS;
    fn line(&self) -> usize {
        self.span.line_range.0
    }
    fn to_diagnostic(&self) -> CompilerError {
        let message = if self.population_empty {
            format!(
                "no production {} {}* declarations found",
                self.policy.production_label, self.policy.action_prefix
            )
        } else {
            format!(
                "production {} action `{}` has no production caller",
                self.policy.production_label, self.action
            )
        };
        CompilerError::new_with_label(self.span, message).with_level(CompilerErrorLevel::Advice)
    }
}

/// HIR-backed caller census for production project actions. Global declaration
/// initializers and `@wire` refresh bodies are registration, not callers.
pub fn action_caller_gaps(
    ws: &Workspace,
    warm: &FxHashSet<ModuleId>,
    policy: &'static ActionLayerPolicy,
) -> Vec<ActionCallerGap> {
    let declarations: Vec<(Ustr, Ustr, Span)> = user_modules(ws, warm)
        .filter(|module| is_production_action_module(module.file, Some(policy)))
        .flat_map(|module| {
            module
                .globals
                .iter()
                .filter(|(_, global)| is_action_name(global.name, Some(policy)))
                .map(move |(_, global)| (global.name, module.file, global.name_span))
        })
        .collect();

    if declarations.is_empty() {
        let Some(module) = user_modules(ws, warm)
            .find(|module| is_production_action_module(module.file, Some(policy)))
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
            policy,
        }];
    }

    let declared: FxHashSet<Ustr> = declarations.iter().map(|(name, _, _)| *name).collect();
    let mut callers: FxHashSet<Ustr> = FxHashSet::default();
    for module in user_modules(ws, warm).filter(|module| !is_test_module(module.file)) {
        for (owner, body) in &module.bodies {
            callers.extend(action_edges_in_body(
                module,
                *owner,
                body,
                &declared,
                Some(policy),
            ));
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
            policy,
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
        if f.is_extern
            || declared_name(f.name.as_str()) == "main"
            || f.name_span.file != module.file
        {
            continue;
        }
        let Some(recv) = f.param_decls.first() else {
            continue;
        };
        let Some(receiver) = nominal_name(ws, recv.ty) else {
            continue;
        };
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
        TypeExpr::App {
            ctor: TypeCtor::Nominal(name),
            params,
        } if params.len() == 0 => Some(name),
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
pub fn unknown_type_names(ws: &Workspace, warm: &FxHashSet<ModuleId>, names: &[Ustr]) -> Vec<Ustr> {
    let known = nominal_type_names(ws, warm);
    names
        .iter()
        .copied()
        .filter(|n| !known.contains(n))
        .collect()
}

/// How a row place left the scope its arena's lifetime governs. One kind per
/// storage class, so a census can report the shape of the escape population
/// as well as its size.
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
pub use row_escape::{RowPopulation, row_escapes_in_body, row_population};

/// Where a body's TRAILING expression ends up, so `row_escape` can tell a
/// returned row from a discarded one. A unit function's tail goes nowhere --
/// `fn unload_sound(s: Sound) { s.handle.remove() }` drops the `T?` it produced.
fn tail_sink(module: &Module, owner: &BodyOwner) -> Option<EscapeKind> {
    match owner {
        BodyOwner::Function(fid) => module.functions[*fid].return_ty.map(|_| EscapeKind::Return),
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
    let (id, TypeExpr::Nullable(inner)) = head_ty(ws, bi, ty) else {
        return false;
    };
    matches!(
        head_type(ws, bi, id.with_idx(inner)),
        TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } if declared_name(n.as_str()) == "Task"
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
        Expr::SafeNavCall {
            receiver, method, ..
        } => {
            format!("{}?.{}()", render_place(body, receiver)?, method)
        }
        // UFCS: `e.get()` lowers to a call whose first arg is the receiver.
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

/// `task_null_abandon` findings across every non-stdlib body in the workspace.
/// Convenience for tests and whole-workspace CLI runs; the surfaces go through
/// [`inference_findings`].
pub fn task_null_abandon(ws: &Workspace) -> Vec<TaskNullAbandon> {
    let warm = FxHashSet::default();
    let mut out = Vec::new();
    for module in user_modules(ws, &warm) {
        let inf = ws.inference(module.id);
        for (owner, body) in module.bodies.iter() {
            out.extend(task_null_abandon_in_body(
                ws,
                inf.and_then(|i| i.per_fn.get(owner)),
                body,
            ));
        }
    }
    out
}

/// The declared (last) segment of a DefKey; a legacy name is already bare.
/// Classifiers that key on a spelling (`main`, `Task`) must read this.
fn declared_name(name: &str) -> &str {
    name.rsplit("::").next().unwrap_or(name)
}
