//! Logical identity and compatibility facts for callable Rock artifacts.
//!
//! `Instance` is the identity of a named concrete declaration.  A lambda has
//! no declaration to borrow, so its identity is the enclosing concrete
//! instance plus a source site.  Backend names, arena indices and `RepId`s are
//! deliberately absent from these values: they are realization details, not
//! artifact identity.

use std::collections::{BTreeMap, BTreeSet};

use bone_semantic::prelude::Span;
use comfy_la_arena::Idx;
use rustc_hash::{FxHashMap, FxHashSet};
use ustr::Ustr;

use crate::artifact_plan::ArtifactBindingPlan;
use crate::instance::Instance;
use crate::mir::{
    ColumnElem, ContainerKind, CoroHookKind, FuncRef, Inst, Lit, MirFunc, Term, ValueCtorForm,
};
use crate::monoty::MonoInterner;

/// The file-independent name of a definition, for the identities the runtime
/// keys state on (plan 2.1). A modules-v1 definition's name IS its DefKey
/// (`<package>::<module path>::<name>`, the only names carrying `::`), which
/// already names the logical module, so the owning FRAGMENT is not part of its
/// identity: moving a definition between two fragments of one module keeps its
/// state and its code slot. A legacy definition is unique only within its file,
/// so it keeps today's `<file>::<name>`.
pub fn logical_def_key(file: Ustr, name: Ustr) -> String {
    match name.as_str().contains("::") {
        true => name.to_string(),
        false => format!("{file}::{name}"),
    }
}

/// The source location used to identify a lambda within one concrete owner.
/// The start offset is stable when the lambda body changes; the enclosing
/// topology fingerprint rejects edits that insert, remove or reorder sibling
/// lambda sites.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct LambdaSiteId {
    pub file: Ustr,
    pub offset: u64,
}

impl LambdaSiteId {
    pub fn from_span(span: Span) -> Option<Self> {
        (span.file != "<unknown>").then_some(Self {
            file: span.file,
            offset: span.offset_range.0 as u64,
        })
    }
}

/// The concrete owner of a callable artifact. Ordinary function bodies use the
/// mono `Instance`; always-emitted global/decorator roots have no declaration,
/// so their source artifact is named directly.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum CodeOwner {
    Instance(Instance),
    Artifact { kind: CodeOwnerKind, module: Ustr, name: Ustr },
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum CodeOwnerKind {
    Global,
    Decorator,
}

/// Compiler-owned identity of an emitted Rock callable body.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum CodeIdentity {
    /// A top-level declaration at one concrete mono instantiation.
    Named(Instance),
    /// An always-emitted artifact root. It is not itself an escaping value, but
    /// it owns lambda identities stored in a global or decorator artifact.
    Artifact(CodeOwner),
    /// A lambda literal in a concrete declaration body.
    Lambda { enclosing: CodeOwner, site: LambdaSiteId },
    /// A source-named nested function. It is still owned by the enclosing
    /// concrete declaration because nested functions do not enter the mono
    /// declaration graph as independent top-level declarations.
    Nested { enclosing: CodeOwner, site: LambdaSiteId, name: Ustr },
    /// Compiler-generated callable machinery stored in a Rock runtime record,
    /// such as a coroutine's resume/advance/defer/hook entry.
    Generated { enclosing: CodeOwner, role: Ustr },
}

impl CodeIdentity {
    pub fn enclosing(&self) -> Option<&Instance> {
        match self {
            Self::Named(i) => Some(i),
            Self::Lambda { enclosing: CodeOwner::Instance(i), .. }
            | Self::Nested { enclosing: CodeOwner::Instance(i), .. }
            | Self::Generated { enclosing: CodeOwner::Instance(i), .. } => Some(i),
            Self::Artifact(_)
            | Self::Lambda { enclosing: CodeOwner::Artifact { .. }, .. }
            | Self::Nested { enclosing: CodeOwner::Artifact { .. }, .. }
            | Self::Generated { enclosing: CodeOwner::Artifact { .. }, .. } => None,
        }
    }

    pub fn owner(&self) -> Option<CodeOwner> {
        match self {
            Self::Named(i) => Some(CodeOwner::Instance(i.clone())),
            Self::Artifact(owner)
            | Self::Lambda { enclosing: owner, .. }
            | Self::Nested { enclosing: owner, .. }
            | Self::Generated { enclosing: owner, .. } => Some(owner.clone()),
        }
    }

    pub fn is_lambda(&self) -> bool {
        matches!(self, Self::Lambda { .. })
    }

    /// A stable runtime key. It names the compiler-owned source/mono identity
    /// and deliberately excludes backend symbols, arena indices and pointers.
    pub fn logical_key(
        &self,
        ws: &bone_semantic::inference::Workspace,
        interner: &MonoInterner,
    ) -> String {
        let instance_key = |instance: &Instance| {
            let module = ws.module(instance.func.module);
            let function = &module.functions[instance.func.func];
            let args = instance
                .arg_types(interner)
                .iter()
                .map(|ty| ty.canonical())
                .collect::<Vec<_>>()
                .join(",");
            format!("{}<{}>", logical_def_key(module.file, function.name), args)
        };
        let owner_key = |owner: &CodeOwner| match owner {
            CodeOwner::Instance(instance) => instance_key(instance),
            CodeOwner::Artifact { kind, module, name } => {
                format!("artifact:{kind:?}:{}", logical_def_key(*module, *name))
            }
        };
        match self {
            Self::Named(instance) => format!("named:{}", instance_key(instance)),
            Self::Artifact(owner) => format!("artifact-root:{}", owner_key(owner)),
            Self::Lambda { enclosing, site } => format!(
                "lambda:{}@{}:{}",
                owner_key(enclosing),
                site.file,
                site.offset
            ),
            Self::Nested { enclosing, site, name } => format!(
                "nested:{}::{}@{}:{}",
                owner_key(enclosing),
                name,
                site.file,
                site.offset
            ),
            Self::Generated { enclosing, role } => {
                format!("generated:{}::{}", owner_key(enclosing), role)
            }
        }
    }
}

/// The ordered lambda population of one enclosing concrete instance.
///
/// A source-site identity alone is not enough: inserting a lambda before an
/// existing one can otherwise make a positional implementation reuse the
/// neighbour's slot. The fingerprint is compared at the owner boundary and
/// makes that edit incompatible even when a backend happens to reuse names.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub struct LambdaTopologyFingerprint(pub u64);

