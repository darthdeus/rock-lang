//! The compiler-owned logical inventory of mutable runtime state.
//!
//! This module is deliberately backend-neutral. Lua and JavaScript pass the
//! same lowered workspace facts here; backend names and arena ids never enter
//! identity or the schema fingerprint.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use bone_semantic::hir::FnBackend;
use bone_semantic::inference::Workspace;
use bone_semantic::prelude::ModuleId;
use ustr::Ustr;

use crate::code_identity::{logical_def_key, CodeArtifact, CodeIdentity, FrameSchema};
use crate::mangle::symbol_of_key;
use crate::lower::{GlobalPlan, NB_PRIM_ELEMS, ScalarCtx, ValueStorage};
use crate::mir::{MirFunc, StaticCell};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StateKind {
    Global,
    Static,
    CoroutineFrame,
    Storage,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateEntry {
    pub kind: StateKind,
    /// Stable compiler identity. This is never a diagnostic or emitted name.
    pub identity: String,
    /// Logical layout/value-cell fingerprint. Initializer identity is kept
    /// separately so a body-only initializer edit does not change schema shape.
    pub fingerprint: String,
    /// Physical names owned by the backend realization. These are not part of
    /// schema identity, but are published so a Session can own every leaf.
    pub physical: Vec<String>,
    pub owner: Option<String>,
    pub initializer_identity: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StateSchemaCensus {
    pub examined: usize,
    pub assigned: usize,
    pub incomplete: usize,
    pub duplicate: usize,
    pub globals: usize,
    pub statics: usize,
    pub frames: usize,
    pub storage: usize,
}

impl StateSchemaCensus {
    pub fn is_total(&self) -> bool {
        self.examined == self.assigned && self.incomplete == 0 && self.duplicate == 0
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StateSchema {
    pub entries: Vec<StateEntry>,
    pub schema_id: String,
    census: StateSchemaCensus,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StateSchemaError {
    Incomplete { kind: StateKind, identity: String },
    Duplicate { identity: String },
    DuplicateLayout { identity: String },
    MissingPhysical { identity: String },
    UnownedPhysical { identity: String, physical: String },
    MissingEntry { kind: StateKind, identity: String },
    UnexpectedEntry { kind: StateKind, identity: String },
}

impl fmt::Display for StateSchemaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Incomplete { kind, identity } => {
                write!(f, "incomplete StateSchema entry {kind:?} `{identity}`")
            }
            Self::Duplicate { identity } => {
                write!(f, "duplicate StateSchema identity `{identity}`")
            }
            Self::DuplicateLayout { identity } => {
                write!(f, "conflicting StateSchema layouts for `{identity}`")
            }
            Self::MissingPhysical { identity } => {
                write!(
                    f,
                    "StateSchema entry `{identity}` has no physical ownership"
                )
            }
            Self::UnownedPhysical { identity, physical } => write!(
                f,
                "StateSchema entry `{identity}` has an unowned physical leaf `{physical}`"
            ),
            Self::MissingEntry { kind, identity } => {
                write!(f, "StateSchema is missing {kind:?} entry `{identity}`")
            }
            Self::UnexpectedEntry { kind, identity } => {
                write!(f, "StateSchema has unexpected {kind:?} entry `{identity}`")
            }
        }
    }
}

impl std::error::Error for StateSchemaError {}

impl StateSchema {
    pub fn census(&self) -> &StateSchemaCensus {
        &self.census
    }

    pub fn entry(&self, kind: StateKind, identity: &str) -> Option<&StateEntry> {
        self.entries
            .iter()
            .find(|e| e.kind == kind && e.identity == identity)
    }

    /// Validate a materialized entry list. This public constructor is also the
    /// mutation-test seam: omission, duplicate identity, and incomplete
    /// entries must fail closed rather than produce a partial schema.
    pub fn from_entries(entries: Vec<StateEntry>) -> Result<Self, StateSchemaError> {
        let mut census = StateSchemaCensus::default();
        let mut by_identity: BTreeMap<(StateKind, String), StateEntry> = BTreeMap::new();
        let mut physical = BTreeSet::new();

        for entry in entries {
            census.examined += 1;
            if entry.identity.is_empty() || entry.fingerprint.is_empty() {
                return Err(StateSchemaError::Incomplete {
                    kind: entry.kind,
                    identity: entry.identity,
                });
            }
            if entry.physical.is_empty() {
                return Err(StateSchemaError::MissingPhysical {
                    identity: entry.identity,
                });
            }
            let key = (entry.kind, entry.identity.clone());
            if let Some(old) = by_identity.get(&key) {
                return Err(
                    if old.fingerprint == entry.fingerprint && old.physical == entry.physical {
                        StateSchemaError::Duplicate {
                            identity: entry.identity,
                        }
                    } else {
                        StateSchemaError::DuplicateLayout {
                            identity: entry.identity,
                        }
                    },
                );
            }
            for name in &entry.physical {
                if name.is_empty() || !physical.insert(name.clone()) {
                    return Err(StateSchemaError::UnownedPhysical {
                        identity: entry.identity.clone(),
                        physical: name.clone(),
                    });
                }
            }
            by_identity.insert(key, entry);
            census.assigned += 1;
        }

        let mut ordered: Vec<StateEntry> = by_identity.into_values().collect();
        ordered.sort_by(|a, b| (a.kind, &a.identity).cmp(&(b.kind, &b.identity)));
        for entry in &ordered {
            match entry.kind {
                StateKind::Global => census.globals += 1,
                StateKind::Static => census.statics += 1,
                StateKind::CoroutineFrame => census.frames += 1,
                StateKind::Storage => census.storage += 1,
            }
        }
        let schema_id = fingerprint(&ordered);
        Ok(Self {
            entries: ordered,
            schema_id,
            census,
        })
    }

    /// Collect every compiler-known mutable state class from the same lowered
    /// facts used by the backend. The caller supplies emitted statics and code
    /// artifacts so no second backend discovery walk exists.
    pub fn collect(
        ws: &Workspace,
        scx: &ScalarCtx,
        artifact: &CodeArtifact,
        statics: &[StaticCell],
    ) -> Result<Self, StateSchemaError> {
        let mut entries = Vec::new();
        let mut expected: BTreeMap<StateKind, BTreeSet<String>> = BTreeMap::new();

        for module in ws.modules.iter().filter(|m| m.id != ModuleId::builtin()) {
            for (_, global) in module.globals.iter().filter(|(_, g)| !g.is_const) {
                let plan = GlobalPlan::for_declared_type(ws, scx, global.ty).map_err(|_| {
                    StateSchemaError::Incomplete {
                        kind: StateKind::Global,
                    identity: global_identity(module.file, global.name),
                    }
                })?;
                let names = physical_leaf_names(&plan, global.name);
                let logical = global_identity(module.file, global.name);
                expected
                    .entry(StateKind::Global)
                    .or_default()
                    .insert(logical.clone());
                let fingerprint = format!(
                    "type={};representation={}",
                    ws.render(None, global.ty),
                    plan.logical_fingerprint()
                );
                let entry = StateEntry {
                    kind: StateKind::Global,
                    identity: logical.clone(),
                    fingerprint,
                    physical: names.iter().map(|n| n.to_string()).collect(),
                    owner: Some(global_owner(module.file, global.name)),
                    initializer_identity: None,
                };
                entries.push(entry);
            }
        }

        let dynamic_entries = collect_dynamic_entries(ws, scx, artifact, statics)?;
        for entry in dynamic_entries {
            expected
                .entry(entry.kind)
                .or_default()
                .insert(entry.identity.clone());
            entries.push(entry);
        }

        for (ctype, size) in NB_PRIM_ELEMS {
            let identity = format!("primitive:{ctype}");
            expected
                .entry(StateKind::Storage)
                .or_default()
                .insert(identity.clone());
            let entry = StateEntry {
                kind: StateKind::Storage,
                identity,
                fingerprint: format!("ctype={ctype};size={size}"),
                physical: vec![format!("storage:{ctype}")],
                owner: None,
                initializer_identity: None,
            };
            entries.push(entry);
        }

        let arena = scx.rep_arena();
        let mut names: Vec<Ustr> = ws
            .struct_by_name
            .keys()
            .copied()
            .filter(|name| {
                arena
                    .nominal_value_storage_required(ws, FnBackend::Lua, *name)
                    .map_or(false, |storage| storage == ValueStorage::CData)
            })
            .collect();
        names.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        for name in names {
            let (leaves, size, align) = arena
                .nominal_leaf_layout(ws, FnBackend::Lua, name)
                .map_err(|_| StateSchemaError::Incomplete {
                    kind: StateKind::Storage,
                    identity: format!("value:{name}"),
                })?;
            let layout = leaves
                .iter()
                .map(|leaf| format!("{}:{}:{}", leaf.path, leaf.cty, leaf.byte_off))
                .collect::<Vec<_>>()
                .join(",");
            let identity = format!("value:{name}");
            expected
                .entry(StateKind::Storage)
                .or_default()
                .insert(identity.clone());
            let entry = StateEntry {
                kind: StateKind::Storage,
                identity,
                fingerprint: format!("ctype={name};size={size};align={align};leaves=[{layout}]"),
                physical: vec![format!("storage:{name}")],
                owner: Some(name.to_string()),
                initializer_identity: None,
            };
            entries.push(entry);
        }

        let schema = Self::from_entries(entries)?;
        for kind in [
            StateKind::Global,
            StateKind::Static,
            StateKind::CoroutineFrame,
            StateKind::Storage,
        ] {
            let actual: BTreeSet<String> = schema
                .entries
                .iter()
                .filter(|entry| entry.kind == kind)
                .map(|entry| entry.identity.clone())
                .collect();
            let expected = expected.get(&kind).cloned().unwrap_or_default();
            if let Some(identity) = expected.difference(&actual).next() {
                return Err(StateSchemaError::MissingEntry {
                    kind,
                    identity: identity.clone(),
                });
            }
            if let Some(identity) = actual.difference(&expected).next() {
                return Err(StateSchemaError::UnexpectedEntry {
                    kind,
                    identity: identity.clone(),
                });
            }
        }
        Ok(schema)
    }

    /// Validate the state classes that a body-only overlay can change without
    /// walking declarations and nominal storage plans again.
    pub fn dynamic_entries_match(
        &self,
        ws: &Workspace,
        scx: &ScalarCtx,
        artifact: &CodeArtifact,
        statics: &[StaticCell],
    ) -> Result<bool, StateSchemaError> {
        let mut current = collect_dynamic_entries(ws, scx, artifact, statics)?;
        let mut previous: Vec<StateEntry> = self
            .entries
            .iter()
            .filter(|entry| {
                matches!(entry.kind, StateKind::Static | StateKind::CoroutineFrame)
            })
            .cloned()
            .collect();
        current.sort_by(|a, b| (a.kind, &a.identity).cmp(&(b.kind, &b.identity)));
        previous.sort_by(|a, b| (a.kind, &a.identity).cmp(&(b.kind, &b.identity)));
        Ok(current == previous)
    }
}

fn collect_dynamic_entries(
    ws: &Workspace,
    scx: &ScalarCtx,
    artifact: &CodeArtifact,
    statics: &[StaticCell],
) -> Result<Vec<StateEntry>, StateSchemaError> {
    let mut entries = Vec::new();

    // Inline expansion can report the same physical static more than once.
    // Collapse that one realization before validating logical membership;
    // two different physical cells with one logical identity remain an
    // error instead of being silently merged.
    let mut static_by_cell: BTreeMap<Ustr, &StaticCell> = BTreeMap::new();
    for cell in statics {
        if let Some(old) = static_by_cell.insert(cell.cell, cell) {
            if old.done_flag != cell.done_flag
                || old.type_str != cell.type_str
                || old.init_hash != cell.init_hash
                || old.owner != cell.owner
                || old.site != cell.site
                || old.logical_name != cell.logical_name
                || old.ordinal != cell.ordinal
            {
                return Err(StateSchemaError::DuplicateLayout {
                    identity: cell.cell.to_string(),
                });
            }
        }
    }
    for cell in static_by_cell.values() {
        let owner = cell
            .owner
            .as_ref()
            .ok_or_else(|| StateSchemaError::Incomplete {
                kind: StateKind::Static,
                identity: cell.cell.to_string(),
            })?;
        let _site = cell.site.ok_or_else(|| StateSchemaError::Incomplete {
            kind: StateKind::Static,
            identity: cell.cell.to_string(),
        })?;
        let owner_key = owner.logical_key(ws, scx.interner());
        let logical = static_identity(&owner_key, cell.logical_name, cell.ordinal);
        entries.push(StateEntry {
            kind: StateKind::Static,
            identity: logical,
            fingerprint: format!("value-cell;done-flag;type={}", cell.type_str),
            physical: vec![cell.cell.to_string(), cell.done_flag.to_string()],
            owner: Some(owner_key),
            initializer_identity: Some(cell.init_hash),
        });
    }

    let mut frame_ids = BTreeSet::new();
    for code in artifact.entries.values() {
        if !matches!(&code.identity, CodeIdentity::Named(_)) {
            continue;
        }
        let Some(frame) = &code.compatibility.frame else {
            continue;
        };
        let logical = code.identity.logical_key(ws, scx.interner());
        if !frame_ids.insert(logical.clone()) {
            continue;
        }
        entries.push(StateEntry {
            kind: StateKind::CoroutineFrame,
            identity: logical.clone(),
            fingerprint: frame_fingerprint(frame),
            physical: vec![format!("frame:{logical}")],
            owner: Some(logical),
            initializer_identity: None,
        });
    }

    Ok(entries)
}

pub fn global_identity(file: Ustr, name: Ustr) -> String {
    format!("global:{}", logical_def_key(file, name))
}

/// The module a global belongs to, as the schema reports it. Entries are
/// compared field by field for reload compatibility, so this follows identity:
/// a v1 global's owner is its logical module (its DefKey's
/// `<package>::<module path>` prefix), not the fragment that happens to declare
/// it; a legacy global's owner is its file.
pub fn global_owner(file: Ustr, name: Ustr) -> String {
    match name.as_str().rfind("::") {
        Some(cut) => name.as_str()[..cut].to_string(),
        None => file.to_string(),
    }
}

/// The physical leaf names a global occupies, in the backend's own identifier
/// namespace: a v1 DefKey is not an identifier, so the leaves are minted from
/// the global's emitted SYMBOL. Lowering mints its `nobox` leaves the same way,
/// so a MIR global name and a schema leaf name are the same string.
pub fn physical_leaf_names(plan: &GlobalPlan, name: Ustr) -> Vec<Ustr> {
    let symbol = symbol_of_key(name);
    plan.leaf_names(symbol).unwrap_or_else(|| vec![symbol])
}

pub fn static_identity(owner: &str, name: Ustr, ordinal: u32) -> String {
    format!("static:{owner}::{name}#{ordinal}")
}

pub fn state_key(identity: &str, leaf: &str) -> String {
    format!("{identity}::{leaf}")
}

/// The Session-owned slots for compiler-generated mutable globals. This is the
/// one shared mapping consumed by both backend printers; a missing entry is an
/// error rather than a bare backend-global fallback.
pub fn global_state_slots(
    ws: &Workspace,
    scx: &ScalarCtx,
) -> Result<BTreeMap<Ustr, String>, StateSchemaError> {
    let mut slots = BTreeMap::new();
    for module in ws.modules.iter().filter(|m| m.id != ModuleId::builtin()) {
        for (_, global) in module.globals.iter().filter(|(_, g)| !g.is_const) {
            let plan = GlobalPlan::for_declared_type(ws, scx, global.ty).map_err(|_| {
                StateSchemaError::Incomplete {
                    kind: StateKind::Global,
                    identity: global_identity(module.file, global.name),
                }
            })?;
            // Keyed on the emitted symbol, which is what both the schema's
            // `physical` names and a lookup from codegen carry.
            let names = physical_leaf_names(&plan, global.name);
            let multi_leaf = names.len() != 1;
            let identity = global_identity(module.file, global.name);
            for (index, name) in names.into_iter().enumerate() {
                let leaf = if !multi_leaf {
                    "value".to_string()
                } else {
                    format!("leaf{index}")
                };
                let key = state_key(&identity, &leaf);
                if let Some(old) = slots.insert(name, key.clone()) {
                    return Err(StateSchemaError::UnownedPhysical {
                        identity,
                        physical: format!("{} (already owned by {old})", name),
                    });
                }
            }
        }
    }
    Ok(slots)
}

/// The Session-owned value and initialized/done slots for one lowered static.
/// Static identity is based on its owning logical callable and source site,
/// never on its emitted cell name or initializer hash.
pub fn static_state_slots(
    ws: &Workspace,
    interner: &crate::monoty::MonoInterner,
    function: &MirFunc,
) -> Result<BTreeMap<Ustr, String>, StateSchemaError> {
    let mut slots = BTreeMap::new();
    for cell in &function.statics {
        let (value, done) = static_cell_state_keys(ws, interner, cell)?;
        let identity = value
            .strip_suffix("::value")
            .expect("static value slot has its role suffix")
            .to_string();
        if slots.insert(cell.cell, value).is_some() || slots.insert(cell.done_flag, done).is_some() {
            return Err(StateSchemaError::DuplicateLayout { identity });
        }
    }
    Ok(slots)
}

pub fn static_cell_state_keys(
    ws: &Workspace,
    interner: &crate::monoty::MonoInterner,
    cell: &StaticCell,
) -> Result<(String, String), StateSchemaError> {
    let owner = cell
        .owner
        .as_ref()
        .ok_or_else(|| StateSchemaError::Incomplete {
            kind: StateKind::Static,
            identity: cell.cell.to_string(),
        })?;
    let _site = cell.site.ok_or_else(|| StateSchemaError::Incomplete {
        kind: StateKind::Static,
        identity: cell.cell.to_string(),
    })?;
    let identity = static_identity(&owner.logical_key(ws, interner), cell.logical_name, cell.ordinal);
    Ok((state_key(&identity, "value"), state_key(&identity, "done")))
}

pub fn entry_state_keys(entry: &StateEntry) -> Vec<String> {
    entry
        .physical
        .iter()
        .enumerate()
        .map(|(index, _)| {
            let leaf = match entry.kind {
                StateKind::Global if entry.physical.len() == 1 => "value".to_string(),
                StateKind::Global => format!("leaf{index}"),
                StateKind::Static if index == 0 => "value".to_string(),
                StateKind::Static if index == 1 => "done".to_string(),
                _ => format!("physical{index}"),
            };
            state_key(&entry.identity, &leaf)
        })
        .collect()
}

fn frame_fingerprint(frame: &FrameSchema) -> String {
    format!(
        "slots=[{}]",
        frame
            .slots
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",")
    )
}

fn fingerprint(entries: &[StateEntry]) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for entry in entries {
        for text in [
            format!("{:?}", entry.kind),
            entry.identity.clone(),
            entry.fingerprint.clone(),
        ] {
            for byte in text.bytes() {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x100000001b3);
            }
            hash ^= 0xff;
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    format!("{hash:016x}")
}
