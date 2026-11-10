//! CFG structuring analysis. For the relooper backends (GLSL, JS): immediate
//! post-dominators, natural loops, and range-`for` recognition. Both backends
//! need structured goto-free output and see the same reducible CFGs skull-mir
//! lowers for a tier-0 fn, so the simple iterative algorithms suffice (tens of
//! blocks). What differs per target is passed in as [`LoopShapes`] and
//! [`CountedBound`]; the structured walk over this analysis is
//! [`relooper`](super::relooper).
//!
//! For the Lua printer, which keeps forward `goto` and so can structure every
//! reducible CFG: [`place`], the dominator-tree placement of each block.

use super::dense::{bi, bx, collect_loop_body, dominates, idoms, pred_list, rpo, succ_list};
use crate::mir::{Block, Cell, Inst, Lit, MirFunc, Term};
use comfy_la_arena::Idx;
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

/// Which natural loops a backend can structure.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LoopShapes {
    /// Only loops with a header-condition test (`while cond`/`for`). A test-less
    /// loop is left unregistered, so the walker rejects it as unstructured.
    TestedOnly,
    /// Also `loop {}` shapes with no dominating test, emitted as a bare
    /// `while (true) { <body> }` whose exits are inline `break`s.
    AllowHeaderless,
}

/// Which range-`for` bounds a backend can render in its loop header.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CountedBound {
    /// Any bound value.
    Any,
    /// Only a `Load` (of a hoisted preheader cell) or a const: the counted path
    /// skips the header block, so a bound computed there would reference a temp
    /// that is never emitted.
    InlineOnly,
}

/// One natural loop: its header (back-edge target), the set of blocks in the
/// loop body, and its single exit (the block outside the loop that a body block
/// branches to). Tier-0 `while`/`for` loops are single-exit by construction.
#[derive(Clone)]
pub struct LoopRegion {
    pub header: Idx<Block>,
    pub body: HashSet<Idx<Block>>,
    pub exit: Idx<Block>,
    /// The header-condition test block of a `while cond`/`for` shape: the body
    /// block that dominates every latch and has a lone out-of-loop successor.
    /// `None` for a `loop {}` whose exits are inline `break`s (no test dominates
    /// the back edges, e.g. a `continue` precedes the `break`); registered only
    /// under [`LoopShapes::AllowHeaderless`].
    pub test: Option<Idx<Block>>,
}

pub struct Structure {
    /// Immediate post-dominator per block; `None` = the virtual exit (a diverging
    /// branch has no real join).
    ipdom: Vec<Option<Idx<Block>>>,
    pub loops: HashMap<Idx<Block>, LoopRegion>,
}

impl Structure {
    pub fn ipdom(&self, b: Idx<Block>) -> Option<Idx<Block>> {
        self.ipdom[bx(b)]
    }

    /// A no-analysis placeholder for a walker that never consults ipdoms or
    /// loops (JS trampoline mode: the post-coroutine-transform CFG is
    /// irreducible by construction, so these analyses don't apply).
    pub fn empty(n: usize) -> Structure {
        Structure {
            ipdom: vec![None; n],
            loops: HashMap::default(),
        }
    }
}

pub fn analyze(f: &MirFunc, shapes: LoopShapes) -> Structure {
    let n = f.blocks.len();
    let succ = succ_list(f);
    let preds = pred_list(n, &succ);

    let fwd_rpo = rpo(n, bx(f.entry), &succ);
    let idom = idoms(n, bx(f.entry), &preds, &fwd_rpo);

    let ipdom = post_idoms(f, n, &succ);
    let loops = natural_loops(f, n, &succ, &preds, &idom, shapes);

    Structure { ipdom, loops }
}

