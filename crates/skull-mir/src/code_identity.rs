//! Backend names, arena indices and `RepId`s are deliberately absent from these
//! identities: they are realization details, not artifact identity.
use std::collections::{BTreeMap, BTreeSet};

use bone_core::prelude::Span;
use comfy_la_arena::Idx;
use rustc_hash::{FxHashMap, FxHashSet};
use ustr::Ustr;

use crate::instance::Instance;
use crate::mir::{
    ColumnElem, ContainerKind, CoroHookKind, FuncRef, Inst, Lit, MirFunc, Term, ValueCtorForm,
};
use crate::monoty::MonoInterner;

/// A modules-v1 name is its DefKey, which already names the logical module, so
/// moving a definition between fragments of one module keeps its state and
/// code slot. A standalone definition keeps `<file>::<name>`.
pub(crate) fn logical_def_key(file: Ustr, name: Ustr) -> String {
    if let Some(key) = name
        .as_str()
        .strip_prefix(bone_core::modules::STANDALONE_PACKAGE_PREFIX)
    {
        return key.to_string();
    }
    match name.as_str().contains("::") {
        true => name.to_string(),
        false => format!("{file}::{name}"),
    }
}

/// The start offset is stable when the lambda body changes; the owner's
/// topology fingerprint rejects inserted, removed or reordered sibling sites.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct LambdaSiteId {
    pub file: Ustr,
    pub offset: u64,
}

impl LambdaSiteId {
    pub fn from_span(span: Span) -> Option<Self> {
        (span.file != "<unknown>").then_some(Self {
            file: span.file,
            offset: span.offset_range().0 as u64,
        })
    }
}

/// Always-emitted global/decorator roots have no declaration, so their source
/// artifact is named directly.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum CodeOwner {
    Instance(Instance),
    Artifact {
        kind: CodeOwnerKind,
        module: Ustr,
        name: Ustr,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum CodeOwnerKind {
    Global,
    Decorator,
}

#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum CodeIdentity {
    Named(Instance),
    /// Not itself an escaping value, but it owns lambda identities stored in a
    /// global or decorator artifact.
    Artifact(CodeOwner),
    Lambda {
        enclosing: CodeOwner,
        site: LambdaSiteId,
    },
    /// Owned by the enclosing declaration: nested functions do not enter the mono
    /// declaration graph as independent top-level declarations.
    Nested {
        enclosing: CodeOwner,
        site: LambdaSiteId,
        name: Ustr,
    },
    Generated {
        enclosing: CodeOwner,
        role: Ustr,
    },
}

impl CodeIdentity {
    pub fn owner(&self) -> CodeOwner {
        match self {
            Self::Named(i) => CodeOwner::Instance(i.clone()),
            Self::Artifact(owner)
            | Self::Lambda {
                enclosing: owner, ..
            }
            | Self::Nested {
                enclosing: owner, ..
            }
            | Self::Generated {
                enclosing: owner, ..
            } => owner.clone(),
        }
    }

    /// A stable runtime key: uses the shared declaration mangling to distinguish
    /// overloads and excludes arena indices and pointers.
    pub fn logical_key(
        &self,
        ws: &bone_core::workspace::Workspace,
        interner: &MonoInterner,
    ) -> String {
        let instance_key = |instance: &Instance| {
            let module = ws.module(instance.func.module);
            let function = &module.functions[instance.func.func];
            let def_key = logical_def_key(module.logical_file, function.name);
            let (scope, _) = def_key.rsplit_once("::").expect("logical definition scope");
            let args = instance
                .arg_types(interner)
                .iter()
                .map(|ty| ty.artifact_key())
                .collect::<Vec<_>>()
                .join(",");
            format!(
                "{}::{}<{}>",
                scope,
                crate::mangle::mangle_fn(ws, function),
                args
            )
        };
        let owner_key = |owner: &CodeOwner| match owner {
            CodeOwner::Instance(instance) => instance_key(instance),
            CodeOwner::Artifact { kind, module, name } => {
                format!(
                    "artifact:{kind:?}:{}",
                    logical_def_key(ws.logical_file(*module), *name)
                )
            }
        };
        // The owner key already names its file, so a site in that file is spelled by
        // offset alone; that keeps build paths out of the key.
        let site_key = |owner: &CodeOwner, site: &LambdaSiteId| {
            let owner_file = match owner {
                CodeOwner::Instance(instance) => ws.module(instance.func.module).file,
                CodeOwner::Artifact { module, .. } => *module,
            };
            match site.file == owner_file {
                true => site.offset.to_string(),
                false => format!("{}:{}", ws.logical_file(site.file), site.offset),
            }
        };
        match self {
            Self::Named(instance) => format!("named:{}", instance_key(instance)),
            Self::Artifact(owner) => format!("artifact-root:{}", owner_key(owner)),
            Self::Lambda { enclosing, site } => format!(
                "lambda:{}@{}",
                owner_key(enclosing),
                site_key(enclosing, site)
            ),
            Self::Nested {
                enclosing,
                site,
                name,
            } => format!(
                "nested:{}::{}@{}",
                owner_key(enclosing),
                name,
                site_key(enclosing, site)
            ),
            Self::Generated { enclosing, role } => {
                format!("generated:{}::{}", owner_key(enclosing), role)
            }
        }
    }
}

