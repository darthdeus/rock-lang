use super::dense::{NO_BLOCK, bi, block_idoms, bx, collect_loop_body, dominates, solve_idoms};
use crate::mir::{Block, Cell, Inst, Lit, MirFunc, Term};
use comfy_la_arena::Idx;
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CountedBound {
    Any,
    /// Only a `Load` of a hoisted preheader cell or a const: the counted path skips
    /// the header block, so a bound computed there would name a never-emitted temp.
    InlineOnly,
}

/// A tested natural loop with a single exit; `while`/`for` loops are single-exit
/// by construction.
#[derive(Clone)]
pub struct LoopRegion {
    pub header: Idx<Block>,
    pub body: HashSet<Idx<Block>>,
    pub exit: Idx<Block>,
}

pub struct Structure {
    /// `None` is the virtual exit: a diverging branch has no real join.
    ipdom: Vec<Option<Idx<Block>>>,
    pub loops: HashMap<Idx<Block>, LoopRegion>,
}

impl Structure {
    pub fn ipdom(&self, b: Idx<Block>) -> Option<Idx<Block>> {
        self.ipdom[bx(b)]
    }
}

pub fn analyze(f: &MirFunc) -> Structure {
    let exit = f.blocks.len() as u32;
    let succ = Succ::from_func(f, Some(exit));
    let preds = Succ::transpose(exit as usize + 1, |edge| {
        for b in 0..exit {
            for &to in succ.of(b as usize) {
                edge(b, to);
            }
        }
    });
    let mut stack = Vec::new();
    let mut order = vec![NO_BLOCK; exit as usize + 1];
    let mut blocks = Vec::new();
    succ.postorder(raw_of(f.entry), &mut stack, &mut order, &mut blocks);
    let idom = block_idoms(order.len(), blocks.iter().rev().copied(), &preds);
    let loops = natural_loops(exit, &succ, &preds, &idom);

    // Only returns enter the virtual exit. Treating Unreachable as an exit
    // would hide the real join of an exhaustive match.
    order.fill(NO_BLOCK);
    blocks.clear();
    preds.postorder(exit, &mut stack, &mut order, &mut blocks);
    let ipdom = block_idoms(order.len(), blocks.iter().rev().copied(), &succ)
        .into_iter()
        .take(exit as usize)
        .map(|b| (b < exit).then(|| bi(b as usize)))
        .collect();
    Structure { ipdom, loops }
}

fn natural_loops(
    exit: u32,
    succ: &Succ,
    preds: &Succ,
    idom: &[u32],
) -> HashMap<Idx<Block>, LoopRegion> {
    let mut latches: HashMap<u32, Vec<u32>> = HashMap::default();
    for latch in 0..exit {
        if idom[latch as usize] == NO_BLOCK {
            continue;
        }
        for &h in succ.of(latch as usize) {
            if h != exit && dominates(idom, h, latch) {
                latches.entry(h).or_default().push(latch);
            }
        }
    }

    let mut out = HashMap::default();
    for (h, hlatches) in latches {
        let mut body: HashSet<u32> = HashSet::default();
        for &l in &hlatches {
            collect_loop_body(h, l, preds, &mut body);
        }
        // The exit is the out-of-loop successor of the shallowest body block that
        // dominates every latch; early `return`/`break` blocks fail that test.
        let mut candidates: Vec<u32> = body
            .iter()
            .copied()
            .filter(|&b| {
                succ.of(b as usize)
                    .iter()
                    .any(|s| *s != exit && !body.contains(s))
                    && hlatches.iter().all(|&l| dominates(idom, b, l))
            })
            .collect();
        candidates.sort_by_key(|&b| b);
        let single_test = candidates
            .iter()
            .copied()
            .find(|&c| candidates.iter().all(|&c2| dominates(idom, c, c2)))
            .and_then(|test| {
                let mut exits = succ
                    .of(test as usize)
                    .iter()
                    .copied()
                    .filter(|s| *s != exit && !body.contains(s));
                let e = exits.next()?;
                exits.next().is_none().then_some((test, e))
            });
        let Some((_, exit)) = single_test else {
            continue; // the GLSL walker rejects untested loops
        };
        out.insert(
            bi(h as usize),
            LoopRegion {
                header: bi(h as usize),
                body: body.iter().map(|&x| bi(x as usize)).collect(),
                exit: bi(exit as usize),
            },
        );
    }
    out
}

