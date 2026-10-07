//! The compiler-owned semantic identity of one emitted runtime Artifact.
//!
//! This is deliberately built from compiler facts before a backend writes its
//! payload. Generated Lua is not an identity authority: backend text is a
//! realization, while the plans below are the inputs that make that text
//! executable and reload-compatible.

use std::collections::BTreeMap;
use std::fmt;

use bone_semantic::inference::Workspace;
use bone_semantic::prelude::ModuleId;
use rustc_hash::FxHashSet;
use skull_mir::code_identity::{CaptureMode, CodeArtifact, CodeEntry, CodeIdentity};
use std::sync::Arc;
use skull_mir::definition::DefinitionPlan;
use skull_mir::lifecycle::LifecyclePlan;
use skull_mir::mir::{comptime_value_hash, ComptimeInputId, ComptimeRead};
use skull_mir::monoty::MonoInterner;
use skull_mir::state_schema::{StateKind, StateSchema};
use ustr::Ustr;

const FINGERPRINT_VERSION: &str = "semantic-artifact-v2";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FingerprintError(pub String);

impl fmt::Display for FingerprintError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for FingerprintError {}

/// Build the exact identity consumed by both replay recording and replay
/// admission. The input is a canonical semantic inventory; no generated
/// backend text is parsed or hashed.
pub fn compute(
    ws: &Workspace,
    interner: &MonoInterner,
    artifact: &CodeArtifact,
    lifecycle: &LifecyclePlan,
    definitions: &DefinitionPlan,
    state: &StateSchema,
    comptime_reads: &[ComptimeRead],
    volatile_vars: &FxHashSet<Ustr>,
    renderer: &str,
) -> Result<String, FingerprintError> {
    compute_inner(ws, interner, artifact, lifecycle, definitions, state, comptime_reads,
        volatile_vars, renderer, None)
}

pub(crate) fn compute_cached(
    ws: &Workspace,
    interner: &MonoInterner,
    artifact: &CodeArtifact,
    lifecycle: &LifecyclePlan,
    definitions: &DefinitionPlan,
    state: &StateSchema,
    comptime_reads: &[ComptimeRead],
    volatile_vars: &FxHashSet<Ustr>,
    renderer: &str,
    code_rows: &CodeRows,
) -> Result<String, FingerprintError> {
    compute_inner(ws, interner, artifact, lifecycle, definitions, state, comptime_reads,
        volatile_vars, renderer, Some(code_rows))
}

