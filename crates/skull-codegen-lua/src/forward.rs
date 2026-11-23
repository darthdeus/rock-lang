use super::*;

/// Cells referenced anywhere the blocks of `scope` emit, dense by `raw(cell)`.
pub(crate) fn referenced_cells_in_region(
    func: &MirFunc,
    scope: &[Idx<Block>],
    tail_forwards: &[Option<&[Idx<Inst>]>],
    suppressed: &[bool],
) -> Vec<bool> {
    let mut cells = vec![false; func.cells.len()];
    for &b in scope {
        let insts = &func.blocks[b].insts;
        let tail_len = tail_forwards[raw(b) as usize].map_or(0, <[_]>::len);
        for &iid in &insts[..insts.len() - tail_len] {
            if suppressed[raw(iid) as usize] || matches!(func.insts[iid], Inst::Lambda { .. }) {
                continue;
            }
            func.insts[iid].cells_each(&mut |c| cells[raw(c) as usize] = true);
        }
    }
    cells
}

#[derive(Clone)]
pub(crate) struct CallArgForward {
    pub(crate) producer: Idx<Inst>,
    pub(crate) prefix_len: usize,
    pub(crate) consumer: Idx<Inst>,
}

/// The call-argument forwards of a body, sorted by consumer.
#[derive(Default)]
pub(crate) struct CallArgForwards(Vec<CallArgForward>);

impl CallArgForwards {
    pub(crate) fn get(&self, id: Idx<Inst>) -> Option<&CallArgForward> {
        let at = self
            .0
            .binary_search_by_key(&raw(id), |f| raw(f.consumer))
            .ok()?;
        Some(&self.0[at])
    }

    pub(crate) fn iter(&self) -> std::slice::Iter<'_, CallArgForward> {
        self.0.iter()
    }
}

fn multi_dests(inst: &Inst) -> Option<&[Idx<Cell>]> {
    match inst {
        Inst::CallMulti { dests, .. } | Inst::CallValueMulti { dests, .. } => Some(dests),
        _ => None,
    }
}

fn cell_reference_counts(func: &MirFunc) -> Vec<u32> {
    let mut counts = vec![0; func.cells.len()];
    for (_, block) in func.blocks.iter() {
        for &id in &block.insts {
            func.insts[id].cells_each(&mut |cell| counts[raw(cell) as usize] += 1);
        }
    }
    counts
}

fn instruction_reference_counts(func: &MirFunc) -> Vec<u32> {
    let mut counts = vec![0; func.insts.len()];
    for (_, block) in func.blocks.iter() {
        for &id in &block.insts {
            func.insts[id].uses_each(&mut |used| {
                counts[raw(used) as usize] += 1;
            });
        }
        block.term.uses_each(&mut |used| {
            counts[raw(used) as usize] += 1;
        });
    }
    counts
}

pub(crate) fn tail_forwards<'a>(
    ws: &Workspace,
    func: &'a MirFunc,
    direct: bool,
) -> Vec<Option<&'a [Idx<Inst>]>> {
    let mut forwards = vec![None; func.blocks.len()];
    let cell_references = std::cell::OnceCell::new();
    for (block_id, block) in func.blocks.iter() {
        let Term::Return(values) = &block.term else {
            continue;
        };
        if direct
            && let Some(&call_id) = block.insts.last()
            && let Inst::Call {
                func: callee_ref, ..
            } = &func.insts[call_id]
        {
            let callee = func_of(ws, *callee_ref);
            let exact_wire =
                func.call_abi_plans
                    .get(&call_id)
                    .is_some_and(|abi| match abi.result_lanes() {
                        0 => values.is_empty(),
                        1 => values.as_slice() == [call_id],
                        _ => false,
                    });
            // Foreign bodies can return extra Lua values beyond their declared wire.
            if exact_wire
                && callee.inline_body.is_none()
                && !callee.is_extern
                && !callee.is_coroutine
            {
                forwards[raw(block_id) as usize] = Some(&block.insts[block.insts.len() - 1..]);
                continue;
            }
        }
        if values.is_empty() || block.insts.len() < values.len() + 1 {
            continue;
        }
        let first = block.insts.len() - values.len() - 1;
        let call_id = block.insts[first];
        let Some(dests) = multi_dests(&func.insts[call_id]) else {
            continue;
        };
        if dests.len() != values.len() {
            continue;
        }
        let cell_references = cell_references.get_or_init(|| cell_reference_counts(func));
        let exact_wire = values
            .iter()
            .zip(dests)
            .enumerate()
            .all(|(i, (&value, &dest))| {
                block.insts[first + i + 1] == value
                    && matches!(func.insts[value], Inst::Load(cell) if cell == dest)
                    && cell_references[raw(dest) as usize] == 2
            });
        if !exact_wire {
            continue;
        }
        forwards[raw(block_id) as usize] = Some(&block.insts[first..]);
    }
    forwards
}

pub(crate) fn call_arg_forwards(ws: &Workspace, func: &MirFunc) -> (CallArgForwards, Vec<bool>) {
    let mut forwards = Vec::new();
    let mut suppressed = vec![false; func.insts.len()];
    let references = std::cell::OnceCell::new();
    if skull_mir::pass_disabled("callfwd") {
        return (CallArgForwards(forwards), suppressed);
    }
    for (_, block) in func.blocks.iter() {
        for first in 0..block.insts.len() {
            let producer_id = block.insts[first];
            let Some(dests) = multi_dests(&func.insts[producer_id]) else {
                continue;
            };
            let consumer_position = first + dests.len() + 1;
            if dests.is_empty() || consumer_position >= block.insts.len() {
                continue;
            }
            let consumer = block.insts[consumer_position];
            let Inst::Call {
                func: consumer_func,
                args: consumer_args,
            } = &func.insts[consumer]
            else {
                continue;
            };
            if func_of(ws, *consumer_func).inline_body.is_some()
                || consumer_args.len() < dests.len()
            {
                continue;
            }
            let prefix_len = consumer_args.len() - dests.len();
            let loads = &block.insts[first + 1..consumer_position];
            let (cell_references, instruction_references) = references.get_or_init(|| {
                (
                    cell_reference_counts(func),
                    instruction_reference_counts(func),
                )
            });
            let exact_wire = loads
                .iter()
                .zip(dests)
                .enumerate()
                .all(|(i, (&load, &dest))| {
                    consumer_args[prefix_len + i] == load
                        && matches!(func.insts[load], Inst::Load(cell) if cell == dest)
                        && cell_references[raw(dest) as usize] == 2
                        && instruction_references[raw(load) as usize] == 1
                });
            if !exact_wire {
                continue;
            }
            suppressed[raw(producer_id) as usize] = true;
            for &load in loads {
                suppressed[raw(load) as usize] = true;
            }
            forwards.push(CallArgForward {
                producer: producer_id,
                prefix_len,
                consumer,
            });
        }
    }
    forwards.sort_unstable_by_key(|f| raw(f.consumer));
    (CallArgForwards(forwards), suppressed)
}

/// Every closure parameter cell is owned by its nested Lua function rather
/// than by the enclosing scope's local declaration.
pub(crate) fn collect_lambda_params(func: &MirFunc) -> HashSet<Idx<Cell>> {
    let mut out = HashSet::default();
    for (_, inst) in func.insts.iter() {
        if let Inst::Lambda { params, .. } = inst {
            out.extend(params.iter().copied());
        }
    }
    out
}