#[derive(Clone, Copy)]
pub struct CountedLoop {
    pub slot: Idx<Cell>,
    pub bound: Idx<Inst>,
    pub inclusive: bool,
    pub body_entry: Idx<Block>,
    pub latch: Idx<Block>,
    pub exit: Idx<Block>,
}

/// Counted only when the header's own `Branch` is the loop test: a
/// `while i < N && ..` header looks similar but exits inside the loop.
pub fn split_loops(
    f: &MirFunc,
    st: &Structure,
    bound: CountedBound,
) -> (
    HashMap<Idx<Block>, CountedLoop>,
    HashMap<Idx<Block>, LoopRegion>,
) {
    let mut counted = HashMap::default();
    let mut general = HashMap::default();
    for (&h, region) in &st.loops {
        match counted_loop(f, h, bound) {
            Some(li) if li.exit == region.exit && region.body.contains(&li.latch) => {
                counted.insert(h, li);
            }
            _ => {
                general.insert(h, region.clone());
            }
        }
    }
    (counted, general)
}

/// The slot must be `loop_binding`, which only `lower_for_range` sets, so a
/// manual `while i < n { ..; i = i + 1 }` counter takes the general path.
pub fn counted_loop(f: &MirFunc, h: Idx<Block>, bound_rule: CountedBound) -> Option<CountedLoop> {
    let Term::Branch {
        cond,
        then_bb,
        else_bb,
    } = f.blocks[h].term
    else {
        return None;
    };
    let (slot, bound, inclusive) = match &f.insts[cond] {
        Inst::IntLt(a, b) => (load_cell(f, *a)?, *b, false),
        Inst::IntLeq(a, b) => (load_cell(f, *a)?, *b, true),
        _ => return None,
    };
    if !f.cells[slot].loop_binding {
        return None;
    }
    if bound_rule == CountedBound::InlineOnly
        && !matches!(f.insts[bound], Inst::Load(_) | Inst::Const(_))
    {
        return None;
    }
    let mut latch = None;
    for (bid, blk) in f.blocks.iter() {
        if blk.term == Term::Goto(h) && is_incr_latch(f, bid, slot) {
            latch = Some(bid);
        }
    }
    Some(CountedLoop {
        slot,
        bound,
        inclusive,
        body_entry: then_bb,
        latch: latch?,
        exit: else_bb,
    })
}

fn load_cell(f: &MirFunc, iid: Idx<Inst>) -> Option<Idx<Cell>> {
    match f.insts[iid] {
        Inst::Load(c) => Some(c),
        _ => None,
    }
}

/// `p`: `load slot; 1; slot + 1; store slot; goto h`.
fn is_incr_latch(f: &MirFunc, p: Idx<Block>, slot: Idx<Cell>) -> bool {
    for &iid in &f.blocks[p].insts {
        if let Inst::Store { cell, value } = f.insts[iid] {
            if cell == slot {
                if let Inst::IntAdd(a, b) = f.insts[value] {
                    let is_one = |x: Idx<Inst>| matches!(f.insts[x], Inst::Const(Lit::Int(1)));
                    let loads_slot =
                        |x: Idx<Inst>| matches!(f.insts[x], Inst::Load(c) if c == slot);
                    return (loads_slot(a) && is_one(b)) || (loads_slot(b) && is_one(a));
                }
                return false;
            }
        }
    }
    false
}

/// Block `b`'s successors are `targets[offsets[b]..offsets[b + 1]]`.
#[derive(Default)]
pub struct Succ {
    pub offsets: Vec<u32>,
    pub targets: Vec<u32>,
}

impl Succ {
    pub(super) fn of(&self, b: usize) -> &[u32] {
        &self.targets[self.offsets[b] as usize..self.offsets[b + 1] as usize]
    }

    fn from_func(f: &MirFunc, exit: Option<u32>) -> Self {
        let mut graph = Self::default();
        graph.offsets.push(0);
        for (_, block) in f.blocks.iter() {
            if let Some(exit) = exit
                && matches!(block.term, Term::Return(_))
            {
                graph.targets.push(exit);
            } else {
                super::successors(&block.term, |to| graph.targets.push(raw_of(to)));
            }
            graph.offsets.push(graph.targets.len() as u32);
        }
        if exit.is_some() {
            graph.offsets.push(graph.targets.len() as u32);
        }
        graph
    }

    pub(super) fn transpose(n: usize, each_edge: impl Fn(&mut dyn FnMut(u32, u32))) -> Self {
        let mut graph = Self::default();
        let mut fill = Vec::new();
        graph.store_predecessors(n, &mut fill, each_edge);
        graph
    }