/// A source-site identity alone is not enough: inserting a lambda before an
/// existing one could let a positional implementation reuse the neighbour's
/// slot. Comparing this at the owner boundary makes that edit incompatible.
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

    /// Sorted, so region emission order cannot move it; tagged to separate the
    /// hash input from site-derived fingerprints. A single topology passes through.
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
    Binding {
        site: LambdaSiteId,
        projection: Ustr,
    },
    Generated {
        role: Ustr,
    },
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
            Self::Binding {
                site,
                projection: base,
            } => Self::Binding {
                site: *site,
                projection: append(*base),
            },
            Self::Generated { role } => Self::Generated {
                role: append(*role),
            },
        }
    }
}

/// Incomplete schemas fail compatibility closed rather than pretending two
/// unknown fields are interchangeable.
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
        Self {
            fields: Vec::new(),
            complete: true,
        }
    }
}

/// Slot ordinals are the logical layout contract; a changed set or order is
/// incompatible.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct FrameSchema {
    pub slots: Vec<u32>,
}

#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct CodeCompatibility {
    /// Canonical ABI shape, never a build-local `RepId` or backend symbol.
    pub call_abi: String,
    pub capture: CaptureSchema,
    pub frame: Option<FrameSchema>,
    pub lambda_topology: LambdaTopologyFingerprint,
}