fn compute_inner(
    ws: &Workspace,
    interner: &MonoInterner,
    artifact: &CodeArtifact,
    lifecycle: &LifecyclePlan,
    definitions: &DefinitionPlan,
    state: &StateSchema,
    comptime_reads: &[ComptimeRead],
    volatile_vars: &FxHashSet<Ustr>,
    renderer: &str,
    code_rows: Option<&CodeRows>,
) -> Result<String, FingerprintError> {
    validate_inputs(
        ws,
        artifact,
        lifecycle,
        definitions,
        state,
        comptime_reads,
        volatile_vars,
    )?;

    let mut h = CanonicalHasher::default();
    h.string(FINGERPRINT_VERSION);
    h.string(renderer);
    h.string(&ws.comptime_inputs.channel);
    h.string(&ws.comptime_inputs.modules_mode);

    let mut volatile = volatile_vars
        .iter()
        .map(|name| name.as_str())
        .collect::<Vec<_>>();
    volatile.sort_unstable();
    h.len(volatile.len());
    for name in volatile {
        h.string(name);
    }

    h.census(
        "code",
        &[
            artifact.census.examined as u64,
            artifact.census.assigned as u64,
            artifact.census.missing_identity as u64,
            artifact.census.duplicate_identity as u64,
            artifact.census.duplicate_body as u64,
            artifact.census.unresolved_body as u64,
            artifact.census.direct_call_only as u64,
            artifact.census.named_values as u64,
            artifact.census.lambda_values as u64,
            artifact.census.capturing_lambdas as u64,
            artifact.census.capture_free_lambdas as u64,
        ],
    );
    if let Some(rows) = code_rows {
        if rows.len() != artifact.entries.len() {
            return Err(FingerprintError("cached code fingerprint inventory is incomplete".to_string()));
        }
        h.len(rows.len());
        rows.append_to(&mut h.bytes);
    } else {
        let mut code = artifact.entries.values()
            .map(|entry| (entry.identity.logical_key(ws, interner), entry))
            .collect::<Vec<_>>();
        code.sort_by(|left, right| left.0.cmp(&right.0));
        h.len(code.len());
        for (key, entry) in code {
            write_code_row(&mut h, &key, entry)?;
        }
    }

    h.census(
        "binding",
        &[
            artifact.binding_plan.census.examined as u64,
            artifact.binding_plan.census.assigned as u64,
            artifact.binding_plan.census.duplicate as u64,
            artifact.binding_plan.census.incomplete as u64,
        ],
    );
    h.len(artifact.binding_plan.entries.len());
    for binding in &artifact.binding_plan.entries {
        h.string(&binding.identity);
        h.string(&binding.physical);
        h.string(binding.kind.as_str());
        h.string(binding.state_key.as_deref().unwrap_or(""));
    }

    h.census(
        "lifecycle",
        &[
            lifecycle.census.declarations as u64,
            lifecycle.census.applications as u64,
            lifecycle.census.admitted as u64,
            lifecycle.census.session_owned as u64,
            lifecycle.census.scene_owned as u64,
            lifecycle.census.plain as u64,
            lifecycle.census.gated as u64,
            lifecycle.census.dependency_edges as u64,
        ],
    );
    h.len(lifecycle.entries.len());
    for entry in &lifecycle.entries {
        h.string(entry.phase.as_str());
        h.string(entry.owner.as_str());
        if let Some(root_type) = &entry.root_type {
            h.boolean(true);
            h.string(&root_type.canonical());
        } else {
            h.boolean(false);
        }
        h.string(&entry.identity.logical_key(ws, interner));
        h.string(entry.diagnostic_name.as_str());
        h.i64(entry.stage);
        h.string(entry.replay_role.tag());
        h.u64(entry.declaration_ordinal as u64);
        h.string(entry.qualification.tag());
        h.len(entry.qualification.dependencies().len());
        for dependency in entry.qualification.dependencies() {
            h.string(&dependency.identity.logical_key(ws, interner));
            h.u64(dependency.fingerprint);
        }
        h.u64(entry.body_fingerprint);
    }

    h.census(
        "definition",
        &[
            definitions.census.declarations as u64,
            definitions.census.accessors as u64,
            definitions.census.applications as u64,
            definitions.census.admitted as u64,
        ],
    );
    h.len(definitions.catalogs.len());
    for catalog in &definitions.catalogs {
        h.string(catalog.catalog.as_str());
        let accessor = catalog
            .accessor
            .as_ref()
            .map(|accessor| function_key(ws, accessor.module, accessor.func))
            .transpose()?;
        h.string(accessor.as_deref().unwrap_or(""));
        if let Some(definition_type) = &catalog.definition_type {
            h.boolean(true);
            h.string(&definition_type.canonical());
        } else {
            h.boolean(false);
        }
    }
    h.len(definitions.entries.len());
    for entry in &definitions.entries {
        h.string(entry.catalog.as_str());
        h.string(entry.key.as_str());
        h.string(&entry.identity.logical_key(ws, interner));
        h.string(entry.diagnostic_name.as_str());
        h.u64(entry.declaration_ordinal as u64);
    }

    h.string(&state.schema_id);
    h.census(
        "state",
        &[
            state.census().examined as u64,
            state.census().assigned as u64,
            state.census().incomplete as u64,
            state.census().duplicate as u64,
            state.census().globals as u64,
            state.census().statics as u64,
            state.census().frames as u64,
            state.census().storage as u64,
        ],
    );
    h.len(state.entries.len());
    for entry in &state.entries {
        h.string(state_kind(entry.kind));
        h.string(&entry.identity);
        h.string(&entry.fingerprint);
        h.len(entry.physical.len());
        for physical in &entry.physical {
            h.string(physical);
        }
        h.string(entry.owner.as_deref().unwrap_or(""));
        match entry.initializer_identity {
            Some(identity) => {
                h.boolean(true);
                h.u64(identity);
            }
            None => h.boolean(false),
        }
        if let Some(metadata) = entry.metadata {
            h.boolean(true);
            h.string(metadata.ownership.as_str());
            h.string(metadata.world.as_str());
            h.string(metadata.realization.as_str());
        } else {
            h.boolean(false);
        }
    }
    h.string(state.scene.scene_registry_identity.as_deref().unwrap_or(""));
    h.string(state.scene.arena_registry_identity.as_deref().unwrap_or(""));
    h.len(state.scene.scene_root_types.len());
    for root in &state.scene.scene_root_types {
        h.string(root);
    }
    h.len(state.scene.arena_identity_fields.len());
    for field in &state.scene.arena_identity_fields {
        h.string(field);
    }
    h.u64(state.scene.scene_lifecycle_entries as u64);

    let reads = canonical_reads(ws, comptime_reads, volatile_vars)?;
    h.len(reads.len());
    for (input, volatile, value_hash) in reads {
        h.string(&input);
        h.boolean(volatile);
        h.u64(value_hash);
    }

    Ok(format!("{:016x}", h.finish()))
}