    fn store_predecessors(
        &mut self,
        n: usize,
        fill: &mut Vec<u32>,
        each_edge: impl Fn(&mut dyn FnMut(u32, u32)),
    ) {
        reset(&mut self.offsets, n + 1, 0);
        each_edge(&mut |_, to| self.offsets[to as usize + 1] += 1);
        for i in 0..n {
            self.offsets[i + 1] += self.offsets[i];
        }
        reset(&mut self.targets, self.offsets[n] as usize, 0);
        fill.clear();
        fill.extend_from_slice(&self.offsets);
        each_edge(&mut |from, to| {
            let at = &mut fill[to as usize];
            self.targets[*at as usize] = from;
            *at += 1;
        });
    }

    fn postorder(
        &self,
        entry: u32,
        stack: &mut Vec<(u32, u32)>,
        order: &mut [u32],
        blocks: &mut Vec<u32>,
    ) {
        stack.push((entry, 0));
        order[entry as usize] = 0;
        while let Some(top) = stack.last_mut() {
            let (b, i) = *top;
            match self.of(b as usize).get(i as usize) {
                Some(&to) => {
                    top.1 += 1;
                    if order[to as usize] == NO_BLOCK {
                        order[to as usize] = 0;
                        stack.push((to, 0));
                    }
                }
                None => {
                    blocks.push(b);
                    stack.pop();
                }
            }
        }
    }
}

/// A block reached by two or more forward edges, or one leaving a loop, is a
/// follower, printed after its placement parent's construct; every other block
/// prints inline. All but `order` is indexed by reverse-postorder position.
pub struct Placement {
    /// Block -> reverse-postorder position, [`NO_BLOCK`] if unreachable.
    order: Vec<u32>,
    blocks: Vec<u32>,
    pos: Vec<Pos>,
    /// `(header, latch)` positions of every back edge, sorted.
    latches: Vec<(u32, u32)>,
    /// Followers grouped by placement parent, each group in reverse postorder;
    /// [`Pos::follow`] is where a parent's group starts.
    follow: Vec<Idx<Block>>,
}

#[derive(Clone, Copy)]
struct Pos {
    /// The innermost loop header containing this position (itself, for a header).
    loop_of: u32,
    /// For a header: the header of the loop enclosing its loop.
    loop_parent: u32,
    loop_size: u32,
    follow: u32,
    flags: u8,
}

const HEADER: u8 = 1;
const FOLLOWER: u8 = 2;
const UNPLACED: u8 = 4;
const IN_PLACE: u8 = 8;

impl Placement {
    fn at(&self, b: Idx<Block>) -> u32 {
        self.order.get(bx(b)).copied().unwrap_or(NO_BLOCK)
    }

    fn flag(&self, b: Idx<Block>, f: u8) -> bool {
        let p = self.at(b);
        p != NO_BLOCK && self.pos[p as usize].flags & f != 0
    }

    pub fn is_header(&self, b: Idx<Block>) -> bool {
        self.flag(b, HEADER)
    }

    pub fn latches(&self, h: Idx<Block>) -> impl Iterator<Item = Idx<Block>> + '_ {
        let p = self.at(h);
        let start = self.latches.partition_point(|&(x, _)| x < p);
        self.latches[start..]
            .iter()
            .take_while(move |&&(x, _)| x == p)
            .map(|&(_, l)| bi(self.blocks[l as usize] as usize))
    }

    pub fn in_loop(&self, h: Idx<Block>, b: Idx<Block>) -> bool {
        let (h, b) = (self.at(h), self.at(b));
        b != NO_BLOCK && in_loop(&self.pos, h, b)
    }

    /// Nested loops included.
    pub fn loop_size(&self, h: Idx<Block>) -> usize {
        let p = self.at(h);
        if p == NO_BLOCK {
            0
        } else {
            self.pos[p as usize].loop_size as usize
        }
    }

    pub fn is_follower(&self, b: Idx<Block>) -> bool {
        self.flag(b, FOLLOWER)
    }

    /// In reverse postorder.
    pub fn followers(&self, b: Idx<Block>) -> &[Idx<Block>] {
        let p = self.at(b);
        if p == NO_BLOCK {
            return &[];
        }
        let p = p as usize;
        let end = self
            .pos
            .get(p + 1)
            .map_or(self.follow.len(), |q| q.follow as usize);
        &self.follow[self.pos[p].follow as usize..end]
    }
}

fn in_loop(pos: &[Pos], h: u32, b: u32) -> bool {
    let mut l = pos[b as usize].loop_of;
    while l != NO_BLOCK {
        if l == h {
            return true;
        }
        l = pos[l as usize].loop_parent;
    }
    false
}

