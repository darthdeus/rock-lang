//! Reference counts and the tail-call and call-argument forwarding analyses.

use super::*;

/// Cells REFERENCED (Load/Store/`MapNext` or multi-return dest) anywhere the
/// blocks of `scope` emit, as a dense `raw(cell) -> bool`. Hoisted closure
/// bodies are separate ordinary MirFuncs, so the declaration walk only covers
/// this function's reachable blocks.
pub(crate) fn referenced_cells_in_region(
    func: &MirFunc,
    scope: &[Idx<Block>],
    tail_forwards: &[Option<TailForward>],
    suppressed: &[bool],
) -> Vec<bool> {
    let mut cells = vec![false; func.cells.len()];
    for &b in scope {
        let tail = tail_forwards[raw(b) as usize].as_ref();
        for (position, &iid) in func.blocks[b].insts.iter().enumerate() {
            if tail.is_some_and(|tail| position >= tail.first_suppressed) {
                continue;
            }
            if suppressed[raw(iid) as usize] {
                continue;
            }
            let mut mark = |c: Idx<Cell>| cells[raw(c) as usize] = true;
            match &func.insts[iid] {
                Inst::Load(c) => mark(*c),
                Inst::Store { cell, .. } => mark(*cell),
                Inst::MapNext {
                    key_cell, val_cell, ..
                } => {
                    mark(*key_cell);
                    mark(*val_cell);
                }
                Inst::CallMulti { dests, .. }
                | Inst::CallValueMulti { dests, .. }
                | Inst::CallBoundMulti { dests, .. } => dests.iter().copied().for_each(mark),
                _ => {}
            }
        }
    }
    cells
}

#[derive(Clone)]
pub(crate) enum TailForwardCall {
    Direct {
        call: Idx<Inst>,
        func: FuncRef,
        args: Vec<Idx<Inst>>,
    },
    Value {
        callee: Idx<Inst>,
        args: Vec<Idx<Inst>>,
    },
}

#[derive(Clone)]
pub(crate) struct TailForward {
    pub(crate) first_suppressed: usize,
    pub(crate) suppressed: Vec<Idx<Inst>>,
    pub(crate) call: TailForwardCall,
}

#[derive(Clone)]
pub(crate) struct CallArgForward {
    pub(crate) producer: TailForwardCall,
    pub(crate) prefix: Vec<Idx<Inst>>,
    pub(crate) consumer: Idx<Inst>,
    pub(crate) func: FuncRef,
}

/// The call-argument forwards of a body, sorted by consumer. Few calls
/// forward, so a slot per instruction was mostly empty, large slots.
#[derive(Default)]
pub(crate) struct CallArgForwards(Vec<CallArgForward>);

impl CallArgForwards {
    /// The forward whose consumer is `id`.
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

/// The cells a multi-return call receives its results in.
fn multi_dests(inst: &Inst) -> Option<&[Idx<Cell>]> {
    match inst {
        Inst::CallMulti { dests, .. } | Inst::CallValueMulti { dests, .. } => Some(dests),
        _ => None,
    }
}

/// The multi-return call `id` as the call a forward re-emits.
fn multi_call(func: &MirFunc, id: Idx<Inst>) -> TailForwardCall {
    match &func.insts[id] {
        Inst::CallMulti { func, args, .. } => TailForwardCall::Direct {
            call: id,
            func: *func,
            args: args.clone(),
        },
        Inst::CallValueMulti { callee, args, .. } => TailForwardCall::Value {
            callee: *callee,
            args: args.clone(),
        },
        other => unreachable!("{other:?} is not a multi-return call"),
    }
}

fn cell_reference_counts(func: &MirFunc) -> Vec<u32> {
    let mut counts = vec![0; func.cells.len()];
    for (_, block) in func.blocks.iter() {
        for &id in &block.insts {
            match &func.insts[id] {
                Inst::Load(cell) | Inst::Store { cell, .. } => counts[raw(*cell) as usize] += 1,
                Inst::CallMulti { dests, .. }
                | Inst::CallValueMulti { dests, .. }
                | Inst::CallBoundMulti { dests, .. } => {
                    for cell in dests {
                        counts[raw(*cell) as usize] += 1;
                    }
                }
                Inst::MapNext {
                    key_cell, val_cell, ..
                } => {
                    counts[raw(*key_cell) as usize] += 1;
                    counts[raw(*val_cell) as usize] += 1;
                }
                Inst::Lambda { params, .. } => {
                    for cell in params {
                        counts[raw(*cell) as usize] += 1;
                    }
                }
                _ => {}
            }
        }
    }
    counts
}

fn instruction_reference_counts(func: &MirFunc) -> Vec<u32> {
    let mut counts = vec![0; func.insts.len()];
    for (_, block) in func.blocks.iter() {
        for &id in &block.insts {
            regalloc::inst_uses(&func.insts[id], &mut |used| {
                counts[raw(used) as usize] += 1;
            });
        }
        regalloc::term_uses(&block.term, &mut |used| {
            counts[raw(used) as usize] += 1;
        });
    }
    counts
}

pub(crate) fn tail_forwards(
    ws: &Workspace,
    func: &MirFunc,
    direct: bool,
) -> Vec<Option<TailForward>> {
    let mut forwards = vec![None; func.blocks.len()];
    // Counted for the first multi-return candidate; most bodies have none.
    let cell_references = std::cell::OnceCell::new();
    for (block_id, block) in func.blocks.iter() {
        let Term::Return(values) = &block.term else {
            continue;
        };
        if direct
            && let Some(&call_id) = block.insts.last()
            && let Inst::Call {
                func: callee_ref,
                args,
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
                forwards[raw(block_id) as usize] = Some(TailForward {
                    first_suppressed: block.insts.len() - 1,
                    suppressed: vec![call_id],
                    call: TailForwardCall::Direct {
                        call: call_id,
                        func: *callee_ref,
                        args: args.clone(),
                    },
                });
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
        forwards[raw(block_id) as usize] = Some(TailForward {
            first_suppressed: first,
            suppressed: block.insts[first..].to_vec(),
            call: multi_call(func, call_id),
        });
    }
    forwards
}

pub(crate) fn call_arg_forwards(ws: &Workspace, func: &MirFunc) -> (CallArgForwards, Vec<bool>) {
    let mut forwards = Vec::new();
    let mut suppressed = vec![false; func.insts.len()];
    // Counted for the first multi-return call that feeds a call; most bodies
    // have none.
    let references = std::cell::OnceCell::new();
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
                producer: multi_call(func, producer_id),
                prefix: consumer_args[..prefix_len].to_vec(),
                consumer,
                func: *consumer_func,
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
