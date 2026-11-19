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
    // The entry MUST be emitted first — Lua executes top-down, so it falls into
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

#[derive(Clone, Debug, Default)]
pub struct PredMap {
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

#[derive(Clone, Debug)]
pub struct Dominators {
    idom: Vec<Option<Idx<Block>>>,
}

impl Dominators {
    pub fn idom(&self, b: Idx<Block>) -> Option<Idx<Block>> {
        self.idom.get(dense::bx(b)).copied().flatten()
    }

    pub fn dominates(&self, a: Idx<Block>, b: Idx<Block>) -> bool {
        let mut x = b;
        loop {
            if x == a {
                return true;
            }
            let Some(p) = self.idom(x) else { return false };
            if p == x {
                return false; // reached the entry without hitting `a`
            }
            x = p;
        }
    }
}

pub fn dominators(func: &MirFunc, entry: Idx<Block>) -> Dominators {
    let n = func.blocks.len();
    let rpo: Vec<usize> = reverse_postorder(func, entry)
        .into_iter()
        .map(dense::bx)
        .collect();
    let mut pred_n = vec![0usize; n + 1];
    for &b in &rpo {
        successors(&func.blocks[dense::bi(b)].term, |s| {
            pred_n[dense::bx(s) + 1] += 1
        });
    }
    for i in 0..n {
        pred_n[i + 1] += pred_n[i];
    }
    let mut preds = vec![0usize; pred_n[n]];
    let mut fill = pred_n.clone();
    for &b in &rpo {
        successors(&func.blocks[dense::bi(b)].term, |s| {
            let k = dense::bx(s);
            preds[fill[k]] = b;
            fill[k] += 1;
        });
    }
    let idom = dense::chk_idoms(n, &rpo, |b| &preds[pred_n[b]..pred_n[b + 1]]);
    Dominators {
        idom: idom.into_iter().map(|d| d.map(dense::bi)).collect(),
    }
}

impl Dominators {
    pub fn intervals(&self) -> DomIntervals {
        let n = self.idom.len();
        let mut child_n = vec![0u32; n + 1];
        for (b, d) in self.idom.iter().enumerate() {
            if let Some(d) = d
                && dense::bx(*d) != b
            {
                child_n[dense::bx(*d) + 1] += 1;
            }
        }
        for i in 0..n {
            child_n[i + 1] += child_n[i];
        }
        let mut fill = child_n.clone();
        let mut children = vec![0u32; child_n[n] as usize];
        let mut roots = Vec::new();
        for (b, d) in self.idom.iter().enumerate() {
            match d {
                Some(d) if dense::bx(*d) != b => {
                    let p = dense::bx(*d);
                    children[fill[p] as usize] = b as u32;
                    fill[p] += 1;
                }
                Some(_) => roots.push(b as u32),
                None => {}
            }
        }
        let mut span = vec![(u32::MAX, 0u32); n];
        let mut clock = 0u32;
        let mut stack: Vec<(u32, u32)> = Vec::new();
        for root in roots {
            stack.push((root, child_n[root as usize]));
            span[root as usize].0 = clock;
            clock += 1;
            while let Some(top) = stack.last_mut() {
                let node = top.0 as usize;
                if top.1 < child_n[node + 1] {
                    let child = children[top.1 as usize];
                    top.1 += 1;
                    span[child as usize].0 = clock;
                    clock += 1;
                    stack.push((child, child_n[child as usize]));
                } else {
                    span[node].1 = clock;
                    stack.pop();
                }
            }
        }
        DomIntervals { span }
    }
}

#[derive(Clone, Debug)]
pub struct DomIntervals {
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

    pub(crate) fn succ_list(f: &MirFunc) -> Vec<Vec<usize>> {
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

    pub fn idoms(
        n: usize,
        entry: usize,
        preds: &[Vec<usize>],
        rpo_order: &[usize],
    ) -> Vec<Option<usize>> {
        let mut idom = chk_idoms(n, rpo_order, |b| &preds[b]);
        idom[entry] = None;
        idom
    }

    pub(crate) fn chk_idoms<'p>(
        n: usize,
        rpo: &[usize],
        preds: impl Fn(usize) -> &'p [usize],
    ) -> Vec<Option<usize>> {
        let mut order = vec![usize::MAX; n];
        for (i, &b) in rpo.iter().enumerate() {
            order[b] = i;
        }
        let mut idom = vec![None; n];
        let Some(&entry) = rpo.first() else {
            return idom;
        };
        idom[entry] = Some(entry);
        let intersect = |idom: &[Option<usize>], mut a: usize, mut b: usize| {
            while a != b {
                while order[a] > order[b] {
                    a = idom[a].expect("a processed block has an idom");
                }
                while order[b] > order[a] {
                    b = idom[b].expect("a processed block has an idom");
                }
            }
            a
        };
        let mut changed = true;
        while changed {
            changed = false;
            for &b in &rpo[1..] {
                let mut new_idom: Option<usize> = None;
                for &p in preds(b) {
                    if idom[p].is_some() {
                        new_idom = Some(match new_idom {
                            None => p,
                            Some(cur) => intersect(&idom, p, cur),
                        });
                    }
                }
                if new_idom.is_some() && idom[b] != new_idom {
                    idom[b] = new_idom;
                    changed = true;
                }
            }
        }
        idom
    }

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

    pub(crate) fn collect_loop_body(
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
