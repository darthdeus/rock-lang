//! CFG analyses over `MirFunc`: successors, reachability, reverse-postorder,
//! predecessors, and dominators. IR-adjacent, so they live next to the arena
//! types every consumer already depends on -- the Lua printer, the relooper
//! backends, and skull-transform all read them from here.

use crate::mir::{Block, MirFunc, Term};
use comfy_la_arena::Idx;
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

/// Successor blocks of a terminator, in branch order (for reverse-postorder).
/// Visit each successor block of `term` (callback form — avoids the per-call
/// `Vec` alloc; this runs per block, per iteration inside the regalloc liveness
/// fixpoints, where it was a malloc/memmove hot spot). No caller needs the set
/// materialized.
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
        Term::Switch { cases, default, .. } => {
            for (_, b) in cases {
                f(*b);
            }
            f(*default);
        }
        Term::Yield { resume, .. } => f(*resume),
    }
}

/// Blocks reachable from `entry`, in arena-id order. Pure `goto`+labels means
/// label order is irrelevant to correctness, so emit in source-faithful id order
/// rather than RPO; unreachable blocks (dead lowering artifacts) are dropped so
/// no dangling `goto` reaches the printer. (RPO returns once counted-loop
/// recovery lands — a real `for` needs its preheader emitted just before the
/// header, which id order doesn't guarantee.)
pub fn reachable_blocks_from(func: &MirFunc, entry: Idx<Block>) -> Vec<Idx<Block>> {
    let mut visited: HashSet<Idx<Block>> = HashSet::default();
    let mut stack = vec![entry];
    visited.insert(entry);
    while let Some(b) = stack.pop() {
        successors(&func.blocks[b].term, |s| {
            if visited.insert(s) {
                stack.push(s);
            }
        });
    }
    // The entry MUST be emitted first — Lua executes top-down, so it falls into
    // the first label regardless of the `entry` field (matters when the entry is
    // not the lowest-id block, e.g. a coroutine's dispatch entry).
    let mut order = vec![entry];
    order.extend(
        func.blocks
            .iter()
            .map(|(id, _)| id)
            .filter(|id| *id != entry && visited.contains(id)),
    );
    order
}

/// Reverse-postorder of the blocks reachable from `entry`. A loop preheader
/// dominates its header, so RPO emits it first — the `for` then falls through
/// into the header position (a `for` can't be a goto target, unlike a label).
/// Unreachable lowering-artifact blocks are dropped.
pub fn reverse_postorder(func: &MirFunc, entry: Idx<Block>) -> Vec<Idx<Block>> {
    let mut post = Vec::new();
    let mut visited: HashSet<Idx<Block>> = HashSet::default();
    let mut stack = vec![(entry, false)];
    visited.insert(entry);
    while let Some((b, processed)) = stack.pop() {
        if processed {
            post.push(b);
            continue;
        }
        stack.push((b, true));
        successors(&func.blocks[b].term, |s| {
            if visited.insert(s) {
                stack.push((s, false));
            }
        });
    }
    post.reverse();
    post
}

/// (all predecessors, goto-only predecessors) of each reachable block. Loop
/// edges are unconditional gotos (preheader->header, latch->header), so the
/// goto-pred set isolates the two loop entries.
pub type PredMap = HashMap<Idx<Block>, Vec<Idx<Block>>>;

pub fn predecessors(func: &MirFunc, rpo: &[Idx<Block>]) -> (PredMap, PredMap) {
    let reach: HashSet<Idx<Block>> = rpo.iter().copied().collect();
    let mut all: PredMap = HashMap::default();
    let mut goto: PredMap = HashMap::default();
    for &b in rpo {
        match &func.blocks[b].term {
            Term::Goto(t) => {
                all.entry(*t).or_default().push(b);
                goto.entry(*t).or_default().push(b);
            }
            t => {
                successors(t, |s| {
                    if reach.contains(&s) {
                        all.entry(s).or_default().push(b);
                    }
                });
            }
        }
    }
    (all, goto)
}

/// Immediate-dominator map over the blocks reachable from `entry` (classic
/// Cooper-Harvey-Kennedy iterative fixpoint over reverse-postorder). The entry
/// maps to itself.
pub fn dominators(func: &MirFunc, entry: Idx<Block>) -> HashMap<Idx<Block>, Idx<Block>> {
    let rpo = reverse_postorder(func, entry);
    let mut order: HashMap<Idx<Block>, usize> = HashMap::default();
    for (i, &b) in rpo.iter().enumerate() {
        order.insert(b, i);
    }
    let (all_preds, _) = predecessors(func, &rpo);
    let mut idom: HashMap<Idx<Block>, Idx<Block>> = HashMap::default();
    idom.insert(entry, entry);
    let mut changed = true;
    while changed {
        changed = false;
        for &b in rpo.iter().skip(1) {
            let mut new_idom: Option<Idx<Block>> = None;
            for &p in all_preds.get(&b).map(|v| v.as_slice()).unwrap_or(&[]) {
                if !idom.contains_key(&p) {
                    continue; // not yet processed
                }
                new_idom = Some(match new_idom {
                    None => p,
                    Some(cur) => intersect(&idom, &order, p, cur),
                });
            }
            if let Some(ni) = new_idom {
                if idom.get(&b) != Some(&ni) {
                    idom.insert(b, ni);
                    changed = true;
                }
            }
        }
    }
    idom
}