impl LambdaTopologyFingerprint {
    pub fn from_sites(sites: &[(LambdaSiteId, usize)]) -> Self {
        let mut h = Fnv64::new();
        h.write_usize(sites.len());
        for (site, params) in sites {
            h.write_str(site.file.as_str());
            h.write_u64(site.offset);
            h.write_usize(*params);
        }
        Self(h.finish())
    }

    /// Combine one owner's distinct region topologies into the single value
    /// the compatibility gate compares. Sorted, so region emission order
    /// cannot move it; tagged to separate the hash input from site-derived
    /// fingerprints. A single topology passes through unchanged.
    pub fn unified(topologies: impl IntoIterator<Item = LambdaTopologyFingerprint>) -> Self {
        let mut sorted: Vec<u64> = topologies.into_iter().map(|topology| topology.0).collect();
        sorted.sort_unstable();
        sorted.dedup();
        if sorted.len() <= 1 {
            return Self(sorted.first().copied().unwrap_or_default());
        }
        let mut h = Fnv64::new();
        h.write_str("owner-union-v1");
        h.write_usize(sorted.len());
        for topology in sorted {
            h.write_u64(topology);
        }
        Self(h.finish())
    }
}

/// How one captured value is transported in an environment record.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum CaptureMode {
    /// The record stores the value at closure creation.
    ByValue,
    /// The record stores a mutable box shared by all copies of the closure.
    Shared,
    /// The record reaches the coroutine frame through the state object.
    Frame,
}

#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum CaptureIdentity {
    Binding { site: LambdaSiteId, projection: Ustr },
    Generated { role: Ustr },
}

impl CaptureIdentity {
    pub fn projected(&self, projection: Ustr) -> Self {
        let append = |base: Ustr| {
            if base.is_empty() {
                projection
            } else {
                Ustr::from(format!("{base}.{projection}").as_str())
            }
        };
        match self {
            Self::Binding { site, projection: base } => {
                Self::Binding { site: *site, projection: append(*base) }
            }
            Self::Generated { role } => Self::Generated { role: append(*role) },
        }
    }
}

/// One logical environment field. Incomplete schemas fail compatibility
/// closed rather than pretending two unknown fields are interchangeable.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct CaptureField {
    pub identity: CaptureIdentity,
    pub mode: CaptureMode,
}

#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub struct CaptureSchema {
    pub fields: Vec<CaptureField>,
    pub complete: bool,
}

impl CaptureSchema {
    pub fn empty() -> Self {
        Self { fields: Vec::new(), complete: true }
    }
}

/// The frame slots visible to a frame-bound callable. Slot ordinals are the
/// logical layout contract; a changed set or order is incompatible.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct FrameSchema {
    pub slots: Vec<u32>,
}

/// Compatibility facts carried beside one logical code identity.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct CodeCompatibility {
    /// Canonical ABI shape, never a build-local `RepId` or backend symbol.
    pub call_abi: String,
    pub capture: CaptureSchema,
    pub frame: Option<FrameSchema>,
    pub lambda_topology: LambdaTopologyFingerprint,
}

impl CodeCompatibility {
    pub fn compatible_with(&self, other: &Self) -> bool {
        self.capture.complete
            && other.capture.complete
            && self == other
    }
}

/// One callable body in an artifact's identity table.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CodeEntry {
    pub identity: CodeIdentity,
    pub compatibility: CodeCompatibility,
    /// A body-content witness used only to reject duplicate identities naming
    /// different bodies. It is not used as identity.
    pub body_fingerprint: u64,
    /// The current backend body realization. This is payload, not identity;
    /// overlays replace it only after logical compatibility succeeds.
    pub body_name: Option<Ustr>,
    pub escaping: bool,
    pub capture_free: bool,
}

/// Self-validating counts for the escaping-value population.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct CodeIdentityCensus {
    pub examined: usize,
    pub assigned: usize,
    pub missing_identity: usize,
    pub duplicate_identity: usize,
    pub duplicate_body: usize,
    pub unresolved_body: usize,
    pub direct_call_only: usize,
    pub named_values: usize,
    pub lambda_values: usize,
    pub capturing_lambdas: usize,
    pub capture_free_lambdas: usize,
}

impl CodeIdentityCensus {
    pub fn is_total(&self) -> bool {
        self.examined == self.assigned
            && self.missing_identity == 0
            && self.duplicate_identity == 0
            && self.duplicate_body == 0
            && self.unresolved_body == 0
    }
}

/// Artifact-owned logical callable table. Backends may attach physical bodies
/// to these entries, but they do not mint or compare the keys.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct CodeArtifact {
    pub entries: BTreeMap<CodeIdentity, CodeEntry>,
    pub census: CodeIdentityCensus,
    pub binding_plan: ArtifactBindingPlan,
}

impl CodeArtifact {
    pub fn body_fingerprint(&self, identity: &CodeIdentity) -> Option<u64> {
        self.entries.get(identity).map(|entry| entry.body_fingerprint)
    }

    pub fn merge(&mut self, other: Self) {
        self.census.direct_call_only += other.census.direct_call_only;
        self.census.missing_identity += other.census.missing_identity;
        self.census.duplicate_identity += other.census.duplicate_identity;
        self.census.duplicate_body += other.census.duplicate_body;
        for entry in other.entries.into_values() {
            self.insert(entry);
        }
    }

    pub fn merge_ref(&mut self, other: &Self) {
        self.census.direct_call_only += other.census.direct_call_only;
        self.census.missing_identity += other.census.missing_identity;
        self.census.duplicate_identity += other.census.duplicate_identity;
        self.census.duplicate_body += other.census.duplicate_body;
        for entry in other.entries.values() {
            self.insert(entry.clone());
        }
    }

    /// Attach backend payload names after emission has chosen its physical
    /// symbols. The logical keys remain unchanged.
    pub fn set_body_names(&mut self, root: &MirFunc, root_name: &str) {
        fn visit(artifact: &mut CodeArtifact, mf: &MirFunc, body_name: &str) {
            if let Some(identity) = &mf.code_identity {
                if let Some(entry) = artifact.entries.get_mut(identity) {
                    entry.body_name = Some(Ustr::from(body_name));
                }
            }
            for nested in &mf.nested_fns {
                visit(artifact, nested, nested.name.as_str());
            }
        }

        visit(self, root, root_name);
    }