fn write_code_row(h: &mut CanonicalHasher, key: &str, entry: &CodeEntry) -> Result<(), FingerprintError> {
    h.string(key);
    h.string(&entry.compatibility.call_abi);
    h.boolean(entry.compatibility.capture.complete);
    h.len(entry.compatibility.capture.fields.len());
    for field in &entry.compatibility.capture.fields {
        match &field.identity {
            skull_mir::code_identity::CaptureIdentity::Binding { site, projection } => {
                h.string("binding");
                h.string(site.file.as_str());
                h.u64(site.offset);
                h.string(projection.as_str());
            }
            skull_mir::code_identity::CaptureIdentity::Generated { role } => {
                h.string("generated");
                h.string(role.as_str());
            }
        }
        h.string(match field.mode {
            CaptureMode::ByValue => "by-value",
            CaptureMode::Shared => "shared",
            CaptureMode::Frame => "frame",
        });
    }
    match &entry.compatibility.frame {
        Some(frame) => {
            h.boolean(true);
            h.len(frame.slots.len());
            for slot in &frame.slots {
                h.u64(u64::from(*slot));
            }
        }
        None => h.boolean(false),
    }
    h.u64(entry.compatibility.lambda_topology.0);
    h.u64(entry.body_fingerprint);
    h.string(entry.body_name.as_deref().ok_or_else(|| {
        FingerprintError(format!(
            "incomplete CodeArtifact body realization for {:?}",
            entry.identity
        ))
    })?);
    h.boolean(entry.escaping);
    h.boolean(entry.capture_free);
    Ok(())
}

// Both indexes share persistent search paths so transaction snapshots retain
// their rows without copying the canonical inventory.
#[derive(Clone, Default)]
pub(crate) struct CodeRows {
    root: Option<Arc<RowNode<(String, CodeIdentity), Vec<u8>>>>,
    identities: Option<Arc<RowNode<CodeIdentity, (String, CodeIdentity)>>>,
}

struct RowNode<K, V> {
    key: Arc<K>,
    value: Arc<V>,
    priority: u64,
    weight: usize,
    left: Option<Arc<Self>>,
    right: Option<Arc<Self>>,
    len: usize,
    bytes: usize,
}

impl<K: Ord, V> RowNode<K, V> {
    fn new(key: Arc<K>, value: Arc<V>, priority: u64, weight: usize,
        left: Option<Arc<Self>>, right: Option<Arc<Self>>) -> Arc<Self>
    {
        let len = 1 + left.as_ref().map_or(0, |node| node.len)
            + right.as_ref().map_or(0, |node| node.len);
        let bytes = weight + left.as_ref().map_or(0, |node| node.bytes)
            + right.as_ref().map_or(0, |node| node.bytes);
        Arc::new(Self { key, value, priority, weight, left, right, len, bytes })
    }

    fn with_children(&self, left: Option<Arc<Self>>, right: Option<Arc<Self>>) -> Arc<Self> {
        Self::new(self.key.clone(), self.value.clone(), self.priority, self.weight, left, right)
    }

    fn before(&self, other: &Self) -> bool {
        (self.priority, &self.key) < (other.priority, &other.key)
    }

