use crate::mir::{Block, MirFunc, Term};
use comfy_la_arena::Idx;

pub fn successors(term: &Term, mut f: impl FnMut(Idx<Block>)) {
    match term {
        Term::Return(_) | Term::Unreachable => {}
        Term::Goto(b) => f(*b),
        Term::Branch {
            then_bb, else_bb, ..
        } => {
            f(*then_bb);
            f(*else_bb);
        }
        Term::Yield { resume, .. } => f(*resume),
    }
}

/// In arena-id order; unreachable blocks are dropped so no dangling `goto`
/// reaches the printer.
pub fn reachable_blocks_from(func: &MirFunc, entry: Idx<Block>) -> Vec<Idx<Block>> {
    let mut visited = vec![false; func.blocks.len()];
    let mut stack = Vec::with_capacity(func.blocks.len());
    stack.push(entry);
    visited[dense::bx(entry)] = true;
    let mut count = 1;
    while let Some(b) = stack.pop() {
        successors(&func.blocks[b].term, |s| {
            let seen = &mut visited[dense::bx(s)];
            if !*seen {
                *seen = true;
                count += 1;
                stack.push(s);
            }
        });
    }
    // The entry MUST be emitted first: Lua executes top-down, so it falls into
    // the first label regardless of the `entry` field (matters when the entry is
    // not the lowest-id block, e.g. a coroutine's dispatch entry).
    let mut order = Vec::with_capacity(count);
    order.push(entry);
    order.extend(
        func.blocks
            .iter()
            .map(|(id, _)| id)
            .filter(|id| *id != entry && visited[dense::bx(*id)]),
    );
    order
}

/// A loop preheader dominates its header, so RPO emits it first: a `for` cannot
/// be a goto target, unlike a label.
pub fn reverse_postorder(func: &MirFunc, entry: Idx<Block>) -> Vec<Idx<Block>> {
    let mut post = Vec::with_capacity(func.blocks.len());
    let mut visited = vec![false; func.blocks.len()];
    let mut stack = Vec::with_capacity(2 * func.blocks.len());
    stack.push((entry, false));
    visited[dense::bx(entry)] = true;
    while let Some((b, processed)) = stack.pop() {
        if processed {
            post.push(b);
            continue;
        }
        stack.push((b, true));
        successors(&func.blocks[b].term, |s| {
            let seen = &mut visited[dense::bx(s)];
            if !*seen {
                *seen = true;
                stack.push((s, false));
            }
        });
    }
    post.reverse();
    post
}

/// Dense by block id; a block with none has no entry.
#[derive(Clone, Debug, Default)]
pub struct PredMap {
    /// `preds[off[b]..off[b + 1]]` are block `b`'s predecessors, in discovery order.
    off: Vec<u32>,
    preds: Vec<Idx<Block>>,
}

impl PredMap {
    pub fn get(&self, b: &Idx<Block>) -> Option<&[Idx<Block>]> {
        let at = dense::bx(*b);
        let (start, end) = (*self.off.get(at)? as usize, *self.off.get(at + 1)? as usize);
        (start < end).then(|| &self.preds[start..end])
    }
}

/// (all predecessors, goto-only predecessors). Loop edges are unconditional
/// gotos, so the goto-only set isolates the two loop entries.
pub fn predecessors(func: &MirFunc, rpo: &[Idx<Block>]) -> (PredMap, PredMap) {
    let n = func.blocks.len();
    let mut reach = vec![false; n];
    for &b in rpo {
        reach[dense::bx(b)] = true;
    }
    let each_edge = |f: &mut dyn FnMut(Idx<Block>, Idx<Block>, bool)| {
        for &b in rpo {
            match &func.blocks[b].term {
                Term::Goto(t) => f(*t, b, true),
                t => successors(t, |s| {
                    if reach[dense::bx(s)] {
                        f(s, b, false);
                    }
                }),
            }
        }
    };
    let mut all_off = vec![0u32; n + 1];
    let mut goto_off = vec![0u32; n + 1];
    each_edge(&mut |to, _, goto| {
        all_off[dense::bx(to) + 1] += 1;
        if goto {
            goto_off[dense::bx(to) + 1] += 1;
        }
    });
    for i in 0..n {
        all_off[i + 1] += all_off[i];
        goto_off[i + 1] += goto_off[i];
    }
    let placeholder = func.entry();
    let mut all_preds = vec![placeholder; all_off[n] as usize];
    let mut goto_preds = vec![placeholder; goto_off[n] as usize];
    let mut all_fill = all_off.clone();
    let mut goto_fill = goto_off.clone();
    each_edge(&mut |to, from, goto| {
        let t = dense::bx(to);
        all_preds[all_fill[t] as usize] = from;
        all_fill[t] += 1;
        if goto {
            goto_preds[goto_fill[t] as usize] = from;
            goto_fill[t] += 1;
        }
    });
    (
        PredMap {
            off: all_off,
            preds: all_preds,
        },
        PredMap {
            off: goto_off,
            preds: goto_preds,
        },
    )
}

/// The entry maps to itself; an unreachable block has no entry.
#[derive(Clone, Debug)]
pub struct Dominators {
    idom: Vec<u32>,
}

impl Dominators {
    pub fn dominates(&self, a: Idx<Block>, b: Idx<Block>) -> bool {
        dense::dominates(&self.idom, dense::bx(a) as u32, dense::bx(b) as u32)
    }

