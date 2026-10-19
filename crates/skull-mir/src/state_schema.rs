//! The compiler-owned logical inventory of mutable runtime state.
//!
//! This module is deliberately backend-neutral. Lua and JavaScript pass the
//! same lowered workspace facts here; backend names and arena ids never enter
//! identity or the schema fingerprint.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use bone_semantic::hir::{FnBackend, StateMetadata, StateOwnership, TypeExpr, VariantPayload};
use bone_semantic::inference::Workspace;
use bone_semantic::prelude::ModuleId;
use ustr::Ustr;
use rustc_hash::FxHashSet;

use crate::code_identity::{logical_def_key, CodeArtifact, CodeIdentity, FrameSchema};
use crate::lifecycle::LifecyclePlan;
use crate::mangle::symbol_of_key;
use crate::lower::{GlobalPlan, NB_PRIM_ELEMS, ScalarCtx, ValueStorage};
use crate::mir::{MirFunc, StaticCell};
use crate::monoty::{convert_decl, MonoCtor, MonoSubst, MonoTy, TyEnv};

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
    pub metadata: Option<StateMetadata>,
    /// The declared host graph cannot contain NativeBuffer storage or raw pointers.
    pub storage_free: bool,
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
pub struct StateSchemaSceneMetadata {
    pub scene_registry_identity: Option<String>,
    pub arena_registry_identity: Option<String>,
    pub scene_root_types: Vec<String>,
    pub arena_identity_fields: Vec<String>,
    pub scene_lifecycle_entries: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StateSchema {
    pub entries: Vec<StateEntry>,
    pub schema_id: String,
    census: StateSchemaCensus,
    pub scene: StateSchemaSceneMetadata,
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
    MissingSceneRegistry,
    MissingArenaRegistry,
    MissingSceneRootTypes,
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
            Self::MissingSceneRegistry => {
                write!(f, "StateSchema has Scene lifecycle roots but no SceneRegistry state root")
            }
            Self::MissingArenaRegistry => {
                write!(f, "StateSchema has Scene lifecycle roots but no ArenaRegistry state root")
            }
            Self::MissingSceneRootTypes => {
                write!(f, "StateSchema has Scene lifecycle roots but no logical root types")
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

    /// Attach compiler-owned Scene lifecycle roots to the same state schema.
    /// Root membership is the canonical logical `MonoTy` key from the
    /// LifecyclePlan, never a diagnostic or emitted name.
    pub fn with_lifecycle_plan(
        mut self,
        lifecycle: &LifecyclePlan,
    ) -> Result<Self, StateSchemaError> {
        let scene_entries = lifecycle
            .entries
            .iter()
            .filter(|entry| entry.owner.as_str() == "scene")
            .collect::<Vec<_>>();
        let mut root_types = BTreeSet::new();
        for entry in &scene_entries {
            let Some(root_type) = entry.root_type.as_ref() else {
                return Err(StateSchemaError::MissingSceneRootTypes);
            };
            let Some(root_type) = crate::lifecycle::scene_root_type_key(root_type) else {
                return Err(StateSchemaError::MissingSceneRootTypes);
            };
            root_types.insert(root_type);
        }
        let mut root_types: Vec<String> = root_types.into_iter().collect();
        root_types.sort();
        if !scene_entries.is_empty() && self.scene.scene_registry_identity.is_none() {
            return Err(StateSchemaError::MissingSceneRegistry);
        }
        if !scene_entries.is_empty() && self.scene.arena_registry_identity.is_none() {
            return Err(StateSchemaError::MissingArenaRegistry);
        }
        if !scene_entries.is_empty() && root_types.is_empty() {
            return Err(StateSchemaError::MissingSceneRootTypes);
        }
        self.scene.scene_root_types = root_types;
        self.scene.scene_lifecycle_entries = scene_entries.len();
        self.scene.arena_identity_fields = if scene_entries.is_empty() {
            Vec::new()
        } else {
            ["scene_id", "arena_id", "slot", "generation"]
                .into_iter()
                .map(str::to_string)
                .collect()
        };
        Ok(self)
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
            scene: StateSchemaSceneMetadata::default(),
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
                    metadata: Some(global.state_metadata),
                    storage_free: global.state_metadata.ownership == StateOwnership::HostProjection
                        && declared_storage_free(ws, global.ty),
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
                metadata: Some(StateMetadata::default()),
                storage_free: false,
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
                metadata: Some(StateMetadata::default()),
                storage_free: false,
            };
            entries.push(entry);
        }

        let mut schema = Self::from_entries(entries)?;
        schema.scene = scene_metadata(ws);
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

fn scene_metadata(ws: &Workspace) -> StateSchemaSceneMetadata {
    let mut metadata = StateSchemaSceneMetadata::default();
    for module in ws.modules.iter().filter(|module| module.id != ModuleId::builtin()) {
        for (_, global) in module.globals.iter().filter(|(_, global)| !global.is_const) {
            let role = registry_role(ws, global.ty);
            let identity = global_identity(module.file, global.name);
            match role {
                Some("SceneRegistry") => metadata.scene_registry_identity = Some(identity),
                Some("ArenaRegistry") => metadata.arena_registry_identity = Some(identity),
                _ => {}
            }
        }
    }
    metadata
}

/// These are nominal compiler contracts, not source-name or runtime-type
/// classifiers: the standard library owns the two reference-struct types and
/// StateSchema records their logical identities for every backend.
fn registry_role(ws: &Workspace, ty: bone_semantic::prelude::TyId) -> Option<&'static str> {
    let logical = convert_decl(ws, ty, &MonoSubst::default()).ok()?;
    let MonoTy::App {
        ctor: MonoCtor::Nominal(name),
        params,
    } = logical
    else {
        return None;
    };
    if !params.is_empty() {
        return None;
    }
    if bone_semantic::prelude::is_intrinsic_key(name, "SceneRegistry") {
        Some("SceneRegistry")
    } else if bone_semantic::prelude::is_intrinsic_key(name, "ArenaRegistry") {
        Some("ArenaRegistry")
    } else {
        None
    }
}

fn declared_storage_free(ws: &Workspace, ty: bone_semantic::prelude::TyId) -> bool {
    let Ok(ty) = convert_decl(ws, ty, &MonoSubst::default()) else { return false };
    storage_free_type(ws, &ty, &mut FxHashSet::default(), 0)
}

fn nominal_subst(
    ws: &Workspace,
    generics: &[bone_semantic::prelude::TyId],
    params: &[MonoTy],
) -> Option<MonoSubst> {
    if generics.len() != params.len() { return None }
    let mut subst = MonoSubst::default();
    for (generic, param) in generics.iter().zip(params) {
        let TypeExpr::Generic(var, _) = TyEnv::Decl.decl_node(ws, *generic).ok()? else {
            return None;
        };
        subst.insert(var, param.clone());
    }
    Some(subst)
}

fn storage_free_type(
    ws: &Workspace,
    ty: &MonoTy,
    visiting: &mut FxHashSet<MonoTy>,
    depth: usize,
) -> bool {
    if depth > 64 || !visiting.insert(ty.clone()) { return false }
    let safe = match ty {
        MonoTy::Int | MonoTy::Uint | MonoTy::Float | MonoTy::Bool | MonoTy::String
        | MonoTy::Unit | MonoTy::Never => true,
        MonoTy::Any | MonoTy::Function { .. } => false,
        MonoTy::Nullable(inner) => storage_free_type(ws, inner, visiting, depth + 1),
        MonoTy::Tuple(parts) => parts.iter().all(|part| storage_free_type(ws, part, visiting, depth + 1)),
        MonoTy::App { ctor: MonoCtor::Array, params } =>
            params.len() == 1 && storage_free_type(ws, &params[0], visiting, depth + 1),
        MonoTy::App { ctor: MonoCtor::Map, params } =>
            params.len() == 2 && params.iter().all(|part| storage_free_type(ws, part, visiting, depth + 1)),
        MonoTy::App { ctor: MonoCtor::Ref, .. } => false,
        MonoTy::App { ctor: MonoCtor::Nominal(name), params } => {
            if bone_semantic::prelude::is_descriptor_key(*name)
                || bone_semantic::prelude::is_intrinsic_key(*name, "FlatVec")
            {
                false
            } else if let Some(sid) = ws.struct_by_name.get(name) {
                let def = &ws.module(sid.module).structs[sid.def];
                nominal_subst(ws, &def.generics, params).is_some_and(|subst| {
                    def.fields.iter().all(|field| {
                        convert_decl(ws, field.ty, &subst).ok().is_some_and(|field_ty|
                            storage_free_type(ws, &field_ty, visiting, depth + 1))
                    })
                })
            } else if let Some(eid) = ws.enums_by_name.get(name) {
                let def = &ws.module(eid.module).enums[eid.def];
                nominal_subst(ws, &def.generics, params).is_some_and(|subst| {
                    def.variants.iter().all(|variant| {
                        let fields: Vec<_> = match &variant.payload {
                            VariantPayload::Unit => Vec::new(),
                            VariantPayload::Tuple(fields) => fields.to_vec(),
                            VariantPayload::Record(fields) => fields.iter().map(|field| field.ty).collect(),
                        };
                        fields.iter().all(|field| {
                            convert_decl(ws, *field, &subst).ok().is_some_and(|field_ty|
                                storage_free_type(ws, &field_ty, visiting, depth + 1))
                        })
                    })
                })
            } else if let Some(nid) = ws.newtypes_by_name.get(name) {
                let def = &ws.module(nid.module).newtypes[nid.def];
                nominal_subst(ws, &def.generics, params).is_some_and(|subst| {
                    convert_decl(ws, def.wrapped, &subst).ok().is_some_and(|wrapped|
                        storage_free_type(ws, &wrapped, visiting, depth + 1))
                })
            } else {
                false
            }
        }
    };
    visiting.remove(ty);
    safe
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
            metadata: Some(StateMetadata::default()),
            storage_free: false,
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
            metadata: Some(StateMetadata::default()),
            storage_free: false,
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
            format!("{:?}", entry.metadata),
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