impl CodeCompatibility {
    pub(crate) fn compatible_with(&self, other: &Self) -> bool {
        self.capture.complete && other.capture.complete && self == other
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CodeEntry {
    pub identity: CodeIdentity,
    pub compatibility: CodeCompatibility,
    /// Only rejects duplicate identities naming different bodies; never identity.
    pub body_fingerprint: u64,
    /// Payload, not identity; overlays replace it only after logical
    /// compatibility succeeds.
    pub body_name: Option<Ustr>,
    pub escaping: bool,
    pub capture_free: bool,
}

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct CodeIdentityCensus {
    pub missing_identity: usize,
    pub duplicate_identity: usize,
}

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct CodeArtifact {
    pub entries: BTreeMap<CodeIdentity, CodeEntry>,
    pub census: CodeIdentityCensus,
}

/// Kept in identity order; a `BTreeMap` would allocate a whole node, about
/// 2.5 KB, for a body's one or two entries.
#[derive(Clone, Debug, Default)]
pub struct BodyCodeArtifact {
    entries: Box<[CodeEntry]>,
    census: CodeIdentityCensus,
}

impl From<CodeArtifact> for BodyCodeArtifact {
    fn from(artifact: CodeArtifact) -> Self {
        Self {
            entries: artifact.entries.into_values().collect(),
            census: artifact.census,
        }
    }
}

impl BodyCodeArtifact {
    pub fn entries(&self) -> &[CodeEntry] {
        &self.entries
    }
}

pub struct StagedCodeSlot<'a> {
    pub key: String,
    pub body: Ustr,
    pub call_abi: &'a str,
}

/// An escaping entry without an emitted body is an error.
pub fn staged_code_slots<'a>(
    ws: &bone_core::workspace::Workspace,
    interner: &MonoInterner,
    source: impl IntoIterator<Item = &'a CodeEntry>,
) -> Result<Vec<StagedCodeSlot<'a>>, String> {
    use bone_core::prelude::*;
    let escaping: Vec<&CodeEntry> = source.into_iter().filter(|entry| entry.escaping).collect();
    let mut entries: Vec<(String, &CodeEntry)> = escaping
        .par_iter()
        .map(|entry| (entry.identity.logical_key(ws, interner), *entry))
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries
        .into_iter()
        .map(|(key, entry)| match entry.body_name {
            Some(body) => Ok(StagedCodeSlot {
                key,
                body,
                call_abi: entry.compatibility.call_abi.as_str(),
            }),
            None => Err(format!("Rock CodeRef `{key}` has no emitted body")),
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodeSlotPublication {
    pub publish: bool,
    /// Staging must be opened by an explicit begin, because no escaping entry is
    /// staged to open it.
    pub explicit_begin: bool,
}

impl CodeArtifact {
    pub fn has_escaping(&self) -> bool {
        self.entries.values().any(|entry| entry.escaping)
    }

    /// Slots are published when anything can hold a CodeRef: an escaping value,
    /// a definition catalog, or a lifecycle declaration.
    pub fn publication(
        &self,
        has_definition_plan: bool,
        has_lifecycle_plan: bool,
    ) -> CodeSlotPublication {
        let escaping = self.has_escaping();
        let publish = escaping || has_definition_plan || has_lifecycle_plan;
        CodeSlotPublication {
            publish,
            explicit_begin: publish && !escaping,
        }
    }

    pub fn is_total(&self) -> bool {
        self.census.missing_identity == 0
            && self.census.duplicate_identity == 0
            && !self
                .entries
                .values()
                .any(|entry| entry.escaping && entry.body_fingerprint == 0)
    }

    /// The census must be total before any slot is staged.
    pub fn check_stageable(&self) -> Result<(), String> {
        if self.is_total() {
            Ok(())
        } else {
            Err(format!(
                "cannot stage incomplete Rock CodeRef census: {:?}",
                self.census
            ))
        }
    }

    pub fn body_fingerprint(&self, identity: &CodeIdentity) -> Option<u64> {
        self.entries
            .get(identity)
            .map(|entry| entry.body_fingerprint)
    }

    fn add_census(&mut self, other: &CodeIdentityCensus) {
        self.census.missing_identity += other.missing_identity;
        self.census.duplicate_identity += other.duplicate_identity;
    }

    pub fn merge(&mut self, other: Self) {
        self.add_census(&other.census);
        for entry in other.entries.into_values() {
            self.insert(entry);
        }
    }

    pub fn merge_body(&mut self, body: &BodyCodeArtifact) {
        self.add_census(&body.census);
        for entry in body.entries() {
            self.insert(entry.clone());
        }
    }

    /// Merge `bodies` as [`Self::merge_body`] would, one after another; an
    /// identity that repeats or is already held takes the ordinary path in order.
    pub fn merge_bodies<'a>(&mut self, bodies: impl IntoIterator<Item = &'a BodyCodeArtifact>) {
        use bone_core::prelude::*;
        let mut incoming: Vec<&CodeEntry> = Vec::new();
        for body in bodies {
            self.add_census(&body.census);
            incoming.extend(body.entries());
        }
        // Stable, so the entries of one identity stay in merge order.
        incoming.sort_by(|a, b| a.identity.cmp(&b.identity));
        let mut once: Vec<&CodeEntry> = Vec::with_capacity(incoming.len());
        let mut repeated: Vec<&CodeEntry> = Vec::new();
        for group in incoming.chunk_by(|a, b| a.identity == b.identity) {
            match group {
                [entry] if !self.entries.contains_key(&entry.identity) => once.push(entry),
                _ => repeated.extend(group),
            }
        }
        let once: Vec<(CodeIdentity, CodeEntry)> = once
            .par_iter()
            .map(|entry| (entry.identity.clone(), (*entry).clone()))
            .collect();
        self.entries.append(&mut once.into_iter().collect());
        for entry in repeated {
            self.insert(entry.clone());
        }
    }

    /// The logical keys remain unchanged.
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

    /// The root body is a definition, not automatically a slot; nested lambda
    /// bodies are escaping values. Named value mints are marked separately.
    pub fn from_mir_tree(root: &MirFunc) -> Self {
        Self::from_mir_tree_with_volatile_vars(root, &FxHashSet::default())
    }

    /// Volatile comptime values are metadata in body witnesses, but the
    /// surrounding code shape is still hashed, so a body edit cannot hide.
    pub fn from_mir_tree_with_volatile_vars(
        root: &MirFunc,
        volatile_vars: &FxHashSet<Ustr>,
    ) -> Self {
        let mut out = Self::default();
        out.visit_mir(root, false, volatile_vars);
        out.unify_owner_topologies();
        out
    }

    /// One owner can own several distinct topologies (a coroutine's step, defer
    /// and hook regions); the gate compares at the owner boundary, so each
    /// non-default topology of such an owner becomes the hash of the sorted set.
    pub fn unify_owner_topologies(&mut self) {
        let mut per_owner: BTreeMap<CodeOwner, BTreeSet<LambdaTopologyFingerprint>> =
            BTreeMap::new();
        for entry in self.entries.values() {
            let owner = entry.identity.owner();
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
            let owner = entry.identity.owner();
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
            _ if escaping => self.record_missing(),
            _ => {}
        }
        for identity in mf.code_value_identities.values() {
            self.mark_value(identity.clone());
        }
        for nested in &mf.nested_fns {
            self.visit_mir(nested, true, volatile_vars);
        }
    }

    /// Generated registration glue lives outside MIR but creates the same logical
    /// function value; keeping this here prevents a second identity mechanism.
    pub fn mark_value(&mut self, identity: CodeIdentity) {
        if let Some(entry) = self.entries.get_mut(&identity) {
            if entry.escaping {
                return;
            }
            entry.escaping = true;
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

    /// The runtime sees the same logical identity table as an ordinary escaping
    /// value.
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
        use std::collections::btree_map::Entry;
        let old = match self.entries.entry(entry.identity.clone()) {
            Entry::Vacant(slot) => {
                slot.insert(entry);
                return;
            }
            Entry::Occupied(slot) => slot.into_mut(),
        };
        if old.body_fingerprint == 0 && entry.body_fingerprint != 0 {
            let escaping = old.escaping || entry.escaping;
            *old = entry;
            old.escaping = escaping;
        } else if old.body_fingerprint != 0 && entry.body_fingerprint == 0 {
            old.escaping |= entry.escaping;
        } else if old.body_fingerprint != entry.body_fingerprint {
            self.census.duplicate_identity += 1;
        }
    }

    pub(crate) fn record_missing(&mut self) {
        self.census.missing_identity += 1;
    }

    fn named_frames(&self) -> BTreeMap<&CodeIdentity, &FrameSchema> {
        self.entries
            .iter()
            .filter(|(identity, _)| matches!(identity, CodeIdentity::Named(_)))
            .filter_map(|(identity, entry)| Some((identity, entry.compatibility.frame.as_ref()?)))
            .collect()
    }

    /// A topology mismatch is an owner-level incompatibility, even if all
    /// surviving individual keys look compatible.
    pub fn compatible_overlay(&self, next: &Self) -> bool {
        for (identity, old) in &self.entries {
            if !old.escaping {
                continue;
            }
            let Some(new) = next.entries.get(identity) else {
                return false;
            };
            if !new.escaping {
                return false;
            }
            if !old.compatibility.compatible_with(&new.compatibility) {
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
        // A live coroutine resumes into the replacement body, so every named
        // frame-bound callable must keep its frame layout, and none may appear or vanish.
        if self.named_frames() != next.named_frames() {
            return false;
        }
        let mut owners: BTreeMap<CodeOwner, LambdaTopologyFingerprint> = BTreeMap::new();
        for entry in self.entries.values().chain(next.entries.values()) {
            let owner = entry.identity.owner();
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

    /// The identity keys remain those of `self`; only body payloads are replaced.
    pub fn with_replacements<'a>(
        &self,
        replacements: impl IntoIterator<Item = &'a CodeEntry>,
    ) -> Self {
        let mut next = self.clone();
        for entry in replacements {
            let identity = &entry.identity;
            if entry.body_fingerprint != 0 || !next.entries.contains_key(identity) {
                let mut entry = entry.clone();
                if next.entries.get(identity).is_some_and(|old| old.escaping) {
                    entry.escaping = true;
                }
                next.entries.insert(identity.clone(), entry);
            } else if entry.escaping {
                // A newly stored function value can refer to a body that already exists:
                // keep its payload, but publish its slot.
                next.entries
                    .get_mut(identity)
                    .expect("existing body")
                    .escaping = true;
            }
        }
        next.census = CodeIdentityCensus::default();
        next
    }
}

fn body_fingerprint(mf: &MirFunc, volatile_vars: &FxHashSet<Ustr>) -> u64 {
    // A duplicate-body witness only, never used to find a slot. Only the reachable
    // body is hashed, so cloned lowering leftovers cannot split one lambda.
    let volatile_comptime_spans = mf
        .comptime_reads
        .iter()
        .filter_map(|read| match &read.input {
            crate::mir::ComptimeInputId::Var(name) if volatile_vars.contains(name) => {
                Some(read.span)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut hasher = BodyHasher::for_body(mf);
    for block in crate::cfg::reachable_blocks_from(mf, mf.entry) {
        hasher.tag(1);
        for inst_id in &mf.blocks[block].insts {
            let volatile = mf
                .inst_spans
                .get(*inst_id)
                .is_some_and(|span| volatile_comptime_spans.contains(span));
            hasher.inst(&mf.insts[*inst_id], &mf.nested_fn_identities, volatile);
        }
        hasher.tag(2);
        hasher.term(&mf.blocks[block].term);
    }
    hasher.finish()
}

struct BodyHasher {
    hash: WordHash,
    inst_ids: CanonicalIds,
    cell_ids: CanonicalIds,
    block_ids: CanonicalIds,
    rep_ids: FxHashMap<u32, usize>,
}

#[derive(Default)]
struct CanonicalIds {
    dense: Vec<u32>,
    sparse: FxHashMap<u32, usize>,
    next: usize,
}

impl CanonicalIds {
    const UNSEEN: u32 = u32::MAX;

    fn sized(len: usize) -> Self {
        Self {
            dense: vec![Self::UNSEEN; len],
            sparse: FxHashMap::default(),
            next: 0,
        }
    }

    fn index(&mut self, raw: u32) -> usize {
        match self.dense.get_mut(raw as usize) {
            Some(slot) if *slot != Self::UNSEEN => *slot as usize,
            Some(slot) => {
                *slot = self.next as u32;
                self.next += 1;
                *slot as usize
            }
            None => {
                let next = self.next;
                let id = *self.sparse.entry(raw).or_insert(next);
                if id == next {
                    self.next += 1;
                }
                id
            }
        }
    }
}

impl Default for BodyHasher {
    fn default() -> Self {
        Self {
            hash: WordHash::new(),
            inst_ids: CanonicalIds::default(),
            cell_ids: CanonicalIds::default(),
            block_ids: CanonicalIds::default(),
            rep_ids: FxHashMap::default(),
        }
    }
}

impl BodyHasher {
    fn for_body(mf: &MirFunc) -> Self {
        Self {
            hash: WordHash::new(),
            inst_ids: CanonicalIds::sized(mf.insts.len()),
            cell_ids: CanonicalIds::sized(mf.cells.len()),
            block_ids: CanonicalIds::sized(mf.blocks.len()),
            rep_ids: FxHashMap::default(),
        }
    }

    fn tag(&mut self, tag: u8) {
        self.hash.write_u8(tag);
    }

    fn string(&mut self, value: &str) {
        self.hash.write_str(value);
    }

    fn artifact_name(&mut self, value: &str) {
        self.hash
            .write_str(bone_core::modules::artifact_name(value));
    }

    fn boolean(&mut self, value: bool) {
        self.hash.write_u8(value as u8);
    }

    fn inst_idx(&mut self, idx: Idx<Inst>) {
        let id = self.inst_ids.index(idx.into_raw().into_u32());
        self.hash.write_u32(id as u32);
    }

    fn cell_idx(&mut self, idx: Idx<crate::mir::Cell>) {
        let id = self.cell_ids.index(idx.into_raw().into_u32());
        self.hash.write_u32(id as u32);
    }

    fn block_idx(&mut self, idx: Idx<crate::mir::Block>) {
        let id = self.block_ids.index(idx.into_raw().into_u32());
        self.hash.write_u32(id as u32);
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

    fn option_artifact_name(&mut self, value: Option<Ustr>) {
        match value {
            Some(value) => {
                self.boolean(true);
                self.artifact_name(value.as_str());
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
            self.artifact_name(name.as_str());
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
            Lit::Omitted => self.tag(7),
        }
    }

    fn column_elem(&mut self, elem: ColumnElem) {
        self.artifact_name(elem.name.as_str());
        self.boolean(elem.nullable);
        self.rep_id(elem.representation());
        self.boolean(elem.is_nobox());
    }

    fn container(&mut self, kind: ContainerKind) {
        match kind {
            ContainerKind::Vec => self.tag(0),
            ContainerKind::VecNonNull => self.tag(12),
            ContainerKind::NativeBuffer => self.tag(1),
            ContainerKind::NativeBufferValue(name) => {
                self.tag(2);
                self.artifact_name(name.as_str());
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
            ContainerKind::VecInBounds => self.tag(9),
            ContainerKind::VecNonNullInBounds => self.tag(13),
            ContainerKind::NativeBufferInBounds => self.tag(10),
            ContainerKind::Record => self.tag(14),
        }
    }

    fn coro_hook(&mut self, kind: CoroHookKind) {
        self.tag(match kind {
            CoroHookKind::OnResume => 0,
            CoroHookKind::OnKilled => 1,
            CoroHookKind::OnHotReload => 2,
            CoroHookKind::OnOwnedHotReload => 3,
        });
    }

    /// Operands go through the IR's own visitors, so only payload that is not an
    /// instruction or cell index is listed per variant.
    fn inst(&mut self, inst: &Inst, aliases: &FxHashMap<Ustr, CodeIdentity>, volatile: bool) {
        use std::hash::Hash;
        std::mem::discriminant(inst).hash(&mut self.hash);
        let mut uses = 0;
        inst.uses_each(&mut |_| uses += 1);
        self.hash.write_usize(uses);
        inst.uses_each(&mut |value| self.inst_idx(value));
        let mut cells = 0;
        inst.cells_each(&mut |_| cells += 1);
        self.hash.write_usize(cells);
        inst.cells_each(&mut |cell| self.cell_idx(cell));
        match inst {
            Inst::Const(lit) => self.lit(lit, volatile),
            Inst::Global(name) => self.aliased_name(*name, aliases),
            Inst::Format { fmt, .. } => self.string(fmt.as_str()),
            Inst::Call { func, .. } | Inst::CallMulti { func, .. } => self.func_ref(*func),
            Inst::CallBoundMulti { generic_idx, .. } => self.hash.write_u32(*generic_idx),
            Inst::Lambda { entry, .. } => self.block_idx(*entry),
            Inst::MakeVecColumns {
                elem,
                representation,
                nobox,
                columns,
                len,
                ..
            } => {
                self.vec_header(*elem, *representation, *nobox);
                for (name, values) in columns {
                    self.string(name.as_str());
                    self.hash.write_usize(values.len());
                }
                self.hash.write_usize(*len);
            }
            Inst::MakeVecColumnHeader {
                elem,
                representation,
                nobox,
                ..
            } => self.vec_header(*elem, *representation, *nobox),
            Inst::MakeVecStrided {
                elem,
                representation,
                nobox,
                stride,
                len,
                ..
            } => {
                self.vec_header(*elem, *representation, *nobox);
                self.hash.write_usize(*stride);
                self.hash.write_usize(*len);
            }
            Inst::MakeVecStridedHeader {
                elem,
                representation,
                nobox,
                stride,
                ..
            } => {
                self.vec_header(*elem, *representation, *nobox);
                self.hash.write_usize(*stride);
            }
            Inst::MakeStruct { ty, fields } => {
                self.option_artifact_name(*ty);
                self.field_names(fields);
            }
            Inst::MakeValueStruct {
                name,
                representation,
                form,
                fields,
            }
            | Inst::MaterializeValueStruct {
                name,
                representation,
                form,
                fields,
            } => {
                self.artifact_name(name.as_str());
                self.rep_id(*representation);
                self.value_ctor_form(*form);
                self.field_names(fields);
            }
            Inst::MakeEnum { tag, ty, named, .. } => {
                self.string(tag.as_str());
                self.artifact_name(ty.as_str());
                self.field_names(named);
            }
            Inst::MakeEnumDyn { ty, named, .. } => {
                self.artifact_name(ty.as_str());
                self.field_names(named);
            }
            Inst::MakeEnv { fn_ref, fields } => {
                match fn_ref {
                    Some(name) => {
                        self.boolean(true);
                        self.aliased_name(*name, aliases);
                    }
                    None => self.boolean(false),
                }
                self.field_names(fields);
            }
            Inst::FfiNewArray {
                ctype,
                representation,
                ..
            } => {
                self.artifact_name(ctype.as_str());
                self.option_rep_id(*representation);
            }
            Inst::FfiViewCast { cty, .. } => self.string(cty.as_str()),
            Inst::Field {
                field, nonthrowing, ..
            } => {
                self.boolean(*nonthrowing);
                self.string(field.as_str());
            }
            Inst::TupleIndex { index, .. } => self.hash.write_usize(*index),
            Inst::Index { kind, .. }
            | Inst::Len { kind, .. }
            | Inst::StoreIndex { kind, .. }
            | Inst::ArrayPush { kind, .. } => self.container(*kind),
            Inst::StoreField { field, .. } => self.string(field.as_str()),
            Inst::StoreGlobal { name, .. } => self.artifact_name(name.as_str()),
            Inst::CoroHook { kind, .. } => self.coro_hook(*kind),
            _ => {}
        }
    }

    fn vec_header(&mut self, elem: Ustr, representation: crate::lower::RepId, nobox: bool) {
        self.artifact_name(elem.as_str());
        self.rep_id(representation);
        self.boolean(nobox);
    }

    fn field_names(&mut self, fields: &[(Ustr, Idx<Inst>)]) {
        self.hash.write_usize(fields.len());
        for (name, _) in fields {
            self.string(name.as_str());
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
            Term::Branch {
                cond,
                then_bb,
                else_bb,
            } => {
                self.tag(3);
                self.inst_idx(*cond);
                self.block_idx(*then_bb);
                self.block_idx(*else_bb);
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
        Self(crate::FNV_OFFSET)
    }

    fn write(&mut self, bytes: &[u8]) {
        self.0 = crate::fnv1a(self.0, bytes);
    }

    fn write_u64(&mut self, value: u64) {
        self.write(&value.to_le_bytes());
    }

    fn write_usize(&mut self, value: usize) {
        self.write_u64(value as u64);
    }

    fn write_str(&mut self, value: &str) {
        self.write_usize(value.len());
        self.write(value.as_bytes());
    }

    fn finish(self) -> u64 {
        self.0
    }
}

/// A body fingerprint only witnesses equality, so one multiply per word is
/// enough. Each step is a bijection of the state, so a single differing word
/// always changes the result.
struct WordHash(u64);

impl WordHash {
    fn new() -> Self {
        Self(crate::FNV_OFFSET)
    }

    fn write_u64(&mut self, word: u64) {
        self.0 = (self.0.rotate_left(5) ^ word).wrapping_mul(0x517c_c1b7_2722_0a95);
    }

    fn write_u8(&mut self, value: u8) {
        self.write_u64(value.into());
    }

    fn write_u32(&mut self, value: u32) {
        self.write_u64(value.into());
    }

    fn write_usize(&mut self, value: usize) {
        self.write_u64(value as u64);
    }

    fn write_str(&mut self, value: &str) {
        self.write_usize(value.len());
        std::hash::Hasher::write(self, value.as_bytes());
    }

    fn finish(self) -> u64 {
        self.0
    }
}

impl std::hash::Hasher for WordHash {
    fn write(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(8) {
            let mut word = [0; 8];
            word[..chunk.len()].copy_from_slice(chunk);
            self.write_u64(u64::from_le_bytes(word));
        }
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bone_core::prelude::ModuleId;
    use comfy_la_arena::{Idx, RawIdx};

    fn declaration() -> crate::mir::FuncRef {
        crate::mir::FuncRef {
            module: ModuleId::builtin(),
            func: Idx::from_raw(RawIdx::from_u32(0)),
        }
    }

    #[test]
    fn mono_arguments_and_enclosing_instances_are_identity_axes() {
        let standalone_name = format!(
            "{}script_file.rock::counter",
            bone_core::modules::STANDALONE_PACKAGE_PREFIX
        );
        assert_eq!(
            logical_def_key(
                ustr::ustr("script_file.rock"),
                Ustr::from(standalone_name.as_str())
            ),
            "script_file.rock::counter",
        );
        let interner = crate::monoty::MonoInterner::new();
        let int_instance =
            Instance::intern(&interner, declaration(), vec![crate::monoty::MonoTy::Int]);
        let float_instance =
            Instance::intern(&interner, declaration(), vec![crate::monoty::MonoTy::Float]);
        assert_ne!(
            CodeIdentity::Named(int_instance.clone()),
            CodeIdentity::Named(float_instance.clone()),
            "two concrete generic instances must not share a code identity"
        );
        let site = LambdaSiteId {
            file: Ustr::from("same.rock"),
            offset: 12,
        };
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
        assert!(!artifact.is_total());
    }

    #[test]
    fn merging_bodies_at_once_matches_merging_them_in_turn() {
        let named = |func| {
            CodeIdentity::Named(Instance::base(FuncRef {
                module: ModuleId::new(0),
                func: Idx::from_raw(RawIdx::from_u32(func)),
            }))
        };
        let resolved = CodeIdentity::Lambda {
            enclosing: CodeOwner::Instance(Instance::base(declaration())),
            site: LambdaSiteId {
                file: Ustr::from("main.rock"),
                offset: 1,
            },
        };
        let (unique, clash, promoted, held) = (named(9), named(40), named(41), named(42));
        let compatibility = CodeCompatibility {
            call_abi: "unit".to_string(),
            capture: CaptureSchema::empty(),
            frame: None,
            lambda_topology: LambdaTopologyFingerprint::default(),
        };
        let entry = |identity: &CodeIdentity, body_fingerprint, escaping, capture_free| CodeEntry {
            identity: identity.clone(),
            compatibility: compatibility.clone(),
            body_fingerprint,
            body_name: Some(Ustr::from("body")),
            escaping,
            capture_free,
        };
        let body = |entries: Vec<CodeEntry>| {
            let mut artifact = CodeArtifact::default();
            for entry in entries {
                artifact.insert(entry);
            }
            artifact.record_missing();
            BodyCodeArtifact::from(artifact)
        };
        let bodies = [
            body(vec![
                entry(&unique, 9, true, true),
                entry(&resolved, 0, true, false),
                entry(&clash, 1, true, true),
                entry(&promoted, 5, false, true),
                entry(&held, 8, true, true),
            ]),
            body(vec![
                entry(&resolved, 6, false, true),
                entry(&clash, 2, false, true),
                entry(&promoted, 0, true, false),
            ]),
            body(vec![
                entry(&clash, 1, false, true),
                entry(&named(1), 1, true, true),
            ]),
        ];
        let mut held_already = CodeArtifact::default();
        held_already.insert(entry(&held, 0, true, true));
        for start in [CodeArtifact::default(), held_already] {
            let mut in_turn = start.clone();
            for body in &bodies {
                in_turn.merge_body(body);
            }
            let mut at_once = start;
            at_once.merge_bodies(&bodies);
            assert_eq!(at_once, in_turn);
            assert_eq!(at_once.census.missing_identity, bodies.len());
            assert_eq!(at_once.census.duplicate_identity, 1);
        }
    }

    #[test]
    fn body_fingerprint_ignores_arena_offsets_but_keeps_body_shape() {
        let fingerprint = |cell_raw, inst_left, inst_right, extra| {
            let mut hasher = BodyHasher::default();
            let aliases = FxHashMap::default();
            let cell_idx = || Idx::from_raw(RawIdx::from_u32(cell_raw));
            let inst_idx = |raw| Idx::from_raw(RawIdx::from_u32(raw));
            hasher.inst(&Inst::Load(cell_idx()), &aliases, false);
            hasher.inst(
                &Inst::IntAdd(inst_idx(inst_left), inst_idx(inst_right)),
                &aliases,
                false,
            );
            if extra {
                hasher.inst(&Inst::Const(Lit::Int(1)), &aliases, false);
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

        let string_fingerprint = |value: &str| {
            let mut hasher = BodyHasher::default();
            hasher.inst(
                &Inst::Const(Lit::Str(Ustr::from(value))),
                &FxHashMap::default(),
                false,
            );
            hasher.finish()
        };
        let standalone = format!(
            "{}fixture.rock::Root",
            bone_core::modules::STANDALONE_PACKAGE_PREFIX
        );
        assert_ne!(
            string_fingerprint(&standalone),
            string_fingerprint("Root"),
            "a user string that looks like a standalone key must stay literal data"
        );
        let instruction_fingerprint = |inst: Inst| {
            let mut hasher = BodyHasher::default();
            hasher.inst(&inst, &FxHashMap::default(), false);
            hasher.finish()
        };
        let value = Idx::from_raw(RawIdx::from_u32(3));
        assert_ne!(
            instruction_fingerprint(Inst::FloatToF32(value)),
            instruction_fingerprint(Inst::VecIndexSlot(value)),
        );
    }
}