/// The CHK "intersect" walk: climb the two dominator chains (by RPO number) until
/// they meet.
fn intersect(
    idom: &HashMap<Idx<Block>, Idx<Block>>,
    order: &HashMap<Idx<Block>, usize>,
    mut a: Idx<Block>,
    mut b: Idx<Block>,
) -> Idx<Block> {
    while a != b {
        while order[&a] > order[&b] {
            a = idom[&a];
        }
        while order[&b] > order[&a] {
            b = idom[&b];
        }
    }
    a
}

/// Does block `a` dominate block `b` (walk `b`'s idom chain to the entry)?
pub fn dominates(idom: &HashMap<Idx<Block>, Idx<Block>>, a: Idx<Block>, b: Idx<Block>) -> bool {
    let mut x = b;
    loop {
        if x == a {
            return true;
        }
        let Some(&p) = idom.get(&x) else { return false };
        if p == x {
            return false; // reached the entry without hitting `a`
        }
        x = p;
    }
}

/// Dense-index CFG primitives for the relooper backends (JS, GLSL), which need
/// structured goto-free output and work over `usize` node indices rather than
/// arena ids. The policy layers built on these -- post-dominators, natural-loop
/// registration, and the `Structure` each backend hands its walker -- stay in
/// the backend crates: they encode per-target structuring decisions, not shared
/// analysis.
pub mod dense {
    use super::successors;
    use crate::mir::{Block, MirFunc};
    use comfy_la_arena::{Idx, RawIdx};
    use rustc_hash::FxHashSet as HashSet;

    pub fn bx(b: Idx<Block>) -> usize {
        b.into_raw().into_u32() as usize
    }

    pub fn bi(n: usize) -> Idx<Block> {
        Idx::from_raw(RawIdx::from_u32(n as u32))
    }

    pub fn succ_list(f: &MirFunc) -> Vec<Vec<usize>> {
        let mut s = vec![Vec::new(); f.blocks.len()];
        for (b, blk) in f.blocks.iter() {
            let i = bx(b);
            successors(&blk.term, |t| s[i].push(bx(t)));
        }
        s
    }

    pub fn pred_list(n: usize, succ: &[Vec<usize>]) -> Vec<Vec<usize>> {
        let mut p = vec![Vec::new(); n];
        for (b, ss) in succ.iter().enumerate() {
            for &s in ss {
                p[s].push(b);
            }
        }
        p
    }

    /// Reverse post-order of the nodes reachable from `entry` over `adj`.
    pub fn rpo(n: usize, entry: usize, adj: &[Vec<usize>]) -> Vec<usize> {
        let mut visited = vec![false; n];
        let mut post = Vec::new();
        let mut stack = vec![(entry, 0usize)];
        visited[entry] = true;
        while let Some(&(node, i)) = stack.last() {
            if i < adj[node].len() {
                stack.last_mut().unwrap().1 += 1;
                let s = adj[node][i];
                if !visited[s] {
                    visited[s] = true;
                    stack.push((s, 0));
                }
            } else {
                post.push(node);
                stack.pop();
            }
        }
        post.reverse();
        post
    }

    /// Cooper-Harvey-Kennedy iterative dominators. Returns the immediate dominator of
    /// each node reachable in `rpo_order` (`entry` maps to `None`).
    pub fn idoms(
        n: usize,
        entry: usize,
        preds: &[Vec<usize>],
        rpo_order: &[usize],
    ) -> Vec<Option<usize>> {
        let mut rpo_index = vec![usize::MAX; n];
        for (i, &b) in rpo_order.iter().enumerate() {
            rpo_index[b] = i;
        }
        let mut idom = vec![None; n];
        idom[entry] = Some(entry);

        let intersect =
            |mut a: usize, mut b: usize, idom: &[Option<usize>], ri: &[usize]| -> usize {
                while a != b {
                    while ri[a] > ri[b] {
                        a = idom[a].unwrap();
                    }
                    while ri[b] > ri[a] {
                        b = idom[b].unwrap();
                    }
                }
                a
            };

        let mut changed = true;
        while changed {
            changed = false;
            for &b in rpo_order {
                if b == entry {
                    continue;
                }
                let mut new_idom: Option<usize> = None;
                for &p in &preds[b] {
                    if idom[p].is_some() {
                        new_idom = Some(match new_idom {
                            None => p,
                            Some(ni) => intersect(p, ni, &idom, &rpo_index),
                        });
                    }
                }
                if idom[b] != new_idom {
                    idom[b] = new_idom;
                    changed = true;
                }
            }
        }
        idom[entry] = None;
        idom
    }

    /// Does node `a` dominate node `b` (walk `b`'s idom chain to the root)?
    pub fn dominates(idom: &[Option<usize>], a: usize, b: usize) -> bool {
        let mut cur = b;
        loop {
            if cur == a {
                return true;
            }
            match idom[cur] {
                Some(p) => cur = p,
                None => return false,
            }
        }
    }

    /// The natural-loop body for the back edge `latch -> header`: every node that
    /// reaches `latch` without passing through `header`.
    pub fn collect_loop_body(
        header: usize,
        latch: usize,
        preds: &[Vec<usize>],
        body: &mut HashSet<usize>,
    ) {
        body.insert(header);
        if body.insert(latch) {
            let mut stack = vec![latch];
            while let Some(x) = stack.pop() {
                for &p in &preds[x] {
                    if p != header && body.insert(p) {
                        stack.push(p);
                    }
                }
            }
        }
    }
}
