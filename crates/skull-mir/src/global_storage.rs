//! Mutable globals and statics live in one table that survives an incremental
//! overlay. Globals take dense slots ordered by stable keys; statics are keyed
//! by logical identity, so adding or removing one never moves another value.
use std::collections::BTreeMap;

use bone_core::hir::FnBackend;
use bone_core::prelude::Nominal;
use bone_core::workspace::Workspace;
use ustr::Ustr;

use crate::code_identity::logical_def_key;
use crate::lower::{GlobalPlan, ScalarCtx, ValueStorage};
use crate::mangle::symbol_of_key;
use crate::mir::StaticCell;

pub fn global_identity(file: Ustr, name: Ustr) -> String {
    format!("global:{}", logical_def_key(file, name))
}

/// A DefKey is not an identifier, so leaves are minted from the emitted symbol,
/// as lowering mints `nobox` leaves: MIR and storage leaf names are equal.
pub(crate) fn physical_leaf_names(plan: &GlobalPlan, name: Ustr) -> Vec<Ustr> {
    let symbol = symbol_of_key(name);
    plan.leaf_names(symbol).unwrap_or_else(|| vec![symbol])
}

pub(crate) fn static_identity(owner: &str, name: Ustr, ordinal: u32) -> String {
    format!("static:{owner}::{name}#{ordinal}")
}

pub(crate) fn storage_key(identity: &str, leaf: &str) -> String {
    format!("{identity}::{leaf}")
}

/// Backends assign slots in key order, never declaration or arena order.
pub fn global_storage_keys(
    ws: &Workspace,
    scx: &ScalarCtx,
) -> Result<BTreeMap<Ustr, String>, String> {
    let mut keys = BTreeMap::new();
    for module in ws.source_modules() {
        for (_, global) in module.globals.iter().filter(|(_, g)| !g.is_const) {
            let identity = global_identity(module.logical_file, global.name);
            let plan = GlobalPlan::for_declared_type(ws, scx, global.ty)
                .map_err(|_| format!("global `{identity}` has no storage plan"))?;
            // Keyed on the emitted symbol, which is what a lookup from codegen carries.
            let names = physical_leaf_names(&plan, global.name);
            let multi_leaf = names.len() != 1;
            for (index, name) in names.into_iter().enumerate() {
                let leaf = if multi_leaf {
                    format!("leaf{index}")
                } else {
                    "value".to_string()
                };
                if let Some(old) = keys.insert(name, storage_key(&identity, &leaf)) {
                    return Err(format!(
                        "global `{identity}` leaf `{name}` is already owned by {old}"
                    ));
                }
            }
        }
    }
    if crate::verify::checks_enabled() && matches!(scx.target(), FnBackend::Lua | FnBackend::Js) {
        for check in scx.global_read_checks(ws).values() {
            if let Some(key) = check.flag_key {
                keys.insert(check.probe, key.to_string());
            }
        }
    }
    Ok(keys)
}

pub struct GlobalDirectoryEntry {
    pub logical_name: Ustr,
    pub plan: GlobalPlan,
    pub identity: String,
    pub name: String,
    pub physical: Vec<Ustr>,
    pub slots: Vec<String>,
}