    /// Build the identity table for one transformed MIR tree. The root body is
    /// a definition and is not automatically a slot; nested lambda bodies are
    /// escaping values. Named value mints are marked separately, so a direct
    /// call-only declaration does not acquire a slot merely because it exists
    /// in the artifact.
    pub fn from_mir_tree(root: &MirFunc) -> Self {
        Self::from_mir_tree_with_volatile_vars(root, &FxHashSet::default())
    }

    /// Build the identity table while treating explicitly volatile comptime
    /// values as metadata in body witnesses. The surrounding code shape is
    /// still hashed, so a body edit cannot hide behind a changing build clock.
    pub fn from_mir_tree_with_volatile_vars(
        root: &MirFunc,
        volatile_vars: &FxHashSet<Ustr>,
    ) -> Self {
        let mut out = Self::default();
        out.visit_mir(root, false, volatile_vars);
        out.unify_owner_topologies();
        out
    }

    /// Present one lambda topology per owner.
    ///
    /// A coroutine's step, defer and hook regions (and any future sibling
    /// bodies sharing an owner) each compute their topology from their own
    /// lambda population, so one owner legitimately owns several distinct
    /// non-default topologies. The compatibility gate compares topologies at
    /// the owner boundary, so the artifact rewrites every non-default
    /// topology of a multi-topology owner to the hash of the sorted distinct
    /// set. The union tracks changes to that distinct set, not which region
    /// owns each fingerprint or how many regions share it: swapping region
    /// fingerprints or changing their multiplicity leaves the union unchanged.
    /// An unrelated body-only edit leaves every union untouched. Owners with
    /// a single topology keep their site-derived fingerprint.
    pub fn unify_owner_topologies(&mut self) {
        let mut per_owner: BTreeMap<CodeOwner, BTreeSet<LambdaTopologyFingerprint>> =
            BTreeMap::new();
        for entry in self.entries.values() {
            let Some(owner) = entry.identity.owner() else { continue };
            let topology = entry.compatibility.lambda_topology;
            if topology == LambdaTopologyFingerprint::default() {
                continue;
            }
            per_owner.entry(owner).or_default().insert(topology);
        }
        let mut unions: BTreeMap<CodeOwner, LambdaTopologyFingerprint> = BTreeMap::new();
        for (owner, topologies) in &per_owner {
            if topologies.len() > 1 {
                unions.insert(
                    owner.clone(),
                    LambdaTopologyFingerprint::unified(topologies.iter().copied()),
                );
            }
        }
        if unions.is_empty() {
            return;
        }
        for entry in self.entries.values_mut() {
            if entry.compatibility.lambda_topology == LambdaTopologyFingerprint::default() {
                continue;
            }
            let Some(owner) = entry.identity.owner() else { continue };
            if let Some(unified) = unions.get(&owner) {
                entry.compatibility.lambda_topology = *unified;
            }
        }
    }

    fn visit_mir(&mut self, mf: &MirFunc, escaping: bool, volatile_vars: &FxHashSet<Ustr>) {
        match (&mf.code_identity, &mf.code_compatibility) {
            (Some(identity), Some(compatibility)) => self.insert(CodeEntry {
                identity: identity.clone(),
                compatibility: compatibility.clone(),
                body_fingerprint: body_fingerprint(mf, volatile_vars),
                body_name: Some(mf.name),
                escaping,
                capture_free: compatibility.capture.fields.is_empty(),
            }),
            _ if escaping => self.record_missing(false),
            _ => {}
        }
        for identity in mf.code_value_identities.values() {
            self.mark_value(identity.clone());
        }
        let value_targets: FxHashSet<_> = mf
            .code_value_identities
            .values()
            .filter_map(|identity| match identity {
                CodeIdentity::Named(instance) => Some(instance.func),
                CodeIdentity::Artifact(_)
                | CodeIdentity::Lambda { .. }
                | CodeIdentity::Nested { .. }
                | CodeIdentity::Generated { .. } => None,
            })
            .collect();
        for block in crate::cfg::reachable_blocks_from(mf, mf.entry) {
            for inst in &mf.blocks[block].insts {
                let target = match mf.insts[*inst] {
                    crate::mir::Inst::Call { func, .. }
                    | crate::mir::Inst::CallMulti { func, .. } => Some(func),
                    _ => None,
                };
                if target.is_some_and(|target| !value_targets.contains(&target)) {
                    self.census.direct_call_only += 1;
                }
            }
        }
        for nested in &mf.nested_fns {
            self.visit_mir(nested, true, volatile_vars);
        }
    }

    /// Mark a compiler-generated or MIR-observed function value as escaping.
    ///
    /// Generated registration glue (decorators and `@sim`) is outside MIR, but
    /// it still creates the same logical function value. Keeping this operation
    /// on the artifact prevents those glue paths from becoming a second identity
    /// mechanism.
    pub fn mark_value(&mut self, identity: CodeIdentity) {
        if let Some(entry) = self.entries.get_mut(&identity) {
            if entry.escaping {
                return;
            }
            entry.escaping = true;
            self.census.examined += 1;
            self.census.assigned += 1;
            match &entry.identity {
                CodeIdentity::Named(_)
                | CodeIdentity::Artifact(_)
                | CodeIdentity::Nested { .. }
                | CodeIdentity::Generated { .. } => {
                    self.census.named_values += 1
                }
                CodeIdentity::Lambda { .. } => {
                    self.census.lambda_values += 1;
                    if entry.capture_free {
                        self.census.capture_free_lambdas += 1;
                    } else {
                        self.census.capturing_lambdas += 1;
                    }
                }
            }
            return;
        }
        self.insert(CodeEntry {
            identity,
            compatibility: CodeCompatibility {
                call_abi: "unresolved-named-value".to_string(),
                capture: CaptureSchema::empty(),
                frame: None,
                lambda_topology: LambdaTopologyFingerprint::default(),
            },
            body_fingerprint: 0,
            body_name: None,
            escaping: true,
            capture_free: true,
        });
    }

