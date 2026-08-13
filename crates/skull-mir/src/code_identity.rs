//! Logical identity and compatibility facts for callable Rock artifacts.
//!
//! `Instance` is the identity of a named concrete declaration.  A lambda has
//! no declaration to borrow, so its identity is the enclosing concrete
//! instance plus a source site.  Backend names, arena indices and `RepId`s are
//! deliberately absent from these values: they are realization details, not
//! artifact identity.

use std::collections::BTreeMap;

use bone_semantic::prelude::Span;
use ustr::Ustr;
use rustc_hash::FxHashSet;

use crate::instance::Instance;
use crate::mir::MirFunc;

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

/// Compiler-owned identity of an emitted Rock callable body.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum CodeIdentity {
    /// A top-level declaration at one concrete mono instantiation.
    Named(Instance),
    /// A lambda literal in a concrete declaration body.
    Lambda { enclosing: Instance, site: LambdaSiteId },
    /// A source-named nested function. It is still owned by the enclosing
    /// concrete declaration because nested functions do not enter the mono
    /// declaration graph as independent top-level declarations.
    Nested { enclosing: Instance, site: LambdaSiteId, name: Ustr },
}

impl CodeIdentity {
    pub fn enclosing(&self) -> Option<&Instance> {
        match self {
            Self::Named(i) => Some(i),
            Self::Lambda { enclosing, .. } | Self::Nested { enclosing, .. } => Some(enclosing),
        }
    }

    pub fn is_lambda(&self) -> bool {
        matches!(self, Self::Lambda { .. })
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
                CodeIdentity::Lambda { .. } | CodeIdentity::Nested { .. } => None,
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

    fn mark_value(&mut self, identity: CodeIdentity) {
        if let Some(entry) = self.entries.get_mut(&identity) {
            if entry.escaping {
                return;
            }
            entry.escaping = true;
            self.census.examined += 1;
            self.census.assigned += 1;
            match &entry.identity {
                CodeIdentity::Named(_) | CodeIdentity::Nested { .. } => self.census.named_values += 1,
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
            escaping: true,
            capture_free: true,
        });
    }
    pub fn insert(&mut self, entry: CodeEntry) {
        if self.entries.contains_key(&entry.identity) {
            let mut newly_counted = None;
            let mut resolved = false;
            let mut duplicate = false;
            {
                let old = self.entries.get_mut(&entry.identity).expect("identity still present");
                if old.body_fingerprint == 0 && entry.body_fingerprint != 0 {
                    resolved = old.escaping;
                    let was_escaping = old.escaping;
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
            CodeIdentity::Named(_) | CodeIdentity::Nested { .. } => self.census.named_values += 1,
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
            let Some(new) = next.entries.get(identity) else { continue };
            if old.compatibility.lambda_topology != new.compatibility.lambda_topology
                || !old.compatibility.compatible_with(&new.compatibility)
            {
                return false;
            }
        }
        let mut owners: BTreeMap<&Instance, LambdaTopologyFingerprint> = BTreeMap::new();
        for entry in self.entries.values().chain(next.entries.values()) {
            let Some(owner) = entry.identity.enclosing() else { continue };
            let topology = entry.compatibility.lambda_topology;
            if let Some(previous) = owners.insert(owner, topology) {
                if previous != topology {
                    return false;
                }
            }
        }
        true
    }
}

fn body_fingerprint(mf: &MirFunc) -> u64 {
    // This is a duplicate-body witness only. It intentionally excludes the
    // identity/compatibility fields and may include MIR-local arena spelling;
    // it is never used to find a slot.
    let mut h = Fnv64::new();
    h.write_str(&format!("{:?}{:?}{:?}", mf.blocks, mf.insts, mf.cells));
    h.finish()
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
            CodeIdentity::Lambda { enclosing: int_instance, site },
            CodeIdentity::Lambda { enclosing: float_instance, site },
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