/// Immediate post-dominators: dominators on the reversed CFG rooted at a virtual
/// exit (node `n`) that every `Return` block flows into. `Unreachable` blocks do
/// NOT connect to the exit: they mark control the compiler proved never runs (an
/// exhaustive-`match` default, code after a diverging call), so counting them as
/// an exit path would defeat post-dominance of the REAL join -- an exhaustive
/// match's `if/else` chain would then double-walk its join block and reject as
/// unstructured control flow.
fn post_idoms(f: &MirFunc, n: usize, succ: &[Vec<usize>]) -> Vec<Option<Idx<Block>>> {
    let exit = n;
    let nn = n + 1;
    // Reverse graph: edge b->s becomes s->b; every `Return` block gets an edge
    // from the virtual exit.
    let mut rev_adj = vec![Vec::new(); nn];
    let mut rev_preds = vec![Vec::new(); nn];
    for b in 0..n {
        for &s in &succ[b] {
            rev_adj[s].push(b);
        }
        // Reverse predecessors of b are its forward successors.
        rev_preds[b].extend_from_slice(&succ[b]);
        if matches!(f.blocks[bi(b)].term, Term::Return(_)) {
            rev_adj[exit].push(b);
            rev_preds[b].push(exit);
        }
    }

    let rev_rpo = rpo(nn, exit, &rev_adj);
    let pidom = idoms(nn, exit, &rev_preds, &rev_rpo);

    let mut out = vec![None; n];
    for b in 0..n {
        out[b] = match pidom[b] {
            Some(x) if x != exit => Some(bi(x)),
            _ => None,
        };
    }
    out
}

fn natural_loops(
    f: &MirFunc,
    n: usize,
    succ: &[Vec<usize>],
    preds: &[Vec<usize>],
    idom: &[Option<usize>],
    shapes: LoopShapes,
) -> HashMap<Idx<Block>, LoopRegion> {
    // Back edges grouped by header, keeping each latch (back-edge source).
    let mut latches: HashMap<usize, Vec<usize>> = HashMap::default();
    for latch in 0..n {
        if idom[latch].is_none() && latch != bx(f.entry) {
            continue; // unreachable
        }
        for &h in &succ[latch] {
            if dominates(idom, h, latch) {
                latches.entry(h).or_default().push(latch);
            }
        }
    }

    let mut out = HashMap::default();
    for (h, hlatches) in latches {
        let mut body: HashSet<usize> = HashSet::default();
        for &l in &hlatches {
            collect_loop_body(h, l, preds, &mut body);
        }
        // The loop's exit is the out-of-loop successor of the condition test: the
        // shallowest body block that dominates every latch and leaves the loop.
        // (Early `return`/`break` blocks branch out too, but don't dominate the
        // latch or aren't the shallowest, so they are not mistaken for the exit.)
        let mut candidates: Vec<usize> = body
            .iter()
            .copied()
            .filter(|&b| {
                succ[b].iter().any(|s| !body.contains(s))
                    && hlatches.iter().all(|&l| dominates(idom, b, l))
            })
            .collect();
        candidates.sort_by_key(|&b| b);
        let single_test = candidates
            .iter()
            .copied()
            .find(|&c| candidates.iter().all(|&c2| dominates(idom, c, c2)))
            .and_then(|test| {
                let mut exits = succ[test].iter().copied().filter(|s| !body.contains(s));
                let e = exits.next()?;
                exits.next().is_none().then_some((test, e))
            });
        let (exit, test) = match (single_test, shapes) {
            (Some((test, exit)), _) => (exit, Some(bi(test))),
            (None, LoopShapes::TestedOnly) => continue, // the walker rejects
            (None, LoopShapes::AllowHeaderless) => {
                // Structurable only if every out-of-loop edge across the body
                // targets ONE exit block; a multi-exit loop is dropped so the
                // walker rejects cleanly.
                let mut exit_targets: Vec<usize> = body
                    .iter()
                    .flat_map(|&b| succ[b].iter().copied().filter(|s| !body.contains(s)))
                    .collect();
                exit_targets.sort_unstable();
                exit_targets.dedup();
                let [exit] = exit_targets[..] else {
                    continue;
                };
                (exit, None)
            }
        };
        out.insert(
            bi(h),
            LoopRegion {
                header: bi(h),
                body: body.iter().map(|&x| bi(x)).collect(),
                exit: bi(exit),
                test,
            },
        );
    }
    out
}

/// A recognized range-`for` loop (the shape `lower_for_range` emits).
#[derive(Clone, Copy)]
pub struct CountedLoop {
    pub slot: Idx<Cell>,
    pub bound: Idx<Inst>,
    pub inclusive: bool,
    pub body_entry: Idx<Block>,
    pub latch: Idx<Block>,
    pub exit: Idx<Block>,
}

