//! Storage identity for mutable globals and `static` locals.
//!
//! Every backend keeps a program's mutable globals and statics in one table
//! that survives an incremental overlay. Globals occupy dense slots ordered by
//! their stable keys; statics are keyed by their logical identity, so a body
//! edit that adds or removes a static never moves another value.

use std::collections::BTreeMap;

use bone_semantic::hir::FnBackend;
use bone_semantic::inference::Workspace;
use bone_semantic::prelude::ModuleId;
use ustr::Ustr;

use crate::code_identity::logical_def_key;
use crate::lower::{GlobalPlan, ScalarCtx, ValueStorage};
use crate::mangle::symbol_of_key;
use crate::mir::StaticCell;

pub fn global_identity(file: Ustr, name: Ustr) -> String {
    format!("global:{}", logical_def_key(file, name))
}

/// The physical leaf names a global occupies, in the backend's own identifier
/// namespace: a v1 DefKey is not an identifier, so the leaves are minted from
/// the global's emitted SYMBOL. Lowering mints its `nobox` leaves the same way,
/// so a MIR global name and a storage leaf name are the same string.
pub fn physical_leaf_names(plan: &GlobalPlan, name: Ustr) -> Vec<Ustr> {
    let symbol = symbol_of_key(name);
    plan.leaf_names(symbol).unwrap_or_else(|| vec![symbol])
}

pub fn static_identity(owner: &str, name: Ustr, ordinal: u32) -> String {
    format!("static:{owner}::{name}#{ordinal}")
}

pub fn storage_key(identity: &str, leaf: &str) -> String {
    format!("{identity}::{leaf}")
}

/// The stable key of every mutable-global leaf, by physical name. Backends
/// assign slots in key order, so the order never depends on declaration order
/// or arena indices.
pub fn global_storage_keys(
    ws: &Workspace,
    scx: &ScalarCtx,
) -> Result<BTreeMap<Ustr, String>, String> {
    let mut keys = BTreeMap::new();
    for module in ws.modules.iter().filter(|m| m.id != ModuleId::builtin()) {
        for (_, global) in module.globals.iter().filter(|(_, g)| !g.is_const) {
            let identity = global_identity(module.file, global.name);
            let plan = GlobalPlan::for_declared_type(ws, scx, global.ty)
                .map_err(|_| format!("global `{identity}` has no storage plan"))?;
            // Keyed on the emitted symbol, which is what a lookup from codegen
            // carries.
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
    Ok(keys)
}

/// One mutable global in the published global directory: its identity, its
/// display name, its physical leaves and the storage slot of each leaf.
pub struct GlobalDirectoryEntry {
    pub identity: String,
    pub name: String,
    pub physical: Vec<Ustr>,
    pub slots: Vec<String>,
}

/// Every mutable global a program publishes, sorted by identity. `slot_of`
/// names the backend's storage slot for a physical leaf.
pub fn global_directory(
    ws: &Workspace,
    scx: &ScalarCtx,
    mut slot_of: impl FnMut(Ustr) -> Option<String>,
) -> Result<Vec<GlobalDirectoryEntry>, String> {
    let mut entries = Vec::new();
    for module in ws.modules.iter().filter(|m| m.id != ModuleId::builtin()) {
        for (_, global) in module.globals.iter().filter(|(_, g)| !g.is_const) {
            let identity = global_identity(module.file, global.name);
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

/// A fingerprint of the mutable-global layout: every global's identity, type,
/// representation and leaves, and the C layout of every cdata value type a
/// global can hold. A program whose layout differs from the one that created
/// the live storage cannot be loaded into it, because a slot would name a
/// different global or hold a value of a different shape.
pub fn global_layout_id(ws: &Workspace, scx: &ScalarCtx) -> Result<String, String> {
    let mut rows = Vec::new();
    for module in ws.modules.iter().filter(|m| m.id != ModuleId::builtin()) {
        for (_, global) in module.globals.iter().filter(|(_, g)| !g.is_const) {
            let identity = global_identity(module.file, global.name);
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
    let arena = scx.rep_arena();
    for name in ws.struct_by_name.keys() {
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
        rows.push(format!(
            "value:{name};size={size};align={align};leaves=[{leaves}]"
        ));
    }
    rows.sort();
    let mut hash = 0xcbf29ce484222325u64;
    for row in &rows {
        for byte in row.bytes().chain(std::iter::once(0xff)) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    Ok(format!("{hash:016x}"))
}

/// The value and done-flag keys of one lowered static. Static identity is based
/// on its owning logical callable and source site, never on its emitted cell
/// name or initializer hash.
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

/// The storage key of every value cell and done-flag among `cells`, by cell
/// name.
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