/// Sorted by identity.
pub fn global_directory(
    ws: &Workspace,
    scx: &ScalarCtx,
    mut slot_of: impl FnMut(Ustr) -> Option<String>,
) -> Result<Vec<GlobalDirectoryEntry>, String> {
    let mut entries = Vec::new();
    for module in ws.source_modules() {
        for (_, global) in module.globals.iter().filter(|(_, g)| !g.is_const) {
            let identity = global_identity(module.logical_file, global.name);
            let plan = GlobalPlan::for_declared_type(ws, scx, global.ty)
                .map_err(|_| format!("no storage plan for global `{}`", global.name))?;
            let physical = physical_leaf_names(&plan, global.name);
            let slots = physical
                .iter()
                .map(|leaf| {
                    slot_of(*leaf).ok_or_else(|| {
                        format!(
                            "global `{}` has no compiler-owned storage slot for `{leaf}`",
                            global.name
                        )
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            let name = identity
                .rsplit("::")
                .next()
                .unwrap_or(&identity)
                .to_string();
            entries.push(GlobalDirectoryEntry {
                logical_name: global.name,
                plan,
                identity,
                name,
                physical,
                slots,
            });
        }
    }
    entries.sort_by(|a, b| a.identity.cmp(&b.identity));
    Ok(entries)
}

/// Covers identity, type, representation, leaves and cdata C layouts: storage
/// created under a different layout cannot be loaded into.
pub fn global_layout_id(ws: &Workspace, scx: &ScalarCtx) -> Result<String, String> {
    let mut rows = Vec::new();
    if scx.target() == FnBackend::Lua {
        rows.push("vec:array-length-at-zero".to_string());
    }
    for module in ws.source_modules() {
        for (_, global) in module.globals.iter().filter(|(_, g)| !g.is_const) {
            let identity = global_identity(module.logical_file, global.name);
            let plan = GlobalPlan::for_declared_type(ws, scx, global.ty)
                .map_err(|_| format!("global `{identity}` has no storage plan"))?;
            let leaves = physical_leaf_names(&plan, global.name)
                .iter()
                .map(|leaf| leaf.as_str())
                .collect::<Vec<_>>()
                .join(",");
            rows.push(format!(
                "{identity};type={};representation={};leaves=[{leaves}]",
                ws.render(None, global.ty),
                plan.logical_fingerprint()
            ));
        }
    }
    if crate::verify::checks_enabled() && matches!(scx.target(), FnBackend::Lua | FnBackend::Js) {
        let mut flags = scx
            .global_read_checks(ws)
            .values()
            .filter(|c| c.flag_key.is_some())
            .map(|c| c.probe.to_string())
            .collect::<Vec<_>>();
        flags.sort();
        flags.dedup();
        rows.extend(flags);
    }
    let arena = scx.rep_arena();
    for (name, _) in ws
        .nominals
        .iter()
        .filter(|(_, def)| matches!(def, Nominal::Struct(_)))
    {
        let cdata = arena
            .nominal_value_storage_required(ws, FnBackend::Lua, *name)
            .is_ok_and(|storage| storage == ValueStorage::CData);
        if !cdata {
            continue;
        }
        let (leaves, size, align) = arena
            .nominal_leaf_layout(ws, FnBackend::Lua, *name)
            .map_err(|_| format!("value type `{name}` has no leaf layout"))?;
        let leaves = leaves
            .iter()
            .map(|leaf| format!("{}:{}:{}", leaf.path, leaf.cty, leaf.byte_off))
            .collect::<Vec<_>>()
            .join(",");
        let name = bone_core::modules::display_name(name.as_str());
        rows.push(format!(
            "value:{name};size={size};align={align};leaves=[{leaves}]"
        ));
    }
    rows.sort();
    let hash = rows.iter().fold(crate::FNV_OFFSET, |hash, row| {
        crate::fnv1a(crate::fnv1a(hash, row.as_bytes()), &[0xff])
    });
    Ok(format!("{hash:016x}"))
}

/// Static identity is its owning logical callable and source site, never its
/// emitted cell name or initializer hash.
pub fn static_cell_storage_keys(
    ws: &Workspace,
    interner: &crate::monoty::MonoInterner,
    cell: &StaticCell,
) -> Result<(String, String), String> {
    let owner = cell
        .owner
        .as_ref()
        .ok_or_else(|| format!("static `{}` has no owning callable", cell.cell))?;
    if cell.site.is_none() {
        return Err(format!("static `{}` has no source site", cell.cell));
    }
    let identity = static_identity(
        &owner.logical_key(ws, interner),
        cell.logical_name,
        cell.ordinal,
    );
    Ok((
        storage_key(&identity, "value"),
        storage_key(&identity, "done"),
    ))
}

/// An overlay re-runs a static's initializer only when this key changed.
pub fn static_cell_key(cell: &StaticCell) -> String {
    format!("{}@{:x}", cell.type_str, cell.init_hash)
}

#[derive(Clone)]
pub struct StaticBaseline {
    pub key: String,
    pub value_slot: String,
    pub done_slot: String,
}

impl StaticBaseline {
    pub fn of(
        ws: &Workspace,
        interner: &crate::monoty::MonoInterner,
        cell: &StaticCell,
    ) -> Result<Self, String> {
        let (value_slot, done_slot) = static_cell_storage_keys(ws, interner, cell)?;
        Ok(Self {
            key: static_cell_key(cell),
            value_slot,
            done_slot,
        })
    }
}

pub fn static_storage_slots<'a>(
    ws: &Workspace,
    interner: &crate::monoty::MonoInterner,
    cells: impl IntoIterator<Item = &'a StaticCell>,
) -> Result<BTreeMap<Ustr, String>, String> {
    let mut slots = BTreeMap::new();
    for cell in cells {
        let (value, done) = static_cell_storage_keys(ws, interner, cell)?;
        if slots.insert(cell.cell, value).is_some() || slots.insert(cell.done_flag, done).is_some()
        {
            return Err(format!("duplicate static storage slot for `{}`", cell.cell));
        }
    }
    Ok(slots)
}

#[derive(Clone, Copy)]
pub(crate) struct GlobalReadCheck {
    pub flag_key: Option<Ustr>,
    pub leaf_count: usize,
    pub probe: Ustr,
    pub name: Ustr,
}

pub(crate) fn global_read_checks(
    ws: &Workspace,
    scx: &ScalarCtx,
) -> rustc_hash::FxHashMap<Ustr, GlobalReadCheck> {
    let mut checks = rustc_hash::FxHashMap::default();
    for module in ws.source_modules() {
        for (gid, global) in module.globals.iter() {
            if global.is_const
                || module.bodies[&bone_core::hir::BodyOwner::Global(gid)]
                    .root
                    .is_some()
            {
                continue;
            }
            let Ok(ty) =
                crate::monoty::convert_decl(ws, global.ty, &crate::lower::SpecSubst::default())
            else {
                continue;
            };
            if ty.peel_nullable().1 {
                continue;
            }
            let Ok(plan) = GlobalPlan::for_declared_type(ws, scx, global.ty) else {
                continue;
            };
            let physical = physical_leaf_names(&plan, global.name);
            // Optional fields can leave every flattened value leaf null.
            let flag = plan
                .leaf_names(symbol_of_key(global.name))
                .map(|_| initialization_flag(global.name));
            let leaf_count = physical.len();
            let flag_key = flag.map(|_| {
                crate::names::ustr(&storage_key(
                    &global_identity(module.logical_file, global.name),
                    "initialized",
                ))
            });
            let check = GlobalReadCheck {
                flag_key,
                leaf_count,
                probe: flag.unwrap_or(symbol_of_key(global.name)),
                name: crate::names::ustr(crate::mangle::declared_name(global.name.as_str())),
            };
            if let Some(flag) = flag {
                checks.insert(flag, check);
                checks.insert(symbol_of_key(global.name), check);
            }
            for leaf in physical {
                checks.insert(
                    leaf,
                    GlobalReadCheck {
                        probe: flag.unwrap_or(leaf),
                        ..check
                    },
                );
            }
        }
    }
    checks
}

fn initialization_flag(name: Ustr) -> Ustr {
    // Rock identifiers cannot contain `$`.
    crate::names::ustr(&format!("$global_initialized_{}", symbol_of_key(name)))
}