    /// Add a compiler-generated callable whose body is emitted outside MIR.
    /// The runtime still sees the same logical identity table as an ordinary
    /// escaping value.
    pub fn add_generated_value(
        &mut self,
        identity: CodeIdentity,
        compatibility: CodeCompatibility,
        body_name: &str,
    ) {
        let mut fingerprint = Fnv64::new();
        fingerprint.write_str(&format!("{identity:?}"));
        self.insert(CodeEntry {
            identity,
            compatibility,
            body_fingerprint: fingerprint.finish(),
            body_name: Some(Ustr::from(body_name)),
            escaping: true,
            capture_free: true,
        });
    }

    pub fn insert(&mut self, entry: CodeEntry) {
        if self.entries.contains_key(&entry.identity) {
            let mut newly_counted = None;
            let mut resolved = false;
            let mut duplicate = false;
            let mut capture_kind_change = None;
            {
                let old = self.entries.get_mut(&entry.identity).expect("identity still present");
                if old.body_fingerprint == 0 && entry.body_fingerprint != 0 {
                    resolved = old.escaping;
                    let was_escaping = old.escaping;
                    if old.escaping
                        && matches!(old.identity, CodeIdentity::Lambda { .. })
                        && old.capture_free != entry.capture_free
                    {
                        capture_kind_change = Some((old.capture_free, entry.capture_free));
                    }
                    let escaping = old.escaping || entry.escaping;
                    *old = entry;
                    old.escaping = escaping;
                    if escaping && !was_escaping {
                        newly_counted = Some((old.identity.clone(), old.capture_free));
                    }
                } else if old.body_fingerprint != 0 && entry.body_fingerprint == 0 {
                    if entry.escaping && !old.escaping {
                        old.escaping = true;
                        newly_counted = Some((old.identity.clone(), old.capture_free));
                    }
                } else if old.body_fingerprint != entry.body_fingerprint {
                    duplicate = true;
                }
            }
            if duplicate {
                self.census.duplicate_identity += 1;
                self.census.duplicate_body += 1;
            }
            if resolved {
                self.census.unresolved_body = self.census.unresolved_body.saturating_sub(1);
            }
            if let Some((was_free, is_free)) = capture_kind_change {
                if was_free {
                    self.census.capture_free_lambdas = self.census.capture_free_lambdas.saturating_sub(1);
                } else {
                    self.census.capturing_lambdas = self.census.capturing_lambdas.saturating_sub(1);
                }
                if is_free {
                    self.census.capture_free_lambdas += 1;
                } else {
                    self.census.capturing_lambdas += 1;
                }
            }
            if let Some((identity, capture_free)) = newly_counted {
                self.census.examined += 1;
                self.census.assigned += 1;
                self.count_kind(&identity, capture_free);
            }
            return;
        }
        if entry.escaping {
            self.census.examined += 1;
            self.census.assigned += 1;
            self.count_kind(&entry.identity, entry.capture_free);
            if entry.body_fingerprint == 0 {
                self.census.unresolved_body += 1;
            }
        }
        self.entries.insert(entry.identity.clone(), entry);
    }

    fn count_kind(&mut self, identity: &CodeIdentity, capture_free: bool) {
        match identity {
            CodeIdentity::Named(_)
            | CodeIdentity::Artifact(_)
            | CodeIdentity::Nested { .. }
            | CodeIdentity::Generated { .. } => {
                self.census.named_values += 1
            }
            CodeIdentity::Lambda { .. } => {
                self.census.lambda_values += 1;
                if capture_free {
                    self.census.capture_free_lambdas += 1;
                } else {
                    self.census.capturing_lambdas += 1;
                }
            }
        }
    }

    pub fn record_missing(&mut self, direct_call_only: bool) {
        if direct_call_only {
            self.census.direct_call_only += 1;
        } else {
            self.census.examined += 1;
            self.census.missing_identity += 1;
        }
    }

    /// Compare the entries belonging to one owner. A topology mismatch is an
    /// owner-level incompatibility, even if all surviving individual keys look
    /// compatible.
    pub fn compatible_overlay(&self, next: &Self) -> bool {
        for (identity, old) in &self.entries {
            if !old.escaping {
                continue;
            }
            let Some(new) = next.entries.get(identity) else { return false };
            if !new.escaping {
                return false;
            }
            if old.compatibility.lambda_topology != new.compatibility.lambda_topology
                || !old.compatibility.compatible_with(&new.compatibility)
            {
                return false;
            }
        }
        if next
            .entries
            .values()
            .any(|entry| entry.escaping && !self.entries.contains_key(&entry.identity))
        {
            return false;
        }
        let mut owners: BTreeMap<CodeOwner, LambdaTopologyFingerprint> = BTreeMap::new();
        for entry in self.entries.values().chain(next.entries.values()) {
            let Some(owner) = entry.identity.owner() else { continue };
            let topology = entry.compatibility.lambda_topology;
            if topology == LambdaTopologyFingerprint::default() {
                continue;
            }
            if let Some(previous) = owners.insert(owner, topology) {
                if previous != topology {
                    return false;
                }
            }
        }
        true
    }

    /// Replace backend body payloads from an already-built compatible artifact.
    /// The identity keys remain those of `self`; this is the overlay operation's
    /// explicit separation between logical slot identity and current body name.
    pub fn with_replacements(&self, replacements: &[&Self]) -> Self {
        let mut next = self.clone();
        for replacement in replacements {
            for (identity, entry) in &replacement.entries {
                if entry.body_fingerprint != 0 || !next.entries.contains_key(identity) {
                    let mut entry = entry.clone();
                    if next.entries.get(identity).is_some_and(|old| old.escaping) {
                        entry.escaping = true;
                    }
                    next.entries.insert(identity.clone(), entry);
                }
            }
        }
        let direct_call_only = self.census.direct_call_only;
        let mut census = CodeIdentityCensus { direct_call_only, ..Default::default() };
        for entry in next.entries.values() {
            if !entry.escaping {
                continue;
            }
            census.examined += 1;
            census.assigned += 1;
            match &entry.identity {
                CodeIdentity::Named(_)
                | CodeIdentity::Artifact(_)
                | CodeIdentity::Nested { .. }
                | CodeIdentity::Generated { .. } => {
                    census.named_values += 1
                }
                CodeIdentity::Lambda { .. } => {
                    census.lambda_values += 1;
                    if entry.capture_free {
                        census.capture_free_lambdas += 1;
                    } else {
                        census.capturing_lambdas += 1;
                    }
                }
            }
            if entry.body_fingerprint == 0 {
                census.unresolved_body += 1;
            }
        }
        next.census = census;
        next
    }

}