    fn insert(root: &Option<Arc<Self>>, key: Arc<K>, value: Arc<V>, priority: u64, weight: usize) -> Arc<Self> {
        let Some(old) = root else { return Self::new(key, value, priority, weight, None, None) };
        match key.cmp(&old.key) {
            std::cmp::Ordering::Equal => Self::new(key, value, priority, weight, old.left.clone(), old.right.clone()),
            std::cmp::Ordering::Less => {
                let left = Self::insert(&old.left, key, value, priority, weight);
                if left.before(old) {
                    let right = old.with_children(left.right.clone(), old.right.clone());
                    left.with_children(left.left.clone(), Some(right))
                } else {
                    old.with_children(Some(left), old.right.clone())
                }
            }
            std::cmp::Ordering::Greater => {
                let right = Self::insert(&old.right, key, value, priority, weight);
                if right.before(old) {
                    let left = old.with_children(old.left.clone(), right.left.clone());
                    right.with_children(Some(left), right.right.clone())
                } else {
                    old.with_children(old.left.clone(), Some(right))
                }
            }
        }
    }

    fn get<'a>(root: &'a Option<Arc<Self>>, key: &K) -> Option<&'a Arc<V>> {
        let mut node = root.as_ref()?;
        loop {
            match key.cmp(node.key.as_ref()) {
                std::cmp::Ordering::Equal => return Some(&node.value),
                std::cmp::Ordering::Less => node = node.left.as_ref()?,
                std::cmp::Ordering::Greater => node = node.right.as_ref()?,
            }
        }
    }

    fn join(left: &Option<Arc<Self>>, right: &Option<Arc<Self>>) -> Option<Arc<Self>> {
        match (left, right) {
            (None, _) => right.clone(),
            (_, None) => left.clone(),
            (Some(a), Some(b)) if a.before(b) => {
                Some(a.with_children(a.left.clone(), Self::join(&a.right, right)))
            }
            (Some(_), Some(b)) => {
                Some(b.with_children(Self::join(left, &b.left), b.right.clone()))
            }
        }
    }

    fn remove(root: &Option<Arc<Self>>, key: &K) -> Option<Arc<Self>> {
        let node = root.as_ref()?;
        match key.cmp(node.key.as_ref()) {
            std::cmp::Ordering::Equal => Self::join(&node.left, &node.right),
            std::cmp::Ordering::Less => Some(node.with_children(Self::remove(&node.left, key), node.right.clone())),
            std::cmp::Ordering::Greater => Some(node.with_children(node.left.clone(), Self::remove(&node.right, key))),
        }
    }

    fn visit(&self, visit: &mut impl FnMut(&V)) {
        if let Some(left) = &self.left { left.visit(visit); }
        visit(&self.value);
        if let Some(right) = &self.right { right.visit(visit); }
    }
}

impl CodeRows {
    pub(crate) fn collect(ws: &Workspace, interner: &MonoInterner, artifact: &CodeArtifact)
        -> Result<Self, FingerprintError>
    {
        let mut rows = Self::default();
        rows.replace(ws, interner, artifact, artifact.entries.keys())?;
        Ok(rows)
    }

    pub(crate) fn replace<'a>(
        &mut self,
        ws: &Workspace,
        interner: &MonoInterner,
        artifact: &CodeArtifact,
        identities: impl IntoIterator<Item = &'a CodeIdentity>,
    ) -> Result<usize, FingerprintError> {
        use std::hash::{Hash, Hasher};
        let mut derived = 0;
        for identity in identities {
            let entry = artifact.entries.get(identity).ok_or_else(|| {
                FingerprintError(format!("missing edited code fingerprint row for {identity:?}"))
            })?;
            let key = Arc::new((entry.identity.logical_key(ws, interner), identity.clone()));
            let mut encoded = CanonicalHasher::default();
            write_code_row(&mut encoded, &key.0, entry)?;
            if let Some(old) = RowNode::get(&self.identities, identity) {
                if old != &key {
                    self.root = RowNode::remove(&self.root, old);
                }
            }
            let priority = xxhash64(key.0.as_bytes());
            let weight = encoded.bytes.len();
            self.root = Some(RowNode::insert(&self.root, key.clone(), Arc::new(encoded.bytes), priority, weight));
            let mut hasher = rustc_hash::FxHasher::default();
            identity.hash(&mut hasher);
            self.identities = Some(RowNode::insert(&self.identities, Arc::new(identity.clone()), key, hasher.finish(), 0));
            derived += 1;
        }
        Ok(derived)
    }

    fn len(&self) -> usize {
        self.root.as_ref().map_or(0, |node| node.len)
    }

    fn append_to(&self, bytes: &mut Vec<u8>) {
        if let Some(root) = &self.root {
            bytes.reserve(root.bytes);
            root.visit(&mut |row| bytes.extend_from_slice(row));
        }
    }
}