    pub fn intervals(&self) -> DomIntervals {
        let n = self.idom.len();
        let tree = structure::Succ::transpose(n, |edge| {
            for (child, &parent) in self.idom.iter().enumerate() {
                let child = child as u32;
                if parent != dense::NO_BLOCK && parent != child {
                    edge(child, parent);
                }
            }
        });
        let mut span = vec![(u32::MAX, 0u32); n];
        let root = (0..n)
            .find(|&b| self.idom[b] == b as u32)
            .expect("dominator root");
        let mut clock = 1;
        let mut stack = vec![(root, 0)];
        span[root].0 = 0;
        while let Some((node, next)) = stack.last_mut() {
            if let Some(&child) = tree.of(*node).get(*next) {
                *next += 1;
                let child = child as usize;
                span[child].0 = clock;
                clock += 1;
                stack.push((child, 0));
            } else {
                span[*node].1 = clock;
                stack.pop();
            }
        }
        DomIntervals { span }
    }
}

pub fn dominators(func: &MirFunc, entry: Idx<Block>) -> Dominators {
    let n = func.blocks.len();
    let rpo = reverse_postorder(func, entry);
    let pred = structure::Succ::transpose(n, |edge| {
        for &block in &rpo {
            successors(&func.blocks[block].term, |to| {
                edge(dense::bx(block) as u32, dense::bx(to) as u32)
            });
        }
    });
    Dominators {
        idom: dense::block_idoms(n, rpo.iter().map(|&block| dense::bx(block) as u32), &pred),
    }
}

#[derive(Clone, Debug)]
pub struct DomIntervals {
    /// `(preorder, end)` per block; an unreachable block has `u32::MAX`.
    span: Vec<(u32, u32)>,
}

impl DomIntervals {
    pub fn dominates(&self, a: Idx<Block>, b: Idx<Block>) -> bool {
        if a == b {
            return true;
        }
        let (Some(&(a_pre, a_end)), Some(&(b_pre, _))) =
            (self.span.get(dense::bx(a)), self.span.get(dense::bx(b)))
        else {
            return false;
        };
        a_pre != u32::MAX && b_pre != u32::MAX && a_pre <= b_pre && b_pre < a_end
    }
}

pub fn dominates(idom: &Dominators, a: Idx<Block>, b: Idx<Block>) -> bool {
    idom.dominates(a, b)
}

pub mod placement;
pub mod relooper;
pub mod structure;

pub mod dense {
    use crate::mir::Block;
    use comfy_la_arena::{Idx, RawIdx};
    use rustc_hash::FxHashSet as HashSet;

    pub fn bx(b: Idx<Block>) -> usize {
        b.into_raw().into_u32() as usize
    }

    pub fn bi(n: usize) -> Idx<Block> {
        Idx::from_raw(RawIdx::from_u32(n as u32))
    }

    pub(super) const NO_BLOCK: u32 = u32::MAX;

    pub(super) fn block_idoms(
        n: usize,
        rpo: impl Iterator<Item = u32> + Clone,
        preds: &super::structure::Succ,
    ) -> Vec<u32> {
        let mut order = vec![NO_BLOCK; n];
        for (i, block) in rpo.clone().enumerate() {
            order[block as usize] = i as u32;
        }
        let mut idom = Vec::new();
        solve_idoms(&mut idom, n, rpo, |b| preds.of(b), |b| order[b]);
        idom
    }

    pub(super) fn solve_idoms<'p>(
        idom: &mut Vec<u32>,
        len: usize,
        mut rpo: impl Iterator<Item = u32> + Clone,
        preds: impl Fn(usize) -> &'p [u32],
        rank: impl Fn(usize) -> u32,
    ) {
        idom.clear();
        idom.resize(len, NO_BLOCK);
        let Some(entry) = rpo.next() else {
            return;
        };
        idom[entry as usize] = entry;
        let mut retreating = false;
        loop {
            let mut changed = false;
            for b in rpo.clone() {
                let b = b as usize;
                let mut parent = NO_BLOCK;
                for &p in preds(b) {
                    let p_rank = rank(p as usize);
                    retreating |= p_rank != NO_BLOCK && p_rank >= rank(b);
                    if idom[p as usize] == NO_BLOCK {
                        continue;
                    }
                    if parent == NO_BLOCK {
                        parent = p;
                        continue;
                    }
                    let mut other = p;
                    while parent != other {
                        while rank(parent as usize) > rank(other as usize) {
                            parent = idom[parent as usize];
                        }
                        while rank(other as usize) > rank(parent as usize) {
                            other = idom[other as usize];
                        }
                    }
                }
                changed |= idom[b] != parent;
                idom[b] = parent;
            }
            // Without a retreating edge every predecessor was final on the first pass.
            if !changed || !retreating {
                break;
            }
        }
    }

    pub(super) fn dominates(idom: &[u32], a: u32, mut b: u32) -> bool {
        while b != a {
            let Some(&parent) = idom.get(b as usize) else {
                return false;
            };
            if parent == NO_BLOCK || parent == b {
                return false;
            }
            b = parent;
        }
        true
    }

    /// Every node that reaches `latch` without passing through `header`.
    pub(crate) fn collect_loop_body(
        header: u32,
        latch: u32,
        preds: &super::structure::Succ,
        body: &mut HashSet<u32>,
    ) {
        body.insert(header);
        if body.insert(latch) {
            let mut stack = vec![latch];
            while let Some(x) = stack.pop() {
                for &p in preds.of(x as usize) {
                    if p != header && body.insert(p) {
                        stack.push(p);
                    }
                }
            }
        }
    }
}