#[derive(Default)]
struct Scratch {
    stack: Vec<(u32, u32)>,
    preds: Succ,
    fill: Vec<u32>,
    idom: Vec<u32>,
    forward_in: Vec<u32>,
    mark: Vec<u32>,
    body: Vec<u32>,
    pairs: Vec<(u32, u32)>,
}

thread_local! {
    static SCRATCH: std::cell::RefCell<Scratch> = std::cell::RefCell::default();
}

fn reset<T: Copy>(v: &mut Vec<T>, len: usize, x: T) {
    v.clear();
    v.resize(len, x);
}

/// `unplaced` blocks (a counted loop's latch) are never followers. `in_place`
/// exits print where they are branched to, even inside a loop they leave.
/// `None` when the CFG is irreducible.
pub fn place(
    entry: Idx<Block>,
    succ: &Succ,
    unplaced: &[Idx<Block>],
    in_place: &[Idx<Block>],
) -> Option<Placement> {
    SCRATCH.with(|s| place_in(&mut s.borrow_mut(), entry, succ, unplaced, in_place))
}

fn place_in(
    s: &mut Scratch,
    entry: Idx<Block>,
    succ: &Succ,
    unplaced: &[Idx<Block>],
    in_place: &[Idx<Block>],
) -> Option<Placement> {
    let n = succ.offsets.len() - 1;
    let mut order = vec![NO_BLOCK; n];
    let mut blocks: Vec<u32> = Vec::with_capacity(n);
    succ.postorder(raw_of(entry), &mut s.stack, &mut order, &mut blocks);
    blocks.reverse();
    let k = blocks.len();
    for (p, &b) in blocks.iter().enumerate() {
        order[b as usize] = p as u32;
    }
    let mut pos = vec![
        Pos {
            loop_of: NO_BLOCK,
            loop_parent: NO_BLOCK,
            loop_size: 0,
            follow: 0,
            flags: 0,
        };
        k
    ];
    for &u in unplaced {
        let p = order[bx(u)];
        if p != NO_BLOCK {
            pos[p as usize].flags |= UNPLACED;
        }
    }
    for &u in in_place {
        let p = order[bx(u)];
        if p != NO_BLOCK {
            pos[p as usize].flags |= IN_PLACE;
        }
    }
    if k == 1 {
        let self_loop = succ.of(blocks[0] as usize).contains(&blocks[0]);
        if !self_loop {
            return Some(Placement {
                order,
                blocks,
                pos,
                latches: Vec::new(),
                follow: Vec::new(),
            });
        }
    }
    s.preds.store_predecessors(k, &mut s.fill, |edge| {
        for (p, &block) in blocks.iter().enumerate() {
            for &to in succ.of(block as usize) {
                edge(p as u32, order[to as usize]);
            }
        }
    });
    let preds_of = |p| s.preds.of(p);
    let idom = &mut s.idom;
    solve_idoms(idom, k, 0..k as u32, preds_of, |p| p as u32);
    reset(&mut s.forward_in, k, 0);
    let mut latches: Vec<(u32, u32)> = Vec::new();
    for (p, &b) in blocks.iter().enumerate() {
        for &t in succ.of(b as usize) {
            let q = order[t as usize];
            if q as usize > p {
                s.forward_in[q as usize] += 1;
                continue;
            }
            let mut d = p as u32;
            while d > q {
                d = idom[d as usize];
            }
            if d != q {
                return None;
            }
            latches.push((q, p as u32));
            pos[q as usize].flags |= HEADER;
        }
    }
    latches.sort_unstable();
    latches.dedup();
    // Innermost loops first: a header claims every unclaimed block of its natural
    // loop, and a nested header already claimed is its child.
    reset(&mut s.mark, k, NO_BLOCK);
    let (mark, body) = (&mut s.mark, &mut s.body);
    let mut end = latches.len();
    while end > 0 {
        let h = latches[end - 1].0;
        let start = latches[..end].partition_point(|&(x, _)| x < h);
        body.clear();
        body.push(h);
        mark[h as usize] = h;
        for &(_, l) in &latches[start..end] {
            if mark[l as usize] != h {
                mark[l as usize] = h;
                body.push(l);
            }
        }
        // `body[0]` is the header, whose predecessors are outside the loop.
        let mut next = 1;
        while next < body.len() {
            let x = body[next];
            next += 1;
            for &q in preds_of(x as usize) {
                if mark[q as usize] != h {
                    mark[q as usize] = h;
                    body.push(q);
                }
            }
        }
        pos[h as usize].loop_size = body.len() as u32;
        for &b in body.iter() {
            let b = b as usize;
            let at = &mut pos[b];
            if at.loop_of == NO_BLOCK {
                at.loop_of = h;
            } else if at.loop_of as usize == b && b != h as usize && at.loop_parent == NO_BLOCK {
                at.loop_parent = h;
            }
        }
        end = start;
    }
    let pairs = &mut s.pairs;
    pairs.clear();
    for p in 1..k {
        if pos[p].flags & UNPLACED != 0 {
            continue;
        }
        let mut parent = idom[p];
        let mut lifted = false;
        let mut l = pos[parent as usize].loop_of;
        // A raise reached from one place never falls through, so it prints there inside
        // the loop instead of after it behind a goto.
        if pos[p].flags & IN_PLACE != 0 && s.forward_in[p] == 1 {
            l = NO_BLOCK;
        }
        while l != NO_BLOCK && !in_loop(&pos, l, p as u32) {
            parent = l;
            lifted = true;
            l = pos[parent as usize].loop_parent;
        }
        if s.forward_in[p] >= 2 || lifted {
            pos[p].flags |= FOLLOWER;
            pairs.push((parent, p as u32));
        }
    }
    // Pairs come in follower position order, so a stable sort by parent leaves each
    // group in reverse postorder.
    pairs.sort_by_key(|&(parent, _)| parent);
    let mut next = 0;
    for (p, at) in pos.iter_mut().enumerate() {
        at.follow = next as u32;
        while next < pairs.len() && pairs[next].0 as usize == p {
            next += 1;
        }
    }
    let follow = pairs
        .iter()
        .map(|&(_, f)| bi(blocks[f as usize] as usize))
        .collect();
    Some(Placement {
        order,
        blocks,
        pos,
        latches,
        follow,
    })
}