fn validate_inputs(
    ws: &Workspace,
    artifact: &CodeArtifact,
    lifecycle: &LifecyclePlan,
    definitions: &DefinitionPlan,
    state: &StateSchema,
    reads: &[ComptimeRead],
    volatile_vars: &FxHashSet<Ustr>,
) -> Result<(), FingerprintError> {
    if !artifact.census.is_total() {
        return Err(FingerprintError(format!(
            "incomplete CodeArtifact census: {:?}",
            artifact.census
        )));
    }
    for entry in artifact.entries.values() {
        if entry.body_name.is_none() {
            return Err(FingerprintError(format!(
                "incomplete CodeArtifact body realization for {:?}",
                entry.identity
            )));
        }
    }
    if !artifact.binding_plan.entries.is_empty() && !artifact.binding_plan.census.is_total() {
        return Err(FingerprintError(format!(
            "incomplete Artifact binding census: {:?}",
            artifact.binding_plan.census
        )));
    }
    if lifecycle.census.applications != lifecycle.census.admitted
        || lifecycle.entries.len() != lifecycle.census.admitted
    {
        return Err(FingerprintError(format!(
            "incomplete LifecyclePlan census: {:?}",
            lifecycle.census
        )));
    }
    if definitions.census.applications != definitions.census.admitted
        || definitions.entries.len() != definitions.census.admitted
    {
        return Err(FingerprintError(format!(
            "incomplete DefinitionPlan census: {:?}",
            definitions.census
        )));
    }
    if !state.census().is_total() {
        return Err(FingerprintError(format!(
            "incomplete StateSchema census: {:?}",
            state.census()
        )));
    }
    for name in volatile_vars {
        if name.is_empty() || !ws.comptime_inputs.vars.contains_key(name) {
            return Err(FingerprintError(format!(
                "volatile metadata declaration `{name}` is missing from resolved build inputs"
            )));
        }
    }
    for read in reads {
        if let ComptimeInputId::Var(name) = &read.input {
            if !ws.comptime_inputs.vars.contains_key(name) {
                return Err(FingerprintError(format!(
                    "comptime read `{name}` is missing from resolved build inputs"
                )));
            }
        }
    }
    Ok(())
}

fn canonical_reads(
    ws: &Workspace,
    reads: &[ComptimeRead],
    volatile_vars: &FxHashSet<Ustr>,
) -> Result<Vec<(String, bool, u64)>, FingerprintError> {
    let mut unique = BTreeMap::<String, (bool, u64)>::new();
    for read in reads {
        let (key, volatile, current_hash) = match &read.input {
            ComptimeInputId::Var(name) => {
                let value = ws.comptime_inputs.vars.get(name).ok_or_else(|| {
                    FingerprintError(format!(
                        "comptime read `{name}` is missing from resolved build inputs"
                    ))
                })?;
                let volatile = volatile_vars.contains(name);
                let current_hash = comptime_value_hash(&value.value);
                if !volatile && current_hash != read.value_hash {
                    return Err(FingerprintError(format!(
                        "stale nonvolatile comptime read `{name}` cannot identify the Artifact"
                    )));
                }
                (
                    format!("var:{name}"),
                    volatile,
                    if volatile { 0 } else { current_hash },
                )
            }
            ComptimeInputId::File(path) => {
                if path.is_empty() {
                    return Err(FingerprintError(
                        "malformed empty embedded comptime path".to_string(),
                    ));
                }
                let text = std::fs::read_to_string(path.as_str()).map_err(|error| {
                    FingerprintError(format!(
                        "embedded comptime input `{path}` is unreadable: {error}"
                    ))
                })?;
                let current_hash = comptime_value_hash(&text);
                if current_hash != read.value_hash {
                    return Err(FingerprintError(format!(
                        "stale embedded comptime input `{path}` cannot identify the Artifact"
                    )));
                }
                (format!("file:{path}"), false, current_hash)
            }
        };
        if let Some((old_volatile, old_hash)) = unique.insert(key.clone(), (volatile, current_hash))
        {
            if old_volatile != volatile || old_hash != current_hash {
                return Err(FingerprintError(format!(
                    "conflicting comptime reads for `{key}`"
                )));
            }
        }
    }
    Ok(unique
        .into_iter()
        .map(|(key, (volatile, hash))| (key, volatile, hash))
        .collect())
}

