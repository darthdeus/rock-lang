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
use skull_mir::code_identity::{CaptureMode, CodeArtifact};
use skull_mir::definition::DefinitionPlan;
use skull_mir::lifecycle::LifecyclePlan;
use skull_mir::mir::{comptime_value_hash, ComptimeInputId, ComptimeRead};
use skull_mir::monoty::MonoInterner;
use skull_mir::state_schema::{StateKind, StateSchema};
use ustr::Ustr;

const FINGERPRINT_VERSION: &str = "semantic-artifact-v1";

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
    validate_inputs(ws, artifact, lifecycle, definitions, state, comptime_reads, volatile_vars)?;

    let mut h = CanonicalHasher::default();
    h.string(FINGERPRINT_VERSION);
    h.string(renderer);
    h.string(&ws.comptime_inputs.channel);

    let mut volatile = volatile_vars.iter().map(|name| name.as_str()).collect::<Vec<_>>();
    volatile.sort_unstable();
    h.len(volatile.len());
    for name in volatile {
        h.string(name);
    }

    h.census("code", &[
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
    ]);
    let mut code = artifact
        .entries
        .values()
        .map(|entry| (entry.identity.logical_key(ws, interner), entry))
        .collect::<Vec<_>>();
    code.sort_by(|left, right| left.0.cmp(&right.0));
    h.len(code.len());
    for (key, entry) in code {
        h.string(&key);
        h.string(&entry.compatibility.call_abi);
        h.boolean(entry.compatibility.capture.complete);
        h.len(entry.compatibility.capture.fields.len());
        for field in &entry.compatibility.capture.fields {
            h.string(field.name.as_str());
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
        h.string(entry.body_name.as_deref().unwrap_or(""));
        h.boolean(entry.escaping);
        h.boolean(entry.capture_free);
    }

    h.census("binding", &[
        artifact.binding_plan.census.examined as u64,
        artifact.binding_plan.census.assigned as u64,
        artifact.binding_plan.census.duplicate as u64,
        artifact.binding_plan.census.incomplete as u64,
    ]);
    h.len(artifact.binding_plan.entries.len());
    for binding in &artifact.binding_plan.entries {
        h.string(&binding.identity);
        h.string(&binding.physical);
        h.string(binding.kind.as_str());
        h.string(binding.state_key.as_deref().unwrap_or(""));
    }

    h.census("lifecycle", &[
        lifecycle.census.declarations as u64,
        lifecycle.census.applications as u64,
        lifecycle.census.admitted as u64,
        lifecycle.census.session_owned as u64,
        lifecycle.census.scene_owned as u64,
        lifecycle.census.plain as u64,
        lifecycle.census.gated as u64,
        lifecycle.census.dependency_edges as u64,
    ]);
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

    h.census("definition", &[
        definitions.census.declarations as u64,
        definitions.census.accessors as u64,
        definitions.census.applications as u64,
        definitions.census.admitted as u64,
    ]);
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
    h.census("state", &[
        state.census().examined as u64,
        state.census().assigned as u64,
        state.census().incomplete as u64,
        state.census().duplicate as u64,
        state.census().globals as u64,
        state.census().statics as u64,
        state.census().frames as u64,
        state.census().storage as u64,
    ]);
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
        return Err(FingerprintError(format!("incomplete CodeArtifact census: {:?}", artifact.census)));
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
        return Err(FingerprintError(format!("incomplete LifecyclePlan census: {:?}", lifecycle.census)));
    }
    if definitions.census.applications != definitions.census.admitted
        || definitions.entries.len() != definitions.census.admitted
    {
        return Err(FingerprintError(format!("incomplete DefinitionPlan census: {:?}", definitions.census)));
    }
    if !state.census().is_total() {
        return Err(FingerprintError(format!("incomplete StateSchema census: {:?}", state.census())));
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
                    FingerprintError(format!("comptime read `{name}` is missing from resolved build inputs"))
                })?;
                let volatile = volatile_vars.contains(name);
                let current_hash = comptime_value_hash(&value.value);
                if !volatile && current_hash != read.value_hash {
                    return Err(FingerprintError(format!(
                        "stale nonvolatile comptime read `{name}` cannot identify the Artifact"
                    )));
                }
                (format!("var:{name}"), volatile, if volatile { 0 } else { current_hash })
            }
            ComptimeInputId::File(path) => {
                if path.is_empty() {
                    return Err(FingerprintError("malformed empty embedded comptime path".to_string()));
                }
                let text = std::fs::read_to_string(path.as_str()).map_err(|error| {
                    FingerprintError(format!("embedded comptime input `{path}` is unreadable: {error}"))
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
        if let Some((old_volatile, old_hash)) = unique.insert(key.clone(), (volatile, current_hash)) {
            if old_volatile != volatile || old_hash != current_hash {
                return Err(FingerprintError(format!("conflicting comptime reads for `{key}`")));
            }
        }
    }
    Ok(unique.into_iter().map(|(key, (volatile, hash))| (key, volatile, hash)).collect())
}

fn function_key(
    ws: &Workspace,
    module: ModuleId,
    function: bone_semantic::prelude::Idx<bone_semantic::hir::Function>,
) -> Result<String, FingerprintError> {
    let module = ws.modules.get(module.index()).ok_or_else(|| {
        FingerprintError(format!("definition accessor references missing module {module:?}"))
    })?;
    let function = module
        .functions
        .iter()
        .find_map(|(id, function_def)| (id == function).then_some(function_def))
        .ok_or_else(|| FingerprintError("definition accessor references missing function".to_string()))?;
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
    state: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use bone_semantic::inference::{ComptimeVarValue, Workspace};

    fn read_fingerprint(value: &str) -> Vec<(String, bool, u64)> {
        let mut ws = Workspace::new();
        ws.comptime_inputs.vars.insert(
            Ustr::from("TOKEN"),
            ComptimeVarValue { value: value.to_string(), secret: false },
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
}

impl CanonicalHasher {
    fn string(&mut self, value: &str) {
        self.len(value.len());
        for byte in value.as_bytes() {
            self.byte(*byte);
        }
    }

    fn len(&mut self, value: usize) {
        self.u64(value as u64);
    }

    fn u64(&mut self, value: u64) {
        for byte in value.to_le_bytes() {
            self.byte(byte);
        }
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
        if self.state == 0 {
            self.state = 0xcbf29ce484222325;
        }
        self.state ^= u64::from(value);
        self.state = self.state.wrapping_mul(0x100000001b3);
    }

    fn finish(self) -> u64 {
        self.state
    }
}
