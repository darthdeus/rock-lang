//! Lints over bone's typechecked HIR. Reads semantic results without re-typechecking.

use bone_core::hir::{
    AnnoValue, BinOp, BindingInfo, BindingKind, BodyOwner, Callee, Expr, Function, Module,
    Statement, TyId, TypeCtor, TypeExpr,
};
use bone_core::prelude::*;
use bone_core::workspace::{BodyInference, BoundNext, BoundTree};
use rock_core::errors::CompilerErrorLevel;
use std::collections::BTreeSet;

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

/// Enum names that lower to `Let` bindings in bare pattern arms.
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

/// Lint names recognized by selection, suppression, and completion.
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

/// A rendered, name-tagged finding shared by the LSP, CLI, and build pass.
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

/// Coordinate changes from the body edit that updated semantic spans.
/// The cache uses this delta instead of inferring shifts from new text.
pub use bone_core::text_edit::TextEditDelta;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LintCacheUpdate {
    pub full: bool,
    /// Changed files were lowered again, so owners and spans may differ without
    /// a text-edit delta describing the change.
    pub relowered: bool,
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

    /// An interface or scope edit that re-lowered `changed_files` and
    /// re-inferred `reinferred`. Module ids must be unchanged; otherwise rebuild.
    pub fn relowered(
        changed_files: impl IntoIterator<Item = Ustr>,
        reinferred: impl IntoIterator<Item = (ModuleId, BodyOwner)>,
    ) -> Self {
        Self {
            relowered: true,
            changed_files: changed_files.into_iter().collect(),
            reinferred: reinferred.into_iter().collect(),
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
            relowered: false,
            changed_files: self.changed_files.clone(),
            reinferred: self.reinferred.clone(),
            edits,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LintCacheStats {
    pub full_rebuilds: usize,
}

#[derive(Clone, Debug, Default)]
struct FindingRow {
    raw: Vec<Finding>,
    visible: Vec<Finding>,
}

impl FindingRow {
    fn refilter(&mut self, suppressions: &FxHashSet<(usize, &'static str)>) {
        if suppressions.is_empty() {
            self.visible.clone_from(&self.raw);
            return;
        }
        self.visible.clear();
        self.visible.extend(
            self.raw
                .iter()
                .filter(|finding| !is_suppressed(finding, suppressions))
                .cloned(),
        );
    }

    fn retain(&mut self, keep: impl Fn(&Finding) -> bool) {
        self.raw.retain(&keep);
        self.visible.retain(keep);
    }

    fn shift(&mut self, edit: &TextEditDelta) {
        shift_findings(&mut self.raw, edit);
        shift_findings(&mut self.visible, edit);
    }
}

#[derive(Clone, Debug, Default)]
struct CachedFileFindings {
    flat: FxHashMap<BodyOwner, FindingRow>,
    inference: FxHashMap<BodyOwner, FindingRow>,
    other: FindingRow,
    suppression_fingerprint: u64,
    suppressions: FxHashSet<(usize, &'static str)>,
}

impl CachedFileFindings {
    fn new(source: &str) -> Self {
        Self {
            suppression_fingerprint: suppression_fingerprint(source),
            suppressions: suppression_entries(source),
            ..Default::default()
        }
    }

    fn refilter(&mut self) {
        for row in self.flat.values_mut().chain(self.inference.values_mut()) {
            row.refilter(&self.suppressions);
        }
        self.other.refilter(&self.suppressions);
    }

    fn add_raw_local(&mut self, module: &Module, finding: Finding) {
        let row = match finding_owner(module, &finding) {
            Some(owner) if is_inference_lint(finding.lint) => {
                self.inference.entry(owner).or_default()
            }
            Some(owner) => self.flat.entry(owner).or_default(),
            None => &mut self.other,
        };
        row.raw.push(finding);
    }

    fn install_body(&mut self, owner: BodyOwner, raw: Vec<Finding>, inference: bool) {
        let rows = if inference {
            &mut self.inference
        } else {
            &mut self.flat
        };
        let row = rows.entry(owner).or_default();
        row.raw = raw;
        row.refilter(&self.suppressions);
    }
}

#[derive(Clone, Debug)]
struct LintCacheTransaction {
    change: LintCacheUpdate,
    applied: bool,
}

type FunctionKey = (ModuleId, Idx<Function>);
type BodyKey = (ModuleId, BodyOwner);
type UserBodyEdges = FxHashMap<BodyKey, FxHashSet<FunctionKey>>;

#[derive(Clone, Debug, Default)]
struct UnusedInvalidation {
    functions: FxHashSet<FunctionKey>,
    locations: FxHashSet<(Ustr, usize)>,
    seeds: FxHashSet<BodyKey>,
    /// Callers of changed declarations; their edges may contain reassigned
    /// function indices after relowering.
    callers: FxHashSet<BodyKey>,
}

#[derive(Clone, Debug, Default)]
struct LintDependencyIndex {
    function_names: FxHashMap<Ustr, Vec<FunctionKey>>,
    body_edges: UserBodyEdges,
    reverse_callers: FxHashMap<FunctionKey, FxHashSet<BodyKey>>,
    always_live_bodies: FxHashSet<BodyKey>,
    reachable: FxHashSet<FunctionKey>,
    declarations: FxHashMap<FunctionKey, Declaration>,
    declaration_fingerprints: FxHashMap<Ustr, String>,
}

#[derive(Clone, Debug)]
struct Declaration {
    file: Ustr,
    span: Span,
    root: bool,
}

impl LintDependencyIndex {
    fn build(ws: &Workspace, warm: &FxHashSet<ModuleId>) -> Self {
        let mut index = Self::default();
        for module in ws.user_modules(warm) {
            index
                .declaration_fingerprints
                .insert(module.file, declaration_fingerprint(ws, module));
            for (fid, _) in module.functions.iter() {
                index.insert_function(module, fid);
            }
            for owner in module.bodies.keys() {
                if !matches!(owner, BodyOwner::Function(_)) {
                    let body = (module.id, *owner);
                    index.body_edges.entry(body).or_default();
                    index.always_live_bodies.insert(body);
                }
            }
        }
        for module in ws.user_modules(warm) {
            for (owner, body) in module.bodies.iter() {
                let edges = body_edges(ws, module.id, *owner, body, &index.function_names)
                    .into_iter()
                    .collect();
                index.install_edges((module.id, *owner), edges);
            }
        }
        let nodes = index.body_edges.keys().copied().collect();
        index.recompute_component(&nodes);
        index
    }

    fn insert_function(&mut self, module: &Module, fid: Idx<Function>) {
        let function = &module.functions[fid];
        let key = (module.id, fid);
        self.function_names
            .entry(function.name)
            .or_default()
            .push(key);
        self.declarations.insert(
            key,
            Declaration {
                file: module.file,
                span: function.name_span,
                root: is_root(function),
            },
        );
        self.body_edges
            .entry((module.id, BodyOwner::Function(fid)))
            .or_default();
    }

    fn remove_edges(&mut self, body: BodyKey) -> FxHashSet<FunctionKey> {
        use std::collections::hash_map::Entry;
        let old = self.body_edges.remove(&body).unwrap_or_default();
        for &target in &old {
            if let Entry::Occupied(mut callers) = self.reverse_callers.entry(target) {
                callers.get_mut().remove(&body);
                if callers.get().is_empty() {
                    callers.remove();
                }
            }
        }
        old
    }

    fn install_edges(&mut self, body: BodyKey, edges: FxHashSet<FunctionKey>) {
        self.remove_edges(body);
        for &target in &edges {
            self.reverse_callers.entry(target).or_default().insert(body);
        }
        self.body_edges.insert(body, edges);
    }

    /// Shift indexed declaration spans with body edits so later invalidation
    /// removes findings at their current offsets.
    fn shift_declarations(&mut self, edit: &TextEditDelta) {
        for declaration in self.declarations.values_mut() {
            edit.shift_span(&mut declaration.span);
        }
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
                BodyOwner::Function(fid) => self
                    .declarations
                    .get(&(node.0, fid))
                    .is_some_and(|decl| decl.root),
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

        self.recompute_seed_components(&seeds, false)
    }

    fn update_declarations(
        &mut self,
        ws: &Workspace,
        warm: &FxHashSet<ModuleId>,
        changed_files: &FxHashSet<Ustr>,
    ) -> UnusedInvalidation {
        let mut invalidation = UnusedInvalidation::default();
        let old_functions: Vec<FunctionKey> = self
            .declarations
            .iter()
            .filter(|(_, decl)| changed_files.contains(&decl.file))
            .map(|(&function, _)| function)
            .collect();
        for &function in &old_functions {
            invalidation.functions.insert(function);
            invalidation
                .seeds
                .insert((function.0, BodyOwner::Function(function.1)));
            let decl = &self.declarations[&function];
            invalidation
                .locations
                .insert((decl.file, decl.span.offset_range().0));
            if let Some(callers) = self.reverse_callers.get(&function) {
                invalidation.seeds.extend(callers.iter().copied());
                invalidation.callers.extend(callers.iter().copied());
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

        let current_functions: Vec<FunctionKey> = ws
            .user_modules(warm)
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
                .insert((module.file, declaration.name_span.offset_range().0));
        }

        for function in old_functions {
            self.remove_edges((function.0, BodyOwner::Function(function.1)));
            self.declarations.remove(&function);
            self.reachable.remove(&function);
        }
        // Reinsert current functions below; their indices may reuse old ids.
        self.function_names.retain(|_, functions| {
            functions.retain(|candidate| !invalidation.functions.contains(candidate));
            !functions.is_empty()
        });

        for &function in &current_functions {
            let module = ws.module(function.0);
            self.insert_function(module, function.1);
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
                self.install_edges((function.0, owner), edges);
            }
        }
        refresh_fingerprints(
            &mut self.declaration_fingerprints,
            ws,
            changed_files,
            |module| declaration_fingerprint(ws, module),
        );
        invalidation
    }

    /// Replace relowered non-function bodies. Their owners may be renumbered;
    /// retract old edges, install current edges, and return reachability seeds.
    fn update_relowered_bodies(
        &mut self,
        ws: &Workspace,
        warm: &FxHashSet<ModuleId>,
        changed_files: &FxHashSet<Ustr>,
    ) -> FxHashSet<BodyKey> {
        let modules: Vec<&Module> = ws
            .user_modules(warm)
            .filter(|module| changed_files.contains(&module.file))
            .collect();
        let mids: FxHashSet<ModuleId> = modules.iter().map(|module| module.id).collect();
        let mut seeds = FxHashSet::default();
        let stale: Vec<BodyKey> = self
            .body_edges
            .keys()
            .copied()
            .filter(|(mid, owner)| mids.contains(mid) && !matches!(owner, BodyOwner::Function(_)))
            .collect();
        for key in stale {
            let old = self.remove_edges(key);
            for target in old {
                seeds.insert((target.0, BodyOwner::Function(target.1)));
            }
            self.always_live_bodies.remove(&key);
        }
        for module in modules {
            for (owner, body) in module
                .bodies
                .iter()
                .filter(|(owner, _)| !matches!(owner, BodyOwner::Function(_)))
            {
                let key = (module.id, *owner);
                let edges: FxHashSet<FunctionKey> =
                    body_edges(ws, module.id, *owner, body, &self.function_names)
                        .into_iter()
                        .collect();
                seeds.insert(key);
                seeds.extend(
                    edges
                        .iter()
                        .map(|&(mid, fid)| (mid, BodyOwner::Function(fid))),
                );
                self.always_live_bodies.insert(key);
                self.install_edges(key, edges);
            }
        }
        seeds
    }

    fn recompute_seed_components(
        &mut self,
        seeds: &FxHashSet<BodyKey>,
        declarations_only: bool,
    ) -> FxHashSet<FunctionKey> {
        let mut covered = FxHashSet::default();
        let mut affected = FxHashSet::default();
        for &seed in seeds {
            if covered.contains(&seed)
                || (declarations_only && !self.body_edges.contains_key(&seed))
            {
                continue;
            }
            let component = self.collect_component(seed);
            covered.extend(component.iter().copied());
            self.recompute_component(&component);
            affected.extend(
                component
                    .into_iter()
                    .filter_map(|(mid, owner)| match owner {
                        BodyOwner::Function(fid)
                            if !declarations_only
                                || self.declarations.contains_key(&(mid, fid)) =>
                        {
                            Some((mid, fid))
                        }
                        _ => None,
                    }),
            );
        }
        affected
    }
}

fn fingerprints_changed(
    fingerprints: &FxHashMap<Ustr, String>,
    ws: &Workspace,
    files: &FxHashSet<Ustr>,
    fingerprint: impl Fn(&Module) -> String,
) -> bool {
    files.iter().any(
        |file| match ws.modules.iter().find(|module| module.file == *file) {
            Some(module) => fingerprints.get(file) != Some(&fingerprint(module)),
            None => fingerprints.contains_key(file),
        },
    )
}

fn refresh_fingerprints(
    fingerprints: &mut FxHashMap<Ustr, String>,
    ws: &Workspace,
    files: &FxHashSet<Ustr>,
    fingerprint: impl Fn(&Module) -> String,
) {
    for &file in files {
        match ws.modules.iter().find(|module| module.file == file) {
            Some(module) => fingerprints.insert(file, fingerprint(module)),
            None => fingerprints.remove(&file),
        };
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

/// Incremental lint derivation cache. Reporting policy stays in the CLI/LSP
/// adapters.
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
    global: FxHashMap<Ustr, FindingRow>,
    last_by_file: FxHashMap<Ustr, Vec<Finding>>,
    dependency: LintDependencyIndex,
    body_transaction: Option<LintCacheTransaction>,
}

impl IncrementalLintCache {
    pub fn begin_body_transaction(&mut self, change: &LintCacheUpdate) {
        debug_assert!(
            !change.full && !change.relowered,
            "body transaction cannot wrap a full or relowered lint update"
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

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn invalidate_file(&mut self, file: Ustr) {
        self.files.remove(&file);
        if let Some(row) = self.global.get_mut(&file) {
            row.visible.clear();
        }
        self.set_output(file, Vec::new());
    }

    pub fn findings_for_file(&self, file: Ustr) -> Vec<Finding> {
        self.last_by_file.get(&file).cloned().unwrap_or_default()
    }

    fn remove_global_findings(&mut self, lint: &str, locations: &FxHashSet<(Ustr, usize)>) {
        let files: FxHashSet<Ustr> = locations.iter().map(|(file, _)| *file).collect();
        for file in files {
            if let Some(row) = self.global.get_mut(&file) {
                row.retain(|finding| {
                    finding.lint != lint || !locations.contains(&(file, finding_offset(finding)))
                });
            }
        }
    }

    fn push_global(&mut self, file: Ustr, raw: Finding) {
        let cached = &self.files[&file];
        let row = self.global.entry(file).or_default();
        if !is_suppressed(&raw, &cached.suppressions) {
            row.visible.push(raw.clone());
        }
        row.raw.push(raw);
    }

    fn replace_unused_findings(
        &mut self,
        ws: &Workspace,
        warm: &FxHashSet<ModuleId>,
        sources: &FxHashMap<Ustr, &str>,
        affected: &FxHashSet<FunctionKey>,
        extra_locations: &FxHashSet<(Ustr, usize)>,
    ) {
        let mut locations = extra_locations.clone();
        let mut dead = Vec::new();
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
            locations.insert((module.file, function.name_span.offset_range().0));
            if !warm.contains(&mid)
                && !self.dependency.reachable.contains(&(mid, fid))
                && !function.is_synthetic
            {
                dead.push((module.file, unused_fn(function)));
            }
        }
        locations.retain(|(file, _)| sources.contains_key(file));
        self.remove_global_findings(UNUSED_FN, &locations);
        for (file, raw) in dead {
            if sources.contains_key(&file) {
                self.push_global(file, raw);
            }
        }
    }

    fn refilter_suppression_files(
        &mut self,
        files: &FxHashSet<Ustr>,
        sources: &FxHashMap<Ustr, &str>,
    ) {
        for &file in files {
            let Some(source) = sources.get(&file).copied() else {
                continue;
            };
            let Some(cached) = self.files.get_mut(&file) else {
                continue;
            };
            cached.suppressions = suppression_entries(source);
            cached.refilter();
            cached.suppression_fingerprint = suppression_fingerprint(source);
            if let Some(row) = self.global.get_mut(&file) {
                row.refilter(&cached.suppressions);
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
        let mut stats = LintCacheStats::default();
        let workspace_files: FxHashSet<Ustr> = ws.source_modules().map(|m| m.file).collect();
        let report_files: FxHashSet<Ustr> = sources.keys().copied().collect();
        let metadata_changed = self.initialized
            && (self.is_bin != is_bin
                || self.warm != *warm
                || self.workspace_files != workspace_files
                || self.report_files != report_files
                || self.selection.as_ref() != Some(sel));
        let body_cacheable = cacheable_selection(sel);
        if !self.initialized
            || change.full
            || metadata_changed
            || !body_cacheable
            || (change.relowered && !self.relowered_update_supported(ws, warm, change))
        {
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
            self.files.insert(file, CachedFileFindings::new(source));
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
        // Relowering may change every body owner, so derive those files anew.
        let reseeded_files: FxHashSet<Ustr> = if change.relowered {
            change
                .changed_files
                .iter()
                .copied()
                .filter(|file| sources.contains_key(file))
                .collect()
        } else {
            FxHashSet::default()
        };
        for &file in &reseeded_files {
            self.reseed_file(ws, warm, file, sources[&file], sel);
        }

        if change.changed_files.is_empty()
            && change.reinferred.is_empty()
            && change.edits.is_empty()
        {
            let findings = self.current_output(sources);
            self.mark_body_transaction_applied();
            return (findings, stats);
        }

        self.warm = warm.clone();
        // Without span deltas, relowering invalidates declarations even if their
        // fingerprints are unchanged.
        let declarations_changed = change.relowered
            || fingerprints_changed(
                &self.dependency.declaration_fingerprints,
                ws,
                &change.changed_files,
                |module| declaration_fingerprint(ws, module),
            );
        let unused_invalidation = if declarations_changed {
            let mut invalidation =
                self.dependency
                    .update_declarations(ws, warm, &change.changed_files);
            if change.relowered {
                invalidation
                    .seeds
                    .extend(self.dependency.update_relowered_bodies(
                        ws,
                        warm,
                        &change.changed_files,
                    ));
            }
            Some(invalidation)
        } else {
            None
        };
        let mut affected_unused = if let Some(invalidation) = &unused_invalidation {
            invalidation.functions.clone()
        } else {
            FxHashSet::default()
        };
        let mut edge_bodies = change.reinferred.clone();
        if change.relowered
            && let Some(invalidation) = &unused_invalidation
        {
            let reinferred: FxHashSet<BodyKey> = change.reinferred.iter().copied().collect();
            edge_bodies.extend(
                invalidation
                    .callers
                    .iter()
                    .copied()
                    .filter(|body| !reinferred.contains(body)),
            );
        }
        affected_unused.extend(self.dependency.update_bodies(ws, &edge_bodies));
        if let Some(invalidation) = &unused_invalidation {
            affected_unused.extend(
                self.dependency
                    .recompute_seed_components(&invalidation.seeds, true),
            );
        }
        let variants = self.variants.clone();
        let suppression_only =
            !suppression_changed_files.is_empty() && change.reinferred.is_empty();
        let mut flat_targets: FxHashSet<(ModuleId, BodyOwner)> = FxHashSet::default();
        let mut inference_targets: FxHashSet<(ModuleId, BodyOwner)> =
            change.reinferred.iter().copied().collect();
        if !reseeded_files.is_empty() {
            inference_targets.retain(|(mid, _)| !reseeded_files.contains(&ws.module(*mid).file));
        }
        for file in &change.changed_files {
            if reseeded_files.contains(file) {
                continue;
            }
            let Some(module) = ws.modules.iter().find(|m| m.file == *file) else {
                continue;
            };
            if suppression_only
                || (!missing_cache_files.contains(file)
                    && change.reinferred.iter().any(|(mid, _)| *mid == module.id))
            {
                continue;
            }
            let owners = || module.bodies.keys().map(|&owner| (module.id, owner));
            flat_targets.extend(owners());
            inference_targets.extend(owners());
        }
        flat_targets.extend(change.reinferred.iter().copied().filter(|(mid, _)| {
            let file = ws.module(*mid).file;
            change.changed_files.contains(&file) && !reseeded_files.contains(&file)
        }));

        let mut dirty_output_files = change.changed_files.clone();
        dirty_output_files.extend(
            flat_targets
                .iter()
                .chain(inference_targets.iter())
                .map(|(mid, _)| ws.module(*mid).file),
        );
        dirty_output_files.extend(affected_unused.iter().map(|(mid, _)| ws.module(*mid).file));
        if let Some(invalidation) = &unused_invalidation {
            dirty_output_files.extend(invalidation.locations.iter().map(|(file, _)| *file));
        }
        dirty_output_files.extend(suppression_changed_files.iter().copied());

        let body_rows = flat_targets.iter().map(|&target| (target, false));
        let body_rows = body_rows.chain(inference_targets.iter().map(|&target| (target, true)));
        for ((mid, owner), inference) in body_rows {
            let module = ws.module(mid);
            if !self.report_files.contains(&module.file) {
                continue;
            }
            let Some(body) = module.bodies.get(&owner) else {
                continue;
            };
            let raw = if inference {
                inference_body_findings(ws, module, owner, body, sel)
            } else {
                flat_body_findings(module, owner, body, &variants, sel)
            };
            self.files
                .entry(module.file)
                .or_default()
                .install_body(owner, raw, inference);
        }
        for edit in &change.edits {
            self.dependency.shift_declarations(edit);
            let Some(cached) = self.files.get_mut(&edit.file) else {
                continue;
            };
            let mid = ws
                .modules
                .iter()
                .find(|m| m.file == edit.file)
                .map_or(ModuleId::builtin(), |m| m.id);
            for (rows, targets) in [
                (&mut cached.flat, &flat_targets),
                (&mut cached.inference, &inference_targets),
            ] {
                for (&owner, row) in rows {
                    if !targets.contains(&(mid, owner)) {
                        row.shift(edit);
                    }
                }
            }
            cached.other.shift(edit);
            if let Some(row) = self.global.get_mut(&edit.file) {
                row.shift(edit);
            }
            dirty_output_files.insert(edit.file);
        }

        // These findings are rederived for reseeded files, so discard old rows.
        for file in &reseeded_files {
            if let Some(row) = self.global.get_mut(file) {
                row.retain(|finding| finding.lint != UNUSED_FN);
            }
        }
        if is_bin
            && sel.enabled(UNUSED_FN)
            && (!affected_unused.is_empty() || unused_invalidation.is_some())
        {
            let extra_locations = unused_invalidation
                .as_ref()
                .map(|invalidation| invalidation.locations.clone())
                .unwrap_or_default();
            self.replace_unused_findings(ws, warm, sources, &affected_unused, &extra_locations);
        }
        if !suppression_changed_files.is_empty() {
            self.refilter_suppression_files(&suppression_changed_files, sources);
        }
        self.refresh_output_files(&dirty_output_files, sources);
        let findings = self.current_output(sources);
        self.mark_body_transaction_applied();
        (findings, stats)
    }

    /// Variant changes affect every body; module IDs must remain stable.
    fn relowered_update_supported(
        &self,
        ws: &Workspace,
        warm: &FxHashSet<ModuleId>,
        change: &LintCacheUpdate,
    ) -> bool {
        if enum_variant_names(ws, warm) != self.variants {
            return false;
        }
        change.changed_files.iter().all(|file| {
            let current = ws
                .modules
                .iter()
                .find(|module| module.file == *file)
                .map(|module| module.id);
            self.dependency
                .declarations
                .iter()
                .filter(|(_, declaration)| declaration.file == *file)
                .all(|((mid, _), _)| Some(*mid) == current)
        })
    }

    /// Derive one file's local entries from scratch, as [`rebuild`] does.
    fn reseed_file(
        &mut self,
        ws: &Workspace,
        warm: &FxHashSet<ModuleId>,
        file: Ustr,
        source: &str,
        sel: &LintOptions,
    ) {
        self.files.remove(&file);
        let mut cached = CachedFileFindings::new(source);
        if let Some(module) = ws.user_modules(warm).find(|module| module.file == file) {
            for finding in local_findings(ws, module, &self.variants, source, sel) {
                cached.add_raw_local(module, finding);
            }
            cached.refilter();
        }
        self.files.insert(file, cached);
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
        self.dependency = if sel.enabled(UNUSED_FN) {
            LintDependencyIndex::build(ws, warm)
        } else {
            LintDependencyIndex::default()
        };
        let modules = lint_modules(
            ws,
            warm,
            is_bin,
            sources,
            sel,
            sel.enabled(UNUSED_FN).then_some(&self.dependency.reachable),
            |module, src, raw, advice| {
                (module.file, report_findings(src, raw.clone(), advice), raw)
            },
        );
        self.initialized = true;
        self.is_bin = is_bin;
        self.warm = warm.clone();
        self.variants = enum_variant_names(ws, warm);
        self.workspace_files = workspace_files;
        self.report_files = report_files;
        self.report_order = ws
            .user_modules(warm)
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
        self.global.clear();
        for (&file, source) in sources {
            self.files.insert(file, CachedFileFindings::new(source));
        }
        let mut findings = Vec::new();
        self.last_by_file.clear();
        self.active_output_ranks.clear();
        for (file, report, raw) in modules {
            for finding in raw {
                let file = finding.file();
                if is_global_lint(finding.lint) {
                    self.global.entry(file).or_default().raw.push(finding);
                } else if let Some(module) = ws.modules.iter().find(|m| m.file == file) {
                    self.files
                        .entry(file)
                        .or_default()
                        .add_raw_local(module, finding);
                }
            }
            if !report.is_empty() {
                findings.extend(report.iter().cloned());
                self.set_output(file, report);
            }
        }
        for module in ws.user_modules(warm) {
            if let Some(cached) = self.files.get_mut(&module.file)
                && sources.contains_key(&module.file)
            {
                cached.refilter();
            }
        }
        for (&file, row) in &mut self.global {
            if let Some(cached) = self.files.get(&file) {
                row.refilter(&cached.suppressions);
            }
        }
        stats.full_rebuilds = 1;
        findings
    }

    fn set_output(&mut self, file: Ustr, findings: Vec<Finding>) {
        let rank = self.report_rank.get(&file);
        if findings.is_empty() {
            self.last_by_file.remove(&file);
            if let Some(rank) = rank {
                self.active_output_ranks.remove(rank);
            }
        } else {
            self.last_by_file.insert(file, findings);
            if let Some(&rank) = rank {
                self.active_output_ranks.insert(rank);
            }
        }
    }

    fn refresh_output_files(&mut self, files: &FxHashSet<Ustr>, sources: &FxHashMap<Ustr, &str>) {
        for &file in files {
            let mut findings = Vec::new();
            if sources.contains_key(&file)
                && let Some(cached) = self.files.get(&file)
            {
                // Non-cacheable selections rebuild, so these rows are the report.
                findings.extend(
                    cached
                        .flat
                        .values()
                        .chain(cached.inference.values())
                        .chain(std::iter::once(&cached.other))
                        .chain(self.global.get(&file))
                        .flat_map(|row| row.visible.iter().cloned()),
                );
                findings.sort_by(cmp_by_offset);
            }
            self.set_output(file, findings);
        }
    }

    fn current_output(&self, sources: &FxHashMap<Ustr, &str>) -> Vec<Finding> {
        let mut out = Vec::new();
        for rank in &self.active_output_ranks {
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
    matches!(lint, UNUSED_FN | ACTION_CALLERS)
}

fn cacheable_selection(sel: &LintOptions) -> bool {
    sel.only.is_none()
}

fn is_inference_lint(lint: &str) -> bool {
    matches!(lint, TRANSMUTE_OF_NULLABLE | TASK_NULL_ABANDON | DEPRECATED)
}

fn finding_offset(finding: &Finding) -> usize {
    finding
        .error
        .labels
        .first()
        .map(|label| label.span.offset_range().0)
        .unwrap_or(finding.error.location.offset)
}

/// Deterministic diagnostic order: offset, lint, message, then label spans.
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

/// Lines containing `allow`, with the numbering and trimming of `str::lines`.
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

fn suppression_fingerprint(source: &str) -> u64 {
    let mut hash = 1469598103934665603u64;
    for (_, line) in allow_lines(source) {
        if line.contains("rock") {
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

fn shift_findings(findings: &mut [Finding], edit: &TextEditDelta) {
    for finding in findings {
        let mut moved = edit.shift_line_col(&mut finding.error.location);
        for label in &mut finding.error.labels {
            moved |= edit.shift_span(&mut label.span);
        }
        if moved {
            if let Some(label) = finding.error.labels.first() {
                finding.line = label.span.line_range().0;
            } else {
                finding.line = finding.error.location.line;
            }
        }
    }
}

/// One notice for the run when any action-layer lint needs a missing policy.
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

fn local_findings(
    ws: &Workspace,
    module: &Module,
    variants: &FxHashSet<Ustr>,
    src: &str,
    sel: &LintOptions,
) -> Vec<Finding> {
    let mut out: Vec<Finding> = module
        .bodies
        .iter()
        .flat_map(|(owner, body)| flat_body_findings(module, *owner, body, variants, sel))
        .collect();
    let migration = sel.enabled(RAW_INPUT_MIGRATION);
    let classification = sel.enabled(RAW_INPUT_CLASSIFICATION);
    if (migration || classification)
        && let Some(policy) = sel.action_policy.as_deref()
    {
        raw_input_findings(module, src, policy, migration, classification, &mut out);
    }
    out.extend(
        module
            .bodies
            .iter()
            .flat_map(|(&owner, body)| inference_body_findings(ws, module, owner, body, sel)),
    );
    out
}

pub fn workspace_findings_by_file_with(
    ws: &Workspace,
    warm: &FxHashSet<ModuleId>,
    is_bin: bool,
    sel: &LintOptions,
) -> FxHashMap<Ustr, Vec<Finding>> {
    workspace_findings_by_file(ws, warm, is_bin, sel, None)
}

fn workspace_findings_by_file(
    ws: &Workspace,
    warm: &FxHashSet<ModuleId>,
    is_bin: bool,
    sel: &LintOptions,
    reachable: Option<&FxHashSet<FunctionKey>>,
) -> FxHashMap<Ustr, Vec<Finding>> {
    let mut out: FxHashMap<Ustr, Vec<Finding>> = FxHashMap::default();
    if is_bin && sel.enabled(UNUSED_FN) {
        out = unused_fns_by_file(ws, warm, reachable);
    }
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
    lint_modules(
        ws,
        warm,
        is_bin,
        sources,
        sel,
        None,
        |_, src, raw, advice| report_findings(src, raw, advice),
    )
    .into_iter()
    .flatten()
    .collect()
}

/// Apply allow comments and add any unsuppressible missing-policy notice.
fn report_findings(src: &str, raw: Vec<Finding>, advice: Option<&Finding>) -> Vec<Finding> {
    let mut out = filter_suppressed_findings(raw, src);
    out.extend(advice.cloned());
    out.sort_by(cmp_by_offset);
    out
}

/// Apply `report` to each file's local and workspace findings in module order.
fn lint_modules<'ws, T: Send>(
    ws: &'ws Workspace,
    warm: &'ws FxHashSet<ModuleId>,
    is_bin: bool,
    sources: &FxHashMap<Ustr, &str>,
    sel: &LintOptions,
    reachable: Option<&FxHashSet<FunctionKey>>,
    report: impl Fn(&'ws Module, &str, Vec<Finding>, Option<&Finding>) -> T + Sync,
) -> Vec<T> {
    let mut by_file = workspace_findings_by_file(ws, warm, is_bin, sel, reachable);
    let variants = enum_variant_names(ws, warm);
    let mut per_module = Vec::new();
    for module in ws.user_modules(warm) {
        let Some(&src) = sources.get(&module.file) else {
            continue;
        };
        per_module.push((module, src, by_file.remove(&module.file)));
    }
    let advice = per_module
        .first()
        .and_then(|(module, _, _)| missing_policy_advice(module, sel));
    per_module
        .into_par_iter()
        .map(|(module, src, workspace_findings)| {
            let mut found = local_findings(ws, module, &variants, src, sel);
            found.extend(workspace_findings.into_iter().flatten());
            report(
                module,
                src,
                found,
                advice
                    .as_ref()
                    .filter(|finding| finding.file() == module.file),
            )
        })
        .collect()
}

/// Drop findings with `// rock: allow(<lint>)` on their line or the line above.
pub fn filter_suppressed_findings(mut findings: Vec<Finding>, source: &str) -> Vec<Finding> {
    if !findings.is_empty() {
        let allow = suppression_entries(source);
        if !allow.is_empty() {
            findings.retain(|finding| !is_suppressed(finding, &allow));
        }
    }
    findings
}

/// Parse standalone or trailing allow comments, including optional whitespace
/// and justification arguments. Unknown lint names do not match.
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
    indexed_reachable: Option<&FxHashSet<FunctionKey>>,
) -> FxHashMap<Ustr, Vec<Finding>> {
    let computed_reachable;
    let reachable = match indexed_reachable {
        Some(reachable) => reachable,
        None => {
            computed_reachable = reachable_user_functions(ws, warm);
            &computed_reachable
        }
    };

    let mut out: FxHashMap<Ustr, Vec<Finding>> = FxHashMap::default();
    for module in ws.user_modules(warm) {
        for (fid, function) in module.functions.iter() {
            if !reachable.contains(&(module.id, fid)) && !function.is_synthetic {
                out.entry(module.file)
                    .or_default()
                    .push(unused_fn(function));
            }
        }
    }
    out
}

fn reachable_user_functions(ws: &Workspace, warm: &FxHashSet<ModuleId>) -> FxHashSet<FunctionKey> {
    let names = user_fn_names(ws, warm);
    let edges_of = |mid, owner, body: &Body| body_edges(ws, mid, owner, body, &names);
    let mut reachable = FxHashSet::default();
    let mut stack = Vec::new();
    for module in ws.user_modules(warm) {
        for (fid, f) in module.functions.iter() {
            if is_root(f) && reachable.insert((module.id, fid)) {
                stack.push((module.id, fid));
            }
        }
        // Non-function bodies run at initialization, so their references are roots.
        for (owner, body) in module.bodies.iter() {
            if matches!(owner, BodyOwner::Function(_)) {
                continue;
            }
            for callee in edges_of(module.id, *owner, body) {
                if reachable.insert(callee) {
                    stack.push(callee);
                }
            }
        }
    }

    while let Some((mid, fid)) = stack.pop() {
        let owner = BodyOwner::Function(fid);
        if let Some(body) = ws.module(mid).bodies.get(&owner) {
            for callee in edges_of(mid, owner, body) {
                if reachable.insert(callee) {
                    stack.push(callee);
                }
            }
        }
    }

    reachable
}

/// A function entered from outside the in-project call graph is a root.
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
            | "struct_expand"
            | "shader_expand"
            | "build_var_expand"
            | "embed_expand"
            | "scene_type_key"
            | "consume"
    )
}

/// References found by inference (including protocol dispatch) plus names of
/// function values, which inference does not record as calls.
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

/// Functions reached through resolved calls and iterator-protocol edges.
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

/// The type of an assignment target; locals use binding types, other places use
/// expression types.
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

/// Render a supported assignment target for the diagnostic.
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

/// Last segment of a DefKey, used when matching compiler-owned names.
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