fn function_key(
    ws: &Workspace,
    module: ModuleId,
    function: bone_semantic::prelude::Idx<bone_semantic::hir::Function>,
) -> Result<String, FingerprintError> {
    let module = ws.modules.get(module.index()).ok_or_else(|| {
        FingerprintError(format!(
            "definition accessor references missing module {module:?}"
        ))
    })?;
    let function = module
        .functions
        .iter()
        .find_map(|(id, function_def)| (id == function).then_some(function_def))
        .ok_or_else(|| {
            FingerprintError("definition accessor references missing function".to_string())
        })?;
    Ok(format!("{}::{}", module.file, function.name))
}

fn state_kind(kind: StateKind) -> &'static str {
    match kind {
        StateKind::Global => "global",
        StateKind::Static => "static",
        StateKind::CoroutineFrame => "coroutine-frame",
        StateKind::Storage => "storage",
    }
}

#[derive(Default)]
struct CanonicalHasher {
    bytes: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use bone_semantic::inference::{ComptimeVarValue, Workspace};

    fn read_fingerprint(value: &str) -> Vec<(String, bool, u64)> {
        let mut ws = Workspace::new();
        ws.comptime_inputs.vars.insert(
            Ustr::from("TOKEN"),
            ComptimeVarValue {
                value: value.to_string(),
                secret: false,
            },
        );
        let read = ComptimeRead {
            input: ComptimeInputId::Var(Ustr::from("TOKEN")),
            value_hash: comptime_value_hash(value),
            span: bone_semantic::prelude::Span::default(),
        };
        canonical_reads(&ws, &[read], &FxHashSet::default()).expect("valid comptime read")
    }

    #[test]
    fn canonical_comptime_value_is_an_identity_input() {
        assert_ne!(read_fingerprint("A"), read_fingerprint("B"));
    }

    #[test]
    fn cached_code_rows_preserve_canonical_order_and_snapshots() {
        use skull_mir::code_identity::{CodeCompatibility, CodeOwner, CodeOwnerKind, CaptureSchema};
        let ws = Workspace::new();
        let interner = MonoInterner::new();
        let mut artifact = CodeArtifact::default();
        for index in 0..256 {
            let identity = CodeIdentity::Artifact(CodeOwner::Artifact {
                kind: CodeOwnerKind::Global, module: Ustr::from("<rows>"),
                name: Ustr::from(&format!("row{index}")),
            });
            artifact.add_generated_value(identity, CodeCompatibility {
                call_abi: "() -> unit".to_string(),
                capture: CaptureSchema { complete: true, ..Default::default() },
                frame: None, lambda_topology: Default::default(),
            }, "body");
        }
        let canonical = |artifact: &CodeArtifact| {
            let mut entries: Vec<_> = artifact.entries.iter()
                .map(|(identity, entry)| (entry.identity.logical_key(&ws, &interner), identity, entry))
                .collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let mut encoded = CanonicalHasher::default();
            for (key, _, entry) in entries {
                write_code_row(&mut encoded, &key, entry).unwrap();
            }
            encoded.bytes
        };
        let mut rows = CodeRows::collect(&ws, &interner, &artifact).unwrap();
        let before = canonical(&artifact);
        let snapshot = rows.clone();
        let identity = artifact.entries.keys().nth(37).unwrap().clone();
        let entry = artifact.entries.get_mut(&identity).unwrap();
        entry.body_fingerprint += 1;
        entry.body_name = Some(Ustr::from("a_different_length_body"));
        rows.replace(&ws, &interner, &artifact, [&identity]).unwrap();
        let addition = CodeIdentity::Generated {
            enclosing: CodeOwner::Artifact {
                kind: CodeOwnerKind::Global, module: Ustr::from("<rows>"), name: Ustr::from("middle"),
            }, role: Ustr::from("late"),
        };
        artifact.add_generated_value(addition.clone(), artifact.entries[&identity].compatibility.clone(), "new");
        rows.replace(&ws, &interner, &artifact, [&addition]).unwrap();
        let mut cached = Vec::new();
        rows.append_to(&mut cached);
        assert_eq!(cached, canonical(&artifact));
        assert_eq!(rows.len(), artifact.entries.len());
        let mut old = Vec::new();
        snapshot.append_to(&mut old);
        assert_eq!(old, before);
        assert_ne!(cached, old);
    }

