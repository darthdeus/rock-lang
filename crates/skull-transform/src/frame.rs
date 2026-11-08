use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use crate::visit::{raw, term_succs};
use comfy_la_arena::Idx;
use skull_mir::mir::*;

/// Materialize a coroutine region's frame accesses as MIR (the JS path's
/// analog of codegen-lua's root `FrameCtx` rendering): over the blocks
/// reachable from `entry` via terminators, a bare `Load`/`Store` of a cell in
/// `slots` becomes a `Field`/`StoreField` on `s` (field `c<slot>`), and a
/// multi-call dest that is a frame cell is redirected through a fresh temp
/// cell + a spliced store-back. `MapNext`'s key and value cells use the same
/// seeded-temp/store-back shape. Non-frame cells remain ordinary step locals;
/// every remaining bare frame cell is a transform finding.
pub fn materialize_frame(
    mf: &mut MirFunc,
    entry: Idx<Block>,
    s_cell: Idx<Cell>,
    slots: &HashMap<Idx<Cell>, u32>,
) {
    materialize_frame_inner(mf, entry, s_cell, slots, true);
}

/// Materialize only the selected shared frame cells in an ordinary detached
/// function. Its other cells are real locals, so the strict exhaustiveness check
/// used by the coroutine step does not apply here.
pub fn materialize_frame_cells(
    mf: &mut MirFunc,
    entry: Idx<Block>,
    s_cell: Idx<Cell>,
    slots: &HashMap<Idx<Cell>, u32>,
) {
    materialize_frame_inner(mf, entry, s_cell, slots, false);
}

