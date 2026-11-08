use rustc_hash::FxHashMap as HashMap;

use comfy_la_arena::Idx;
use skull_mir::mir::*;

/// Scalarize `buf[i] = <value-struct literal>` for a `NativeBuffer<value>`.
///
/// `StoreIndex { kind: NativeBuffer, value: MakeValueStruct }` otherwise builds
/// the WHOLE nested cdata (`Vertex_ct(Vec2_ct(..), Color_ct(..), ..)`) and copies
/// it into the slot -- ~5 cdata allocations per element that don't sink across
/// the buffer store. rock instead grabs the slot reference and writes each leaf
/// field in place (`slot.pos.x = ..`), allocating nothing. This benefits
/// glyph-quad loops such as `verts[ci+N] = make_vertex(..)`.
///
/// Rewrite the store to `slot = buf[i]` (a bounds-checked `__nb_checked_get`,
/// same as the original `__nb_checked_set`) followed by per-leaf `StoreField`s,
/// descending into nested value-struct fields via `Field(slot, name)` (FFI
/// returns a reference for aggregate fields, so the writes go through). The dead
/// `MakeValueStruct`s fall to DCE. Only fires when the stored value is a literal
/// `MakeValueStruct` -- an opaque cdata (a variable) is copied whole as before.
pub fn scalarize_native_buffer_stores(func: &mut MirFunc) {
    // (store inst, base, index, value-MakeValueStruct, kind) for each rewritable
    // store. Both NativeBuffer flavors: a value-element buffer arrives as
    // `NativeBufferValue` (carried through to the slot so the JS emitter keeps
    // the flat-slot element info).
    let targets: Vec<(Idx<Inst>, Idx<Inst>, Idx<Inst>, Idx<Inst>, ContainerKind)> = func
        .blocks
        .iter()
        .flat_map(|(_, b)| b.insts.iter().copied())
        .filter_map(|i| match &func.insts[i] {
            Inst::StoreIndex {
                base,
                index,
                value,
                kind,
            } if kind.is_native_buffer()
                && matches!(&func.insts[*value], Inst::MakeValueStruct { .. }) =>
            {
                Some((i, *base, *index, *value, *kind))
            }
            _ => None,
        })
        .collect();
    if targets.is_empty() {
        return;
    }
    let mut replacement: HashMap<Idx<Inst>, Vec<Idx<Inst>>> = HashMap::default();
    for (store_i, base, index, value, kind) in targets {
        let slot = func.alloc_inst(Inst::Index { base, index, kind });
        let mut new = vec![slot];
        gen_leaf_stores(func, slot, value, &mut new);
        replacement.insert(store_i, new);
    }
    let block_ids: Vec<Idx<Block>> = func.blocks.iter().map(|(id, _)| id).collect();
    for bid in block_ids {
        if !func.blocks[bid]
            .insts
            .iter()
            .any(|i| replacement.contains_key(i))
        {
            continue;
        }
        let old = std::mem::take(&mut func.blocks[bid].insts);
        let mut new = Vec::with_capacity(old.len());
        for i in old {
            match replacement.get(&i) {
                Some(ins) => new.extend_from_slice(ins),
                None => new.push(i),
            }
        }
        func.blocks[bid].insts = new;
    }
}

/// Emit one `StoreField(slot, field, value)` per IMMEDIATE field of `mvs` (a
/// `MakeValueStruct`). Does NOT descend into nested value-struct fields: a
/// nested field's value is assigned whole (`slot.pos = Vec2_ct(x, y)`), matching
/// rock. The nested ctor then SINKS on the trace (it never escapes -- only its
/// bytes are copied into the slot). Recursing instead (`slot.pos.x = x`) would
/// READ `slot.pos`, and indexing an aggregate cdata field allocates a reference
/// cdata per access -- the very allocation we are removing.
fn gen_leaf_stores(func: &mut MirFunc, slot: Idx<Inst>, mvs: Idx<Inst>, out: &mut Vec<Idx<Inst>>) {
    let Inst::MakeValueStruct { fields, .. } = &func.insts[mvs] else {
        return;
    };
    let fields = fields.clone();
    for (fname, op) in fields {
        out.push(func.alloc_inst(Inst::StoreField {
            base: slot,
            field: fname,
            value: op,
        }));
    }
}