    #[test]
    fn xxhash64_matches_reference_vectors() {
        assert_eq!(xxhash64(b""), 0xef46_db37_51d8_e999);
        assert_eq!(xxhash64(b"a"), 0xd24e_c4f1_a98c_6e5b);
        assert_eq!(xxhash64(b"hello"), 0x26c7_827d_889f_6da3);
        assert_eq!(
            xxhash64(b"01234567890123456789012345678901"),
            0xe5cc_9f41_1ea1_10ba
        );
        assert_eq!(
            xxhash64(b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ"),
            0x7639_d419_de61_4eed
        );
    }
}

impl CanonicalHasher {
    fn string(&mut self, value: &str) {
        self.len(value.len());
        self.bytes.extend_from_slice(value.as_bytes());
    }

    fn len(&mut self, value: usize) {
        self.u64(value as u64);
    }

    fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn i64(&mut self, value: i64) {
        self.u64(value as u64);
    }

    fn boolean(&mut self, value: bool) {
        self.byte(value as u8);
    }

    fn census(&mut self, name: &str, values: &[u64]) {
        self.string(name);
        self.len(values.len());
        for value in values {
            self.u64(*value);
        }
    }

    fn byte(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn finish(self) -> u64 {
        xxhash64(&self.bytes)
    }
}

fn xxhash64(bytes: &[u8]) -> u64 {
    const P1: u64 = 0x9e37_79b1_85eb_ca87;
    const P2: u64 = 0xc2b2_ae3d_27d4_eb4f;
    const P3: u64 = 0x1656_67b1_9e37_79f9;
    const P4: u64 = 0x85eb_ca77_c2b2_ae63;
    const P5: u64 = 0x27d4_eb2f_1656_67c5;

    let read_u64 = |chunk: &[u8]| u64::from_le_bytes(chunk.try_into().expect("eight bytes"));
    let round = |acc: u64, input: u64| {
        acc.wrapping_add(input.wrapping_mul(P2))
            .rotate_left(31)
            .wrapping_mul(P1)
    };
    let merge_round = |acc: u64, value: u64| {
        (acc ^ round(0, value)).wrapping_mul(P1).wrapping_add(P4)
    };

    let mut offset = 0;
    let mut hash = if bytes.len() >= 32 {
        let mut v1 = P1.wrapping_add(P2);
        let mut v2 = P2;
        let mut v3 = 0;
        let mut v4 = 0_u64.wrapping_sub(P1);
        while offset + 32 <= bytes.len() {
            v1 = round(v1, read_u64(&bytes[offset..offset + 8]));
            v2 = round(v2, read_u64(&bytes[offset + 8..offset + 16]));
            v3 = round(v3, read_u64(&bytes[offset + 16..offset + 24]));
            v4 = round(v4, read_u64(&bytes[offset + 24..offset + 32]));
            offset += 32;
        }
        let merged = v1
            .rotate_left(1)
            .wrapping_add(v2.rotate_left(7))
            .wrapping_add(v3.rotate_left(12))
            .wrapping_add(v4.rotate_left(18));
        merge_round(
            merge_round(merge_round(merge_round(merged, v1), v2), v3),
            v4,
        )
    } else {
        P5
    };

    hash = hash.wrapping_add(bytes.len() as u64);
    while offset + 8 <= bytes.len() {
        hash ^= round(0, read_u64(&bytes[offset..offset + 8]));
        hash = hash.rotate_left(27).wrapping_mul(P1).wrapping_add(P4);
        offset += 8;
    }
    if offset + 4 <= bytes.len() {
        let value = u32::from_le_bytes(
            bytes[offset..offset + 4]
                .try_into()
                .expect("four bytes"),
        );
        hash ^= u64::from(value).wrapping_mul(P1);
        hash = hash.rotate_left(23).wrapping_mul(P2).wrapping_add(P3);
        offset += 4;
    }
    while offset < bytes.len() {
        hash ^= u64::from(bytes[offset]).wrapping_mul(P5);
        hash = hash.rotate_left(11).wrapping_mul(P1);
        offset += 1;
    }
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(P2);
    hash ^= hash >> 29;
    hash = hash.wrapping_mul(P3);
    hash ^ (hash >> 32)
}
