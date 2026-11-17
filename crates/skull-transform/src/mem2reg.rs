use std::cell::OnceCell;

use comfy_la_arena::Idx;
use rustc_hash::FxHashSet as HashSet;
use skull_mir::cfg::{dominators, reachable_blocks_from};
use skull_mir::mir::{Block, Cell, Inst, MirFunc};

use crate::visit::raw;

#[derive(Clone, Default)]
struct CellUses {
    store: Option<(Idx<Block>, usize, Idx<Inst>)>,
    loads: Vec<(Idx<Block>, usize, Idx<Inst>)>,
    excluded: bool,
}

fn loaded_root(func: &MirFunc, mut value: Idx<Inst>) -> Option<Idx<Cell>> {
    loop {
        match &func.insts[value] {
            Inst::Load(cell) => return Some(*cell),
            Inst::Field { base, .. } | Inst::TupleIndex { base, .. } => value = *base,
            _ => return None,
        }
    }
}

pub(crate) fn promote_direct_cells(func: &mut MirFunc) {
    if func.is_coroutine || func.cells.is_empty() {
        return;
    }
    let reachable: HashSet<_> = reachable_blocks_from(func, func.entry())
        .into_iter()
        .collect();
    let mut cells = vec![CellUses::default(); func.cells.len()];
    for &cell in func.params.iter().chain(&func.inline_cleanup_load_cells) {
        cells[raw(cell)].excluded = true;
    }
    for (block, body) in func.blocks.iter() {
        if !reachable.contains(&block) {
            for &id in &body.insts {
                func.insts[id].cells_each(&mut |cell| cells[raw(cell)].excluded = true);
            }
            continue;
        }
        for (pos, &id) in body.insts.iter().enumerate() {
            let inst = &func.insts[id];
            match inst {
                Inst::Load(cell) => cells[raw(*cell)].loads.push((block, pos, id)),
                Inst::Store { cell, value } => {
                    let uses = &mut cells[raw(*cell)];
                    uses.excluded |= uses.store.replace((block, pos, *value)).is_some();
                }
                Inst::StoreField { base, .. } => {
                    if let Some(cell) = loaded_root(func, *base) {
                        cells[raw(cell)].excluded = true;
                    }
                }
                _ => inst.cell_writes_each(&mut |cell| cells[raw(cell)].excluded = true),
            }
        }
    }

    let idom = OnceCell::new();
    let mut subst = Vec::new();
    for uses in cells {
        let Some((block, pos, value)) = uses.store else {
            continue;
        };
        if uses.excluded || uses.loads.is_empty() {
            continue;
        }
        if !uses.loads.iter().all(|&(load_block, load_pos, _)| {
            if block == load_block {
                pos < load_pos
            } else {
                idom.get_or_init(|| dominators(func, func.entry()))
                    .dominates(block, load_block)
            }
        }) {
            continue;
        }
        if subst.is_empty() {
            subst.resize(func.insts.len(), None);
        }
        for (_, _, load) in uses.loads {
            subst[raw(load)] = Some(value);
        }
    }
    if !subst.is_empty() {
        super::copy_prop::substitute_uses(func, &subst);
        super::dce(func);
    }
}
