use bone_core::hir::{FnBackend, StructDef};
use rustc_hash::FxHashMap;
use skull_mir::lower::struct_field_plan;
use ustr::Ustr;

use super::{CodegenCtx, ContainerKind, Emitter, Idx, Inst, Lit, Workspace, write_lua_string};

pub(super) struct RecordLayout {
    pub(super) fields: Vec<Ustr>,
    pub(super) slots: FxHashMap<Ustr, u32>,
}

pub(super) type RecordSlots = FxHashMap<Ustr, RecordLayout>;

pub(super) fn plan(ws: &Workspace) -> RecordSlots {
    ws.source_modules()
        .flat_map(|module| module.structs.values())
        .filter(|s| !s.is_value && !s.is_nobox && !s.lua_named && !s.fields.is_empty())
        .filter_map(|s| {
            let mut fields = Vec::new();
            record_fields(ws, s, "", &mut fields, &mut Vec::new())?;
            let slots = fields
                .iter()
                .enumerate()
                .map(|(i, name)| (*name, (i + 1) as u32))
                .collect();
            Some((s.name, RecordLayout { fields, slots }))
        })
        .collect()
}

fn record_fields(
    ws: &Workspace,
    def: &StructDef,
    prefix: &str,
    out: &mut Vec<Ustr>,
    visiting: &mut Vec<Ustr>,
) -> Option<()> {
    if visiting.contains(&def.name) {
        return None;
    }
    visiting.push(def.name);
    let key = |name: Ustr| {
        if prefix.is_empty() {
            name
        } else {
            Ustr::from(&format!("{prefix}{name}"))
        }
    };
    for field in &def.fields {
        let plan =
            struct_field_plan(ws, FnBackend::Lua, def.name, field.name, !prefix.is_empty()).ok()?;
        if let Some((name, nullable)) = plan.embed() {
            let id = ws.nominals.struct_id(name)?;
            let child = &ws.module(id.module).structs[id.def];
            record_fields(ws, child, &format!("{}.", key(field.name)), out, visiting)?;
            if nullable {
                out.push(key(field.name));
            }
        } else if let Some(fields) = plan.flat_storage_keys(field.name) {
            out.extend(fields.into_iter().map(key));
        } else {
            out.push(key(field.name));
        }
    }
    visiting.pop();
    Some(())
}

impl CodegenCtx {
    pub fn uses_record_slots(&self, name: Ustr) -> bool {
        self.record_slots.contains_key(&name)
    }

    pub fn record_slot_metadata(&self, ws: &Workspace) -> String {
        let mut out = String::new();
        let mut records: Vec<_> = self.record_slots.iter().collect();
        records.sort_unstable_by_key(|&(name, _)| name);
        for (&name, layout) in records {
            let ty = skull_mir::mangle::type_symbol(ws, name);
            out.push_str("do local mt=__rock_box_mts[");
            write_lua_string(&mut out, ty.as_str());
            out.push_str("]; mt.__rock_fields={");
            for field in &layout.fields {
                write_lua_string(&mut out, field.as_str());
                out.push(',');
            }
            out.push_str("}; local slots={} for i,k in ipairs(mt.__rock_fields) do slots[k]=i end mt.__rock_slots=slots end\n");
        }
        out
    }
}

impl Emitter<'_> {
    pub(super) fn record_slot(&self, kind: ContainerKind, index: Idx<Inst>) -> Option<u32> {
        let ContainerKind::Record(Some(owner)) = kind else {
            return None;
        };
        let Inst::Const(Lit::Str(field)) = self.func.insts[index] else {
            return None;
        };
        self.ctx
            .record_slots
            .get(&owner)?
            .slots
            .get(&field)
            .copied()
    }
}
