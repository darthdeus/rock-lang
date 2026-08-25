use comfy_la_arena::Idx;
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use skull_mir::cfg::{dominates, dominators, reachable_blocks_from};
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
    reachable: Vec<Idx<Block>>,
    defs: Vec<Definition>,
    loads: Vec<Vec<(Idx<Block>, usize, Idx<Inst>)>>,
    excluded: Vec<bool>,
    referenced: Vec<bool>,
    stores: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mem2RegCensus {
    pub blocks: usize,
    pub cells: usize,
    pub loads: usize,
    pub stores: usize,
    pub loaded_cells: usize,
    pub direct_promotable_cells: usize,
    pub direct_promotable_loads: usize,
    pub parameter_promotable_cells: usize,
    pub parameter_promotable_loads: usize,
    pub store_promotable_cells: usize,
    pub store_promotable_loads: usize,
    pub cross_block_promotable_loads: usize,
    pub multi_definition_cells: usize,
    pub non_dominating_single_definition_cells: usize,
    pub other_single_definition_cells: usize,
    pub current_cell_slots: usize,
    pub guaranteed_removable_cell_slots: usize,
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
    let mut referenced = vec![false; func.cells.len()];
    let mut stores = 0usize;

    for &cell in &func.params {
        record_definition(&mut defs, cell, Definition::Param);
        referenced[raw(cell)] = true;
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
                referenced[raw(*cell)] = true;
            }
            if let Inst::StoreField { base, .. } = inst {
                if let Some(cell) = loaded_root(func, *base) {
                    excluded[raw(cell)] = true;
                }
            }
            instruction_definitions(inst, |cell, is_store| {
                referenced[raw(cell)] = true;
                if is_store {
                    stores += 1;
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
        reachable,
        defs,
        loads,
        excluded,
        referenced,
        stores,
    }
}

fn is_direct(idom: &HashMap<Idx<Block>, Idx<Block>>, analysis: &Analysis, cell: usize) -> bool {
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

pub fn mem2reg_census(func: &MirFunc) -> Mem2RegCensus {
    let analysis = analyze(func);
    let idom = dominators(func, func.entry());

    let mut direct = vec![false; func.cells.len()];
    let mut result = Mem2RegCensus {
        blocks: analysis.reachable.len(),
        cells: func.cells.len(),
        stores: analysis.stores,
        ..Mem2RegCensus::default()
    };

    for (cell, cell_loads) in analysis.loads.iter().enumerate() {
        if cell_loads.is_empty() {
            continue;
        }
        result.loaded_cells += 1;
        result.loads += cell_loads.len();
        let definition = analysis.defs[cell];
        let all_dominated = is_direct(&idom, &analysis, cell);
        if all_dominated {
            direct[cell] = true;
            result.direct_promotable_cells += 1;
            result.direct_promotable_loads += cell_loads.len();
            match definition {
                Definition::Param => {
                    result.parameter_promotable_cells += 1;
                    result.parameter_promotable_loads += cell_loads.len();
                }
                Definition::Store { .. } => {
                    result.store_promotable_cells += 1;
                    result.store_promotable_loads += cell_loads.len();
                }
                _ => unreachable!(),
            }
            result.cross_block_promotable_loads += cell_loads
                .iter()
                .filter(|&&(load_block, _, _)| match definition {
                    Definition::Param => load_block != func.entry(),
                    Definition::Store { block, .. } => load_block != block,
                    _ => false,
                })
                .count();
        } else {
            match definition {
                Definition::Multiple => result.multi_definition_cells += 1,
                Definition::Store { .. } => result.non_dominating_single_definition_cells += 1,
                Definition::Other => result.other_single_definition_cells += 1,
                Definition::None | Definition::Param => {}
            }
        }
    }

    let colors = skull_codegen_lua::color_cells(func);
    let current_colors: HashSet<u32> = colors
        .iter()
        .enumerate()
        .filter(|(cell, _)| analysis.referenced[*cell])
        .map(|(_, color)| *color)
        .filter(|&color| color != u32::MAX)
        .collect();
    result.current_cell_slots = current_colors.len();
    let mut candidate_colors: HashSet<u32> = direct
        .iter()
        .enumerate()
        .filter_map(|(cell, &is_direct)| {
            let color = colors[cell];
            (is_direct && analysis.referenced[cell] && color != u32::MAX).then_some(color)
        })
        .collect();
    for (cell, &color) in colors.iter().enumerate() {
        if analysis.referenced[cell] && !direct[cell] {
            candidate_colors.remove(&color);
        }
    }
    for &param in &func.params {
        candidate_colors.remove(&colors[raw(param)]);
    }
    candidate_colors.remove(&u32::MAX);
    result.guaranteed_removable_cell_slots = candidate_colors.len();
    result
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