/// Split `st.loops` into range-`for` loops (emitted as a counted `for`) and the
/// rest (the general `while` path). The counted path is valid only when the
/// header's own `Branch` IS the loop test (its else is the real exit): a
/// `while i < N && ..` header also looks like `i < N` with an `i + 1` latch, but
/// its exit lies inside the loop, so that one takes the general path.
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
        match match_counted_loop(f, h, bound) {
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

/// `Some(CountedLoop)` iff `h` is a `for i in lo..hi` header: a `Branch` on
/// `IntLt/IntLeq(load slot, bound)`, entered by a pure-increment latch
/// (`load slot; 1; add; store slot; goto h`). The `slot` cell must be
/// `loop_binding` -- desugar provenance that only `lower_for_range` sets, so a
/// `while i < n { ..; i = i + 1 }` (manual counter) is NOT mistaken for a
/// range-for and takes the general path instead.
fn match_counted_loop(f: &MirFunc, h: Idx<Block>, bound_rule: CountedBound) -> Option<CountedLoop> {
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

/// `p`: `load slot; 1; slot + 1; store slot; goto h` -- a pure `slot += 1` latch.
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

/// Dense successor lists in compressed form: block `b`'s successors are
/// `targets[offsets[b]..offsets[b + 1]]`, so a body costs two allocations
/// rather than one per block.
#[derive(Default)]
pub struct Succ {
    pub offsets: Vec<u32>,
    pub targets: Vec<u32>,
}

impl Succ {
    fn of(&self, b: usize) -> &[u32] {
        &self.targets[self.offsets[b] as usize..self.offsets[b + 1] as usize]
    }
}

/// Where each block of a reducible CFG is printed, for a printer that nests
/// blocks along the dominator tree and keeps forward jumps (Ramsey, "Beyond
/// Relooper"). A block reached by two or more forward edges, or one that
/// leaves a loop, is a *follower*: it is printed after the construct of its
/// placement parent, its immediate dominator lifted out of every loop that
/// does not contain it. Every other block is printed inline at its one
/// forward edge.
///
/// Everything but `order` is indexed by reverse-postorder position, over
/// the blocks reachable from the entry only.
pub struct Placement {
    /// Block -> its reverse-postorder position, [`NO_BLOCK`] if unreachable.
    order: Vec<u32>,
    /// Position -> block.
    blocks: Vec<u32>,
    pos: Vec<Pos>,
    /// `(header, latch)` positions of every back edge, sorted.
    latches: Vec<(u32, u32)>,
    /// Followers grouped by placement parent, each group in reverse
    /// postorder; [`Pos::follow`] is where a parent's group starts.
    follow: Vec<Idx<Block>>,
}

#[derive(Clone, Copy)]
struct Pos {
    /// The innermost loop header containing this position (itself, for a
    /// header).
    loop_of: u32,
    /// For a header: the header of the loop enclosing its loop.
    loop_parent: u32,
    /// For a header: how many blocks its natural loop holds.
    loop_size: u32,
    follow: u32,
    flags: u8,
}

const NO_BLOCK: u32 = u32::MAX;
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

    /// A loop header: the target of a back edge.
    pub fn is_header(&self, b: Idx<Block>) -> bool {
        self.flag(b, HEADER)
    }

    /// The back-edge sources of header `h`.
    pub fn latches(&self, h: Idx<Block>) -> impl Iterator<Item = Idx<Block>> + '_ {
        let p = self.at(h);
        let start = self.latches.partition_point(|&(x, _)| x < p);
        self.latches[start..]
            .iter()
            .take_while(move |&&(x, _)| x == p)
            .map(|&(_, l)| bi(self.blocks[l as usize] as usize))
    }

    /// Whether `b` lies in the natural loop headed by `h` (or a nested one).
    pub fn in_loop(&self, h: Idx<Block>, b: Idx<Block>) -> bool {
        let (h, b) = (self.at(h), self.at(b));
        b != NO_BLOCK && in_loop(&self.pos, h, b)
    }

    /// How many blocks the natural loop headed by `h` holds, nested loops
    /// included.
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

    /// The followers placed after `b`'s construct, in reverse postorder.
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

/// Working storage for [`place`], kept per thread: one call per printed body
/// would otherwise allocate all of it again.
#[derive(Default)]
struct Scratch {
    stack: Vec<(u32, u32)>,
    pred_start: Vec<u32>,
    preds: Vec<u32>,
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

/// [`Placement`] over `succ` (successors that may differ from the function's
/// own edges) from `entry`. `unplaced` blocks are never printed as statements
/// (a counted loop's latch, which its `for` line subsumes), so they are never
/// followers. `in_place` exits (blocks that raise) print where they are
/// branched to, even inside a loop they leave. `None` when the CFG is
/// irreducible.
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
    // Reverse postorder by an iterative depth-first search.
    let mut order = vec![NO_BLOCK; n];
    let mut blocks: Vec<u32> = Vec::with_capacity(n);
    s.stack.clear();
    s.stack.push((raw_of(entry), 0));
    order[bx(entry)] = 0;
    while let Some(top) = s.stack.last_mut() {
        let (b, i) = *top;
        match succ.of(b as usize).get(i as usize) {
            Some(&t) => {
                top.1 += 1;
                if order[t as usize] == NO_BLOCK {
                    order[t as usize] = 0;
                    s.stack.push((t, 0));
                }
            }
            None => {
                blocks.push(b);
                s.stack.pop();
            }
        }
    }
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
    // Predecessors by position. Every successor of a reachable block is
    // reachable, so the edges of `blocks` are all of them.
    reset(&mut s.pred_start, k + 1, 0);
    for &b in &blocks {
        for &t in succ.of(b as usize) {
            s.pred_start[order[t as usize] as usize + 1] += 1;
        }
    }
    for p in 0..k {
        s.pred_start[p + 1] += s.pred_start[p];
    }
    reset(&mut s.preds, s.pred_start[k] as usize, 0);
    s.fill.clear();
    s.fill.extend_from_slice(&s.pred_start);
    for (p, &b) in blocks.iter().enumerate() {
        for &t in succ.of(b as usize) {
            let q = order[t as usize] as usize;
            s.preds[s.fill[q] as usize] = p as u32;
            s.fill[q] += 1;
        }
    }
    let (pred_start, preds) = (&s.pred_start, &s.preds);
    let preds_of = |p: usize| &preds[pred_start[p] as usize..pred_start[p + 1] as usize];
    // Immediate dominators (Cooper, Harvey, Kennedy); in position space a
    // dominator always has the smaller position.
    let idom = &mut s.idom;
    reset(idom, k, NO_BLOCK);
    idom[0] = 0;
    let mut retreating = false;
    let mut changed = true;
    while changed {
        changed = false;
        for p in 1..k {
            let mut new = NO_BLOCK;
            for &q in preds_of(p) {
                retreating |= q as usize >= p;
                if idom[q as usize] == NO_BLOCK {
                    continue;
                }
                new = if new == NO_BLOCK {
                    q
                } else {
                    let (mut a, mut b) = (q, new);
                    while a != b {
                        while a > b {
                            a = idom[a as usize];
                        }
                        while b > a {
                            b = idom[b as usize];
                        }
                    }
                    a
                };
            }
            if idom[p] != new {
                idom[p] = new;
                changed = true;
            }
        }
        // Without a retreating edge every predecessor was final when its
        // successor was visited, so the first pass settled them all.
        changed &= retreating;
    }
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
    // Innermost loops first: a header claims every unclaimed block of its
    // natural loop, and a nested header already claimed is its child.
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
        // A raise reached from one place never falls through, so it prints
        // there inside the loop instead of after it behind a goto.
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
    // Pairs come in position order of the follower, so a stable sort by
    // parent leaves each group in reverse postorder.
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

    /// Build a MirFunc with `n` blocks (0 = entry) and the given terminators.
    /// Branch conds are a single shared dummy bool const.
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
        // A loop with a diamond inside, a side exit, and an unreachable block 7.
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
        let fast = idom.intervals();
        for a in 0..8 {
            for b in 0..8 {
                assert_eq!(
                    fast.dominates(bi(a), bi(b)),
                    idom.dominates(bi(a), bi(b)),
                    "dominates({a}, {b})"
                );
            }
        }
        assert!(fast.dominates(bi(2), bi(5)));
        assert!(!fast.dominates(bi(3), bi(5)));
    }

    #[test]
    fn diamond() {
        // 0 -> {1, 2} -> 3(return)
        let f = mk(
            4,
            vec![
                branch(1, 2),
                Term::Goto(bi(3)),
                Term::Goto(bi(3)),
                Term::Return(vec![]),
            ],
        );
        let s = analyze(&f, LoopShapes::TestedOnly);
        assert_eq!(s.ipdom(bi(0)), Some(bi(3)));
        assert_eq!(s.ipdom(bi(1)), Some(bi(3)));
        assert_eq!(s.ipdom(bi(2)), Some(bi(3)));
        assert_eq!(s.ipdom(bi(3)), None); // virtual exit
        assert!(s.loops.is_empty());
    }

    #[test]
    fn diverged_arm() {
        // 0 -> {1(return), 2}; 2 -> 3(return). ipdom(0) collapses to virtual exit.
        let f = mk(
            4,
            vec![
                branch(1, 2),
                Term::Return(vec![]),
                Term::Goto(bi(3)),
                Term::Return(vec![]),
            ],
        );
        let s = analyze(&f, LoopShapes::TestedOnly);
        assert_eq!(s.ipdom(bi(0)), None);
        assert_eq!(s.ipdom(bi(2)), Some(bi(3)));
        assert!(s.loops.is_empty());
    }

    #[test]
    fn unreachable_arm_is_not_an_exit_path() {
        // An exhaustive `match` as an expression: both arms store one result
        // cell and jump to a real join, while the dead default arm is a
        // REACHABLE `Unreachable` block (0 -> {1, 2}; 2 -> {3, 4-unreachable};
        // 1 and 3 -> 5; 5 returns). The dead arm must not count as an exit path:
        // the outer branch keeps the real join, and the last test's ipdom is its
        // LIVE arm. Counting the dead arm instead collapses both branches to the
        // virtual exit, both arms absorb the continuation, and the join
        // double-walks into `unstructured control flow`. (`diverged_arm` above
        // is the control: a `Return` arm still collapses.)
        let f = mk(
            6,
            vec![
                branch(1, 2),
                Term::Goto(bi(5)),
                branch(3, 4),
                Term::Goto(bi(5)),
                Term::Unreachable,
                Term::Return(vec![]),
            ],
        );
        let s = analyze(&f, LoopShapes::TestedOnly);
        assert_eq!(s.ipdom(bi(0)), Some(bi(5)));
        assert_eq!(s.ipdom(bi(1)), Some(bi(5)));
        assert_eq!(s.ipdom(bi(2)), Some(bi(3)));
        assert_eq!(s.ipdom(bi(3)), Some(bi(5)));
        assert_eq!(s.ipdom(bi(5)), None); // virtual exit
        assert!(s.loops.is_empty());
    }

    #[test]
    fn nested_loop() {
        // 0 -> 1(outer header) -> {2(inner header), 6(exit,return)}
        // 2 -> {3, 5}; 3 -> 4; 4 -> 2(inner back edge); 5 -> 1(outer back edge)
        let f = mk(
            7,
            vec![
                Term::Goto(bi(1)),
                branch(2, 6),
                branch(3, 5),
                Term::Goto(bi(4)),
                Term::Goto(bi(2)),
                Term::Goto(bi(1)),
                Term::Return(vec![]),
            ],
        );
        for shapes in [LoopShapes::TestedOnly, LoopShapes::AllowHeaderless] {
            let s = analyze(&f, shapes);
            let outer = s.loops.get(&bi(1)).expect("outer loop");
            assert_eq!(outer.exit, bi(6));
            assert_eq!(outer.test, Some(bi(1)));
            assert!(
                outer.body.contains(&bi(1))
                    && outer.body.contains(&bi(5))
                    && outer.body.contains(&bi(2))
            );
            assert!(!outer.body.contains(&bi(6)));
            let inner = s.loops.get(&bi(2)).expect("inner loop");
            assert_eq!(inner.exit, bi(5));
            assert_eq!(inner.test, Some(bi(2)));
            assert!(inner.body.contains(&bi(2)) && inner.body.contains(&bi(4)));
            assert!(!inner.body.contains(&bi(1)) && !inner.body.contains(&bi(5)));
        }
    }

    #[test]
    fn headerless_loop_registers_only_when_allowed() {
        // `loop { if c { continue } ; if d { break } }`:
        // 0 -> 1(header); 1 -> {1(continue), 2}; 2 -> {3(exit), 1}; 3 returns.
        // Two latches (1 and 2), and no block dominating both leaves the loop
        // except 1, whose successors stay inside, so there is no single test.
        let f = mk(
            4,
            vec![
                Term::Goto(bi(1)),
                branch(1, 2),
                branch(3, 1),
                Term::Return(vec![]),
            ],
        );
        assert!(analyze(&f, LoopShapes::TestedOnly).loops.is_empty());
        let s = analyze(&f, LoopShapes::AllowHeaderless);
        let l = s.loops.get(&bi(1)).expect("headerless loop");
        assert_eq!(l.exit, bi(3));
        assert_eq!(l.test, None);
    }

    fn dense_succ(f: &MirFunc) -> Succ {
        let mut s = Succ::default();
        s.offsets.push(0);
        for list in super::super::dense::succ_list(f) {
            s.targets.extend(list.iter().map(|&t| t as u32));
            s.offsets.push(s.targets.len() as u32);
        }
        s
    }

    #[test]
    fn place_rejects_an_irreducible_cfg() {
        // 0 -> {1, 2}; 1 -> 2; 2 -> 1: the cycle has two entries.
        let f = mk(
            4,
            vec![
                branch(1, 2),
                branch(2, 3),
                Term::Goto(bi(1)),
                Term::Return(vec![]),
            ],
        );
        assert!(place(bi(0), &dense_succ(&f), &[], &[]).is_none());
    }

    #[test]
    fn place_puts_a_join_after_its_dominator() {
        // 0 -> {1, 2} -> 3: the join follows the diamond; the arms are inline.
        let f = mk(
            4,
            vec![
                branch(1, 2),
                Term::Goto(bi(3)),
                Term::Goto(bi(3)),
                Term::Return(vec![]),
            ],
        );
        let p = place(bi(0), &dense_succ(&f), &[], &[]).expect("reducible");
        assert_eq!(p.followers(bi(0)), &[bi(3)]);
        assert!(!p.is_follower(bi(1)) && !p.is_follower(bi(2)));
    }

    #[test]
    fn place_lifts_a_loop_exit_out_of_the_loop() {
        // 0 -> 1(header) -> {2, 3}; 2 -> 1; 3 returns. The exit 3 is
        // dominated by the header but printed after the loop, as a follower of
        // the header's own parent.
        let f = mk(
            4,
            vec![
                Term::Goto(bi(1)),
                branch(2, 3),
                Term::Goto(bi(1)),
                Term::Return(vec![]),
            ],
        );
        let p = place(bi(0), &dense_succ(&f), &[], &[]).expect("reducible");
        assert!(p.is_header(bi(1)));
        assert_eq!(p.latches(bi(1)).collect::<Vec<_>>(), vec![bi(2)]);
        assert!(p.in_loop(bi(1), bi(2)) && !p.in_loop(bi(1), bi(3)));
        assert_eq!(p.followers(bi(1)), &[bi(3)]);
    }

    #[test]
    fn place_keeps_a_raising_loop_exit_where_it_branches_from() {
        // As above, but the exit 3 raises: it prints inside the loop.
        let f = mk(
            4,
            vec![
                Term::Goto(bi(1)),
                branch(2, 3),
                Term::Goto(bi(1)),
                Term::Unreachable,
            ],
        );
        let p = place(bi(0), &dense_succ(&f), &[], &[bi(3)]).expect("reducible");
        assert!(!p.in_loop(bi(1), bi(3)) && !p.is_follower(bi(3)));
        assert!(p.followers(bi(1)).is_empty());
    }

    #[test]
    fn place_nests_an_inner_loop_in_its_outer_loop() {
        let f = mk(
            7,
            vec![
                Term::Goto(bi(1)),
                branch(2, 6),
                branch(3, 5),
                Term::Goto(bi(4)),
                Term::Goto(bi(2)),
                Term::Goto(bi(1)),
                Term::Return(vec![]),
            ],
        );
        let p = place(bi(0), &dense_succ(&f), &[], &[]).expect("reducible");
        assert!(p.in_loop(bi(1), bi(4)) && p.in_loop(bi(2), bi(4)));
        assert!(p.in_loop(bi(1), bi(5)) && !p.in_loop(bi(2), bi(5)));
        // The inner exit stays inside the outer loop.
        assert_eq!(p.followers(bi(2)), &[bi(5)]);
        assert_eq!(p.followers(bi(1)), &[bi(6)]);
    }
}
