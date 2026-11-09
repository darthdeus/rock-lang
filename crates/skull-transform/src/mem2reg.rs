use comfy_la_arena::Idx;
use rustc_hash::FxHashSet as HashSet;
use skull_mir::cfg::{Dominators, dominates, dominators, reachable_blocks_from};
use skull_mir::mir::{Block, Cell, Inst, MirFunc};

#[derive(Clone, Copy, Debug)]
enum Definition {
    None,
    Param,
    Store {
        block: Idx<Block>,
        pos: usize,
        value: Idx<Inst>,
    },
    Other,
    Multiple,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mem2RegPromotion {
    pub cells: usize,
    pub loads: usize,
}

struct Analysis {
    defs: Vec<Definition>,
    loads: Vec<Vec<(Idx<Block>, usize, Idx<Inst>)>>,
    excluded: Vec<bool>,
}

fn raw<T>(id: Idx<T>) -> usize {
    u32::from(id.into_raw()) as usize
}

fn record_definition(defs: &mut [Definition], cell: Idx<Cell>, next: Definition) {
    let slot = &mut defs[raw(cell)];
    *slot = match (*slot, next) {
        (Definition::None, next) => next,
        _ => Definition::Multiple,
    };
}

fn instruction_definitions(inst: &Inst, mut push: impl FnMut(Idx<Cell>, bool)) {
    match inst {
        Inst::Store { cell, .. } => push(*cell, true),
        Inst::MapNext {
            key_cell, val_cell, ..
        } => {
            push(*key_cell, false);
            push(*val_cell, false);
        }
        Inst::CallMulti { dests, .. }
        | Inst::CallValueMulti { dests, .. }
        | Inst::CallBoundMulti { dests, .. } => {
            for &cell in dests {
                push(cell, false);
            }
        }
        _ => {}
    }
}

fn instruction_cells(inst: &Inst, mut push: impl FnMut(Idx<Cell>)) {
    if let Inst::Load(cell) = inst {
        push(*cell);
    }
    instruction_definitions(inst, |cell, _| push(cell));
    if let Inst::Lambda { params, .. } = inst {
        for &cell in params {
            push(cell);
        }
    }
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

fn analyze(func: &MirFunc) -> Analysis {
    let reachable = reachable_blocks_from(func, func.entry());
    let reachable_set: HashSet<Idx<Block>> = reachable.iter().copied().collect();
    let mut defs = vec![Definition::None; func.cells.len()];
    let mut loads: Vec<Vec<(Idx<Block>, usize, Idx<Inst>)>> = vec![Vec::new(); func.cells.len()];
    let mut excluded = vec![false; func.cells.len()];

    for &cell in &func.params {
        record_definition(&mut defs, cell, Definition::Param);
    }
    for &cell in &func.inline_cleanup_load_cells {
        excluded[raw(cell)] = true;
    }

    for (block, body) in func.blocks.iter() {
        if !reachable_set.contains(&block) {
            for &iid in &body.insts {
                instruction_cells(&func.insts[iid], |cell| excluded[raw(cell)] = true);
            }
            continue;
        }
        for (pos, &iid) in body.insts.iter().enumerate() {
            let inst = &func.insts[iid];
            if let Inst::Load(cell) = inst {
                loads[raw(*cell)].push((block, pos, iid));
            }
            if let Inst::StoreField { base, .. } = inst {
                if let Some(cell) = loaded_root(func, *base) {
                    excluded[raw(cell)] = true;
                }
            }
            instruction_definitions(inst, |cell, is_store| {
                if is_store {
                    let Inst::Store { value, .. } = inst else {
                        unreachable!()
                    };
                    record_definition(
                        &mut defs,
                        cell,
                        Definition::Store {
                            block,
                            pos,
                            value: *value,
                        },
                    );
                } else {
                    record_definition(&mut defs, cell, Definition::Other);
                }
            });
        }
    }

    Analysis {
        defs,
        loads,
        excluded,
    }
}

fn is_direct(idom: &Dominators, analysis: &Analysis, cell: usize) -> bool {
    if analysis.excluded[cell] || analysis.loads[cell].is_empty() {
        return false;
    }
    match analysis.defs[cell] {
        Definition::Param => true,
        Definition::Store {
            block: def_block,
            pos: def_pos,
            ..
        } => analysis.loads[cell]
            .iter()
            .all(|&(load_block, load_pos, _)| {
                if def_block == load_block {
                    def_pos < load_pos
                } else {
                    dominates(idom, def_block, load_block)
                }
            }),
        _ => false,
    }
}

pub fn promote_direct_cells(func: &mut MirFunc) -> Mem2RegPromotion {
    if func.is_coroutine {
        return Mem2RegPromotion::default();
    }
    let analysis = analyze(func);
    let idom = dominators(func, func.entry());
    let mut subst = vec![None; func.insts.len()];
    let mut result = Mem2RegPromotion::default();

    for cell in 0..func.cells.len() {
        if !is_direct(&idom, &analysis, cell) {
            continue;
        }
        let Definition::Store { value, .. } = analysis.defs[cell] else {
            continue;
        };
        result.cells += 1;
        result.loads += analysis.loads[cell].len();
        for &(_, _, load) in &analysis.loads[cell] {
            subst[raw(load)] = Some(value);
        }
    }
    if result.cells == 0 {
        return result;
    }

    let remap = |value: Idx<Inst>| super::resolve_subst(&subst, value);
    for (_, inst) in func.insts.iter_mut() {
        super::remap_inst(inst, &remap);
    }
    for (_, block) in func.blocks.iter_mut() {
        super::remap_term(&mut block.term, &remap);
    }
    super::dce(func);
    result
}