fn raw_of(b: Idx<Block>) -> u32 {
    bx(b) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use comfy_la_arena::RawIdx;
    use ustr::Ustr;

    fn mk(n: usize, terms: Vec<Term>) -> MirFunc {
        let mut f = MirFunc::new(Ustr::from("t"));
        for _ in 1..n {
            f.new_block();
        }
        let _dummy = f.insts.alloc(Inst::Const(Lit::Bool(true)));
        for (i, t) in terms.into_iter().enumerate() {
            f.blocks[bi(i)].term = t;
        }
        f
    }

    fn cond() -> Idx<Inst> {
        Idx::from_raw(RawIdx::from_u32(0))
    }

    fn branch(then_bb: usize, else_bb: usize) -> Term {
        Term::Branch {
            cond: cond(),
            then_bb: bi(then_bb),
            else_bb: bi(else_bb),
        }
    }

    #[test]
    fn dominance_intervals_agree_with_the_idom_walk() {
        let f = mk(
            8,
            vec![
                Term::Goto(bi(1)),
                branch(2, 6),
                branch(3, 4),
                Term::Goto(bi(5)),
                Term::Goto(bi(5)),
                Term::Goto(bi(1)),
                Term::Return(vec![]),
                Term::Goto(bi(5)),
            ],
        );
        let idom = crate::cfg::dominators(&f, bi(0));
        let intervals = idom.intervals();
        for a in 0..8 {
            for b in 0..8 {
                assert_eq!(
                    intervals.dominates(bi(a), bi(b)),
                    idom.dominates(bi(a), bi(b)),
                    "dominates({a}, {b})"
                );
            }
        }
        assert!(intervals.dominates(bi(2), bi(5)));
        assert!(!intervals.dominates(bi(3), bi(5)));
    }

    #[test]
    fn headerless_loop_is_rejected_but_placement_recognizes_it() {
        // 0 -> 1(header); 1 -> {1, 2}; 2 -> {3(exit), 1}: two latches and no single
        // test block that leaves the loop.
        let f = mk(
            4,
            vec![
                Term::Goto(bi(1)),
                branch(1, 2),
                branch(3, 1),
                Term::Return(vec![]),
            ],
        );
        assert!(analyze(&f).loops.is_empty());
        let p = place(bi(0), &dense_succ(&f), &[], &[]).expect("reducible");
        assert!(p.is_header(bi(1)));
        assert_eq!(p.latches(bi(1)).collect::<Vec<_>>(), vec![bi(1), bi(2)]);
        assert_eq!(p.followers(bi(1)), &[bi(3)]);
    }

    fn dense_succ(f: &MirFunc) -> Succ {
        Succ::from_func(f, None)
    }
}