fn materialize_frame_inner(
    mf: &mut MirFunc,
    entry: Idx<Block>,
    s_cell: Idx<Cell>,
    slots: &HashMap<Idx<Cell>, u32>,
    strict: bool,
) {
    let field_of = |slot: u32| ustr::Ustr::from(format!("c{slot}").as_str());
    let mut region: Vec<Idx<Block>> = Vec::new();
    {
        let mut seen: HashSet<Idx<Block>> = HashSet::default();
        let mut stack = vec![entry];
        seen.insert(entry);
        while let Some(b) = stack.pop() {
            region.push(b);
            term_succs(&mf.blocks[b].term, |t| {
                if seen.insert(t) {
                    stack.push(t);
                }
            });
        }
    }
    let mut minted: HashSet<Idx<Cell>> = HashSet::default();
    for &bid in &region {
        let old: Vec<Idx<Inst>> = std::mem::take(&mut mf.blocks[bid].insts);
        let mut new: Vec<Idx<Inst>> = Vec::with_capacity(old.len());
        for id in old {
            match mf.insts[id].clone() {
                Inst::Load(c) if slots.contains_key(&c) => {
                    let sv = mf.alloc_inst(Inst::Load(s_cell));
                    new.push(sv);
                    mf.insts[id] = Inst::Field {
                        base: sv,
                        field: field_of(slots[&c]),
                    };
                    new.push(id);
                }
                Inst::Store { cell, value } if slots.contains_key(&cell) => {
                    let sv = mf.alloc_inst(Inst::Load(s_cell));
                    new.push(sv);
                    mf.insts[id] = Inst::StoreField {
                        base: sv,
                        field: field_of(slots[&cell]),
                        value,
                    };
                    new.push(id);
                }
                Inst::CallMulti { dests, .. }
                | Inst::CallValueMulti { dests, .. }
                | Inst::CallBoundMulti { dests, .. } => {
                    // Redirect frame-cell dests to fresh temps, then store each
                    // back to its `s` field right after the call.
                    let redirects: Vec<(usize, Idx<Cell>, Idx<Cell>)> = dests
                        .iter()
                        .enumerate()
                        .filter(|(_, c)| slots.contains_key(c))
                        .map(|(i, &c)| {
                            let temp = mf.new_cell(None);
                            minted.insert(temp);
                            (i, c, temp)
                        })
                        .collect();
                    if let Inst::CallMulti { dests, .. }
                    | Inst::CallValueMulti { dests, .. }
                    | Inst::CallBoundMulti { dests, .. } = &mut mf.insts[id]
                    {
                        for &(i, _, temp) in &redirects {
                            dests[i] = temp;
                        }
                    }
                    new.push(id);
                    for &(_, frame_cell, temp) in &redirects {
                        let lt = mf.alloc_inst(Inst::Load(temp));
                        let sv = mf.alloc_inst(Inst::Load(s_cell));
                        let sf = mf.alloc_inst(Inst::StoreField {
                            base: sv,
                            field: field_of(slots[&frame_cell]),
                            value: lt,
                        });
                        new.extend([lt, sv, sf]);
                    }
                }
                // `key, val = next(t, key)` with a framed cursor/value cell:
                // redirect both to fresh temps, SEED the key temp from its frame
                // field first (the key cell is IN/OUT -- the cursor carried across
                // iterations), then store both temps back to their fields. Without
                // this a framed cursor would read from `s.c<slot>` (materialized
                // `Load`) but be written to a bare local, so the loop never advances.
                Inst::MapNext {
                    key_cell,
                    val_cell,
                    table,
                } if slots.contains_key(&key_cell) || slots.contains_key(&val_cell) => {
                    let k_use = if slots.contains_key(&key_cell) {
                        let t = mf.new_cell(None);
                        minted.insert(t);
                        let sv = mf.alloc_inst(Inst::Load(s_cell));
                        let fld = mf.alloc_inst(Inst::Field {
                            base: sv,
                            field: field_of(slots[&key_cell]),
                        });
                        let st = mf.alloc_inst(Inst::Store {
                            cell: t,
                            value: fld,
                        });
                        new.extend([sv, fld, st]);
                        t
                    } else {
                        key_cell
                    };
                    let v_use = if slots.contains_key(&val_cell) {
                        let t = mf.new_cell(None);
                        minted.insert(t);
                        t
                    } else {
                        val_cell
                    };
                    mf.insts[id] = Inst::MapNext {
                        key_cell: k_use,
                        val_cell: v_use,
                        table,
                    };
                    new.push(id);
                    for (frame_cell, temp) in [(key_cell, k_use), (val_cell, v_use)]
                        .into_iter()
                        .filter(|(c, _)| slots.contains_key(c))
                    {
                        let lt = mf.alloc_inst(Inst::Load(temp));
                        let sv = mf.alloc_inst(Inst::Load(s_cell));
                        let sf = mf.alloc_inst(Inst::StoreField {
                            base: sv,
                            field: field_of(slots[&frame_cell]),
                            value: lt,
                        });
                        new.extend([lt, sv, sf]);
                    }
                }
                _ => new.push(id),
            }
        }
        mf.blocks[bid].insts = new;
    }
    if !strict {
        return;
    }
    // Exhaustiveness: a surviving bare cell is permitted only when it is not a
    // planned frame slot. Precise liveness intentionally leaves segment-local
    // cells bare; the slot check still catches a missed materialization.
    for &bid in &region {
        for &id in &mf.blocks[bid].insts {
            let bad = match &mf.insts[id] {
                Inst::Load(c) if slots.contains_key(c) => Some(*c),
                Inst::Store { cell, .. } if slots.contains_key(cell) => Some(*cell),
                Inst::MapNext { key_cell, .. } if slots.contains_key(key_cell) => Some(*key_cell),
                Inst::MapNext { val_cell, .. } if slots.contains_key(val_cell) => Some(*val_cell),
                _ => None,
            };
            if let Some(c) = bad {
                panic!(
                    "materialize_frame: unframed cell c{} ({:?}) survives in region of `{}` \
                     (frame-membership finding)",
                    raw(c),
                    mf.cells[c].name,
                    mf.name
                );
            }
        }
    }
}
