//! The compiler-owned semantic identity of one emitted runtime Artifact.

use std::collections::BTreeMap;
use std::fmt;

use bone_semantic::inference::Workspace;
use bone_semantic::prelude::ModuleId;
use rustc_hash::FxHashSet;
use skull_mir::code_identity::{CaptureMode, CodeArtifact, CodeEntry};
use skull_mir::definition::DefinitionPlan;
use skull_mir::lifecycle::LifecyclePlan;
use skull_mir::mir::{ComptimeInputId, ComptimeRead, comptime_value_hash};
use skull_mir::monoty::MonoInterner;
use ustr::Ustr;

const FINGERPRINT_VERSION: &str = "semantic-artifact-v3";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FingerprintError(pub String);

impl fmt::Display for FingerprintError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for FingerprintError {}

/// The canonical identity published with allocation-site diagnostics.
pub fn compute(
    ws: &Workspace,
    interner: &MonoInterner,
    artifact: &CodeArtifact,
    lifecycle: &LifecyclePlan,
    definitions: &DefinitionPlan,
    global_layout: &str,
    comptime_reads: &[ComptimeRead],
    volatile_vars: &FxHashSet<Ustr>,
    renderer: &str,
) -> Result<String, FingerprintError> {
    validate_inventory(ws, artifact, volatile_vars)?;

    let mut h = CanonicalHasher::default();
    h.string(FINGERPRINT_VERSION);
    h.string(renderer);
    h.string(&ws.comptime_inputs.channel);

    let mut volatile = volatile_vars
        .iter()
        .map(|name| name.as_str())
        .collect::<Vec<_>>();
    volatile.sort_unstable();
    h.len(volatile.len());
    for name in volatile {
        h.string(name);
    }

    let mut code = artifact
        .entries
        .values()
        .map(|entry| (entry.identity.logical_key(ws, interner), entry))
        .collect::<Vec<_>>();
    code.sort_by(|left, right| left.0.cmp(&right.0));
    h.len(code.len());
    for (key, entry) in code {
        write_code_row(&mut h, &key, entry);
    }

    h.string("lifecycle");
    h.len(lifecycle.declarations);
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
        h.u64(entry.declaration_ordinal as u64);
        h.string(entry.qualification.tag());
        h.len(entry.qualification.dependencies().len());
        for dependency in entry.qualification.dependencies() {
            h.string(&dependency.identity.logical_key(ws, interner));
            h.u64(dependency.fingerprint);
        }
        h.u64(entry.body_fingerprint);
    }

    h.len(definitions.catalogs.len());
    for catalog in &definitions.catalogs {
        h.string(catalog.catalog.as_str());
        let accessor = catalog
            .accessor
            .as_ref()
            .map(|accessor| {
                let (file, name) = definition_accessor(ws, accessor.module, accessor.func)?;
                Ok::<_, FingerprintError>(format!("{file}::{name}"))
            })
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

    h.string(global_layout);

    let reads = canonical_reads(ws, comptime_reads, volatile_vars)?;
    h.len(reads.len());
    for (input, volatile, value_hash) in reads {
        h.string(&input);
        h.boolean(volatile);
        h.u64(value_hash);
    }

    Ok(format!("{:016x}", h.finish()))
}

fn write_code_row(h: &mut CanonicalHasher, key: &str, entry: &CodeEntry) {
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
    h.string(
        entry
            .body_name
            .as_deref()
            .expect("validated body realization"),
    );
    h.boolean(entry.escaping);
    h.boolean(entry.capture_free);
}

pub(crate) fn validate(
    ws: &Workspace,
    artifact: &CodeArtifact,
    definitions: &DefinitionPlan,
    reads: &[ComptimeRead],
) -> Result<(), FingerprintError> {
    let volatile_vars = &ws.comptime_inputs.volatile_vars;
    validate_inventory(ws, artifact, volatile_vars)?;
    for catalog in &definitions.catalogs {
        if let Some(accessor) = &catalog.accessor {
            definition_accessor(ws, accessor.module, accessor.func)?;
        }
    }
    canonical_reads(ws, reads, volatile_vars)?;
    Ok(())
}

fn validate_inventory(
    ws: &Workspace,
    artifact: &CodeArtifact,
    volatile_vars: &FxHashSet<Ustr>,
) -> Result<(), FingerprintError> {
    if !artifact.is_total() {
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
    for name in volatile_vars {
        if name.is_empty() || !ws.comptime_inputs.vars.contains_key(name) {
            return Err(FingerprintError(format!(
                "volatile metadata declaration `{name}` is missing from resolved build inputs"
            )));
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
                let text = ws
                    .comptime_inputs
                    .embed_files
                    .reread(path.as_str())
                    .map_err(|error| {
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

fn definition_accessor(
    ws: &Workspace,
    module: ModuleId,
    function: bone_semantic::prelude::Idx<bone_semantic::hir::Function>,
) -> Result<(&str, &str), FingerprintError> {
    let module = ws.modules.get(module.index()).ok_or_else(|| {
        FingerprintError(format!(
            "definition accessor references missing module {module:?}"
        ))
    })?;
    let function = module
        .functions
        .iter()
        .find_map(|(id, item)| (id == function).then_some(item))
        .ok_or_else(|| {
            FingerprintError("definition accessor references missing function".to_string())
        })?;
    Ok((module.file.as_str(), function.name.as_str()))
}

#[derive(Default)]
struct CanonicalHasher {
    bytes: Vec<u8>,
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
    let merge_round =
        |acc: u64, value: u64| (acc ^ round(0, value)).wrapping_mul(P1).wrapping_add(P4);

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
        let value = u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("four bytes"));
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
