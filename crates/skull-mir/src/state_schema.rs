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

use crate::code_identity::{CodeArtifact, CodeIdentity, FrameSchema, LambdaSiteId};
use crate::lower::{GlobalPlan, NB_PRIM_ELEMS, ScalarCtx, ValueStorage};
use crate::mir::StaticCell;

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
    /// metadata must fail closed rather than produce a partial schema.
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

        for module in ws.modules.iter().filter(|m| m.id != ModuleId::builtin()) {
            for (_, global) in module.globals.iter().filter(|(_, g)| !g.is_const) {
                let plan = GlobalPlan::for_declared_type(ws, scx, global.ty).map_err(|_| {
                    StateSchemaError::Incomplete {
                        kind: StateKind::Global,
                        identity: global_identity(module.file, global.name),
                    }
                })?;
                let names = plan
                    .leaf_names(global.name)
                    .unwrap_or_else(|| vec![global.name]);
                let logical = global_identity(module.file, global.name);
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
                    owner: Some(module.file.to_string()),
                    initializer_identity: None,
                };
                entries.push(entry);
            }
        }

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
            let site = cell.site.ok_or_else(|| StateSchemaError::Incomplete {
                kind: StateKind::Static,
                identity: cell.cell.to_string(),
            })?;
            let owner_key = owner.logical_key(ws, scx.interner());
            let logical = static_identity(&owner_key, site);
            let entry = StateEntry {
                kind: StateKind::Static,
                identity: logical,
                fingerprint: format!("value-cell;done-flag;type={}", cell.type_str),
                physical: vec![cell.cell.to_string(), cell.done_flag.to_string()],
                owner: Some(owner_key),
                initializer_identity: Some(cell.init_hash),
            };
            entries.push(entry);
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
            let entry = StateEntry {
                kind: StateKind::CoroutineFrame,
                identity: logical.clone(),
                fingerprint: frame_fingerprint(frame),
                physical: vec![format!("frame:{logical}")],
                owner: Some(logical),
                initializer_identity: None,
            };
            entries.push(entry);
        }

        for (ctype, size) in NB_PRIM_ELEMS {
            let entry = StateEntry {
                kind: StateKind::Storage,
                identity: format!("primitive:{ctype}"),
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
                arena.nominal_value_storage(ws, FnBackend::Lua, *name) == Some(ValueStorage::CData)
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
            let entry = StateEntry {
                kind: StateKind::Storage,
                identity: format!("value:{name}"),
                fingerprint: format!("ctype={name};size={size};align={align};leaves=[{layout}]"),
                physical: vec![format!("storage:{name}")],
                owner: Some(name.to_string()),
                initializer_identity: None,
            };
            entries.push(entry);
        }

        Self::from_entries(entries)
    }
}

fn global_identity(file: Ustr, name: Ustr) -> String {
    format!("global:{}::{}", file, name)
}

fn static_identity(owner: &str, site: LambdaSiteId) -> String {
    format!("static:{owner}@{}:{}", site.file, site.offset)
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