fn body_fingerprint(mf: &MirFunc, volatile_vars: &FxHashSet<Ustr>) -> u64 {
    // This is a duplicate-body witness only. It intentionally excludes the
    // identity/compatibility fields and is canonicalized across MIR clones;
    // it is never used to find a slot.
    // Generic inlining can leave unrelated cloned lowering artifacts in the
    // same arenas. Hash only the reachable body so those artifacts cannot make
    // two copies of one lambda look like different bodies.
    let volatile_comptime_spans = mf
        .comptime_reads
        .iter()
        .filter_map(|read| match &read.input {
            crate::mir::ComptimeInputId::Var(name) if volatile_vars.contains(name) => Some(read.span),
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut hasher = BodyHasher::default();
    for block in crate::cfg::reachable_blocks_from(mf, mf.entry) {
        hasher.tag(1);
        for inst_id in &mf.blocks[block].insts {
            let volatile = mf
                .inst_spans
                .get(*inst_id)
                .is_some_and(|span| volatile_comptime_spans.contains(span));
            hasher.inst_with_volatile(&mf.insts[*inst_id], &mf.nested_fn_identities, volatile);
        }
        hasher.tag(2);
        hasher.term(&mf.blocks[block].term);
    }
    hasher.finish()
}

struct BodyHasher {
    hash: Fnv64,
    inst_ids: FxHashMap<u32, usize>,
    cell_ids: FxHashMap<u32, usize>,
    block_ids: FxHashMap<u32, usize>,
    rep_ids: FxHashMap<u32, usize>,
}

impl Default for BodyHasher {
    fn default() -> Self {
        Self {
            hash: Fnv64::new(),
            inst_ids: FxHashMap::default(),
            cell_ids: FxHashMap::default(),
            block_ids: FxHashMap::default(),
            rep_ids: FxHashMap::default(),
        }
    }
}

impl BodyHasher {
    fn tag(&mut self, tag: u8) {
        self.hash.write_u8(tag);
    }

    fn string(&mut self, value: &str) {
        self.hash.write_str(value);
    }

    fn boolean(&mut self, value: bool) {
        self.hash.write_u8(value as u8);
    }

    fn index<T>(map: &mut FxHashMap<u32, usize>, idx: Idx<T>) -> usize {
        let raw = idx.into_raw().into_u32();
        let next = map.len();
        *map.entry(raw).or_insert(next)
    }

    fn inst_idx(&mut self, idx: Idx<Inst>) {
        self.hash.write_usize(Self::index(&mut self.inst_ids, idx));
    }

    fn cell_idx(&mut self, idx: Idx<crate::mir::Cell>) {
        self.hash.write_usize(Self::index(&mut self.cell_ids, idx));
    }

    fn block_idx(&mut self, idx: Idx<crate::mir::Block>) {
        self.hash.write_usize(Self::index(&mut self.block_ids, idx));
    }

    fn rep_id(&mut self, id: crate::lower::RepId) {
        let next = self.rep_ids.len();
        let canonical = *self.rep_ids.entry(id.raw()).or_insert(next);
        self.hash.write_usize(canonical);
    }

    fn option_rep_id(&mut self, id: Option<crate::lower::RepId>) {
        match id {
            Some(id) => {
                self.boolean(true);
                self.rep_id(id);
            }
            None => self.boolean(false),
        }
    }

    fn value_ctor_form(&mut self, form: ValueCtorForm) {
        self.tag(match form {
            ValueCtorForm::Declared => 0,
            ValueCtorForm::Leaves => 1,
        });
    }

    fn inst_list(&mut self, values: &[Idx<Inst>]) {
        self.hash.write_usize(values.len());
        for value in values {
            self.inst_idx(*value);
        }
    }

    fn cell_list(&mut self, values: &[Idx<crate::mir::Cell>]) {
        self.hash.write_usize(values.len());
        for value in values {
            self.cell_idx(*value);
        }
    }

    fn unary(&mut self, tag: u8, value: Idx<Inst>) {
        self.tag(tag);
        self.inst_idx(value);
    }

    fn binary(&mut self, tag: u8, left: Idx<Inst>, right: Idx<Inst>) {
        self.tag(tag);
        self.inst_idx(left);
        self.inst_idx(right);
    }

    fn option_string(&mut self, value: Option<Ustr>) {
        match value {
            Some(value) => {
                self.boolean(true);
                self.string(value.as_str());
            }
            None => self.boolean(false),
        }
    }

    fn aliased_name(&mut self, name: Ustr, aliases: &FxHashMap<Ustr, CodeIdentity>) {
        if let Some(identity) = aliases.get(&name) {
            self.tag(1);
            self.string(&format!("__rock_nested_{identity:?}"));
        } else {
            self.tag(0);
            self.string(name.as_str());
        }
    }

    fn func_ref(&mut self, func: FuncRef) {
        self.hash.write_u32(func.module.raw());
        self.hash.write_u32(func.func.into_raw().into_u32());
    }

    fn lit(&mut self, lit: &Lit, volatile_comptime: bool) {
        match lit {
            Lit::Int(value) => {
                self.tag(0);
                self.hash.write_u64(*value as u64);
            }
            Lit::Uint(value) => {
                self.tag(1);
                self.hash.write_u32(*value);
            }
            Lit::Float(value) => {
                self.tag(2);
                self.hash.write_u64(value.to_bits());
            }
            Lit::Bool(value) => {
                self.tag(3);
                self.boolean(*value);
            }
            Lit::Str(value) => {
                if volatile_comptime {
                    self.tag(6);
                } else {
                    self.tag(4);
                    self.string(value.as_str());
                }
            }
            Lit::Nil => self.tag(5),
        }
    }

    fn column_elem(&mut self, elem: ColumnElem) {
        self.string(elem.name.as_str());
        self.boolean(elem.nullable);
        self.rep_id(elem.representation());
        self.boolean(elem.is_nobox());
    }

    fn container(&mut self, kind: ContainerKind) {
        match kind {
            ContainerKind::Vec => self.tag(0),
            ContainerKind::NativeBuffer => self.tag(1),
            ContainerKind::NativeBufferValue(name) => {
                self.tag(2);
                self.string(name.as_str());
            }
            ContainerKind::VecValueColumns(elem) => {
                self.tag(3);
                self.column_elem(elem);
            }
            ContainerKind::VecColumn => self.tag(4),
            ContainerKind::NativeBufferLeaf => self.tag(5),
            ContainerKind::Map => self.tag(6),
            ContainerKind::VecValueStrided(elem) => {
                self.tag(7);
                self.column_elem(elem);
            }
            ContainerKind::VecStrideSlot => self.tag(8),
        }
    }

    fn coro_hook(&mut self, kind: CoroHookKind) {
        self.tag(match kind {
            CoroHookKind::OnResume => 0,
            CoroHookKind::OnKilled => 1,
            CoroHookKind::OnHotReload => 2,
        });
    }

    fn inst_with_volatile(
        &mut self,
        inst: &Inst,
        aliases: &FxHashMap<Ustr, CodeIdentity>,
        volatile_comptime: bool,
    ) {
        match inst {
            Inst::Const(lit) => {
                self.tag(0);
                self.lit(lit, volatile_comptime);
            }
            Inst::Global(name) => {
                self.tag(1);
                self.aliased_name(*name, aliases);
            }
            Inst::Load(cell) => {
                self.tag(2);
                self.cell_idx(*cell);
            }
            Inst::IntAdd(a, b) => self.binary(3, *a, *b),
            Inst::IntSub(a, b) => self.binary(4, *a, *b),
            Inst::IntMul(a, b) => self.binary(5, *a, *b),
            Inst::IntDiv(a, b) => self.binary(6, *a, *b),
            Inst::IntMod(a, b) => self.binary(7, *a, *b),
            Inst::UintAdd(a, b) => self.binary(8, *a, *b),
            Inst::UintSub(a, b) => self.binary(9, *a, *b),
            Inst::UintMul(a, b) => self.binary(10, *a, *b),
            Inst::UintDiv(a, b) => self.binary(11, *a, *b),
            Inst::UintMod(a, b) => self.binary(12, *a, *b),
            Inst::FloatAdd(a, b) => self.binary(13, *a, *b),
            Inst::FloatSub(a, b) => self.binary(14, *a, *b),
            Inst::FloatMul(a, b) => self.binary(15, *a, *b),
            Inst::FloatDiv(a, b) => self.binary(16, *a, *b),
            Inst::FloatMod(a, b) => self.binary(17, *a, *b),
            Inst::StringConcat(a, b) => self.binary(18, *a, *b),
            Inst::ToString(value) => self.unary(19, *value),
            Inst::Format { value, fmt } => {
                self.tag(20);
                self.inst_idx(*value);
                self.string(fmt.as_str());
            }
            Inst::FormatDyn { value, fmt } => {
                self.tag(21);
                self.inst_idx(*value);
                self.inst_idx(*fmt);
            }
            Inst::IntLt(a, b) => self.binary(22, *a, *b),
            Inst::IntLeq(a, b) => self.binary(23, *a, *b),
            Inst::IntGt(a, b) => self.binary(24, *a, *b),
            Inst::IntGeq(a, b) => self.binary(25, *a, *b),
            Inst::UintLt(a, b) => self.binary(26, *a, *b),
            Inst::UintLeq(a, b) => self.binary(27, *a, *b),
            Inst::UintGt(a, b) => self.binary(28, *a, *b),
            Inst::UintGeq(a, b) => self.binary(29, *a, *b),
            Inst::FloatLt(a, b) => self.binary(30, *a, *b),
            Inst::FloatLeq(a, b) => self.binary(31, *a, *b),
            Inst::FloatGt(a, b) => self.binary(32, *a, *b),
            Inst::FloatGeq(a, b) => self.binary(33, *a, *b),
            Inst::Eq(a, b) => self.binary(34, *a, *b),
            Inst::NotEq(a, b) => self.binary(35, *a, *b),
            Inst::FloatToInt(value) => self.unary(36, *value),
            Inst::IntToFloat(value) => self.unary(37, *value),
            Inst::UintToFloat(value) => self.unary(38, *value),
            Inst::FloatToF32(value) => self.unary(93, *value),
            Inst::IntToUint(value) => self.unary(39, *value),
            Inst::UintToInt(value) => self.unary(40, *value),
            Inst::FloatToUint(value) => self.unary(41, *value),
            Inst::UintBitcastInt(value) => self.unary(42, *value),
            Inst::IntBitcastUint(value) => self.unary(43, *value),
            Inst::Neg(value) => self.unary(44, *value),
            Inst::Not(value) => self.unary(45, *value),
            Inst::BitNot(value) => self.unary(46, *value),
            Inst::UintBitNot(value) => self.unary(47, *value),
            Inst::BitAnd(a, b) => self.binary(48, *a, *b),
            Inst::BitOr(a, b) => self.binary(49, *a, *b),
            Inst::BitXor(a, b) => self.binary(50, *a, *b),
            Inst::Shl(a, b) => self.binary(51, *a, *b),
            Inst::Shr(a, b) => self.binary(52, *a, *b),
            Inst::UintBitAnd(a, b) => self.binary(53, *a, *b),
            Inst::UintBitOr(a, b) => self.binary(54, *a, *b),
            Inst::UintBitXor(a, b) => self.binary(55, *a, *b),
            Inst::UintShl(a, b) => self.binary(56, *a, *b),
            Inst::UintShr(a, b) => self.binary(57, *a, *b),
            Inst::Call { func, args } => {
                self.tag(58);
                self.func_ref(*func);
                self.inst_list(args);
            }
            Inst::CallMulti { dests, func, args } => {
                self.tag(59);
                self.cell_list(dests);
                self.func_ref(*func);
                self.inst_list(args);
            }
            Inst::CallValue { callee, args } => {
                self.tag(60);
                self.inst_idx(*callee);
                self.inst_list(args);
            }
            Inst::CallValueMulti { dests, callee, args } => {
                self.tag(61);
                self.cell_list(dests);
                self.inst_idx(*callee);
                self.inst_list(args);
            }
            Inst::CallBoundMulti { dests, generic_idx, args } => {
                self.tag(62);
                self.cell_list(dests);
                self.hash.write_u32(*generic_idx);
                self.inst_list(args);
            }
            Inst::Lambda { params, entry } => {
                self.tag(63);
                self.cell_list(params);
                self.block_idx(*entry);
            }
            Inst::MakeTuple(values) => {
                self.tag(64);
                self.inst_list(values);
            }
            Inst::MakeVec(values) => {
                self.tag(65);
                self.inst_list(values);
            }
            Inst::MakeVecColumns { elem, representation, nobox, columns, len, capacity } => {
                self.tag(66);
                self.string(elem.as_str());
                self.rep_id(*representation);
                self.boolean(*nobox);
                self.hash.write_usize(columns.len());
                for (name, values) in columns {
                    self.string(name.as_str());
                    self.inst_list(values);
                }
                self.hash.write_usize(*len);
                self.inst_idx(*capacity);
            }
            Inst::MakeVecColumnHeader { elem, representation, nobox, capacity } => {
                self.tag(67);
                self.string(elem.as_str());
                self.rep_id(*representation);
                self.boolean(*nobox);
                self.inst_idx(*capacity);
            }
            Inst::MakeVecStrided {
                elem,
                representation,
                nobox,
                stride,
                values,
                len,
                capacity,
            } => {
                self.tag(89);
                self.string(elem.as_str());
                self.rep_id(*representation);
                self.boolean(*nobox);
                self.hash.write_usize(*stride);
                self.inst_list(values);
                self.hash.write_usize(*len);
                self.inst_idx(*capacity);
            }
            Inst::MakeVecStridedHeader {
                elem,
                representation,
                nobox,
                stride,
                capacity,
            } => {
                self.tag(90);
                self.string(elem.as_str());
                self.rep_id(*representation);
                self.boolean(*nobox);
                self.hash.write_usize(*stride);
                self.inst_idx(*capacity);
            }
            Inst::VecStrideNilToFalse(value) => {
                self.tag(91);
                self.inst_idx(*value);
            }
            Inst::VecStrideFalseToNil(value) => {
                self.tag(92);
                self.inst_idx(*value);
            }
            Inst::MakeStruct { ty, fields } => {
                self.tag(68);
                self.option_string(*ty);
                self.hash.write_usize(fields.len());
                for (name, value) in fields {
                    self.string(name.as_str());
                    self.inst_idx(*value);
                }
            }
            Inst::MakeValueStruct { name, representation, form, fields } => {
                self.tag(69);
                self.string(name.as_str());
                self.rep_id(*representation);
                self.value_ctor_form(*form);
                self.hash.write_usize(fields.len());
                for (field, value) in fields {
                    self.string(field.as_str());
                    self.inst_idx(*value);
                }
            }
            Inst::MaterializeValueStruct { name, representation, form, fields } => {
                self.tag(70);
                self.string(name.as_str());
                self.rep_id(*representation);
                self.value_ctor_form(*form);
                self.hash.write_usize(fields.len());
                for (field, value) in fields {
                    self.string(field.as_str());
                    self.inst_idx(*value);
                }
            }
            Inst::MakeNullableValue { present, value } => {
                self.tag(89);
                self.inst_idx(*present);
                self.inst_idx(*value);
            }
            Inst::MakeEnum { tag, ty, positional, named } => {
                self.tag(71);
                self.string(tag.as_str());
                self.string(ty.as_str());
                self.inst_list(positional);
                self.hash.write_usize(named.len());
                for (name, value) in named {
                    self.string(name.as_str());
                    self.inst_idx(*value);
                }
            }
            Inst::MakeEnumDyn { tag, ty, positional, named } => {
                self.tag(72);
                self.inst_idx(*tag);
                self.string(ty.as_str());
                self.inst_list(positional);
                self.hash.write_usize(named.len());
                for (name, value) in named {
                    self.string(name.as_str());
                    self.inst_idx(*value);
                }
            }
            Inst::MakeEnv { fn_ref, fields } => {
                self.tag(73);
                match fn_ref {
                    Some(name) => {
                        self.boolean(true);
                        self.aliased_name(*name, aliases);
                    }
                    None => self.boolean(false),
                }
                self.hash.write_usize(fields.len());
                for (name, value) in fields {
                    self.string(name.as_str());
                    self.inst_idx(*value);
                }
            }
            Inst::FfiNewArray { ctype, count, representation } => {
                self.tag(74);
                self.string(ctype.as_str());
                self.inst_idx(*count);
                self.option_rep_id(*representation);
            }
            Inst::FfiViewCast { cty, ptr } => {
                self.tag(75);
                self.string(cty.as_str());
                self.inst_idx(*ptr);
            }
            Inst::Field { base, field } => {
                self.tag(76);
                self.inst_idx(*base);
                self.string(field.as_str());
            }
            Inst::TupleIndex { base, index } => {
                self.tag(77);
                self.inst_idx(*base);
                self.hash.write_usize(*index);
            }
            Inst::Index { base, index, kind } => {
                self.tag(78);
                self.inst_idx(*base);
                self.inst_idx(*index);
                self.container(*kind);
            }
            Inst::VecCheckedIndex { vec, index } => {
                self.tag(79);
                self.inst_idx(*vec);
                self.inst_idx(*index);
            }
            Inst::Len { base, kind } => {
                self.tag(80);
                self.inst_idx(*base);
                self.container(*kind);
            }
            Inst::Store { cell, value } => {
                self.tag(81);
                self.cell_idx(*cell);
                self.inst_idx(*value);
            }
            Inst::StoreField { base, field, value } => {
                self.tag(82);
                self.inst_idx(*base);
                self.string(field.as_str());
                self.inst_idx(*value);
            }
            Inst::StoreIndex { base, index, value, kind } => {
                self.tag(83);
                self.inst_idx(*base);
                self.inst_idx(*index);
                self.inst_idx(*value);
                self.container(*kind);
            }
            Inst::StoreGlobal { name, value } => {
                self.tag(84);
                self.string(name.as_str());
                self.inst_idx(*value);
            }
            Inst::ArrayPush { arr, value, kind } => {
                self.tag(85);
                self.inst_idx(*arr);
                self.inst_idx(*value);
                self.container(*kind);
            }
            Inst::CoroSetInner(value) => self.unary(86, *value),
            Inst::CoroHook { kind, lambda } => {
                self.tag(87);
                self.coro_hook(*kind);
                self.inst_idx(*lambda);
            }
            Inst::MapNext { key_cell, val_cell, table } => {
                self.tag(88);
                self.cell_idx(*key_cell);
                self.cell_idx(*val_cell);
                self.inst_idx(*table);
            }
        }
    }

    fn term(&mut self, term: &Term) {
        match term {
            Term::Return(values) => {
                self.tag(0);
                self.inst_list(values);
            }
            Term::Unreachable => self.tag(1),
            Term::Goto(block) => {
                self.tag(2);
                self.block_idx(*block);
            }
            Term::Branch { cond, then_bb, else_bb } => {
                self.tag(3);
                self.inst_idx(*cond);
                self.block_idx(*then_bb);
                self.block_idx(*else_bb);
            }
            Term::Switch { discr, cases, default } => {
                self.tag(4);
                self.inst_idx(*discr);
                self.hash.write_usize(cases.len());
                for (lit, block) in cases {
                    self.lit(lit, false);
                    self.block_idx(*block);
                }
                self.block_idx(*default);
            }
            Term::Yield { values, resume } => {
                self.tag(5);
                self.inst_list(values);
                self.block_idx(*resume);
            }
        }
    }

    fn finish(self) -> u64 {
        self.hash.finish()
    }
}

struct Fnv64(u64);

impl Fnv64 {
    fn new() -> Self {
        Self(0xcbf29ce484222325)
    }

    fn write_u8(&mut self, byte: u8) {
        self.0 ^= u64::from(byte);
        self.0 = self.0.wrapping_mul(0x100000001b3);
    }

    fn write_u32(&mut self, value: u32) {
        for byte in value.to_le_bytes() {
            self.write_u8(byte);
        }
    }

    fn write_u64(&mut self, value: u64) {
        for byte in value.to_le_bytes() {
            self.write_u8(byte);
        }
    }

    fn write_usize(&mut self, value: usize) {
        self.write_u64(value as u64);
    }

    fn write_str(&mut self, value: &str) {
        self.write_usize(value.len());
        for byte in value.as_bytes() {
            self.write_u8(*byte);
        }
    }

    fn finish(self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bone_semantic::prelude::ModuleId;
    use comfy_la_arena::{Idx, RawIdx};

    fn declaration() -> crate::mir::FuncRef {
        crate::mir::FuncRef {
            module: ModuleId::builtin(),
            func: Idx::from_raw(RawIdx::from_u32(0)),
        }
    }

    #[test]
    fn mono_arguments_and_enclosing_instances_are_identity_axes() {
        let interner = crate::monoty::MonoInterner::new();
        let int_instance = Instance::intern(&interner, declaration(), vec![crate::monoty::MonoTy::Int]);
        let float_instance = Instance::intern(&interner, declaration(), vec![crate::monoty::MonoTy::Float]);
        assert_ne!(
            CodeIdentity::Named(int_instance.clone()),
            CodeIdentity::Named(float_instance.clone()),
            "two concrete generic instances must not share a code identity"
        );
        let site = LambdaSiteId { file: Ustr::from("same.rock"), offset: 12 };
        assert_ne!(
            CodeIdentity::Lambda {
                enclosing: CodeOwner::Instance(int_instance),
                site,
            },
            CodeIdentity::Lambda {
                enclosing: CodeOwner::Instance(float_instance),
                site,
            },
            "the enclosing concrete instance is part of lambda identity"
        );
    }

    #[test]
    fn duplicate_identity_with_different_body_is_not_a_valid_artifact() {
        let identity = CodeIdentity::Named(Instance::base(declaration()));
        let compatibility = CodeCompatibility {
            call_abi: "unit".to_string(),
            capture: CaptureSchema::empty(),
            frame: None,
            lambda_topology: LambdaTopologyFingerprint::default(),
        };
        let entry = |body_fingerprint| CodeEntry {
            identity: identity.clone(),
            compatibility: compatibility.clone(),
            body_fingerprint,
            body_name: Some(Ustr::from("body")),
            escaping: true,
            capture_free: true,
        };
        let mut artifact = CodeArtifact::default();
        artifact.insert(entry(1));
        artifact.insert(entry(2));
        assert_eq!(artifact.census.duplicate_identity, 1);
        assert_eq!(artifact.census.duplicate_body, 1);
        assert!(!artifact.census.is_total());
    }

    #[test]
    fn borrowed_merge_matches_owned_merge() {
        let identity = CodeIdentity::Named(Instance::base(declaration()));
        let compatibility = CodeCompatibility {
            call_abi: "unit".to_string(),
            capture: CaptureSchema::empty(),
            frame: None,
            lambda_topology: LambdaTopologyFingerprint::default(),
        };
        let entry = CodeEntry {
            identity,
            compatibility,
            body_fingerprint: 1,
            body_name: Some(Ustr::from("body")),
            escaping: true,
            capture_free: true,
        };
        let mut owned = CodeArtifact::default();
        owned.insert(entry.clone());
        let mut borrowed = owned.clone();
        let incoming = CodeArtifact {
            entries: [(entry.identity.clone(), entry)].into_iter().collect(),
            census: CodeIdentityCensus::default(),
            ..CodeArtifact::default()
        };

        owned.merge(incoming.clone());
        borrowed.merge_ref(&incoming);
        assert_eq!(borrowed, owned);
    }

    #[test]
    fn body_fingerprint_ignores_arena_offsets_but_keeps_body_shape() {
        let fingerprint = |cell_raw, inst_left, inst_right, extra| {
            let mut hasher = BodyHasher::default();
            let aliases = FxHashMap::default();
            let cell_idx = || Idx::from_raw(RawIdx::from_u32(cell_raw));
            let inst_idx = |raw| Idx::from_raw(RawIdx::from_u32(raw));
            hasher.inst_with_volatile(&Inst::Load(cell_idx()), &aliases, false);
            hasher.inst_with_volatile(
                &Inst::IntAdd(inst_idx(inst_left), inst_idx(inst_right)),
                &aliases,
                false,
            );
            if extra {
                hasher.inst_with_volatile(&Inst::Const(Lit::Int(1)), &aliases, false);
            }
            hasher.finish()
        };
        assert_eq!(
            fingerprint(10, 11, 10, false),
            fingerprint(900, 901, 900, false),
        );
        assert_ne!(
            fingerprint(10, 11, 10, false),
            fingerprint(10, 11, 10, true),
        );
    }
}
