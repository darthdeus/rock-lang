use std::sync::{Arc, Mutex};

use bone_core::hir::StructDef;
use rustc_hash::FxHashMap;
use ustr::Ustr;

use super::{CodegenCtx, ContainerKind, Emitter, Idx, Inst, Lit, Workspace, write_lua_string};

pub(super) struct RecordLayout {
    pub(super) fields: Vec<Ustr>,
    pub(super) slots: FxHashMap<Ustr, u32>,
}

/// Slot layouts of declared records, planned up front, and of generic record
/// instances, planned when lowering first names one.
pub(super) struct RecordSlots {
    declared: FxHashMap<Ustr, Arc<RecordLayout>>,
    instances: Mutex<FxHashMap<Ustr, Option<Arc<RecordLayout>>>>,
}

fn layout(ws: &Workspace, s: &StructDef, name: Ustr) -> Option<Arc<RecordLayout>> {
    if s.is_value || s.is_nobox || s.lua_named || s.fields.is_empty() {
        return None;
    }
    let fields = skull_mir::lower::record_keys(ws, name)?;
    let slots = fields
        .iter()
        .enumerate()
        .map(|(i, name)| (*name, (i + 1) as u32))
        .collect();
    Some(Arc::new(RecordLayout { fields, slots }))
}

pub(super) fn plan(ws: &Workspace) -> RecordSlots {
    RecordSlots {
        declared: ws
            .source_modules()
            .flat_map(|module| module.structs.values())
            .filter_map(|s| Some((s.name, layout(ws, s, s.name)?)))
            .collect(),
        instances: Mutex::default(),
    }
}

impl RecordSlots {
    pub(super) fn get(&self, ws: &Workspace, name: Ustr) -> Option<Arc<RecordLayout>> {
        if let Some(layout) = self.declared.get(&name) {
            return Some(Arc::clone(layout));
        }
        skull_mir::layout_instance::instance(name)?;
        let mut instances = self.instances.lock().expect("record instance slots");
        instances
            .entry(name)
            .or_insert_with(|| {
                let sid = ws.nominals.struct_id(name)?;
                if sid.module.is_builtin() {
                    return None;
                }
                layout(ws, &ws.module(sid.module).structs[sid.def], name)
            })
            .clone()
    }
}

impl CodegenCtx {
    pub fn uses_record_slots(&self, ws: &Workspace, name: Ustr) -> bool {
        self.record_slots.get(ws, name).is_some()
    }

    /// `instances` are the layout instances the build reaches.
    pub fn record_slot_metadata(&self, ws: &Workspace, instances: &[Ustr]) -> String {
        let mut out = String::new();
        let mut records: Vec<_> = self.record_slots.declared.keys().copied().collect();
        records.extend(instances.iter().copied());
        records.sort_unstable();
        records.dedup();
        for name in records {
            let Some(layout) = self.record_slots.get(ws, name) else {
                continue;
            };
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
    /// A record literal's operands by slot, `None` where the literal leaves a
    /// slot absent, then the fields that have no slot. Positional entries keep
    /// every slot in the table's array part; keyed ones would land in its hash
    /// part. `None` when reordering would swap two operands that can fail.
    pub(super) fn slot_order<'f>(
        &self,
        layout: &RecordLayout,
        fields: &'f [(Ustr, Idx<Inst>)],
    ) -> Option<(Vec<Option<Idx<Inst>>>, Vec<&'f (Ustr, Idx<Inst>)>)> {
        let mut slots = vec![None; layout.fields.len()];
        let mut named = Vec::new();
        let mut last = 0;
        let mut in_order = true;
        for field in fields {
            let Some(&slot) = layout.slots.get(&field.0) else {
                named.push(field);
                last = usize::MAX;
                continue;
            };
            let slot = slot as usize;
            if slots[slot - 1].replace(field.1).is_some() {
                return None;
            }
            in_order &= slot > last;
            last = slot;
        }
        let effects = fields.iter().filter(|(_, id)| self.folds_effect(*id));
        (in_order || effects.count() <= 1).then_some((slots, named))
    }

    pub(super) fn record_slot(&self, kind: ContainerKind, index: Idx<Inst>) -> Option<u32> {
        let ContainerKind::Record(Some(owner)) = kind else {
            return None;
        };
        let Inst::Const(Lit::Str(field)) = self.func.insts[index] else {
            return None;
        };
        self.ctx
            .record_slots
            .get(self.ws, owner)?
            .slots
            .get(&field)
            .copied()
    }
}
