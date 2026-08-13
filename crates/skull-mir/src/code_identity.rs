//! Logical identity and compatibility facts for callable Rock artifacts.
//!
//! `Instance` is the identity of a named concrete declaration.  A lambda has
//! no declaration to borrow, so its identity is the enclosing concrete
//! instance plus a source site.  Backend names, arena indices and `RepId`s are
//! deliberately absent from these values: they are realization details, not
//! artifact identity.

use std::collections::BTreeMap;

use bone_semantic::prelude::Span;
use rustc_hash::FxHashSet;
use ustr::Ustr;

use crate::instance::Instance;
use crate::mir::{Inst, MirFunc};
use crate::monoty::MonoInterner;

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
            format!("{}::{}<{}>", module.file, function.name, args)
        };
        let owner_key = |owner: &CodeOwner| match owner {
            CodeOwner::Instance(instance) => instance_key(instance),
            CodeOwner::Artifact { kind, module, name } => {
                format!("artifact:{kind:?}:{}::{name}", module)
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

/// One logical environment field. `name` is a source binding name when the
/// compiler can prove it is unique; incomplete schemas fail compatibility
/// closed rather than pretending two unknown fields are interchangeable.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct CaptureField {
    pub name: Ustr,
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
}

impl CodeArtifact {
    pub fn merge(&mut self, other: Self) {
        self.census.direct_call_only += other.census.direct_call_only;
        self.census.missing_identity += other.census.missing_identity;
        self.census.duplicate_identity += other.census.duplicate_identity;
        self.census.duplicate_body += other.census.duplicate_body;
        for entry in other.entries.into_values() {
            self.insert(entry);
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
        let mut out = Self::default();
        out.visit_mir(root, false);
        out
    }

    fn visit_mir(&mut self, mf: &MirFunc, escaping: bool) {
        match (&mf.code_identity, &mf.code_compatibility) {
            (Some(identity), Some(compatibility)) => self.insert(CodeEntry {
                identity: identity.clone(),
                compatibility: compatibility.clone(),
                body_fingerprint: body_fingerprint(mf),
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
            self.visit_mir(nested, true);
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

fn body_fingerprint(mf: &MirFunc) -> u64 {
    // This is a duplicate-body witness only. It intentionally excludes the
    // identity/compatibility fields and is canonicalized across MIR clones;
    // it is never used to find a slot.
    let aliases: BTreeMap<Ustr, Ustr> = mf
        .nested_fn_identities
        .iter()
        .map(|(name, identity)| {
            (*name, Ustr::from(format!("__rock_nested_{identity:?}").as_str()))
        })
        .collect();

    // Generic inlining can leave unrelated cloned lowering artifacts in the
    // same arenas. Hash only the reachable body so those artifacts cannot make
    // two copies of one lambda look like different bodies.
    let mut body = String::new();
    for block in crate::cfg::reachable_blocks_from(mf, mf.entry) {
        body.push_str("block:");
        for inst_id in &mf.blocks[block].insts {
            let mut inst = mf.insts[*inst_id].clone();
            match &mut inst {
                Inst::Global(name) => {
                    if let Some(alias) = aliases.get(name) {
                        *name = *alias;
                    }
                }
                Inst::MakeEnv { fn_ref: Some(name), .. } => {
                    if let Some(alias) = aliases.get(name) {
                        *name = *alias;
                    }
                }
                _ => {}
            }
            body.push_str(&format!("{inst:?};"));
        }
        body.push_str(&format!("term:{:?};", mf.blocks[block].term));
    }
    let debug = canonicalize_arena_indices(&body);
    let mut h = Fnv64::new();
    h.write_str(&debug);
    h.finish()
}

fn canonicalize_arena_indices(debug: &str) -> String {
    let marker = "Idx::<";
    let mut out = String::with_capacity(debug.len());
    let mut maps: BTreeMap<String, BTreeMap<u64, usize>> = BTreeMap::new();
    let mut cursor = 0;
    while let Some(relative) = debug[cursor..].find(marker) {
        let start = cursor + relative;
        out.push_str(&debug[cursor..start]);
        let type_start = start + marker.len();
        let Some(type_end_relative) = debug[type_start..].find(">(") else {
            out.push_str(&debug[start..]);
            return out;
        };
        let type_end = type_start + type_end_relative;
        let number_start = type_end + 2;
        let Some(number_end_relative) = debug[number_start..].find(')') else {
            out.push_str(&debug[start..]);
            return out;
        };
        let number_end = number_start + number_end_relative;
        let Ok(original) = debug[number_start..number_end].parse::<u64>() else {
            out.push_str(&debug[start..number_end + 1]);
            cursor = number_end + 1;
            continue;
        };
        let kind = &debug[type_start..type_end];
        let values = maps.entry(kind.to_string()).or_default();
        let canonical = if let Some(canonical) = values.get(&original) {
            *canonical
        } else {
            let canonical = values.len();
            values.insert(original, canonical);
            canonical
        };
        out.push_str(marker);
        out.push_str(kind);
        out.push_str(">(");
        out.push_str(&canonical.to_string());
        out.push(')');
        cursor = number_end + 1;
    }
    out.push_str(&debug[cursor..]);
    out
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
}
