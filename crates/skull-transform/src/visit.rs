use comfy_la_arena::Idx;
use skull_mir::cfg::successors;
use skull_mir::mir::*;

/// Field name for a frame cell in the state table.
#[inline]
pub(crate) fn raw<T>(i: Idx<T>) -> u32 {
    i.into_raw().into()
}

/// Blocks reachable from the function entry via terminators — the co-fn's own
/// CFG, EXCLUDING closure sub-blocks (reached only through `Inst::Lambda`).
pub(crate) fn top_level_blocks(f: &MirFunc) -> Vec<Idx<Block>> {
    let mut seen = vec![false; f.blocks.len()];
    let mut stack = vec![f.entry];
    seen[raw(f.entry) as usize] = true;
    let mut order = Vec::with_capacity(f.blocks.len());
    while let Some(b) = stack.pop() {
        order.push(b);
        successors(&f.blocks[b].term, |s| {
            let seen = &mut seen[raw(s) as usize];
            if !*seen {
                *seen = true;
                stack.push(s);
            }
        });
    }
    order
}

/// Value operand of a terminator (the only ones are `cond`/`discr`/return value).
pub(crate) fn term_value(term: &Term) -> Option<Idx<Inst>> {
    match term {
        // Coroutine returns are single/zero by the time spill runs (a co fn never
        // scalarizes its completion value); the first covers it.
        Term::Return(vs) => vs.first().copied(),
        Term::Branch { cond, .. } => Some(*cond),
        Term::Switch { discr, .. } => Some(*discr),
        Term::Yield { values, .. } => values.first().copied(),
        _ => None,
    }
}
